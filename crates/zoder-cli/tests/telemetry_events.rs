//! End-to-end guards for zoder's structured telemetry (ZD-10).
//!
//! With `ZODER_LOG_FORMAT=json` a `zoder loop` writes one JSON object per
//! event to stderr, including one `zoder.turn/1` event per author turn and
//! one `zoder.run/1` event per process. In the default human format nothing
//! on stderr is JSON and the operator text is unchanged.
//!
//! The loop runs against an isolated `$ZODER_HOME` whose engine socket does
//! not exist, so the author turn fails without a daemon and without a model
//! call: offline and deterministic.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn zoder(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zoder"));
    cmd.env("ZODER_HOME", home)
        .env("ZEROCLAW_CONFIG_DIR", home)
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("MINIMAX_API_KEY")
        .env_remove("ZODER_API_KEY")
        .env_remove("RUST_LOG")
        .env_remove("ZODER_LOG_FORMAT")
        // No daemon: the tests must never depend on a running engine.
        .env("ZEROCLAW_SOCKET", home.join("no-such-daemon.sock"));
    cmd
}

/// A git repository with one committed file and one uncommitted change.
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

/// A valid config and corpus whose only provider is a dead local port, so the
/// run gets past config loading and fails on the engine.
fn seed_home(dir: &Path) {
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
            "base_url": "http://127.0.0.1:9/v1",
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

fn run_loop(format: Option<&str>) -> Output {
    let dir = tempfile::tempdir().expect("tempdir");
    seed_home(dir.path());
    let repo = seed_repo(dir.path());
    let mut cmd = zoder(dir.path());
    if let Some(format) = format {
        cmd.env("ZODER_LOG_FORMAT", format);
    }
    cmd.current_dir(&repo)
        .args(["loop", "--max-iters", "1", "do something"])
        .output()
        .expect("spawn zoder")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The stderr lines that parse as JSON objects.
fn json_objects(output: &Output) -> Vec<serde_json::Value> {
    stderr(output)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(serde_json::Value::is_object)
        .collect()
}

#[test]
fn json_log_format_emits_turn_and_run_events() {
    let output = run_loop(Some("json"));
    let err = stderr(&output);
    let objects = json_objects(&output);
    let turn = objects
        .iter()
        .find(|o| o["schema"] == "zoder.turn/1")
        .unwrap_or_else(|| panic!("no zoder.turn/1 event on stderr:\n{err}"));
    assert_eq!(turn["target"], "zoder::turn", "{turn}");
    assert_eq!(turn["iteration"], 1, "{turn}");
    assert_eq!(turn["phase"], "author", "{turn}");
    // No daemon: the author turn errors before a session exists.
    assert_eq!(turn["outcome"], "error", "{turn}");
    assert_eq!(turn["cancel_outcome"], "no_session", "{turn}");
    assert!(
        turn["elapsed_ms"].is_u64(),
        "elapsed_ms must be a number: {turn}"
    );
    assert!(turn["estimated_cost_usd"].is_null(), "{turn}");
    assert!(turn["billed_cost_usd"].is_null(), "{turn}");

    let runs: Vec<_> = objects
        .iter()
        .filter(|o| o["schema"] == "zoder.run/1")
        .collect();
    assert_eq!(runs.len(), 1, "exactly one zoder.run/1 event:\n{err}");
    let run = runs[0];
    assert_eq!(run["target"], "zoder::run", "{run}");
    assert_eq!(run["command"], "loop", "{run}");
    let code = output.status.code().expect("exit code");
    assert_ne!(code, 0, "an unreachable engine must not exit 0:\n{err}");
    assert_eq!(run["exit_code"], code, "{run}");
    assert!(run["duration_ms"].is_u64(), "{run}");
    assert!(
        run["error"].is_string(),
        "a failed run names its error: {run}"
    );
    assert_eq!(run["deadline_hit"], false, "{run}");
    for object in &objects {
        assert!(
            object["ts"].is_string(),
            "every object carries ts: {object}"
        );
        assert!(
            object["level"].is_string(),
            "every object carries level: {object}"
        );
    }
}

#[test]
fn human_log_format_keeps_the_prefixed_text() {
    let output = run_loop(None);
    let err = stderr(&output);
    assert!(
        json_objects(&output).is_empty(),
        "human mode must not write JSON to stderr:\n{err}"
    );
    assert!(
        err.contains("\n[loop] iter 1/1: author…\n"),
        "the operator line must be unchanged:\n{err}"
    );
}

#[test]
fn an_unknown_log_format_keeps_human_text() {
    let output = run_loop(Some("yaml"));
    let err = stderr(&output);
    assert!(
        json_objects(&output).is_empty(),
        "an unknown format must never switch to JSON:\n{err}"
    );
    assert!(err.contains("[loop] iter 1/1: author…"), "{err}");
}
