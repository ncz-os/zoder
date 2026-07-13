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
    output_last_message: Option<String>,
    events_file: Option<String>,
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
        async move {
            crate::cmd_exec_agentic(
                cli,
                Some(task2),
                output_last_message.clone(),
                events_file.clone(),
            )
            .await
        },
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

/// Read a recipe file at `path` with the same regular-file + size-cap
/// guards that `crate::config::read_bounded_regular_file` applies to the
/// main zoder config. Two threat models converge here:
///
///   * A `.json` recipe file that is a symlink to `/dev/zero` (or any
///     other device / FIFO / dangling symlink) would block or OOM the
///     process on the previous `fs::read_to_string` call. Detecting the
///     symlink with `symlink_metadata` — the same idiom `jobs.rs` uses
///     to refuse symlinks in this crate — catches the case before any
///     read is attempted. Non-symlink non-regular files (FIFOs,
///     character devices, sockets) are caught by `is_file()` on the same
///     metadata.
///   * A `.json` recipe file that is several GiB would slurp into memory
///     before parsing fails, risking OOM on a lightweight listing. The
///     size is checked up front against `max_bytes`; the actual read is
///     wrapped in `Read::take(max_bytes + 1)` so a file that grew
///     between the stat and the read still cannot exceed the cap.
///
/// `max_bytes` is the upper bound the caller wants to enforce; pass
/// `Config::MAX_CONFIG_BYTES` (2 MiB) to mirror the existing
/// pricing/config cap convention for files that need the full body.
fn read_recipe_file(path: &Path, max_bytes: u64) -> anyhow::Result<String> {
    use std::io::Read;

    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("stat recipe {}", path.display()))?;
    if meta.file_type().is_symlink() {
        anyhow::bail!(
            "recipe {} is a symlink; refusing to follow (it may point \
             outside the recipes dir, e.g. to /dev/zero, and would hang or \
             OOM the process)",
            path.display()
        );
    }
    if !meta.is_file() {
        anyhow::bail!(
            "recipe {} is not a regular file (FIFOs, devices, sockets, and \
             other non-regular files are rejected before the read to avoid \
             blocking or OOMing the process)",
            path.display()
        );
    }
    if meta.len() > max_bytes {
        anyhow::bail!(
            "recipe {} rejected — {} bytes exceeds {} byte limit",
            path.display(),
            meta.len(),
            max_bytes
        );
    }

    let f =
        std::fs::File::open(path).with_context(|| format!("opening recipe {}", path.display()))?;
    let mut s = String::new();
    f.take(max_bytes + 1)
        .read_to_string(&mut s)
        .with_context(|| format!("reading recipe {}", path.display()))?;
    Ok(s)
}

pub(crate) async fn cmd_recipe(cli: &crate::Cli, action: &RecipeCmd) -> anyhow::Result<()> {
    match action {
        RecipeCmd::List => cmd_recipe_list(&recipes_dir()),
        RecipeCmd::Show { file } => cmd_recipe_show(file),
        RecipeCmd::Run { file } => cmd_recipe_run(cli, file).await,
    }
}

/// `recipe list` entry point: locks stdout and delegates to the
/// testable inner writer.
fn cmd_recipe_list(dir: &Path) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    cmd_recipe_list_write(dir, &mut out)
}

/// Test-friendly core of `recipe list`: enumerates `<dir>/*.json`,
/// prints `<name>  <first-60-chars-of-prompt>` per entry, and skips
/// files that are not regular files or are oversized (the
/// bounded-read guard). The defense matches `pricing.rs` /
/// `config.rs`: a symlink-to-`/dev/zero` or a multi-GiB recipe would
/// otherwise hang or OOM `zoder recipe list`.
fn cmd_recipe_list_write<W: std::io::Write>(dir: &Path, out: &mut W) -> anyhow::Result<()> {
    let mut found = false;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            found = true;
            let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
            // Bounded read — same cap convention as `recipe show` /
            // `recipe run` (2 MiB). The pre-fix code called
            // `read_to_string` unconditionally, which hangs on a
            // symlink-to-/dev/zero and OOMs on a multi-GiB recipe.
            // Only a 60-character preview is rendered, but the cap is
            // deliberately larger than that so a legitimate recipe
            // whose `prompt` field is large (which appears FIRST in
            // the JSON because it is the first struct field) still
            // parses cleanly. Parse failures are still tolerated the
            // same way the pre-fix code tolerated them (empty
            // preview), but a symlink / FIFO / device / oversized
            // entry is warned-and-skipped rather than blocking the
            // listing.
            let raw = match read_recipe_file(&p, Config::MAX_CONFIG_BYTES) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("zoder: warning: skipping recipe {} — {:#}", p.display(), e);
                    continue;
                }
            };
            let prompt = serde_json::from_str::<Recipe>(&raw)
                .map(|r| r.prompt)
                .unwrap_or_default();
            let preview: String = prompt.chars().take(60).collect();
            writeln!(out, "{name:24}  {preview}")?;
        }
    }
    if !found {
        writeln!(
            out,
            "no recipes in {} (create <name>.json: {{\"prompt\":\"...\"}})",
            dir.display()
        )?;
    }
    Ok(())
}

