//! Codex-compatible command surface: `review`, `adversarial-review`, `rescue`,
//! `transfer`, and a file-backed background job registry (`status`/`result`/
//! `cancel`). Reviews run as single completions over a chosen model (the diff is
//! embedded), with optional multi-reviewer fan-out; `rescue` is an agentic,
//! write-capable run. Everything routes through the same provider/engine + cost
//! ledger as `exec`, so spend is captured uniformly.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use zoder_core::{
    BillingMode, ChatRequest, Config, Decision, Entry, HealthStore, Ledger, Message, ModelEntry,
    OpenAiProvider, PolicyGate, PricingCatalog,
};

use crate::{Engine, ReviewScope};

// ---------------------------------------------------------------------------
// Single completion (used by review/adversarial-review).
// ---------------------------------------------------------------------------

/// Result of one reviewer completion.
struct Completion {
    model: String,
    content: String,
    cost_usd: f64,
}

/// Run one non-streamed completion on `model_override` (else the routed/`-m`
/// model), record it in the ledger, and return the text + cost.
async fn complete_once(
    cli: &crate::Cli,
    model_override: Option<&str>,
    system: &str,
    user: &str,
    max_tokens: u32,
) -> anyhow::Result<Completion> {
    let eng = Engine::load()?;

    let model = match model_override {
        Some(m) => m.to_string(),
        None => match &cli.model {
            Some(m) => m.clone(),
            None => {
                // Default reviewer = a strong CROSS-FAMILY model, NOT the author's
                // own. Self-review is weak; and routing the review to the author's
                // flat-subscription provider (env-auth) 401s while the agentic
                // engine authed fine (field report 2026-06-30). A cross-family EIH
                // reviewer routes to the working-auth provider.
                let health = HealthStore::load(&eng.cfg.health_path);
                let (chain, _) = crate::resolve_chain(cli, &eng, &health)?;
                let author = chain.first().cloned().unwrap_or_default();
                crate::default_cross_family_reviewer(&author).to_string()
            }
        },
    };

    // Per-model routing: resolve the provider that actually serves this model
    // (e.g. a pinned MiniMax-M3 -> the minimax provider), not always the default
    // provider — otherwise a reviewer model could be sent to the wrong endpoint.
    let provider_cfg = eng
        .cfg
        .provider_for_model(&model)
        .ok_or_else(|| anyhow!("no provider configured for reviewer model {model}"))?;

    // Gate the reviewer/panel model. Reviewers run non-interactively (panel +
    // fix loop), so a PAID reviewer is REJECTED rather than prompted — pass
    // --allow-paid to use one. Closes the bypass where -m / --reviewer / --panel
    // could spend with no confirmation or free-verification.
    let strict_free = (eng.cfg.strict_free && !cli.lenient_telemetry) || cli.require_free;
    let gate = PolicyGate::new(&eng.cfg, cli.allow_paid, strict_free);
    let model_entry = eng
        .corpus
        .get(&model)
        .cloned()
        .unwrap_or_else(|| ModelEntry {
            id: model.clone(),
            gated_reason: Some("unknown reviewer model: not in corpus, cannot verify free".into()),
            ..Default::default()
        });
    let provider_paid = provider_cfg.paid || provider_cfg.billing != BillingMode::Free;
    if let Decision::NeedConfirm(why) = gate.check(&model_entry, provider_paid) {
        anyhow::bail!(
            "reviewer/panel model '{model}' requires paid spend; pass --allow-paid to use it.\n{why}"
        );
    }

    let messages = vec![Message::new("system", system), Message::new("user", user)];
    let req = ChatRequest {
        model: model.clone(),
        messages,
        max_tokens,
        temperature: 0.1,
        stream: false,
        show_reasoning: false,
        reasoning_effort: cli.reasoning.clone(),
    };
    let provider = OpenAiProvider::new(provider_cfg)?;
    let res = provider
        .stream_chat(&req, None)
        .await
        .map_err(|e| anyhow!("{}", e.message))?;

    let tokens_in = res.prompt_tokens.unwrap_or(0);
    let tokens_out = res.completion_tokens.unwrap_or(res.tokens_out);
    let pricing = PricingCatalog::load(&Config::home().join("pricing.json"));
    let cost = res
        .telemetry
        .cost_usd
        .unwrap_or_else(|| pricing.cost(&model, tokens_in, tokens_out));
    // Post-verify the reviewer call was actually served free (catch a free->paid
    // fallback) and record any violation in the ledger rather than marking clean.
    let violation = gate.verify_free(&model_entry, &res.telemetry).err();
    if let Some(v) = &violation {
        eprintln!("zoder: POLICY VIOLATION (reviewer): {v}");
    }
    let ledger = Ledger::new(&eng.cfg.ledger_path);
    let _ = ledger.record(&Entry {
        ts_utc: Utc::now(),
        provider: provider_cfg.id.clone(),
        model: model.clone(),
        host: zoder_core::ledger::host_of_model(&model),
        tokens_in,
        tokens_out,
        cost_usd: cost,
        calls: 1,
        violation,
    });

    Ok(Completion {
        model,
        content: res.content,
        cost_usd: cost,
    })
}

