//! FinOps observability for zoder (mirror of `@openclaw/tokenomics/finops.ts`).
//!
//! Read-only, non-enforcing. Provides allocation, realized-rate calculation,
//! cache-discount savings, cheapest-equivalent-model advisor (report only),
//! and simple burn forecasting over the append-only ledger.
//!
//! All outputs are for human (or external tool) consumption; this module
//! never enforces a budget, never aborts a run, never swaps a model. nvzoder
//! is a sole-developer internal fork, so the surface is observe / attribute /
//! advise / report only.

use crate::config::Theme;
use crate::ledger::{Entry, FinOpsTags, Ledger};
use crate::pricing::{ModelPrice, PricingCatalog};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;

fn saturating_f64_add(a: f64, b: f64) -> f64 {
    let sum = a + b;
    if sum.is_finite() {
        sum
    } else {
        f64::MAX
    }
}
use std::io::IsTerminal;

/// Minimal themed colorizer for FinOps text output. Mirrors the CLI `Pal`:
/// colour is suppressed when stdout is not a TTY or `NO_COLOR` is set, and the
/// SGR codes come from the active org [`Theme`] so `zoder finops` matches
/// `zoder report` branding.
struct Paint {
    on: bool,
    theme: Theme,
}
impl Paint {
    fn new(theme: &Theme) -> Self {
        let forced = std::env::var_os("CLICOLOR_FORCE").is_some();
        let no_color = std::env::var_os("NO_COLOR").is_some();
        Self {
            on: !no_color && (forced || std::io::stdout().is_terminal()),
            theme: theme.clone(),
        }
    }
    fn wrap(&self, s: &str, code: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn header(&self, s: &str) -> String {
        self.wrap(s, &self.theme.header)
    }
    fn accent(&self, s: &str) -> String {
        self.wrap(s, &self.theme.accent)
    }
    fn warn(&self, s: &str) -> String {
        self.wrap(s, &self.theme.warn)
    }
    fn dim(&self, s: &str) -> String {
        self.wrap(s, &self.theme.dim)
    }
}

/// Spend grouped by a single dimension (caller / task / model / provider).
#[derive(Debug, Clone, Serialize)]
pub struct SpendGroup {
    pub key: String,
    pub cost_usd: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub calls: u64,
    pub unknown_cost_tokens: u64,
    pub unknown_cost_calls: u64,
}

/// Effective $/1M tok, computed from actual ledger entries.
#[derive(Debug, Clone, Serialize)]
pub struct ModelRealized {
    pub model: String,
    pub cost_usd: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub tokens: u64,
    pub calls: u64,
    pub unknown_cost_tokens: u64,
    pub unknown_cost_calls: u64,
    /// None if no tokens (serialized as null in JSON).
    pub realized_usd_per_mtok: Option<f64>,
}

/// Cache-discount savings estimate (provider already applied the discount,
/// this just makes it visible).
#[derive(Debug, Clone, Serialize)]
pub struct CacheSavingsRow {
    pub model: String,
    pub calls: u64,
    pub tokens_in: u64,
    pub est_cached_tokens: f64,
    pub est_savings_usd: f64,
    pub input_usd_per_mtok: f64,
    pub cache_read_usd_per_mtok: f64,
}

/// Report-only advisor: what you'd have paid at the cheapest catalog rate.
#[derive(Debug, Clone, Serialize)]
pub struct AdvisorRow {
    pub paid_model: String,
    pub paid_cost_usd: f64,
    pub calls: u64,
    pub tokens: u64,
    pub cheapest_alt_model: String,
    pub cheapest_alt_usd_per_mtok: f64,
    pub cheapest_alt_estimated_cost_usd: f64,
    pub potential_savings_usd: f64,
    pub potential_savings_ratio: f64,
}

/// Simple linear burn forecast (naive; the goal is to see a number).
#[derive(Debug, Clone, Serialize)]
pub struct BurnForecast {
    pub window_days: u32,
    pub avg_daily_cost_usd: f64,
    pub median_daily_cost_usd: f64,
    pub trend_usd_per_day: f64,
    pub forecast_7d_usd: f64,
    pub forecast_30d_usd: f64,
    pub sample_days: usize,
}

/// One-shot report a CLI can hand to a user.
#[derive(Debug, Clone, Serialize)]
pub struct FinOpsReport {
    pub generated: String,
    pub since: String,
    pub until: String,
    pub total_cost_usd: f64,
    pub total_tokens: u64,
    pub total_calls: u64,
    pub unknown_cost_tokens: u64,
    pub unknown_cost_calls: u64,
    pub by_caller: Vec<SpendGroup>,
    pub by_task: Vec<SpendGroup>,
    /// Spend grouped by model publisher host (e.g. `meta`, `anthropic`),
    /// summed across every provider that served that publisher.
    pub by_host: Vec<SpendGroup>,
    pub by_model_realized: Vec<ModelRealized>,
    pub cache_savings: Vec<CacheSavingsRow>,
    pub advisor: Vec<AdvisorRow>,
    pub forecast: BurnForecast,
}

fn effective_rate(p: &ModelPrice) -> f64 {
    let i = p.input_usd_per_mtok;
    let o = p.output_usd_per_mtok;
    if i > 0.0 || o > 0.0 {
        0.7 * i + 0.3 * o
    } else {
        p.usd_per_mtok
    }
}

fn linear_slope(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len();
    if n < 2 {
        return 0.0;
    }
    let mean_x: f64 = xs.iter().sum::<f64>() / n as f64;
    let mean_y: f64 = ys.iter().sum::<f64>() / n as f64;
    let mut num = 0.0;
    let mut den = 0.0;
    for i in 0..n {
        num += (xs[i] - mean_x) * (ys[i] - mean_y);
        den += (xs[i] - mean_x).powi(2);
    }
    if den == 0.0 {
        0.0
    } else {
        num / den
    }
}

fn median(nums: &[f64]) -> f64 {
    if nums.is_empty() {
        return 0.0;
    }
    let mut s = nums.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let m = s.len() / 2;
    if s.len().is_multiple_of(2) {
        (s[m - 1] + s[m]) / 2.0
    } else {
        s[m]
    }
}

fn parse_tags(e: &Entry) -> FinOpsTags {
    e.tags.clone()
}

#[derive(Debug, Clone, Copy)]
pub enum Dimension {
    Caller,
    Task,
    Model,
    Provider,
    /// Model publisher (segment before `/` in the model id), summed across
    /// every provider that served it — the FinOps complement of `Provider`.
    Host,
}

/// Group ledger rows by a tag string field. Absent/empty values land in `__untagged__`.
pub fn spend_by_dimension(
    ledger: &Ledger,
    dim: Dimension,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> anyhow::Result<Vec<SpendGroup>> {
    let entries = ledger.entries_strict()?;
    Ok(spend_by_dimension_from_entries(
        entries_in(&entries, since, until),
        dim,
    ))
}

fn spend_by_dimension_from_entries<'a>(
    entries: impl IntoIterator<Item = &'a Entry>,
    dim: Dimension,
) -> Vec<SpendGroup> {
    let mut acc: BTreeMap<String, SpendGroup> = BTreeMap::new();
    for e in entries {
        let tags = parse_tags(e);
        let key_opt: Option<String> = match dim {
            Dimension::Caller => tags.caller.clone(),
            Dimension::Task => tags.task.clone(),
            Dimension::Model => Some(e.model.clone()),
            Dimension::Provider => Some(e.provider.clone()),
            Dimension::Host => {
                let h = e.effective_host();
                if h.is_empty() {
                    None
                } else {
                    Some(h)
                }
            }
        };
        let key = match key_opt {
            Some(k) if !k.is_empty() => k,
            _ => "__untagged__".to_string(),
        };
        let entry = acc.entry(key.clone()).or_insert(SpendGroup {
            key,
            cost_usd: 0.0,
            tokens_in: 0,
            tokens_out: 0,
            calls: 0,
            unknown_cost_tokens: 0,
            unknown_cost_calls: 0,
        });
        if e.cost_unknown {
            entry.unknown_cost_tokens = entry
                .unknown_cost_tokens
                .saturating_add(e.tokens_in.saturating_add(e.tokens_out));
            entry.unknown_cost_calls = entry.unknown_cost_calls.saturating_add(e.calls);
            continue;
        }
        entry.cost_usd = saturating_f64_add(entry.cost_usd, e.cost_usd);
        entry.tokens_in = entry.tokens_in.saturating_add(e.tokens_in);
        entry.tokens_out = entry.tokens_out.saturating_add(e.tokens_out);
        entry.calls = entry.calls.saturating_add(e.calls);
    }
    let mut v: Vec<SpendGroup> = acc.into_values().collect();
    v.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    v
}

/// Realized $/Mtok per model — what you actually paid per million tokens,
/// not what the catalog says.
pub fn realized_rate_by_model(
    ledger: &Ledger,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> anyhow::Result<Vec<ModelRealized>> {
    let entries = ledger.entries_strict()?;
    Ok(realized_rate_from_entries(entries_in(
        &entries, since, until,
    )))
}

fn realized_rate_from_entries<'a>(
    entries: impl IntoIterator<Item = &'a Entry>,
) -> Vec<ModelRealized> {
    let mut acc: BTreeMap<String, ModelRealized> = BTreeMap::new();
    for e in entries {
        let tot = e.tokens_in.saturating_add(e.tokens_out);
        let r = acc.entry(e.model.clone()).or_insert(ModelRealized {
            model: e.model.clone(),
            cost_usd: 0.0,
            tokens_in: 0,
            tokens_out: 0,
            tokens: 0,
            calls: 0,
            unknown_cost_tokens: 0,
            unknown_cost_calls: 0,
            realized_usd_per_mtok: None,
        });
        if e.cost_unknown {
            r.unknown_cost_tokens = r.unknown_cost_tokens.saturating_add(tot);
            r.unknown_cost_calls = r.unknown_cost_calls.saturating_add(e.calls);
            continue;
        }
        r.cost_usd = saturating_f64_add(r.cost_usd, e.cost_usd);
        r.tokens_in = r.tokens_in.saturating_add(e.tokens_in);
        r.tokens_out = r.tokens_out.saturating_add(e.tokens_out);
        r.tokens = r.tokens.saturating_add(tot);
        r.calls = r.calls.saturating_add(e.calls);
    }
    for r in acc.values_mut() {
        r.realized_usd_per_mtok = if r.tokens > 0 {
            Some((r.cost_usd / r.tokens as f64) * 1_000_000.0)
        } else {
            None
        };
    }
    let mut v: Vec<ModelRealized> = acc.into_values().collect();
    v.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    v
}

pub fn cache_savings_by_model(
    ledger: &Ledger,
    pricing: &PricingCatalog,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> anyhow::Result<Vec<CacheSavingsRow>> {
    let entries = ledger.entries_strict()?;
    Ok(cache_savings_from_entries(
        entries_in(&entries, since, until),
        pricing,
    ))
}

fn cache_savings_from_entries<'a>(
    entries: impl IntoIterator<Item = &'a Entry>,
    pricing: &PricingCatalog,
) -> Vec<CacheSavingsRow> {
    let mut rows: BTreeMap<String, CacheSavingsRow> = BTreeMap::new();
    for e in entries {
        if e.cost_unknown {
            continue;
        }
        let tags = parse_tags(e);
        let hit = tags.cache_hit_ratio.unwrap_or(0.0);
        if hit <= 0.0 {
            continue;
        }
        let p = match pricing.models.get(&e.model) {
            Some(p) => p,
            None => continue,
        };
        let cache_rate = p.cache_read_usd_per_mtok;
        let input_rate = if p.input_usd_per_mtok > 0.0 {
            p.input_usd_per_mtok
        } else {
            p.usd_per_mtok
        };
        if input_rate <= 0.0 || cache_rate >= input_rate {
            continue;
        }
        let cached_tokens = (e.tokens_in as f64) * hit;
        let savings = ((input_rate - cache_rate) * cached_tokens) / 1_000_000.0;
        let r = rows.entry(e.model.clone()).or_insert(CacheSavingsRow {
            model: e.model.clone(),
            calls: 0,
            tokens_in: 0,
            est_cached_tokens: 0.0,
            est_savings_usd: 0.0,
            input_usd_per_mtok: input_rate,
            cache_read_usd_per_mtok: cache_rate,
        });
        r.calls = r.calls.saturating_add(e.calls);
        r.tokens_in = r.tokens_in.saturating_add(e.tokens_in);
        r.est_cached_tokens = saturating_f64_add(r.est_cached_tokens, cached_tokens);
        r.est_savings_usd = saturating_f64_add(r.est_savings_usd, savings);
    }
    let mut v: Vec<CacheSavingsRow> = rows.into_values().collect();
    v.sort_by(|a, b| {
        b.est_savings_usd
            .partial_cmp(&a.est_savings_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    v
}

pub fn cheapest_equivalent_advisor(
    ledger: &Ledger,
    pricing: &PricingCatalog,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> anyhow::Result<Vec<AdvisorRow>> {
    let entries = ledger.entries_strict()?;
    Ok(advisor_from_entries(
        entries_in(&entries, since, until),
        pricing,
    ))
}

fn advisor_from_entries<'a>(
    entries: impl IntoIterator<Item = &'a Entry>,
    pricing: &PricingCatalog,
) -> Vec<AdvisorRow> {
    let mut paid: BTreeMap<String, (f64, u64, u64)> = BTreeMap::new();
    for e in entries {
        if e.cost_unknown || e.cost_usd <= 0.0 {
            continue;
        }
        let r = paid.entry(e.model.clone()).or_insert((0.0, 0, 0));
        r.0 = saturating_f64_add(r.0, e.cost_usd);
        r.1 = r.1.saturating_add(e.calls);
        r.2 = r.2.saturating_add(e.tokens_in.saturating_add(e.tokens_out));
    }
    if paid.is_empty() {
        return Vec::new();
    }
    let mut rates: Vec<(String, f64)> = pricing
        .models
        .iter()
        .filter_map(|(k, p)| {
            let r = effective_rate(p);
            if r > 0.0 {
                Some((k.clone(), r))
            } else {
                None
            }
        })
        .collect();
    rates.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<AdvisorRow> = Vec::new();
    for (model, (cost, calls, tokens)) in &paid {
        let alt = rates.iter().find(|(m, _)| m != model);
        let row = match alt {
            Some((alt_model, alt_rate)) => {
                let alt_cost = (alt_rate * (*tokens as f64)) / 1_000_000.0;
                let savings = (cost - alt_cost).max(0.0);
                AdvisorRow {
                    paid_model: model.clone(),
                    paid_cost_usd: *cost,
                    calls: *calls,
                    tokens: *tokens,
                    cheapest_alt_model: alt_model.clone(),
                    cheapest_alt_usd_per_mtok: *alt_rate,
                    cheapest_alt_estimated_cost_usd: alt_cost,
                    potential_savings_usd: savings,
                    potential_savings_ratio: if *cost > 0.0 { savings / cost } else { 0.0 },
                }
            }
            None => AdvisorRow {
                paid_model: model.clone(),
                paid_cost_usd: *cost,
                calls: *calls,
                tokens: *tokens,
                cheapest_alt_model: "(none — paid model is cheapest in catalog)".to_string(),
                cheapest_alt_usd_per_mtok: 0.0,
                cheapest_alt_estimated_cost_usd: 0.0,
                potential_savings_usd: 0.0,
                potential_savings_ratio: 0.0,
            },
        };
        out.push(row);
    }
    out.sort_by(|a, b| {
        b.potential_savings_usd
            .partial_cmp(&a.potential_savings_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

pub fn forecast_burn(
    ledger: &Ledger,
    window_days: u32,
    until: DateTime<Utc>,
) -> anyhow::Result<BurnForecast> {
    let entries = ledger.entries_strict()?;
    forecast_burn_from_entries(&entries, window_days, until)
}

fn forecast_burn_from_entries(
    entries: &[Entry],
    window_days: u32,
    until: DateTime<Utc>,
) -> anyhow::Result<BurnForecast> {
    let Some(days_before_until) = window_days.checked_sub(1) else {
        return Ok(BurnForecast {
            window_days,
            avg_daily_cost_usd: 0.0,
            median_daily_cost_usd: 0.0,
            trend_usd_per_day: 0.0,
            forecast_7d_usd: 0.0,
            forecast_30d_usd: 0.0,
            sample_days: 0,
        });
    };
    let start_date = until
        .date_naive()
        .checked_sub_days(chrono::Days::new(u64::from(days_before_until)))
        .ok_or_else(|| anyhow::anyhow!("forecast window is outside the supported date range"))?;
    let since = start_date
        .and_hms_opt(0, 0, 0)
        .expect("midnight is always a valid time")
        .and_utc();
    let mut daily = vec![0.0; window_days as usize];
    for e in entries_in(entries, Some(since), Some(until)) {
        if e.cost_unknown {
            continue;
        }
        let offset = e
            .ts_utc
            .date_naive()
            .signed_duration_since(start_date)
            .num_days();
        if let Ok(offset) = usize::try_from(offset) {
            if let Some(total) = daily.get_mut(offset) {
                *total = saturating_f64_add(*total, e.cost_usd);
            }
        }
    }
    let xs: Vec<f64> = (0..window_days).map(f64::from).collect();
    let slope = linear_slope(&xs, &daily);
    let mean_x = xs.iter().sum::<f64>() / xs.len() as f64;
    let mean_y = daily.iter().sum::<f64>() / daily.len() as f64;
    let intercept = mean_y - slope * mean_x;
    let last_x = f64::from(days_before_until);
    let project = |n: f64| (intercept + slope * (last_x + n)).max(0.0);
    Ok(BurnForecast {
        window_days,
        avg_daily_cost_usd: mean_y,
        median_daily_cost_usd: median(&daily),
        trend_usd_per_day: slope,
        forecast_7d_usd: project(7.0),
        forecast_30d_usd: project(30.0),
        sample_days: daily.len(),
    })
}

pub fn build_finops_report(
    ledger: &Ledger,
    pricing: &PricingCatalog,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    window_days: u32,
) -> anyhow::Result<FinOpsReport> {
    // One strict, shared-lock-protected snapshot feeds every section. This
    // prevents concurrent appends from producing internally inconsistent totals.
    let snapshot = ledger.entries_strict()?;
    build_finops_report_from_entries(&snapshot, pricing, since, until, window_days)
}

fn build_finops_report_from_entries(
    snapshot: &[Entry],
    pricing: &PricingCatalog,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    window_days: u32,
) -> anyhow::Result<FinOpsReport> {
    let entries: Vec<&Entry> = entries_in(snapshot, Some(since), Some(until)).collect();
    let mut cost = 0.0;
    let mut tokens = 0u64;
    let mut calls = 0u64;
    let mut unknown_cost_tokens = 0u64;
    let mut unknown_cost_calls = 0u64;
    for e in &entries {
        if e.cost_unknown {
            unknown_cost_tokens =
                unknown_cost_tokens.saturating_add(e.tokens_in.saturating_add(e.tokens_out));
            unknown_cost_calls = unknown_cost_calls.saturating_add(e.calls);
            continue;
        }
        cost = saturating_f64_add(cost, e.cost_usd);
        tokens = tokens.saturating_add(e.tokens_in.saturating_add(e.tokens_out));
        calls = calls.saturating_add(e.calls);
    }
    Ok(FinOpsReport {
        generated: Utc::now().to_rfc3339(),
        since: since.to_rfc3339(),
        until: until.to_rfc3339(),
        total_cost_usd: cost,
        total_tokens: tokens,
        total_calls: calls,
        unknown_cost_tokens,
        unknown_cost_calls,
        by_caller: spend_by_dimension_from_entries(entries.iter().copied(), Dimension::Caller),
        by_task: spend_by_dimension_from_entries(entries.iter().copied(), Dimension::Task),
        by_host: spend_by_dimension_from_entries(entries.iter().copied(), Dimension::Host),
        by_model_realized: realized_rate_from_entries(entries.iter().copied()),
        cache_savings: cache_savings_from_entries(entries.iter().copied(), pricing),
        advisor: advisor_from_entries(entries.iter().copied(), pricing),
        forecast: forecast_burn_from_entries(snapshot, window_days, until)?,
    })
}

fn entries_in(
    entries: &[Entry],
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> impl Iterator<Item = &Entry> {
    entries
        .iter()
        .filter(move |entry| since.is_none_or(|value| entry.ts_utc >= value))
        .filter(move |entry| until.is_none_or(|value| entry.ts_utc <= value))
}

fn fmt_count(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 10_000 {
        format!("{:.0}K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn render_finops_text(rep: &FinOpsReport, p: &Paint) -> String {
    let mut s = String::new();
    let since_short: String = rep.since.chars().take(10).collect();
    let until_short: String = rep.until.chars().take(10).collect();
    s.push_str(&p.header(&format!("FinOps report — {since_short} → {until_short}")));
    s.push('\n');
    s.push_str(&format!(
        "  known-cost calls: {}\n",
        fmt_count(rep.total_calls)
    ));
    s.push_str(&format!("  known-cost tokens: {}\n", rep.total_tokens));
    s.push_str(&format!(
        "  known total cost: {}\n",
        p.warn(&format!("${:.2}", rep.total_cost_usd))
    ));
    if rep.unknown_cost_calls > 0 {
        s.push_str(&format!(
            "  unknown cost:     {} calls / {} tokens (excluded from spend and rates)\n",
            fmt_count(rep.unknown_cost_calls),
            fmt_count(rep.unknown_cost_tokens)
        ));
    }
    s.push('\n');
    if !rep.by_caller.is_empty() {
        s.push_str(&p.dim("  by caller:"));
        s.push('\n');
        for g in rep.by_caller.iter().take(10) {
            s.push_str(&format!(
                "    {:<28} {}  {} calls\n",
                g.key,
                p.warn(&format!("${:>10.2}", g.cost_usd)),
                fmt_count(g.calls)
            ));
        }
        s.push('\n');
    }
    if !rep.by_host.is_empty() {
        s.push_str(&p.dim("  by host (model publisher, across providers):"));
        s.push('\n');
        for g in rep.by_host.iter().take(10) {
            s.push_str(&format!(
                "    {:<28} {}  {} calls\n",
                g.key,
                p.warn(&format!("${:>10.2}", g.cost_usd)),
                fmt_count(g.calls)
            ));
        }
        s.push('\n');
    }
    if !rep.by_model_realized.is_empty() {
        s.push_str(&p.dim("  realized $/Mtok (top models by spend):"));
        s.push('\n');
        for r in rep.by_model_realized.iter().take(10) {
            let rate = match r.realized_usd_per_mtok {
                Some(v) => p.accent(&format!("${v:.2}/Mtok")),
                None => p.dim("n/a"),
            };
            s.push_str(&format!(
                "    {:<48} {}  {} calls  {}\n",
                r.model,
                p.warn(&format!("${:>10.2}", r.cost_usd)),
                fmt_count(r.calls),
                rate
            ));
        }
        s.push('\n');
    }
    let cache_hits: Vec<&CacheSavingsRow> = rep
        .cache_savings
        .iter()
        .filter(|c| c.est_savings_usd > 0.0)
        .collect();
    if !cache_hits.is_empty() {
        s.push_str(&p.dim("  cache-discount savings (estimate):"));
        s.push('\n');
        for c in cache_hits.iter().take(10) {
            s.push_str(&format!(
                "    {:<48} est. saved {} ({:.0} cached tokens)\n",
                c.model,
                p.accent(&format!("${:.2}", c.est_savings_usd)),
                c.est_cached_tokens
            ));
        }
        s.push('\n');
    }
    s.push_str(&p.dim(&format!(
        "  forecast ({}d base, {} sample days):",
        rep.forecast.window_days, rep.forecast.sample_days
    )));
    s.push('\n');
    s.push_str(&format!(
        "    avg daily {}  trend ${:.2}/day  7d {}  30d {}\n",
        p.warn(&format!("${:.2}", rep.forecast.avg_daily_cost_usd)),
        rep.forecast.trend_usd_per_day,
        p.warn(&format!("${:.2}", rep.forecast.forecast_7d_usd)),
        p.warn(&format!("${:.2}", rep.forecast.forecast_30d_usd))
    ));
    s
}

/// CLI subcommand entry. Returns process exit code.
pub fn cli_run(
    ledger: &Ledger,
    pricing: &PricingCatalog,
    theme: &Theme,
    argv: &[String],
) -> anyhow::Result<i32> {
    let sub = argv.get(1).map(|s| s.as_str()).unwrap_or("");
    let since: Option<DateTime<Utc>> = parse_flag(argv, "--since").map(parse_date).transpose()?;
    let until: Option<DateTime<Utc>> = parse_flag(argv, "--until").map(parse_date).transpose()?;
    let window_days: u32 = parse_flag(argv, "--window-days")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(30);
    let json = argv.iter().any(|a| a == "--json");
    let paint = Paint::new(theme);
    let until_dt = until.unwrap_or_else(Utc::now);
    let since_dt = since.unwrap_or_else(|| until_dt - chrono::Duration::days(30));
    if since_dt > until_dt {
        anyhow::bail!("invalid date range: --since must be earlier than or equal to --until");
    }
    if sub == "forecast" {
        let fc = forecast_burn(ledger, window_days, until_dt)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&fc)?);
        } else {
            println!(
                "{}",
                paint.header(&format!(
                    "FinOps forecast ({} sample days over {}-day window)",
                    fc.sample_days, fc.window_days
                ))
            );
            println!("  avg daily cost:    ${:.2}", fc.avg_daily_cost_usd);
            println!("  median daily cost: ${:.2}", fc.median_daily_cost_usd);
            println!("  trend ($/day):     ${:.2}", fc.trend_usd_per_day);
            println!("  forecast +7d:      ${:.2}", fc.forecast_7d_usd);
            println!("  forecast +30d:     ${:.2}", fc.forecast_30d_usd);
        }
        return Ok(0);
    }
    let rep = build_finops_report(ledger, pricing, since_dt, until_dt, window_days)?;
    match sub {
        "advisor" => {
            if rep.advisor.is_empty() {
                println!("No billed spend in window — nothing to advise on.");
                return Ok(0);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&rep.advisor)?);
                return Ok(0);
            }
            let since_short: String = rep.since.chars().take(10).collect();
            let until_short: String = rep.until.chars().take(10).collect();
            println!(
                "{}",
                paint.header(
                    "FinOps advisor — paid-model alternatives (report only, never enforced)"
                )
            );
            println!(
                "  window: {} → {} ({} calls, ${:.2} total)",
                since_short, until_short, rep.total_calls, rep.total_cost_usd
            );
            for row in &rep.advisor {
                if row.potential_savings_usd <= 0.0 {
                    continue;
                }
                println!(
                    "  {}: paid ${:.2} across {} calls; cheapest alt {} @ ${:.2}/Mtok would have cost ${:.2} (save ${:.2}, {:.1}%).",
                    row.paid_model,
                    row.paid_cost_usd,
                    row.calls,
                    row.cheapest_alt_model,
                    row.cheapest_alt_usd_per_mtok,
                    row.cheapest_alt_estimated_cost_usd,
                    row.potential_savings_usd,
                    row.potential_savings_ratio * 100.0
                );
            }
            Ok(0)
        }
        "report" => {
            if json {
                println!("{}", serde_json::to_string_pretty(&rep)?);
            } else {
                println!("{}", render_finops_text(&rep, &paint));
            }
            Ok(0)
        }
        _ => {
            eprintln!(
                "finops: unknown subcommand {:?} (expected: report | advisor | forecast)",
                sub
            );
            Ok(1)
        }
    }
}

fn parse_flag(argv: &[String], key: &str) -> Option<String> {
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if a == key {
            return it.next().cloned();
        }
        if let Some(rest) = a.strip_prefix(&format!("{}=", key)) {
            return Some(rest.to_string());
        }
    }
    None
}

fn parse_date(s: String) -> anyhow::Result<DateTime<Utc>> {
    if let Ok(d) = DateTime::parse_from_rfc3339(&s) {
        return Ok(d.with_timezone(&Utc));
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
        return Ok(DateTime::<Utc>::from_naive_utc_and_offset(
            d.and_hms_opt(0, 0, 0)
                .expect("midnight is always a valid time"),
            Utc,
        ));
    }
    anyhow::bail!("invalid date {s:?}: expected YYYY-MM-DD or RFC 3339")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn rec(led: &Ledger, ts: &str, provider: &str, model: &str, cost: f64, tin: u64, tout: u64) {
        let ts_utc = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S")
            .unwrap()
            .and_utc();
        led.record(&Entry {
            ts_utc,
            provider: provider.into(),
            model: model.into(),
            host: model
                .split_once('/')
                .map(|(h, _)| h.to_string())
                .unwrap_or_default(),
            tokens_in: tin,
            tokens_out: tout,
            cost_usd: cost,
            cost_unknown: false,
            calls: 1,
            violation: None,
            tags: crate::ledger::FinOpsTags::default(),
        })
        .unwrap();
    }

    #[test]
    fn host_dimension_sums_publisher_across_providers() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        rec(
            &led,
            "2026-06-10 10:00:00",
            "openrouter",
            "meta/llama-3.3-70b",
            0.02,
            1000,
            500,
        );
        rec(
            &led,
            "2026-06-11 10:00:00",
            "enterprise-gw",
            "meta/llama-3.3-70b",
            0.0,
            2000,
            800,
        );
        rec(
            &led,
            "2026-06-12 10:00:00",
            "openrouter",
            "anthropic/claude-3.5",
            0.05,
            3000,
            900,
        );
        // Un-prefixed id → no publisher host → lands in __untagged__.
        rec(
            &led,
            "2026-06-13 10:00:00",
            "openai",
            "gpt-4o",
            0.10,
            500,
            200,
        );

        let since = chrono::NaiveDate::from_ymd_opt(2026, 6, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let until = chrono::NaiveDate::from_ymd_opt(2026, 6, 30)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap()
            .and_utc();

        let groups = spend_by_dimension(&led, Dimension::Host, Some(since), Some(until)).unwrap();
        let meta = groups
            .iter()
            .find(|g| g.key == "meta")
            .expect("meta host group present");
        assert_eq!(meta.calls, 2, "meta summed across both providers");
        assert!((meta.cost_usd - 0.02).abs() < 1e-9);
        assert!(groups.iter().any(|g| g.key == "anthropic"));
        assert!(
            groups.iter().any(|g| g.key == "__untagged__"),
            "un-prefixed model id has no publisher host"
        );

        // And it surfaces in the full report's by_host allocation.
        let pricing = PricingCatalog::default();
        let rep = build_finops_report(&led, &pricing, since, until, 30).unwrap();
        assert!(rep.by_host.iter().any(|g| g.key == "meta" && g.calls == 2));
    }

    #[test]
    fn finops_dimensions_read_nested_entry_tags() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        let mut entry = Entry {
            ts_utc: Utc::now(),
            provider: "p".into(),
            model: "vendor/model".into(),
            host: "vendor".into(),
            tokens_in: 100,
            tokens_out: 20,
            cost_usd: 0.5,
            cost_unknown: false,
            calls: 1,
            violation: None,
            tags: FinOpsTags::default(),
        };
        entry.tags.caller = Some("ci".into());
        entry.tags.task = Some("review".into());
        entry.tags.cache_hit_ratio = Some(0.5);
        led.record(&entry).unwrap();

        let callers = spend_by_dimension(&led, Dimension::Caller, None, None).unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].key, "ci");
        let tasks = spend_by_dimension(&led, Dimension::Task, None, None).unwrap();
        assert_eq!(tasks[0].key, "review");
        assert_eq!(parse_tags(&entry).cache_hit_ratio, Some(0.5));
    }

    #[test]
    fn strict_reporting_refuses_malformed_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let led = Ledger::new(&path);
        rec(&led, "2026-06-10 10:00:00", "p", "m", 1.0, 10, 5);
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(b"{\"truncated\":\n").unwrap();
        let error = spend_by_dimension(&led, Dimension::Model, None, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("malformed"), "{error}");
    }

    #[test]
    fn report_sections_derive_from_one_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        rec(&led, "2026-06-10 10:00:00", "p", "m", 1.0, 10, 5);
        let snapshot = led.entries_strict().unwrap();
        rec(&led, "2026-06-11 10:00:00", "p", "m", 2.0, 20, 10);
        let since = chrono::NaiveDate::from_ymd_opt(2026, 6, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let until = chrono::NaiveDate::from_ymd_opt(2026, 6, 30)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap()
            .and_utc();
        let report = build_finops_report_from_entries(
            &snapshot,
            &PricingCatalog::default(),
            since,
            until,
            30,
        )
        .unwrap();
        assert_eq!(report.total_cost_usd, 1.0);
        assert_eq!(report.by_model_realized[0].cost_usd, report.total_cost_usd);
        assert_eq!(report.by_model_realized[0].calls, report.total_calls);
    }

    #[test]
    fn finops_segregates_unknown_cost_from_totals_and_realized_rate() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        rec(
            &led,
            "2026-06-10 10:00:00",
            "p",
            "known/model",
            1.0,
            750,
            250,
        );
        let unknown = Entry {
            ts_utc: chrono::NaiveDateTime::parse_from_str(
                "2026-06-11 10:00:00",
                "%Y-%m-%d %H:%M:%S",
            )
            .unwrap()
            .and_utc(),
            provider: "p".into(),
            model: "unknown/model".into(),
            host: "unknown".into(),
            tokens_in: 1_500,
            tokens_out: 500,
            cost_usd: 0.0,
            cost_unknown: true,
            calls: 1,
            violation: None,
            tags: crate::ledger::FinOpsTags::default(),
        };
        led.record(&unknown).unwrap();
        let since = chrono::NaiveDate::from_ymd_opt(2026, 6, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let until = chrono::NaiveDate::from_ymd_opt(2026, 7, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let report =
            build_finops_report(&led, &PricingCatalog::default(), since, until, 30).unwrap();
        assert_eq!(report.total_cost_usd, 1.0);
        assert_eq!(report.total_tokens, 1_000);
        assert_eq!(report.total_calls, 1);
        assert_eq!(report.unknown_cost_tokens, 2_000);
        assert_eq!(report.unknown_cost_calls, 1);
        let unknown_rate = report
            .by_model_realized
            .iter()
            .find(|row| row.model == "unknown/model")
            .unwrap();
        assert_eq!(unknown_rate.realized_usd_per_mtok, None);
        assert_eq!(unknown_rate.unknown_cost_tokens, 2_000);
        let known_rate = report
            .by_model_realized
            .iter()
            .find(|row| row.model == "known/model")
            .unwrap();
        assert_eq!(known_rate.realized_usd_per_mtok, Some(1_000.0));
        let rendered = render_finops_text(&report, &Paint::new(&Theme::default()));
        assert!(rendered.contains("unknown cost:"));
        assert!(rendered.contains("excluded from spend and rates"));
    }

    #[test]
    fn forecast_includes_zero_cost_days_across_the_full_window() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        rec(
            &led,
            "2026-06-15 10:00:00",
            "p",
            "vendor/model",
            30.0,
            100,
            10,
        );
        let until = chrono::NaiveDate::from_ymd_opt(2026, 6, 30)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap()
            .and_utc();

        let forecast = forecast_burn(&led, 30, until).unwrap();

        assert_eq!(forecast.sample_days, 30);
        assert!((forecast.avg_daily_cost_usd - 1.0).abs() < 1e-9);
        assert_eq!(forecast.median_daily_cost_usd, 0.0);
    }

    #[test]
    fn forecast_projects_from_the_fitted_regression_intercept() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        for (day, cost) in [(1, 3.0), (2, 5.0), (3, 7.0), (4, 9.0)] {
            rec(
                &led,
                &format!("2026-06-{day:02} 10:00:00"),
                "p",
                "vendor/model",
                cost,
                100,
                10,
            );
        }
        let until = chrono::NaiveDate::from_ymd_opt(2026, 6, 4)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap()
            .and_utc();

        let forecast = forecast_burn(&led, 4, until).unwrap();

        assert!((forecast.trend_usd_per_day - 2.0).abs() < 1e-9);
        assert!((forecast.forecast_7d_usd - 23.0).abs() < 1e-9);
        assert!((forecast.forecast_30d_usd - 69.0).abs() < 1e-9);
    }

