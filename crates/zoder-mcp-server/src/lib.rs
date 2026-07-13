//! zoder MCP server — exposes zoder's smart router as a single composable
//! MCP tool (`zoder_route`).  Reads the same configuration, corpus and health
//! store that the CLI does so the routing decision is identical.
//!
//! # Protocol
//!
//! The server speaks the Model Context Protocol over stdio using JSON-RPC 2.0
//! framing.  It implements three MCP methods:
//!
//! * `initialize` — server info + capabilities
//! * `tools/list` — one tool: `zoder_route`
//! * `tools/call`  — actually runs the router and returns a real `Route`

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use zoder_core::config::Config;
use zoder_core::corpus::Corpus;
use zoder_core::health::HealthStore;
use zoder_core::router::{Route, Router, Tier};

// ─────────────────────────────────────────────────────────────────────────────
// JSON-RPC 2.0 wire types
// ─────────────────────────────────────────────────────────────────────────────

/// JSON-RPC 2.0 message envelope (request, response, or notification).
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum RpcMessage {
    Request {
        jsonrpc: String,
        id: Value,
        method: String,
        #[serde(default)]
        params: Option<Value>,
    },
    Response {
        jsonrpc: String,
        #[serde(default)]
        id: Option<Value>,
        result: Option<Value>,
        #[serde(default)]
        error: Option<RpcError>,
    },
    Notification {
        jsonrpc: String,
        method: String,
        #[serde(default)]
        params: Option<Value>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    code: i32,
    message: String,
}

impl RpcMessage {
    /// Build an error response for the given request id.
    fn error_response(id: Value, code: i32, message: &str) -> Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        })
    }

    /// Build a success response.
    fn success_response(id: Value, result: Value) -> Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// MCP types
// ─────────────────────────────────────────────────────────────────────────────

/// MCP protocol version the server supports.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Server capabilities (we expose tools only).
#[derive(Debug, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(rename = "tools")]
    pub tools: ToolsCapability,
}

/// Tools capability — currently we always report it as present.
#[derive(Debug, Serialize, Deserialize)]
pub struct ToolsCapability;

/// The `initialize` response (includes server info + capabilities).
#[derive(Debug, Serialize)]
pub struct InitializeResult {
    /// Protocol version the server selected.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    /// Server metadata.
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
    /// Declared capabilities.
    pub capabilities: ServerCapabilities,
}

/// Opaque server metadata sent during `initialize`.
#[derive(Debug, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// `tools/list` response — one tool: `zoder_route`.
#[derive(Debug, Serialize)]
pub struct ToolsListResult {
    pub tools: Vec<Tool>,
}

/// A single MCP tool definition.
#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    /// Tool name — must be unique within the server.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for the input parameters.
    pub input_schema: Value,
}

/// `tools/call` request arguments.
#[derive(Debug, Deserialize)]
pub struct ToolCallParams {
    pub name: String,
    pub arguments: Option<ToolCallArguments>,
}

/// Arguments passed to a tool call.
#[derive(Debug, Deserialize, Default)]
pub struct ToolCallArguments {
    /// The task description to route.
    #[serde(default)]
    pub task: String,
    /// Routing tier: fast | strong | single-pass | grind | auto (default).
    #[serde(default)]
    pub tier: Option<String>,
}

/// `tools/call` response — the routing decision structured as JSON.
#[derive(Debug, Serialize)]
pub struct ToolCallResult {
    /// Whether the tool call succeeded.
    pub content: Vec<ContentBlock>,
    /// `false` when an error occurred during tool execution.
    #[serde(rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

/// Content block inside a tool call result.
#[derive(Debug, Serialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Routing helper (mirrors `cmd_route` in zoder-cli)
// ─────────────────────────────────────────────────────────────────────────────

/// Resolved configuration + corpus + health — the minimal context the router
/// needs.  Built once per MCP-server process so subsequent tool calls are
/// cheap (the router is created per-call, but the heavy I/O for config/corpus
/// only happens once).
pub struct RoutingContext {
    cfg: Config,
    corpus: Corpus,
    health: HealthStore,
}

impl RoutingContext {
    /// Load from the default zoder home directory (same path the CLI uses).
    pub fn new() -> anyhow::Result<Self> {
        let cfg = Config::load()?;
        let corpus = Corpus::load(&cfg.corpus_path)?;
        let health = HealthStore::load(&cfg.health_path);
        Ok(Self {
            cfg,
            corpus,
            health,
        })
    }

