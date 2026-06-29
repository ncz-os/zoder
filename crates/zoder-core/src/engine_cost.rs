//! Client for the local zeroclaw **cost engine** over its Unix-socket JSON-RPC
//! (`cost/query`). This is how `zoder/zoder report` consumes the engine's
//! metered token usage + `cost_usd` (the figures the zerocode dashboard shows),
//! rather than relying only on the local ledger.
//!
//! The engine prices cloud models from `<data_dir>/pricing.json` (the catalog
//! the pricing feed writes); free-tier models stay `$0`. The transport is the
//! same NDJSON JSON-RPC the zerocode TUI speaks: connect, `initialize`, then one
//! request/response. Everything here is read-only and best-effort — a missing
//! or unreachable daemon yields an error the caller degrades on (falling back to
//! the ledger), never a panic.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// ACP protocol version the daemon's `initialize` expects. Mirrors
/// `zeroclaw_api::jsonrpc::ACP_PROTOCOL_VERSION`.
const ACP_PROTOCOL_VERSION: u64 = 1;

/// How long to wait for the whole connect → initialize → query exchange.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-model usage roll-up, mirroring the engine's `ModelStats` wire shape.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelStats {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub request_count: usize,
}

/// Per-agent usage roll-up, mirroring the engine's `AgentCostStats` wire shape.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentStats {
    #[serde(default)]
    pub agent_alias: String,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub request_count: usize,
}

/// The engine's `CostSummary`. For a bounded `[from, to)` query,
/// `session_cost_usd` / `total_tokens` / `by_model` are scoped to the window;
/// `daily_cost_usd` / `monthly_cost_usd` remain the daemon's today/this-month
/// aggregates regardless of bounds.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CostSummary {
    #[serde(default)]
    pub session_cost_usd: f64,
    #[serde(default)]
    pub daily_cost_usd: f64,
    #[serde(default)]
    pub monthly_cost_usd: f64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub request_count: usize,
    #[serde(default)]
    pub by_model: HashMap<String, ModelStats>,
    #[serde(default)]
    pub by_agent: HashMap<String, AgentStats>,
}

impl CostSummary {
    /// Windowed total cost (the `[from, to)` sum for a bounded query).
    pub fn window_cost_usd(&self) -> f64 {
        self.session_cost_usd
    }
}

/// Query the local cost engine for an optional `[from, to)` window and/or a
/// single agent. `from`/`to` are sent as RFC3339; omit both for the daemon's
/// default (session + today + this-month) summary.
pub async fn fetch_engine_cost(
    socket: &Path,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    agent: Option<&str>,
) -> anyhow::Result<CostSummary> {
    let exchange = async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .with_context(|| format!("connecting to cost engine at {}", socket.display()))?;
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);

        // First frame must be `initialize` (the daemon rejects everything else
        // until the handshake completes).
        write_frame(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": "nvz-init",
                "method": "initialize",
                "params": { "protocol_version": ACP_PROTOCOL_VERSION },
            }),
        )
        .await?;
        read_response(&mut reader, "nvz-init").await?;

        let mut params = serde_json::Map::new();
        if let Some(f) = from {
            params.insert("from".into(), json!(f.to_rfc3339()));
        }
        if let Some(t) = to {
            params.insert("to".into(), json!(t.to_rfc3339()));
        }
        if let Some(a) = agent {
            params.insert("agent".into(), json!(a));
        }
        write_frame(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": "nvz-cost",
                "method": "cost/query",
                "params": Value::Object(params),
            }),
        )
        .await?;
        let result = read_response(&mut reader, "nvz-cost").await?;
        let summary: CostSummary =
            serde_json::from_value(result).context("decoding cost/query result")?;
        Ok::<CostSummary, anyhow::Error>(summary)
    };

    tokio::time::timeout(QUERY_TIMEOUT, exchange)
        .await
        .map_err(|_| anyhow!("cost engine query timed out after {QUERY_TIMEOUT:?}"))?
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Value) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Read NDJSON frames until the response with `want_id` arrives, skipping
/// interleaved notifications (no `id`) and unrelated responses.
async fn read_response<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    want_id: &str,
) -> anyhow::Result<Value> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("reading from cost engine")?;
        if n == 0 {
            bail!("cost engine closed the connection before responding");
        }
        let frame: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if frame.get("id").and_then(Value::as_str) != Some(want_id) {
            continue;
        }
        if let Some(err) = frame.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("cost engine returned an error: {msg}");
        }
        return Ok(frame.get("result").cloned().unwrap_or(Value::Null));
    }
}
