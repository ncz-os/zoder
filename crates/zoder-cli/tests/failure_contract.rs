//! End-to-end guards for the machine-readable failure contract of
//! `zoder review` and `zoder loop`.
//!
//! These run the real binary against an isolated `$ZODER_HOME`, so they pin
//! the OPERATOR-VISIBLE contract: the exit code, the stable class code, the
//! retryable flag and the human message. A failure that is only classified
//! in the source but surfaces as a bare `1` on the command line is a broken
//! contract, and these tests catch it.
//!
//! The vocabulary and its exit codes are documented in `docs/EXIT_CODES.md`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The exit codes this contract assigns (decision D3): 0 success, 10 blocked,
/// 20 transient, 30 budget, 40 config, 50 permanent/internal, 75 reserved for
/// the oneshot band. They must match `docs/EXIT_CODES.md` and
/// `crates/zoder-cli/src/failure.rs`.
mod exit_code {
    pub const SUCCESS: i32 = 0;
    /// `blocked`: the review completed and refused the change.
    pub const BLOCKED: i32 = 10;
    /// `transient`: retry the same invocation.
    pub const TRANSIENT: i32 = 20;
    /// `budget`: wait for the window to reset.
    pub const BUDGET: i32 = 30;
    /// `config`: an operator must change something.
    pub const CONFIG: i32 = 40;
    /// `permanent` / `internal`: nothing to fix, nothing to retry. Also the
    /// code an UNCLASSIFIED failure exits with.
    pub const UNCLASSIFIED: i32 = 50;
    /// Reserved for the oneshot refusal band; never a review/loop code.
    pub const RESERVED_ONESHOT: i32 = 75;
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

fn zoder(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zoder"));
    cmd.env("ZODER_HOME", home)
        .env("ZEROCLAW_CONFIG_DIR", home)
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("MINIMAX_API_KEY")
        .env_remove("ZODER_API_KEY")
        // No daemon: the tests must never depend on a running engine.
        .env("ZEROCLAW_SOCKET", home.join("no-such-daemon.sock"));
    cmd
}

fn run(cmd: &mut Command) -> Output {
    cmd.output().expect("spawn zoder")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn exit_code(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("the process must exit with a code")
}

/// A git repository with one committed file and one uncommitted change, so
/// `zoder review` has a real diff to work with.
fn seed_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).expect("create repo");
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.name", "Jason Perlow"]);
    git(&["config", "user.email", "jperlow@gmail.com"]);
    fs::write(repo.join("src.txt"), "one\n").expect("write file");
    git(&["add", "src.txt"]);
    git(&["commit", "-q", "-m", "seed"]);
    fs::write(repo.join("src.txt"), "one\ntwo\n").expect("edit file");
    repo
}

/// The class code the JSON envelope carries, from either the `failure`
/// object (inside a result) or the `error` object (no result).
fn json_class(output: &Output) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_str(&stdout(output)).ok()?;
    doc.get("failure")
        .or_else(|| doc.get("error"))
        .and_then(|f| f.get("class"))
        .and_then(|c| c.as_str())
        .map(str::to_string)
}

/// The stable code the JSON envelope carries. It is the SPECIFIC failure a
/// caller dispatches on; `class` is only the retry policy.
fn json_code(output: &Output) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_str(&stdout(output)).ok()?;
    doc.get("failure")
        .or_else(|| doc.get("error"))
        .and_then(|f| f.get("code"))
        .and_then(|c| c.as_str())
        .map(str::to_string)
}

