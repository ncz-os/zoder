//! Goose-parity command surface: `session` (interactive UI), `run` (headless
//! agentic), `recipe` (saved templates), `mcp` (list engine extensions), and
//! `configure`. These are thin wrappers over the agentic engine + config that
//! `exec`/`tui` already provide, so behavior and cost accounting stay uniform.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

use zoder_core::Config;

use crate::{McpCmd, RecipeCmd};

// ---------------------------------------------------------------------------
// run: headless agentic (goose `run`).
// ---------------------------------------------------------------------------

/// Body of `cmd_run` split out from the entry point so the dispatch /
/// in-worker / inline behaviors can be exercised without actually forking
/// a worker. `dispatch_fn` represents "spawn a background worker and
/// return its id"; `inline_fn` represents "run the agentic turn inline".
/// In production both are the real implementations; tests can swap either
/// for a stub.
///
/// Returns the printed-to-stdout job id when a background dispatch
/// happens, or `None` when the run was executed inline.
pub(crate) async fn run_with_dispatch<F, Fut>(
    cli: &crate::Cli,
    background: bool,
    in_worker: bool,
    dispatch_fn: F,
    inline_fn: Fut,
) -> anyhow::Result<Option<String>>
where
    F: FnOnce(&str, &Path) -> anyhow::Result<String>,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let cwd = crate::agentic_cwd(cli)?;
    // `--background` here is the agentic `run` flavor of the same job
    // registry that `review`/`rescue`/`loop` use. Previously the flag
    // was silently accepted-and-ignored (the inline worker still ran on
    // `--background`), so `status`/`result`/`cancel` never saw `run`
    // jobs. Re-exec self into the worker under `ZODER_JOB_DIR` so the
    // worker writes its terminal status on exit and the foreground
    // process can hand the new job id back to the caller.
    //
    // `in_worker` is the resolved-up-front `active_job_dir().is_some()`
    // flag, computed by the caller. Exposing it as a parameter keeps
    // unit tests parallel-safe (env mutation is process-wide and racy
    // under parallel `cargo test`).
    if background && !in_worker {
        let id = dispatch_fn("run", &cwd)?;
        println!("{id}");
        if !cli.quiet {
            eprintln!("[zoder] started background job {id} (zoder status {id} / result {id})");
        }
        return Ok(Some(id));
    }
    inline_fn.await?;
    Ok(None)
}

pub(crate) async fn cmd_run(
    cli: &crate::Cli,
    text: Option<String>,
    instructions: Option<String>,
    background: bool,
) -> anyhow::Result<()> {
    let task = match (text, instructions) {
        (Some(t), _) => t,
        (None, Some(file)) => std::fs::read_to_string(&file)
            .with_context(|| format!("reading instructions file {file:?}"))?,
        (None, None) => crate::read_prompt(None)?, // stdin / -
    };
    let task2 = task.clone();
    run_with_dispatch(
        cli,
        background,
        crate::agentic::active_job_dir().is_some(),
        crate::agentic::spawn_background,
        async move { crate::cmd_exec_agentic(cli, Some(task2)).await },
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// recipe: saved prompt/agent templates (goose `recipe`).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Recipe {
    prompt: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    oneshot: bool,
}

fn recipes_dir() -> PathBuf {
    Config::home().join("recipes")
}

/// Resolve a recipe argument: an existing path, or a bare name in the recipes
/// dir (with or without a `.json` suffix).
fn resolve_recipe_path(arg: &str) -> PathBuf {
    let p = PathBuf::from(arg);
    if p.exists() {
        return p;
    }
    let dir = recipes_dir();
    let with_ext = dir.join(format!("{arg}.json"));
    if with_ext.exists() {
        return with_ext;
    }
    dir.join(arg)
}

pub(crate) async fn cmd_recipe(cli: &crate::Cli, action: &RecipeCmd) -> anyhow::Result<()> {
    match action {
        RecipeCmd::List => {
            let dir = recipes_dir();
            let mut found = false;
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.extension().and_then(|s| s.to_str()) == Some("json") {
                        found = true;
                        let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
                        let prompt = std::fs::read_to_string(&p)
                            .ok()
                            .and_then(|raw| serde_json::from_str::<Recipe>(&raw).ok())
                            .map(|r| r.prompt)
                            .unwrap_or_default();
                        let preview: String = prompt.chars().take(60).collect();
                        println!("{name:24}  {preview}");
                    }
                }
            }
            if !found {
                println!(
                    "no recipes in {} (create <name>.json: {{\"prompt\":\"...\"}})",
                    dir.display()
                );
            }
            Ok(())
        }
        RecipeCmd::Show { file } => {
            let path = resolve_recipe_path(file);
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading recipe {}", path.display()))?;
            println!("{raw}");
            Ok(())
        }
        RecipeCmd::Run { file } => {
            let path = resolve_recipe_path(file);
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading recipe {}", path.display()))?;
            let recipe: Recipe = serde_json::from_str(&raw)
                .with_context(|| format!("parsing recipe {} (expected JSON)", path.display()))?;

            // Apply recipe overrides onto a cloned Cli.
            let mut rcli = cli.clone();
            if rcli.model.is_none() {
                rcli.model = recipe.model.clone();
            }
            if rcli.agent.is_none() {
                rcli.agent = recipe.agent.clone();
            }
            if rcli.cd.is_none() {
                rcli.cd = recipe.cwd.clone();
            }
            if recipe.oneshot {
                rcli.oneshot = true;
            }
            // Route through the same dispatcher as a bare prompt.
            crate::cmd_exec(&rcli, Some(recipe.prompt)).await
        }
    }
}

