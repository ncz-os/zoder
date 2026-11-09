//! Golden tests for the `--json` failure contract (D3).
//!
//! Every failure class the CLI can produce must keep a STABLE shape:
//! the same exit code, the same stderr marker, and the same JSON error
//! code on stdout. These tests drive the PRODUCTION code end to end
//! through the built `zoder` binary, offline:
//!
//! * `review --json` against a scratch HOME (config + corpus written by
//!   the test in the exact shape `Config::load` / `Corpus::load`
//!   accept — see `write_home`) and a scratch git repo;
//! * provider HTTP is a local [`wiremock`] stand-in on the
//!   `openai-chat` wire path (`{base_url}/v1/chat/completions`), so
//!   503 / 401 / invalid-verdict / timeout shapes are deterministic;
//! * no real model calls, no network, no daemon — except the cancel
//!   test, which installs the same stand-in engine daemon the
//!   `shared_daemon_lifecycle` tests use (a Unix-socket server that
//!   never acknowledges `session/cancel`) and drives the PRODUCTION
//!   reap scope (`zoder rescue`).
//!
//! What is NOT covered here, and why:
//!
//! * **loop unresolved** (`final_verdict: "unresolved"`): reaching the
//!   unresolved path needs the AUTHOR turns to run, and author turns
//!   execute on the engine daemon. The loop goldens below pin the two
//!   shapes reachable without a daemon (missing engine config; engine
//!   with no agent route for the model). Driving a true `unresolved`
//!   golden needs a daemon stand-in that completes author turns with
//!   empty diffs — daemon-bound, left for a follow-up.
//! * **engine budget refusal**: the refusal string lives on the daemon
//!   side, so it is daemon-bound for the same reason.
//!
//! A failing assertion here means the contract moved: update D3 and
//! the assertion together, never the assertion alone.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// The frozen exit ladder from `docs/EXIT_CODES.md` (decision D3). Every
// `review` / `loop` failure exits the rung its class documents, so a pin
// names the RUNG, never the pre-contract `1`.
//
// `permanent` and `internal` share the 50 rung by design: a permanent
// failure is an internal-class outcome (nothing to fix, nothing to retry).
const EXIT_TRANSIENT: i32 = 20; // retry the same invocation
const EXIT_CONFIG: i32 = 40; // an operator must change something
const EXIT_PERMANENT: i32 = 50; // identical repeat failure / unclassified
/// `zoder rescue` is OUTSIDE the contract scope, so it keeps the historical
/// code `1` (see `failure::in_contract_scope`).
const EXIT_UNCONTRACTED: i32 = 1;

/// Where the built `zoder` binary lives, relative to this test binary.
fn zoder_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let dep_dir = exe.parent().expect("deps dir");
    let candidate = dep_dir.join("..").join("zoder");
    if candidate.exists() {
        return candidate;
    }
    let candidate = dep_dir.join("..").join("release").join("zoder");
    if candidate.exists() {
        return candidate;
    }
    panic!("cannot locate the zoder binary next to {}", exe.display());
}

/// A scratch directory that removes itself on drop.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn fresh(name: &str) -> Self {
        // Short prefix: `<scratch>/cf/data/daemon.sock` must stay under
        // the ~108-byte AF_UNIX limit on macOS temp dirs.
        let dir = std::env::temp_dir().join(format!("zg-{}-{}", name, std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).ok();
        }
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        Self { dir }
    }

    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// Write the minimal HOME a `review --json` run needs, in the exact