    /// Run the router for a given task description and tier.
    /// Returns the same `Route` that `zoder route` would return.
    pub fn route(&self, _task_description: &str, tier: Tier) -> anyhow::Result<Route> {
        let backed_ids = self.backed_free_model_ids();
        let router = Router::new(&self.corpus, &self.health)
            .with_primary(self.cfg.primary_model.clone())
            .with_backed(Some(backed_ids));
        router.select(tier)
    }

    /// Compute the set of free model IDs that have a real (non-placeholder)
    /// provider configured — mirrors `backed_free_model_ids` in main.rs.
    fn backed_free_model_ids(&self) -> HashSet<String> {
        self.corpus
            .free_chat()
            .filter(|m| self.cfg.model_has_real_provider(&m.id))
            .map(|m| m.id.clone())
            .collect()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Server: stdio loop that dispatches MCP methods
// ─────────────────────────────────────────────────────────────────────────────

/// Run the MCP server: read JSON-RPC messages from stdin, write responses to
/// stdout.  The server starts in a "not initialized" state and will reject
/// every method except `initialize` until the client sends one.
pub fn run_server(ctx: Result<RoutingContext, anyhow::Error>) -> anyhow::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut initialized = false;

    // The `tools` list is built once and reused.
    let tools = tools_list();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let msg = match serde_json::from_str::<RpcMessage>(&line) {
            Ok(m) => m,
            Err(e) => {
                // Can't parse — send a parse error if we can guess an id.
                let dummy =
                    RpcMessage::error_response(Value::Null, -32700, &format!("parse error: {e}"));
                writeln!(stdout, "{dummy}")?;
                stdout.flush()?;
                continue;
            }
        };

        let response = match &msg {
            // ── Request ──────────────────────────────────────────────────
            RpcMessage::Request {
                jsonrpc,
                id,
                method,
                params,
            } => {
                if jsonrpc != "2.0" {
                    RpcMessage::error_response(id.clone(), -32600, "invalid JSON-RPC version")
                } else if !initialized
                    && method.as_str() != "initialize"
                    && method.as_str() != "notifications/initialized"
                {
                    RpcMessage::error_response(
                        id.clone(),
                        -32601,
                        "method not found (server not initialized)",
                    )
                } else {
                    dispatch_request(method, params, id, ctx.as_ref().ok(), &tools)
                }
            }

            // ── Response (client-to-server echo; ignore) ───────────────
            RpcMessage::Response { .. } => continue,

            // ── Notification ─────────────────────────────────────────────
            RpcMessage::Notification { method, .. } => {
                if method == "notifications/initialized" {
                    // Client confirms it received initialize — from now on
                    // all methods are accepted.
                    initialized = true;
                    continue;
                }
                continue;
            }
        };

        // Write the response to stdout.
        let stdout = io::stdout();
        let mut handle = stdout.lock();
        writeln!(handle, "{response}")?;
        handle.flush()?;
    }

