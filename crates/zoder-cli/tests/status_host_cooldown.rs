//! End-to-end guards for `zoder status` reporting a host cooldown (D8 step 12).
//!
//! These run the REAL binary against an isolated `$ZODER_HOME`, so they pin the
//! operator-visible contract:
//!
//!  - with a cooldown marker beside the ledger, `status --json` emits ONE JSON
//!    document that carries `host_cooldown` — nothing is printed before it, so
//!    a caller can pipe the output straight into a JSON parser;
//!  - without `--json` the human status line is printed instead of the JSON.
//!
//! The bug these guard: `cmd_status` called `print_host_cooldown(false)`
//! unconditionally, so `--json` mode printed the human `[zoder] ...` line to
//! stdout FIRST and the JSON payload second. `json.loads(stdout)` then failed
//! on a document the operator had explicitly asked to be machine-readable.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// Write a live cooldown marker beside the ledger in `home`.
///
/// The marker format is documented in `docs/HOST_COOLDOWN.md`; this writes the
/// same shape the budget gate writes, far enough in the future that it is
/// unambiguously active for the duration of the test.
fn write_marker(home: &Path) {
    fs::create_dir_all(home).expect("create home");
    let marker = serde_json::json!({
        "version": 1,
        "reason": "monthly_spend_cap",
        "detail": "monthly spend cap reached: $12.00 spent of $10.00 cap",
        "limit_usd": 10.0,
        "spent_usd": 12.0,
        "created_at": "2026-11-01T12:00:00Z",
        "expires_at": "2099-01-01T00:00:00Z",
    });
    fs::write(
        home.join("ledger.jsonl.cooldown.json"),
        serde_json::to_vec_pretty(&marker).expect("serialize marker"),
    )
    .expect("write marker");
}

fn status(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zoder"))
        .env("ZODER_HOME", home)
        .env("ZEROCLAW_CONFIG_DIR", home)
        .env_remove("ZODER_API_KEY")
        .args(args)
        .output()
        .expect("spawn zoder status")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// `status --json` with an active marker must produce a single, parseable JSON
/// document whose `host_cooldown` says the host is backed off. The regression
/// is any non-JSON text ahead of the payload.
#[test]
fn status_json_with_active_cooldown_is_one_parseable_document() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    write_marker(&home);

    let out = status(&home, &["status", "--json"]);
    let text = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| {
        panic!(
            "status --json must emit ONE JSON document; parse failed: {e}\n--- stdout ---\n{text}"
        )
    });
    let cooldown = parsed
        .get("host_cooldown")
        .expect("an active cooldown must be exposed under `host_cooldown` in --json mode");
    assert_eq!(
        cooldown.get("active").and_then(|v| v.as_bool()),
        Some(true),
        "the marker is live, so the reported cooldown must be active: {cooldown}"
    );
    assert!(
        !text.contains("[zoder]"),
        "no human status line may precede the JSON payload; got:\n{text}"
    );
}

/// Without `--json`, the human status line IS the output — the marker must be
/// announced in words, not as raw JSON.
#[test]
fn status_human_mode_prints_the_cooldown_line() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    write_marker(&home);

    let out = status(&home, &["status"]);
    let text = stdout(&out);
    assert!(
        text.contains("host cooldown"),
        "human `status` must announce the cooldown; got:\n{text}"
    );
}