/// shape the loaders accept: `Config::load` requires `id` (not
/// `name`), `auth`, `default_provider`, `corpus_path` and friends;
/// `Corpus::load` routes only entries with `free`, `route_candidate`
/// and `kind == "chat"`. `serves` is the model-id prefix list the
/// fake provider claims; the corpus carries exactly those ids.
fn write_home(home: &Path, base_url: &str, serves: &[&str]) {
    let config = serde_json::json!({
        "providers": [
            {
                "id": "fake",
                "kind": "openai-chat",
                "base_url": base_url,
                "auth": {"type": "none"},
                "billing": "free",
                "serves": serves,
            }
        ],
        "default_provider": "fake",
        "strict_free": false,
        "corpus_path": home.join("model_corpus.json"),
        "ledger_path": home.join("ledger.jsonl"),
        "health_path": home.join("health.json"),
        "reviewer_model": serves.first().copied().unwrap_or("fake-model"),
    });
    std::fs::write(home.join("config.json"), config.to_string()).expect("write config.json");
    let models: Vec<serde_json::Value> = serves
        .iter()
        .map(|id| {
            serde_json::json!({
                "id": id,
                "free": true,
                "route_candidate": true,
                "kind": "chat",
            })
        })
        .collect();
    let corpus = serde_json::json!({"source": "test", "models": models});
    std::fs::write(home.join("model_corpus.json"), corpus.to_string())
        .expect("write model_corpus.json");
    std::fs::create_dir_all(home.join("engine")).expect("create the engine dir");
    std::fs::write(home.join("engine").join("config.toml"), "").expect("write engine config");
}

/// A scratch git repo with one committed file, then dirtied, so
/// `--scope working-tree` has a reviewable diff.
fn dirty_repo(scratch: &Scratch) -> PathBuf {
    let repo = scratch.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create the repo dir");
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} must succeed");
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "golden@example.invalid"]);
    run(&["config", "user.name", "golden"]);
    run(&["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("note.txt"), "one\n").expect("write the file");
    run(&["add", "note.txt"]);
    run(&["commit", "-qm", "seed"]);
    std::fs::write(repo.join("note.txt"), "two\n").expect("dirty the file");
    repo
}

/// Run `zoder review --json` in `repo` with `home` as the config HOME.
fn run_review(home: &Path, repo: &Path, extra_args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(zoder_binary());
    cmd.arg("review")
        .arg("--json")
        .arg("--scope")
        .arg("working-tree")
        .arg("--model")
        .arg("fake-model")
        .args(extra_args)
        .current_dir(repo)
        .env("ZODER_HOME", home)
        .env("ZEROCLAW_CONFIG_DIR", home)
        .env("ZODER_SKIP_CATALOG_ENRICHMENT", "1");
    cmd.output().expect("run zoder review --json")
}

/// Parse stdout as JSON, with the raw text in the failure message.
fn stdout_json(output: &std::process::Output) -> serde_json::Value {
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("stdout must be JSON, got: {text:?}"))
}

/// A valid completion verdict body for the `openai-chat` wire path.
/// The inner `content` is itself a JSON verdict document.
fn verdict_body() -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-ok",
        "object": "chat.completion",
        "created": 1,
        "model": "fake-model",            // NOTE: `next_steps` is required — a verdict without it fails
            // validation and the run retries the HTTP call, so a body
            // missing it can never pin the recovery side of 0/N.
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "{\"verdict\": \"approve\", \"summary\": \"fine\", \"findings\": [], \"next_steps\": []}"
                },
                "finish_reason": "stop"
            }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

/// A 200 body with a valid chat-completion envelope but a non-JSON
/// verdict `content`: the invalid-verdict shape (D3), distinct from a
/// malformed HTTP body.
fn invalid_verdict_body() -> serde_json::Value {
    let mut body = verdict_body();
    body["choices"][0]["message"]["content"] =
        serde_json::Value::String("this is not verdict json".to_owned());
    body
}

#[test]
fn empty_diff_review_is_stable() {
    // A clean repo has no diff to review: the permanent rung (50),
    // the empty_diff
    // stderr marker, and a JSON envelope whose result AND verdict
    // both say empty_diff — while `complete` stays true, because the
    // run itself succeeded (D3 keeps the verdict stable too).
    let scratch = Scratch::fresh("empty-diff");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("create the home dir");
    write_home(&home, "http://127.0.0.1:1", &["fake-model"]);
    let repo = scratch.path().join("repo");
    std::fs::create_dir_all(&repo).expect("create the repo dir");
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .expect("git init");
    assert!(status.success());

    let output = run_review(&home, &repo, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(EXIT_PERMANENT),
        "stderr was: {stderr}"
    );
    assert!(
        stderr.starts_with("zoder: error: empty_diff:"),
        "stderr must start with the empty_diff marker, got: {stderr:?}"
    );
    let body = stdout_json(&output);
    assert_eq!(body["complete"], true);
    assert_eq!(body["result"], "empty_diff");
    assert_eq!(body["verdict"], "request_changes");
    // The failure travels INSIDE the result document (the command did
    // produce one), and it is the permanent `empty_diff` handle.
    assert_eq!(body["failure"]["code"], "empty_diff", "{body}");
    assert_eq!(body["failure"]["class"], "permanent", "{body}");
}

