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
use tokio::process::Child;

/// How to reach the ACP engine. Today: an already-running daemon over its
/// Unix socket. Future: spawn a child process (e.g. `goose acp`) and speak
/// ACP over its stdio. The JSON-RPC layer is identical in both cases — only
/// the transport half-acquisition differs.
#[derive(Debug, Clone)]
pub enum EngineTransport {
    /// Connect to an existing daemon over a Unix-domain socket.
    UnixSocket(PathBuf),
    /// Spawn `command args…` and speak ACP over its stdio. `env` is appended
    /// to the inherited environment (extra vars override on key clash).
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
}

/// A live, connected ACP transport: a byte-stream reader + writer. For
/// [`EngineTransport::Stdio`] the [`Child`] handle is retained so the spawned
/// process is NOT killed when this struct is dropped; for [`EngineTransport::UnixSocket`]
/// it's `None`. The reader/writer halves are exposed by value and are the only
/// handles the JSON-RPC layer ever touches.
pub struct ConnectedTransport {
    pub reader: TransportReader,
    pub writer: TransportWriter,
    /// Retained so a spawned stdio engine isn't reaped mid-turn. Never read.
    _child: Option<Child>,
}

/// Read-half of a connected ACP transport (works for either a Unix socket or a
/// spawned child's stdout).
pub enum TransportReader {
    Unix(tokio::io::ReadHalf<UnixStream>),
    ChildStdout(tokio::process::ChildStdout),
}

/// Write-half of a connected ACP transport (works for either a Unix socket or a
/// spawned child's stdin).
pub enum TransportWriter {
    Unix(tokio::io::WriteHalf<UnixStream>),
    ChildStdin(tokio::process::ChildStdin),
}

impl tokio::io::AsyncRead for TransportReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            TransportReader::Unix(r) => std::pin::Pin::new(r).poll_read(cx, buf),
            TransportReader::ChildStdout(r) => std::pin::Pin::new(r).poll_read(cx, buf),
        }
    }
}

impl tokio::io::AsyncWrite for TransportWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match &mut *self {
            TransportWriter::Unix(w) => std::pin::Pin::new(w).poll_write(cx, buf),
            TransportWriter::ChildStdin(w) => std::pin::Pin::new(w).poll_write(cx, buf),
        }
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            TransportWriter::Unix(w) => std::pin::Pin::new(w).poll_flush(cx),
            TransportWriter::ChildStdin(w) => std::pin::Pin::new(w).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match &mut *self {
            TransportWriter::Unix(w) => std::pin::Pin::new(w).poll_shutdown(cx),
            TransportWriter::ChildStdin(w) => std::pin::Pin::new(w).poll_shutdown(cx),
        }
    }
}

/// Open the configured transport and return a connected reader/writer pair.
///
/// - [`EngineTransport::UnixSocket`] mirrors today's behavior byte-for-byte:
///   `UnixStream::connect` then `tokio::io::split`.
/// - [`EngineTransport::Stdio`] spawns `command args…` with piped
///   stdin/stdout and returns those halves; the [`Child`] is retained on the
///   returned [`ConnectedTransport`] so it isn't dropped (and the process
///   killed) when the JSON-RPC code only holds the halves.
pub async fn connect_transport(
    transport: &EngineTransport,
) -> anyhow::Result<ConnectedTransport> {
    match transport {
        EngineTransport::UnixSocket(path) => {
            let stream = UnixStream::connect(path)
                .await
                .with_context(|| format!("connecting to engine at {}", path.display()))?;
            let (reader, writer) = tokio::io::split(stream);
            Ok(ConnectedTransport {
                reader: TransportReader::Unix(reader),
                writer: TransportWriter::Unix(writer),
                _child: None,
            })
        }
        EngineTransport::Stdio {
            command,
            args,
            env,
        } => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .kill_on_drop(false);
            for (k, v) in env {
                cmd.env(k, v);
            }
            let mut child = cmd
                .spawn()
                .with_context(|| format!("spawning ACP engine `{command}`"))?;
            // `.take()` detaches the half from the Child so dropping Child
            // doesn't auto-close them — and conversely dropping the half
            // doesn't signal the child (the Child owns the death semantics).
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow!("child had no piped stdin"))?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| anyhow!("child had no piped stdout"))?;
            Ok(ConnectedTransport {
                reader: TransportReader::ChildStdout(stdout),
                writer: TransportWriter::ChildStdin(stdin),
                // Retain the Child so the spawned engine isn't reaped mid-turn.
                _child: Some(child),
            })
        }
    }
}

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

