//! End-to-end probe of the live review-routing fleet.
//!
//! Exercises the actual review dispatch path against:
//!   * Local TYDEUS qwen38  — `http://192.168.207.73:8006/v1`
//!   * Local TYDEUS nemotron35 — `http://192.168.207.73:8002/v1`
//!   * Approved free NVIDIA EIH — `https://integrate.api.nvidia.com/v1`
//!     (requires `NVIDIA_API_KEY` in the environment).
//!
//! Gated by env vars so CI without fleet access doesn't fail:
//!   * `ZODER_FLEET_PROBE=1` enables the probe; otherwise it no-ops with
//!     a "skipped" diagnostic and exits 0.
//!   * `ZODER_FLEET_PROBE_FLEET_ONLY=1` skips the NVIDIA EIH arm even
//!     when the API key is set (useful when the host has no internet).
//!
//! Records actual provider/model, verdict, timing, and any failures to a
//! JSON line on stdout (one per model). The bounded test harness pipes
//! the JSON into the report path that lives at
//! `/mnt/datapool/projects/ncz-session-2026-09-30/reports/zoder-review-path.md`
//! on ARGONAS. The probe itself is the source of truth; downstream
//! tooling can rerun it on demand.
//!
//! Wall-clock budgets per model are tight (`ZODER_FLEET_PROBE_BUDGET_S`,
//! default 30s) so a wedged provider never burns the harness. A model
//! that doesn't return within the budget is recorded as a timeout, NOT
//! as a successful review — failing closed is the rule.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// One row in the probe's report. Serialized as JSON Lines so the
/// downstream ARGONAS report parser can ingest the probe's output
/// directly without re-parsing free-form text.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProbeRow {
    /// Logical endpoint label (e.g. "tydeus_qwen38").
    endpoint: String,
    /// Model id actually resolved by `complete_once`.
    resolved_model: String,
    /// URL of the upstream endpoint.
    upstream_url: String,
    /// Verdict string parse_review produced.
    verdict: String,
    /// Wall-clock duration of the dispatch, in milliseconds.
    elapsed_ms: u128,
    /// Whether the dispatch succeeded.
    success: bool,
    /// Diagnostic message — present whenever `success == false`.
    error: Option<String>,
    /// First 240 chars of the model output, when available.
    excerpt: Option<String>,
    /// ISO-8601 UTC timestamp of the probe row.
    timestamp: String,
}