#[test]
fn no_reviewer_model_is_stable() {
    // A valid config whose providers serve only `other-model`, while
    // the run requests `fake-model`: the run fails closed on the
    // transient rung (20), complete false, and the no-provider
    // summary that names the requested model and the configured
    // routes.
    let scratch = Scratch::fresh("no-reviewer");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("create the home dir");
    write_home(&home, "http://127.0.0.1:1", &["other-model"]);
    let repo = dirty_repo(&scratch);

    let output = run_review(&home, &repo, &[]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(EXIT_TRANSIENT),
        "stderr was: {stderr}"
    );
    assert!(
        stderr.contains("0/1 reviewers completed"),
        "stderr must carry the 0/N marker, got: {stderr:?}"
    );
    let body = stdout_json(&output);
    assert_eq!(body["complete"], false);
    assert_eq!(body["verdict"], "request_changes");
    let summary = body["reviewers"][0]["summary"].as_str().unwrap_or("");
    assert!(
        summary.contains("no real provider is configured for reviewer model 'fake-model'"),
        "reviewer summary must name the unrouted model, got: {summary:?}"
    );
}

/// The shared shape of every 0/N completion: the transient rung,
/// complete false, the "0/1 reviewers completed" stderr marker, a
/// top-level verdict of request_changes, a failed `(failed)` reviewer
/// slot, and the typed `reviewers_unavailable` handle.
fn assert_zero_of_n(output: &std::process::Output) -> serde_json::Value {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(EXIT_TRANSIENT),
        "stderr was: {stderr}"
    );
    assert!(
        stderr.contains("0/1 reviewers completed"),
        "stderr must carry the 0/N marker, got: {stderr:?}"
    );
    let body = stdout_json(output);
    assert_eq!(body["complete"], false);
    assert_eq!(body["verdict"], "request_changes");
    assert_eq!(body["reviewers"][0]["model"], "(failed)");
    // The typed handle a fleet worker dispatches on, not just the exit
    // rung: every 0/N completion is `reviewers_unavailable` (transient).
    assert_eq!(body["failure"]["code"], "reviewers_unavailable", "{body}");
    assert_eq!(body["failure"]["class"], "transient", "{body}");
    body
}

#[tokio::test]
async fn provider_503_zero_of_n_is_stable() {
    // Every attempt answers 503: the verdict carries the provider
    // status in its summary and the run exits the transient rung (20).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
        .mount(&server)
        .await;

    let scratch = Scratch::fresh("503");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("create the home dir");
    write_home(&home, &server.uri(), &["fake-model"]);
    let repo = dirty_repo(&scratch);

    let output = run_review(&home, &repo, &["--retries", "0"]);
    let body = assert_zero_of_n(&output);
    let summary = body["reviewers"][0]["summary"].as_str().unwrap_or("");
    assert!(
        summary.contains("provider HTTP 503 Service Unavailable: overloaded"),
        "verdict summary must carry the provider status, got: {summary:?}"
    );
}

#[tokio::test]
async fn provider_401_zero_of_n_is_stable() {
    // A 401 is an auth failure, not a retryable outage: same 0/N
    // envelope, and the summary names the 401.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .mount(&server)
        .await;

    let scratch = Scratch::fresh("401");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("create the home dir");
    write_home(&home, &server.uri(), &["fake-model"]);
    let repo = dirty_repo(&scratch);

    let output = run_review(&home, &repo, &["--retries", "0"]);
    let body = assert_zero_of_n(&output);
    let summary = body["reviewers"][0]["summary"].as_str().unwrap_or("");
    assert!(
        summary.contains("provider HTTP 401 Unauthorized: bad key"),
        "verdict summary must carry the auth status, got: {summary:?}"
    );
}

