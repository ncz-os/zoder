//! End-to-end guards for `zoder init --profile` and the shipped single-host
//! template.
//!
//! These run the real binary against an isolated `$ZODER_HOME`, so they pin the
//! operator-visible contract: a fresh init writes the template, a second init
//! never overwrites it, and the template validates with either hosted provider
//! key on its own.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
        .env_remove("ZODER_API_KEY");
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

fn assert_ok(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

/// Seed the routing corpus/pricing so `exec --dry-run` can resolve a model.
fn seed_corpus(home: &Path) {
    fs::copy(
        repo_root().join("corpus/model_corpus.json"),
        home.join("model_corpus.json"),
    )
    .expect("copy corpus");
    fs::copy(
        repo_root().join("pricing/catalog.json"),
        home.join("pricing.json"),
    )
    .expect("copy pricing");
}

#[test]
fn init_writes_the_standalone_template_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let target = dir.path().join("config.standalone.toml");

    let output = run(zoder(dir.path()).args(["init", "--profile", "standalone"]));
    assert_ok(&output, "first init");
    assert!(target.is_file(), "init must write {}", target.display());
    let body = fs::read_to_string(&target).expect("read template");
    assert!(body.contains("mode = \"standalone\""));
    assert!(body.contains("DEEPSEEK_API_KEY"));
    assert!(body.contains("MINIMAX_API_KEY"));

    // A second init must leave the operator's config byte-identical.
    fs::write(&target, "operator edit\n").expect("edit config");
    let output = run(zoder(dir.path()).args(["init", "--profile", "standalone"]));
    assert_ok(&output, "second init");
    assert_eq!(
        fs::read_to_string(&target).expect("read config"),
        "operator edit\n",
        "init without --force must never overwrite an existing config"
    );

    let output = run(zoder(dir.path()).args(["init", "--profile", "standalone", "--force"]));
    assert_ok(&output, "forced init");
    assert!(
        fs::read_to_string(&target)
            .expect("read config")
            .contains("mode = \"standalone\""),
        "--force replaces the config with the template"
    );
}

#[test]
fn init_leaves_an_existing_config_json_untouched() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("config.json"), "{}\n").expect("write config.json");

    let output = run(zoder(dir.path()).args(["init", "--profile", "standalone"]));
    assert_ok(&output, "init with existing config");
    assert!(
        !dir.path().join("config.standalone.toml").exists(),
        "init must not add a second config over an existing config.json"
    );
    assert!(stdout(&output).contains("leaving it untouched"));
}

#[test]
fn init_fleet_writes_the_profile_only_fleet_template() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = run(zoder(dir.path()).args(["init", "--profile", "fleet"]));
    assert_ok(&output, "fleet init");
    let body = fs::read_to_string(dir.path().join("config.fleet.toml")).expect("read fleet config");
    assert!(body.contains("mode = \"fleet\""));
    assert!(
        !body.contains("base_url"),
        "fleet providers belong to the fleet's own config repository"
    );
}

#[test]
fn init_only_minimax_key_defaults_the_author_to_minimax() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = run(zoder(dir.path()).env("MINIMAX_API_KEY", "smoke-key").args([
        "init",
        "--profile",
        "standalone",
    ]));
    assert_ok(&output, "minimax-only init");
    let body = fs::read_to_string(dir.path().join("config.standalone.toml")).expect("read config");
    assert!(body.contains("default_provider = \"minimax\""));
    assert!(body.contains("primary_model = \"MiniMax-M3\""));
    assert!(body.contains("only MINIMAX_API_KEY is set"));
}

#[test]
fn the_standalone_template_validates_with_either_provider_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed_corpus(dir.path());
    // Both keys at init time means the shipped template is written verbatim.
    let output = run(zoder(dir.path())
        .env("DEEPSEEK_API_KEY", "smoke-key")
        .env("MINIMAX_API_KEY", "smoke-key")
        .args(["init", "--profile", "standalone"]));
    assert_ok(&output, "init");

    for key in ["DEEPSEEK_API_KEY", "MINIMAX_API_KEY"] {
        let output = run(zoder(dir.path())
            .env(key, "smoke-key")
            .args(["config", "--validate"]));
        assert_ok(&output, &format!("config --validate with only {key}"));
        assert!(
            stdout(&output).contains("config:      VALID"),
            "stdout:\n{}",
            stdout(&output)
        );
    }
}

