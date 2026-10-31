//! End-to-end guards for `zoder gate` (decision D4.7).
//!
//! These run the REAL binary against a throwaway repo, so they pin the
//! operator-visible contract rather than the internals:
//!
//!  - a red gate still prints its report (the `--json` document names the
//!    failing steps) and only then exits non-zero;
//!  - `--root DIR` runs every step IN `DIR`, not in the caller's cwd;
//!  - `--step-timeout SECS` bounds a wedged step instead of hanging the gate.
//!
//! A fake `cargo` on PATH stands in for the toolchain: the derived Rust plan
//! probes `cargo`, so the fixture decides pass/fail/cwd/hang without needing a
//! real build. Nothing here reaches the network or a model provider.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// A repo carrying only a Rust marker file, so `derive_plan` yields the
/// baseline plan (fmt / clippy / build / test / security) and every step
/// resolves its tool through the fake `cargo` on PATH.
fn rust_repo(dir: &Path) {
    fs::create_dir_all(dir).expect("create repo dir");
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
    )
    .expect("write Cargo.toml");
}

/// Stage a fake `cargo` plus stubs for the baseline's security tools, whose
/// bodies are `script`. Returns the directory to prepend to PATH. The stubs
/// make the whole derived plan runnable so no step is skipped for a missing
/// tool — every Rust baseline step then exercises `run_command`.
fn fake_bin(dir: &Path, script: &str) -> PathBuf {
    let bin = dir.join("fakebin");
    fs::create_dir_all(&bin).expect("create fakebin");
    for tool in ["cargo", "cargo-deny", "cargo-audit"] {
        let path = bin.join(tool);
        fs::write(&path, format!("#!/bin/sh\n{script}\n")).expect("write fake tool");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod fake tool");
        }
    }
    bin
}

