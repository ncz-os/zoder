//! Agentic driver for the local zeroclaw engine over its Unix-socket JSON-RPC
//! (the "rpc" surface; the gateway also exposes the same lifecycle over wss).
//!
//! This is what turns `zoder/zoder exec` from a single-shot chat completion
//! into a real codex-style agentic loop: it drives a zeroclaw agent session
//! (tool use, file edits, build/test) in a working directory and streams the
//! turn back, auto-answering tool-approval requests under a policy.
//!
//! Lifecycle (mirrors zeroclaw `crates/zeroclaw-runtime/src/rpc/dispatch.rs`):
//!   connect -> `initialize` -> `session/new {agent_alias,cwd,chat_mode:"acp"}`
//!   -> (optional) `session/configure {overrides:{model}}`
//!   -> `session/prompt {session_id,prompt}` (returns `{}` immediately)
//!   -> consume `session/update` notifications until `turn_complete`
//!   -> approvals answered with `session/approve`.
//!
//! Cost/usage is written authoritatively by the engine's own cost tracker into
//! the shared ledger the dashboard reads; the caller reconciles exact `cost_usd`
//! via `engine_cost::fetch_engine_cost` (a windowed `cost/query`). Token counts
//! surfaced here (from `context_usage`) are best-effort for live display.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// ACP protocol version the daemon's `initialize` expects (matches
/// `zeroclaw_api::jsonrpc` and `engine_cost.rs`).
const ACP_PROTOCOL_VERSION: u64 = 1;

/// Tools auto-approved under [`ApprovalPolicy::Allowlist`] — **READ-ONLY only**.
/// Write/exec tools (`shell`, `bash`, `edit`, `apply_patch`, `write`, `git`, …)
/// are deliberately NOT here: they require explicit [`ApprovalPolicy::All`]
/// (`--approve all`), so an unattended run defaults to a safe read-only posture.
/// Matched EXACTLY (see [`decide_approval`]) — never by substring — so a tool
/// like `dangerous_shell_proxy` or `delete_file_write` can't slip through on a
/// substring of `shell`/`write`.
pub const DEFAULT_AUTO_APPROVE: &[&str] = &[
    "read",
    "read_file",
    "list_files",
    "list",
    "glob",
    "grep",
    "search",
];

/// What to do with a tool-approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Approve every tool call (fully autonomous; for CI/headless coding).
    All,
    /// Approve only tools on the allowlist; deny the rest.
    Allowlist,
    /// Deny everything (read-only review runs).
    None,
}

/// Inputs for one agentic turn.
#[derive(Debug, Clone)]
pub struct AgentOptions {
    /// Unix socket of the running daemon.
    pub socket: PathBuf,
    /// Configured agent alias (`[agents.<alias>]`) to run as.
    pub agent_alias: String,
    /// Working directory (repo root) for the run.
    pub cwd: PathBuf,
    /// The user prompt / task.
    pub prompt: String,
    /// Optional per-session model override (rpc-only; for models without an alias).
    pub model_override: Option<String>,
    /// Resume an existing session id (None = new session).
    pub session_id: Option<String>,
    /// Stream `agent_thought_chunk` (reasoning) too.
    pub show_reasoning: bool,
    /// Tool-approval policy.
    pub approval: ApprovalPolicy,
    /// Overall wall-clock budget for the turn.
    pub timeout: Duration,
}

impl AgentOptions {
    pub fn new(
        socket: impl Into<PathBuf>,
        agent_alias: impl Into<String>,
        cwd: impl Into<PathBuf>,
        prompt: impl Into<String>,
    ) -> Self {
        Self {
            socket: socket.into(),
            agent_alias: agent_alias.into(),
            cwd: cwd.into(),
            prompt: prompt.into(),
            model_override: None,
            session_id: None,
            show_reasoning: false,
            approval: ApprovalPolicy::Allowlist,
            timeout: Duration::from_secs(900),
        }
    }
}

/// Streaming events surfaced to the caller during a turn (for live output).
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Assistant message text delta.
    Text(String),
    /// Reasoning/thinking delta.
    Thought(String),
    /// Model invoked a tool.
    ToolCall { name: String },
    /// Tool produced output.
    ToolResult { name: String },
    /// A tool-approval request was auto-handled.
    Approval { tool: String, approved: bool },
    /// Per-LLM-call context/token usage.
    Usage { input_tokens: u64 },
}

