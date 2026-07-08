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
use fs2::FileExt;
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

/// Tolerance for future-dated observations. A record whose `observed_at`
/// is in the future by no more than this many seconds is treated as
/// Fresh (small NTP drift / clock skew); a larger future offset means
/// the clock has rolled back and the record can't be trusted.
///
/// Without this constant, the original code collapsed ANY negative age
/// to Fresh, which let a clock rollback persist as "Fresh 90%" and gate
/// routing until the wall clock caught up (Finding #6).
const FUTURE_TIMESTAMP_TOLERANCE_SECS: i64 = 60;

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
            // Negative ages: small NTP drift / clock skew within
            // `FUTURE_TIMESTAMP_TOLERANCE_SECS` is treated as Fresh;
            // a larger future offset means the clock has rolled back
            // and the record can't be trusted (Finding #6 — collapse to
            // Degraded rather than letting the future-dated reading
            // gate routing until the wall clock catches up).
            Some(s) if s < -FUTURE_TIMESTAMP_TOLERANCE_SECS => TelemetryHealth::Degraded,
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
    /// Subscription is at its guard, but a configured chargeback budget still
    /// has room, so the caller may continue on the explicitly paid path.
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
        // Codex: must match every header `parse_codex` actually consults.
        // The previous version only matched three of the ten headers the
        // parser reads, so a Codex response that carried ONLY a header
        // outside that set (e.g. a proxy that strips
        // `x-codex-primary-used-percent` but forwards
        // `x-codex-secondary-used-percent`, or a stripped-shape response
        // carrying only `x-codex-credits-has-credits` /
        // `x-codex-primary-reset-at` / `x-codex-primary-window-minutes`)
        // would fall through every branch and `parse_headers` would
        // return `None` — silently dropping a real secondary-window
        // reading. The hard invariant is "absent is unknown, never 0",
        // and the corollary is "a recognizable Codex header set must
        // route to the Codex parser" (Finding: detect/parse drift).
        if headers.get("x-codex-plan-type").is_some()
            || headers.get("x-codex-active-limit").is_some()
            || headers.get("x-codex-credits-has-credits").is_some()
            || headers.get("x-codex-primary-used-percent").is_some()
            || headers.get("x-codex-primary-window-minutes").is_some()
            || headers.get("x-codex-primary-reset-at").is_some()
            || headers.get("x-codex-primary-reset-after-seconds").is_some()
            || headers.get("x-codex-secondary-used-percent").is_some()
            || headers.get("x-codex-secondary-window-minutes").is_some()
            || headers.get("x-codex-secondary-reset-at").is_some()
            || headers
                .get("x-codex-secondary-reset-after-seconds")
                .is_some()
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
///
/// `used_percent` is `Option<f64>` (Finding #5): a status-only header set
/// (Anthropic's `unified-{status,remaining,reset}` with no `limit`, or
/// Codex's bare `x-codex-plan-type: pro`) is no longer materialized as a
/// "0% used" window — instead, the parser only constructs a window when
/// an actual numeric reading or a valid `limit`/`remaining` pair is
/// present, and `used_percent = None` means "known to exist, no number
/// yet". The routing layer treats `None` like an unknown window: it
/// contributes no signal and cannot bind.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowSnapshot {
    /// 0..=100 percent of the window consumed. May exceed 100 when over.
    /// `None` when the window is known to exist but no numeric value
    /// was available (status-only / remaining-only header sets) — never
    /// fabricates a `0.0` reading.
    #[serde(default)]
    pub used_percent: Option<f64>,
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
#[serde(deny_unknown_fields)]
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
    raw.trim()
        .trim_end_matches('%')
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
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

    let primary_used = headers
        .get("x-codex-primary-used-percent")
        .and_then(parse_pct);
    let primary = WindowSnapshot {
        used_percent: primary_used,
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
                    .filter(|secs| *secs >= 0)
                    .and_then(|secs| Utc::now().timestamp().checked_add(secs))
            }),
        label: Some("primary".to_string()),
    };
    let secondary_used = headers
        .get("x-codex-secondary-used-percent")
        .and_then(parse_pct);
    let secondary = WindowSnapshot {
        used_percent: secondary_used,
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
                    .filter(|secs| *secs >= 0)
                    .and_then(|secs| Utc::now().timestamp().checked_add(secs))
            }),
        label: Some("secondary".to_string()),
    };

    // Finding #5: a Codex response that only carries `x-codex-plan-type`
    // (no percent, no reset, no window length) used to materialize both
    // primary and secondary windows at a fabricated 0%. We now leave
    // `primary` / `secondary` as `None` unless there's an actual numeric
    // reading OR a reset signal — a bare plan label isn't a window.
    let primary_present = primary.used_percent.is_some() || primary.reset_at_epoch.is_some();
    let secondary_present = secondary.used_percent.is_some() || secondary.reset_at_epoch.is_some();
    snap.primary = if primary_present { Some(primary) } else { None };
    snap.secondary = if secondary_present {
        Some(secondary)
    } else {
        None
    };
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
            (Some(l), Some(r)) if l > 0 && (0..=l).contains(&r) => {
                Some(((l - r) as f64 / l as f64) * 100.0)
            }
            _ => None,
        };
        if let Some(p) = pct {
            snap.primary = Some(WindowSnapshot {
                used_percent: Some(p),
                window_minutes: None,
                reset_at_epoch: reset,
                label: Some("requests".to_string()),
            });
        }
        let (limit, remaining, reset) = anthropic_legacy_pair(headers, "tokens");
        let pct = match (limit, remaining) {
            (Some(l), Some(r)) if l > 0 && (0..=l).contains(&r) => {
                Some(((l - r) as f64 / l as f64) * 100.0)
            }
            _ => None,
        };
        if let Some(p) = pct {
            snap.secondary = Some(WindowSnapshot {
                used_percent: Some(p),
                window_minutes: None,
                reset_at_epoch: reset,
                label: Some("tokens".to_string()),
            });
        }
    }

    snap
}