#[tokio::test]
async fn invalid_verdict_zero_of_n_is_stable() {
    // The provider answers 200 with a valid envelope but a non-JSON
    // verdict body: the run does not retry forever, it completes
    // 0/N and names the invalid_verdict cause.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(invalid_verdict_body()))
        .mount(&server)
        .await;

    let scratch = Scratch::fresh("invalid-verdict");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("create the home dir");
    write_home(&home, &server.uri(), &["fake-model"]);
    let repo = dirty_repo(&scratch);

    let output = run_review(&home, &repo, &["--retries", "0"]);
    let body = assert_zero_of_n(&output);
    let summary = body["reviewers"][0]["summary"].as_str().unwrap_or("");
    assert!(
        summary.contains("invalid JSON twice"),
        "verdict summary must name the invalid-verdict cause, got: {summary:?}"
    );
    assert!(
        summary.contains("invalid_verdict"),
        "verdict summary must carry the invalid_verdict code, got: {summary:?}"
    );
}

#[tokio::test]
async fn provider_timeout_zero_of_n_is_stable() {
    // The provider never answers within the request timeout: the
    // attempt times out fast (the 2s budget, not the 30s server
    // delay) and the run completes 0/N.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(verdict_body())
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&server)
        .await;

    let scratch = Scratch::fresh("timeout");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("create the home dir");
    write_home(&home, &server.uri(), &["fake-model"]);
    let repo = dirty_repo(&scratch);

    let output = run_review(&home, &repo, &["--retries", "0", "--request-timeout", "2"]);
    let body = assert_zero_of_n(&output);
    let summary = body["reviewers"][0]["summary"].as_str().unwrap_or("");
    assert!(
        summary.contains("request timeout: no response in 2s"),
        "verdict summary must name the timeout, got: {summary:?}"
    );
}

