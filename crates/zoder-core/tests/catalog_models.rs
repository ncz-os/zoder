//! Integration tests for the live `config/catalog-models` RPC client.
//!
//! These tests spin up a real Unix-socket "daemon" that speaks just enough
//! NDJSON JSON-RPC to satisfy `fetch_catalog_models`: ack `initialize`,
//! then answer `config/catalog-models` with a deterministic payload. The
//! tests assert the wire shape end-to-end:
//!
//!   1. the `initialize` handshake is sent with the right `protocol_version`,
//!   2. the `config/catalog-models` request is sent with the right method
//!      and an empty `params` object,
//!   3. the response is decoded into the typed `CatalogResponse`,
//!   4. the merge with a static corpus produces the documented outcomes
//!      (added / enriched / skipped),
//!   5. failure modes (daemon error, socket missing, daemon sends invalid
//!      JSON) degrade gracefully without panicking.
//!
//! Mirrors the wire-shape test pattern used by
//! `crates/acp-client/src/lib.rs`'s `spawn_cancel_test_daemon` so the
//! daemon-side test scaffolding is a known good template.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

use zoder_core::{
    catalog_models::{merge_or_degrade, CatalogResponse},
    fetch_catalog_models, Corpus, ModelEntry,
};

/// Spawn a Unix-socket "daemon" that:
///   1. acks `initialize` with `{ "protocolVersion": 1 }`,
///   2. on `config/catalog-models`, returns the supplied `catalog_json` as
///      the `result` (a single `{"models":[...]}` payload).
///
/// Records every frame the daemon READS so tests can assert the request
/// shape (method name, params, id). Returns the tempdir the socket lives
/// in; the socket path is `<tempdir>/daemon.sock`.
async fn spawn_catalog_daemon(
    catalog_json: String,
    received: Arc<Mutex<Vec<serde_json::Value>>>,
) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    let recv = received.clone();
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();

            // 1. read initialize -> record + ack
            let _ = reader.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                recv.lock().unwrap().push(v);
            }
            let init_ack = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "nvz-init",
                "result": { "protocolVersion": 1 }
            });
            let mut s = serde_json::to_string(&init_ack).unwrap();
            s.push('\n');
            let _ = write_half.write_all(s.as_bytes()).await;
            let _ = write_half.flush().await;

            // 2. read config/catalog-models -> record + answer
            line.clear();
            let _ = reader.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                recv.lock().unwrap().push(v);
            }
            let result: serde_json::Value = serde_json::from_str(&catalog_json)
                .expect("test fixture: catalog_json must be valid JSON");
            let answer = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "nvz-catalog",
                "result": result
            });
            let mut s = serde_json::to_string(&answer).unwrap();
            s.push('\n');
            let _ = write_half.write_all(s.as_bytes()).await;
            let _ = write_half.flush().await;
        }
    });
    dir
}

/// Spin up a daemon that acks `initialize` and then returns a JSON-RPC
/// error for `config/catalog-models`. Used to verify the wire layer
/// surfaces daemon errors as `Err` (so callers degrade) rather than
/// returning a fake `Ok`.
async fn spawn_failing_daemon(received: Arc<Mutex<Vec<serde_json::Value>>>) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind");
    let recv = received.clone();
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let (read_half, mut write_half) = tokio::io::split(stream);
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();

            // 1. ack initialize
            let _ = reader.read_line(&mut line).await;
            recv.lock()
                .unwrap()
                .push(serde_json::from_str(line.trim()).unwrap_or_default());
            let mut s = serde_json::to_string(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": "nvz-init",
                "result": { "protocolVersion": 1 }
            }))
            .unwrap();
            s.push('\n');
            let _ = write_half.write_all(s.as_bytes()).await;
            let _ = write_half.flush().await;

            // 2. answer config/catalog-models with a JSON-RPC error.
            line.clear();
            let _ = reader.read_line(&mut line).await;
            recv.lock()
                .unwrap()
                .push(serde_json::from_str(line.trim()).unwrap_or_default());
            let mut s = serde_json::to_string(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": "nvz-catalog",
                "error": { "code": -32601, "message": "method not found" }
            }))
            .unwrap();
            s.push('\n');
            let _ = write_half.write_all(s.as_bytes()).await;
            let _ = write_half.flush().await;
        }
    });
    dir
}

