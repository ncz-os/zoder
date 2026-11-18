//! FB131 (2026-11-03, re-pinned 2026-11-18 onto current master): golden +
//! frame-level characterisation tests for the zoder MCP server.
//!
//! Two layers:
//! 1. tool-schema goldens: `tests/golden/tool_*.json`, one file per advertised
//!    tool's schema object, structural-compared against the real `tools/list`
//!    output of a live server (any future field/schema drift fails here).
//! 2. stdio JSON-RPC frame tests: a real `zoder-mcp-server` child process is
//!    driven line-by-line with `$ZODER_HOME` AND `$HOME` pointed at a fresh
//!    scratch dir (under the crate's `target/`, removed on drop), and one frame
//!    is exchanged at a time. `$HOME` must be pinned too because the SOP
//!    forwarding path falls back to `$HOME/.zoder/data/daemon.sock`; without
//!    that pin the SOP-forward contract below would silently depend on whether
//!    the build host happens to run a zeroclaw daemon. Each input class must
//!    produce a stable, documented response shape and NEVER a panic:
//!      - protocol errors: -32700 parse error, -32600 invalid version,
//!        -32601 method-not-found (unknown tool / unknown method /
//!        not-yet-initialized),
//!      - -32602 invalid params per class (missing shape, wrong type,
//!        oversized task/tier, invalid tier, missing SOP arguments),
//!      - tool-level failures arrive as `result.isError=true` with a readable
//!        text payload per MCP (unavailable routing context, failed routing
//!        decision, failed SOP RPC forward) — never a protocol error.
//!
//! The `task` argument to `zoder_route` is an optional traceability label:
//! routing is tier-based, so absent/empty/whitespace/null tasks are VALID and
//!! flow into routing (they fail soft with an isError result here because the
//! seeded corpus is legitimately empty). Only wrong-TYPE task values fail the
//! parse (-32602); oversized task/tier and unknown tiers stay -32602.
//!
//! Characterisation findings pinned here (recorded in the lane summary, NOT
//! fixed — tests-only task):
//! - F1: JSON-RPC's -32800 (cancelled) is not implemented anywhere: a
//!   `$ /cancelRequest` REQUEST surfaces as -32601 "method not found" and
//!   `notifications/cancelled` is silently ignored (both asserted).
//! - F2: Tool.input_schema serializes with the snake_case wire key
//!   "input_schema" (not MCP's camelCase "inputSchema") — the goldens pin the
//!   current shape so any silent change also fails them.
//! - F3 (superseded 2026-11-18): the context-unavailable check used to emit
//!   protocol error -32603 before any arg validation; master now returns it
//!   as a tool-level `result.isError=true` ("routing context not available").
//! - F4 (superseded 2026-11-18): a missing task used to be -32602 ("task
//!   parameter is required..."); `task` is optional now, so those inputs
//!   reach routing and fail soft with isError instead.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_zoder-mcp-server");

struct Server {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    /// Scratch `$HOME`/`$ZODER_HOME` this server ran against, removed on drop.
    home: std::path::PathBuf,
}

impl Server {
    /// Bare env: `$ZODER_HOME` is empty, so `Config::load` succeeds but the
    /// model corpus file is missing and the routing context stays `None` —
    /// every `tools/call` short-circuits to the isError tool result before
    /// any arg validation.
    fn spawn() -> Self {
        Self::spawn_with_corpus(false)
    }

    /// Seed `$ZODER_HOME/model_corpus.json` with a legitimately-empty corpus
    /// (`{"models":[]}` — empty loads Ok by contract) so the routing context
    /// exists and the per-tool argument validation is reachable.
    fn spawn_seeded() -> Self {
        Self::spawn_with_corpus(true)
    }

