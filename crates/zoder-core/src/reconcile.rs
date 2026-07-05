//! Provider cost reconciliation (nightly true-up, not real-time metering).
//!
//! Most providers expose only token *usage* in the response; zoder prices that
//! locally from the catalog. A couple of providers also expose billed *dollars*
//! programmatically, but only at organization granularity, behind an admin key,
//! and with a lag. These are reconciliation backends used to true-up the local
//! ledger, not to meter individual calls:
//!
//!   - OpenAI    `GET /v1/organization/costs`        (admin key; daily buckets)
//!   - Anthropic `GET /v1/organizations/cost_report` (admin key; daily buckets)
//!
//! Each returns daily-bucketed USD totals; we sum them over a trailing window.
//! Admin keys are read from the environment and are distinct from the inference
//! keys used to make model calls.

use chrono::{Duration, Utc};
use std::time::Duration as StdDuration;

/// Billed dollars reported by a provider's cost API over a window.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReconResult {
    pub provider: String,
    pub days: i64,
    pub billed_usd: f64,
    pub source: String,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("zoder/", env!("CARGO_PKG_VERSION")))
        .timeout(StdDuration::from_secs(45))
        .build()
        .unwrap_or_default()
}

/// OpenAI Costs API. `admin_key` is an Admin API key (env `OPENAI_ADMIN_KEY`),
/// which is separate from the inference key and cannot be used for model calls.
pub async fn openai_costs(admin_key: &str, days: i64) -> anyhow::Result<ReconResult> {
    let start = (Utc::now() - Duration::days(days.max(1))).timestamp();
    let url = format!(
        "https://api.openai.com/v1/organization/costs?start_time={start}&bucket_width=1d&limit=180"
    );
    let body = client()
        .get(&url)
        .bearer_auth(admin_key)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let root: serde_json::Value = serde_json::from_str(&body)?;
    let mut total = 0.0;
    if let Some(buckets) = root.get("data").and_then(|d| d.as_array()) {
        for b in buckets {
            if let Some(results) = b.get("results").and_then(|r| r.as_array()) {
                for r in results {
                    let amount = r
                        .get("amount")
                        .and_then(|a| a.get("value"))
                        .and_then(|v| v.as_f64())
                        .ok_or_else(|| {
                            anyhow::anyhow!("OpenAI cost result is missing a numeric amount.value")
                        })?;
                    if !amount.is_finite() || amount < 0.0 {
                        anyhow::bail!("OpenAI cost result contains an invalid amount: {amount}");
                    }
                    total += amount;
                    if !total.is_finite() {
                        anyhow::bail!("OpenAI cost total overflowed");
                    }
                }
            }
        }
    }
    Ok(ReconResult {
        provider: "openai".into(),
        days,
        billed_usd: total,
        source: "openai /v1/organization/costs".into(),
    })
}

/// Anthropic Cost Report API. `admin_key` is an Admin API key (env
/// `ANTHROPIC_ADMIN_KEY`, prefix `sk-ant-admin...`). Amounts are returned as
/// decimal strings in cents; we convert to dollars. Not available for
/// individual accounts or the Bedrock-hosted product.
pub async fn anthropic_costs(admin_key: &str, days: i64) -> anyhow::Result<ReconResult> {
    let starting_at = (Utc::now() - Duration::days(days.max(1))).format("%Y-%m-%dT%H:%M:%SZ");
    let url = format!(
        "https://api.anthropic.com/v1/organizations/cost_report?starting_at={starting_at}&bucket_width=1d&limit=31"
    );
    let body = client()
        .get(&url)
        .header("x-api-key", admin_key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let root: serde_json::Value = serde_json::from_str(&body)?;
    let mut cents = 0.0;
    if let Some(buckets) = root.get("data").and_then(|d| d.as_array()) {
        for b in buckets {
            if let Some(results) = b.get("results").and_then(|r| r.as_array()) {
                for r in results {
                    let amount: f64 = match r.get("amount") {
                        Some(serde_json::Value::String(s)) => s.parse().map_err(|_| {
                            anyhow::anyhow!("Anthropic cost result has invalid amount {s:?}")
                        })?,
                        Some(serde_json::Value::Number(n)) => n.as_f64().ok_or_else(|| {
                            anyhow::anyhow!("Anthropic cost result has non-f64 amount {n}")
                        })?,
                        _ => anyhow::bail!("Anthropic cost result is missing amount"),
                    };
                    if !amount.is_finite() || amount < 0.0 {
                        anyhow::bail!("Anthropic cost result contains invalid amount: {amount}");
                    }
                    cents += amount;
                    if !cents.is_finite() {
                        anyhow::bail!("Anthropic cost total overflowed");
                    }
                }
            }
        }
    }
    Ok(ReconResult {
        provider: "anthropic".into(),
        days,
        billed_usd: cents / 100.0,
        source: "anthropic /v1/organizations/cost_report".into(),
    })
}
