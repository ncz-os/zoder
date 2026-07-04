//! Local subscription-utilization tracking.
//!
//! Most zoder users do not run a fleet-wide MNEMOS/KNEMON service, so
//! per-provider rate-limit telemetry has to be tracked **on the box** that
//! actually makes calls. This module is the self-contained piece:
//!
//! 1. [`parse_headers`] — turn a flat `HeaderMap` (or any
//!    `Iterator<Item = (&str, &str)>`) into a [`RateLimitSnapshot`] for the
//!    known vendors (OpenAI Codex `x-codex-*`, Anthropic
//!    `anthropic-ratelimit-unified-*`, plus the older `anthropic-ratelimit-*`
//!    request/token variants).
//! 2. [`RouteDecision`] + [`decide`] — pure routing function that consumes a
//!    snapshot and a [`RouteKnobs`] and returns `PreferSub` / `FallBackToFree`
//!    / `Chargeback` so the router can maximize paid-subscription usage
//!    without busting the cap.
//! 3. [`UtilizationStore`] — append-only JSON store keyed by
//!    `(provider, account_id, plan)` at `~/.zoder/utilization.json`. Callers
//!    feed it fresh snapshots; the store resolves "reset_at passed" by
//!    reading the persisted `reset_at_epoch` at lookup time, so callers do
//!    not have to keep a clock.
//!
//! Network is intentionally **not** in here. The library is pure parse +
//! decide + persist; the CLI / engine layer is responsible for pulling
//! headers off responses and handing them in. This keeps it testable
//! without a mock server and keeps the surface area auditable.
//!
//! ## Routing model
//!
//! `used = max(primary_used_pct, secondary_used_pct)`; if the snapshot's
//! `reset_at_epoch` is in the past, `used` is treated as `0` (the window
//! has rolled over). Two thresholds:
//!
//! * `use_target` (default 80) — below this, prefer the paid subscription.
//! * `cap_guard`  (default 85) — at/above this, fall back to the free tier
//!   until reset (mode `block`) or gate on a dollar budget (mode
//!   `chargeback`). Between `use_target` and `cap_guard` is a hysteresis
//!   band where we keep using the subscription as long as the guard has not
//!   tripped.
//!
//! Config knobs are per `(provider, account_id, plan)` so a personal
//! account that lives on a different budget than a team seat can be tuned
//! without affecting other accounts on the same machine.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Public defaults / tunables.
// ---------------------------------------------------------------------------

/// Default `use_target` (percent). Below this we route to the paid
/// subscription without hesitation.
pub const DEFAULT_USE_TARGET: f64 = 80.0;

/// Default `cap_guard` (percent). At/above this we route OFF the paid
/// subscription (free fallback or chargeback) to avoid busting the cap.
pub const DEFAULT_CAP_GUARD: f64 = 85.0;

/// Default `budget_mode`.
pub const DEFAULT_BUDGET_MODE: BudgetMode = BudgetMode::Block;

/// On-disk filename under `$ZODER_HOME` (or `~/.zoder`).
pub const UTILIZATION_FILENAME: &str = "utilization.json";

// ---------------------------------------------------------------------------
// Enums.
// ---------------------------------------------------------------------------

/// Budget policy for a `(provider, account_id, plan)` triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetMode {
    /// Hard-block the subscription at/above `cap_guard`; route to free tier.
    Block,
    /// Allow the subscription to keep spending past `cap_guard` until the
    /// chargeback budget is exhausted, then fall back to free.
    Chargeback,
}

impl Default for BudgetMode {
    fn default() -> Self {
        DEFAULT_BUDGET_MODE
    }
}

/// Routing verdict. The router turns this into a concrete model pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteDecision {
    /// Use the paid subscription for this request.
    PreferSub,
    /// Use the free-tier fallback for this request.
    FallBackToFree,
    /// Subscription is exhausted; we are operating inside a chargeback
    /// budget and the dollar budget is also gone — same effect as
    /// `FallBackToFree` but tagged so the ledger can record the cause.
    Chargeback,
}

impl RouteDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            RouteDecision::PreferSub => "prefer_sub",
            RouteDecision::FallBackToFree => "fall_back_to_free",
            RouteDecision::Chargeback => "chargeback",
        }
    }
}

/// Known providers. We carry the OpenAI Codex variant (`openai_codex`)
/// explicitly because its header shape (`x-codex-*`) is distinct from the
/// plain OpenAI chat-completions surface (`openai`).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    Other,
    Openai,
    OpenaiCodex,
    Anthropic,
    /// MiniMax (no rate-limit headers; tracked via local counter). Has
    /// no `parse_*` counterpart in the header-fed path because MiniMax
    /// does not publish `x-codex-*` / `anthropic-ratelimit-*` headers;
    /// the counter-fed path in `UtilizationStore::record_counter` is
    /// what surfaces its utilization.
    MiniMax,
}

impl Provider {
    /// Heuristic from an arbitrary header name; used by callers that
    /// already know which provider handled the request and just want a
    /// typed value. Returns `None` when no header in the input hints at a
    /// known vendor.
    pub fn detect(headers: &dyn HeaderLookup) -> Option<Provider> {
        if headers.get("x-codex-plan-type").is_some()
            || headers.get("x-codex-active-limit").is_some()
            || headers.get("x-codex-primary-used-percent").is_some()
        {
            return Some(Provider::OpenaiCodex);
        }
        if headers
            .get("anthropic-ratelimit-unified-5h-status")
            .is_some()
            || headers.get("anthropic-ratelimit-unified-status").is_some()
            || headers.get("anthropic-ratelimit-requests-limit").is_some()
            || headers
                .get("anthropic-ratelimit-tokens-remaining")
                .is_some()
            || headers
                .get("anthropic-ratelimit-unified-5h-utilization")
                .is_some()
        {
            return Some(Provider::Anthropic);
        }
        // OpenAI plain chat-completions publishes `x-ratelimit-*` headers;
        // we don't parse those in detail (no per-window structure), but we
        // do tag them so callers can record a presence-only sighting.
        if headers.get("x-ratelimit-limit-requests").is_some()
            || headers.get("x-ratelimit-limit-tokens").is_some()
            || headers.get("x-ratelimit-remaining-requests").is_some()
            || headers.get("x-ratelimit-remaining-tokens").is_some()
        {
            return Some(Provider::Openai);
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Header abstraction.
// ---------------------------------------------------------------------------

/// Minimal cross-version header lookup. Implemented for `reqwest::HeaderMap`
/// (when callers wrap one) and for `&[(String, String)]` slices so unit tests
/// don't need reqwest in scope. Key matching is **case-insensitive** per
/// RFC 7230.
pub trait HeaderLookup {
    fn get(&self, name: &str) -> Option<&str>;
}

impl HeaderLookup for BTreeMap<String, String> {
    fn get(&self, name: &str) -> Option<&str> {
        // Try exact match first, then a case-insensitive linear scan. The
        // maps callers feed us are small (one response's worth of headers)
        // so a linear scan is fine and keeps the public trait dep-free.
        if let Some(v) = self.get(name) {
            return Some(v.as_str());
        }
        let needle = name.to_ascii_lowercase();
        self.iter()
            .find(|(k, _)| k.to_ascii_lowercase() == needle)
            .map(|(_, v)| v.as_str())
    }
}

// ---------------------------------------------------------------------------
// Snapshot.
// ---------------------------------------------------------------------------

/// One rate-limit window on a subscription, lifted straight off a response
/// header set. Either percent-based (Codex) or limit/remaining/reset
/// (Anthropic); both shapes project to a unified `used_percent` +
/// `window_minutes` + `reset_at_epoch` view.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowSnapshot {
    /// 0..=100 percent of the window consumed. May exceed 100 when over.
    #[serde(default)]
    pub used_percent: f64,
    /// Window length in minutes, if the provider reports it.
    #[serde(default)]
    pub window_minutes: Option<u32>,
    /// Epoch seconds at which the provider says this window rolls over.
    /// `None` when the header didn't carry one.
    #[serde(default)]
    pub reset_at_epoch: Option<i64>,
    /// Provider-published label for the window (e.g. `primary`,
    /// `secondary`, `5h`, `7d`). Optional — used by tests / debugging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Everything we know about a `(provider, account_id, plan)` triple after
/// the most recent response. Callers feed one of these to the routing
/// decision fn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitSnapshot {
    pub provider: Provider,
    pub account_id: String,
    pub plan: String,
    pub primary: Option<WindowSnapshot>,
    pub secondary: Option<WindowSnapshot>,
    /// Codex-only: whether the account currently has any credits.
    #[serde(default)]
    pub has_credits: Option<bool>,
    /// When this snapshot was observed (UTC). Callers should set this at
    /// ingest time; the routing decision uses `now()` and the snapshot's
    /// `reset_at_epoch` to decide whether the window has rolled over.
    #[serde(default)]
    pub observed_at: Option<DateTime<Utc>>,
}

impl RateLimitSnapshot {
    pub fn new(provider: Provider, account_id: impl Into<String>, plan: impl Into<String>) -> Self {
        Self {
            provider,
            account_id: account_id.into(),
            plan: plan.into(),
            ..Default::default()
        }
    }

    /// Parse a `reqwest::header::HeaderMap` (i.e. a live response) into a
    /// snapshot for `provider`. Returns `None` when the headers don't carry
    /// any window information for the provider — distinct from
    /// [`parse_headers`], which first detects the vendor from the headers
    /// themselves; here the caller already knows who handled the request.
    ///
    /// `account_id` and `plan` are caller-supplied (the providers don't
    /// always publish them on every response — keeping the parser
    /// orthogonal is what lets the same snapshot fit the store's
    /// `(provider, account_id, plan)` key without leaking provider-specific
    /// quirks).
    pub fn from_headers(
        headers: &reqwest::header::HeaderMap,
        provider: Provider,
        account_id: impl Into<String>,
        plan: impl Into<String>,
    ) -> Option<Self> {
        let view = ReqwestHeaderView(headers);
        let account_id = account_id.into();
        let plan = plan.into();
        // Reuse the vendor-specific parsers via the same one-shot entry
        // point. `parse_headers` already detects the provider from the
        // header set; when the caller disagrees, we still respect the
        // parsed result as long as the detected vendor matches the known
        // type (otherwise we'd silently downgrade an Anthropic response
        // on a Codex provider and persist it under the wrong key).
        let mut snap = parse_headers(&view, &account_id, &plan)?;
        if snap.provider != provider {
            // Reset to caller's claim but only when the headers actually
            // looked like the vendor's set; if not, drop the snapshot
            // rather than guess.
            if Provider::detect(&view) == Some(provider) {
                snap.provider = provider;
            } else {
                return None;
            }
        }
        Some(snap)
    }
}

/// Adapter that exposes a `reqwest::header::HeaderMap` through the
/// [`HeaderLookup`] trait so the existing parser entry points stay
/// callable from a live response without a copy. Case-insensitive lookup
/// is handled by [`HeaderLookup`].
struct ReqwestHeaderView<'a>(pub &'a reqwest::header::HeaderMap);

impl<'a> HeaderLookup for ReqwestHeaderView<'a> {
    fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).and_then(|v| v.to_str().ok())
    }
}

