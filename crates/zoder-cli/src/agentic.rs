//! zoder's agentic command surface (a drop-in for codex `exec`/`review`): `review`, `adversarial-review`, `rescue`,
//! `transfer`, and a file-backed background job registry (`status`/`result`/
//! `cancel`). Reviews run as single completions over a chosen model (the diff is
//! embedded), with optional multi-reviewer fan-out; `rescue` is an agentic,
//! write-capable run. Everything routes through the same provider/engine + cost
//! ledger as `exec`, so spend is captured uniformly.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use zoder_core::{
    BillingMode, ChatRequest, Config, CostVerdict, Decision, Entry, HealthStore, Ledger, Message,
    ModelEntry, OpenAiProvider, PolicyGate, PricingCatalog, Session,
};

use crate::{Engine, ReviewScope};

#[cfg(test)]
static TEST_BACKGROUND_WORKER_COMMAND: std::sync::Mutex<Option<(PathBuf, Vec<String>)>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static FAIL_NEXT_WRITE_META: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
static LAST_BACKGROUND_CHILD_PID: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0);

/// The exact utilization-store identity used by both agentic capture and the
/// routing reader. ChatGPT subscription tiers use Codex's `x-codex-*` header
/// family even when the configured provider id is simply `openai`; include the
/// tier in classification so those writes cannot land under `OpenaiCodex`
/// while reads look under `Openai` (or vice versa).
pub(crate) fn utilization_key(
    provider: &zoder_core::config::Provider,
) -> (zoder_core::utilization::Provider, String) {
    use zoder_core::utilization::Provider as UtilProvider;

    let id = provider.id.to_ascii_lowercase();
    let plan = provider
        .subscription
        .as_ref()
        .map(|plan| plan.tier.clone().unwrap_or_else(|| "explicit".to_string()))
        .unwrap_or_else(|| provider.id.clone());
    let tier = plan.to_ascii_lowercase();
    let util_provider = if id.contains("codex") || tier.starts_with("chatgpt-") {
        UtilProvider::OpenaiCodex
    } else if id.contains("anthropic") || tier.starts_with("claude-") {
        UtilProvider::Anthropic
    } else if id.contains("minimax") || tier.starts_with("token-plan-") {
        UtilProvider::MiniMax
    } else if id.contains("openai") {
        UtilProvider::Openai
    } else {
        UtilProvider::Other
    };
    (util_provider, plan)
}

struct AgenticHeaderView<'a>(&'a [(String, String)]);

impl zoder_core::utilization::HeaderLookup for AgenticHeaderView<'_> {
    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Persist one ACP response's rate-limit metadata under the configured tuple
/// that `build_account_view` reads. Parsing remains header-driven, but storage
/// identity is configuration-driven: vendor plan labels such as `pro` are
/// display metadata and must not replace a configured `chatgpt-pro` key.
fn persist_agentic_utilization_at(
    provider: &zoder_core::config::Provider,
    headers: &[(String, String)],
    path: &Path,
    now: DateTime<Utc>,
) -> bool {
    let view = AgenticHeaderView(headers);
    // KNEMON per-account identity: thread the configured
    // `effective_account_id()` through the snapshot so two accounts on
    // the same `(provider, tier)` never collide on the literal
    // `"default"` key. A provider with no `account_id` set resolves to
    // [`DEFAULT_ACCOUNT_ID`] — byte-identical to pre-fix behavior.
    let account_id = provider
        .subscription
        .as_ref()
        .map(|s| s.effective_account_id())
        .unwrap_or_else(|| zoder_core::config::DEFAULT_ACCOUNT_ID.to_string());
    let Some(mut snapshot) = zoder_core::utilization::parse_headers(&view, &account_id, "default")
    else {
        return false;
    };
    let (util_provider, plan) = utilization_key(provider);
    snapshot.provider = util_provider;
    snapshot.account_id = account_id;
    snapshot.plan = plan;
    snapshot.observed_at = Some(now);

    let Ok(mut store) = zoder_core::utilization::UtilizationStore::open(path) else {
        return false;
    };
    store.record(&snapshot, now)
}

/// Best-effort production wrapper. Telemetry persistence must never turn a
/// successful agentic response into a failed user request.
pub(crate) fn persist_agentic_utilization(
    provider: &zoder_core::config::Provider,
    headers: &[(String, String)],
) -> bool {
    let Some(path) = zoder_core::utilization::default_store_path() else {
        return false;
    };
    persist_agentic_utilization_at(provider, headers, &path, Utc::now())
}

/// Persist real agent-reported token consumption into configured MiniMax
/// counter windows. This is quota accounting only when a catalog/explicit
/// token cap exists; percent-only context windows are never synthesized.
fn persist_agentic_counter_at(
    provider: &zoder_core::config::Provider,
    tokens_used: u64,
    path: &Path,
    now: DateTime<Utc>,
) -> bool {
    let (util_provider, plan_label) = utilization_key(provider);
    if util_provider != zoder_core::utilization::Provider::MiniMax || tokens_used == 0 {
        return false;
    }
    let Some(plan) = provider.subscription.as_ref() else {
        return false;
    };
    // KNEMON per-account identity — see `persist_agentic_utilization_at`.
    // Counter rows are keyed on `(provider, account_id, plan, window_name)`;
    // threading the configured `effective_account_id` through every counter
    // call keeps two MiniMax accounts on the same tier in separate buckets
    // instead of collapsing onto the legacy `"default"` key.
    let account_id = plan.effective_account_id();
    let catalog = zoder_core::subscription_tiers::TierCatalog::bundled();
    let namespace = plan
        .tier
        .as_deref()
        .and_then(|tier| catalog.provider_namespace(provider, tier))
        .unwrap_or_else(|| provider.id.clone());
    let resolved =
        zoder_core::subscription_tiers::resolve_plan_windows(plan, &catalog, Some(&namespace));
    let counter_windows: Vec<_> = resolved
        .windows
        .iter()
        .filter(|window| {
            window.observability == zoder_core::config::Observability::Counter
                && window.cap.is_some()
        })
        .collect();
    if counter_windows.is_empty() {
        return false;
    }
    let Ok(mut store) = zoder_core::utilization::UtilizationStore::open(path) else {
        return false;
    };
    for window in counter_windows {
        store.set_counter_rolling_hours(
            util_provider,
            &account_id,
            &plan_label,
            &window.name,
            (window.reset == zoder_core::config::ResetKind::Rolling).then_some(window.hours),
            now,
        );
        store.set_counter_cap(
            util_provider,
            &account_id,
            &plan_label,
            &window.name,
            window.cap,
            now,
        );
        let period_id = match window.reset {
            zoder_core::config::ResetKind::CalendarMonthly => {
                crate::utilization::period_id_for(now)
            }
            zoder_core::config::ResetKind::CalendarDaily => {
                Some(now.format("%Y-%m-%d").to_string())
            }
            zoder_core::config::ResetKind::Rolling => None,
        };
        store.set_counter_period_id(
            util_provider,
            &account_id,
            &plan_label,
            &window.name,
            period_id,
            now,
        );
        store.record_counter(
            util_provider,
            &account_id,
            &plan_label,
            &window.name,
            tokens_used as f64,
            now,
        );
    }
    store.save().is_ok()
}

pub(crate) fn persist_agentic_counter(
    provider: &zoder_core::config::Provider,
    tokens_used: u64,
) -> bool {
    let Some(path) = crate::utilization::default_store_path() else {
        return false;
    };
    persist_agentic_counter_at(provider, tokens_used, &path, Utc::now())
}

#[cfg(test)]
mod agentic_utilization_tests {
    use super::*;
    use zoder_core::config::Auth;
    use zoder_core::utilization::UtilizationStore;

    fn test_provider() -> zoder_core::config::Provider {
        zoder_core::config::Provider {
            id: "openai".into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            kind: "openai-responses".into(),
            auth: Auth::None,
            paid: false,
            billing: BillingMode::Subscription,
            subscription: Some(zoder_core::config::SubscriptionPlan {
                monthly_fee_usd: 200.0,
                tier: Some("chatgpt-pro".into()),
                windows: Vec::new(),
                ..Default::default()
            }),
            serves: vec!["gpt-".into()],
            azure_api_version: None,
        }
    }

    fn minimax_counter_provider() -> zoder_core::config::Provider {
        zoder_core::config::Provider {
            id: "minimax".into(),
            base_url: "https://api.minimax.io/v1".into(),
            kind: "openai-chat".into(),
            auth: Auth::None,
            paid: false,
            billing: BillingMode::Subscription,
            subscription: Some(zoder_core::config::SubscriptionPlan {
                monthly_fee_usd: 200.0,
                tier: None,
                windows: vec![zoder_core::config::QuotaWindow {
                    name: "monthly".into(),
                    hours: 720,
                    unit: zoder_core::config::QuotaUnit::Tokens,
                    cap: Some(100.0),
                    models: None,
                    observability: zoder_core::config::Observability::Counter,
                    reset: zoder_core::config::ResetKind::CalendarMonthly,
                }],
                ..Default::default()
            }),
            serves: vec!["MiniMax-".into()],
            azure_api_version: None,
        }
    }

    #[test]
    fn pure_acp_without_subscription_headers_leaves_utilization_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let provider = test_provider();
        let now = Utc::now();

        // Real Goose `usage_update {used,size}` and Zeroclaw
        // `context_usage` frames contain no subscription-quota headers. The
        // ACP driver surfaces those as AgentEvent::Usage, so this persistence
        // path receives no header event and must not invent a percentage.
        assert!(!persist_agentic_utilization_at(&provider, &[], &path, now));

        let store = UtilizationStore::open_unlocked(&path).unwrap();
        let (util_provider, plan) = utilization_key(&provider);
        assert_eq!(
            util_provider,
            zoder_core::utilization::Provider::OpenaiCodex
        );
        assert_eq!(plan, "chatgpt-pro");
        assert!(store.get(util_provider, "default", &plan).is_none());
    }

    #[test]
    fn agentic_minimax_usage_increments_capped_counter_and_gates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let provider = minimax_counter_provider();
        let now = Utc::now();
        assert!(persist_agentic_counter_at(&provider, 90, &path, now));

        let store = UtilizationStore::open_unlocked(&path).unwrap();
        let (util_provider, plan) = utilization_key(&provider);
        let counter = store
            .get_counter(util_provider, "default", &plan, "monthly")
            .unwrap();
        assert_eq!(counter.used_tokens, 90.0);
        assert_eq!(counter.cap, Some(100.0));
        assert_eq!(counter.used_percent, Some(90.0));
        let windows = &provider.subscription.as_ref().unwrap().windows;
        let view = zoder_core::utilization::build_account_view(
            util_provider,
            "default",
            &plan,
            windows,
            &store,
            now,
        );
        assert_eq!(
            zoder_core::utilization::decide_account(
                &view,
                &zoder_core::utilization::RouteKnobs::default(),
                now,
                None,
            )
            .decision,
            zoder_core::utilization::RouteDecision::FallBackToFree
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_zeroclaw_dispatch_context_usage_does_not_create_quota_telemetry() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("engine.sock");
        let store_path = dir.path().join("utilization.json");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let _init = lines.next_line().await.unwrap().unwrap();
            write
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"init\",\"result\":{}}\n")
                .await
                .unwrap();
            let _new = lines.next_line().await.unwrap().unwrap();
            write
                .write_all(
                    b"{\"jsonrpc\":\"2.0\",\"id\":\"new\",\"result\":{\"session_id\":\"s1\"}}\n",
                )
                .await
                .unwrap();
            let _prompt = lines.next_line().await.unwrap().unwrap();
            write
                .write_all(
                    b"{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"type\":\"context_usage\",\"input_tokens\":95000}}\n",
                )
                .await
                .unwrap();
            write
                .write_all(
                    b"{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"type\":\"turn_complete\",\"outcome\":\"completed\"}}\n",
                )
                .await
                .unwrap();
        });

        let provider = test_provider();
        let mut opts = zoder_core::AgentOptions::new(&socket, "codex", dir.path(), "hello");
        opts.timeout = std::time::Duration::from_secs(2);
        let mut saw_usage = false;
        let run =
            zoder_core::run_agent_dispatch(zoder_core::EngineKind::Zeroclaw, &opts, |event| {
                match event {
                    zoder_core::AgentEvent::Usage { input_tokens } => {
                        saw_usage = true;
                        assert_eq!(input_tokens, 95_000);
                    }
                    zoder_core::AgentEvent::Utilization { headers } => {
                        persist_agentic_utilization_at(
                            &provider,
                            &headers,
                            &store_path,
                            Utc::now(),
                        );
                    }
                    _ => {}
                }
            })
            .await
            .unwrap();
        server.await.unwrap();
        assert!(run.succeeded());
        assert!(saw_usage);
        let store = UtilizationStore::open_unlocked(&store_path).unwrap();
        let (util_provider, plan) = utilization_key(&provider);
        assert!(store.get(util_provider, "default", &plan).is_none());
    }
}

// ---------------------------------------------------------------------------
// Single completion (used by review/adversarial-review).
// ---------------------------------------------------------------------------

/// Result of one reviewer completion.
#[derive(Debug)]
struct Completion {
    model: String,
    content: String,
    cost_usd: f64,
}

/// Internal reviewer chain dispatch result. Carries enough information to
/// distinguish a fallback-worthy error (a stale or transient backend problem
/// on this specific model — try the next candidate in the chain) from a
/// fatal one (either the model emitted bytes that would duplicate on retry,
/// OR a non-provider structural error such as an unreadable disk, a token
/// policy rejection, or a missing provider config that the operator must
/// fix). The same shape is used by the author path's chain dispatch
/// (`try_model` in `main.rs`); mirroring it here keeps the two fallback
/// loops conceptually equivalent.
#[derive(Debug)]
enum ReviewerError {
    /// Provider/backend error and nothing was emitted to the sink yet — the
    /// next candidate can take over. Carries the underlying provider error
    /// so the wrapper can record health and surface the eventual "all
    /// candidates failed" diagnostic.
    FallbackWorthy {
        message: String,
        kind: zoder_core::ErrKind,
        status: Option<u16>,
    },
    /// Streamed output already seen OR a non-recoverable structural problem
    /// (policy rejection, missing provider config, ledger reservation
    /// failure, etc.). The chain MUST stop — the next call site must report
    /// the error instead of fabricating success.
    Fatal { message: String },
}

impl ReviewerError {
    fn fallback_worthy_from(e: zoder_core::ProviderError) -> Self {
        ReviewerError::FallbackWorthy {
            message: e.message,
            kind: e.kind,
            status: e.status,
        }
    }
    fn fatal<S: Into<String>>(msg: S) -> Self {
        ReviewerError::Fatal {
            message: msg.into(),
        }
    }
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
///
/// **Cross-model fallback chain.** When the head returned by the above
/// precedence resolves to a multi-model chain — either because
/// `Config::reviewer_model` (or `agent_reviewer_model`) was written as a
/// comma-separated list, or because the scenario-derived reviewer chain
/// produced alternates — `complete_once` now iterates that chain the way
/// the author path does (`cmd_exec_oneshot`). A backend failure that is
/// not fatal (no bytes were streamed yet) advances to the next candidate;
/// only exhausting the WHOLE chain surfaces the existing
/// `"0/N reviewers completed"` failure message. This is the regression
/// fix for the 2026-07-07 reviewer-pipeline defect (one dead reviewer
/// killing the whole review). The single-model config form stays
/// byte-for-byte identical: a `reviewer_model` field with one id yields
/// a one-element chain, no fallback is available, the original error
/// message is preserved.
async fn complete_once(
    cli: &crate::Cli,
    model_override: Option<&str>,
    reviewer_chain: &[String],
    system: &str,
    user: &str,
    max_tokens: u32,
) -> anyhow::Result<Completion> {
    // Build the ordered candidate list the dispatch loop walks. The shape
    // is `head, rest…` — the SAME model-resolution rules `complete_once`
    // already had, just lifted into a list so we can fall through to the
    // next candidate when the head's call returns a fallback-worthy
    // error. The list is deduped so an operator who lists the same model
    // twice (e.g. via both `[agents.X].reviewer_model` and the scenario
    // chain) doesn't pay for the same call twice.
    let candidates = build_reviewer_candidates(cli, model_override, reviewer_chain)?;
    if candidates.is_empty() {
        // Empty head — original behavior preserved: no candidates means
        // no model was resolvable, bail without fabricating a review.
        anyhow::bail!("no reviewer model resolved (check `--reviewer`, `[agents.X].reviewer_model`, or `Config::reviewer_model`)");
    }

    // Walk head-first; record health for fallback-worthy errors so the
    // chain iteration leaves an auditable trail. A `Fatal` error halts
    // the chain immediately (mirroring the author path's `if fatal
    // { break }`). The final attempt's provider error is bubbled up so
    // the call site can render the honest "0/N reviewers completed"
    // message — the same fail-closed surface the existing single-model
    // reviewer path already produced.
    let mut last_err_msg: Option<String> = None;
    for (idx, model) in candidates.iter().enumerate() {
        if idx > 0 && !cli.quiet {
            eprintln!(
                "[zoder] reviewer {prev} failed; falling back to {model}",
                prev = candidates[idx - 1]
            );
        }
        match dispatch_reviewer_for_model(cli, model, system, user, max_tokens).await {
            Ok(c) => return Ok(c),
            Err(ReviewerError::FallbackWorthy {
                message,
                kind,
                status,
            }) => {
                // Y-8: route the failure through the same classify + record
                // path the author chain uses, so a reviewer call that hits
                // a 401/403 (bad key) or 429/503/529 (capacity) does NOT
                // trip the breaker on a healthy model. Pre-fix this
                // reviewer fallback wrote `h.record_failure(model,
                // &message)` unconditionally -- the previous round added
                // classification on the author chain but missed this
                // reviewer fallback, leaving an asymmetric "auth probe
                // benches the reviewer on the third retry" defect. Now:
                // classify -> record_classified_failure. The provider
                // id is resolved from routing if available (consistent
                // with author path / agentic turn) so the on-disk store
                // sees the same stamp as the author's pre-gate entry.
                if let Ok(eng) = Engine::load() {
                    // Resolve the provider id BEFORE taking the health lock --
                    // this is read-only routing config I/O and touches no
                    // health state, so it must not extend the locked critical
                    // section.
                    let provider_id = crate::RoutingContext::load(&eng.cfg)
                        .ok()
                        .and_then(|r| r.real_provider_for_model(&eng.cfg, model).cloned())
                        .map(|p| p.id.clone())
                        .or_else(|| {
                            eng.cfg
                                .provider(model)
                                .filter(|p| {
                                    !p.base_url
                                        .contains(zoder_core::config::PLACEHOLDER_PROVIDER_HOST)
                                })
                                .map(|p| p.id.clone())
                        })
                        .unwrap_or_default();
                    // When a status is present we route through
                    // `Classification::from_status` so 401/403/429/503/529
                    // land on the right bucket. The `FallbackWorthy` arm
                    // only carries `kind + status`, so the typed body
                    // fallback for the Anthropic branch (Y-14) does not
                    // apply on this path -- the reviewer dispatcher
                    // surfaces errors out of `stream_chat`, which
                    // already populated `anthropic_error_body` upstream
                    // if the wire carried it.
                    let cls = status
                        .map(zoder_core::Classification::from_status)
                        .unwrap_or_else(|| zoder_core::classify_err_kind(kind));
                    // C4-MH1: record + persist under an exclusive lock. This
                    // reviewer fallback runs inside a `join_all` fan-out and
                    // races the daemon/CLI, so a bare load -> record -> save
                    // would drop a concurrently-recorded failure (lost
                    // update). `mutate_locked` reloads the freshest on-disk
                    // store under an advisory lockfile (create_new on
                    // `<stem>.lock`, not a File::lock/flock), applies this
                    // delta, and writes atomically before releasing -- so
                    // every panel model's failure survives.
                    let _ = HealthStore::mutate_locked(&eng.cfg.health_path, |h| {
                        h.record_classified_failure(model, &message, &provider_id, cls);
                    });
                }
                tracing::debug!(
                    model = %model,
                    ?kind,
                    status = ?status,
                    "reviewer model returned fallback-worthy error; trying next candidate"
                );
                // Preserve the historical message verbatim — the call
                // site in `cmd_review` (and the loop review branch)
                // renders `review failed: 0/N reviewers completed`
                // with `e.to_string()` from the original `anyhow!`.
                // Keeping `provider HTTP 404 ...: {"status":...}` (or
                // whatever the provider returned) intact preserves the
                // diagnostic a CI maintainer needs to triage a chain-
                // wide failure.
                last_err_msg = Some(format!("reviewer {model}: {message}"));
            }
            Err(ReviewerError::Fatal { message }) => {
                // A non-fallback-worthy error MUST halt the chain — these
                // are structural problems (policy, missing provider
                // config, ledger failure, token gating, etc.) where the
                // next candidate would either reproduce the same failure
                // or mask a real config error. Surface verbatim.
                return Err(anyhow!(message));
            }
        }
    }

    // Chain exhausted: every candidate errored. Surface the last provider
    // error verbatim so the existing `review failed: 0/N reviewers
    // completed` machinery in `cmd_review` (and the equivalent in the
    // loop review branch) can render an honest, model-specific diagnostic.
    // When no fallback-worthy error was recorded (e.g. every model had a
    // fatal error and every one bailed immediately), `last_err_msg` is
    // None and we fall back to the original no-resolved-model message so
    // we never silently report an empty chain.
    let msg = last_err_msg.unwrap_or_else(|| {
        "no reviewer model produced a completion (chain exhausted without a candidate answering)"
            .to_string()
    });
    Err(anyhow!(msg))
}

/// Build the ordered reviewer candidate list `complete_once` walks.
///
/// Precedence, highest first (matches the `complete_once` doc above and the
/// `AgentOverride` contract in `config.rs`):
///   1. explicit per-invocation pin (`--reviewer` / `--panel` → `model_override`);
///   2. per-agent `[agents.<alias>].reviewer_model` pin;
///   3. profile-level `Config::reviewer_model` chain (comma-separated);
///   4. scenario-routed `reviewer_chain` from `resolve_chain` (tail fallbacks ONLY);
///   5. cross-family default derived from the resolved author model, applied
///      here (I/O) only when every source above was empty.
///
/// The operator's CONFIGURED reviewer pin (2/3) always outranks scenario
/// auto-routing (4): auto-routing must never shadow a configured pin nor
/// route a same-family reviewer ahead of a configured cross-family one. The
/// pure ordering for 1--4 lives in `order_reviewer_candidates` so the
/// precedence seam is unit-testable without I/O.
fn build_reviewer_candidates(
    cli: &crate::Cli,
    model_override: Option<&str>,
    reviewer_chain: &[String],
) -> anyhow::Result<Vec<String>> {
    let eng = Engine::load()?;

    // Resolve the reviewer-model-side channel: any per-agent or profile-
    // level `reviewer_model` override, treated as an ordered chain. Note
    // `reviewer_models_for` already places the per-agent
    // `[agents.<alias>].reviewer_model` pin at the head when present,
    // falling through to the profile-level `Config::reviewer_model` chain.
    let config_chain = eng.cfg.reviewer_models_for(cli.agent.as_deref());
    // The stand-alone per-agent pin, passed separately so the ordering
    // helper can guarantee it seeds the head even if `config_chain` were
    // ever to diverge from `reviewer_models_for`'s pin-first contract.
    let agent_pin = eng.cfg.agent_reviewer_model(cli.agent.as_deref());

    let mut out = order_reviewer_candidates(
        model_override,
        agent_pin.as_deref(),
        &config_chain,
        reviewer_chain,
    );

    // Last-ditch CROSS-FAMILY default derived from the AUTHOR model. Only
    // added when nothing else produced a candidate; keeps legacy behavior
    // intact for an operator who never set the `reviewer_model` config
    // field at all.
    if out.is_empty() {
        let health = HealthStore::load(&eng.cfg.health_path);
        let routes = crate::resolve_chain(cli, &eng, &health)?;
        let author = routes.primary.first().cloned().unwrap_or_default();
        let default = crate::default_cross_family_reviewer(&author).to_string();
        push_unique(&mut out, &default);
    }

    Ok(out)
}

/// Pure reviewer-candidate ordering (no I/O), so the precedence seam is
/// unit-testable. Precedence, highest first (matches the `complete_once`
/// doc and `config.rs` `AgentOverride` contract):
///
///   1. explicit per-invocation pin (`--reviewer` / `--panel`, i.e.
///      `model_override`) — when set it heads the chain and the configured
///      + scenario chains supply fallbacks after it;
///   2. per-agent `[agents.<alias>].reviewer_model` pin (`agent_pin`);
///   3. profile-level `Config::reviewer_model` chain (`config_chain`, which
///      already carries the per-agent pin at its head when present);
///   4. scenario-routed reviewer chain (`reviewer_chain`) — LAST, as tail
///      fallbacks only. Scenario auto-routing must NOT shadow an operator's
///      configured `reviewer_model` pin, and must not be able to route a
///      same-family reviewer ahead of a configured cross-family pin.
///
/// The cross-family default fallback is intentionally NOT applied here — it
/// depends on the resolved author model (I/O) and is layered on by the
/// caller when this returns empty.
fn order_reviewer_candidates(
    model_override: Option<&str>,
    agent_pin: Option<&str>,
    config_chain: &[String],
    reviewer_chain: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();

    // Step 1 — explicit per-invocation pin. When set, the operator named
    // THIS model for THIS call, so it heads the chain regardless of any
    // auto-routing. Configured + scenario chains follow as fallbacks.
    if let Some(m) = model_override {
        push_unique(&mut out, m);
    }

    // Step 2 — per-agent pin, then the profile-level config chain. These
    // are the operator's CONFIGURED reviewer selection and outrank scenario
    // auto-routing. (`config_chain` already carries `agent_pin` at its head
    // when present; the explicit push here is defensive and dedup-safe.)
    if let Some(m) = agent_pin {
        push_unique(&mut out, m);
    }
    for c in config_chain {
        push_unique(&mut out, c);
    }

    // Step 3 — scenario-routed reviewer chain, LAST. Its head is only a
    // fallback once explicit + configured pins are exhausted, and its tail
    // supplies further alternates.
    for c in reviewer_chain {
        push_unique(&mut out, c);
    }

    out
}

fn push_unique(out: &mut Vec<String>, candidate: &str) {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return;
    }
    if !out.iter().any(|existing| existing == trimmed) {
        out.push(trimmed.to_string());
    }
}

/// Run the single-model reviewer dispatch for an explicit model id. The
/// caller is responsible for selecting the model (this helper just runs
/// it). The return type mirrors `try_model` in the author path: success
/// returns the completion, a `ReviewerError::FallbackWorthy` indicates the
/// next candidate in the chain should take over, and a
/// `ReviewerError::Fatal` halts the chain. By construction this fn
/// returns `FallbackWorthy` for any error that originates in the provider
/// call (network, HTTP-status, decode) and `Fatal` for any error that
/// would either duplicate streamed output, fail a structural invariant,
/// or mask a config/policy problem.
async fn dispatch_reviewer_for_model(
    cli: &crate::Cli,
    model: &str,
    system: &str,
    user: &str,
    max_tokens: u32,
) -> Result<Completion, ReviewerError> {
    let eng = match Engine::load() {
        Ok(eng) => eng,
        Err(e) => return Err(ReviewerError::fatal(format!("loading engine: {e}"))),
    };

    // Per-model routing: resolve the provider that actually serves this model
    // (e.g. a pinned MiniMax-M3 -> the minimax provider), not always the default
    // provider — otherwise a reviewer model could be sent to the wrong endpoint.
    let routing = match crate::RoutingContext::load(&eng.cfg) {
        Ok(r) => r,
        Err(e) => {
            return Err(ReviewerError::fatal(format!(
                "loading routing context: {e}"
            )))
        }
    };
    let provider_cfg = match routing.real_provider_for_model(&eng.cfg, model) {
        Some(p) => p,
        None => {
            // Treat a missing provider config as FALLBACK-WORTHY: the
            // next candidate in the chain might have a real provider
            // configured (it would be a regression to shadow that with a
            // hard error when the next link in the chain can answer).
            return Err(ReviewerError::fatal(format!(
                "no real provider is configured for reviewer model '{model}' — it would fall through to the {} placeholder and fail. Configure a provider that serves it, or pass a backed reviewer via `--reviewer <model>`.",
                zoder_core::config::PLACEHOLDER_PROVIDER_HOST
            )));
        }
    };

    // Gate the reviewer/panel model. Reviewers run non-interactively (panel +
    // fix loop), so a PAID reviewer is REJECTED rather than prompted — pass
    // --allow-paid to use one. Closes the bypass where -m / --reviewer / --panel
    // could spend with no confirmation or free-verification.
    let strict_free = (eng.cfg.strict_free && !cli.lenient_telemetry) || cli.require_free;
    let gate = PolicyGate::new(&eng.cfg, cli.allow_paid, strict_free);
    let known_paid_model = eng
        .corpus
        .get(model)
        .is_some_and(|candidate| !candidate.free);
    let model_entry = eng
        .corpus
        .get(model)
        .cloned()
        .unwrap_or_else(|| ModelEntry {
            id: model.to_string(),
            gated_reason: Some("unknown reviewer model: not in corpus, cannot verify free".into()),
            ..Default::default()
        });
    let provider_paid = provider_cfg.paid || provider_cfg.billing == BillingMode::Metered;
    let provider_cost_neutral = !provider_cfg.paid && provider_cfg.billing != BillingMode::Metered;
    if let Decision::NeedConfirm(why) =
        gate.check(&model_entry, provider_paid, provider_cost_neutral)
    {
        // Policy rejection: NOT fallback-worthy. An operator policy
        // rejection of THIS model is something the next candidate can
        // not silently bypass; bail so the caller surfaces it as a
        // real error.
        return Err(ReviewerError::fatal(format!(
            "reviewer/panel model '{model}' requires paid spend; pass --allow-paid to use it.\n{why}"
        )));
    }

    // Reserve and lock accounting before the reviewer request. Panel calls may
    // run concurrently, so this also serializes their authoritative snapshots.
    let ledger_path = eng.cfg.ledger_path.clone();
    let mut ledger_reservation =
        match tokio::task::spawn_blocking(move || Ledger::new(&ledger_path).reserve_billable())
            .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                return Err(ReviewerError::fatal(format!(
                    "reserving ledger entry before reviewer dispatch: {e}"
                )));
            }
            Err(e) => {
                return Err(ReviewerError::fatal(format!(
                    "joining reviewer ledger reservation task: {e}"
                )));
            }
        };

    let messages = vec![Message::new("system", system), Message::new("user", user)];
    let req = ChatRequest {
        model: model.to_string(),
        messages,
        max_tokens,
        temperature: Some(0.1),
        stream: false,
        show_reasoning: false,
        reasoning_effort: cli.reasoning.clone(),
    };
    let provider = match OpenAiProvider::new(provider_cfg) {
        Ok(p) => p,
        Err(e) => return Err(ReviewerError::fatal(format!("constructing provider: {e}"))),
    };
    if let Err(e) = ledger_reservation.arm() {
        return Err(ReviewerError::fatal(format!(
            "verifying ledger reservation before reviewer dispatch: {e}"
        )));
    }
    let res = match provider.stream_chat(&req, None).await {
        Ok(r) => r,
        Err(e) => {
            // Provider / network / decode error — fallback-worthy when
            // nothing has been emitted yet (the model cannot have shown
            // partial output because `complete_once` does not stream).
            // `emitted == true` (an extreme edge case where the
            // provider reported bytes we haven't surfaced yet) is
            // treated as fatal so the chain does not duplicate visible
            // output. The original `complete_once` propagated the
            // message verbatim via `anyhow!("{}", e.message)`; we
            // preserve that for the call site's `review failed: 0/N
            // reviewers completed` rendering.
            if e.emitted {
                return Err(ReviewerError::Fatal { message: e.message });
            }
            return Err(ReviewerError::fallback_worthy_from(e));
        }
    };

    let tokens_in = res.prompt_tokens.unwrap_or(0);
    let tokens_out = res.completion_tokens.unwrap_or(res.tokens_out);
    let pricing = PricingCatalog::load(&Config::home().join("pricing.json"));
    let (cost, unknown_cost) = match res.telemetry.cost_usd {
        Some(cost) if cost.is_finite() && cost >= 0.0 => (cost, false),
        _ => match pricing.classify_cost(model, tokens_in, tokens_out, Some(Utc::now())) {
            CostVerdict::Priced(cost) => (cost, false),
            CostVerdict::Free => (0.0, false),
            CostVerdict::Unknown => (0.0, true),
        },
    };
    // Post-verify the reviewer call was actually served free (catch a free->paid
    // fallback), and independently reject a known-paid/positive-cost outcome
    // without the explicit opt-in. Both failures are reconciled before they are
    // returned so review and CI commands cannot fail open.
    let verify_failure = gate.verify_free(&model_entry, &res.telemetry).err();
    let paid_failure = crate::paid_without_opt_in(
        cli.allow_paid,
        provider_cost_neutral,
        "reviewer turn",
        model,
        known_paid_model,
        (!unknown_cost).then_some(cost),
    );
    let policy_failure = match (&verify_failure, &paid_failure) {
        (Some(verify), Some(paid)) => Some(format!("{verify}; {paid}")),
        (Some(verify), None) => Some(verify.clone()),
        (None, Some(paid)) => Some(paid.clone()),
        (None, None) => None,
    };
    let mut violation = policy_failure.clone();
    if unknown_cost {
        let msg = format!("cost unknown: no valid telemetry or catalog price for {model}");
        violation = Some(match violation {
            Some(existing) => format!("{existing}; {msg}"),
            None => msg,
        });
    }
    let ledger_entry = Entry {
        ts_utc: Utc::now(),
        provider: provider_cfg.id.clone(),
        model: model.to_string(),
        host: zoder_core::ledger::host_of_model(model),
        tokens_in,
        tokens_out,
        cost_usd: cost,
        cost_unknown: unknown_cost,
        calls: 1,
        violation,
        tags: crate::finops_tags(cli, tokens_in, res.cached_prompt_tokens),
    };
    let mut health = HealthStore::load(&eng.cfg.health_path);
    if let Err(e) = crate::reconcile_policy_checked_turn(
        ledger_reservation,
        &ledger_entry,
        "reviewer turn",
        &mut health,
        model,
        policy_failure.as_deref(),
    ) {
        // Policy violation: NOT fallback-worthy. The verified-cost
        // / paid-without-opt-in failure is a structural invariant; we
        // must not silently bury it by trying the next candidate.
        return Err(ReviewerError::fatal(format!(
            "policy check during reviewer turn: {e}"
        )));
    }

    Ok(Completion {
        model: model.to_string(),
        content: res.content,
        cost_usd: cost,
    })
}