#[tokio::test]
async fn provider_fails_then_recovers_is_stable() {
    // The first attempt fails but the recovery succeeds: the run is
    // complete, the verdict is approve. This pins the success side
    // of the 0/N boundary.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(503).set_body_string("blip"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(verdict_body()))
        .mount(&server)
        .await;

    let scratch = Scratch::fresh("recover");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("create the home dir");
    write_home(&home, &server.uri(), &["fake-model"]);
    let repo = dirty_repo(&scratch);

    let output = run_review(&home, &repo, &["--retries", "1"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "recovery must exit 0, stderr was: {stderr}"
    );
    let body = stdout_json(&output);
    assert_eq!(body["complete"], true);
    assert_eq!(body["verdict"], "approve");
}

#[test]
fn diff_over_max_bytes_is_stable() {
    // A diff over --max-diff-bytes is refused BEFORE any provider is
    // touched: the permanent rung, the typed error envelope, and the
    // stderr marker.
    let scratch = Scratch::fresh("oversize");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("create the home dir");
    write_home(&home, "http://127.0.0.1:1", &["fake-model"]);
    let repo = dirty_repo(&scratch);
    std::fs::write(repo.join("note.txt"), "x".repeat(200_000)).expect("grow the diff");

    let output = run_review(&home, &repo, &["--max-diff-bytes", "100"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(EXIT_PERMANENT),
        "stderr was: {stderr}"
    );
    let body = stdout_json(&output);
    assert_eq!(body["error"]["code"], "diff_too_large", "{body}");
    assert_eq!(body["error"]["class"], "permanent", "{body}");
    assert!(
        stderr.starts_with("zoder: error: review diff is"),
        "stderr must start with the oversize marker, got: {stderr:?}"
    );
}

#[test]
fn loop_without_engine_config_is_stable() {
    // The loop with an empty HOME fails closed: the config rung and a
    // typed `config` error envelope — never a silent exit 0.
    let scratch = Scratch::fresh("loop-no-config");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("create the home dir");
    let repo = dirty_repo(&scratch);

    let output = Command::new(zoder_binary())
        .arg("loop")
        .arg("--json")
        .arg("--max-iters")
        .arg("1")
        .arg("--model")
        .arg("fake-model")
        .arg("fix the typo")
        .current_dir(&repo)
        .env("ZODER_HOME", &home)
        .env("ZEROCLAW_CONFIG_DIR", &home)
        .env("ZODER_SKIP_CATALOG_ENRICHMENT", "1")
        .output()
        .expect("run zoder loop --json");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(EXIT_CONFIG),
        "stderr was: {stderr}"
    );
    let body = stdout_json(&output);
    assert_eq!(body["error"]["code"], "config", "{body}");
    assert_eq!(body["error"]["class"], "config", "{body}");
    // The marker is a LINE of its own, not the first bytes of stderr:
    // `zoder loop` may announce before it errors (the reap scope prints
    // "[zoder] left shared zeroclaw daemon pid=…" when a host-wide
    // daemon is spared), so asserting `starts_with` pinned the host's
    // state instead of the contract. Anchor the `zoder: error:` line.
    assert!(
        stderr
            .lines()
            .any(|line| line.starts_with("zoder: error: loading engine config: open corpus")),
        "stderr must carry the missing-corpus marker, got: {stderr:?}"
    );
}

#[test]
fn loop_without_agent_route_is_stable() {
    // The loop with zoder providers but no engine agent route for the
    // model: author turns cannot start, so the loop exits the
    // transient rung (20) with final_verdict reviewer_unavailable
    // and an author_error naming the missing route. A true `unresolved` needs a live engine
    // daemon (see the module docs), so this is the no-daemon pin.
    let scratch = Scratch::fresh("loop-no-agent");
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("create the home dir");
    write_home(&home, "http://127.0.0.1:1", &["fake-model"]);
    let repo = dirty_repo(&scratch);

    let output = Command::new(zoder_binary())
        .arg("loop")
        .arg("--json")
        .arg("--max-iters")
        .arg("1")
        .arg("--loop-timeout")
        .arg("960")
        .arg("--model")
        .arg("fake-model")
        .arg("fix the typo")
        .current_dir(&repo)
        .env("ZODER_HOME", &home)
        .env("ZEROCLAW_CONFIG_DIR", &home)
        .env("ZODER_SKIP_CATALOG_ENRICHMENT", "1")
        .output()
        .expect("run zoder loop --json");
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The loop ends `reviewer_unavailable` — class transient, rung 20.
    assert_eq!(
        output.status.code(),
        Some(EXIT_TRANSIENT),
        "stderr was: {stderr}"
    );
    let body = stdout_json(&output);
    assert_eq!(body["final_verdict"], "reviewer_unavailable");
    assert_eq!(body["resolved"], false);
    let log = body["log"].as_array().cloned().unwrap_or_default();
    let author_error = log
        .iter()
        .filter_map(|entry| entry.get("author_error").and_then(|v| v.as_str()))
        .next()
        .unwrap_or("");
    assert!(
        author_error.contains("is not configured on any zeroclaw agent"),
        "log must name the missing agent route, got: {author_error:?}"
    );
}

/// Wait for `path` to exist, up to `budget`.
fn wait_for_file(path: &Path, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    path.exists()
}

/// The stand-in engine daemon: the same shape the
/// `shared_daemon_lifecycle` tests use (copied so this file stays
/// self-contained). It speaks enough ACP JSON-RPC for the production
/// spawn path, keeps every turn "in progress" forever, and — the
/// point of the copy — NEVER acknowledges `session/cancel`, while
/// counting every cancel in `<state>/cancel_count`.
///
/// Its `config/get` advertises the `custom.fake` engine provider
/// profile and the `default` agent route the cancel test installs in
/// the engine TOML, so route resolution succeeds and the turn really
/// starts (and then hangs) instead of failing at the gate.
const SILENT_ZEROCLAW_PY: &str = r#"#!/usr/bin/env python3
import json, os, signal, socket, sys, threading, time

argv = sys.argv[1:]
SOCKET = None
STATE = None
MODEL = "fake-model"
i = 0
while i < len(argv):
    a = argv[i]
    if a == "--socket" and i + 1 < len(argv):
        SOCKET = argv[i + 1]
        i += 2
    elif a == "--state-dir" and i + 1 < len(argv):
        STATE = argv[i + 1]
        i += 2
    elif a == "--config-dir" and i + 1 < len(argv):
        cfg = argv[i + 1]
        SOCKET = os.path.join(cfg, "data", "daemon.sock")
        STATE = os.path.join(cfg, "data")
        i += 2
    else:
        i += 1

if SOCKET is None or STATE is None:
    sys.stderr.write("zeroclaw: no socket path given\n")
    sys.exit(2)

os.makedirs(STATE, exist_ok=True)
CANCEL_FILE = os.path.join(STATE, "cancel_count")


def handle_cancel():
    try:
        with open(CANCEL_FILE) as f:
            n = int(f.read().strip() or "0")
    except Exception:
        n = 0
    with open(CANCEL_FILE, "w") as f:
        f.write(str(n + 1) + "\n")


LOCK = threading.Lock()
CONNS = [0]


def on_term(signum, frame):
    os._exit(0)


signal.signal(signal.SIGTERM, on_term)

srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(SOCKET)
srv.listen(64)


def reply(conn, rid, result):
    try:
        conn.sendall((json.dumps({"jsonrpc": "2.0", "id": rid, "result": result}) + "\n").encode())
    except Exception:
        pass


def handle(conn, sid):
    buf = b""
    try:
        while True:
            data = conn.recv(65536)
            if not data:
                break
            buf += data
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                line = line.strip()
                if not line:
                    continue
                try:
                    frame = json.loads(line)
                except Exception:
                    continue
                method = frame.get("method", "")
                rid = frame.get("id")
                if method == "initialize":
                    reply(conn, rid, {"protocol_version": 1, "agent_capabilities": {}})
                elif method == "agents/status":
                    reply(conn, rid, {"agents": [{"alias": "default", "enabled": True,
                        "live_sessions": 0, "persisted_sessions": 0, "channels": []}]})
                elif method == "config/get":
                    reply(conn, rid, {
                        "agents": {"default": {"model": MODEL, "model_provider": "custom.fake"}},
                        "providers": {
                            "models": {"custom": {"fake": {"model": MODEL, "kind": "openai",
                                "uri": "http://127.0.0.1:1"}}}
                        },
                        "acp": {"default_agent": "default"},
                    })
                elif method == "session/new":
                    reply(conn, rid, {"session_id": sid})
                elif method == "session/prompt":
                    reply(conn, rid, {})
                    try:
                        conn.sendall((json.dumps({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": {"session_id": sid,
                                        "type": "agent_message_chunk",
                                        "update": {"sessionUpdate": "agent_message_chunk",
                                                    "content": {"type": "text",
                                                                "text": "fake turn"}}}}) + "\n").encode())
                    except Exception:
                        pass
                elif method == "session/cancel":
                    # Count but do NOT acknowledge: the unconfirmed-cancel shape.
                    with LOCK:
                        handle_cancel()
    except Exception:
        pass
    finally:
        try:
            conn.close()
        except Exception:
            pass