/// A valid `config.json` plus `model_corpus.json` for `$ZODER_HOME`, so a run
/// gets PAST config loading and can fail on the engine itself. Without this a
/// run dies at `loading engine config` and every failure looks like `config`.
///
/// The single provider points at `base_url` (a `wiremock` server in the
/// success test, a dead port otherwise).
fn seed_home_with_provider(dir: &Path, base_url: &str) {
    let corpus = serde_json::json!({
        "source": "test",
        "models": [{"id": "seed-model/one", "free": true, "routable": true}],
    });
    fs::write(
        dir.join("model_corpus.json"),
        serde_json::to_string(&corpus).expect("serialize corpus"),
    )
    .expect("write corpus");
    let config = serde_json::json!({
        "providers": [{
            "id": "seed",
            "base_url": base_url,
            "kind": "openai-chat",
            "auth": {"type": "none"},
            "billing": "free",
            "serves": ["seed-model/"],
        }],
        "default_provider": "seed",
        "strict_free": false,
        "corpus_path": dir.join("model_corpus.json"),
        "ledger_path": dir.join("ledger.jsonl"),
        "health_path": dir.join("health.json"),
        "reviewer_model": "seed-model/one",
    });
    fs::write(
        dir.join("config.json"),
        serde_json::to_string(&config).expect("serialize config"),
    )
    .expect("write config");
}

/// [`seed_home_with_provider`] pointed at a dead local port: config loading
/// succeeds, every provider call fails.
fn seed_home(dir: &Path) {
    seed_home_with_provider(dir, "http://127.0.0.1:9/v1");
}

/// Whether the envelope lets a caller retry as-is. The envelope never
/// publishes a `retryable` key: it is DERIVED from the class, and only the
/// `transient` class is retryable. `None` when no class is present.
fn json_retryable(output: &Output) -> Option<bool> {
    json_class(output).map(|class| class == "transient")
}

fn json_message(output: &Output) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_str(&stdout(output)).ok()?;
    doc.get("failure")
        .or_else(|| doc.get("error"))
        .and_then(|f| f.get("detail"))
        .and_then(|d| d.get("message"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
}

/// The class a named code carries, per the published table. `None` for a code
/// this test does not know, so a new code is not silently accepted.
fn documented_class(code: &str) -> Option<&'static str> {
    Some(match code {
        "review_blocked" => "blocked",
        "reviewers_unavailable"
        | "author_failed"
        | "author_timeout"
        | "engine_unreachable"
        | "cancel_unconfirmed"
        | "worktree_tainted" => "transient",
        "diff_too_large" | "diff_truncated" | "empty_diff" | "loop_exhausted" | "internal" => {
            "permanent"
        }
        "no_reviewer_model"
        | "reviewer_budget_exhausted"
        | "policy_violation"
        | "config"
        | "usage" => "config",
        "engine_budget_exceeded" => "budget",
        _ => return None,
    })
}

/// The class the envelope's `code` carries, checked against the published
/// table. Panics when the two disagree, which is the contract violation a
/// caller would otherwise discover in production.
fn carrier_class(output: &Output) -> String {
    let code = json_code(output).expect("the JSON must carry a code");
    let class = json_class(output).expect("the JSON must carry a class");
    let documented =
        documented_class(&code).unwrap_or_else(|| panic!("undocumented failure code {code:?}"));
    assert_eq!(
        class, documented,
        "code {code:?} must carry class {documented:?}, envelope says {class:?}"
    );
    class
}

/// A `zoder review` whose diff is too large to review: the per-diff byte cap
/// is 1 byte, so the refusal is deterministic and needs no provider.
///
/// The code is `diff_too_large`, its class is `permanent` (the same
/// invocation fails identically) and the exit code is 50 — the
/// `permanent`/`internal` rung of the D3 ladder.
#[test]
fn a_diff_that_cannot_be_reviewed_is_classified_diff_too_large() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seed_repo(dir.path());

    let output = run(zoder(dir.path()).current_dir(&repo).args([
        "--json",
        "review",
        "--max-diff-bytes",
        "1",
    ]));

    assert_eq!(
        exit_code(&output),
        exit_code::UNCLASSIFIED,
        "a diff above the cap must exit {}\nstdout:\n{}\nstderr:\n{}",
        exit_code::UNCLASSIFIED,
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        json_code(&output).as_deref(),
        Some("diff_too_large"),
        "the JSON must name the diff_too_large code\nstdout:\n{}",
        stdout(&output)
    );
    assert_eq!(
        json_class(&output).as_deref(),
        Some("permanent"),
        "diff_too_large carries the permanent class\nstdout:\n{}",
        stdout(&output)
    );
    let message = json_message(&output).unwrap_or_default();
    assert!(
        message.contains("byte") || message.contains("above the"),
        "the message must explain the size refusal: {message:?}"
    );
}

