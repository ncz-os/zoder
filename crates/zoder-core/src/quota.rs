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
use chrono::{Duration, Utc};
use serde::Serialize;

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
    let used: f64 = entries
        .iter()
        .filter(|e| e.provider == provider_id && e.ts_utc >= since && e.ts_utc <= now)
        .map(|e| unit_amount(e, w.unit))
        .sum();
    let pct = if w.cap > 0.0 { used / w.cap } else { 0.0 };
    WindowUsage {
        name: w.name.clone(),
        hours: w.hours,
        unit: format!("{:?}", w.unit).to_ascii_lowercase(),
        used,
        cap: w.cap,
        pct,
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