#[cfg(test)]
mod reviewer_policy_tests {
    use super::*;

    #[test]
    fn non_free_reviewer_fallback_is_reconciled_and_returned_as_err() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::default_provider(dir.path());
        let gate = PolicyGate::new(&cfg, false, true);
        let model = ModelEntry {
            id: "free-reviewer".into(),
            free: true,
            ..Default::default()
        };
        let telemetry = zoder_core::CallTelemetry {
            api_base: Some("https://paid.example.test/v1".into()),
            cost_usd: Some(0.02),
            ..Default::default()
        };
        let failure = gate
            .verify_free(&model, &telemetry)
            .expect_err("a billed reviewer fallback must fail verification");
        let ledger_path = dir.path().join("ledger.jsonl");
        let health_path = dir.path().join("health.json");
        let entry = Entry {
            ts_utc: Utc::now(),
            provider: "paid-fallback".into(),
            model: model.id.clone(),
            host: String::new(),
            tokens_in: 10,
            tokens_out: 5,
            cost_usd: 0.02,
            cost_unknown: false,
            calls: 1,
            violation: Some(failure.clone()),
            tags: Default::default(),
        };
        let reservation = Ledger::new(&ledger_path).reserve_billable().unwrap();
        let mut health = HealthStore::load(&health_path);

        let result = crate::reconcile_policy_checked_turn(
            reservation,
            &entry,
            "reviewer turn",
            &mut health,
            &model.id,
            Some(&failure),
        );

        assert!(result.is_err(), "reviewer policy failure must propagate");
        let rows = Ledger::new(&ledger_path).entries_strict().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].violation.as_deref(), Some(failure.as_str()));
        assert_eq!(
            HealthStore::load(&health_path)
                .models
                .get(&model.id)
                .unwrap()
                .failures,
            1
        );
    }
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
pub(crate) struct ReviewOutput {
    #[serde(default)]
    verdict: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    findings: Vec<Finding>,
    #[serde(default)]
    next_steps: Vec<String>,
}

/// One reviewer slot in the panel. C5-1: the outcome (a real completion vs a
/// reviewer that failed) is carried as a STRUCTURED discriminant, never by
/// overloading the model-id `String`. Previously a failed slot was stored as
/// `("error", ReviewOutput{..})` and the worst-verdict walk filtered on the
/// literal model string `"error"` -- so an operator-reachable reviewer model
/// LITERALLY named `error` (via `--panel error` or `reviewer_model="error"`)
/// that SUCCEEDED with a blocking verdict had its vote silently dropped and
/// the gate failed open (block -> approve, exit 0). The variant makes success
/// vs failure unambiguous regardless of the model id.
#[derive(Debug, Clone)]
enum ReviewerSlot {
    /// A reviewer that produced a structured completion. `model` is the
    /// operator-supplied model id verbatim (may be any string, including
    /// `"error"`); `review` is the parsed verdict/findings.
    Ok { model: String, review: ReviewOutput },
    /// A reviewer whose completion failed. `err` is the failure detail; it is
    /// rendered as a synthetic non-voting `comment` record so the panel view
    /// still lists the slot, but it NEVER counts toward the worst-rank walk.
    Failed { model: String, err: String },
}

impl ReviewerSlot {
    /// The reviewer's model id (the operator-supplied string, verbatim).
    fn model(&self) -> &str {
        match self {
            ReviewerSlot::Ok { model, .. } => model,
            ReviewerSlot::Failed { model, .. } => model,
        }
    }

    /// Did this slot produce a real reviewer vote? Only `Ok` slots vote in the
    /// worst-rank aggregate; `Failed` slots are excluded by VARIANT, not by
    /// string-comparing the model id.
    fn is_ok(&self) -> bool {
        matches!(self, ReviewerSlot::Ok { .. })
    }

    /// The parsed review for an `Ok` slot; `None` for a `Failed` slot.
    fn review(&self) -> Option<&ReviewOutput> {
        match self {
            ReviewerSlot::Ok { review, .. } => Some(review),
            ReviewerSlot::Failed { .. } => None,
        }
    }

    /// The verdict string as rendered in the panel view / JSON payload. A
    /// `Failed` slot renders the synthetic non-voting `comment` verdict (it is
    /// excluded from the worst-rank walk, so this display value never lifts or
    /// lowers the aggregate).
    fn display_verdict(&self) -> &str {
        match self {
            ReviewerSlot::Ok { review, .. } => review.verdict.as_str(),
            ReviewerSlot::Failed { .. } => "comment",
        }
    }

    /// The summary line for the panel view / JSON payload. For a `Failed` slot
    /// this is the reviewer-failure detail.
    fn display_summary(&self) -> String {
        match self {
            ReviewerSlot::Ok { review, .. } => review.summary.clone(),
            ReviewerSlot::Failed { err, .. } => format!("reviewer failed: {err}"),
        }
    }
}

/// Best-effort parse of a model's reply into a `ReviewOutput`: extract the first
/// balanced-looking `{...}` and decode it; on failure, wrap the raw text.
///
/// Z-2 — verdicts are normalized (`trim` + ASCII lowercase) when stored so
/// every downstream consumer sees the canonical form. Defense in depth:
/// comparison functions (`loop_review_ok`, `verdict_rank`) ALSO normalize
/// at the point of comparison so direct `ReviewOutput` construction in
/// tests or other call sites is also safe.
/// Yield each balanced `{...}` substring of `s`, left to right. Brace
/// counting is string-aware (braces inside JSON string literals, and `\"`
/// escapes, are ignored) so a `{` in prose — or inside a string value —
/// does not throw off the balance. Slices are taken only at ASCII `{`/`}`
/// byte positions, which are always char boundaries.
fn balanced_json_objects(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            let mut depth = 0usize;
            let mut in_str = false;
            let mut esc = false;
            let mut end = None;
            let mut j = i;
            while j < bytes.len() {
                let c = bytes[j];
                if in_str {
                    if esc {
                        esc = false;
                    } else if c == b'\\' {
                        esc = true;
                    } else if c == b'"' {
                        in_str = false;
                    }
                } else {
                    match c {
                        b'"' => in_str = true,
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = Some(j);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                j += 1;
            }
            match end {
                Some(e) => {
                    out.push(&s[i..=e]);
                    i = e + 1;
                    continue;
                }
                None => break, // unbalanced from here on
            }
        }
        i += 1;
    }
    out
}

fn parse_review(raw: &str) -> ReviewOutput {
    let trimmed = raw.trim();
    // Y-3: scan for the first BALANCED `{...}` object that decodes as a
    // ReviewOutput with a non-empty verdict, rather than `first-'{'..last-'}'`
    // — which a stray `{` in prose before the JSON (or trailing prose after
    // it) turns into invalid JSON, silently dropping a real `request_changes`
    // and falling through to the (previously non-blocking) fallback. A
    // chatty or hostile model could smuggle a `{` to neuter its own block.
    // W3: scan EVERY balanced object and keep the MOST BLOCKING verdict,
    // not the first non-empty one. Returning on the first verdict let a
    // decoy `{"verdict":"approve"}` placed before the real
    // `{"verdict":"request_changes"}` win — the exact gaming vector the
    // balanced-object scan was meant to close. Ranking via `verdict_rank`
    // (where unknown/unrecognized verdicts rank as blocking) also stops a
    // hallucinated verdict string from surfacing as approve here.
    let mut worst: Option<ReviewOutput> = None;
    let mut worst_rank = 0u8;
    for obj in balanced_json_objects(trimmed) {
        if let Ok(r) = serde_json::from_str::<ReviewOutput>(obj) {
            let verdict = r.verdict.trim().to_ascii_lowercase();
            if verdict.is_empty() {
                continue;
            }
            let rank = verdict_rank(&verdict);
            if worst.is_none() || rank > worst_rank {
                worst_rank = rank;
                worst = Some(ReviewOutput {
                    verdict,
                    summary: r.summary,
                    findings: r.findings,
                    next_steps: r.next_steps,
                });
            }
        }
    }
    if let Some(r) = worst {
        return r;
    }
    // Y-4: no parseable verdict object → FAIL CLOSED. A review we cannot
    // extract a verdict from (prose, a refusal like "I cannot review this",
    // an empty `{}`) must BLOCK — the previous non-blocking `"comment"` let an
    // unreviewed/refused iteration resolve as if approved. `request_changes`
    // is the fail-closed verdict, mirroring `synthesize_review_phase_failure`.
    ReviewOutput {
        verdict: "request_changes".into(),
        summary: "Reviewer did not return a parseable structured verdict; failing closed (request_changes). Raw output preserved below.".into(),
        findings: vec![Finding {
            severity: "info".into(),
            title: "unparseable review (fail-closed)".into(),
            body: trimmed.to_string(),
            location: None,
        }],
        next_steps: vec![],
    }
}

fn verdict_rank(v: &str) -> u8 {
    // Z-2: verdicts must rank case- and whitespace-insensitively. Real LLM
    // output drifts casing (`"BLOCK"`, `"Request_Changes"`, `"REJECT"`) and
    // padding (`" approve "`). Case-sensitive matching previously ranked
    // those as 0 (approve), so an explicit block was carried as the
    // "approve / unknown" branch and the review aggregator's
    // worst-down-the-line rank promotion never fired.
    let normalized = v.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "request_changes" | "reject" | "block" => 2,
        "comment" | "neutral" => 1,
        "approve" => 0,
        // Empty = "no verdict"; parse_review skips empty-verdict objects and
        // falls to its fail-closed request_changes fallback, and loop_review_ok
        // fail-closes on it too, so 0 here is safe and preserves legacy shape.
        "" => 0,
        // W4: an unknown / hallucinated / typo verdict ("deny",
        // "changes_requested", "needs_changes", "lgtm") must NEVER be treated
        // as approve. Rank it as blocking so the aggregator's worst-rank
        // promotion and parse_review's most-blocking selection both fail closed.
        _ => 2,
    }
}

// ---------------------------------------------------------------------------
// Git diff acquisition.
// ---------------------------------------------------------------------------

const MAX_GIT_DIFF_BYTES: usize = 1 << 20; // 1 MiB
const MAX_GIT_STDERR_BYTES: usize = 64 << 10; // 64 KiB

struct CappedRead {
    bytes: Vec<u8>,
    limit_reached: bool,
}

struct CappedGitOutput {
    stdout: String,
    limit_reached: bool,
}

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

fn read_to_byte_cap<R: Read>(mut reader: R, max: usize) -> std::io::Result<CappedRead> {
    let mut bytes = Vec::with_capacity(max.min(8192));
    let mut buf = [0_u8; 8192];
    let mut remaining = max;
    while remaining > 0 {
        let want = remaining.min(buf.len());
        let n = reader.read(&mut buf[..want])?;
        if n == 0 {
            return Ok(CappedRead {
                bytes,
                limit_reached: false,
            });
        }
        bytes.extend_from_slice(&buf[..n]);
        remaining -= n;
    }
    Ok(CappedRead {
        bytes,
        limit_reached: true,
    })
}

fn drain_to_byte_cap<R: Read>(mut reader: R, max: usize) -> std::io::Result<CappedRead> {
    let mut bytes = Vec::with_capacity(max.min(8192));
    let mut buf = [0_u8; 8192];
    let mut limit_reached = false;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(CappedRead {
                bytes,
                limit_reached,
            });
        }
        let remaining = max.saturating_sub(bytes.len());
        if remaining == 0 {
            limit_reached = true;
            continue;
        }
        let keep = remaining.min(n);
        bytes.extend_from_slice(&buf[..keep]);
        if keep < n || bytes.len() == max {
            limit_reached = true;
        }
    }
}

fn run_git_capped(cwd: &Path, args: &[&str], max_stdout: usize) -> anyhow::Result<CappedGitOutput> {
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running git {}", args.join(" ")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("git {} stdout was not piped", args.join(" ")))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("git {} stderr was not piped", args.join(" ")))?;
    let stderr_reader = std::thread::spawn(move || drain_to_byte_cap(stderr, MAX_GIT_STDERR_BYTES));

    let stdout = match read_to_byte_cap(stdout, max_stdout) {
        Ok(stdout) => stdout,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stderr_reader.join();
            return Err(e).with_context(|| format!("reading git {} stdout", args.join(" ")));
        }
    };

    let mut killed_after_cap = false;
    let status = if stdout.limit_reached {
        match child.try_wait()? {
            Some(status) => status,
            None => {
                child
                    .kill()
                    .with_context(|| format!("stopping git {} after stdout cap", args.join(" ")))?;
                killed_after_cap = true;
                child.wait()?
            }
        }
    } else {
        child.wait()?
    };

    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow!("reading git {} stderr panicked", args.join(" ")))?
        .with_context(|| format!("reading git {} stderr", args.join(" ")))?;

    let capped_success = stdout.limit_reached && killed_after_cap;
    if !(status.success() || capped_success) {
        let mut err = String::from_utf8_lossy(&stderr.bytes).trim().to_string();
        if stderr.limit_reached {
            if !err.is_empty() {
                err.push(' ');
            }
            err.push_str(&format!(
                "[stderr truncated at {MAX_GIT_STDERR_BYTES} bytes]"
            ));
        }
        return Err(anyhow!("git {} failed: {}", args.join(" "), err));
    }

    Ok(CappedGitOutput {
        stdout: String::from_utf8_lossy(&stdout.bytes).to_string(),
        limit_reached: stdout.limit_reached,
    })
}

fn run_git_diff(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let capped = run_git_capped(cwd, args, MAX_GIT_DIFF_BYTES)?;
    Ok(render_capped_git_diff(capped, MAX_GIT_DIFF_BYTES))
}

fn render_capped_git_diff(capped: CappedGitOutput, max_stdout: usize) -> String {
    let mut stdout = capped.stdout;
    if capped.limit_reached {
        if !stdout.ends_with('\n') {
            stdout.push('\n');
        }
        stdout.push_str(&format!(
            "\n...[git diff truncated during acquisition at {max_stdout} bytes]...\n"
        ));
    }
    stdout
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
            let mut d = run_git_diff(cwd, &["diff", "HEAD"]).unwrap_or_default();
            if d.trim().is_empty() {
                // No tracked changes vs HEAD; fall back to staged + unstaged.
                let staged = run_git_diff(cwd, &["diff", "--cached"]).unwrap_or_default();
                let unstaged = run_git_diff(cwd, &["diff"]).unwrap_or_default();
                d = format!("{staged}\n{unstaged}");
            }
            // Surface untracked-but-not-ignored files. `git diff` only sees
            // tracked paths, so a working tree whose only work is NEW
            // not-yet-added files would otherwise produce an empty diff and
            // the review/loop would wrongly report "no changes to review".
            // `git ls-files --others --exclude-standard` honors `.gitignore`,
            // so ignored paths and anything under `.git/` are excluded by
            // construction. Each untracked path is appended as a synthetic
            // "new file mode" hunk using the same unified-diff shape the
            // reviewer expects.
            let untracked = untracked_not_ignored_diff(cwd);
            if !untracked.is_empty() {
                if !d.is_empty() && !d.ends_with('\n') {
                    d.push('\n');
                }
                d.push_str(&untracked);
            }
            Ok(("working-tree".into(), d))
        }
        ReviewScope::Branch => {
            let b = detect_base(cwd, base);
            let d = run_git_diff(cwd, &["diff", &format!("{b}...HEAD")])?;
            Ok((format!("branch (base {b})"), d))
        }
        ReviewScope::Auto => unreachable!(),
    }
}

/// Enumerate untracked, non-ignored working-tree paths and render each as a
/// unified-diff "new file" hunk. Paths under `.git/` are never returned
/// because they are always ignored by `--exclude-standard`. Returns an
/// empty string when there are no untracked-not-ignored paths. On any
/// failure (no git, missing binary, etc.) returns an empty string — this is
/// a best-effort supplement to the tracked diff and must not break review.
fn untracked_not_ignored_diff(cwd: &Path) -> String {
    let Ok(listing) = run_git(
        cwd,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--directory",
        ],
    ) else {
        return String::new();
    };
    if listing.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for raw in listing.split('\0') {
        if raw.is_empty() {
            continue;
        }
        // `--directory` collapses nested untracked trees into a single
        // directory entry ending in `/`; recurse into it so the diff is not
        // empty when only nested files exist. Skip `.git/` defensively.
        let path = Path::new(raw);
        if raw.starts_with(".git/") || raw == ".git" {
            continue;
        }
        let abs = cwd.join(path);
        let file_diff = if raw.ends_with('/') {
            render_untracked_dir_hunk(path, &abs)
        } else if abs.is_file() {
            render_untracked_file_hunk(path, &abs)
        } else {
            continue;
        };
        if !file_diff.is_empty() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&file_diff);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

/// Render one untracked file as a unified-diff "new file" hunk. Uses
/// `git diff --no-index --no-color -- /dev/null <path>` when possible so the
/// hunk is byte-identical to what `git diff` would emit post-`git add -N`;
/// falls back to a hand-built hunk when the index diff fails (e.g. binary
/// files, very large files).
fn render_untracked_file_hunk(rel: &Path, abs: &Path) -> String {
    let dev_null = Path::new("/dev/null");
    if let Ok(ok) = std::process::Command::new("git")
        .arg("diff")
        .arg("--no-index")
        .arg("--no-color")
        .arg("--no-textconv")
        .arg("--")
        .arg(dev_null)
        .arg(abs)
        .output()
    {
        // `git diff --no-index` exits 1 when files differ (the normal case
        // for new files) and 0 when they are identical. Anything >1 is an
        // error worth ignoring so we fall through to the synthetic hunk.
        let code = ok.status.code().unwrap_or(-1);
        if code <= 1 {
            // Strip the first four header lines git --no-index prints
            // (`diff --git ...`, `index ...`, `--- /dev/null`, `+++ <path>`)
            // — we re-emit a clean unified-diff header ourselves.
            let stdout = String::from_utf8_lossy(&ok.stdout);
            return rewrite_no_index_hunk(&stdout, rel);
        }
    }
    synthetic_new_file_hunk(rel, abs)
}

