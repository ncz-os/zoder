//! Subscription quota accounting.
//!
//! A subscription provider (flat monthly fee + rolling rate-limit windows) has
//! a $0 *marginal* cost per call: the scarce resource is the window cap, not
//! dollars. This module measures how much of each rolling window a provider has
//! consumed (from the local ledger) and the amortized per-call cost of the flat
//! fee, so the report can show "62% of the 5h window" instead of a misleading
//! per-token dollar figure.

use crate::config::{QuotaUnit, QuotaWindow, ResetKind, SubscriptionPlan};
use crate::ledger::Entry;
use crate::subscription_tiers::{resolve_plan_windows, TierCatalog, WindowProvenance};
use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use serde::Serialize;

/// Distinct `source` value for operator-entered windows in serialized reports.
/// Catalog windows carry the catalog row's own `source` string
/// (e.g. `"minimax_token_plan"`, `"observed"`); windows with no catalog origin
/// carry this constant so JSON consumers can filter/sort/label report rows
/// unambiguously without parsing the `confidence` field.
pub const SOURCE_OPERATOR: &str = "operator";

/// pct at/above which a window is flagged as approaching its cap.
pub const APPROACHING_THRESHOLD: f64 = 0.8;

/// Consumption of one rolling window for a provider.
#[derive(Debug, Clone, Serialize)]
pub struct WindowUsage {
    pub name: String,
    pub hours: u32,
    pub unit: String,
    pub used: f64,
    /// Cap value, in `unit`, over the rolling window. `None` = unknown /
    /// percent-only — the window exists but the cap value isn't known
    /// locally. `pct` against `None` is always 0 (treated as headroom,
    /// never as exhausted).
    pub cap: Option<f64>,
    /// Fraction of the cap consumed (0..=1; can exceed 1 when over cap).
    pub pct: f64,
    /// When the next relief arrives in this rolling window: the oldest in-window
    /// call ages out at `oldest_ts + hours`, freeing its share of the cap. `None`
    /// when the window is empty (already at full capacity). RFC3339 UTC.
    pub next_reset_utc: Option<String>,
    /// True when usage is at/above [`APPROACHING_THRESHOLD`] of the cap.
    pub approaching: bool,
    /// `confidence` of the cap value (`published` | `observed` | `estimated`).
    /// Always carried so the caller can label reports "ESTIMATED" honestly;
    /// `None` for explicit (hand-entered) windows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    /// Provenance of this window: `None` (legacy callers without a `source`
    /// arg), the catalog row's own `source` string (e.g.
    /// `"minimax_token_plan"`, `"observed"`), or [`crate::quota::SOURCE_OPERATOR`]
    /// when the window came from the operator's config rather than a catalog
    /// preset. Carried PER WINDOW (not per plan) so override / appended
    /// windows in a `PresetWithOverrides` plan are not mislabeled as
    /// catalog-backed. Always `Some` after going through [`plan_usage`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

fn unit_amount(e: &Entry, unit: QuotaUnit) -> f64 {
    match unit {
        QuotaUnit::Tokens => e.tokens_in.saturating_add(e.tokens_out) as f64,
        // Requests, Messages, and Sessions each contribute one to the
        // rolling-window counter for the matching ledger entry. Distinct
        // semantically (a single agent turn counts one in each unit), but
        // the per-Entry contribution is the same: 1 per matching row.
        // Multi-turn sessions would need an Entry-level `session_id`
        // field that's out of scope for the declarative-config change.
        QuotaUnit::Requests | QuotaUnit::Messages | QuotaUnit::Sessions => 1.0,
    }
}

/// Consumption of one rolling window for `provider_id`, measured over the
/// trailing `window.hours` from now. The optional `confidence` + `source` are
/// plumbed through from the catalog so the report can label preset-driven
/// windows (and leave explicit windows unlabeled) without mislabeling
/// operator-entered overrides / extras as catalog-backed estimates.
///
/// `confidence = None` AND `source = None` ↔ hand-entered window (legacy
/// `confidence`-only callers get identical behavior to before).
pub fn window_usage(
    entries: &[Entry],
    provider_id: &str,
    w: &QuotaWindow,
    confidence: Option<&str>,
    source: Option<&str>,
) -> WindowUsage {
    window_usage_at(entries, provider_id, w, confidence, source, Utc::now())
}

/// Clock-injected form used by routing and boundary tests. Calendar windows
/// count only the active UTC calendar period; `hours` applies only to rolling
/// windows.
pub fn window_usage_at(
    entries: &[Entry],
    provider_id: &str,
    w: &QuotaWindow,
    confidence: Option<&str>,
    source: Option<&str>,
    now: DateTime<Utc>,
) -> WindowUsage {
    let in_active_window = |ts: DateTime<Utc>| match w.reset {
        ResetKind::Rolling => ts >= now - Duration::hours(w.hours as i64),
        ResetKind::CalendarMonthly => ts.year() == now.year() && ts.month() == now.month(),
        ResetKind::CalendarDaily => ts.date_naive() == now.date_naive(),
    };
    let mut used = 0.0;
    let mut oldest: Option<DateTime<Utc>> = None;
    for e in entries
        .iter()
        .filter(|e| e.provider == provider_id && e.ts_utc <= now && in_active_window(e.ts_utc))
    {
        used += unit_amount(e, w.unit);
        oldest = Some(oldest.map_or(e.ts_utc, |o| o.min(e.ts_utc)));
    }
    let pct = match w.cap {
        // Unknown cap → observable only as a percent on the *provider*'s
        // side; locally we treat this window as permanently below cap
        // (headroom) so it never falsely flips a subscription to
        // "exhausted" and demotes it from the smart router. The contract
        // documented on `QuotaWindow::cap` is exactly this — `None` means
        // headroom, never zero.
        None => 0.0,
        // A known positive cap is the normal math. `cap = 0.0` (operator
        // typo) is treated as "no headroom" rather than dividing by zero
        // so a bad config doesn't make `pct` explode to `inf`.
        Some(c) if c > 0.0 => used / c,
        Some(_) => 0.0,
    };
    let next_reset = match w.reset {
        ResetKind::Rolling => oldest.map(|o| o + Duration::hours(w.hours as i64)),
        ResetKind::CalendarDaily => Some(
            Utc.from_utc_datetime(
                &(now.date_naive() + Duration::days(1))
                    .and_hms_opt(0, 0, 0)
                    .expect("midnight is valid"),
            ),
        ),
        ResetKind::CalendarMonthly => {
            let (year, month) = if now.month() == 12 {
                (now.year() + 1, 1)
            } else {
                (now.year(), now.month() + 1)
            };
            Some(
                Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
                    .single()
                    .expect("first of month is valid"),
            )
        }
    };
    let next_reset_utc = next_reset.map(|reset| reset.to_rfc3339());
    WindowUsage {
        name: w.name.clone(),
        hours: w.hours,
        unit: format!("{:?}", w.unit).to_ascii_lowercase(),
        used,
        // Pass the cap (or `None`) through verbatim — the report JSON
        // surfaces "unknown" as a JSON `null` instead of a fake `0`,
        // which would mislead downstream consumers into reading "used /
        // unknown" as a perfectly fine integer.
        cap: w.cap,
        pct,
        next_reset_utc,
        approaching: pct >= APPROACHING_THRESHOLD,
        confidence: confidence.map(|s| s.to_string()),
        source: source.map(|s| s.to_string()),
    }
}

/// Backward-compat shim: original 4-arg signature for callers that don't have
/// a per-window `source` to plumb through (the old `confidence`-only API).
/// Equivalent to [`window_usage`] with `source = None`.
pub fn window_usage_confidence_only(
    entries: &[Entry],
    provider_id: &str,
    w: &QuotaWindow,
    confidence: Option<&str>,
) -> WindowUsage {
    window_usage(entries, provider_id, w, confidence, None)
}

/// Consumption of every window in a plan, for one provider. When the plan
/// declares a `tier`, the effective windows are resolved through the
/// `catalog` first (preset → explicit overrides by `name`). When the plan
/// has no `tier` or the resolver falls back to explicit windows, the
/// confidence tag is `None` (hand-entered).
///
/// **Per-window provenance** (the fix vs. the prior single-`confidence` tag):
/// the resolver attaches a [`WindowProvenance`] to every emitted window, and
/// we propagate it row-by-row. Catalog windows inherit the catalog row's
/// `confidence` + `source` (e.g. `("observed", "minimax_token_plan")`);
/// operator-entered override windows and "extra" appended windows are tagged
/// `None` confidence with `source = "operator"` so JSON consumers can
/// distinguish them from catalog rows without parsing the `confidence` field.
/// No more "every window in this plan reads as catalog-backed" mislabeling.
pub fn plan_usage(
    entries: &[Entry],
    provider_id: &str,
    plan: &SubscriptionPlan,
    catalog: &TierCatalog,
) -> Vec<WindowUsage> {
    plan_usage_for_catalog_provider(entries, provider_id, plan, catalog, provider_id)
}

/// Resolve a plan through a canonical catalog namespace while continuing to
/// account ledger rows under the operator's arbitrary provider id.
pub fn plan_usage_for_catalog_provider(
    entries: &[Entry],
    ledger_provider_id: &str,
    plan: &SubscriptionPlan,
    catalog: &TierCatalog,
    catalog_provider_id: &str,
) -> Vec<WindowUsage> {
    let resolved = resolve_plan_windows(plan, catalog, Some(catalog_provider_id));
    debug_assert_eq!(
        resolved.windows.len(),
        resolved.provenance.len(),
        "resolver invariant violated: windows.len() != provenance.len() ({} vs {})",
        resolved.windows.len(),
        resolved.provenance.len(),
    );
    resolved
        .windows
        .iter()
        .zip(resolved.provenance.iter())
        .map(|(w, prov)| {
            let (confidence, source) = match prov {
                WindowProvenance::Catalog { confidence, source } => {
                    (Some(confidence.as_str()), Some(source.as_str()))
                }
                WindowProvenance::Operator => (None, Some(SOURCE_OPERATOR)),
            };
            window_usage(entries, ledger_provider_id, w, confidence, source)
        })
        .collect()
}

/// Amortized $/call for the flat fee: the monthly fee spread across the calls
/// actually made on this provider in the trailing 30 days. Returns 0 when the
/// plan has no fee or no calls were made. Accepts the catalog so it stays
/// symmetric with [`plan_usage`]; the catalog is not consulted here (the
/// `monthly_fee_usd` lives on the plan, not the catalog), but accepting it
/// keeps the call sites uniform and lets a future enhancement factor the
/// catalog's `monthly_fee_usd` override in one place.
pub fn amortized_per_call(
    entries: &[Entry],
    provider_id: &str,
    plan: &SubscriptionPlan,
    _catalog: &TierCatalog,
) -> f64 {
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

#[cfg(test)]
mod tests {
    //! Per-window confidence + source threading (the BLOCKER fix).
    //!
    //! Asserts that `plan_usage` propagates the catalog's `(confidence,
    //! source)` tuple ONLY to preset windows that survived untouched, and
    //! tags operator-entered override / appended windows with `confidence
    //! = None` and `source = "operator"`. Before the fix, every window in a
    //! `PresetWithOverrides` plan inherited the catalog confidence — this
    //! suite is the regression guard.

    use super::*;
    use crate::config::{QuotaUnit, QuotaWindow, SubscriptionPlan};
    use crate::subscription_tiers::{
        Confidence, ProviderTiers, TierCatalog, TierEntry, TierWindow,
    };
    use chrono::{Duration, TimeZone, Utc};
    use std::collections::BTreeMap;

    fn w(name: &str, hours: u32, cap: f64, unit: QuotaUnit) -> QuotaWindow {
        QuotaWindow {
            name: name.into(),
            hours,
            unit,
            cap: Some(cap),
            models: None,
            observability: crate::config::Observability::default(),
            reset: crate::config::ResetKind::default(),
        }
    }

    fn tw(name: &str, hours: u32, cap: f64, unit: QuotaUnit) -> TierWindow {
        TierWindow {
            name: name.into(),
            hours,
            unit,
            cap: Some(cap),
            models: None,
            observability: crate::config::Observability::default(),
            reset: crate::config::ResetKind::default(),
        }
    }

    /// One Anthropic `claude-max-20x` tier with `(Observed, "observed")` and
    /// two windows: a 5h of 900 messages and a weekly of 8000 messages.
    fn catalog() -> TierCatalog {
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "claude-max-20x".into(),
            TierEntry {
                monthly_fee_usd: 200.0,
                confidence: Confidence::Observed,
                source: "observed".into(),
                windows: vec![
                    tw("5h", 5, 900.0, QuotaUnit::Messages),
                    tw("weekly", 168, 8000.0, QuotaUnit::Messages),
                ],
            },
        );
        let mut providers = BTreeMap::new();
        providers.insert(
            "anthropic".into(),
            ProviderTiers {
                tiers: tiers.clone(),
            },
        );
        TierCatalog {
            version: 1,
            as_of: "2026-06-30".into(),
            disclaimer: "ESTIMATES".into(),
            providers,
        }
    }

    fn by_name(usage: &[WindowUsage]) -> std::collections::HashMap<String, WindowUsage> {
        usage.iter().map(|w| (w.name.clone(), w.clone())).collect()
    }

    #[test]
    fn calendar_monthly_window_has_full_headroom_after_month_boundary() {
        let june = Utc.with_ymd_and_hms(2026, 6, 30, 23, 0, 0).unwrap();
        let july = Utc.with_ymd_and_hms(2026, 7, 1, 0, 1, 0).unwrap();
        let entry = Entry {
            ts_utc: june,
            provider: "minimax-sub".into(),
            model: "MiniMax-M3".into(),
            host: "api.minimax.io".into(),
            tokens_in: 100,
            tokens_out: 0,
            cost_usd: 0.0,
            cost_unknown: false,
            calls: 1,
            violation: None,
            tags: crate::ledger::FinOpsTags::default(),
        };
        let window = QuotaWindow {
            name: "monthly".into(),
            hours: 720,
            unit: QuotaUnit::Tokens,
            cap: Some(100.0),
            models: None,
            observability: crate::config::Observability::Counter,
            reset: crate::config::ResetKind::CalendarMonthly,
        };
        let usage = window_usage_at(&[entry], "minimax-sub", &window, None, None, july);
        assert_eq!(usage.used, 0.0);
        assert_eq!(usage.pct, 0.0);
        assert_eq!(
            usage.next_reset_utc.as_deref(),
            Some("2026-08-01T00:00:00+00:00")
        );
    }

    #[test]
    fn plan_usage_no_tier_leaves_every_window_unlabeled_for_confidence() {
        // Explicit-only plan: every window is hand-entered → `confidence =
        // None`. Source is uniformly tagged `Some("operator")` so downstream
        // JSON consumers can filter all operator-provenance rows with the
        // same predicate regardless of whether they came from a full-explicit
        // plan or a `PresetWithOverrides` override / extra.
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![w("5h", 5, 100.0, QuotaUnit::Messages)],
            tier: None,
            ..Default::default()
        };
        let cat = TierCatalog::empty();
        let out = plan_usage(&[], "anthropic", &plan, &cat);
        assert_eq!(out.len(), 1);
        assert!(out[0].confidence.is_none());
        assert_eq!(out[0].source.as_deref(), Some("operator"));
    }