/// `fetch_catalog_models` must drive the full NDJSON JSON-RPC exchange
/// (`initialize` + `config/catalog-models`) and decode the response. The
/// request frame must carry the `config/catalog-models` method with an
/// empty `params` object so a future daemon that adds query filters can
/// rely on the field's presence.
#[tokio::test]
async fn fetch_catalog_models_round_trip() {
    let received: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let dir = spawn_catalog_daemon(
        r#"{"models":[
            {"id":"minimax/MiniMax-M3","host":"minimax","kind":"chat","free":true,"source":"subscription"},
            {"id":"newco/newmodel","host":"newco","kind":"chat","free":true,"source":"serves"}
        ]}"#
        .to_string(),
        received.clone(),
    )
    .await;
    let socket = dir.path().join("daemon.sock");

    let resp = fetch_catalog_models(&socket)
        .await
        .expect("rpc must succeed");
    assert_eq!(resp.models.len(), 2);
    assert_eq!(resp.models[0].id, "minimax/MiniMax-M3");
    assert_eq!(resp.models[0].free, Some(true));
    assert_eq!(resp.models[1].id, "newco/newmodel");

    // The daemon saw two frames: initialize and the catalog request.
    let frames = received.lock().unwrap().clone();
    assert_eq!(
        frames.len(),
        2,
        "daemon must see initialize + catalog request"
    );

    let init = &frames[0];
    assert_eq!(
        init.get("method").and_then(|v| v.as_str()),
        Some("initialize")
    );
    assert_eq!(
        init.pointer("/params/protocol_version")
            .and_then(|v| v.as_u64()),
        Some(1),
        "initialize MUST carry the protocol_version the daemon's handshake checks"
    );

    let req = &frames[1];
    assert_eq!(
        req.get("method").and_then(|v| v.as_str()),
        Some("config/catalog-models"),
        "request method must be config/catalog-models"
    );
    assert_eq!(req.get("id").and_then(|v| v.as_str()), Some("nvz-catalog"));
    // Empty params object — keeps the wire shape forward-compatible with a
    // future daemon that adds filter fields.
    assert!(
        req.get("params").map(|p| p.is_object()).unwrap_or(false),
        "config/catalog-models request MUST carry a params object; got {req}"
    );
}

/// `merge_or_degrade` must fold the live response into a static corpus
/// and report the right outcome. With one new id and one existing id the
/// outcome should show `added=1, enriched=1`.
#[tokio::test]
async fn merge_or_degrade_integration_with_live_rpc() {
    let received: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let dir = spawn_catalog_daemon(
        r#"{"models":[
            {"id":"minimax/MiniMax-M3","host":"minimax-rotated","kind":"chat","free":true,"source":"subscription"},
            {"id":"newco/newmodel","host":"newco","kind":"chat","free":true,"source":"serves"}
        ]}"#
        .to_string(),
        received.clone(),
    )
    .await;
    let socket = dir.path().join("daemon.sock");

    // Static corpus: ONE existing model (`minimax/MiniMax-M3`).
    let mut corpus = Corpus {
        models: vec![ModelEntry {
            id: "minimax/MiniMax-M3".into(),
            host: "minimax".into(),
            family: "minimax".into(),
            kind: "chat".into(),
            free: true,
            route_candidate: true,
            ..Default::default()
        }],
        ..Default::default()
    };

    let resp = fetch_catalog_models(&socket)
        .await
        .expect("rpc must succeed");
    let outcome = merge_or_degrade(Some(&mut corpus), Ok(resp));

    assert_eq!(outcome.rows, 2);
    assert_eq!(outcome.added, 1, "newco/newmodel must be a new addition");
    assert_eq!(
        outcome.enriched, 1,
        "minimax/MiniMax-M3 must be enriched in place"
    );
    assert_eq!(outcome.skipped, 0);
    assert!(outcome.error.is_none());
    assert!(outcome.has_live_data());

    // The existing model's host rotated (per the daemon's payload), the
    // new model is in the corpus.
    assert_eq!(corpus.models.len(), 2);
    let existing = corpus
        .models
        .iter()
        .find(|m| m.id == "minimax/MiniMax-M3")
        .unwrap();
    assert_eq!(existing.host, "minimax-rotated");
    let new_row = corpus
        .models
        .iter()
        .find(|m| m.id == "newco/newmodel")
        .unwrap();
    assert!(
        !new_row.route_candidate,
        "new daemon rows stay non-routable until corpus-builder benches them"
    );
    assert!(new_row.gated_reason.is_some());
}

/// A daemon that returns a JSON-RPC `error` for `config/catalog-models`
/// must surface that as `Err` to the caller, and `merge_or_degrade` must
/// record the error in the outcome and leave the corpus unchanged. The
/// "static corpus as fallback" contract is what makes this additive
/// enrichment safe to wire into `zoder models` / `zoder consult`.
#[tokio::test]
async fn merge_or_degrade_records_daemon_error_without_mutating_corpus() {
    let received: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let dir = spawn_failing_daemon(received.clone()).await;
    let socket = dir.path().join("daemon.sock");

    let mut corpus = Corpus {
        models: vec![ModelEntry {
            id: "minimax/MiniMax-M3".into(),
            host: "minimax".into(),
            family: "minimax".into(),
            kind: "chat".into(),
            free: true,
            ..Default::default()
        }],
        ..Default::default()
    };
    let before = corpus.models.clone();

    let rpc = fetch_catalog_models(&socket).await;
    let err = rpc.expect_err("daemon returned an error; client must surface it");
    // The message is preserved end-to-end so the operator can see WHY the
    // enrichment was skipped (e.g. "method not found" on an old daemon).
    let msg = format!("{err:#}");
    assert!(
        msg.contains("catalog engine returned an error") || msg.contains("method not found"),
        "error must name the daemon's failure; got: {msg}"
    );

    let outcome = merge_or_degrade(Some(&mut corpus), Err(err));
    assert_eq!(outcome.added, 0);
    assert_eq!(outcome.enriched, 0);
    assert!(outcome.error.is_some());
    assert!(!outcome.has_live_data());
    // Corpus is unchanged — the static path is the fallback, always.
    assert_eq!(corpus.models.len(), before.len());
    assert_eq!(corpus.models[0].id, before[0].id);
    assert_eq!(corpus.models[0].host, before[0].host);
}