// ---------------------------------------------------------------------------
// Review output schema (mirrors codex-plugin-cc review-output.schema.json).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Finding {
    #[serde(default)]
    severity: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    location: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ReviewOutput {
    #[serde(default)]
    verdict: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    findings: Vec<Finding>,
    #[serde(default)]
    next_steps: Vec<String>,
}

/// Best-effort parse of a model's reply into a `ReviewOutput`: extract the first
/// balanced-looking `{...}` and decode it; on failure, wrap the raw text.
fn parse_review(raw: &str) -> ReviewOutput {
    let trimmed = raw.trim();
    let candidate = match (trimmed.find('{'), trimmed.rfind('}')) {
        (Some(a), Some(b)) if b > a => &trimmed[a..=b],
        _ => trimmed,
    };
    if let Ok(r) = serde_json::from_str::<ReviewOutput>(candidate) {
        if !r.verdict.is_empty() || !r.summary.is_empty() || !r.findings.is_empty() {
            return r;
        }
    }
    ReviewOutput {
        verdict: "comment".into(),
        summary: "Reviewer did not return structured JSON; raw output preserved below.".into(),
        findings: vec![Finding {
            severity: "info".into(),
            title: "raw review".into(),
            body: trimmed.to_string(),
            location: None,
        }],
        next_steps: vec![],
    }
}

fn verdict_rank(v: &str) -> u8 {
    match v {
        "request_changes" | "reject" | "block" => 2,
        "comment" | "neutral" => 1,
        _ => 0, // approve / unknown
    }
}

// ---------------------------------------------------------------------------
// Git diff acquisition.
// ---------------------------------------------------------------------------