    #[test]
    fn plan_usage_preset_alone_tags_every_window_with_catalog_provenance() {
        // Preset with no operator windows → every row inherits the catalog
        // row's (Observed, "observed").
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![],
            tier: Some("claude-max-20x".into()),
            ..Default::default()
        };
        let cat = catalog();
        let out = plan_usage(&[], "anthropic", &plan, &cat);
        assert_eq!(out.len(), 2);
        let by_n = by_name(&out);
        assert_eq!(by_n["5h"].confidence.as_deref(), Some("observed"));
        assert_eq!(by_n["5h"].source.as_deref(), Some("observed"));
        assert_eq!(by_n["weekly"].confidence.as_deref(), Some("observed"));
        assert_eq!(by_n["weekly"].source.as_deref(), Some("observed"));
    }

    #[test]
    fn plan_usage_preset_with_overrides_threading_per_window_provenance() {
        // THIS is the regression guard for the BLOCKER. Pre-fix, every
        // window in a `PresetWithOverrides` plan was labeled with the
        // catalog confidence. We now expect:
        //   - "5h" (overridden)       → None / "operator"
        //   - "weekly" (preserved)    → "observed" / "observed"
        //   - "daily" (appended)      → None / "operator"
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![
                w("5h", 5, 1500.0, QuotaUnit::Messages),    // override
                w("daily", 24, 500.0, QuotaUnit::Messages), // append (extra)
            ],
            tier: Some("claude-max-20x".into()),
            ..Default::default()
        };
        let cat = catalog();
        let out = plan_usage(&[], "anthropic", &plan, &cat);
        assert_eq!(out.len(), 3);
        let by_n = by_name(&out);

        // 5h: overridden → operator.
        assert_eq!(
            by_n["5h"].confidence, None,
            "overridden window must not carry catalog confidence"
        );
        assert_eq!(
            by_n["5h"].source.as_deref(),
            Some("operator"),
            "overridden window must be tagged source = \"operator\""
        );
        // Override cap must reach the usage row (operator's value, not the
        // catalog's 900).
        assert_eq!(by_n["5h"].cap, Some(1500.0));

        // weekly: untouched preset row → catalog provenance intact.
        assert_eq!(by_n["weekly"].confidence.as_deref(), Some("observed"));
        assert_eq!(by_n["weekly"].source.as_deref(), Some("observed"));
        assert_eq!(by_n["weekly"].cap, Some(8000.0));

        // daily: appended → operator.
        assert_eq!(by_n["daily"].confidence, None);
        assert_eq!(by_n["daily"].source.as_deref(), Some("operator"));
        assert_eq!(by_n["daily"].cap, Some(500.0));
    }

    #[test]
    fn plan_usage_unknown_tier_every_window_operator() {
        // Unknown tier (or empty catalog) → can't inherit provenance from
        // nothing → operator-entered windows are unlabeled for confidence but
        // tagged `source = "operator"` so JSON consumers have a uniform
        // signal across every operator-provenance path.
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![w("5h", 5, 250.0, QuotaUnit::Messages)],
            tier: Some("claude-max-does-not-exist".into()),
            ..Default::default()
        };
        let cat = catalog(); // catalog exists but tier is unknown.
        let out = plan_usage(&[], "anthropic", &plan, &cat);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].confidence, None);
        assert_eq!(out[0].source.as_deref(), Some("operator"));
        assert_eq!(out[0].cap, Some(250.0));
    }

    #[test]
    fn plan_usage_carries_confidence_for_distinct_catalog_tiers() {
        // A different catalog row with a different `confidence` / `source`
        // must round-trip unchanged through `plan_usage`. Guards against
        // accidentally hard-coding one tier's provenance in the engine.
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "pro".into(),
            TierEntry {
                monthly_fee_usd: 20.0,
                confidence: Confidence::Published,
                source: "minimax_token_plan".into(),
                windows: vec![tw("5h", 5, 200.0, QuotaUnit::Messages)],
            },
        );
        let mut providers = BTreeMap::new();
        providers.insert(
            "anthropic".into(),
            ProviderTiers {
                tiers: tiers.clone(),
            },
        );
        let cat = TierCatalog {
            version: 1,
            as_of: "2026-06-30".into(),
            disclaimer: "d".into(),
            providers,
        };
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![],
            tier: Some("pro".into()),
            ..Default::default()
        };
        let out = plan_usage(&[], "anthropic", &plan, &cat);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].confidence.as_deref(), Some("published"));
        assert_eq!(out[0].source.as_deref(), Some("minimax_token_plan"));
    }

    #[test]
    fn plan_usage_measured_calls_in_lookback() {
        // Smoke test that the actual measurement loop still works with the
        // new signature: synthesize a few entries in the last 5h, confirm
        // `used` and `pct` reflect them while confidence / source are
        // independent.
        let now = Utc::now();
        let mk = |provider: &str, mins_ago: i64, calls: u64| Entry {
            ts_utc: now - Duration::minutes(mins_ago),
            provider: provider.into(),
            model: "x".into(),
            host: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            cost_unknown: false,
            calls,
            violation: None,
            tags: crate::ledger::FinOpsTags::default(),
        };
        let entries = vec![mk("anthropic", 10, 1), mk("anthropic", 60, 1)];
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![],
            tier: Some("claude-max-20x".into()),
            ..Default::default()
        };
        let cat = catalog();
        let out = plan_usage(&entries, "anthropic", &plan, &cat);
        assert_eq!(out.len(), 2);
        // Both windows picked up the catalog provenance.
        let by_n = by_name(&out);
        assert_eq!(by_n["5h"].confidence.as_deref(), Some("observed"));
        assert_eq!(by_n["weekly"].confidence.as_deref(), Some("observed"));
        // Note: `unit_amount` returns 1.0 per matching Entry for
        // QuotaUnit::Messages, so two entries in-window → `used >= 2.0`.
        assert!(by_n["5h"].used >= 2.0);
    }
}
