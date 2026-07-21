//! Client for the local zeroclaw SOP (Standard Operating Procedure) graph /
//! overlay surface over its Unix-socket JSON-RPC (`sops/graph` and
//! `sops/run-overlay`).
//!
//! zeroclaw exposes SOP graphs declaratively (`sops/graph`) and a run-specific
//! overlay of state on top of that graph (`sops/run-overlay`). This module is a
//! thin read-only client for both: connect, `initialize`, send the request,
//! decode the response, return. Mirrors the read-only, best-effort shape of
//! [`crate::engine_cost`] (which already does the same dance for
//! `cost/query`) so the failure / timeout story is identical.
//!
//! The wire shape is intentionally tolerant — fields we don't need today are
//! allowed through `serde_json::Value` so a future engine build that adds new
//! per-step metadata doesn't force a zoder release.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// ACP protocol version the daemon's `initialize` expects. Mirrors
/// `zeroclaw_api::jsonrpc::ACP_PROTOCOL_VERSION` and `engine_cost.rs`.
const ACP_PROTOCOL_VERSION: u64 = 1;

/// How long to wait for the whole connect → initialize → sops/graph exchange.
/// The graph is small (a few KB at most) and the overlay is even smaller, so
/// 5s matches the cost-engine ceiling and is well under any sane operator
/// shell prompt.
const SOP_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// One node in the SOP step graph. The engine enumerates steps with a stable
/// `id`, an optional human-readable `name`, an optional `kind` (e.g.
/// `decision`, `action`, `branch`), and a `next` list of step ids that follow
/// it in the canonical execution order. Anything we don't model today (and
/// the engine may add tomorrow) is captured in `extra` for forward
/// compatibility.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct SopStep {
    /// Stable id the engine uses to reference this step (e.g. `"triage"`).
    #[serde(default)]
    pub id: String,
    /// Optional human label (e.g. `"Triage incoming alert"`). May be empty
    /// when the engine didn't bother to label a step.
    #[serde(default)]
    pub name: String,
    /// Optional kind tag (`"decision" | "action" | "branch" | "start" |
    /// "end" | ...`). Empty when absent — operators must not key on this
    /// being missing vs. empty.
    #[serde(default)]
    pub kind: String,
    /// Step ids that follow this one in the canonical execution order. Empty
    /// when the engine doesn't know yet, or when this is a terminal step.
    #[serde(default)]
    pub next: Vec<String>,
    /// Forward-compat bucket: anything else the engine returned for this
    /// step is preserved verbatim so a future zoder can inspect it without a
    /// new release here.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// The full SOP step graph for a run. We carry the run id the response is
/// scoped to (so the caller can confirm the engine answered for the same
/// `<run-id>` they asked about, and so the JSON dump round-trips) and the
/// ordered set of steps.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct SopGraph {
    /// The run id this graph was returned for. Mirrors the request's `run_id`
    /// — surfaced so a JSON consumer can correlate without re-passing it.
    #[serde(default)]
    pub run_id: String,
    /// Optional SOP name / alias (e.g. `"incident-response-v1"`). Empty when
    /// the engine didn't return one.
    #[serde(default)]
    pub sop: String,
    /// Ordered (or unordered, when the engine doesn't guarantee order) list
    /// of steps. Callers should index by [`SopStep::id`] for lookups.
    #[serde(default)]
    pub steps: Vec<SopStep>,
    /// Forward-compat bucket.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl SopGraph {
    /// Look up a step by its id. Returns `None` when the engine didn't
    /// include that id (typo on the operator side, or the engine trimmed
    /// unreachable steps).
    pub fn step(&self, id: &str) -> Option<&SopStep> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// Active step id from the run overlay (the step currently executing),
    /// or `None` when the overlay says no step is active or when we haven't
    /// fetched an overlay yet.
    ///
    /// This is a convenience over the overlay's [`SopOverlay::active_step`]
    /// field — provided on `SopGraph` so a caller that already fetched both
    /// doesn't have to thread the overlay through every call site.
    pub fn active_step<'a>(&self, overlay: Option<&'a SopOverlay>) -> Option<&'a str> {
        overlay.and_then(|o| {
            if o.active_step.is_empty() {
                None
            } else {
                Some(o.active_step.as_str())
            }
        })
    }
}