fn anthropic_unified_window(headers: &dyn HeaderLookup, suffix: &str) -> Option<WindowSnapshot> {
    // Finding #5: a server that publishes `status` (or just `utilization`)
    // without a numeric value used to fabricate a 0% reading. We now
    // leave `used_percent = None` when no numeric utilization was
    // available — the window is still recorded (status-only IS a
    // signal that the window exists), but it contributes no percent.
    // We don't gate on `status` here so a `utilization`-only header set
    // still parses.
    let status = headers.get(&format!("anthropic-ratelimit-unified-{suffix}-status"));
    let util = headers
        .get(&format!("anthropic-ratelimit-unified-{suffix}-utilization"))
        .and_then(parse_pct);
    let reset_at = headers
        .get(&format!("anthropic-ratelimit-unified-{suffix}-reset"))
        .and_then(parse_epoch_seconds);
    // If the caller sent no unified-* headers at all for this suffix,
    // return None so the caller doesn't fall back to a fake window.
    if status.is_none()
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
/// without a `limit` we cannot compute a real percent, so we surface
/// `used_percent = None` (Finding #5 — never fabricate a numeric reading
/// that the headers didn't actually publish) and forward the reset. The
/// snapshot is only emitted when at least one of the three headers is
/// present; otherwise the caller can keep falling back to the legacy pair.
fn anthropic_unified_window_nosuffix(headers: &dyn HeaderLookup) -> Option<WindowSnapshot> {
    let status = headers.get("anthropic-ratelimit-unified-status");
    let remaining = headers.get("anthropic-ratelimit-unified-remaining");
    let reset = headers.get("anthropic-ratelimit-unified-reset");
    if status.is_none() && remaining.is_none() && reset.is_none() {
        return None;
    }
    Some(WindowSnapshot {
        // No `limit` published in this shape and no numeric utilization —
        // honest representation is "known window, no number". The caller
        // treats `None` like an unknown reading: this window contributes
        // no signal and cannot bind (degraded headroom baseline).
        used_percent: None,
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
/// Calendar windows reset at their next period boundary. Rolling windows
/// persist timestamped increments and retain only the trailing configured
/// number of hours, so locally observed usage ages out without a provider
/// reset signal.
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
    /// `(used_tokens / cap) * 100.0` when `cap.is_some() && cap > 0.0`,
    /// in the same 0..=100 percent scale as [`WindowSnapshot::used_percent`]
    /// and [`WindowView::used_percent`] (Finding #3). The pre-fix
    /// implementation stored the bare ratio (0.85 for 85%); the router
    /// then compared it against `cap_guard = 85` and would not gate
    /// until 85x the cap. `None` otherwise — never divide by zero,
    /// never claim a percent the store can't actually compute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    /// Provider-driven reset signal (e.g. the next calendar-month boundary
    /// for `reset: CalendarMonthly`). `None` when the provider has not
    /// published one; the caller decides whether the window has aged out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<DateTime<Utc>>,
    /// Calendar window identity for `CounterWindow` rows that have a
    /// calendar-shaped reset (Finding #4). Format depends on the
    /// catalog: monthly windows use `"YYYY-MM"`, daily windows use
    /// `"YYYY-MM-DD"`. When this row's `period_id` disagrees with the
    /// period computed from `now`, [`record_counter`] atomically resets
    /// `used_tokens` (and `used_percent`) before applying the new
    /// increment. `None` for rolling (non-calendar) windows — those
    /// never reset on a calendar boundary, by construction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_id: Option<String>,
    /// Rolling-window length. `None` for calendar windows and legacy
    /// unconfigured counters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolling_hours: Option<u32>,
    /// Timestamped token increments used to recompute a trailing rolling
    /// window. Older stores have no buckets; configuration migrates their
    /// aggregate into one bucket at the row's previous `last_updated`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub increments: Vec<CounterIncrement>,
    /// UTC observation timestamp of the most recent increment.
    pub last_updated: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounterIncrement {
    pub observed_at: DateTime<Utc>,
    pub tokens: f64,
}

// ---------------------------------------------------------------------------
// Routing decision.
// ---------------------------------------------------------------------------

/// Effective used-percent for the snapshot, taking `reset_at_epoch` into
/// account. When the provider-published reset time is in the past, the
/// window has rolled over and headroom is full again.
///
/// `WindowSnapshot.used_percent` is `Option<f64>` (Finding #5): a window
/// known to exist but with no numeric value (status-only Anthropic
/// headers) returns 0% here so the legacy [`decide`] path doesn't trip
/// its own cap_guard on a fabricated 0%. The L4 path filters `None`
/// windows out of `observable` entirely, so a status-only window there
/// contributes no signal — both paths agree that "no numeric reading"
/// is never a reason to gate routing on a 0%.
pub fn effective_used(snap: &RateLimitSnapshot, now: DateTime<Utc>) -> f64 {
    let now_epoch = now.timestamp();
    let pct = |w: &WindowSnapshot| -> f64 {
        match w.reset_at_epoch {
            Some(t) if t <= now_epoch => 0.0,
            _ => w.used_percent.unwrap_or(0.0),
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
/// Freshness gate (Finding #6 + adversarial review Z-18): the legacy
/// path used to ignore `observed_at` entirely, so a two-hour-old 90%
/// snapshot kept gating routing even though the equivalent
/// [`AccountView`] would mark it Degraded and exclude it from binding.
/// We now derive [`TelemetryHealth`] from `observed_at` whenever the
/// caller actually sets it (the live wire-up via [`parse_headers`]
/// always does), and FAIL CLOSED on anything Degraded — the same
/// "trust nothing more than 60 minutes old" invariant
/// [`decide_account`] already honors. A Degraded snapshot is
/// untrustworthy either direction (it may have refilled, but it also
/// may not have), so the only safe routing decision is to NOT pin the
/// paid sub through it. A Fresh / Stale snapshot continues through
/// the normal use_target / cap_guard bands; a Degraded one returns
/// [`RouteDecision::FallBackToFree`] regardless of the snapshot's
/// `used_percent` or `has_credits` — preferring the subscription on
/// data we can't trust would be the fail-OPEN bug the adversarial
/// review caught (Z-18). Material future-dated `observed_at` (more
/// than [`FUTURE_TIMESTAMP_TOLERANCE_SECS`] ahead of `now`) is treated
/// as clock-skewed Degraded rather than Fresh.
///
/// A snapshot with `observed_at = None` is trusted at face value (no
/// freshness inference): the live wire-up always sets `observed_at`
/// (so this branch only hits synthetic test fixtures / internal
/// callers that know what they're doing — same trust contract as the
/// pre-fix code). Callers that want the fail-closed path on absent
/// telemetry should branch on `None` themselves BEFORE handing a
/// snapshot to `decide()` (see `candidate_eligible` in
/// [`crate::scenarios`], Z-19).
pub fn decide(
    snap: &RateLimitSnapshot,
    knobs: &RouteKnobs,
    now: DateTime<Utc>,
    chargeback_remaining_usd: Option<f64>,
) -> RouteDecision {
    // Stale reset: window rolled over -> full headroom.
    let used = effective_used(snap, now);

    // Freshness gate (Z-18, fail-closed): only applied when the caller
    // supplied an `observed_at`. The live capture path always does;
    // tests / internal callers that pass `observed_at = None` keep the
    // legacy trust contract. A Degraded snapshot is untrustworthy — we
    // must NOT use it to keep the sub eligible (a 99% reading may
    // still be 99%, but it may also have refilled) and we must NOT use
    // it to fall back on a low reading (the 5% may have spiked to 95%
    // since the last sighting). The only safe verdict is to fall
    // back to free; the router then either picks a Free class
    // candidate or surfaces a routing error to the caller.
    if let Some(observed_at) = snap.observed_at {
        let snap_age = (now - observed_at).num_seconds();
        if TelemetryHealth::from_age_secs(Some(snap_age)) == TelemetryHealth::Degraded {
            return RouteDecision::FallBackToFree;
        }
    }

    // No credits (Codex-specific) is a gating signal only while the snapshot
    // itself is trustworthy. A degraded `false` may have rolled over or been
    // replenished and must not pin routing away from the subscription.
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
    /// Provider-published credit availability. `Some(false)` is a hard
    /// fallback signal even when the snapshot contains no rate-limit windows.
    pub has_credits: Option<bool>,
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
        //
        // `HeaderMatch` carries `(Option<f64>, DateTime)` now that
        // `WindowSnapshot.used_percent` is `Option<f64>` (Finding #5):
        // a header observation at 0% is `Some(0.0)`; a status-only
        // Anthropic window is `None`. We forward both as-is so the
        // downstream `decide_account` filter
        // (`used_percent.is_some() && health != Degraded`) naturally
        // excludes the no-number windows from binding. A header
        // observation at 0% IS a real signal (provider just told us
        // we're fresh), so we don't gate on "non-zero" — None means
        // "no header sighting" or "no numeric value", not "0% used".
        let counter_is_current = counter.is_none_or(|c| match cw.reset {
            crate::config::ResetKind::Rolling => true,
            crate::config::ResetKind::CalendarMonthly => {
                c.period_id.as_deref() == Some(now.format("%Y-%m").to_string().as_str())
            }
            crate::config::ResetKind::CalendarDaily => {
                c.period_id.as_deref() == Some(now.format("%Y-%m-%d").to_string().as_str())
            }
        });
        let used_percent: Option<f64> = match cw.observability {
            // Counter path: trust the store's stored percent whenever
            // we have one. The store never invents a percent for a
            // cap-less row, so `Some(...)` is always "numeric and
            // correct" (cap * used is finite).
            crate::config::Observability::Counter if counter_is_current => {
                counter.and_then(|c| effective_counter_percent(c, now))
            }
            crate::config::Observability::Counter => Some(0.0),
            // Header path: take the matching header window's percent
            // if we have one (Some = real reading, None = no numeric
            // value). Either is forwarded unchanged.
            crate::config::Observability::Header => header_match.and_then(|(pct, _)| pct),
            // PercentOnly: surface the header reading if we have one
            // (the operator / provider only publishes a percent, so
            // even a counter row with no cap would be useless here).
            //
            // The counter row is intentionally NOT consulted here.
            // PercentOnly windows have no API-exposed cap, so any
            // percent derived from `c.used_tokens / c.cap` would be a
            // fabricated, "computed" percent that PercentOnly windows
            // must never carry (the earlier "last-ditch"
            // `effective_counter_percent(c, now)` fallback leaked a
            // computed percent whenever a counter row happened to
            // have a cap set — e.g. legacy data or a misconfigured
            // wire-up — making the router treat a forged reading as
            // a real one and gate on it). The only legitimate source
            // for a PercentOnly window is the header reading; when
            // that's absent the window is unknown (None) and cannot
            // gate routing on its own.
            crate::config::Observability::PercentOnly => header_match.and_then(|(pct, _)| pct),
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
        has_credits: header_record.and_then(|record| {
            (TelemetryHealth::from_age_secs(header_record_age) != TelemetryHealth::Degraded)
                .then_some(record.has_credits)
                .flatten()
        }),
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
///     AND `(time_to_reset / (binding.hours*3600)) <= knobs.reset_imminence_threshold`
///     -> `{PreferSub, binding.used_percent, Some(binding.name)}`.
///     We're about to get full headroom back; don't pay the cost of
///     falling back.
///   - Hard cap gating uses the raw percentage of every observable window;
///     health-weighted binding is only for ranking and diagnostics.
///   - Otherwise bands on the observable windows:
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
    if account.has_credits == Some(false) {
        return AccountDecision {
            decision: RouteDecision::FallBackToFree,
            strength: 100.0,
            binding_window: None,
        };
    }
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
    // A health discount may influence which window is most useful for ranking
    // and diagnostics, but it must never turn an observed cap breach into
    // headroom. For example, Stale 100% scores 80 while Fresh 84% scores 84;
    // the latter may remain the binding diagnostic, while the former still
    // hard-gates the account.
    let hard_cap_breach = observable
        .iter()
        .any(|w| w.used_percent.unwrap_or(0.0) >= knobs.cap_guard);
    // Reset-relaxation: cap_guard trips AND time-to-reset is small
    // relative to the window's cycle.
    //
    // Finding #7: the original implementation only looked at the
    // BINDING window, so a fresh 5h at 90% with reset in 5 minutes
    // returned PreferSub immediately, ignoring that the weekly
    // window on the same account was also at/above the guard with a
    // reset still 5 days away. The relax-only-when-every-at-guard-
    // window-is-itself-imminently-resetting rule below closes that
    // hole: we relax only when every observable window currently at
    // or above `cap_guard` has its own `time_to_reset / cycle_secs`
    // within `reset_imminence_threshold`. A window without a reset
    // signal can't be confirmed-imminent, so it counts as
    // NOT-relaxable — better to fall back than to overshoot a cap
    // we can't actually see rolling over.
    let is_window_imminently_resetting = |w: &WindowView| -> bool {
        let cycle_secs = (w.hours as f64) * 3600.0;
        if cycle_secs <= 0.0 {
            return false;
        }
        match w.reset_at {
            None => false,
            Some(r) => {
                // A reset_at strictly in the past can't be CONFIRMED
                // imminent: the vendor hasn't yet surfaced the
                // rollover (clock skew, stale header, briefly-delayed
                // timer), so we have no signal for how soon this
                // window will actually reset. Treating "already
                // past reset_at" as "imminent" would silently flip
                // the cap_guard gate off for any heavily-used
                // account whose last sighting was a minute old —
                // a stale snapshot would let the user overshoot the
                // cap by the time the real signal arrives. Fold the
                // past-reset case into the same NOT-relaxable bucket
                // as `None`: when the rollover isn't reliably
                // imminent, prefer the conservative path and fall
                // back rather than risk letting a paid turn through
                // a guard we can no longer see.
                let raw = (r - now).num_seconds();
                if raw < 0 {
                    return false;
                }
                let time_to_reset = raw as f64;
                (time_to_reset / cycle_secs) <= knobs.reset_imminence_threshold
            }
        }
    };
    let relax = hard_cap_breach
        && observable
            .iter()
            .filter(|w| w.used_percent.unwrap_or(0.0) >= knobs.cap_guard)
            .all(|w| is_window_imminently_resetting(w));
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
    let decision = if hard_cap_breach || forecast_breach {
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
///
/// **Concurrency** (Finding #9): the store owns a sidecar lockfile
/// (`<path>.lock`) opened by [`UtilizationStore::open`], and holds an
/// exclusive `flock(2)` on it for the entire read-modify-write window.
/// `save()` releases the lock by dropping the store (or by writing
/// atomically: a `fsync`d temp file in the same directory, then
/// `rename(2)`). Two processes racing on the same file therefore
/// serialize cleanly: A's `open` blocks until B's `save` drops the lock,
/// A reads B's updated JSON, A applies its own delta, A saves. The
/// flock is automatically released if the process dies.
/// On-disk schema version for [`UtilizationStore`]. Bumped when a
/// persisted field's interpretation changes in a way that requires
/// migration of existing files. See [`default_schema_version`] and the
/// per-version migration code in `UtilizationStore::open`.
pub const CURRENT_SCHEMA_VERSION: u32 = 2;

/// Default schema version for files written before the field existed.
/// Files without an explicit `schema_version` are treated as `1`
/// (legacy fraction storage) and migrated on load.
pub const fn default_schema_version() -> u32 {
    1
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UtilizationStore {
    #[serde(default)]
    pub records: BTreeMap<String, UtilizationRecord>,
    /// Counter-fed windows (KNEMON Layer 3B). Keyed by
    /// `(provider, account_id, plan, window_name)` — the `window_name`
    /// segment disambiguates the `monthly` / `5h` / `weekly` windows the
    /// catalog declares for the same `(provider, account, plan)`.
    #[serde(default)]
    pub counters: BTreeMap<String, CounterWindow>,
    /// On-disk schema version. `1` = legacy fraction storage (used_percent
    /// in `[0, 1]`), `2` = the current percentage storage
    /// (`used_percent` in `[0, 100]`). Missing fields deserialize as
    /// `Default::default()` (= `1`) so files written before this field
    /// existed migrate cleanly on first load. After load the store
    /// re-emits `schema_version = CURRENT_SCHEMA_VERSION` on save.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Sidecar lockfile. Held for the lifetime of the store; Drop
    /// closes the FD, which releases the `flock(2)`. `#[serde(skip)]`
    /// because the lock is a process-local handle, not part of the
    /// persisted JSON.
    #[serde(skip)]
    lock: Option<fs::File>,
}

/// Manual `Clone` is unavailable because the lockfile FD is non-cloneable.
/// The store's read-modify-write window is short by design (one
/// request → one capture → one save), and the cross-process
/// serialization guarantee depends on this single-owner property.
impl Clone for UtilizationStore {
    fn clone(&self) -> Self {
        // Cannot clone an exclusive flock (it is process-unique), so
        // cloning a `UtilizationStore` would defeat the cross-process
        // serialization guarantee. Fail fast at compile time? No — we
        // can't prevent `clone()` at runtime, so the call must panic
        // with a clear message rather than silently race.
        panic!(
            "UtilizationStore cannot be cloned: the sidecar flock is process-unique; \
             share the store by reference instead"
        )
    }
}

/// Manual `Default` because we can't derive it once `Clone` is hand-
/// rolled. The default-constructed store has no path and no lock — it
/// is in-memory only, the same shape the pre-fix code had.
impl Default for UtilizationStore {
    fn default() -> Self {
        Self {
            records: BTreeMap::new(),
            counters: BTreeMap::new(),
            schema_version: CURRENT_SCHEMA_VERSION,
            path: None,
            lock: None,
        }
    }
}

fn key(provider: Provider, account_id: &str, plan: &str) -> String {
    format!("{provider:?}::{account_id}::{plan}")
}

fn counter_key(provider: Provider, account_id: &str, plan: &str, window_name: &str) -> String {
    format!("{provider:?}::{account_id}::{plan}::{window_name}")
}

/// Compute the calendar-period identity for `now`, suitable for
/// persisting on a [`CounterWindow`] and comparing on subsequent
/// `record_counter` calls (Finding #4). Returns `None` for callers
/// that don't want calendar-boundary handling; the corresponding
/// `CounterWindow` rows have `period_id = None` and never reset.
pub fn period_id_for(now: DateTime<Utc>) -> Option<String> {
    Some(now.format("%Y-%m").to_string())
}

fn current_period_for_stored_id(now: DateTime<Utc>, stored: &str) -> String {
    if stored.len() == 10 {
        now.format("%Y-%m-%d").to_string()
    } else {
        now.format("%Y-%m").to_string()
    }
}

/// Recompute `used_percent` from `used_tokens` and `cap` in the unified
/// 0..=100 scale (Finding #3). The previous implementation returned the
/// raw `used_tokens / cap` ratio (e.g. 0.85 for 85%), which the router
/// then compared against `cap_guard = 85` — silently 85× too lenient.
/// Callers inside this module use this helper rather than open-coding
/// the multiply so the rounding / cap-zero rule lives in one place.
fn compute_used_percent(used_tokens: f64, cap: Option<f64>) -> Option<f64> {
    match cap {
        Some(c) if used_tokens.is_finite() && used_tokens >= 0.0 && c.is_finite() && c > 0.0 => {
            let percent = (used_tokens / c) * 100.0;
            percent.is_finite().then_some(percent)
        }
        _ => None,
    }
}

fn effective_counter_percent(counter: &CounterWindow, now: DateTime<Utc>) -> Option<f64> {
    let used_tokens = match counter.rolling_hours {
        Some(hours) => {
            let cutoff = now - chrono::Duration::hours(i64::from(hours));
            counter
                .increments
                .iter()
                .filter(|increment| increment.observed_at >= cutoff)
                .map(|increment| increment.tokens)
                .sum()
        }
        None => counter.used_tokens,
    };
    compute_used_percent(used_tokens, counter.cap)
}

/// Detect-and-migrate legacy fractional `CounterWindow.used_percent`
/// rows written by an older binary (Finding #3). The pre-fix code
/// stored `used_tokens / cap` (a fraction, commonly in [0, 1] but greater
/// than 1 once a cap was exceeded); the new code
/// stores the same value times 100 (a percentage in [0, 100]).
///
/// The migration has two branches (Z-12):
///
/// 1. `cap` is `Some` — recompute from `used_tokens / cap * 100`. The
///    pre-fix v1 code may have stored a stale or fractional
///    `used_percent`; the recomputation is the source of truth and is
///    what the existing
///    `legacy_counter_percent_migration_recomputes_from_tokens_and_cap`
///    test pins.
///
/// 2. `cap` is `None` (PercentOnly window or "cap not yet recorded") —
///    we cannot recompute. Preserve the stored `used_percent`,
///    converting the legacy fractional range `0..=2` to the percentage
///    range `0..=200` by multiplying by 100. Dropping the stored value
///    here would make `decide_account`'s `used_percent.is_some()` filter
///    exclude the window entirely, restoring full headroom on a
///    near-exhausted cap-less budget. A `None` stored value with
///    `cap = None` stays `None` — we have no signal and must not
///    fabricate one.
///
/// The caller additionally gates this migration on the persisted
/// schema version, preventing legitimate sub-2% values in v2 from being
/// corrupted.
fn migrate_fractional_counter_percent(cw: &mut CounterWindow) {
    if cw.cap.is_some() {
        // cap=Some: source-of-truth recompute. The v1 store may have
        // stored a stale or fractional value; used_tokens/cap*100 wins.
        cw.used_percent = compute_used_percent(cw.used_tokens, cw.cap);
        return;
    }
    // cap=None: preserve the stored reading (with legacy-fractional
    // -> percentage conversion). compute_used_percent returns None
    // here unconditionally, so we cannot delegate. Mirroring the
    // cap=Some branch's 0..=2 upper bound covers exhausted legacy
    // counters (e.g. 1.2 -> 120.0) without touching legitimate
    // percentage-scale values (e.g. 85.0 -> 85.0). A None stored value
    // stays None (no signal, no fabrication).
    if let Some(p) = cw.used_percent {
        if (0.0..=2.0).contains(&p) {
            cw.used_percent = Some(p * 100.0);
        }
    }
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
    /// Path of the sidecar lockfile for `data_path`. The lockfile lives
    /// next to the data file so it shares the same directory's atomic
    /// rename semantics; the extension `.lock` keeps it visibly distinct
    /// from the JSON data file.
    fn lockfile_path(data_path: &Path) -> PathBuf {
        let mut s = data_path.as_os_str().to_owned();
        s.push(".lock");
        PathBuf::from(s)
    }

    /// Open a store at `path`. Creates an empty one if the file doesn't
    /// exist yet. Returns an error only on real I/O / parse failures —
    /// a missing file is fine.
    ///
    /// Acquires an exclusive `flock(2)` on a sidecar `<path>.lock`
    /// file for the lifetime of the returned store. This serializes
    /// read-modify-write across processes so two concurrent
    /// `record_counter` callers cannot lose an increment (Finding #9).
    /// The lock is released when the store is dropped.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, UtilizationError> {
        let path = path.as_ref();
        // Acquire the cross-process lock FIRST, before reading the data
        // file. Otherwise another process could write between our read
        // and our write and we'd lose its update. The lock is held until
        // the returned store is dropped (or until `save` runs, after
        // which we keep the lock so a follow-up read-modify-write
        // remains consistent — the typical wire-up is "open + record +
        // save" inside one short-lived scope).
        let lock = Self::acquire_lock(path)?;
        match fs::read(path) {
            Ok(bytes) => {
                if bytes.is_empty() {
                    Ok(Self {
                        records: BTreeMap::new(),
                        counters: BTreeMap::new(),
                        schema_version: CURRENT_SCHEMA_VERSION,
                        path: Some(path.to_path_buf()),
                        lock: Some(lock),
                    })
                } else {
                    let mut store: Self =
                        serde_json::from_slice(&bytes).map_err(UtilizationError::Parse)?;
                    store.path = Some(path.to_path_buf());
                    store.lock = Some(lock);
                    // Migrate legacy fractional `CounterWindow.used_percent`
                    // rows in place (Finding #3) — but ONLY for files that
                    // predate the percentage fix. The fix is gated on the
                    // persisted `schema_version` field so a brand-new row
                    // whose `used_percent` happens to land in (0, 1)
                    // (perfectly legitimate when the cap is enormous, e.g.
                    // MiniMax's 5.1B monthly cap with low real usage)
                    // doesn't get falsely re-multiplied by 100 and rendered
                    // as e.g. "4.9%" instead of "0.049%".
                    if store.schema_version < 2 {
                        for cw in store.counters.values_mut() {
                            migrate_fractional_counter_percent(cw);
                        }
                        store.schema_version = CURRENT_SCHEMA_VERSION;
                    }
                    Ok(store)
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self {
                records: BTreeMap::new(),
                counters: BTreeMap::new(),
                schema_version: CURRENT_SCHEMA_VERSION,
                path: Some(path.to_path_buf()),
                lock: Some(lock),
            }),
            Err(e) => Err(UtilizationError::Io(e)),
        }
    }

    /// Open a store at `path` WITHOUT acquiring a cross-process lock.
    /// Used for read-only inspection (the CLI's `report` subcommand)
    /// where cross-process writes are tolerated and we don't want to
    /// block on another process's active capture. The returned store
    /// cannot be safely `save()`-d concurrently with another writer —
    /// use [`UtilizationStore::open`] (the locked variant) for that.
    pub fn open_unlocked(path: impl AsRef<Path>) -> Result<Self, UtilizationError> {
        let path = path.as_ref();
        match fs::read(path) {
            Ok(bytes) => {
                if bytes.is_empty() {
                    Ok(Self {
                        records: BTreeMap::new(),
                        counters: BTreeMap::new(),
                        schema_version: CURRENT_SCHEMA_VERSION,
                        path: Some(path.to_path_buf()),
                        lock: None,
                    })
                } else {
                    let mut store: Self =
                        serde_json::from_slice(&bytes).map_err(UtilizationError::Parse)?;
                    store.path = Some(path.to_path_buf());
                    store.lock = None;
                    // Same legacy-fraction migration as `open()` —
                    // read-only paths still want consistent 0..=100
                    // percents so reports / forecasts never render a
                    // fractional row as "0.9%" again. Gated on
                    // `schema_version < 2` for the same reason: a real
                    // 0.049% reading must not be silently re-multiplied.
                    if store.schema_version < 2 {
                        for cw in store.counters.values_mut() {
                            migrate_fractional_counter_percent(cw);
                        }
                        store.schema_version = CURRENT_SCHEMA_VERSION;
                    }
                    Ok(store)
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self {
                records: BTreeMap::new(),
                counters: BTreeMap::new(),
                schema_version: CURRENT_SCHEMA_VERSION,
                path: Some(path.to_path_buf()),
                lock: None,
            }),
            Err(e) => Err(UtilizationError::Io(e)),
        }
    }

    /// Open (creating if needed) the sidecar lockfile at
    /// `<data_path>.lock` and acquire an exclusive `flock(2)` on it.
    /// Blocks until the lock is acquired (kernel-level wait — short in
    /// practice because the typical critical section is one capture +
    /// one save).
    fn acquire_lock(data_path: &Path) -> Result<fs::File, UtilizationError> {
        let lock_path = Self::lockfile_path(data_path);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(UtilizationError::Io)?;
        }
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(UtilizationError::Io)?;
        // `lock_exclusive` blocks until the kernel grants the lock; on
        // process death the FD is closed by the kernel and the lock is
        // released, so a crashed process can't deadlock the next caller.
        f.lock_exclusive().map_err(UtilizationError::Io)?;
        Ok(f)
    }

    /// Open a store at the default location. Returns `None` when neither
    /// `$ZODER_HOME` nor `~/.zoder` resolves.
    pub fn open_default() -> Result<Option<Self>, UtilizationError> {
        match default_store_path() {
            Some(p) => Ok(Some(Self::open(p)?)),
            None => Ok(None),
        }
    }

    /// Upsert a snapshot. No-op only if it has neither windows nor a credits
    /// signal. A credits-only `has_credits=false` sighting must survive so L4
    /// can make the same hard-fallback decision as the legacy path.
    pub fn upsert(&mut self, snap: &RateLimitSnapshot, now: DateTime<Utc>) {
        if snap.primary.is_none() && snap.secondary.is_none() && snap.has_credits.is_none() {
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
        if snap.primary.is_none() && snap.secondary.is_none() && snap.has_credits.is_none() {
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
    /// `(used_tokens / cap) * 100.0` whenever the cap is known (the
    /// 0..=100 scale that `cap_guard` and the renderer both use —
    /// Finding #3). When the cap is `None` (a `PercentOnly` window,
    /// or any window whose cap has not yet been recorded via
    /// [`set_counter_cap`]), `used_percent` stays `None` — we never
    /// invent a percent.
    ///
    /// Calendar reset (Finding #4): when this row was created with
    /// `period_id = Some(...)` (by the catalog-aware wire-up in
    /// [`crate::provider::capture_counter_usage`]) and the period
    /// computed from `now` differs, we atomically zero `used_tokens`
    /// (and `used_percent`) BEFORE applying the new increment — so
    /// June usage cannot survive into July, and the percentage unit
    /// bug never compounds across the boundary. Rows without a
    /// `period_id` (legacy / non-calendar windows) never reset here;
    /// the caller is responsible for any rolling-window semantics.
    ///
    /// Window `reset_at` is preserved across increments when the
    /// caller has already set it; otherwise it's left at `None`.
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
                // Calendar windows get a `period_id` seeded lazily on the
                // first increment so subsequent increments can detect a
                // boundary cross; non-calendar windows stay None.
                period_id: None,
                rolling_hours: None,
                increments: Vec::new(),
                last_updated: now,
            });
        // Calendar-boundary reset (Finding #4): when the caller has
        // bound the window to a period_id (set via the catalog-aware
        // wire-up in `provider::capture_counter_usage` — see
        // `set_counter_period_id`) and the period derived from `now`
        // differs from the persisted one, zero out the window BEFORE
        // applying the new increment. This is the atomic reset the
        // store previously lacked: June usage cannot bleed into
        // July, even though `record_counter` is the only mutator.
        if let Some(stored_period) = entry.period_id.clone() {
            let current_period = current_period_for_stored_id(now, &stored_period);
            if stored_period != current_period {
                entry.used_tokens = 0.0;
                entry.used_percent = None;
                entry.period_id = Some(current_period);
                entry.increments.clear();
            }
        }
        // Defensive: a malformed / negative increment is a no-op, not a
        // subtraction. A provider that occasionally reports 0 usage
        // (streaming-usage off, usage field absent) still wants a row
        // touch but never a negative balance.
        //
        // Z-11 (rolling-window monotonicity vs. stale-clock / OOO
        // capture): the rolling-window cutoff is computed against the
        // capture's `now`, and `last_updated` is unconditionally
        // rewritten to `now` at the end of this method. A stale-clock
        // capture (now < last_updated) would therefore (a) recompute
        // the cutoff against the stale `now` and drop just-recorded
        // in-window increments on the next sum, AND (b) roll
        // `last_updated` backwards, marking the window
        // `TelemetryHealth::Degraded` on the next routing decision and
        // excluding it from `decide_account`'s `observable` set. With
        // `tokens_used > 0` the failure mode widens: the stale capture
        // also pushes a phantom `CounterIncrement` at the stale
        // timestamp, attributing usage to a time the provider never
        // reported. Either failure mode restores full headroom on a
        // near-exhausted window — the exact FALSE-headroom symptom the
        // 2026-07-08 review flagged.
        //
        // Reject the capture as a no-op when `now < last_updated` on a
        // rolling window. The calendar-reset branch above has already
        // run, so a genuinely new period has already been reset (the
        // new period's `last_updated` is set at the end of this
        // method). A genuine reset for a non-calendar rolling window
        // cannot happen via clock motion alone — only via the operator
        // explicitly setting `rolling_hours` or clearing `increments`.
        //
        // The guard is scoped to `rolling_hours.is_some()` so calendar
        // windows and the non-rolling accumulator are unaffected
        // (pinned by `non_rolling_clock_rollback_capture_preserves_used_tokens`).
        if entry.rolling_hours.is_some() && now < entry.last_updated {
            return entry.used_tokens;
        }
        if let Some(hours) = entry.rolling_hours {
            let cutoff = now - chrono::Duration::hours(i64::from(hours));
            entry
                .increments
                .retain(|increment| increment.observed_at >= cutoff);
            if tokens_used.is_finite() && tokens_used > 0.0 {
                entry.increments.push(CounterIncrement {
                    observed_at: now,
                    tokens: tokens_used,
                });
            }
            entry.used_tokens = entry
                .increments
                .iter()
                .map(|increment| increment.tokens)
                .sum::<f64>()
                .min(f64::MAX);
        } else if tokens_used.is_finite() && tokens_used > 0.0 {
            entry.used_tokens = (entry.used_tokens + tokens_used).min(f64::MAX);
        }
        // Recompute percent from the cap, if any. `cap = Some(0.0)` is
        // treated as "no headroom" (0%, not NaN/inf) so a bad
        // configuration never produces an exploded percent. Finding
        // #3: percent is in the 0..=100 scale, not the bare ratio.
        entry.used_percent = compute_used_percent(entry.used_tokens, entry.cap);
        entry.last_updated = now;
        entry.used_tokens
    }

    /// Bind a `CounterWindow` row to a calendar-period identity
    /// (Finding #4). Called by the catalog-aware wire-up after the
    /// store has been opened and the window's `ResetKind` has been
    /// resolved. A row without a `period_id` (legacy / non-calendar)
    /// never triggers a reset on `record_counter`. Setting
    /// `period_id = None` is the way to opt a previously-periodic
    /// window back out of automatic reset (e.g. operator changed the
    /// catalog from `CalendarMonthly` to `Rolling`).
    pub fn set_counter_period_id(
        &mut self,
        provider: Provider,
        account_id: &str,
        plan: &str,
        window_name: &str,
        period_id: Option<String>,
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
            period_id: period_id.clone(),
            rolling_hours: None,
            increments: Vec::new(),
            last_updated: now,
        });
        match period_id {
            Some(period_id) => {
                // Preserve an existing period until `record_counter` sees it.
                // Overwriting June with July here would erase the only signal
                // that the following increment must reset the counter first.
                if entry.period_id.is_none() {
                    entry.period_id = Some(period_id);
                }
            }
            None => entry.period_id = None,
        }
        entry.last_updated = now;
    }

    /// Configure trailing-window accounting for a counter. Existing rows from
    /// schema versions that predate timestamped increments are conservatively
    /// migrated as one increment at their last observation time.
    pub fn set_counter_rolling_hours(
        &mut self,
        provider: Provider,
        account_id: &str,
        plan: &str,
        window_name: &str,
        rolling_hours: Option<u32>,
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
            period_id: None,
            rolling_hours,
            increments: Vec::new(),
            last_updated: now,
        });
        if rolling_hours.is_some() && entry.increments.is_empty() && entry.used_tokens > 0.0 {
            entry.increments.push(CounterIncrement {
                observed_at: entry.last_updated,
                tokens: entry.used_tokens,
            });
        }
        entry.rolling_hours = rolling_hours;
        if rolling_hours.is_none() {
            entry.increments.clear();
        }
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
            period_id: None,
            rolling_hours: None,
            increments: Vec::new(),
            last_updated: now,
        });
        entry.cap = cap;
        // Finding #3: percent is in the 0..=100 scale.
        entry.used_percent = compute_used_percent(entry.used_tokens, entry.cap);
        entry.last_updated = now;
    }

    /// Recompute `used_percent` for one counter-fed window from its
    /// currently-stored `used_tokens` and `cap`. Use after
    /// [`set_counter_cap`] to surface the percent before the next
    /// `record_counter` lands. Returns the new percent (in the
    /// 0..=100 scale — Finding #3), or `None` when the cap is
    /// unknown / non-positive.
    pub fn recompute_counter_percent(
        &mut self,
        provider: Provider,
        account_id: &str,
        plan: &str,
        window_name: &str,
    ) -> Option<f64> {
        let k = counter_key(provider, account_id, plan, window_name);
        let entry = self.counters.get_mut(&k)?;
        let pct = compute_used_percent(entry.used_tokens, entry.cap);
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
    ///
    /// Atomic write: serializes to `<path>.tmp.<pid>.<nanos>`, `fsync`s
    /// it, then `rename(2)`s it onto `<path>`. The rename is atomic on
    /// POSIX (same filesystem), so a crash mid-write leaves either the
    /// old file or the new file — never a half-written, unparseable
    /// blob (Finding #9).
    ///
    /// The cross-process `flock(2)` acquired in `open()` is still held
    /// during the write; callers that need to release it sooner should
    /// drop the store.
    pub fn save(&self) -> Result<(), UtilizationError> {
        let path = self.path.as_ref().ok_or(UtilizationError::NoPath)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(UtilizationError::Io)?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(UtilizationError::Parse)?;

        // Same-directory temp file so `rename(2)` is atomic (cross-
        // directory rename can fall back to copy+delete on some
        // platforms, defeating the atomicity). The `<pid>.<nanos>`
        // suffix lets concurrent processes (or the same process on
        // retry) pick distinct names so two writers can't collide.
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = match path.file_name() {
            Some(_) => {
                let mut s = path.as_os_str().to_owned();
                s.push(format!(".tmp.{pid}.{nanos}"));
                PathBuf::from(s)
            }
            None => {
                return Err(UtilizationError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "utilization store path has no file_name component",
                )))
            }
        };

        // Write the temp file.
        {
            let mut f = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)
                .map_err(UtilizationError::Io)?;
            use std::io::Write;
            f.write_all(&bytes).map_err(UtilizationError::Io)?;
            // `sync_all` flushes the file's data AND metadata to disk
            // before the rename. Without it, a power loss between the
            // rename and the kernel's flush can leave the file empty
            // or partial.
            f.sync_all().map_err(UtilizationError::Io)?;
        }

        // Atomic rename. On Unix this is guaranteed atomic for same-
        // filesystem renames; the temp file lives next to the data
        // file so this is always satisfied.
        fs::rename(&tmp, path).map_err(UtilizationError::Io)?;
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
        assert_eq!(p.used_percent, Some(42.0));
        assert_eq!(p.window_minutes, Some(300));
        // `reset-at` wins over the synthesized `reset-after-seconds`.
        assert_eq!(p.reset_at_epoch, Some(1783159200));
        let s = snap.secondary.unwrap();
        assert_eq!(s.used_percent, Some(12.0));
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
        assert_eq!(p.used_percent, Some(5.0));
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
        assert_eq!(p.used_percent, Some(65.5));
        assert_eq!(p.window_minutes, Some(300));
        assert_eq!(p.label.as_deref(), Some("5h"));
        let s = snap.secondary.unwrap();
        assert_eq!(s.used_percent, Some(20.0));
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
        assert!((p.used_percent.unwrap() - 80.0).abs() < 1e-9);
        assert_eq!(p.label.as_deref(), Some("requests"));
        // Secondary from `tokens` pair: 50% used.
        let s = snap.secondary.unwrap();
        assert!((s.used_percent.unwrap() - 50.0).abs() < 1e-9);
    }

    #[test]
    fn parse_anthropic_suffix_minutes() {
        assert_eq!(anthropic_suffix_minutes("5h"), Some(300));
        assert_eq!(anthropic_suffix_minutes("7d"), Some(10080));
        assert_eq!(anthropic_suffix_minutes("1m"), Some(1));
        assert_eq!(anthropic_suffix_minutes("garbage"), None);
    }

    #[test]
    fn parsers_reject_non_finite_negative_and_inconsistent_utilization() {
        for raw in ["NaN", "inf", "-1"] {
            let h = hm([("x-codex-primary-used-percent", raw)]);
            assert!(parse_codex(&h, "acct", "pro").primary.is_none());
        }
        let h = hm([
            ("anthropic-ratelimit-requests-limit", "100"),
            ("anthropic-ratelimit-requests-remaining", "101"),
        ]);
        assert!(parse_anthropic(&h, "acct", "max").primary.is_none());
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

    // Regression: `Provider::detect` and `parse_codex` were drifting — the
    // detector only checked three of the ten `x-codex-*` headers the parser
    // actually consults. A Codex response carrying ONLY a header outside
    // that set (e.g. a proxy that strips `x-codex-primary-used-percent` but
    // forwards `x-codex-secondary-used-percent`, or a stripped-shape
    // response carrying only `x-codex-credits-has-credits` /
    // `x-codex-primary-reset-at` / `x-codex-primary-window-minutes`) made
    // `detect` return `None`, so `parse_headers` returned `None` and the
    // entire header set was silently dropped — including a real
    // secondary-window reading. The hard invariant ("absent is unknown,
    // never 0", and its corollary "a recognizable Codex header set must
    // route to the Codex parser") requires the detector to match every
    // header the parser consumes. This test pins that contract for the
    // secondary-only case the previous detector missed.
    #[test]
    fn detect_routes_secondary_only_codex_headers_to_codex_parser() {
        // Detect must tag the header set as Codex.
        let h = hm([("x-codex-secondary-used-percent", "50")]);
        assert_eq!(
            Provider::detect(&h),
            Some(Provider::OpenaiCodex),
            "a Codex-shaped secondary-only header set must be detected as Codex"
        );
        // parse_headers must surface the secondary reading (the parser
        // builds a real `secondary` window with `used_percent = 50`; the
        // bug previously returned `None` here).
        let mut s = parse_headers(&h, "acct", "pro").expect(
            "a Codex-shaped secondary-only header set must not be dropped \
             by parse_headers",
        );
        assert_eq!(s.provider, Provider::OpenaiCodex);
        let secondary = s
            .secondary
            .as_ref()
            .expect("secondary window must be populated from the secondary header");
        assert_eq!(secondary.used_percent, Some(50.0));
        // And the routing decision must use it — a 50% secondary reading
        // alone must not be discarded by the parser (the regression
        // this test guards). Pin the snapshot's `observed_at` to the
        // test's `now` so the freshness gate is Fresh (the gate is
        // keyed off the age relative to `now`; a parser-produced
        // `observed_at = Utc::now()` would otherwise be far in the
        // future relative to the test's hard-coded 2026-07-04 `now`
        // and Degraded, which would now fail closed — that's the
        // Z-18 fix and is correct, but unrelated to the
        // secondary-window detection contract this test pins).
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        s.observed_at = Some(now);
        assert_eq!(
            decide(&s, &RouteKnobs::default(), now, None),
            RouteDecision::PreferSub
        );
        // And the sibling-tolerance contract: a malformed value on the
        // primary header must not also discard the secondary reading.
        // Before the fix this combination dropped the entire header set.
        let h2 = hm([
            ("x-codex-primary-used-percent", "NaN"),
            ("x-codex-secondary-used-percent", "37"),
        ]);
        let mut s2 = parse_headers(&h2, "acct", "pro").expect(
            "a malformed primary header must not cause a valid secondary \
             reading to be dropped",
        );
        s2.observed_at = Some(now);
        assert_eq!(s2.provider, Provider::OpenaiCodex);
        assert!(
            s2.primary.is_none(),
            "malformed primary percent must surface as None (no signal), \
             not as Some(_) — Finding #5"
        );
        assert_eq!(
            s2.secondary.as_ref().and_then(|w| w.used_percent),
            Some(37.0),
            "valid sibling-window reading must survive a malformed value \
             on the other window (sibling tolerance)"
        );
    }

    // -------- effective_used ---------------------------------------

    #[test]
    fn effective_used_takes_max_of_windows() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let snap = RateLimitSnapshot {
            primary: Some(WindowSnapshot {
                used_percent: Some(30.0),
                window_minutes: Some(300),
                reset_at_epoch: None,
                label: Some("primary".into()),
            }),
            secondary: Some(WindowSnapshot {
                used_percent: Some(90.0),
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
                used_percent: Some(99.0),
                window_minutes: Some(300),
                reset_at_epoch: Some(now.timestamp() - 10), // 10s ago
                label: Some("primary".into()),
            }),
            secondary: Some(WindowSnapshot {
                used_percent: Some(10.0),
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
        RateLimitSnapshot {
            provider: Provider::OpenaiCodex,
            account_id: "acct".into(),
            plan: "pro".into(),
            primary: Some(WindowSnapshot {
                used_percent: Some(pct),
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
    fn degraded_no_credits_signal_does_not_gate() {
        // Adversarial review Z-18 (fail-closed: degraded snapshot must
        // not keep a possibly-exhausted sub eligible). A Degraded
        // snapshot is untrustworthy either direction; the routing
        // decision must fail CLOSED (FallBackToFree), NOT PreferSub.
        // This previously asserted PreferSub (the fail-OPEN behavior
        // the comment below originally justified); the test was
        // updated as part of the Z-18 fix to pin the corrected
        // fail-closed contract.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let mut snap = snap_with_primary(99.0, None);
        snap.has_credits = Some(false);
        snap.observed_at = Some(now - chrono::Duration::hours(2));
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::FallBackToFree,
            "a Degraded snapshot must fail CLOSED regardless of has_credits; \
             PreferSub here would let a possibly-exhausted sub through on stale data",
        );
    }

    #[test]
    fn decide_degraded_snapshot_high_usage_falls_back_to_free() {
        // Z-18 (fail-closed: stale telemetry must not keep a
        // possibly-exhausted sub eligible). A snapshot whose
        // `observed_at` is more than 60 minutes old is Degraded; a
        // 99% reading on it cannot be trusted — the window may have
        // refilled, but it also may not have, and routing through a
        // (possibly) 99%-used subscription is the fail-OPEN bug. The
        // decision must be `FallBackToFree`, NOT `PreferSub`. Must
        // FAIL on the pre-fix code (which returns `PreferSub` for
        // any Degraded snapshot).
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let mut snap = snap_with_primary(99.0, None);
        snap.observed_at = Some(now - chrono::Duration::hours(2));
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::FallBackToFree,
            "a Degraded snapshot at 99% used must fail CLOSED, NOT PreferSub",
        );
    }

    #[test]
    fn decide_fresh_snapshot_low_usage_still_prefers_sub() {
        // Non-regression for the Z-18 fix: a FRESH snapshot at low
        // usage must STILL PreferSub — only stale/absent telemetry
        // changes to the conservative path. (Sanity check that the
        // fail-closed fix didn't over-correct.)
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let mut snap = snap_with_primary(30.0, None);
        snap.observed_at = Some(now - chrono::Duration::seconds(30));
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::PreferSub,
            "a Fresh snapshot at 30% used must still PreferSub",
        );
    }

    #[test]
    fn decide_stale_but_not_degraded_snapshot_still_uses_bands() {
        // Non-regression for the Z-18 fix: a Stale (5..=60 min)
        // snapshot must STILL flow through the normal use_target /
        // cap_guard bands — the freshness gate only fires on
        // Degraded. A 30% stale reading is still under use_target
        // -> PreferSub.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let mut snap = snap_with_primary(30.0, None);
        snap.observed_at = Some(now - chrono::Duration::minutes(30));
        assert_eq!(
            decide(
                &snap,
                &knobs_with(80.0, 85.0, BudgetMode::Block, None),
                now,
                None
            ),
            RouteDecision::PreferSub,
            "a Stale-but-not-Degraded snapshot at 30% used must still PreferSub via the normal bands",
        );
    }

    #[test]
    fn decide_uses_max_of_primary_and_secondary() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let snap = RateLimitSnapshot {
            primary: Some(WindowSnapshot {
                used_percent: Some(20.0),
                window_minutes: Some(300),
                reset_at_epoch: None,
                label: Some("primary".into()),
            }),
            secondary: Some(WindowSnapshot {
                used_percent: Some(90.0),
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
        {
            let mut s = UtilizationStore::open(&path).unwrap();
            assert!(s.records.is_empty());

            let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
            let snap = snap_with_primary(42.0, None);
            s.upsert(&snap, now);
            s.save().unwrap();
        } // drop the first store so the lockfile is released for the read-back.

        let s2 = UtilizationStore::open(&path).unwrap();
        let rec = s2
            .get(Provider::OpenaiCodex, "acct", "pro")
            .expect("record persisted");
        assert_eq!(rec.primary.as_ref().unwrap().used_percent, Some(42.0));
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
        assert_eq!(p.used_percent, Some(92.0));
        // `reset-after-seconds` synthesizes an epoch ≈ now + 600.
        let now = Utc::now().timestamp();
        let reset = p.reset_at_epoch.expect("reset must be synthesized");
        assert!(
            (reset - (now + 600)).abs() <= 5,
            "reset {reset} should be ~now+600 (now={now})",
        );
        let s = snap.secondary.expect("secondary window must be present");
        assert_eq!(s.used_percent, Some(37.0));
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
        // surfaces as None (status-only — known window, no number).
        // Finding #5: we no longer fabricate a 0% reading when the
        // headers don't carry one. The legacy-style `reset` header is
        // still parsed as an RFC3339 epoch.
        let p = snap.primary.expect("primary window must be present");
        assert_eq!(p.used_percent, None);
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
        {
            let mut s = UtilizationStore::open(&path).unwrap();
            let snap = snap_with_primary(72.5, None);
            let recorded = s.record(&snap, Utc::now());
            assert!(recorded, "snapshot with windows must be persisted");
        } // drop the first store so the lockfile is released for the read-back.

        let s2 = UtilizationStore::open(&path).unwrap();
        let rec = s2
            .get(Provider::OpenaiCodex, "acct", "pro")
            .expect("record persisted");
        assert_eq!(rec.primary.as_ref().unwrap().used_percent, Some(72.5));
        // The read-back path the CLI uses:
        let loaded = s2
            .get(Provider::OpenaiCodex, "acct", "pro")
            .map(|r| r.as_snapshot());
        let loaded = loaded.expect("as_snapshot must yield a snapshot");
        assert_eq!(
            loaded.primary.as_ref().unwrap().used_percent,
            Some(72.5),
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
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let n = 1234.0_f64;
        let expected_pct;

        {
            let mut s = UtilizationStore::open(&path).unwrap();

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
            // Finding #3: used_percent is on the 0..=100 scale.
            expected_pct = (2.0 * n) / MM_MONTHLY_CAP * 100.0;
            let got_pct = w
                .used_percent
                .expect("used_percent must be Some when cap is known");
            assert!(
                (got_pct - expected_pct).abs() < 1e-9,
                "used_percent must equal (2N/cap)*100 = {expected_pct}, got {got_pct}",
            );
            // Flush to disk before the next open.
            s.save().unwrap();
        } // drop the first store so the lockfile is released for the read-back.

        // And the JSON on disk: round-trip.
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
        // Finding #3: percent is in 0..=100 — n=999, cap=10_000 -> 9.99%.
        assert!(
            (pct - (n / 10_000.0) * 100.0).abs() < 1e-12,
            "after set_counter_cap, percent must be (used/cap)*100: got {pct}",
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
    fn rolling_counter_discards_increments_outside_trailing_window() {
        let mut store = UtilizationStore::default();
        let start = Utc.with_ymd_and_hms(2026, 7, 4, 0, 0, 0).unwrap();
        store.set_counter_rolling_hours(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "5h",
            Some(5),
            start,
        );
        store.set_counter_cap(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "5h",
            Some(100.0),
            start,
        );
        assert_eq!(
            store.record_counter(Provider::MiniMax, "default", MM_PLAN, "5h", 90.0, start),
            90.0
        );

        let later = start + chrono::Duration::hours(6);
        assert_eq!(
            store.record_counter(Provider::MiniMax, "default", MM_PLAN, "5h", 10.0, later),
            10.0
        );
        let counter = store
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "5h")
            .unwrap();
        assert_eq!(counter.used_percent, Some(10.0));
        assert_eq!(counter.increments.len(), 1);
    }

    #[test]
    fn calendar_counter_binding_preserves_old_period_until_increment_resets() {
        let mut s = UtilizationStore::default();
        let june = Utc.with_ymd_and_hms(2026, 6, 30, 23, 59, 0).unwrap();
        let july = Utc.with_ymd_and_hms(2026, 7, 1, 0, 1, 0).unwrap();
        s.set_counter_cap(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            Some(1_000.0),
            june,
        );
        s.set_counter_period_id(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            period_id_for(june),
            june,
        );
        assert_eq!(
            s.record_counter(
                Provider::MiniMax,
                "default",
                MM_PLAN,
                "monthly",
                900.0,
                june
            ),
            900.0
        );

        // This mirrors the live capture order: bind current period first,
        // then record. The binding call must not erase the persisted June id.
        s.set_counter_period_id(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            period_id_for(july),
            july,
        );
        assert_eq!(
            s.record_counter(Provider::MiniMax, "default", MM_PLAN, "monthly", 10.0, july),
            10.0
        );
        let w = s
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "monthly")
            .unwrap();
        assert_eq!(w.period_id.as_deref(), Some("2026-07"));
        assert_eq!(w.used_percent, Some(1.0));
    }

    #[test]
    fn routing_sees_expired_monthly_counter_as_full_headroom_before_increment() {
        let mut store = UtilizationStore::default();
        let june = Utc.with_ymd_and_hms(2026, 6, 30, 23, 59, 0).unwrap();
        let july = Utc.with_ymd_and_hms(2026, 7, 1, 0, 1, 0).unwrap();
        store.set_counter_cap(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            Some(1_000.0),
            june,
        );
        store.set_counter_period_id(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            period_id_for(june),
            june,
        );
        store.record_counter(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            900.0,
            june,
        );
        let window = crate::config::QuotaWindow {
            name: "monthly".into(),
            hours: 720,
            unit: crate::config::QuotaUnit::Tokens,
            cap: Some(1_000.0),
            models: None,
            observability: crate::config::Observability::Counter,
            reset: crate::config::ResetKind::CalendarMonthly,
        };
        let view = build_account_view(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            &[window],
            &store,
            july,
        );
        assert_eq!(view.windows[0].used_percent, Some(0.0));
        assert_eq!(
            decide_account(&view, &RouteKnobs::default(), july, None).decision,
            RouteDecision::PreferSub
        );
    }

    #[test]
    fn daily_counter_resets_on_day_boundary_not_only_month_boundary() {
        let mut s = UtilizationStore::default();
        let day_one = Utc.with_ymd_and_hms(2026, 7, 4, 23, 59, 0).unwrap();
        let day_two = Utc.with_ymd_and_hms(2026, 7, 5, 0, 1, 0).unwrap();
        s.set_counter_period_id(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "daily",
            Some("2026-07-04".into()),
            day_one,
        );
        s.record_counter(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "daily",
            50.0,
            day_one,
        );
        assert_eq!(
            s.record_counter(Provider::MiniMax, "default", MM_PLAN, "daily", 2.0, day_two),
            2.0
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
        {
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
        } // drop the first store so the lockfile is released for the read-back.

        // Re-open from the same path. Both the header record and
        // the counter row survive.
        let s2 = UtilizationStore::open(&path).unwrap();
        let rec = s2
            .get(Provider::OpenaiCodex, "acct", "pro")
            .expect("header-fed record must round-trip");
        assert_eq!(rec.primary.as_ref().unwrap().used_percent, Some(72.5));
        let cw = s2
            .get_counter(Provider::MiniMax, "acct-mm", MM_PLAN, "monthly")
            .expect("counter row must round-trip");
        assert_eq!(cw.used_tokens, 2_500_000.0);
        assert_eq!(cw.cap, Some(MM_MONTHLY_CAP));
        // Finding #3: stored percent is in 0..=100, not the bare ratio.
        assert_eq!(
            cw.used_percent,
            Some((2_500_000.0 / MM_MONTHLY_CAP) * 100.0)
        );
        assert_eq!(cw.provider, Provider::MiniMax);
        assert_eq!(cw.plan, MM_PLAN);
        assert_eq!(cw.window_name, "monthly");
        // Drop s2 so the legacy-file re-open below isn't blocked by
        // the cross-process lock we still hold.
        drop(s2);

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

    #[test]
    fn legacy_counter_percent_migration_recomputes_from_tokens_and_cap() {
        let dir = tempfile::tempdir().unwrap();
        let now = "2026-07-04T12:00:00Z";
        let counter = |name: &str, used_percent: f64| {
            serde_json::json!({
                "provider": "mini_max",
                "account_id": "default",
                "plan": "max",
                "window_name": name,
                "used_tokens": used_percent * 1000.0,
                "cap": 1000.0,
                "used_percent": used_percent,
                "last_updated": now
            })
        };
        let legacy_path = dir.path().join("legacy-v1.json");
        std::fs::write(
            &legacy_path,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "records": {},
                "counters": {
                    "full": counter("full", 1.0),
                    "exhausted": counter("exhausted", 1.2),
                    "three_x_cap": counter("three_x_cap", 3.0)
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let migrated = UtilizationStore::open_unlocked(&legacy_path).unwrap();
        assert_eq!(migrated.counters["full"].used_percent, Some(100.0));
        assert_eq!(migrated.counters["exhausted"].used_percent, Some(120.0));
        assert_eq!(migrated.counters["three_x_cap"].used_percent, Some(300.0));

        let current_path = dir.path().join("current-v2.json");
        std::fs::write(
            &current_path,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 2,
                "records": {},
                "counters": {"small": counter("small", 1.2)}
            }))
            .unwrap(),
        )
        .unwrap();
        let current = UtilizationStore::open_unlocked(&current_path).unwrap();
        assert_eq!(current.counters["small"].used_percent, Some(1.2));
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
            has_credits: None,
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
            has_credits: None,
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
            has_credits: None,
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
    fn cap_breach_overrides_health_weight() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let stale_weekly = WindowView {
            name: "weekly".into(),
            used_percent: Some(100.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Stale,
            reset_at: None,
            hours: 168,
        };
        let fresh_5h = WindowView {
            name: "5h".into(),
            used_percent: Some(84.0),
            observability: crate::config::Observability::Header,
            health: TelemetryHealth::Fresh,
            reset_at: None,
            hours: 5,
        };
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            has_credits: None,
            windows: vec![stale_weekly, fresh_5h],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad = decide_account(&acct, &knobs, now, None);
        assert_eq!(ad.decision, RouteDecision::FallBackToFree);
        assert_eq!(
            ad.binding_window.as_deref(),
            Some("5h"),
            "health weighting remains diagnostic, but cannot mask the weekly cap breach"
        );
        assert!((ad.strength - 84.0).abs() < 1e-9);
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
            has_credits: None,
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
            has_credits: None,
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
            has_credits: None,
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
            has_credits: None,
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

    #[test]
    fn l4_band_drive_at_40_pct_prefers_sub() {
        // (e drive) 40% used, below use_target=80 -> PreferSub.
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            has_credits: None,
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
            has_credits: None,
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
            has_credits: None,
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
            has_credits: None,
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
            has_credits: None,
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
            has_credits: None,
            windows: vec![fresh_5h(40.0)],
        };
        let acct_busy = AccountView {
            provider: Provider::Anthropic,
            account_id: "busy".into(),
            plan: "max".into(),
            has_credits: None,
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
    fn route_knobs_reject_unknown_policy_fields() {
        let err = serde_json::from_value::<RouteKnobs>(serde_json::json!({
            "use_target": 70.0,
            "cap_guard": 85.0,
            "budget_mode": "block",
            "chargeback_budget_usd": null,
            "cap_gaurd": 90.0
        }))
        .unwrap_err();
        assert!(err.to_string().contains("cap_gaurd"), "{err}");
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
        // Finding #3: percent is in 0..=100 — 250_000 / 1_000_000 = 25%.
        assert!((w.used_percent.unwrap() - 25.0).abs() < 1e-9);
        assert_eq!(w.health, TelemetryHealth::Fresh);
    }

    #[test]
    fn codex_five_hour_header_binds_chatgpt_short_window() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let mut store = UtilizationStore::default();
        let snapshot = RateLimitSnapshot {
            provider: Provider::OpenaiCodex,
            account_id: "default".into(),
            plan: "chatgpt-pro".into(),
            primary: Some(WindowSnapshot {
                used_percent: Some(90.0),
                reset_at_epoch: None,
                window_minutes: Some(300),
                label: Some("primary".into()),
            }),
            secondary: None,
            has_credits: Some(true),
            observed_at: Some(now),
        };
        store.upsert(&snapshot, now);
        let window = crate::config::QuotaWindow {
            name: "5h".into(),
            hours: 5,
            unit: crate::config::QuotaUnit::Messages,
            cap: Some(200.0),
            models: None,
            observability: crate::config::Observability::Header,
            reset: crate::config::ResetKind::Rolling,
        };
        let view = build_account_view(
            Provider::OpenaiCodex,
            "default",
            "chatgpt-pro",
            &[window],
            &store,
            now,
        );
        assert_eq!(view.windows[0].used_percent, Some(90.0));
        assert_eq!(
            decide_account(&view, &RouteKnobs::default(), now, None).decision,
            RouteDecision::FallBackToFree
        );
    }

    #[test]
    fn credits_only_snapshot_survives_and_forces_l4_fallback() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let mut store = UtilizationStore::default();
        let snapshot = RateLimitSnapshot {
            provider: Provider::OpenaiCodex,
            account_id: "default".into(),
            plan: "chatgpt-pro".into(),
            primary: None,
            secondary: None,
            has_credits: Some(false),
            observed_at: Some(now),
        };
        assert!(store.record(&snapshot, now));
        let view = build_account_view(
            Provider::OpenaiCodex,
            "default",
            "chatgpt-pro",
            &[],
            &store,
            now,
        );
        assert_eq!(view.has_credits, Some(false));
        assert_eq!(
            decide_account(&view, &RouteKnobs::default(), now, None).decision,
            RouteDecision::FallBackToFree
        );
    }

    #[test]
    fn degraded_credits_false_does_not_pin_l4_to_fallback() {
        let observed = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let mut store = UtilizationStore::default();
        let snapshot = RateLimitSnapshot {
            provider: Provider::OpenaiCodex,
            account_id: "default".into(),
            plan: "chatgpt-pro".into(),
            primary: None,
            secondary: None,
            has_credits: Some(false),
            observed_at: Some(observed),
        };
        store.upsert(&snapshot, observed);
        let now = observed + chrono::Duration::hours(2);
        let view = build_account_view(
            Provider::OpenaiCodex,
            "default",
            "chatgpt-pro",
            &[],
            &store,
            now,
        );
        assert_eq!(view.has_credits, None);
        assert_eq!(
            decide_account(&view, &RouteKnobs::default(), now, None).decision,
            RouteDecision::PreferSub
        );
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

    #[test]
    fn l4_build_account_view_percent_only_does_not_leak_counter_computed_percent() {
        // Regression: a PercentOnly window MUST never acquire a computed
        // percent through the counter path. The store-level invariant is
        // already pinned by
        // `counter_percent_only_window_never_gets_a_computed_percent`
        // (Finding #3 / `record_counter` returns None when cap is None).
        //
        // This test pins the normalization-level invariant in
        // `build_account_view`: even when a counter row for a PercentOnly
        // window happens to have a cap set (legacy data, misconfigured
        // wire-up, or a manually-mutated store), the PercentOnly branch
        // must NOT consult it. `effective_counter_percent` would happily
        // compute `(used_tokens / cap) * 100` and surface that as the
        // window's percent — but a "computed" percent is exactly what the
        // PercentOnly invariant forbids: caps aren't API-exposed for
        // these windows, and fabricating one lets the router treat a
        // forged reading as a real one (a PercentOnly window at 50%
        // would be eligible to drive gating through `hard_cap_breach`).
        //
        // Before the fix this assertion failed: the PercentOnly branch
        // fell back to `effective_counter_percent(c, now)` and the
        // PercentOnly window came back with `used_percent = Some(50.0)`.
        let mut s = UtilizationStore::default();
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        // Counter row shaped like a misconfigured PercentOnly: cap set
        // (so `used_percent` is computed to 50.0), used_tokens = 50.
        // No header observation is recorded — the only signal is the
        // counter row, and PercentOnly must ignore it.
        s.set_counter_cap(Provider::Anthropic, "a", "max", "weekly", Some(100.0), now);
        s.record_counter(Provider::Anthropic, "a", "max", "weekly", 50.0, now);
        // Sanity: the store-level state IS a "computed percent" (50%).
        let cw = s
            .get_counter(Provider::Anthropic, "a", "max", "weekly")
            .expect("weekly counter row must exist");
        assert_eq!(
            cw.used_percent,
            Some(50.0),
            "test setup: the counter row carries a computed percent (50/100=50%)"
        );
        assert_eq!(
            cw.cap,
            Some(100.0),
            "test setup: the cap is set (misconfigured PercentOnly shape)"
        );
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
        // The invariant: PercentOnly must NOT carry a computed percent.
        assert!(
            w.used_percent.is_none(),
            "PercentOnly window leaked a counter-computed percent {:?}; \
             the router would treat this as a real reading and gate on it",
            w.used_percent,
        );
        // And the routed decision must NOT gate: with no observable
        // signal, KNEMON keeps the sub (None = headroom baseline).
        let ad = decide_account(&view, &RouteKnobs::default(), now, None);
        assert_eq!(
            ad.decision,
            RouteDecision::PreferSub,
            "PercentOnly with no header must keep the sub (None=headroom)"
        );
        assert!(
            ad.binding_window.is_none(),
            "no window must be binding when the only reading was a leaked computed percent"
        );
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
            has_credits: None,
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
            has_credits: None,
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
            has_credits: None,
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
            has_credits: None,
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

    // Regression: when a window's `reset_at` is strictly in the past
    // (clock skew / stale header / delayed rollover), the original code
    // clamped `(r - now).num_seconds()` to `0`, which then satisfied
    // `(time_to_reset / cycle_secs) <= reset_imminence_threshold` (0 <= 0.10)
    // and incorrectly flipped `is_window_imminently_resetting` to true.
    // That, in turn, let `hard_cap_breach` trip the cap_guard and
    // `relax` quietly turn it back off for a heavily-used account
    // whose last sighting was a few seconds stale — exactly the
    // "stale snapshot unblocks a paid turn through a guard we can
    // no longer see" failure mode the conservative reset-relaxation
    // rule is meant to prevent. The fix refuses to treat a past
    // `reset_at` as imminent; this test pins that contract.
    #[test]
    fn decide_account_does_not_relax_when_reset_at_is_in_the_past() {
        let now = Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        // 5h window, used 95%, reset_at 30 seconds AGO (stale header):
        // we have no reliable signal for when (or if) the rollover
        // will actually arrive, so the cap_guard must hold.
        let reset_in_the_past = now - chrono::Duration::seconds(30);
        let acct = AccountView {
            provider: Provider::Anthropic,
            account_id: "a".into(),
            plan: "max".into(),
            has_credits: None,
            windows: vec![fc_win(
                "5h",
                5,
                95.0,
                TelemetryHealth::Fresh,
                Some(reset_in_the_past),
            )],
        };
        let knobs = knobs_with(80.0, 85.0, BudgetMode::Block, None);
        let ad = decide_account(&acct, &knobs, now, None);
        assert_eq!(
            ad.decision,
            RouteDecision::FallBackToFree,
            "a stale reset_at (in the past) must not relax the cap_guard \
             gate; old code returned PreferSub because clamping to 0 \
             looked like an imminent reset"
        );
        assert_eq!(ad.binding_window.as_deref(), Some("5h"));
    }

    // =====================================================================
    // Z-11 / Z-12 failing-first pins (2026-07-08 adversarial review).
    //
    // The 2026-07-08 review of zoder's utilization tracking flagged two
    // defects that together let the router see FALSE headroom on a
    // near-exhausted budget, so a heavy user would silently fall through
    // to the free tier or a cap-less chargeback instead of being gated.
    //
    // Each test below pins one defect at the public-method boundary.
    // They MUST fail against the pre-fix code in this module and MUST
    // pass once the corresponding fix lands in `record_counter` (Z-11)
    // and `migrate_fractional_counter_percent` (Z-12).
    //
    // Path note: the 2026-07-08 task description named the path
    // `crates/zoder-cli/src/utilization.rs`, but this module is owned
    // by the `zoder-core` crate — it is what the CLI binary depends
    // on for every counter read/write. The defects and their fixes
    // are necessarily in `zoder-core/src/utilization.rs`. The CLI
    // crate exposes a thin re-export at
    // `crates/zoder-cli/src/utilization.rs` for binary-local imports.
    // =====================================================================

    // ----- Z-11: rolling-window used_tokens monotonicity -----

    /// Z-11 main pin. Record two real increments inside a 5h rolling
    /// window, then feed a stale-clock capture with `tokens_used = 0`
    /// at an `now` earlier than `last_updated`. Pre-fix zeros the
    /// window; post-fix is a no-op.
    #[test]
    fn rolling_window_used_tokens_not_zeroed_by_stale_clock_capture() {
        let mut store = UtilizationStore::default();
        let t0 = Utc.with_ymd_and_hms(2026, 7, 4, 10, 0, 0).unwrap();
        store.set_counter_rolling_hours(Provider::MiniMax, "default", MM_PLAN, "5h", Some(5), t0);
        store.set_counter_cap(Provider::MiniMax, "default", MM_PLAN, "5h", Some(100.0), t0);
        let t1 = t0 + chrono::Duration::minutes(30);
        let _ = store.record_counter(Provider::MiniMax, "default", MM_PLAN, "5h", 50.0, t1);
        let _ = store.record_counter(Provider::MiniMax, "default", MM_PLAN, "5h", 45.0, t1);
        let used_after_stale =
            store.record_counter(Provider::MiniMax, "default", MM_PLAN, "5h", 0.0, t0);
        assert_eq!(used_after_stale, 95.0);
        let counter = store
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "5h")
            .unwrap();
        assert_eq!(counter.used_tokens, 95.0);
        assert_eq!(counter.used_percent, Some(95.0));
        assert_eq!(counter.increments.len(), 2);
        assert_eq!(counter.last_updated, t1);
    }

    /// Z-11 secondary pin: stale-clock capture with `tokens_used > 0`
    /// must NOT push a phantom increment AND drop real in-window
    /// increments. Pre-fix summed only the phantom increment.
    #[test]
    fn rolling_window_clock_rollback_capture_is_a_no_op() {
        let mut store = UtilizationStore::default();
        let t0 = Utc.with_ymd_and_hms(2026, 7, 4, 10, 0, 0).unwrap();
        store.set_counter_rolling_hours(Provider::MiniMax, "default", MM_PLAN, "5h", Some(5), t0);
        store.set_counter_cap(Provider::MiniMax, "default", MM_PLAN, "5h", Some(100.0), t0);
        let t1 = t0 + chrono::Duration::minutes(30);
        let _ = store.record_counter(Provider::MiniMax, "default", MM_PLAN, "5h", 50.0, t1);
        let _ = store.record_counter(Provider::MiniMax, "default", MM_PLAN, "5h", 45.0, t1);
        let _ = store.record_counter(Provider::MiniMax, "default", MM_PLAN, "5h", 999.0, t0);
        let counter = store
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "5h")
            .unwrap();
        assert_eq!(counter.used_tokens, 95.0);
        assert_eq!(counter.last_updated, t1);
        assert_eq!(counter.increments.len(), 2);
    }

    /// Non-breaking contract: the clock-rollback guard is scoped to
    /// `rolling_hours.is_some()`, so non-rolling accumulators are
    /// unaffected. Pre-fix and post-fix must both preserve used_tokens.
    #[test]
    fn non_rolling_clock_rollback_capture_preserves_used_tokens() {
        let mut store = UtilizationStore::default();
        let t0 = Utc.with_ymd_and_hms(2026, 7, 4, 10, 0, 0).unwrap();
        store.set_counter_cap(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            Some(100.0),
            t0,
        );
        let _ = store.record_counter(Provider::MiniMax, "default", MM_PLAN, "monthly", 60.0, t0);
        let t_stale = t0 - chrono::Duration::seconds(1);
        let used_after_stale = store.record_counter(
            Provider::MiniMax,
            "default",
            MM_PLAN,
            "monthly",
            0.0,
            t_stale,
        );
        assert_eq!(used_after_stale, 60.0);
        let counter = store
            .get_counter(Provider::MiniMax, "default", MM_PLAN, "monthly")
            .unwrap();
        assert_eq!(counter.used_tokens, 60.0);
    }

    // ----- Z-12: v1->v2 migration preserves used_percent when cap = None -----

    /// Z-12 main pin: migrate a legacy v1 record with `used_percent = 85`
    /// and `cap = None` through `UtilizationStore::open_unlocked` (which
    /// triggers `migrate_fractional_counter_percent`). Pre-fix dropped
    /// the stored 85 to None; post-fix preserves it.
    #[test]
    fn legacy_counter_percent_migration_preserves_stored_percent_when_cap_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("legacy-v1-capless.json");
        let counter = serde_json::json!({
            "provider": "mini_max",
            "account_id": "default",
            "plan": "max",
            "window_name": "percent_only",
            "used_tokens": 85_000.0,
            "used_percent": 85.0,
            "last_updated": "2026-07-04T12:00:00Z"
        });
        std::fs::write(
            &legacy_path,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "records": {},
                "counters": {"percent_only": counter}
            }))
            .unwrap(),
        )
        .unwrap();
        let migrated = UtilizationStore::open_unlocked(&legacy_path).unwrap();
        let cw = migrated.counters.get("percent_only").unwrap();
        assert_eq!(cw.used_percent, Some(85.0));
    }

    /// Z-12 secondary pin: legacy fractional values (1.2 -> 120,
    /// 0.85 -> 85) with cap = None must be converted by the
    /// 0..=2 -> 0..=200 fractional-to-percentage rule.
    #[test]
    fn legacy_counter_percent_migration_converts_legacy_fraction_when_cap_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("legacy-v1-capless-fractional.json");
        let row = |name: &str, used_percent: f64| {
            serde_json::json!({
                "provider": "mini_max",
                "account_id": "default",
                "plan": "max",
                "window_name": name,
                "used_tokens": 0.0,
                "used_percent": used_percent,
                "last_updated": "2026-07-04T12:00:00Z"
            })
        };
        std::fs::write(
            &legacy_path,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "records": {},
                "counters": {
                    "exhausted": row("exhausted", 1.2),
                    "ok": row("ok", 0.85),
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let migrated = UtilizationStore::open_unlocked(&legacy_path).unwrap();
        assert_eq!(migrated.counters["exhausted"].used_percent, Some(120.0));
        assert_eq!(migrated.counters["ok"].used_percent, Some(85.0));
    }

    /// Non-breaking contract: a v1 row with BOTH `used_percent = None`
    /// AND `cap = None` must stay None after migration.
    #[test]
    fn legacy_counter_percent_migration_stays_none_when_stored_is_none_and_cap_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("legacy-v1-capless-none.json");
        let counter = serde_json::json!({
            "provider": "mini_max",
            "account_id": "default",
            "plan": "max",
            "window_name": "truly_unknown",
            "used_tokens": 0.0,
            "last_updated": "2026-07-04T12:00:00Z"
        });
        std::fs::write(
            &legacy_path,
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "records": {},
                "counters": {"truly_unknown": counter}
            }))
            .unwrap(),
        )
        .unwrap();
        let migrated = UtilizationStore::open_unlocked(&legacy_path).unwrap();
        let cw = &migrated.counters["truly_unknown"];
        assert!(cw.used_percent.is_none());
    }
}