/// `recipe show <file>` entry point. Resolves the path the same way
/// the pre-fix code did, then reads it via the bounded regular-file
/// helper so a symlink, FIFO, device, or oversized file is rejected
/// with a clear error instead of hanging the CLI.
fn cmd_recipe_show(file: &str) -> anyhow::Result<()> {
    let path = resolve_recipe_path(file);
    let raw = read_recipe_file(&path, Config::MAX_CONFIG_BYTES)
        .with_context(|| format!("reading recipe {}", path.display()))?;
    println!("{raw}");
    Ok(())
}

/// `recipe run <file>` entry point. Same bounded read as
/// `recipe show`; the full prompt body is needed downstream, so the
/// cap mirrors `Config::MAX_CONFIG_BYTES` (2 MiB) — the same magnitude
/// the existing `pricing.rs` / `config.rs` "small text file" caps use
/// rather than inventing a new bound.
async fn cmd_recipe_run(cli: &crate::Cli, file: &str) -> anyhow::Result<()> {
    let path = resolve_recipe_path(file);
    let raw = read_recipe_file(&path, Config::MAX_CONFIG_BYTES)
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

#[cfg(test)]
mod recipe_tests {
    use super::*;

    fn write_recipe(dir: &Path, name: &str, prompt: &str) -> PathBuf {
        let p = dir.join(format!("{name}.json"));
        std::fs::write(&p, serde_json::json!({ "prompt": prompt }).to_string())
            .expect("write recipe");
        p
    }

    /// DEFECT: `recipe list` must not follow a symlink at a `.json`
    /// candidate path — pre-fix `read_to_string` would follow it into
    /// whatever the link points at (a writer-less FIFO would hang the
    /// listing forever; `/dev/zero` would OOM it). The bounded reader
    /// rejects the symlink itself via `symlink_metadata` before any
    /// read is attempted, so the entry is skipped (with a stderr
    /// warning) rather than followed.
    #[test]
    fn recipe_list_skips_symlinked_json_to_dev_null() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink("/dev/null", dir.path().join("evil.json"))
            .expect("symlink /dev/null");
        write_recipe(dir.path(), "good", "a real recipe prompt");

        let mut out: Vec<u8> = Vec::new();
        cmd_recipe_list_write(dir.path(), &mut out).expect("list must not error");
        let text = String::from_utf8(out).expect("utf8 output");

        assert!(
            text.contains("good"),
            "the legitimate recipe must still be listed: {text:?}"
        );
        assert!(
            !text.contains("evil"),
            "a symlinked recipe must be skipped, not followed: {text:?}"
        );
    }

    /// DEFECT (oversized variant): a `.json` recipe file larger than
    /// the bounded-read cap must be skipped by `recipe list` rather
    /// than read in full. Pre-fix code had no size check at all.
    #[test]
    fn recipe_list_skips_oversized_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let huge_prompt = "a".repeat((Config::MAX_CONFIG_BYTES as usize) + 4096);
        write_recipe(dir.path(), "toobig", &huge_prompt);
        write_recipe(dir.path(), "good", "short prompt");

        let mut out: Vec<u8> = Vec::new();
        cmd_recipe_list_write(dir.path(), &mut out).expect("list must not error");
        let text = String::from_utf8(out).expect("utf8 output");

        assert!(
            text.contains("good"),
            "the legitimately sized recipe must still be listed: {text:?}"
        );
        assert!(
            !text.contains("toobig"),
            "an oversized recipe must be skipped rather than read in full: {text:?}"
        );
    }

    /// Regression guard for the ordinary path: a well-formed, small
    /// recipe file must list with its 60-character prompt preview,
    /// matching pre-fix behavior for the common case.
    #[test]
    fn recipe_list_shows_prompt_preview_for_normal_recipe() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_recipe(dir.path(), "normal", "fix the bug in the parser please");

        let mut out: Vec<u8> = Vec::new();
        cmd_recipe_list_write(dir.path(), &mut out).expect("list must not error");
        let text = String::from_utf8(out).expect("utf8 output");

        assert!(
            text.contains("normal") && text.contains("fix the bug in the parser please"),
            "normal recipe must list its name + prompt preview: {text:?}"
        );
    }

    /// `recipe show` must refuse a symlinked recipe path with a clear
    /// error rather than following it — same hazard as `list`, applied
    /// to the single-file read path.
    #[test]
    fn recipe_show_rejects_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let link = dir.path().join("evil.json");
        std::os::unix::fs::symlink("/dev/null", &link).expect("symlink /dev/null");

        let err = read_recipe_file(&link, Config::MAX_CONFIG_BYTES)
            .expect_err("a symlinked recipe must be rejected");
        assert!(
            err.to_string().contains("symlink"),
            "error must name the symlink hazard: {err:#}"
        );
    }

    /// `read_recipe_file` must reject a file above the given cap
    /// before reading its full contents.
    #[test]
    fn read_recipe_file_rejects_oversized_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("big.json");
        std::fs::write(&p, "a".repeat(4096)).expect("write");

        let err = read_recipe_file(&p, 1024).expect_err("a file over the cap must be rejected");
        assert!(
            err.to_string().contains("exceeds") || err.to_string().contains("limit"),
            "error must name the size limit: {err:#}"
        );
    }
}