    fn spawn_with_corpus(seed: bool) -> Self {
        // Under the crate's own target/ rather than the system temp dir: the
        // scratch dir is large-ish and outlives neither the test nor the run,
        // and a build host may mount /tmp as a small tmpfs.
        let home = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("fb131-scratch")
            .join(format!(
                "{}-{}-{}",
                std::process::id(),
                std::thread::current().name().unwrap_or("unnamed").replace(
                    |c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-',
                    "_"
                ),
                if seed { "seeded" } else { "bare" },
            ));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        if seed {
            std::fs::write(home.join("model_corpus.json"), r#"{"models":[]}"#)
                .expect("write seed corpus");
        }
        let mut child = Command::new(BIN)
            .env("ZODER_HOME", &home)
            // `daemon_socket_path()` falls back to `$HOME/.zoder/data/daemon.sock`
            // when neither ZEROCLAW_SOCKET nor ZEROCLAW_CONFIG_DIR is set, so
            // `$ZODER_HOME` alone is NOT enough to isolate the SOP forwarding
            // path: on a host actually running a zeroclaw daemon, `sop_list`
            // would answer from the live daemon and the SOP-forward contract these
            // tests pin would silently depend on the build host's state. Pin
            // `$HOME` at the scratch dir too so the fallback resolves inside
            // it and every SOP forward fails deterministically.
            .env("HOME", &home)
            // keep away from any host-level daemon or config that would make
            // the SOP/RPC and routing paths non-deterministic in CI
            .env_remove("ZEROCLAW_SOCKET")
            .env_remove("ZEROCLAW_CONFIG_DIR")
            .env_remove("RUST_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn zoder-mcp-server");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        Server {
            child,
            stdin,
            reader: BufReader::new(stdout),
            home,
        }
    }

    fn send(&mut self, frame: &Value) {
        writeln!(self.stdin, "{frame}").expect("write frame");
        self.stdin.flush().expect("flush frame");
    }

    fn send_raw(&mut self, raw: &str) {
        writeln!(self.stdin, "{raw}").expect("write raw frame");
        self.stdin.flush().expect("flush raw frame");
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        let n = self
            .reader
            .read_line(&mut line)
            .expect("server did not answer with a frame");
        assert!(n > 0, "server closed stdout (crash or exit — a panic path)");
        serde_json::from_str(&line).expect("response frame is valid JSON")
    }

    /// initialize + notifications/initialized; returns the init response.
    fn handshake(&mut self) -> Value {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        }));
        let init = self.read();
        assert_eq!(init["jsonrpc"], "2.0");
        assert_eq!(init["id"], 1);
        assert!(init["error"].is_null());
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }));
        init
    }

    /// Send a request and assert its error response carries `code`.
    fn expect_error(&mut self, frame: &Value, code: i64, msg_substr: &str) {
        self.send(frame);
        let resp = self.read();
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], frame["id"], "id must be echoed");
        assert!(resp["result"].is_null(), "must be an error: {resp}");
        assert_eq!(
            resp["error"]["code"], code,
            "class mismatch for {frame}: {resp}"
        );
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains(msg_substr),
            "message shape mismatch for {frame}: {resp}"
        );
    }

    /// Send a request and assert it fails SOFT at the tool level: a `result`
    /// object with `isError=true` and a text payload containing `substr`.
    /// Per MCP, a tool that ran and failed must NOT surface as a JSON-RPC
    /// protocol error (those stay hidden from the calling model).
    fn expect_tool_error(&mut self, frame: &Value, msg_substr: &str) {
        self.send(frame);
        let resp = self.read();
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], frame["id"], "id must be echoed");
        assert!(resp["error"].is_null(), "must be a result: {resp}");
        assert_eq!(
            resp["result"]["isError"], true,
            "must be a tool failure: {resp}"
        );
        let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains(msg_substr),
            "tool-error text mismatch for {frame}: {resp}"
        );
    }

    /// Send a frame, prove NO response was produced, then prove the loop is
    /// still alive and un-panicked with a probe request.
    fn expect_no_response(&mut self, frame: &Value) {
        self.send(frame);
        // probe: exactly one response must arrive and it must be the probe's
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": 777,
            "method": "initialize",
            "params": {}
        }));
        let resp = self.read();
        assert_eq!(resp["id"], 777, "unexpected frame before probe: {resp}");
        assert!(resp["error"].is_null());
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn req(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

// ─────────────────────────────────── golden: tools/list vs per-tool files ──

#[test]
fn golden_tools_list_matches_per_tool_schema_files() {
    let mut server = Server::spawn();
    server.handshake();
    server.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));
    let resp = server.read();
    assert!(resp["error"].is_null(), "tools/list must succeed: {resp}");
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 5, "five tools are advertised");
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("tool name"))
        .collect();
    assert_eq!(
        names,
        [
            "zoder_route",
            "sop_list",
            "sop_execute",
            "sop_advance",
            "sop_status"
        ],
        "advertised order is pinned (clients may rely on it)"
    );
    for (name, tool) in names.iter().zip(tools.iter()) {
        let path = format!(
            "{}/tests/golden/tool_{NAME}.json",
            env!("CARGO_MANIFEST_DIR"),
            NAME = name
        );
        let golden: Value = serde_json::from_str(
            &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}")),
        )
        .expect("golden is valid JSON");
        assert_eq!(
            tool, &golden,
            "tool {name} wire shape drifted from its golden file"
        );
        // schema well-formedness, independent from the golden blob
        let schema = tool["input_schema"]
            .as_object()
            .expect("input_schema object (F2: snake_case wire key)");
        assert_eq!(schema["type"], "object");
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            let props = schema["properties"].as_object().expect("properties");
            for r in required {
                assert!(
                    props.contains_key(r.as_str().expect("required str")),
                    "{name}: required {r} not declared"
                );
            }
        }
    }
}

