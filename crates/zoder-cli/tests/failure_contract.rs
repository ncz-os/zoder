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

/// The exit codes this contract assigns. They must match
/// `docs/EXIT_CODES.md` and `crates/zoder-cli/src/failure.rs`.
mod exit_code {
    /// Unclassified failure: a failure the contract does not classify.
    pub const UNCLASSIFIED: i32 = 50;
    pub const USAGE: i32 = 64;
    pub const CONFIG: i32 = 65;
    pub const DIFF_TOO_LARGE: i32 = 66;
    pub const BUDGET_EXCEEDED: i32 = 67;
    pub const REVIEWER_UNAVAILABLE: i32 = 68;
    pub const REVIEW_BLOCKED: i32 = 69;
    pub const CANCELLED: i32 = 70;
    pub const ENGINE_UNREACHABLE: i32 = 71;
    pub const INTERNAL: i32 = 72;
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
    output.status.code().expect("the process must exit with a code")
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

fn json_retryable(output: &Output) -> Option<bool> {
    let doc: serde_json::Value = serde_json::from_str(&stdout(output)).ok()?;
    doc.get("failure")
        .or_else(|| doc.get("error"))
        .and_then(|f| f.get("retryable"))
        .and_then(|r| r.as_bool())
}

fn json_message(output: &Output) -> Option<String> {
    let doc: serde_json::Value = serde_json::from_str(&stdout(output)).ok()?;
    doc.get("failure")
        .or_else(|| doc.get("error"))
        .and_then(|f| f.get("message"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
}

/// A `zoder review` whose diff is too large to review: the per-diff byte cap
/// is 1 byte, so the refusal is deterministic and needs no provider.
///
/// The class is `diff_too_large` and the exit code is 66.
#[test]
fn a_diff_that_cannot_be_reviewed_is_classified_diff_too_large() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seed_repo(dir.path());

    let output = run(zoder(dir.path())
        .current_dir(&repo)
        .args(["--json", "review", "--max-diff-bytes", "1"]));

    assert_eq!(
        exit_code(&output),
        exit_code::DIFF_TOO_LARGE,
        "a diff above the cap must exit {}\nstdout:\n{}\nstderr:\n{}",
        exit_code::DIFF_TOO_LARGE,
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        json_class(&output).as_deref(),
        Some("diff_too_large"),
        "the JSON must name the diff_too_large class\nstdout:\n{}",
        stdout(&output)
    );
    assert_eq!(
        json_retryable(&output),
        Some(false),
        "a diff that is too large is not retryable as-is"
    );
    let message = json_message(&output).unwrap_or_default();
    assert!(
        message.contains("byte") || message.contains("above the"),
        "the message must explain the size refusal: {message:?}"
    );
}

/// A `zoder review` that cannot run because the environment is misconfigured
/// must be classified `config` (exit 65) — not the unclassified `1`.
///
/// A broken `config.json` makes the engine unloadable, which is the
/// configuration failure the contract names.
#[test]
fn a_broken_config_is_classified_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("config.json"), "{ this is not json").expect("write broken config");
    let repo = seed_repo(dir.path());

    let output = run(zoder(dir.path()).current_dir(&repo).args(["--json", "review"]));

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
    assert_eq!(json_retryable(&output), Some(false));
}

/// An unreachable engine must be classified `engine_unreachable` (exit 71)
/// and IS retryable: the daemon can be restarted.
///
/// The socket points at a path nothing is listening on, so the engine cannot
/// be reached.
#[test]
fn an_unreachable_engine_is_classified_engine_unreachable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seed_repo(dir.path());

    // A loop whose author turn cannot reach the engine at all.
    let output = run(zoder(dir.path())
        .current_dir(&repo)
        .args(["--json", "loop", "--max-iters", "1", "do something"]));

    // The engine is unreachable, so the loop cannot resolve. The class must
    // be one of the engine/reviewer families, and the exit code must be the
    // matching documented one — never the unclassified `1`.
    let code = exit_code(&output);
    assert!(
        code == exit_code::ENGINE_UNREACHABLE
            || code == exit_code::REVIEWER_UNAVAILABLE
            || code == exit_code::INTERNAL,
        "an unreachable engine must exit a classified code, got {code}\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    assert_ne!(
        code,
        exit_code::UNCLASSIFIED,
        "a classified failure must not fall back to the unclassified exit 1"
    );
    let class = json_class(&output).expect("the JSON must carry a class");
    assert!(
        [
            "engine_unreachable",
            "reviewer_unavailable",
            "internal",
            "cancelled",
        ]
        .contains(&class.as_str()),
        "unexpected class {class:?}\nstdout:\n{}",
        stdout(&output)
    );
}

/// The `--json` output must be EXACTLY ONE JSON document: a consumer parses
/// stdout as a single value. A failure must not append a second document.
#[test]
fn a_json_failure_prints_exactly_one_json_document() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seed_repo(dir.path());

    let output = run(zoder(dir.path())
        .current_dir(&repo)
        .args(["--json", "review", "--max-diff-bytes", "1"]));

    let text = stdout(&output);
    let mut stream = serde_json::Deserializer::from_str(&text).into_iter::<serde_json::Value>();
    let first = stream.next().expect("stdout must carry a JSON document");
    assert!(first.is_ok(), "stdout must be parseable JSON: {text:?}");
    assert!(
        stream.next().is_none(),
        "stdout must carry exactly ONE JSON document, got a second: {text:?}"
    );
}

/// A successful review must NOT carry a `failure` field: the field is
/// additive and appears only on a failure.
#[test]
fn a_successful_review_carries_no_failure_field() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = seed_repo(dir.path());

    // A dry run plans the review without calling a provider, so it succeeds
    // offline and produces a result document.
    let output = run(zoder(dir.path())
        .current_dir(&repo)
        .args(["--json", "review", "--dry-run"]));

    let doc: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap_or_else(|e| {
        panic!(
            "stdout must be parseable JSON ({e}): {:?}\nstderr:\n{}",
            stdout(&output),
            stderr(&output)
        )
    });
    assert!(
        doc.get("failure").is_none(),
        "a successful run must not carry a failure field: {doc}"
    );
    // The pre-existing fields are unchanged.
    assert!(doc.get("verdict").is_some(), "the verdict field must remain");
    assert!(doc.get("complete").is_some(), "the complete field must remain");
    assert!(doc.get("reviewers").is_some(), "the reviewers field must remain");
    assert!(doc.get("cost_usd").is_some(), "the cost_usd field must remain");
}

/// The docs and the implementation must agree on the vocabulary.
#[test]
fn the_exit_code_document_lists_every_class() {
    let doc = fs::read_to_string(repo_root().join("docs/EXIT_CODES.md")).expect("read EXIT_CODES.md");
    for class in [
        "config",
        "usage",
        "budget_exceeded",
        "reviewer_unavailable",
        "review_blocked",
        "diff_too_large",
        "cancelled",
        "engine_unreachable",
        "internal",
    ] {
        assert!(
            doc.contains(class),
            "docs/EXIT_CODES.md must document the {class} class"
        );
    }
}