def accept_loop():
    while True:
        try:
            conn, _ = srv.accept()
        except OSError:
            return
        with LOCK:
            CONNS[0] += 1
            n = CONNS[0]
        threading.Thread(target=handle, args=(conn, "sess-" + str(n)), daemon=True).start()


threading.Thread(target=accept_loop, daemon=True).start()
while True:
    time.sleep(3600)
"#;

/// Write the engine TOML (`$ZEROCLAW_CONFIG_DIR/config.toml`) the
/// rescue run needs: the `default` agent route for `fake-model`
/// through the `custom.fake` engine provider profile. `custom` (not
/// `fake`) is the implementation family so the transport resolves to
/// `openai-chat` instead of failing the mapping check.
fn write_engine_toml(cfg_dir: &Path) {
    let body = "[agents.default]\n\
          model = \"fake-model\"\n\
          model_provider = \"custom.fake\"\n\
          \n\
          [providers.models.custom.fake]\n\
          uri = \"http://127.0.0.1:1\"\n\
          model = \"fake-model\"\n";
    std::fs::write(cfg_dir.join("config.toml"), body).expect("write engine config.toml");
}

/// Add the `engine_provider_ref` binding the zoder-side `fake`
/// provider to the `custom.fake` engine profile, so route
/// verification accepts the rescue run's model pin.
fn bind_engine_provider_ref(home: &Path) {
    let path = home.join("config.json");
    let raw = std::fs::read_to_string(&path).expect("read config.json");
    let mut body: serde_json::Value = serde_json::from_str(&raw).expect("parse config.json");
    body["providers"][0]["engine_provider_ref"] =
        serde_json::Value::String("custom.fake".to_owned());
    std::fs::write(&path, body.to_string()).expect("rewrite config.json");
}