/// Outcome of an agentic turn.
#[derive(Debug, Clone)]
pub struct AgentRun {
    pub session_id: String,
    /// `completed` | `cancelled` | `failed`.
    pub outcome: String,
    /// Final assistant content (concatenated text chunks; `content` on
    /// `turn_complete` wins if present).
    pub content: String,
    /// Best-effort prompt-token high-water mark (from `context_usage`).
    pub input_tokens: u64,
    /// Number of tool calls observed.
    pub tool_calls: u32,
}

impl AgentRun {
    pub fn succeeded(&self) -> bool {
        self.outcome == "completed"
    }
}

/// Poll-connect to the daemon socket until it accepts a connection or `budget`
/// elapses. Useful right after spawning an ephemeral daemon.
pub async fn wait_for_socket(socket: &Path, budget: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        match UnixStream::connect(socket).await {
            Ok(_) => return Ok(()),
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => {
                return Err(anyhow!(
                    "daemon socket {} not ready within {:?}: {e}",
                    socket.display(),
                    budget
                ))
            }
        }
    }
}

/// Prove the daemon is actually answering RPC, not merely accepting the socket:
/// connect and complete an `initialize` handshake within [`SETUP_RPC_TIMEOUT`].
/// A socket that accepts the connection but never answers `initialize` (a daemon
/// still starting, wedged, or a stale binding) is NOT ready — readiness checks
/// that only `connect` would wrongly treat it as up and then fail the first real
/// turn.
pub async fn probe_ready(socket: &Path, budget: Duration) -> anyhow::Result<()> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to engine at {}", socket.display()))?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    write_frame(
        &mut write_half,
        &json!({
            "jsonrpc": "2.0",
            "id": "init",
            "method": "initialize",
            "params": { "protocol_version": ACP_PROTOCOL_VERSION },
        }),
    )
    .await?;
    // `budget` bounds the handshake so a socket that accepts but never answers
    // can't pin a startup poll for the full setup timeout (the caller's own
    // deadline / child-exit check stays responsive).
    read_result(&mut reader, "init", budget).await?;
    Ok(())
}

/// Create a fresh engine session bound to `cwd` and return its id (without
/// prompting). Used by `transfer` to hand off a resumable thread.
pub async fn new_session(socket: &Path, agent_alias: &str, cwd: &Path) -> anyhow::Result<String> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to engine at {}", socket.display()))?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    write_frame(
        &mut write_half,
        &json!({
            "jsonrpc": "2.0",
            "id": "init",
            "method": "initialize",
            "params": { "protocol_version": ACP_PROTOCOL_VERSION },
        }),
    )
    .await?;
    read_result(&mut reader, "init", SETUP_RPC_TIMEOUT).await?;
    write_frame(
        &mut write_half,
        &json!({
            "jsonrpc": "2.0",
            "id": "new",
            "method": "session/new",
            "params": {
                "agent_alias": agent_alias,
                "cwd": cwd.to_string_lossy(),
                "chat_mode": "acp",
            },
        }),
    )
    .await?;
    let res = read_result(&mut reader, "new", SETUP_RPC_TIMEOUT).await?;
    res.get("session_id")
        .and_then(Value::as_str)
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("session/new returned no session_id"))
}

/// Drive one agentic turn to completion, invoking `on_event` for each streamed
/// event (for live terminal output). Returns the final outcome.
pub async fn run_agent<F: FnMut(AgentEvent)>(
    opts: &AgentOptions,
    mut on_event: F,
) -> anyhow::Result<AgentRun> {
    // `drive` enforces the real budget internally (a deadline on the streaming
    // loop) and, on timeout, returns the partial turn (streamed text, tool-call
    // count, and the session id for resume) with `outcome = "timeout"` instead
    // of discarding it. On-disk file edits already survive because the engine
    // applies them as tools run; this preserves the rest so a timed-out turn is
    // never "zero output".
    //
    // The outer guard is only a backstop for a socket that hangs during session
    // setup (before any partial exists). Give it headroom so the inner deadline
    // always fires first and partial work survives.
    let backstop = opts.timeout + Duration::from_secs(30);
    let fut = drive(opts, &mut on_event);
    tokio::time::timeout(backstop, fut)
        .await
        .map_err(|_| anyhow!("agentic turn hung during setup; exceeded {:?}", backstop))?
}