// ---------------------------------------------------------------------------
// mcp: list engine-configured extensions/servers (goose extensions).
// ---------------------------------------------------------------------------

use zoder_core::{parse_mcp_servers_file, McpServerSpec, McpTransportKind};

/// Engine config file: `<engine_config_dir>/config.toml`.
///
/// `pub(crate)` so `main.rs` can reuse it when populating the
/// goose `AgentOptions::mcp_servers` field (the `mcp list` command
/// in this same module reads the same file). Keeping a single
/// resolution site means there is exactly one place to change if
/// the engine config location ever moves.
pub(crate) fn engine_config_file_for_cli() -> PathBuf {
    crate::zeroclaw_data_dir()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default()
        .join("config.toml")
}

/// Backwards-compatible alias used internally by this module; new
/// callers should use [`engine_config_file_for_cli`] so the
/// resolution logic isn't duplicated.
fn engine_config_file() -> PathBuf {
    engine_config_file_for_cli()
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
    let exit_code = cmd_mcp_write(action, &engine_config_file(), &mut out)?;
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
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
) -> anyhow::Result<i32> {
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
                    return Ok(0);
                }
            };

            if *json {
                // Stable contract: serde_json over `Vec<McpServerSpec>`.
                // The follow-up slice that hands these to goose ACP
                // `session/new` consumes the same shape via
                // `parse_mcp_servers_file` + `serde_json::to_value`.
                let value = serde_json::to_string_pretty(&specs)?;
                writeln!(out, "{value}")?;
                return Ok(0);
            }

            if specs.is_empty() {
                writeln!(out, "none configured ({}).", path.display())?;
                writeln!(
                    out,
                    "add them under [mcp_servers.<name>] in the engine config."
                )?;
                return Ok(0);
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
            Ok(0)
        }
        McpCmd::Get { name, json } => {
            let specs = match parse_mcp_servers_file(path) {
                Ok(specs) => specs,
                Err(e) => {
                    writeln!(
                        out,
                        "failed to parse engine config at {}: {e}",
                        path.display()
                    )?;
                    writeln!(
                        out,
                        "add MCP servers under [mcp_servers.<name>] in the engine config."
                    )?;
                    return Ok(0);
                }
            };

            // Find the server by name
            match specs.iter().find(|s| &s.name == name) {
                Some(spec) => {
                    if *json {
                        let value = serde_json::to_string_pretty(spec)?;
                        writeln!(out, "{value}")?;
                    } else {
                        writeln!(out, "MCP server '{}' in {}:", name, path.display())?;
                        writeln!(out, "  name:         {}", spec.name)?;
                        writeln!(out, "  transport:    {}", transport_label(spec.transport))?;
                        writeln!(out, "  enabled:      {}", describe_enabled(spec))?;
                        writeln!(out, "  transport-detail: {}", describe_transport(spec))?;
                    }
                    Ok(0)
                }
                None => {
                    // Server not found - list available servers and show error
                    if specs.is_empty() {
                        writeln!(out, "no MCP servers configured in {}.", path.display())?;
                        writeln!(
                            out,
                            "add them under [mcp_servers.<name>] in the engine config."
                        )?;
                    } else {
                        writeln!(out, "no MCP server named '{}' found.", name)?;
                        writeln!(out, "available servers:")?;
                        for spec in &specs {
                            writeln!(out, "  - {}", spec.name)?;
                        }
                    }
                    Ok(0)
                }
            }
        }
        McpCmd::Test { name } => {
            let specs = match parse_mcp_servers_file(path) {
                Ok(specs) => specs,
                Err(e) => {
                    writeln!(
                        out,
                        "failed to parse engine config at {}: {e}",
                        path.display()
                    )?;
                    writeln!(
                        out,
                        "add MCP servers under [mcp_servers.<name>] in the engine config."
                    )?;
                    return Ok(0);
                }
            };

            // Find the server by name
            match specs.iter().find(|s| &s.name == name) {
                Some(spec) => {
                    match spec.transport {
                        McpTransportKind::Http => {
                            // Test HTTP transport with a GET request
                            if let Some(url) = &spec.url {
                                let start_time = std::time::Instant::now();
                                // Create a client with a 5-second timeout
                                let client = reqwest::blocking::Client::builder()
                                    .timeout(std::time::Duration::from_secs(5))
                                    .build()
                                    .map_err(|e| {
                                        anyhow::anyhow!("failed to build HTTP client: {}", e)
                                    })?;
                                match client.get(url).send() {
                                    Ok(response) => {
                                        let duration = start_time.elapsed();
                                        let status = response.status();
                                        writeln!(out, "MCP server '{}' reachable via HTTP", name)?;
                                        writeln!(out, "  URL: {}", url)?;
                                        writeln!(
                                            out,
                                            "  Status: {} ({})",
                                            status,
                                            status.as_str()
                                        )?;
                                        writeln!(out, "  Latency: {:.2?}", duration)?;
                                        // Exit code 0 for reachable
                                        Ok(0)
                                    }
                                    Err(e) => {
                                        let duration = start_time.elapsed();
                                        writeln!(
                                            out,
                                            "MCP server '{}' unreachable via HTTP",
                                            name
                                        )?;
                                        writeln!(out, "  URL: {}", url)?;
                                        writeln!(out, "  Error: {} (after {:.2?})", e, duration)?;
                                        // Exit code 1 for unreachable
                                        Ok(1)
                                    }
                                }
                            } else {
                                writeln!(out, "MCP server '{}' has no URL for HTTP testing", name)?;
                                Ok(1)
                            }
                        }
                        McpTransportKind::Stdio => {
                            // Test stdio transport by spawning the command
                            if let Some(command) = &spec.command {
                                let start_time = std::time::Instant::now();

                                // Spawn the command with timeout
                                let mut child = std::process::Command::new(command)
                                    .args(&spec.args)
                                    .envs(&spec.env)
                                    .stdin(std::process::Stdio::piped())
                                    .stdout(std::process::Stdio::piped())
                                    .stderr(std::process::Stdio::piped())
                                    .spawn()
                                    .map_err(|e| {
                                        anyhow::anyhow!(
                                            "failed to spawn command {}: {}",
                                            command,
                                            e
                                        )
                                    })?;

                                // Send minimal JSON-RPC 2.0 initialize request
                                let initialize_request = r#"{"jsonrpc": "2.0", "method": "initialize", "id": 1, "params": {}}"#;
                                if let Some(ref mut stdin) = child.stdin {
                                    std::io::Write::write_all(stdin, initialize_request.as_bytes())
                                        .map_err(|e| {
                                            anyhow::anyhow!("failed to write to stdin: {}", e)
                                        })?;
                                }

                                // Wait for response with timeout using a helper thread approach
                                let timeout = std::time::Duration::from_secs(5);

                                // Use a helper thread to read with timeout. `child` is moved
                                // into the thread since `wait_with_output` needs ownership;
                                // the PID is captured beforehand so the timeout branch can
                                // still kill the process without owning the `Child` handle.
                                let (tx, rx) = std::sync::mpsc::channel();
                                let child_pid = child.id();

                                // Spawn a thread to wait for the child. Send errors are
                                // ignored: if the main thread already gave up after a
                                // timeout, the receiver is dropped and there's nothing to
                                // deliver the (now-stale) result to.
                                std::thread::spawn(move || match child.wait_with_output() {
                                    Ok(output) => {
                                        let _ = tx.send(Ok(output));
                                    }
                                    Err(e) => {
                                        let _ = tx.send(Err(e));
                                    }
                                });

                                // Wait for either the child to finish or timeout
                                let result = match rx.recv_timeout(timeout) {
                                    Ok(Ok(output)) => {
                                        let duration = start_time.elapsed();
                                        if output.status.success() {
                                            let response = String::from_utf8_lossy(&output.stdout);
                                            writeln!(
                                                out,
                                                "MCP server '{}' reachable via stdio",
                                                name
                                            )?;
                                            writeln!(
                                                out,
                                                "  Command: {} {}",
                                                command,
                                                spec.args.join(" ")
                                            )?;
                                            writeln!(out, "  Response: {} bytes", response.len())?;
                                            if !response.trim().is_empty() {
                                                writeln!(
                                                    out,
                                                    "  Sample response: {}",
                                                    response.trim()
                                                )?;
                                            }
                                            writeln!(
                                                out,
                                                "  Status: {} (after {:.2?})",
                                                output.status, duration
                                            )?;
                                            Ok(0)
                                        } else {
                                            let stderr = String::from_utf8_lossy(&output.stderr);
                                            writeln!(
                                                out,
                                                "MCP server '{}' reachable but command failed",
                                                name
                                            )?;
                                            writeln!(
                                                out,
                                                "  Command: {} {}",
                                                command,
                                                spec.args.join(" ")
                                            )?;
                                            writeln!(out, "  Error: {}", stderr.trim())?;
                                            writeln!(
                                                out,
                                                "  Status: {} (after {:.2?})",
                                                output.status, duration
                                            )?;
                                            Ok(1)
                                        }
                                    }
                                    Ok(Err(e)) => {
                                        let duration = start_time.elapsed();
                                        writeln!(out, "MCP server '{}' unreachable via stdio (error reading output)", name)?;
                                        writeln!(
                                            out,
                                            "  Command: {} {}",
                                            command,
                                            spec.args.join(" ")
                                        )?;
                                        writeln!(out, "  Error: {} (after {:.2?})", e, duration)?;
                                        Ok(1)
                                    }
                                    Err(_) => {
                                        // Timeout reached - the `Child` itself was moved into
                                        // the helper thread above, so it can't be killed via
                                        // `child.kill()` here; signal it by PID instead. The
                                        // helper thread's own `wait_with_output()` reaps it
                                        // once the signal lands, so no separate wait is needed
                                        // on this side.
                                        let duration = start_time.elapsed();
                                        #[cfg(unix)]
                                        {
                                            // SAFETY: child_pid was captured from a live Child
                                            // via `.id()` before the Child was moved; sending
                                            // SIGKILL to a PID that has already exited is a
                                            // harmless no-op (ESRCH), not undefined behavior.
                                            let rc = unsafe {
                                                libc::kill(child_pid as i32, libc::SIGKILL)
                                            };
                                            if rc != 0 {
                                                eprintln!(
                                                    "Failed to kill child process {child_pid}: {}",
                                                    std::io::Error::last_os_error()
                                                );
                                            }
                                        }
                                        #[cfg(not(unix))]
                                        {
                                            eprintln!(
                                                "cannot forcibly kill child process {child_pid} on this platform; it may continue running in the background"
                                            );
                                        }
                                        writeln!(
                                            out,
                                            "MCP server '{}' unreachable via stdio (timeout)",
                                            name
                                        )?;
                                        writeln!(
                                            out,
                                            "  Command: {} {}",
                                            command,
                                            spec.args.join(" ")
                                        )?;
                                        writeln!(out, "  Timeout after {:.2?}", duration)?;
                                        Ok(1)
                                    }
                                };

                                result
                            } else {
                                writeln!(
                                    out,
                                    "MCP server '{}' has no command for stdio testing",
                                    name
                                )?;
                                Ok(1)
                            }
                        }
                        McpTransportKind::Unknown => {
                            writeln!(
                                out,
                                "MCP server '{}' has unknown transport type and cannot be tested",
                                name
                            )?;
                            Ok(1)
                        }
                    }
                }
                None => {
                    // Server not found - list available servers and show error
                    if specs.is_empty() {
                        writeln!(out, "no MCP servers configured in {}.", path.display())?;
                        writeln!(
                            out,
                            "add them under [mcp_servers.<name>] in the engine config."
                        )?;
                    } else {
                        writeln!(out, "no MCP server named '{}' found.", name)?;
                        writeln!(out, "available servers:")?;
                        for spec in &specs {
                            writeln!(out, "  - {}", spec.name)?;
                        }
                    }
                    Ok(1)
                }
            }
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

    // Load WITHOUT the validate-and-bail baked into `Config::load` so that
    // *this* function is the authority on how config problems are reported and
    // which exit code `--validate` produces. A `Config::load()?` here would
    // propagate an Err on any validation problem — hard-erroring with a
    // generic message and burying the `config problems:` printout + the
    // `--validate` exit branch below as dead code (C3-1).
    //
    // A genuinely unreadable/unparseable config (missing home, bad JSON, a
    // trailing comma, …) comes back as an Err; surface it as a single config
    // problem the same graceful way `mcp list` does, rather than a raw serde
    // backtrace (C3-2).
    let loaded = Config::load_unvalidated();
    let errs = configure_problems(loaded.as_ref());

    if errs.is_empty() {
        // Safe to unwrap: an Ok load with no validate() problems.
        let cfg = loaded.expect("no problems implies Ok(cfg)");
        println!("\nproviders ({}):", cfg.providers.len());
        for p in &cfg.providers {
            let kind = if p.paid { "paid" } else { "free" };
            println!("  {:12} {:8} {}", p.id, kind, p.base_url);
        }
        println!("\nconfig OK");
        return Ok(());
    }

    eprintln!("\nconfig problems:");
    for e in &errs {
        eprintln!("  - {e}");
    }
    match configure_exit_decision(&errs, validate) {
        ConfigureOutcome::Exit(code) => std::process::exit(code),
        ConfigureOutcome::Ok => Ok(()),
    }
}

