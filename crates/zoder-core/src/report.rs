//! `zoder report`: local-first usage + chargeback rollups from the spend
//! ledger, priced by the pricing catalog.
//!
//! Time-series buckets at a chosen granularity (hour/day/week/month) over an
//! explicit window, plus a by-model breakdown and the avoided-spend headline
//! (free tokens valued at the frontier baseline). Per-call cost comes from the
//! ledger entry itself; the pricing catalog drives only the counterfactual.

use crate::ledger::{Entry, Ledger};
use crate::pricing::PricingCatalog;
use chrono::{DateTime, Datelike, IsoWeek, Utc};
use serde::Serialize;
use std::collections::BTreeMap;

/// Bucket granularity for a report's time series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gran {
    Hour,
    Day,
    Week,
    Month,
}

impl Gran {
    fn key(&self, ts: &DateTime<Utc>) -> String {
        match self {
            Gran::Hour => ts.format("%Y-%m-%d %H:00").to_string(),
            Gran::Day => ts.format("%Y-%m-%d").to_string(),
            Gran::Week => {
                let w: IsoWeek = ts.iso_week();
                format!("{}-W{:02}", w.year(), w.week())
            }
            Gran::Month => ts.format("%Y-%m").to_string(),
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Gran::Hour => "hour",
            Gran::Day => "day",
            Gran::Week => "week",
            Gran::Month => "month",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Bucket {
    pub key: String,
    pub cost_usd: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub calls: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RowByModel {
    pub model: String,
    pub cost_usd: f64,
    pub tokens: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub calls: u64,
    /// True when the model actually incurred a chargeback (a paid cloud model).
    /// Derived from recorded spend, never from a fuzzy catalog match, so free
    /// free-tier models are never mislabeled paid.
    pub billed: bool,
    /// Catalog input rate in USD per 1M tokens. Populated only for `billed`
    /// models (kept at 0 for free models so the split stays unambiguous).
    pub input_usd_per_mtok: f64,
    /// Catalog output rate in USD per 1M tokens (paid models only).
    pub output_usd_per_mtok: f64,
}

/// Per-publisher-host rollup. The `host` is the model-id publisher (e.g.
/// `meta`, `anthropic`, `enterprise`) — the segment before `/`. Distinct from the
/// per-model and per-provider views: the same publisher's traffic is summed
/// across every provider that served it (enterprise direct + OpenRouter + …).
#[derive(Debug, Clone, Default, Serialize)]
pub struct RowByHost {
    pub host: String,
    pub cost_usd: f64,
    pub tokens: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub calls: u64,
    /// True when any call attributed to this host actually incurred a charge.
    pub billed: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    /// Human label for the window, e.g. "this month", "Q2 2026", "YTD 2026".
    pub period: String,
    pub since: String,
    pub until: String,
    pub days: i64,
    /// Granularity of `buckets`: "hour" | "day" | "week" | "month".
    pub bucket_gran: String,
    pub buckets: Vec<Bucket>,
    pub by_model: Vec<RowByModel>,
    /// Per-publisher-host rollup (sorted by cost desc, then tokens). Empty when
    /// no entry has a resolvable publisher host (un-prefixed model ids).
    pub by_host: Vec<RowByHost>,
    /// Externally-billed chargeback over the window (the cash number).
    pub total_cost_usd: f64,
    pub total_tokens: u64,
    pub total_calls: u64,
    /// Tokens served at $0 chargeback (free-tier).
    pub free_tokens: u64,
    pub billed_tokens: u64,
    /// Free tokens valued at the frontier baseline (avoided external spend).
    pub avoided_usd: f64,
    /// Counterfactual: ALL tokens (free + billed) valued at the frontier
    /// baseline -- "what this period would have cost on the baseline model".
    pub counterfactual_usd: f64,
    pub baseline_model: String,
    pub baseline_usd_per_mtok: f64,
}

/// Build a report over an explicit `[since, until]` window from the local
/// ledger, bucketing the time series at `gran` granularity. `period` is a
/// human label for the window (e.g. "this month", "Q2 2026"). Cost is taken
/// from each ledger entry (the truth for what was actually paid); the pricing
/// catalog is used only for the avoided-spend / counterfactual baseline.
pub fn build_report(
    ledger: &Ledger,
    pricing: &PricingCatalog,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    gran: Gran,
    period: &str,
) -> anyhow::Result<Report> {
    build_report_from_entries(
        &ledger.entries_in(Some(since), Some(until))?,
        pricing,
        since,
        until,
        gran,
        period,
    )
}

/// Same as [`build_report`] but takes a pre-filtered entry slice. Used by
/// `zoder report --vendor <name>` to scope the report to a vendor's providers
/// (so totals, counterfactual, and avoided-spend headline are all
/// recomputed over only the matching entries — the headline stays
/// meaningful instead of mixing enterprise and non-enterprise traffic).
pub fn build_report_from_entries(
    entries: &[Entry],
    pricing: &PricingCatalog,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    gran: Gran,
    period: &str,
) -> anyhow::Result<Report> {
    let mut buckets: BTreeMap<String, Bucket> = BTreeMap::new();
    let mut models: BTreeMap<String, RowByModel> = BTreeMap::new();
    let mut hosts: BTreeMap<String, RowByHost> = BTreeMap::new();
    let mut rep = Report {
        period: period.to_string(),
        since: since.format("%Y-%m-%d").to_string(),
        until: until.format("%Y-%m-%d").to_string(),
        days: (until - since).num_days().max(1),
        bucket_gran: gran.label().to_string(),
        baseline_model: pricing.baseline_model.clone(),
        baseline_usd_per_mtok: pricing.baseline_usd_per_mtok,
        ..Default::default()
    };

    for e in entries {
        // The ledger's recorded cost is the truth for what you ACTUALLY paid:
        // it reflects how the call was billed (free/subscription providers record
        // $0; metered providers record their charge). The pricing catalog is used
        // only for the counterfactual baseline below, never to re-bill free usage.
        let cost = e.cost_usd;
        let billed = e.cost_usd > 0.0;
        let tok = e.tokens_in + e.tokens_out;

        let key = gran.key(&e.ts_utc);
        let b = buckets.entry(key.clone()).or_default();
        b.key = key;
        b.cost_usd += cost;
        b.tokens_in += e.tokens_in;
        b.tokens_out += e.tokens_out;
        b.calls += 1;

        let m = models.entry(e.model.clone()).or_default();
        m.model = e.model.clone();
        m.cost_usd += cost;
        m.tokens += tok;
        m.tokens_in += e.tokens_in;
        m.tokens_out += e.tokens_out;
        m.calls += 1;
        m.billed |= billed;

        // Per-publisher-host rollup (publisher prefix of the model id). Skipped
        // for un-prefixed model ids so a bare `gpt-4o` doesn't create a "" host.
        let host = e.effective_host();
        if !host.is_empty() {
            let h = hosts.entry(host.clone()).or_default();
            h.host = host;
            h.cost_usd += cost;
            h.tokens += tok;
            h.tokens_in += e.tokens_in;
            h.tokens_out += e.tokens_out;
            h.calls += 1;
            h.billed |= billed;
        }

        rep.total_cost_usd += cost;
        rep.total_tokens += tok;
        rep.total_calls += 1;
        if billed {
            rep.billed_tokens += tok;
        } else {
            rep.free_tokens += tok;
        }
    }

    // Baseline for the avoided-spend / counterfactual headline. The per-call
    // cost already comes from the ledger entry itself (authoritative: LiteLLM
    // header cost / backend usage). By default the baseline is DERIVED from
    // observed spend -- the highest effective $/Mtok among paid models actually
    // used in the window -- so the headline is meaningful even with no pricing
    // catalog. A catalog `baseline_usd_per_mtok` (> 0) overrides the derivation.
    let mut baseline_per_mtok = pricing.baseline_usd_per_mtok;
    let mut baseline_model = pricing.baseline_model.clone();
    if baseline_per_mtok <= 0.0 {
        let mut best_rate = 0.0_f64;
        let mut best_model = String::new();
        for row in models.values() {
            if row.billed && row.tokens > 0 {
                let rate = (row.cost_usd / row.tokens as f64) * 1_000_000.0;
                if rate > best_rate {
                    best_rate = rate;
                    best_model = row.model.clone();
                }
            }
        }
        baseline_per_mtok = best_rate;
        baseline_model = best_model;
    }
    rep.baseline_usd_per_mtok = baseline_per_mtok;
    rep.baseline_model = baseline_model;
    rep.avoided_usd = (rep.free_tokens as f64 / 1_000_000.0) * baseline_per_mtok;
    rep.counterfactual_usd = (rep.total_tokens as f64 / 1_000_000.0) * baseline_per_mtok;
    rep.buckets = buckets.into_values().collect();
    // Attach catalog input/output rates to the paid models so the report can
    // show what each paid call costs per Mtok. Free models keep $0 rates.
    for row in models.values_mut() {
        if row.billed {
            if let Some(price) = pricing.lookup(&row.model) {
                row.input_usd_per_mtok = price.input_usd_per_mtok;
                row.output_usd_per_mtok = price.output_usd_per_mtok;
            }
        }
    }
    let mut by_model: Vec<RowByModel> = models.into_values().collect();
    by_model.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.tokens.cmp(&a.tokens))
    });
    rep.by_model = by_model;
    let mut by_host: Vec<RowByHost> = hosts.into_values().collect();
    by_host.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.tokens.cmp(&a.tokens))
    });
    rep.by_host = by_host;
    Ok(rep)
}