async fn drive<F: FnMut(AgentEvent)>(
    opts: &AgentOptions,
    on_event: &mut F,
) -> anyhow::Result<AgentRun> {
    // Wall-clock budget for the whole turn. The streaming loop below stops at
    // this instant and returns whatever has accumulated (outcome="timeout"),
    // rather than the caller dropping the future and losing all partial work.
    let deadline = tokio::time::Instant::now() + opts.timeout;
    let stream = UnixStream::connect(&opts.socket)
        .await
        .with_context(|| format!("connecting to engine at {}", opts.socket.display()))?;
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    // 1. initialize (must be first; no env forwarded -> daemon uses its own
    //    provider config, avoiding API-key clashes).
    write_frame(
        &mut write_half,
        &json!({
            "jsonrpc": "2.0",
            "id": "init",
            "method": "initialize",
            "params": { "protocol_version": ACP_PROTOCOL_VERSION },
        }),
    )
    .await?;
    read_result(&mut reader, "init", SETUP_RPC_TIMEOUT).await?;

    // 2. session/new (acp = Code mode: pins cwd, excludes long-term memory tools).
    let mut new_params = serde_json::Map::new();
    new_params.insert("agent_alias".into(), json!(opts.agent_alias));
    new_params.insert("cwd".into(), json!(opts.cwd.to_string_lossy()));
    new_params.insert("chat_mode".into(), json!("acp"));
    if let Some(sid) = &opts.session_id {
        new_params.insert("session_id".into(), json!(sid));
    }
    write_frame(
        &mut write_half,
        &json!({
            "jsonrpc": "2.0",
            "id": "new",
            "method": "session/new",
            "params": Value::Object(new_params),
        }),
    )
    .await?;
    let new_res = read_result(&mut reader, "new", SETUP_RPC_TIMEOUT).await?;
    let session_id = new_res
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("session/new returned no session_id"))?
        .to_string();

    // 3. optional model override (rpc-only).
    if let Some(model) = &opts.model_override {
        write_frame(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": "cfg",
                "method": "session/configure",
                "params": { "session_id": session_id, "overrides": { "model": model } },
            }),
        )
        .await?;
        // Best-effort: ignore configure failures (older daemons), keep going.
        let _ = read_result(&mut reader, "cfg", SETUP_RPC_TIMEOUT).await;
    }

    // 4. session/prompt — response is `{}`; real output streams as notifications.
    write_frame(
        &mut write_half,
        &json!({
            "jsonrpc": "2.0",
            "id": "prompt",
            "method": "session/prompt",
            "params": { "session_id": session_id, "prompt": opts.prompt },
        }),
    )
    .await?;

    // 5. consume session/update notifications until turn_complete (or the
    //    deadline elapses, in which case we keep the partial turn).
    let mut content = String::new();
    let mut input_tokens = 0u64;
    let mut tool_calls = 0u32;
    let mut line = String::new();
    let outcome: String = loop {
        line.clear();
        let n = match tokio::time::timeout_at(deadline, reader.read_line(&mut line)).await {
            // Budget exhausted. CANCEL the turn server-side and then DRAIN until
            // the engine confirms it wound the turn down — otherwise a ghost
            // agent keeps editing files after we return. The drain also parses
            // any final `turn_complete`/text that races the deadline, so a turn
            // that finished right at the budget isn't mislabeled `timeout` with
            // stale content.
            Err(_) => {
                let _ = write_frame(
                    &mut write_half,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": "cancel",
                        "method": "session/cancel",
                        "params": { "session_id": session_id },
                    }),
                )
                .await;
                break drain_after_cancel(
                    &mut reader,
                    opts.show_reasoning,
                    on_event,
                    &mut content,
                    &mut input_tokens,
                    &mut tool_calls,
                    &mut line,
                )
                .await;
            }
            // A read error AFTER the prompt was accepted is not a setup failure:
            // tools/edits may already have run. Convert it to a partial turn
            // (not an Err) so the caller never re-runs this prompt on another
            // model (which would duplicate side effects) — `run_agent` only
            // returns Err during pre-prompt setup, which the agentic fallback
            // relies on for side-effect safety.
            Ok(Ok(n)) => n,
            Ok(Err(_)) => break "disconnected".to_string(),
        };
        if n == 0 {
            // The engine dropped the socket mid-turn. Do NOT discard the work:
            // the accumulated text, tool count, and session id are real, and any
            // on-disk edits already applied as tools ran. Return them with a
            // non-success outcome so the caller preserves partial output, writes
            // the ledger, and records a HEALTH FAILURE (routing learns this
            // model/alias didn't finish) — instead of bailing, which threw all
            // of that away and surfaced as a generic error with no fallback.
            break "disconnected".to_string();
        }
        let frame: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // JSON-RPC *responses* (an `id`, no `method`). Previously only a
        // `prompt`-id error was treated as fatal and every other error response
        // was silently dropped — so a failed `session/approve` left the engine
        // blocked on approval and the turn stalled until the deadline. Handle
        // every error response.
        if frame.get("method").is_none() {
            if let Some(err) = frame.get("error") {
                let id = frame.get("id").and_then(Value::as_str).unwrap_or("");
                let msg = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                if id == "prompt" {
                    bail!("session/prompt failed: {msg}");
                }
                if id.starts_with("approve-") {
                    // The engine rejected our approval decision: it is still
                    // waiting and the turn cannot progress. Stop now with a
                    // non-success outcome and preserve the partial transcript
                    // rather than hanging until the wall-clock budget.
                    tracing::warn!(%id, %msg, "approval rejected by engine; ending turn");
                    break "failed".to_string();
                }
                tracing::warn!(%id, %msg, "ignoring non-fatal engine error response");
            }
            // Responses are never `session/update`; nothing else to do.
            continue;
        }

        if frame.get("method").and_then(Value::as_str) != Some("session/update") {
            continue;
        }
        let params = frame.get("params").cloned().unwrap_or(Value::Null);
        if params.get("type").and_then(Value::as_str) == Some("approval_request") {
            let req_id = params
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let tool = params
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let approved = decide_approval(opts.approval, &tool);
            let decision = if approved { "approve" } else { "deny" };
            // A write failure here is post-side-effect (tools have already run);
            // preserve the partial turn rather than returning Err (which the
            // caller would treat as a safe-to-retry setup failure).
            if write_frame(
                &mut write_half,
                &json!({
                    "jsonrpc": "2.0",
                    "id": format!("approve-{req_id}"),
                    "method": "session/approve",
                    "params": {
                        "session_id": session_id,
                        "request_id": req_id,
                        "decision": decision,
                    },
                }),
            )
            .await
            .is_err()
            {
                break "failed".to_string();
            }
            on_event(AgentEvent::Approval { tool, approved });
            continue;
        }
        if let Some(oc) = apply_update(
            &params,
            opts.show_reasoning,
            on_event,
            &mut content,
            &mut input_tokens,
            &mut tool_calls,
        ) {
            break oc;
        }
    };

    Ok(AgentRun {
        session_id,
        outcome,
        content,
        input_tokens,
        tool_calls,
    })
}

