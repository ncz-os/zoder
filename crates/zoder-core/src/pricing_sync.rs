//! Pricing-catalog sync from public model price lists.
//!
//! There is no universal real-time "cost API" across providers: every provider
//! returns token *usage* in the response, but billed-dollar figures only come
//! from per-provider, lagging reconciliation APIs (see `reconcile`). The
//! practical source of truth for `$/token` rates is therefore a community-
//! maintained catalog, priced locally from the usage the call already reports.
//!
//! This module builds a [`PricingCatalog`] from two public sources:
//!   - LiteLLM `model_prices_and_context_window.json` (primary; ~2.5k models),
//!   - OpenRouter `GET /api/v1/models` (secondary; normalized cross-provider).
//!
//! Rates are stored as USD per 1M tokens. The sync is network-tolerant: a
//! failed source is recorded and skipped, so a partial or offline refresh
//! never destroys the existing catalog.

use crate::pricing::{ModelPrice, OffPeak, Peak, PricingCatalog, TimeWindow};
use std::collections::HashMap;
use std::time::Duration;

const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/models";

/// Public price-list source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    LiteLlm,
    OpenRouter,
}

impl Source {
    /// Parse a `--source` value: `litellm`, `openrouter`/`or`, or `both`.
    pub fn parse_list(s: &str) -> Vec<Source> {
        match s.to_ascii_lowercase().as_str() {
            "litellm" | "ll" => vec![Source::LiteLlm],
            "openrouter" | "or" => vec![Source::OpenRouter],
            _ => vec![Source::LiteLlm, Source::OpenRouter],
        }
    }
}

/// Per-token → per-Mtok scale (public lists quote `$ / token`).
const PER_TOK_TO_MTOK: f64 = 1_000_000.0;

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("zoder/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(45))
        .build()
        .unwrap_or_default()
}

async fn fetch(c: &reqwest::Client, url: &str) -> anyhow::Result<String> {
    let resp = c.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("{url}: HTTP {}", resp.status());
    }
    Ok(resp.text().await?)
}

/// LiteLLM stores numeric `$ / token`.
fn num(v: &serde_json::Value, key: &str) -> Option<f64> {
    v.get(key)
        .and_then(|x| x.as_f64())
        .filter(|n| n.is_finite() && *n >= 0.0)
}

/// OpenRouter stores `$ / token` as strings (to avoid float drift).
fn strnum(v: &serde_json::Value, key: &str) -> Option<f64> {
    let parsed = match v.get(key) {
        Some(serde_json::Value::String(s)) => s.parse().ok(),
        Some(serde_json::Value::Number(n)) => n.as_f64(),
        _ => None,
    };
    parsed.filter(|n: &f64| n.is_finite() && *n >= 0.0)
}

fn merge_litellm(models: &mut HashMap<String, ModelPrice>, body: &str) -> anyhow::Result<usize> {
    let root: serde_json::Value = serde_json::from_str(body)?;
    let obj = root
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("litellm: top-level is not an object"))?;
    let mut n = 0;
    for (id, m) in obj {
        if id == "sample_spec" || !m.is_object() {
            continue;
        }
        let input = num(m, "input_cost_per_token");
        let output = num(m, "output_cost_per_token");
        // Both primary rates must actually be published. A missing output
        // component cannot honestly be treated as $0 for output-only usage.
        if input.is_none() || output.is_none() {
            continue;
        }
        let price = ModelPrice {
            peak: None,
            usd_per_mtok: 0.0,
            input_usd_per_mtok: input.unwrap_or(0.0) * PER_TOK_TO_MTOK,
            output_usd_per_mtok: output.unwrap_or(0.0) * PER_TOK_TO_MTOK,
            cache_read_usd_per_mtok: num(m, "cache_read_input_token_cost").unwrap_or(0.0)
                * PER_TOK_TO_MTOK,
            cache_write_usd_per_mtok: num(m, "cache_creation_input_token_cost").unwrap_or(0.0)
                * PER_TOK_TO_MTOK,
            reasoning_usd_per_mtok: num(m, "output_cost_per_reasoning_token").unwrap_or(0.0)
                * PER_TOK_TO_MTOK,
            source: "litellm".into(),
            off_peak: None,
        };
        models.insert(id.to_ascii_lowercase(), price);
        n += 1;
    }
    Ok(n)
}