#[test]
fn unconfirmed_cancel_rescue_is_stable() {
    // A daemon that never acknowledges session/cancel, plus a rescue
    // whose agent budget elapses: the run must fail closed (exit 1,
    // "rescue ended: failed") with the cancel OBSERVED by the daemon
    // (`cancel_count >= 1`), instead of hanging or exiting clean.
    //
    // Platform note: the reaper's "cancel not confirmed after N
    // attempt(s)" line is Linux-only — `ReapPlan::new` needs
    // `/proc/uptime` (`boot_ticks`), which returns `None` off Linux,
    // so on macOS the reap scope is inert and only the turn's own
    // watchdog cancel fires. The assertions below are the portable
    // subset that holds on both: fail-closed exit, delivered cancel.
    // The reaper line itself is pinned by `reaper.rs` unit tests and
    // the `shared_daemon_lifecycle` suite where the platform allows.
    let scratch = Scratch::fresh("cancel");
    let bin_dir = scratch.path().join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create the bin dir");
    let fake = bin_dir.join("zeroclaw");
    std::fs::write(&fake, SILENT_ZEROCLAW_PY).expect("write the stand-in zeroclaw");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&fake)
            .expect("stat the stand-in zeroclaw")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake, perms).expect("chmod the stand-in");
    }
    // Short layout: `<scratch>/cf/data/daemon.sock` must stay under the
    // ~108-byte AF_UNIX limit, and HOME must point at the scratch so the
    // sibling lookup (`exe_dir`, then `~/.local/bin`, then `$PATH`) finds
    // the stand-in instead of a real installed `zeroclaw`.
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).expect("create the home dir");
    std::fs::create_dir_all(home.join("engine")).expect("create the engine dir");
    std::fs::write(home.join("engine").join("config.toml"), "").expect("write engine config");
    write_home(&home, "http://127.0.0.1:1", &["fake-model"]);
    bind_engine_provider_ref(&home);
    let cfg_dir = scratch.path().join("cf");
    std::fs::create_dir_all(cfg_dir.join("data")).expect("create the cfg dir");
    write_engine_toml(&cfg_dir);

    let output = Command::new(zoder_binary())
        .arg("rescue")
        .arg("-m")
        .arg("fake-model")
        .arg("--agent-timeout")
        .arg("2")
        .arg("investigate the stub failure")
        .env("ZODER_HOME", &home)
        .env("ZEROCLAW_CONFIG_DIR", &cfg_dir)
        .env("HOME", scratch.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin_dir.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .expect("run zoder rescue");
    let stderr = String::from_utf8_lossy(&output.stderr);
    // `rescue` is outside the contract scope: it keeps the historical 1.
    assert_eq!(
        output.status.code(),
        Some(EXIT_UNCONTRACTED),
        "stderr was: {stderr}"
    );
    assert!(
        stderr.contains("rescue ended: failed"),
        "stderr must carry the fail-closed marker, got: {stderr:?}"
    );
    let cancel_count_path = cfg_dir.join("data").join("cancel_count");
    assert!(
        wait_for_file(&cancel_count_path, Duration::from_secs(10)),
        "the daemon must have seen the cancel; stderr was: {stderr:?}"
    );
    let count: u64 = std::fs::read_to_string(&cancel_count_path)
        .expect("read cancel_count")
        .trim()
        .parse()
        .expect("cancel_count is a number");
    assert!(
        count >= 1,
        "the turn's watchdog cancel must reach the daemon, got count={count}"
    );
}
