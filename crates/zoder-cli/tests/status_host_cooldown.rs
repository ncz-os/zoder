//! End-to-end guards for `zoder status` reporting a host cooldown (D8 step 12).
//!
//! These run the REAL binary against an isolated `$ZODER_HOME`, so they pin the
//! operator-visible contract:
//!
//!  - with a cooldown marker beside the ledger, `status --json` emits ONE JSON
//!    document that is still the job LIST — nothing is printed before it, so a
//!    caller can pipe the output straight into a JSON parser;
//!  - the TOP-LEVEL type of that document never changes: it is always the job
//!    list, whether or not the host is in cooldown;
//!  - the cooldown is still announced — on the human path as a `[zoder]` line
//!    on stdout, on the JSON path as the same line on STDERR (plus the existing
//!    `host_cooldown` exit code) — so a human operator loses nothing.
//!
//! The bugs these guard:
//!
//!  1. `cmd_status` called `print_host_cooldown(false)` unconditionally, so
//!     `--json` mode printed the human `[zoder] ...` line to stdout FIRST and
//!     the JSON payload second. `json.loads(stdout)` then failed on a document
//!     the operator had explicitly asked to be machine-readable.
//!  2. The cooldown was announced by REPLACING the top-level array with an
//!     object (`{"host_cooldown":…,"jobs":[…]}`). `jobs = json.loads(stdout)`
//!     then yielded a list on a quiet host and a dict on exactly the run that
//!     tripped the cap — the moment automation most needs it to keep working.
//!     The payload shape is now fixed: the job list is always the top level,
//!     and the cooldown rides on the exit CODES + STDERR instead.

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

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `status --json` must NOT change its top-level type when the host is in
/// cooldown.
///
/// This is the D8 step 12 regression: the payload used to be the job list on a
/// quiet host and `{"host_cooldown":…,"jobs":[…]}` while the marker was live,
/// so `jobs = json.loads(stdout); jobs[0]` blew up on exactly the run that
/// tripped the cap. The document is now ALWAYS the job list.
#[test]
fn status_json_top_level_shape_does_not_change_with_a_cooldown() {
    let quiet = tempfile::tempdir().expect("tempdir");
    let quiet_home = quiet.path().join("home");
    fs::create_dir_all(&quiet_home).expect("create home");
    let quiet_out = status(&quiet_home, &["status", "--json"]);
    let quiet_parsed: serde_json::Value = serde_json::from_str(&stdout(&quiet_out))
        .expect("quiet `status --json` is one JSON document");
    assert!(
        quiet_parsed.is_array(),
        "with no marker the payload is the job list: {quiet_parsed}"
    );

    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    write_marker(&home);
    let out = status(&home, &["status", "--json"]);
    let text = stdout(&out);
    let parsed: serde_json::Value =
        serde_json::from_str(&text).expect("an active cooldown must still emit ONE JSON document");
    assert!(
        parsed.is_array(),
        "a live cooldown must not change the top-level type of `status --json`; \
         a caller doing `json.loads(stdout)[0]` breaks exactly when the cap trips; got:\n{text}"
    );

    // Announcing the cooldown must not cost an operator the signal, though:
    // in `--json` mode it goes to stderr (stdout stays pure JSON).
    let err = stderr(&out);
    assert!(
        err.contains("host cooldown"),
        "the cooldown must still be announced on stderr in --json mode; got:\n{err}"
    );
    assert!(
        !text.contains("[zoder]"),
        "no human status line may precede the JSON payload; got:\n{text}"
    );
}

/// `status --json` with an active marker must produce a single, parseable JSON
/// document (the job list) and announce the cooldown on stderr. The regression
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
    assert!(
        parsed.is_array(),
        "the job list stays the top level of `status --json`: {parsed}"
    );
    assert!(
        !text.contains("[zoder]"),
        "no human status line may precede the JSON payload; got:\n{text}"
    );
    assert!(
        stderr(&out).contains("host cooldown"),
        "an active cooldown must be reported to the operator; got stderr:\n{}",
        stderr(&out)
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
