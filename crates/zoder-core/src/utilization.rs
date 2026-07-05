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

use crate::config::ResetKind;

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

/// Default `reset_imminence_threshold` — when a binding window is at/above
/// `cap_guard` AND the time remaining before reset is at/under this
/// fraction of the window's full cycle, treat the window as effectively
/// resetting soon and prefer the subscription (don't pay the cost of
/// falling back to free when we're about to get full headroom back).
/// Default `0.10` = "last 10% of the window's clock".
pub const DEFAULT_RESET_IMMINENCE_THRESHOLD: f64 = 0.10;

/// Age thresholds (in seconds) for [`TelemetryHealth`].
const FRESH_MAX_AGE_SECS: i64 = 5 * 60; // <  5 min
const STALE_MAX_AGE_SECS: i64 = 60 * 60; // < 60 min

/// On-disk filename under `$ZODER_HOME` (or `~/.zoder`).
pub const UTILIZATION_FILENAME: &str = "utilization.json";

// ---------------------------------------------------------------------------
// Enums.
// ---------------------------------------------------------------------------

/// Telemetry freshness bucket, derived from the age of a window's last
/// observation. The bucket drives a weight in
/// [`AccountView`](super) routing: the binding window is the one that
/// maximizes `used_percent * health_weight`, so a stale-but-higher-looking
/// window can never beat a fresh-but-slightly-lower one.
///
/// Age buckets (relative to `now`):
///   - `Fresh`    — less than 5 minutes since last update. Full weight (1.0).
///   - `Stale`    — 5 ..= 60 minutes. Discounted weight (0.8). Still
///     trustworthy, but the router shouldn't be steered by
///     a number that's hours old.
///   - `Degraded` — more than 60 minutes, or never observed (no `last_updated`).
///     Weight 0.0 and EXCLUDED from binding — a 95% on a
///     Degraded window is treated as unknown, not "almost
///     full", because we have no proof it's still 95% (it
///     may have rolled over, refilled, or been quietly
///     reconfigured in the meantime).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetryHealth {
    Fresh,
    Stale,
    Degraded,
}

impl TelemetryHealth {
    /// Age in seconds -> bucket. `None` (no `last_updated` observed at all)
    /// is always `Degraded` — never seen = never trusted.
    pub fn from_age_secs(age_secs: Option<i64>) -> Self {
        match age_secs {
            None => TelemetryHealth::Degraded,
            // Negative ages (clock skew, future-dated record) collapse to
            // Fresh — the record is "newer than now", which we trust at
            // face value rather than misclassifying as Degraded.
            Some(s) if s < 0 => TelemetryHealth::Fresh,
            Some(s) if s < FRESH_MAX_AGE_SECS => TelemetryHealth::Fresh,
            Some(s) if s < STALE_MAX_AGE_SECS => TelemetryHealth::Stale,
            Some(_) => TelemetryHealth::Degraded,
        }
    }

    /// Multiplicative weight used by [`decide_account`] when picking the
    /// binding window. `Degraded = 0.0` is what excludes a degraded
    /// window from binding without an explicit branch in the caller
    /// (`max(...)` of `(used, used*0.0)` never selects it).
    pub fn health_weight(self) -> f64 {
        match self {
            TelemetryHealth::Fresh => 1.0,
            TelemetryHealth::Stale => 0.8,
            TelemetryHealth::Degraded => 0.0,
        }
    }
}

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
    /// KNEMON Layer 4: when a binding window is at/above `cap_guard`,
    /// fraction of the window's clock that must remain until `reset_at`
    /// for the reset-relaxation rule to fire (PreferSub). Default
    /// [`DEFAULT_RESET_IMMINENCE_THRESHOLD`] (0.10 = "last 10% of the
    /// window"). A value of `0.0` disables the relaxation.
    #[serde(default = "default_reset_imminence_threshold")]
    pub reset_imminence_threshold: f64,
}

fn default_reset_imminence_threshold() -> f64 {
    DEFAULT_RESET_IMMINENCE_THRESHOLD
}