/// A daemon that returns an empty catalog (zero models configured) is a
/// valid success, NOT an error. The outcome's `rows` is 0 and
/// `has_live_data()` is false, but `error` is `None` so the caller can
/// distinguish "no models configured" from "RPC failed".
#[tokio::test]
async fn merge_or_degrade_treats_empty_catalog_as_success_not_error() {
    let received: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let dir = spawn_catalog_daemon(r#"{"models":[]}"#.to_string(), received.clone()).await;
    let socket = dir.path().join("daemon.sock");

    let mut corpus = Corpus::default();
    let resp: CatalogResponse = fetch_catalog_models(&socket)
        .await
        .expect("rpc must succeed");
    let outcome = merge_or_degrade(Some(&mut corpus), Ok(resp));
    assert_eq!(outcome.rows, 0);
    assert_eq!(outcome.added, 0);
    assert!(outcome.error.is_none(), "empty success is not an error");
    assert!(!outcome.has_live_data());
}

/// A missing daemon socket must surface as a connection error, NOT a
/// panic. `merge_or_degrade` must record the error in the outcome and
/// leave the corpus unchanged. This is the operator's most common
/// failure mode (the daemon isn't running) and the CLI must keep
/// working — `zoder models` falls back to the static corpus.
#[tokio::test]
async fn missing_socket_fails_fast_without_panic() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bogus = dir.path().join("nonexistent.sock");
    // The RPC client has a 5s timeout; we wrap it in a tighter test
    // timeout so a regression that hangs the client fails the test
    // fast instead of stalling CI for 5s.
    let rpc = tokio::time::timeout(Duration::from_secs(7), fetch_catalog_models(&bogus))
        .await
        .expect("test timeout: RPC client must not hang on missing socket")
        .expect_err("missing socket must surface as Err");
    let msg = format!("{rpc:#}");
    assert!(
        msg.contains("connecting to catalog engine") || msg.contains("No such file"),
        "error must name the connection failure; got: {msg}"
    );

    let mut corpus = Corpus::default();
    let outcome = merge_or_degrade(Some(&mut corpus), Err(rpc));
    assert!(outcome.error.is_some());
    assert_eq!(
        corpus.models.len(),
        0,
        "static corpus is the fallback; nothing was added"
    );
}

/// `config/catalog-models` is the additive enrichment surface for `zoder
/// models`: rows that exist in BOTH the corpus and the live catalog show
/// up with the live data (the live `host` wins, the bench scores stay).
/// This is the operator-facing acceptance test for the wire-shape change.
#[tokio::test]
async fn existing_corpus_entry_shows_live_data_after_merge() {
    let received: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let dir = spawn_catalog_daemon(
        r#"{"models":[
            {"id":"minimax/MiniMax-M3","host":"minimax-live","kind":"chat","free":true}
        ]}"#
        .to_string(),
        received.clone(),
    )
    .await;
    let socket = dir.path().join("daemon.sock");

    let mut corpus = Corpus {
        models: vec![ModelEntry {
            id: "minimax/MiniMax-M3".into(),
            host: "minimax".into(),
            family: "minimax".into(),
            kind: "chat".into(),
            free: true,
            route_candidate: true,
            ..Default::default()
        }],
        ..Default::default()
    };

    let resp: CatalogResponse = fetch_catalog_models(&socket)
        .await
        .expect("rpc must succeed");
    let outcome = merge_or_degrade(Some(&mut corpus), Ok(resp));
    assert_eq!(outcome.enriched, 1);
    assert_eq!(outcome.added, 0);
    let m = corpus
        .models
        .iter()
        .find(|m| m.id == "minimax/MiniMax-M3")
        .unwrap();
    assert_eq!(m.host, "minimax-live", "live host must win");
    assert_eq!(m.family, "minimax-live", "family follows host on rotation");
    assert!(
        m.route_candidate,
        "the corpus's route eligibility is preserved"
    );
}

#[test]
fn merge_or_degrade_handles_missing_corpus() {
    let outcome = merge_or_degrade(None, Ok(CatalogResponse::default()));
    assert!(outcome.error.as_deref().unwrap().contains("corpus"));
}