// ---------------------------------------------------------------------------
// mcp: list engine-configured extensions/servers (goose extensions).
// ---------------------------------------------------------------------------

use zoder_core::{parse_mcp_servers_file, McpServerSpec, McpTransportKind};

/// Engine config file: `<engine_config_dir>/config.toml`.
fn engine_config_file() -> PathBuf {
    crate::zeroclaw_data_dir()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default()
        .join("config.toml")
}

/// Render one server's transport in a compact, human-readable form
/// suitable for the `mcp list` table column. Captures exactly the
/// shape a future slice will hand to the goose ACP `session/new`
/// call, so what the user sees here matches what the engine will
/// receive.
fn describe_transport(spec: &McpServerSpec) -> String {
    match spec.transport {
        McpTransportKind::Stdio => {
            let cmd = spec.command.as_deref().unwrap_or("?");
            if spec.args.is_empty() {
                cmd.to_string()
            } else {
                format!("{cmd} {}", spec.args.join(" "))
            }
        }
        McpTransportKind::Http => spec.url.as_deref().unwrap_or("?").to_string(),
        McpTransportKind::Unknown => "(unknown transport)".to_string(),
    }
}

/// Render one server's `enabled` state for the listing's tail
/// column. Absence is shown as "enabled" — presence under
/// `[mcp_servers.<name>]` already implies intent, and only an
/// explicit `enabled = false` should be visually demoted.
fn describe_enabled(spec: &McpServerSpec) -> &'static str {
    match spec.enabled {
        Some(false) => "disabled",
        _ => "enabled",
    }
}

pub(crate) fn cmd_mcp(_cli: &crate::Cli, action: &McpCmd) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    cmd_mcp_write(action, &engine_config_file(), &mut out)
}

/// Inner, test-friendly form of `cmd_mcp` that reads the engine
/// config from an explicit path and writes its output to any
/// `Write`. The wrapper picks the production path and stdout;
/// tests inject a temp file and an in-memory buffer. This keeps
/// the parse/format logic covered by unit tests without flaking
/// on the engine's `HOME` / `ZEROCLAW_CONFIG_DIR` resolution or
/// on stdout capture races.
pub(crate) fn cmd_mcp_write<W: std::io::Write>(
    action: &McpCmd,
    path: &Path,
    out: &mut W,
) -> anyhow::Result<()> {
    match action {
        McpCmd::List { json } => {
            // Engine config may legitimately be absent — the user
            // hasn't created it yet. The parser already treats a
            // missing file as "no servers configured" (returns
            // empty Vec), so the listing below renders the
            // canonical "none configured" + hint.
            let specs = match parse_mcp_servers_file(path) {
                Ok(specs) => specs,
                Err(e) => {
                    // Whole-file parse failure. Surface a clean
                    // message and the hint; don't spew a backtrace
                    // for a config the user wrote by hand.
                    writeln!(
                        out,
                        "failed to parse engine config at {}: {e}",
                        path.display()
                    )?;
                    writeln!(
                        out,
                        "add MCP servers under [mcp_servers.<name>] in the engine config."
                    )?;
                    return Ok(());
                }
            };

            if *json {
                // Stable contract: serde_json over `Vec<McpServerSpec>`.
                // The follow-up slice that hands these to goose ACP
                // `session/new` consumes the same shape via
                // `parse_mcp_servers_file` + `serde_json::to_value`.
                let value = serde_json::to_string_pretty(&specs)?;
                writeln!(out, "{value}")?;
                return Ok(());
            }

            if specs.is_empty() {
                writeln!(out, "none configured ({}).", path.display())?;
                writeln!(
                    out,
                    "add them under [mcp_servers.<name>] in the engine config."
                )?;
                return Ok(());
            }

            writeln!(out, "MCP servers in {}:", path.display())?;
            // Compute a stable column width from the longest name so
            // the table lines up regardless of how many servers
            // there are. Names are short in practice, but the
            // formatted output is meant to be readable, not minimal.
            let name_w = specs
                .iter()
                .map(|s| s.name.chars().count())
                .max()
                .unwrap_or(0)
                .max(4);
            let transport_w = specs
                .iter()
                .map(|s| transport_label(s.transport).chars().count())
                .max()
                .unwrap_or(0)
                .max("transport".len());
            writeln!(
                out,
                "  {:<name_w$}  {:<transport_w$}  {:<8}  transport-detail",
                "name", "transport", "enabled",
            )?;
            for spec in &specs {
                writeln!(
                    out,
                    "  {:<name_w$}  {:<transport_w$}  {:<8}  {}",
                    spec.name,
                    transport_label(spec.transport),
                    describe_enabled(spec),
                    describe_transport(spec),
                )?;
            }
            Ok(())
        }
    }
}