/// A run that cannot start because the environment is misconfigured must be
/// classified `config` (exit 40).
///
/// A `config.json` that is not JSON makes the engine unloadable. `zoder loop`
/// treats an unloadable engine as fatal (its exec-safety policy comes from
/// that config), which is the configuration failure the contract names.
/// `zoder review` deliberately degrades to built-in defaults instead of
/// failing here, so it is not the command under test.
#[test]
fn a_broken_config_is_classified_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("config.json"), "{ this is not json").expect("write broken config");
    let repo = seed_repo(dir.path());

    let output = run(zoder(dir.path()).current_dir(&repo).args([
        "--json",
        "loop",
        "--max-iters",
        "1",
        "do something",
    ]));

    assert_eq!(
        exit_code(&output),
        exit_code::CONFIG,
        "a broken config must exit {}\nstdout:\n{}\nstderr:\n{}",
        exit_code::CONFIG,
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        json_class(&output).as_deref(),
        Some("config"),
        "the JSON must name the config class\nstdout:\n{}",
        stdout(&output)
    );
    assert_eq!(
        json_code(&output).as_deref(),
        Some("config"),
        "the JSON must name the config code\nstdout:\n{}",
        stdout(&output)
    );
    // `config` is the operator-fix class, so it is NOT retryable as-is.
    assert_eq!(
        json_retryable(&output),
        Some(false),
        "config is not retryable"
    );
}

/// An unreachable engine must NOT be reported as a success, and must carry a
/// class the caller can act on.
///
/// The config is valid (so the run gets past config loading) and the socket
/// points at a path nothing listens on, so the engine itself cannot be
/// reached. The published code is `engine_unreachable` (class `transient`,
/// exit 20), and this test pins the observed behaviour: whatever the loop
/// records for the unreachable turn, the run is classified, carries a code and
/// a message, and never exits 0.
#[test]
fn an_unreachable_engine_is_classified_and_never_approves() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed_home(dir.path());
    let repo = seed_repo(dir.path());

    // A loop whose author turn cannot reach the engine at all.
    let output = run(zoder(dir.path()).current_dir(&repo).args([
        "--json",
        "loop",
        "--max-iters",
        "1",
        "do something",
    ]));

    let code = exit_code(&output);
    assert_ne!(
        code,
        exit_code::SUCCESS,
        "an unreachable engine must not exit 0\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    // Every classified failure on the ladder is one of the documented rungs,
    // and 75 is reserved for the oneshot band.
    assert!(
        [
            exit_code::BLOCKED,
            exit_code::TRANSIENT,
            exit_code::BUDGET,
            exit_code::CONFIG,
            exit_code::UNCLASSIFIED,
        ]
        .contains(&code),
        "an unreachable engine must exit a documented rung, got {code}\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    // The reserved oneshot band is never a review/loop exit code.
    assert_ne!(
        code,
        exit_code::RESERVED_ONESHOT,
        "exit {} is reserved for the oneshot refusal band and must never be a review/loop code\nstdout:\n{}\nstderr:\n{}",
        exit_code::RESERVED_ONESHOT,
        stdout(&output),
        stderr(&output)
    );
    let carrier = json_code(&output).expect("the JSON must carry a code");
    // The class the envelope claims must be the class the published table
    // assigns to that code.
    let class = carrier_class(&output);
    let message = json_message(&output).unwrap_or_default();
    assert!(
        !message.is_empty(),
        "a classified failure must carry a message\nstdout:\n{}",
        stdout(&output)
    );
    // The class must be one the contract defines.
    assert!(
        ["transient", "permanent", "config", "budget", "blocked"].contains(&class.as_str()),
        "unexpected class {class:?} for code {carrier:?}\nstdout:\n{}",
        stdout(&output)
    );
}

/// The `--json` output must be EXACTLY ONE JSON document: a consumer parses
/// stdout as a single value. A failure must not append a second document.
#[test]
fn a_json_failure_prints_exactly_one_json_document() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seed_repo(dir.path());

    let output = run(zoder(dir.path()).current_dir(&repo).args([
        "--json",
        "review",
        "--max-diff-bytes",
        "1",
    ]));

    let text = stdout(&output);
    let mut stream = serde_json::Deserializer::from_str(&text).into_iter::<serde_json::Value>();
    let first = stream.next().expect("stdout must carry a JSON document");
    assert!(first.is_ok(), "stdout must be parseable JSON: {text:?}");
    assert!(
        stream.next().is_none(),
        "stdout must carry exactly ONE JSON document, got a second: {text:?}"
    );
}

