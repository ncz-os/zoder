//! Subscription quota accounting.
//!
//! A subscription provider (flat monthly fee + rolling rate-limit windows) has
//! a $0 *marginal* cost per call: the scarce resource is the window cap, not
//! dollars. This module measures how much of each rolling window a provider has
//! consumed (from the local ledger) and the amortized per-call cost of the flat
//! fee, so the report can show "62% of the 5h window" instead of a misleading
//! per-token dollar figure.

use crate::config::{QuotaUnit, QuotaWindow, SubscriptionPlan};
use crate::ledger::Entry;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

/// pct at/above which a window is flagged as approaching its cap.
pub const APPROACHING_THRESHOLD: f64 = 0.8;

/// Consumption of one rolling window for a provider.
#[derive(Debug, Clone, Serialize)]
pub struct WindowUsage {
    pub name: String,
    pub hours: u32,
    pub unit: String,
    pub used: f64,
    pub cap: f64,
    /// Fraction of the cap consumed (0..=1; can exceed 1 when over cap).
    pub pct: f64,
    /// When the next relief arrives in this rolling window: the oldest in-window
    /// call ages out at `oldest_ts + hours`, freeing its share of the cap. `None`
    /// when the window is empty (already at full capacity). RFC3339 UTC.
    pub next_reset_utc: Option<String>,
    /// True when usage is at/above [`APPROACHING_THRESHOLD`] of the cap.
    pub approaching: bool,
}

fn unit_amount(e: &Entry, unit: QuotaUnit) -> f64 {
    match unit {
        QuotaUnit::Tokens => (e.tokens_in + e.tokens_out) as f64,
        QuotaUnit::Requests | QuotaUnit::Messages => 1.0,
    }
}

/// Consumption of one rolling window for `provider_id`, measured over the
/// trailing `window.hours` from now.
pub fn window_usage(entries: &[Entry], provider_id: &str, w: &QuotaWindow) -> WindowUsage {
    let now = Utc::now();
    let since = now - Duration::hours(w.hours as i64);
    let mut used = 0.0;
    let mut oldest: Option<DateTime<Utc>> = None;
    for e in entries
        .iter()
        .filter(|e| e.provider == provider_id && e.ts_utc >= since && e.ts_utc <= now)
    {
        used += unit_amount(e, w.unit);
        oldest = Some(oldest.map_or(e.ts_utc, |o| o.min(e.ts_utc)));
    }
    let pct = if w.cap > 0.0 { used / w.cap } else { 0.0 };
    let next_reset_utc = oldest.map(|o| (o + Duration::hours(w.hours as i64)).to_rfc3339());
    WindowUsage {
        name: w.name.clone(),
        hours: w.hours,
        unit: format!("{:?}", w.unit).to_ascii_lowercase(),
        used,
        cap: w.cap,
        pct,
        next_reset_utc,
        approaching: pct >= APPROACHING_THRESHOLD,
    }
}

/// Consumption of every window in a plan, for one provider.
pub fn plan_usage(
    entries: &[Entry],
    provider_id: &str,
    plan: &SubscriptionPlan,
) -> Vec<WindowUsage> {
    plan.windows
        .iter()
        .map(|w| window_usage(entries, provider_id, w))
        .collect()
}

/// Amortized $/call for the flat fee: the monthly fee spread across the calls
/// actually made on this provider in the trailing 30 days. Returns 0 when the
/// plan has no fee or no calls were made.
pub fn amortized_per_call(entries: &[Entry], provider_id: &str, plan: &SubscriptionPlan) -> f64 {
    if plan.monthly_fee_usd <= 0.0 {
        return 0.0;
    }
    let since = Utc::now() - Duration::days(30);
    let calls = entries
        .iter()
        .filter(|e| e.provider == provider_id && e.ts_utc >= since)
        .count();
    if calls == 0 {
        0.0
    } else {
        plan.monthly_fee_usd / calls as f64
    }
}