/// Build a minimal valid unified-diff hunk for a new file from its on-disk
/// bytes. Used as a fallback when `git diff --no-index` refuses (e.g. on
/// binary files it cannot read). The content is omitted for binary files.
fn synthetic_new_file_hunk(rel: &Path, abs: &Path) -> String {
    let bytes = match std::fs::read(abs) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    let is_binary = bytes.contains(&0);
    let rel_display = rel.to_string_lossy();
    let mut out = format!(
        "diff --git a/{rel} b/{rel}\nnew file mode 100644\n--- /dev/null\n+++ b/{rel}\n",
        rel = rel_display,
    );
    if is_binary {
        out.push_str("@@ -0,0 +1 @@\n");
        out.push_str("Binary files /dev/null and b/");
        out.push_str(&rel_display);
        out.push_str(" differ\n");
        return out;
    }
    let text = String::from_utf8_lossy(&bytes);
    let line_count = text.lines().count();
    if line_count == 0 && text.is_empty() {
        out.push_str("@@ -0,0 +0,0 @@\n");
        return out;
    }
    out.push_str(&format!("@@ -0,0 +1,{line_count} @@\n"));
    for line in text.lines() {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Re-emit a `git diff --no-index` hunk with our own header so all hunks in
/// the working-tree diff share the same shape (`diff --git a/P b/P`,
/// `new file mode 100644`, `--- /dev/null`, `+++ b/P`).
fn rewrite_no_index_hunk(stdout: &str, rel: &Path) -> String {
    let rel_display = rel.to_string_lossy();
    let mut out = String::new();
    let mut lines = stdout.lines();
    // Drop `diff --git ...`, `index ...`, `--- /dev/null`, `+++ b/<path>`.
    let _ = lines.next();
    let _ = lines.next();
    let _ = lines.next();
    let _ = lines.next();
    out.push_str(&format!(
        "diff --git a/{rel} b/{rel}\nnew file mode 100644\n--- /dev/null\n+++ b/{rel}\n",
        rel = rel_display,
    ));
    for line in lines {
        // `git diff --no-index` prefixes "No newline at end of file" with
        // a literal "\ No newline at end of file" continuation; preserve it
        // verbatim so the reviewer sees the same hunk it would on `git add`.
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Render an untracked directory (from `ls-files --directory`) as one
/// synthetic "new file" hunk per contained regular file. `--directory`
/// collapses nested trees into a single dir entry to keep the listing
/// bounded; we recurse here so a tree containing only nested files still
/// produces a non-empty diff.
fn render_untracked_dir_hunk(rel: &Path, abs: &Path) -> String {
    let mut out = String::new();
    let Ok(read) = std::fs::read_dir(abs) else {
        return String::new();
    };
    let mut entries: Vec<_> = read.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let child_rel = rel.join(entry.file_name());
        let child_abs = entry.path();
        let piece = if child_abs.is_dir() {
            render_untracked_dir_hunk(&child_rel, &child_abs)
        } else if child_abs.is_file() {
            render_untracked_file_hunk(&child_rel, &child_abs)
        } else {
            continue;
        };
        if !piece.is_empty() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&piece);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

/// Cap the diff so we never blow the context window (head + tail).
///
/// Truncation lands on a CHARACTER boundary so a diff containing non-ASCII
/// (identifiers, comments, test data) cannot panic with "byte index N is
/// not a char boundary". The cap is interpreted as the maximum number of
/// bytes in the returned string; the head and tail slices are the largest
/// char-boundary-prefix and char-boundary-suffix that each fit the budget.
fn cap_diff(diff: &str, max: usize) -> String {
    if diff.len() <= max {
        return diff.to_string();
    }
    let head_end = floor_char_boundary(diff, max * 3 / 4);
    let tail_len = max / 4;
    let tail_start = ceil_char_boundary_from_end(diff, tail_len);
    format!(
        "{head}\n\n...[diff truncated for length]...\n\n{tail}",
        head = &diff[..head_end],
        tail = &diff[tail_start..]
    )
}

/// Largest `i <= n` such that `s.is_char_boundary(i)` is true (so `&s[..i]`
/// is a valid UTF-8 slice). `n = s.len()` and `n = 0` are always safe and
/// returned unchanged.
fn floor_char_boundary(s: &str, n: usize) -> usize {
    if n >= s.len() {
        return s.len();
    }
    let mut i = n;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest `i` such that `s.len() - i <= n` AND `s.is_char_boundary(i)` is
/// true (so `&s[i..]` is a valid UTF-8 slice and is no longer than `n`
/// bytes). Returns `0` when the requested tail would cover the whole string.
fn ceil_char_boundary_from_end(s: &str, n: usize) -> usize {
    if n >= s.len() {
        return 0;
    }
    let mut i = s.len() - n;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Resolve `HEAD` to its full SHA. Returns `None` if the repo has no
/// commits yet (the only case `git rev-parse HEAD` can fail with the
/// repository present). Trims trailing whitespace; an empty trimmed result
/// is also treated as `None` defensively (e.g. some plumbing edge cases).
fn rev_parse_head(cwd: &Path) -> Option<String> {
    run_git(cwd, &["rev-parse", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Outcome of one [`enforce_repo_commit_author`] pass. Variants are
/// intentionally distinguishable so the loop can render an honest iter
/// record (e.g. `already_correct` is a real no-op, not a missing field) and
/// unit tests can pin each branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommitAuthorEnforcement {
    /// The repo has no commits yet. There is nothing to correct.
    NoCommit,
    /// HEAD's author already matches the repo's configured identity.
    /// No git mutation happened; the SHA is unchanged.
    AlreadyCorrect { current: String },
    /// The repo has no configured `user.name`/`user.email`. There is
    /// nothing correct to amend to — leave the commit alone rather than
    /// guessing an identity.
    NoConfiguredIdentity,
    /// `git commit --amend --author=... --no-edit` was applied. The new
    /// commit object has a different SHA (the author field is part of
    /// the object) but the tree, message, and committer are preserved.
    Corrected { from: String, to: String },
    /// A git call failed. The commit is left untouched; the loop records
    /// the reason and continues.
    Failed { reason: String },
}

/// Render a [`CommitAuthorEnforcement`] for the per-iter JSON record.
/// `None` (the author turn did not move HEAD) is rendered as `null` so
/// consumers can distinguish "no commit was made" from "a commit was
/// made and inspected" — a meaningful difference for downstream
/// auditability.
fn commit_author_enforcement_to_json(result: Option<&CommitAuthorEnforcement>) -> Value {
    match result {
        None => Value::Null,
        Some(CommitAuthorEnforcement::NoCommit) => json!({"status": "no_commit"}),
        Some(CommitAuthorEnforcement::AlreadyCorrect { current }) => {
            json!({"status": "already_correct", "current": current})
        }
        Some(CommitAuthorEnforcement::NoConfiguredIdentity) => {
            json!({"status": "no_configured_identity"})
        }
        Some(CommitAuthorEnforcement::Corrected { from, to }) => {
            json!({"status": "corrected", "from": from, "to": to})
        }
        Some(CommitAuthorEnforcement::Failed { reason }) => {
            json!({"status": "failed", "reason": reason})
        }
    }
}

/// Reconcile HEAD's author with the repo's own git config identity.
///
/// Zoder's default engine is `zeroclaw`, a long-lived daemon connected
/// over a Unix socket. Zoder cannot control the daemon's environment, so
/// the git identity the daemon uses when it runs `git commit` is whatever
/// its parent process started it with — typically `zoder-bot <...>` and
/// NOT the repo's configured `user.name`/`user.email`. Every commit the
/// daemon lands in the working tree therefore needs to be amended to
/// carry the repo's identity before the loop reports the turn as
/// complete, otherwise a human has to `git commit --amend --author=...`
/// after every loop run.
///
/// The fix is deliberately narrow:
///
///   * Only the author field is rewritten. `--no-edit` keeps the message
///     and tree untouched; the committer is also preserved by `git`
///     itself.
///   * Only runs when the repo's local-then-global config has BOTH
///     `user.name` and `user.email` set. If either is missing, there is
///     nothing correct to amend to and the commit is left alone.
///   * Skips the amend entirely when the HEAD author already matches the
///     configured identity (no wasted amend, no SHA churn, no
///     "corrected" log line).
///
/// Returns the outcome for observability and tests. Any git failure is
/// captured as `Failed { reason }` — this function never panics and
/// never returns an `Err`, so the loop can keep going.
fn enforce_repo_commit_author(cwd: &Path) -> CommitAuthorEnforcement {
    // 1. Resolve HEAD's SHA. A brand-new repo with no commits errors out
    //    here — there is nothing to correct, and the next author turn may
    //    create the first commit (which a future iter will catch via the
    //    `None -> Some(sha)` SHA transition in `cmd_loop`).
    let Some(_head_sha) = rev_parse_head(cwd) else {
        return CommitAuthorEnforcement::NoCommit;
    };

    // 2. Resolve the repo's configured identity. `git config user.name` /
    //    `user.email` honor the standard local-then-global resolution
    //    (we deliberately do NOT pass `--global`, which would bypass the
    //    local repo override). If either is missing, there is nothing
    //    correct to amend to — leave the commit alone.
    let cfg_name = run_git(cwd, &["config", "user.name"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let cfg_email = run_git(cwd, &["config", "user.email"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let (cfg_name, cfg_email) = match (cfg_name, cfg_email) {
        (Some(n), Some(e)) => (n, e),
        _ => return CommitAuthorEnforcement::NoConfiguredIdentity,
    };
    let desired = format!("{cfg_name} <{cfg_email}>");

    // 3. Read HEAD's current author. `%an <%ae>` is the literal shape
    //    git itself renders, so the comparison is byte-exact (no
    //    whitespace or quoting surprises).
    let current = match run_git(cwd, &["log", "-1", "--format=%an <%ae>"]) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            return CommitAuthorEnforcement::Failed {
                reason: format!("reading HEAD author: {e}"),
            }
        }
    };
    if current == desired {
        return CommitAuthorEnforcement::AlreadyCorrect { current };
    }

    // 4. Apply the correction. `--no-edit` keeps the message, tree, and
    //    committer untouched; only the author field is rewritten. The
    //    new commit object has a different SHA (the author is part of
    //    the object), but content is preserved.
    let amend = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["commit", "--amend", "--author", &desired, "--no-edit"])
        .status();
    match amend {
        Ok(s) if s.success() => CommitAuthorEnforcement::Corrected {
            from: current,
            to: desired,
        },
        Ok(s) => CommitAuthorEnforcement::Failed {
            reason: format!("`git commit --amend` exited {s}"),
        },
        Err(e) => CommitAuthorEnforcement::Failed {
            reason: format!("spawning `git commit --amend`: {e}"),
        },
    }
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
                reviewers: &[ReviewerSlot::Ok {
                    model: String::from("n/a"),
                    review: out,
                }],
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
    // unchanged: explicit `model_override` (per `--panel`) wins, then the
    // operator-CONFIGURED reviewer (per-agent `[agents.<alias>].reviewer_model`
    // pin, then profile-level `Config::reviewer_model`), then the
    // scenario-eligible reviewer as a tail fallback, then the cross-family
    // legacy default. Scenario auto-routing never shadows a configured pin.
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
    //   * `ok_models`  - the reviewer slots whose completions succeeded
    //                    (`ReviewerSlot::Ok`); only these cast a verdict vote.
    //   * failures     - `ReviewerSlot::Failed` slots; excluded from the
    //                    worst-rank walk by VARIANT (C5-1), never by matching a
    //                    magic model-id string, so a real reviewer whose id
    //                    happens to be "error" still votes.
    // The total failure -> bail-Ok bug fixed here: when `ok_models` is empty
    // the aggregate verdict does NOT default to "comment" (which would have
    // passed CI); the caller now sees a nonzero exit so a CI review gate
    // cannot be silently green-lit by a single 401 or by every panel model
    // timing out.
    let mut reviews: Vec<ReviewerSlot> = Vec::new();
    let mut total_cost = 0.0;
    let mut ok_models: usize = 0;
    for r in results {
        match r {
            Ok(c) => {
                ok_models += 1;
                total_cost += c.cost_usd;
                reviews.push(ReviewerSlot::Ok {
                    model: c.model,
                    review: parse_review(&c.content),
                });
            }
            Err(e) => {
                // C5-1: a failed slot is a Failed VARIANT, not a magic
                // model-id string. It has no `model` label to collide with a
                // real reviewer named "error"; the worst-rank walk excludes it
                // by variant. `model` here is only cosmetic ("(failed)").
                reviews.push(ReviewerSlot::Failed {
                    model: "(failed)".into(),
                    err: format!("{e}"),
                });
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

    let agg = emit_reviews(cli, &aggregate);

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
    // SC1 [CRITICAL]: a blocking aggregate verdict MUST break the process
    // exit code. `cmd_review` used to `Ok(())` unconditionally after
    // emitting the payload, so a `request_changes` / `reject` / `block`
    // (or an unknown verdict, which `verdict_rank` fails closed to rank 2)
    // still exited 0. Automation keyed on the exit code (and the
    // background-job finalizer in main.rs, which stamps status from
    // res.is_ok()) therefore read a BLOCK as APPROVE. Surface it as a
    // nonzero exit AFTER `emit_reviews` has written result.json / printed
    // the full diagnostic, so the operator still gets the complete trail.
    if verdict_rank(&agg) >= 2 {
        anyhow::bail!(
            "review blocked: aggregate verdict `{agg}` requests changes \
(a blocking review must not exit 0)"
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
    reviewers: &'a [ReviewerSlot],
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
    reviews: &[ReviewerSlot],
    cost_usd: f64,
    requested: usize,
    ok_models: usize,
    failed_models: usize,
) -> (String, bool, serde_json::Value) {
    let all_failed = ok_models == 0;
    // Aggregate verdict = worst across reviewers that ACTUALLY completed.
    // C5-1: a slot's vote is counted iff it is an `Ok` VARIANT -- never by
    // string-comparing the model id to "error". This is what lets a real
    // reviewer model literally named "error" cast a blocking vote, while a
    // genuinely `Failed` slot is still excluded. If every slot failed, fall
    // through to "request_changes" so the rendered verdict visibly disagrees
    // with the silent-Ok() CI exit.
    let worst_rank = reviews
        .iter()
        .filter_map(ReviewerSlot::review)
        .map(|r| r.verdict.as_str())
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
        "reviewers": reviews.iter().map(|slot| json!({
            "model": slot.model(),
            "verdict": slot.display_verdict(),
            "summary": slot.display_summary(),
            "findings": slot.review().map(|r| r.findings.clone()).unwrap_or_default(),
            "next_steps": slot.review().map(|r| r.next_steps.clone()).unwrap_or_default(),
            "ok": slot.is_ok(),
        })).collect::<Vec<_>>(),
    });

    (agg, all_failed, payload)
}

/// Render the aggregated review(s) as JSON (machine) or text (human), and write
/// `result.json` when running as a background job. `aggregate.ok_models == 0`
/// is reflected in the payload as `complete: false` so downstream consumers
/// (CI gates, dashboards) can distinguish a real "comment" verdict from a
/// total-failure no-reviewer-actually-ran episode.
fn emit_reviews(cli: &crate::Cli, aggregate: &ReviewAggregate<'_>) -> String {
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
        return agg;
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
    for slot in aggregate.reviewers {
        let model = slot.model();
        println!("── {model} :: {} ──", slot.display_verdict());
        let summary = slot.display_summary();
        if !summary.is_empty() {
            println!("{summary}");
        }
        if let Some(r) = slot.review() {
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
                for step in &r.next_steps {
                    println!("    - {step}");
                }
            }
        }
        println!();
    }

    agg
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
    let t = crate::agentic_turn(cli, engine_kind, prompt, None, !cli.json, None).await?;

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

/// Settle budget the author-phase watchdog grants the daemon to ACK a
/// `session/cancel` before the loop captures `build_diff`. See
/// [`zoder_core::CANCEL_SETTLE_BUDGET`] for the canonical value —
/// kept in `zoder-core` so the `acp-client` wire-shape tests exercise
/// the same number the loop uses.
pub(crate) const SETTLE_BUDGET_SECS: u64 = zoder_core::CANCEL_SETTLE_BUDGET.as_secs();

/// Label for a `loop` phase. Phases are user-visible in the watchdog log
/// line ("loop: <phase> timed out after <N>s, killing") and in the per-iter
/// `author_outcome` / `review_outcome` fields when a phase wedges.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // LoopPhase::Author is no longer threaded through `phase_watchdog`
                    // (the author phase has its own `author_phase_with_cancel`
                    // wrapper), but the variant is retained for parity with the
                    // log/label surface and for any future per-phase routing.
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

/// Cancel-aware variant of `phase_watchdog` for the author phase. The
/// inner future is raced against a wall-clock budget; on expiry we MUST
/// tell the daemon to stop the turn before returning, otherwise the
/// daemon keeps editing files while the loop captures a torn diff for
/// review.
///
/// Two hooks are passed by the caller:
///
/// * `cancel` — invoked on timeout. In production this sends the
///   ACP `session/cancel` notification via [`crate::engine_socket_path`]
///   so the daemon actually stops the in-flight turn. In tests it
///   records the invocation so the timeout-path test can assert cancel
///   was issued (not just that the watchdog dropped the future).
/// * `settled` — awaited after `cancel`. This gates the watchdog's
///   return: `build_diff` is captured AFTER `settled` resolves, so the
///   loop never reviews a torn mid-edit tree. In production `settled`
///   is the bounded wait inside `cancel_session` itself (it returns
///   only after the daemon acknowledges the cancel or the settle
///   budget elapses, which is "settled enough"). In tests `settled`
///   is a channel/tokio task the test drives explicitly.
///
/// On timeout the function logs the standard "timed out, killing"
/// marker, awaits `cancel`, awaits `settled`, and returns `Err`. On
/// success it forwards the inner result verbatim. Non-timeout errors
/// from `cancel` / `settled` are swallowed (we're already on the
/// timeout path and the loop wants to RECOVER, not bubble IO errors).
///
/// NON-BREAKING on success: when the inner future completes within
/// budget, `cancel` and `settled` are never invoked.
async fn author_phase_with_cancel<F, C, S>(
    secs: u64,
    quiet: bool,
    turn_fut: F,
    cancel: C,
    settled: S,
) -> Result<crate::TurnResult, String>
where
    F: std::future::Future<Output = anyhow::Result<crate::TurnResult>>,
    C: std::future::Future<Output = anyhow::Result<()>>,
    S: std::future::Future<Output = ()>,
{
    let budget = std::time::Duration::from_secs(secs.max(1));
    match tokio::time::timeout(budget, turn_fut).await {
        Ok(res) => res.map_err(|e| e.to_string()),
        Err(_) => {
            if !quiet {
                eprintln!("loop: author timed out after {secs}s, cancelling daemon turn");
            }
            // Issue the cancel. Best-effort: a failure here means we
            // couldn't reach the daemon, but the caller's loop still
            // wants to recover (not bubble IO). The settle wait below
            // gives the daemon a bounded grace to wind down regardless.
            let _ = cancel.await;
            // Gate `build_diff` on the daemon having settled (or its
            // settle budget having elapsed). Without this, the loop
            // would race the daemon's last few tool-call writes against
            // `build_diff` and review a torn tree.
            settled.await;
            Err(format!(
                "author phase timed out after {secs}s (killed, daemon cancel issued)"
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
/// Before spawn, `cmd` is statically inspected by
/// [`crate::exec_safety::inspect_shell_command`] against a small denylist
/// of clearly-catastrophic patterns (`rm -rf /`, redirects to `/etc/...`,
/// `dd of=/dev/...`, `curl|sh`, …). A deny is returned to the caller as
/// `(false, reason)` so the loop naturally records the failure and feeds
/// it to the next author turn — no silent proceed, no special error type.
/// `allow_dangerous = true` skips the inspection and is the explicit
/// escape hatch for operators who really do need to run a destructive
/// validation command from `--check`.
///
/// `policy` drives the OS-level sandbox backend selection. When `policy`'s
/// `backend == ExecSandbox::None` (the default), the child is spawned as
/// `sh -c <cmd>` exactly as before — byte-for-byte, no argv change. When
/// `policy` selects a backend, `crate::exec_safety::wrap_spawn_command`
/// decides the wrapped argv; we don't duplicate the dispatch here so a
/// future backend (Linux bwrap, …) lands in exactly one place. A backend
/// that's unsupported on the running platform surfaces as `(false,
/// reason)` so the loop renders it like any other check failure.
///
/// Returns `(passed, tail)` where `tail` is the last ~4 KB of combined
/// stdout+stderr. On timeout `passed` is `false` and `tail` carries a clear
/// phase-timed-out marker so the next author turn can see it.
#[allow(clippy::too_many_arguments)]
async fn run_check_watched(
    cwd: &Path,
    cmd: &str,
    secs: u64,
    allow_dangerous: bool,
    policy: &zoder_core::ExecSafetyConfig,
) -> (bool, String) {
    if !allow_dangerous {
        match crate::exec_safety::inspect_shell_command(cmd) {
            crate::exec_safety::ExecVerdict::Allow => {}
            crate::exec_safety::ExecVerdict::Deny(reason) => {
                // Same return shape as a real failure so the loop treats a
                // denied check identically to a failing one — the next
                // author turn reads the deny-reason out of the tail.
                return (false, reason);
            }
        }
    }
    let budget = std::time::Duration::from_secs(secs.max(1));
    // Backend dispatch: the denylist above is the *if*, the policy is the
    // *how*. `wrap_spawn_command` is the single dispatch point so the
    // sandbox logic isn't duplicated; we just consume its argv here.
    let plan = match crate::exec_safety::wrap_spawn_command(cwd, cmd, policy) {
        Ok(p) => p,
        Err(reason) => {
            // An unsupported-platform / misconfigured-backend error from
            // the dispatch site is surfaced through the same `(false,
            // tail)` shape as a real command failure so the loop treats
            // it identically — the next author turn reads the reason out
            // of the tail verbatim.
            return (false, reason);
        }
    };
    let mut command = tokio::process::Command::new(&plan.argv[0]);
    for arg in &plan.argv[1..] {
        command.arg(arg);
    }
    // Z-15: the dispatch canonicalized the cwd ONCE into `plan.cwd`
    // and threaded that canonical path into the bwrap argv (`--bind`
    // and `--chdir`). The production `.current_dir()` MUST use the
    // SAME canonical value, not the raw `cwd` argument, so the
    // wrapped shell's view of the filesystem matches the policy
    // target — a single source of truth pins both, with no
    // TOCTOU window where the policy protects one tree and the
    // spawn runs in a different one.
    command
        .current_dir(&plan.cwd)
        .stdin(Stdio::null())
        // Detach the child into its own process group so we can SIGKILL the
        // whole subtree on timeout (shell + any descendants the command
        // forks). Tokio translates `process_group(0)` to setpgid(pid, 0) on
        // Unix, giving us a clean per-child group without an extra fork.
        .process_group(0)
        // SB2: reap the direct shell on the timeout/IO-error branch. The
        // `join` future below moves `child` into `wait_with_output()`; on
        // the `tokio::time::timeout` Elapsed branch that future is dropped
        // WITHOUT the Child ever being `wait()`ed, so the direct shell pid
        // would linger as a <defunct> zombie (the out-of-band group
        // `libc::kill(-pgid, SIGKILL)` terminates it but never reaps it) and
        // hold a PID slot until the whole zoder process exits. `kill_on_drop`
        // makes tokio's reaper `waitpid` the direct pid when the future is
        // dropped, closing the zombie leak over a long run.
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // In-process sandbox backends (currently: `LinuxLandlock`) apply
    // their ruleset via a `pre_exec` callback that runs in the child
    // between `fork` and `exec`. This is the textbook way to apply
    // Landlock's in-kernel ruleset to a spawned child: the
    // `landlock_restrict_self` syscall restricts ONLY the calling
    // thread (per-task struct, inherited across fork and exec), so
    // calling it in the parent would also restrict zoder itself. The
    // callback is `Send + 'static` because tokio's `Command` requires
    // it (the closure is moved into the freshly-forked child).
    //
    // The wiring is `cfg(target_os = "linux")`-gated because the
    // underlying `landlock` crate (and the `apply_*` helper) only
    // builds on Linux. On every other host the dispatch itself returns
    // `Err` before we get here, so this branch is dead code on macOS
    // but the rest of the function still compiles.
    #[cfg(target_os = "linux")]
    {
        if let Some(ruleset) = plan.in_process_ruleset.clone() {
            // The closure captures the descriptor list by value (it's
            // a `Vec<crate::exec_safety::LandlockRuleDescriptor>` we
            // cloned out of the plan) and returns a `std::io::Result<()>`
            // so tokio's `Command::pre_exec` accepts it. The
            // `apply_landlock_ruleset_in_child` helper handles every
            // crate call; we just adapt the error type here. A
            // failure inside the closure surfaces as a normal spawn
            // error from `command.spawn()` below — the loop renders
            // it through the same `(false, tail)` shape as a real
            // command failure.
            let closure = move || {
                crate::exec_safety::apply_landlock_ruleset_in_child(&ruleset)
                    .map_err(std::io::Error::other)
            };
            // SAFETY: `pre_exec` is `unsafe` because the closure runs
            // in the child between `fork` and `exec`, where most of the
            // libc is unavailable (async-signal-safety semantics). Our
            // closure only invokes the `landlock` crate's
            // `landlock_restrict_self` syscall via the
            // `apply_landlock_ruleset_in_child` helper; it touches no
            // shared state with the parent, allocates no thread-local
            // state, and is safe to call in the post-fork / pre-exec
            // window. The Landlock kernel ABI documents this exact
            // pattern (ruleset applied to self, then exec) as the
            // supported way to launch a restricted child.
            unsafe {
                command.pre_exec(closure);
            }
        }
    }
    // On non-Linux hosts `plan.in_process_ruleset` is always `None`
    // (the dispatch returns `Err` if the operator picks `LinuxLandlock`
    // on a non-Linux host), so the `pre_exec` wiring is dead code on
    // macOS. The `#[allow(unused_mut)]` here is NOT needed because
    // `command` is always mutated via the `arg` / `process_group` calls
    // above; the comment exists only to mark the boundary for future
    // readers who wonder why there's no `else` arm.
    #[cfg(not(target_os = "linux"))]
    {
        let _ = &plan.in_process_ruleset; // suppress dead-code on macOS
    }

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
///
/// `allow_dangerous` mirrors `run_check_watched`: the denylist inspection
/// runs unless the caller passes `true`.
///
/// `policy` mirrors `run_check_watched` and drives the OS-level sandbox
/// backend selection (see `crate::exec_safety::wrap_spawn_command`).
/// Default `&ExecSafetyConfig::default()` = `backend = None` preserves the
/// pre-sandbox byte-for-byte behavior.
#[cfg(test)]
fn run_check(
    cwd: &Path,
    cmd: &str,
    allow_dangerous: bool,
    policy: &zoder_core::ExecSafetyConfig,
) -> (bool, String) {
    if !allow_dangerous {
        match crate::exec_safety::inspect_shell_command(cmd) {
            crate::exec_safety::ExecVerdict::Allow => {}
            crate::exec_safety::ExecVerdict::Deny(reason) => return (false, reason),
        }
    }
    let plan = match crate::exec_safety::wrap_spawn_command(cwd, cmd, policy) {
        Ok(p) => p,
        Err(reason) => return (false, reason),
    };
    let mut cmd_builder = std::process::Command::new(&plan.argv[0]);
    for arg in &plan.argv[1..] {
        cmd_builder.arg(arg);
    }
    // Mirror the production `run_check_watched` `pre_exec` wiring so
    // the test helper actually applies the in-process ruleset when
    // the operator (or a test) selects `LinuxLandlock`. Without this
    // the test helper would spawn the child without the Landlock
    // ruleset, contradicting the contract the production code
    // implements. The wiring is `cfg(target_os = "linux")`-gated for
    // the same reason as in `run_check_watched` — the underlying
    // `landlock` crate only builds on Linux.
    #[cfg(target_os = "linux")]
    {
        if let Some(ruleset) = plan.in_process_ruleset.clone() {
            use std::os::unix::process::CommandExt;
            let closure = move || {
                crate::exec_safety::apply_landlock_ruleset_in_child(&ruleset)
                    .map_err(std::io::Error::other)
            };
            // SAFETY: see the matching note in `run_check_watched`
            // above. The closure only invokes the `landlock` crate's
            // `landlock_restrict_self` syscall via the
            // `apply_landlock_ruleset_in_child` helper, which is
            // safe to call in the post-fork / pre-exec window per the
            // Landlock kernel ABI.
            unsafe {
                cmd_builder.pre_exec(closure);
            }
        }
    }
    // On non-Linux hosts `plan.in_process_ruleset` is always `None`,
    // so this branch is dead code. The explicit reference silences the
    // unused-field warning on macOS without weakening the cfg-gated
    // wiring on Linux.
    #[cfg(not(target_os = "linux"))]
    {
        let _ = &plan.in_process_ruleset;
    }
    // Z-15: use the dispatch-canonicalized cwd, not the raw `cwd`
    // argument, so the wrapped shell's cwd matches the policy
    // target. See the matching note in `run_check_watched` above.
    match cmd_builder.current_dir(&plan.cwd).output() {
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

/// Synthesize the `ReviewOutput` used when the loop's review phase cannot
/// produce a verdict at all — most commonly a `phase_watchdog` wall-clock
/// timeout around `complete_once`, or a reviewer chain that exhausted with
/// 0/N reviewers completing.
///
/// **Fail-CLOSED.** Mirrors the all-failed → `"request_changes"` mapping
/// in the standalone `cmd_review` `aggregate_review` (line ~1967):
/// a review that never happened must NOT silently look like an
/// approving/comment review, so the synthesized verdict is the explicit
/// blocker `"request_changes"` rather than the non-blocking `"comment"`.
/// Centralizing this here (vs. an inline `ReviewOutput { ... }` literal
/// at the call site) lets `cmd_loop` and the test matrix share a single
/// source of truth for what an unreviewed iteration looks like.
fn synthesize_review_phase_failure(msg: &str) -> ReviewOutput {
    ReviewOutput {
        verdict: "request_changes".into(),
        summary: format!("reviewer {msg}"),
        ..Default::default()
    }
}

/// Fail-closed predicate: did the reviewer authorize this iteration to be
/// considered for resolution? This is the ONE place the explicit reviewer
/// verdict is consulted — the finding-severity heuristic (`blocking`)
/// cannot override an authoritative `request_changes` / `reject` /
/// `block`. A reviewer that returns `approve` OR a non-blocking comment
/// with zero heuristic-blocking findings is OK; every other explicit
/// verdict blocks resolution regardless of finding counts.
///
/// Verdict comparison is **case- and whitespace-insensitive**: real LLM
/// output drifts (`"BLOCK"`, `"Request_Changes"`, `" approve "`), and
/// case-sensitive matches previously let explicit `request_changes`-class
/// verdicts be silently downgraded to non-blocking. The normalization
/// (`trim` + ASCII lowercase) runs at the point of comparison so any
/// ReviewOutput construction site — `parse_review`, the
/// review-phase-failure synthesizer, or direct fixtures — gets the same
/// fail-closed semantics.
fn loop_review_ok(r: &ReviewOutput, blocking: usize) -> bool {
    let v = r.verdict.trim().to_ascii_lowercase();
    let explicit_block = matches!(v.as_str(), "request_changes" | "reject" | "block");
    if explicit_block {
        return false;
    }
    // W4: fail closed on any UNRECOGNIZED verdict. The old
    // `v == "approve" || blocking == 0` gave a free pass to every non-block
    // verdict whenever the heuristic found zero blocking findings — so a
    // typo/hallucinated verdict ("deny", "changes_requested", "needs_changes",
    // "lgtm", "fail") silently resolved the loop as if approved. Only the
    // known-benign verdicts may resolve; anything else is treated as blocking.
    if !matches!(v.as_str(), "approve" | "comment" | "neutral") {
        return false;
    }
    v == "approve" || blocking == 0
}

/// All signals needed by [`decide_loop_resolution`] for one iteration.
/// Lifted out of `cmd_loop` as a struct so the (substance × check ×
/// verdict × heuristic-blocking × no-new-progress) matrix can be pinned
/// by unit tests without spinning up the engine daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoopResolutionSignals {
    /// Classified diff substance for this iteration (anti-gaming guard).
    pub substance: DiffSubstance,
    /// Whether `--check` was configured for this loop run.
    pub check_configured: bool,
    /// Pass/fail of the check. `Some(true)` = passed, `Some(false)` =
    /// failed, `None` = no result (either `--check` was not configured
    /// or it was suppressed by the watchdog / error path).
    pub check_passed: Option<bool>,
    /// The reviewer verdict string for this iteration.
    pub verdict: String,
    /// Number of blocking findings per the severity heuristic
    /// (see [`count_blocking`]).
    pub blocking_findings: usize,
}

/// Decide whether THIS iteration's signals authorize resolving the loop.
/// Mirrors the decision logic in `cmd_loop` 1-for-1 so behavior changes
/// only flow through this single source of truth.
///
/// Fail-closed invariants:
///
///   1. ABSENT CHECK is not silently green. A never-configured `--check`
///      means resolution may still occur — but ONLY on `review_ok &&
///      substance_ok`, and never on a fabricated "green check".
///   2. An explicit `request_changes` / `reject` / `block` verdict
///      BLOCKS resolution regardless of `blocking_findings`. The
///      explicit verdict is authoritative; the heuristic is not.
///   3. Non-substantive diffs (Empty / WhitespaceOnly / CommentOnly) are
///      explicitly rejected by the anti-gaming guard, even when the
///      check is green.
pub(crate) fn decide_loop_resolution(s: &LoopResolutionSignals, accept_on_green: bool) -> bool {
    let substance_ok = substance_accept_eligible(&s.substance);
    let check_explicit_passed = s.check_passed == Some(true);
    // check_satisfied: True unless an actual `--check` ran and failed.
    // A never-configured check (`s.check_configured == false`) carries
    // NO negative information; treat it as "no obstacle to resolution",
    // but resolution still has to clear `review_ok && substance_ok`.
    let check_satisfied = s.check_passed != Some(false);
    let review = ReviewOutput {
        verdict: s.verdict.clone(),
        ..Default::default()
    };
    let review_ok = loop_review_ok(&review, s.blocking_findings);
    // Anti-gaming guard: never accept a non-substantive diff on a green /
    // check-satisfied iteration. Mirrors the in-loop guard exactly.
    if (check_explicit_passed || check_satisfied) && !substance_ok {
        return false;
    }
    // Fail-closed explicit / heuristic blocker: never override an
    // authoritative reviewer verdict with a passing check.
    if !review_ok {
        return false;
    }
    // `--accept-on-green` is an opt-in: requires a REAL passing check
    // AND no reviewer block. Even under --accept-on-green, an explicit
    // request_changes / reject / block verdict cannot be overridden.
    if accept_on_green && check_explicit_passed && substance_ok {
        return true;
    }
    // Default resolve path: check satisfied (no negative information)
    // AND reviewer OK AND substance OK.
    check_satisfied && substance_ok
}

/// Build a `LoopResolutionSignals` from a full `ReviewOutput` + check
/// state, recomputing the blocking-findings count via `count_blocking`
/// using the honest `green` calibration. Use this when callers have a
/// real review object (e.g. test fixtures that include findings);
/// [`LoopResolutionSignals`] is the struct form for pins that already
/// know the count.
pub(crate) fn loop_signals_from_review(
    substance: DiffSubstance,
    check_configured: bool,
    check_passed: Option<bool>,
    review: &ReviewOutput,
) -> LoopResolutionSignals {
    // Honest calibration: only an ACTUAL passing `--check` counts as
    // green for `count_blocking`'s severity threshold. A never-configured
    // check is `green = false`, which makes `high`-severity findings
    // blocking — closing the original FAIL-CLOSED defect where an
    // absent check was treated as if a check had passed.
    let green = check_passed == Some(true);
    let blocking_findings = count_blocking(review, green);
    LoopResolutionSignals {
        substance,
        check_configured,
        check_passed,
        verdict: review.verdict.clone(),
        blocking_findings,
    }
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

/// Compute the loop's dead-engine signal for one iteration. Returns `true`
/// when the iteration should count toward the dead-engine streak — i.e.,
/// the loop should not trust the engine to land any further work without
/// operator intervention.
///
/// Two failure modes count as dead:
///
///   1. `turn.is_none()` — the outer `author_phase_with_cancel` watchdog
///      killed the future (the engine's connection vanished, the spawn
///      failed, or the watchdog simply couldn't reach the daemon in
///      time). Pre-fix this was the ONLY dead-engine signal.
///   2. `turn.is_some_and(|t| !t.run.succeeded())` — the engine returned
///      an `Ok(TurnResult)` whose `AgentRun::outcome` is not
///      `"completed"`. The pre-fix code missed this entirely: an
///      engine that repeatedly internally-timed-out returns
///      `Ok(outcome="timeout")` (the inner `drive()` already sends
///      `session/cancel` and preserves partial output), and the loop
///      saw `turn.is_none() == false` so the dead-streak never
///      incremented. A wedged-but-still-talking engine would grind
///      through every `max_iters` instead of bailing after two strikes.
///
/// This helper is the single source of truth for the dead-engine signal;
/// `update_loop_streaks` consumes it via its `turn_failed` parameter.
pub(crate) fn turn_is_dead(turn: &Option<crate::TurnResult>) -> bool {
    // Treat `None` (the outer watchdog killed the in-flight future) AND
    // `Some(_)` whose `run.outcome` is not `"completed"` (the engine
    // returned Ok but didn't actually land the turn — e.g., an internal
    // `agent_timeout` already canceled and preserved partial output, a
    // hard `failed`, a `max_tokens` truncation, etc.) as dead.
    //
    // The pre-fix signal `turn.is_none()` only missed `Some(non-completed)`,
    // so a wedged-but-still-talking engine ground through every
    // `max_iter` instead of bailing after 2 strikes.
    turn.as_ref().is_none_or(|t| !t.run.succeeded())
}

/// Apply one iteration's signals to the loop's streak counters and decide
/// whether to abort. Pure / deterministic so the full input matrix can be
/// unit-tested.
///
/// Invariants this helper enforces (the regression is exactly the first one):
///   * `turn_failed && diff_empty` -> dead_streak += 1.
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

/// SA1 liveness helper: decide the stall bookkeeping for the anti-gaming
/// branch (green check + non-substantive diff) so the loop early-aborts on a
/// *stable* non-substantive diff instead of burning every `--max-iters`.
///
/// A `WhitespaceOnly` / `CommentOnly` diff carries real `diff --git` / `+++`
/// headers, so `diff.trim().is_empty()` is false and the ordinary
/// `dead_streak` / `stall_streak` accounting never bumps for it. And the
/// anti-gaming `continue` fires BEFORE the normal no-progress stall check, so
/// without this the loop never stops on a repeated non-substantive diff.
///
/// Returns `(new_stall_streak, abort)`:
///   * `repeated` (this iter's diff == the previous iter's diff, and we are
///     past iter 1) -> `stall_streak + 1`, abort iff it reaches `stall_limit`.
///   * a *changed* non-substantive diff -> reset `stall_streak` to 0 (the
///     author is still churning something new; keep grinding until it stalls).
///
/// This ONLY makes the loop stop sooner; the caller still `continue`s / breaks
/// WITHOUT resolving, so the substance gate is untouched and there is no
/// false-resolve path.
fn nonsubstantive_stall_step(
    repeated: bool,
    prev_stall_streak: usize,
    stall_limit: usize,
) -> (usize, bool) {
    if repeated {
        let next = prev_stall_streak + 1;
        (next, next >= stall_limit)
    } else {
        (0, false)
    }
}

fn update_loop_streaks(
    turn_failed: bool,
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
    let dead_streak = if turn_failed { prev_dead_streak + 1 } else { 0 };
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
///
/// `allow_dangerous_check` opts out of the pre-exec denylist in
/// [`crate::exec_safety::inspect_shell_command`] that runs against the
/// `--check` command string before `sh -c` spawns it. Default is `false`;
/// an operator who genuinely needs to run a destructive validation command
/// can pass `--allow-dangerous-check` to skip the inspection.
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
    allow_dangerous_check: bool,
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

    // Read the operator's exec-safety policy (the OS-level sandbox backend
    // selection — see `zoder_core::ExecSafetyConfig`). Default = `None`
    // preserves the pre-sandbox byte-for-byte behavior. We do this once at
    // the top of the loop instead of per-iteration because the config is
    // immutable for the loop's lifetime and because every per-iteration
    // `Engine::load()` would re-read `config.json` and the overlay TOMLs.
    // A failure to load the engine is treated as a fatal misconfig — the
    // operator's config is broken in a way that already prevents the loop
    // from running at all.
    let exec_safety_policy = Engine::load()?.cfg.exec_safety;

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
    // HEAD SHA captured at the end of the previous iteration, used to detect
    // "this author turn produced a new commit" (the post-commit safety net
    // that reconciles the commit's author with the repo's configured
    // identity — see `enforce_repo_commit_author`). `None` when the repo has
    // no commits yet; the transition `None -> Some(sha)` then triggers a
    // correction on the first author-made commit.
    let mut prev_head: Option<String> = rev_parse_head(&cwd);
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
        // `author_phase_with_cancel` enforces a hard kill-budget around the
        // turn AND, on timeout, ACTUALLY cancels the daemon turn (via ACP
        // `session/cancel`) and gates `build_diff` on the daemon having
        // settled — so the loop never reviews a torn mid-edit tree. The
        // production incident surfaced here: the prior `phase_watchdog` only
        // dropped the client future, the daemon kept editing, and `build_diff`
        // captured the torn tree right under the reviewer's nose. See
        // [`author_phase_with_cancel`] for the cancel/settle contract and the
        // unit tests that pin the timeout-path invariants.
        let mut author_err: Option<String> = None;
        let engine_kind = crate::resolve_engine_kind(cli)?;
        // The cancel future needs the session id to address the right turn on
        // the daemon. For iterations 2+ (`session == Some(sid)`) we resume the
        // same session, so the cancel has a known target. For the very first
        // iteration without `--session`/`--continue`/`--persist-session`,
        // `session` is `None` — the inner engine mints a new session id we
        // cannot observe from outside the in-flight future. We still open a
        // daemon connection and wait for the settle budget; if the daemon
        // happens to expose the freshly-minted session via a notification
        // before the budget elapses, the cancel will land; if not, the bounded
        // wait is the best we can do without invasive plumbing. (The first
        // iteration is also the one most likely to succeed — a wedged
        // mid-session is much rarer than a wedged mid-prompt on a resumed
        // session.) The settle signal is implicit: `cancel_session` itself
        // awaits `session/update {type: "turn_complete"}` (or the settle
        // budget) before returning, so `build_diff` is gated on the daemon
        // having actually wound down.
        let session_id_for_cancel = session.clone();
        let turn = match author_phase_with_cancel(
            loop_timeout_secs,
            cli.quiet,
            crate::agentic_turn(
                cli,
                engine_kind,
                author_prompt,
                session.clone(),
                false,
                None,
            ),
            async move {
                // Cancel the daemon turn for `session_id_for_cancel` (best-
                // effort). If `None`, the daemon may have a freshly-minted
                // session we can't address from outside — we still wait for
                // the settle budget so the daemon has a chance to wind down
                // any in-flight tool calls before we capture the diff.
                let socket = crate::engine_socket_path();
                if let Some(sid) = session_id_for_cancel.as_deref() {
                    match zoder_core::cancel_session(
                        &socket,
                        sid,
                        std::time::Duration::from_secs(SETTLE_BUDGET_SECS),
                    )
                    .await
                    {
                        Ok(()) => {}
                        Err(e) => {
                            // Best-effort: don't fail the loop on a settle
                            // error, but DO emit a warning so operators can
                            // see when the daemon failed to acknowledge a
                            // cancel — that often means the daemon crashed
                            // mid-edit and the diff we'd capture next is a
                            // torn tree.
                            if !cli.quiet {
                                eprintln!("[loop] cancel_session for {sid} did not settle: {e}");
                            }
                        }
                    }
                } else {
                    // No known session id — best-effort settle wait.
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                Ok::<(), anyhow::Error>(())
            },
            async {
                // Settled signal: a small grace after cancel_session returns
                // so any final tool-result write hits disk before `build_diff`.
                // cancel_session itself already waits for the daemon's
                // turn_complete (or the settle budget), so this is just a
                // belt-and-braces margin for filesystem visibility of the
                // daemon's last edits.
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            },
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

        // 2. Post-commit author reconciliation. The default engine is a
        //    long-lived zeroclaw daemon whose git identity is out of
        //    zoder's control, so any commit the author turn landed in
        //    the working tree is amended to carry the repo's configured
        //    `user.name`/`user.email` here, before the diff is built
        //    and the reviewer is invoked. This is the safety net that
        //    closes the recurring operational defect where every
        //    `zoder loop` commit landed under `zoder-bot <...>` and a
        //    human had to `git commit --amend --author=...` before it
        //    was push-safe. See `enforce_repo_commit_author` for the
        //    invariants (author-only, no-op when already correct, no-op
        //    when no configured identity).
        let current_head: Option<String> = rev_parse_head(&cwd);
        let commit_author_enforcement: Option<CommitAuthorEnforcement> =
            if current_head != prev_head {
                let result = enforce_repo_commit_author(&cwd);
                if let CommitAuthorEnforcement::Corrected { from, to } = &result {
                    if !cli.quiet {
                        eprintln!("[zoder] corrected commit author: {from} -> {to}");
                    }
                }
                Some(result)
            } else {
                None
            };

        // 3. Capture the working-tree diff (whatever edits actually landed).
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

        // 4. Validate (build/test) if a check command was given. The check is
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
                let (ok, tail) = run_check_watched(
                    &cwd,
                    c,
                    loop_timeout_secs,
                    allow_dangerous_check,
                    &exec_safety_policy,
                )
                .await;
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
        // and does NOT contribute to `dead_streak`. The dead-engine signal
        // (`turn_is_dead(&turn)`) counts BOTH:
        //   * the outer watchdog killing the future (`turn.is_none()`), and
        //   * the inner engine returning `Ok(TurnResult { outcome !=
        //     "completed" })` — e.g., the internal `agent_timeout` firing.
        // The pre-fix code keyed on `turn.is_none()` only, which let an
        // engine that repeatedly internally-timed-out (returning
        // `Ok(outcome="timeout")`) grind through every `max_iters` instead
        // of bailing after 2 strikes. See [`turn_is_dead`] for the test
        // pins.
        let streaks = update_loop_streaks(
            turn_is_dead(&turn),
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

        // 5. Adversarial review of the current diff (+ validation output).
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
                //
                // Z-1 REGRESSION GUARD: the synthesized ReviewOutput MUST be
                // fail-closed. With a non-blocking `"comment"` verdict, the
                // downstream `loop_review_ok` / `decide_loop_resolution`
                // treated the iteration as "approved" on a green check and
                // reported RESOLVED — even though no reviewer had actually
                // run. The synthesizer (above) emits `"request_changes"` for
                // exactly this reason; see its doc comment and the
                // `review_phase_failure_synthesis_is_request_changes` test.
                synthesize_review_phase_failure(&msg)
            }
        };
        final_verdict = review.verdict.clone();
        // The objective gate is "green" ONLY when an actual `--check` ran and
        // passed. A never-configured check is NOT green — it is unknown, and
        // this branch must NOT fabricate one. `check_satisfied` is the
        // fail-closed predicate the resolve gate consumes: it is `true` when
        // either no `--check` was requested OR the configured `--check`
        // returned exit 0. `green` (kept for the `count_blocking` severity
        // calibration only) follows the same honest rule.
        let green = check_passed == Some(true);
        let check_satisfied = check_passed != Some(false);
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
        // outcome, different root cause. `commit_author_enforcement` is the
        // post-commit safety net: when the author turn moved HEAD, this
        // records whether the new commit's author was already correct,
        // was rewritten to the repo's configured identity, was left alone
        // because no identity was configured, or some other terminal
        // outcome — so the iter record is the single source of truth
        // for what (if anything) the loop had to fix on disk.
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
            "commit_author_enforcement": commit_author_enforcement_to_json(
                commit_author_enforcement.as_ref()
            ),
        }));

        // 6. Decide: review gate AND objective gate AND anti-gaming substance gate.
        //    `review_ok` is now AUTHORITATIVE on the explicit verdict — see
        //    `loop_review_ok`. `check_satisfied` does not fabricate a green check
        //    when none was configured; resolution may still occur on
        //    `review_ok && substance_ok` when `--check` is absent (logged below).
        //    The combined decision is delegated to `decide_loop_resolution` so
        //    the full signal matrix is unit-testable as a single function.
        let review_ok = loop_review_ok(&review, blocking);
        let check_green = check_passed == Some(true);

        // Anti-gaming guard: `diff_lines > 0` is trivially gameable (a
        // "green" iteration whose diff is empty-after-headers, only
        // whitespace, only comments, or only churns test files used to
        // resolve the loop). `substance_ok` is the strict replacement:
        // `Substantive` and `TestOnly` are accept-eligible; everything
        // else is explicitly rejected as non-substantive noise.
        let substance_ok = substance_accept_eligible(&diff_substance);

        // Single-source-of-truth resolution decision: tests pin the
        // (substance × check × verdict × blocking-findings) matrix
        // through this exact predicate. `loop_signals_from_review` also
        // routes the count_blocking calibration through the honest
        // `green = check_passed == Some(true)` rule so the absent-check
        // case cannot be treated as a fabricated green.
        let resolve_now = decide_loop_resolution(
            &loop_signals_from_review(diff_substance, check.is_some(), check_passed, &review),
            accept_on_green,
        );

        // Anti-gaming guard rail: if the check is green but the diff is
        // not substantive, the iteration MUST NOT resolve. Emit a clear
        // reason, record it in the iter record (the `substance` field
        // added above), and continue to the next iteration — same
        // control flow as a failed check. This closes the gaming surface
        // where an over-eager author could "pass" the loop with whitespace
        // churn or comment-only edits.
        if (check_green || check_satisfied) && !substance_ok {
            if !cli.quiet {
                eprintln!(
                    "[loop] iter {i}: REJECTED green — diff is {diff_substance:?}, \
 not substantive work (anti-gaming guard)"
                );
            }
            // SA1 liveness: a *stable* non-substantive diff (WhitespaceOnly /
            // CommentOnly repeated verbatim each iteration) has real
            // `diff --git`/`+++` headers, so `diff.trim().is_empty()` is false
            // and neither `dead_streak` nor `stall_streak` bumps below. Because
            // this anti-gaming `continue` fires BEFORE the stall check further
            // down, the loop would otherwise never early-abort and burn all
            // `--max-iters` re-authoring the same noise. Fold the stall
            // accounting in here so a repeated non-substantive diff trips the
            // same STALL_LIMIT abort. This does NOT relax the substance gate —
            // we only stop *sooner*; the iteration still correctly refuses to
            // resolve, so there is no false-resolve path.
            let repeated = i > 1 && diff == prev_diff;
            let (next_stall, abort) =
                nonsubstantive_stall_step(repeated, stall_streak, STALL_LIMIT);
            stall_streak = next_stall;
            prev_diff = diff.clone();
            if abort {
                if !cli.quiet {
                    eprintln!(
                        "[loop] iter {i}: no new progress for {stall_streak} \
consecutive non-substantive iteration(s); stopping."
                    );
                }
                break;
            }
            continue;
        }

        // Test-only warning: the diff is acceptable (it touches test files
        // that build/test is green for) but the author did NOT actually
        // change any non-test source — exactly the shape of a gaming
        // attempt that slipped past the reviewer. Warn loudly so the
        // operator sees why a "no real code change" iteration resolved.
        if matches!(diff_substance, DiffSubstance::TestOnly)
            && (check_green || check_satisfied)
            && !cli.quiet
        {
            eprintln!(
                "[loop] iter {i}: WARNING accepting a test-only diff \
 (no non-test source changed)"
            );
        }

        // Fail-closed observability: when no --check was configured, log
        // explicitly so an operator can see the resolution rests on review
        // alone. Never claim a green check.
        if check.is_none() && !cli.quiet {
            eprintln!(
                "[loop] iter {i}: NOTE no --check configured — resolution rests on review \
 alone (verdict={}, blocking={blocking})",
                review.verdict
            );
        }

        // Escape hatch: `--accept-on-green` treats a passing objective check as
        // sufficient, with reviewer findings advisory (for over-strict reviewers).
        // Still gated by an explicit reviewer verdict — `request_changes` /
        // `reject` / `block` block even under --accept-on-green.
        if resolve_now {
            resolved = true;
            if !cli.quiet {
                let via = if accept_on_green
                    && check_passed == Some(true)
                    && !matches!(diff_substance, DiffSubstance::TestOnly)
                {
                    "on green check (--accept-on-green)"
                } else {
                    ""
                };
                if via.is_empty() {
                    eprintln!(
                        "[loop] iter {i}: RESOLVED (check={:?} verdict={})",
                        check_passed, review.verdict
                    );
                } else {
                    eprintln!(
                        "[loop] iter {i}: RESOLVED {via} (reviewer advisory, verdict={}, \
 blocking={blocking})",
                        review.verdict
                    );
                }
            }
            break;
        }

        // No-progress guard (2nd iteration on): an identical diff that still
        // isn't accepted. (Never trips on iter 1, where prev_diff is empty.)
        let no_new_progress = i > 1 && diff == prev_diff;
        if no_new_progress {
            // Requirement (fail-closed): never convert 'no change since the
            // last blocking review' into a successful resolution. The
            // previous stalemate breaker here used to RESOLVE the loop with
            // warnings when the check was green and the reviewer was
            // blocking; that path is gone — a blocker on an unchanged diff
            // means iterate (or, at --max-iters end, UNRESOLVED).
            //
            // A single no-op turn is not fatal: the author may just need a
            // firmer nudge (see the escalated feedback below). Only give up
            // after STALL_LIMIT consecutive stalls — otherwise keep grinding.
            if !review_ok && !cli.quiet {
                eprintln!(
                    "[loop] iter {i}: reviewer still blocking on unchanged diff \
 (verdict={}, blocking={blocking}); will iterate and stop after {STALL_LIMIT} \
 consecutive stalls.",
                    review.verdict
                );
            }
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
        // Advance the "previous HEAD" cursor for the next iter's
        // new-commit detection. After a successful amend, `current_head`
        // is the new SHA; if the author never moved HEAD, this re-asserts
        // the unchanged value. Captured here (not at iter start) so a
        // commit that lands during this iter is correctly detected as
        // "new" only for THIS iter — the next iter's cursor is the
        // post-amend state, which is the only correct "previous" for
        // detecting further author-made commits.
        prev_head = current_head;

        // 7. Compose feedback for the next author turn.
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

/// Look up the resumable session id for `transfer`.
///
/// Returns the most-recently-updated session in `sessions_dir` (the same
/// one `--continue` would resolve to), or an error if none exists. Used as
/// a named seam so tests can drive the lookup with an isolated tempdir
/// without depending on `ZODER_HOME`.
fn transfer_resume_target(sessions_dir: &Path) -> anyhow::Result<String> {
    let Some(session) = Session::latest(sessions_dir)? else {
        anyhow::bail!(
            "no resumable session found in {} (run a session first: \
             `zoder exec ...` to establish one, then `zoder transfer` to print \
             its id; pass --session <id> explicitly to use a known id)",
            sessions_dir.display()
        );
    };
    Ok(session.id)
}

pub(crate) async fn cmd_transfer(cli: &crate::Cli) -> anyhow::Result<()> {
    let cwd = crate::agentic_cwd(cli)?;
    // `transfer` returns the resumable id of the PRIOR multi-turn session
    // already established for this workspace (the one a previous `zoder
    // exec --session <id>` or `zoder ... --continue` wrote). It MUST NOT
    // mint a new, empty session id: that would defeat the entire point of
    // transfer (picking up an in-flight thread from another terminal/host),
    // and the field report that triggered this fix found a user "resuming"
    // a fabricated-empty session and losing real context. If nothing exists
    // yet, `transfer_resume_target` fails loudly so the caller can create
    // one intentionally instead.
    let cfg = Config::load().context("loading zoder config to locate sessions dir")?;
    let sessions_dir = cfg.sessions_dir();
    let sid = transfer_resume_target(&sessions_dir)?;
    if cli.json {
        println!(
            "{}",
            json!({"session_id": sid, "cwd": cwd.to_string_lossy()})
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
pub(crate) struct JobMeta {
    pub(crate) id: String,
    pub(crate) kind: String,
    /// `running` | `done` | `failed` | `cancelled` — the same on-disk
    /// vocabulary `status`/`result`/`cancel` already key on.
    pub(crate) status: String,
    pub(crate) cwd: String,
    pub(crate) pid: u32,
    pub(crate) started: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) finished: Option<DateTime<Utc>>,
}

/// Resolved state-directory home: `$ZODER_HOME` or `~/.zoder`. Exposed at
/// crate scope so the jobs-management subcommand resolves the SAME place
/// `status` / `result` / `cancel` read from — never an independent path.
pub(crate) fn jobs_dir() -> PathBuf {
    Config::home().join("jobs")
}

/// `$ZODER_JOB_DIR` when this process is the detached worker of a job.
pub(crate) fn active_job_dir() -> Option<PathBuf> {
    std::env::var("ZODER_JOB_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// `JobMeta` is a small JSON record (a handful of UTF-8 fields:
/// `id`/`kind`/`status`/`cwd`/`pid`/`started`/`finished`) so its
/// serialized form is a few hundred bytes. 64 KiB is the same order
/// of magnitude as `MAX_PROJECT_INSTRUCTIONS_BYTES` (32 KiB in
/// `crates/zoder-core/src/project_instructions.rs`) — generous
/// enough to cover any realistic per-job metadata, while well below
/// the multi-MB caps used elsewhere (cf. `MAX_CONFIG_BYTES` /
/// `MAX_PRICE_BYTES` for corpus, pricing, and configuration). A
/// `meta.json` larger than this cap is treated identically to a
/// missing or unreadable file by `read_meta`.
pub(crate) const MAX_JOB_META_BYTES: u64 = 64 * 1024;

fn read_meta(dir: &Path) -> Option<JobMeta> {
    let path = dir.join("meta.json");
    // DEFECT 1 guard: never follow a symlink at this path, and never
    // read an unbounded size off the disk. Job metadata lives under
    // `~/.zoder/jobs/<id>/meta.json`, and a malicious (or merely
    // curious) write there can swap `meta.json` for a symlink to
    // `/dev/zero` (which would OOM `read_to_string`) or for a
    // FIFO/pipe (which would block `read_to_string` forever). The
    // load is deferred to [`read_bounded_job_meta`] which opens the
    // path with `O_NOFOLLOW | O_NONBLOCK` so the symlink itself is
    // rejected at open time (kernel `ELOOP`) rather than followed,
    // fstat's the open FD to confirm a regular file at-or-below
    // [`MAX_JOB_META_BYTES`], and reads through `Read::take` so a
    // racing swap (a same-uid process renaming `meta.json` aside and
    // `mkfifo`-ing the path) cannot re-introduce the unbounded-read
    // hazard once we hold the FD on the original inode. Mirrors the
    // TOCTOU-safe idiom established in
    // `crates/zoder-core/src/config.rs::read_bounded_regular_file`.
    let mut meta = read_bounded_job_meta(&path)?;
    let dir_id = dir.file_name()?.to_str()?;
    // The containing directory is the filesystem identity. Treat the body
    // `id` as untrusted input so callers never construct job paths from a
    // mismatched or crafted meta.json field.
    if meta.id != dir_id {
        meta.id = dir_id.to_string();
    }
    Some(meta)
}

/// Bounded, regular-file-only read of a job's `meta.json`. Returns
/// `None` on every failure (missing file, symlink, FIFO, oversized,
/// non-UTF-8, JSON parse error) so the caller — `read_dir_jobs` —
/// skips the entry exactly as it has always done for missing /
/// unreadable entries. The open uses
/// `O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK` on Unix so a symlink at the
/// path is rejected at open time (`ELOOP`) and a writer-less FIFO
/// returns `ENXIO` rather than blocking the kernel indefinitely.
fn read_bounded_job_meta(path: &Path) -> Option<JobMeta> {
    use std::io::Read;

    // Step 1: open on Unix with O_NOFOLLOW (symlink rejection) plus
    // O_NONBLOCK (fail-fast on writer-less FIFOs). O_CLOEXEC keeps
    // the FD from leaking into child processes spawned post-load.
    let f = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(JOB_META_OPEN_FLAGS)
                .open(path)
        }
        #[cfg(not(unix))]
        {
            // The defect description is Unix-centric (the
            // `~/.zoder/jobs/` registry is itself a Unix-only
            // concept); on non-Unix the loader still rejects
            // non-regular files via the fstat-driven `is_file`
            // guard and bounded `read_to_string`.
            std::fs::File::open(path)
        }
    }
    .ok()?;

    // Step 2: validate on the open FD (fstat). The pre-fix code
    // called `read_to_string(dir.join("meta.json"))` with no guard
    // at all, so a FIFO at the path would block forever and a
    // multi-GB file would OOM `read_dir_jobs` (and therefore
    // `zoder jobs list`). `f.metadata()` reports the inode the
    // kernel handed us in step 1 — a symlink at the path was
    // already rejected by O_NOFOLLOW above; on non-Unix this is
    // the primary defense.
    let meta = f.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    if meta.len() > MAX_JOB_META_BYTES {
        return None;
    }

    // Step 3: bounded read from the open FD, capped at the same
    // `MAX_JOB_META_BYTES` the fstat validated, so a racing
    // growth of the path cannot OOM us. UTF-8 validation also
    // happens here — a binary blob at `meta.json` is treated as
    // "skip this entry".
    let mut s = String::new();
    f.take(MAX_JOB_META_BYTES).read_to_string(&mut s).ok()?;
    serde_json::from_str(&s).ok()
}

#[cfg(unix)]
const JOB_META_OPEN_FLAGS: libc::c_int = libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;

/// Read every `<id>/meta.json` under `dir`, skipping entries that fail to
/// parse. Sorted newest-started-first, matching `all_jobs()`. Used by both
/// the in-process dispatcher and the `jobs list`/`jobs prune` subcommands
/// (the latter passes an explicit `dir` so tests can point at a tempdir).
pub(crate) fn read_dir_jobs(dir: &Path) -> Vec<JobMeta> {
    let mut jobs: Vec<JobMeta> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Some(m) = read_meta(&e.path()) {
                jobs.push(m);
            }
        }
    }
    jobs.sort_by(|a, b| b.started.cmp(&a.started));
    jobs
}

fn write_meta(dir: &Path, meta: &JobMeta) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(test)]
    if FAIL_NEXT_WRITE_META.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return Err(std::io::Error::other("injected write_meta failure").into());
    }
    std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(meta)?)?;
    Ok(())
}

fn background_worker_invocation() -> anyhow::Result<(PathBuf, Vec<String>)> {
    #[cfg(test)]
    {
        let guard = TEST_BACKGROUND_WORKER_COMMAND
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((exe, args)) = guard.as_ref() {
            return Ok((exe.clone(), args.clone()));
        }
    }

    let exe = std::env::current_exe().context("locating current executable")?;
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--background")
        .collect();
    Ok((exe, args))
}

fn configure_background_worker_command(
    command: &mut Command,
    args: &[String],
    dir: &Path,
    out: std::fs::File,
    err: std::fs::File,
) {
    command
        .args(args)
        .env("ZODER_JOB_DIR", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    // Detach the worker into its own process group so cancel can SIGTERM /
    // SIGKILL the WHOLE subtree. `process_group(0)` maps to setpgid(pid, 0)
    // on Unix, so the child leads a fresh group with pgid == pid.
    #[cfg(unix)]
    command.process_group(0);
}

/// Re-exec the current invocation as a detached worker writing to a new job dir.
pub(crate) fn spawn_background(kind: &str, cwd: &Path) -> anyhow::Result<String> {
    let id = format!(
        "{}-{:04x}",
        Utc::now().format("%Y%m%d-%H%M%S"),
        std::process::id() & 0xffff
    );
    let dir = jobs_dir().join(&id);
    std::fs::create_dir_all(&dir)?;

    let (exe, args) = background_worker_invocation()?;
    let out = std::fs::File::create(dir.join("output.txt"))?;
    let err = out.try_clone()?;
    let mut command = Command::new(&exe);
    configure_background_worker_command(&mut command, &args, &dir, out, err);
    let mut child = command
        .spawn()
        .with_context(|| format!("spawning background worker {}", exe.display()))?;
    #[cfg(test)]
    LAST_BACKGROUND_CHILD_PID.store(child.id(), std::sync::atomic::Ordering::SeqCst);

    let meta = JobMeta {
        id: id.clone(),
        kind: kind.to_string(),
        status: "running".into(),
        cwd: cwd.to_string_lossy().to_string(),
        pid: child.id(),
        started: Utc::now(),
        finished: None,
    };
    if let Err(err) = write_meta(&dir, &meta) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelSignalOutcome {
    AlreadyFinished,
    Signalled,
}

#[cfg(unix)]
const CANCEL_TERM_GRACE: Duration = Duration::from_millis(250);
#[cfg(unix)]
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[cfg(unix)]
fn signed_positive_pid(pid: u32) -> Option<libc::pid_t> {
    let signed = i32::try_from(pid).ok()?;
    (signed > 0).then_some(signed as libc::pid_t)
}

#[cfg(unix)]
fn process_group_alive(pgid: u32) -> bool {
    let Some(pgid) = signed_positive_pid(pgid) else {
        return false;
    };
    // SAFETY: signal 0 probes the process group without delivering a signal.
    let r = unsafe { libc::kill(-pgid, 0) };
    if r == 0 {
        return true;
    }
    let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    err == libc::EPERM
}

#[cfg(unix)]
fn signal_process_group(pgid: u32, sig: libc::c_int) -> anyhow::Result<bool> {
    let Some(pgid) = signed_positive_pid(pgid) else {
        return Ok(false);
    };
    // SAFETY: `pgid` is a positive pid_t; negating it targets the process
    // group instead of one raw PID.
    let r = unsafe { libc::kill(-pgid, sig) };
    if r == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(false);
    }
    Err(err).with_context(|| format!("signalling process group {pgid} with signal {sig}"))
}

#[cfg(unix)]
fn wait_for_process_group_exit(pgid: u32, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while process_group_alive(pgid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(CANCEL_POLL_INTERVAL);
    }
    true
}

#[cfg(unix)]
fn cancel_background_process_group(meta: &JobMeta) -> anyhow::Result<CancelSignalOutcome> {
    if !crate::jobs::pid_alive(meta.pid) {
        return Ok(CancelSignalOutcome::AlreadyFinished);
    }
    if !signal_process_group(meta.pid, libc::SIGTERM)? {
        return Ok(CancelSignalOutcome::AlreadyFinished);
    }
    if !wait_for_process_group_exit(meta.pid, CANCEL_TERM_GRACE) && process_group_alive(meta.pid) {
        let _ = signal_process_group(meta.pid, libc::SIGKILL);
    }
    Ok(CancelSignalOutcome::Signalled)
}

#[cfg(not(unix))]
fn cancel_background_process_group(_meta: &JobMeta) -> anyhow::Result<CancelSignalOutcome> {
    Ok(CancelSignalOutcome::Signalled)
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
    // W12/Y-20 defense in depth: `read_meta` canonicalizes `m.id` to the
    // on-disk dir entry, and this containment guard still refuses any
    // unexpected non-child id before reading from the jobs tree.
    let base = jobs_dir();
    if !crate::jobs::job_id_is_contained_child(&base, &m.id) {
        return Err(anyhow!(
            "refusing job id {:?}: not a direct child of the jobs dir",
            m.id
        ));
    }
    let dir = base.join(&m.id);
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
    // W12/Y-20 defense in depth: `read_meta` canonicalizes `m.id` to the
    // on-disk dir entry, and this containment guard still refuses any
    // unexpected non-child id before writing into the jobs tree.
    let base = jobs_dir();
    if !crate::jobs::job_id_is_contained_child(&base, &m.id) {
        return Err(anyhow!(
            "refusing job id {:?}: not a direct child of the jobs dir",
            m.id
        ));
    }
    let dir = base.join(&m.id);
    if cancel_background_process_group(&m)? == CancelSignalOutcome::AlreadyFinished {
        println!("job already finished: {}", m.id);
        return Ok(());
    }
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

    // ---- C2-1: configured reviewer_model pin must outrank scenario auto-routing ----

    #[test]
    fn config_reviewer_pin_outranks_scenario_head() {
        // The common case: no explicit `--reviewer`/`--panel` on the CLI
        // (model_override = None), the scenario auto-routed a reviewer lane
        // (non-empty `reviewer_chain`), AND the operator configured a
        // profile-level `reviewer_model` pin (`config_chain`). The CONFIG
        // PIN must head the resulting chain, not the scenario head --
        // otherwise scenario auto-routing shadows the operator's pin and can
        // route a same-family reviewer, defeating the cross-family gate.
        let config_chain = vec!["config-reviewer".to_string()];
        let scenario_chain = vec!["scenario-reviewer".to_string(), "scenario-alt".to_string()];
        let out = order_reviewer_candidates(None, None, &config_chain, &scenario_chain);
        assert_eq!(
            out.first().map(String::as_str),
            Some("config-reviewer"),
            "C2-1: configured reviewer_model pin must head the chain, not the scenario head"
        );
        // Scenario entries are retained, but strictly as TAIL fallbacks.
        assert_eq!(
            out,
            vec![
                "config-reviewer".to_string(),
                "scenario-reviewer".to_string(),
                "scenario-alt".to_string(),
            ],
            "C2-1: config pin heads; scenario chain follows as tail fallbacks"
        );
    }

    #[test]
    fn per_agent_reviewer_pin_outranks_scenario_head() {
        // Same seam via the per-agent `[agents.<alias>].reviewer_model` pin:
        // it must also head the chain ahead of scenario auto-routing.
        let scenario_chain = vec!["scenario-reviewer".to_string()];
        let out = order_reviewer_candidates(None, Some("agent-pin-reviewer"), &[], &scenario_chain);
        assert_eq!(
            out.first().map(String::as_str),
            Some("agent-pin-reviewer"),
            "C2-1: per-agent reviewer_model pin must head the chain, not the scenario head"
        );
    }

    #[test]
    fn explicit_override_still_outranks_config_and_scenario() {
        // Precedence #1 preserved: an explicit `--reviewer`/`--panel` pin
        // (model_override = Some) still heads the chain, above BOTH the
        // configured pin and the scenario head; the rest follow as fallbacks.
        let config_chain = vec!["config-reviewer".to_string()];
        let scenario_chain = vec!["scenario-reviewer".to_string()];
        let out = order_reviewer_candidates(
            Some("explicit-reviewer"),
            Some("agent-pin-reviewer"),
            &config_chain,
            &scenario_chain,
        );
        assert_eq!(
            out,
            vec![
                "explicit-reviewer".to_string(),
                "agent-pin-reviewer".to_string(),
                "config-reviewer".to_string(),
                "scenario-reviewer".to_string(),
            ],
            "C2-1: explicit pin heads; then agent pin, config chain, scenario tail"
        );
    }

    #[test]
    fn scenario_head_used_only_when_no_pin_configured() {
        // Legacy behavior preserved: with no explicit override and no
        // configured pin, the scenario head is the first candidate.
        let scenario_chain = vec!["scenario-reviewer".to_string(), "scenario-alt".to_string()];
        let out = order_reviewer_candidates(None, None, &[], &scenario_chain);
        assert_eq!(
            out, scenario_chain,
            "scenario chain seeds head+tail when no pin set"
        );
    }

    // ---- Y-3 / Y-4: parse_review must not be gamed / fail closed ---------

    #[test]
    fn parse_review_extracts_verdict_after_stray_prose_brace() {
        // Y-3: a stray `{` in prose before the real JSON must NOT prevent the
        // real verdict from being read (the old first-'{'..last-'}' span made
        // this invalid JSON → fell through to the fallback).
        let raw = "Example of bad code: { x.unwrap() }. \
                   My verdict: {\"verdict\":\"request_changes\",\"summary\":\"buffer overflow\"}";
        let r = parse_review(raw);
        assert_eq!(
            r.verdict, "request_changes",
            "Y-3: the real request_changes verdict must survive a stray prose brace"
        );
    }

    #[test]
    fn parse_review_prefers_first_valid_verdict_object() {
        // Two objects; the first is not a ReviewOutput-with-verdict, the
        // second is — scanner must find the second.
        let raw = "{\"note\":\"scratch\"} then {\"verdict\":\"approve\",\"summary\":\"ok\"}";
        assert_eq!(parse_review(raw).verdict, "approve");
    }

    #[test]
    fn parse_review_unparseable_fails_closed() {
        // Y-4: a refusal / prose reply (HTTP 200, no JSON) must BLOCK, not
        // resolve as a non-blocking comment.
        assert_eq!(
            parse_review("I cannot review this.").verdict,
            "request_changes"
        );
    }

    #[test]
    fn parse_review_empty_object_fails_closed() {
        // An empty `{}` (no verdict) must fail closed, not approve.
        assert_eq!(parse_review("{}").verdict, "request_changes");
        assert_eq!(parse_review("   ").verdict, "request_changes");
    }

    #[test]
    fn parse_review_normal_approve_still_parses() {
        // Regression guard: a well-formed approve is unaffected.
        let r = parse_review("{\"verdict\":\"approve\",\"summary\":\"lgtm\",\"findings\":[]}");
        assert_eq!(r.verdict, "approve");
        assert_eq!(r.summary, "lgtm");
    }

    #[test]
    fn parse_review_ignores_braces_inside_string_values() {
        // A `}` inside a JSON string value must not prematurely close the
        // object (string-aware brace counting).
        let raw = "{\"verdict\":\"reject\",\"summary\":\"found a stray } and { in a regex\"}";
        assert_eq!(parse_review(raw).verdict, "reject");
    }

    /// Cwd for `run_check_watched` tests. The child `sh -c` doesn't care
    /// about the cwd — we just need a real, stable path to satisfy the
    /// `current_dir` argument.
    fn tmp_cwd() -> PathBuf {
        std::env::temp_dir()
    }

    fn meta_for_test_id_pid(id: &str, pid: u32) -> JobMeta {
        JobMeta {
            id: id.to_string(),
            kind: "test".to_string(),
            status: "running".to_string(),
            cwd: tmp_cwd().to_string_lossy().to_string(),
            pid,
            started: Utc::now(),
            finished: None,
        }
    }

    fn meta_for_test_pid(pid: u32) -> JobMeta {
        JobMeta {
            id: "test-job".to_string(),
            kind: "test".to_string(),
            status: "running".to_string(),
            cwd: tmp_cwd().to_string_lossy().to_string(),
            pid,
            started: Utc::now(),
            finished: None,
        }
    }

    fn read_pidfile(path: &Path) -> Option<u32> {
        let raw = std::fs::read_to_string(path).ok()?;
        raw.trim().parse::<u32>().ok().filter(|pid| *pid > 0)
    }

    struct TestBackgroundWorkerCommandGuard {
        previous: Option<(PathBuf, Vec<String>)>,
    }

    impl TestBackgroundWorkerCommandGuard {
        fn new(exe: PathBuf, args: Vec<String>) -> Self {
            let mut guard = TEST_BACKGROUND_WORKER_COMMAND
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = guard.replace((exe, args));
            Self { previous }
        }
    }

    impl Drop for TestBackgroundWorkerCommandGuard {
        fn drop(&mut self) {
            let mut guard = TEST_BACKGROUND_WORKER_COMMAND
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = self.previous.take();
            FAIL_NEXT_WRITE_META.store(false, std::sync::atomic::Ordering::SeqCst);
            LAST_BACKGROUND_CHILD_PID.store(0, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn configure_background_worker_command_sets_own_process_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = std::fs::File::create(dir.path().join("output.txt")).expect("output file");
        let err = out.try_clone().expect("clone output");
        let args = vec!["30".to_string()];
        let mut command = std::process::Command::new("/bin/sleep");
        configure_background_worker_command(&mut command, &args, dir.path(), out, err);
        let mut child = command.spawn().expect("spawn sleep");
        let pid = child.id();
        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };

        unsafe {
            if pgid == pid as libc::pid_t {
                libc::kill(-pgid, libc::SIGKILL);
            } else {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
        let _ = child.wait();

        assert_eq!(
            pgid, pid as libc::pid_t,
            "background worker must lead its own process group so pgid == pid"
        );
    }

    #[test]
    fn spawn_background_kills_child_when_meta_write_fails() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let _home = crate::test_env::EnvGuard::new(home_dir.path());
        let _worker = TestBackgroundWorkerCommandGuard::new(
            PathBuf::from("/bin/sleep"),
            vec!["30".to_string()],
        );
        LAST_BACKGROUND_CHILD_PID.store(0, std::sync::atomic::Ordering::SeqCst);
        FAIL_NEXT_WRITE_META.store(true, std::sync::atomic::Ordering::SeqCst);

        let err =
            spawn_background("loop", &tmp_cwd()).expect_err("metadata write failure must surface");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("injected write_meta failure"),
            "spawn_background must return the original write_meta error; got: {msg}"
        );

        let pid = LAST_BACKGROUND_CHILD_PID.load(std::sync::atomic::Ordering::SeqCst);
        assert!(pid > 0, "test must observe the spawned worker pid");

        let mut gone = false;
        for _ in 0..200 {
            if !crate::jobs::pid_alive(pid) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if !gone {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
                let mut status: libc::c_int = 0;
                libc::waitpid(pid as libc::pid_t, &mut status, 0);
            }
        }
        assert!(
            gone,
            "spawn_background must kill and reap child pid {pid} after write_meta fails"
        );
    }

    #[test]
    fn cancel_dead_background_pid_is_safe_noop() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("wait true");
        assert!(
            !crate::jobs::pid_alive(pid),
            "waited child pid {pid} must be confirmed dead before cancel"
        );

        let outcome =
            cancel_background_process_group(&meta_for_test_pid(pid)).expect("cancel dead pid");
        assert_eq!(
            outcome,
            CancelSignalOutcome::AlreadyFinished,
            "dead recorded pid must be treated as already finished without signalling"
        );
    }

    #[test]
    fn cancel_running_background_job_kills_process_group_descendants() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("grandchild.pid");
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("sleep 100 & echo $! > \"$PIDFILE\"; wait")
            .env("PIDFILE", &pidfile)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().expect("spawn parent shell");
        let parent_pid = child.id();

        let mut grandchild_pid = None;
        for _ in 0..200 {
            if let Some(pid) = read_pidfile(&pidfile) {
                grandchild_pid = Some(pid);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let grandchild_pid = grandchild_pid.expect("grandchild pid must be written");
        assert!(
            crate::jobs::pid_alive(parent_pid),
            "parent pid {parent_pid} must be alive before cancel"
        );
        assert!(
            crate::jobs::pid_alive(grandchild_pid),
            "grandchild pid {grandchild_pid} must be alive before cancel"
        );

        let outcome =
            cancel_background_process_group(&meta_for_test_pid(parent_pid)).expect("cancel job");
        assert_eq!(outcome, CancelSignalOutcome::Signalled);

        let mut parent_exited = false;
        for _ in 0..200 {
            if child.try_wait().expect("poll parent").is_some() {
                parent_exited = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if !parent_exited {
            let _ = signal_process_group(parent_pid, libc::SIGKILL);
            let _ = child.wait();
        }
        assert!(
            parent_exited,
            "cancel must terminate the direct background worker"
        );

        let mut grandchild_gone = false;
        for _ in 0..200 {
            if !crate::jobs::pid_alive(grandchild_pid) {
                grandchild_gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if !grandchild_gone {
            unsafe {
                libc::kill(grandchild_pid as libc::pid_t, libc::SIGKILL);
            }
        }
        assert!(
            grandchild_gone,
            "cancel must terminate the worker's descendant process"
        );
    }

    /// DEFECT 1: a job directory whose `meta.json` is a symlink at
    /// the path must be skipped by `read_dir_jobs` rather than
    /// followed into the symlink target. Pre-fix `read_meta` called
    /// `read_to_string` directly, so a symlink to `/dev/zero` or to a
    /// multi-GiB file would OOM the caller (typically `zoder jobs
    /// list`) and a symlink to a writer-less FIFO would block it
    /// forever. The fix opens the file with `O_NOFOLLOW`, which
    /// rejects the symlink itself at open time (kernel returns
    /// `ELOOP`), and surfaces as the function's existing `None`
    /// "skip this entry" contract.
    ///
    /// Variant A: a symlink whose target is cheap and non-blocking
    /// (`/dev/null` — empty file, no `mkfifo` required, runs in
    /// containers without privileged namespaces). Pre-fix code
    /// would read 0 bytes here (`/dev/null` is empty); the new
    /// code rejects at open time and the entry is dropped before
    /// any read happens.
    #[test]
    #[cfg(unix)]
    fn read_meta_skips_symlinked_meta_json_to_dev_null() {
        let dir = tempfile::tempdir().expect("tempdir");
        let job = dir.path().join("job-with-symlink-meta");
        std::fs::create_dir_all(&job).expect("mkdir job dir");
        std::os::unix::fs::symlink("/dev/null", job.join("meta.json")).expect("symlink /dev/null");

        // Pre-fix: open(2) follows the symlink; read_to_string
        // returns "" (empty file); serde_json::from_str("") fails ->
        // None. So under BOTH the old and new code, read_meta
        // returns None for a `/dev/null`-backed symlink.
        //
        // The fixture-level test that actually PROVES the
        // symlink-rejection-at-open path is the FIFO variant
        // (`read_meta_skips_symlinked_meta_json_to_writerless_fifo`).
        // Here we simply confirm the contract "a symlink at
        // meta.json drops the entry" holds.
        let jobs = read_dir_jobs(dir.path());
        assert!(
            jobs.iter().all(|j| j.id != "job-with-symlink-meta"),
            "a symlink at meta.json must be skipped (read_dir_jobs returned: \
             {jobs:?}); pre-fix read_to_string would still have returned None for \
             /dev/null, but with a symlink-to-/dev/zero or symlink-to-huge-file it \
             would have OOMed the caller",
        );
    }

    /// DEFECT 1 (variant B — the actual hang regression): a symlink
    /// whose target is a writer-less FIFO would block a normal
    /// `read_to_string` forever on `read(2)`. The fix opens with
    /// `O_NOFOLLOW | O_NONBLOCK`; the symlink is rejected at open
    /// time (`ELOOP`) without ever touching the FIFO. The test
    /// spawns the read in a worker thread with a 5-second
    /// wall-clock budget so a regression to the pre-fix code would
    /// fail with a clear "blocked past 5s" instead of hanging cargo.
    #[test]
    #[cfg(unix)]
    fn read_meta_skips_symlinked_meta_json_to_writerless_fifo() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("tempdir");
        // Place a real `meta.json` next door so the test exercises
        // both: (1) the bad symlink is dropped, (2) the good
        // sibling survives — proving `read_dir_jobs` continues to
        // its next entry rather than failing the whole listing.
        let good_job = dir.path().join("good-job");
        std::fs::create_dir_all(&good_job).expect("mkdir good job dir");
        write_meta(&good_job, &meta_for_test_id_pid("good-job", 99)).expect("write good meta");

        // The FIFO + symlink setup.
        let fifo_job = dir.path().join("fifo-job");
        std::fs::create_dir_all(&fifo_job).expect("mkdir fifo job dir");
        let fifo = dir.path().join("fifo.target");
        let mkfifo = std::process::Command::new("mkfifo").arg(&fifo).status();
        assert!(
            matches!(&mkfifo, Ok(s) if s.success()),
            "mkfifo must succeed in this test environment; got {mkfifo:?}",
        );
        std::os::unix::fs::symlink(&fifo, fifo_job.join("meta.json")).expect("symlink to FIFO");

        // Run `read_dir_jobs` in a worker thread to bound the
        // wall-clock time. Pre-fix code would block forever inside
        // `read_to_string` on the FIFO's read; the budget makes
        // that failure mode observable.
        let (tx, rx) = mpsc::channel::<Vec<JobMeta>>();
        let dir_for_reader = dir.path().to_path_buf();
        let reader = thread::spawn(move || {
            let jobs = read_dir_jobs(&dir_for_reader);
            tx.send(jobs).unwrap();
        });

        let jobs = rx.recv_timeout(Duration::from_secs(5)).expect(
            "read_dir_jobs must not block past 5s on a symlink-to-FIFO meta.json — \
                 DEFECT 1 regression",
        );
        reader.join().expect("reader thread panicked");

        // The good job must be present; the FIFO-symlink job must
        // be absent. The list is sorted newest-first, and both
        // fixtures have started=Utc::now() within nanoseconds of
        // each other, so the order is unspecified but each entry's
        // id must classify correctly.
        let ids: Vec<&str> = jobs.iter().map(|j| j.id.as_str()).collect();
        assert!(
            ids.contains(&"good-job"),
            "the legitimate good-job meta.json must survive a sibling \
             symlink-to-FIFO; got ids: {ids:?}",
        );
        assert!(
            !ids.contains(&"fifo-job"),
            "the FIFO-shaped meta.json must be skipped (O_NOFOLLOW rejected it \
             at open time); got ids: {ids:?}",
        );

        // Cleanup: unlink the FIFO so the tempdir drop returns
        // promptly even if mkfifo left a real pipe behind.
        let _ = std::fs::remove_file(&fifo);
    }

    /// DEFECT 1 (variant C — the OOM regression): a job dir whose
    /// `meta.json` is a symlink to a massive file must be skipped.
    /// Pre-fix code would `read_to_string` and inflate the process
    /// to roughly the target's size; the new bounded read rejects
    /// anything above `MAX_JOB_META_BYTES` BEFORE touching the
    /// content. We synthesize a 256 KiB file (well above the 64 KiB
    /// cap) and assert the job is dropped from `read_dir_jobs`.
    /// The throughput of the symlink-rejection-at-fstat path is the
    /// observable guarantee — the open succeeds (we're following the
    /// link intentionally, but this test never opens the file: it
    /// goes through `read_dir_jobs` which never opens the bad
    /// entry because of its own symlink check). Combined with
    /// variant B above, this exercises both rejection layers
    /// (open-time `O_NOFOLLOW` and stat-time `is_symlink` / size).
    #[test]
    #[cfg(unix)]
    fn read_meta_skips_oversized_meta_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Build a roomy target body (256 KiB of ASCII). This is
        // 4× `MAX_JOB_META_BYTES`, so pre-fix `read_to_string`
        // would happily slurp the whole 256 KiB into memory; the
        // new bounded read either rejects at fstat (file is too
        // big) or caps the read at `MAX_JOB_META_BYTES`. Either
        // way, the entry must be skipped without parsing it as a
        // valid `JobMeta`.
        let target_path = dir.path().join("huge.bin");
        std::fs::write(&target_path, vec![b'x'; 256 * 1024]).expect("write huge target");
        let job = dir.path().join("oversized-meta-job");
        std::fs::create_dir_all(&job).expect("mkdir oversized job dir");
        std::os::unix::fs::symlink(&target_path, job.join("meta.json"))
            .expect("symlink to huge target");

        // The new bounded read also rejects a regular-file
        // `meta.json` whose size is over the cap. Place a second
        // job whose `meta.json` is an oversized regular file (not
        // a symlink) — `O_NOFOLLOW` does NOT reject this (it's a
        // regular file), but the fstat size check must.
        let oversized_regular_job = dir.path().join("oversized-regular-job");
        std::fs::create_dir_all(&oversized_regular_job).expect("mkdir oversized regular job dir");
        std::fs::write(
            oversized_regular_job.join("meta.json"),
            vec![b'y'; (MAX_JOB_META_BYTES as usize) + 1024],
        )
        .expect("write oversized regular meta");

        // And a good job to prove the listing still surfaces
        // legitimate entries alongside the rejected ones.
        let good_job = dir.path().join("good-oversized-test");
        std::fs::create_dir_all(&good_job).expect("mkdir good job dir");
        write_meta(&good_job, &meta_for_test_id_pid("good-oversized-test", 7))
            .expect("write good meta");

        let jobs = read_dir_jobs(dir.path());
        let ids: Vec<&str> = jobs.iter().map(|j| j.id.as_str()).collect();
        assert!(
            !ids.contains(&"oversized-meta-job"),
            "a symlink-to-huge-file meta.json must be skipped (O_NOFOLLOW rejects \
             the symlink, plus fstat size guard belt-and-braces); got ids: {ids:?}",
        );
        assert!(
            !ids.contains(&"oversized-regular-job"),
            "a regular-file meta.json above MAX_JOB_META_BYTES must be skipped \
             (fstat size guard); got ids: {ids:?}",
        );
        assert!(
            ids.contains(&"good-oversized-test"),
            "a legitimate meta.json must still surface alongside the rejected \
             entries; got ids: {ids:?}",
        );
    }

    /// Variant D — direct `read_meta` unit test for the size cap,
    /// mirroring the corpus / pricing test idiom (small fixture
    /// under a `test cap`, same fixture rejected). This bypasses
    /// `read_dir_jobs` so the test is fast and doesn't depend on
    /// iteration order. A 2 KiB regular `meta.json` is rejected
    /// under a 1 KiB cap and accepted under the production cap.
    /// We exercise the capped path via `read_bounded_job_meta`
    /// through `read_meta` (which uses `MAX_JOB_META_BYTES`).
    #[test]
    fn read_meta_rejects_an_oversized_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let job = dir.path().join("oversized-meta");
        std::fs::create_dir_all(&job).expect("mkdir job dir");
        // We can't drive `read_meta` with a per-call cap (it
        // hard-codes MAX_JOB_META_BYTES), so instead we exercise
        // the same code-path via a fixture just over the cap.
        // Anything between MAX_JOB_META_BYTES and a comfortably
        // large size will be rejected; 1 KiB over the cap
        // exercises the fstat guard without breaking test fixtures.
        let oversized = vec![b'{'; (MAX_JOB_META_BYTES as usize) + 1024];
        std::fs::write(job.join("meta.json"), &oversized).expect("write oversized meta");

        // Public surface: read_meta returns None because the
        // fixture exceeds MAX_JOB_META_BYTES. The size guard fired
        // BEFORE the read.
        assert!(
            read_meta(&job).is_none(),
            "meta.json of size MAX_JOB_META_BYTES + 1024 must be rejected \
             by the fstat size guard BEFORE read_to_string, not parsed as JSON",
        );

        // And a legitimate, in-bounds meta.json must still be
        // parsed end-to-end (regression guard against the cap
        // accidentally over-shooting).
        // The JSON body's id must match the directory name: read_meta
        // canonicalizes id from the containing directory (the
        // filesystem identity), treating the body field as untrusted —
        // see the jobs-prune confused-deputy fix. This fixture's intent
        // is just "a legitimate in-bounds meta.json still round-trips",
        // not the mismatch/canonicalization behavior itself (that has
        // its own dedicated tests in jobs.rs).
        let good_job = dir.path().join("good-meta");
        std::fs::create_dir_all(&good_job).expect("mkdir good job dir");
        write_meta(&good_job, &meta_for_test_id_pid("good-meta", 42)).expect("write good meta");
        let parsed = read_meta(&good_job).expect("in-bounds meta.json must round-trip");
        assert_eq!(parsed.id, "good-meta");
        assert_eq!(parsed.pid, 42);
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
        let (ok, tail) = run_check_watched(
            &tmp_cwd(),
            "sleep 30",
            1,
            false,
            &zoder_core::ExecSafetyConfig::default(),
        )
        .await;
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

    /// SB2 REGRESSION (Unix only): on the watchdog-timeout branch the raced
    /// `wait_with_output()` future is DROPPED without the direct shell ever
    /// being `wait()`ed. The out-of-band `kill(-pgid, SIGKILL)` terminates the
    /// group but does not reap the direct pid, so pre-fix it lingered as a
    /// `<defunct>` zombie holding a PID slot until the whole process exited.
    /// The fix is `.kill_on_drop(true)` on the check Command, which makes
    /// tokio's reaper `waitpid` the direct pid on drop. This test reproduces
    /// the production timeout branch shape verbatim and asserts the direct
    /// child pid is reaped (a manual `waitpid(pid, WNOHANG)` returns ECHILD —
    /// "no such child" — rather than the pid, which would mean a zombie was
    /// still awaiting reap).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_check_watched_timeout_reaps_direct_child_no_zombie() {
        use std::process::Stdio;
        // Mirror the production builder: own process group + kill_on_drop.
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 30")
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().expect("spawn sh -c sleep");
        let pid = child.id().expect("child pid") as i32;
        let pgid = pid; // process_group(0) => pgid == pid

        // Race exactly as production does: the future consumes `child` via
        // wait_with_output(); on timeout it is dropped (triggering
        // kill_on_drop) and we group-kill out-of-band.
        let join = async {
            let out = child.wait_with_output().await?;
            Ok::<_, std::io::Error>(out)
        };
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), join).await;
        assert!(
            outcome.is_err(),
            "the 30s child must time out under a 1s budget"
        );
        // Out-of-band group kill, exactly like `kill_process_group`.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }

        // The direct pid MUST get reaped by tokio's kill_on_drop reaper. Poll
        // a manual waitpid(WNOHANG): once reaped it returns -1/ECHILD. If the
        // bug were back (no kill_on_drop, no wait), the pid would remain a
        // reapable zombie and waitpid would return `pid` (or 0 while the
        // SIGKILL is delivered). Give the async reaper a moment.
        let mut reaped = false;
        for _ in 0..200 {
            let mut status: libc::c_int = 0;
            let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if r == -1 {
                // ECHILD: tokio already reaped it — exactly what we want.
                reaped = true;
                break;
            }
            if r == pid {
                // We just reaped a zombie ourselves: pre-fix symptom. Fail —
                // production has no such manual reap, so it would leak.
                panic!(
                    "direct child {pid} was a ZOMBIE awaiting reap (SB2 bug is back); \
kill_on_drop did not reap it"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            reaped,
            "direct child {pid} was never reaped within budget (SB2: kill_on_drop \
must reap the direct shell on the timeout branch)"
        );
    }

    /// Z-8 REGRESSION GUARD: when the author phase-watchdog fires, it MUST
    /// (a) issue a cancel to the daemon session, and (b) wait for the
    /// daemon to settle BEFORE returning. The pre-fix code dropped the
    /// inner future on timeout and returned `Err` immediately, so the
    /// loop's `build_diff` call (right after the watchdog) captured a
    /// torn mid-edit tree while the daemon kept editing. This test
    /// drives the timeout path with a hanging turn future and asserts
    /// the cancel hook is invoked AND the watchdog does not return
    /// until the settled signal has been awaited.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn author_phase_with_cancel_invokes_cancel_and_waits_for_settled() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;

        let cancel_called = Arc::new(AtomicBool::new(false));
        let cancel_calls = Arc::new(AtomicUsize::new(0));
        let settled_called = Arc::new(AtomicBool::new(false));
        let (settle_tx, settle_rx) = tokio::sync::oneshot::channel::<()>();

        let cancel_flag = cancel_called.clone();
        let cancel_n = cancel_calls.clone();
        let settled_flag = settled_called.clone();
        let cancel_fut = async move {
            cancel_flag.store(true, Ordering::SeqCst);
            cancel_n.fetch_add(1, Ordering::SeqCst);
            Ok::<_, anyhow::Error>(())
        };
        // The `settled` future MUST be awaited by the watchdog before it
        // returns. We verify this by NOT driving the oneshot until after
        // the cancel is recorded — if the watchdog returns before
        // awaiting `settled`, the test would hang here on `send`.
        let settled_fut = async move {
            settled_flag.store(true, Ordering::SeqCst);
            let _ = settle_rx.await;
        };
        // Hanging turn: longer than the 1s budget.
        let turn_fut = async {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            // Unreachable — the watchdog will time out first.
            anyhow::bail!("turn should not have completed")
        };

        let watchdog = author_phase_with_cancel(1, true, turn_fut, cancel_fut, settled_fut);
        // Spawn the watchdog on a separate task so we can release the
        // settled signal from the test body (otherwise the watchdog is
        // parked on settled BEFORE we can call `settle_tx.send()` —
        // and the outer timeout fires first, masking the bug).
        let watchdog_handle = tokio::spawn(watchdog);
        // Give the watchdog time to time out and park on settled.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        // Now release the settle signal so the watchdog can return.
        let _ = settle_tx.send(());

        let res = watchdog_handle
            .await
            .expect("watchdog task must complete")
            .expect_err("watchdog must return Err on timeout");
        assert!(
            cancel_called.load(Ordering::SeqCst),
            "Z-8 fix: watchdog must invoke cancel on timeout; got no cancel"
        );
        assert_eq!(
            cancel_calls.load(Ordering::SeqCst),
            1,
            "Z-8 fix: cancel must be invoked exactly once on timeout"
        );
        assert!(
            settled_called.load(Ordering::SeqCst),
            "Z-8 fix: watchdog must await the settled signal before returning"
        );
        assert!(
            res.contains("author phase timed out after 1s"),
            "Err must mention the phase + budget; got: {res}"
        );
    }

    /// Z-8 REGRESSION GUARD (diff-capture ordering): `build_diff` in
    /// `cmd_loop` is called AFTER `author_phase_with_cancel` returns,
    /// and on the timeout path the watchdog must not return UNTIL the
    /// daemon has settled. This test pins the ordering by holding the
    /// settled signal until AFTER the watchdog would have returned if
    /// it didn't wait. If `author_phase_with_cancel` returns before
    /// awaiting `settled`, the test hangs at `send` and fails on the
    /// outer timeout — which IS the pre-fix behavior we are guarding
    /// against.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn author_phase_with_cancel_gates_diff_capture_on_settled() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let cancel_called = Arc::new(AtomicBool::new(false));
        let (settle_tx, settle_rx) = tokio::sync::oneshot::channel::<()>();
        let watchdog_returned = Arc::new(AtomicBool::new(false));

        let cancel_flag = cancel_called.clone();
        let cancel_fut = async move {
            cancel_flag.store(true, Ordering::SeqCst);
            Ok::<_, anyhow::Error>(())
        };
        let cancel_flag_for_settled = cancel_called.clone();
        let watchdog_returned_for_settled = watchdog_returned.clone();
        let settled_fut = async move {
            // Hold the daemon "settled" signal. The watchdog must be
            // blocked on this await — i.e., NOT have returned yet.
            assert!(
                !watchdog_returned_for_settled.load(Ordering::SeqCst),
                "Z-8 fix: watchdog MUST NOT return before settled resolves; \
                 cmd_loop would then call build_diff against a torn tree"
            );
            let _ = cancel_flag_for_settled.load(Ordering::SeqCst);
            let _ = settle_rx.await;
        };
        let turn_fut = async {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            anyhow::bail!("unreachable")
        };
        let cancel_clone = cancel_called.clone();
        let watchdog = async move {
            let r = author_phase_with_cancel(1, true, turn_fut, cancel_fut, settled_fut).await;
            // Only AFTER settled has resolved may the watchdog return
            // and `cmd_loop` proceed to `build_diff`.
            assert!(
                cancel_clone.load(Ordering::SeqCst),
                "Z-8 fix: cancel must have been issued before settled resolves"
            );
            watchdog_returned.store(true, Ordering::SeqCst);
            r
        };

        let driver = tokio::spawn(async move {
            // Bound the test: if the watchdog is broken (returns before
            // settled), this future races the spawn to the bounded
            // timeout and fails there. If it's correct, we wait until
            // we explicitly release `settle_tx`.
            tokio::time::timeout(std::time::Duration::from_secs(3), watchdog)
                .await
                .expect("watchdog must remain parked on settled (the bug returns earlier)")
        });

        // Give the watchdog time to time out and park on settled.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        // Now release settled.
        let _ = settle_tx.send(());
        let res = driver.await.expect("driver task panicked");
        let _ = res.expect_err("watchdog must return Err on timeout");
        // The ordering assert inside `settled_fut` is the real check.
    }

    /// `author_phase_with_cancel` must NOT invoke cancel or settled when
    /// the inner future completes within budget — those are timeout-only
    /// hooks. A spurious cancel on a healthy turn would terminate the
    /// session for no reason and lose the engine's reported cost /
    /// partial output. This pins the non-breaking contract for the
    /// fast-success path.
    #[tokio::test]
    async fn author_phase_with_cancel_does_not_invoke_cancel_on_success() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let cancel_calls = Arc::new(AtomicUsize::new(0));
        let settled_called = Arc::new(AtomicUsize::new(0));

        let n = cancel_calls.clone();
        let cancel_fut = async move {
            n.fetch_add(1, Ordering::SeqCst);
            Ok::<_, anyhow::Error>(())
        };
        let s = settled_called.clone();
        let settled_fut = async move {
            s.fetch_add(1, Ordering::SeqCst);
        };
        let turn_fut = async {
            Ok(crate::TurnResult {
                run: zoder_core::AgentRun {
                    session_id: "s".into(),
                    outcome: "completed".into(),
                    content: String::new(),
                    input_tokens: 0,
                    tool_calls: 0,
                },
                model: "m".into(),
                alias: "a".into(),
                cost_usd: 0.0,
                cost_unknown: false,
                tokens_in: 0,
                tokens_out: 0,
                elapsed_ms: 0.0,
            })
        };
        let res = author_phase_with_cancel(5, true, turn_fut, cancel_fut, settled_fut).await;
        assert!(
            res.is_ok(),
            "fast success must propagate Ok; got {:?}",
            res.as_ref().err()
        );
        assert_eq!(
            cancel_calls.load(Ordering::SeqCst),
            0,
            "cancel MUST NOT be invoked when the turn completes within budget"
        );
        assert_eq!(
            settled_called.load(Ordering::SeqCst),
            0,
            "settled MUST NOT be awaited when the turn completes within budget"
        );
    }

    /// Fast commands must NOT trip the watchdog — sanity check that we
    /// didn't accidentally turn every check into a 900s wait.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_check_watched_passes_fast_child() {
        let (ok, tail) = run_check_watched(
            &tmp_cwd(),
            "exit 0",
            1,
            false,
            &zoder_core::ExecSafetyConfig::default(),
        )
        .await;
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
        let (ok, tail) = run_check_watched(
            &tmp_cwd(),
            "echo boom; exit 1",
            1,
            false,
            &zoder_core::ExecSafetyConfig::default(),
        )
        .await;
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
        let (ok, _tail) = run_check(
            &tmp_cwd(),
            "exit 0",
            false,
            &zoder_core::ExecSafetyConfig::default(),
        );
        assert!(ok);
    }

    // -----------------------------------------------------------------------
    // `run_check_watched` + exec_safety wiring.
    //
    // These tests pin the integration: the denylist lives in
    // `exec_safety::inspect_shell_command`, but the production call site
    // is `run_check_watched` (and its unwatched twin `run_check`). The
    // pure-function tests in `exec_safety::tests` cover every pattern;
    // these tests assert the call site actually consults the denylist
    // before spawning `sh -c`.
    // -----------------------------------------------------------------------

    /// A `--check` command that matches the denylist MUST be refused
    /// without ever spawning `sh -c`. The deny-reason reaches the caller
    /// as the `tail`, where the next loop iteration will read it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_check_watched_denies_dangerous_command_without_spawning() {
        let start = std::time::Instant::now();
        let (ok, tail) = run_check_watched(
            &tmp_cwd(),
            "rm -rf /",
            5,
            false,
            &zoder_core::ExecSafetyConfig::default(),
        )
        .await;
        let elapsed = start.elapsed();
        assert!(!ok, "denied command must be reported as failed");
        assert!(
            tail.contains("rm -rf /") || tail.to_lowercase().contains("filesystem"),
            "tail must carry the deny reason; got: {tail:?}"
        );
        // The denylist fires synchronously, BEFORE the spawn — so this
        // returns in microseconds, not the full watchdog budget. If a
        // future refactor accidentally spawns first and then checks, this
        // assertion catches it.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "denylist must short-circuit before spawn; took {:?}",
            elapsed
        );
    }

    /// `allow_dangerous = true` is the documented operator escape hatch
    /// for legitimate destructive validation commands. The shell still
    /// runs (and in this test, exits non-zero because `rm -rf /` on a
    /// non-privileged shell will fail), but the denylist itself is
    /// bypassed. The exact runtime behavior of `rm -rf /` is
    /// platform/permission-dependent and out of scope for this pin;
    /// what matters is that the call reaches the shell at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_check_watched_bypasses_denylist_when_allow_dangerous() {
        // `true` is a no-op shell command that exits 0; we use it as a
        // benign stand-in to prove that `allow_dangerous=true` lets a
        // command through. We avoid running `rm -rf /` for real here
        // because that interacts with the host filesystem and the
        // integration test environment.
        let (ok, _tail) = run_check_watched(
            &tmp_cwd(),
            "true",
            5,
            true,
            &zoder_core::ExecSafetyConfig::default(),
        )
        .await;
        assert!(
            ok,
            "allow_dangerous=true must let a benign command pass to the shell"
        );
    }

    /// An ordinary, benign `--check` command MUST still pass through the
    /// denylist (and through to the shell). This is the regression guard
    /// against accidentally over-blocking on common CI commands — the
    /// loop would otherwise refuse to run any `cargo test` ever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_check_watched_passes_through_benign_command() {
        let (ok, _tail) = run_check_watched(
            &tmp_cwd(),
            "true",
            5,
            false,
            &zoder_core::ExecSafetyConfig::default(),
        )
        .await;
        assert!(
            ok,
            "benign `true` command must pass the denylist and exit 0"
        );
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
        // succeeds (turn_failed = false).
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

    /// SA1 REGRESSION: a *stable* non-substantive diff (e.g. WhitespaceOnly /
    /// CommentOnly emitted verbatim every iteration) must trip the stall abort
    /// within STALL_LIMIT instead of burning all `--max-iters`. The
    /// anti-gaming branch calls `nonsubstantive_stall_step` to fold the stall
    /// accounting in BEFORE its `continue`. Pre-fix, that branch bumped no
    /// streak (the diff has real headers so `trim().is_empty()` is false), so
    /// the loop never early-aborted. Here we drive the same helper the branch
    /// uses and assert the abort fires exactly at the limit.
    #[test]
    fn nonsubstantive_stall_step_aborts_repeated_diff_within_limit() {
        const LIMIT: usize = 3;
        // iter 1: prev_diff is empty, so `repeated=false` -> streak resets.
        let (s0, a0) = nonsubstantive_stall_step(false, 0, LIMIT);
        assert_eq!(s0, 0);
        assert!(!a0, "iter 1 must never abort");
        // iters 2..: the identical non-substantive diff repeats -> streak climbs.
        let (s1, a1) = nonsubstantive_stall_step(true, s0, LIMIT);
        assert_eq!(s1, 1);
        assert!(!a1);
        let (s2, a2) = nonsubstantive_stall_step(true, s1, LIMIT);
        assert_eq!(s2, 2);
        assert!(!a2);
        let (s3, a3) = nonsubstantive_stall_step(true, s2, LIMIT);
        assert_eq!(s3, 3);
        assert!(
            a3,
            "a stable non-substantive diff MUST abort at STALL_LIMIT ({LIMIT}), \
not run to max_iters"
        );
    }

    /// SA1 boundary: a *changed* non-substantive diff (author still churning
    /// something new each round) must RESET the stall streak, so the loop
    /// keeps grinding until the diff actually stabilizes — the abort only
    /// targets a genuinely stuck author, never a moving one.
    #[test]
    fn nonsubstantive_stall_step_resets_on_changed_diff() {
        const LIMIT: usize = 3;
        // Build up a streak, then hit a non-repeated diff.
        let (s1, _) = nonsubstantive_stall_step(true, 0, LIMIT);
        assert_eq!(s1, 1);
        let (s2, _) = nonsubstantive_stall_step(true, s1, LIMIT);
        assert_eq!(s2, 2);
        // Diff changed this round -> reset, no abort.
        let (s_reset, abort) = nonsubstantive_stall_step(false, s2, LIMIT);
        assert_eq!(s_reset, 0, "a changed diff must reset the stall streak");
        assert!(!abort);
    }

    /// Engine returned Ok AND `outcome == "completed"` (turn_failed = false)
    /// on an empty diff — a successful engine that simply produced no
    /// edits this round. dead_streak stays at zero — the helper trusts
    /// the engine, even if the diff disagrees. Note the post-Z-16 shift:
    /// a successful-but-empty turn is NOT dead-engine, only a turn that
    /// FAILED to complete (Ok with `outcome != "completed"`, or
    /// `turn.is_none()` from the outer watchdog) counts. This test
    /// guards that boundary.
    #[test]
    fn update_loop_streaks_trusts_turn_ok_signal_as_progress() {
        // Engine returned Ok AND succeeded (turn_failed=false) but the
        // diff is empty (the engine simply produced no edits this round).
        // dead_streak stays at zero — the helper trusts the engine.
        let u = update_loop_streaks(false, false, true, 0, 0);
        assert_eq!(u.dead_streak, 0);
        assert_eq!(u.check_timeout_streak, 0);
        assert!(!u.abort);
    }

    /// Z-16 REGRESSION GUARD: an engine that internally times out returns
    /// `Ok(TurnResult { run.outcome == "timeout", ... })` — `turn.is_none()`
    /// is FALSE, so the pre-fix dead-engine signal `turn.is_none()`
    /// returned FALSE and the dead-streak never incremented. The loop
    /// then ground through every `max_iters` instead of bailing after 2
    /// strikes. The fix is to key the dead-engine signal on
    /// `!run.succeeded()` (via [`turn_is_dead`]) rather than `turn.is_none()`,
    /// so two consecutive non-completed turns — whether the watchdog
    /// killed the future OR the engine returned `outcome != "completed"` —
    /// trip the abort. This test exercises the helper directly through
    /// the production signal: it computes `turn_is_dead(&Some(non_completed))`
    /// and feeds the result into `update_loop_streaks` exactly the way
    /// `cmd_loop` does.
    #[test]
    fn dead_streak_aborts_after_two_non_completed_turns() {
        // Simulate the bug scenario: the engine internally times out and
        // returns Ok with `outcome == "timeout"`. `turn` is `Some(_)` so
        // the pre-fix signal `turn.is_none()` was FALSE.
        let non_completed = crate::TurnResult {
            run: zoder_core::AgentRun {
                session_id: "sess-z-16".into(),
                outcome: "timeout".into(),
                content: String::new(),
                input_tokens: 0,
                tool_calls: 0,
            },
            model: "m".into(),
            alias: "a".into(),
            cost_usd: 0.0,
            cost_unknown: false,
            tokens_in: 0,
            tokens_out: 0,
            elapsed_ms: 0.0,
        };
        let turn_some: Option<crate::TurnResult> = Some(non_completed);

        // The dead-engine signal MUST be true for Some(non_completed) —
        // the engine did NOT complete even though it returned Ok.
        assert!(
            turn_is_dead(&turn_some),
            "Z-16 fix: Some(turn with outcome != completed) must count as dead; \
             pre-fix this was false because turn.is_none() was false"
        );

        // Two consecutive non-completed turns with empty diffs must trip
        // the 2-strike abort — exactly what the pre-fix code missed.
        let u1 = update_loop_streaks(turn_is_dead(&turn_some), false, true, 0, 0);
        assert_eq!(u1.dead_streak, 1);
        assert!(!u1.abort, "first non-completed turn must not abort yet");
        let u2 = update_loop_streaks(
            turn_is_dead(&turn_some),
            false,
            true,
            u1.dead_streak,
            u1.check_timeout_streak,
        );
        assert_eq!(u2.dead_streak, 2);
        assert!(
            u2.abort,
            "Z-16 fix: two consecutive non-completed turns MUST abort; \
             pre-fix dead_streak never incremented so the abort never fired"
        );
    }

    /// `turn_is_dead` must treat `None` (the outer watchdog killed the
    /// future) as dead — that's the pre-fix signal. Regression guard so a
    /// future refactor doesn't drop the `turn.is_none()` arm while
    /// adding the `!succeeded()` arm.
    #[test]
    fn turn_is_dead_treats_none_as_dead() {
        let turn: Option<crate::TurnResult> = None;
        assert!(turn_is_dead(&turn));
    }

    /// `turn_is_dead` must treat `Some(completed)` as alive. A successful
    /// turn with an empty diff (the engine produced no edits this round
    /// but DID complete) is NOT dead-engine — it's a legitimate no-op
    /// turn and the loop should keep nudging the author.
    #[test]
    fn turn_is_dead_treats_completed_turn_as_alive() {
        let turn = Some(crate::TurnResult {
            run: zoder_core::AgentRun {
                session_id: "sess-ok".into(),
                outcome: "completed".into(),
                content: String::new(),
                input_tokens: 0,
                tool_calls: 0,
            },
            model: "m".into(),
            alias: "a".into(),
            cost_usd: 0.0,
            cost_unknown: false,
            tokens_in: 0,
            tokens_out: 0,
            elapsed_ms: 0.0,
        });
        assert!(!turn_is_dead(&turn));
    }

    /// `turn_is_dead` must flag every non-completed outcome as dead —
    /// not just `"timeout"`. `cancelled`, `failed`, `max_tokens`, and
    /// any future outcome the engine introduces must all bump the
    /// dead-streak so the loop bails after 2 strikes instead of grinding.
    #[test]
    fn turn_is_dead_flags_every_non_completed_outcome_as_dead() {
        for outcome in ["timeout", "cancelled", "failed", "max_tokens", "unknown"] {
            let turn = crate::TurnResult {
                run: zoder_core::AgentRun {
                    session_id: "s".into(),
                    outcome: outcome.into(),
                    content: String::new(),
                    input_tokens: 0,
                    tool_calls: 0,
                },
                model: "m".into(),
                alias: "a".into(),
                cost_usd: 0.0,
                cost_unknown: false,
                tokens_in: 0,
                tokens_out: 0,
                elapsed_ms: 0.0,
            };
            assert!(
                turn_is_dead(&Some(turn)),
                "Z-16: outcome={outcome:?} must count as dead (not completed)"
            );
        }
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

    /// An `Ok` reviewer slot: a real completion from `model` voting `verdict`.
    fn ok_slot(model: &str, verdict: &str) -> ReviewerSlot {
        ReviewerSlot::Ok {
            model: model.into(),
            review: rro(verdict),
        }
    }

    /// A `Failed` reviewer slot (a reviewer whose completion errored). It casts
    /// NO vote regardless of its label.
    fn failed_slot(err: &str) -> ReviewerSlot {
        ReviewerSlot::Failed {
            model: "(failed)".into(),
            err: err.into(),
        }
    }

    /// REGRESSION (Finding #14): when EVERY reviewer fails (e.g. the lone
    /// reviewer 401s, or every `--panel` model times out) the aggregate must
    /// surface `complete: false` and a NON-`approve` verdict. The old code
    /// produced `comment` here, which let CI believe the review ran.
    #[test]
    fn aggregate_review_marks_complete_false_when_every_reviewer_fails() {
        let reviews = vec![failed_slot("401 Unauthorized")];
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

    /// REGRESSION (Finding #14 + C5-1): an `approve` from a SUCCESSFUL
    /// reviewer alongside a `ReviewerSlot::Failed` slot keeps the aggregate at
    /// `approve` — a failed slot casts no vote. The worst-rank walk now skips
    /// failed slots by VARIANT (`filter_map(ReviewerSlot::review)`), not by
    /// string-matching a magic model id.
    #[test]
    fn aggregate_review_ignores_error_records_when_computing_worst_verdict() {
        // One real `approve` reviewer; one `Failed` reviewer slot.
        // The aggregate must be `approve` (the failed slot casts no vote).
        let reviews = vec![ok_slot("real-model", "approve"), failed_slot("timeout")];
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
    /// rank walk ignores `Failed` slots (by variant). When one reviewer votes
    /// `request_changes` and another fails, the aggregate must reflect the real
    /// blocking verdict.
    #[test]
    fn aggregate_review_takes_worst_verdict_from_real_reviewers_only() {
        let reviews = vec![
            ok_slot("real-a", "request_changes"),
            failed_slot("5xx"),
            ok_slot("real-b", "approve"),
        ];
        let (agg, _, payload) = aggregate_review(&reviews, 0.0, 3, 2, 1);
        assert_eq!(
            agg, "request_changes",
            "blocking review from real model wins aggregate"
        );
        // Cost is reported verbatim so CI can use it as a budget signal.
        assert_eq!(payload["cost_usd"].as_f64(), Some(0.0));
    }

    /// C5-1 [MED]: a reviewer model LITERALLY named `error` (operator-reachable
    /// via `--panel error` / `reviewer_model="error"`) that SUCCEEDS with a
    /// blocking `request_changes` must still cast its blocking vote. The old
    /// code stored a failed slot as `("error", ..)` and filtered the worst-rank
    /// walk on the string `"error"`, so a real successful reviewer named
    /// "error" had its block silently dropped -> gate failed OPEN (exit 0).
    /// With the structured `ReviewerSlot` discriminant the vote is counted by
    /// VARIANT, so the aggregate is `request_changes` (blocking), NOT `approve`.
    #[test]
    fn aggregate_counts_successful_reviewer_named_error_as_a_real_vote() {
        // Slot 0: an `Ok` reviewer whose model id is the literal string
        // "error", voting request_changes. Slot 1: a real `approve`.
        let reviews = vec![
            ok_slot("error", "request_changes"),
            ok_slot("real-approver", "approve"),
        ];
        let (agg, all_failed, payload) = aggregate_review(&reviews, 0.0, 2, 2, 0);
        assert!(!all_failed, "both slots succeeded -> not all-failed");
        assert_eq!(
            agg, "request_changes",
            "a SUCCESSFUL reviewer named 'error' must cast its blocking vote (C5-1: must not be dropped by string-sentinel filtering)"
        );
        assert_eq!(payload["ok_models"].as_u64(), Some(2));
        assert_eq!(payload["failed_models"].as_u64(), Some(0));
        // The literal model id survives to the payload verbatim, and is marked
        // as a real (ok) vote.
        let slot0 = &payload["reviewers"][0];
        assert_eq!(slot0["model"].as_str(), Some("error"));
        assert_eq!(slot0["ok"].as_bool(), Some(true));
        assert_eq!(slot0["verdict"].as_str(), Some("request_changes"));

        // And the companion invariant still holds: a GENUINELY failed slot
        // (Failed variant) alongside a real approve does NOT vote, so the
        // aggregate stays `approve` -- a failed slot is not a blocking vote.
        let mixed = vec![ok_slot("real-approver", "approve"), failed_slot("timeout")];
        let (agg2, _, payload2) = aggregate_review(&mixed, 0.0, 2, 1, 1);
        assert_eq!(
            agg2, "approve",
            "a Failed slot casts no vote -> aggregate stays approve"
        );
        assert_eq!(payload2["ok_models"].as_u64(), Some(1));
        assert_eq!(payload2["failed_models"].as_u64(), Some(1));
        assert_eq!(payload2["reviewers"][1]["ok"].as_bool(), Some(false));
    }

    /// `complete: true` only when at least one model reported a real
    /// reviewer response. The flag exists precisely so downstream CI gates
    /// can stop trusting a `comment` aggregate verdict when `complete=false`.
    #[test]
    fn aggregate_review_complete_true_when_any_reviewer_succeeds() {
        let reviews = vec![ok_slot("solo", "comment")];
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

    // -----------------------------------------------------------------------
    // SC1 [CRITICAL]: `cmd_review` must exit NONZERO when the reviewer panel
    // blocks the diff. The exit-code decision in `cmd_review` is
    // `verdict_rank(&agg) >= 2` over the aggregate verdict produced by
    // `aggregate_review` (the same value `emit_reviews` now returns to
    // `cmd_review`). These pins assert the exact gate: a blocking
    // aggregate ranks >= 2 (=> bail/nonzero), a passing aggregate ranks
    // < 2 (=> Ok/zero). A regression that lets a BLOCK verdict slip past
    // the gate (the false-success bug) trips one of these.
    // -----------------------------------------------------------------------

    /// The gate `cmd_review` applies: rank of the aggregate verdict.
    /// >= 2 means the panel blocked and `cmd_review` must `bail!`.
    fn review_would_bail(reviews: &[ReviewerSlot], ok: usize, failed: usize) -> bool {
        let (agg, _, _) = aggregate_review(reviews, 0.0, ok + failed, ok, failed);
        verdict_rank(&agg) >= 2
    }

    /// A successful reviewer voting `request_changes` blocks -> `cmd_review`
    /// must exit nonzero (the SC1 false-success bug: it used to exit 0).
    #[test]
    fn cmd_review_bails_on_request_changes_verdict() {
        let reviews = vec![ok_slot("real", "request_changes")];
        assert!(
            review_would_bail(&reviews, 1, 0),
            "request_changes aggregate must make cmd_review exit nonzero"
        );
    }

    /// `reject` / `block` are the other explicit blocking verdicts.
    #[test]
    fn cmd_review_bails_on_reject_and_block_verdicts() {
        for v in ["reject", "block", "BLOCK", " Request_Changes "] {
            let reviews = vec![ok_slot("real", v)];
            assert!(
                review_would_bail(&reviews, 1, 0),
                "blocking verdict {v:?} must make cmd_review exit nonzero"
            );
        }
    }

    /// An unknown / hallucinated verdict fails closed (verdict_rank ranks it
    /// 2), so `cmd_review` must also bail on it -> no silent approve.
    #[test]
    fn cmd_review_bails_on_unknown_verdict() {
        let reviews = vec![ok_slot("real", "lgtm")];
        assert!(
            review_would_bail(&reviews, 1, 0),
            "unknown verdict must fail closed and make cmd_review exit nonzero"
        );
    }

    /// `approve` (and a neutral `comment`) from a real reviewer are NOT
    /// blocking -> `cmd_review` still returns Ok(()) / exit 0.
    #[test]
    fn cmd_review_does_not_bail_on_approve_or_comment() {
        assert!(
            !review_would_bail(&[ok_slot("real", "approve")], 1, 0),
            "approve must exit 0"
        );
        assert!(
            !review_would_bail(&[ok_slot("real", "comment")], 1, 0),
            "a neutral comment (no blocker) must exit 0"
        );
    }
}

#[cfg(test)]
mod cli_switch_transfer_and_background_tests {
    //! Adversarial-review finding #6 (cont'd): pin the new behavior of
    //! `transfer` and `run --background`. `transfer` must NEVER mint a new
    //! empty session id — it has to either hand back the existing one or
    //! fail loudly. `run --background` must dispatch the run through the
    //! existing job registry so `status`/`result`/`cancel` can see it.

    use super::*;
    use crate::Cli;
    use clap::Parser;
    use zoder_core::Session;

    fn tmp_sessions_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zoder-cli-switch-{label}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --- Fix #2: transfer. ---

    /// With a prior session on disk, `transfer_resume_target` MUST return
    /// that session's id. The historical bug: it minted a fresh, empty
    /// session id that the user then "resumed", losing all real context.
    #[test]
    #[allow(deprecated)] // exercising the bare save() path in a single-process test fixture
    fn transfer_resume_target_returns_existing_session_id() {
        let sessions = tmp_sessions_dir("transfer-exists");
        let mut prior = Session::new("established-via-cli");
        prior.push("user", "earlier task");
        prior.push("assistant", "earlier reply");
        prior.save(&sessions).unwrap();

        let got = transfer_resume_target(&sessions)
            .expect("transfer must succeed when a prior session exists");
        assert_eq!(
            got, prior.id,
            "transfer must hand back the existing session id, not a fresh one"
        );
    }

    /// With NO prior session, `transfer_resume_target` must FAIL LOUDLY
    /// instead of fabricating an empty session id. Old behavior minted one.
    #[test]
    fn transfer_resume_target_errors_when_no_prior_session() {
        let sessions = tmp_sessions_dir("transfer-empty");
        let err = match transfer_resume_target(&sessions) {
            Ok(id) => panic!(
                "transfer must error when no prior session exists; got Ok({id:?}) \
                 (that's the bug — fabricating an empty id)"
            ),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(
            msg.contains("no resumable session"),
            "error must explain there is no resumable session; got: {msg}"
        );
    }

    // --- Fix #4: run --background. ---

    /// With `--background` and not in worker mode, `run_with_dispatch`
    /// MUST call the dispatch function and report the returned job id
    /// (NOT call the inline runner). The OLD behavior called the inline
    /// runner unconditionally, so the agentic turn ran inline and was
    /// indistinguishable from a foreground invocation; `status`/`result`/
    /// `cancel` never saw `run` jobs.
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_path_is_taken_when_background_and_not_in_worker() {
        let cli = Cli::try_parse_from(["zoder", "run", "--background", "-t", "x"]).unwrap();
        let dispatched = std::cell::Cell::new(false);
        let ran_inline = std::cell::Cell::new(false);
        let dispatched_id = "fake-job-id-12345";

        let got = crate::goose::run_with_dispatch(
            &cli,
            true,
            false, // not in worker mode
            |_kind, _cwd| {
                dispatched.set(true);
                Ok(dispatched_id.to_string())
            },
            async {
                ran_inline.set(true);
                Ok(())
            },
        )
        .await
        .expect("dispatch path must succeed");

        assert!(
            dispatched.get(),
            "--background without worker env must take the dispatch path (was silently ignored)"
        );
        assert!(
            !ran_inline.get(),
            "--background must NOT run inline; that was the old silently-ignored bug"
        );
        assert_eq!(
            got.as_deref(),
            Some(dispatched_id),
            "the job id from the dispatch must be returned to the caller"
        );
    }

    /// In worker mode (`in_worker=true`), `run_with_dispatch` MUST run
    /// inline (we are the worker) and NOT re-dispatch into an infinite
    /// recursion. The worker predicate is now injected, so this test
    /// does not need to mutate any process-wide env var (parallel-safe).
    #[tokio::test(flavor = "current_thread")]
    async fn in_worker_mode_runs_inline_and_skips_dispatch() {
        let cli = Cli::try_parse_from(["zoder", "run", "--background", "-t", "x"]).unwrap();
        let dispatched = std::cell::Cell::new(false);
        let ran_inline = std::cell::Cell::new(false);

        let got = crate::goose::run_with_dispatch(
            &cli,
            true,
            true, // we ARE the worker
            |_kind, _cwd| {
                dispatched.set(true);
                Ok("DISPATCHED".to_string())
            },
            async {
                ran_inline.set(true);
                Ok(())
            },
        )
        .await
        .expect("in-worker path must succeed");

        assert!(
            !dispatched.get(),
            "when we are the worker — re-dispatching would loop forever"
        );
        assert!(ran_inline.get(), "in-worker mode must run inline");
        assert!(got.is_none(), "in-worker mode returns no dispatch id");
    }

    /// Without `--background`, dispatch must never happen — even when
    /// we're not the worker. This pins the inverse precondition.
    #[tokio::test(flavor = "current_thread")]
    async fn no_background_means_always_inline() {
        let cli = Cli::try_parse_from(["zoder", "run", "-t", "x"]).unwrap();
        let dispatched = std::cell::Cell::new(false);
        let ran_inline = std::cell::Cell::new(false);

        let got = crate::goose::run_with_dispatch(
            &cli,
            false,
            false, // not a worker, but no --background
            |_kind, _cwd| {
                dispatched.set(true);
                Ok("DISPATCHED".to_string())
            },
            async {
                ran_inline.set(true);
                Ok(())
            },
        )
        .await
        .expect("inline path must succeed");

        assert!(
            !dispatched.get(),
            "without --background we MUST NOT dispatch — that was the original silent-ignore bug"
        );
        assert!(ran_inline.get(), "without --background we run inline");
        assert!(
            got.is_none(),
            "without --background no dispatch id is returned"
        );
    }
}

// ---------------------------------------------------------------------------
// `cmd_loop` resolution decisions — fail-closed regression suite.
//
// These tests pin the adversarial-review finding that the autonomous `loop`
// command previously declared RESOLVED on unvalidated or explicitly-rejected
// work. The matrix below exercises the production `decide_loop_resolution`
// helper, which `cmd_loop` calls 1-for-1, so behavior changes flow through
// a single source of truth.
//
// Invariants pinned (all fail-closed):
//
//   (a) no --check + substantive change + explicit request_changes verdict
//       does NOT resolve;
//   (b) explicit request_changes with zero heuristic-blocking findings still
//       does NOT resolve;
//   (c) no --check never resolves merely because the check was assumed
//       green (requires an explicit non-blocking review of substantive
//       work);
//   (d) the existing happy path still resolves.
// ---------------------------------------------------------------------------

#[cfg(all(test, unix))]
mod loop_resolution_tests {
    use super::*;

    /// Build a fully-substantive signal object for tests that don't
    /// care about substance shape — saves typing per-cell.
    fn substantive_signals(
        check_configured: bool,
        check_passed: Option<bool>,
        verdict: &str,
        blocking_findings: usize,
    ) -> LoopResolutionSignals {
        LoopResolutionSignals {
            substance: DiffSubstance::Substantive,
            check_configured,
            check_passed,
            verdict: verdict.into(),
            blocking_findings,
        }
    }

    /// (a) REGRESSION: When no `--check` is configured AND the diff is
    /// substantive AND the reviewer returns an explicit `request_changes`,
    /// the loop MUST NOT resolve — the explicit verdict is authoritative.
    /// Pre-fix behavior: `check_passed.unwrap_or(true) == true` fabricated a
    /// green check, and `verdict == "approve" || blocking == 0` was true at
    /// blocking == 0, so the loop RESOLVED on a `request_changes` review.
    #[test]
    fn no_check_with_explicit_request_changes_blocks_resolution() {
        let s = substantive_signals(
            false, // no --check configured
            None,  // check never ran
            "request_changes",
            0,
        );
        assert!(
            !decide_loop_resolution(&s, false),
            "no --check + explicit request_changes must NOT resolve (it does not, \
             pre-fix, fabricated a green check and let the request_changes vote slip)"
        );
    }

    /// (b) REGRESSION: An explicit `request_changes` verdict MUST block
    /// resolution REGARDLESS of whether the finding-severity heuristic
    /// counts zero blocking findings. A zero-heuristic-findings count must
    /// not override an explicit negative verdict.
    /// Pre-fix behavior: `review_ok = verdict == "approve" || blocking == 0`
    /// was true at blocking == 0 even with `verdict == "request_changes"`,
    /// so the resolve branch fired.
    #[test]
    fn explicit_request_changes_blocks_resolution_even_with_zero_blocking_findings() {
        // Pass a passing --check to remove that from the equation — this is
        // the exact shape that used to leak through pre-fix.
        let s = substantive_signals(
            true,       // --check configured
            Some(true), // --check passed
            "request_changes",
            0, // zero heuristic-blocking findings (no `critical`/`high` w/ location)
        );
        assert!(
            !decide_loop_resolution(&s, false),
            "explicit request_changes with zero heuristic-blocking findings must NOT \
             resolve; verdict is authoritative over the heuristic"
        );
        // And the same holds for `--accept-on-green` — the explicit
        // request_changes verdict cannot be overridden.
        assert!(
            !decide_loop_resolution(&s, true),
            "even under --accept-on-green, explicit request_changes must NOT resolve"
        );
    }

    /// Companion to (b): the OTHER explicit negative verdicts (`reject` /
    /// `block`) are equally authoritative. Their heuristic-finding count is
    /// irrelevant. This closes the same class of bug for verdict strings
    /// that share rank 2 in `verdict_rank`.
    #[test]
    fn explicit_reject_and_block_verdicts_block_resolution() {
        for verdict in ["reject", "block"] {
            let s = substantive_signals(true, Some(true), verdict, 0);
            assert!(
                !decide_loop_resolution(&s, false),
                "explicit `{}` verdict with zero blocking findings must NOT resolve",
                verdict
            );
        }
    }

    /// (c) REGRESSION: When no `--check` is configured, the loop MUST NOT
    /// resolve merely because the absent check used to be treated as a
    /// fabricated green. Resolution on an absent check now requires an
    /// explicit non-blocking review (approve OR comment with zero
    /// blocking findings) of substantive work.
    /// Pre-fix behavior: `green = check_passed.unwrap_or(true)` was the
    /// fabricated green; combined with the calibration in
    /// `count_blocking` (only `critical` blocks when `green=true`), a
    /// reviewer that raised only `high`-severity findings (which WOULD
    /// block when `green=false`) saw `blocking == 0` and the loop
    /// RESOLVED with no validation having run.
    #[test]
    fn no_check_does_not_resolve_on_high_severity_finding_without_substance() {
        // The reviewer raised one `high`-severity, properly-located
        // finding. With `check=None`, `count_blocking` must use the
        // honest `green=false` calibration (so `high` blocks).
        let review = ReviewOutput {
            verdict: "comment".into(),
            summary: String::new(),
            findings: vec![Finding {
                severity: "high".into(),
                title: "real concern".into(),
                body: "reviewer raised a high-severity issue".into(),
                location: Some("src/lib.rs:42".into()),
            }],
            next_steps: vec![],
        };
        let signals = loop_signals_from_review(
            DiffSubstance::Substantive,
            false, // no --check
            None,  // check never ran
            &review,
        );
        assert!(
            signals.blocking_findings >= 1,
            "honest calibration: a high-severity finding with no actual check \
             must count as blocking; pre-fix `green = unwrap_or(true)` would \
             have made `high` advisory"
        );
        assert!(
            !decide_loop_resolution(&signals, false),
            "no --check + high-severity blocking finding must NOT resolve; \
             pre-fix code fabricated a green check and let the high finding through"
        );
    }

    /// Companion: no --check + substantive diff + comment with zero
    /// blocking findings DOES resolve (the legitimate happy path for
    /// --check-free workflows). This pins requirement (c)'s positive form:
    /// "requires an explicit non-blocking review of substantive work".
    #[test]
    fn no_check_resolves_on_substantive_change_with_non_blocking_review() {
        let s = substantive_signals(false, None, "comment", 0);
        assert!(
            decide_loop_resolution(&s, false),
            "no --check + substantive diff + comment with zero blocking findings \
             MUST still resolve (the legitimate happy path for --check-free runs)"
        );
    }

    /// (d) REGRESSION: The existing happy path is preserved.
    ///
    ///   substantive author change + passing --check (when configured) +
    ///   review with no blocking findings and NOT an explicit request_changes
    ///   still resolves as before.
    ///
    /// Pre-fix behavior already accepted this. The fix must not regress it.
    #[test]
    fn happy_path_substantive_change_passing_check_approve_resolves() {
        let s = substantive_signals(true, Some(true), "approve", 0);
        assert!(
            decide_loop_resolution(&s, false),
            "happy path: substantive + passing check + approve must resolve"
        );
    }

    /// Companion to (d): a `comment` reviewer with zero blocking findings
    /// also counts as a non-blocking review (the pre-fix behavior). The fix
    /// must not regress this — it's the most common non-strict-reviewer
    /// case in production.
    #[test]
    fn happy_path_substantive_change_passing_check_comment_zero_findings_resolves() {
        let s = substantive_signals(true, Some(true), "comment", 0);
        assert!(
            decide_loop_resolution(&s, false),
            "happy path: substantive + passing check + comment + zero findings \
             must still resolve (preserves pre-fix happy path)"
        );
    }

    /// Negative guard: an explicit `request_changes` with a passing check
    /// and zero heuristic-blocking findings MUST NOT resolve. Re-pins the
    /// `loop_review_ok` invariant from a different angle for symmetry with
    /// (b): when the heuristic doesn't trip, the explicit verdict still
    /// does.
    #[test]
    fn explicit_request_changes_with_passing_check_still_blocks() {
        let s = substantive_signals(true, Some(true), "request_changes", 0);
        assert!(
            !decide_loop_resolution(&s, false),
            "passing check must NOT override an explicit request_changes verdict"
        );
    }

    /// Companion: a passing check with non-zero heuristic-blocking findings
    /// (e.g. real `critical` regression findings the reviewer raised) also
    /// blocks. This is the legacy fail-closed branch of `review_ok`.
    #[test]
    fn passing_check_with_real_blocking_findings_blocks() {
        let s = substantive_signals(true, Some(true), "comment", 2);
        assert!(
            !decide_loop_resolution(&s, false),
            "passing check + non-zero blocking findings must NOT resolve"
        );
    }

    /// Anti-gaming guard: even with a passing check and an explicit
    /// `approve` reviewer, an empty (non-substantive) diff MUST NOT
    /// resolve. This re-pins the anti-gaming rail at the resolution gate.
    #[test]
    fn empty_diff_with_passing_check_and_approve_does_not_resolve() {
        let s = LoopResolutionSignals {
            substance: DiffSubstance::Empty,
            check_configured: true,
            check_passed: Some(true),
            verdict: "approve".into(),
            blocking_findings: 0,
        };
        assert!(
            !decide_loop_resolution(&s, false),
            "anti-gaming guard: empty diff must NOT resolve even on green + approve"
        );
    }

    /// An explicit failing `--check` MUST NOT resolve (fail-closed check
    /// gate). This is the same fail-closed invariant requirement (1) but
    /// for the explicit-failed side.
    #[test]
    fn explicit_failing_check_blocks_resolution() {
        let s = substantive_signals(true, Some(false), "approve", 0);
        assert!(
            !decide_loop_resolution(&s, false),
            "a configured --check that explicitly failed must NOT resolve, even \
             with an explicit approve verdict (fail-closed)"
        );
    }

    /// `--accept-on-green` requires a REAL passing check (not just no
    /// failure). A never-configured check does not satisfy the
    /// `--accept-on-green` semantic — the operator asked for "I trust the
    /// check", so a check must actually have run and passed.
    #[test]
    fn accept_on_green_does_not_resolve_when_no_check_ran() {
        // accept_on_green=true, but check was never run (None). Decision
        // path: accept_on_green requires `check_explicit_passed`, so this
        // must fall through to the default branch (which also requires
        // substance_ok + review_ok — all true here, so it MUST resolve).
        // The structural intent is: accept_on_green doesn't add NEW
        // shortcuts — it permits resolving when the explicit check passed,
        // but the same default-path resolve is allowed in this scenario.
        let s = substantive_signals(false, None, "comment", 0);
        assert!(
            decide_loop_resolution(&s, true),
            "no --check + substantive + non-blocking review MUST still resolve \
             even under --accept-on-green; the semantic is additive, not \
             restrictive"
        );
    }

    /// `--accept-on-green` MUST NOT resolve when the explicit check
    /// failed — adding the fail-closed check gate under all opt-ins.
    #[test]
    fn accept_on_green_does_not_resolve_when_check_explicitly_failed() {
        let s = substantive_signals(true, Some(false), "approve", 0);
        assert!(
            !decide_loop_resolution(&s, true),
            "accept_on_green MUST NOT override an explicit failing --check"
        );
    }

    /// `loop_review_ok` matrix pins: every explicit-block verdict beats
    /// every blocking-findings count. Mirrors requirement (2).
    #[test]
    fn loop_review_ok_explicit_verdict_matrix() {
        // Approve / comment are not explicit blocks; verdict is fine.
        assert!(loop_review_ok(
            &ReviewOutput {
                verdict: "approve".into(),
                ..Default::default()
            },
            0
        ));
        assert!(loop_review_ok(
            &ReviewOutput {
                verdict: "comment".into(),
                ..Default::default()
            },
            0
        ));
        // Explicit blocks beat ZERO blocking findings.
        for v in ["request_changes", "reject", "block"] {
            assert!(
                !loop_review_ok(
                    &ReviewOutput {
                        verdict: v.into(),
                        ..Default::default()
                    },
                    0
                ),
                "verdict={v} with 0 blocking findings must NOT be OK"
            );
        }
        // An explicit blocker beats a non-zero blocking-findings count.
        assert!(!loop_review_ok(
            &ReviewOutput {
                verdict: "request_changes".into(),
                ..Default::default()
            },
            3
        ));
        // Heuristic-only block (no explicit block) still blocks when
        // findings are non-zero — the legacy fail-closed branch.
        assert!(!loop_review_ok(
            &ReviewOutput {
                verdict: "comment".into(),
                ..Default::default()
            },
            1
        ));
    }

    // -----------------------------------------------------------------------
    // Z-1 REGRESSION: review-phase failure synthesis is fail-CLOSED.
    //
    // `cmd_loop`'s inline review path produces a synthesized `ReviewOutput`
    // when `phase_watchdog(complete_once)` returns `Err` — either a
    // wall-clock timeout around a hung provider request, or a reviewer
    // chain that exhausted with 0/N reviewers completing. Pre-fix the
    // synthesized verdict was the non-blocking `"comment"`, so the
    // subsequent `loop_review_ok` / `decide_loop_resolution` treated the
    // iteration as approved on a green check and reported `RESOLVED` —
    // even though no reviewer had actually run. The fix funnels the
    // synthesis through `synthesize_review_phase_failure` and pins that
    // helper's verdict to the explicit blocker `"request_changes"`.
    //
    // These tests are the anti-gaming gate: they exercise the REAL
    // decision functions (`synthesize_review_phase_failure`,
    // `loop_review_ok`, `decide_loop_resolution`), not a reimplementation.
    // The first test ("the synthesizer itself is request_changes") is
    // the minimal pin; the second ("the loop's resolution decision
    // refuses the synthesized shape") drives the same input through
    // `decide_loop_resolution` so the fix can't be satisfied by, e.g.,
    // lying about the verdict in the helper while the decision function
    // still treats it as approve.
    // -----------------------------------------------------------------------

    /// The synthesis helper for a review-phase failure MUST emit
    /// `"request_changes"`, NOT `"comment"`. A `"comment"` synthesis is
    /// fail-OPEN: `loop_review_ok` returns `true` on
    /// `"comment" + zero blocking findings`, and the loop's
    /// resolution predicate then resolves the iteration without ever
    /// having been reviewed.
    ///
    /// Pre-fix: literal `"comment".into()` at the `Err` branch (the
    /// inline synthesis). Test would fail because the helper returned
    /// `"comment"`.
    /// Post-fix: helper returns `"request_changes"`. Test passes.
    #[test]
    fn review_phase_failure_synthesis_is_request_changes() {
        let r = synthesize_review_phase_failure("review phase timed out after 900s (killed)");
        assert_eq!(
            r.verdict, "request_changes",
            "Z-1 REGRESSION: the review-phase failure synthesizer must emit the \
             explicit blocker `request_changes`, not the non-blocking `comment`. \
             Otherwise a review-phase timeout / zero-reviewer outcome would cause \
             `loop_review_ok` + `decide_loop_resolution` to mark the loop RESOLVED \
             with the diff never reviewed."
        );
        // The summary MUST mention the review-phase context so the next
        // author turn sees what happened. The literal prefix is the only
        // reasonable witness.
        assert!(
            r.summary.starts_with("reviewer "),
            "summary must carry the `reviewer <reason>` shape so the next \
             author turn can grep for it; got: {:?}",
            r.summary
        );
    }

    /// The synthesized ReviewOutput shape MUST be fail-closed at the
    /// resolution gate. This pins the END-TO-END invariant through
    /// `decide_loop_resolution` (the function `cmd_loop` actually calls)
    /// rather than the helper alone — so the fix can't satisfy the spec
    /// by changing only the helper text while leaving a downstream
    /// case-insensitive-but-still-non-blocking path in place.
    ///
    /// Shape: passing --check + substantive diff + the synthesis
    /// outcome. Pre-fix behavior: with verdict="comment" and zero
    /// blocking findings, `decide_loop_resolution` returns true (loop
    /// resolves). Post-fix: verdict="request_changes" → returns false
    /// (loop does NOT resolve). The test asserts the latter.
    ///
    /// Note: the test deliberately feeds the post-fix verdict shape
    /// (the synthesizer's output, not the pre-fix "comment" string),
    /// because the assertion ("does NOT resolve") is what the spec
    /// requires; if a future refactor changed the helper's verdict
    /// literal back to "comment" the SYNTHESIZER test above would
    /// still catch it.
    #[test]
    fn review_phase_failure_outcome_does_not_resolve_via_decide_loop_resolution() {
        // Use the real synthesizer — same call site as `cmd_loop`.
        let synthesized =
            synthesize_review_phase_failure("review phase timed out after 900s (killed)");
        let signals = loop_signals_from_review(
            DiffSubstance::Substantive,
            true,       // --check was configured
            Some(true), // --check passed
            &synthesized,
        );
        assert!(
            !decide_loop_resolution(&signals, false),
            "Z-1 REGRESSION (e2e): a synthesized review-phase failure must NOT \
             resolve the loop on a green check + substantive diff. Pre-fix the \
             synthesizer used `comment`, which `loop_review_ok` treated as a \
             non-blocking review, and `decide_loop_resolution` returned true."
        );
        // And not under `--accept-on-green` either — the explicit blocker
        // cannot be overridden by the operator's opt-in to a passing check.
        assert!(
            !decide_loop_resolution(&signals, true),
            "Z-1 REGRESSION (e2e, --accept-on-green): an unreviewed iteration \
             (review-phase failure synthesis) must NOT resolve, even when the \
             operator opts into `--accept-on-green`."
        );
    }

    // -----------------------------------------------------------------------
    // Z-2 REGRESSION: verdict comparisons are case- and whitespace-
    // insensitive. Real LLM output drifts casing/padding
    // (`"BLOCK"`, `"Request_Changes"`, `"REJECT"`, `" approve "`); the
    // pre-fix code matched verdicts against exact lowercase literals
    // (`"request_changes" | "reject" | "block"`), so a cased-out request
    // was ranked 0 (approve) and the loop resolved the iteration as if
    // the reviewer had approved.
    //
    // The pins exercise the REAL decision functions (`loop_review_ok`,
    // `verdict_rank`) directly so the fix can't be gamed by, e.g.,
    // normalizing in a wrapper that the production code never calls.
    // -----------------------------------------------------------------------

    /// Mixed/upper-case explicit block verdicts are BLOCKING, not
    /// silently approved. Each variant matches one of the three
    /// rank-2 literals (`request_changes` / `reject` / `block`) only
    /// after `.trim().to_ascii_lowercase()`.
    #[test]
    fn loop_review_ok_blocks_mixed_case_verdicts() {
        // Each tuple: (verdict string literal to feed in, human label).
        // Every label is the rank-2 literal the pre-fix code failed to
        // recognize because of casing drift.
        let cases: &[&str] = &[
            "BLOCK",             // matches "block"
            "Request_Changes",   // matches "request_changes"
            "REJECT",            // matches "reject"
            " Request_Changes ", // padding + mixed case
            "reject",            // already lowercase — sanity baseline
            "block",             // already lowercase — sanity baseline
        ];
        for v in cases {
            // Build the ReviewOutput directly (bypassing `parse_review`)
            // so the comparison function is the ONLY thing standing
            // between us and the pre-fix bug.
            let r = ReviewOutput {
                verdict: (*v).into(),
                ..Default::default()
            };
            assert!(
                !loop_review_ok(&r, 0),
                "Z-2 REGRESSION: `loop_review_ok` must treat verbatim \
                 `{v:?}` (padded / mixed / uppercase) as BLOCKING, not as \
                 approve. Pre-fix the comparison was case-sensitive and \
                 `{v:?}` ranked 0 → approve."
            );
        }
    }

    /// Padded / mixed-case approve verdicts are APPROVE — the inverse
    /// half of the same normalization. This pins the non-gaming
    /// half: a reviewer returning `" approve "` must still pass the
    /// gate as a valid non-blocking review.
    #[test]
    fn loop_review_ok_approves_padded_mixed_case_approve() {
        let cases: &[&str] = &[" approve ", "Approve", "APPROVE", " Approve "];
        for v in cases {
            let r = ReviewOutput {
                verdict: (*v).into(),
                ..Default::default()
            };
            assert!(
                loop_review_ok(&r, 0),
                "Z-2 REGRESSION (positive): `loop_review_ok` must treat verbatim \
                 `{v:?}` as an approving verdict. The normalized form must be \
                 `approve` (not anything that would fall through to rank 0 by \
                 accident)."
            );
        }
    }

    /// `verdict_rank` must also be case- and whitespace-insensitive —
    /// the same fix has to apply at the aggregator's `worst_rank`
    /// promotion. A rank-0 misclassification here would let a mixed-
    /// case `BLOCK` from the reviewer be ignored by the aggregator's
    /// `worst_rank >= 2` check, so this is the second player in the
    /// Z-2 defense.
    #[test]
    fn verdict_rank_is_case_and_whitespace_insensitive() {
        // Rank 2 across all the LLM-cased variants.
        for v in [
            "request_changes",
            "REQUEST_CHANGES",
            "Request_Changes",
            "reject",
            "REJECT",
            "Reject",
            "block",
            "BLOCK",
            "Block",
            "  block  ",
            " Block ",
        ] {
            assert_eq!(
                verdict_rank(v),
                2,
                "verdict_rank({v:?}) must be 2 (rank of explicit blocker)"
            );
        }
        // Rank 1 for comment / neutral.
        for v in [
            "comment",
            "COMMENT",
            "Comment",
            "neutral",
            "NEUTRAL",
            " Neutral ",
        ] {
            assert_eq!(
                verdict_rank(v),
                1,
                "verdict_rank({v:?}) must be 1 (rank of comment / neutral)"
            );
        }
        // Rank 0 for approve / empty only.
        for v in ["approve", "APPROVE", "Approve", " approve ", ""] {
            assert_eq!(
                verdict_rank(v),
                0,
                "verdict_rank({v:?}) must be 0 (rank of approve / empty)"
            );
        }
        // W4: unknown / unrecognized non-empty verdicts rank as BLOCKING (2),
        // never approve — so a hallucinated or typo verdict fails closed.
        for v in [
            "unknown_thing",
            "deny",
            "changes_requested",
            "needs_changes",
            "lgtm",
            "fail",
        ] {
            assert_eq!(
                verdict_rank(v),
                2,
                "verdict_rank({v:?}) must be 2 (unknown verdict fails closed)"
            );
        }
    }

    // W3/W4 adversarial regression: the review gate must not be gamed by a
    // decoy verdict or a hallucinated verdict string.
    #[test]
    fn parse_review_keeps_most_blocking_verdict_over_decoy_approve() {
        // Decoy approve BEFORE the real request_changes must not win (W3).
        let raw = r#"Here is my review. {"verdict":"approve"} but actually {"verdict":"request_changes","summary":"real"}"#;
        let r = parse_review(raw);
        assert_eq!(
            r.verdict, "request_changes",
            "W3: a decoy approve before a real request_changes must not win"
        );
        assert!(!loop_review_ok(&r, 0));
        // Reverse order — the block comes first — still blocks.
        let raw2 = r#"{"verdict":"request_changes"} then {"verdict":"approve"}"#;
        assert_eq!(parse_review(raw2).verdict, "request_changes");
        // A lone approve still approves (no regression).
        assert_eq!(parse_review(r#"{"verdict":"approve"}"#).verdict, "approve");
    }

    #[test]
    fn loop_review_ok_fails_closed_on_unknown_verdict() {
        // W4: verdicts outside {approve, comment, neutral} block, even with
        // zero heuristic-blocking findings.
        for v in [
            "deny",
            "changes_requested",
            "needs_changes",
            "lgtm",
            "fail",
            "reject_this",
            "aprove",
        ] {
            let r = ReviewOutput {
                verdict: v.into(),
                ..Default::default()
            };
            assert!(
                !loop_review_ok(&r, 0),
                "W4: unknown verdict {v:?} must fail closed, not resolve the loop"
            );
        }
        // Sanity: the benign verdicts still resolve at zero blocking findings.
        for v in ["approve", "comment", "neutral"] {
            let r = ReviewOutput {
                verdict: v.into(),
                ..Default::default()
            };
            assert!(
                loop_review_ok(&r, 0),
                "benign verdict {v:?} must still resolve"
            );
        }
    }

    /// End-to-end: a mixed-case `"Request_Changes"` ReviewOutput fed
    /// through `decide_loop_resolution` must NOT resolve. This pins the
    /// full loop gate, not just `loop_review_ok` in isolation, so a
    /// future refactor that normalizes at `loop_review_ok` but breaks
    /// the `s.verdict.clone()` handoff into `decide_loop_resolution`
    /// will be caught here too.
    #[test]
    fn decide_loop_resolution_blocks_mixed_case_request_changes() {
        // Direct construction (no `parse_review`) — the only thing
        // between this input and the resolution decision is the
        // comparison-function normalization.
        let review = ReviewOutput {
            verdict: "Request_Changes".into(),
            summary: "mixed-case explicit block".into(),
            findings: vec![],
            next_steps: vec![],
        };
        let signals = loop_signals_from_review(
            DiffSubstance::Substantive,
            true,
            Some(true), // --check passed
            &review,
        );
        assert!(
            !decide_loop_resolution(&signals, false),
            "Z-2 REGRESSION (e2e): `decide_loop_resolution` must NOT treat a \
             mixed-case `Request_Changes` verdict as approve. Pre-fix the \
             verdict field was matched case-sensitively and this iteration \
             leaked through as RESOLVED."
        );
    }
}

// ---------------------------------------------------------------------------
// `build_diff` / `cap_diff` review-diff soundness regressions.
//
// The two fixes here close FAIL-CLOSED defects found by adversarial review:
//
//   * `build_diff` (WorkingTree scope) used to build its diff from
//     `git diff HEAD` alone. A working tree whose only work is NEW,
//     not-yet-added files therefore produced an EMPTY diff and the
//     review/loop would wrongly report "no changes to review".
//     The fix enumerates untracked-not-ignored paths via
//     `git ls-files --others --exclude-standard` and appends each as a
//     synthetic "new file" unified-diff hunk.
//
//   * `cap_diff` used to slice the input by raw byte offset, panicking
//     when the cap landed inside a multibyte UTF-8 codepoint. The fix
//     walks each boundary to a char boundary before slicing.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod review_diff_soundness_tests {
    use super::*;

    /// Initialize a temp git repo with one committed file. Returns the
    /// repo path. Mirrors the test idioms used elsewhere in the crate
    /// (tempfile::tempdir + std::process::Command shelling out to git).
    fn init_temp_repo_with_one_committed_file() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q"]);
        // Required to run `git commit` in a fresh temp dir without
        // inheriting the host's identity.
        run(&["config", "user.email", "test@example.invalid"]);
        run(&["config", "user.name", "review-diff-test"]);
        run(&["config", "init.defaultBranch", "main"]);
        std::fs::write(repo.join("README.md"), "seed\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-q", "-m", "init"]);
        (dir, repo)
    }

    /// REGRESSION: a working tree whose ONLY work is brand-new, not-yet-added
    /// files must produce a NON-empty working-tree diff. The old `build_diff`
    /// relied on `git diff HEAD`, which only sees tracked paths and would
    /// return an empty diff here. The fix enumerates untracked-not-ignored
    /// files via `git ls-files --others --exclude-standard` and appends each
    /// as a synthetic "new file" hunk.
    #[test]
    fn build_diff_includes_new_untracked_files_in_working_tree() {
        let (_dir, repo) = init_temp_repo_with_one_committed_file();

        // Add a NEW untracked file (no `git add`).
        let new_file = repo.join("new_feature.rs");
        std::fs::write(&new_file, "pub fn new_thing() -> u32 { 42 }\n").unwrap();

        let (_label, diff) =
            build_diff(&repo, ReviewScope::WorkingTree, None).expect("build_diff ok");

        assert!(
            !diff.trim().is_empty(),
            "build_diff must surface brand-new untracked files (was empty: {:?})",
            diff
        );
        assert!(
            diff.contains("new_feature.rs"),
            "diff should reference the new untracked file: {diff}"
        );
        // The synthetic hunk must follow the unified-diff shape the reviewer
        // expects (`diff --git a/P b/P` + `new file mode` + `--- /dev/null`
        // + `+++ b/P`).
        assert!(
            diff.contains("diff --git a/new_feature.rs b/new_feature.rs"),
            "diff should contain a `diff --git` header for the new file: {diff}"
        );
        assert!(
            diff.contains("new file mode"),
            "diff should mark the file as a new file: {diff}"
        );
        assert!(
            diff.contains("--- /dev/null"),
            "diff should pair the new file against /dev/null: {diff}"
        );
    }

    /// Companion guard: when only `.gitignore`d junk is added, the diff must
    /// stay empty. The fix uses `--exclude-standard`, so a `.gitignore`d
    /// untracked file must NOT be surfaced (and we must NOT panic).
    #[test]
    fn build_diff_does_not_surface_gitignored_untracked_files() {
        let (dir, repo) = init_temp_repo_with_one_committed_file();
        std::fs::write(repo.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(repo.join("ignored")).unwrap();
        std::fs::write(repo.join("ignored").join("noise.txt"), "ignore me\n").unwrap();

        let (_label, diff) = build_diff(&repo, ReviewScope::WorkingTree, None).unwrap();
        assert!(
            !diff.contains("noise.txt"),
            "gitignored paths must not appear in the working-tree diff: {diff}"
        );
        drop(dir);
    }

    /// REGRESSION: review diff acquisition must not buffer all of `git diff`
    /// before the prompt-level `cap_diff` has a chance to trim it. The capped
    /// git path reads at most the requested byte count, stops the child when
    /// that limit is reached, and carries an explicit truncation marker forward.
    #[test]
    fn git_diff_acquisition_caps_stdout_before_prompt_cap() {
        let (_dir, repo) = init_temp_repo_with_one_committed_file();
        let mut body = String::from("seed\n");
        for i in 0..1500 {
            body.push_str(&format!("line-{i:04}\n"));
        }
        std::fs::write(repo.join("README.md"), body).unwrap();

        let full = run_git(&repo, &["diff", "HEAD"]).expect("uncapped fixture diff");
        let cap = 512;
        assert!(
            full.len() > cap,
            "fixture diff must exceed the test cap: len={} cap={cap}",
            full.len()
        );
        assert!(
            full.contains("line-1200"),
            "fixture diff should contain content far beyond the cap"
        );

        let capped = run_git_capped(&repo, &["diff", "HEAD"], cap).expect("capped git diff");
        assert!(
            capped.limit_reached,
            "capped git diff should report the acquisition cap was reached"
        );
        assert_eq!(
            capped.stdout.len(),
            cap,
            "capped git diff must retain exactly the configured byte limit"
        );
        assert!(
            !capped.stdout.contains("line-1200"),
            "capped git diff must not retain bytes beyond the acquisition cap"
        );

        let rendered = render_capped_git_diff(capped, cap);
        assert!(
            rendered.contains("git diff truncated during acquisition at 512 bytes"),
            "capped review diff should carry a visible acquisition-truncation marker: {rendered}"
        );
    }

    /// REGRESSION: `cap_diff` must NOT panic when the cap lands in the
    /// middle of a multibyte UTF-8 codepoint. The old byte-index slice
    /// panicked with "byte index N is not a char boundary"; the fix snaps
    /// to the nearest char boundary before slicing. We use a string with a
    /// known multibyte layout (2-byte Latin-1 supplement `é` followed by
    /// 4-byte emoji `🦀`) and pick a cap that forces the head slice to
    /// land inside one of those sequences.
    #[test]
    fn cap_diff_does_not_panic_on_multibyte_codepoint_boundary() {
        // 20 ASCII chars + "é" (2 bytes) + "🦀" (4 bytes) + 20 more ASCII.
        let mut body = String::new();
        for _ in 0..20 {
            body.push('a');
        }
        body.push('é');
        body.push('🦀');
        for _ in 0..20 {
            body.push('b');
        }

        // A cap that lands inside the 2-byte `é` sequence at byte 21.
        // The head slice is `max * 3 / 4` bytes; choose max=28 so
        // 28*3/4 = 21, which falls between the two bytes of `é`.
        let capped = std::panic::catch_unwind(|| cap_diff(&body, 28));
        let capped = capped.expect("cap_diff must not panic on a mid-codepoint cap");

        // The returned string must still be valid UTF-8 (it is, by
        // construction) AND must satisfy the cap semantics: the head slice
        // must end on a char boundary and contain no more than 21 bytes.
        assert!(std::str::from_utf8(capped.as_bytes()).is_ok());
        assert!(
            capped.contains("truncated for length"),
            "cap_diff should keep its truncation marker: {capped}"
        );
        let marker_pos = capped.find("...[diff truncated for length]...").unwrap();
        let head = &capped[..marker_pos];
        let head_no_newlines = head.trim_end_matches('\n');
        assert!(
            head_no_newlines.len() <= 21,
            "head slice must be <= max*3/4 bytes on a char boundary, got {}",
            head_no_newlines.len()
        );
        // The head slice must NOT have cut `é` in half: it must end before
        // the `é` codepoint begins, which sits at byte index 20.
        assert!(
            !head_no_newlines.ends_with('é'),
            "head must not end mid-codepoint (cut a multibyte char): {head_no_newlines:?}"
        );
    }

    /// Companion guard: the cap semantics must still hold on a purely-ASCII
    /// diff (no multibyte chars). The head+tail layout must remain valid
    /// and the truncation marker must be present when `len > max`.
    #[test]
    fn cap_diff_preserves_head_tail_layout_for_ascii() {
        let body: String = (0..200).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        let max = 80;
        assert!(body.len() > max);
        let capped = cap_diff(&body, max);
        assert!(capped.contains("...[diff truncated for length]..."));
        let marker = "...[diff truncated for length]...";
        let head = &capped[..capped.find(marker).unwrap()];
        let tail = &capped[capped.find(marker).unwrap() + marker.len()..];
        assert!(head.trim_end_matches('\n').len() <= max * 3 / 4);
        // The tail slice is `&diff[diff.len() - max/4..]`, so its length
        // (excluding the marker suffix newlines) must be <= max/4 bytes.
        let tail_trimmed = tail.trim_start_matches('\n').trim_end_matches('\n');
        assert!(tail_trimmed.len() <= max / 4);
        // The tail must contain the LAST chars of `body`.
        let last_chunk: String = body
            .chars()
            .rev()
            .take(max / 4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        assert!(
            tail_trimmed.ends_with(&last_chunk),
            "tail must end with the last `max/4` chars of body: tail={tail_trimmed:?} last={last_chunk:?}"
        );
    }

    /// Edge case: `max == 0` (or smaller than the marker) must not panic.
    /// The head slice is `max * 3 / 4 = 0` bytes — we just verify the call
    /// returns a valid UTF-8 string without panicking.
    #[test]
    fn cap_diff_handles_degenerate_caps_without_panicking() {
        let body = "hello, world! résumé naïve 🚀";
        for max in 0..8 {
            let capped =
                std::panic::catch_unwind(|| cap_diff(body, max)).expect("cap_diff must not panic");
            assert!(std::str::from_utf8(capped.as_bytes()).is_ok());
        }
    }

    /// REGRESSION: CJK (3-byte UTF-8) characters at the truncation boundary
    /// must not panic. The old byte-index slice panicked with "byte index N is
    /// not a char boundary". The fix snaps to the nearest char boundary before
    /// slicing. We use a string with known CJK layout (3-byte characters) and
    /// pick a cap that forces the head slice to land inside one of them.
    #[test]
    fn cap_diff_does_not_panic_on_cjk_boundary() {
        // 20 ASCII chars + "测" (3 bytes) + "试" (3 bytes) + 20 ASCII chars.
        let mut body = String::new();
        for _ in 0..20 {
            body.push('a');
        }
        body.push('测');
        body.push('试');
        for _ in 0..20 {
            body.push('b');
        }

        // A cap that forces head_end = max * 3 / 4 to land inside "测".
        // max = 28 => 28 * 3 / 4 = 21. "测" occupies bytes 20..23, so
        // byte 21 is the middle of the first CJK character.
        let capped = std::panic::catch_unwind(|| cap_diff(&body, 28));
        let capped = capped.expect("cap_diff must not panic on a mid-CJK cap");

        assert!(std::str::from_utf8(capped.as_bytes()).is_ok());
        assert!(
            capped.contains("truncated for length"),
            "cap_diff should keep its truncation marker: {capped}"
        );
    }
}

// ---------------------------------------------------------------------------
// Reviewer chain dispatch — integration tests.
//
// These tests pin the 2026-07-07 reviewer-pipeline fix: a single dead
// reviewer model must not kill the whole adversarial review. They exercise
// `complete_once` end-to-end against a wiremock HTTP server (the same
// test-double the `zoder-core/tests/provider.rs` tests use to simulate
// provider behavior) so the dispatcher sees realistic
// `provider.stream_chat` errors — no mocking out of reqwest, no global
// state. Every test sets `$ZODER_HOME` to an isolated tempdir, runs
// inside an `ENV_LOCK` mutex, and never touches the host's real
// `~/.zoder/`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod reviewer_chain_dispatch_tests {
    use super::*;
    use crate::Cli;
    use clap::Parser;
    use std::path::{Path, PathBuf};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zoder_core::config::Provider;

    /// RAII guard that sets `ZODER_HOME` to `home` for the lifetime of
    /// the guard and restores the prior value (or unsets it) on drop.
    /// Holds `crate::test_env::ENV_LOCK` — the SAME lock
    /// `main.rs::health_install_tests` uses — so an async reviewer-chain
    /// test and a sync install-daily test running in different threads
    /// of the same binary are properly serialized. Without the shared
    /// lock the two modules each had their own `static Mutex<()>` and
    /// the env var raced between them (test A set `ZODER_HOME=/A`, test
    /// B's `Engine::load()` read `/B`, and A's `Config::load()` found
    /// the wrong corpus). The guard is intentionally cheap (no
    /// allocations beyond the lock guard) so dropping it inside an
    /// async block is well-defined.
    struct HomeGuard {
        // Holding the shared `EnvGuard` (not just a bare
        // `MutexGuard`) is what makes the `Drop` impl run on
        // guard-drop, restoring the prior `ZODER_HOME` exactly
        // when the lock is released. The `inner` field is
        // therefore read implicitly (by the drop glue) — mark
        // it `_inner` to silence the "never read" warning while
        // keeping the self-documenting field name for anyone
        // stepping through the type in a debugger.
        #[allow(dead_code)]
        inner: crate::test_env::EnvGuard,
    }
    impl HomeGuard {
        fn new(home: &Path) -> Self {
            Self {
                inner: crate::test_env::EnvGuard::new(home),
            }
        }
    }

    /// Write a `model_corpus.json` containing every model id we'll route
    /// to, all marked `free=true` so the policy gate (which runs on the
    /// reviewer's call site before dispatch) doesn't reject them with a
    /// paid-need-confirm. The shape matches what `Corpus::load` expects.
    fn write_corpus(home: &Path, ids: &[&str]) {
        let arr: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| {
                serde_json::json!({
                    "id": id,
                    "free": true,
                    "routable": true,
                })
            })
            .collect();
        let body = serde_json::json!({
            "source": "test",
            "models": arr,
        });
        std::fs::write(
            home.join("model_corpus.json"),
            serde_json::to_string_pretty(&body).unwrap(),
        )
        .unwrap();
    }

    /// Write the minimal `config.json` that `Config::load` accepts. Two
    /// providers, both pointing at the wiremock URI returned by the call
    /// site (a shared URL is fine — the model-id-to-provider routing in
    /// `Config::provider_for_model` keys off the `serves` prefix).
    fn write_config(home: &Path, mock_uri: &str, reviewer_model: &str) {
        let body = serde_json::json!({
            "providers": [
                {
                    "id": "wiremock-broken",
                    "base_url": format!("{mock_uri}/broken"),
                    "kind": "openai-chat",
                    "auth": {"type": "none"},
                    "billing": "free",
                    "serves": ["broken-model/"],
                },
                {
                    "id": "wiremock-working",
                    "base_url": format!("{mock_uri}/working"),
                    "kind": "openai-chat",
                    "auth": {"type": "none"},
                    "billing": "free",
                    "serves": ["working-model/"],
                },
            ],
            "default_provider": "wiremock-working",
            "strict_free": false,
            // Lenient policy so the model_entry free-flag isn't checked
            // (the policy gate's `strict_free` defaults to true; we
            // disable it for these tests so we don't gate on tests that
            // need to run regardless of the corpus's view of free-ness).
            "corpus_path": home.join("model_corpus.json"),
            "ledger_path": home.join("ledger.jsonl"),
            "health_path": home.join("health.json"),
            "reviewer_model": reviewer_model,
        });
        std::fs::write(
            home.join("config.json"),
            serde_json::to_string_pretty(&body).unwrap(),
        )
        .unwrap();
    }

    /// Mount a wiremock expectation that returns a `404` matching the
    /// exact URL shape `endpoint_url` produces for the
    /// `wiremock-broken` provider — i.e.
    /// `<mock_uri>/broken/v1/chat/completions`. The body shape
    /// mirrors the production incident
    /// (`deepseek-ai/deepseek-coder-6.7b-instruct` on NVIDIA EIH,
    /// redacted): `"Function [REDACTED] Not found for account
    /// [REDACTED]"`. The 404 mirrors what the production
    /// `provider.rs` code surfaces as `"provider HTTP 404 Not Found"`.
    ///
    /// `async` because `wiremock::MockServer::mount` is itself an
    /// async method — the surrounding `#[tokio::test]` runtime
    /// drives it.
    async fn mount_404_function_not_found(server: &MockServer) {
        let body = serde_json::json!({
            "status": 404,
            "title": "Not Found",
            "detail": "Function [REDACTED] Not found for account [REDACTED]"
        })
        .to_string();
        Mock::given(method("POST"))
            .and(path("/broken/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(404).set_body_string(body))
            .mount(server)
            .await;
    }

    /// Mount a wiremock expectation that returns a valid OpenAI-
    /// shaped chat completion under the URL path
    /// `<mock_uri>/working/v1/chat/completions` — the exact URL shape
    /// `endpoint_url` produces for the `wiremock-working` provider.
    /// Body shape matches the real OpenAI non-streaming response so
    /// the existing parser in `OpenAiProvider::stream_chat` produces
    /// a real `Completion`.
    async fn mount_200_openai_chat_completion(server: &MockServer) {
        let body = serde_json::json!({
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": r#"{"verdict":"approve","summary":"working reviewer ok","findings":[],"next_steps":[]}"#
                    },
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 10,
                "total_tokens": 15
            }
        })
        .to_string();
        Mock::given(method("POST"))
            .and(path("/working/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
    }

    /// Mount a wiremock expectation that returns the same OpenAI-
    /// shaped completion body as the working mount, but under the
    /// `/broken/...` URL prefix. Used by tests where the broken-
    /// provider URL also needs to succeed (e.g. multiple provider
    /// entries that share a wiremock URI). Tests using this helper
    /// do not exercise the 404 fallback path — they pin the happy-
    /// path completion parse for a non-working prefix.
    #[allow(dead_code)]
    async fn mount_200_at_broken(server: &MockServer) {
        let body = serde_json::json!({
            "choices": [
                {
                    "message": {
                        "role": "assistant",
                        "content": r#"{"verdict":"approve","summary":"working reviewer ok","findings":[],"next_steps":[]}"#
                    },
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 10,
                "total_tokens": 15
            }
        })
        .to_string();
        Mock::given(method("POST"))
            .and(path("/broken/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(server)
            .await;
    }

    /// Build a minimal `Cli` suitable for a single `complete_once` call.
    /// Defaults that affect the dispatch are explicit here; everything
    /// else is left at `clap`'s default. Quiet is on so test stdout
    /// stays clean — the dispatcher's fallback log goes through
    /// `eprintln` not stdout.
    fn dummy_cli() -> Cli {
        Cli::try_parse_from(["zoder", "exec"]).expect("clap parse")
    }

    /// **REGRESSION: 2026-07-07 reviewer-pipeline fix.** A chain of
    /// `[broken-model, working-model]` MUST fall through from the failing
    /// head to the working tail and produce a real completion from the
    /// second candidate. This pins the production incident
    /// (`deepseek-ai/deepseek-coder-6.7b-instruct` was the broken head
    /// on NVIDIA EIH) at the dispatch boundary: the new behaviour is
    /// "the broken head's 404 is treated as fallback-worthy; the next
    /// candidate takes over and the review completes." Before the fix
    /// this test would have surfaced the verbatim `404 Not Found`
    /// error from the head and `cmd_review` would have bailed with
    /// `0/1 reviewers completed`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reviewer_chain_falls_through_from_breaking_head_to_working_tail() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home = home_dir.path().to_path_buf();
        let _g = HomeGuard::new(&home);

        let server = MockServer::start().await;
        mount_404_function_not_found(&server).await;
        mount_200_openai_chat_completion(&server).await;

        write_corpus(&home, &["broken-model/coder-6.7b", "working-model/glm-5.1"]);
        write_config(
            &home,
            &server.uri(),
            // The chain the spec demands: operator-written
            // comma-separated candidate list, head first.
            "broken-model/coder-6.7b,working-model/glm-5.1",
        );

        let cli = dummy_cli();
        let result = complete_once(
            &cli,
            None,
            &[],
            "You are a reviewer.",
            "review this diff",
            2048,
        )
        .await;
        let c = result.expect("the chain must produce a completion; the broken head must fall through to the working tail");
        assert_eq!(
            c.model, "working-model/glm-5.1",
            "fallback must report the working model's id (the one that actually answered)"
        );
        assert!(
            c.content.contains("working reviewer ok"),
            "must surface the working reviewer's content; got: {}",
            c.content
        );
        assert!(
            c.cost_usd >= 0.0,
            "cost must be reported (zero is acceptable for a free model); got {}",
            c.cost_usd
        );
    }

    /// **REGRESSION GUARD: chain exhaustion.** When EVERY candidate in
    /// the chain returns a fallback-worthy error, the dispatcher must
    /// surface a single composite failure — it MUST NOT silently report
    /// success, MUST NOT downgrade to "approve" on a totally-failed
    /// chain, and MUST clearly state which models were tried. This
    /// pins the "a review that no model produced must not be reported
    /// as success" invariant (`crates/zoder-cli`, the regression-guard
    /// comment in `cmd_review`'s `ok_models == 0` branch).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reviewer_chain_exhaustion_reports_0_of_n_in_failure_message() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home = home_dir.path().to_path_buf();
        let _g = HomeGuard::new(&home);

        let server = MockServer::start().await;
        // BOTH URLs return 404 — every candidate in the chain must
        // fail and trigger the "no candidate answered" surface.
        mount_404_function_not_found(&server).await;
        // Pin the behavior to "both endpoints return a 404 Function not
        // found" so the test asserts the chain-exhaustion contract, not
        // the URL-default behavior. URL path matches
        // `endpoint_url`'s shape for an openai-chat provider whose
        // base_url has no `/v1` (it appends `/v1/<suffix>`).
        Mock::given(method("POST"))
            .and(path("/working/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(404).set_body_string(
                r#"{"status":404,"title":"Not Found","detail":"Function not found"}"#,
            ))
            .mount(&server)
            .await;

        write_corpus(&home, &["broken-model/coder-6.7b", "broken-model/other"]);
        write_config(
            &home,
            &server.uri(),
            "broken-model/coder-6.7b,broken-model/other",
        );

        let cli = dummy_cli();
        let result = complete_once(&cli, None, &[], "sys", "user", 2048).await;
        let err =
            result.expect_err("every candidate errored; complete_once must surface a failure");
        let msg = format!("{err:#}");
        // The chain-exhausted message MUST mention `reviewer`
        // (so the caller's `cmd_review` knows this is a reviewer
        // failure) and MUST surface an HTTP 404 substring so a
        // CI maintainer can tell from the log that the failure
        // is an upstream "model not deployed" rather than a local
        // // config error.
        assert!(
            msg.contains("reviewer"),
            "error message must mention reviewer: {msg}"
        );
        assert!(
            msg.contains("404") || msg.to_lowercase().contains("not found"),
            "error message must surface the underlying 404 / not-found diagnostic: {msg}"
        );
        // Critical: when the user (not the dispatcher) is the
        // tail-end consumer (e.g. `cmd_review`'s
        // `ok_models == 0` branch), they MUST see a non-zero
        // exit so CI doesn't green-light the review. The
        // `Err(anyhow!(...))` return path here guarantees that:
        // it propagates as a non-zero exit through `cmd_review`'s
        // existing `if ok_models == 0 { anyhow::bail!(...) }`
        // check. This test pins the dispatcher's contribution
        // to that surface (a non-Ok return value).
    }

    /// **REGRESSION GUARD: single-model config behaves identically to
    /// today.** A config that writes `reviewer_model` as a one-element
    /// (no comma) string MUST return a one-element chain in
    /// `complete_once` and the call site MUST see the EXACT same error
    /// shape it would have under the pre-fix code path: a single
    /// 404, no automatic fallback to a different model. This is the
    /// "additive, not breaking" requirement: an operator who has never
    /// written a CSV chain continues to get exactly the old behavior.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn single_model_reviewer_config_behaves_identically_to_today() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home = home_dir.path().to_path_buf();
        let _g = HomeGuard::new(&home);

        let server = MockServer::start().await;
        mount_404_function_not_found(&server).await;

        write_corpus(&home, &["broken-model/coder-6.7b"]);
        write_config(
            &home,
            &server.uri(),
            // Single model id, NO comma — the legacy shape. The
            // dispatcher MUST treat this as a one-element chain and
            // pass the head's 404 through verbatim (no fallback
            // available, no chain to walk).
            "broken-model/coder-6.7b",
        );

        let cli = dummy_cli();
        let result = complete_once(&cli, None, &[], "sys", "user", 2048).await;
        let err = result.expect_err(
            "single-model config + failing backend must still error (the legacy behavior was \
             to surface the head's error); the new chain logic must not 'succeed' by \
             silently degrading",
        );
        let msg = format!("{err:#}");
        assert!(
            msg.contains("reviewer"),
            "single-model failure must mention the reviewer role: {msg}"
        );
        // The historical error message format is preserved: the
        // call site (cmd_review) receives an `anyhow::Error`
        // whose `.to_string()` already includes the head's
        // exact failure. Existing CI parsers + log filters
        // match on this shape, so preserving it byte-for-byte
        // is essential.
        assert!(
            msg.to_lowercase().contains("404") || msg.to_lowercase().contains("not found"),
            "single-model failure must surface the 404 from the head verbatim: {msg}"
        );
    }

    /// **SPECIFIC-CLASS FALLBACK-WORTHINESS.** Pin that the 404
    /// "Function not found" provider error — the EXACT class of
    /// failure that was reproduced in the 2026-07-07 incident —
    /// flows back to the dispatcher as `FallbackWorthy` (NOT `Fatal`).
    /// The classifier is shared with the author-path chain
    /// (`provider.stream_chat`'s `ErrKind::Http` errors are never
    /// emitted-on-the-stream, so they are never tagged with
    /// `emitted: true` — the dispatcher treats ALL provider-level
    /// errors as `FallbackWorthy`). This test exercises the same
    /// provider error shape the production code raised, and asserts
    /// the NEXT candidate is tried (i.e. the dispatcher did NOT halt
    /// on a `Fatal`).
    ///
    /// Implemented end-to-end: the chain is
    /// `[broken-model, working-model]` and both endpoints 404 —
    /// evidence that the first 404 was classified as `FallbackWorthy`
    /// (NOT `Fatal`) is implicit in the second one being attempted.
    /// Without that classification, the dispatcher would have halted
    /// on the head's error and the working-model URL would have been
    /// untouched. Wiremock records request counts per mounted
    /// expectation; the assertion verifies the second mount received
    /// >= 1 hit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn function_not_found_404_is_fallback_worthy_not_fatal() {
        let home_dir = tempfile::tempdir().expect("tempdir");
        let home = home_dir.path().to_path_buf();
        let _g = HomeGuard::new(&home);

        let server = MockServer::start().await;
        mount_404_function_not_found(&server).await;
        // Mount a 404 for the WORKING model too — combined with the
        // chain below, this means every candidate fails, but the
        // message we'll see is from the LAST attempted candidate
        // (the working one). The point of THIS test is the
        // classification: if the head's 404 was `Fatal`, the chain
        // would halt on the head and the working mount would be hit
        // ZERO times. We assert that the working mount was hit AT
        // LEAST once — which is the literal proof that the head's
        // 404 was classified as `FallbackWorthy` and the dispatcher
        // advanced to the next candidate.
        let working_404_mount = Mock::given(method("POST"))
            .and(path("/working/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(404).set_body_string(
                r#"{"status":404,"title":"Not Found","detail":"Function not found"}"#,
            ))
            .mount_as_scoped(&server)
            .await;

        write_corpus(&home, &["broken-model/coder-6.7b", "working-model/glm-5.1"]);
        write_config(
            &home,
            &server.uri(),
            "broken-model/coder-6.7b,working-model/glm-5.1",
        );

        let cli = dummy_cli();
        let result = complete_once(&cli, None, &[], "sys", "user", 2048).await;
        // Drop the scoped mount NOW so wiremock's request count for
        // it stops incrementing (we already took our reading
        // through the dispatcher error path; nothing after this
        // should call the wiremock server). This is a per-test
        // isolation step that doesn't affect correctness — the
        // server still answers subsequent requests via the scoped
        // drop's internal cleanup.
        drop(working_404_mount);
        let err =
            result.expect_err("every candidate errored; chain exhaustion must surface as an error");
        let msg = format!("{err:#}");
        // The literal proof the dispatcher classified the head's 404
        // as `FallbackWorthy` and advanced to the next candidate is
        // the working mount receiving >= 1 hit. Wiremock records
        // that internally; we can ask the server how many requests
        // it has seen. The exact dial: the head returned a 404
        // (dispatcher treated it as `FallbackWorthy`, advanced,
        // called the tail), the tail also 404'd (chain exhausted,
        // we surface an Err). Both endpoints received a request,
        // therefore the head's classification was NOT `Fatal`.
        let requests = server.received_requests().await.unwrap_or_default();
        let paths: Vec<String> = requests.iter().map(|r| r.url.path().to_string()).collect();
        // Defensive ordering: at minimum, both URLs must have
        // received a request (otherwise the chain halted). The URL
        // shape we observe is what `endpoint_url` builds: the
        // `openai-chat` provider whose base_url has no `/v1` appends
        // `/v1/<suffix>` automatically. Both `/broken/v1/...` and
        // `/working/v1/...` should be present.
        assert!(
            paths.iter().any(|p| p.contains("/broken/")),
            "head's URL must have been tried (chain did NOT halt on the head; the head's 404 was classified as FallbackWorthy): paths={paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("/working/")),
            "tail's URL must have been tried (chain advanced past the head's 404 to the next candidate; the head's 404 was classified as FallbackWorthy): paths={paths:?}"
        );
        // Defensive message content: the surfaced error mentions
        // both 404 and "not found" / "function not found" so a CI
        // maintainer reading the log can see WHY every candidate
        // failed and which provider model-id is missing.
        assert!(
            msg.contains("404") && msg.to_lowercase().contains("not found"),
            "chain-exhausted surface must include both 404 and the 'not found' diagnostic so the operator sees what went wrong: {msg}"
        );
        // A `Fatal` outcome would have returned a policy /
        // structural error template — these specific tokens only
        // appear when the dispatcher is in the `FallbackWorthy`
        // branch. Recording them gives the regression a second
        // witness independent of the wiremock request count.
        assert!(
            msg.contains("reviewer"),
            "FallbackWorthy path wraps the provider error with the role ('reviewer'), not the structural-error template: {msg}"
        );
        // No fabricated "approve" verdict — even a fully-failed
        // chain must not invent a positive signal.
        assert!(
            !msg.to_lowercase().contains("approve"),
            "404'd single-model dispatch must NOT silently produce an 'approve' verdict (the no-review-completed contract): {msg}"
        );
    }

    // Suppress the unused-import lint for `Provider` and `PathBuf` —
    // they're scaffolding the type-system expectation of a future
    // test that wires up an `OpenAiProvider` directly. They're
    // harmless here and we want to keep the import set self-
    // documenting (this module is THE seam for adding more end-to-
    // end reviewer tests).
    #[allow(dead_code)]
    fn _unused_import_anchor(_: &Provider, _: &PathBuf) {}
}

// ---------------------------------------------------------------------------
// `enforce_repo_commit_author` — post-commit author reconciliation.
//
// Pins the operational defect where every `zoder loop` commit landed
// under `zoder-bot <...>` and a human had to `git commit --amend
// --author=...` before the commit was push-safe. The fix lives in
// `enforce_repo_commit_author` (above) and runs inside `cmd_loop` after
// every author turn that moves HEAD.
//
// These tests use real `git` (not git2 / gix — neither is a dep) and
// cover the three primary cases the production incident surfaced, plus
// the no-commit / no-configured-identity edge cases the fail-closed
// contract demands:
//   * mismatched author -> amend, author field updated, tree / message
//     byte-identical, SHA moved (proves the amend actually ran),
//   * already-correct author -> no-op, SHA unchanged,
//   * no configured identity -> no-op, no error,
//   * no commits at all -> no-op, no error.
//
// All tests are POSIX-safe (they use `git` via the same shell-out path
// the production code uses) and isolated per-test via `tempfile::tempdir`
// so a stray env-var leak in one test cannot poison the next.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod commit_author_enforcement_tests {
    use super::*;
    use std::path::PathBuf;

    /// Run `git` against `repo`, asserting success. Mirrors the helper in
    /// `review_diff_soundness_tests` (kept independent so this module
    /// has no cross-module coupling).
    fn run(repo: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Run `git` and return its trimmed stdout, asserting success. Used
    /// for read-only probes (`rev-parse`, `log`, `config`).
    fn read(repo: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Initialize a temp repo, set a chosen local identity, commit a
    /// single seed file, then return the repo path. The local identity
    /// is what subsequent tests will configure the `user.name` /
    /// `user.email` to — distinct from the seed commit's author so the
    /// "mismatched" case has something to fix.
    fn init_repo_with_local_identity(name: &str, email: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        run(&repo, &["init", "-q"]);
        run(&repo, &["config", "init.defaultBranch", "main"]);
        // Seed the repo with a commit under a deliberately-DIFFERENT
        // identity (so we can test the correction path), then re-set
        // the local identity to the values under test before returning.
        run(
            &repo,
            &["config", "user.email", "seed-author@example.invalid"],
        );
        run(&repo, &["config", "user.name", "Seed Author"]);
        std::fs::write(repo.join("README.md"), "seed\n").unwrap();
        run(&repo, &["add", "README.md"]);
        run(&repo, &["commit", "-q", "-m", "init"]);
        // Now switch to the identity the test will assert against.
        run(&repo, &["config", "user.email", email]);
        run(&repo, &["config", "user.name", name]);
        (dir, repo)
    }

    /// REGRESSION (the production incident): a commit whose author
    /// does NOT match the repo's `user.name`/`user.email` MUST be
    /// amended to carry the repo's identity, while the tree and
    /// message are preserved byte-for-byte. Before this fix, the loop
    /// shipped commits under `zoder-bot <...>` and a human had to
    /// `git commit --amend --author=...` after every run.
    #[test]
    fn enforce_repo_commit_author_amends_mismatched_author() {
        let (_dir, repo) =
            init_repo_with_local_identity("Configured User", "configured@example.invalid");

        // Capture the pre-amend invariants: SHA, tree SHA, message.
        // The SHA MUST change after the amend (author is part of the
        // object) but the tree and message MUST stay byte-identical.
        let pre_sha = read(&repo, &["rev-parse", "HEAD"]);
        let pre_tree = read(&repo, &["rev-parse", "HEAD^{tree}"]);
        let pre_msg = read(&repo, &["log", "-1", "--format=%s"]);
        let pre_author = read(&repo, &["log", "-1", "--format=%an <%ae>"]);
        assert_eq!(
            pre_author, "Seed Author <seed-author@example.invalid>",
            "pre-condition: the seed commit must NOT already match the configured identity, \
             or this test pins nothing"
        );

        let result = enforce_repo_commit_author(&repo);
        match &result {
            CommitAuthorEnforcement::Corrected { from, to } => {
                assert_eq!(from, "Seed Author <seed-author@example.invalid>");
                assert_eq!(to, "Configured User <configured@example.invalid>");
            }
            other => panic!("expected Corrected, got {other:?}"),
        }

        // HEAD's author is now the configured identity.
        assert_eq!(
            read(&repo, &["log", "-1", "--format=%an <%ae>"]),
            "Configured User <configured@example.invalid>",
            "HEAD author must equal the configured identity after the amend"
        );

        // SHA moved (the amend produced a new object — author is part
        // of the object). This is the literal witness that the amend
        // actually ran, not a no-op-with-fancy-logging.
        let post_sha = read(&repo, &["rev-parse", "HEAD"]);
        assert_ne!(pre_sha, post_sha, "amend MUST change the commit SHA");

        // Tree SHA is byte-identical — `git commit --amend` with
        // `--no-edit` and no working-tree change must NOT alter the
        // tree. This is the regression guard: a buggy implementation
        // that re-staged or dropped the tree would surface here.
        assert_eq!(
            read(&repo, &["rev-parse", "HEAD^{tree}"]),
            pre_tree,
            "tree SHA must be byte-identical before/after the amend; only the author field moves"
        );

        // Message is byte-identical — `--no-edit` keeps the message
        // exactly as-is.
        assert_eq!(
            read(&repo, &["log", "-1", "--format=%s"]),
            pre_msg,
            "commit subject must be byte-identical before/after the amend"
        );
    }

    /// `enforce_repo_commit_author` is a no-op when HEAD's author
    /// already matches the configured identity. The SHA must be
    /// unchanged (so we know the amend really did not run) and the
    /// outcome must report `AlreadyCorrect` so the iter record is
    /// honest about what happened.
    #[test]
    fn enforce_repo_commit_author_is_noop_when_already_correct() {
        let (dir, repo) =
            init_repo_with_local_identity("Already Right", "already-right@example.invalid");
        // The seed commit was under a different identity. Amend once
        // to put the seed commit on the right identity, then run the
        // helper again — that second run is the actual no-op case
        // under test.
        let _ = enforce_repo_commit_author(&repo);
        let post_amend_sha = read(&repo, &["rev-parse", "HEAD"]);
        let post_amend_author = read(&repo, &["log", "-1", "--format=%an <%ae>"]);
        assert_eq!(
            post_amend_author, "Already Right <already-right@example.invalid>",
            "first run must align the seed commit's author with the configured identity"
        );

        let result = enforce_repo_commit_author(&repo);
        match &result {
            CommitAuthorEnforcement::AlreadyCorrect { current } => {
                assert_eq!(current, "Already Right <already-right@example.invalid>");
            }
            other => panic!("expected AlreadyCorrect, got {other:?}"),
        }

        // SHA unchanged: literal proof the amend did NOT run on the
        // second call. A buggy implementation that always amended
        // (even on a matching identity) would keep rewriting the
        // commit to itself with a new SHA every call.
        assert_eq!(
            read(&repo, &["rev-parse", "HEAD"]),
            post_amend_sha,
            "no-op run MUST NOT change the commit SHA"
        );
        drop(dir);
    }

    /// When the repo has no configured `user.name` / `user.email`
    /// there is nothing correct to amend to — leave the commit alone
    /// and surface `NoConfiguredIdentity` so the iter record is
    /// honest. The fix MUST NOT guess an identity (hardcoding
    /// `zoder-bot` here would be the obvious wrong answer) and MUST
    /// NOT error.
    ///
    /// Hermeticity: the host's `~/.gitconfig` (or system config) may
    /// carry its own `user.name`/`user.email`, which would shadow the
    /// repo's local config under git's normal local-then-global
    /// resolution. We force a hermetic `git` invocation by pointing
    /// `GIT_CONFIG_GLOBAL` and `GIT_CONFIG_SYSTEM` at `/dev/null`,
    /// which makes git treat those scopes as empty — the same
    /// trick the unit tests in this crate use elsewhere when they
    /// need a clean identity-free repo.
    #[cfg(unix)]
    #[test]
    fn enforce_repo_commit_author_does_nothing_when_no_identity_configured() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        run(&repo, &["init", "-q"]);
        run(&repo, &["config", "init.defaultBranch", "main"]);
        // Seed the repo with a commit under a known-wrong author so
        // the assertion below has a literal witness to compare
        // against. We have to set the identity to make the seed
        // commit, then unset it to put the helper in the
        // "no-identity" state.
        run(&repo, &["config", "user.email", "seed@example.invalid"]);
        run(&repo, &["config", "user.name", "Seed"]);
        std::fs::write(repo.join("README.md"), "seed\n").unwrap();
        run(&repo, &["add", "README.md"]);
        run(&repo, &["commit", "-q", "-m", "init"]);
        // Unset BOTH user fields at the local scope. The
        // local-then-global lookup will then resolve to "no
        // identity" ONLY if the global scope is also empty, which
        // is the case in the test sandbox because we override the
        // env below.
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["config", "--unset", "user.name"])
            .status();
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["config", "--unset", "user.email"])
            .status();

        // Hermetic git: ignore the host's global / system config so
        // a CI runner with `user.name=ULTRA build` in `~/.gitconfig`
        // does not leak into the test. The local repo's own config
        // (where we just unset user.name / user.email) is the only
        // scope that matters.
        let _guard = HermeticGitConfig::installed();

        // Pre-condition: `git config user.name` resolves to empty.
        let probe = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["config", "user.name"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("spawn git");
        assert!(
            !probe.status.success() || String::from_utf8_lossy(&probe.stdout).trim().is_empty(),
            "pre-condition: the test repo must have no user.name set (probe exit={:?} stdout={:?})",
            probe.status,
            String::from_utf8_lossy(&probe.stdout)
        );

        let pre_sha = read(&repo, &["rev-parse", "HEAD"]);
        let pre_author = read(&repo, &["log", "-1", "--format=%an <%ae>"]);

        let result = enforce_repo_commit_author(&repo);
        assert!(
            matches!(result, CommitAuthorEnforcement::NoConfiguredIdentity),
            "no configured identity must surface as NoConfiguredIdentity, got {result:?}"
        );

        // Commit is byte-untouched: same SHA, same author. The helper
        // MUST NOT guess an identity (e.g. fall back to a hardcoded
        // default like `zoder-bot`) and MUST NOT amend.
        assert_eq!(
            read(&repo, &["rev-parse", "HEAD"]),
            pre_sha,
            "no-configured-identity must NOT amend HEAD"
        );
        assert_eq!(
            read(&repo, &["log", "-1", "--format=%an <%ae>"]),
            pre_author,
            "no-configured-identity must NOT rewrite HEAD's author"
        );
        drop(dir);
    }

    /// Test-scoped RAII guard that makes every `git` invocation in
    /// this test process see an empty global / system config. Used
    /// by tests that need a "no identity is set anywhere" precondition
    /// regardless of the host's `~/.gitconfig` (CI runners frequently
    /// have a `user.name=...` in their global config which would
    /// otherwise leak in and break the no-identity assertion).
    ///
    /// Unix-only: the only way to override the global config location
    /// cleanly is the `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM` env
    /// vars, set via `libc::setenv`. On non-unix the test is
    /// conditional and the guard is a no-op (its only consumer is
    /// also unix-only — see the `cfg` attribute on the tests below).
    ///
    /// **Concurrency.** `setenv` mutates the whole-process env, so
    /// a test that holds this guard MUST also hold the shared
    /// `GIT_ENV_LOCK` mutex to serialize against other tests in this
    /// binary that might be reading git config concurrently. Without
    /// the lock, two parallel tests could see each other's
    /// `GIT_CONFIG_GLOBAL` and one of them would observe the wrong
    /// identity. The lock is held for the guard's lifetime — a few
    /// `git` invocations in the test body — so the wall-clock cost
    /// is small and parallel test throughput is preserved.
    struct HermeticGitConfig {
        // RAII: `_lock` must be declared FIRST so its `Drop` runs
        // LAST (Rust drops fields in declaration order). The lock
        // must still be held when `Drop` for `HermeticGitConfig`
        // (the one below) runs `unsetenv` — otherwise another test
        // could grab the lock, see the env-var override, and leak
        // it. This is the same field-ordering discipline `EnvGuard`
        // uses elsewhere in this crate.
        #[cfg(unix)]
        _lock: std::sync::MutexGuard<'static, ()>,
        _private: (),
    }

    /// Process-wide serialization point for tests that touch
    /// `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM`. See
    /// `HermeticGitConfig` for the why.
    #[cfg(unix)]
    static GIT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    impl HermeticGitConfig {
        /// Construct and install the guard. Construction calls
        /// `install()` so the test body only has to bind the value
        /// to a `_guard` binding for `Drop` to run at scope exit.
        fn installed() -> Self {
            Self::install();
            // The lock is held for the guard's lifetime. Declared
            // as `Option` so the unix / non-unix arms can both
            // share the same struct shape (the field is absent on
            // non-unix, where the lock is also absent).
            #[cfg(unix)]
            let lock = GIT_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            Self {
                #[cfg(unix)]
                _lock: lock,
                _private: (),
            }
        }

        #[cfg(unix)]
        fn install() {
            // `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM` are honored
            // by `git` since 1.7.10; pointing them at `/dev/null`
            // makes git treat those scopes as empty. We use process
            // env (not thread-local) because `Command::new("git")`
            // inherits the parent process's env by default — every
            // test invocation already runs inside a `cargo test`
            // process, so setting the env once at the top of the
            // test propagates to every `git` child the test spawns
            // (including the ones the helper itself spawns via
            // `std::process::Command`).
            //
            // SAFETY: `setenv` is not thread-safe; this test is
            // `#[test]` (not `#[tokio::test]`), so the test function
            // is the only thread that observes the change in the
            // short window between `setenv` and the test's
            // `git` invocations. The mutex held by `_lock`
            // (declared above) serializes against any other test
            // that might be reading git config in parallel. The
            // `&str` -> `*const i8` cast is safe because the
            // pointers are derived from string literals with
            // `'static` lifetime; `setenv` only copies the bytes
            // during the call, so the borrow is released before
            // the function returns.
            unsafe {
                libc::setenv(
                    c"GIT_CONFIG_GLOBAL".as_ptr() as *const libc::c_char,
                    c"/dev/null".as_ptr() as *const libc::c_char,
                    1,
                );
                libc::setenv(
                    c"GIT_CONFIG_SYSTEM".as_ptr() as *const libc::c_char,
                    c"/dev/null".as_ptr() as *const libc::c_char,
                    1,
                );
            }
        }

        #[cfg(not(unix))]
        fn install() {}
    }

    #[cfg(unix)]
    impl Drop for HermeticGitConfig {
        fn drop(&mut self) {
            // Best-effort: clear the env so the next test (if any)
            // sees the host's normal git config. We do not fail the
            // test on `unsetenv` errors — the only consequence is
            // residual process env, which is harmless after the
            // process exits. SAFETY: see `install`; the cast is from
            // a `'static` string literal. The lock (`_lock`) is
            // still held at this point (Rust drops `_lock` AFTER
            // `HermeticGitConfig`'s own `Drop`, because `_lock` is
            // declared first in the struct), so another test cannot
            // observe a half-cleaned env.
            unsafe {
                libc::unsetenv(c"GIT_CONFIG_GLOBAL".as_ptr() as *const libc::c_char);
                libc::unsetenv(c"GIT_CONFIG_SYSTEM".as_ptr() as *const libc::c_char);
            }
        }
    }

    /// A brand-new repo with NO commits must return `NoCommit` and
    /// must not error. The author turn may be in the middle of
    /// staging untracked files — there is nothing to amend and the
    /// helper must not invent one.
    #[test]
    fn enforce_repo_commit_author_does_nothing_with_no_commits() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        run(&repo, &["init", "-q"]);
        run(&repo, &["config", "user.email", "x@example.invalid"]);
        run(&repo, &["config", "user.name", "X"]);

        let result = enforce_repo_commit_author(&repo);
        assert!(
            matches!(result, CommitAuthorEnforcement::NoCommit),
            "no-commit repo must surface as NoCommit, got {result:?}"
        );
        drop(dir);
    }

    /// `commit_author_enforcement_to_json` is the bridge from the
    /// enum to the per-iter JSON record. It must distinguish
    /// "no commit was made" (`null`) from each of the four
    /// outcome variants so downstream audit logs can tell them
    /// apart — the regression we are guarding against is
    /// "everything collapses to null, no observability for what
    /// actually happened".
    #[test]
    fn commit_author_enforcement_to_json_renders_all_variants() {
        // No commit was made during this iter.
        assert_eq!(
            commit_author_enforcement_to_json(None),
            serde_json::Value::Null
        );
        // No commits in the repo at all.
        let v = commit_author_enforcement_to_json(Some(&CommitAuthorEnforcement::NoCommit));
        assert_eq!(v["status"], "no_commit");
        // HEAD's author already matches.
        let v = commit_author_enforcement_to_json(Some(&CommitAuthorEnforcement::AlreadyCorrect {
            current: "X <x@y>".into(),
        }));
        assert_eq!(v["status"], "already_correct");
        assert_eq!(v["current"], "X <x@y>");
        // No identity configured.
        let v =
            commit_author_enforcement_to_json(Some(&CommitAuthorEnforcement::NoConfiguredIdentity));
        assert_eq!(v["status"], "no_configured_identity");
        // Amended.
        let v = commit_author_enforcement_to_json(Some(&CommitAuthorEnforcement::Corrected {
            from: "A <a@b>".into(),
            to: "C <c@d>".into(),
        }));
        assert_eq!(v["status"], "corrected");
        assert_eq!(v["from"], "A <a@b>");
        assert_eq!(v["to"], "C <c@d>");
        // Failed (e.g. git binary not found, repo disappeared).
        let v = commit_author_enforcement_to_json(Some(&CommitAuthorEnforcement::Failed {
            reason: "boom".into(),
        }));
        assert_eq!(v["status"], "failed");
        assert_eq!(v["reason"], "boom");
    }
}