/// What `configure` should do once the config problems (if any) have been
/// printed. Split out from [`cmd_configure`] so the exit *decision* is unit
/// testable without spawning a process or capturing `std::process::exit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigureOutcome {
    /// Return `Ok(())` from `configure` (exit code 0).
    Ok,
    /// Terminate the process with this exit code.
    Exit(i32),
}

/// Map a `Config::load_unvalidated()` result into the ordered list of config
/// problems that `configure` should print. An empty list means the config is
/// OK. A read/parse failure (bad JSON, missing home, trailing comma, …) is
/// rendered as a single problem — the whole anyhow chain via `{:#}` — rather
/// than hard-erroring the command (mirrors the graceful `mcp list` path).
pub(crate) fn configure_problems(loaded: Result<&Config, &anyhow::Error>) -> Vec<String> {
    match loaded {
        Ok(cfg) => cfg.validate(),
        Err(e) => vec![format!("{e:#}")],
    }
}

/// Given the printed problem list and the `--validate` flag, decide how
/// `configure` should terminate. With `--validate`, any problem is a nonzero
/// exit; without it, the command still returns success (problems are advisory).
/// An empty problem list is always `Ok`.
pub(crate) fn configure_exit_decision(problems: &[String], validate: bool) -> ConfigureOutcome {
    if problems.is_empty() {
        ConfigureOutcome::Ok
    } else if validate {
        ConfigureOutcome::Exit(1)
    } else {
        ConfigureOutcome::Ok
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
    fn run_capturing(action: &McpCmd, path: &Path) -> (String, anyhow::Result<i32>) {
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

    /// `mcp get <name>` tests
    #[test]
    fn mcp_get_existing_server_json() {
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

        let (out, res) = run_capturing(
            &McpCmd::Get {
                name: "lookup".to_string(),
                json: true,
            },
            &path,
        );
        res.expect("cmd_mcp_write");

        let spec: zoder_core::McpServerSpec =
            serde_json::from_str(&out).expect("json parses to McpServerSpec");
        assert_eq!(spec.name, "lookup");
        assert_eq!(spec.transport, zoder_core::McpTransportKind::Stdio);
        assert_eq!(spec.command.as_deref(), Some("node"));
        assert_eq!(spec.args, vec!["server.js".to_string()]);
        assert!(spec.url.is_none());
    }

    #[test]
    fn mcp_get_existing_server_human() {
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

        let (out, res) = run_capturing(
            &McpCmd::Get {
                name: "lookup".to_string(),
                json: false,
            },
            &path,
        );
        res.expect("cmd_mcp_write");

        assert!(
            out.contains("MCP server 'lookup' in"),
            "output should contain server info; got:\n{out}"
        );
        assert!(
            out.contains("name:         lookup"),
            "output should contain name; got:\n{out}"
        );
        assert!(
            out.contains("transport:    stdio"),
            "output should contain transport info; got:\n{out}"
        );
    }

    #[test]
    fn mcp_get_missing_server_error() {
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

        let (out, res) = run_capturing(
            &McpCmd::Get {
                name: "nonexistent".to_string(),
                json: false,
            },
            &path,
        );
        res.expect("cmd_mcp_write");

        assert!(
            out.contains("no MCP server named 'nonexistent' found"),
            "output should contain error message; got:\n{out}"
        );
        assert!(
            out.contains("available servers:"),
            "output should list available servers; got:\n{out}"
        );
        assert!(
            out.contains("- lookup"),
            "output should list lookup server; got:\n{out}"
        );
        assert!(
            out.contains("- mcp_kiwi_com"),
            "output should list mcp_kiwi_com server; got:\n{out}"
        );
    }

    #[test]
    fn mcp_get_no_servers_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            r#"
[profile]
primary_model = "openai/gpt-4o"
"#,
        );

        let (out, res) = run_capturing(
            &McpCmd::Get {
                name: "nonexistent".to_string(),
                json: false,
            },
            &path,
        );
        res.expect("cmd_mcp_write");

        assert!(
            out.contains("no MCP servers configured in"),
            "output should indicate no servers configured; got:\n{out}"
        );
    }

    /// Test command unit tests
    #[test]
    fn mcp_test_unreachable_http_returns_promptly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            r#"
[mcp_servers.test_http]
url = "http://127.0.0.1:1"
"#,
        );

        let start_time = std::time::Instant::now();
        let (out, res) = run_capturing(
            &McpCmd::Test {
                name: "test_http".to_string(),
            },
            &path,
        );
        let duration = start_time.elapsed();

        res.expect("cmd_mcp_write");
        // Should return quickly (less than 10 seconds)
        assert!(
            duration < std::time::Duration::from_secs(10),
            "Test should return promptly, not wait for timeout"
        );
        // Should report the unreachable HTTP server
        assert!(
            out.contains("unreachable via HTTP"),
            "output should indicate unreachable HTTP server; got:\n{out}"
        );
        // NOTE: no separate "!out.contains(\"reachable via HTTP\")" check here --
        // "unreachable via HTTP" itself contains "reachable via HTTP" as a
        // substring, so that assertion was always false. The positive
        // assertion above already pins the exact message.
    }

    #[test]
    fn mcp_test_unreachable_stdio_returns_promptly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            r#"