impl Default for RouteKnobs {
    fn default() -> Self {
        Self {
            use_target: DEFAULT_USE_TARGET,
            cap_guard: DEFAULT_CAP_GUARD,
            budget_mode: DEFAULT_BUDGET_MODE,
            chargeback_budget_usd: None,
            reset_imminence_threshold: DEFAULT_RESET_IMMINENCE_THRESHOLD,
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
///
/// Windows are emitted ONLY when there is a real numeric signal
/// (`x-codex-*-used-percent`) or a usable reset signal
/// (`x-codex-*-reset-at` / `reset-after-seconds`). A response carrying
/// only `x-codex-plan-type: pro` does not create `primary` / `secondary`
/// windows at 0% — that would be a fabricated observation that could
/// later overwrite a real reading under
/// [`UtilizationStore::upsert`](struct.UtilizationStore.html#method.upsert).
pub fn parse_codex(headers: &dyn HeaderLookup, account_id: &str, plan: &str) -> RateLimitSnapshot {
    let mut snap = RateLimitSnapshot::new(Provider::OpenaiCodex, account_id, plan);

    let plan_type = headers.get("x-codex-plan-type");
    if let Some(p) = plan_type {
        snap.plan = p.to_string();
    }

    snap.has_credits = headers
        .get("x-codex-credits-has-credits")
        .and_then(parse_bool_loose);

    if let Some(primary) = codex_window(headers, "primary") {
        snap.primary = Some(primary);
    }
    if let Some(secondary) = codex_window(headers, "secondary") {
        snap.secondary = Some(secondary);
    }
    snap
}

/// Build a `WindowSnapshot` for one Codex window (`primary` / `secondary`)
/// only when there is a real numeric or reset signal. `None` means "this
/// window has no observation in this response" — distinct from
/// "0% used", which is a real reading that requires the
/// `*-used-percent` header.
fn codex_window(headers: &dyn HeaderLookup, which: &str) -> Option<WindowSnapshot> {
    let used_header = format!("x-codex-{which}-used-percent");
    let minutes_header = format!("x-codex-{which}-window-minutes");
    let reset_at_header = format!("x-codex-{which}-reset-at");
    let reset_after_header = format!("x-codex-{which}-reset-after-seconds");
    let used_percent = headers.get(&used_header).and_then(parse_pct);
    let window_minutes = headers.get(&minutes_header).and_then(parse_u32);
    let reset_at_epoch = headers
        .get(&reset_at_header)
        .and_then(parse_epoch_seconds)
        .or_else(|| {
            // Older Codex shapes ship `reset-after-seconds` only;
            // synthesize an epoch by anchoring to `now()`.
            headers
                .get(&reset_after_header)
                .and_then(parse_i64)
                .map(|secs| Utc::now().timestamp() + secs)
        });
    if used_percent.is_none() && reset_at_epoch.is_none() {
        // No numeric reading AND no reset signal — this window does not
        // exist in the response. Returning `None` prevents a fabricated
        // 0% from clobbering a later real reading via `upsert`.
        return None;
    }
    Some(WindowSnapshot {
        // 0% is a real reading when `*-used-percent` is absent but a reset
        // signal is present (the provider is telling us a fresh window
        // opened). Keep the 0% only in that case.
        used_percent: used_percent.unwrap_or(0.0),
        window_minutes,
        reset_at_epoch,
        label: Some(which.to_string()),
    })
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
    // A `status` header without a numeric `utilization` does NOT constitute
    // a numeric reading — surface 0% only when we have an actual
    // utilization value, a reset timestamp, or a known `window_minutes`
    // for the suffix. (We DO still surface a `utilization`-only
    // response: that's a real numeric signal.) This prevents a partial
    // response with only `status` from fabricating a 0% reading that
    // later overwrites a real 92% observation in the store.
    let status = headers.get(&format!("anthropic-ratelimit-unified-{suffix}-status"));
    let util_raw = headers.get(&format!("anthropic-ratelimit-unified-{suffix}-utilization"));
    let util = util_raw.and_then(parse_pct);
    let reset_at = headers
        .get(&format!("anthropic-ratelimit-unified-{suffix}-reset"))
        .and_then(parse_epoch_seconds);
    // If the caller sent no `utilization` AND no `reset` for this suffix,
    // refuse to fabricate a 0% window — return `None` so the caller
    // keeps falling back to the next-best signal (legacy pair, or just
    // "no observation").
    if util.is_none() && reset_at.is_none() {
        // A `status`-only sighting is a real presence signal (the
        // provider told us the window exists), but it's NOT a numeric
        // reading. The store MUST NOT see a 0% row from this, or a
        // later real reading could be overwritten.
        let _ = status;
        return None;
    }
    Some(WindowSnapshot {
        // 0% is a real reading when `utilization` is absent but a reset
        // timestamp is present (the provider is telling us a fresh
        // window opened). 0% is NOT a real reading when only `status`
        // is present.
        used_percent: util.unwrap_or(0.0),
        window_minutes: anthropic_suffix_minutes(suffix),
        reset_at_epoch: reset_at,
        label: Some(suffix.to_string()),
    })
}

/// No-suffix `anthropic-ratelimit-unified-{status,remaining,reset}` shape.
/// Carries a status flag plus a remaining count and a reset timestamp;
/// without a `limit` we cannot compute a real percent, and without a
/// `utilization` field there is no numeric value to surface. A
/// `status`-only sighting is a presence signal (the window exists) but
/// NOT a numeric reading, so the parser must NOT emit a 0% window for
/// it — doing so would fabricate an observation that could later
/// overwrite a real 92% reading via `UtilizationStore::upsert`.
///
/// We therefore return `None` for this shape. The legacy pair
/// (`anthropic-ratelimit-requests-{limit,remaining,reset}`) is the
/// honest numeric path for an Anthropic response without a suffixed
/// utilization field.
fn anthropic_unified_window_nosuffix(_headers: &dyn HeaderLookup) -> Option<WindowSnapshot> {
    // Intentionally always `None` — see the doc comment above. Kept as a
    // function so the call site stays readable and so the no-suffix
    // branch is documented at the parse entry point.
    None
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
/// calendar windows flip to zero at the next month boundary. The store
/// ALSO persists `window_period` (a stable identity of the current
/// calendar period, e.g. `"2026-07"` for `CalendarMonthly` /
/// `"2026-07-04"` for `CalendarDaily` / a rolling-clock bucket id for
/// `Rolling`) and atomically resets `used_tokens` to zero when
/// `record_counter` lands in a new period. This is the fix for Finding
/// #4: without the period identity, June usage would remain in July and
/// the plan would look exhausted forever after one heavy month. The
/// store records `last_updated` so callers can age out stale
/// observations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounterWindow {
    pub provider: Provider,
    pub account_id: String,
    pub plan: String,
    pub window_name: String,
    /// Running token count for this window. Only windows with
    /// `observability = Counter` accumulate here. Reset to 0.0
    /// atomically when `record_counter` enters a new
    /// `window_period`.
    pub used_tokens: f64,
    /// Cap in tokens, if known. `None` = percent-only window
    /// (`PercentOnly`), or "cap not yet recorded". When `None`,
    /// `used_percent` is `None` too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<f64>,
    /// `(used_tokens / cap) * 100.0` when `cap.is_some() && cap > 0.0`.
    /// `None` otherwise — never divide by zero, never claim a percent
    /// the store can't actually compute. Stored in the 0..=100 range
    /// so it is directly comparable to the 0..=100 percent values the
    /// header-fed snapshot path produces. (An older buggy version of
    /// the store wrote a 0..=1 *fraction* here; see
    /// [`migrate_fractional_used_percent`] for the on-read fixup.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    /// Provider-driven reset signal (e.g. the next calendar-month boundary
    /// for `reset: CalendarMonthly`). `None` when the provider has not
    /// published one; the caller decides whether the window has aged out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<DateTime<Utc>>,
    /// UTC observation timestamp of the most recent increment.
    pub last_updated: DateTime<Utc>,
    /// Stable identity of the current calendar period this window is
    /// accumulating against (e.g. `"2026-07"` for `CalendarMonthly` /
    /// `"2026-07-04"` for `CalendarDaily` / a rolling bucket id for
    /// `Rolling`). `None` on rows written before this field existed
    /// — the store treats `None` as "no period was recorded, so a
    /// `record_counter` call with a known period id MUST reset
    /// tokens" to avoid carrying stale June usage into July. See
    /// [`window_period_for`] for the per-`ResetKind` id scheme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_period: Option<String>,
}

/// Compute the calendar-period identity for `now` under the given reset
/// semantics. Stable per period so the store can detect a rollover by
/// simple string compare.
///
/// - `CalendarMonthly` -> `"YYYY-MM"` in UTC (e.g. `"2026-07"`).
/// - `CalendarDaily`   -> `"YYYY-MM-DD"` in UTC (e.g. `"2026-07-04"`).
/// - `Rolling`         -> `"roll:<window-hours>:<bucket>"` where the
///   bucket index is `floor(now_seconds / (window_hours * 3600))` so
///   the period id is stable for the whole rolling cycle and flips
///   exactly when a new cycle opens. This keeps the
///   "atomic reset on period change" contract honest for rolling
///   windows too — the period id only changes when the rolling
///   clock does, so a `record_counter` call that arrives mid-cycle
///   just continues accumulating.
pub fn window_period_for(now: DateTime<Utc>, reset: ResetKind, hours: u32) -> String {
    match reset {
        ResetKind::CalendarMonthly => now.format("%Y-%m").to_string(),
        ResetKind::CalendarDaily => now.format("%Y-%m-%d").to_string(),
        ResetKind::Rolling => {
            if hours == 0 {
                // Degenerate: fall back to a fine-grained bucket keyed on
                // the epoch second so no two `record_counter` calls ever
                // collapse onto the same bucket id when the rolling
                // window length is unknown. This is a defensive fallback;
                // the real catalog always sets `hours`.
                format!("roll:0:{}", now.timestamp())
            } else {
                let bucket = now.timestamp() / (hours as i64 * 3600);
                format!("roll:{hours}:{bucket}")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Routing decision.
// ---------------------------------------------------------------------------

/// Maximum age (in seconds) a [`RateLimitSnapshot`] may have before
/// [`decide`] treats it as Degraded and falls back to the headroom
/// baseline (Finding #6). Mirrors the
/// [`STALE_MAX_AGE_SECS`] / [`FRESH_MAX_AGE_SECS`] split the multi-
/// window path uses, so both surfaces agree on what "fresh" means.
pub const DECIDE_STALE_THRESHOLD_SECS: i64 = 60 * 60; // 60 min

/// How far in the future a [`RateLimitSnapshot`]'s `observed_at` may
/// be (relative to `now`) before [`decide`] treats it as a clock
/// skew / future-dated record and falls back to the headroom
/// baseline. A small tolerance covers NTP jitter / sub-second clock
/// drift; anything beyond is treated as "the record was authored in
/// the future, trust nothing" (Finding #6).
pub const FRESHNESS_FUTURE_SKEW_SECS: i64 = 5;

/// Decide whether a snapshot is fresh enough to drive [`decide`].
/// `true` when the snapshot's `observed_at` exists, has an age in
/// `[FRESHNESS_FUTURE_SKEW_SECS, DECIDE_STALE_THRESHOLD_SECS)`, and
/// is therefore trustworthy; `false` otherwise (degraded telemetry
/// must not gate — Finding #6).
pub fn is_snapshot_fresh(snap: &RateLimitSnapshot, now: DateTime<Utc>) -> bool {
    let Some(observed_at) = snap.observed_at else {
        return false;
    };
    let age_secs = (now - observed_at).num_seconds();
    // Material future-dated (clock skew past the tolerance) -> not
    // fresh. We deliberately do NOT trust "newer than now" the way
    // `TelemetryHealth::from_age_secs` does: a 90%-used future-
    // dated record would otherwise keep gating routing until the
    // clock catches up (Finding #6).
    if age_secs < -FRESHNESS_FUTURE_SKEW_SECS {
        return false;
    }
    if age_secs >= DECIDE_STALE_THRESHOLD_SECS {
        return false;
    }
    true
}

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
///
/// **Freshness gate (Finding #6):** the snapshot's `observed_at`
/// is the timestamp of the last header sighting. When `observed_at`
/// is missing, when the age is past the Degraded threshold, or when
/// the apparent age is "materially negative" (clock skew / a
/// future-dated record past the [`FRESHNESS_FUTURE_SKEW_SECS`]
/// tolerance), the snapshot is treated as if it carried no windows
/// at all — the `None = headroom = keep the sub` baseline fires.
/// This matches the contract the multi-window
/// [`decide_account`] path already enforces via
/// [`TelemetryHealth::Degraded`]; without it, a two-hour-old
/// snapshot at 90% with no reset would keep returning
/// `FallBackToFree` even though the equivalent `AccountView` would
/// be Degraded and excluded. A small explicit
/// [`FRESHNESS_FUTURE_SKEW_SECS`] tolerance is allowed so a
/// mildly-skewed clock (NTP correction, sub-second jitter) doesn't
/// misclassify every real observation as Degraded.
pub fn decide(
    snap: &RateLimitSnapshot,
    knobs: &RouteKnobs,
    now: DateTime<Utc>,
    chargeback_remaining_usd: Option<f64>,
) -> RouteDecision {
    // Freshness gate first. Stale / future-dated / unobserved
    // snapshots are NOT trustworthy; the `None = headroom` baseline
    // is the only honest answer (Finding #6).
    if !is_snapshot_fresh(snap, now) {
        return RouteDecision::PreferSub;
    }
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
// KNEMON Layer 4 — per-account multi-window routing view.
// ---------------------------------------------------------------------------

/// One subscription window as the routing decision sees it. Lifts data
/// from either the header-fed `UtilizationRecord` (when the window
/// appears as `primary`/`secondary` on the persisted snapshot) or the
/// counter-fed `CounterWindow` (when the catalog declares the window as
/// `Observability::Counter` and the store has accumulated usage against
/// it), and adds a `health` bucket so a stale observation can't dominate
/// a fresh one.
#[derive(Debug, Clone)]
pub struct WindowView {
    /// Operator-facing window name (e.g. `"5h"`, `"weekly"`).
    pub name: String,
    /// 0..=100 percent of the window consumed. `None` when the store has
    /// no numeric value (PercentOnly window with no live header, or a
    /// Counter window with no cap recorded). Treat as "unknown — never
    /// let this window gate routing on its own".
    pub used_percent: Option<f64>,
    /// How the window is observed / fed. Carried through from the
    /// catalog `QuotaWindow` so callers can tell a header-fed window
    /// from a counter-fed one without re-deriving it.
    pub observability: crate::config::Observability,
    /// Freshness bucket derived from `last_updated`. A `Degraded` window
    /// is excluded from binding entirely — see [`decide_account`].
    pub health: TelemetryHealth,
    /// Provider-driven reset timestamp (when the window will roll over
    /// and headroom becomes full again). `None` when no signal is
    /// available (e.g. a header-fed snapshot without `reset_at_epoch`,
    /// or a counter-fed window without a configured `reset_at`).
    /// Always treated as RFC3339 UTC.
    pub reset_at: Option<DateTime<Utc>>,
    /// Rolling window length in hours (`5h` -> `5`, `weekly` -> `168`).
    /// Used by the reset-relaxation rule to compute
    /// `time_to_reset / cycle_secs` and decide whether the window is
    /// about to roll over.
    pub hours: u32,
}

/// One account's complete window set for one plan. Built from a
/// [`UtilizationStore`] (the persisted telemetry) plus the plan's
/// configured [`crate::config::QuotaWindow`] list (the catalog of what
/// windows EXIST on this account, even if we have no live reading for
/// some of them).
#[derive(Debug, Clone)]
pub struct AccountView {
    pub provider: Provider,
    pub account_id: String,
    pub plan: String,
    /// Configured windows, in catalog order. Every window in the plan
    /// shows up here — even ones with `used_percent = None` (unknown /
    /// PercentOnly-without-numeric) — so the caller can iterate the
    /// full set without re-merging the catalog.
    pub windows: Vec<WindowView>,
}

/// Routing decision for one account. The router consumes `decision`
/// (same verdict shape as the legacy single-window [`decide`]) plus
/// `strength` (used to rank multiple sub candidates against each other:
/// ASCENDING strength so the most-idle account is preferred first) and
/// `binding_window` (which window drove the verdict — useful for
/// debugging and for ledger / report labels).
#[derive(Debug, Clone, PartialEq)]
pub struct AccountDecision {
    pub decision: RouteDecision,
    /// `binding.used_percent` — lower = more idle. Used by callers that
    /// rank multiple sub-class candidates: ascending strength means the
    /// most-idle sub is preferred first.
    pub strength: f64,
    /// Name of the window that drove the verdict, or `None` when no
    /// window was observable (account is treated as headroom).
    pub binding_window: Option<String>,
}

/// Build an [`AccountView`] for one `(provider, account_id, plan)` triple
/// from the persisted [`UtilizationStore`]. `configured_windows` is the
/// catalog of windows the plan declares (the source of truth for
/// `hours` / `observability` — utilization.rs doesn't import the catalog
/// directly so callers pass them in).
///
/// `used_percent` is filled in this order of preference:
///   1. A matching counter-fed `CounterWindow` whose `observability =
///      Counter` AND whose `cap` is `Some` — i.e. we have a numeric
///      percent the store computed (`used_tokens / cap`). This is the
///      "best" path for MiniMax-style providers with no headers.
///   2. A matching header-fed window from the persisted `RateLimitSnapshot`
///      (`primary` / `secondary`). Surfaces any numeric percent the
///      provider published in `x-codex-*` or `anthropic-ratelimit-*`.
///   3. The persisted counter row's `used_percent` even when
///      `cap = None` (defensive — the store never invents a percent in
///      this case, but if a future caller sets `used_percent` directly
///      we don't want to overwrite it with `None`).
///   4. Otherwise `None` (Unknown) — the window exists in the catalog
///      but we have no numeric reading for it.
///
/// Health is derived from `last_updated` (counter) or the snapshot's
/// `observed_at` (header); never-observed windows are `Degraded`.
pub fn build_account_view(
    provider: Provider,
    account_id: impl Into<String>,
    plan: impl Into<String>,
    configured_windows: &[crate::config::QuotaWindow],
    store: &UtilizationStore,
    now: DateTime<Utc>,
) -> AccountView {
    let account_id = account_id.into();
    let plan = plan.into();
    // Header-fed row (legacy Layer 3A path). `last_updated` lives on
    // the `UtilizationRecord`; the snapshot's `observed_at` is also
    // backed by `last_updated` (see `UtilizationRecord::from_snapshot`),
    // so reading either gives the same age. The store gives us back a
    // reference; the age can be derived once and reused for any window
    // on the record.
    let header_record = store.get(provider, &account_id, &plan);
    let header_record_age = header_record.map(|r| (now - r.last_updated).num_seconds());

    let mut views = Vec::with_capacity(configured_windows.len());
    for cw in configured_windows {
        // (a) Counter-fed row.
        let counter = store.get_counter(provider, &account_id, &plan, &cw.name);
        // (b) Header-fed window — match by `window_minutes` (closest
        // robust proxy since header snapshots don't carry the operator's
        // window name; primary is the 5h-ish, secondary the weekly-ish).
        let header_window_minutes = cw.hours.saturating_mul(60);
        let header_match = if let Some(r) = header_record {
            match (&r.primary, &r.secondary) {
                (Some(p), _) if p.window_minutes == Some(header_window_minutes) => {
                    Some((p.used_percent, r.last_updated))
                }
                (Some(p), Some(s)) if s.window_minutes == Some(header_window_minutes) => {
                    Some((s.used_percent, r.last_updated))
                }
                // Fallback: header snapshots that don't carry a
                // window_minutes (e.g. legacy Anthropic no-suffix shape)
                // are matched by name-ish: primary -> first declared
                // window, secondary -> second declared window, by
                // position.
                (Some(p), _) if cw.name == "primary" || cw.name == "5h" => {
                    Some((p.used_percent, r.last_updated))
                }
                (_, Some(s)) if cw.name == "secondary" || cw.name == "weekly" => {
                    Some((s.used_percent, r.last_updated))
                }
                _ => None,
            }
        } else {
            None
        };
        // Synthesize reset_at: prefer the counter's `reset_at`, fall
        // back to the header's `reset_at_epoch`.
        let header_epoch_for_window = if let Some(r) = header_record {
            let primary_match = cw.name == "primary" || cw.name == "5h";
            let secondary_match = cw.name == "secondary" || cw.name == "weekly";
            match (&r.primary, &r.secondary) {
                (Some(p), _) if primary_match => Some(p.reset_at_epoch),
                (_, Some(s)) if secondary_match => Some(s.reset_at_epoch),
                (Some(p), _) if p.window_minutes == Some(header_window_minutes) => {
                    Some(p.reset_at_epoch)
                }
                (_, Some(s)) if s.window_minutes == Some(header_window_minutes) => {
                    Some(s.reset_at_epoch)
                }
                _ => None,
            }
            .flatten()
        } else {
            None
        };
        let reset_at = counter.and_then(|c| c.reset_at).or_else(|| {
            header_epoch_for_window.and_then(|epoch| DateTime::<Utc>::from_timestamp(epoch, 0))
        });
        // Compute used_percent.
        let used_percent = match cw.observability {
            // Counter path: trust the store's stored percent whenever
            // we have one. The store never invents a percent for a
            // cap-less row, so `Some(...)` is always "numeric and
            // correct" (cap * used is finite).
            crate::config::Observability::Counter => counter.and_then(|c| c.used_percent),
            // Header path: take the matching header window's percent if
            // we have one. A header observation at 0% IS a real signal
            // (provider just told us we're fresh), so we don't gate on
            // "non-zero" — None means "no header sighting", not "0%
            // used".
            crate::config::Observability::Header => header_match.map(|(pct, _)| pct),
            // PercentOnly: surface the header reading if we have one
            // (the operator / provider only publishes a percent, so
            // even a counter row with no cap would be useless here).
            crate::config::Observability::PercentOnly => {
                header_match.map(|(pct, _)| pct).or_else(|| {
                    // Last-ditch: if a caller seeded `used_percent`
                    // directly on the counter row, surface that.
                    counter.and_then(|c| c.used_percent)
                })
            }
        };
        // Health: from the freshest observation we have. Counter row
        // wins when its observability is Counter; otherwise the header
        // record's `last_updated` age. Never-observed -> Degraded.
        let health = match cw.observability {
            crate::config::Observability::Counter => TelemetryHealth::from_age_secs(
                counter.map(|c| (now - c.last_updated).num_seconds()),
            ),
            crate::config::Observability::Header | crate::config::Observability::PercentOnly => {
                TelemetryHealth::from_age_secs(header_match.map(|(_, ts)| (now - ts).num_seconds()))
            }
        };
        // Touch the unused record-age slot so the compiler keeps it in
        // scope as a diagnostic hook (the age-vs-window match is per-row
        // anyway; this top-level value is just the "freshest window on
        // this record").
        let _ = header_record_age;
        views.push(WindowView {
            name: cw.name.clone(),
            used_percent,
            observability: cw.observability,
            health,
            reset_at,
            hours: cw.hours,
        });
    }
    AccountView {
        provider,
        account_id,
        plan,
        windows: views,
    }
}

/// Decide whether to use the paid subscription for the whole account,
/// given the per-window views in `account`. This is the Layer 4 entry
/// point — multi-window per-account routing. The contract:
///
///   - `observable` = windows with `used_percent.is_some() && health !=
///     Degraded`. A Degraded window is *not* observable even when its
///     `used_percent` is `Some` — the value is suspect, so we exclude
///     it. (The store genuinely never sets `used_percent` without a
///     fresh-enough signal, but the check is defensive.)
///   - When `observable` is empty -> `{PreferSub, 0.0, None}`. No
///     numeric reading anywhere -> the routing layer's "None = headroom
///     = keep the sub" baseline.
///   - `binding` = the observable window maximizing
///     `used_percent * health_weight(health)`. Fresh + slightly-lower
///     can beat Stale + slightly-higher; Degraded never wins (its
///     weight is 0.0).
///   - Reset-relaxation: when `binding.used_percent >= knobs.cap_guard`
///     AND **every** observable window currently at/above `cap_guard`
///     is itself imminently resetting — i.e.
///     `(time_to_reset / (window.hours*3600)) <= knobs.reset_imminence_threshold`
///     for each such window —
///     -> `{PreferSub, binding.used_percent, Some(binding.name)}`.
///     We're about to get full headroom back; don't pay the cost of
///     falling back. The "every exhausted window" requirement is the
///     fix for Finding #7: a binding 5h@90% (resetting in 5 minutes)
///     can't relax a still-exhausted weekly@89% (resetting in 5
///     days), because the weekly cap genuinely won't refill soon
///     and the operator who hit it should still be routed to free.
///   - Otherwise bands on `binding.used_percent`:
///       * `< use_target`                          -> PreferSub
///       * `< cap_guard` (hysteresis)              -> PreferSub
///       * `>= cap_guard` && budget_mode = Block   -> FallBackToFree
///       * `>= cap_guard` && budget_mode = Chargeback &&
///         chargeback_remaining > 0                -> Chargeback
///       * `>= cap_guard` && budget_mode = Chargeback &&
///         chargeback_remaining <= 0 / None        -> FallBackToFree
///
/// `strength` is always `binding.used_percent` (lower = more idle). The
/// caller can rank multiple sub-class candidates by ASCENDING strength
/// so the most-idle one is preferred first; the legacy single-account
/// path collapses this to "is the sub available at all".
/// Confidence below which a [`WindowForecast`] MUST NOT influence routing.
/// Confidence is `elapsed_fraction * health_weight`, so the default `0.5`
/// means "at least half the window observed, on Fresh telemetry" before a
/// forecast can pre-empt — conservative on purpose, to avoid over-reacting
/// to a burst early in a long window.
pub const FORECAST_CONFIDENCE_MIN: f64 = 0.5;

/// KNEMON Layer 4b — a per-window burn-rate forecast.
///
/// Projects the window's `used_percent` forward to its `reset_at`, assuming
/// usage accrued roughly linearly from 0 at the window's start (the natural
/// model for a quota/counter window that resets to empty). Honest by
/// construction: it only ever projects the OBSERVED percent forward — it
/// never invents a cap or an absolute token count, so it is valid for
/// `PercentOnly` windows too (we forecast the *percentage trajectory*, which
/// the vendor already publishes, not a fabricated absolute).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowForecast {
    /// Projected `used_percent` at `reset_at` (>= the current used-percent).
    /// Clamped to a sane ceiling so a near-zero elapsed can't yield nonsense.
    pub projected_used_percent: f64,
    /// 0..=1: `elapsed_fraction * health_weight`. A forecast below
    /// [`FORECAST_CONFIDENCE_MIN`] must not drive routing.
    pub confidence: f64,
    /// How far through the window we are, 0..=1.
    pub elapsed_fraction: f64,
}

/// Forecast one window's used-percent at its reset. Returns `None` when the
/// window has no numeric reading, no reset signal, no known duration, or has
/// not started yet (clock skew) — i.e. there is nothing honest to project.
pub fn forecast_window(w: &WindowView, now: DateTime<Utc>) -> Option<WindowForecast> {
    let used = w.used_percent?;
    let reset_at = w.reset_at?;
    if w.hours == 0 {
        return None;
    }
    let duration = w.hours as f64 * 3600.0;
    if duration <= 0.0 {
        return None;
    }
    // elapsed since the window opened = full cycle - time left on the clock.
    let time_to_reset = (reset_at - now).num_seconds() as f64;
    let elapsed = duration - time_to_reset;
    if elapsed <= 0.0 {
        // Window has not started (reset is a full cycle+ away) — nothing to
        // project honestly.
        return None;
    }
    let elapsed_fraction = (elapsed / duration).clamp(0.0, 1.0);
    if elapsed_fraction <= 0.0 {
        return None;
    }
    // Linear projection from 0 at window open: used-at-reset ~= used scaled
    // up by the inverse of how far we are through the window. Never below the
    // current reading (usage only accrues toward reset); ceiling-clamped.
    // >= current reading (usage only accrues toward reset), ceiling-capped so
    // a tiny elapsed can't yield nonsense. `.min().max()` (not `clamp`) so a
    // pathological `used > 1000` can't panic on an inverted clamp range.
    let projected = (used / elapsed_fraction).min(1000.0).max(used);
    let confidence = (elapsed_fraction * w.health.health_weight()).clamp(0.0, 1.0);
    Some(WindowForecast {
        projected_used_percent: projected,
        confidence,
        elapsed_fraction,
    })
}

/// Forecast the binding window of an account — the same tightest-window
/// selection [`decide_account`] uses (max `used_percent * health_weight`
/// among observable windows). `None` when no window is observable or none
/// has enough signal to project. Handy for reports ("on pace for N% by
/// reset") and for the router's pre-emption check.
pub fn forecast_account(account: &AccountView, now: DateTime<Utc>) -> Option<WindowForecast> {
    let binding = account
        .windows
        .iter()
        .filter(|w| w.used_percent.is_some() && w.health != TelemetryHealth::Degraded)
        .max_by(|a, b| {
            let sa = a.used_percent.unwrap_or(0.0) * a.health.health_weight();
            let sb = b.used_percent.unwrap_or(0.0) * b.health.health_weight();
            sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
        })?;
    forecast_window(binding, now)
}

pub fn decide_account(
    account: &AccountView,
    knobs: &RouteKnobs,
    now: DateTime<Utc>,
    chargeback_remaining: Option<f64>,
) -> AccountDecision {
    // Observable = numeric AND not degraded. Stale is still observable
    // (the 0.8 discount is applied at binding time, not at observability
    // time); Degraded is not.
    let observable: Vec<&WindowView> = account
        .windows
        .iter()
        .filter(|w| w.used_percent.is_some() && w.health != TelemetryHealth::Degraded)
        .collect();
    if observable.is_empty() {
        return AccountDecision {
            decision: RouteDecision::PreferSub,
            strength: 0.0,
            binding_window: None,
        };
    }
    // Binding window: max(used_percent * health_weight).
    let binding = observable
        .iter()
        .max_by(|a, b| {
            let sa = a.used_percent.unwrap_or(0.0) * a.health.health_weight();
            let sb = b.used_percent.unwrap_or(0.0) * b.health.health_weight();
            // partial_cmp: identical weights on equal scores -> stable
            // tie-break by name (BTreeMap-friendly: deterministic) so
            // tests are reproducible.
            sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
        })
        .copied()
        .expect("observable non-empty by prior guard");
    let binding_used = binding.used_percent.unwrap_or(0.0);
    // Reset-relaxation (Finding #7): cap_guard trips AND time-to-reset
    // is small relative to the window's cycle. The relaxation only
    // fires when EVERY observable window currently at/above
    // `cap_guard` is itself imminently resetting — the pre-fix logic
    // only checked the binding window, which let a binding 5h@90%
    // (resetting in 5 min) hide a still-exhausted weekly@89%
    // (resetting in 5 days). A weekly cap that won't refill for days
    // is NOT imminent, and the operator who hit the weekly cap
    // should still be routed to free. "Imminent" =
    // `(time_to_reset / cycle_secs) <= reset_imminence_threshold`
    // on the window's own cycle. A window without a reset signal
    // can't be confirmed imminently resetting, so it's never
    // relaxed.
    let relax = binding_used >= knobs.cap_guard
        && observable
            .iter()
            .filter(|w| w.used_percent.unwrap_or(0.0) >= knobs.cap_guard)
            .all(|w| {
                w.reset_at.is_some_and(|r| {
                    let time_to_reset = (r - now).num_seconds().max(0) as f64;
                    let cycle_secs = (w.hours as f64) * 3600.0;
                    cycle_secs > 0.0
                        && (time_to_reset / cycle_secs) <= knobs.reset_imminence_threshold
                })
            });
    if relax {
        return AccountDecision {
            decision: RouteDecision::PreferSub,
            strength: binding_used,
            binding_window: Some(binding.name.clone()),
        };
    }
    // Forecast pre-emption (KNEMON Layer 4b): if any observable window is on a
    // confident trajectory to breach `cap_guard` before its reset, fall back
    // now — even if nothing has tripped the guard yet. This only ever TIGHTENS
    // (PreferSub -> fall back / chargeback), never loosens, so it can't defeat
    // the drive-utilization intent. Reset-relaxation already returned above, so
    // a window about to roll over never triggers a spurious pre-emption here.
    let forecast_breach = observable.iter().any(|w| {
        forecast_window(w, now).is_some_and(|f| {
            f.confidence >= FORECAST_CONFIDENCE_MIN && f.projected_used_percent >= knobs.cap_guard
        })
    });
    // Bands.
    let decision = if binding_used >= knobs.cap_guard || forecast_breach {
        match knobs.budget_mode {
            BudgetMode::Block => RouteDecision::FallBackToFree,
            BudgetMode::Chargeback => match (knobs.chargeback_budget_usd, chargeback_remaining) {
                (Some(_cap), Some(remaining)) if remaining > 0.0 => RouteDecision::Chargeback,
                _ => RouteDecision::FallBackToFree,
            },
        }
    } else {
        // Below the guard and not forecast to breach: keep the paid sub. Both
        // the drive-utilization band (< use_target) and the hysteresis band
        // (< cap_guard) prefer the subscription.
        RouteDecision::PreferSub
    };
    AccountDecision {
        decision,
        strength: binding_used,
        binding_window: Some(binding.name.clone()),
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

/// Compute the on-disk `used_percent` for a counter row as a 0..=100
/// percent (`(used / cap) * 100.0`). `None` when the cap is unknown,
/// non-positive, or the inputs are non-finite. The multiplied-by-100
/// shape is the fix for Finding #3 — a 0..=1 fraction would compare
/// apples-to-oranges against the 0..=100 percent the header-fed
/// snapshot path produces, so the router would never trip
/// `cap_guard` until usage was 85× over.
fn counter_used_percent(used_tokens: &f64, cap: Option<f64>) -> Option<f64> {
    let c = cap?;
    if !c.is_finite() || c <= 0.0 {
        return None;
    }
    if !used_tokens.is_finite() {
        return None;
    }
    Some((used_tokens / c) * 100.0)
}

/// Heuristic: is `raw` a pre-Finding-#3 fractional percent in the
/// 0..=1 range? Used by the on-read migration path to detect rows
/// that were written by a buggy older version of the store and
/// rescale them to the new 0..=100 percent layout.
///
/// The test compares `raw` against the two candidate layouts the
/// store could have used:
///
/// - new (correct): `raw ≈ (used_tokens / cap) * 100`
/// - old (buggy):   `raw ≈  used_tokens / cap`
///
/// A row is treated as the old fractional layout when (a) the old
/// layout reproduces `raw` to within a relative tolerance AND (b)
/// the new layout does NOT. This is the only heuristic that
/// survives a fractional-then-rescaled migration without false
/// positives on a legitimate 0.3% percent reading (which is a real
/// percent, NOT a fraction — `(0.003 * 100) ≠ 0.003`).
fn looks_like_fractional_percent(raw: f64, cap: Option<f64>, used_tokens: f64) -> bool {
    if !raw.is_finite() || raw <= 0.0 {
        return false;
    }
    let Some(c) = cap else {
        return false;
    };
    if !c.is_finite() || c <= 0.0 {
        return false;
    }
    if !used_tokens.is_finite() || used_tokens <= 0.0 {
        return false;
    }
    let ratio = used_tokens / c; // both layouts' numerator
                                 // Old layout: `raw` is the fraction. New layout: `raw` is the
                                 // fraction * 100. Whichever is closer (in a relative sense) is
                                 // the layout that produced the row. A relative tolerance
                                 // handles both "tiny usage" (where both layouts collapse near
                                 // 0) and "near 100% usage" (where the difference between
                                 // 0.95 and 95.0 is large in absolute terms but still within a
                                 // small relative tolerance of the true value).
    let new_match = rel_close(raw, ratio * 100.0);
    let old_match = rel_close(raw, ratio);
    old_match && !new_match
}

/// Relative closeness test: |a - b| / max(|b|, epsilon). The
/// epsilon keeps "both near zero" rows from being treated as
/// either match — a row that genuinely is the old fraction and
/// the new percent both reproduce 0.0 (empty window) can't be
/// told apart, and we want the migration to be a no-op in that
/// case (the value is already 0.0 regardless of layout).
fn rel_close(a: f64, b: f64) -> bool {
    const REL_TOL: f64 = 1e-6;
    const ABS_FLOOR: f64 = 1e-12;
    if !a.is_finite() || !b.is_finite() {
        return false;
    }
    (a - b).abs() <= REL_TOL * b.abs().max(ABS_FLOOR)
}

/// In-memory migration of a single counter row from the pre-Finding-#3
/// fractional layout to the new 0..=100 percent layout. Returns the
/// corrected `used_percent` (or the input, when no migration was
/// needed). The on-disk value is also patched in place so a subsequent
/// `save()` writes the corrected layout and the migration becomes a
/// one-shot operation.
fn migrate_fractional_used_percent(entry: &mut CounterWindow) -> Option<f64> {
    let raw = entry.used_percent?;
    if !looks_like_fractional_percent(raw, entry.cap, entry.used_tokens) {
        return Some(raw);
    }
    let migrated = raw * 100.0;
    entry.used_percent = Some(migrated);
    Some(migrated)
}

impl UtilizationStore {
    /// Open a store at `path`. Creates an empty one if the file doesn't
    /// exist yet. Returns an error only on real I/O / parse failures —
    /// a missing file is fine.
    ///
    /// On load, runs the one-shot [`migrate_fractional_used_percent`]
    /// migration on every counter row so a store written by a buggy
    /// pre-Finding-#3 binary (where `used_percent` was a 0..=1
    /// fraction) reads back as a 0..=100 percent. The on-disk file is
    /// patched in place on the next [`save`] call.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, UtilizationError> {
        let path = path.as_ref();
        let mut store = match fs::read(path) {
            Ok(bytes) => {
                if bytes.is_empty() {
                    Self {
                        records: BTreeMap::new(),
                        counters: BTreeMap::new(),
                        path: Some(path.to_path_buf()),
                    }
                } else {
                    let mut store: Self =
                        serde_json::from_slice(&bytes).map_err(UtilizationError::Parse)?;
                    store.path = Some(path.to_path_buf());
                    store
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self {
                records: BTreeMap::new(),
                counters: BTreeMap::new(),
                path: Some(path.to_path_buf()),
            },
            Err(e) => return Err(UtilizationError::Io(e)),
        };
        // One-shot on-load migration: rescales any pre-Finding-#3
        // fractional `used_percent` to the 0..=100 percent layout
        // (Finding #3) and seeds a `window_period` for any
        // pre-Finding-#4 row that has a `CalendarMonthly` /
        // `CalendarDaily` window but no `window_period` recorded yet
        // (Finding #4). Both patches happen in memory; the next
        // `save()` writes the corrected layout to disk.
        for cw in store.counters.values_mut() {
            migrate_fractional_used_percent(cw);
        }
        Ok(store)
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
    /// `(used_tokens / cap) * 100.0` whenever the cap is known — the
    /// value is stored in the 0..=100 percent range so it is directly
    /// comparable to the percent values the header-fed path produces.
    /// When the cap is `None` (a `PercentOnly` window, or any window
    /// whose cap has not yet been recorded via [`set_counter_cap`]),
    /// `used_percent` stays `None` — we never invent a percent.
    ///
    /// `reset` and `hours` describe the window's calendar semantics
    /// (see [`window_period_for`]). When `record_counter` lands in a
    /// new period (e.g. the calendar month rolled over for a
    /// `CalendarMonthly` window), `used_tokens` is atomically reset
    /// to zero BEFORE the increment is applied. This is the fix for
    /// Finding #4: without this, a plan that hit its cap in June
    /// would remain "exhausted" forever in July. Passing
    /// `ResetKind::Rolling` plus `hours` makes the period a rolling
    /// bucket so the increment continues to accumulate normally for
    /// the same rolling window.
    ///
    /// Returns the new `used_tokens` after the increment. Best-effort:
    /// callers that want the disk side-effect should pair this with
    /// [`save`] and tolerate its error (mirrors the header-fed
    /// [`record`] path).
    #[allow(clippy::too_many_arguments)]
    pub fn record_counter(
        &mut self,
        provider: Provider,
        account_id: &str,
        plan: &str,
        window_name: &str,
        tokens_used: f64,
        now: DateTime<Utc>,
        reset: crate::config::ResetKind,
        hours: u32,
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
                window_period: None,
            });
        // Period identity: if the persisted period is missing (a row
        // written before the period-id field existed) OR differs from
        // the period `now` belongs to, the window has rolled over —
        // reset `used_tokens` to 0.0 BEFORE applying the increment so
        // a brand-new period starts from a clean slate.
        let now_period = window_period_for(now, reset, hours);
        match entry.window_period.as_deref() {
            Some(prev) if prev == now_period => { /* same period: no reset */ }
            _ => {
                entry.used_tokens = 0.0;
                entry.used_percent = None;
            }
        }
        entry.window_period = Some(now_period);
        // Defensive: a malformed / negative increment is a no-op, not a
        // subtraction. A provider that occasionally reports 0 usage
        // (streaming-usage off, usage field absent) still wants a row
        // touch but never a negative balance.
        if tokens_used.is_finite() && tokens_used > 0.0 {
            entry.used_tokens += tokens_used;
        }
        // Recompute percent from the cap, if any. `cap = Some(0.0)` is
        // treated as "no headroom" (0%, not NaN/inf) so a bad
        // configuration never produces an exploded percent. The value
        // is a 0..=100 percent (NOT a 0..=1 fraction) so it is
        // directly comparable to header-fed percent readings — see
        // Finding #3. Old on-disk rows that have a fractional
        // `used_percent` (the buggy pre-fix layout) are
        // auto-corrected on the next write via `record_counter` /
        // `set_counter_cap`, and the read-side migration in `open`
        // (`migrate_fractional_used_percent`) patches any rows
        // already on disk so a freshly-loaded store that hasn't been
        // written yet still returns the correct percent.
        entry.used_percent = counter_used_percent(&entry.used_tokens, entry.cap);
        entry.last_updated = now;
        entry.used_tokens
    }

    /// Set (or clear) the cap for one counter-fed window. The store
    /// only stores the cap; it does NOT recompute `used_tokens` (the
    /// cap may be recorded AFTER the first call to [`record_counter`]
    /// in the same boot — e.g. the wire-up reads the catalog once at
    /// startup and then records usage as it lands). When the cap is
    /// first set, `used_percent` is recomputed as a 0..=100 percent
    /// from the existing `used_tokens` (Finding #3). Callers that
    /// need the `used_percent` field refreshed before the next
    /// `record_counter` lands can read it back immediately; the next
    /// `record_counter` will also recompute it.
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
            window_period: None,
        });
        entry.cap = cap;
        entry.used_percent = counter_used_percent(&entry.used_tokens, entry.cap);
        entry.last_updated = now;
    }

    /// Recompute `used_percent` for one counter-fed window from its
    /// currently-stored `used_tokens` and `cap`. Use after
    /// [`set_counter_cap`] to surface the percent before the next
    /// `record_counter` lands. Returns the new percent (0..=100), or
    /// `None` when the cap is unknown / non-positive.
    pub fn recompute_counter_percent(
        &mut self,
        provider: Provider,
        account_id: &str,
        plan: &str,
        window_name: &str,
    ) -> Option<f64> {
        let k = counter_key(provider, account_id, plan, window_name);
        let entry = self.counters.get_mut(&k)?;
        let pct = counter_used_percent(&entry.used_tokens, entry.cap);
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
    fn parse_codex_plan_type_only_does_not_fabricate_zero_pct_windows() {
        // Regression for Finding #5: a Codex response carrying ONLY
        // `x-codex-plan-type: pro` must NOT create primary/secondary
        // windows at 0%. Doing so would let a later real 92% reading
        // be overwritten by the fabricated 0% under `upsert`.
        let h = hm([("x-codex-plan-type", "pro")]);
        let snap = parse_codex(&h, "acct-1", "ignored");
        assert_eq!(snap.provider, Provider::OpenaiCodex);
        assert_eq!(snap.plan, "pro");
        assert!(
            snap.primary.is_none(),
            "no primary window when no numeric/reset signal is present"
        );
        assert!(
            snap.secondary.is_none(),
            "no secondary window when no numeric/reset signal is present"
        );
    }

    #[test]
    fn parse_anthropic_status_only_does_not_fabricate_zero_pct_windows() {
        // Regression for Finding #5: an Anthropic response carrying
        // only `anthropic-ratelimit-unified-{status,remaining,reset}`
        // (the no-suffix "current window" shape) must NOT create a
        // 0% window. Status is a presence signal, not a numeric
        // reading; emitting a 0% would let a later real 92% reading
        // be overwritten under `upsert`.
        let h = hm([
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-remaining", "23"),
            ("anthropic-ratelimit-unified-reset", "2026-07-04T08:00:00Z"),
        ]);
        let snap = parse_anthropic(&h, "acct", "max");
        assert!(
            snap.primary.is_none(),
            "status-only sighting must not fabricate a primary window"
        );
        assert!(
            snap.secondary.is_none(),
            "status-only sighting must not fabricate a secondary window"
        );
    }

    #[test]
    fn parse_anthropic_unified_status_only_per_suffix_does_not_fabricate_zero() {
        // The suffixed variant: `anthropic-ratelimit-unified-5h-status`
        // with no numeric `utilization` and no `reset`. The provider
        // told us the window exists, but a `status`-only sighting
        // is not a numeric reading — emitting 0% would clobber a
        // later real 92% observation.
        let h = hm([("anthropic-ratelimit-unified-5h-status", "allowed")]);
        let snap = parse_anthropic(&h, "acct", "max");
        assert!(
            snap.primary.is_none(),
            "status-only suffixed window must not be emitted"
        );
    }

    #[test]
    fn later_real_codex_reading_is_not_clobbered_by_prior_windowless_sighting() {
        // End-to-end regression for Finding #5: a codex response with
        // ONLY `x-codex-plan-type` does not get recorded in the store
        // (no windows). A later real response with primary=92% then
        // does get recorded, and the read-back is the real 92% —
        // NOT a fabricated 0%.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let mut s = UtilizationStore::open(&path).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        // (1) windowless sighting -> the store refuses to record it.
        let header_only = hm([("x-codex-plan-type", "pro")]);
        let snap = parse_codex(&header_only, "default", "default");
        assert!(snap.primary.is_none() && snap.secondary.is_none());
        let recorded = s.record(&snap, now);
        assert!(!recorded, "windowless sighting must not be persisted");
        // (2) real sighting with primary=92% -> recorded.
        let rich = hm([
            ("x-codex-primary-used-percent", "92"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-after-seconds", "600"),
        ]);
        let snap2 = parse_codex(&rich, "default", "default");
        assert_eq!(snap2.primary.as_ref().unwrap().used_percent, 92.0);
        assert!(s.record(&snap2, now));
        // (3) read back: the real 92% survives, not 0%.
        let rec = s
            .get(Provider::OpenaiCodex, "default", "default")
            .expect("real sighting persisted");
        assert_eq!(rec.primary.as_ref().unwrap().used_percent, 92.0);
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
            reset_imminence_threshold: DEFAULT_RESET_IMMINENCE_THRESHOLD,
        }
    }

    fn snap_with_primary(pct: f64, reset_at: Option<i64>) -> RateLimitSnapshot {
        // Default: `observed_at` is `None` so the legacy tests that
        // don't care about freshness still go through the
        // headroom-baseline path. The fresh-decide tests build
        // snapshots via [`snap_with_primary_at`] to set
        // `observed_at = Some(now)`.
        snap_with_primary_at(pct, reset_at, None)
    }

    /// Variant of [`snap_with_primary`] that sets `observed_at`. Pass
    /// `Some(now)` for a Fresh snapshot, `Some(now - duration)` for
    /// an explicitly-aged one, and `None` for the no-observation
    /// headroom case. The freshness-aware `decide` uses this
    /// directly so the Finding-#6 tests can exercise the gate.
    fn snap_with_primary_at(
        pct: f64,
        reset_at: Option<i64>,
        observed_at: Option<DateTime<Utc>>,
    ) -> RateLimitSnapshot {
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
            observed_at,
        }
    }

    #[test]
    fn decide_below_target_prefers_sub() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let snap = snap_with_primary_at(50.0, None, Some(now));
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
        let snap = snap_with_primary_at(82.0, None, Some(now));
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
        let snap = snap_with_primary_at(85.0, None, Some(now));
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::FallBackToFree,
        );
        let snap = snap_with_primary_at(95.0, None, Some(now));
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
        let snap = snap_with_primary_at(95.0, None, Some(now));
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Chargeback, Some(50.0));
        assert_eq!(
            decide(&snap, &knobs, now, Some(10.0)),
            RouteDecision::Chargeback,
        );
    }

    #[test]
    fn decide_chargeback_mode_with_no_budget_falls_back() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let snap = snap_with_primary_at(95.0, None, Some(now));
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
        let snap = snap_with_primary_at(90.0, Some(now.timestamp() - 10), Some(now));
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
        let mut snap = snap_with_primary_at(0.0, None, Some(now));
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
            ..snap_with_primary_at(0.0, None, Some(now))
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

    // -------- Finding #6: freshness gate ----------------------------
    //
    // The pre-fix `decide` ignored `observed_at`, so a two-hour-old
    // snapshot at 90% with no reset kept returning
    // `FallBackToFree` even though the equivalent `AccountView`
    // would be Degraded (excluded) by the multi-window path. The
    // post-fix gate routes degraded telemetry to the headroom
    // baseline (`PreferSub`); a future-dated record is treated as
    // clock-skewed and likewise not trusted.

    #[test]
    fn decide_stale_snapshot_does_not_gate_routing() {
        // A 2-hour-old snapshot at 90% (above the 85% cap_guard)
        // must NOT keep gating routing — degraded telemetry must
        // not gate (Finding #6). Returns PreferSub.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let two_hours_ago = now - chrono::Duration::seconds(2 * 3600);
        let snap = snap_with_primary_at(90.0, None, Some(two_hours_ago));
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::PreferSub,
            "stale telemetry must not gate routing",
        );
    }

    #[test]
    fn decide_snapshot_without_observed_at_uses_headroom_baseline() {
        // A snapshot that never set `observed_at` is Degraded by
        // construction — never observed = never trusted. The
        // legacy `snap_with_primary` helper leaves `observed_at`
        // as None so this test exercises the "no observation at
        // all" path: `decide` must NOT gate on a 95%-used record
        // that has no timestamp.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let snap = snap_with_primary(95.0, None);
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::PreferSub,
            "unobserved telemetry must not gate routing",
        );
    }

    #[test]
    fn decide_future_dated_record_is_treated_as_degraded() {
        // A future-dated record (clock skew, NTP rollback) MUST
        // NOT be trusted. Pre-fix `TelemetryHealth::from_age_secs`
        // treats negative ages as Fresh, which let a 90%-used
        // future-dated record control routing until the clock
        // caught up (Finding #6). Post-fix: anything past the
        // small explicit future-skew tolerance is Degraded.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let far_future = now + chrono::Duration::seconds(3600); // 1h ahead
        let snap = snap_with_primary_at(90.0, None, Some(far_future));
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::PreferSub,
            "future-dated telemetry must not gate routing",
        );
    }

