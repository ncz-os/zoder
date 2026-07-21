//! Integration tests for the SOP graph + overlay wire layer.
//!
//! Spins up a real `tokio::net::UnixListener` that speaks the engine's NDJSON
//! JSON-RPC dialect (the same dialect `engine_cost::fetch_engine_cost` is
//! tested against), so the wire layer is pinned end-to-end without needing a
//! running zeroclaw daemon.
//!
//! The pure-data parts (parsing, rendering, state-bucket grouping) are
//! covered by the unit tests in `crates/zoder-core/src/sop_graph.rs`; this
//! file is the `socket + read_line + write_frame + serde` boundary.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use zoder_core::{
    fetch_sop_graph, fetch_sop_graph_report, fetch_sop_overlay, render_sop_graph_human,
    SopGraphReport,
};

/// Run a server closure on a fresh `UnixListener`. The listener is bound to a
/// tempdir socket path that gets returned alongside the task handle; dropping
/// the task handle does NOT abort the listener — the test must `await` it
/// (or simply let it run to completion) to keep the RPC exchange coherent.
async fn spawn_engine<F, Fut>(server: F) -> (PathBuf, tokio::task::JoinHandle<()>)
where
    F: FnOnce(tokio::net::UnixStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    let socket_path = socket.clone();
    let handle = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            server(stream).await;
        }
    });
    // Hold the tempdir open until the handle is awaited by the caller (the
    // path is returned; the dir is leaked at process end, which is fine for
    // a test fixture).
    std::mem::forget(dir);
    (socket_path, handle)
}

async fn write_response<W: AsyncWriteExt + Unpin>(w: &mut W, id: &str, result: Value) {
    let mut s = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    }))
    .unwrap();
    s.push('\n');
    let _ = w.write_all(s.as_bytes()).await;
    let _ = w.flush().await;
}

async fn write_error<W: AsyncWriteExt + Unpin>(w: &mut W, id: &str, message: &str) {
    let mut s = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32000, "message": message },
    }))
    .unwrap();
    s.push('\n');
    let _ = w.write_all(s.as_bytes()).await;
    let _ = w.flush().await;
}

#[tokio::test]
async fn fetch_sop_graph_returns_decoded_graph() {
    let (socket, server) = spawn_engine(|stream| async move {
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // initialize
        let _ = reader.read_line(&mut line).await;
        write_response(
            &mut write_half,
            "sop-graph-init",
            json!({"protocolVersion": 1}),
        )
        .await;

        // sops/graph
        line.clear();
        let _ = reader.read_line(&mut line).await;
        let req: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(req["method"], "sops/graph");
        assert_eq!(req["params"]["run_id"], "r-42");
        write_response(
            &mut write_half,
            "sop-graph",
            json!({
                "run_id": "r-42",
                "sop": "incident-response",
                "steps": [
                    {"id": "triage", "name": "Triage", "kind": "decision", "next": ["ack"]},
                    {"id": "ack", "name": "Ack", "kind": "action", "next": []},
                ],
            }),
        )
        .await;
    })
    .await;

    let g = tokio::time::timeout(Duration::from_secs(5), fetch_sop_graph(&socket, "r-42"))
        .await
        .expect("fetch_sop_graph timed out")
        .expect("fetch_sop_graph errored");

    assert_eq!(g.run_id, "r-42");
    assert_eq!(g.sop, "incident-response");
    assert_eq!(g.steps.len(), 2);
    assert_eq!(g.steps[0].id, "triage");
    assert_eq!(g.steps[0].next, vec!["ack"]);
    assert_eq!(g.steps[1].id, "ack");
    assert!(g.steps[1].next.is_empty());

    let _ = server.await;
}

#[tokio::test]
async fn fetch_sop_graph_propagates_engine_error_message() {
    let (socket, server) = spawn_engine(|stream| async move {
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        let _ = reader.read_line(&mut line).await;
        write_response(
            &mut write_half,
            "sop-graph-init",
            json!({"protocolVersion": 1}),
        )
        .await;

        line.clear();
        let _ = reader.read_line(&mut line).await;
        write_error(&mut write_half, "sop-graph", "unknown run_id").await;
    })
    .await;

    let err = tokio::time::timeout(Duration::from_secs(5), fetch_sop_graph(&socket, "missing"))
        .await
        .expect("timeout")
        .expect_err("must error when engine rejects");
    assert!(
        err.to_string().contains("unknown run_id"),
        "engine error message not propagated: {err}"
    );

    let _ = server.await;
}

#[tokio::test]
async fn fetch_sop_graph_rejects_empty_run_id() {
    // We don't even need a listener for this — empty run_id is a local
    // precondition, not a wire condition. A bogus socket path is fine as
    // long as the function never touches it.
    let bogus = std::path::PathBuf::from("/tmp/this-socket-does-not-exist.sock");
    let err = fetch_sop_graph(&bogus, "")
        .await
        .expect_err("empty run_id must error");
    assert!(err.to_string().contains("empty"), "err: {err}");
}