// ---------------------------------------------------------------------------
// Per-account knobs.
// ---------------------------------------------------------------------------

/// Routing knobs for one `(provider, account_id, plan)` triple.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteKnobs {
    /// Below this used-percent, route to the paid subscription.
    /// Default: [`DEFAULT_USE_TARGET`].
    pub use_target: f64,
    /// At/above this used-percent, route OFF the subscription (free or
    /// chargeback-mode). Default: [`DEFAULT_CAP_GUARD`].
    pub cap_guard: f64,
    /// How to behave once `cap_guard` is hit.
    pub budget_mode: BudgetMode,
    /// When `budget_mode = chargeback`, the dollar budget past the cap
    /// we are willing to spend before falling back. `None` when not in
    /// chargeback mode or when no explicit budget has been set.
    pub chargeback_budget_usd: Option<f64>,
}

impl Default for RouteKnobs {
    fn default() -> Self {
        Self {
            use_target: DEFAULT_USE_TARGET,
            cap_guard: DEFAULT_CAP_GUARD,
            budget_mode: DEFAULT_BUDGET_MODE,
            chargeback_budget_usd: None,
        }
    }
}

impl RouteKnobs {
    /// Build knobs for a triple. Unknown knobs fall back to the global
    /// defaults, so a sparse config still works.
    pub fn for_triple(provider: Provider, account_id: &str, plan: &str) -> Self {
        Self::default()
            .with_provider_defaults(provider)
            .with_override(provider, account_id, plan)
    }

    /// Layer the provider-wide defaults on top of the global defaults.
    /// Currently a no-op placeholder; callers can compose external catalogs
    /// in here without changing the trait.
    pub fn with_provider_defaults(self, _provider: Provider) -> Self {
        self
    }

    /// Layer the per-(provider, account, plan) override on top. Caller
    /// supplies overrides; this method is a no-op default impl so callers
    /// that don't have an override store still compile.
    pub fn with_override(self, _provider: Provider, _account_id: &str, _plan: &str) -> Self {
        self
    }
}

// ---------------------------------------------------------------------------
// Header parsing.
// ---------------------------------------------------------------------------

fn parse_pct(raw: &str) -> Option<f64> {
    raw.trim().trim_end_matches('%').parse::<f64>().ok()
}

fn parse_u32(raw: &str) -> Option<u32> {
    raw.trim().parse::<u32>().ok()
}

fn parse_i64(raw: &str) -> Option<i64> {
    raw.trim().parse::<i64>().ok()
}

fn parse_bool_loose(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Parse an ISO-8601 / RFC3339 timestamp string into epoch seconds. We
/// tolerate `Z` and explicit offsets by going through chrono's parser.
/// Returns `None` for unparseable values.
fn parse_epoch_seconds(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }
    // Some Codex headers ship a bare epoch-seconds string instead of an
    // RFC3339 timestamp; detect that by length + digit-only.
    if s.chars().all(|c| c.is_ascii_digit()) {
        return s.parse::<i64>().ok();
    }
    None
}

/// Parse Codex `x-codex-*` headers into a snapshot. `account_id` and
/// `plan` are caller-supplied (Codex publishes them but not always in the
/// same response — keep the parser orthogonal).
pub fn parse_codex(headers: &dyn HeaderLookup, account_id: &str, plan: &str) -> RateLimitSnapshot {
    let mut snap = RateLimitSnapshot::new(Provider::OpenaiCodex, account_id, plan);

    let plan_type = headers.get("x-codex-plan-type");
    if let Some(p) = plan_type {
        snap.plan = p.to_string();
    }

    snap.has_credits = headers
        .get("x-codex-credits-has-credits")
        .and_then(parse_bool_loose);

    let primary = WindowSnapshot {
        used_percent: headers
            .get("x-codex-primary-used-percent")
            .and_then(parse_pct)
            .unwrap_or(0.0),
        window_minutes: headers
            .get("x-codex-primary-window-minutes")
            .and_then(parse_u32),
        reset_at_epoch: headers
            .get("x-codex-primary-reset-at")
            .and_then(parse_epoch_seconds)
            .or_else(|| {
                // Older Codex shapes ship `reset-after-seconds` only;
                // synthesize an epoch by anchoring to `now()`.
                headers
                    .get("x-codex-primary-reset-after-seconds")
                    .and_then(parse_i64)
                    .map(|secs| Utc::now().timestamp() + secs)
            }),
        label: Some("primary".to_string()),
    };
    let secondary = WindowSnapshot {
        used_percent: headers
            .get("x-codex-secondary-used-percent")
            .and_then(parse_pct)
            .unwrap_or(0.0),
        window_minutes: headers
            .get("x-codex-secondary-window-minutes")
            .and_then(parse_u32),
        reset_at_epoch: headers
            .get("x-codex-secondary-reset-at")
            .and_then(parse_epoch_seconds)
            .or_else(|| {
                headers
                    .get("x-codex-secondary-reset-after-seconds")
                    .and_then(parse_i64)
                    .map(|secs| Utc::now().timestamp() + secs)
            }),
        label: Some("secondary".to_string()),
    };

    snap.primary = Some(primary);
    snap.secondary = Some(secondary);
    snap
}