/// Per-step state in the run overlay. `state` is the load-bearing field
/// (`pending | active | done | failed | skipped`); everything else is
/// operator context (notes, error string, duration) the engine might attach.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct SopOverlayStep {
    /// Step id this overlay entry applies to. Foreign-key back to
    /// [`SopStep::id`].
    #[serde(default)]
    pub id: String,
    /// State of the step in this run.
    #[serde(default)]
    pub state: String,
    /// Free-form notes the engine attached (e.g. `"rerouted to on-call"`,
    /// `"approved by alice"`). Empty when absent.
    #[serde(default)]
    pub notes: String,
    /// Optional error message when `state == "failed"`. Empty otherwise.
    #[serde(default)]
    pub error: String,
    /// Forward-compat bucket.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// Per-run overlay of state on top of the canonical graph. The overlay tells
/// the operator which step is currently executing, which are done / pending
/// / failed / skipped, and (when the engine knows) a routing hint for what
/// should happen next.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct SopOverlay {
    /// The run id this overlay is scoped to.
    #[serde(default)]
    pub run_id: String,
    /// The id of the step currently executing. Empty when no step is active
    /// (overlay says the run is finished, or hasn't started yet).
    #[serde(default)]
    pub active_step: String,
    /// Per-step state, keyed by [`SopOverlayStep::id`]. A step missing from
    /// `steps` is implicitly `pending` per the engine convention; we don't
    /// synthesize those entries, callers should default-fill on lookup.
    #[serde(default)]
    pub steps: Vec<SopOverlayStep>,
    /// Optional routing hint the engine attaches to suggest what to do next
    /// (e.g. `"branch:escalate"`). Empty when absent.
    #[serde(default)]
    pub routing: String,
    /// Overall run outcome: `running | succeeded | failed | cancelled`. Empty
    /// when the engine didn't return one.
    #[serde(default)]
    pub outcome: String,
    /// Forward-compat bucket.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl SopOverlay {
    /// Convenience: find the overlay entry for `step_id`, or `None` if the
    /// engine didn't report one (implicitly `pending`).
    pub fn step(&self, step_id: &str) -> Option<&SopOverlayStep> {
        self.steps.iter().find(|s| s.id == step_id)
    }

    /// Group the steps by their reported state, in the canonical order:
    /// `active`, `done`, `failed`, `skipped`, `pending`. Steps the engine
    /// didn't report on land in `pending`.
    ///
    /// Returned vector preserves the step-id order within each bucket, and
    /// preserves the order the steps appeared in `self.steps` for any ties.
    /// Useful for the human renderer, which wants "active first, then
    /// failures, then done, then pending".
    pub fn grouped_states<'a>(&'a self, graph: &'a SopGraph) -> Vec<(&'a str, Vec<&'a str>)> {
        const BUCKETS: &[&str] = &["active", "done", "failed", "skipped", "pending"];
        let mut out: Vec<(&str, Vec<&str>)> = BUCKETS.iter().map(|b| (*b, Vec::new())).collect();
        let reported: std::collections::HashMap<&str, &SopOverlayStep> =
            self.steps.iter().map(|s| (s.id.as_str(), s)).collect();
        for step in &graph.steps {
            let bucket = match reported.get(step.id.as_str()) {
                Some(o) if !o.state.is_empty() => o.state.as_str(),
                _ => "pending",
            };
            if let Some((_, v)) = out.iter_mut().find(|(name, _)| *name == bucket) {
                v.push(step.id.as_str());
            }
        }
        // Also catch overlay-only ids the graph doesn't know about (engine
        // drifted). Park them in pending so they still show up.
        for o in &self.steps {
            if !graph.steps.iter().any(|s| s.id == o.id) {
                let bucket = if o.state.is_empty() {
                    "pending"
                } else {
                    o.state.as_str()
                };
                if let Some((_, v)) = out.iter_mut().find(|(name, _)| *name == bucket) {
                    v.push(o.id.as_str());
                }
            }
        }
        out
    }
}

/// Combined view: the canonical graph + the run's overlay on top of it.
/// Returned by [`fetch_sop_graph`] when both round-trips succeed. Either
/// field can be set independently when the operator asks for a partial
/// fetch — callers building one of these manually should populate both.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SopGraphReport {
    pub graph: SopGraph,
    pub overlay: Option<SopOverlay>,
}

