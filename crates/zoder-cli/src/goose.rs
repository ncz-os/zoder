//! Goose-parity command surface: `session` (interactive UI), `run` (headless
//! agentic), `recipe` (saved templates), `mcp` (list engine extensions), and
//! `configure`. These are thin wrappers over the agentic engine + config that
//! `exec`/`tui` already provide, so behavior and cost accounting stay uniform.

use std::path::PathBuf;

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

use zoder_core::Config;

use crate::{McpCmd, RecipeCmd};

// ---------------------------------------------------------------------------
// run: headless agentic (goose `run`).
// ---------------------------------------------------------------------------

pub(crate) async fn cmd_run(
    cli: &crate::Cli,
    text: Option<String>,
    instructions: Option<String>,
    _background: bool,
) -> anyhow::Result<()> {
    let task = match (text, instructions) {
        (Some(t), _) => t,
        (None, Some(file)) => std::fs::read_to_string(&file)
            .with_context(|| format!("reading instructions file {file:?}"))?,
        (None, None) => crate::read_prompt(None)?, // stdin / -
    };
    // `run` is a headless agentic execution; background detachment is provided by
    // the codex-surface job registry (`rescue --background`). Here we run inline.
    crate::cmd_exec_agentic(cli, Some(task)).await
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

/// Engine config file: `<engine_config_dir>/config.toml`.
fn engine_config_file() -> PathBuf {
    crate::zeroclaw_data_dir()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default()
        .join("config.toml")
}

pub(crate) fn cmd_mcp(_cli: &crate::Cli, action: &McpCmd) -> anyhow::Result<()> {
    match action {
        McpCmd::List => {
            let path = engine_config_file();
            let Ok(raw) = std::fs::read_to_string(&path) else {
                println!("engine config not found at {}", path.display());
                return Ok(());
            };
            // No TOML dependency in the CLI: scan section headers for MCP/extension
            // tables (e.g. `[mcp_servers.foo]`, `[[mcp]]`, `[extensions.bar]`).
            let mut names: Vec<String> = Vec::new();
            for line in raw.lines() {
                let l = line.trim();
                let inner = l.trim_start_matches('[').trim_end_matches(']').trim();
                let lower = inner.to_ascii_lowercase();
                if (l.starts_with('['))
                    && (lower.starts_with("mcp") || lower.starts_with("extension"))
                {
                    names.push(inner.to_string());
                }
            }
            if names.is_empty() {
                println!("no MCP extensions configured in {}", path.display());
                println!("add them under [mcp_servers.<name>] in the engine config.");
            } else {
                println!("MCP extensions in {}:", path.display());
                for n in names {
                    println!("  {n}");
                }
            }
            Ok(())
        }
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