/// Run `zoder` (built from this workspace) against a model on a live
/// endpoint. Uses the live installed `zoder` binary if found, else
/// falls back to `cargo run --bin zoder` against the workspace at
/// `out_dir`.
fn run_zoder_review(
    cli_bin: &PathBuf,
    workspace: &PathBuf,
    model: &str,
    extra_env: &[(&str, &str)],
    budget: Duration,
) -> std::io::Result<std::process::Output> {
    let mut cmd = if cli_bin.exists() {
        let mut c = std::process::Command::new(cli_bin);
        c.arg("-C")
            .arg(workspace.join("repo"))
            .arg("review")
            .arg("--scope")
            .arg("working-tree")
            .arg("--agent")
            .arg("reviewer")
            .arg("--oneshot")
            .arg("--approve")
            .arg("none")
            .arg("-m")
            .arg(model);
        c
    } else {
        let mut c = std::process::Command::new("cargo");
        c.arg("run")
            .arg("--quiet")
            .arg("--bin")
            .arg("zoder")
            .arg("--")
            .arg("-C")
            .arg(workspace.join("repo"))
            .arg("review")
            .arg("--scope")
            .arg("working-tree")
            .arg("--agent")
            .arg("reviewer")
            .arg("--oneshot")
            .arg("--approve")
            .arg("none")
            .arg("-m")
            .arg(model);
        c.current_dir(workspace);
        c
    };
    cmd.env("ZODER_HOME", workspace)
        .env("ZEROCLAW_CONFIG_DIR", workspace.join("engine"))
        // Pin the review cwd to the test repo, NOT the ZODER_HOME
        // directory. Without `-C` the harness walks up to find a git
        // repo and may land on the operator's checkout instead of the
        // staged probe repo — which can balloon the diff above the
        // 9000-byte review chunk limit.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let child = cmd.spawn()?;
    let started = Instant::now();
    // Bounded wait: poll exit_status with a 200ms tick. If we exceed
    // `budget`, kill the child and return its (truncated) output.
    let mut child = child;
    loop {
        match child.try_wait()? {
            Some(status) => {
                let mut out = std::process::Output {
                    status,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                };
                // Best-effort: drain pipes.
                if let Some(mut stdout) = child.stdout.take() {
                    use std::io::Read;
                    let _ = stdout.read_to_end(&mut out.stdout);
                }
                if let Some(mut stderr) = child.stderr.take() {
                    use std::io::Read;
                    let _ = stderr.read_to_end(&mut out.stderr);
                }
                return Ok(out);
            }
            None => {
                if started.elapsed() > budget {
                    let _ = child.kill();
                    let _ = child.wait();
                    let out = std::process::Output {
                        status: std::process::ExitStatus::default(),
                        stdout: Vec::new(),
                        stderr: format!(
                            "probe-budget-exceeded: killed after {}s\n",
                            budget.as_secs()
                        )
                        .into_bytes(),
                    };
                    return Ok(out);
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn parse_verdict_from_output(stdout: &[u8]) -> (String, Option<String>) {
    let text = String::from_utf8_lossy(stdout);
    let stripped = strip_ansi(&text);
    let verdict_line = stripped
        .lines()
        .find(|l| l.trim_start().starts_with("verdict:"))
        .map(|l| {
            l.trim_start()
                .trim_start_matches("verdict:")
                .trim()
                .to_string()
        })
        .unwrap_or_default();
    let model_line = stripped
        .lines()
        .find(|l| l.contains(" :: "))
        .map(|l| l.to_string());
    (verdict_line, model_line)
}

fn strip_ansi(s: &str) -> String {
    // Minimal ANSI-stripper: CSI sequences ESC [ ... letter.
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Probe a single endpoint and return a ProbeRow. The caller decides
/// which endpoints to probe; this fn does the actual dispatch.
fn probe_endpoint(
    endpoint: String,
    upstream_url: String,
    model: String,
    extra_env: Vec<(&'static str, &'static str)>,
    cli_bin: &PathBuf,
    workspace: &PathBuf,
    budget: Duration,
) -> ProbeRow {
    let started = Instant::now();
    let now = chrono::Utc::now().to_rfc3339();
    let result = run_zoder_review(cli_bin, workspace, &model, &extra_env, budget);
    let elapsed_ms = started.elapsed().as_millis();
    match result {
        Ok(out) => {
            let stdout = &out.stdout;
            let stderr = String::from_utf8_lossy(&out.stderr);
            let (verdict, model_excerpt) = parse_verdict_from_output(stdout);
            let ok = out.status.success()
                && !verdict.is_empty()
                && !stderr.contains("probe-budget-exceeded");
            ProbeRow {
                endpoint,
                resolved_model: model,
                upstream_url,
                verdict,
                elapsed_ms,
                success: ok,
                error: if ok {
                    None
                } else {
                    Some(format!(
                        "exit={:?} stderr={}",
                        out.status.code(),
                        stderr.chars().take(400).collect::<String>()
                    ))
                },
                excerpt: model_excerpt.map(|m| m.chars().take(240).collect::<String>()),
                timestamp: now,
            }
        }
        Err(e) => ProbeRow {
            endpoint,
            resolved_model: model,
            upstream_url,
            verdict: String::new(),
            elapsed_ms,
            success: false,
            error: Some(format!("io: {e}")),
            excerpt: None,
            timestamp: now,
        },
    }
}

fn main() -> anyhow::Result<()> {
    // Skip-the-probe default: if ZODER_FLEET_PROBE is unset, emit one
    // JSON-Lines row marking the probe as skipped so downstream tooling
    // can distinguish "ran and passed" from "didn't run at all".
    if std::env::var("ZODER_FLEET_PROBE").ok().as_deref() != Some("1") {
        let row = ProbeRow {
            endpoint: "skipped".into(),
            resolved_model: String::new(),
            upstream_url: String::new(),
            verdict: String::new(),
            elapsed_ms: 0,
            success: true,
            error: Some("ZODER_FLEET_PROBE!=1; probe skipped".into()),
            excerpt: None,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };
        println!("{}", serde_json::to_string(&row)?);
        return Ok(());
    }

    // Locate the workspace's CLI binary (a sibling of `Cargo.toml`).
    let workspace = std::env::current_dir()?;
    let cli_bin = workspace.join("target/release/zoder");
    let cli_bin = if cli_bin.exists() {
        cli_bin
    } else {
        workspace.join("target/debug/zoder")
    };

    let budget = Duration::from_secs(
        std::env::var("ZODER_FLEET_PROBE_BUDGET_S")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30),
    );

    // Stand up an isolated ZODER_HOME in a tempdir so the probe
    // doesn't disturb the operator's `~/.zoder/`. We avoid the
    // `tempfile` crate here because this is a release-mode bin (not
    // a `#[cfg(test)]` module), so we synthesize a tempdir path from
    // `std::env::temp_dir()` plus a UUID-ish suffix and `mkdir` it
    // ourselves. The directory is best-effort cleaned up at exit.
    let probe_suffix = format!(
        "zoder-fleet-probe-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    );
    let home = std::env::temp_dir().join(probe_suffix);
    std::fs::create_dir_all(&home)?;
    let home_for_cleanup = home.clone();
    std::env::set_var("ZODER_HOME", &home);
    std::env::set_var("ZEROCLAW_CONFIG_DIR", home.join("engine"));

    // Stage a config.toml that maps the three probed endpoints to
    // distinct providers, each carrying the right `serves` prefix and
    // the EIH key when present. This is the *test* half of the probe;
    // the operator's ~/.zoder is untouched.
    let engine_dir = home.join("engine");
    std::fs::create_dir_all(&engine_dir)?;
    // Engine config (zeroclaw reads `config.toml`). Kept SEPARATE from
    // the zoder-side `config.json` so neither loader tries to parse
    // the other surface — zoder#15/#16/#17 bit hard on that overlap.
    let mut engine_config = String::from(
        r#"
schema_version = 3

[risk_profiles.default]
level = "full"
workspace_only = true
allowed_commands = ["*"]
allowed_roots = ["/"]
forbidden_paths = []
auto_approve = ["*"]

[runtime_profiles.guest]
agentic = true
agentic_timeout_secs = 60
max_actions_per_hour = 200
max_cost_per_day_cents = 100
max_delegation_depth = 0
max_tool_iterations = 50
shell_timeout_secs = 30

[providers.models.tydeus.qwen38]
type = "openai-compatible"
model = "qwen38"
uri = "http://192.168.207.73:8006/v1"
chat_template_kwargs = { force_nonempty_content = true }

[providers.models.tydeus.nemotron35]
type = "openai-compatible"
model = "nemotron35"
uri = "http://192.168.207.73:8002/v1"
chat_template_kwargs = { force_nonempty_content = true }

[agents.reviewer]
model_provider = "tydeus.qwen38"
"#,
    );
    // NOTE: the NVIDIA_EIH push below appends more sections to BOTH
    // `engine_config` and `fleet_probe_overlay` BEFORE either is
    // written to disk. The two `std::fs::write` calls therefore live
    // AFTER the NVIDIA block, not at the end of this block.

    // zoder-side `config.json`. The vendor overlay convention
    // (`config.<vendor>.toml`) lets a probe include its providers
    // without colliding with the operator's `config.toml` surface.
    // `[[providers]]` rows MUST carry `engine_provider_ref` so the
    // dispatcher resolves the engine model id (e.g. `qwen38`) instead
    // of dispatching the alias to a non-existent backend. We don't
    // list any providers directly in config.json — they all live in
    // the overlay below.
    let zoder_config = format!(
        r#"{{
  "providers": [],
  "default_provider": "tydeus-qwen38",
  "strict_free": false,
  "corpus_path": "{}",
  "ledger_path": "{}",
  "health_path": "{}",
  "reviewer_model": "qwen38"
}}
"#,
        home.join("model_corpus.json").display(),
        home.join("ledger.jsonl").display(),
        home.join("health.json").display(),
    );
    std::fs::write(home.join("config.json"), zoder_config)?;

    // Vendor overlay (`config.fleet_probe.toml`) — zoder picks this
    // up automatically via the `config.<vendor>.toml` glob, no need
    // for an explicit list in config.json.
    let mut fleet_probe_overlay = String::from(
        r#"[[providers]]
id = "tydeus-qwen38"
engine_provider_ref = "tydeus.qwen38"
base_url = "http://192.168.207.73:8006/v1"
kind = "openai-chat"
auth = { type = "bearer", token = "local-none" }
paid = false
billing = "free"
serves = ["qwen38"]

[[providers]]
id = "tydeus-nemotron35"
engine_provider_ref = "tydeus.nemotron35"
base_url = "http://192.168.207.73:8002/v1"
kind = "openai-chat"
auth = { type = "bearer", token = "local-none" }
paid = false
billing = "free"
serves = ["nemotron35"]
"#,
    );
    if let Ok(api_key) = std::env::var("NVIDIA_API_KEY") {
        if !api_key.is_empty() {
            // Engine-side registry (zeroclaw reads this so the
            // `model_provider` resolution surfaces a real provider).
            engine_config.push_str(
                r#"
[providers.models.nvidia_eih.nemotron_super]
type = "openai-compatible"
model = "nvidia/nemotron-3-super-120b-a12b"
uri = "https://integrate.api.nvidia.com/v1"
chat_template_kwargs = { force_nonempty_content = true }
api_key_env = "NVIDIA_API_KEY"
"#,
            );
            // zoder-side provider row (in fleet_probe overlay, with
            // the EIH key resolved at probe start).
            fleet_probe_overlay.push_str(&format!(
                r#"
[[providers]]
id = "nvidia-eih"
engine_provider_ref = "nvidia_eih.nemotron_super"
base_url = "https://integrate.api.nvidia.com/v1"
kind = "openai-chat"
auth = {{ type = "bearer", token = "{api_key}" }}
paid = false
billing = "free"
serves = ["nvidia/nemotron-3-super-120b-a12b"]
"#
            ));
            // Stand-in env so `Engine::load()` doesn't need the API key
            // re-resolved.
            std::env::set_var("NVIDIA_API_KEY", api_key);
        }
    }
    std::fs::write(engine_dir.join("config.toml"), engine_config.as_bytes())?;
    std::fs::write(
        home.join("config.fleet_probe.toml"),
        fleet_probe_overlay.as_bytes(),
    )?;

    // Probes to run, in order. Declared before the corpus writing so
    // every probed model id ends up in the corpus with `free=true`.
    type ProbeSpec = (String, String, String, Vec<(&'static str, &'static str)>);
    let mut probes: Vec<ProbeSpec> = vec![
        (
            "tydeus_qwen38".into(),
            "http://192.168.207.73:8006/v1".into(),
            "qwen38".into(),
            vec![],
        ),
        (
            "tydeus_nemotron35".into(),
            "http://192.168.207.73:8002/v1".into(),
            "nemotron35".into(),
            vec![],
        ),
    ];
    if std::env::var("NVIDIA_API_KEY")
        .ok()
        .filter(|v| !v.is_empty())
        .is_some()
        && std::env::var("ZODER_FLEET_PROBE_FLEET_ONLY")
            .ok()
            .as_deref()
            != Some("1")
    {
        probes.push((
            "nvidia_eih_nemotron_super".into(),
            "https://integrate.api.nvidia.com/v1".into(),
            "nvidia/nemotron-3-super-120b-a12b".into(),
            vec![],
        ));
    }

    // Stage a minimal model_corpus.json so the policy gate admits
    // every probed model. Mark each as `free=true` so a strict_free
    // config doesn't reject them with a paid-confirm.
    let corpus_ids: Vec<&str> = probes
        .iter()
        .map(|(_, _, model, _)| model.as_str())
        .collect();
    let mut seen = std::collections::HashSet::new();
    let corpus_models: Vec<serde_json::Value> = corpus_ids
        .into_iter()
        .filter(|id| seen.insert(id.to_string()))
        .map(|id| {
            serde_json::json!({
                "id": id,
                "free": true,
                "routable": true,
            })
        })
        .collect();
    std::fs::write(
        home.join("model_corpus.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "source": "fleet_probe",
            "models": corpus_models,
        }))
        .unwrap(),
    )?;

    // Probes to run, in order. Already declared above (so the corpus
    // can include every probed model id); the actual `for (endpoint,
    // url, model, envs) in probes` loop below drives the dispatch.

    // Stage a minimal git workspace so `zoder review --scope working-tree`
    // has a non-empty diff to review. Keep the diff under REVIEW_CHUNK_BYTES
    // (9000) so the reviewer's chunking pass doesn't reject it as an
    // oversized hunk.
    let repo = home.join("repo");
    std::fs::create_dir_all(&repo)?;
    let init_status = std::process::Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "init", "-q"])
        .status();
    if !matches!(init_status, Ok(s) if s.success()) {
        anyhow::bail!("git init failed in {}", repo.display());
    }
    std::process::Command::new("git")
        .args([
            "-C",
            repo.to_str().unwrap(),
            "-c",
            "user.email=probe@example.invalid",
            "-c",
            "user.name=probe",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "init",
        ])
        .status()?;
    std::fs::write(repo.join("README.md"), "probe diff for review routing\n")?;
    std::process::Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "add", "README.md"])
        .status()?;

    // Run the actual probes with the live workspace.
    let mut all_ok = true;
    for (endpoint, url, model, envs) in probes {
        let row = probe_endpoint(endpoint, url, model, envs, &cli_bin, &home, budget);
        if !row.success {
            all_ok = false;
        }
        println!("{}", serde_json::to_string(&row)?);
    }

    if !all_ok {
        // Best-effort cleanup UNLESS the operator asked us to keep
        // the staged home around for post-mortem (`ZODER_FLEET_PROBE_KEEP=1`).
        if std::env::var("ZODER_FLEET_PROBE_KEEP").ok().as_deref() != Some("1") {
            let _ = std::fs::remove_dir_all(&home_for_cleanup);
        } else {
            eprintln!(
                "[fleet_probe] ZODER_FLEET_PROBE_KEEP=1: staged home kept at {}",
                home_for_cleanup.display()
            );
        }
        std::process::exit(2);
    }
    // Best-effort cleanup on success too.
    if std::env::var("ZODER_FLEET_PROBE_KEEP").ok().as_deref() != Some("1") {
        let _ = std::fs::remove_dir_all(&home_for_cleanup);
    }
    Ok(())
}