/// Fetch the canonical SOP step graph for `run_id`. Best-effort, read-only:
/// returns `Err` on socket / handshake / parse failure so the caller can
/// degrade gracefully (the engine is the same one the zerocode dashboard
/// reads, so these match the TUI).
pub async fn fetch_sop_graph(socket: &Path, run_id: &str) -> anyhow::Result<SopGraph> {
    if run_id.is_empty() {
        bail!("fetch_sop_graph: run_id is empty");
    }
    let exchange = async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .with_context(|| format!("connecting to cost engine at {}", socket.display()))?;
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);

        // First frame must be `initialize` (the daemon rejects everything else
        // until the handshake completes). Mirrors the cost-engine wire shape
        // byte-for-byte.
        write_frame(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": "sop-graph-init",
                "method": "initialize",
                "params": { "protocol_version": ACP_PROTOCOL_VERSION },
            }),
        )
        .await?;
        read_response(&mut reader, "sop-graph-init").await?;

        write_frame(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": "sop-graph",
                "method": "sops/graph",
                "params": { "run_id": run_id },
            }),
        )
        .await?;
        let result = read_response(&mut reader, "sop-graph").await?;
        let graph: SopGraph =
            serde_json::from_value(result).context("decoding sops/graph result")?;
        Ok::<SopGraph, anyhow::Error>(graph)
    };

    tokio::time::timeout(SOP_QUERY_TIMEOUT, exchange)
        .await
        .map_err(|_| anyhow!("sops/graph query timed out after {SOP_QUERY_TIMEOUT:?}"))?
}

/// Fetch the per-run overlay of state on top of the canonical graph. Same
/// transport + timeout as [`fetch_sop_graph`] — separate function so a caller
/// that already has the graph can refresh the overlay without paying for a
/// second graph round-trip.
pub async fn fetch_sop_overlay(socket: &Path, run_id: &str) -> anyhow::Result<SopOverlay> {
    if run_id.is_empty() {
        bail!("fetch_sop_overlay: run_id is empty");
    }
    let exchange = async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .with_context(|| format!("connecting to cost engine at {}", socket.display()))?;
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);

        write_frame(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": "sop-overlay-init",
                "method": "initialize",
                "params": { "protocol_version": ACP_PROTOCOL_VERSION },
            }),
        )
        .await?;
        read_response(&mut reader, "sop-overlay-init").await?;

        write_frame(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": "sop-overlay",
                "method": "sops/run-overlay",
                "params": { "run_id": run_id },
            }),
        )
        .await?;
        let result = read_response(&mut reader, "sop-overlay").await?;
        let overlay: SopOverlay =
            serde_json::from_value(result).context("decoding sops/run-overlay result")?;
        Ok::<SopOverlay, anyhow::Error>(overlay)
    };

    tokio::time::timeout(SOP_QUERY_TIMEOUT, exchange)
        .await
        .map_err(|_| anyhow!("sops/run-overlay query timed out after {SOP_QUERY_TIMEOUT:?}"))?
}