#[test]
fn config_validate_fails_on_a_broken_overlay_but_plain_config_still_reports_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    // An unknown table makes the whole overlay unparseable, so every provider
    // in it is dropped. That must never be reported as VALID again.
    fs::write(
        dir.path().join("config.fleet.toml"),
        r#"
[not_a_real_table]
"provider-a/*" = "provider-a"

[[providers]]
id = "fleet-provider"
base_url = "https://gw.example.invalid/v1"
kind = "openai-chat"
auth = { type = "env", var = "FLEET_KEY" }
"#,
    )
    .expect("write broken overlay");

    let output = run(zoder(dir.path()).args(["config", "--validate"]));
    assert!(
        !output.status.success(),
        "config --validate must exit non-zero when an overlay was ignored\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        !out.contains("config:      VALID"),
        "an ignored overlay must never validate as VALID:\n{out}"
    );
    assert!(out.contains("PROBLEM(S)"), "stdout:\n{out}");
    assert!(
        out.contains("config.fleet.toml") && out.contains("unknown field"),
        "the problem must name the file and the parse error:\n{out}"
    );

    // Without --validate the command stays the diagnostic tool: exit 0, print
    // the normal report, and still show the problem.
    let output = run(zoder(dir.path()).args(["config"]));
    assert_ok(&output, "config without --validate");
    let out = stdout(&output);
    assert!(out.contains("home:"), "stdout:\n{out}");
    assert!(out.contains("default:     default"), "stdout:\n{out}");
    assert!(out.contains("PROBLEM(S)"), "stdout:\n{out}");
    assert!(
        out.contains("config.fleet.toml") && out.contains("unknown field"),
        "the diagnostic report must show the ignored overlay:\n{out}"
    );
}

#[test]
fn config_validate_rejects_invalid_overlay_model_families_and_accepts_valid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let overlay = dir.path().join("config.fleet.toml");

    for (body, key) in [
        ("[model_families]\n\"\" = \"family-a\"\n", "\"\""),
        ("[model_families]\n\"*\" = \"family-a\"\n", "\"*\""),
    ] {
        fs::write(&overlay, body).expect("write invalid overlay");
        let output = run(zoder(dir.path()).args(["config", "--validate"]));
        assert!(
            !output.status.success(),
            "config --validate must exit non-zero for {body:?}\nstdout:\n{}\nstderr:\n{}",
            stdout(&output),
            stderr(&output)
        );
        let out = stdout(&output);
        assert!(!out.contains("config:      VALID"), "stdout:\n{out}");
        assert!(out.contains("PROBLEM(S)"), "stdout:\n{out}");
        assert!(
            out.contains("model_families"),
            "the problem must name the map:\n{out}"
        );
        assert!(
            out.contains(key),
            "the problem must name the key {key}:\n{out}"
        );
    }

    // A valid families-only overlay (no [[providers]]) is meaningful and
    // validates cleanly.
    fs::write(
        &overlay,
        "[model_families]\n\"provider-a/*\" = \"provider-a\"\n\"provider-a/model-x\" = \"shared-family\"\n",
    )
    .expect("write valid overlay");
    let output = run(zoder(dir.path()).args(["config", "--validate"]));
    assert_ok(&output, "config --validate with a valid families overlay");
    let out = stdout(&output);
    assert!(out.contains("config:      VALID"), "stdout:\n{out}");
}

#[test]
fn exec_dry_run_resolves_a_route_with_either_provider_key() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed_corpus(dir.path());
    let output = run(zoder(dir.path())
        .env("DEEPSEEK_API_KEY", "smoke-key")
        .env("MINIMAX_API_KEY", "smoke-key")
        .args(["init", "--profile", "standalone"]));
    assert_ok(&output, "init");

    for key in ["DEEPSEEK_API_KEY", "MINIMAX_API_KEY"] {
        let output =
            run(zoder(dir.path())
                .env(key, "smoke-key")
                .args(["exec", "--dry-run", "smoke"]));
        assert_ok(&output, &format!("exec --dry-run with only {key}"));
        let out = stdout(&output);
        assert!(
            out.contains("[dry-run]") && out.contains("model="),
            "exec --dry-run must resolve a model route, got stdout:\n{out}\nstderr:\n{}",
            stderr(&output)
        );
    }
}