/// Parse stdout as exactly ONE JSON document (the `--json` contract).
fn single_json_document(output: &Output) -> serde_json::Value {
    let text = stdout(output);
    let mut stream = serde_json::Deserializer::from_str(&text).into_iter::<serde_json::Value>();
    let first = stream
        .next()
        .unwrap_or_else(|| panic!("stdout must carry a JSON document: {text:?}"))
        .unwrap_or_else(|e| panic!("stdout must be parseable JSON ({e}): {text:?}"));
    assert!(
        stream.next().is_none(),
        "stdout must carry exactly ONE JSON document: {text:?}"
    );
    first
}

/// Review (Muse + DeepSeek, high): an EMPTY diff is its own failure,
/// `empty_diff`, and the result document must say so in its `failure`
/// member. Before the fix the result carried no `failure` at all (only the
/// legacy `error` string), so a `--json` caller could not read the code.
#[test]
fn an_empty_diff_is_classified_empty_diff_in_the_result() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seed_repo(dir.path());
    // Commit the pending edit so the tree is clean and `--base HEAD` is an
    // empty range: nothing was truncated, there is simply nothing to review.
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["commit", "-qam", "edit"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let output = run(zoder(dir.path())
        .current_dir(&repo)
        .args(["--json", "review", "--base", "HEAD"]));

    assert_eq!(
        exit_code(&output),
        exit_code::UNCLASSIFIED,
        "an empty diff exits the permanent rung\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let doc = single_json_document(&output);
    assert_eq!(
        doc["failure"]["code"], "empty_diff",
        "the result must carry the empty_diff failure: {doc}"
    );
    assert_ne!(doc["failure"]["code"], "diff_truncated", "{doc}");
    assert_eq!(doc["failure"]["class"], "permanent", "{doc}");
    assert_eq!(carrier_class(&output), "permanent");
    // The legacy fields keep their meaning.
    assert_eq!(doc["result"], "empty_diff", "{doc}");
    assert_eq!(doc["empty_diff"], true, "{doc}");
}

/// Review (Muse + DeepSeek, high): a diff genuinely cut off during
/// acquisition is `diff_truncated`, and that classification reaches the
/// emitted result document (and the exit code) intact.
#[test]
fn a_truncated_diff_is_classified_diff_truncated_in_the_result() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seed_repo(dir.path());
    // Well over the 1 MiB acquisition cap, as a TRACKED-file edit so the
    // working-tree diff carries it.
    let mut body = String::from("one\n");
    for i in 0..60_000 {
        body.push_str(&format!("line-{i:05}-padding-padding-padding\n"));
    }
    fs::write(repo.join("src.txt"), body).expect("write big edit");

    let output = run(zoder(dir.path()).current_dir(&repo).args([
        "--json",
        "review",
        "--scope",
        "working-tree",
        "--max-diff-bytes",
        "200000000",
    ]));

    assert_eq!(
        exit_code(&output),
        exit_code::UNCLASSIFIED,
        "a truncated diff exits the permanent rung\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let doc = single_json_document(&output);
    assert_eq!(
        doc["failure"]["code"], "diff_truncated",
        "the result must carry the diff_truncated failure: {doc}"
    );
    assert_eq!(carrier_class(&output), "permanent");
    assert!(
        doc["failure"]["detail"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("truncated during acquisition")),
        "{doc}"
    );
}

/// A successful review must NOT carry a `failure` field: the field is
/// additive and appears only on a failure.
#[tokio::test]
async fn a_successful_review_carries_no_failure_field() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seed_repo(dir.path());

    // A provider that returns a valid APPROVE verdict, so the review really
    // completes and really succeeds.
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "mock",
            "object": "chat.completion",
            "created": 0,
            "model": "seed-model/one",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "{\"verdict\":\"approve\",\"summary\":\"No concrete defects.\",\"findings\":[],\"next_steps\":[]}",
                },
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
        })))
        .mount(&provider)
        .await;
    seed_home_with_provider(dir.path(), &provider.uri());

    let output = run(zoder(dir.path())
        .current_dir(&repo)
        .args(["--json", "review"]));

    let doc: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap_or_else(|e| {
        panic!(
            "stdout must be parseable JSON ({e}): {:?}\nstderr:\n{}",
            stdout(&output),
            stderr(&output)
        )
    });
    assert_eq!(
        exit_code(&output),
        exit_code::SUCCESS,
        "an approved review must exit 0\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert!(
        doc.get("failure").is_none(),
        "a successful run must not carry a failure field: {doc}"
    );
    // The pre-existing fields are unchanged.
    assert!(
        doc.get("verdict").is_some(),
        "the verdict field must remain"
    );
    assert!(
        doc.get("complete").is_some(),
        "the complete field must remain"
    );
    assert!(
        doc.get("reviewers").is_some(),
        "the reviewers field must remain"
    );
    assert!(
        doc.get("cost_usd").is_some(),
        "the cost_usd field must remain"
    );
    assert_eq!(
        doc.get("verdict").and_then(|v| v.as_str()),
        Some("approve"),
        "the mock approved the diff: {doc}"
    );
}