/// Fetch both the graph and the overlay in a single connection. Used by the
/// `zoder sop graph <run-id>` CLI command, which is the documented entry
/// point. The two are kept in one helper so the operator gets an atomic view
/// (no half-updated state if the engine is mid-reload between the two calls)
/// and so the wire layer doesn't have to be exposed at the CLI boundary.
pub async fn fetch_sop_graph_report(
    socket: &Path,
    run_id: &str,
    include_overlay: bool,
) -> anyhow::Result<SopGraphReport> {
    if run_id.is_empty() {
        bail!("fetch_sop_graph_report: run_id is empty");
    }
    if !include_overlay {
        let graph = fetch_sop_graph(socket, run_id).await?;
        return Ok(SopGraphReport {
            graph,
            overlay: None,
        });
    }

    // Combined path: open one connection, do one `initialize`, then both
    // RPC calls on the same stream so we don't pay two connect round-trips
    // and so a daemon restart between the two requests can't produce a
    // torn view.
    let exchange = async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .with_context(|| format!("connecting to cost engine at {}", socket.display()))?;
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);

        write_frame(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": "sop-report-init",
                "method": "initialize",
                "params": { "protocol_version": ACP_PROTOCOL_VERSION },
            }),
        )
        .await?;
        read_response(&mut reader, "sop-report-init").await?;

        write_frame(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": "sop-report-graph",
                "method": "sops/graph",
                "params": { "run_id": run_id },
            }),
        )
        .await?;
        let graph_result = read_response(&mut reader, "sop-report-graph").await?;
        let graph: SopGraph =
            serde_json::from_value(graph_result).context("decoding sops/graph result")?;

        write_frame(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": "sop-report-overlay",
                "method": "sops/run-overlay",
                "params": { "run_id": run_id },
            }),
        )
        .await?;
        let overlay_result = read_response(&mut reader, "sop-report-overlay").await?;
        let overlay: SopOverlay =
            serde_json::from_value(overlay_result).context("decoding sops/run-overlay result")?;
        Ok::<SopGraphReport, anyhow::Error>(SopGraphReport {
            graph,
            overlay: Some(overlay),
        })
    };

    tokio::time::timeout(SOP_QUERY_TIMEOUT, exchange)
        .await
        .map_err(|_| anyhow!("sops graph+overlay query timed out after {SOP_QUERY_TIMEOUT:?}"))?
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Value) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Read NDJSON frames until the response with `want_id` arrives, skipping
/// interleaved notifications (no `id`) and unrelated responses. Mirrors
/// `engine_cost::read_response` so the wire layer stays symmetric across the
/// cost engine and the SOP graph.
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
            .context("reading from engine")?;
        if n == 0 {
            bail!("engine closed the connection before responding");
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
            bail!("engine returned an error: {msg}");
        }
        return Ok(frame.get("result").cloned().unwrap_or(Value::Null));
    }
}

/// Render a `SopGraphReport` as a clean plain-text summary for the CLI's
/// human output. Layout:
///
/// ```text
/// run  <run_id>   sop  <sop>
/// outcome: <outcome>   active: <step>   routing: <routing>
///
/// graph (N steps):
///   [ ] <id>  <name>                       kind=<kind> -> <next...>
///   [active] <id>  <name>
///   ...
///
/// overlay:
///   active: <step>
///   done:   <id1>, <id2>, ...
///   failed: <id3>
///   skipped:
///   pending: <id4>
/// ```
///
/// Pure: no I/O, no clock, no color. Unit-testable in isolation.
pub fn render_sop_graph_human(report: &SopGraphReport, run_id: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("run    {}\n", run_id));
    out.push_str(&format!("sop    {}\n", report.graph.sop));
    match &report.overlay {
        Some(o) => {
            out.push_str(&format!("outcome {}\n", display_or_dash(&o.outcome)));
            out.push_str(&format!("active  {}\n", display_or_dash(&o.active_step)));
            out.push_str(&format!("routing {}\n", display_or_dash(&o.routing)));
        }
        None => {
            out.push_str("outcome (no overlay fetched)\n");
        }
    }
    out.push('\n');

    if report.graph.steps.is_empty() {
        out.push_str("graph  (no steps reported)\n");
        return out;
    }
    out.push_str(&format!(
        "graph  ({} step{}):\n",
        report.graph.steps.len(),
        if report.graph.steps.len() == 1 {
            ""
        } else {
            "s"
        }
    ));

    // Build a state lookup from the overlay (if any) so each step line can
    // be tagged. Default to `pending` when the engine didn't report one.
    let state_lookup: std::collections::HashMap<&str, &SopOverlayStep> = match &report.overlay {
        Some(o) => o.steps.iter().map(|s| (s.id.as_str(), s)).collect(),
        None => std::collections::HashMap::new(),
    };

    for step in &report.graph.steps {
        let state = state_lookup
            .get(step.id.as_str())
            .map(|o| o.state.as_str())
            .unwrap_or("pending");
        let tag = state_tag(state);
        let label = if step.name.is_empty() {
            step.id.as_str()
        } else {
            step.name.as_str()
        };
        let kind_part = if step.kind.is_empty() {
            String::new()
        } else {
            format!(" kind={}", step.kind)
        };
        let next_part = if step.next.is_empty() {
            String::new()
        } else {
            format!(" -> {}", step.next.join(","))
        };
        out.push_str(&format!(
            "  {tag} {id:<20} {label}{kind_part}{next_part}\n",
            id = step.id,
        ));
    }

    // Trailing notes / error for any step that's `failed`.
    if let Some(overlay) = &report.overlay {
        let mut any_failed_note = false;
        for o in &overlay.steps {
            if o.state == "failed" && !o.error.is_empty() {
                if !any_failed_note {
                    out.push('\n');
                    out.push_str("errors:\n");
                    any_failed_note = true;
                }
                let note = if o.notes.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", o.notes)
                };
                out.push_str(&format!("  {}: {}{}\n", o.id, o.error, note));
            }
        }
    }
    out
}