[mcp_servers.test_stdio]
command = "sleep"
args = ["10"]
"#,
        );

        let start_time = std::time::Instant::now();
        let (out, res) = run_capturing(
            &McpCmd::Test {
                name: "test_stdio".to_string(),
            },
            &path,
        );
        let duration = start_time.elapsed();

        res.expect("cmd_mcp_write");
        // Should return quickly (less than 10 seconds)
        assert!(
            duration < std::time::Duration::from_secs(10),
            "Test should return promptly, not wait for timeout"
        );
        // Should report the unreachable stdio server
        assert!(
            out.contains("unreachable via stdio"),
            "output should indicate unreachable stdio server; got:\n{out}"
        );
        // NOTE: no separate "!out.contains(\"reachable via stdio\")" check here --
        // same substring gotcha as the HTTP test above.
    }

    #[test]
    fn mcp_test_missing_server_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            r#"
[mcp_servers.lookup]
command = "node"
args = ["server.js"]
"#,
        );

        let (out, res) = run_capturing(
            &McpCmd::Test {
                name: "nonexistent".to_string(),
            },
            &path,
        );
        res.expect("cmd_mcp_write");

        assert!(
            out.contains("no MCP server named 'nonexistent' found"),
            "output should contain error message; got:\n{out}"
        );
        assert!(
            out.contains("available servers:"),
            "output should list available servers; got:\n{out}"
        );
        assert!(
            out.contains("- lookup"),
            "output should list lookup server; got:\n{out}"
        );
    }

    #[test]
    fn mcp_test_unknown_transport_reports_untestable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            r#"