/// The docs and the implementation must agree on the vocabulary: every class,
/// every named code and every rung of the D3 ladder.
#[test]
fn the_exit_code_document_lists_every_class_and_code() {
    let doc =
        fs::read_to_string(repo_root().join("docs/EXIT_CODES.md")).expect("read EXIT_CODES.md");
    for class in ["transient", "permanent", "config", "budget", "blocked"] {
        assert!(
            doc.contains(class),
            "docs/EXIT_CODES.md must document the {class} class"
        );
    }
    for code in [
        "review_blocked",
        "reviewers_unavailable",
        "diff_too_large",
        "diff_truncated",
        "empty_diff",
        "no_reviewer_model",
        "loop_exhausted",
        "reviewer_budget_exhausted",
        "author_failed",
        "author_timeout",
        "policy_violation",
        "engine_budget_exceeded",
        "engine_unreachable",
        "cancel_unconfirmed",
        "worktree_tainted",
        "config",
        "usage",
        "internal",
    ] {
        assert!(
            doc.contains(&format!("`{code}`")),
            "docs/EXIT_CODES.md must document the {code} code"
        );
    }
    // The published ladder: 0 success and 10/20/30/40/50 classified, 75
    // reserved. The retired 64..=72 band must be gone.
    for rung in ["`0`", "`10`", "`20`", "`30`", "`40`", "`50`", "`75`"] {
        assert!(
            doc.contains(rung),
            "docs/EXIT_CODES.md must publish the {rung} rung"
        );
    }
}