/// Apply one non-approval `session/update` notification to the running turn
/// accumulators, emitting live events. Returns `Some(outcome)` when the frame
/// is `turn_complete` (the turn is over), else `None`. Approval requests need
/// write access to the socket and are handled by the caller.
fn apply_update<F: FnMut(AgentEvent)>(
    params: &Value,
    show_reasoning: bool,
    on_event: &mut F,
    content: &mut String,
    input_tokens: &mut u64,
    tool_calls: &mut u32,
) -> Option<String> {
    match params.get("type").and_then(Value::as_str).unwrap_or("") {
        "agent_message_chunk" => {
            if let Some(t) = params.get("text").and_then(Value::as_str) {
                content.push_str(t);
                on_event(AgentEvent::Text(t.to_string()));
            }
        }
        "agent_thought_chunk" => {
            if show_reasoning {
                if let Some(t) = params.get("text").and_then(Value::as_str) {
                    on_event(AgentEvent::Thought(t.to_string()));
                }
            }
        }
        "tool_call" => {
            *tool_calls += 1;
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            on_event(AgentEvent::ToolCall { name });
        }
        "tool_result" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            on_event(AgentEvent::ToolResult { name });
        }
        "context_usage" => {
            if let Some(it) = params.get("input_tokens").and_then(Value::as_u64) {
                *input_tokens = (*input_tokens).max(it);
                on_event(AgentEvent::Usage { input_tokens: it });
            }
        }
        "turn_complete" => {
            let oc = params
                .get("outcome")
                .and_then(Value::as_str)
                .unwrap_or("completed")
                .to_string();
            if let Some(c) = params.get("content").and_then(Value::as_str) {
                if !c.is_empty() {
                    *content = c.to_string();
                }
            }
            return Some(oc);
        }
        _ => {}
    }
    None
}