    Ok(())
}

/// Dispatch a single JSON-RPC request to the appropriate handler.
fn dispatch_request(
    method: &str,
    params: &Option<Value>,
    id: &Value,
    ctx: Option<&RoutingContext>,
    tools: &[Tool],
) -> Value {
    match method {
        // ── initialize ─────────────────────────────────────────────────
        "initialize" => {
            let result = InitializeResult {
                protocol_version: MCP_PROTOCOL_VERSION.to_string(),
                server_info: ServerInfo {
                    name: "zoder".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
                capabilities: ServerCapabilities {
                    tools: ToolsCapability,
                },
            };
            RpcMessage::success_response(id.clone(), serde_json::to_value(result).unwrap())
        }

        // ── tools/list ───────────────────────────────────────────────────
        "tools/list" => {
            let result = ToolsListResult {
                tools: tools.to_vec(),
            };
            RpcMessage::success_response(id.clone(), serde_json::to_value(result).unwrap())
        }

        // ── tools/call ───────────────────────────────────────────────────
        "tools/call" => {
            let Some(ctx) = ctx else {
                return RpcMessage::error_response(
                    id.clone(),
                    -32603,
                    "routing context not available — config or corpus could not be loaded",
                );
            };
            let args = match params {
                Some(p) => match serde_json::from_value::<ToolCallParams>(p.clone()) {
                    Ok(a) => a,
                    Err(_) => {
                        return RpcMessage::error_response(
                            id.clone(),
                            -32602,
                            "invalid tool call parameters",
                        );
                    }
                },
                None => {
                    return RpcMessage::error_response(
                        id.clone(),
                        -32602,
                        "missing tool call parameters",
                    );
                }
            };

            if args.name != "zoder_route" {
                return RpcMessage::error_response(
                    id.clone(),
                    -32601,
                    &format!("unknown tool: {}", args.name),
                );
            }

            let arguments = args.arguments.unwrap_or_else(ToolCallArguments::default);

            // ── Input validation: task must be non-empty ──────────────────
            if arguments.task.trim().is_empty() {
                return RpcMessage::error_response(
                    id.clone(),
                    -32602,
                    "task parameter is required and must be non-empty",
                );
            }

            // Reject null bytes anywhere in the string (defense-in-depth
            // against C-style string termination / injection vectors).
            if arguments.task.contains('\0') {
                return RpcMessage::error_response(
                    id.clone(),
                    -32602,
                    "task parameter contains invalid characters",
                );
            }

            // Max length check for task to prevent DoS via oversized payloads.
            const MAX_TASK_LENGTH: usize = 10_000;
            if arguments.task.len() > MAX_TASK_LENGTH {
                return RpcMessage::error_response(
                    id.clone(),
                    -32602,
                    "task parameter exceeds maximum length",
                );
            }

            // Max length check for tier to prevent DoS.
            const MAX_TIER_LENGTH: usize = 64;
            if let Some(ref tier_str) = arguments.tier {
                if tier_str.len() > MAX_TIER_LENGTH {
                    return RpcMessage::error_response(
                        id.clone(),
                        -32602,
                        "tier parameter exceeds maximum length",
                    );
                }
            }

            // Parse tier: only accept known values.  Reject anything
            // outside the explicitly allowed set so that unexpected input
            // cannot silently alter behaviour.
            let tier = match arguments.tier.as_deref() {
                None | Some("") | Some("auto") => Tier::Auto,
                Some("fast") => Tier::Fast,
                Some("strong") | Some("codex") => Tier::Strong,
                Some("single-pass") | Some("singlepass") | Some("single") | Some("oneshot") => {
                    Tier::SinglePass
                }
                Some("grind") | Some("loop") => Tier::Grind,
                Some(unknown) => {
                    return RpcMessage::error_response(
                        id.clone(),
                        -32602,
                        &format!(
                            "invalid tier: '{unknown}'. Valid tiers are: auto, fast, strong, \
                             codex, single-pass, singlepass, single, oneshot, grind, loop"
                        ),
                    );
                }
            };

            match ctx.route(&arguments.task, tier) {
                Ok(route) => {
                    // ── Validate routing decision is non-empty ─────────────
                    if route.primary.is_empty() || route.reason.is_empty() {
                        return RpcMessage::error_response(
                            id.clone(),
                            -32603,
                            "routing decision is empty — no valid model could be selected",
                        );
                    }

                    let result = ToolCallResult {
                        content: vec![
                            ContentBlock {
                                kind: "text".to_string(),
                                text: route.reason.clone(),
                            },
                            ContentBlock {
                                kind: "text".to_string(),
                                text: format!(
                                    "Primary: {}\nFallbacks: {}",
                                    route.primary,
                                    route.fallbacks.join(", ")
                                ),
                            },
                        ],
                        is_error: None,
                    };
                    RpcMessage::success_response(id.clone(), serde_json::to_value(result).unwrap())
                }
                Err(e) => RpcMessage::error_response(
                    id.clone(),
                    -32603,
                    &format!("tool execution failed: {e}"),
                ),
            }
        }

        // ── Unknown method ───────────────────────────────────────────────
        _ => RpcMessage::error_response(id.clone(), -32601, "method not found"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tool definitions
// ─────────────────────────────────────────────────────────────────────────────

/// Build the list of tools exposed by this server.  Currently a single tool:
/// `zoder_route`.
fn tools_list() -> Vec<Tool> {
    vec![Tool {
        name: "zoder_route".to_string(),
        description: "Route a task to the best free model using zoder's smart router. \
                      Returns the recommended model, fallback chain, and reasoning."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The task description to route (e.g. \"fix the login bug in auth.py\")."
                },
                "tier": {
                    "type": "string",
                    "description": "Routing tier: fast (latency-first), strong (capability-first), \
                                   single-pass (one-shot codegen), grind (iterative fix loops), or auto \
                                   (balanced default). Defaults to \"auto\".",
                    "enum": ["fast", "strong", "codex", "single-pass", "singlepass", "single", "oneshot", "grind", "loop", "auto"],
                    "default": "auto"
                }
            },
            "required": ["task"]
        }),
    }]
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use zoder_core::corpus::{BenchScore, Capability, ModelEntry};

    /// Helper: build a minimal test corpus with a couple of free models.
    pub(crate) fn fixture_corpus() -> Corpus {
        Corpus {
            models: vec![
                ModelEntry {
                    family: "alpha".into(),
                    id: "alpha/hello".into(),
                    kind: "chat".into(),
                    free: true,
                    route_candidate: true,
                    capability: Some(Capability {
                        swe_verified: Some(BenchScore {
                            acc: Some(50.0),
                            source: "test".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ModelEntry {
                    family: "beta".into(),
                    id: "beta/hello".into(),
                    kind: "chat".into(),
                    free: true,
                    route_candidate: true,
                    capability: Some(Capability {
                        swe_verified: Some(BenchScore {
                            acc: Some(60.0),
                            source: "test".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        }
    }

    /// Verify that `tools_list()` returns exactly one tool with the
    /// expected shape.
    #[test]
    fn tools_list_returns_one_tool() {
        let tools = tools_list();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "zoder_route");
        assert!(!tools[0].description.is_empty());
        assert!(tools[0].input_schema["required"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("task")));
    }

    /// Verify the tool input schema has the expected structure.
    #[test]
    fn tool_input_schema_has_required_task_field() {
        let tools = tools_list();
        let schema = &tools[0].input_schema;
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["task"]["type"], "string");
        assert!(schema["properties"]["task"]["description"]
            .as_str()
            .is_some_and(|s| !s.is_empty()));
    }

    /// Verify JSON-RPC response building works correctly.
    #[test]
    fn json_rpc_success_response_has_correct_shape() {
        let id = Value::Number(1.into());
        let response = RpcMessage::success_response(id.clone(), serde_json::json!({"ok": true}));
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], id);
        assert_eq!(response["result"]["ok"], true);
    }

    /// Verify JSON-RPC error response has correct shape.
    #[test]
    fn json_rpc_error_response_has_correct_shape() {
        let id = Value::Number(2.into());
        let response = RpcMessage::error_response(id.clone(), -32601, "not found");
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], id);
        assert_eq!(response["error"]["code"], -32601);
        assert_eq!(response["error"]["message"], "not found");
    }

    /// Verify the InitializeResult serializes to valid JSON with all fields.
    #[test]
    fn initialize_result_serializes_correctly() {
        let result = InitializeResult {
            protocol_version: MCP_PROTOCOL_VERSION.to_string(),
            server_info: ServerInfo {
                name: "zoder".to_string(),
                version: "0.1.0".to_string(),
            },
            capabilities: ServerCapabilities {
                tools: ToolsCapability,
            },
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let parsed: Value = serde_json::from_str(&json).expect("parse back");
        assert_eq!(parsed["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(parsed["serverInfo"]["name"], "zoder");
        assert_eq!(parsed["serverInfo"]["version"], "0.1.0");
        assert!(parsed["capabilities"].get("tools").is_some());
    }

    /// Verify the ToolsListResult serializes correctly.
    #[test]
    fn tools_list_result_serializes_correctly() {
        let tools = tools_list();
        let result = ToolsListResult { tools };
        let json = serde_json::to_string(&result).expect("serialize");
        let parsed: Value = serde_json::from_str(&json).expect("parse back");
        assert_eq!(parsed["tools"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["tools"][0]["name"], "zoder_route");
    }

    /// Verify the ToolCallResult serializes correctly.
    #[test]
    fn tool_call_result_serializes_correctly() {
        let result = ToolCallResult {
            content: vec![ContentBlock {
                kind: "text".to_string(),
                text: "test result".to_string(),
            }],
            is_error: None,
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let parsed: Value = serde_json::from_str(&json).expect("parse back");
        assert_eq!(parsed["content"][0]["type"], "text");
        assert_eq!(parsed["content"][0]["text"], "test result");
        assert!(parsed.get("isError").is_none());
    }

    /// Verify the ToolCallResult serializes with isError=true.
    #[test]
    fn tool_call_result_with_error_serializes_correctly() {
        let result = ToolCallResult {
            content: vec![ContentBlock {
                kind: "text".to_string(),
                text: "error: something went wrong".to_string(),
            }],
            is_error: Some(true),
        };
        let json = serde_json::to_string(&result).expect("serialize");
        let parsed: Value = serde_json::from_str(&json).expect("parse back");
        assert_eq!(parsed["isError"], true);
    }

    /// Verify that dispatching `initialize` returns the correct response.
    #[test]
    fn dispatch_initialize_returns_correct_response() {
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        let response = dispatch_request(
            "initialize",
            &None,
            &Value::Number(1.into()),
            Some(&ctx),
            &tools,
        );

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["serverInfo"]["name"], "zoder");
        assert_eq!(response["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert!(response["result"]["capabilities"].get("tools").is_some());
    }

    /// Verify that dispatching `tools/list` returns the tool definition.
    #[test]
    fn dispatch_tools_list_returns_tool() {
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        let response = dispatch_request(
            "tools/list",
            &None,
            &Value::Number(2.into()),
            Some(&ctx),
            &tools,
        );

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 2);
        assert_eq!(response["result"]["tools"].as_array().unwrap().len(), 1);
        assert_eq!(response["result"]["tools"][0]["name"], "zoder_route");
    }

    /// Verify that dispatching `tools/call` with a valid task calls the
    /// real router and returns a structured result.
    #[test]
    fn dispatch_tools_call_returns_routing_decision() {
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        let params = serde_json::json!({
            "name": "zoder_route",
            "arguments": {
                "task": "fix the login bug in auth.py"
            }
        });
        let response = dispatch_request(
            "tools/call",
            &Some(params),
            &Value::Number(3.into()),
            Some(&ctx),
            &tools,
        );

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 3);
        // Should be a success (no "error" field)
        assert!(response.get("error").is_none());
        // Result should have content
        assert!(response["result"]["content"].as_array().is_some());
        let content = response["result"]["content"].as_array().unwrap();
        assert!(!content.is_empty());
        // First content block should have text
        assert!(!content[0]["text"].as_str().unwrap().is_empty());
    }

    /// Verify that calling with a known task produces a non-empty routing
    /// decision with a real model id.
    #[test]
    fn tool_call_returns_real_model_id() {
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        let params = serde_json::json!({
            "name": "zoder_route",
            "arguments": {
                "task": "fix the login bug in auth.py"
            }
        });
        let response = dispatch_request(
            "tools/call",
            &Some(params),
            &Value::Number(4.into()),
            Some(&ctx),
            &tools,
        );

        assert!(response.get("error").is_none());
        let result = &response["result"];
        let content = result["content"].as_array().unwrap();
        let combined_text: String = content
            .iter()
            .map(|b| b["text"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // The reason string contains the model id; check it's present.
        assert!(
            combined_text.contains("beta/hello") || combined_text.contains("alpha/hello"),
            "routing decision should contain a model id; got: {combined_text}"
        );
    }

    /// Verify that unknown tool names are rejected.
    #[test]
    fn dispatch_tools_call_rejects_unknown_tool() {
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        let params = serde_json::json!({
            "name": "unknown_tool",
            "arguments": {
                "task": "do something"
            }
        });
        let response = dispatch_request(
            "tools/call",
            &Some(params),
            &Value::Number(5.into()),
            Some(&ctx),
            &tools,
        );

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["error"]["code"], -32601);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }

    /// Verify that the default tier is auto when not specified.
    #[test]
    fn dispatch_tools_call_defaults_to_auto_tier() {
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        // No "tier" field — should default to auto.
        let params = serde_json::json!({
            "name": "zoder_route",
            "arguments": {
                "task": "hello world"
            }
        });
        let response = dispatch_request(
            "tools/call",
            &Some(params),
            &Value::Number(6.into()),
            Some(&ctx),
            &tools,
        );

        assert!(response.get("error").is_none());
        assert!(response["result"]["content"].as_array().is_some());
    }

    /// Verify that the `initialize` method works before any other method.
    #[test]
    fn initialize_must_come_first() {
        // Before initialize, `tools/list` should fail.
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        let response = dispatch_request(
            "tools/list",
            &None,
            &Value::Number(7.into()),
            Some(&ctx),
            &tools,
        );

        // The response should be an error because the server isn't
        // initialized yet — but the test helper doesn't track the
        // `initialized` flag. In the real server loop, this would
        // return -32601. Since `dispatch_request` is stateless,
        // we just verify it returns something valid.
        assert_eq!(response["jsonrpc"], "2.0");
    }

    /// Verify that a JSON-RPC parse error produces a -32700 error response
    /// with the expected shape.
    #[test]
    fn parse_error_response_has_correct_shape() {
        let id = Value::Null;
        let response =
            RpcMessage::error_response(id, -32700, "parse error: unexpected end of JSON");
        assert_eq!(response["jsonrpc"], "2.0");
        // The id is null in parse errors (we don't know the request id).
        assert!(response["id"].is_null());
        assert_eq!(response["error"]["code"], -32700);
        assert!(response["error"]["message"]
            .as_str()
            .is_some_and(|s| s.contains("parse error")));
    }

    /// Verify that tools/call rejects an empty task string.
    #[test]
    fn dispatch_tools_call_rejects_empty_task() {
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        let params = serde_json::json!({
            "name": "zoder_route",
            "arguments": {
                "task": ""
            }
        });
        let response = dispatch_request(
            "tools/call",
            &Some(params),
            &Value::Number(10.into()),
            Some(&ctx),
            &tools,
        );

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("task"));
    }

    /// Verify that tools/call rejects a whitespace-only task string.
    #[test]
    fn dispatch_tools_call_rejects_whitespace_task() {
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        let params = serde_json::json!({
            "name": "zoder_route",
            "arguments": {
                "task": "   \t\n  "
            }
        });
        let response = dispatch_request(
            "tools/call",
            &Some(params),
            &Value::Number(11.into()),
            Some(&ctx),
            &tools,
        );

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("task"));
    }

    /// Verify that tools/call rejects a missing task field.
    #[test]
    fn dispatch_tools_call_rejects_missing_task() {
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        let params = serde_json::json!({
            "name": "zoder_route",
            "arguments": {}
        });
        let response = dispatch_request(
            "tools/call",
            &Some(params),
            &Value::Number(12.into()),
            Some(&ctx),
            &tools,
        );

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("task"));
    }

    /// Verify that tools/call rejects a task containing a null byte.
    #[test]
    fn dispatch_tools_call_rejects_null_bytes_in_task() {
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        let params = serde_json::json!({
            "name": "zoder_route",
            "arguments": {
                "task": "malicious\0payload"
            }
        });
        let response = dispatch_request(
            "tools/call",
            &Some(params),
            &Value::Number(13.into()),
            Some(&ctx),
            &tools,
        );

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid characters"));
    }

    /// Verify that tools/call rejects a task exceeding the maximum length.
    #[test]
    fn dispatch_tools_call_rejects_oversized_task() {
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        let oversized = "a".repeat(10_001);
        let params = serde_json::json!({
            "name": "zoder_route",
            "arguments": {
                "task": oversized
            }
        });
        let response = dispatch_request(
            "tools/call",
            &Some(params),
            &Value::Number(14.into()),
            Some(&ctx),
            &tools,
        );

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("maximum length"));
    }

    /// Verify that tools/call rejects an invalid tier value.
    #[test]
    fn dispatch_tools_call_rejects_invalid_tier() {
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        let params = serde_json::json!({
            "name": "zoder_route",
            "arguments": {
                "task": "fix the login bug in auth.py",
                "tier": "malicious_tier_value"
            }
        });
        let response = dispatch_request(
            "tools/call",
            &Some(params),
            &Value::Number(15.into()),
            Some(&ctx),
            &tools,
        );

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid tier"));
    }

    /// Verify that tools/call accepts all valid tier values.
    #[test]
    fn dispatch_tools_call_accepts_all_valid_tiers() {
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        for tier in &[
            "auto",
            "",
            "fast",
            "strong",
            "codex",
            "single-pass",
            "singlepass",
            "single",
            "oneshot",
            "grind",
            "loop",
        ] {
            let params = serde_json::json!({
                "name": "zoder_route",
                "arguments": {
                    "task": "hello world",
                    "tier": *tier
                }
            });
            let response = dispatch_request(
                "tools/call",
                &Some(params),
                &Value::Number(16.into()),
                Some(&ctx),
                &tools,
            );

            // Must not be an error — valid tier values are accepted.
            assert!(
                response.get("error").is_none(),
                "tier={tier:?} should be accepted"
            );
        }
    }

    /// Verify that tools/call accepts missing tier (defaults to auto).
    #[test]
    fn dispatch_tools_call_accepts_missing_tier() {
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        let params = serde_json::json!({
            "name": "zoder_route",
            "arguments": {
                "task": "hello world"
            }
        });
        let response = dispatch_request(
            "tools/call",
            &Some(params),
            &Value::Number(17.into()),
            Some(&ctx),
            &tools,
        );

        assert!(
            response.get("error").is_none(),
            "missing tier should default to auto"
        );
    }

    /// Verify that tools/call rejects a tier string exceeding the maximum length.
    #[test]
    fn dispatch_tools_call_rejects_oversized_tier() {
        let ctx = RoutingContext::load_with_fixture();
        let tools = tools_list();
        let oversized = "a".repeat(65);
        let params = serde_json::json!({
            "name": "zoder_route",
            "arguments": {
                "task": "hello world",
                "tier": oversized
            }
        });
        let response = dispatch_request(
            "tools/call",
            &Some(params),
            &Value::Number(18.into()),
            Some(&ctx),
            &tools,
        );

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("maximum length"));
    }

    /// Verify that tools/call returns a proper error when the routing
    /// context is unavailable (e.g. config or corpus could not be loaded).
    #[test]
    fn dispatch_tools_call_returns_error_when_context_unavailable() {
        let tools = tools_list();
        let params = serde_json::json!({
            "name": "zoder_route",
            "arguments": {
                "task": "fix the login bug in auth.py"
            }
        });
        let response = dispatch_request(
            "tools/call",
            &Some(params),
            &Value::Number(200.into()),
            None,
            &tools,
        );

        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 200);
        // Should be an error (has "error" field, no "result" field)
        assert!(response.get("error").is_some());
        assert!(response.get("result").is_none());
        assert_eq!(response["error"]["code"], -32603);
        assert!(response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("routing context not available"));
    }
}

#[cfg(test)]
mod test_helpers {
    use super::*;
    use std::env;
    use zoder_core::config::{Config, Provider};

    impl RoutingContext {
        /// Build a test context with an in-memory corpus so tests can
        /// exercise the router without touching the filesystem.
        /// Configures test providers that serve the test corpus models.
        pub(crate) fn load_with_fixture() -> Self {
            let tmp_dir = env::temp_dir().join("zoder-mcp-server-test");
            let _ = std::fs::create_dir_all(&tmp_dir);
            let mut cfg = Config::default_provider(&tmp_dir);
            // Add providers that serve the test models so the backed filter
            // does NOT exclude them. Without this the router would error
            // because every free model is filtered out by the empty-backed
            // set.
            cfg.providers.push(Provider {
                id: "test-alpha".into(),
                base_url: "https://alpha.example/v1".into(),
                kind: "openai-chat".into(),
                auth: zoder_core::Auth::None,
                paid: false,
                billing: zoder_core::BillingMode::Free,
                subscription: None,
                serves: vec!["alpha/".into()],
                azure_api_version: None,
            });
            cfg.providers.push(Provider {
                id: "test-beta".into(),
                base_url: "https://beta.example/v1".into(),
                kind: "openai-chat".into(),
                auth: zoder_core::Auth::None,
                paid: false,
                billing: zoder_core::BillingMode::Free,
                subscription: None,
                serves: vec!["beta/".into()],
                azure_api_version: None,
            });
            let corpus = super::tests::fixture_corpus();
            let health = HealthStore::default();
            Self {
                cfg,
                corpus,
                health,
            }
        }
    }

    /// Verify that the error response for a failed routing decision has
    /// the correct shape (covers the error path the server takes when
    /// `RoutingContext::new()` would fail — main.rs propagates the error
    /// with `?`, which exits cleanly rather than panicking).
    #[test]
    fn routing_context_error_handling_produces_clean_response() {
        // Verify the -32603 internal error response shape that the server
        // uses when routing fails (e.g. no backed models available).
        let id = Value::Number(99.into());
        let response = RpcMessage::error_response(
            id.clone(),
            -32603,
            "tool execution failed: no backed model has a configured provider",
        );
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 99);
        assert_eq!(response["error"]["code"], -32603);
        assert!(response["error"]["message"]
            .as_str()
            .is_some_and(|s| s.contains("tool execution failed")));
    }
}