fn display_or_dash(s: &str) -> &str {
    if s.is_empty() {
        "-"
    } else {
        s
    }
}

fn state_tag(state: &str) -> &'static str {
    match state {
        "active" => "[active]",
        "done" => "[done]  ",
        "failed" => "[FAIL]  ",
        "skipped" => "[skip]  ",
        _ => "[.....] ",
    }
}

#[cfg(test)]
mod tests {
    //! Pure-data unit tests. The full socket round-trip is exercised in
    //! `crates/zoder-core/tests/sop_graph.rs` against a real
    //! `tokio::net::UnixListener` so the wire layer is pinned without
    //! needing the zeroclaw engine.

    use super::*;

    #[test]
    fn sop_step_parses_minimal_wire_shape() {
        // The engine may emit steps with just `id` and no other fields —
        // must not fail to deserialize.
        let v = json!({ "id": "triage" });
        let s: SopStep = serde_json::from_value(v).unwrap();
        assert_eq!(s.id, "triage");
        assert_eq!(s.name, "");
        assert_eq!(s.kind, "");
        assert!(s.next.is_empty());
    }

    #[test]
    fn sop_step_parses_full_wire_shape() {
        let v = json!({
            "id": "triage",
            "name": "Triage incoming alert",
            "kind": "decision",
            "next": ["ack", "escalate"],
            "extra_field_engine_may_add": 42,
        });
        let s: SopStep = serde_json::from_value(v).unwrap();
        assert_eq!(s.id, "triage");
        assert_eq!(s.name, "Triage incoming alert");
        assert_eq!(s.kind, "decision");
        assert_eq!(s.next, vec!["ack", "escalate"]);
        // Forward-compat: any field we don't model lives under `extra` so a
        // future schema addition doesn't break us.
        assert_eq!(s.extra.get("extra_field_engine_may_add"), Some(&json!(42)));
    }