[mcp_servers.test_unknown]
# `type` is set to an unrecognized value so `build_spec` treats this as a
# real (if malformed) server entry rather than rejecting it outright --
# a table with truly zero transport-hint fields (no type/command/cmd/url/uri)
# is deliberately rejected upstream as "not a server entry at all" and never
# reaches this command, so this is the only way to exercise the Unknown
# transport branch of `mcp test`.
type = "custom"
"#,
        );

        let (out, res) = run_capturing(
            &McpCmd::Test {
                name: "test_unknown".to_string(),
            },
            &path,
        );
        res.expect("cmd_mcp_write");

        assert!(
            out.contains("has unknown transport type"),
            "output should indicate unknown transport; got:\n{out}"
        );
    }
}

// ---------------------------------------------------------------------------
// configure tests — the problem-reporting + exit-decision logic behind
// `zoder configure [--validate]`. Testing the pure helpers (not the process)
// so `--validate`'s nonzero exit is verified without spawning a subprocess.
//
// C3-1: an invalid config must reach the problem-list + exit branch (it used
//        to be dead code behind a bailing `Config::load()?`).
// C3-2: a malformed config.json must be reported as one graceful problem, not
//        a raw serde backtrace / hard bail.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod configure_tests {
    use super::*;

    #[test]
    fn valid_config_yields_no_problems_and_ok_exit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{
                "providers": [{
                    "id": "acme",
                    "base_url": "https://gw.acme.example/v1",
                    "kind": "openai-chat",
                    "auth": {"type": "none"}
                }],
                "corpus_path": "/tmp/zoder-c3b/corpus.json",
                "ledger_path": "/tmp/zoder-c3b/ledger.json",
                "health_path": "/tmp/zoder-c3b/health.json",
                "default_provider": "acme"
            }"#,
        )
        .unwrap();
        let loaded = Config::load_unvalidated_from(dir.path());
        let problems = configure_problems(loaded.as_ref());
        assert!(
            problems.is_empty(),
            "valid config has no problems: {problems:?}"
        );
        // Both with and without --validate: OK.
        assert_eq!(
            configure_exit_decision(&problems, true),
            ConfigureOutcome::Ok
        );
        assert_eq!(
            configure_exit_decision(&problems, false),
            ConfigureOutcome::Ok
        );
    }

    #[test]
    fn invalid_config_with_validate_exits_nonzero() {
        // C3-1: duplicate provider id -> problem list is non-empty AND
        // --validate must select a nonzero process exit. Previously this whole
        // branch was unreachable because Config::load()? bailed first.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{
                "providers": [
                    {"id": "dup", "base_url": "https://a.example/v1", "kind": "openai-chat", "auth": {"type": "none"}},
                    {"id": "dup", "base_url": "https://b.example/v1", "kind": "openai-chat", "auth": {"type": "none"}}
                ],
                "corpus_path": "/tmp/zoder-c3b/corpus.json",
                "ledger_path": "/tmp/zoder-c3b/ledger.json",
                "health_path": "/tmp/zoder-c3b/health.json",
                "default_provider": "dup"
            }"#,
        )
        .unwrap();
        let loaded = Config::load_unvalidated_from(dir.path());
        let problems = configure_problems(loaded.as_ref());
        assert!(
            problems.iter().any(|e| e.contains("duplicate provider id")),
            "invalid config must produce a problem list: {problems:?}"
        );
        match configure_exit_decision(&problems, true) {
            ConfigureOutcome::Exit(code) => assert_eq!(code, 1),
            other => panic!("--validate on an invalid config must exit nonzero, got {other:?}"),
        }
    }

    #[test]
    fn invalid_config_without_validate_does_not_hard_bail() {
        // C3-1: WITHOUT --validate the same invalid config must NOT hard-error
        // out of load; the command reports problems and returns success.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{ "providers": [], "default_provider": "", "corpus_path": "/tmp/zoder-c3b/corpus.json", "ledger_path": "/tmp/zoder-c3b/ledger.json", "health_path": "/tmp/zoder-c3b/health.json" }"#,
        )
        .unwrap();
        let loaded = Config::load_unvalidated_from(dir.path());
        assert!(
            loaded.is_ok(),
            "an invalid-but-parseable config must not bail out of load"
        );
        let problems = configure_problems(loaded.as_ref());
        assert!(
            problems
                .iter()
                .any(|e| e.contains("no providers configured")),
            "problems should be reported: {problems:?}"
        );
        assert_eq!(
            configure_exit_decision(&problems, false),
            ConfigureOutcome::Ok,
            "without --validate the command still returns success"
        );
    }

    #[test]
    fn malformed_json_config_is_reported_gracefully() {
        // C3-2: a trailing comma makes config.json unparseable. It must come
        // back as ONE readable problem (mirroring `mcp list`), not a bail.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{
                "providers": [
                    {"id": "x", "base_url": "https://a.example/v1", "kind": "openai-chat", "auth": {"type": "none"}},
                ],
                "default_provider": "x"
            }"#,
        )
        .unwrap();
        let loaded = Config::load_unvalidated_from(dir.path());
        assert!(loaded.is_err(), "malformed JSON must be an Err from load");
        let problems = configure_problems(loaded.as_ref());
        assert_eq!(problems.len(), 1, "one graceful problem: {problems:?}");
        assert!(
            problems[0].contains("parsing zoder config at"),
            "problem should name the parse failure + path: {problems:?}"
        );
        // --validate on a malformed config still exits nonzero.
        match configure_exit_decision(&problems, true) {
            ConfigureOutcome::Exit(code) => assert_eq!(code, 1),
            other => panic!("malformed config + --validate must exit nonzero, got {other:?}"),
        }
    }
}

/// Write the final assistant message `content` to `path`, creating any
/// missing parent directories. Used by `zoder run --output-last-message`
/// so CI/supervisors can read the result from a file instead of stdout.
pub(crate) fn write_last_message(path: &str, content: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    let p = std::path::Path::new(path);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent dir for {path}"))?;
        }
    }
    std::fs::write(p, content).with_context(|| format!("writing last message to {path}"))?;
    Ok(())
}

#[cfg(test)]
mod goose_tests {
    /// DEFECT: `write_last_message` must create parent directories
    /// when they don't exist (the --output-last-message flag
    /// can write to arbitrary paths, including nested ones).
    #[test]
    fn write_last_message_creates_parent_and_writes_output_last_message() {
        let dir = std::env::temp_dir().join(format!(
            "zoder-olm-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let target = dir.join("nested").join("last.txt");
        let content = "final assistant message body";
        super::write_last_message(target.to_str().unwrap(), content).unwrap();
        let got = std::fs::read_to_string(&target).unwrap();
        assert_eq!(got, content);
        std::fs::remove_dir_all(&dir).ok();
    }
}