/// `zoder quota minimax` asks the provider's OWN usage API for the plan
/// quota, so it dials the provider's endpoint directly and never builds a
/// zoder provider client — the refusal that client carries never runs on
/// this path. A fresh standalone install ships the host-free unset sentinel,
/// so the command must refuse HERE and name the setting to fill in, instead
/// of sending a request to a name that resolves nowhere.
///
/// The test drives the real binary, so it exercises the whole production
/// path: the provider selection, the endpoint refusal and the operator-facing
/// message.
#[test]
fn quota_refuses_an_unset_provider_endpoint() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed_corpus(dir.path());
    let output = run(zoder(dir.path())
        .env("DEEPSEEK_API_KEY", "smoke-key")
        .env("MINIMAX_API_KEY", "smoke-key")
        .args(["init", "--profile", "standalone"]));
    assert_ok(&output, "init");

    let output = run(zoder(dir.path())
        .env("MINIMAX_API_KEY", "smoke-key")
        .args(["quota", "minimax"]));
    assert!(
        !output.status.success(),
        "an unset endpoint must not be dialed\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(
        err.contains("no endpoint configured"),
        "the operator must be told to set base_url\nstderr:\n{err}"
    );
    assert!(
        !err.contains("token_plan/remains request failed"),
        "the refusal must happen before the request is sent, not after it \
         failed\nstderr:\n{err}"
    );
}

/// A fresh standalone install has no endpoint yet: the shipped template names
/// no provider endpoint. The Goose engine hands the routed provider's endpoint
/// to a spawned child process as OPENAI_BASE_URL / OPENAI_HOST, and that path
/// never builds a zoder provider client — so zoder must refuse HERE, before
/// the child is started, instead of handing it a name that cannot resolve.
///
/// The Goose path is the one that is deterministic on every host: it never
/// touches the zeroclaw daemon, and a missing `goose` binary is not an
/// "engine unavailable" error, so the run cannot silently downgrade to the
/// single-shot path and produce the same refusal from there. The assertions
/// below therefore distinguish "refused before dispatch" from "refused later
/// by accident".
#[test]
fn a_fresh_standalone_install_refuses_to_dial_an_unset_endpoint() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed_corpus(dir.path());
    let output = run(zoder(dir.path())
        .env("DEEPSEEK_API_KEY", "smoke-key")
        .args(["init", "--profile", "standalone"]));
    assert_ok(&output, "init");

    let output = run(zoder(dir.path())
        .env("DEEPSEEK_API_KEY", "smoke-key")
        .args(["exec", "--engine", "goose", "--allow-paid", "smoke"]));
    assert!(
        !output.status.success(),
        "an unset endpoint must not be dialed\nstdout:\n{}\nstderr:\n{}",
        stdout(&output),
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(
        err.contains("no endpoint configured"),
        "the operator must be told to set base_url\nstderr:\n{err}"
    );
    assert!(
        !err.contains("falling back to single-shot"),
        "the refusal must happen before the engine is contacted, not after a \
         failed dispatch\nstderr:\n{err}"
    );
}

/// A non-tty run that declines a PAID model (no `--allow-paid`, nothing to
/// prompt) is a policy refusal, not a budget stop, and `--json` gets exactly
/// one error object on stdout.
///
/// Review of the r2 budget-exit change: the paid-model confirmation was routed
/// through the budget decline, so this refusal exited 78 ("budget cap
/// exceeded") -- a fleet back-off keyed on the budget class would park the
/// host until the month resets for a refusal that no reset clears -- and the
/// decline printed its own envelope before `cmd_exec` printed the generic one,
/// so stdout carried two JSON objects.
#[test]
fn a_non_tty_paid_model_decline_is_one_json_error_not_a_budget_stop() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed_corpus(dir.path());
    let output = run(zoder(dir.path())
        .env("DEEPSEEK_API_KEY", "smoke-key")
        .args(["init", "--profile", "standalone"]));
    assert_ok(&output, "init");

    let output = run(zoder(dir.path())
        .env("DEEPSEEK_API_KEY", "smoke-key")
        .stdin(std::process::Stdio::null())
        .args(["--json", "exec", "--oneshot", "smoke"]));
    let out = stdout(&output);
    let err = stderr(&output);
    assert_eq!(
        output.status.code(),
        Some(1),
        "a policy decline keeps the generic failure code, not the budget code\n\
         stdout:\n{out}\nstderr:\n{err}"
    );
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "--json must print exactly one error object\nstdout:\n{out}"
    );
    let v: serde_json::Value = serde_json::from_str(lines[0]).expect("stdout is JSON");
    assert_eq!(v["outcome"], "error");
    assert_eq!(v["succeeded"], false);
    assert!(
        v.get("code").is_none() && !out.contains("budget"),
        "a paid-model decline must not be labelled a budget stop: {v}"
    );
    assert!(
        v["error"]
            .as_str()
            .unwrap_or("")
            .contains("paid model use declined"),
        "the envelope must say why: {v}"
    );
    assert!(!err.contains("budget cap exceeded"), "stderr:\n{err}");
}