// ─────────────────────────────────────────────── session-state frame gate ──

#[test]
fn frame_methods_before_initialize_are_rejected() {
    let mut server = Server::spawn();
    server.expect_error(
        &req(2, "tools/list", json!({})),
        -32601,
        "(server not initialized)",
    );
}

#[test]
fn frame_unknown_jsonrpc_version_is_rejected_with_id_echo() {
    let mut server = Server::spawn();
    let frame = json!({ "jsonrpc": "1.0", "id": 33, "method": "initialize", "params": {} });
    server.send(&frame);
    let resp = server.read();
    assert_eq!(
        resp["id"], 33,
        "id echo is required for dispatchable frames"
    );
    assert_eq!(resp["error"]["code"], -32600);
    assert_eq!(resp["error"]["message"], "invalid JSON-RPC version");
}

#[test]
fn frame_unparseable_line_is_parse_error_with_null_id() {
    let mut server = Server::spawn();
    server.send_raw("this is not json {");
    let resp = server.read();
    assert!(resp["id"].is_null());
    assert_eq!(resp["error"]["code"], -32700);
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("parse error"),
        "{}",
        resp
    );
}

#[test]
fn frame_unknown_method_maps_to_method_not_found() {
    let mut server = Server::spawn();
    server.handshake();
    server.expect_error(
        &req(4, "not/a/method", json!({})),
        -32601,
        "method not found",
    );
}

#[test]
fn frame_client_response_and_notifications_produce_no_output() {
    let mut server = Server::spawn();
    server.handshake();
    // client-to-server echo AND all notifications must never be answered
    server.expect_no_response(&json!({
        "jsonrpc": "2.0",
        "id": 99,
        "result": { "ok": true }
    }));
    server.expect_no_response(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/progress",
        "params": { "progress": 1 }
    }));
    // F1: notifications/cancelled is silently ignored (see summary)
    server.expect_no_response(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": 1 }
    }));
}

// ────────────────────────────────────────────────── tools/call arg classes ─

#[test]
fn frame_tools_call_unknown_tool_is_method_not_found() {
    let mut server = Server::spawn_seeded();
    server.handshake();
    server.expect_error(
        &req(
            5,
            "tools/call",
            json!({ "name": "tp9_totally_unknown_tool", "arguments": {} }),
        ),
        -32601,
        "unknown tool: tp9_totally_unknown_tool",
    );
}

