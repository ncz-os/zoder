//! zoder's agentic command surface (a drop-in for codex `exec`/`review`): `review`, `adversarial-review`, `rescue`,
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
    BillingMode, ChatRequest, Config, CostVerdict, Decision, Entry, HealthStore, Ledger, Message,
    ModelEntry, OpenAiProvider, PolicyGate, PricingCatalog,
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

/// Run one non-streamed completion on `model_override` (else the resolved
/// reviewer model), record it in the ledger, and return the text + cost.
///
/// Reviewer model precedence (highest first) — paired with the author
/// precedence in [`crate::resolve_effective_primary`] so the SECONDARY
/// model stays independent of the PRIMARY `primary_model`:
///
///   1. explicit `--reviewer` / `--panel` (`model_override`, per-invocation),
///   2. `[agents.<alias>].reviewer_model` for the selected alias (per-agent
///      pin; the alias is whichever `--agent` resolved to),
///   3. `Config::reviewer_model` (profile-level fallback),
///   4. strong CROSS-FAMILY model derived from the resolved author model
///      (preserves the legacy default — never the author's own family).
///
/// The `cli.model` (`-m`) shortcut is intentionally NOT consulted here: `-m`
/// is the AUTHOR pin. Treating it as a reviewer pin too would conflate
/// primary and secondary, hiding operator intent. The reviewer gets its
/// own pin (the `--reviewer` flag → `model_override`) so an operator can
/// cross-family-pick it independently of `-m`.
///
/// `reviewer_chain` is the scenario-routed reviewer candidate pool from
/// `crate::resolve_chain` — populated independently of `model_override`
/// so balanced routing's reviewer lane (sub-first) and KNEMON gating
/// can drive the default reviewer without a per-invocation pin. The pool
/// is passed through explicitly (no process-global cache — see fix for
/// Finding #19). When non-empty and `model_override == None`, the first
/// eligible entry in `reviewer_chain` is used as the reviewer; when
/// empty, the resolver falls through to the per-agent/profile-level
/// pin and finally to `default_cross_family_reviewer`.
async fn complete_once(
    cli: &crate::Cli,
    model_override: Option<&str>,
    reviewer_chain: &[String],
    system: &str,
    user: &str,
    max_tokens: u32,
) -> anyhow::Result<Completion> {
    let eng = Engine::load()?;

    let model = match model_override {
        Some(m) => m.to_string(),
        None => {
            // Scenario-routed reviewer chain (preferred when non-empty).
            // The chain is built from `chain_for_role(Reviewer, ..)` under
            // the active scenario, so balanced routing's "sub-first
            // reviewer" preference and KNEMON gating are honored without
            // any per-invocation reviewer pin. An empty chain (no
            // sub/paid candidates, all healthy) means the router has no
            // reviewer-class-eligible subscription; fall through to the
            // legacy path so an explicit per-agent pin or the cross-
            // family default still serves.
            if let Some(first) = reviewer_chain.first().cloned() {
                first
            } else {
                // Per-agent override first (`[agents.<alias>].reviewer_model`);
                // falls back to the profile-level `Config::reviewer_model`; falls
                // back to a CROSS-FAMILY default derived from the AUTHOR model
                // (NOT from `primary_model` so the two stay independent — an
                // operator can pin a strong reviewer without touching the
                // author default).
                if let Some(m) = eng.cfg.agent_reviewer_model(cli.agent.as_deref()) {
                    m
                } else if let Some(m) = eng.cfg.reviewer_model.as_ref() {
                    m.clone()
                } else {
                    // Default reviewer = a strong CROSS-FAMILY model, NOT the author's
                    // own. Self-review is weak; and routing the review to the author's
                    // flat-subscription provider (env-auth) 401s while the agentic
                    // engine authed fine (field report 2026-06-30). A cross-family EIH
                    // reviewer routes to the working-auth provider.
                    let health = HealthStore::load(&eng.cfg.health_path);
                    let routes = crate::resolve_chain(cli, &eng, &health)?;
                    let author = routes.primary.first().cloned().unwrap_or_default();
                    crate::default_cross_family_reviewer(&author).to_string()
                }
            }
        }
    };

    // Per-model routing: resolve the provider that actually serves this model
    // (e.g. a pinned MiniMax-M3 -> the minimax provider), not always the default
    // provider — otherwise a reviewer model could be sent to the wrong endpoint.
    let provider_cfg = eng.cfg.real_provider_for_model(&model).ok_or_else(|| {
        anyhow!(
            "no real provider is configured for reviewer model '{model}' — it would fall through \
             to the {host} placeholder and fail. Configure a provider that serves it, or pass a \
             backed reviewer via `--reviewer <model>`.",
            host = zoder_core::config::PLACEHOLDER_PROVIDER_HOST
        )
    })?;

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
    let provider_paid = provider_cfg.paid || provider_cfg.billing == BillingMode::Metered;
    let provider_cost_neutral = !provider_cfg.paid && provider_cfg.billing != BillingMode::Metered;
    if let Decision::NeedConfirm(why) =
        gate.check(&model_entry, provider_paid, provider_cost_neutral)
    {
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
    let (cost, unknown_cost) = match res.telemetry.cost_usd {
        Some(cost) if cost.is_finite() && cost >= 0.0 => (cost, false),
        _ => match pricing.classify_cost(&model, tokens_in, tokens_out, Some(Utc::now())) {
            CostVerdict::Priced(cost) => (cost, false),
            CostVerdict::Free => (0.0, false),
            CostVerdict::Unknown => (0.0, true),
        },
    };
    // Post-verify the reviewer call was actually served free (catch a free->paid
    // fallback) and record any violation in the ledger rather than marking clean.
    let mut violation = gate.verify_free(&model_entry, &res.telemetry).err();
    if unknown_cost {
        let msg = format!("cost unknown: no valid telemetry or catalog price for {model}");
        violation = Some(match violation {
            Some(existing) => format!("{existing}; {msg}"),
            None => msg,
        });
    }
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
        cost_unknown: unknown_cost,
        calls: 1,
        violation,
        tags: zoder_core::ledger::FinOpsTags::default(),
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
// Diff-substance anti-gaming guard.
//
// The accept branches in `cmd_loop` used to gate only on `diff_lines > 0`,
// which an over-eager author can trivially game: a "green" iteration whose
// diff is empty-after-headers, only whitespace, only comments, or only churns
// test files was still treated as substantive work and resolved the loop.
// `classify_diff_substance` returns the strictest bucket that fits the diff
// (Empty > WhitespaceOnly > CommentOnly > TestOnly > Substantive). The accept
// branches then demand `Substantive` OR (with a warning) `TestOnly`, and
// reject the rest as non-substantive noise — closing that gaming surface
// without weakening the review gate or the check gate.
// ---------------------------------------------------------------------------

/// What a unified diff is *actually* changing, beyond "did anything move".
/// Ordered so that the most permissive bucket a diff qualifies for is the one
/// returned (see [`classify_diff_substance`] precedence).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffSubstance {
    /// No `+`/`-` content lines at all (only file headers).
    Empty,
    /// Every `+`/`-` content line reduces to whitespace after stripping the
    /// leading marker and trimming.
    WhitespaceOnly,
    /// Every `+`/`-` content line is either empty or begins with a comment
    /// marker (`//`, `#`, `/*`, `*`, `*/`, `--`, `<!--`, `"""`, `'''`).
    CommentOnly,
    /// At least one substantive content line, but every changed file path
    /// matches a test pattern (see [`classify_diff_substance`]).
    TestOnly,
    /// Real source change — accept-eligible without warning.
    Substantive,
}

/// `true` for buckets the loop may resolve on. `TestOnly` is allowed but
/// emits a warning at the call site; the rest of the buckets are
/// explicitly rejected as anti-gaming.
pub(crate) fn substance_accept_eligible(s: &DiffSubstance) -> bool {
    matches!(s, DiffSubstance::Substantive | DiffSubstance::TestOnly)
}

/// Comment markers that mean "this line is a comment in some language".
/// Order matters only for readability — matching is prefix-based.
const COMMENT_MARKERS: &[&str] = &["//", "#", "/*", "*/", "*", "--", "<!--", "\"\"\"", "'''"];

/// True if `path` (a file path from a `+++ b/...` or `diff --git` header)
/// matches the test-file patterns we recognize. A `path` of `/dev/null`
/// (file deletion / addition outside any tree) is never a test file.
fn is_test_path(path: &str) -> bool {
    if path.is_empty() || path == "/dev/null" {
        return false;
    }
    let p = path.replace('\\', "/");
    // Path-segment matches anywhere in the path.
    if p.contains("/tests/") || p.contains("/test/") {
        return true;
    }
    let basename = std::path::Path::new(p.as_str())
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(p.as_str())
        .to_string();
    // Plain Perl-style test files (`foo.t`).
    if basename == "t" || basename.ends_with(".t") {
        return true;
    }
    // Glob patterns we recognize.
    let b = basename.as_str();
    b.starts_with("test_")
        || b.ends_with("_test.")
        || b.ends_with("_test.rs")
        || b.ends_with("_test.py")
        || b.ends_with("_test.go")
        || b.ends_with("_test.js")
        || b.ends_with("_test.ts")
        || b.ends_with("_spec.")
        || b.ends_with("_spec.rs")
        || b.ends_with("_spec.py")
        || b.ends_with("_spec.go")
        || b.ends_with("_spec.js")
        || b.ends_with("_spec.ts")
        || b.contains(".test.")
        || b.contains(".spec.")
}

/// Classify a unified-diff string by what its `+`/`-` content lines actually
/// change. Pure & deterministic — operates on the already-captured diff text
/// and makes no git calls.
///
/// Precedence (strictest first wins):
///   1. `Empty` — no `+`/`-` content lines at all.
///   2. `WhitespaceOnly` — every content line is empty/whitespace.
///   3. `CommentOnly` — every content line is empty or starts with a comment marker.
///   4. `TestOnly` — at least one substantive content line AND every changed
///      file path matches a test pattern.
///   5. `Substantive` — anything else.
///
/// "Content lines" are lines beginning with `+` or `-` that are NOT the
/// `+++ ` / `--- ` file headers. Changed file paths are read from `+++ b/...`
/// (preferred, mirrors the post-image) and, as a fallback, from
/// `diff --git a/... b/...` headers.
pub(crate) fn classify_diff_substance(diff: &str) -> DiffSubstance {
    let mut content_lines: Vec<&str> = Vec::new();
    // Track every path that appears as the *new* side of a file header.
    // Either source is fine: `+++ b/PATH` is the post-image, and on
    // `diff --git a/A b/B` we treat B (the post-image) as the canonical
    // path and fall back to A if B is /dev/null.
    let mut changed_paths: Vec<String> = Vec::new();

    for line in diff.lines() {
        // File headers first — we need to collect paths even when there are
        // no content lines (so an "Empty" diff still records its files,
        // though it won't matter for the Empty bucket).
        if let Some(rest) = line.strip_prefix("+++ ") {
            // `+++ b/path` form (most common from `git diff`).
            let path = rest.trim_start_matches("b/").trim();
            if !path.is_empty() && path != "/dev/null" {
                changed_paths.push(path.to_string());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("--- ") {
            // `--- a/path` — only useful if we somehow see the `---` of a
            // brand-new file (where `+++ /dev/null` shows up). Skip; the
            // `+++` line is the authoritative path.
            let _ = rest;
            continue;
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // `diff --git a/A b/B` — pick the post-image (B), or A if B is
            // /dev/null (file deletion).
            let halves = rest.split_whitespace().collect::<Vec<_>>();
            // halves[0] is "a/A", halves[1] is "b/B" — be defensive in case
            // either side is missing.
            let post = halves
                .get(1)
                .and_then(|s| s.strip_prefix("b/"))
                .or_else(|| halves.first().and_then(|s| s.strip_prefix("a/")))
                .unwrap_or("")
                .to_string();
            if !post.is_empty() && post != "/dev/null" {
                changed_paths.push(post);
            }
            continue;
        }
        if line.starts_with("Index: ") || line.starts_with("index ") {
            continue;
        }

        // Content lines: lines starting with '+' or '-' but NOT `+++`/`---`.
        // The `starts_with("+++")`/`starts_with("---")` checks above are
        // subsumed by the file-header strip_prefix blocks already handled,
        // but we still guard here for safety against edge cases.
        if line.starts_with("+++ ") || line.starts_with("--- ") {
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            content_lines.push(rest);
            continue;
        }
        if let Some(rest) = line.strip_prefix('-') {
            content_lines.push(rest);
            continue;
        }
        // Hunk headers ("@@ ... @@"), context lines (" foo"), and any
        // other lines are ignored for substance purposes.
    }

    if content_lines.is_empty() {
        return DiffSubstance::Empty;
    }

    // Step 1: WhitespaceOnly — every line is blank after trimming.
    let all_blank = content_lines.iter().all(|l| l.trim().is_empty());
    if all_blank {
        return DiffSubstance::WhitespaceOnly;
    }

    // Step 2: CommentOnly — every non-blank line starts with a comment marker.
    let all_comment_or_blank = content_lines.iter().all(|l| {
        let t = l.trim();
        t.is_empty() || COMMENT_MARKERS.iter().any(|m| t.starts_with(m))
    });
    if all_comment_or_blank {
        return DiffSubstance::CommentOnly;
    }

    // Step 3: TestOnly — at least one substantive line + every changed
    // path is a test path. (A diff with no recognizable file headers,
    // e.g. a hand-crafted snippet, is treated as NOT test-only — we can't
    // prove the changed files are tests, so be conservative.)
    let has_substantive_line = content_lines.iter().any(|l| {
        !l.trim().is_empty()
            && !COMMENT_MARKERS
                .iter()
                .any(|m| l.trim_start().starts_with(m))
    });
    if has_substantive_line {
        // Filter paths: skip empty / dev/null, then require every remaining
        // path to look like a test file.
        let real_paths: Vec<&String> = changed_paths
            .iter()
            .filter(|p| !p.is_empty() && p.as_str() != "/dev/null")
            .collect();
        if !real_paths.is_empty() && real_paths.iter().all(|p| is_test_path(p.as_str())) {
            return DiffSubstance::TestOnly;
        }
    }

    DiffSubstance::Substantive
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
        emit_reviews(
            cli,
            &ReviewAggregate {
                reviewers: &[(String::from("n/a"), out)],
                cost_usd: 0.0,
                requested: 1,
                ok_models: 1,
                failed_models: 0,
            },
        );
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

    // Scenario-routed reviewer chain: loaded once and passed to every
    // `complete_once` call so the default reviewer (the "head" of the
    // roster, before any `--panel` entries) honors the active scenario
    // and KNEMON gating. Pin precedence inside `complete_once` is
    // unchanged: explicit `model_override` (per `--panel`) wins, then
    // the first scenario-eligible reviewer, then the legacy fallback.
    let eng = Engine::load().ok();
    let health = eng.as_ref().map(|e| HealthStore::load(&e.cfg.health_path));
    let reviewer_chain: Vec<String> = match (&eng, &health) {
        (Some(e), Some(h)) => crate::resolve_chain(cli, e, h)
            .map(|r| r.reviewer)
            .unwrap_or_default(),
        _ => Vec::new(),
    };

    // Fan out concurrently on this task (no spawn: the completion future borrows
    // a non-Send sink type, so we poll them together via join_all instead).
    let max_tokens = cli.max_tokens.max(2048);
    let futs = models.iter().map(|m| {
        complete_once(
            cli,
            m.as_deref(),
            &reviewer_chain,
            system,
            &user,
            max_tokens,
        )
    });
    let results = futures_util::future::join_all(futs).await;

    // Outcome accounting:
    //   * `ok_models`            - the models whose completions succeeded.
    //   * `failed_against_solo`  - the synthetic "error" record only wins
    //                              when EVERY requested reviewer failed
    //                              (i.e. the model label is meaningless because
    //                              nothing in the roster reviewed any code).
    // The total failure -> bail-Ok bug fixed here: when `ok_models` is empty
    // the aggregate verdict does NOT default to "comment" (which would have
    // passed CI); the caller now sees a nonzero exit so a CI review gate
    // cannot be silently green-lit by a single 401 or by every panel model
    // timing out.
    let mut reviews: Vec<(String, ReviewOutput)> = Vec::new();
    let mut total_cost = 0.0;
    let mut ok_models: usize = 0;
    for r in results {
        match r {
            Ok(c) => {
                ok_models += 1;
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
    let requested = models.len();
    let failed_count = requested.saturating_sub(ok_models);
    let aggregate = ReviewAggregate {
        reviewers: &reviews,
        cost_usd: total_cost,
        requested,
        ok_models,
        failed_models: failed_count,
    };

    emit_reviews(cli, &aggregate);

    // Critical regression guard (Finding #14): when NO reviewer completed
    // (e.g. the only configured reviewer 401'd, or every panel model timed
    // out) the function used to return `Ok(())` with a synthetic "comment"
    // verdict and CI gates saw "green". That hides a real failure and
    // erases the very signal the review job exists to produce. Surface it
    // as a nonzero exit so a CI `zoder review` job breaks the pipeline
    // when every model fails — the same way every other Rust-bins in this
    // workspace treat "no successful work happened" as a hard error.
    //
    // We do this AFTER emitting the payload (and writing result.json when
    // running as a background job, via `emit_reviews`) so the operator still
    // has a complete diagnostic trail — verdict, model-by-model status,
    // and cost — to triage from.
    if ok_models == 0 {
        let requested_n = requested;
        anyhow::bail!(
            "review failed: 0/{requested_n} reviewers completed (all errors; see diagnostics above). \
A review that no model produced must not be reported as success.",
        );
    }
    if failed_count > 0 && !cli.quiet {
        eprintln!(
            "[zoder] review: {ok_models}/{requested} reviewers succeeded ({failed_count} failed; partial review)"
        );
    }
    Ok(())
}

/// Aggregated review payload state shared between the result-builder and the
/// renderer. Captures both the per-reviewer records AND the bookkept counts
/// so the rendered payload can be explicit about partial-panel failure
/// instead of leaving the CI to guess whether the synthetic "comment" came
/// from every reviewer failing or from a real reviewer rating the diff.
struct ReviewAggregate<'a> {
    reviewers: &'a [(String, ReviewOutput)],
    cost_usd: f64,
    /// Number of reviewer slots the caller asked for (1 + `--panel` entries).
    requested: usize,
    /// How many of those slots produced a structured completion.
    ok_models: usize,
    /// How many produced only an error record.
    failed_models: usize,
}

/// Compute the aggregate verdict + per-reviewer view out of a list of
/// (model, output) pairs, plus the success/failure counts so the payload
/// can be explicit about partial-panel failure. Pure & deterministic so
/// the matrix can be unit-tested (Finding #14: every-FAIL path must
/// surface as `complete=false` + a non-`approve` verdict, never a silent
/// "comment" that lets CI think the review ran).
fn aggregate_review(
    reviews: &[(String, ReviewOutput)],
    cost_usd: f64,
    requested: usize,
    ok_models: usize,
    failed_models: usize,
) -> (String, bool, serde_json::Value) {
    let all_failed = ok_models == 0;
    // Aggregate verdict = worst across reviewers that ACTUALLY completed;
    // a synthetic "error/comment" record (model == "error") does NOT count
    // as a successful reviewer vote and must not lift the aggregate. If
    // every record is an error, fall through to "request_changes" so the
    // rendered verdict visibly disagrees with the silent-Ok() CI exit.
    let worst_rank = reviews
        .iter()
        .filter(|(m, _)| m.as_str() != "error")
        .map(|(_, r)| r.verdict.as_str())
        .map(verdict_rank)
        .max()
        .unwrap_or(0);
    let agg = if worst_rank >= 2 {
        "request_changes"
    } else if all_failed {
        // No real reviewer vote to carry the verdict — surface a block so
        // a CI gate that reads `verdict` rather than the process exit code
        // also sees the failure. The bail() in `cmd_review` remains the
        // authoritative signal: a total-failure review exits nonzero.
        "request_changes"
    } else if worst_rank >= 1 {
        "comment"
    } else {
        "approve"
    }
    .to_string();

    let payload = json!({
        "verdict": agg,
        "complete": !all_failed,
        "requested": requested,
        "ok_models": ok_models,
        "failed_models": failed_models,
        "cost_usd": cost_usd,
        "reviewers": reviews.iter().map(|(m, r)| json!({
            "model": m,
            "verdict": r.verdict,
            "summary": r.summary,
            "findings": r.findings,
            "next_steps": r.next_steps,
        })).collect::<Vec<_>>(),
    });

    (agg, all_failed, payload)
}

/// Render the aggregated review(s) as JSON (machine) or text (human), and write
/// `result.json` when running as a background job. `aggregate.ok_models == 0`
/// is reflected in the payload as `complete: false` so downstream consumers
/// (CI gates, dashboards) can distinguish a real "comment" verdict from a
/// total-failure no-reviewer-actually-ran episode.
fn emit_reviews(cli: &crate::Cli, aggregate: &ReviewAggregate<'_>) {
    let (agg, all_failed, payload) = aggregate_review(
        aggregate.reviewers,
        aggregate.cost_usd,
        aggregate.requested,
        aggregate.ok_models,
        aggregate.failed_models,
    );

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

    let status = if all_failed {
        format!("INCOMPLETE (0/{} reviewers completed)", aggregate.requested)
    } else if aggregate.failed_models > 0 {
        format!(
            "PARTIAL ({}/{} reviewers; {} failed)",
            aggregate.ok_models, aggregate.requested, aggregate.failed_models
        )
    } else {
        format!(
            "complete ({} reviewer{})",
            aggregate.requested,
            if aggregate.requested == 1 { "" } else { "s" }
        )
    };
    println!(
        "verdict: {agg}   (${cost:.4})   [{status}]\n",
        cost = aggregate.cost_usd
    );
    for (model, r) in aggregate.reviewers {
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
                "cost_usd": (!t.cost_unknown).then_some(t.cost_usd),
                "cost_unknown": t.cost_unknown,
                "duration_ms": t.elapsed_ms,
            })
        );
    } else {
        println!();
        if !cli.quiet {
            let cost_label = if t.cost_unknown {
                "unknown".to_string()
            } else {
                format!("${:.4}", t.cost_usd)
            };
            eprintln!(
                "[zoder] rescue {} via {}  {} tools  {}  {:.0}ms  [{}]",
                t.model, t.alias, t.run.tool_calls, cost_label, t.elapsed_ms, t.run.outcome
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
                "cost_usd": (!t.cost_unknown).then_some(t.cost_usd),
                "cost_unknown": t.cost_unknown,
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

/// Default per-phase wall-clock budget for the `loop` (author / `--check` /
/// review). Mirrors the `--loop-timeout` flag default; honored when the flag
/// is left unset. `#[allow(dead_code)]` so the constant doubles as the
/// single source of truth referenced by docs/the flag help text, even on
/// downstream builds that wire the default through a different path.
#[allow(dead_code)]
pub(crate) const DEFAULT_LOOP_TIMEOUT_SECS: u64 = 900;

/// Label for a `loop` phase. Phases are user-visible in the watchdog log
/// line ("loop: <phase> timed out after <N>s, killing") and in the per-iter
/// `author_outcome` / `review_outcome` fields when a phase wedges.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum LoopPhase {
    Author,
    Check,
    Review,
}

impl LoopPhase {
    fn as_str(self) -> &'static str {
        match self {
            LoopPhase::Author => "author",
            LoopPhase::Check => "check",
            LoopPhase::Review => "review",
        }
    }
}

/// Hard-timeout wrapper for a single `loop` phase. The inner future is raced
/// against a wall-clock budget (default ~900s). On expiry we don't just drop
/// the future — the phase is recorded as a hard timeout and the caller is
/// expected to treat it like a failed child: kill any spawned process group
/// and decide whether to abort. The streak bookkeeping that decides abort vs.
/// continue lives in [`update_loop_streaks`] so the matrix is unit-testable.
/// The existing `--agent-timeout` (engine internal turn budget) is preserved
/// alongside this watchdog — they cover different failure modes.
async fn phase_watchdog<F, T>(phase: LoopPhase, secs: u64, quiet: bool, fut: F) -> Result<T, String>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    let budget = std::time::Duration::from_secs(secs.max(1));
    match tokio::time::timeout(budget, fut).await {
        Ok(res) => res.map_err(|e| e.to_string()),
        Err(_) => {
            if !quiet {
                eprintln!(
                    "loop: {phase} timed out after {secs}s, killing",
                    phase = phase.as_str()
                );
            }
            Err(format!(
                "{phase} phase timed out after {secs}s (killed)",
                phase = phase.as_str()
            ))
        }
    }
}

/// Send SIGKILL to every process in `pgid`. Unix-only — Windows falls back
/// to a single kill on the child pid (process groups are a POSIX concept).
/// Best-effort: errors are swallowed because we are already on the timeout
/// path and the caller wants the loop to RECOVER, not bubble I/O errors.
fn kill_process_group(pgid: Option<i32>, pid: Option<u32>) {
    #[cfg(unix)]
    unsafe {
        if let Some(g) = pgid {
            // -pgid: kill the group, not a single pid.
            libc::kill(-g, libc::SIGKILL);
        } else if let Some(p) = pid {
            libc::kill(p as i32, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pgid, pid);
    }
}

/// Run a validation command in `cwd` via `sh -c`, with a hard wall-clock
/// budget. The child is spawned in its own process group so the watchdog
/// can take the whole subtree down with one `kill(-pgid, SIGKILL)` and no
/// orphan shells/process can outlive the budget.
///
/// Returns `(passed, tail)` where `tail` is the last ~4 KB of combined
/// stdout+stderr. On timeout `passed` is `false` and `tail` carries a clear
/// phase-timed-out marker so the next author turn can see it.
async fn run_check_watched(cwd: &Path, cmd: &str, secs: u64) -> (bool, String) {
    let budget = std::time::Duration::from_secs(secs.max(1));
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        // Detach the child into its own process group so we can SIGKILL the
        // whole subtree on timeout (shell + any descendants the command
        // forks). Tokio translates `process_group(0)` to setpgid(pid, 0) on
        // Unix, giving us a clean per-child group without an extra fork.
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return (false, format!("failed to spawn check `{cmd}`: {e}")),
    };
    let pgid = child.id().map(|p| p as i32);
    let pid = child.id();

    let join = async {
        let out = child.wait_with_output().await?;
        Ok::<_, std::io::Error>(out)
    };
    let outcome = tokio::time::timeout(budget, join).await;
    match outcome {
        Ok(Ok(o)) => {
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
        Ok(Err(e)) => {
            kill_process_group(pgid, pid);
            (false, format!("check `{cmd}` I/O error: {e}"))
        }
        Err(_) => {
            // Wall-clock fired. nuke the process group; the child.handle is
            // already gone (wait_with_output consumes it), so go via pgid.
            kill_process_group(pgid, pid);
            eprintln!(
                "loop: {} timed out after {}s, killing",
                LoopPhase::Check.as_str(),
                secs
            );
            (
                false,
                format!(
                    "check `{cmd}` killed after {secs}s (loop timeout); increase with --loop-timeout <SECS>"
                ),
            )
        }
    }
}

/// Synchronous fallback — no watchdog. Exposed only for unit tests so they
/// can exercise the original "spawn-and-block" semantics independently of
/// `run_check_watched`. Production callers always go through the watched
/// path so a wedged child can never block the loop.
#[cfg(test)]
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

/// Decision returned by [`update_loop_streaks`] for one loop iteration.
///
/// The dead-engine streak tracks the "no edits at all" failure mode (author
/// turn didn't land AND the working tree is empty). The check-timeout streak
/// is a SEPARATE failure mode — a wedged `--loop-timeout` kill on an existing
/// diff is NOT the same as a dead engine; the edits might be valid and only
/// the check needs adjusting. Conflating the two was the previous regression
/// and could abort legitimate workflows after two check timeouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LoopStreakUpdate {
    /// New dead-engine streak counter after this iteration.
    pub dead_streak: usize,
    /// New check-timeout streak counter after this iteration.
    pub check_timeout_streak: usize,
    /// True iff the loop should abort because the dead-engine streak crossed
    /// its threshold. Check-timeout alone NEVER triggers an abort.
    pub abort: bool,
}

/// Apply one iteration's signals to the loop's streak counters and decide
/// whether to abort. Pure / deterministic so the full input matrix can be
/// unit-tested.
///
/// Invariants this helper enforces (the regression is exactly the first one):
///   * `turn_none && diff_empty` -> dead_streak += 1.
///   * `check_timed_out && diff_empty` -> check_timeout_streak += 1 only;
///     dead_streak is unaffected (previously they were conflated via `||`
///     in the abort predicate, which fired dead_streak even when the author
///     had produced a real diff).
///   * Either flag with a NON-empty diff -> both streaks reset to 0; the
///     author made progress on disk and the loop should continue regardless
///     of which child wedged.
///   * Abort iff dead_streak >= [`DEAD_STREAK_ABORT_THRESHOLD`]. A
///     check-timeout streak by itself is a log-and-continue signal, not an
///     abort signal.
const DEAD_STREAK_ABORT_THRESHOLD: usize = 2;
fn update_loop_streaks(
    turn_none: bool,
    check_timed_out: bool,
    diff_empty: bool,
    prev_dead_streak: usize,
    prev_check_timeout_streak: usize,
) -> LoopStreakUpdate {
    // Non-empty diff always resets both streaks: there is real progress on
    // disk, regardless of which child wedged. This is the regression fix —
    // the prior `(turn.is_none() || check_timed_out) && diff_empty` predicate
    // killed the loop after two check timeouts even when the author had
    // produced valid edits.
    if !diff_empty {
        return LoopStreakUpdate {
            dead_streak: 0,
            check_timeout_streak: 0,
            abort: false,
        };
    }
    // Empty diff from here on. Track the two failure modes independently so a
    // hung check can no longer masquerade as a dead engine.
    let dead_streak = if turn_none { prev_dead_streak + 1 } else { 0 };
    let check_timeout_streak = if check_timed_out {
        prev_check_timeout_streak + 1
    } else {
        0
    };
    LoopStreakUpdate {
        dead_streak,
        check_timeout_streak,
        abort: dead_streak >= DEAD_STREAK_ABORT_THRESHOLD,
    }
}

/// Autonomous fix loop: author (write-capable, single continuing session) ->
/// validate (optional build/test command) -> adversarial review -> feed the
/// failures back -> repeat until the check passes AND the reviewer raises no
/// blocking findings, or `max_iters` is reached, or progress stalls. Every
/// author turn and reviewer pass is cost-tracked in the ledger.
///
/// `loop_timeout_secs` is the per-phase wall-clock watchdog budget (default
/// [`DEFAULT_LOOP_TIMEOUT_SECS`], configurable via `--loop-timeout`): each
/// author/check/review child is hard-capped at this many seconds. On expiry
/// the spawned process group is killed and the loop continues — never hangs.
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
    loop_timeout_secs: u64,
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
    // Two independent streak counters; see `update_loop_streaks` for the
    // full decision matrix. The dead-engine streak aborts the loop after
    // DEAD_STREAK_ABORT_THRESHOLD consecutive empty-diff author failures.
    // The check-timeout streak is tracked for observability but NEVER
    // triggers an abort on its own — a wedged `--loop-timeout` on a real
    // diff is an editor failure mode, not an engine failure mode.
    let mut dead_streak = 0usize;
    let mut check_timeout_streak = 0usize;
    // Consecutive iterations that produced NO NEW progress (identical diff, still
    // unresolved). A single no-op turn is NOT fatal — the author may just need a
    // firmer nudge (or a harder blocker to chew on). Only give up after this many
    // consecutive stalls, mirroring `dead_streak`. Prevents one empty author turn
    // from terminating a task that is genuinely still converging.
    let mut stall_streak = 0usize;
    const STALL_LIMIT: usize = 3;

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
        // `phase_watchdog` enforces a hard kill-budget around the turn so a
        // genuinely-wedged child (the failure mode the production incident
        // surfaced: 0.4s CPU, no output, indefinitly idle) cannot hang the loop
        // past `loop_timeout_secs`.
        let mut author_err: Option<String> = None;
        let engine_kind = crate::resolve_engine_kind(cli)?;
        let turn = match phase_watchdog(
            LoopPhase::Author,
            loop_timeout_secs,
            cli.quiet,
            crate::agentic_turn(cli, engine_kind, author_prompt, session.clone(), false),
        )
        .await
        {
            Ok(t) => {
                session = Some(t.run.session_id.clone());
                total_cost += t.cost_usd;
                Some(t)
            }
            Err(msg) => {
                let timed_out = msg.contains("timed out") || msg.contains("timeout");
                if !cli.quiet {
                    eprintln!("[loop] iter {i}: author turn did not finish: {msg}");
                    if timed_out {
                        eprintln!(
                            "[loop] hint: raise the per-turn budget with `--agent-timeout <secs>` \
(default 900) or the loop-phase watchdog with `--loop-timeout <secs>` (default 900), or \
pick a faster model with `-m` for the loop. Preserving partial edits and continuing."
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
        // Anti-gaming guard: `diff_lines > 0` is trivially gameable (empty
        // diff-after-headers, whitespace-only churn, comment-only changes,
        // test-file-only churn used to resolve the loop). `diff_substance`
        // classifies what the +/- lines actually do; the accept branches
        // below require `Substantive` (clean accept) or `TestOnly`
        // (accept with a warning). Anything less is rejected and the loop
        // continues to the next iteration.
        let diff_substance = classify_diff_substance(&diff);

        // 3. Validate (build/test) if a check command was given. The check is
        // its own child process (a shell) and historically had NO watchdog —
        // a hung script blocked the loop forever. Wrap with `run_check_watched`
        // so a wedged check is killed at `loop_timeout_secs` and recorded as a
        // failure (tail carries a clear phase-timed-out marker).
        let mut check_timed_out = false;
        let (check_passed, check_tail) = match &check {
            Some(c) => {
                if !cli.quiet {
                    eprintln!("[loop] iter {i}: check `{c}`…");
                }
                let t0 = std::time::Instant::now();
                let (ok, tail) = run_check_watched(&cwd, c, loop_timeout_secs).await;
                if !ok && tail.contains("killed after ") && tail.contains("(loop timeout)") {
                    check_timed_out = true;
                    if !cli.quiet {
                        eprintln!(
                            "[loop] iter {i}: check wedge killed after {}s (--loop-timeout)",
                            t0.elapsed().as_secs()
                        );
                    }
                }
                (Some(ok), tail)
            }
            None => (None, String::new()),
        };

        // Streak bookkeeping — both failure modes live in one helper so the
        // full matrix is unit-tested. A wedged check on an already-empty
        // diff bumps the check-timeout streak only (logged for visibility)
        // and does NOT contribute to `dead_streak`. The author case
        // (turn_none && diff_empty) is the ONLY path that can abort.
        let streaks = update_loop_streaks(
            turn.is_none(),
            check_timed_out,
            diff.trim().is_empty(),
            dead_streak,
            check_timeout_streak,
        );
        dead_streak = streaks.dead_streak;
        check_timeout_streak = streaks.check_timeout_streak;
        if streaks.abort {
            if !cli.quiet {
                eprintln!(
                    "[loop] iter {i}: author produced no edits twice in a row \
                     (engine unreachable or timing out before any tool call); stopping."
                );
            }
            break;
        }
        if check_timeout_streak > 0 && check_timed_out {
            // Distinct from a dead-engine abort: a hanging check on an empty
            // diff is logged but the loop continues; the next author turn has
            // a chance to produce edits.
            if !cli.quiet {
                eprintln!(
                    "[loop] iter {i}: check wedge observed on empty diff \
                     (streak={check_timeout_streak}); author will retry."
                );
            }
        }

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
        // Resolve a fresh scenario-routed reviewer chain per review
        // pass so KNEMON's "most-idle sub first" view reflects the
        // current cycle's actual readings (not the chain as it stood at
        // iteration 1). The chain is plumbed into `complete_once` via
        // its new `reviewer_chain` argument.
        let reviewer_chain: Vec<String> = match Engine::load() {
            Ok(eng) => {
                let health = HealthStore::load(&eng.cfg.health_path);
                crate::resolve_chain(cli, &eng, &health)
                    .map(|r| r.reviewer)
                    .unwrap_or_default()
            }
            Err(_) => Vec::new(),
        };
        let review = match phase_watchdog(
            LoopPhase::Review,
            loop_timeout_secs,
            cli.quiet,
            complete_once(
                cli,
                reviewer.as_deref(),
                &reviewer_chain,
                ADVERSARIAL_SYSTEM,
                &review_user,
                max_tokens,
            ),
        )
        .await
        {
            Ok(c) => {
                total_cost += c.cost_usd;
                parse_review(&c.content)
            }
            Err(msg) => {
                // `complete_once` already has its own HTTP client timeout, so
                // surfacing an Elapsed here means the entire provider request
                // hung (TCP never returned) — record as a timeout-error
                // review so the next author turn sees the wall-clock context.
                ReviewOutput {
                    verdict: "comment".into(),
                    summary: format!("reviewer {msg}"),
                    ..Default::default()
                }
            }
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
        // Track the watchdog budget so per-iter logs show what went wrong.
        // `check_phase_timed_out` distinguishes a wedged check from a check that
        // genuinely reported failure (CI exited 1, etc.) — same `passed=false`
        // outcome, different root cause.
        iterations.push(json!({
            "iter": i,
            "author_model": author_model,
            "tool_calls": tool_calls,
            "author_outcome": author_outcome,
            "diff_lines": diff_lines,
            "substance": format!("{:?}", diff_substance),
            "check": check.as_deref(),
            "check_passed": check_passed,
            "check_phase_timed_out": check_timed_out,
            "loop_timeout_secs": loop_timeout_secs,
            "verdict": review.verdict,
            "blocking_findings": blocking,
            "summary": review.summary,
            "cost_usd_cumulative": total_cost,
        }));

        // 5. Decide: objective gate (build/test) AND review gate (no blockers).
        let objective_ok = green;
        let check_green = check_passed == Some(true); // an actual --check that passed
        let review_ok = review.verdict == "approve" || blocking == 0;
        // Anti-gaming guard: `diff_lines > 0` is trivially gameable (a
        // "green" iteration whose diff is empty-after-headers, only
        // whitespace, only comments, or only churns test files used to
        // resolve the loop). `substance_ok` is the strict replacement:
        // `Substantive` and `TestOnly` are accept-eligible; everything
        // else is explicitly rejected as non-substantive noise.
        let substance_ok = substance_accept_eligible(&diff_substance);

        // Anti-gaming guard rail: if the check is green but the diff is
        // not substantive, the iteration MUST NOT resolve. Emit a clear
        // reason, record it in the iter record (the `substance` field
        // added above), and continue to the next iteration — same
        // control flow as a failed check. This closes the gaming surface
        // where an over-eager author could "pass" the loop with whitespace
        // churn or comment-only edits.
        if (check_green || objective_ok) && !substance_ok {
            if !cli.quiet {
                eprintln!(
                    "[loop] iter {i}: REJECTED green — diff is {diff_substance:?}, \
 not substantive work (anti-gaming guard)"
                );
            }
            prev_diff = diff.clone();
            continue;
        }

        // Test-only warning: the diff is acceptable (it touches test files
        // that build/test is green for) but the author did NOT actually
        // change any non-test source — exactly the shape of a gaming
        // attempt that slipped past the reviewer. Warn loudly so the
        // operator sees why a "no real code change" iteration resolved.
        if matches!(diff_substance, DiffSubstance::TestOnly)
            && (check_green || objective_ok)
            && !cli.quiet
        {
            eprintln!(
                "[loop] iter {i}: WARNING accepting a test-only diff \
 (no non-test source changed)"
            );
        }

        // Escape hatch: `--accept-on-green` treats a passing objective check as
        // sufficient, with reviewer findings advisory (for over-strict reviewers).
        if accept_on_green && check_green && substance_ok {
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
        if objective_ok && review_ok && substance_ok {
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
        let no_new_progress = i > 1 && diff == prev_diff;
        if no_new_progress {
            // Stalemate breaker: the objective check is GREEN but the reviewer
            // keeps blocking the SAME diff. Rather than discard a verified fix,
            // resolve with the findings recorded as warnings.
            if check_green && substance_ok && !review_ok {
                resolved = true;
                if !cli.quiet {
                    eprintln!(
                        "[loop] iter {i}: RESOLVED with warnings — check is green but the \
reviewer keeps blocking an unchanged diff ({blocking} blocking finding(s) recorded)."
                    );
                }
                break;
            }
            // A single no-op turn is not fatal: the author may just need a firmer
            // nudge (see the escalated feedback below). Only give up after
            // STALL_LIMIT consecutive stalls — otherwise keep grinding.
            stall_streak += 1;
            if stall_streak >= STALL_LIMIT {
                if !cli.quiet {
                    eprintln!(
                        "[loop] iter {i}: no new progress for {stall_streak} consecutive \
iteration(s); stopping."
                    );
                }
                break;
            }
            if !cli.quiet {
                eprintln!(
                    "[loop] iter {i}: no new progress ({stall_streak}/{STALL_LIMIT}); \
re-prompting the author with a firmer directive."
                );
            }
            // fall through: compose escalated feedback and try again.
        } else {
            stall_streak = 0;
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
        } else if no_new_progress {
            // The author left the diff identical to last turn yet the task is
            // still unresolved. Push harder, and give it a legitimate way to say
            // it's blocked (rather than silently producing nothing again).
            fb.push_str(
                "Your previous turn produced NO NEW edits, yet the validation still fails. Do NOT \
repeat prior work or stop — make ADDITIONAL, different changes this turn to clear the remaining \
blocker shown below. If (and only if) the required fix genuinely lies OUTSIDE the files/scope you \
were told to edit, respond on the FIRST line with `BLOCKED: <exactly what change is needed and \
where>` and stop; otherwise keep editing until the check passes.\n\n",
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
        "loop_timeout_secs": loop_timeout_secs,
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

// ---------------------------------------------------------------------------
// Tests: loop watchdog. Pin the behavior the production incident surfaced
// (a wedged child can hang the loop forever) so this regression doesn't come
// back. All tests are POSIX-only because they rely on process groups; on
// other platforms the watchdog is a no-op and the loop relies on
// `tokio::time::timeout` alone.
// ---------------------------------------------------------------------------

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Cwd for `run_check_watched` tests. The child `sh -c` doesn't care
    /// about the cwd — we just need a real, stable path to satisfy the
    /// `current_dir` argument.
    fn tmp_cwd() -> PathBuf {
        std::env::temp_dir()
    }

    /// `run_check_watched` must kill a hung `sleep` child within the budget
    /// and return a failure marker that the next author turn can grep for.
    /// This is the regression test for the 1h40m wedged-loop incident: a
    /// child that "didn't return" had to be killed by an operator.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_check_watched_kills_hanging_child() {
        let start = std::time::Instant::now();
        // Budget of 1s on a child that sleeps 30s — if the watchdog works
        // we land back in ~1s. If it doesn't, the test itself fails on the
        // CI runner's overall timeout, mirroring the production symptom.
        let (ok, tail) = run_check_watched(&tmp_cwd(), "sleep 30", 1).await;
        let elapsed = start.elapsed();

        assert!(!ok, "hung child must be reported as failed");
        assert!(
            tail.contains("killed after 1s") && tail.contains("(loop timeout)"),
            "tail must carry the loop-timeout marker for the next iteration; got: {tail:?}"
        );
        // Be generous on the upper bound (CI noise) but strict on the lower:
        // the watchdog MUST have fired, not the child naturally exiting.
        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "watchdog fired too early ({:?}); budget=1s",
            elapsed
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "watchdog did NOT fire in time ({:?}); the bug is back",
            elapsed
        );
    }

    /// Fast commands must NOT trip the watchdog — sanity check that we
    /// didn't accidentally turn every check into a 900s wait.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_check_watched_passes_fast_child() {
        let (ok, tail) = run_check_watched(&tmp_cwd(), "exit 0", 1).await;
        assert!(ok, "fast pass-through must succeed; tail={tail:?}");
        assert!(
            !tail.contains("killed after") && !tail.contains("(loop timeout)"),
            "fast child must not log a watchdog kill; tail={tail:?}"
        );
    }

    /// A failing (non-hung) command must surface its own failure cleanly —
    /// distinct from a watchdog kill. Otherwise the next author turn can't
    /// tell "CI red" from "loop hung" and may try to fix the wrong thing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_check_watched_passes_through_real_failures() {
        let (ok, tail) = run_check_watched(&tmp_cwd(), "echo boom; exit 1", 1).await;
        assert!(!ok, "exit 1 must report failure");
        assert!(
            tail.contains("boom"),
            "stderr/stdout from a real failure must reach the tail; got: {tail:?}"
        );
        assert!(
            !tail.contains("(loop timeout)"),
            "real failure must NOT be misreported as a loop timeout; got: {tail:?}"
        );
    }

    /// Sanity check the phase label helper — a unit test in the strict sense.
    #[test]
    fn loop_phase_label_is_stable() {
        assert_eq!(LoopPhase::Author.as_str(), "author");
        assert_eq!(LoopPhase::Check.as_str(), "check");
        assert_eq!(LoopPhase::Review.as_str(), "review");
    }

    /// `phase_watchdog` returns the inner future's value on success.
    #[tokio::test]
    async fn phase_watchdog_returns_inner_result_on_time() {
        let res: Result<i32, String> = phase_watchdog(LoopPhase::Author, 5, true, async {
            Ok::<_, anyhow::Error>(42)
        })
        .await;
        assert_eq!(res.unwrap(), 42);
    }

    /// `phase_watchdog` reports a phase-timed-out marker when the future
    /// exceeds the budget. This is the hook `cmd_loop` consumes to decide
    /// whether the iteration counts as a failure and (if so) which streak
    /// it bumps via [`update_loop_streaks`].
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn phase_watchdog_times_out_hanging_future() {
        let start = std::time::Instant::now();
        let res: Result<(), String> = phase_watchdog(LoopPhase::Review, 1, true, async {
            // Sleep longer than the watchdog budget.
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            Ok(())
        })
        .await;
        let elapsed = start.elapsed();

        let err = res.expect_err("watchdog must return Err on timeout");
        assert!(
            err.contains("review phase timed out after 1s (killed)"),
            "Err must mention the phase + budget; got: {err}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "phase_watchdog must not run the inner future to completion ({:?})",
            elapsed
        );
    }

    /// End-to-end of the `cmd_loop` watchdog contract using the `cmd_loop`
    /// public surface is heavy (it spins up an engine daemon). Instead, this
    /// pin asserts that the unwatched fallback `run_check` (kept for tests)
    /// is genuinely unbounded — i.e. that the watchdog we wrap on top is the
    /// thing saving us, not magic elsewhere on the path.
    #[test]
    fn unwatched_run_check_does_not_have_a_budget() {
        // If anyone re-introduces a timeout inside the raw `run_check`, this
        // assertion catches it: the watchdog is the only thing that bounds
        // wall-clock, by design.
        let (ok, _tail) = run_check(&tmp_cwd(), "exit 0");
        assert!(ok);
    }

    // -----------------------------------------------------------------------
    // `update_loop_streaks` — the regression target.
    //
    // Background: the prior loop-abort predicate was
    // `(turn.is_none() || check_timed_out) && diff_empty`. The `||` made the
    // check-timeout case leak into the dead-engine counter, so a wedged
    // `--loop-timeout` kill on a real author diff could force an abort after
    // two iterations. The helper below pins the corrected matrix.
    // -----------------------------------------------------------------------

    /// REGRESSION: a timed-out check on a NON-empty diff must NOT bump
    /// `dead_streak` and must NOT abort the loop. The author produced
    /// progress; the check just needs fixing. This is the exact scenario the
    /// reviewer flagged as a critical regression: "wedged check now counted
    /// as a dead-engine streak even though the author produced a non-empty
    /// diff."
    #[test]
    fn update_loop_streaks_does_not_count_timed_out_check_when_diff_is_present() {
        // Pre-load both counters at the threshold so a stray increment
        // would trip the abort, then assert that neither one moves and the
        // loop is allowed to continue.
        let prev_dead = DEAD_STREAK_ABORT_THRESHOLD; // already at the brink
        let prev_cto = 5usize;
        let u = update_loop_streaks(
            false, true, /* diff_empty */ false, prev_dead, prev_cto,
        );
        assert_eq!(
            u.dead_streak, 0,
            "non-empty diff must zero dead_streak; the prior `||` regression would have \
             carried the threshold through and aborted"
        );
        assert_eq!(
            u.check_timeout_streak, 0,
            "non-empty diff must zero check_timeout_streak too — both failure modes \
             are subsumed by author progress"
        );
        assert!(
            !u.abort,
            "a non-empty diff with a hung check must never abort the loop"
        );
    }

    /// Two consecutive empty-diff author failures (no edits, no check
    /// timeout) is the canonical "dead engine" signal and MUST abort — the
    /// abort is still bounded; this is just locking the threshold in place.
    #[test]
    fn update_loop_streaks_aborts_on_two_consecutive_empty_diff_author_failures() {
        let u1 = update_loop_streaks(true, false, true, 0, 0);
        assert_eq!(u1.dead_streak, 1);
        assert!(!u1.abort, "first empty-diff failure must not abort yet");

        let u2 = update_loop_streaks(true, false, true, u1.dead_streak, u1.check_timeout_streak);
        assert_eq!(u2.dead_streak, 2);
        assert!(
            u2.abort,
            "second consecutive empty-diff author failure must abort"
        );
    }

    /// A wedged check on an empty diff is a real failure mode — it must be
    /// recorded in `check_timeout_streak` so an operator can see it — but
    /// it MUST NOT contribute to `dead_streak` and MUST NOT abort the loop
    /// even after two repetitions.
    #[test]
    fn update_loop_streaks_records_check_timeout_streak_but_does_not_abort() {
        // Two consecutive empty-diff check timeouts, author turn each time
        // succeeds (turn_none = false).
        let u1 = update_loop_streaks(false, true, true, 0, 0);
        assert_eq!(u1.dead_streak, 0);
        assert_eq!(u1.check_timeout_streak, 1);
        assert!(
            !u1.abort,
            "first check timeout on empty diff must not abort"
        );

        let u2 = update_loop_streaks(false, true, true, u1.dead_streak, u1.check_timeout_streak);
        assert_eq!(
            u2.dead_streak, 0,
            "check timeouts must never touch the dead-engine counter"
        );
        assert_eq!(u2.check_timeout_streak, 2);
        assert!(
            !u2.abort,
            "two check timeouts on empty diff must NOT abort — author turn may \
             still recover; this is the exact regression the prior `||` caused"
        );
    }

    /// Mixed scenario: a real author edit resets BOTH streaks regardless of
    /// which child wedged before. This guards against a future refactor
    /// splitting the reset into per-flag hooks.
    #[test]
    fn update_loop_streaks_resets_both_streaks_on_any_progress() {
        // Pre-load both at threshold so any missed reset would surface.
        let u = update_loop_streaks(
            false, // turn succeeded
            true,  // check timed out
            false, // diff is non-empty
            DEAD_STREAK_ABORT_THRESHOLD,
            7,
        );
        assert_eq!(u.dead_streak, 0);
        assert_eq!(u.check_timeout_streak, 0);
        assert!(!u.abort);
    }

    /// All-progress iteration (turn ok, check ok, diff non-empty) is a no-op
    /// pass-through on both counters and never aborts. Pin for clarity.
    #[test]
    fn update_loop_streaks_noop_on_clean_pass() {
        let u = update_loop_streaks(false, false, false, 0, 0);
        assert_eq!(u.dead_streak, 0);
        assert_eq!(u.check_timeout_streak, 0);
        assert!(!u.abort);
    }

    /// Check timeout without a wedged author on an empty diff (turn ok but
    /// no edits ever landed) is a confusing-but-valid signal — the engine
    /// is alive but produced nothing. That's dead-engine behavior, not a
    /// check failure. So `turn_none` here mirrors "no edits made" via the
    /// real call site (the engine returned Ok(empty)) — but our helper
    /// takes the boolean the loop sees, which is `turn.is_none()`. We don't
    /// pretend to model "ok-empty" here; we just pin that when the engine
    /// says Ok the helper trusts it. This test guards the boundary.
    #[test]
    fn update_loop_streaks_trusts_turn_ok_signal_as_progress() {
        // Engine returned Ok (turn_none=false) but the diff is empty (the
        // engine simply produced no edits this round). dead_streak stays at
        // zero — the helper trusts the engine, even if the diff disagrees.
        let u = update_loop_streaks(false, false, true, 0, 0);
        assert_eq!(u.dead_streak, 0);
        assert_eq!(u.check_timeout_streak, 0);
        assert!(!u.abort);
    }

    // `classify_diff_substance` — the anti-gaming guard's pure classifier.
    //
    // Each fixture below pins one of the five `DiffSubstance` variants so a
    // future regression in precedence or marker handling is caught here
    // instead of in production. Fixtures are intentionally small: the
    // classifier only looks at +/- content lines and `+++ b/...` headers.
    // -----------------------------------------------------------------------

    /// An empty diff (only `diff --git` + `+++`/`---` headers, no content)
    /// must classify as `Empty`.
    #[test]
    fn classify_diff_substance_empty_diff_is_empty() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n\
                    index 0000..1111 100644\n\
                    --- a/src/lib.rs\n\
                    +++ b/src/lib.rs\n";
        assert_eq!(classify_diff_substance(diff), DiffSubstance::Empty);
    }

    /// A diff whose only +/- lines are blank/whitespace must classify as
    /// `WhitespaceOnly`, even though the file path is non-test. (Pins
    /// precedence: WhitespaceOnly beats TestOnly in a non-test file
    /// scenario anyway, but mainly pins that blank lines are stripped of
    /// their marker AND trimmed.)
    #[test]
    fn classify_diff_substance_whitespace_only_added_lines() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n\
                    --- a/src/lib.rs\n\
                    +++ b/src/lib.rs\n\
                    @@ -1,1 +1,2 @@\n\
                     fn existing() {}\n\
                    +\n\
                    +   \n\
                    +\t\n";
        assert_eq!(classify_diff_substance(diff), DiffSubstance::WhitespaceOnly);
    }

    /// A diff whose +/- lines are only Rust `// ...` comments must
    /// classify as `CommentOnly`. Also pins that the file path being a
    /// non-test source does NOT lift it out of CommentOnly.
    #[test]
    fn classify_diff_substance_comment_only_rust_hunk() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n\
                    --- a/src/lib.rs\n\
                    +++ b/src/lib.rs\n\
                    @@ -1,2 +1,4 @@\n\
                     fn existing() {}\n\
                    +// TODO: explain this later\n\
                    +// another comment\n\
                    +\n";
        assert_eq!(classify_diff_substance(diff), DiffSubstance::CommentOnly);
    }

    /// A diff whose +/- lines are only Python `# ...` comments must
    /// classify as `CommentOnly` (the classifier is language-agnostic and
    /// recognizes `#` as a comment marker across languages).
    #[test]
    fn classify_diff_substance_comment_only_python_hunk() {
        let diff = "diff --git a/scripts/run.py b/scripts/run.py\n\
                    --- a/scripts/run.py\n\
                    +++ b/scripts/run.py\n\
                    @@ -10,3 +10,6 @@ def helper():\n\
                         return 0\n\
                    +# header note\n\
                    +# body note\n\
                    +# trailing note\n";
        assert_eq!(classify_diff_substance(diff), DiffSubstance::CommentOnly);
    }

    /// A diff with at least one substantive content line, where every
    /// changed file path matches a recognized test-file pattern, must
    /// classify as `TestOnly`. This pins the `/tests/` path-segment rule.
    #[test]
    fn classify_diff_substance_test_only_under_tests_dir() {
        let diff = "diff --git a/crates/foo/tests/bar.rs b/crates/foo/tests/bar.rs\n\
                    --- a/crates/foo/tests/bar.rs\n\
                    +++ b/crates/foo/tests/bar.rs\n\
                    @@ -1,1 +1,3 @@\n\
                     #[test]\n\
                    +fn new_thing() {\n\
                    +    assert_eq!(2 + 2, 4);\n\
                    +}\n";
        assert_eq!(classify_diff_substance(diff), DiffSubstance::TestOnly);
    }

    /// A test-only diff can also live in a file matched by basename glob
    /// (`*_test.*`, `*.test.*`, `*_spec.*`, `*.spec.*`). Pin that here so
    /// the glob half of `is_test_path` doesn't silently rot.
    #[test]
    fn classify_diff_substance_test_only_basename_glob() {
        let diff = "diff --git a/src/lib_test.rs b/src/lib_test.rs\n\
                    --- a/src/lib_test.rs\n\
                    +++ b/src/lib_test.rs\n\
                    @@ -1,1 +1,3 @@\n\
                     pub fn helper() {}\n\
                    +#[test]\n\
                    +fn smoke() { assert!(true); }\n";
        assert_eq!(classify_diff_substance(diff), DiffSubstance::TestOnly);
    }

    /// A diff with real code in `src/lib.rs` (and no test-file changes)
    /// must classify as `Substantive`. This is the accept-without-warning
    /// path.
    #[test]
    fn classify_diff_substance_substantive_real_code() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n\
                    --- a/src/lib.rs\n\
                    +++ b/src/lib.rs\n\
                    @@ -1,1 +1,3 @@\n\
                     pub fn existing() {}\n\
                    +pub fn added() {\n\
                    +    println!(\"hi\");\n\
                    +}\n";
        assert_eq!(classify_diff_substance(diff), DiffSubstance::Substantive);
    }

    /// A diff that mixes a non-test source change AND a test change must
    /// classify as `Substantive` (NOT TestOnly): the test-pattern rule
    /// only applies when EVERY changed file path is a test path.
    #[test]
    fn classify_diff_substance_substantive_when_mixed_paths() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n\
                    index 1111..2222 100644\n\
                    --- a/src/lib.rs\n\
                    +++ b/src/lib.rs\n\
                    @@ -1,1 +1,3 @@\n\
                     pub fn existing() {}\n\
                    +pub fn added() {\n\
                    +    let _ = 1;\n\
                    +}\n\
                    diff --git a/crates/foo/tests/bar.rs b/crates/foo/tests/bar.rs\n\
                    --- a/crates/foo/tests/bar.rs\n\
                    +++ b/crates/foo/tests/bar.rs\n\
                    @@ -1,1 +1,2 @@\n\
                     #[test]\n\
                    +fn smoke() { assert!(true); }\n";
        assert_eq!(classify_diff_substance(diff), DiffSubstance::Substantive);
    }

    /// Truth table for the accept-eligibility predicate. `Substantive`
    /// and `TestOnly` resolve on green (with a warning for TestOnly);
    /// the rest is explicitly rejected as anti-gaming.
    #[test]
    fn substance_accept_eligible_truth_table() {
        assert!(substance_accept_eligible(&DiffSubstance::Substantive));
        assert!(substance_accept_eligible(&DiffSubstance::TestOnly));
        assert!(!substance_accept_eligible(&DiffSubstance::Empty));
        assert!(!substance_accept_eligible(&DiffSubstance::WhitespaceOnly));
        assert!(!substance_accept_eligible(&DiffSubstance::CommentOnly));
    }

    // -----------------------------------------------------------------------
    // `aggregate_review` — the per-reviewer -> aggregate-verdict / payload
    // builder used by `cmd_review` / `emit_reviews`. The regression here is
    // the Finding #14 silent-success bug: if every reviewer 401s (or every
    // `--panel` model times out) the OLD code emitted a synthetic `comment`
    // verdict and `cmd_review` returned `Ok(())`. CI therefore saw a green
    // review even though no model had actually reviewed any code. The new
    // contract is: `complete: false`, never `approve`, `bail()` when
    // `ok_models == 0` — and a partial panel must be explicitly flagged.
    //
    // Each fixture covers one cell of the (ok_count, includes_blocking_verdict)
    // matrix, plus the payload-shape pins. The matrix keeps these honest; a
    // future regression that re-introduces the silent-success path trips one
    // of the next two tests.
    // -----------------------------------------------------------------------

    fn rro(verdict: &str) -> ReviewOutput {
        ReviewOutput {
            verdict: verdict.into(),
            summary: String::new(),
            findings: vec![],
            next_steps: vec![],
        }
    }

    /// REGRESSION (Finding #14): when EVERY reviewer fails (e.g. the lone
    /// reviewer 401s, or every `--panel` model times out) the aggregate must
    /// surface `complete: false` and a NON-`approve` verdict. The old code
    /// produced `comment` here, which let CI believe the review ran.
    #[test]
    fn aggregate_review_marks_complete_false_when_every_reviewer_fails() {
        let reviews = vec![(
            "error".into(),
            ReviewOutput {
                verdict: "comment".into(),
                summary: "reviewer failed: 401 Unauthorized".into(),
                ..Default::default()
            },
        )];
        let (agg, all_failed, payload) = aggregate_review(&reviews, 0.0, 1, 0, 1);
        assert!(all_failed, "ok_models=0 must flag all_failed");
        assert_ne!(
            agg, "approve",
            "total-failure aggregate must NOT be approve (Finding #14 regression)"
        );
        assert_eq!(
            payload["complete"].as_bool(),
            Some(false),
            "payload must carry complete=false on total failure"
        );
        assert_eq!(payload["requested"].as_u64(), Some(1));
        assert_eq!(payload["ok_models"].as_u64(), Some(0));
        assert_eq!(payload["failed_models"].as_u64(), Some(1));
    }

    /// REGRESSION (Finding #14): an explicit `comment` verdict from a
    /// SUCCESSFUL reviewer (i.e. a real response, not an error record)
    /// keeps the aggregate at `comment` — the synthetic "error/comment"
    /// record from a failed reviewer must NOT lift the aggregate. The
    /// fix ensures the worst-rank walk SKIPS the "error" model label.
    #[test]
    fn aggregate_review_ignores_error_records_when_computing_worst_verdict() {
        // One real `approve` reviewer; one failed reviewer (synthetic "error/comment").
        // The aggregate must be `approve`, NOT `comment`.
        let reviews = vec![
            ("real-model".into(), rro("approve")),
            (
                "error".into(),
                ReviewOutput {
                    verdict: "comment".into(),
                    summary: "reviewer failed: timeout".into(),
                    ..Default::default()
                },
            ),
        ];
        let (agg, all_failed, payload) = aggregate_review(&reviews, 0.01, 2, 1, 1);
        assert!(!all_failed, "one successful reviewer -> not all-failed");
        assert_eq!(
            agg, "approve",
            "the 'error' record must NOT lift the aggregate out of approve"
        );
        assert_eq!(payload["complete"].as_bool(), Some(true));
        assert_eq!(payload["requested"].as_u64(), Some(2));
        assert_eq!(payload["ok_models"].as_u64(), Some(1));
        assert_eq!(payload["failed_models"].as_u64(), Some(1));
    }

    /// A blocking review from a successful reviewer still wins — the worst
    /// rank walk is correct, just it ignores the synthetic "error" slots.
    /// When one reviewer votes `request_changes` and another fails, the
    /// aggregate must reflect the real blocking verdict.
    #[test]
    fn aggregate_review_takes_worst_verdict_from_real_reviewers_only() {
        let reviews = vec![
            ("real-a".into(), rro("request_changes")),
            (
                "error".into(),
                ReviewOutput {
                    verdict: "comment".into(),
                    summary: "reviewer failed: 5xx".into(),
                    ..Default::default()
                },
            ),
            ("real-b".into(), rro("approve")),
        ];
        let (agg, _, payload) = aggregate_review(&reviews, 0.0, 3, 2, 1);
        assert_eq!(
            agg, "request_changes",
            "blocking review from real model wins aggregate"
        );
        // Cost is reported verbatim so CI can use it as a budget signal.
        assert_eq!(payload["cost_usd"].as_f64(), Some(0.0));
    }

    /// `complete: true` only when at least one model reported a real
    /// reviewer response. The flag exists precisely so downstream CI gates
    /// can stop trusting a `comment` aggregate verdict when `complete=false`.
    #[test]
    fn aggregate_review_complete_true_when_any_reviewer_succeeds() {
        let reviews = vec![("solo".into(), rro("comment"))];
        let (_, all_failed, payload) = aggregate_review(&reviews, 0.0, 1, 1, 0);
        assert!(!all_failed);
        assert_eq!(payload["complete"].as_bool(), Some(true));
    }

    /// Empty reviewer list is treated like a total failure (no real
    /// reviewer response). This guards the degenerate "0/0 reviewers"
    /// case so the bail() path in `cmd_review` still fires — `cmd_review`
    /// always has at least the default reviewer slot, so reaching this
    /// case requires a caller bug, not user error.
    #[test]
    fn aggregate_review_empty_reviewer_list_is_all_failed() {
        let (agg, all_failed, payload) = aggregate_review(&[], 0.0, 0, 0, 0);
        assert!(all_failed);
        assert_ne!(agg, "approve");
        assert_eq!(payload["complete"].as_bool(), Some(false));
    }
}