fn run_git(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Resolve a base ref for branch review: explicit `base`, else the upstream's
/// merge-base, else `origin/HEAD`/`main`/`master`, else the root commit.
fn detect_base(cwd: &Path, base: Option<&str>) -> String {
    if let Some(b) = base {
        return b.to_string();
    }
    for cand in [
        "@{upstream}",
        "origin/HEAD",
        "origin/main",
        "main",
        "master",
    ] {
        if let Ok(out) = run_git(cwd, &["merge-base", "HEAD", cand]) {
            let t = out.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    "HEAD".to_string()
}

/// Build the diff for the requested scope. Returns `(label, diff)`.
fn build_diff(
    cwd: &Path,
    scope: ReviewScope,
    base: Option<&str>,
) -> anyhow::Result<(String, String)> {
    let dirty = run_git(cwd, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let effective = match scope {
        ReviewScope::Auto => {
            if dirty {
                ReviewScope::WorkingTree
            } else {
                ReviewScope::Branch
            }
        }
        s => s,
    };
    match effective {
        ReviewScope::WorkingTree => {
            let mut d = run_git(cwd, &["diff", "HEAD"]).unwrap_or_default();
            if d.trim().is_empty() {
                // No tracked changes vs HEAD; fall back to staged + unstaged.
                let staged = run_git(cwd, &["diff", "--cached"]).unwrap_or_default();
                let unstaged = run_git(cwd, &["diff"]).unwrap_or_default();
                d = format!("{staged}\n{unstaged}");
            }
            Ok(("working-tree".into(), d))
        }
        ReviewScope::Branch => {
            let b = detect_base(cwd, base);
            let d = run_git(cwd, &["diff", &format!("{b}...HEAD")])?;
            Ok((format!("branch (base {b})"), d))
        }
        ReviewScope::Auto => unreachable!(),
    }
}

/// Cap the diff so we never blow the context window (head + tail).
fn cap_diff(diff: &str, max: usize) -> String {
    if diff.len() <= max {
        return diff.to_string();
    }
    let head = &diff[..max * 3 / 4];
    let tail = &diff[diff.len() - max / 4..];
    format!("{head}\n\n...[diff truncated for length]...\n\n{tail}")
}

// ---------------------------------------------------------------------------
// review / adversarial-review.
// ---------------------------------------------------------------------------

const REVIEW_SYSTEM: &str = "You are a meticulous senior software engineer performing a code review. \
Identify bugs, anti-patterns, missing tests, security issues, and documentation gaps. \
Respond with ONLY a single JSON object (no markdown, no prose) matching this schema: \
{\"verdict\":\"approve|request_changes|comment\",\"summary\":\"...\",\"findings\":[{\"severity\":\"critical|high|medium|low|info\",\"title\":\"...\",\"body\":\"...\",\"location\":\"path:line (optional)\"}],\"next_steps\":[\"...\"]}";

const ADVERSARIAL_SYSTEM: &str = "You are a demanding, skeptical staff engineer and security auditor performing an ADVERSARIAL review. \
Aggressively pressure-test the logic: assume the author missed edge cases, race conditions, error handling, injection/abuse vectors, and incorrect assumptions. Be specific and uncompromising. \
Respond with ONLY a single JSON object (no markdown, no prose) matching this schema: \
{\"verdict\":\"approve|request_changes|comment\",\"summary\":\"...\",\"findings\":[{\"severity\":\"critical|high|medium|low|info\",\"title\":\"...\",\"body\":\"...\",\"location\":\"path:line (optional)\"}],\"next_steps\":[\"...\"]}";

pub(crate) async fn cmd_review(
    cli: &crate::Cli,
    base: Option<String>,
    scope: ReviewScope,
    panel: Option<String>,
    background: bool,
    adversarial: bool,
    focus: &[String],
) -> anyhow::Result<()> {
    let cwd = crate::agentic_cwd(cli)?;

    // Background: re-exec self detached, then return the job id.
    if background && active_job_dir().is_none() {
        let id = spawn_background(
            if adversarial {
                "adversarial-review"
            } else {
                "review"
            },
            &cwd,
        )?;
        println!("{id}");
        if !cli.quiet {
            eprintln!("[zoder] started background job {id} (zoder status {id} / result {id})");
        }
        return Ok(());
    }

    let (label, diff) = build_diff(&cwd, scope, base.as_deref())?;
    if diff.trim().is_empty() {
        let out = ReviewOutput {
            verdict: "approve".into(),
            summary: format!("No changes to review ({label})."),
            findings: vec![],
            next_steps: vec![],
        };
        emit_reviews(cli, &[(String::from("n/a"), out)], 0.0);
        return Ok(());
    }

    let system = if adversarial {
        ADVERSARIAL_SYSTEM
    } else {
        REVIEW_SYSTEM
    };
    let focus_txt = focus.join(" ");
    let user = if focus_txt.trim().is_empty() {
        format!(
            "Review the following {label} diff:\n\n```diff\n{}\n```",
            cap_diff(&diff, 120_000)
        )
    } else {
        format!(
            "Review the following {label} diff. Focus especially on: {focus_txt}\n\n```diff\n{}\n```",
            cap_diff(&diff, 120_000)
        )
    };

    // Reviewer roster: the routed/`-m` model plus any `--panel` models.
    let mut models: Vec<Option<String>> = vec![None];
    if let Some(p) = &panel {
        for m in p.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            models.push(Some(m.to_string()));
        }
    }

    // Fan out concurrently on this task (no spawn: the completion future borrows
    // a non-Send sink type, so we poll them together via join_all instead).
    let max_tokens = cli.max_tokens.max(2048);
    let futs = models
        .iter()
        .map(|m| complete_once(cli, m.as_deref(), system, &user, max_tokens));
    let results = futures_util::future::join_all(futs).await;

    let mut reviews: Vec<(String, ReviewOutput)> = Vec::new();
    let mut total_cost = 0.0;
    for r in results {
        match r {
            Ok(c) => {
                total_cost += c.cost_usd;
                reviews.push((c.model, parse_review(&c.content)));
            }
            Err(e) => {
                reviews.push((
                    "error".into(),
                    ReviewOutput {
                        verdict: "comment".into(),
                        summary: format!("reviewer failed: {e}"),
                        ..Default::default()
                    },
                ));
            }
        }
    }

    emit_reviews(cli, &reviews, total_cost);
    Ok(())
}

/// Render the aggregated review(s) as JSON (machine) or text (human), and write
/// `result.json` when running as a background job.
fn emit_reviews(cli: &crate::Cli, reviews: &[(String, ReviewOutput)], cost: f64) {
    // Aggregate verdict = worst across reviewers.
    let agg = reviews
        .iter()
        .map(|(_, r)| r.verdict.as_str())
        .max_by_key(|v| verdict_rank(v))
        .unwrap_or("approve")
        .to_string();

    let payload = json!({
        "verdict": agg,
        "cost_usd": cost,
        "reviewers": reviews.iter().map(|(m, r)| json!({
            "model": m,
            "verdict": r.verdict,
            "summary": r.summary,
            "findings": r.findings,
            "next_steps": r.next_steps,
        })).collect::<Vec<_>>(),
    });

    if let Some(dir) = active_job_dir() {
        let _ = std::fs::write(
            dir.join("result.json"),
            serde_json::to_string_pretty(&payload).unwrap_or_default(),
        );
    }

    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
        return;
    }

    println!("verdict: {agg}   (${cost:.4})\n");
    for (model, r) in reviews {
        println!("── {model} :: {} ──", r.verdict);
        if !r.summary.is_empty() {
            println!("{}", r.summary);
        }
        for f in &r.findings {
            let loc = f
                .location
                .as_deref()
                .map(|l| format!(" [{l}]"))
                .unwrap_or_default();
            println!("  • ({}) {}{}", f.severity, f.title, loc);
            if !f.body.is_empty() {
                for line in f.body.lines() {
                    println!("      {line}");
                }
            }
        }
        if !r.next_steps.is_empty() {
            println!("  next:");
            for s in &r.next_steps {
                println!("    - {s}");
            }
        }
        println!();
    }
}

// ---------------------------------------------------------------------------
// rescue (agentic, write-capable) + transfer.
// ---------------------------------------------------------------------------

pub(crate) async fn cmd_rescue(
    cli: &crate::Cli,
    task: &[String],
    background: bool,
) -> anyhow::Result<()> {
    let cwd = crate::agentic_cwd(cli)?;
    if background && active_job_dir().is_none() {
        let id = spawn_background("rescue", &cwd)?;
        println!("{id}");
        if !cli.quiet {
            eprintln!("[zoder] started background job {id} (zoder status {id} / result {id})");
        }
        return Ok(());
    }
    let task_txt = task.join(" ");
    let task_txt = if task_txt.trim().is_empty() {
        crate::read_prompt(None)?
    } else {
        task_txt
    };
    let prompt = format!(
        "You are in RESCUE mode: investigate and resolve a stubborn bug or failing diagnostic. \
Reproduce the problem, find the root cause, implement a minimal fix, and verify it (build/tests). \
Explain the root cause and the fix when done.\n\nTask: {task_txt}"
    );

    // Drive the turn directly (rather than via cmd_exec_agentic) so a wall-clock
    // timeout PRESERVES partial work instead of yielding zero output: on-disk
    // edits already survive (the engine applies them as tools run), and here we
    // also capture the streamed transcript and a resumable session id. This is
    // the fix for the DB2 field test where `rescue` timed out at 600s with
    // nothing to show for it.
    let engine_kind = crate::resolve_engine_kind(cli)?;
    let t = crate::agentic_turn(cli, engine_kind, prompt, None, !cli.json).await?;

    let ok = t.run.succeeded();
    let timed_out = t.run.outcome == "timeout";

    if cli.json {
        println!(
            "{}",
            json!({
                "kind": "rescue",
                "model": t.model,
                "agent": t.alias,
                "session_id": t.run.session_id,
                "outcome": t.run.outcome,
                "ok": ok,
                "content": t.run.content,
                "tool_calls": t.run.tool_calls,
                "cost_usd": t.cost_usd,
                "duration_ms": t.elapsed_ms,
            })
        );
    } else {
        println!();
        if !cli.quiet {
            eprintln!(
                "[zoder] rescue {} via {}  {} tools  ${:.4}  {:.0}ms  [{}]",
                t.model, t.alias, t.run.tool_calls, t.cost_usd, t.elapsed_ms, t.run.outcome
            );
            if timed_out {
                eprintln!(
                    "[rescue] timed out after {:.0}s — partial work preserved: on-disk edits kept, \
{} chars of transcript captured, {} tool call(s) made. Resume where it left off with:\n  \
zoder rescue --session {} \"continue\"\nOr give it more room: raise --agent-timeout <secs> \
(default 900), or pick a stronger/faster model with -m.",
                    t.elapsed_ms / 1000.0,
                    t.run.content.len(),
                    t.run.tool_calls,
                    t.run.session_id,
                );
            }
        }
    }

    // Persist partial artifacts to the job dir so a BACKGROUND rescue that timed
    // out still yields the transcript, the resumable session id, and the outcome
    // — not just `ok=false` with nothing to inspect.
    if let Some(dir) = active_job_dir() {
        if !t.run.content.is_empty() {
            let _ = std::fs::write(dir.join("content.txt"), &t.run.content);
        }
        let _ = std::fs::write(
            dir.join("result.json"),
            json!({
                "kind": "rescue",
                "ok": ok,
                "outcome": t.run.outcome,
                "session_id": t.run.session_id,
                "model": t.model,
                "tool_calls": t.run.tool_calls,
                "cost_usd": t.cost_usd,
                "duration_ms": t.elapsed_ms,
            })
            .to_string(),
        );
    }

    if !ok {
        anyhow::bail!("rescue ended: {}", t.run.outcome);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// loop: continuous author -> validate (build/test) -> adversarial review -> fix.
// ---------------------------------------------------------------------------

/// Run a validation command in `cwd` via `sh -c`. Returns `(passed, tail)` where
/// `tail` is the last ~4 KB of combined stdout+stderr — the part a fixer needs.
fn run_check(cwd: &Path, cmd: &str) -> (bool, String) {
    match std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .output()
    {
        Ok(o) => {
            let mut combined = String::new();
            combined.push_str(&String::from_utf8_lossy(&o.stdout));
            combined.push_str(&String::from_utf8_lossy(&o.stderr));
            // Tail on a char boundary so we never split a multi-byte codepoint.
            let tail = if combined.len() > 4096 {
                let mut start = combined.len() - 4096;
                while start < combined.len() && !combined.is_char_boundary(start) {
                    start += 1;
                }
                combined[start..].to_string()
            } else {
                combined
            };
            (o.status.success(), tail)
        }
        Err(e) => (false, format!("failed to run check `{cmd}`: {e}")),
    }
}

/// Does a finding cite a concrete code location (`path`, `path:line`, …)? We use
/// this to filter hallucinated high-severity findings from weak reviewers: a real
/// blocking defect can point at where it lives.
fn has_concrete_location(f: &Finding) -> bool {
    match f.location.as_deref().map(str::trim) {
        Some(l) if !l.is_empty() => {
            let lc = l.to_lowercase();
            // reject vague placeholders; require a path/line-ish token.
            !matches!(lc.as_str(), "n/a" | "none" | "general" | "various" | "-")
                && l.chars().any(|c| c == '.' || c == '/' || c == ':')
        }
        _ => false,
    }
}

/// Count "blocking" findings. Severity that blocks depends on whether the
/// objective gate is already green: when the build/test check passes we only
/// block on `critical` (treat `high` as advisory), otherwise `critical|high`.
/// In both cases a blocking finding must cite a concrete location, which filters
/// the hallucinated high-severity findings over-strict free reviewers emit on an
/// already-correct tree.
fn count_blocking(r: &ReviewOutput, green: bool) -> usize {
    r.findings
        .iter()
        .filter(|f| {
            let s = f.severity.to_lowercase();
            let sev_blocks = if green {
                s == "critical"
            } else {
                s == "critical" || s == "high"
            };
            sev_blocks && has_concrete_location(f)
        })
        .count()
}

/// Autonomous fix loop: author (write-capable, single continuing session) ->
/// validate (optional build/test command) -> adversarial review -> feed the
/// failures back -> repeat until the check passes AND the reviewer raises no
/// blocking findings, or `max_iters` is reached, or progress stalls. Every
/// author turn and reviewer pass is cost-tracked in the ledger.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_loop(
    cli: &crate::Cli,
    task: &[String],
    instructions: Option<String>,
    max_iters: usize,
    check: Option<String>,
    reviewer: Option<String>,
    base: Option<String>,
    scope: ReviewScope,
    accept_on_green: bool,
    background: bool,
) -> anyhow::Result<()> {
    let cwd = crate::agentic_cwd(cli)?;
    if background && active_job_dir().is_none() {
        let id = spawn_background("loop", &cwd)?;
        println!("{id}");
        if !cli.quiet {
            eprintln!("[zoder] started background job {id} (zoder status {id} / result {id})");
        }
        return Ok(());
    }

    // Task text: trailing args, else -i FILE, else stdin.
    let mut task_txt = task.join(" ");
    if task_txt.trim().is_empty() {
        if let Some(f) = &instructions {
            task_txt =
                std::fs::read_to_string(f).with_context(|| format!("reading instructions {f}"))?;
        } else {
            task_txt = crate::read_prompt(None)?;
        }
    }

    let max_iters = max_iters.max(1);
    let mut session: Option<String> = None;
    let mut prev_diff = String::new();
    let mut iterations: Vec<Value> = Vec::new();
    let mut total_cost = 0.0;
    let mut feedback = String::new();
    let started = std::time::Instant::now();
    let mut resolved = false;
    let mut final_verdict = String::from("comment");
    // Consecutive iterations where the author neither finished nor left any edit
    // on disk (engine unreachable / repeatedly timing out fast). Bounds the loop
    // so a dead engine can't spin reviews on an empty diff until `max_iters`.
    let mut dead_streak = 0usize;

    for i in 1..=max_iters {
        // 1. Author turn — continue the SAME engine session for memory.
        let author_prompt = if i == 1 {
            let mut p = format!(
                "You are the AUTHOR in an autonomous fix loop. Implement a COMPLETE, correct fix \
for the task below. Make minimal, focused changes and add or adjust tests where appropriate. \
Use your file and shell tools to edit the repository directly. Do not stop until the change is \
coherent and self-consistent.\n\nTASK:\n{task_txt}\n"
            );
            if let Some(c) = &check {
                p.push_str(&format!(
                    "\nThe change MUST make this command pass (exit 0): `{c}`. Run it yourself to \
verify before you finish.\n"
                ));
            }
            p
        } else {
            format!(
                "Continue the SAME fix in this repository. The previous attempt was NOT accepted. \
Address ALL of the following and update the code and tests accordingly, then re-run the \
validation command and make it pass.\n\n{feedback}\n\nOriginal task (for reference):\n{task_txt}\n"
            )
        };

        if !cli.quiet {
            eprintln!("\n[loop] iter {i}/{max_iters}: author…");
        }
        // The author turn is best-effort: a wall-clock timeout (or transient
        // engine error) must NOT discard the round. The engine applies edits to
        // disk as tool calls run, so partial work survives; we still validate,
        // review, and feed the failure back so the next iteration can finish it.
        let mut author_err: Option<String> = None;
        let engine_kind = crate::resolve_engine_kind(cli)?;
        let turn = match crate::agentic_turn(
            cli,
            engine_kind,
            author_prompt,
            session.clone(),
            false,
        )
        .await
        {
            Ok(t) => {
                session = Some(t.run.session_id.clone());
                total_cost += t.cost_usd;
                Some(t)
            }
            Err(e) => {
                let msg = e.to_string();
                let timed_out = msg.contains("timed out") || msg.contains("timeout");
                if !cli.quiet {
                    eprintln!("[loop] iter {i}: author turn did not finish: {msg}");
                    if timed_out {
                        eprintln!(
                            "[loop] hint: raise the per-turn budget with `--agent-timeout <secs>` \
(default 900), or pick a faster model with `-m` for the loop. Preserving partial \
edits and continuing."
                        );
                    }
                }
                author_err = Some(msg);
                None
            }
        };

        // 2. Capture the working-tree diff (whatever edits actually landed).
        let (label, diff) = build_diff(&cwd, scope, base.as_deref())?;
        let diff_lines = diff.lines().count();

        // Bail out if a failed author keeps producing nothing (dead engine).
        if turn.is_none() && diff.trim().is_empty() {
            dead_streak += 1;
            if dead_streak >= 2 {
                if !cli.quiet {
                    eprintln!(
                        "[loop] iter {i}: author produced no edits twice in a row \
(engine unreachable or timing out before any tool call); stopping."
                    );
                }
                break;
            }
        } else {
            dead_streak = 0;
        }

        // 3. Validate (build/test) if a check command was given.
        let (check_passed, check_tail) = match &check {
            Some(c) => {
                if !cli.quiet {
                    eprintln!("[loop] iter {i}: check `{c}`…");
                }
                let (ok, tail) = run_check(&cwd, c);
                (Some(ok), tail)
            }
            None => (None, String::new()),
        };

        // 4. Adversarial review of the current diff (+ validation output).
        let review_user = {
            let mut u = format!(
                "Review this {label} diff for the task:\n{task_txt}\n\n```diff\n{}\n```\n",
                cap_diff(&diff, 100_000)
            );
            if let Some(p) = check_passed {
                u.push_str(&format!(
                    "\nValidation command `{}` currently {}.\n",
                    check.as_deref().unwrap_or(""),
                    if p { "PASSES" } else { "FAILS" }
                ));
                if p {
                    // Green-aware calibration: the objective gate already proves
                    // the change works. Keep the reviewer adversarial but stop it
                    // manufacturing blockers on a correct tree — block only on real
                    // regressions, each citing a concrete location.
                    u.push_str(
                        "\nThe objective gate is GREEN: the build/tests pass, so the change is \
functionally correct. Do NOT block on style, naming, missing-test-coverage, or hypothetical \
concerns. Use verdict `request_changes` with a `critical` finding ONLY for a concrete \
correctness or security REGRESSION introduced by this diff, and every blocking finding MUST \
cite an exact `location` (path:line). Otherwise return `approve` (or `comment` for non-blocking \
nits).\n",
                    );
                } else {
                    u.push_str(&format!(
                        "\nValidation output (tail):\n```\n{check_tail}\n```\n"
                    ));
                }
            }
            u
        };
        if !cli.quiet {
            eprintln!("[loop] iter {i}: adversarial review…");
        }
        let max_tokens = cli.max_tokens.max(2048);
        let review = match complete_once(
            cli,
            reviewer.as_deref(),
            ADVERSARIAL_SYSTEM,
            &review_user,
            max_tokens,
        )
        .await
        {
            Ok(c) => {
                total_cost += c.cost_usd;
                parse_review(&c.content)
            }
            Err(e) => ReviewOutput {
                verdict: "comment".into(),
                summary: format!("reviewer failed: {e}"),
                ..Default::default()
            },
        };
        final_verdict = review.verdict.clone();
        // The objective gate is "green" when the check passed (or none was given).
        let green = check_passed.unwrap_or(true);
        let blocking = count_blocking(&review, green);

        let author_model = turn.as_ref().map(|t| t.model.clone());
        let tool_calls = turn.as_ref().map(|t| t.run.tool_calls).unwrap_or(0);
        let author_outcome = match (&turn, &author_err) {
            (Some(t), _) => t.run.outcome.clone(),
            (None, Some(e)) => format!("interrupted: {e}"),
            (None, None) => "interrupted".to_string(),
        };
        iterations.push(json!({
            "iter": i,
            "author_model": author_model,
            "tool_calls": tool_calls,
            "author_outcome": author_outcome,
            "diff_lines": diff_lines,
            "check": check.as_deref(),
            "check_passed": check_passed,
            "verdict": review.verdict,
            "blocking_findings": blocking,
            "summary": review.summary,
            "cost_usd_cumulative": total_cost,
        }));

        // 5. Decide: objective gate (build/test) AND review gate (no blockers).
        let objective_ok = green;
        let check_green = check_passed == Some(true); // an actual --check that passed
        let review_ok = review.verdict == "approve" || blocking == 0;

        // Escape hatch: `--accept-on-green` treats a passing objective check as
        // sufficient, with reviewer findings advisory (for over-strict reviewers).
        if accept_on_green && check_green && diff_lines > 0 {
            resolved = true;
            if !cli.quiet {
                eprintln!(
                    "[loop] iter {i}: RESOLVED on green check (--accept-on-green; \
reviewer advisory, verdict={}, blocking={blocking})",
                    review.verdict
                );
            }
            break;
        }
        if objective_ok && review_ok && diff_lines > 0 {
            resolved = true;
            if !cli.quiet {
                eprintln!(
                    "[loop] iter {i}: RESOLVED (check={:?} verdict={})",
                    check_passed, review.verdict
                );
            }
            break;
        }

        // No-progress guard (2nd iteration on): an identical diff that still
        // isn't accepted. (Never trips on iter 1, where prev_diff is empty.)
        if i > 1 && diff == prev_diff {
            // Stalemate breaker: the objective check is GREEN but the reviewer
            // keeps blocking the SAME diff. Rather than discard a verified fix,
            // resolve with the findings recorded as warnings.
            if check_green && diff_lines > 0 && !review_ok {
                resolved = true;
                if !cli.quiet {
                    eprintln!(
                        "[loop] iter {i}: RESOLVED with warnings — check is green but the \
reviewer keeps blocking an unchanged diff ({blocking} blocking finding(s) recorded)."
                    );
                }
                break;
            }
            if !cli.quiet {
                eprintln!("[loop] iter {i}: no progress (diff unchanged); stopping.");
            }
            break;
        }
        prev_diff = diff.clone();

        // 6. Compose feedback for the next author turn.
        let mut fb = String::new();
        if let Some(e) = &author_err {
            fb.push_str(&format!(
                "Your previous turn was INTERRUPTED before you finished ({e}). Any edits you \
already made are still on disk. Resume from where you left off and finish efficiently — \
prioritize making the validation command pass.\n\n"
            ));
        }
        if diff.trim().is_empty() {
            fb.push_str(
                "You made NO changes to the repository in the previous turn. You MUST actually \
edit the source files using your file/shell tools (e.g. write to src/lib.rs), not just describe \
the fix. Apply the changes now.\n\n",
            );
        }
        if check_passed == Some(false) {
            fb.push_str(&format!(
                "The validation command `{}` is still FAILING. Output (tail):\n{}\n\n",
                check.as_deref().unwrap_or(""),
                check_tail
            ));
        }
        if !review.summary.is_empty() {
            fb.push_str(&format!("Reviewer summary: {}\n", review.summary));
        }
        for f in &review.findings {
            fb.push_str(&format!("- [{}] {}: {}\n", f.severity, f.title, f.body));
        }
        if !review.next_steps.is_empty() {
            fb.push_str("Required next steps:\n");
            for s in &review.next_steps {
                fb.push_str(&format!("- {s}\n"));
            }
        }
        feedback = fb;
    }

    let payload = json!({
        "kind": "loop",
        "task": task_txt,
        "resolved": resolved,
        "iterations": iterations.len(),
        "final_verdict": final_verdict,
        "check": check,
        "total_cost_usd": total_cost,
        "duration_ms": started.elapsed().as_millis(),
        "log": iterations,
    });

    if let Some(dir) = active_job_dir() {
        let _ = std::fs::write(
            dir.join("result.json"),
            serde_json::to_string_pretty(&payload).unwrap_or_default(),
        );
    }

    if cli.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        println!(
            "\n=== loop {} after {} iteration(s)  ${total_cost:.4} ===",
            if resolved {
                "RESOLVED"
            } else {
                "STOPPED (unresolved)"
            },
            iterations.len()
        );
        for it in &iterations {
            println!(
                "  iter {} : tools={} diff_lines={} check={} verdict={}",
                it["iter"], it["tool_calls"], it["diff_lines"], it["check_passed"], it["verdict"]
            );
        }
    }

    if !resolved {
        anyhow::bail!(
            "loop ended unresolved after {} iteration(s)",
            iterations.len()
        );
    }
    Ok(())
}

pub(crate) async fn cmd_transfer(cli: &crate::Cli) -> anyhow::Result<()> {
    let cwd = crate::agentic_cwd(cli)?;
    let alias = crate::resolve_agent_alias(cli, cli.model.as_deref().unwrap_or(""));
    let socket = crate::ensure_engine_daemon().await?;
    let sid = zoder_core::new_session(&socket, &alias, &cwd).await?;
    if cli.json {
        println!(
            "{}",
            json!({"session_id": sid, "agent": alias, "cwd": cwd.to_string_lossy()})
        );
    } else {
        println!("session: {sid}");
        println!(
            "resume with: zoder --session {sid} -C {} \"<next step>\"",
            cwd.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Background job registry.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobMeta {
    id: String,
    kind: String,
    status: String, // running | done | failed | cancelled
    cwd: String,
    pid: u32,
    started: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finished: Option<DateTime<Utc>>,
}

fn jobs_dir() -> PathBuf {
    Config::home().join("jobs")
}

/// `$ZODER_JOB_DIR` when this process is the detached worker of a job.
pub(crate) fn active_job_dir() -> Option<PathBuf> {
    std::env::var("ZODER_JOB_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

fn read_meta(dir: &Path) -> Option<JobMeta> {
    let raw = std::fs::read_to_string(dir.join("meta.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_meta(dir: &Path, meta: &JobMeta) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(meta)?)?;
    Ok(())
}

/// Re-exec the current invocation as a detached worker writing to a new job dir.
fn spawn_background(kind: &str, cwd: &Path) -> anyhow::Result<String> {
    let id = format!(
        "{}-{:04x}",
        Utc::now().format("%Y%m%d-%H%M%S"),
        std::process::id() & 0xffff
    );
    let dir = jobs_dir().join(&id);
    std::fs::create_dir_all(&dir)?;

    let exe = std::env::current_exe().context("locating current executable")?;
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--background")
        .collect();
    let out = std::fs::File::create(dir.join("output.txt"))?;
    let err = out.try_clone()?;
    let child = std::process::Command::new(&exe)
        .args(&args)
        .env("ZODER_JOB_DIR", &dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .spawn()
        .with_context(|| format!("spawning background worker {}", exe.display()))?;

    write_meta(
        &dir,
        &JobMeta {
            id: id.clone(),
            kind: kind.to_string(),
            status: "running".into(),
            cwd: cwd.to_string_lossy().to_string(),
            pid: child.id(),
            started: Utc::now(),
            finished: None,
        },
    )?;
    Ok(id)
}

/// Mark a worker's job terminal (called from `main` once the work returns).
pub(crate) fn finalize_job(dir: &Path, ok: bool) {
    if let Some(mut meta) = read_meta(dir) {
        if meta.status == "running" {
            meta.status = if ok { "done" } else { "failed" }.into();
            meta.finished = Some(Utc::now());
            let _ = write_meta(dir, &meta);
        }
    }
}

fn all_jobs() -> Vec<JobMeta> {
    let mut jobs: Vec<JobMeta> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(jobs_dir()) {
        for e in rd.flatten() {
            if let Some(m) = read_meta(&e.path()) {
                jobs.push(m);
            }
        }
    }
    jobs.sort_by(|a, b| b.started.cmp(&a.started));
    jobs
}

fn resolve_job(id: Option<&str>, running_only: bool) -> Option<JobMeta> {
    let jobs = all_jobs();
    match id {
        Some(want) => jobs.into_iter().find(|j| j.id == want),
        None => jobs
            .into_iter()
            .find(|j| !running_only || j.status == "running"),
    }
}

pub(crate) fn cmd_status(cli: &crate::Cli, job: Option<String>, all: bool) -> anyhow::Result<()> {
    if let Some(want) = &job {
        let m = resolve_job(Some(want), false).ok_or_else(|| anyhow!("no such job: {want}"))?;
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&m)?);
        } else {
            println!("{} [{}] {} (pid {})", m.id, m.status, m.kind, m.pid);
            println!("  cwd: {}", m.cwd);
            println!("  started: {}", m.started.to_rfc3339());
            if let Some(f) = m.finished {
                println!("  finished: {}", f.to_rfc3339());
            }
        }
        return Ok(());
    }

    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    let jobs: Vec<JobMeta> = all_jobs()
        .into_iter()
        .filter(|j| all || cwd.as_deref().map(|c| c == j.cwd).unwrap_or(true))
        .collect();
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&jobs)?);
        return Ok(());
    }
    if jobs.is_empty() {
        println!("no background jobs");
        return Ok(());
    }
    println!("{:<20} {:<12} {:<18} started", "id", "status", "kind");
    for j in &jobs {
        println!(
            "{:<20} {:<12} {:<18} {}",
            j.id,
            j.status,
            j.kind,
            j.started.format("%Y-%m-%d %H:%M:%S")
        );
    }
    Ok(())
}

pub(crate) fn cmd_result(cli: &crate::Cli, job: Option<String>) -> anyhow::Result<()> {
    let m = resolve_job(job.as_deref(), false).ok_or_else(|| anyhow!("no matching job"))?;
    let dir = jobs_dir().join(&m.id);
    let result = std::fs::read_to_string(dir.join("result.json")).ok();
    if cli.json {
        let val: Value = result
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Null);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "job": m,
                "result": val,
            }))?
        );
        return Ok(());
    }
    match result {
        Some(r) => {
            // Pretty-print structured result if it's a review payload.
            if let Ok(v) = serde_json::from_str::<Value>(&r) {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("{r}");
            }
        }
        None => {
            // No structured result; show captured output.
            let out = std::fs::read_to_string(dir.join("output.txt")).unwrap_or_default();
            println!(
                "[{}] {} — no structured result; captured output:\n{out}",
                m.status, m.id
            );
        }
    }
    Ok(())
}

pub(crate) fn cmd_cancel(_cli: &crate::Cli, job: Option<String>) -> anyhow::Result<()> {
    let m = resolve_job(job.as_deref(), true).ok_or_else(|| anyhow!("no running job to cancel"))?;
    #[cfg(unix)]
    unsafe {
        libc::kill(m.pid as i32, libc::SIGTERM);
    }
    let dir = jobs_dir().join(&m.id);
    if let Some(mut meta) = read_meta(&dir) {
        meta.status = "cancelled".into();
        meta.finished = Some(Utc::now());
        let _ = write_meta(&dir, &meta);
    }
    println!("cancelled {}", m.id);
    Ok(())
}