/// Parse Anthropic headers into a snapshot. The published shape on the
/// unified endpoint is `anthropic-ratelimit-unified-<window>-{status,
/// utilization, reset}`. The older pre-unified shape uses
/// `anthropic-ratelimit-{requests,tokens}-{limit,remaining,reset}`. The
/// no-suffix variant — `anthropic-ratelimit-unified-{status,remaining,
/// reset}` — is also handled (Anthropic publishes a "current window"
/// view alongside the suffixed rollups; see
/// [`anthropic_unified_window_nosuffix`]). All three are handled.
///
/// `window_minutes` is derived from the suffix (`5h` -> 300, `7d` -> 10080)
/// plus the explicit names (`1m`, `1h`, `5h`, `7d`); unknown suffixes
/// leave `window_minutes = None`.
pub fn parse_anthropic(
    headers: &dyn HeaderLookup,
    account_id: &str,
    plan: &str,
) -> RateLimitSnapshot {
    let mut snap = RateLimitSnapshot::new(Provider::Anthropic, account_id, plan);

    // Unified endpoint first.
    let primary = anthropic_unified_window(headers, "5h").unwrap_or_default();
    let secondary = anthropic_unified_window(headers, "7d").unwrap_or_default();

    if primary != WindowSnapshot::default() {
        snap.primary = Some(primary);
    }
    if secondary != WindowSnapshot::default() {
        snap.secondary = Some(secondary);
    }

    // No-suffix shape: `anthropic-ratelimit-unified-{status,remaining,
    // reset}`. Anthropic publishes these alongside the suffixed rollups
    // as a "current window" view. Only used as a fallback so we don't
    // shadow a richer 5h/7d sighting.
    if snap.primary.is_none() {
        if let Some(w) = anthropic_unified_window_nosuffix(headers) {
            snap.primary = Some(w);
        }
    }

    // Fall back to the older `anthropic-ratelimit-requests-*` /
    // `anthropic-ratelimit-tokens-*` shape when the unified endpoint did
    // not publish a snapshot. We synthesize a *single* primary window from
    // whichever pair is present and trust the operator to know what unit
    // they're optimizing (it's percent-of-cap either way).
    if snap.primary.is_none() {
        let (limit, remaining, reset) = anthropic_legacy_pair(headers, "requests");
        let pct = match (limit, remaining) {
            (Some(l), Some(r)) if l > 0 => Some(((l - r) as f64 / l as f64) * 100.0),
            _ => None,
        };
        if let Some(p) = pct {
            snap.primary = Some(WindowSnapshot {
                used_percent: p,
                window_minutes: None,
                reset_at_epoch: reset,
                label: Some("requests".to_string()),
            });
        }
        let (limit, remaining, reset) = anthropic_legacy_pair(headers, "tokens");
        let pct = match (limit, remaining) {
            (Some(l), Some(r)) if l > 0 => Some(((l - r) as f64 / l as f64) * 100.0),
            _ => None,
        };
        if let Some(p) = pct {
            snap.secondary = Some(WindowSnapshot {
                used_percent: p,
                window_minutes: None,
                reset_at_epoch: reset,
                label: Some("tokens".to_string()),
            });
        }
    }

    snap
}

fn anthropic_unified_window(headers: &dyn HeaderLookup, suffix: &str) -> Option<WindowSnapshot> {
    // Some servers publish `status` without a numeric `utilization`; that
    // means "known, no numeric value", which we surface as 0% so the
    // caller at least knows the window exists. Real values always come
    // with a `utilization` field. We don't gate on `status` here so a
    // `utilization`-only header set still parses.
    let _status = headers.get(&format!("anthropic-ratelimit-unified-{suffix}-status"));
    let util = headers
        .get(&format!("anthropic-ratelimit-unified-{suffix}-utilization"))
        .and_then(parse_pct)
        .unwrap_or(0.0);
    let reset_at = headers
        .get(&format!("anthropic-ratelimit-unified-{suffix}-reset"))
        .and_then(parse_epoch_seconds);
    // If the caller sent no unified-* headers at all for this suffix,
    // return None so the caller doesn't fall back to a fake 0%.
    if _status.is_none()
        && headers
            .get(&format!("anthropic-ratelimit-unified-{suffix}-utilization"))
            .is_none()
    {
        return None;
    }
    Some(WindowSnapshot {
        used_percent: util,
        window_minutes: anthropic_suffix_minutes(suffix),
        reset_at_epoch: reset_at,
        label: Some(suffix.to_string()),
    })
}

/// No-suffix `anthropic-ratelimit-unified-{status,remaining,reset}` shape.
/// Carries a status flag plus a remaining count and a reset timestamp;
/// without a `limit` we cannot compute a real percent, so we surface 0%
/// (a known window, no numeric value — same convention as the suffixed
/// status-only case) and forward the reset. The snapshot is only emitted
/// when at least one of the three headers is present; otherwise the
/// caller can keep falling back to the legacy pair.
fn anthropic_unified_window_nosuffix(headers: &dyn HeaderLookup) -> Option<WindowSnapshot> {
    let status = headers.get("anthropic-ratelimit-unified-status");
    let remaining = headers.get("anthropic-ratelimit-unified-remaining");
    let reset = headers.get("anthropic-ratelimit-unified-reset");
    if status.is_none() && remaining.is_none() && reset.is_none() {
        return None;
    }
    Some(WindowSnapshot {
        // No `limit` published in this shape; surface 0% per the same
        // status-only convention the suffixed parser uses, and let the
        // operator's window context decide whether 0% means "fresh".
        used_percent: 0.0,
        window_minutes: None,
        reset_at_epoch: reset.and_then(parse_epoch_seconds),
        label: Some("unified".to_string()),
    })
}

fn anthropic_legacy_pair(
    headers: &dyn HeaderLookup,
    unit: &str,
) -> (Option<i64>, Option<i64>, Option<i64>) {
    let limit = headers
        .get(&format!("anthropic-ratelimit-{unit}-limit"))
        .and_then(parse_i64);
    let remaining = headers
        .get(&format!("anthropic-ratelimit-{unit}-remaining"))
        .and_then(parse_i64);
    let reset = headers
        .get(&format!("anthropic-ratelimit-{unit}-reset"))
        .and_then(parse_epoch_seconds);
    (limit, remaining, reset)
}

fn anthropic_suffix_minutes(suffix: &str) -> Option<u32> {
    // Accept `Nm` / `Nh` / `Nd`. Anything else returns `None`.
    if suffix.len() < 2 {
        return None;
    }
    let (num, unit) = suffix.split_at(suffix.len() - 1);
    let n: u32 = num.parse().ok()?;
    match unit {
        "m" => Some(n),
        "h" => Some(n.saturating_mul(60)),
        "d" => Some(n.saturating_mul(60 * 24)),
        _ => None,
    }
}

/// One-shot parser. Detects the vendor from the header set and returns
/// either a fully-populated [`RateLimitSnapshot`] or `None` when the
/// headers don't look like any vendor we know about.
pub fn parse_headers(
    headers: &dyn HeaderLookup,
    account_id: &str,
    plan: &str,
) -> Option<RateLimitSnapshot> {
    let provider = Provider::detect(headers)?;
    let mut snap = match provider {
        Provider::OpenaiCodex => parse_codex(headers, account_id, plan),
        Provider::Anthropic => parse_anthropic(headers, account_id, plan),
        Provider::Openai | Provider::Other | Provider::MiniMax => {
            // OpenAI plain chat-completions carries `x-ratelimit-*` but
            // no window structure; persist a presence-only marker.
            // MiniMax has NO rate-limit headers at all — the
            // counter-fed path in `record_counter` is the only signal
            // we have for it; parsing headers here is a no-op.
            RateLimitSnapshot::new(provider, account_id, plan)
        }
    };
    snap.observed_at = Some(Utc::now());
    Some(snap)
}

// ---------------------------------------------------------------------------
// Counter-fed utilization (KNEMON Layer 3B).
// ---------------------------------------------------------------------------