#[test]
fn frame_tools_call_missing_or_shapeless_params_are_invalid_params() {
    let mut server = Server::spawn_seeded();
    server.handshake();
    // params entirely absent
    server.send(&json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call" }));
    let resp = server.read();
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(resp["error"]["message"], "missing tool call parameters");
    // params not matching ToolCallParams (name non-string, then name absent)
    for bad in [json!(7), json!({"arguments": {}})] {
        server.expect_error(
            &req(7, "tools/call", bad),
            -32602,
            "invalid tool call parameters",
        );
    }
}

#[test]
fn frame_zoder_route_wrong_type_arguments_are_invalid_params() {
    let mut server = Server::spawn_seeded();
    server.handshake();
    for (bad, substr) in [
        (
            json!({ "name": "zoder_route", "arguments": { "task": 123 } }),
            "invalid zoder_route arguments",
        ),
        (
            json!({ "name": "zoder_route", "arguments": { "task": true } }),
            "invalid zoder_route arguments",
        ),
    ] {
        server.send(&req(8, "tools/call", bad));
        let resp = server.read();
        assert_eq!(resp["error"]["code"], -32602, "{resp}");
        assert!(
            resp["error"]["message"].as_str().unwrap().contains(substr),
            "{resp}"
        );
    }
    // serde seq quirk (F4, re-pinned): a bare array parses via the
    // field-default path, so it reaches routing and fails soft with a
    // tool-level isError result instead of the parse-failure message.
    server.expect_tool_error(
        &req(
            8,
            "tools/call",
            json!({ "name": "zoder_route", "arguments": [] }),
        ),
        "tool execution failed",
    );
}

#[test]
fn frame_zoder_route_missing_and_empty_task_are_valid_and_route() {
    // `task` is an optional traceability label (routing is tier-based), so
    // absent/empty/whitespace/null tasks all VALIDATE and flow into routing.
    // The seeded corpus is legitimately empty, so routing cannot succeed here
    // and the contract is a soft tool-level isError — never -32602, never a
    // panic. (Re-pinned 2026-11-18: these were -32602 "task parameter ..."
    // when task was required.)
    let mut server = Server::spawn_seeded();
    server.handshake();
    for bad in [
        json!({ "name": "zoder_route", "arguments": {} }),
        json!({ "name": "zoder_route", "arguments": { "task": "" } }),
        json!({ "name": "zoder_route", "arguments": { "task": "   \t\n " } }),
        json!({ "name": "zoder_route", "arguments": { "task": null } }),
    ] {
        server.expect_tool_error(&req(9, "tools/call", bad), "tool execution failed");
    }
}

#[test]
fn frame_zoder_route_oversized_task_and_tier_are_invalid_params() {
    let mut server = Server::spawn_seeded();
    server.handshake();
    server.expect_error(
        &req(
            10,
            "tools/call",
            json!({ "name": "zoder_route", "arguments": { "task": "a".repeat(10_001) } }),
        ),
        -32602,
        "exceeds maximum length",
    );
    server.expect_error(
        &req(
            11,
            "tools/call",
            json!({ "name": "zoder_route", "arguments": { "task": "x", "tier": "b".repeat(65) } }),
        ),
        -32602,
        "exceeds maximum length",
    );
}

#[test]
fn frame_zoder_route_invalid_tier_is_invalid_params() {
    let mut server = Server::spawn_seeded();
    server.handshake();
    server.expect_error(
        &req(
            12,
            "tools/call",
            json!({ "name": "zoder_route", "arguments": { "task": "x", "tier": "hyper" } }),
        ),
        -32602,
        "invalid tier: 'hyper'",
    );
}

#[test]
fn frame_zoder_route_valid_call_fails_soft_on_empty_corpus_no_panic() {
    // Seeded corpus is legitimately empty, so a well-formed route attempt
    // cannot succeed; a stable tool-level isError result ("tool execution
    // failed" prefix) and NO panic/crash is the contract under pinning.
    // (Re-pinned 2026-11-18: this was a protocol -32603 before master moved
    // tool failures to result.isError per MCP.)
    let mut server = Server::spawn_seeded();
    server.handshake();
    server.expect_tool_error(
        &req(
            13,
            "tools/call",
            json!({ "name": "zoder_route", "arguments": { "task": "fix the login bug" } }),
        ),
        "tool execution failed",
    );
}

#[test]
fn frame_tools_call_context_unavailable_short_circuits_every_class() {
    // With no model_corpus.json on disk the routing context is None, and
    // tools/call checks it FIRST — so none of the deeper arg classes
    // (-32602, unknown-tool -32601) are reachable in a bare env; every call
    // fails soft with the same tool-level isError result. (Re-pinned
    // 2026-11-18: this was a protocol -32603 before master moved tool
    // failures to result.isError per MCP.) The seeded tests above pin what
    // happens once the context exists.
    let mut server = Server::spawn();
    server.handshake();
    for frame in [
        req(
            20,
            "tools/call",
            json!({ "name": "zoder_route", "arguments": { "task": "x" } }),
        ),
        req(21, "tools/call", json!({ "name": "tp7_no_such_tool" })),
        req(22, "tools/call", json!(7)),
    ] {
        server.expect_tool_error(&frame, "routing context not available");
    }
}

// ───────────────────────────────────────────────────────────── SOP tools ──

#[test]
fn frame_sop_wrapper_argument_classes_are_stable() {
    let mut server = Server::spawn_seeded();
    server.handshake();
    // sop_execute: absent/non-string name -> parse failure; whitespace-only
    // name parses fine and is rejected by the emptiness filter (two distinct
    // messages, one class)
    for (bad, substr) in [
        (
            json!({ "name": "sop_execute", "arguments": {} }),
            "name parameter is required and must be non-empty",
        ),
        (
            json!({ "name": "sop_execute", "arguments": { "name": "  " } }),
            "name parameter is required and must be non-empty",
        ),
        (
            json!({ "name": "sop_execute", "arguments": { "name": 7 } }),
            "invalid sop_execute arguments",
        ),
    ] {
        server.send(&req(14, "tools/call", bad));
        let resp = server.read();
        assert_eq!(resp["error"]["code"], -32602, "{resp}");
        assert!(
            resp["error"]["message"].as_str().unwrap().contains(substr),
            "{resp}"
        );
    }
    // sop_advance without the full name/run_id/decision triple
    for bad in [
        json!({ "name": "sop_advance", "arguments": {} }),
        json!({ "name": "sop_advance", "arguments": { "name": "s" } }),
        json!({ "name": "sop_advance", "arguments": { "name": "s", "run_id": "r" } }),
    ] {
        server.expect_error(
            &req(15, "tools/call", bad),
            -32602,
            "sop_advance requires name, run_id, and decision",
        );
    }
}

#[test]
fn frame_sop_wrappers_forward_and_fail_into_tool_error_not_unknown_tool() {
    // Seeded env (context loads) but no daemon socket: the forward fails and
    // the contract is a tool-level isError result with the tool name as
    // prefix — never "unknown tool", never a protocol error, never a panic
    // (FB92 followup context). (Re-pinned 2026-11-18: this was a protocol
    // -32603 before master moved tool failures to result.isError per MCP.)
    let mut server = Server::spawn_seeded();
    server.handshake();
    for (tool, args) in [
        ("sop_list", json!({})),
        (
            "sop_execute",
            json!({ "name": "my-sop", "payload": "{\"k\":1}" }),
        ),
        (
            "sop_advance",
            json!({ "name": "my-sop", "run_id": "run-1", "decision": "approve" }),
        ),
        (
            "sop_status",
            json!({ "sop_name": "my-sop", "run_id": "run-1" }),
        ),
    ] {
        let frame = req(16, "tools/call", json!({ "name": tool, "arguments": args }));
        server.expect_tool_error(&frame, &format!("{tool} failed: "));
    }
}

// ─────────────────────────────────────────────────── F1: cancelled calls ──

#[test]
fn frame_cancel_request_is_not_mapped_to_jsonrpc_32800() {
    // F1 characterisation: JSON-RPC's -32800 Cancelled is not implemented.
    // A cancellation REQUEST surfaces as "method not found" today — that is
    // what this pins; changing it is a production change (out of scope).
    let mut server = Server::spawn();
    server.handshake();
    server.expect_error(
        &req(17, "$/cancelRequest", json!({ "requestId": 3 })),
        -32601,
        "method not found",
    );
}