/// Seed a `$ZODER_HOME` whose single route is a METERED provider (so the
/// pre-call budget gate prices it from the catalog) under a per-call cap far
/// below the projected cost. Nothing here reaches the network: the budget
/// gate refuses before dispatch, which is the behavior under test.
fn seed_over_budget_home(home: &Path) {
    seed_corpus(home);
    fs::write(
        home.join("config.json"),
        format!(
            r#"{{
  "providers": [
    {{
      "id": "deepseek",
      "base_url": "https://api.invalid/v1",
      "kind": "openai-chat",
      "auth": {{ "type": "env", "var": "DEEPSEEK_API_KEY" }},
      "billing": "metered",
      "serves": ["deepseek/"]
    }}
  ],
  "default_provider": "deepseek",
  "primary_model": "deepseek/deepseek-chat",
  "budget": {{ "max_cost_per_call_usd": 0.000001, "est_output_tokens": 100000 }},
  "corpus_path": "{}",
  "ledger_path": "{}",
  "health_path": "{}"
}}
"#,
            home.join("model_corpus.json").display(),
            home.join("ledger.jsonl").display(),
            home.join("health.json").display(),
        ),
    )
    .expect("write over-budget config");
}

/// D8 step 5 (Z3, second report): on a non-tty an OVER-BUDGET verdict must
/// exit with the distinct budget code (78, `EX_CONFIG`) and a JSON error
/// envelope naming the budget stop — not the generic `call declined` refusal
/// with exit 1.
///
/// The budget exit code existed only as a helper contract: nothing in the
/// real binary reached it. A metered route fails the paid-model policy gate
/// FIRST (`paid = false` is still a metered provider, so the gate asks for
/// confirmation), so a non-tty run whose projected cost blows every cap
/// exited 1 with `paid model use declined` — indistinguishable from a broken
/// invocation, and unparseable by the fleet back-off that keys on the budget
/// class. The budget verdict has to be decided before the policy refusal: the
/// caller is asking to spend, and the answer that matters is the cap.
#[test]
fn over_budget_non_tty_exits_with_budget_code_and_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed_over_budget_home(dir.path());

    let output = run(zoder(dir.path())
        .env("DEEPSEEK_API_KEY", "smoke-key")
        .stdin(std::process::Stdio::null())
        .args(["--json", "exec", "--oneshot", "smoke"]));
    let out = stdout(&output);
    let err = stderr(&output);
    assert_eq!(
        output.status.code(),
        Some(78),
        "a budget stop must exit with the budget code\nstdout:\n{out}\nstderr:\n{err}"
    );
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "--json must print exactly one error object\nstdout:\n{out}"
    );
    let v: serde_json::Value = serde_json::from_str(lines[0]).expect("stdout is JSON");
    assert_eq!(v["outcome"], "error");
    assert_eq!(v["succeeded"], false);
    assert_eq!(
        v["code"], 78,
        "the envelope must carry the budget code: {v}"
    );
    assert_eq!(
        v["reason"], "over_budget",
        "the envelope must name the budget reason: {v}"
    );
    assert_eq!(
        v["model"], "deepseek/deepseek-chat",
        "the envelope must name the model the cap stopped: {v}"
    );
    assert!(
        v["error"].as_str().unwrap_or("").contains("BUDGET"),
        "the envelope must explain the cap: {v}"
    );
    // The policy refusal is NOT the answer here: the caller asked to spend and
    // the cap said no.
    assert!(
        !v["error"]
            .as_str()
            .unwrap_or("")
            .contains("paid model use declined"),
        "a budget stop must not be reported as a plain policy decline: {v}"
    );
}
