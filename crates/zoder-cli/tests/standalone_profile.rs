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