/// Run `zoder gate` with an isolated `$ZODER_HOME` and the fake tool first on
/// PATH. `cwd` is the caller's working directory, so a step that ignores
/// `--root` is visible as the wrong `pwd -P`.
fn gate(cwd: &Path, home: &Path, path_dir: &Path, args: &[&str]) -> Output {
    let mut paths = vec![path_dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    let joined = std::env::join_paths(paths).expect("join PATH");
    Command::new(env!("CARGO_BIN_EXE_zoder"))
        .current_dir(cwd)
        .env("ZODER_HOME", home)
        .env("ZEROCLAW_CONFIG_DIR", home)
        .env("PATH", joined)
        .args(args)
        .output()
        .expect("spawn zoder gate")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn red_gate_prints_its_json_report_before_exiting_non_zero() {
    // D4.7: `cmd_gate` used to call `process::exit` BEFORE rendering, so a
    // red gate produced no stdout at all. A caller that asked for `--json`
    // got an empty pipe and an exit status, with nothing naming the failing
    // steps. The report must be printed first, then the non-zero exit.
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    rust_repo(&repo);
    // Every step fails: the gate is unambiguously red.
    let bin = fake_bin(tmp.path(), "exit 1");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("home");

    let output = gate(
        &repo,
        &home,
        &bin,
        &["gate", "--root", repo.to_str().unwrap(), "--json"],
    );

    assert!(
        !output.status.success(),
        "a red gate must exit non-zero\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let body = stdout(&output);
    assert!(
        !body.trim().is_empty(),
        "a red gate must still print its report (D4.7); stdout was empty"
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("red gate --json must emit valid JSON");
    assert_eq!(
        parsed["exit_code"],
        serde_json::json!(1),
        "the payload must carry the exit code it is about to return: {body}"
    );
    assert_eq!(
        parsed["report"]["is_passed"],
        serde_json::json!(false),
        "the printed report must not claim a pass: {body}"
    );
    let results = parsed["report"]["results"]
        .as_array()
        .expect("report.results must be an array");
    assert!(
        !results.is_empty(),
        "the printed report must name the steps that were run: {body}"
    );
    assert!(
        results
            .iter()
            .all(|r| r["outcome"] != serde_json::json!("passed")),
        "every step was configured to fail, so none may read as passed: {body}"
    );
}

#[test]
fn red_gate_prints_the_human_report_before_exiting_non_zero() {
    // The same D4.7 contract for the pretty renderer: an operator who runs
    // `zoder gate` (no --json) must see WHY it blocked, not a silent exit.
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    rust_repo(&repo);
    let bin = fake_bin(tmp.path(), "exit 1");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("home");

    let output = gate(
        &repo,
        &home,
        &bin,
        &["gate", "--root", repo.to_str().unwrap()],
    );

    assert!(
        !output.status.success(),
        "a red gate must exit non-zero\nstdout:\n{}",
        stdout(&output)
    );
    let body = stdout(&output);
    assert!(
        body.contains("FAILED"),
        "the human report must be printed before the non-zero exit, got: {body:?}"
    );
}

#[test]
fn gate_root_runs_the_steps_inside_that_root() {
    // D4.7: `PathEnv::run_command` never set `current_dir`, so
    // `zoder gate --root DIR` executed every step in the CALLER's cwd — the
    // gate was checking the wrong tree.
    //
    // The signal is deliberately a STEP that passes only in the root while
    // the overall gate still ends GREEN, so the assertion cannot be satisfied
    // (or defeated) by the render-before-exit fix this same slice carries: on
    // the base the step fails for the wrong cwd, and a green gate there DOES
    // print its JSON, so the failure below is unambiguously about the cwd.
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    rust_repo(&repo);
    let expected = fs::canonicalize(&repo).expect("canonicalize repo");
    // The step records the cwd it actually saw, then always succeeds, so the
    // gate stays green and the report is the only evidence needed.
    let observer = tmp.path().join("cwd-seen");
    let bin = fake_bin(tmp.path(), &format!("pwd -P > '{}'", observer.display()));
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    // The caller's cwd is deliberately a DIFFERENT directory from the root.
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("elsewhere");

    let output = gate(
        &elsewhere,
        &home,
        &bin,
        &[
            "gate",
            "--root",
            repo.to_str().unwrap(),
            "--local-iterate",
            "--json",
        ],
    );

    let body = stdout(&output);
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("gate --json must emit valid JSON");
    let results = parsed["report"]["results"]
        .as_array()
        .expect("report.results must be an array");
    assert!(
        !results.is_empty(),
        "the Rust fixture must derive a plan: {body}"
    );
    assert!(
        output.status.success(),
        "the fixture steps always succeed, so a green gate must exit 0\nstdout:\n{body}"
    );
    let seen = fs::read_to_string(&observer).expect("the fake step must have run");
    assert_eq!(
        seen.trim(),
        expected.display().to_string(),
        "every step must run inside `--root` ({}), not the caller's cwd ({})",
        expected.display(),
        elsewhere.display()
    );
}

#[test]
fn gate_step_timeout_kills_a_wedged_step() {
    // D4.7: a step that hangs must not hang the gate forever. `--step-timeout
    // 1` with a step that sleeps far longer must come back quickly, report
    // the step as failed, and exit non-zero.
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    rust_repo(&repo);
    let bin = fake_bin(tmp.path(), "sleep 120");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("home");

    let started = Instant::now();
    let output = gate(
        &repo,
        &home,
        &bin,
        &[
            "gate",
            "--root",
            repo.to_str().unwrap(),
            "--local-iterate",
            "--step-timeout",
            "1",
            "--json",
        ],
    );
    let elapsed = started.elapsed();

    // Elapsed time is the assertion that isolates THIS regression: without a
    // per-step timeout the 120s `sleep` blocks the gate indefinitely, so this
    // must come first and fail on its own terms even if the report is missing.
    assert!(
        elapsed < Duration::from_secs(30),
        "--step-timeout 1 must cut the wedged steps short, took {elapsed:?}"
    );
    // Four Rust baseline steps at one second each is ~4s; 30s is a wide
    // margin over that and far under the 120s sleep the fake would impose.
    assert!(
        !output.status.success(),
        "a timed-out required step must fail the gate\nstdout:\n{}",
        stdout(&output)
    );
    let body = stdout(&output);
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("gate --json must emit valid JSON");
    let results = parsed["report"]["results"]
        .as_array()
        .expect("report.results must be an array");
    assert!(
        !results.is_empty(),
        "the Rust fixture must derive a plan: {body}"
    );
    for result in results {
        assert_ne!(
            result["outcome"],
            serde_json::json!("passed"),
            "a step that outlived --step-timeout must not read as passed: {body}"
        );
    }
    assert_eq!(
        parsed["report"]["is_passed"],
        serde_json::json!(false),
        "the timed-out steps must leave the gate red: {body}"
    );
}