    #[test]
    fn sop_graph_step_lookup_hits_and_misses() {
        let g = SopGraph {
            run_id: "r-1".into(),
            sop: "incident-response".into(),
            steps: vec![
                SopStep {
                    id: "a".into(),
                    ..Default::default()
                },
                SopStep {
                    id: "b".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(g.step("a").unwrap().id, "a");
        assert_eq!(g.step("b").unwrap().id, "b");
        assert!(g.step("c").is_none());
    }

    #[test]
    fn sop_overlay_grouped_states_orders_buckets_canonically() {
        let graph = SopGraph {
            steps: vec![
                SopStep {
                    id: "a".into(),
                    ..Default::default()
                },
                SopStep {
                    id: "b".into(),
                    ..Default::default()
                },
                SopStep {
                    id: "c".into(),
                    ..Default::default()
                },
                SopStep {
                    id: "d".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let overlay = SopOverlay {
            steps: vec![
                SopOverlayStep {
                    id: "a".into(),
                    state: "done".into(),
                    ..Default::default()
                },
                SopOverlayStep {
                    id: "b".into(),
                    state: "active".into(),
                    ..Default::default()
                },
                SopOverlayStep {
                    id: "c".into(),
                    state: "failed".into(),
                    ..Default::default()
                },
                // `d` deliberately omitted → falls into `pending`.
            ],
            ..Default::default()
        };
        let grouped = overlay.grouped_states(&graph);
        let map: std::collections::HashMap<&str, &Vec<&str>> =
            grouped.iter().map(|(k, v)| (*k, v)).collect();
        assert_eq!(map["active"], &vec!["b"]);
        assert_eq!(map["done"], &vec!["a"]);
        assert_eq!(map["failed"], &vec!["c"]);
        assert_eq!(map["skipped"], &Vec::<&str>::new());
        assert_eq!(map["pending"], &vec!["d"]);
    }

    #[test]
    fn render_sop_graph_human_includes_active_step_and_routing() {
        let report = SopGraphReport {
            graph: SopGraph {
                run_id: "r-7".into(),
                sop: "incident-response".into(),
                steps: vec![
                    SopStep {
                        id: "triage".into(),
                        name: "Triage".into(),
                        kind: "decision".into(),
                        next: vec!["ack".into()],
                        ..Default::default()
                    },
                    SopStep {
                        id: "ack".into(),
                        name: "Acknowledge".into(),
                        kind: "action".into(),
                        next: vec![],
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            overlay: Some(SopOverlay {
                run_id: "r-7".into(),
                active_step: "ack".into(),
                routing: "branch:escalate".into(),
                outcome: "running".into(),
                steps: vec![
                    SopOverlayStep {
                        id: "triage".into(),
                        state: "done".into(),
                        ..Default::default()
                    },
                    SopOverlayStep {
                        id: "ack".into(),
                        state: "active".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }),
        };
        let s = render_sop_graph_human(&report, "r-7");
        // Header lines must include the operator-facing fields.
        assert!(s.contains("run    r-7"), "missing run line: {s}");
        assert!(
            s.contains("sop    incident-response"),
            "missing sop line: {s}"
        );
        assert!(s.contains("active  ack"), "missing active line: {s}");
        assert!(
            s.contains("routing branch:escalate"),
            "missing routing line: {s}"
        );
        // Step lines must show the state tag. We anchor on the step id so
        // the test is robust against harmless whitespace tweaks inside the
        // tag columns.
        assert!(
            s.contains("[done]")
                && s.lines()
                    .any(|l| l.contains("triage") && l.contains("[done]")),
            "missing done tag for triage: {s}"
        );
        assert!(
            s.contains("[active]")
                && s.lines()
                    .any(|l| l.contains("ack") && l.contains("[active]")),
            "missing active tag for ack: {s}"
        );
        // The transition edge from `triage -> ack` should be visible so the
        // operator can read the routing at a glance.
        assert!(s.contains("-> ack"), "missing next link: {s}");
    }

    #[test]
    fn render_sop_graph_human_handles_no_overlay() {
        let report = SopGraphReport {
            graph: SopGraph {
                run_id: "r-2".into(),
                sop: "noop".into(),
                steps: vec![SopStep {
                    id: "only".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            overlay: None,
        };
        let s = render_sop_graph_human(&report, "r-2");
        assert!(
            s.contains("(no overlay fetched)"),
            "missing placeholder: {s}"
        );
        // Without an overlay every step falls into the default `[.....]`
        // tag, so the operator can see the graph but not the state.
        assert!(
            s.lines()
                .any(|l| l.contains("only") && l.contains("[.....]")),
            "missing default tag: {s}"
        );
    }

    #[test]
    fn render_sop_graph_human_handles_empty_step_list() {
        let report = SopGraphReport {
            graph: SopGraph {
                run_id: "r-3".into(),
                sop: "empty".into(),
                steps: vec![],
                ..Default::default()
            },
            overlay: None,
        };
        let s = render_sop_graph_human(&report, "r-3");
        assert!(
            s.contains("(no steps reported)"),
            "empty-graph placeholder missing: {s}"
        );
    }

    #[test]
    fn render_sop_graph_human_emits_failed_notes_section() {
        let report = SopGraphReport {
            graph: SopGraph {
                run_id: "r-4".into(),
                sop: "incident".into(),
                steps: vec![
                    SopStep {
                        id: "x".into(),
                        ..Default::default()
                    },
                    SopStep {
                        id: "y".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            overlay: Some(SopOverlay {
                run_id: "r-4".into(),
                active_step: "".into(),
                routing: "".into(),
                outcome: "failed".into(),
                steps: vec![
                    SopOverlayStep {
                        id: "x".into(),
                        state: "failed".into(),
                        error: "boom".into(),
                        notes: "rerouted to on-call".into(),
                        ..Default::default()
                    },
                    SopOverlayStep {
                        id: "y".into(),
                        state: "done".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }),
        };
        let s = render_sop_graph_human(&report, "r-4");
        assert!(
            s.lines().any(|l| l.contains("x") && l.contains("[FAIL]")),
            "failed tag missing: {s}"
        );
        assert!(s.contains("errors:"), "errors header missing: {s}");
        assert!(s.contains("x: boom"), "error line missing: {s}");
        assert!(s.contains("rerouted to on-call"), "notes missing: {s}");
    }
}