/// One-word label for the transport kind, for the table column.
fn transport_label(kind: McpTransportKind) -> &'static str {
    match kind {
        McpTransportKind::Stdio => "stdio",
        McpTransportKind::Http => "http",
        McpTransportKind::Unknown => "?",
    }
}

// ---------------------------------------------------------------------------
// configure (goose `configure`).
// ---------------------------------------------------------------------------

pub(crate) fn cmd_configure(edit: bool, validate: bool) -> anyhow::Result<()> {
    let cfg_path = Config::home().join("config.json");
    let engine_cfg = engine_config_file();

    if edit {
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
        // Ensure the file exists so the editor opens something.
        if !cfg_path.exists() {
            if let Some(p) = cfg_path.parent() {
                std::fs::create_dir_all(p).ok();
            }
            let default = Config::default_provider(&Config::home());
            std::fs::write(&cfg_path, serde_json::to_string_pretty(&default)?)?;
        }
        let status = std::process::Command::new(&editor)
            .arg(&cfg_path)
            .status()
            .with_context(|| format!("launching editor {editor}"))?;
        if !status.success() {
            return Err(anyhow!("editor exited with {status}"));
        }
        return Ok(());
    }

    // Default: show locations + a validation summary.
    println!("config:        {}", cfg_path.display());
    println!("engine config: {}", engine_cfg.display());
    println!("recipes:       {}", recipes_dir().display());
    println!("jobs:          {}", Config::home().join("jobs").display());

    let cfg = Config::load()?;
    let errs = cfg.validate();
    if errs.is_empty() {
        println!("\nproviders ({}):", cfg.providers.len());
        for p in &cfg.providers {
            let kind = if p.paid { "paid" } else { "free" };
            println!("  {:12} {:8} {}", p.id, kind, p.base_url);
        }
        println!("\nconfig OK");
        Ok(())
    } else {
        eprintln!("\nconfig problems:");
        for e in &errs {
            eprintln!("  - {e}");
        }
        if validate {
            std::process::exit(1);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// mcp list tests — exercise the parser + formatter end-to-end against
// temp files. The runtime/session path is NOT touched here (that's a
// separate live-validated slice); these tests just lock down the
// PARSING + DISPLAY behavior of `mcp list` and its `--json` mode.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod mcp_list_tests {
    use super::*;

    fn write_config(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let p = dir.join("config.toml");
        std::fs::write(&p, body).expect("write config");
        p
    }

    /// Run the command with an in-memory buffer as "stdout" and
    /// return the captured text. Keeps the test off the real
    /// stdout so it doesn't race sibling tests.
    fn run_capturing(action: &McpCmd, path: &Path) -> (String, anyhow::Result<()>) {
        let mut buf: Vec<u8> = Vec::new();
        let res = cmd_mcp_write(action, path, &mut buf);
        let s = String::from_utf8(buf).unwrap_or_default();
        (s, res)
    }

    /// `mcp list --json` against a config with one stdio + one
    /// http server must emit both as structured JSON, parseable
    /// back into `Vec<McpServerSpec>` and matching the on-disk
    /// shape.
    #[test]
    fn mcp_list_json_emits_both_servers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            r#"
[mcp_servers.lookup]
command = "node"
args = ["server.js"]

[mcp_servers.mcp_kiwi_com]
url = "https://mcp.kiwi.com"
"#,
        );

        let (out, res) = run_capturing(&McpCmd::List { json: true }, &path);
        res.expect("cmd_mcp_write");

        let specs: Vec<zoder_core::McpServerSpec> =
            serde_json::from_str(&out).expect("json parses to McpServerSpec list");
        assert_eq!(specs.len(), 2);

        let lookup = specs
            .iter()
            .find(|s| s.name == "lookup")
            .expect("lookup present");
        assert_eq!(lookup.transport, zoder_core::McpTransportKind::Stdio);
        assert_eq!(lookup.command.as_deref(), Some("node"));
        assert_eq!(lookup.args, vec!["server.js".to_string()]);
        assert!(lookup.url.is_none());

        let kiwi = specs
            .iter()
            .find(|s| s.name == "mcp_kiwi_com")
            .expect("kiwi present");
        assert_eq!(kiwi.transport, zoder_core::McpTransportKind::Http);
        assert_eq!(kiwi.url.as_deref(), Some("https://mcp.kiwi.com"));
        assert!(kiwi.command.is_none());
    }

    /// A config with no MCP tables must render the "none
    /// configured" hint, and `--json` must emit `[]`.
    #[test]
    fn mcp_list_no_servers_prints_hint_and_emits_empty_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            r#"
[profile]
primary_model = "openai/gpt-4o"
"#,
        );

        let (human, res) = run_capturing(&McpCmd::List { json: false }, &path);
        res.expect("cmd_mcp_write human");
        assert!(
            human.contains("none configured"),
            "human output should say none configured; got:\n{human}"
        );
        assert!(
            human.contains("[mcp_servers.<name>]"),
            "human output should retain the add-here hint; got:\n{human}"
        );

        let (json_out, res) = run_capturing(&McpCmd::List { json: true }, &path);
        res.expect("cmd_mcp_write json");
        let specs: Vec<zoder_core::McpServerSpec> =
            serde_json::from_str(&json_out).expect("json parses to empty list");
        assert!(specs.is_empty());
    }

    /// A missing config file is not an error: it renders the
    /// "none configured" hint, same as a present-but-empty one.
    #[test]
    fn mcp_list_missing_file_prints_hint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.toml");
        let (out, res) = run_capturing(&McpCmd::List { json: false }, &path);
        res.expect("cmd_mcp_write");
        assert!(
            out.contains("none configured"),
            "missing file should be treated as none configured; got:\n{out}"
        );
    }

    /// Legacy `[extensions.<name>]` heading form must still be
    /// surfaced — that's the third form the old scanner
    /// recognized.
    #[test]
    fn mcp_list_legacy_extensions_table_is_parsed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            r#"