fn merge_openrouter(
    models: &mut HashMap<String, ModelPrice>,
    body: &str,
    overwrite: bool,
) -> anyhow::Result<usize> {
    let root: serde_json::Value = serde_json::from_str(body)?;
    let data = root
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow::anyhow!("openrouter: missing data[]"))?;
    let mut n = 0;
    for m in data {
        let Some(id) = m.get("id").and_then(|x| x.as_str()) else {
            continue;
        };
        let Some(pricing) = m.get("pricing") else {
            continue;
        };
        let key = id.to_ascii_lowercase();
        if !overwrite && models.contains_key(&key) {
            continue;
        }
        let input = strnum(pricing, "prompt");
        let output = strnum(pricing, "completion");
        if input.is_none() || output.is_none() {
            continue;
        }
        let price = ModelPrice {
            peak: None,
            usd_per_mtok: 0.0,
            input_usd_per_mtok: input.unwrap_or(0.0) * PER_TOK_TO_MTOK,
            output_usd_per_mtok: output.unwrap_or(0.0) * PER_TOK_TO_MTOK,
            cache_read_usd_per_mtok: strnum(pricing, "input_cache_read").unwrap_or(0.0)
                * PER_TOK_TO_MTOK,
            cache_write_usd_per_mtok: strnum(pricing, "input_cache_write").unwrap_or(0.0)
                * PER_TOK_TO_MTOK,
            reasoning_usd_per_mtok: strnum(pricing, "internal_reasoning").unwrap_or(0.0)
                * PER_TOK_TO_MTOK,
            source: "openrouter".into(),
            off_peak: None,
        };
        models.insert(key, price);
        n += 1;
    }
    Ok(n)
}

/// What a sync run pulled in, including any per-source failures.
#[derive(Debug, Default)]
pub struct SyncStats {
    pub litellm: usize,
    pub openrouter: usize,
    pub total: usize,
    pub errors: Vec<String>,
}

/// Coding-task token mix used to collapse input/output rates into the single
/// `baseline_usd_per_mtok` the avoided-spend headline consumes.
fn blended(p: &ModelPrice) -> f64 {
    if p.input_usd_per_mtok > 0.0 || p.output_usd_per_mtok > 0.0 {
        0.3 * p.input_usd_per_mtok + 0.7 * p.output_usd_per_mtok
    } else {
        p.usd_per_mtok
    }
}

/// Build a catalog from the given sources (LiteLLM primary; OpenRouter fills
/// gaps without overwriting). `baseline_model`, if found, sets the avoided-
/// spend baseline used by the report counterfactual.
/// The curated time-of-day pricing overlay, embedded in the binary so the
/// runtime catalog carries the same windows the shipped price list does.
const PEAK_PRICING_JSON: &str = include_str!("../../../pricing/peak-pricing.json");