/// One counter-fed utilization window. Used for providers (MiniMax) that do
/// NOT publish rate-limit headers and whose usage has to be measured locally
/// by counting tokens off the chat-completion response. The store keeps a
/// running `used_tokens` total, and recomputes `used_percent` whenever the
/// cap is known.
///
/// `cap = None` is a valid state — it means "this window exists but we don't
/// know its cap; surface the running token count but never compute a
/// percent." PercentOnly subscription windows fall into this bucket by
/// construction (the operator / provider only publishes a percent).
///
/// Reset is intentionally a free-form `Option<DateTime<Utc>>`: monthly
/// calendar windows flip to zero at the next month boundary (caller's
/// responsibility to detect + reset; the store just records the
/// observation), and rolling windows in this layer are NOT supported
/// because the whole point of counter-fed tracking is that there is no
/// header-driven reset signal. The store records `last_updated` so callers
/// can age out stale observations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounterWindow {
    pub provider: Provider,
    pub account_id: String,
    pub plan: String,
    pub window_name: String,
    /// Running token count for this window. Only windows with
    /// `observability = Counter` accumulate here.
    pub used_tokens: f64,
    /// Cap in tokens, if known. `None` = percent-only window
    /// (`PercentOnly`), or "cap not yet recorded". When `None`,
    /// `used_percent` is `None` too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<f64>,
    /// `used_tokens / cap` when `cap.is_some() && cap > 0.0`. `None`
    /// otherwise — never divide by zero, never claim a percent the store
    /// can't actually compute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    /// Provider-driven reset signal (e.g. the next calendar-month boundary
    /// for `reset: CalendarMonthly`). `None` when the provider has not
    /// published one; the caller decides whether the window has aged out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<DateTime<Utc>>,
    /// UTC observation timestamp of the most recent increment.
    pub last_updated: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Routing decision.
// ---------------------------------------------------------------------------

/// Effective used-percent for the snapshot, taking `reset_at_epoch` into
/// account. When the provider-published reset time is in the past, the
/// window has rolled over and headroom is full again.
pub fn effective_used(snap: &RateLimitSnapshot, now: DateTime<Utc>) -> f64 {
    let now_epoch = now.timestamp();
    let pct = |w: &WindowSnapshot| -> f64 {
        match w.reset_at_epoch {
            Some(t) if t <= now_epoch => 0.0,
            _ => w.used_percent,
        }
    };
    match (&snap.primary, &snap.secondary) {
        (Some(p), Some(s)) => pct(p).max(pct(s)),
        (Some(p), None) => pct(p),
        (None, Some(s)) => pct(s),
        (None, None) => 0.0,
    }
}

/// Decide whether to use the paid subscription, fall back to free, or
/// chargeback. `chargeback_remaining_usd` is the live remaining dollar
/// budget in chargeback mode; pass `None` when not applicable. Callers
/// that want the same behavior regardless of time should set `now` to a
/// fixed [`DateTime`] in tests.
pub fn decide(
    snap: &RateLimitSnapshot,
    knobs: &RouteKnobs,
    now: DateTime<Utc>,
    chargeback_remaining_usd: Option<f64>,
) -> RouteDecision {
    // Stale reset: window rolled over -> full headroom.
    let used = effective_used(snap, now);

    // No credits (Codex-specific) -> we can't spend what we don't have.
    if snap.has_credits == Some(false) {
        return RouteDecision::FallBackToFree;
    }

    if used < knobs.use_target {
        return RouteDecision::PreferSub;
    }
    if used < knobs.cap_guard {
        // Hysteresis band: keep using the subscription unless the guard
        // trips. This is the "maximize paid usage" path.
        return RouteDecision::PreferSub;
    }

    match knobs.budget_mode {
        BudgetMode::Block => RouteDecision::FallBackToFree,
        BudgetMode::Chargeback => match (knobs.chargeback_budget_usd, chargeback_remaining_usd) {
            (Some(_cap), Some(remaining)) if remaining > 0.0 => RouteDecision::Chargeback,
            _ => RouteDecision::FallBackToFree,
        },
    }
}

// ---------------------------------------------------------------------------
// Persistent store.
// ---------------------------------------------------------------------------

/// One row in `~/.zoder/utilization.json`. Keyed by
/// `(provider, account_id, plan)` so a personal account that is logged
/// in on multiple machines aggregates correctly — the local store is the
/// **most recent** sighting, not a sum.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UtilizationRecord {
    pub provider: Provider,
    pub account_id: String,
    pub plan: String,
    pub primary: Option<WindowSnapshot>,
    pub secondary: Option<WindowSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_credits: Option<bool>,
    /// RFC3339 UTC.
    pub last_updated: DateTime<Utc>,
}

/// Resolve the default `$ZODER_HOME` / `~/.zoder` path. Tests can override
/// by passing an explicit path to [`UtilizationStore::open`].
pub fn default_store_path() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("ZODER_HOME") {
        if !home.is_empty() {
            return Some(PathBuf::from(home).join(UTILIZATION_FILENAME));
        }
    }
    dirs::home_dir().map(|h| h.join(".zoder").join(UTILIZATION_FILENAME))
}

/// Persistent store. Reads the on-disk JSON on open, serializes on save.
/// Keyed by `(provider, account_id, plan)`.
///
/// In addition to the header-fed `records` (KNEMON Layer 3A — snapshots
/// parsed from `x-codex-*` / `anthropic-ratelimit-*` response headers),
/// the store carries `counters` (KNEMON Layer 3B — running token counts
/// for providers that publish no rate-limit headers, e.g. MiniMax). Both
/// are persisted to the same file so a single `~/.zoder/utilization.json`
/// holds the whole utilization picture for the box.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UtilizationStore {
    #[serde(default)]
    pub records: BTreeMap<String, UtilizationRecord>,
    /// Counter-fed windows (KNEMON Layer 3B). Keyed by
    /// `(provider, account_id, plan, window_name)` — the `window_name`
    /// segment disambiguates the `monthly` / `5h` / `weekly` windows the
    /// catalog declares for the same `(provider, account, plan)`.
    #[serde(default)]
    pub counters: BTreeMap<String, CounterWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

fn key(provider: Provider, account_id: &str, plan: &str) -> String {
    format!("{provider:?}::{account_id}::{plan}")
}

fn counter_key(provider: Provider, account_id: &str, plan: &str, window_name: &str) -> String {
    format!("{provider:?}::{account_id}::{plan}::{window_name}")
}

impl UtilizationRecord {
    /// Build a record from a fresh snapshot. `last_updated` is set to
    /// `now`; if the snapshot already carries `observed_at`, that wins.
    pub fn from_snapshot(snap: &RateLimitSnapshot, now: DateTime<Utc>) -> Self {
        Self {
            provider: snap.provider,
            account_id: snap.account_id.clone(),
            plan: snap.plan.clone(),
            primary: snap.primary.clone(),
            secondary: snap.secondary.clone(),
            has_credits: snap.has_credits,
            last_updated: snap.observed_at.unwrap_or(now),
        }
    }

    /// Project into a snapshot at `now`, so callers can run [`decide`]
    /// against persisted state without re-parsing headers. The "reset
    /// passed" expiry is handled by [`effective_used`], not by mutating
    /// the stored record — we keep the original `reset_at_epoch` so we
    /// can tell *when* the rollover happened.
    pub fn as_snapshot(&self) -> RateLimitSnapshot {
        RateLimitSnapshot {
            provider: self.provider,
            account_id: self.account_id.clone(),
            plan: self.plan.clone(),
            primary: self.primary.clone(),
            secondary: self.secondary.clone(),
            has_credits: self.has_credits,
            observed_at: Some(self.last_updated),
        }
    }
}

