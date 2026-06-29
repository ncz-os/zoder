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
use crate::ledger::{Entry, Ledger};
use crate::pricing::{ModelPrice, PricingCatalog};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;
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

/// Optional FinOps tags attached to a ledger entry at ingestion time.
/// Mirrors the TypeScript `FinOpsTags` interface (snake_case wire format).
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
pub struct FinOpsTags {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_ratio: Option<f64>,
}

/// Spend grouped by a single dimension (caller / task / model / provider).
#[derive(Debug, Clone, Serialize)]
pub struct SpendGroup {
    pub key: String,
    pub cost_usd: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub calls: u64,
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

fn day_key(ts: &DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d").to_string()
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
    if s.len() % 2 == 0 {
        (s[m - 1] + s[m]) / 2.0
    } else {
        s[m]
    }
}

fn parse_tags(e: &Entry) -> FinOpsTags {
    serde_json::from_value(serde_json::to_value(e).unwrap_or(serde_json::Value::Null))
        .unwrap_or_default()
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
    let entries = ledger.entries_in(since, until)?;
    let mut acc: BTreeMap<String, SpendGroup> = BTreeMap::new();
    for e in entries {
        let tags = parse_tags(&e);
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
        });
        entry.cost_usd += e.cost_usd;
        entry.tokens_in += e.tokens_in;
        entry.tokens_out += e.tokens_out;
        entry.calls += e.calls;
    }
    let mut v: Vec<SpendGroup> = acc.into_values().collect();
    v.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(v)
}