#[tokio::test]
async fn fetch_sop_graph_report_includes_overlay_when_requested() {
    let (socket, server) = spawn_engine(|stream| async move {
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // initialize
        let _ = reader.read_line(&mut line).await;
        write_response(
            &mut write_half,
            "sop-report-init",
            json!({"protocolVersion": 1}),
        )
        .await;

        // sops/graph
        line.clear();
        let _ = reader.read_line(&mut line).await;
        let graph_req: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(graph_req["method"], "sops/graph");
        assert_eq!(graph_req["params"]["run_id"], "r-7");
        write_response(
            &mut write_half,
            "sop-report-graph",
            json!({
                "run_id": "r-7",
                "sop": "incident",
                "steps": [
                    {"id": "triage", "name": "Triage", "next": ["ack"]},
                    {"id": "ack", "name": "Ack", "next": []},
                ],
            }),
        )
        .await;

        // sops/run-overlay
        line.clear();
        let _ = reader.read_line(&mut line).await;
        let overlay_req: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(overlay_req["method"], "sops/run-overlay");
        assert_eq!(overlay_req["params"]["run_id"], "r-7");
        write_response(
            &mut write_half,
            "sop-report-overlay",
            json!({
                "run_id": "r-7",
                "active_step": "ack",
                "routing": "branch:escalate",
                "outcome": "running",
                "steps": [
                    {"id": "triage", "state": "done"},
                    {"id": "ack", "state": "active"},
                ],
            }),
        )
        .await;
    })
    .await;

    let report = tokio::time::timeout(
        Duration::from_secs(5),
        fetch_sop_graph_report(&socket, "r-7", true),
    )
    .await
    .expect("timeout")
    .expect("report errored");

    assert_eq!(report.graph.steps.len(), 2);
    let overlay = report.overlay.as_ref().expect("overlay missing");
    assert_eq!(overlay.active_step, "ack");
    assert_eq!(overlay.routing, "branch:escalate");
    assert_eq!(overlay.outcome, "running");
    assert_eq!(overlay.steps.len(), 2);

    // Sanity: the pure renderer composes the full report into a clean
    // string without panicking. The exact shape is pinned by unit tests;
    // here we just assert the load-bearing strings appear.
    let rendered = render_sop_graph_human(&report, "r-7");
    assert!(rendered.contains("run    r-7"), "{rendered}");
    assert!(rendered.contains("active  ack"), "{rendered}");
    assert!(rendered.contains("[done]"), "{rendered}");
    assert!(rendered.contains("[active]"), "{rendered}");

    let _ = server.await;
}

#[tokio::test]
async fn fetch_sop_graph_report_skips_overlay_when_not_requested() {
    let (socket, server) = spawn_engine(|stream| async move {
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        let _ = reader.read_line(&mut line).await;
        write_response(
            &mut write_half,
            "sop-graph-init",
            json!({"protocolVersion": 1}),
        )
        .await;

        line.clear();
        let _ = reader.read_line(&mut line).await;
        write_response(
            &mut write_half,
            "sop-graph",
            json!({
                "run_id": "r-9",
                "sop": "noop",
                "steps": [{"id": "only", "name": "Only", "next": []}],
            }),
        )
        .await;

        // Intentionally close the socket without a `sops/run-overlay`
        // round-trip — the test asserts the client never asks for one.
    })
    .await;

    let report = tokio::time::timeout(
        Duration::from_secs(5),
        fetch_sop_graph_report(&socket, "r-9", false),
    )
    .await
    .expect("timeout")
    .expect("report errored");

    assert_eq!(report.graph.run_id, "r-9");
    assert!(
        report.overlay.is_none(),
        "overlay must be None when not requested, got {:?}",
        report.overlay
    );

    let _ = server.await;
}

#[tokio::test]
async fn fetch_sop_overlay_returns_decoded_overlay() {
    let (socket, server) = spawn_engine(|stream| async move {
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        let _ = reader.read_line(&mut line).await;
        write_response(
            &mut write_half,
            "sop-overlay-init",
            json!({"protocolVersion": 1}),
        )
        .await;

        line.clear();
        let _ = reader.read_line(&mut line).await;
        let req: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(req["method"], "sops/run-overlay");
        write_response(
            &mut write_half,
            "sop-overlay",
            json!({
                "run_id": "r-12",
                "active_step": "",
                "outcome": "succeeded",
                "routing": "",
                "steps": [
                    {"id": "a", "state": "done"},
                    {"id": "b", "state": "done"},
                ],
            }),
        )
        .await;
    })
    .await;

    let overlay = tokio::time::timeout(Duration::from_secs(5), fetch_sop_overlay(&socket, "r-12"))
        .await
        .expect("timeout")
        .expect("overlay errored");

    assert_eq!(overlay.run_id, "r-12");
    assert_eq!(overlay.outcome, "succeeded");
    assert_eq!(overlay.steps.len(), 2);

    let _ = server.await;
}

#[tokio::test]
async fn sop_graph_grouped_states_handles_real_engine_drift() {
    // Build a real `SopGraphReport` from JSON (as the wire layer would),
    // then drive the overlay's `grouped_states` to confirm the bucket
    // ordering is robust against an overlay entry the graph doesn't know
    // about (engine drifted; common during upgrades).
    let report = SopGraphReport {
        graph: serde_json::from_value(json!({
            "run_id": "r-drift",
            "sop": "drift",
            "steps": [
                {"id": "a", "name": "A", "next": []},
                {"id": "b", "name": "B", "next": []},
            ],
        }))
        .unwrap(),
        overlay: Some(
            serde_json::from_value(json!({
                "run_id": "r-drift",
                "active_step": "a",
                "outcome": "running",
                "steps": [
                    {"id": "a", "state": "active"},
                    {"id": "b", "state": "done"},
                    {"id": "c_brand_new", "state": "pending"},
                ],
            }))
            .unwrap(),
        ),
    };
    let grouped = report
        .overlay
        .as_ref()
        .unwrap()
        .grouped_states(&report.graph);
    let map: std::collections::HashMap<&str, &Vec<&str>> =
        grouped.iter().map(|(k, v)| (*k, v)).collect();
    assert_eq!(map["active"], &vec!["a"]);
    assert_eq!(map["done"], &vec!["b"]);
    // The brand-new id lands in `pending` because the graph doesn't know
    // it — important so the operator can see the drift instead of seeing a
    // truncated overlay.
    assert_eq!(map["pending"], &vec!["c_brand_new"]);
}