impl UtilizationStore {
    /// Open a store at `path`. Creates an empty one if the file doesn't
    /// exist yet. Returns an error only on real I/O / parse failures —
    /// a missing file is fine.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, UtilizationError> {
        let path = path.as_ref();
        match fs::read(path) {
            Ok(bytes) => {
                if bytes.is_empty() {
                    let store = Self {
                        records: BTreeMap::new(),
                        counters: BTreeMap::new(),
                        path: Some(path.to_path_buf()),
                    };
                    return Ok(store);
                }
                let mut store: Self =
                    serde_json::from_slice(&bytes).map_err(UtilizationError::Parse)?;
                store.path = Some(path.to_path_buf());
                Ok(store)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self {
                records: BTreeMap::new(),
                counters: BTreeMap::new(),
                path: Some(path.to_path_buf()),
            }),
            Err(e) => Err(UtilizationError::Io(e)),
        }
    }

    /// Open a store at the default location. Returns `None` when neither
    /// `$ZODER_HOME` nor `~/.zoder` resolves.
    pub fn open_default() -> Result<Option<Self>, UtilizationError> {
        match default_store_path() {
            Some(p) => Ok(Some(Self::open(p)?)),
            None => Ok(None),
        }
    }

    /// Upsert a snapshot. No-op if the snapshot has no windows — we
    /// don't want a presence-only OpenAI sighting to wipe a richer Codex
    /// record under the same key.
    pub fn upsert(&mut self, snap: &RateLimitSnapshot, now: DateTime<Utc>) {
        if snap.primary.is_none() && snap.secondary.is_none() {
            return;
        }
        let k = key(snap.provider, &snap.account_id, &snap.plan);
        let rec = UtilizationRecord::from_snapshot(snap, now);
        self.records.insert(k, rec);
    }

    /// Look up a record.
    pub fn get(
        &self,
        provider: Provider,
        account_id: &str,
        plan: &str,
    ) -> Option<&UtilizationRecord> {
        self.records.get(&key(provider, account_id, plan))
    }

    /// One-shot capture: upsert the snapshot into the in-memory store,
    /// then flush to disk. Best-effort — callers that don't care about
    /// persistence errors (the routing layer feeding live telemetry) just
    /// log at debug and move on. Returns `true` when the snapshot had
    /// windows to record, `false` when it was a presence-only sighting
    /// that the store chose to drop (so the caller can decide whether to
    /// bother logging).
    pub fn record(&mut self, snap: &RateLimitSnapshot, now: DateTime<Utc>) -> bool {
        if snap.primary.is_none() && snap.secondary.is_none() {
            return false;
        }
        self.upsert(snap, now);
        // Save is intentionally best-effort; I/O failure surfaces via
        // `Err` but the routing layer feeds telemetry and must not be
        // poisoned by transient disk issues.
        let _ = self.save();
        true
    }

    /// Record a token-usage increment against a counter-fed window
    /// (KNEMON Layer 3B). The persisted `used_tokens` is increased by
    /// `tokens_used`, and `used_percent` is recomputed as
    /// `used_tokens / cap` whenever the cap is known. When the cap is
    /// `None` (a `PercentOnly` window, or any window whose cap has not yet
    /// been recorded via [`set_counter_cap`]), `used_percent` stays
    /// `None` — we never invent a percent.
    ///
    /// Window `reset_at` is preserved across increments when the
    /// caller has already set it; otherwise it's left at `None`. The
    /// caller is responsible for detecting a calendar boundary and
    /// resetting `used_tokens` to zero (the store's contract is
    /// "increment, never auto-reset" — auto-resetting on a misread clock
    /// would silently destroy utilization data).
    ///
    /// Returns the new `used_tokens` after the increment. Best-effort:
    /// callers that want the disk side-effect should pair this with
    /// [`save`] and tolerate its error (mirrors the header-fed
    /// [`record`] path).
    pub fn record_counter(
        &mut self,
        provider: Provider,
        account_id: &str,
        plan: &str,
        window_name: &str,
        tokens_used: f64,
        now: DateTime<Utc>,
    ) -> f64 {
        let k = counter_key(provider, account_id, plan, window_name);
        let entry = self
            .counters
            .entry(k.clone())
            .or_insert_with(|| CounterWindow {
                provider,
                account_id: account_id.to_string(),
                plan: plan.to_string(),
                window_name: window_name.to_string(),
                used_tokens: 0.0,
                cap: None,
                used_percent: None,
                reset_at: None,
                last_updated: now,
            });
        // Defensive: a malformed / negative increment is a no-op, not a
        // subtraction. A provider that occasionally reports 0 usage
        // (streaming-usage off, usage field absent) still wants a row
        // touch but never a negative balance.
        if tokens_used.is_finite() && tokens_used > 0.0 {
            entry.used_tokens += tokens_used;
        }
        // Recompute percent from the cap, if any. `cap = Some(0.0)` is
        // treated as "no headroom" (0%, not NaN/inf) so a bad
        // configuration never produces an exploded percent.
        entry.used_percent = match entry.cap {
            Some(c) if c > 0.0 => Some(entry.used_tokens / c),
            _ => None,
        };
        entry.last_updated = now;
        entry.used_tokens
    }

    /// Set (or clear) the cap for one counter-fed window. The store only
    /// stores the cap; it does NOT recompute `used_tokens` (the cap may
    /// be recorded AFTER the first call to [`record_counter`] in the
    /// same boot — e.g. the wire-up reads the catalog once at startup
    /// and then records usage as it lands). Callers that need the
    /// `used_percent` field refreshed after `set_counter_cap` should
    /// re-read the entry: the next `record_counter` call will
    /// recompute it. If the caller wants the percent immediately, see
    /// [`recompute_counter_percent`].
    pub fn set_counter_cap(
        &mut self,
        provider: Provider,
        account_id: &str,
        plan: &str,
        window_name: &str,
        cap: Option<f64>,
        now: DateTime<Utc>,
    ) {
        let k = counter_key(provider, account_id, plan, window_name);
        let entry = self.counters.entry(k).or_insert_with(|| CounterWindow {
            provider,
            account_id: account_id.to_string(),
            plan: plan.to_string(),
            window_name: window_name.to_string(),
            used_tokens: 0.0,
            cap: None,
            used_percent: None,
            reset_at: None,
            last_updated: now,
        });
        entry.cap = cap;
        entry.used_percent = match entry.cap {
            Some(c) if c > 0.0 => Some(entry.used_tokens / c),
            _ => None,
        };
        entry.last_updated = now;
    }

    /// Recompute `used_percent` for one counter-fed window from its
    /// currently-stored `used_tokens` and `cap`. Use after
    /// [`set_counter_cap`] to surface the percent before the next
    /// `record_counter` lands. Returns the new percent, or `None` when
    /// the cap is unknown / non-positive.
    pub fn recompute_counter_percent(
        &mut self,
        provider: Provider,
        account_id: &str,
        plan: &str,
        window_name: &str,
    ) -> Option<f64> {
        let k = counter_key(provider, account_id, plan, window_name);
        let entry = self.counters.get_mut(&k)?;
        let pct = match entry.cap {
            Some(c) if c > 0.0 => Some(entry.used_tokens / c),
            _ => None,
        };
        entry.used_percent = pct;
        pct
    }

    /// Look up a counter-fed window. `None` when the window has never
    /// been recorded (a fresh box that has not yet seen a counter-fed
    /// response from this `(provider, account, plan)`).
    pub fn get_counter(
        &self,
        provider: Provider,
        account_id: &str,
        plan: &str,
        window_name: &str,
    ) -> Option<&CounterWindow> {
        self.counters
            .get(&counter_key(provider, account_id, plan, window_name))
    }

    /// Persist to the path we were opened from. Creates parent dirs.
    pub fn save(&self) -> Result<(), UtilizationError> {
        let path = self.path.as_ref().ok_or(UtilizationError::NoPath)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(UtilizationError::Io)?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(UtilizationError::Parse)?;
        fs::write(path, bytes).map_err(UtilizationError::Io)?;
        Ok(())
    }
}