/// Create a fresh engine session bound to `cwd` and return its id (without
/// prompting). Used by `transfer` to hand off a resumable thread.
pub async fn new_session(socket: &Path, agent_alias: &str, cwd: &Path) -> anyhow::Result<String> {
    let conn = connect_transport(&EngineTransport::UnixSocket(socket.to_path_buf())).await?;
    let mut reader = BufReader::new(conn.reader);
    let mut write_half = conn.writer;
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
    read_result(&mut reader, "init").await?;
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
    let res = read_result(&mut reader, "new").await?;
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
    let conn = connect_transport(&EngineTransport::UnixSocket(opts.socket.clone())).await?;
    let mut reader = BufReader::new(conn.reader);
    let mut write_half = conn.writer;

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
    read_result(&mut reader, "init").await?;

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
    let new_res = read_result(&mut reader, "new").await?;
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
        let _ = read_result(&mut reader, "cfg").await;
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
            // Budget exhausted. CANCEL the turn server-side so the engine stops
            // editing files (otherwise a ghost agent keeps running after we
            // return + fail the job), then preserve whatever streamed so far.
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
                // Best-effort: give the engine a moment to ack/wind down the turn.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    reader.read_line(&mut line),
                )
                .await;
                break "timeout".to_string();
            }
            Ok(r) => r.context("reading from engine")?,
        };
        if n == 0 {
            bail!("engine closed the connection before turn completed");
        }
        let frame: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // A JSON-RPC error response on our prompt id is fatal.
        if frame.get("id").and_then(Value::as_str) == Some("prompt") {
            if let Some(err) = frame.get("error") {
                let msg = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                bail!("session/prompt failed: {msg}");
            }
            continue;
        }

        if frame.get("method").and_then(Value::as_str) != Some("session/update") {
            continue;
        }
        let params = frame.get("params").cloned().unwrap_or(Value::Null);
        let kind = params.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "agent_message_chunk" => {
                if let Some(t) = params.get("text").and_then(Value::as_str) {
                    content.push_str(t);
                    on_event(AgentEvent::Text(t.to_string()));
                }
            }
            "agent_thought_chunk" => {
                if opts.show_reasoning {
                    if let Some(t) = params.get("text").and_then(Value::as_str) {
                        on_event(AgentEvent::Thought(t.to_string()));
                    }
                }
            }
            "tool_call" => {
                tool_calls += 1;
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
                    input_tokens = input_tokens.max(it);
                    on_event(AgentEvent::Usage { input_tokens: it });
                }
            }
            "approval_request" => {
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
                write_frame(
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
                .await?;
                on_event(AgentEvent::Approval { tool, approved });
            }
            "turn_complete" => {
                let oc = params
                    .get("outcome")
                    .and_then(Value::as_str)
                    .unwrap_or("completed")
                    .to_string();
                if let Some(c) = params.get("content").and_then(Value::as_str) {
                    if !c.is_empty() {
                        content = c.to_string();
                    }
                }
                break oc;
            }
            _ => {}
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

/// Read NDJSON frames until the response with `want_id` arrives (skipping
/// notifications and unrelated responses). Returns the `result` value.
async fn read_result(
    reader: &mut (impl AsyncBufReadExt + Unpin),
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