/// Realized $/Mtok per model — what you actually paid per million tokens,
/// not what the catalog says.
pub fn realized_rate_by_model(
    ledger: &Ledger,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> anyhow::Result<Vec<ModelRealized>> {
    let entries = ledger.entries_in(since, until)?;
    let mut acc: BTreeMap<String, ModelRealized> = BTreeMap::new();
    for e in entries {
        let tot = e.tokens_in + e.tokens_out;
        let r = acc.entry(e.model.clone()).or_insert(ModelRealized {
            model: e.model.clone(),
            cost_usd: 0.0,
            tokens_in: 0,
            tokens_out: 0,
            tokens: 0,
            calls: 0,
            realized_usd_per_mtok: None,
        });
        r.cost_usd += e.cost_usd;
        r.tokens_in += e.tokens_in;
        r.tokens_out += e.tokens_out;
        r.tokens += tot;
        r.calls += e.calls;
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
    Ok(v)
}

pub fn cache_savings_by_model(
    ledger: &Ledger,
    pricing: &PricingCatalog,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> anyhow::Result<Vec<CacheSavingsRow>> {
    let entries = ledger.entries_in(since, until)?;
    let mut rows: BTreeMap<String, CacheSavingsRow> = BTreeMap::new();
    for e in entries {
        let tags = parse_tags(&e);
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
        r.calls += e.calls;
        r.tokens_in += e.tokens_in;
        r.est_cached_tokens += cached_tokens;
        r.est_savings_usd += savings;
    }
    let mut v: Vec<CacheSavingsRow> = rows.into_values().collect();
    v.sort_by(|a, b| {
        b.est_savings_usd
            .partial_cmp(&a.est_savings_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(v)
}

pub fn cheapest_equivalent_advisor(
    ledger: &Ledger,
    pricing: &PricingCatalog,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> anyhow::Result<Vec<AdvisorRow>> {
    let entries = ledger.entries_in(since, until)?;
    let mut paid: BTreeMap<String, (f64, u64, u64)> = BTreeMap::new();
    for e in entries {
        if e.cost_usd <= 0.0 {
            continue;
        }
        let r = paid.entry(e.model.clone()).or_insert((0.0, 0, 0));
        r.0 += e.cost_usd;
        r.1 += 1;
        r.2 += e.tokens_in + e.tokens_out;
    }
    if paid.is_empty() {
        return Ok(Vec::new());
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
    Ok(out)
}

pub fn forecast_burn(
    ledger: &Ledger,
    window_days: u32,
    until: DateTime<Utc>,
) -> anyhow::Result<BurnForecast> {
    let since = until - chrono::Duration::days(window_days as i64);
    let entries = ledger.entries_in(Some(since), Some(until))?;
    let mut daily: BTreeMap<String, f64> = BTreeMap::new();
    for e in entries {
        let k = day_key(&e.ts_utc);
        *daily.entry(k).or_insert(0.0) += e.cost_usd;
    }
    let mut keys: Vec<String> = daily.keys().cloned().collect();
    keys.sort();
    let ys: Vec<f64> = keys.iter().map(|k| daily[k]).collect();
    let xs: Vec<f64> = (0..keys.len()).map(|i| i as f64).collect();
    let slope = linear_slope(&xs, &ys);
    let mean_y = if ys.is_empty() {
        0.0
    } else {
        ys.iter().sum::<f64>() / ys.len() as f64
    };
    let last_x = if xs.is_empty() {
        0.0
    } else {
        *xs.last().unwrap()
    };
    let project = |n: f64| (mean_y + slope * (last_x + n)).max(0.0);
    Ok(BurnForecast {
        window_days,
        avg_daily_cost_usd: mean_y,
        median_daily_cost_usd: median(&ys),
        trend_usd_per_day: slope,
        forecast_7d_usd: project(7.0),
        forecast_30d_usd: project(30.0),
        sample_days: ys.len(),
    })
}

pub fn build_finops_report(
    ledger: &Ledger,
    pricing: &PricingCatalog,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    window_days: u32,
) -> anyhow::Result<FinOpsReport> {
    let entries = ledger.entries_in(Some(since), Some(until))?;
    let mut cost = 0.0;
    let mut tokens = 0u64;
    let mut calls = 0u64;
    for e in entries {
        cost += e.cost_usd;
        tokens += e.tokens_in + e.tokens_out;
        calls += e.calls;
    }
    Ok(FinOpsReport {
        generated: Utc::now().to_rfc3339(),
        since: since.to_rfc3339(),
        until: until.to_rfc3339(),
        total_cost_usd: cost,
        total_tokens: tokens,
        total_calls: calls,
        by_caller: spend_by_dimension(ledger, Dimension::Caller, Some(since), Some(until))?,
        by_task: spend_by_dimension(ledger, Dimension::Task, Some(since), Some(until))?,
        by_host: spend_by_dimension(ledger, Dimension::Host, Some(since), Some(until))?,
        by_model_realized: realized_rate_by_model(ledger, Some(since), Some(until))?,
        cache_savings: cache_savings_by_model(ledger, pricing, Some(since), Some(until))?,
        advisor: cheapest_equivalent_advisor(ledger, pricing, Some(since), Some(until))?,
        forecast: forecast_burn(ledger, window_days, until)?,
    })
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
        "  total calls:     {}\n",
        fmt_count(rep.total_calls)
    ));
    s.push_str(&format!("  total tokens:    {}\n", rep.total_tokens));
    s.push_str(&format!(
        "  total cost:      {}\n",
        p.warn(&format!("${:.2}", rep.total_cost_usd))
    ));
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
    let since: Option<DateTime<Utc>> = parse_flag(argv, "--since").map(parse_date);
    let until: Option<DateTime<Utc>> = parse_flag(argv, "--until").map(parse_date);
    let window_days: u32 = parse_flag(argv, "--window-days")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(30);
    let json = argv.iter().any(|a| a == "--json");
    let paint = Paint::new(theme);
    let until_dt = until.unwrap_or_else(Utc::now);
    let since_dt = since.unwrap_or_else(|| until_dt - chrono::Duration::days(30));
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

fn parse_date(s: String) -> DateTime<Utc> {
    if let Ok(d) = DateTime::parse_from_rfc3339(&s) {
        return d.with_timezone(&Utc);
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
        return DateTime::<Utc>::from_naive_utc_and_offset(d.and_hms_opt(0, 0, 0).unwrap(), Utc);
    }
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            calls: 1,
            violation: None,
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
}