/// Errors from the store / parser surface.
#[derive(Debug, thiserror::Error)]
pub enum UtilizationError {
    #[error("utilization: I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("utilization: parse error: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("utilization: store has no path; open() with an explicit path first")]
    NoPath,
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn hm<const N: usize>(pairs: [(&str, &str); N]) -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert(k.to_string(), v.to_string());
        }
        m
    }

    // -------- header parsing ----------------------------------------

    #[test]
    fn parse_codex_full_headers() {
        let h = hm([
            ("x-codex-plan-type", "pro"),
            ("x-codex-active-limit", "100"),
            ("x-codex-primary-used-percent", "42"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "2026-07-04T10:00:00Z"),
            ("x-codex-primary-reset-after-seconds", "600"),
            ("x-codex-secondary-used-percent", "12"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-at", "2026-07-11T10:00:00Z"),
            ("x-codex-credits-has-credits", "true"),
        ]);
        let snap = parse_codex(&h, "acct-1", "ignored");
        assert_eq!(snap.provider, Provider::OpenaiCodex);
        assert_eq!(snap.plan, "pro");
        assert_eq!(snap.has_credits, Some(true));
        let p = snap.primary.unwrap();
        assert_eq!(p.used_percent, 42.0);
        assert_eq!(p.window_minutes, Some(300));
        // `reset-at` wins over the synthesized `reset-after-seconds`.
        assert_eq!(p.reset_at_epoch, Some(1783159200));
        let s = snap.secondary.unwrap();
        assert_eq!(s.used_percent, 12.0);
        assert_eq!(s.window_minutes, Some(10080));
    }

    #[test]
    fn parse_codex_falls_back_to_reset_after_seconds() {
        let h = hm([
            ("x-codex-primary-used-percent", "5"),
            ("x-codex-primary-reset-after-seconds", "120"),
        ]);
        let snap = parse_codex(&h, "acct-1", "pro");
        let p = snap.primary.unwrap();
        assert_eq!(p.used_percent, 5.0);
        let now = Utc::now().timestamp();
        let reset = p.reset_at_epoch.expect("reset must be synthesized");
        assert!(
            (reset - (now + 120)).abs() <= 5,
            "reset {reset} should be ~now+120 (now={now})",
        );
    }

    #[test]
    fn parse_codex_no_credits() {
        let h = hm([
            ("x-codex-primary-used-percent", "50"),
            ("x-codex-credits-has-credits", "false"),
        ]);
        let snap = parse_codex(&h, "acct", "free");
        assert_eq!(snap.has_credits, Some(false));
    }

    #[test]
    fn parse_anthropic_unified_shape() {
        let h = hm([
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "65.5"),
            (
                "anthropic-ratelimit-unified-5h-reset",
                "2026-07-04T08:00:00Z",
            ),
            ("anthropic-ratelimit-unified-7d-utilization", "20"),
        ]);
        let snap = parse_anthropic(&h, "acct", "max");
        assert_eq!(snap.provider, Provider::Anthropic);
        let p = snap.primary.unwrap();
        assert_eq!(p.used_percent, 65.5);
        assert_eq!(p.window_minutes, Some(300));
        assert_eq!(p.label.as_deref(), Some("5h"));
        let s = snap.secondary.unwrap();
        assert_eq!(s.used_percent, 20.0);
        assert_eq!(s.window_minutes, Some(10080));
    }

    #[test]
    fn parse_anthropic_legacy_pair() {
        let h = hm([
            ("anthropic-ratelimit-requests-limit", "1000"),
            ("anthropic-ratelimit-requests-remaining", "200"),
            ("anthropic-ratelimit-requests-reset", "2026-07-04T08:00:00Z"),
            ("anthropic-ratelimit-tokens-limit", "1000000"),
            ("anthropic-ratelimit-tokens-remaining", "500000"),
        ]);
        let snap = parse_anthropic(&h, "acct", "max");
        // Primary is derived from `requests` pair: 80% used.
        let p = snap.primary.unwrap();
        assert!((p.used_percent - 80.0).abs() < 1e-9);
        assert_eq!(p.label.as_deref(), Some("requests"));
        // Secondary from `tokens` pair: 50% used.
        let s = snap.secondary.unwrap();
        assert!((s.used_percent - 50.0).abs() < 1e-9);
    }

    #[test]
    fn parse_anthropic_suffix_minutes() {
        assert_eq!(anthropic_suffix_minutes("5h"), Some(300));
        assert_eq!(anthropic_suffix_minutes("7d"), Some(10080));
        assert_eq!(anthropic_suffix_minutes("1m"), Some(1));
        assert_eq!(anthropic_suffix_minutes("garbage"), None);
    }

    #[test]
    fn parse_headers_detects_vendor() {
        let h = hm([("x-codex-primary-used-percent", "10")]);
        let s = parse_headers(&h, "acct", "pro").unwrap();
        assert_eq!(s.provider, Provider::OpenaiCodex);
        let h = hm([("anthropic-ratelimit-unified-5h-utilization", "10")]);
        let s = parse_headers(&h, "acct", "max").unwrap();
        assert_eq!(s.provider, Provider::Anthropic);
        let h = hm([("x-ratelimit-limit-requests", "100")]);
        let s = parse_headers(&h, "acct", "free").unwrap();
        assert_eq!(s.provider, Provider::Openai);
        let h = hm([("content-type", "application/json")]);
        assert!(parse_headers(&h, "acct", "free").is_none());
    }

    // -------- effective_used ---------------------------------------

    #[test]
    fn effective_used_takes_max_of_windows() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let snap = RateLimitSnapshot {
            primary: Some(WindowSnapshot {
                used_percent: 30.0,
                window_minutes: Some(300),
                reset_at_epoch: None,
                label: Some("primary".into()),
            }),
            secondary: Some(WindowSnapshot {
                used_percent: 90.0,
                window_minutes: Some(10080),
                reset_at_epoch: None,
                label: Some("secondary".into()),
            }),
            ..RateLimitSnapshot::default()
        };
        assert_eq!(effective_used(&snap, now), 90.0);
    }

    #[test]
    fn effective_used_resets_when_reset_at_passes() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let snap = RateLimitSnapshot {
            primary: Some(WindowSnapshot {
                used_percent: 99.0,
                window_minutes: Some(300),
                reset_at_epoch: Some(now.timestamp() - 10), // 10s ago
                label: Some("primary".into()),
            }),
            secondary: Some(WindowSnapshot {
                used_percent: 10.0,
                window_minutes: Some(10080),
                reset_at_epoch: None,
                label: Some("secondary".into()),
            }),
            ..RateLimitSnapshot::default()
        };
        // Primary rolled over -> 0%; secondary still 10%; max = 10%.
        assert_eq!(effective_used(&snap, now), 10.0);
    }

    // -------- decide -----------------------------------------------

    fn knobs_with(target: f64, guard: f64, mode: BudgetMode, budget: Option<f64>) -> RouteKnobs {
        RouteKnobs {
            use_target: target,
            cap_guard: guard,
            budget_mode: mode,
            chargeback_budget_usd: budget,
        }
    }

    fn snap_with_primary(pct: f64, reset_at: Option<i64>) -> RateLimitSnapshot {
        RateLimitSnapshot {
            provider: Provider::OpenaiCodex,
            account_id: "acct".into(),
            plan: "pro".into(),
            primary: Some(WindowSnapshot {
                used_percent: pct,
                window_minutes: Some(300),
                reset_at_epoch: reset_at,
                label: Some("primary".into()),
            }),
            secondary: None,
            has_credits: Some(true),
            observed_at: None,
        }
    }

    #[test]
    fn decide_below_target_prefers_sub() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let snap = snap_with_primary(50.0, None);
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::PreferSub,
        );
    }

    #[test]
    fn decide_hysteresis_band_keeps_prefer_sub() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let snap = snap_with_primary(82.0, None);
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::PreferSub,
        );
    }

    #[test]
    fn decide_at_or_above_cap_guard_blocks() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let snap = snap_with_primary(85.0, None);
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::FallBackToFree,
        );
        let snap = snap_with_primary(95.0, None);
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::FallBackToFree,
        );
    }

    #[test]
    fn decide_chargeback_mode_with_budget_remaining() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let snap = snap_with_primary(95.0, None);
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Chargeback, Some(50.0));
        assert_eq!(
            decide(&snap, &knobs, now, Some(10.0)),
            RouteDecision::Chargeback,
        );
    }

    #[test]
    fn decide_chargeback_mode_with_no_budget_falls_back() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let snap = snap_with_primary(95.0, None);
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Chargeback, Some(50.0));
        assert_eq!(
            decide(&snap, &knobs, now, Some(0.0)),
            RouteDecision::FallBackToFree,
        );
        assert_eq!(
            decide(&snap, &knobs, now, None),
            RouteDecision::FallBackToFree,
        );
    }

    #[test]
    fn decide_reset_at_expiry_resets_headroom() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        // 90% used but reset was 10s ago -> 0% effective -> PreferSub.
        let snap = snap_with_primary(90.0, Some(now.timestamp() - 10));
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::PreferSub,
        );
    }

    #[test]
    fn decide_no_credits_blocks_immediately() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let mut snap = snap_with_primary(0.0, None);
        snap.has_credits = Some(false);
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::FallBackToFree,
        );
    }

    #[test]
    fn decide_uses_max_of_primary_and_secondary() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let snap = RateLimitSnapshot {
            primary: Some(WindowSnapshot {
                used_percent: 20.0,
                window_minutes: Some(300),
                reset_at_epoch: None,
                label: Some("primary".into()),
            }),
            secondary: Some(WindowSnapshot {
                used_percent: 90.0,
                window_minutes: Some(10080),
                reset_at_epoch: None,
                label: Some("secondary".into()),
            }),
            ..snap_with_primary(0.0, None)
        };
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::FallBackToFree,
        );
    }

    // -------- store round-trip -------------------------------------

    #[test]
    fn store_upsert_and_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let mut s = UtilizationStore::open(&path).unwrap();
        assert!(s.records.is_empty());

        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let snap = snap_with_primary(42.0, None);
        s.upsert(&snap, now);
        s.save().unwrap();

        let s2 = UtilizationStore::open(&path).unwrap();
        let rec = s2
            .get(Provider::OpenaiCodex, "acct", "pro")
            .expect("record persisted");
        assert_eq!(rec.primary.as_ref().unwrap().used_percent, 42.0);
        assert_eq!(rec.plan, "pro");
    }

    #[test]
    fn store_skips_windowless_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let mut s = UtilizationStore::open(&path).unwrap();
        let snap = RateLimitSnapshot::new(Provider::Openai, "acct", "free");
        s.upsert(&snap, Utc::now());
        assert!(s.records.is_empty());
    }

    #[test]
    fn store_missing_file_yields_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let s = UtilizationStore::open(&path).unwrap();
        assert!(s.records.is_empty());
        assert_eq!(s.path.as_deref(), Some(path.as_path()));
    }

    // -------- live capture / feed plumbing (KNEMON wire-up) ---------
    //
    // These tests prove the round trip the CLI relies on:
    //   1. `RateLimitSnapshot::from_headers` parses a real `reqwest::header
    //      ::HeaderMap` for both known vendors (OpenAI-Codex `x-codex-*`
    //      and Anthropic `anthropic-ratelimit-unified-*`).
    //   2. `UtilizationStore::record` upserts + saves in one call, and the
    //      read-back `get(...).as_snapshot()` returns the same window.
    //   3. `decide` flips from `PreferSub` (the None=headroom baseline) to
    //      `FallBackToFree` when fed a persisted snapshot whose used-percent
    //      crosses `cap_guard`. This is the test that actually proves a fed
    //      snapshot changes routing vs the no-signal baseline.

    fn codex_headers() -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        // Codex `x-codex-*` shape: percent + reset window + plan label.
        h.insert("x-codex-primary-used-percent", "92".parse().unwrap());
        h.insert(
            "x-codex-primary-reset-after-seconds",
            "600".parse().unwrap(),
        );
        h.insert("x-codex-secondary-used-percent", "37".parse().unwrap());
        h.insert(
            "x-codex-secondary-reset-after-seconds",
            "86400".parse().unwrap(),
        );
        h.insert("x-codex-plan-type", "pro".parse().unwrap());
        h
    }

    fn anthropic_headers() -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-unified-status",
            "allowed".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-remaining",
            "23".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-unified-reset",
            "2026-07-04T08:00:00Z".parse().unwrap(),
        );
        h
    }

    #[test]
    fn from_headers_parses_codex_live_headers() {
        let h = codex_headers();
        let snap = RateLimitSnapshot::from_headers(&h, Provider::OpenaiCodex, "default", "default")
            .expect("codex headers must yield a snapshot");
        assert_eq!(snap.provider, Provider::OpenaiCodex);
        let p = snap.primary.expect("primary window must be present");
        assert_eq!(p.used_percent, 92.0);
        // `reset-after-seconds` synthesizes an epoch ≈ now + 600.
        let now = Utc::now().timestamp();
        let reset = p.reset_at_epoch.expect("reset must be synthesized");
        assert!(
            (reset - (now + 600)).abs() <= 5,
            "reset {reset} should be ~now+600 (now={now})",
        );
        let s = snap.secondary.expect("secondary window must be present");
        assert_eq!(s.used_percent, 37.0);
        // The Codex `x-codex-plan-type` header is surfaced onto the snapshot.
        assert_eq!(snap.plan, "pro");
    }

    #[test]
    fn from_headers_parses_anthropic_live_headers() {
        let h = anthropic_headers();
        let snap = RateLimitSnapshot::from_headers(&h, Provider::Anthropic, "default", "default")
            .expect("anthropic headers must yield a snapshot");
        assert_eq!(snap.provider, Provider::Anthropic);
        // Anthropic's unified endpoint without a numeric `utilization`
        // surfaces as 0% (status-only). The legacy-style `reset` header is
        // parsed as an RFC3339 epoch.
        let p = snap.primary.expect("primary window must be present");
        assert_eq!(p.used_percent, 0.0);
        assert_eq!(
            p.reset_at_epoch,
            Some(1783152000),
            "2026-07-04T08:00:00Z must parse to 1783152000",
        );
    }

    #[test]
    fn from_headers_drops_mismatched_provider_claim() {
        // Anthropic-shaped headers + caller claims Codex -> reject the
        // snapshot rather than file it under the wrong key.
        let h = anthropic_headers();
        let snap = RateLimitSnapshot::from_headers(&h, Provider::OpenaiCodex, "default", "default");
        assert!(
            snap.is_none(),
            "provider/vendor mismatch must NOT silently re-tag",
        );
    }

    #[test]
    fn store_record_round_trip_via_disk() {
        // End-to-end: record via the public one-shot, then re-open the
        // store from the same path and confirm the read-back matches.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let mut s = UtilizationStore::open(&path).unwrap();
        let snap = snap_with_primary(72.5, None);
        let recorded = s.record(&snap, Utc::now());
        assert!(recorded, "snapshot with windows must be persisted");

        let s2 = UtilizationStore::open(&path).unwrap();
        let rec = s2
            .get(Provider::OpenaiCodex, "acct", "pro")
            .expect("record persisted");
        assert_eq!(rec.primary.as_ref().unwrap().used_percent, 72.5);
        // The read-back path the CLI uses:
        let loaded = s2
            .get(Provider::OpenaiCodex, "acct", "pro")
            .map(|r| r.as_snapshot());
        let loaded = loaded.expect("as_snapshot must yield a snapshot");
        assert_eq!(
            loaded.primary.as_ref().unwrap().used_percent,
            72.5,
            "fed snapshot used_percent must match what was persisted",
        );
    }

    #[test]
    fn store_record_skips_windowless_snapshots() {
        // Mirrors the no-headers case the live capture hits for MiniMax
        // (no headroom headers today): the store refuses to write a
        // presence-only sighting, so a later Codex load isn't poisoned.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let mut s = UtilizationStore::open(&path).unwrap();
        let snap = RateLimitSnapshot::new(Provider::Openai, "default", "default");
        assert!(!s.record(&snap, Utc::now()));
        assert!(s.records.is_empty());
    }

    #[test]
    fn fed_snapshot_at_cap_guard_flips_routing_to_fall_back() {
        // The prove-the-wire-isn't-vacuous test. Same scenario / knobs /
        // now both sides; left side is the historical `None`=headroom
        // baseline, right side is a snapshot the capture path would have
        // just persisted at 92% used (well above the `balanced` 85%
        // cap_guard). The baseline keeps the sub; the fed snapshot
        // routes it off. That's the entire KNEMON feed loop in a single
        // assertion.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        // Baseline: no signal -> KNEMON contract is "keep the sub".
        let baseline_snap = RateLimitSnapshot::default();
        assert_eq!(
            decide(&baseline_snap, &knobs, now, None),
            RouteDecision::PreferSub,
            "None=headroom baseline must PreferSub",
        );
        // Fed: persisted snapshot at 92% -> cap_guard trips -> off.
        let fed_snap = snap_with_primary(92.0, None);
        assert_eq!(
            decide(&fed_snap, &knobs, now, None),
            RouteDecision::FallBackToFree,
            "fed snapshot at/above cap_guard must FallBackToFree",
        );
    }

    // -------- counter-fed (KNEMON Layer 3B) -----------------------
    //
    // MiniMax publishes NO rate-limit headers, so its utilization has to
    // be measured locally by counting tokens off the chat-completion
    // response and writing them to the store. These tests pin the
    // contract:
    //
    //   (a) two responses of N tokens each accumulate to 2N, and the
    //       monthly window's used_percent equals 2N / 5.1e9.
    //   (b) a PercentOnly window is never given a computed percent by
    //       the counter path — even if a caller somehow drives
    //       `record_counter` against a PercentOnly window name, the
    //       store refuses to invent a percent.
    //   (c) the persisted counter window round-trips through the
    //       on-disk JSON via `UtilizationStore::open` (the file the
    //       live wire-up writes to).

    /// Catalog tier id for the MiniMax plan the spec exercises. Mirrors
    /// `subscriptions/tiers.json`; the live wire-up uses the same id.
    const MM_PLAN: &str = "minimax-max";
    /// Cap on the `monthly` window for `minimax-max`, copied from
    /// `subscriptions/tiers.json`. Kept in sync on purpose — the spec
    /// (a) asserts a specific numeric ratio, so the test must agree
    /// with the catalog.
    const MM_MONTHLY_CAP: f64 = 5_100_000_000.0;

    #[test]
    fn counter_two_responses_accumulate_to_2n_with_correct_percent() {
        // (a) two responses of N tokens each accumulate to 2N in the
        // monthly window, used_percent = 2N / 5.1e9. We use a
        // small N (1234) so the arithmetic is exact and the percent
        // is easy to verify by hand: 2*1234 / 5.1e9 = 4.839e-7.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let mut s = UtilizationStore::open(&path).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();

        // Seed the cap the way the live wire-up does (the wire-up
        // pulls it from the catalog, then sets it on the store).
        s.set_counter_cap(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            Some(MM_MONTHLY_CAP),
            now,
        );
        // First response: N tokens.
        let n = 1234.0_f64;
        let used_after_first =
            s.record_counter(Provider::MiniMax, "default", MM_PLAN, "monthly", n, now);
        assert_eq!(used_after_first, n, "first response adds exactly N tokens");
        // Second response: another N tokens.
        let used_after_second =
            s.record_counter(Provider::MiniMax, "default", MM_PLAN, "monthly", n, now);
        assert_eq!(used_after_second, 2.0 * n, "two responses accumulate to 2N");

        // Read back via the typed accessor.
        let w = s
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "monthly")
            .expect("monthly counter window must persist");
        assert_eq!(w.used_tokens, 2.0 * n);
        let expected_pct = (2.0 * n) / MM_MONTHLY_CAP;
        let got_pct = w
            .used_percent
            .expect("used_percent must be Some when cap is known");
        assert!(
            (got_pct - expected_pct).abs() < 1e-12,
            "used_percent must equal 2N/5.1e9 = {expected_pct}, got {got_pct}",
        );
        // And the JSON on disk: round-trip.
        s.save().unwrap();
        let s2 = UtilizationStore::open(&path).unwrap();
        let w2 = s2
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "monthly")
            .expect("monthly counter window must round-trip through disk");
        assert_eq!(w2.used_tokens, 2.0 * n);
        assert_eq!(w2.used_percent, Some(expected_pct));
        assert_eq!(w2.provider, Provider::MiniMax);
        assert_eq!(w2.plan, MM_PLAN);
    }

    #[test]
    fn counter_percent_only_window_never_gets_a_computed_percent() {
        // (b) A PercentOnly window MUST stay percent-less under the
        // counter path. We exercise the store directly with a
        // PercentOnly-shaped window (cap = None). The wire-up skips
        // PercentOnly windows entirely (`record_counter` is only
        // called for Counter windows), but a defensive assertion
        // here pins the contract: even if a caller calls
        // `record_counter` against a percent-only window name with
        // no cap set, the store never invents a percent.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let mut s = UtilizationStore::open(&path).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        // No `set_counter_cap` call — cap stays None (PercentOnly).
        // Simulate a response that fed N tokens into the `5h`
        // PercentOnly window (the wire-up would never do this, but
        // the store's contract must hold either way).
        let n = 999.0_f64;
        let used = s.record_counter(Provider::MiniMax, "default", MM_PLAN, "5h", n, now);
        assert_eq!(used, n, "used_tokens still records the increment");
        let w = s
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "5h")
            .expect("5h counter row must exist");
        assert_eq!(
            w.used_tokens, n,
            "running counter must reflect the increment"
        );
        assert!(
            w.used_percent.is_none(),
            "PercentOnly window must never carry a computed percent: {:?}",
            w.used_percent,
        );
        assert!(
            w.cap.is_none(),
            "cap stays None (never set on a PercentOnly window): {:?}",
            w.cap,
        );

        // Now seed a cap LATER (simulating the catalog being
        // refreshed) — recompute must still produce the right
        // percent, and crucially only AFTER the cap is known. This
        // is the part of the contract the wire-up depends on: a
        // cap-less increment is safe; a percent is only emitted
        // when the cap exists.
        s.set_counter_cap(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "5h",
            Some(10_000.0),
            now,
        );
        // `set_counter_cap` recomputes used_percent from the
        // existing used_tokens — verify that path.
        let w = s
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "5h")
            .unwrap();
        let pct = w
            .used_percent
            .expect("with cap set, used_percent must be Some");
        assert!(
            (pct - (n / 10_000.0)).abs() < 1e-12,
            "after set_counter_cap, percent must be used/cap: got {pct}",
        );

        // And critically: a fresh `record_counter` on a DIFFERENT
        // percent-only window (the `weekly` one) with no cap set
        // MUST stay percent-less. This is the regression guard.
        let used_w = s.record_counter(Provider::MiniMax, "default", MM_PLAN, "weekly", 500.0, now);
        assert_eq!(used_w, 500.0);
        let w_w = s
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "weekly")
            .expect("weekly counter row must exist");
        assert_eq!(w_w.used_tokens, 500.0);
        assert!(
            w_w.used_percent.is_none(),
            "a percent-only window with no cap set must never get a percent: {:?}",
            w_w.used_percent,
        );
    }

    #[test]
    fn counter_persists_round_trip_through_utilization_store() {
        // (c) the persisted counter window round-trips through the
        // on-disk JSON. This is the file-format contract: any
        // `~/.zoder/utilization.json` written by a newer binary
        // must be readable by an older one (the field is
        // `#[serde(default)]`, so a missing `counters` key is
        // accepted as "no counters" — that's the backward path).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        // Write a rich record + a counter entry in one store, then
        // re-open.
        let mut s = UtilizationStore::open(&path).unwrap();
        // A header-fed record (legacy Layer 3A path).
        let snap = snap_with_primary(72.5, None);
        s.record(&snap, now);
        // A counter-fed entry (Layer 3B path).
        s.set_counter_cap(
            Provider::MiniMax,
            "acct-mm",
            MM_PLAN,
            "monthly",
            Some(MM_MONTHLY_CAP),
            now,
        );
        s.record_counter(
            Provider::MiniMax,
            "acct-mm",
            MM_PLAN,
            "monthly",
            2_500_000.0,
            now,
        );
        s.save().unwrap();

        // Re-open from the same path. Both the header record and
        // the counter row survive.
        let s2 = UtilizationStore::open(&path).unwrap();
        let rec = s2
            .get(Provider::OpenaiCodex, "acct", "pro")
            .expect("header-fed record must round-trip");
        assert_eq!(rec.primary.as_ref().unwrap().used_percent, 72.5);
        let cw = s2
            .get_counter(Provider::MiniMax, "acct-mm", MM_PLAN, "monthly")
            .expect("counter row must round-trip");
        assert_eq!(cw.used_tokens, 2_500_000.0);
        assert_eq!(cw.cap, Some(MM_MONTHLY_CAP));
        assert_eq!(cw.used_percent, Some(2_500_000.0 / MM_MONTHLY_CAP));
        assert_eq!(cw.provider, Provider::MiniMax);
        assert_eq!(cw.plan, MM_PLAN);
        assert_eq!(cw.window_name, "monthly");

        // Backward-compat: a hand-edited file that omits the
        // `counters` key entirely must still parse (this is the
        // older-binary-vs-newer-file path). The `#[serde(default)]`
        // on `counters` is what makes this work.
        let legacy = r#"{
            "records": {}
        }"#;
        let path2 = dir.path().join("legacy.json");
        std::fs::write(&path2, legacy).unwrap();
        let s3 = UtilizationStore::open(&path2).unwrap();
        assert!(s3.counters.is_empty());
        assert!(s3.records.is_empty());
    }
}