/// After sending `session/cancel`, drain notifications within a bounded grace
/// window so we (a) keep parsing real output that raced the deadline and (b)
/// wait for the engine to confirm the turn wound down. Returns the turn's
/// outcome: the engine's own `turn_complete` outcome if it arrives, else
/// `"timeout"` (cancel requested, not confirmed before the grace window).
#[allow(clippy::too_many_arguments)]
async fn drain_after_cancel<F: FnMut(AgentEvent)>(
    reader: &mut (impl AsyncBufReadExt + Unpin),
    show_reasoning: bool,
    on_event: &mut F,
    content: &mut String,
    input_tokens: &mut u64,
    tool_calls: &mut u32,
    line: &mut String,
) -> String {
    const CANCEL_GRACE: Duration = Duration::from_secs(5);
    let grace_deadline = tokio::time::Instant::now() + CANCEL_GRACE;
    loop {
        line.clear();
        match tokio::time::timeout_at(grace_deadline, reader.read_line(line)).await {
            // Grace window elapsed without a terminal frame: cancel was
            // requested but not confirmed. The caller treats a non-`completed`
            // outcome as a failure and preserves the partial transcript.
            Err(_) => break "timeout".to_string(),
            // Socket closed: the engine is gone, so the turn is definitively
            // over. Whatever streamed so far is preserved.
            Ok(Ok(0)) | Ok(Err(_)) => break "timeout".to_string(),
            Ok(Ok(_)) => {}
        }
        let frame: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if frame.get("method").and_then(Value::as_str) != Some("session/update") {
            continue;
        }
        let params = frame.get("params").cloned().unwrap_or(Value::Null);
        // Skip late approval requests during cancel (we are tearing down); just
        // apply real output + watch for the terminal `turn_complete`.
        if params.get("type").and_then(Value::as_str) == Some("approval_request") {
            continue;
        }
        if let Some(oc) = apply_update(
            &params,
            show_reasoning,
            on_event,
            content,
            input_tokens,
            tool_calls,
        ) {
            // The engine confirmed the turn ended (often `cancelled`); use its
            // real outcome instead of a blanket `timeout`.
            break oc;
        }
    }
}

fn decide_approval(policy: ApprovalPolicy, tool: &str) -> bool {
    match policy {
        ApprovalPolicy::All => true,
        ApprovalPolicy::None => false,
        ApprovalPolicy::Allowlist => {
            // EXACT match only. A substring test would auto-approve hostile or
            // destructive tools whose names merely contain a safe one
            // (`dangerous_shell_proxy` ⊃ `shell`, `delete_file_write` ⊃ `write`).
            let t = tool.to_ascii_lowercase();
            DEFAULT_AUTO_APPROVE.iter().any(|a| t == *a)
        }
    }
}

async fn write_frame(
    writer: &mut (impl AsyncWriteExt + Unpin),
    frame: &Value,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Per-RPC budget for a setup request (`initialize`, `session/new`,
/// `session/configure`). A daemon that accepts the socket but never answers
/// must not hang the loop indefinitely — without this the only backstop was the
/// 900s+ turn guard (and `new_session` had no backstop at all).
const SETUP_RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Read NDJSON frames until the response with `want_id` arrives (skipping
/// notifications and unrelated responses), or `budget` elapses. Returns the
/// `result` value.
async fn read_result(
    reader: &mut (impl AsyncBufReadExt + Unpin),
    want_id: &str,
    budget: Duration,
) -> anyhow::Result<Value> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut line = String::new();
    loop {
        line.clear();
        let n = match tokio::time::timeout_at(deadline, reader.read_line(&mut line)).await {
            Ok(r) => r.context("reading from engine")?,
            Err(_) => bail!("engine did not respond to {want_id} within {budget:?}"),
        };
        if n == 0 {
            bail!("engine closed the connection before responding to {want_id}");
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
            bail!("engine error on {want_id}: {msg}");
        }
        return Ok(frame.get("result").cloned().unwrap_or(Value::Null));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_is_read_only_and_exact_match() {
        // read-only tools auto-approve under Allowlist.
        assert!(decide_approval(ApprovalPolicy::Allowlist, "read"));
        assert!(decide_approval(ApprovalPolicy::Allowlist, "grep"));
        // write/exec tools are NOT auto-approved under Allowlist (need --approve all).
        assert!(!decide_approval(ApprovalPolicy::Allowlist, "shell"));
        assert!(!decide_approval(ApprovalPolicy::Allowlist, "write"));
        assert!(!decide_approval(ApprovalPolicy::Allowlist, "git"));
        // substring impostors are rejected (the ACE vector the substring match allowed).
        assert!(!decide_approval(
            ApprovalPolicy::Allowlist,
            "dangerous_shell_proxy"
        ));
        assert!(!decide_approval(
            ApprovalPolicy::Allowlist,
            "delete_file_write"
        ));
        assert!(!decide_approval(
            ApprovalPolicy::Allowlist,
            "readonly_exfil"
        ));
    }

    #[test]
    fn policy_all_approves_everything_none_denies() {
        assert!(decide_approval(ApprovalPolicy::All, "shell"));
        assert!(decide_approval(ApprovalPolicy::All, "anything_at_all"));
        assert!(!decide_approval(ApprovalPolicy::None, "read"));
    }
}