/// One overlay entry: the models it matches, the window it covers and the
/// multipliers it applies.
#[derive(Debug, Clone, serde::Deserialize)]
struct PeakOverlayEntry {
    #[serde(rename = "match", default)]
    match_: Vec<String>,
    /// "peak" for a busy-hours premium, anything else for an off-peak
    /// discount.
    #[serde(default)]
    kind: String,
    #[serde(default)]
    window_utc: Vec<String>,
    #[serde(default)]
    weekdays_only: bool,
    #[serde(default)]
    input_mult: Option<f64>,
    #[serde(default)]
    output_mult: Option<f64>,
    #[serde(default)]
    base_mult: Option<f64>,
    #[serde(default)]
    window_mult: Option<f64>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct PeakOverlay {
    #[serde(default)]
    windows: Vec<PeakOverlayEntry>,
}

/// True when the overlay entry matches the model id.
fn overlay_matches(entry: &PeakOverlayEntry, model_id: &str) -> bool {
    let low = model_id.to_ascii_lowercase();
    entry
        .match_
        .iter()
        .any(|sub| low.contains(&sub.to_ascii_lowercase()))
}

/// "HH:MM" UTC to minutes since midnight.
fn hhmm_to_min(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h > 24 || m > 59 {
        return None;
    }
    Some((h * 60 + m) % 1440)
}

/// Apply the embedded time-of-day overlay to the freshly synced models.
///
/// The off-peak entry discounts the base rates inside its window. The peak
/// entries rescale the STANDARD rates by `base_mult` and bill the premium
/// rates inside their windows; every matching peak entry contributes a
/// window, so a provider with two daily peak windows gets both. The cache
/// rate scales with the input rate in both directions: a provider charges
/// the same fraction of the miss rate for a hit.
fn apply_peak_overlay(models: &mut HashMap<String, ModelPrice>) {
    let overlay: PeakOverlay = match serde_json::from_str(PEAK_PRICING_JSON) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("zoder: warning: the time-of-day price overlay could not be read ({e})");
            return;
        }
    };
    if overlay.windows.is_empty() {
        return;
    }
    for (id, price) in models.iter_mut() {
        if let Some(off) = overlay
            .windows
            .iter()
            .find(|w| w.kind != "peak" && overlay_matches(w, id))
        {
            let im = off.input_mult.unwrap_or(1.0);
            let om = off.output_mult.unwrap_or(1.0);
            if let (Some(start), Some(end)) = (
                off.window_utc.first().and_then(|s| hhmm_to_min(s)),
                off.window_utc.get(1).and_then(|s| hhmm_to_min(s)),
            ) {
                price.off_peak = Some(OffPeak {
                    input_usd_per_mtok: price.input_usd_per_mtok * im,
                    output_usd_per_mtok: price.output_usd_per_mtok * om,
                    cache_read_usd_per_mtok: price.cache_read_usd_per_mtok * im,
                    window_start_utc_min: start,
                    window_end_utc_min: end,
                });
            }
        }
        let peaks: Vec<&PeakOverlayEntry> = overlay
            .windows
            .iter()
            .filter(|w| w.kind == "peak" && overlay_matches(w, id))
            .collect();
        if peaks.is_empty() {
            continue;
        }
        let base_mult = peaks[0].base_mult.unwrap_or(1.0);
        let window_mult = peaks[0].window_mult.unwrap_or(1.0);
        let mut windows = Vec::new();
        for w in &peaks {
            let (Some(start), Some(end)) = (
                w.window_utc.first().and_then(|s| hhmm_to_min(s)),
                w.window_utc.get(1).and_then(|s| hhmm_to_min(s)),
            ) else {
                continue;
            };
            windows.push(TimeWindow {
                window_start_utc_min: start,
                window_end_utc_min: end,
                weekdays_only: w.weekdays_only,
            });
        }
        if windows.is_empty() {
            continue;
        }
        price.peak = Some(Peak {
            input_usd_per_mtok: price.input_usd_per_mtok * window_mult,
            output_usd_per_mtok: price.output_usd_per_mtok * window_mult,
            cache_read_usd_per_mtok: price.cache_read_usd_per_mtok * window_mult,
            windows,
        });
        // The standard rates are the base scaled by the off-peak factor.
        price.input_usd_per_mtok *= base_mult;
        price.output_usd_per_mtok *= base_mult;
        price.cache_read_usd_per_mtok *= base_mult;
    }
}