[extensions.bitbucket]
type = "stdio"
cmd = "/usr/local/bin/bitbucket-mcp"
args = ["--stdio"]
"#,
        );
        let (out, res) = run_capturing(&McpCmd::List { json: true }, &path);
        res.expect("cmd_mcp_write");

        let specs: Vec<zoder_core::McpServerSpec> =
            serde_json::from_str(&out).expect("json parses");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "bitbucket");
        assert_eq!(specs[0].transport, zoder_core::McpTransportKind::Stdio);
        assert_eq!(specs[0].source, zoder_core::McpSource::ExtensionsTable);
        assert_eq!(
            specs[0].command.as_deref(),
            Some("/usr/local/bin/bitbucket-mcp")
        );
    }

    /// The human-readable table includes the name, transport,
    /// enabled flag, and the command-or-url detail for each
    /// configured server. This is what users will see in
    /// `mcp list` output, so lock it down.
    #[test]
    fn mcp_list_human_output_lists_each_server_with_transport_detail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            r#"
[mcp_servers.lookup]
command = "node"
args = ["server.js"]

[mcp_servers.mcp_kiwi_com]
url = "https://mcp.kiwi.com"
"#,
        );
        let (out, res) = run_capturing(&McpCmd::List { json: false }, &path);
        res.expect("cmd_mcp_write");
        // Both names appear
        assert!(
            out.contains("lookup"),
            "human output should list lookup; got:\n{out}"
        );
        assert!(
            out.contains("mcp_kiwi_com"),
            "human output should list mcp_kiwi_com; got:\n{out}"
        );
        // Both transports appear with their human labels
        assert!(
            out.contains("stdio"),
            "human output should show stdio; got:\n{out}"
        );
        assert!(
            out.contains("http"),
            "human output should show http; got:\n{out}"
        );
        // The transport details (command + url) appear
        assert!(
            out.contains("node"),
            "human output should include command; got:\n{out}"
        );
        assert!(
            out.contains("https://mcp.kiwi.com"),
            "human output should include url; got:\n{out}"
        );
    }
}