    #[test]
    fn forecast_trend_preserves_gaps_between_activity_dates() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        rec(
            &led,
            "2026-06-01 10:00:00",
            "p",
            "vendor/model",
            3.0,
            100,
            10,
        );
        rec(
            &led,
            "2026-06-04 10:00:00",
            "p",
            "vendor/model",
            9.0,
            100,
            10,
        );
        let until = chrono::NaiveDate::from_ymd_opt(2026, 6, 4)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap()
            .and_utc();

        let forecast = forecast_burn(&led, 4, until).unwrap();

        // The regression is over [3, 0, 0, 9], not the compressed [3, 9].
        assert!((forecast.trend_usd_per_day - 1.8).abs() < 1e-9);
        assert!((forecast.forecast_7d_usd - 18.3).abs() < 1e-9);
    }

    #[test]
    fn cli_rejects_invalid_dates_and_reversed_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        let pricing = PricingCatalog::default();
        let theme = Theme::default();

        let invalid = ["finops", "report", "--since", "2026-99-99"].map(str::to_string);
        let error = cli_run(&led, &pricing, &theme, &invalid).unwrap_err();
        assert!(error.to_string().contains("invalid date"));

        let reversed = [
            "finops",
            "report",
            "--since",
            "2026-07-02",
            "--until",
            "2026-07-01",
        ]
        .map(str::to_string);
        let error = cli_run(&led, &pricing, &theme, &reversed).unwrap_err();
        assert!(error.to_string().contains("--since must be earlier"));
    }

    #[test]
    fn advisor_counts_underlying_calls_in_compacted_rows() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        let mut compacted = Entry {
            ts_utc: Utc::now(),
            provider: "p".into(),
            model: "paid/model".into(),
            host: "paid".into(),
            tokens_in: 700,
            tokens_out: 300,
            cost_usd: 1.0,
            cost_unknown: false,
            calls: 7,
            violation: None,
            tags: FinOpsTags::default(),
        };
        led.record(&compacted).unwrap();
        compacted.calls = u64::MAX;
        led.record(&compacted).unwrap();

        let mut pricing = PricingCatalog::default();
        pricing.models.insert(
            "cheap/model".into(),
            ModelPrice {
                usd_per_mtok: 1.0,
                ..Default::default()
            },
        );
        let rows = cheapest_equivalent_advisor(&led, &pricing, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].calls, u64::MAX);
    }
}
