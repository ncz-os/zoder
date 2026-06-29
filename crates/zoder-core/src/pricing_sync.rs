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

use crate::pricing::{ModelPrice, PricingCatalog};
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
fn num(v: &serde_json::Value, key: &str) -> f64 {
    v.get(key).and_then(|x| x.as_f64()).unwrap_or(0.0)
}

/// OpenRouter stores `$ / token` as strings (to avoid float drift).
fn strnum(v: &serde_json::Value, key: &str) -> f64 {
    match v.get(key) {
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0.0),
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
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
        let price = ModelPrice {
            usd_per_mtok: 0.0,
            input_usd_per_mtok: num(m, "input_cost_per_token") * PER_TOK_TO_MTOK,
            output_usd_per_mtok: num(m, "output_cost_per_token") * PER_TOK_TO_MTOK,
            cache_read_usd_per_mtok: num(m, "cache_read_input_token_cost") * PER_TOK_TO_MTOK,
            cache_write_usd_per_mtok: num(m, "cache_creation_input_token_cost") * PER_TOK_TO_MTOK,
            reasoning_usd_per_mtok: num(m, "output_cost_per_reasoning_token") * PER_TOK_TO_MTOK,
            source: "litellm".into(),
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
        let price = ModelPrice {
            usd_per_mtok: 0.0,
            input_usd_per_mtok: strnum(pricing, "prompt") * PER_TOK_TO_MTOK,
            output_usd_per_mtok: strnum(pricing, "completion") * PER_TOK_TO_MTOK,
            cache_read_usd_per_mtok: strnum(pricing, "input_cache_read") * PER_TOK_TO_MTOK,
            cache_write_usd_per_mtok: strnum(pricing, "input_cache_write") * PER_TOK_TO_MTOK,
            reasoning_usd_per_mtok: strnum(pricing, "internal_reasoning") * PER_TOK_TO_MTOK,
            source: "openrouter".into(),
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