pub async fn sync_catalog(
    sources: &[Source],
    baseline_model: Option<&str>,
) -> anyhow::Result<(PricingCatalog, SyncStats)> {
    let c = client();
    let mut models: HashMap<String, ModelPrice> = HashMap::new();
    let mut stats = SyncStats::default();

    if sources.contains(&Source::LiteLlm) {
        match fetch(&c, LITELLM_URL).await {
            Ok(body) => match merge_litellm(&mut models, &body) {
                Ok(n) => stats.litellm = n,
                Err(e) => stats.errors.push(format!("litellm: {e}")),
            },
            Err(e) => stats.errors.push(format!("litellm: {e}")),
        }
    }
    if sources.contains(&Source::OpenRouter) {
        match fetch(&c, OPENROUTER_URL).await {
            Ok(body) => match merge_openrouter(&mut models, &body, false) {
                Ok(n) => stats.openrouter = n,
                Err(e) => stats.errors.push(format!("openrouter: {e}")),
            },
            Err(e) => stats.errors.push(format!("openrouter: {e}")),
        }
    }

    if models.is_empty() {
        anyhow::bail!(
            "no pricing fetched from any source ({})",
            stats.errors.join("; ")
        );
    }

    // The curated time-of-day overlay: the feeds publish one rate, but a
    // provider can bill a discount in an off-peak window and a premium in a
    // busy one.
    apply_peak_overlay(&mut models);

    let mut cat = PricingCatalog {
        generated: chrono::Utc::now().to_rfc3339(),
        window: "live".into(),
        models,
        ..Default::default()
    };
    if let Some(bm) = baseline_model {
        if let Some(p) = cat.lookup(bm) {
            cat.baseline_usd_per_mtok = blended(p);
            cat.baseline_model = bm.to_string();
        }
    }
    stats.total = cat.models.len();
    Ok((cat, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_skips_missing_or_malformed_primary_rates_but_keeps_explicit_zero() {
        let body = r#"{
            "missing": {"mode": "chat"},
            "partial": {"input_cost_per_token": 0.000001},
            "bad": {"input_cost_per_token": -1, "output_cost_per_token": 0.000002},
            "free": {"input_cost_per_token": 0, "output_cost_per_token": 0}
        }"#;
        let mut models = HashMap::new();
        assert_eq!(merge_litellm(&mut models, body).unwrap(), 1);
        assert_eq!(models.len(), 1);
        assert!(models.contains_key("free"));
    }

    use chrono::TimeZone;

    /// The runtime overlay and the shipped price list must agree: both are
    /// built from the same curated windows, so a model priced by one is
    /// priced identically by the other. A drift here would mean the shipped
    /// fallback prices a call differently from a fresh sync.
    #[test]
    fn the_runtime_overlay_and_the_shipped_price_list_agree() {
        let shipped: crate::pricing::PricingCatalog =
            serde_json::from_str(crate::pricing::bundled_catalog_json())
                .expect("the shipped price list parses");
        let price = shipped
            .lookup("deepseek-flash")
            .expect("the shipped price list knows the model");
        // The runtime path starts from the public feed's rate and applies
        // the curated overlay to it.
        let mut models: HashMap<String, ModelPrice> = HashMap::new();
        models.insert(
            "deepseek-flash".to_string(),
            ModelPrice {
                usd_per_mtok: 0.0,
                input_usd_per_mtok: 0.30,
                output_usd_per_mtok: 1.20,
                cache_read_usd_per_mtok: 0.006,
                ..Default::default()
            },
        );
        apply_peak_overlay(&mut models);
        let runtime = models.get("deepseek-flash").expect("the model survives");
        assert_eq!(runtime.input_usd_per_mtok, price.input_usd_per_mtok);
        assert_eq!(runtime.output_usd_per_mtok, price.output_usd_per_mtok);
        assert_eq!(
            runtime.cache_read_usd_per_mtok,
            price.cache_read_usd_per_mtok
        );
        let runtime_peak = runtime.peak.as_ref().expect("the overlay adds the band");
        let shipped_peak = price.peak.as_ref().expect("the shipped list has the band");
        assert_eq!(
            runtime_peak.input_usd_per_mtok,
            shipped_peak.input_usd_per_mtok
        );
        assert_eq!(
            runtime_peak.output_usd_per_mtok,
            shipped_peak.output_usd_per_mtok
        );
        assert_eq!(
            runtime_peak.cache_read_usd_per_mtok,
            shipped_peak.cache_read_usd_per_mtok
        );
        assert_eq!(runtime_peak.windows.len(), shipped_peak.windows.len());
    }

    /// The busy-hours band bills the premium rates inside its windows and
    /// the standard rates outside them, on a weekday. The windows are
    /// weekday-only, so the weekend bills the standard rates all day.
    #[test]
    fn the_busy_hours_band_applies_on_weekdays_only() {
        let mut models: HashMap<String, ModelPrice> = HashMap::new();
        models.insert(
            "deepseek-flash".to_string(),
            ModelPrice {
                usd_per_mtok: 0.0,
                input_usd_per_mtok: 0.30,
                output_usd_per_mtok: 1.20,
                cache_read_usd_per_mtok: 0.006,
                ..Default::default()
            },
        );
        apply_peak_overlay(&mut models);
        let price = models.get("deepseek-flash").expect("the model survives");
        // 02:00 UTC on a Tuesday is inside the first window; 05:00 UTC is
        // outside both.
        let tuesday_peak = chrono::Utc.with_ymd_and_hms(2026, 10, 13, 2, 0, 0).unwrap();
        let tuesday_off = chrono::Utc.with_ymd_and_hms(2026, 10, 13, 5, 0, 0).unwrap();
        // 1M input tokens, no output, no cache hits.
        let peak_cost = price.cost_io_at_ts(1_000_000, 0, 0, tuesday_peak);
        let off_cost = price.cost_io_at_ts(1_000_000, 0, 0, tuesday_off);
        assert!(
            (peak_cost - 0.30).abs() < 1e-9,
            "the busy-hours band billed {peak_cost}"
        );
        assert!(
            (off_cost - 0.15).abs() < 1e-9,
            "the standard rate billed {off_cost}"
        );
    }

    #[test]
    fn openrouter_sync_does_not_turn_invalid_strings_into_free_rates() {
        let body = r#"{"data":[
            {"id":"bad","pricing":{"prompt":"NaN","completion":"bogus"}},
            {"id":"partial","pricing":{"prompt":"0.1"}},
            {"id":"free","pricing":{"prompt":"0","completion":"0"}}
        ]}"#;
        let mut models = HashMap::new();
        assert_eq!(merge_openrouter(&mut models, body, true).unwrap(), 1);
        assert!(models.contains_key("free"));
        assert!(!models.contains_key("bad"));
        assert!(!models.contains_key("partial"));
    }
}