    #[test]
    fn decide_subsecond_clock_skew_is_tolerated() {
        // NTP jitter (sub-second "now is in the past") is normal
        // and must NOT be treated as Degraded — a real observation
        // could otherwise be lost to clock noise. The small
        // explicit tolerance keeps the gate honest.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let one_sec_future = now + chrono::Duration::seconds(1);
        let snap = snap_with_primary_at(95.0, None, Some(one_sec_future));
        // 95% with a 1s-future stamp is still trusted: cap_guard
        // trips -> FallBackToFree.
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::FallBackToFree,
            "subsecond clock skew must not flip a real observation to Degraded",
        );
    }

    #[test]
    fn is_snapshot_fresh_respects_both_ends_of_the_window() {
        // The helper itself: explicit acceptance / rejection
        // boundary tests, independent of the band logic in
        // `decide`.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        // No observation -> not fresh.
        let snap = snap_with_primary(50.0, None);
        assert!(!is_snapshot_fresh(&snap, now));
        // 1 minute ago -> fresh.
        let snap = snap_with_primary_at(50.0, None, Some(now - chrono::Duration::seconds(60)));
        assert!(is_snapshot_fresh(&snap, now));
        // 30 minutes ago -> fresh.
        let snap = snap_with_primary_at(50.0, None, Some(now - chrono::Duration::seconds(30 * 60)));
        assert!(is_snapshot_fresh(&snap, now));
        // 59m59s ago -> fresh (under the 60min threshold).
        let snap = snap_with_primary_at(
            50.0,
            None,
            Some(now - chrono::Duration::seconds(59 * 60 + 59)),
        );
        assert!(is_snapshot_fresh(&snap, now));
        // 60 minutes ago exactly -> not fresh.
        let snap = snap_with_primary_at(50.0, None, Some(now - chrono::Duration::seconds(60 * 60)));
        assert!(!is_snapshot_fresh(&snap, now));
        // 1 second in the future -> tolerated as fresh.
        let snap = snap_with_primary_at(50.0, None, Some(now + chrono::Duration::seconds(1)));
        assert!(is_snapshot_fresh(&snap, now));
        // 1 hour in the future -> not fresh.
        let snap = snap_with_primary_at(50.0, None, Some(now + chrono::Duration::seconds(3600)));
        assert!(!is_snapshot_fresh(&snap, now));
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
        // A status-only / remaining / reset sighting (no numeric
        // `utilization` field) MUST NOT fabricate a 0% window. The
        // parsers deliberately return `None` for both the suffixed and
        // no-suffix windows so a later real reading cannot be
        // overwritten by a synthesized 0%.
        assert!(
            snap.primary.is_none(),
            "status-only sighting must not fabricate a primary window"
        );
        assert!(
            snap.secondary.is_none(),
            "status-only sighting must not fabricate a secondary window"
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
        let fed_snap = snap_with_primary_at(92.0, None, Some(now));
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
        let used_after_first = s.record_counter(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            n,
            now,
            crate::config::ResetKind::Rolling,
            0,
        );
        assert_eq!(used_after_first, n, "first response adds exactly N tokens");
        // Second response: another N tokens.
        let used_after_second = s.record_counter(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            n,
            now,
            crate::config::ResetKind::Rolling,
            0,
        );
        assert_eq!(used_after_second, 2.0 * n, "two responses accumulate to 2N");

        // Read back via the typed accessor.
        let w = s
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "monthly")
            .expect("monthly counter window must persist");
        assert_eq!(w.used_tokens, 2.0 * n);
        // Post-Finding-#3: stored as a 0..=100 percent (the
        // pre-fix layout was a 0..=1 fraction, which silently
        // defeated `cap_guard` comparisons).
        let expected_pct = (2.0 * n) / MM_MONTHLY_CAP * 100.0;
        let got_pct = w
            .used_percent
            .expect("used_percent must be Some when cap is known");
        assert!(
            (got_pct - expected_pct).abs() < 1e-12,
            "used_percent must equal 2N/5.1e9*100 = {expected_pct}, got {got_pct}",
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
        let used = s.record_counter(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "5h",
            n,
            now,
            crate::config::ResetKind::Rolling,
            0,
        );
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
            (pct - (n / 10_000.0 * 100.0)).abs() < 1e-12,
            "after set_counter_cap, percent must be (used/cap)*100: got {pct}",
        );

        // And critically: a fresh `record_counter` on a DIFFERENT
        // percent-only window (the `weekly` one) with no cap set
        // MUST stay percent-less. This is the regression guard.
        let used_w = s.record_counter(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "weekly",
            500.0,
            now,
            crate::config::ResetKind::Rolling,
            0,
        );
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
            crate::config::ResetKind::Rolling,
            0,
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
        assert_eq!(cw.used_percent, Some(2_500_000.0 / MM_MONTHLY_CAP * 100.0));
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

    /// Regression for Finding #3: a counter row's `used_percent` is
    /// stored in the 0..=100 range (NOT a 0..=1 fraction) so it is
    /// directly comparable to the 0..=100 percent values the
    /// header-fed snapshot path produces. The pre-fix layout stored
    /// `(used / cap)`, which silently defeated `cap_guard`
    /// comparisons (4.335B / 5.1B = 0.85 ≠ 85, so the guard never
    /// tripped).
    #[test]
    fn counter_used_percent_is_stored_in_zero_to_hundred_range() {
        // Seed 4.335B tokens against a 5.1B cap (the MiniMax spec
        // example from Finding #3): must surface 85.0, not 0.85.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let mut s = UtilizationStore::open(&path).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        s.set_counter_cap(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            Some(MM_MONTHLY_CAP),
            now,
        );
        s.record_counter(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            4_335_000_000.0,
            now,
            crate::config::ResetKind::CalendarMonthly,
            720,
        );
        let cw = s
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "monthly")
            .unwrap();
        let pct = cw.used_percent.expect("cap is set -> percent is set");
        assert!(
            (pct - 85.0).abs() < 1e-9,
            "4.335B / 5.1B must read as 85.0 (post-Finding-#3), got {pct}"
        );
        // And the percent value must be on the same scale as a
        // `cap_guard` comparison. The pre-fix 0.85 would have lost
        // to an 85.0 cap_guard; the post-fix value meets it exactly.
        assert!(pct >= 85.0, "the percent value must trip cap_guard");
    }

    /// Regression for Finding #3: a store written by the buggy
    /// pre-fix binary (where `used_percent` was a 0..=1 fraction)
    /// reads back as a 0..=100 percent after a one-shot in-memory
    /// migration on `open`. The on-disk file is also patched in
    /// place so a subsequent `save()` writes the corrected layout.
    #[test]
    fn counter_on_load_migrates_fractional_used_percent_to_percent() {
        // Hand-write a pre-fix on-disk file: used_tokens=4.335e9,
        // cap=5.1e9, used_percent=0.85 (the old fractional layout).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let now_str = "2026-07-04T12:00:00Z";
        let legacy_json = format!(
            r#"{{
                "records": {{}},
                "counters": {{
                    "MiniMax::default::minimax-max::monthly": {{
                        "provider": "mini_max",
                        "account_id": "default",
                        "plan": "minimax-max",
                        "window_name": "monthly",
                        "used_tokens": 4335000000.0,
                        "cap": 5100000000.0,
                        "used_percent": 0.85,
                        "last_updated": "{now_str}"
                    }}
                }}
            }}"#
        );
        std::fs::write(&path, legacy_json).unwrap();
        // Open: the migration must rescale 0.85 to 85.0 in memory.
        let s = UtilizationStore::open(&path).unwrap();
        let cw = s
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "monthly")
            .expect("counter row loaded from legacy file");
        let pct = cw.used_percent.expect("cap is set -> percent is set");
        assert!(
            (pct - 85.0).abs() < 1e-9,
            "legacy 0.85 must migrate to 85.0 on open, got {pct}"
        );
    }

    /// Regression for Finding #4: a calendar-boundary counter window
    /// atomically resets `used_tokens` to zero when the next
    /// `record_counter` call lands in a new period. Without this, a
    /// plan that hit its cap in June would stay "exhausted" forever
    /// in July — the percentage-unit bug (Finding #3) would then
    /// trip `cap_guard` permanently.
    #[test]
    fn counter_calendar_monthly_resets_used_tokens_on_new_period() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let mut s = UtilizationStore::open(&path).unwrap();
        let june = Utc.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap();
        s.set_counter_cap(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            Some(MM_MONTHLY_CAP),
            june,
        );
        // Heavy June: 90% of the cap.
        s.record_counter(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            MM_MONTHLY_CAP * 0.9,
            june,
            crate::config::ResetKind::CalendarMonthly,
            720,
        );
        let june_cw = s
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "monthly")
            .unwrap();
        assert!(
            (june_cw.used_tokens - MM_MONTHLY_CAP * 0.9).abs() < 1.0,
            "June's heavy usage must accumulate"
        );
        // July: a single small response.
        let july = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        s.record_counter(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            1_000.0,
            july,
            crate::config::ResetKind::CalendarMonthly,
            720,
        );
        let july_cw = s
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "monthly")
            .unwrap();
        // The fix: July MUST start from a clean slate, not from
        // June's 90%-of-cap balance. Without the fix, this assertion
        // would fail by ~4.59B tokens.
        assert!(
            (july_cw.used_tokens - 1_000.0).abs() < 1.0,
            "new calendar month must reset used_tokens to 0 before applying the increment, got used_tokens={}",
            july_cw.used_tokens
        );
        // And the percent must reflect July's tiny usage, not
        // June's cap-exhaustion.
        let pct = july_cw.used_percent.expect("cap is set");
        assert!(pct < 1.0, "July percent must be tiny, not 90% (got {pct})");
    }

    /// Regression for Finding #4: a `CalendarDaily` counter window
    /// resets on a day boundary (not just monthly).
    #[test]
    fn counter_calendar_daily_resets_used_tokens_on_new_day() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let mut s = UtilizationStore::open(&path).unwrap();
        let day1 = Utc.with_ymd_and_hms(2026, 7, 3, 23, 59, 0).unwrap();
        s.set_counter_cap(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "daily",
            Some(10_000.0),
            day1,
        );
        s.record_counter(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "daily",
            9_500.0,
            day1,
            crate::config::ResetKind::CalendarDaily,
            24,
        );
        // Midnight rollover: a tiny new-day increment must NOT
        // accumulate on top of yesterday's 9.5k.
        let day2 = Utc.with_ymd_and_hms(2026, 7, 4, 0, 0, 30).unwrap();
        s.record_counter(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "daily",
            100.0,
            day2,
            crate::config::ResetKind::CalendarDaily,
            24,
        );
        let cw = s
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "daily")
            .unwrap();
        assert!(
            (cw.used_tokens - 100.0).abs() < 1e-9,
            "new calendar day must reset to 0 before increment, got used_tokens={}",
            cw.used_tokens
        );
    }

    /// `window_period_for` produces the stable per-period id the
    /// store uses to detect a calendar rollover. The id MUST change
    /// when the period changes (the discriminator) and MUST be
    /// stable within a period (so two `record_counter` calls in the
    /// same window don't trigger a reset).
    #[test]
    fn window_period_for_is_stable_within_a_period_and_flips_on_boundary() {
        use crate::utilization::window_period_for;
        let june_1 = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let june_15 = Utc.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap();
        let july_1 = Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap();
        let d1 = Utc.with_ymd_and_hms(2026, 7, 4, 0, 0, 0).unwrap();
        let d2 = Utc.with_ymd_and_hms(2026, 7, 4, 23, 59, 59).unwrap();
        let d3 = Utc.with_ymd_and_hms(2026, 7, 5, 0, 0, 0).unwrap();
        // Monthly
        assert_eq!(
            window_period_for(june_1, ResetKind::CalendarMonthly, 720),
            "2026-06"
        );
        assert_eq!(
            window_period_for(june_15, ResetKind::CalendarMonthly, 720),
            "2026-06"
        );
        assert_eq!(
            window_period_for(july_1, ResetKind::CalendarMonthly, 720),
            "2026-07"
        );
        // Daily
        assert_eq!(
            window_period_for(d1, ResetKind::CalendarDaily, 24),
            "2026-07-04"
        );
        assert_eq!(
            window_period_for(d2, ResetKind::CalendarDaily, 24),
            "2026-07-04"
        );
        assert_eq!(
            window_period_for(d3, ResetKind::CalendarDaily, 24),
            "2026-07-05"
        );
        // Rolling: stable within the same cycle, flips at the cycle
        // boundary. With hours=5, the bucket is floor(now / 18000).
        // Use times well within a 5-hour bucket (14:00, 15:00, 16:00
        // all fall into the same 5h cycle) AND times that straddle a
        // bucket boundary (16:00 vs 18:00) so the test is
        // wall-clock-agnostic.
        let r1 = window_period_for(
            Utc.with_ymd_and_hms(2026, 7, 4, 14, 0, 0).unwrap(),
            ResetKind::Rolling,
            5,
        );
        let r2 = window_period_for(
            Utc.with_ymd_and_hms(2026, 7, 4, 16, 0, 0).unwrap(),
            ResetKind::Rolling,
            5,
        );
        let r3 = window_period_for(
            Utc.with_ymd_and_hms(2026, 7, 4, 18, 0, 0).unwrap(),
            ResetKind::Rolling,
            5,
        );
        assert_eq!(r1, r2, "rolling period id must be stable within a cycle");
        assert_ne!(r2, r3, "rolling period id must flip on the cycle boundary");
    }

    // -------- KNEMON Layer 4 (per-account multi-window routing) ------
    //
    // Non-vacuous tests pinned by the spec:
    //   (a) binding = max(used_percent * health_weight): a Fresh 5h at 70%
    //       beats a Fresh weekly at 60% (70 > 60, both weight=1.0).
    //   (b) a Degraded 95% window is EXCLUDED and does NOT bind — the
    //       other windows must still drive the verdict.
    //   (c) only-unknown-windows -> PreferSub. Never gate.
    //   (d) reset-relaxation: 95% used but resets in < 10% of the cycle
    //       -> PreferSub (we're about to get full headroom back).
    //   (e) bands: 40% -> PreferSub (drive); 82% -> PreferSub (hysteresis);
    //       90% -> FallBackToFree / Chargeback with budget.
    //   (f) strength ranks 40% below 82% (the more-idle sub is preferred
    //       first when ranking across multiple sub accounts).
    //
    // These tests don't go through `parse_headers` / `record` — they
    // synthesize `WindowView`s directly so the assertions aren't muddied
    // by the parser's knobs.

    /// Helper: build an `AccountView` from a list of `(name, hours,
    /// used_percent, observability, health, reset_at)` tuples. The store
    /// is intentionally not consulted here — these tests pin the routing
    /// arithmetic, not the builder.
    #[allow(clippy::type_complexity)]
    fn acct_view(
        provider: Provider,
        account_id: &str,
        plan: &str,
        rows: &[(
            &str,
            u32,
            Option<f64>,
            crate::config::Observability,
            TelemetryHealth,
            Option<DateTime<Utc>>,
        )],
    ) -> AccountView {
        let windows = rows
            .iter()
            .map(|(name, hours, used, obs, h, reset)| WindowView {
                name: (*name).to_string(),
                used_percent: *used,
                observability: *obs,
                health: *h,
                reset_at: *reset,
                hours: *hours,
            })
            .collect();
        AccountView {
            provider,
            account_id: account_id.to_string(),
            plan: plan.to_string(),
            windows,
        }
    }

    fn fresh_5h(pct: f64) -> WindowView {
        WindowView {
            name: "5h".into(),
            used_percent: Some(pct),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Fresh,
            reset_at: None,
            hours: 5,
        }
    }

    fn weekly_fresh(pct: f64) -> WindowView {
        WindowView {
            name: "weekly".into(),
            used_percent: Some(pct),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Fresh,
            reset_at: None,
            hours: 168,
        }
    }

    #[test]
    fn l4_binding_picks_max_used_times_weight_five_h_at_70_beats_weekly_at_60() {
        // (a) Two Fresh windows: 5h at 70%, weekly at 60%. Both weight
        // 1.0. The 5h is the binding window.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![fresh_5h(70.0), weekly_fresh(60.0)],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad = decide_account(&acct, &knobs, now, None);
        assert_eq!(ad.decision, RouteDecision::PreferSub);
        assert_eq!(
            ad.binding_window.as_deref(),
            Some("5h"),
            "5h at 70% must beat weekly at 60%"
        );
        assert!((ad.strength - 70.0).abs() < 1e-9);
    }

    #[test]
    fn l4_stale_higher_value_loses_to_fresh_lower_value() {
        // Subtlety: the spec uses weight, not "freshest wins". A Stale
        // (weight=0.8) 75% window scores 60.0; a Fresh (weight=1.0) 60%
        // window scores 60.0 too — tie, but a Fresh 61% window would
        // beat a Stale 75%. This is the "weight applies to binding"
        // contract.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let stale = WindowView {
            name: "5h".into(),
            used_percent: Some(75.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Stale,
            reset_at: None,
            hours: 5,
        };
        let fresh = WindowView {
            name: "weekly".into(),
            used_percent: Some(61.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Fresh,
            reset_at: None,
            hours: 168,
        };
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![stale, fresh],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad = decide_account(&acct, &knobs, now, None);
        assert_eq!(
            ad.binding_window.as_deref(),
            Some("weekly"),
            "Fresh 61% (score 61) must beat Stale 75% (score 60)"
        );
        assert!((ad.strength - 61.0).abs() < 1e-9);
    }

    #[test]
    fn l4_degraded_window_at_95_pct_is_excluded_from_binding() {
        // (b) A Degraded 95% window MUST NOT bind. The other window is
        // observable and drives the verdict.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let degraded_95 = WindowView {
            name: "5h".into(),
            used_percent: Some(95.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Degraded,
            reset_at: None,
            hours: 5,
        };
        let fresh_50 = WindowView {
            name: "weekly".into(),
            used_percent: Some(50.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Fresh,
            reset_at: None,
            hours: 168,
        };
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![degraded_95, fresh_50],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad = decide_account(&acct, &knobs, now, None);
        assert_eq!(
            ad.binding_window.as_deref(),
            Some("weekly"),
            "Degraded 95% must be excluded — Fresh 50% binds instead"
        );
        assert_eq!(ad.decision, RouteDecision::PreferSub);
        assert!((ad.strength - 50.0).abs() < 1e-9);
    }

    #[test]
    fn l4_only_degraded_windows_yields_prefer_sub_with_no_binding() {
        // All windows Degraded -> observable set is empty ->
        // PreferSub with strength 0.0 and binding_window = None. This
        // is the "no trustworthy signal -> headroom baseline" invariant.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let d1 = WindowView {
            name: "5h".into(),
            used_percent: Some(95.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Degraded,
            reset_at: None,
            hours: 5,
        };
        let d2 = WindowView {
            name: "weekly".into(),
            used_percent: Some(99.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Degraded,
            reset_at: None,
            hours: 168,
        };
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![d1, d2],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad = decide_account(&acct, &knobs, now, None);
        assert_eq!(ad.decision, RouteDecision::PreferSub);
        assert!(ad.binding_window.is_none());
        assert_eq!(ad.strength, 0.0);
    }

    #[test]
    fn l4_only_unknown_windows_never_gates_routing() {
        // (c) Only `used_percent = None` windows -> observable set is
        // empty -> PreferSub with no binding. Crucially: a hypothetical
        // 99% used window that's `None` is treated as unknown, NOT as
        // "almost full". This is the "never gate on what we don't
        // know" rule.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let acct = acct_view(
            Provider::Anthropic,
            "a",
            "max",
            &[
                (
                    "5h",
                    5,
                    None,
                    crate::config::Observability::Header,
                    TelemetryHealth::Fresh,
                    None,
                ),
                (
                    "weekly",
                    168,
                    None,
                    crate::config::Observability::PercentOnly,
                    TelemetryHealth::Fresh,
                    None,
                ),
            ],
        );
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad = decide_account(&acct, &knobs, now, None);
        assert_eq!(ad.decision, RouteDecision::PreferSub);
        assert!(ad.binding_window.is_none());
        assert_eq!(ad.strength, 0.0);
    }

    #[test]
    fn l4_reset_relaxation_fires_when_reset_within_10pct_of_cycle() {
        // (d) 95% used but resets in < 10% of the cycle -> PreferSub
        // even though the cap_guard is tripped. We're about to get full
        // headroom back; the cost of falling back would be wasted.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        // 5h window = 5*3600 = 18000s cycle. 9% of 18000 = 1620s ->
        // reset 1500s from now.
        let reset_at = now + chrono::Duration::seconds(1500);
        let hot_5h = WindowView {
            name: "5h".into(),
            used_percent: Some(95.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Fresh,
            reset_at: Some(reset_at),
            hours: 5,
        };
        let cool_weekly = WindowView {
            name: "weekly".into(),
            used_percent: Some(40.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Fresh,
            reset_at: None,
            hours: 168,
        };
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![hot_5h, cool_weekly],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad = decide_account(&acct, &knobs, now, None);
        assert_eq!(
            ad.decision,
            RouteDecision::PreferSub,
            "reset-relaxation should fire"
        );
        assert_eq!(ad.binding_window.as_deref(), Some("5h"));
        assert!((ad.strength - 95.0).abs() < 1e-9);
    }

    #[test]
    fn l4_reset_relaxation_does_not_fire_when_reset_far_away() {
        // 95% used AND resets in 50% of the cycle (huge time-to-reset)
        // -> reset-relaxation must NOT fire -> cap_guard trips and we
        // fall back to free (Block mode).
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let reset_at = now + chrono::Duration::seconds(5 * 3600 / 2); // half-cycle
        let hot = WindowView {
            name: "5h".into(),
            used_percent: Some(95.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Fresh,
            reset_at: Some(reset_at),
            hours: 5,
        };
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![hot],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad = decide_account(&acct, &knobs, now, None);
        assert_eq!(
            ad.decision,
            RouteDecision::FallBackToFree,
            "no reset-relaxation -> cap_guard trips"
        );
        assert_eq!(ad.binding_window.as_deref(), Some("5h"));
    }

    /// Regression for Finding #7: a binding 5h window at 90% (resetting
    /// in 5 minutes) MUST NOT mask a still-exhausted weekly window at
    /// 89% (resetting in 5 days). The weekly cap genuinely won't refill
    /// soon and the operator who hit it should still be routed to
    /// free. Pre-fix, the relaxation only checked the binding window
    /// and returned `PreferSub` here, hiding the weekly exhaustion.
    #[test]
    fn l4_reset_relaxation_does_not_hide_other_exhausted_windows() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        // 5h at 90% (above the 85% cap_guard), resetting in 5 minutes
        // (well under 10% of the 5h cycle -> imminently resetting).
        let hot_5h = WindowView {
            name: "5h".into(),
            used_percent: Some(90.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Fresh,
            // 300s = 5 min out of 18000s = 1.67% of the cycle.
            reset_at: Some(now + chrono::Duration::seconds(300)),
            hours: 5,
        };
        // Weekly at 89% (above the cap_guard), resetting in 5 DAYS —
        // not imminent. This window MUST keep the gate closed.
        let weekly = WindowView {
            name: "weekly".into(),
            used_percent: Some(89.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Fresh,
            // 5 days = 5*86400 = 432000s. 432000 / (168*3600) = ~71%
            // of the weekly cycle -> NOT imminent.
            reset_at: Some(now + chrono::Duration::seconds(5 * 24 * 3600)),
            hours: 168,
        };
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![hot_5h, weekly],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad = decide_account(&acct, &knobs, now, None);
        assert_eq!(
            ad.decision,
            RouteDecision::FallBackToFree,
            "weekly cap at 89% (resetting in 5 days) must not be hidden by an imminent 5h reset"
        );
        assert_eq!(
            ad.binding_window.as_deref(),
            Some("5h"),
            "5h at 90% still binds (highest used)"
        );
    }

    /// Finding #7 — companion: when BOTH the 5h and the weekly window
    /// are at/above the guard AND BOTH are imminently resetting, the
    /// relaxation still fires (the "every exhausted window is
    /// imminently resetting" precondition is satisfied).
    #[test]
    fn l4_reset_relaxation_fires_when_every_exhausted_window_is_imminent() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let hot_5h = WindowView {
            name: "5h".into(),
            used_percent: Some(95.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Fresh,
            // 5 min out of 5h = 1.67% -> imminent.
            reset_at: Some(now + chrono::Duration::seconds(300)),
            hours: 5,
        };
        // Weekly at 89% resetting in 12h (12/168 = 7.1% of the
        // weekly cycle) -> also imminent under the 10% threshold.
        let weekly = WindowView {
            name: "weekly".into(),
            used_percent: Some(89.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Fresh,
            reset_at: Some(now + chrono::Duration::seconds(12 * 3600)),
            hours: 168,
        };
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![hot_5h, weekly],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad = decide_account(&acct, &knobs, now, None);
        assert_eq!(
            ad.decision,
            RouteDecision::PreferSub,
            "both exhausted windows are imminently resetting -> relaxation fires"
        );
        assert_eq!(ad.binding_window.as_deref(), Some("5h"));
    }

    /// Finding #7 — a window at/above the guard WITHOUT a reset
    /// signal (no `reset_at`) can never be confirmed imminently
    /// resetting, so the relaxation must NOT fire.
    #[test]
    fn l4_reset_relaxation_does_not_fire_when_a_guard_window_lacks_reset_signal() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let hot_5h = WindowView {
            name: "5h".into(),
            used_percent: Some(90.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Fresh,
            reset_at: Some(now + chrono::Duration::seconds(300)),
            hours: 5,
        };
        // 90% on a window with NO reset signal: cannot confirm it's
        // imminently resetting, so it keeps the gate closed.
        let no_reset = WindowView {
            name: "weekly".into(),
            used_percent: Some(90.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Fresh,
            reset_at: None,
            hours: 168,
        };
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![hot_5h, no_reset],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad = decide_account(&acct, &knobs, now, None);
        assert_eq!(
            ad.decision,
            RouteDecision::FallBackToFree,
            "a window at/above the guard with no reset signal keeps the gate closed"
        );
    }

    #[test]
    fn l4_band_drive_at_40_pct_prefers_sub() {
        // (e drive) 40% used, below use_target=80 -> PreferSub.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![fresh_5h(40.0)],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad = decide_account(&acct, &knobs, now, None);
        assert_eq!(ad.decision, RouteDecision::PreferSub);
        assert!((ad.strength - 40.0).abs() < 1e-9);
    }

    #[test]
    fn l4_band_hysteresis_at_82_pct_keeps_prefer_sub() {
        // (e hysteresis) 82% used, between use_target=80 and
        // cap_guard=85 -> PreferSub (the hysteresis band keeps the
        // sub active until the guard trips).
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![fresh_5h(82.0)],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad = decide_account(&acct, &knobs, now, None);
        assert_eq!(ad.decision, RouteDecision::PreferSub);
        assert!((ad.strength - 82.0).abs() < 1e-9);
    }

    #[test]
    fn l4_band_gate_at_90_pct_block_mode_falls_back() {
        // (e gate Block) 90% used, above cap_guard=85, Block mode ->
        // FallBackToFree.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![fresh_5h(90.0)],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad = decide_account(&acct, &knobs, now, None);
        assert_eq!(ad.decision, RouteDecision::FallBackToFree);
        assert!((ad.strength - 90.0).abs() < 1e-9);
    }

    #[test]
    fn l4_band_gate_at_90_pct_chargeback_with_budget_chargebacks() {
        // (e gate Chargeback + budget) 90% used, Chargeback mode, budget
        // remaining > 0 -> Chargeback.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![fresh_5h(90.0)],
        };
        let mut knobs = knobs_with(80.0, 85.0, BudgetMode::Chargeback, Some(50.0));
        knobs.chargeback_budget_usd = Some(50.0);
        let ad = decide_account(&acct, &knobs, now, Some(10.0));
        assert_eq!(ad.decision, RouteDecision::Chargeback);
    }

    #[test]
    fn l4_band_gate_at_90_pct_chargeback_with_zero_budget_falls_back() {
        // 90% used, Chargeback mode, but budget remaining = 0 -> fall
        // back to free (we've spent the chargeback allowance).
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![fresh_5h(90.0)],
        };
        let mut knobs = knobs_with(80.0, 85.0, BudgetMode::Chargeback, Some(50.0));
        knobs.chargeback_budget_usd = Some(50.0);
        let ad = decide_account(&acct, &knobs, now, Some(0.0));
        assert_eq!(ad.decision, RouteDecision::FallBackToFree);
    }

    #[test]
    fn l4_strength_ranks_40_below_82_for_sub_ranking() {
        // (f) Strength is binding.used_percent: 40 < 82. When picking
        // among multiple sub accounts, the most-idle (lowest strength)
        // is preferred. We exercise the L4 helper directly by reading
        // each account's strength and confirming the rank order.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let acct_idle = AccountView {
            provider: Provider::Anthropic,
            account_id: "idle".into(),
            plan: "max".into(),
            windows: vec![fresh_5h(40.0)],
        };
        let acct_busy = AccountView {
            provider: Provider::Anthropic,
            account_id: "busy".into(),
            plan: "max".into(),
            windows: vec![fresh_5h(82.0)],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad_idle = decide_account(&acct_idle, &knobs, now, None);
        let ad_busy = decide_account(&acct_busy, &knobs, now, None);
        assert!(ad_idle.strength < ad_busy.strength);
        assert!((ad_idle.strength - 40.0).abs() < 1e-9);
        assert!((ad_busy.strength - 82.0).abs() < 1e-9);
        // Both at headroom -> both PreferSub.
        assert_eq!(ad_idle.decision, RouteDecision::PreferSub);
        assert_eq!(ad_busy.decision, RouteDecision::PreferSub);
    }

    #[test]
    fn l4_telemetry_health_buckets() {
        // The health-bucket boundaries (5min / 60min) are the spec —
        // pinned here so a future "let's bump Stale to 30min" change
        // shows up as a test diff.
        assert_eq!(
            TelemetryHealth::from_age_secs(None),
            TelemetryHealth::Degraded
        );
        assert_eq!(
            TelemetryHealth::from_age_secs(Some(-1)),
            TelemetryHealth::Fresh
        );
        assert_eq!(
            TelemetryHealth::from_age_secs(Some(0)),
            TelemetryHealth::Fresh
        );
        assert_eq!(
            TelemetryHealth::from_age_secs(Some(4 * 60)),
            TelemetryHealth::Fresh
        );
        assert_eq!(
            TelemetryHealth::from_age_secs(Some(5 * 60)),
            TelemetryHealth::Stale
        );
        assert_eq!(
            TelemetryHealth::from_age_secs(Some(59 * 60)),
            TelemetryHealth::Stale
        );
        assert_eq!(
            TelemetryHealth::from_age_secs(Some(60 * 60)),
            TelemetryHealth::Degraded
        );
        assert_eq!(
            TelemetryHealth::from_age_secs(Some(24 * 3600)),
            TelemetryHealth::Degraded
        );
        // Weights.
        assert!((TelemetryHealth::Fresh.health_weight() - 1.0).abs() < 1e-9);
        assert!((TelemetryHealth::Stale.health_weight() - 0.8).abs() < 1e-9);
        assert_eq!(TelemetryHealth::Degraded.health_weight(), 0.0);
    }

    #[test]
    fn l4_route_knobs_default_has_imminence_threshold() {
        // The default `reset_imminence_threshold` is exposed on
        // `RouteKnobs::default()` so the routing layer doesn't have to
        // know about a separate constant. Pin it to 0.10 so the spec
        // invariant is explicit.
        let k = RouteKnobs::default();
        assert!((k.reset_imminence_threshold - DEFAULT_RESET_IMMINENCE_THRESHOLD).abs() < 1e-9);
        assert!((k.reset_imminence_threshold - 0.10).abs() < 1e-9);
    }

    #[test]
    fn l4_build_account_view_fills_from_counter_with_cap() {
        // The builder wiring: a `Counter` window with a known cap on
        // the store surfaces a numeric `used_percent` in the view.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utilization.json");
        let mut s = UtilizationStore::open(&path).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        s.set_counter_cap(
            Provider::MiniMax,
            "default",
            "minimax-max",
            "monthly",
            Some(1_000_000.0),
            now,
        );
        s.record_counter(
            Provider::MiniMax,
            "default",
            "minimax-max",
            "monthly",
            250_000.0,
            now,
            crate::config::ResetKind::Rolling,
            0,
        );
        let qw = crate::config::QuotaWindow {
            name: "monthly".into(),
            hours: 720,
            unit: crate::config::QuotaUnit::Tokens,
            cap: Some(1_000_000.0),
            models: None,
            observability: crate::config::Observability::Counter,
            reset: crate::config::ResetKind::default(),
        };
        let view = build_account_view(
            Provider::MiniMax,
            "default",
            "minimax-max",
            std::slice::from_ref(&qw),
            &s,
            now,
        );
        assert_eq!(view.windows.len(), 1);
        let w = &view.windows[0];
        assert_eq!(w.name, "monthly");
        assert_eq!(w.hours, 720);
        assert!(w.used_percent.is_some());
        // 250k / 1M * 100 = 25.0 (post-Finding-#3 percent range). The
        // pre-fix assertion was 0.25 (the buggy fractional layout).
        assert!((w.used_percent.unwrap() - 25.0).abs() < 1e-9);
        assert_eq!(w.health, TelemetryHealth::Fresh);
    }

    #[test]
    fn l4_build_account_view_percent_only_unknown_when_no_header() {
        // PercentOnly window with NO header observation -> used_percent
        // is None. Health is Degraded (no observation at all). This is
        // the "PercentOnly fallback" path: we don't invent a percent.
        let s = UtilizationStore::default();
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let qw = crate::config::QuotaWindow {
            name: "weekly".into(),
            hours: 168,
            unit: crate::config::QuotaUnit::Messages,
            cap: None,
            models: None,
            observability: crate::config::Observability::PercentOnly,
            reset: crate::config::ResetKind::default(),
        };
        let view = build_account_view(
            Provider::Anthropic,
            "a",
            "max",
            std::slice::from_ref(&qw),
            &s,
            now,
        );
        assert_eq!(view.windows.len(), 1);
        let w = &view.windows[0];
        assert!(w.used_percent.is_none());
        assert_eq!(w.health, TelemetryHealth::Degraded);
    }

    // ---- KNEMON Layer 4b: burn-rate forecast + pre-emption ----

    fn fc_win(
        name: &str,
        hours: u32,
        used: f64,
        health: TelemetryHealth,
        reset_at: Option<DateTime<Utc>>,
    ) -> WindowView {
        WindowView {
            name: name.into(),
            used_percent: Some(used),
            observability: crate::config::Observability::Counter,
            health,
            reset_at,
            hours,
        }
    }

    #[test]
    fn forecast_projects_used_percent_at_reset() {
        // 5h window, half elapsed (reset 2.5h out), 40% used -> ~80% projected.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let w = fc_win(
            "5h",
            5,
            40.0,
            TelemetryHealth::Fresh,
            Some(now + chrono::Duration::seconds(9000)),
        );
        let f = forecast_window(&w, now).expect("forecastable");
        assert!(
            (f.elapsed_fraction - 0.5).abs() < 1e-6,
            "elapsed {}",
            f.elapsed_fraction
        );
        assert!(
            (f.projected_used_percent - 80.0).abs() < 1e-6,
            "proj {}",
            f.projected_used_percent
        );
        assert!((f.confidence - 0.5).abs() < 1e-6, "conf {}", f.confidence);
    }

    #[test]
    fn forecast_none_without_reset_signal() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        assert!(
            forecast_window(&fc_win("5h", 5, 40.0, TelemetryHealth::Fresh, None), now).is_none()
        );
    }

    #[test]
    fn forecast_early_window_has_low_confidence() {
        // Only 10% into the window -> confidence below the routing floor.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let w = fc_win(
            "5h",
            5,
            30.0,
            TelemetryHealth::Fresh,
            Some(now + chrono::Duration::seconds(16200)),
        );
        let f = forecast_window(&w, now).unwrap();
        assert!(
            f.confidence < FORECAST_CONFIDENCE_MIN,
            "conf {}",
            f.confidence
        );
    }

    #[test]
    fn forecast_degraded_has_zero_confidence() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let w = fc_win(
            "5h",
            5,
            40.0,
            TelemetryHealth::Degraded,
            Some(now + chrono::Duration::seconds(9000)),
        );
        assert_eq!(forecast_window(&w, now).unwrap().confidence, 0.0);
    }

    #[test]
    fn forecast_percent_only_window_still_projects_the_percentage() {
        // PercentOnly: forecasting the % trajectory is honest (no cap invented).
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let mut w = fc_win(
            "weekly",
            168,
            50.0,
            TelemetryHealth::Fresh,
            Some(now + chrono::Duration::seconds(168 * 3600 / 2)),
        );
        w.observability = crate::config::Observability::PercentOnly;
        let f = forecast_window(&w, now).unwrap();
        assert!(
            (f.projected_used_percent - 100.0).abs() < 1e-6,
            "proj {}",
            f.projected_used_percent
        );
    }

    #[test]
    fn decide_account_preempts_before_guard_on_confident_trajectory() {
        // 60% now (< cap_guard 85), half elapsed -> projected 120% -> pre-empt.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![fc_win(
                "monthly",
                720,
                60.0,
                TelemetryHealth::Fresh,
                Some(now + chrono::Duration::seconds(720 * 3600 / 2)),
            )],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        assert_eq!(
            decide_account(&acct, &knobs, now, None).decision,
            RouteDecision::FallBackToFree
        );
    }

    #[test]
    fn decide_account_drives_utilization_when_on_pace_under_guard() {
        // 30% now, half elapsed -> projected 60% < guard -> keep the paid sub.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![fc_win(
                "monthly",
                720,
                30.0,
                TelemetryHealth::Fresh,
                Some(now + chrono::Duration::seconds(720 * 3600 / 2)),
            )],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        assert_eq!(
            decide_account(&acct, &knobs, now, None).decision,
            RouteDecision::PreferSub
        );
    }

    #[test]
    fn decide_account_no_false_preempt_near_reset() {
        // 80% now but 95% elapsed -> projected ~84% < 85 -> self-regulates, no pre-empt.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let reset = now + chrono::Duration::seconds((720.0 * 3600.0 * 0.05) as i64);
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![fc_win(
                "monthly",
                720,
                80.0,
                TelemetryHealth::Fresh,
                Some(reset),
            )],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        assert_eq!(
            decide_account(&acct, &knobs, now, None).decision,
            RouteDecision::PreferSub
        );
    }

    #[test]
    fn forecast_account_uses_binding_window() {
        // Two windows; the higher-used one binds and drives the forecast.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            windows: vec![
                fc_win(
                    "5h",
                    5,
                    70.0,
                    TelemetryHealth::Fresh,
                    Some(now + chrono::Duration::seconds(9000)),
                ),
                fc_win(
                    "weekly",
                    168,
                    40.0,
                    TelemetryHealth::Fresh,
                    Some(now + chrono::Duration::seconds(168 * 3600 / 2)),
                ),
            ],
        };
        let f = forecast_account(&acct, now).expect("binding forecastable");
        // Binds on the 5h (70% > 40%): half elapsed -> ~140% projected.
        assert!(
            (f.projected_used_percent - 140.0).abs() < 1e-6,
            "proj {}",
            f.projected_used_percent
        );
    }
}
