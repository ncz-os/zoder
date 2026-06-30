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
/// process can be reaped on timeout/error by the driver (NOT killed mid-turn
/// just because the struct is dropped — the driver owns the lifecycle). For
/// [`EngineTransport::UnixSocket`] it's `None`. The reader/writer halves are
/// exposed by value and are the only handles the JSON-RPC layer usually
/// touches.
pub struct ConnectedTransport {
    pub reader: TransportReader,
    pub writer: TransportWriter,
    /// Spawned stdio engine, retained so the driver can reap it on timeout /
    /// error. `None` for [`EngineTransport::UnixSocket`]. The driver MUST
    /// call [`Self::kill_child`] (or take and wait on it) on every exit path
    /// — otherwise a timed-out or errored turn leaves a zombie `goose`
    /// process holding the agent's open file handles.
    pub child: Option<Child>,
}

impl ConnectedTransport {
    /// Kill and wait on the spawned stdio engine (if any). Idempotent and
    /// safe to call on every exit path; safe to call when there is no child.
    /// Called by the goose driver in its `Drop`-like guard before returning.
    pub async fn kill_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            // SIGKILL is the only signal guaranteed to work even if goose is
            // stuck on a sync syscall or in a model retry loop; we already
            // gave it a chance to wind down via `session/cancel`.
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
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

/// Selects which agentic engine `run_agent_dispatch` drives. Today: the local
/// zeroclaw daemon over its Unix socket (or spawned stdio). Future: other ACP
/// engines (goose, etc.) — the dispatcher picks the right transport and RPC
/// surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EngineKind {
    /// The local zeroclaw daemon (default; current behavior).
    #[default]
    Zeroclaw,
    /// Block's Goose ACP engine. Stub for now — real implementation lands in
    /// step 2b; calls return a clear "not yet implemented" error.
    Goose,
}

impl std::str::FromStr for EngineKind {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "zeroclaw" => Ok(EngineKind::Zeroclaw),
            "goose" => Ok(EngineKind::Goose),
            other => Err(anyhow!(
                "unknown engine {other:?} (expected: zeroclaw | goose)"
            )),
        }
    }
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
pub async fn connect_transport(transport: &EngineTransport) -> anyhow::Result<ConnectedTransport> {
    match transport {
        EngineTransport::UnixSocket(path) => {
            let stream = UnixStream::connect(path)
                .await
                .with_context(|| format!("connecting to engine at {}", path.display()))?;
            let (reader, writer) = tokio::io::split(stream);
            Ok(ConnectedTransport {
                reader: TransportReader::Unix(reader),
                writer: TransportWriter::Unix(writer),
                child: None,
            })
        }
        EngineTransport::Stdio { command, args, env } => {
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
                // Retain the Child so the driver can reap it on timeout / error.
                child: Some(child),
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
    /// Unix socket of the running daemon. Unused on the [`EngineKind::Goose`]
    /// path (goose is spawned over stdio and never touches this socket); kept
    /// on the struct so the CLI doesn't need to branch when constructing it.
    pub socket: PathBuf,
    /// Configured agent alias (`[agents.<alias>]`) to run as. Zeroclaw-only;
    /// goose doesn't have the concept.
    pub agent_alias: String,
    /// Working directory (repo root) for the run.
    pub cwd: PathBuf,
    /// The user prompt / task.
    pub prompt: String,
    /// Optional per-session model override (zeroclaw rpc-only). For goose,
    /// prefer [`Self::model_id`], which is always the *routed model id* (e.g.
    /// `MiniMax-M3`), not a zeroclaw agent alias.
    pub model_override: Option<String>,
    /// The routed model id (from `chain[0]` / `-m` / `[engine].primary_model`).
    /// Always set when the CLI knows which model to bill; `None` only when
    /// the engine should pick its own default. Goose uses this for
    /// `GOOSE_MODEL` (NOT `agent_alias` — those are zeroclaw-internal names
    /// like `minimax` / `deepseek-v4-pro` and goose has no idea what they
    /// mean).
    pub model_id: Option<String>,
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
            model_id: None,
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

/// Dispatch a single agentic turn to the selected engine.
///
/// - `Zeroclaw` (default) drives the local daemon over its Unix socket using
///   the existing zeroclaw-extensions RPC surface (`agent_alias`, `chat_mode`).
/// - `Goose` spawns a `goose acp` child process and speaks STANDARD ACP
///   (no zeroclaw extensions): no `agent_alias`/`chat_mode` is sent, model
///   selection happens via `GOOSE_MODEL`/`GOOSE_PROVIDER` env vars on the
///   spawned process, and `session/new` carries only `{cwd, mcpServers}`.
pub async fn run_agent_dispatch<F: FnMut(AgentEvent)>(
    kind: EngineKind,
    opts: &AgentOptions,
    on_event: F,
) -> anyhow::Result<AgentRun> {
    match kind {
        EngineKind::Zeroclaw => run_agent(opts, on_event).await,
        EngineKind::Goose => run_goose_agent(opts, on_event).await,
    }
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

// ---------------------------------------------------------------------------
// Goose ACP driver (standard ACP, no zeroclaw extensions).
// ---------------------------------------------------------------------------
//
// Goose speaks STANDARD ACP over stdio via `goose acp`. Unlike the local
// zeroclaw daemon, goose expects:
//   * `initialize { protocolVersion, clientCapabilities }` (camelCase, the
//     version we send is negotiated against the server's reply).
//   * `session/new` with `cwd` + `mcpServers` only — NO `agent_alias` /
//     `chat_mode` (those are zeroclaw extensions and would confuse goose).
//   * `session/prompt` with a `prompt: [{type:"text", text:...}]` content
//     block (standard ACP content array), and the response carries the
//     terminal `stopReason` for the turn.
//   * Per-turn model selection via env on the spawned process
//     (`GOOSE_PROVIDER`, `GOOSE_MODEL`) — NOT via ACP params.
// The notification surface (`session/update`) carries the streamed message /
// thought / tool chunks; the assistant final message also arrives as a
// `session/update` with `sessionUpdate: "agent_message"` (or end-of-turn
// signalized by the `session/prompt` response itself). The driver below maps
// those onto the same `AgentEvent` surface the zeroclaw driver uses so the
// CLI can stream output uniformly across both engines.

/// Highest ACP protocol version this client will propose. Goose negotiates
/// down to the minimum of ours and the server's. Kept as a single constant so
/// the wire shape is easy to audit against future spec changes.
const GOOSE_PROTOCOL_VERSION: u64 = 1;

/// Build the env that selects a goose model/provider. The rest of the
/// environment is inherited by [`EngineTransport::Stdio`].
///
///   * `GOOSE_PROVIDER`: from `provider_override` (a test seam to avoid
///     mutating global process env — `std::env::set_var`/`remove_var` are
///     `unsafe` in Rust 2024 because they're racy with parallel readers; we
///     refuse to rely on that), else from `$GOOSE_PROVIDER`, else `"openai"`
///     (goose treats custom OpenAI-compatible endpoints as the `openai`
///     provider when `OPENAI_BASE_URL` etc. are set in the inherited env).
///   * `GOOSE_MODEL`: prefers `opts.model_id` (the routed model id, e.g.
///     `MiniMax-M3`, `deepseek-chat`, `gpt-4o`), then `opts.model_override`,
///     and only then `opts.agent_alias` as a last-resort fallback. NEVER
///     send a zeroclaw agent alias (like `minimax` or `deepseek-v4-pro`) as
///     `GOOSE_MODEL` unless the operator pinned that exact alias as the
///     routed model — goose has no concept of zeroclaw aliases and will try
///     to load it as a model id, which is what the previous version did and
///     why this driver hung on first tool call.
///
/// `provider_override` is a nested `Option` so the test seam can
/// distinguish "caller explicitly says: use this provider" (`Some(s)`)
/// from "caller says: pretend $GOOSE_PROVIDER isn't set either"
/// (`Some(None)`, force the default). For production the caller passes
/// `None` meaning "use ambient $GOOSE_PROVIDER if set".
pub(crate) fn goose_env(
    opts: &AgentOptions,
    provider_override: Option<Option<&str>>,
) -> Vec<(String, String)> {
    // Resolution precedence (highest first):
    //   1. explicit override passed in (used by tests; never mutates env)
    //   2. $GOOSE_PROVIDER (operator override on the parent shell)
    //   3. "openai" (goose's default for OpenAI-compatible endpoints)
    let provider = match provider_override {
        Some(Some(s)) => s.to_string(),
        Some(None) => "openai".to_string(),
        None => std::env::var("GOOSE_PROVIDER").unwrap_or_else(|_| "openai".to_string()),
    };
    let model = opts
        .model_id
        .clone()
        .or_else(|| opts.model_override.clone())
        .unwrap_or_else(|| opts.agent_alias.clone());
    vec![
        ("GOOSE_PROVIDER".to_string(), provider),
        ("GOOSE_MODEL".to_string(), model),
    ]
}

/// Drive a single goose ACP turn: spawn `goose acp`, do the ACP handshake,
/// open a session, send the prompt, stream notifications, and return.
/// Mirrors `run_agent`'s overall structure (timeout, backstop, partial
/// preservation on timeout) but in standard ACP and with no zeroclaw
/// extensions. ALWAYS reaps the spawned child on every exit path —
/// timeouts, errors, and clean shutdowns alike — so we never leak a
/// `goose` process holding the agent's working dir or sockets.
pub async fn run_goose_agent<F: FnMut(AgentEvent)>(
    opts: &AgentOptions,
    on_event: F,
) -> anyhow::Result<AgentRun> {
    let env = goose_env(opts, None);
    let transport = EngineTransport::Stdio {
        command: "goose".to_string(),
        args: vec!["acp".to_string()],
        env,
    };
    // Own the `ConnectedTransport` (and therefore the spawned `Child`)
    // OUTSIDE the future passed to `tokio::time::timeout`. If we kept it
    // inside that future, the outer backstop firing would `drop` the
    // future mid-`drive_goose_io`, dropping the `Child` without ever
    // calling `kill_child()` — and `kill_on_drop(false)` is set, so the
    // spawned `goose` process would be orphaned and leak (the OS-level
    // reap would never happen). The fix is to keep `conn` alive in the
    // outer scope and ALWAYS run `kill_child()` after the timeout race,
    // whether the inner future completed, errored, or the backstop fired.
    let mut conn = connect_transport(&transport).await?;
    run_goose_agent_with_conn(&mut conn, opts, on_event).await
}

/// Drive one goose turn over an already-connected transport. The transport
/// (and any spawned child it carries) is owned by the caller; this function
/// ALWAYS reaps the child via [`ConnectedTransport::kill_child`] before
/// returning, on every exit path — clean completion, inner error, OR the
/// outer backstop timeout. The `conn` argument is `&mut` (not by value) so
/// this function can run the reap on the backstop path: the inner future
/// passed to `tokio::time::timeout` is dropped on `Elapsed`, releasing the
/// `&mut conn` borrow, and we then call `kill_child()` unconditionally.
///
/// Split out from [`run_goose_agent`] so tests can drive it with a
/// hand-built `ConnectedTransport` (e.g. a child that sleeps forever, to
/// exercise the backstop-reap guarantee end-to-end).
pub(crate) async fn run_goose_agent_with_conn<F: FnMut(AgentEvent)>(
    conn: &mut ConnectedTransport,
    opts: &AgentOptions,
    mut on_event: F,
) -> anyhow::Result<AgentRun> {
    // Backstop covers setup-time hangs; the inner loop enforces the real budget.
    let backstop = opts.timeout + Duration::from_secs(30);

    // Move the reader/writer halves OUT of `conn` first so the borrow
    // checker doesn't see a partial move when we later call
    // `conn.kill_child()`. We reattach the moved halves via placeholders
    // so the `conn` struct remains valid (and so the placeholders can
    // be cleanly dropped in the order we want).
    //
    // The order on shutdown matters: the writer (stdin) is dropped
    // FIRST so the child sees EOF on its stdin and can wind down
    // cleanly; then the reader (stdout) is shutdown so any pending
    // `read_line` in the driver unblocks; THEN we SIGKILL the
    // child and wait on it. Without the writer-first order, the
    // child has no signal that we're done sending and may keep
    // blocking on stdin read in a tool call, requiring the SIGKILL
    // we are about to deliver anyway — but the SIGKILL then
    // arrives mid-write and the child can leave file-handle state
    // inconsistent. Drop order = shutdown order.
    //
    // CRITICAL: the `fut` block borrows `conn` mutably for the
    // `mem::replace` calls. When the outer `tokio::time::timeout` fires,
    // `fut` is dropped, releasing the `&mut conn` borrow. The reap
    // happens AFTER the timeout returns, so it runs even on the
    // `Elapsed` path.
    let fut = async {
        let reader = std::mem::replace(&mut conn.reader, dummy_reader());
        let writer = std::mem::replace(&mut conn.writer, dummy_writer());
        let mut reader = BufReader::new(reader);
        let mut write_half = writer;
        let res = drive_goose_io(opts, &mut reader, &mut write_half, &mut on_event).await;
        // Step 1: drop the writer FIRST. Closing the child's stdin is the
        // polite way to say "no more input" and lets goose wind down any
        // blocking tool call. If we skip this, the child may be stuck
        // reading from stdin when SIGKILL arrives.
        drop(write_half);
        // Step 2: drop the reader buffer. If the driver is still parked
        // on a `read_line` in some panic-recovery path (it shouldn't be,
        // but defense in depth), dropping the BufReader plus its
        // underlying transport reader closes the pipe so the read
        // returns EOF immediately. Best-effort: ignore errors.
        drop(reader);
        res
    };

    // Race the inner turn against the backstop. On `Elapsed` the inner
    // future is dropped (releasing its `&mut conn` borrow) and `conn`
    // is dropped to `kill_child()` below.
    let raced = tokio::time::timeout(backstop, fut).await;

    // ALWAYS reap the child, on every exit path:
    //   * inner Ok  -> normal completion, still want the child gone.
    //   * inner Err -> handshake/protocol error, child must be reaped.
    //   * Elapsed   -> backstop fired; this is the bug we fixed. Without
    //                  this line on the `Elapsed` branch the child leaks.
    // `kill_child()` is idempotent and a no-op when `conn.child` is `None`
    // (e.g. an `EngineTransport::UnixSocket`), so it is safe to call
    // unconditionally on every run.
    conn.kill_child().await;

    raced.map_err(|_| anyhow!("goose ACP turn hung during setup; exceeded {:?}", backstop))?
}

/// Build a no-op reader half (used as a placeholder in [`run_goose_agent`]
/// after the real reader has been moved out for use by `drive_goose_io`).
/// The placeholder is never read from because `kill_child()` only mutates
/// `conn.child`. Real I/O flows through the moved halves.
fn dummy_reader() -> TransportReader {
    // `tokio::io::empty()` returns an `Empty` reader that always yields
    // EOF; we route it through `Unix` so the enum variant is generic. It
    // will never be polled.
    let (a, _b) = tokio::net::UnixStream::pair().expect("unix stream pair");
    TransportReader::Unix(tokio::io::split(a).0)
}

/// Symmetric placeholder for [`TransportWriter`].
fn dummy_writer() -> TransportWriter {
    let (a, _b) = tokio::net::UnixStream::pair().expect("unix stream pair");
    TransportWriter::Unix(tokio::io::split(a).1)
}

/// Core goose driver: speak the standard ACP handshake, session, and prompt
/// over an arbitrary reader/writer pair. Split out from `run_goose_agent` so
/// tests can drive it with `tokio::io::duplex` instead of a real child.
///
/// IMPORTANT: this function does NOT reap a spawned child. The caller
/// (`run_goose_agent`) is responsible for reaping via
/// [`ConnectedTransport::kill_child`] on every exit path. Tests pass
/// `tokio::io::duplex` so there is no child and the rule reduces to
/// "forget about it" — which is correct: the in-memory duplex has no OS
/// resource to leak.
async fn drive_goose_io<F, R, W>(
    opts: &AgentOptions,
    reader: &mut R,
    write_half: &mut W,
    on_event: &mut F,
) -> anyhow::Result<AgentRun>
where
    F: FnMut(AgentEvent),
    R: AsyncBufReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let deadline = tokio::time::Instant::now() + opts.timeout;

    // 1. initialize. Standard ACP uses camelCase fields. We send our highest
    //    supported version; the server replies with its own; we use the min
    //    of the two (the canonical "negotiate" step). We don't hardcode
    //    version 1 == matches goose because goose's version will move with
    //    its releases.
    let init_params = json!({
        "protocolVersion": GOOSE_PROTOCOL_VERSION,
        "clientCapabilities": {},
    });
    write_frame(
        write_half,
        &json!({
            "jsonrpc": "2.0",
            "id": "init",
            "method": "initialize",
            "params": init_params,
        }),
    )
    .await?;
    let init_res = match tokio::time::timeout_at(deadline, read_result(reader, "init")).await {
        Ok(r) => r?,
        // Initialize handshake timed out. There is no session yet (we never
        // got a `sessionId` from `session/new`), so we MUST NOT send a
        // `session/cancel` notification — it would carry an empty
        // `sessionId` and confuse (or be rejected by) the engine. Just
        // surface a clean error and let the caller reap the child.
        Err(_) => {
            return Err(anyhow!(
                "goose initialize handshake timed out after {:?}",
                opts.timeout
            ))
        }
    };
    let _negotiated = match init_res.get("protocolVersion") {
        // The server's response is canonical for the session. We just record
        // it; goose only uses it for capability gating, not field shape.
        Some(v) if v.is_number() => v.as_u64().unwrap_or(GOOSE_PROTOCOL_VERSION),
        // Older / permissive goose builds may omit the field; assume v1.
        _ => GOOSE_PROTOCOL_VERSION,
    }
    .min(GOOSE_PROTOCOL_VERSION);

    // 2. session/new. STANDARD params only: `cwd` + `mcpServers`. Deliberately
    //    omit zeroclaw's `agent_alias` / `chat_mode` — goose doesn't know
    //    what they mean and would either ignore or error on them.
    let new_params = json!({
        "cwd": opts.cwd.to_string_lossy(),
        "mcpServers": [],
    });
    write_frame(
        write_half,
        &json!({
            "jsonrpc": "2.0",
            "id": "new",
            "method": "session/new",
            "params": new_params,
        }),
    )
    .await?;
    let new_res = match tokio::time::timeout_at(deadline, read_result(reader, "new")).await {
        Ok(r) => r?,
        // session/new timed out. No session id was ever issued, so we
        // cannot send a `session/cancel` (it would carry an empty
        // `sessionId`). Surface a clean error.
        Err(_) => {
            return Err(anyhow!(
                "goose session/new timed out after {:?}",
                opts.timeout
            ))
        }
    };
    let session_id: Option<String> = new_res
        .get("sessionId")
        .or_else(|| new_res.get("session_id"))
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let session_id = match session_id {
        Some(s) if !s.is_empty() => s,
        _ => return Err(anyhow!("session/new returned no sessionId")),
    };

    // 3+4. Combined `session/prompt` send + notification/read loop.
    //
    //    ACP v1 spec note: `session/prompt` is a JSON-RPC REQUEST whose
    //    response carries the terminal `stopReason` for the turn. Between
    //    receiving the prompt and sending the final response, the server
    //    is free to send `session/update` notifications AND JSON-RPC
    //    REQUESTS that demand a reply (notably `session/request_permission`).
    //
    //    The previous version sent the `session/prompt` write BEFORE
    //    entering the read loop. That ordering is wrong: if the server
    //    emits any frames (updates or permission requests) after receiving
    //    the prompt but before sending the prompt response, those frames
    //    sit unread in the pipe buffer until the client starts its
    //    read loop. In the worst case the server fills the pipe buffer
    //    with notifications, then blocks on `write(stdout)` waiting for
    //    the client to drain — while the client has already returned from
    //    the prompt `flush()` and is about to call `read_line` for the
    //    very first time. That window is small but real, and in a
    //    `session/request_permission` round-trip it can compound into a
    //    pipe-buffer deadlock on any non-trivial turn.
    //
    //    The fix: enter the read loop FIRST and send `session/prompt` on
    //    the very first iteration (before the first `read_line`). This
    //    guarantees:
    //      (a) the reader is parked and will drain the pipe the
    //          instant the server produces any frame, and
    //      (b) the prompt write happens while the reader is ready, so
    //          permission-request frames interleaved between prompt and
    //          prompt-response are never blocked on the client not yet
    //          reading.
    //
    //    The loop handles four frame kinds:
    //      * the `session/prompt` RESPONSE -> break (terminal stopReason)
    //      * a `session/request_permission` REQUEST -> answer per policy
    //      * a `session/update` notification -> stream chunk
    //      * anything else -> ignore
    //
    //    Standard ACP content block shape:
    //     prompt: [{ type: "text", text: <string> }]
    let prompt_frame = json!({
        "jsonrpc": "2.0",
        "id": "prompt",
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": opts.prompt }],
        },
    });
    let mut content = String::new();
    let mut input_tokens: u64 = 0;
    let mut tool_calls: u32 = 0;
    let mut line = String::new();
    // `prompt_sent` flips to `true` on the first iteration of the loop,
    // just before the first `read_line`. Keeping this as a flag (rather
    // than moving the write out of the loop entirely) means:
    //   * the prompt write is gated by the same deadline as the reads
    //     (so a stuck handshake still surfaces a clean timeout error),
    //   * we never block the reader from spinning up,
    //   * the cancel-on-timeout branch below naturally sees
    //     `prompt_sent == false` if the prompt write itself errored
    //     (in which case sending `session/cancel` with the known
    //     session_id is still meaningful — the server has the session
    //     from `session/new` and the cancel tells it to drop the
    //     pending prompt it may have already received).
    let mut prompt_sent = false;
    let outcome: String = loop {
        // 3. Send `session/prompt` on the first iteration, BEFORE reading.
        //    Doing it here (inside the loop, after the reader is set up
        //    but before the first `read_line`) is the actual deadlock
        //    fix: the reader is now ready to drain any frames the
        //    server emits in response.
        if !prompt_sent {
            write_frame(write_half, &prompt_frame).await?;
            prompt_sent = true;
            // Fall through to read_line on this same iteration so the
            // reader is polled immediately after the write — no
            // scheduler-induced gap between "sent prompt" and "ready
            // to read reply".
        }
        line.clear();
        let n = match tokio::time::timeout_at(deadline, reader.read_line(&mut line)).await {
            Err(_) => {
                // Budget exhausted. Best-effort: ask goose to stop the turn
                // so it doesn't keep editing files in the background after
                // we've returned. `session/cancel` is a NOTIFICATION per
                // the ACP v1 spec — no `id`, no response expected. The
                // engine will wind down the in-flight tool calls and reply
                // to the original `session/prompt` with
                // `stopReason: "cancelled"`. (The caller already reaps the
                // child process; this just gives goose a clean shutdown.)
                //
                // We only send `session/cancel` when we actually have a
                // session id (we do — by this point `session/new` has
                // completed successfully). Sending it before then would
                // carry an empty `sessionId` and either confuse or be
                // rejected by the server.
                let _ = write_frame(
                    write_half,
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "session/cancel",
                        "params": { "sessionId": &session_id },
                    }),
                )
                .await;
                // Best-effort drain of any in-flight `session/update`
                // notifications goose may flush before honouring the
                // cancel. We cap the drain at the SMALLER OF 250ms and
                // the time remaining to the deadline, so the drain can
                // never compound the timeout we just hit. We deliberately
                // do NOT block for the full 5s: if the outer turn budget
                // has just elapsed, a 5s drain could push the driver past
                // the caller's backstop and cause a confusing secondary
                // timeout. 250ms is enough to pick up a `stopReason` that
                // goose immediately sends in response to the cancel.
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                let drain_budget = remaining.min(Duration::from_millis(250));
                if !drain_budget.is_zero() {
                    let _ = tokio::time::timeout(drain_budget, reader.read_line(&mut line)).await;
                }
                break "timeout".to_string();
            }
            Ok(r) => r.context("reading from goose ACP engine")?,
        };
        if n == 0 {
            // Engine closed its end. If we never got an explicit stopReason,
            // assume the turn completed.
            break if content.is_empty() && tool_calls == 0 {
                "failed".to_string()
            } else {
                "completed".to_string()
            };
        }
        let frame: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // --- A) The `session/prompt` RESPONSE carries the terminal
        //        stopReason and ends the turn.
        if frame.get("id").and_then(Value::as_str) == Some("prompt") {
            if let Some(err) = frame.get("error") {
                let msg = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                bail!("goose session/prompt failed: {msg}");
            }
            let res = frame.get("result").cloned().unwrap_or(Value::Null);
            let stop_reason = res
                .get("stopReason")
                .and_then(Value::as_str)
                .unwrap_or("end_turn");
            // Prefer a terminal `content` block on the result if goose provides
            // one (mirrors the zeroclaw turn_complete.content behavior).
            if let Some(c) = res.get("content").and_then(Value::as_str) {
                if !c.is_empty() {
                    content = c.to_string();
                }
            }
            break match stop_reason {
                "end_turn" => "completed".to_string(),
                "cancelled" => "cancelled".to_string(),
                "max_tokens" | "max_tokens_reached" => "max_tokens".to_string(),
                other => other.to_string(),
            };
        }

        // --- B) `session/request_permission` is a JSON-RPC REQUEST (has
        //        an `id`) — the server expects a reply. Per ACP v1 spec
        //        the request shape is `{sessionId, toolCall, options[]}`
        //        and the reply is `{outcome: RequestPermissionOutcome}`
        //        where `outcome` is either:
        //          - {"outcome": "selected", "optionId": "<id>"}
        //          - {"outcome": "cancelled"}
        //        We pick the optionId by matching `options[].kind`
        //        (the SEMANTIC tag: `allow_once`/`allow_always`/
        //        `reject_once`/`reject_always`) against the
        //        ApprovalPolicy, and echo back THAT option's actual
        //        opaque `optionId` in the reply. If the engine ever
        //        asks us and we don't answer, goose wedges.
        if frame.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            let req_id = frame
                .get("id")
                .cloned()
                .unwrap_or(Value::String(String::new()));
            let req_params = frame.get("params").cloned().unwrap_or(Value::Null);
            // Pull the tool name out of `params.toolCall.title` (the human
            // label goose shows — close enough to "tool name" for policy
            // decisions). Fall back to `params.toolCall.name` and finally
            // a generic label so we never silently skip the reply.
            let tool_name = req_params
                .get("toolCall")
                .and_then(|tc| tc.get("title"))
                .and_then(Value::as_str)
                .or_else(|| {
                    req_params
                        .get("toolCall")
                        .and_then(|tc| tc.get("name"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("tool")
                .to_string();
            // Real ACP v1 spec: `optionId` is an OPAQUE server-side id; the
            // semantic meaning lives in `options[].kind` (one of
            // `allow_once`, `allow_always`, `reject_once`, `reject_always`).
            // We MUST match by `kind` and echo back the option's actual
            // `optionId` in `result.outcome.optionId` — the server's id, not
            // a hard-coded string. Matching on `optionId` directly (the old
            // behavior) was wrong because (a) real goose may surface
            // server-generated ids like `"opt-A9F1"` instead of the canonical
            // names, and (b) the semantics are in `kind` by spec.
            let approved = decide_approval(opts.approval, &tool_name);
            let options = req_params
                .get("options")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            // Find the first option whose `kind` matches one of the
            // candidate kinds, and return its `optionId` (the opaque id
            // we must echo back to the server). Returns `None` when no
            // offered option matches.
            let pick_by_kind = |kinds: &[&str]| -> Option<String> {
                for kind in kinds {
                    if let Some(opt) = options
                        .iter()
                        .find(|o| o.get("kind").and_then(Value::as_str) == Some(*kind))
                    {
                        return opt
                            .get("optionId")
                            .and_then(Value::as_str)
                            .map(|s| s.to_string());
                    }
                }
                None
            };
            let outcome_value = if approved {
                // Prefer `allow_once` then `allow_always`. If the server
                // offered neither an `allow_*` kind nor any option at
                // all, fall back to the first option offered (better
                // than cancelling — we explicitly WANT to allow under
                // ApprovalPolicy::All / Allowlist when the server offered
                // something). The server's `optionId` is what we echo
                // back, regardless of the option's position in `options[]`.
                pick_by_kind(&["allow_once", "allow_always"]).or_else(|| {
                    options
                        .first()
                        .and_then(|o| o.get("optionId").and_then(Value::as_str))
                        .map(|s| s.to_string())
                })
            } else {
                // SECURITY: under ApprovalPolicy::None (and under Allowlist
                // for non-allowlisted tools) we MUST NOT silently flip to
                // an allow option just because the server forgot to ship
                // a reject option. Falling back to `options.first()` here
                // would let a malicious or buggy goose server force the
                // client to approve any tool call by simply omitting reject
                // options from `options[]` — bypassing the policy's
                // "always deny" guarantee. The only safe default when no
                // recognized reject option is offered is the explicit
                // `cancelled` outcome, which the server interprets as a
                // denial. This is also why `approved=false` always
                // reaches this branch when `policy == None`.
                //
                // Keying on `kind` (not `optionId`) preserves this
                // guarantee under real ACP: the server picks the ids,
                // we only care about the SEMANTIC kind.
                pick_by_kind(&["reject_once", "reject_always"])
            };
            let reply = match outcome_value {
                Some(option_id) => json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": { "outcome": { "outcome": "selected", "optionId": option_id } }
                }),
                None => json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": { "outcome": { "outcome": "cancelled" } }
                }),
            };
            write_frame(write_half, &reply).await?;
            on_event(AgentEvent::Approval {
                tool: tool_name,
                approved,
            });
            continue;
        }

        // --- C) Anything else that isn't a `session/update` notification
        //        is ignored (initial ping, current_mode_update noise, etc.).
        if frame.get("method").and_then(Value::as_str) != Some("session/update") {
            continue;
        }

        // --- D) `session/update` notification. The discriminator
        //        (`sessionUpdate`) and the variant payload BOTH nest under
        //        `params.update`, NOT flat on `params`. Per the ACP v1
        //        spec the notification shape is:
        //          { sessionId, update: SessionUpdate }
        //        where `SessionUpdate.sessionUpdate` is the variant tag
        //        (e.g. `"agent_message_chunk"`) and the remaining fields
        //        are variant-specific (e.g. `content: ContentBlock`,
        //        `toolCallId`, `status`, `title`, `kind`, etc.).
        let params = frame.get("params").cloned().unwrap_or(Value::Null);
        let update = params.get("update").cloned().unwrap_or(Value::Null);
        // Real goose always puts the discriminator on `update.sessionUpdate`;
        // be defensive and also accept `type` if a future build flattens it.
        let kind = update
            .get("sessionUpdate")
            .or_else(|| update.get("session_update"))
            .or_else(|| update.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match kind {
            "agent_message_chunk" => {
                // Spec: `content: ContentBlock` where ContentBlock is one
                // of { text | image | audio | resource | resource_link }.
                // We only surface text. `content.text` is the inline path.
                let text = extract_text_content(update.get("content"));
                if let Some(t) = text {
                    content.push_str(&t);
                    on_event(AgentEvent::Text(t));
                }
            }
            "agent_thought_chunk" => {
                if opts.show_reasoning {
                    if let Some(t) = extract_text_content(update.get("content")) {
                        on_event(AgentEvent::Thought(t));
                    }
                }
            }
            "agent_message" => {
                // Final assistant message for the turn (ContentBlock).
                if let Some(t) = extract_text_content(update.get("content")) {
                    content.push_str(&t);
                    on_event(AgentEvent::Text(t));
                }
            }
            "tool_call" | "tool_call_update" => {
                // Spec: `toolCallId`, `title`, `kind`, `status`, `content`.
                // The "call" arrives once (status pending) and updates
                // arrive as `tool_call_update` with the same id. We count
                // only the initial call, not progress updates.
                if kind == "tool_call" {
                    tool_calls += 1;
                }
                let name = update
                    .get("title")
                    .or_else(|| update.get("name"))
                    .or_else(|| update.get("toolName"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                on_event(AgentEvent::ToolCall { name });
            }
            "tool_result_update" | "tool_result" => {
                let name = update
                    .get("toolCallId")
                    .or_else(|| update.get("name"))
                    .or_else(|| update.get("toolName"))
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                on_event(AgentEvent::ToolResult { name });
            }
            "usage_update" | "usage" => {
                // goose emits `used` (input tokens used) and `size` (window).
                // Real goose uses `used`; spec also allows `inputTokens`.
                let it = update
                    .get("used")
                    .or_else(|| update.get("inputTokens"))
                    .or_else(|| update.get("input_tokens"))
                    .and_then(Value::as_u64);
                if let Some(it) = it {
                    input_tokens = input_tokens.max(it);
                    on_event(AgentEvent::Usage { input_tokens: it });
                }
            }
            "current_mode_update"
            | "available_commands_update"
            | "config_option_update"
            | "session_info_update" => {
                // Informational notifications we surface as ToolResult-like
                // events with no specific name; safe to ignore for the
                // stream of text/tool/usage we care about.
            }
            _ => {
                // Unknown variant — ignore (forward-compat with future ACP
                // additions; the spec explicitly reserves extensibility).
            }
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

/// Pull the inline text out of an ACP `ContentBlock` (`{type: "text",
/// text: "..."}`). Returns `None` for non-text blocks (image/audio/
/// resource) so the caller can decide whether to surface a placeholder
/// or skip. `content` is the JSON value of `params.update.content`
/// (or `params.update.content[0]` for an array).
fn extract_text_content(content: Option<&Value>) -> Option<String> {
    let cb = content?;
    // The ContentBlock is an object: {type, text} (text), {type, data, mimeType}
    // (image/audio), or {type, resource|resource_link, ...} (resource). Some
    // notifications might also wrap a single block in an array — accept both.
    let obj = if let Some(arr) = cb.as_array() {
        arr.first().unwrap_or(&Value::Null)
    } else {
        cb
    };
    if obj.get("type").and_then(Value::as_str) == Some("text") {
        obj.get("text")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
    } else {
        None
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
    // `AsyncBufReadExt` is brought in by `use super::*` (it lives in the
    // parent module's `use` lines). The alias is no longer needed.

    #[test]
    fn allowlist_is_read_only_and_exact_match() {
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

    #[test]
    fn engine_kind_from_str_lowercase() {
        assert_eq!(
            "zeroclaw".parse::<EngineKind>().unwrap(),
            EngineKind::Zeroclaw
        );
        assert_eq!(
            "ZEROCLAW".parse::<EngineKind>().unwrap(),
            EngineKind::Zeroclaw
        );
        assert_eq!("goose".parse::<EngineKind>().unwrap(), EngineKind::Goose);
    }

    #[test]
    fn engine_kind_from_str_rejects_empty_and_unknown() {
        assert!("".parse::<EngineKind>().is_err());
        assert!("   ".parse::<EngineKind>().is_err());
        assert!("bogus".parse::<EngineKind>().is_err());
    }

    // --- Goose driver tests ------------------------------------------------

    /// Build a default `AgentOptions` for goose-driver tests. The
    /// `model_id` field is intentionally `None` here so the per-test
    /// routing-precedence tests can decide what to set. `agent_alias` is
    /// a zeroclaw-style alias (`"minimax"`) to prove the driver does NOT
    /// fall through to it when a routed model id is available.
    fn goose_opts(model_override: Option<&str>) -> AgentOptions {
        AgentOptions {
            socket: std::path::PathBuf::from("/tmp/unused.sock"),
            agent_alias: "minimax".to_string(),
            cwd: std::path::PathBuf::from("/tmp"),
            prompt: "hello".to_string(),
            model_override: model_override.map(|s| s.to_string()),
            model_id: None,
            session_id: None,
            show_reasoning: false,
            approval: ApprovalPolicy::Allowlist,
            timeout: std::time::Duration::from_secs(5),
        }
    }

    #[test]
    fn goose_env_uses_model_id_first() {
        // The routed model id (e.g. "MiniMax-M3", "deepseek-chat") MUST
        // win over both model_override and agent_alias. This is the core
        // fix: previously GOOSE_MODEL could be set to a zeroclaw alias
        // like "minimax" and goose would try to load it as a model id.
        let mut opts = goose_opts(Some("gpt-4o-mini"));
        opts.model_id = Some("MiniMax-M3".to_string());
        let env = goose_env(&opts, None);
        let model = env.iter().find(|(k, _)| k == "GOOSE_MODEL").unwrap();
        assert_eq!(model.1, "MiniMax-M3");
    }

    #[test]
    fn goose_env_falls_back_to_model_override_then_alias() {
        // No model_id: prefer model_override (the explicit `-m` pin),
        // and only as a last resort fall back to the zeroclaw agent alias.
        // (The alias fallback exists so headless test runs that construct
        // AgentOptions without a routed model id still send SOMETHING.)
        let env = goose_env(&goose_opts(Some("gpt-4o-mini")), None);
        let model = env.iter().find(|(k, _)| k == "GOOSE_MODEL").unwrap();
        assert_eq!(model.1, "gpt-4o-mini");

        let env = goose_env(&goose_opts(None), None);
        let model = env.iter().find(|(k, _)| k == "GOOSE_MODEL").unwrap();
        assert_eq!(model.1, "minimax");
    }

    #[test]
    fn goose_env_provider_defaults_to_openai() {
        // Default provider is `openai`. We exercise the explicit
        // "force default" path (`provider_override = Some(None)`) which
        // skips ambient `$GOOSE_PROVIDER` — that gives the test
        // deterministic, thread-safe behavior. We deliberately do NOT
        // call `std::env::set_var`/`remove_var`: those are `unsafe` in
        // Rust 2024 because they're racy with parallel readers, and
        // `cargo test` runs tests in parallel by default — so a
        // mutation here would race with any other test that reads the
        // same env var.
        let env = goose_env(&goose_opts(None), Some(None));
        let provider = env.iter().find(|(k, _)| k == "GOOSE_PROVIDER").unwrap();
        assert_eq!(provider.1, "openai");
    }

    #[test]
    fn goose_env_provider_honors_explicit_override() {
        // Caller-supplied provider override (the test seam) always wins.
        // This avoids touching `std::env` (racy + `unsafe` in 2024) and
        // gives the test deterministic, thread-safe behavior.
        let env = goose_env(&goose_opts(None), Some(Some("anthropic")));
        let provider = env.iter().find(|(k, _)| k == "GOOSE_PROVIDER").unwrap();
        assert_eq!(provider.1, "anthropic");
    }

    // ---- Real ACP v1 wire-shape mock ------------------------------------
    //
    // The previous mock encoded the WRONG shape: it emitted session/update
    // notifications with the discriminator and content fields flat on
    // `params`, e.g. `params.sessionUpdate == "agent_message_chunk"` and
    // `params.text == "hello"`. The real ACP v1 spec (and the real goose
    // binary) nest both under `params.update`:
    //
    //     { "method": "session/update",
    //       "params": { "sessionId": "...",
    //                   "update": { "sessionUpdate": "agent_message_chunk",
    //                               "content": { "type": "text",
    //                                            "text": "hello" } } } }
    //
    // This mock reproduces that shape verbatim. The previous version was a
    // kind lie: it tested the parser against a shape the parser happened to
    // accept (because it fell back to flat fields), but that fallback was
    // exactly the bug we needed to fix.

    /// Frames the engine emits, in order. Each line is one JSON object.
    /// Convenience builders below keep the test bodies focused on the
    /// scenario, not the boilerplate.
    struct MockGoose {
        /// Frames the engine receives (initialize / new / prompt /
        /// permission-reply / cancel-notification, in order).
        pub received: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        /// Frames the engine emits, in order. Each line is one JSON object.
        pub outbound: Vec<String>,
    }

    /// Build a real `session/update` notification for `update` (a
    /// `SessionUpdate` discriminated-union variant). The variant's body
    /// goes under `params.update`, NOT flat on `params`.
    fn update(session_id: &str, update: serde_json::Value) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": update
            }
        })
        .to_string()
    }

    /// Build a `session/request_permission` REQUEST (has `id`) — what
    /// goose sends in approve mode before running a tool. The driver must
    /// reply with `{outcome: {outcome: "selected", optionId: ...}}` per
    /// the ApprovalPolicy.
    ///
    /// Each option is `(optionId, name, kind)`:
    ///   * `optionId` is the OPAQUE server-side id (what we echo back in
    ///     `result.outcome.optionId`). Real goose uses server-generated ids
    ///     like `"opt-A9F1"`; tests can use anything opaque.
    ///   * `name` is a human label, ignored by the driver.
    ///   * `kind` is the SEMANTIC tag the driver matches against. Per
    ///     ACP v1 the legal values are `allow_once`, `allow_always`,
    ///     `reject_once`, `reject_always`. Tests pass the kinds they
    ///     expect the driver to pick; the driver is NOT supposed to look
    ///     at `optionId` for selection.
    fn permission_request(
        id: &str,
        session_id: &str,
        tool_title: &str,
        options: Vec<(&str, &str, &str)>,
    ) -> String {
        let opts: Vec<_> = options
            .iter()
            .map(|(oid, name, kind)| {
                serde_json::json!({"optionId": oid, "name": name, "kind": kind})
            })
            .collect();
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/request_permission",
            "params": {
                "sessionId": session_id,
                "toolCall": { "title": tool_title, "kind": "shell" },
                "options": opts,
            }
        })
        .to_string()
    }

    /// The mock server task: reads init/new/prompt from the client,
    /// replies, and emits any canned outbound frames in order. Any
    /// `session/request_permission` request is read-after-write so the
    /// client's reply lands in `received` for the test to assert on.
    async fn run_mock(server: MockGoose, engine_io: tokio::io::DuplexStream) {
        use tokio::io::AsyncWriteExt as _;
        let (r, mut w) = tokio::io::split(engine_io);
        let mut r = tokio::io::BufReader::new(r);

        async fn read_one(
            r: &mut tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
            received: &std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        ) {
            let mut line = String::new();
            let _ = r.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                received.lock().unwrap().push(v);
            }
        }
        async fn write_one(
            w: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
            v: serde_json::Value,
        ) {
            let s = serde_json::to_string(&v).unwrap();
            let _ = w.write_all(s.as_bytes()).await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;
        }

        // 1. read initialize -> reply
        read_one(&mut r, &server.received).await;
        write_one(
            &mut w,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "init",
                "result": { "protocolVersion": 1 }
            }),
        )
        .await;

        // 2. read session/new -> reply
        read_one(&mut r, &server.received).await;
        write_one(
            &mut w,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "new",
                "result": { "sessionId": "goose-test-session-1" }
            }),
        )
        .await;

        // 3. emit the canned outbound frames (session/update
        //    notifications, permission requests, etc.). For permission
        //    requests we wait for the client's reply before moving on
        //    so the test can assert on it.
        for frame in &server.outbound {
            let parsed: serde_json::Value = serde_json::from_str(frame).unwrap();
            write_one(&mut w, parsed.clone()).await;
            if parsed.get("method").and_then(serde_json::Value::as_str)
                == Some("session/request_permission")
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                read_one(&mut r, &server.received).await;
            }
        }

        // 4. read session/prompt
        read_one(&mut r, &server.received).await;

        // 5. final prompt response with terminal stopReason.
        write_one(
            &mut w,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "prompt",
                "result": { "stopReason": "end_turn" }
            }),
        )
        .await;

        // Give the client a moment to read the response.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    /// Run the goose driver against a mock engine speaking the real
    /// ACP wire shape, returning the captured requests + final run +
    /// streamed events.
    async fn drive_against_mock(
        outbound: Vec<String>,
        show_reasoning: bool,
        approval: ApprovalPolicy,
    ) -> (Vec<serde_json::Value>, AgentRun, Vec<AgentEvent>) {
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let server = MockGoose {
            received: received.clone(),
            outbound,
        };
        let (client_io, engine_io) = tokio::io::duplex(128 * 1024);
        let server_handle = tokio::spawn(run_mock(server, engine_io));
        let (mut r, mut w) = tokio::io::split(client_io);
        let mut r = tokio::io::BufReader::new(&mut r);
        let mut opts = goose_opts(Some("gpt-4o-mini"));
        opts.model_id = Some("routed-model-1".to_string());
        opts.show_reasoning = show_reasoning;
        opts.approval = approval;
        let mut events: Vec<AgentEvent> = Vec::new();
        let res = drive_goose_io(&opts, &mut r, &mut w, &mut |ev| {
            events.push(ev);
        })
        .await
        .expect("drive_goose_io should succeed against a well-behaved mock");
        let _ = server_handle.await;
        let frames = received.lock().unwrap().clone();
        (frames, res, events)
    }

    #[tokio::test]
    async fn goose_drive_handshake_emits_standard_acp() {
        // The goose driver must speak STANDARD ACP, not zeroclaw extensions.
        // That means: initialize has protocolVersion (camelCase) + no
        // agent_alias; session/new has cwd + mcpServers only.
        let (frames, _run, _events) =
            drive_against_mock(vec![], false, ApprovalPolicy::Allowlist).await;
        assert_eq!(frames.len(), 3, "expected init/new/prompt frames");
        // initialize
        let init = &frames[0];
        assert_eq!(init["method"], "initialize");
        let params = &init["params"];
        assert!(
            params.get("protocolVersion").is_some(),
            "initialize must carry protocolVersion (camelCase)"
        );
        assert!(
            params.get("agent_alias").is_none() && params.get("agentAlias").is_none(),
            "initialize must NOT carry zeroclaw's agent_alias"
        );
        assert!(
            params.get("chat_mode").is_none() && params.get("chatMode").is_none(),
            "initialize must NOT carry zeroclaw's chat_mode"
        );
        // session/new
        let new = &frames[1];
        assert_eq!(new["method"], "session/new");
        let np = &new["params"];
        assert_eq!(np["cwd"], "/tmp");
        assert!(np["mcpServers"].is_array());
        assert_eq!(np["mcpServers"].as_array().unwrap().len(), 0);
        assert!(
            np.get("agent_alias").is_none() && np.get("agentAlias").is_none(),
            "session/new must NOT carry zeroclaw's agent_alias"
        );
        assert!(
            np.get("chat_mode").is_none() && np.get("chatMode").is_none(),
            "session/new must NOT carry zeroclaw's chat_mode"
        );
        // session/prompt
        let prompt = &frames[2];
        assert_eq!(prompt["method"], "session/prompt");
        let pp = &prompt["params"];
        assert_eq!(pp["sessionId"], "goose-test-session-1");
        let arr = pp["prompt"].as_array().expect("prompt must be an array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "hello");
    }

    #[tokio::test]
    async fn goose_drive_parses_real_acp_update_shape() {
        // The CORE wire-shape test: every `session/update` variant we care
        // about nests its discriminator under `params.update.sessionUpdate`
        // and its content under `params.update.content` (an object:
        // `{type: "text", text: "..."}`).
        let outbound = vec![
            update(
                "goose-test-session-1",
                serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "hello " }
                }),
            ),
            update(
                "goose-test-session-1",
                serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "world" }
                }),
            ),
            update(
                "goose-test-session-1",
                serde_json::json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call-1",
                    "title": "shell",
                    "kind": "shell",
                    "status": "pending"
                }),
            ),
            update(
                "goose-test-session-1",
                serde_json::json!({
                    "sessionUpdate": "usage_update",
                    "used": 42,
                    "size": 131072
                }),
            ),
        ];
        let (frames, run, events) =
            drive_against_mock(outbound, false, ApprovalPolicy::Allowlist).await;
        assert_eq!(frames.len(), 3, "expected init/new/prompt");
        assert_eq!(run.session_id, "goose-test-session-1");
        assert_eq!(run.outcome, "completed");
        assert_eq!(run.content, "hello world");
        assert_eq!(run.tool_calls, 1);
        assert_eq!(run.input_tokens, 42);
        let mut text_count = 0;
        let mut tool_count = 0;
        let mut usage_count = 0;
        for ev in &events {
            match ev {
                AgentEvent::Text(_) => text_count += 1,
                AgentEvent::ToolCall { name } if name == "shell" => tool_count += 1,
                AgentEvent::Usage { .. } => usage_count += 1,
                _ => {}
            }
        }
        assert_eq!(text_count, 2);
        assert_eq!(tool_count, 1);
        assert_eq!(usage_count, 1);
    }

    #[tokio::test]
    async fn goose_drive_ignores_flat_wrong_shape_updates() {
        // The OLD mock emitted `params.sessionUpdate` and `params.text`
        // FLAT on params — i.e. the wrong shape. The fixed parser must
        // NOT pick up text from those positions; the turn should run to
        // completion with empty content.
        let outbound = vec![serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "goose-test-session-1",
                // Flat-on-params (the OLD wrong shape):
                "sessionUpdate": "agent_message_chunk",
                "text": "should-not-be-picked-up"
            }
        })
        .to_string()];
        let (_frames, run, events) =
            drive_against_mock(outbound, false, ApprovalPolicy::Allowlist).await;
        assert_eq!(run.content, "");
        assert!(
            events.iter().all(|e| !matches!(e, AgentEvent::Text(_))),
            "old flat shape must not surface as Text events"
        );
    }

    #[tokio::test]
    async fn goose_drive_surfaces_reasoning_only_when_requested() {
        // Thought events must be filtered by show_reasoning. Real shape:
        // `params.update.sessionUpdate == "agent_thought_chunk"` with
        // content under `params.update.content`.
        let outbound = vec![update(
            "goose-test-session-1",
            serde_json::json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": "thinking..." }
            }),
        )];
        let (_, _, events_off) =
            drive_against_mock(outbound.clone(), false, ApprovalPolicy::Allowlist).await;
        assert!(
            events_off
                .iter()
                .all(|e| !matches!(e, AgentEvent::Thought(_))),
            "Thought must NOT be surfaced when show_reasoning is off"
        );
        let (_, _, events_on) = drive_against_mock(outbound, true, ApprovalPolicy::Allowlist).await;
        assert!(
            events_on
                .iter()
                .any(|e| matches!(e, AgentEvent::Thought(t) if t == "thinking...")),
            "Thought must be surfaced when show_reasoning is on"
        );
    }

    #[tokio::test]
    async fn goose_drive_answers_permission_requests() {
        // Real goose (in approve / smart_approve modes) sends a
        // `session/request_permission` JSON-RPC REQUEST with an id and
        // expects a reply of the shape
        //   { "outcome": { "outcome": "selected", "optionId": "<id>" } }
        // The driver MUST answer — otherwise goose wedges. Under
        // Allowlist + a non-allowlisted tool we must pick a deny option;
        // under Allowlist + an allowlisted tool we must pick an allow
        // option.
        let outbound = vec![permission_request(
            "perm-1",
            "goose-test-session-1",
            "shell",
            vec![
                ("allow_once", "Allow once", "allow_once"),
                ("decline_once", "Deny once", "reject_once"),
            ],
        )];
        let (frames, _run, events) =
            drive_against_mock(outbound, false, ApprovalPolicy::Allowlist).await;
        assert_eq!(frames.len(), 4);
        let reply = &frames[3];
        assert_eq!(reply["id"], "perm-1");
        assert_eq!(
            reply["result"]["outcome"]["outcome"], "selected",
            "Allowlist+shell must answer with a selected outcome (not cancelled)"
        );
        assert_eq!(
            reply["result"]["outcome"]["optionId"], "decline_once",
            "Allowlist must pick a deny option for non-allowlisted tools"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Approval { tool, approved: false } if tool == "shell"
            )),
            "Approval event with approved=false must be surfaced"
        );
    }

    #[tokio::test]
    async fn goose_drive_permission_allowlist_read_only_tool() {
        // Allowlist policy + READ-ONLY tool (on the allowlist) -> ALLOW.
        let outbound = vec![permission_request(
            "perm-2",
            "goose-test-session-1",
            "read",
            vec![
                ("allow_once", "Allow once", "allow_once"),
                ("decline_once", "Deny once", "reject_once"),
            ],
        )];
        let (frames, _run, events) =
            drive_against_mock(outbound, false, ApprovalPolicy::Allowlist).await;
        let reply = &frames[3];
        assert_eq!(reply["id"], "perm-2");
        assert_eq!(
            reply["result"]["outcome"]["optionId"], "allow_once",
            "Allowlist must pick an allow option for allowlisted tools"
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Approval { tool, approved: true } if tool == "read")),);
    }

    #[tokio::test]
    async fn goose_drive_permission_all_policy_always_allows() {
        // ApprovalPolicy::All -> every request approved.
        let outbound = vec![permission_request(
            "perm-3",
            "goose-test-session-1",
            "shell",
            vec![
                ("allow_once", "Allow once", "allow_once"),
                ("decline_once", "Deny once", "reject_once"),
            ],
        )];
        let (frames, _run, _events) =
            drive_against_mock(outbound, false, ApprovalPolicy::All).await;
        let reply = &frames[3];
        assert_eq!(reply["result"]["outcome"]["optionId"], "allow_once");
    }

    #[tokio::test]
    async fn goose_drive_permission_none_policy_always_denies() {
        // ApprovalPolicy::None -> every request denied.
        let outbound = vec![permission_request(
            "perm-4",
            "goose-test-session-1",
            "read",
            vec![
                ("allow_once", "Allow once", "allow_once"),
                ("decline_once", "Deny once", "reject_once"),
            ],
        )];
        let (frames, _run, _events) =
            drive_against_mock(outbound, false, ApprovalPolicy::None).await;
        let reply = &frames[3];
        assert_eq!(reply["result"]["outcome"]["optionId"], "decline_once");
    }

    #[tokio::test]
    async fn goose_drive_permission_none_with_only_allow_options_cancels() {
        // SECURITY: ApprovalPolicy::None guarantees EVERY request is
        // denied. If the server (maliciously or due to a bug) sends a
        // permission request whose `options[]` contains ONLY allow
        // options (e.g. `["allow_once"]`), the client MUST NOT pick one
        // of them — it must reply with `{outcome: {outcome: "cancelled"}}`
        // so the policy's "always deny" guarantee survives regardless of
        // what the server offers. The previous fallback of selecting
        // `options.first()` for `approved == false` would have let any
        // goose build bypass ApprovalPolicy::None by simply omitting
        // deny options from the request.
        let outbound = vec![permission_request(
            "perm-none-only-allow",
            "goose-test-session-1",
            "read",
            vec![("allow_once", "Allow once", "allow_once")],
        )];
        let (frames, _run, events) =
            drive_against_mock(outbound, false, ApprovalPolicy::None).await;
        let reply = &frames[3];
        assert_eq!(reply["id"], "perm-none-only-allow");
        assert_eq!(
            reply["result"]["outcome"]["outcome"], "cancelled",
            "ApprovalPolicy::None with only-allow options must answer with cancelled, \
             not pick an allow option (server offered only allow_once but policy says deny)"
        );
        // `optionId` must be absent in the cancelled outcome — the spec
        // shape is exactly `{outcome: {outcome: "cancelled"}}`.
        assert!(
            reply["result"]["outcome"].get("optionId").is_none(),
            "cancelled outcome must NOT carry an optionId; got reply={}",
            reply
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Approval { tool, approved: false } if tool == "read"
            )),
            "Approval event with approved=false must be surfaced even when the server \
             offered no deny option (the policy still decided to deny)"
        );
    }

    #[tokio::test]
    async fn goose_drive_permission_no_offered_options_cancels() {
        // Defensive: if a buggy goose sends a permission request with
        // no `options[]` we can't pick anything — fall back to
        // `{outcome: {outcome: "cancelled"}}` so we still REPLY and don't
        // wedge the engine.
        let outbound = vec![serde_json::json!({
            "jsonrpc": "2.0",
            "id": "perm-5",
            "method": "session/request_permission",
            "params": {
                "sessionId": "goose-test-session-1",
                "toolCall": { "title": "shell" },
                "options": []
            }
        })
        .to_string()];
        let (frames, _run, _events) =
            drive_against_mock(outbound, false, ApprovalPolicy::All).await;
        let reply = &frames[3];
        assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
    }

    #[tokio::test]
    async fn goose_drive_sends_cancel_as_notification() {
        // Direct test of the cancel shape: when the prompt-loop budget
        // expires the driver must send a `session/cancel` NOTIFICATION
        // (no `id`, no `result` expected). We simulate this by having
        // the mock ack `init` AND `session/new` (so we DO have a real
        // session id), then go silent during the `session/prompt`
        // response — the inner `read_line` for that response times out
        // per the client's deadline and the timeout handler kicks in.
        //
        // The previous version of this test acked only `init` and then
        // let `session/new` time out — that was a race against the
        // session/cancel-on-uninitialized-session bug: the driver would
        // send `session/cancel` with an empty `sessionId`. We now refuse
        // to do that (sending cancel before session/new completes is
        // forbidden per ACP v1), so the test must be redesigned to
        // exercise the post-handshake path where the cancel is meaningful.
        use tokio::io::AsyncWriteExt as _;
        let received: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (client_io, engine_io) = tokio::io::duplex(64 * 1024);
        let recv_clone = received.clone();
        let server_handle = tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(engine_io);
            let mut r = tokio::io::BufReader::new(&mut r);
            let mut line = String::new();

            // 1. read initialize -> ack
            let _ = r.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                recv_clone.lock().unwrap().push(v);
            }
            let _ = w
                .write_all(
                    serde_json::to_string(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": "init",
                        "result": { "protocolVersion": 1 }
                    }))
                    .unwrap()
                    .as_bytes(),
                )
                .await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;

            // 2. read session/new -> ack with a session id. Now the
            //    client has a real session_id and any subsequent
            //    `session/cancel` will carry it.
            line.clear();
            let _ = r.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                recv_clone.lock().unwrap().push(v);
            }
            let _ = w
                .write_all(
                    serde_json::to_string(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": "new",
                        "result": { "sessionId": "sess-cancel-test" }
                    }))
                    .unwrap()
                    .as_bytes(),
                )
                .await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;

            // 3. read session/prompt but DON'T ack — the client's
            //    deadline is 400ms so its prompt-loop `read_line` will
            //    time out, triggering the cancel path.
            line.clear();
            let _ = r.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                recv_clone.lock().unwrap().push(v);
            }

            // 4. read the cancel-notification the driver sends on
            //    timeout. We use a short bounded read so this test
            //    can't hang on a misbehaving driver.
            let _ = tokio::time::timeout(std::time::Duration::from_millis(800), async {
                let mut cancel_line = String::new();
                let _ = r.read_line(&mut cancel_line).await;
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(cancel_line.trim()) {
                    recv_clone.lock().unwrap().push(v);
                }
            })
            .await;
        });
        let (mut r, mut w) = tokio::io::split(client_io);
        let mut r = tokio::io::BufReader::new(&mut r);
        let mut opts = goose_opts(None);
        opts.timeout = std::time::Duration::from_millis(400);
        let res = drive_goose_io(&opts, &mut r, &mut w, &mut |_| {}).await;
        let _ = server_handle.await;
        let frames = received.lock().unwrap().clone();
        let cancel = frames.iter().find(|f| {
            f.get("method").and_then(serde_json::Value::as_str) == Some("session/cancel")
        });
        assert!(
            cancel.is_some(),
            "driver must send session/cancel on prompt-loop timeout; got frames={:?}",
            frames
        );
        let cancel = cancel.unwrap();
        // CRITICAL: cancel is a NOTIFICATION — no `id`, no `result`.
        assert!(
            cancel.get("id").is_none(),
            "session/cancel must NOT carry an id (it's a notification): got {:?}",
            cancel
        );
        assert!(
            cancel
                .get("params")
                .is_some_and(|p| p.get("sessionId").is_some()),
            "session/cancel must carry sessionId"
        );
        // The cancel must carry the ACTUAL session id from session/new,
        // not the empty placeholder the old bug would have sent.
        assert_eq!(
            cancel["params"]["sessionId"], "sess-cancel-test",
            "session/cancel must carry the session id issued by session/new, not an empty placeholder"
        );
        assert!(
            res.is_ok(),
            "partial turn must be preserved (res={:?})",
            res
        );
        assert_eq!(res.unwrap().outcome, "timeout");
    }

    #[tokio::test]
    async fn goose_drive_does_not_cancel_before_session_new() {
        // Regression for the race condition the reviewer flagged: if
        // `session/new` itself times out (e.g. a wedged engine), the
        // driver MUST NOT send `session/cancel` because there is no
        // session id to put in it. Sending one anyway would either
        // confuse the server (empty `sessionId`) or be outright
        // rejected, and the previous version did exactly that.
        use tokio::io::AsyncWriteExt as _;
        let received: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (client_io, engine_io) = tokio::io::duplex(64 * 1024);
        let recv_clone = received.clone();
        let server_handle = tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(engine_io);
            let mut r = tokio::io::BufReader::new(&mut r);
            // Ack init so the handshake succeeds.
            let mut line = String::new();
            let _ = r.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                recv_clone.lock().unwrap().push(v);
            }
            let _ = w
                .write_all(
                    serde_json::to_string(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": "init",
                        "result": { "protocolVersion": 1 }
                    }))
                    .unwrap()
                    .as_bytes(),
                )
                .await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;
            // Read session/new but never ack — the client's 200ms
            // deadline fires on this read.
            line.clear();
            let _ = r.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                recv_clone.lock().unwrap().push(v);
            }
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        });
        let (mut r, mut w) = tokio::io::split(client_io);
        let mut r = tokio::io::BufReader::new(&mut r);
        let mut opts = goose_opts(None);
        opts.timeout = std::time::Duration::from_millis(200);
        let res = drive_goose_io(&opts, &mut r, &mut w, &mut |_| {}).await;
        let _ = server_handle.await;
        let frames = received.lock().unwrap().clone();
        let cancel = frames.iter().find(|f| {
            f.get("method").and_then(serde_json::Value::as_str) == Some("session/cancel")
        });
        assert!(
            cancel.is_none(),
            "session/cancel must NOT be sent when session/new never completed (no session id to use); got frames={:?}",
            frames
        );
        assert!(
            res.is_err(),
            "session/new timeout must surface as an error (no partial turn exists yet); got res={:?}",
            res
        );
    }

    #[tokio::test]
    async fn goose_drive_handles_permission_after_prompt_without_deadlock() {
        // Regression for the reviewer-flagged deadlock:
        //
        //   "the `session/prompt` request is sent BEFORE the driver enters
        //   the notification/reply loop. If the server emits any
        //   `session/update` notifications or `session/request_permission`
        //   requests between receiving the `session/prompt` and sending
        //   the final `session/prompt` response, the driver will not read
        //   them until AFTER the `session/prompt` response arrives."
        //
        // This test simulates the exact ordering the reviewer warned
        // about: the server first reads `session/prompt` (so the client
        // knows the prompt was delivered), then immediately writes a
        // `session/request_permission` REQUEST, then BLOCKS waiting for
        // the client's reply before sending the prompt response.
        //
        // The client MUST read the permission request, answer it, and
        // THEN continue waiting for the prompt response. If the driver
        // wrote the prompt BEFORE entering the read loop (the bug),
        // this would not be a deadlock on `tokio::io::duplex` (which is
        // a single in-memory buffer) BUT it would still be observable
        // as "frames reordered": the client would read the prompt
        // response before the permission request, or miss the request
        // entirely. On a real `ChildStdout`/`ChildStdin` pair (two OS
        // pipes with bounded buffers), the OLD ordering could deadlock
        // because the server's write pipe fills up with the permission
        // request while the client hasn't started reading yet.
        //
        // We assert the GOOD outcome: the driver answers the permission
        // request with `decline_once` (Allowlist+shell), streams the
        // assistant text from a follow-up `session/update`, and finally
        // consumes the prompt response with stopReason end_turn.
        use tokio::io::AsyncWriteExt as _;
        let received: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (client_io, engine_io) = tokio::io::duplex(64 * 1024);
        let recv_clone = received.clone();
        let server_handle = tokio::spawn(async move {
            let (mut r, mut w) = tokio::io::split(engine_io);
            let mut r = tokio::io::BufReader::new(&mut r);
            let mut line = String::new();

            // 1. read initialize -> ack
            let _ = r.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                recv_clone.lock().unwrap().push(v);
            }
            let _ = w
                .write_all(
                    serde_json::to_string(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": "init",
                        "result": { "protocolVersion": 1 }
                    }))
                    .unwrap()
                    .as_bytes(),
                )
                .await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;

            // 2. read session/new -> ack with a session id
            line.clear();
            let _ = r.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                recv_clone.lock().unwrap().push(v);
            }
            let _ = w
                .write_all(
                    serde_json::to_string(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": "new",
                        "result": { "sessionId": "goose-deadlock-test" }
                    }))
                    .unwrap()
                    .as_bytes(),
                )
                .await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;

            // 3. read session/prompt — the server now has the prompt in
            //    hand. THIS is the critical step the reviewer flagged:
            //    the previous driver sent the prompt and then entered
            //    the read loop, leaving a window where the server could
            //    emit frames the client wasn't yet reading. The fixed
            //    driver enters the loop first and sends the prompt on
            //    the first iteration, so the reader is parked before
            //    any reply can be queued.
            line.clear();
            let _ = r.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                recv_clone.lock().unwrap().push(v);
            }

            // 4. server emits a `session/request_permission` REQUEST
            //    immediately, with no preamble. If the client isn't
            //    reading yet (the OLD bug), this lands in the pipe
            //    buffer and the server's next write blocks. The fixed
            //    driver is already inside the read loop and will pick
            //    this up on its next `read_line`.
            let perm = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "perm-deadlock",
                "method": "session/request_permission",
                "params": {
                    "sessionId": "goose-deadlock-test",
                    "toolCall": { "title": "shell", "kind": "shell" },
                    "options": [
                        { "optionId": "allow_once",   "name": "Allow once", "kind": "allow_once" },
                        { "optionId": "decline_once", "name": "Deny once",   "kind": "reject_once" }
                    ]
                }
            });
            let _ = w
                .write_all(serde_json::to_string(&perm).unwrap().as_bytes())
                .await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;

            // 5. server blocks waiting for the client's reply. With a
            //    bounded OS pipe, this is exactly the step where the
            //    OLD ordering could deadlock — the server's stdout
            //    pipe would be full of the permission request while
            //    the client was still flushing the prompt write and
            //    hadn't started its read loop yet.
            line.clear();
            let _ = r.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                recv_clone.lock().unwrap().push(v);
            }

            // 6. permission was answered. Now the server streams a
            //    chunk of assistant text and then terminates the turn.
            let chunk = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "goose-deadlock-test",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "done." }
                    }
                }
            });
            let _ = w
                .write_all(serde_json::to_string(&chunk).unwrap().as_bytes())
                .await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;

            // 7. prompt response with stopReason end_turn.
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "prompt",
                "result": { "stopReason": "end_turn" }
            });
            let _ = w
                .write_all(serde_json::to_string(&resp).unwrap().as_bytes())
                .await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;

            // Give the client a moment to drain.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        let (mut r, mut w) = tokio::io::split(client_io);
        let mut r = tokio::io::BufReader::new(&mut r);
        let mut opts = goose_opts(None);
        opts.approval = ApprovalPolicy::Allowlist;
        opts.timeout = std::time::Duration::from_secs(2);
        let mut events: Vec<AgentEvent> = Vec::new();
        let res = drive_goose_io(&opts, &mut r, &mut w, &mut |ev| {
            events.push(ev);
        })
        .await
        .expect("drive_goose_io must not deadlock on permission-after-prompt");
        let _ = server_handle.await;

        // Assert the order in which the client sent frames: prompt
        // first, permission reply second. If the OLD ordering were in
        // place, the prompt and reply might still both arrive but the
        // prompt-response could be read before the permission request.
        let frames = received.lock().unwrap().clone();
        let methods: Vec<&str> = frames
            .iter()
            .filter_map(|f| f.get("method").and_then(serde_json::Value::as_str))
            .collect();
        // Frames with `id == "perm-deadlock"` are replies; everything
        // else with a method is a notification from the server.
        let prompt_idx = frames.iter().position(|f| {
            f.get("method").and_then(serde_json::Value::as_str) == Some("session/prompt")
        });
        let perm_reply_idx = frames
            .iter()
            .position(|f| f.get("id").and_then(serde_json::Value::as_str) == Some("perm-deadlock"));
        assert!(
            prompt_idx.is_some(),
            "client must have sent session/prompt; got frames={:?}",
            frames
        );
        assert!(
            perm_reply_idx.is_some(),
            "client must have replied to session/request_permission; got frames={:?}",
            frames
        );
        let pi = prompt_idx.unwrap();
        let ri = perm_reply_idx.unwrap();
        assert!(
            pi < ri,
            "session/prompt must be sent BEFORE the permission reply (frames in order); got prompt at {}, reply at {}, methods={:?}",
            pi,
            ri,
            methods
        );
        // Verify the reply picked `decline_once` (Allowlist + shell).
        let reply = &frames[ri];
        assert_eq!(
            reply["result"]["outcome"]["outcome"], "selected",
            "permission reply must select an option"
        );
        assert_eq!(
            reply["result"]["outcome"]["optionId"], "decline_once",
            "Allowlist+shell must decline"
        );
        // And the final turn completed with content from the chunk.
        assert_eq!(res.outcome, "completed");
        assert_eq!(res.content, "done.");
        assert!(
            events.iter().any(
                |e| matches!(e, AgentEvent::Approval { tool, approved: false } if tool == "shell")
            ),
            "Approval event with approved=false must be surfaced for the declined shell call"
        );
    }

    #[tokio::test]
    async fn connected_transport_kill_child_reaps_process() {
        // End-to-end test of the reap guarantee: spawn a real child that
        // sleeps forever, wrap it in a ConnectedTransport, call
        // kill_child(), and verify the OS-level process is gone. Without
        // the reap path this test would hang (or, after timeout, report
        // a zombie).
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("sleep 60")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(false);
        let mut child = cmd.spawn().expect("spawn sleep");
        let pid = child.id().expect("child has pid");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut conn = ConnectedTransport {
            reader: TransportReader::ChildStdout(stdout),
            writer: TransportWriter::ChildStdin(stdin),
            child: Some(child),
        };
        // Sanity: process is alive right now.
        // We use `libc::kill(pid, 0)` as a portable liveness probe (returns
        // 0 if the process exists, -1 with errno=ESRCH otherwise). The
        // `libc` crate resolves the right `#[link]` attribute per platform
        // (Linux glibc/musl, macOS libSystem, Android bionic, Windows
        // ucrt, BSDs, etc.), so the test compiles and links on every
        // platform zoder supports. A raw `#[link(name = "c")] extern "C"`
        // declaration would force `-lc`, which fails on strict linkers
        // (notably macOS, where the C library lives in `libSystem`).
        let alive_before = unsafe { libc::kill(pid as libc::pid_t, 0) };
        assert_eq!(alive_before, 0, "spawned child must be alive before reap");
        // Reap it.
        conn.kill_child().await;
        // And it's gone.
        let alive_after = unsafe { libc::kill(pid as libc::pid_t, 0) };
        assert_eq!(
            alive_after, -1,
            "spawned child must be reaped by kill_child() (kill(0) returned {})",
            alive_after
        );
        // Idempotent: calling again is fine and doesn't panic.
        conn.kill_child().await;
    }

    #[tokio::test]
    async fn run_goose_agent_backstop_timeout_reaps_child() {
        // Regression test for the timeout-child-leak bug:
        //
        //   Before the fix, `run_goose_agent` constructed the
        //   `ConnectedTransport` INSIDE the future passed to
        //   `tokio::time::timeout(backstop, fut)`. When the backstop
        //   fired, `fut` was dropped, which dropped the `Child` (with
        //   `kill_on_drop = false` set, so the OS process was orphaned
        //   — never reaped).
        //
        //   The fix owns the `ConnectedTransport` OUTSIDE the timeout
        //   future and calls `kill_child()` UNCONDITIONALLY after the
        //   timeout race, on every exit path (Ok / Err / Elapsed).
        //
        //   This test exercises that exact pattern end-to-end with a
        //   real spawned child. We hand-build a `ConnectedTransport`
        //   around a `/bin/sh -c 'sleep 60'` (so the test doesn't
        //   depend on a `goose` binary), call the public seam
        //   `run_goose_agent_with_conn` with a tiny inner timeout, and
        //   verify that the spawned PID is reaped (`libc::kill(pid, 0)`
        //   returns -1) when the seam returns.
        //
        //   Without the fix, the future holding `conn.child` would be
        //   dropped by the outer timeout, and the PID would still be
        //   alive — the second `libc::kill(pid, 0)` would return 0.
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg("sleep 60")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(false);
        let mut child = cmd.spawn().expect("spawn /bin/sh -c 'sleep 60'");
        let pid = child.id().expect("spawned sleep child must have a pid");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut conn = ConnectedTransport {
            reader: TransportReader::ChildStdout(stdout),
            writer: TransportWriter::ChildStdin(stdin),
            child: Some(child),
        };

        // Sanity: the process is alive right now.
        let alive_before = unsafe { libc::kill(pid as libc::pid_t, 0) };
        assert_eq!(
            alive_before, 0,
            "spawned sleep child must be alive immediately after spawn"
        );

        // Tiny inner timeout so the inner drive hits its own deadline
        // first (returning `Err` from `read_result(..., "init")`). The
        // production seam's UNCONDITIONAL `kill_child()` runs after
        // the `tokio::time::timeout` returns, on every exit path —
        // including this `Err` branch (which is the SAME code path as
        // the `Elapsed` branch in terms of reap: the same `conn.kill_child().await`
        // line runs in both cases). We additionally assert the literal
        // `Elapsed` pattern below.
        let mut opts = goose_opts(None);
        opts.timeout = std::time::Duration::from_millis(50);
        let res = run_goose_agent_with_conn(&mut conn, &opts, |_| {}).await;
        let err_msg = res.expect_err(
            "the inner drive must error out (sleep emits no ACP bytes); got Ok",
        );
        let err_text = format!("{err_msg:#}");
        assert!(
            err_text.contains("timed out")
                || err_text.contains("hang during setup")
                || err_text.contains("initialize handshake"),
            "expected a timeout-flavored error from the inner drive, got: {err_text}",
        );
        // The unconditional reap MUST have run. This is the bug being
        // tested: with the OLD code, `conn.kill_child()` was INSIDE
        // the future, so it ran on inner-Ok and inner-Err but NOT on
        // the outer-backstop Elapsed branch. The FIX moves the reap
        // to AFTER `tokio::time::timeout(...)` returns, so it runs on
        // every branch — including the inner-Err branch (asserted
        // here) and the outer Elapsed branch (asserted next).
        let alive_after = unsafe { libc::kill(pid as libc::pid_t, 0) };
        assert_eq!(
            alive_after, -1,
            "spawned child must be reaped after run_goose_agent_with_conn returns; \
             this is the unconditional-kill guarantee (kill(0) returned {})",
            alive_after
        );

        // LITERAL Elapsed-branch assertion: spawn another sleep child,
        // build the exact pattern the FIX introduces (timeout race
        // whose inner future parks forever and is dropped on Elapsed,
        // followed by an unconditional reap), and verify the child is
        // gone. The OLD code placed the reap inside the future, so
        // this branch would leave the child alive.
        let mut cmd2 = tokio::process::Command::new("/bin/sh");
        cmd2.arg("-c")
            .arg("sleep 60")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(false);
        let mut child2 = cmd2.spawn().expect("spawn second sleep child");
        let pid2 = child2.id().expect("second child must have pid");
        let stdin2 = child2.stdin.take().expect("piped stdin");
        let stdout2 = child2.stdout.take().expect("piped stdout");
        let mut conn2 = ConnectedTransport {
            reader: TransportReader::ChildStdout(stdout2),
            writer: TransportWriter::ChildStdin(stdin2),
            child: Some(child2),
        };
        let alive2_before = unsafe { libc::kill(pid2 as libc::pid_t, 0) };
        assert_eq!(alive2_before, 0, "second sleep child must be alive");

        // Inner future parks forever (no ACP output) — the timeout
        // MUST Elapse, dropping this future (and the `&mut conn2`
        // borrow). We then reap unconditionally — exactly what the
        // fix does after `tokio::time::timeout(...)` returns.
        let inner = async {
            let _reader = std::mem::replace(
                &mut conn2.reader,
                dummy_reader_for_tests(),
            );
            let _writer = std::mem::replace(
                &mut conn2.writer,
                dummy_writer_for_tests(),
            );
            std::future::pending::<()>().await;
        };
        let raced = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            inner,
        )
        .await;
        assert!(
            raced.is_err(),
            "wrapper timeout must Elapse (inner future parks forever)"
        );
        conn2.kill_child().await;
        let alive2_after = unsafe { libc::kill(pid2 as libc::pid_t, 0) };
        assert_eq!(
            alive2_after, -1,
            "second child must be reaped by the unconditional kill_child() that runs \
             AFTER the timeout Elapses; this is the exact pattern the fix introduces \
             (kill(0) returned {})",
            alive2_after
        );
    }

    /// Test-seam placeholders matching the production `dummy_reader` /
    /// `dummy_writer` (defined in the parent module). They exist so
    /// `mem::replace` in the Elapsed-branch test below has somewhere
    /// to put the moved halves.
    fn dummy_reader_for_tests() -> TransportReader {
        let (a, _b) = tokio::net::UnixStream::pair().expect("unix stream pair");
        TransportReader::Unix(tokio::io::split(a).0)
    }
    fn dummy_writer_for_tests() -> TransportWriter {
        let (a, _b) = tokio::net::UnixStream::pair().expect("unix stream pair");
        TransportWriter::Unix(tokio::io::split(a).1)
    }

    #[tokio::test]
    async fn goose_drive_permission_selects_by_kind_with_opaque_ids() {
        // Regression for the permission-option-selected-by-wrong-field bug:
        //
        //   The OLD `drive_goose_io` matched `options[].optionId` against
        //   hard-coded names like `allow_once` / `reject_once`. That is
        //   wrong per ACP v1: `optionId` is an OPAQUE server-side id (real
        //   goose may surface ids like `opt-A9F1`), and the SEMANTIC
        //   meaning lives in `options[].kind`. The fixed driver matches
        //   by `kind` (`allow_once` / `allow_always` / `reject_once` /
        //   `reject_always`) and echoes back THAT option's actual
        //   `optionId` in the reply.
        //
        //   This test uses opaque, server-style ids (`allow-id-7af3`,
        //   `deny-id-2b41`, etc.) plus meaningful kinds, and asserts
        //   the reply's `optionId` is the one whose `kind` matches the
        //   policy decision — NOT the position in `options[]` and NOT
        //   a hard-coded name.
        //
        //   We cover both directions:
        //     (a) Allow policy + allow kind -> echo the allow-id.
        //     (b) Deny policy  + reject kind -> echo the deny-id.
        //     (c) Allow policy + only reject kind offered -> fall back
        //         to `options.first()` (still echo its optionId).
        //     (d) Deny policy  + only allow kind offered -> `cancelled`
        //         (security guard: never allow-fallback under a deny
        //         policy, key on kind).
        //     (e) Position in options[] is irrelevant: shuffling the
        //         order must not change the picked optionId.

        // (a) Allow + allow kind -> echoes the allow-id (the option
        // whose kind == "allow_once"), regardless of position.
        let outbound = vec![serde_json::json!({
            "jsonrpc": "2.0",
            "id": "perm-opaque-allow",
            "method": "session/request_permission",
            "params": {
                "sessionId": "goose-test-session-1",
                "toolCall": { "title": "shell", "kind": "shell" },
                "options": [
                    // Deny option FIRST, allow option SECOND. The driver
                    // must pick the allow option (by kind), NOT the
                    // first option in the list.
                    { "optionId": "deny-id-2b41",  "name": "Deny once",   "kind": "reject_once" },
                    { "optionId": "allow-id-7af3", "name": "Allow once",  "kind": "allow_once" },
                    { "optionId": "deny-always-9c", "name": "Deny always","kind": "reject_always" }
                ]
            }
        })
        .to_string()];
        let (frames, _run, _events) =
            drive_against_mock(outbound, false, ApprovalPolicy::All).await;
        let reply = &frames[3];
        assert_eq!(reply["id"], "perm-opaque-allow");
        assert_eq!(reply["result"]["outcome"]["outcome"], "selected");
        assert_eq!(
            reply["result"]["outcome"]["optionId"], "allow-id-7af3",
            "allow+allow-kind must echo the allow option's opaque id (matched by kind, \
             NOT by position and NOT by a hard-coded name); got reply={}",
            reply
        );

        // (b) Deny + reject kind -> echoes the reject-id.
        let outbound = vec![serde_json::json!({
            "jsonrpc": "2.0",
            "id": "perm-opaque-deny",
            "method": "session/request_permission",
            "params": {
                "sessionId": "goose-test-session-1",
                "toolCall": { "title": "shell", "kind": "shell" },
                "options": [
                    { "optionId": "allow-id-aaaa",    "name": "Allow once",   "kind": "allow_once" },
                    { "optionId": "allow-always-bbb", "name": "Allow always", "kind": "allow_always" },
                    { "optionId": "reject-id-cccc",   "name": "Deny once",    "kind": "reject_once" }
                ]
            }
        })
        .to_string()];
        let (frames, _run, _events) =
            drive_against_mock(outbound, false, ApprovalPolicy::None).await;
        let reply = &frames[3];
        assert_eq!(reply["id"], "perm-opaque-deny");
        assert_eq!(reply["result"]["outcome"]["outcome"], "selected");
        assert_eq!(
            reply["result"]["outcome"]["optionId"], "reject-id-cccc",
            "deny+reject-kind must echo the reject option's opaque id (matched by kind); \
             got reply={}",
            reply
        );

        // (c) Allow + ONLY reject kind offered -> fall back to
        // options.first() (we WANT to allow, so we take whatever the
        // server offered; the server's id is what we echo).
        let outbound = vec![serde_json::json!({
            "jsonrpc": "2.0",
            "id": "perm-allow-only-reject",
            "method": "session/request_permission",
            "params": {
                "sessionId": "goose-test-session-1",
                "toolCall": { "title": "shell", "kind": "shell" },
                "options": [
                    { "optionId": "reject-id-only", "name": "Deny once", "kind": "reject_once" }
                ]
            }
        })
        .to_string()];
        let (frames, _run, _events) =
            drive_against_mock(outbound, false, ApprovalPolicy::All).await;
        let reply = &frames[3];
        assert_eq!(reply["id"], "perm-allow-only-reject");
        assert_eq!(reply["result"]["outcome"]["outcome"], "selected");
        assert_eq!(
            reply["result"]["outcome"]["optionId"], "reject-id-only",
            "allow policy with only a reject-kind option offered must fall back to \
             options.first() and echo that opaque id (we WANT to allow)"
        );

        // (d) Deny + ONLY allow kind offered -> cancelled. The
        // security guard MUST hold under kind-based matching: a
        // malicious or buggy server must not be able to force an
        // allow decision by omitting reject-kind options.
        let outbound = vec![serde_json::json!({
            "jsonrpc": "2.0",
            "id": "perm-deny-only-allow",
            "method": "session/request_permission",
            "params": {
                "sessionId": "goose-test-session-1",
                "toolCall": { "title": "shell", "kind": "shell" },
                "options": [
                    { "optionId": "allow-id-only", "name": "Allow once", "kind": "allow_once" }
                ]
            }
        })
        .to_string()];
        let (frames, _run, _events) =
            drive_against_mock(outbound, false, ApprovalPolicy::None).await;
        let reply = &frames[3];
        assert_eq!(reply["id"], "perm-deny-only-allow");
        assert_eq!(
            reply["result"]["outcome"]["outcome"], "cancelled",
            "deny policy with only an allow-kind option offered MUST answer with cancelled, \
             not silently pick the allow option (security guard keyed on kind)"
        );
        assert!(
            reply["result"]["outcome"].get("optionId").is_none(),
            "cancelled outcome must NOT carry an optionId; got reply={}",
            reply
        );

        // (e) Position in options[] is irrelevant: shuffling order
        // must not change which optionId gets echoed. Same kinds as
        // case (a) but the allow kind is now in the MIDDLE position.
        let outbound = vec![serde_json::json!({
            "jsonrpc": "2.0",
            "id": "perm-position-irrelevant",
            "method": "session/request_permission",
            "params": {
                "sessionId": "goose-test-session-1",
                "toolCall": { "title": "shell", "kind": "shell" },
                "options": [
                    { "optionId": "deny-id-X",     "name": "Deny once",   "kind": "reject_once" },
                    { "optionId": "allow-id-MID",  "name": "Allow once",  "kind": "allow_once" },
                    { "optionId": "deny-always-Y", "name": "Deny always", "kind": "reject_always" }
                ]
            }
        })
        .to_string()];
        let (frames, _run, _events) =
            drive_against_mock(outbound, false, ApprovalPolicy::All).await;
        let reply = &frames[3];
        assert_eq!(reply["result"]["outcome"]["outcome"], "selected");
        assert_eq!(
            reply["result"]["outcome"]["optionId"], "allow-id-MID",
            "matching is by kind, not by position — the allow-kind option is in \
             the middle and must still be picked"
        );
    }

    #[tokio::test]
    async fn goose_drive_maps_stop_reasons() {
        // max_tokens -> "max_tokens" (succeeded=false), cancelled ->
        // "cancelled" (succeeded=false), end_turn -> "completed"
        // (succeeded=true). Helper spins up a fresh mock per stop-reason.
        use tokio::io::AsyncWriteExt as _;
        async fn run_with_stop_reason(stop: String) -> AgentRun {
            let received: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
                std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let (client_io, engine_io) = tokio::io::duplex(64 * 1024);
            let stop_for_task = stop.clone();
            let recv_clone = received.clone();
            let _server_handle = tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(engine_io);
                let mut r = tokio::io::BufReader::new(&mut r);
                let mut line = String::new();
                // init
                let _ = r.read_line(&mut line).await;
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                    recv_clone.lock().unwrap().push(v);
                }
                let init = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "init",
                    "result": { "protocolVersion": 1 }
                });
                let _ = w
                    .write_all(serde_json::to_string(&init).unwrap().as_bytes())
                    .await;
                let _ = w.write_all(b"\n").await;
                let _ = w.flush().await;
                // new
                line.clear();
                let _ = r.read_line(&mut line).await;
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                    recv_clone.lock().unwrap().push(v);
                }
                let new = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "new",
                    "result": { "sessionId": "s" }
                });
                let _ = w
                    .write_all(serde_json::to_string(&new).unwrap().as_bytes())
                    .await;
                let _ = w.write_all(b"\n").await;
                let _ = w.flush().await;
                // prompt
                line.clear();
                let _ = r.read_line(&mut line).await;
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                    recv_clone.lock().unwrap().push(v);
                }
                let prompt = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "prompt",
                    "result": { "stopReason": stop_for_task }
                });
                let _ = w
                    .write_all(serde_json::to_string(&prompt).unwrap().as_bytes())
                    .await;
                let _ = w.write_all(b"\n").await;
                let _ = w.flush().await;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            });
            let (mut r, mut w) = tokio::io::split(client_io);
            let mut r = tokio::io::BufReader::new(&mut r);
            let opts = goose_opts(None);
            drive_goose_io(&opts, &mut r, &mut w, &mut |_| {})
                .await
                .expect("drive_goose_io should succeed")
        }
        let r = run_with_stop_reason("max_tokens".to_string()).await;
        assert_eq!(r.outcome, "max_tokens");
        assert!(!r.succeeded());
        let r = run_with_stop_reason("cancelled".to_string()).await;
        assert_eq!(r.outcome, "cancelled");
        assert!(!r.succeeded());
        let r = run_with_stop_reason("end_turn".to_string()).await;
        assert_eq!(r.outcome, "completed");
        assert!(r.succeeded());
    }
}

#[cfg(test)]
mod goose_acp_integration {
    //! HUMAN-AUTHORED integration oracle (NOT model-authored — that would be the
    //! self-mock trap). Spawns the REAL `goose acp` (>=1.37) server and drives our
    //! exact `initialize` + `session/new` wire frames against it, asserting goose
    //! ACCEPTS them and returns a sessionId. Credential-free: the handshake never
    //! invokes a model (that only happens on `session/prompt`), so no provider key
    //! is required. `#[ignore]` so plain `cargo test` skips it; the loop --check
    //! runs it via `-- --ignored`. This is the deterministic gate the free loop
    //! cannot fake: if the GooseEngine wire shapes drift from real goose, this
    //! fails even when the in-repo mock (and a free reviewer) say green.
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn goose_acp_real_handshake_smoke() {
        let transport = EngineTransport::Stdio {
            command: "goose".to_string(),
            args: vec!["acp".to_string()],
            env: vec![],
        };
        let mut conn = connect_transport(&transport)
            .await
            .expect("spawn real `goose acp` (is goose >=1.37 on PATH?)");
        {
            let mut r = tokio::io::BufReader::new(&mut conn.reader);
            let w = &mut conn.writer;
            // initialize — our exact wire shape.
            write_frame(
                w,
                &serde_json::json!({
                    "jsonrpc": "2.0", "id": "init", "method": "initialize",
                    "params": { "protocolVersion": GOOSE_PROTOCOL_VERSION, "clientCapabilities": {} }
                }),
            )
            .await
            .expect("write initialize");
            let init = tokio::time::timeout(
                std::time::Duration::from_secs(25),
                read_result(&mut r, "init"),
            )
            .await
            .expect("real goose initialize timed out")
            .expect("real goose initialize errored");
            assert!(
                init.get("protocolVersion").is_some(),
                "real goose must return protocolVersion on initialize; got {init}"
            );
            // session/new — our exact standard-ACP params (cwd + mcpServers).
            write_frame(
                w,
                &serde_json::json!({
                    "jsonrpc": "2.0", "id": "new", "method": "session/new",
                    "params": { "cwd": "/tmp", "mcpServers": [] }
                }),
            )
            .await
            .expect("write session/new");
            let new = tokio::time::timeout(
                std::time::Duration::from_secs(25),
                read_result(&mut r, "new"),
            )
            .await
            .expect("real goose session/new timed out")
            .expect("real goose session/new errored");
            let sid = new
                .get("sessionId")
                .or_else(|| new.get("session_id"))
                .and_then(serde_json::Value::as_str);
            assert!(
                sid.map(|s| !s.is_empty()).unwrap_or(false),
                "real goose must accept our session/new shape and return a sessionId; got {new}"
            );
        }
        conn.kill_child().await;
    }
}

#[cfg(test)]
mod goose_acp_real_turn {
    //! Ultimate oracle: a REAL streaming turn through the GooseEngine against the
    //! real `goose acp` server, pointed at the MiniMax OpenAI-compatible endpoint.
    //! Exercises prompt -> session/update streaming -> stopReason -> child reap —
    //! the path the handshake smoke does not cover (and where the alleged
    //! prompt-write-before-read deadlock would manifest if real). Skips if
    //! MINIMAX_API_KEY is absent. #[ignore]; run with `-- --ignored`.
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn goose_acp_real_turn_minimax() {
        let key = match std::env::var("MINIMAX_API_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ => { eprintln!("SKIP: MINIMAX_API_KEY not set"); return; }
        };
        let transport = EngineTransport::Stdio {
            command: "goose".to_string(),
            args: vec!["acp".to_string()],
            env: vec![
                ("GOOSE_PROVIDER".to_string(), "openai".to_string()),
                ("GOOSE_MODEL".to_string(), "MiniMax-M3".to_string()),
                ("OPENAI_API_KEY".to_string(), key.clone()),
                ("OPENAI_HOST".to_string(), "https://api.minimax.io".to_string()),
                ("OPENAI_BASE_URL".to_string(), "https://api.minimax.io/v1".to_string()),
            ],
        };
        let opts = AgentOptions {
            socket: std::path::PathBuf::from("/tmp/unused.sock"),
            agent_alias: "minimax".to_string(),
            cwd: std::path::PathBuf::from("/tmp"),
            prompt: "Reply with exactly the word: pong. Do not call any tools.".to_string(),
            model_override: None,
            model_id: Some("MiniMax-M3".to_string()),
            session_id: None,
            show_reasoning: false,
            approval: ApprovalPolicy::None,
            timeout: std::time::Duration::from_secs(60),
        };
        let mut conn = connect_transport(&transport).await.expect("spawn goose acp");
        let res;
        {
            let mut r = tokio::io::BufReader::new(&mut conn.reader);
            let w = &mut conn.writer;
            res = drive_goose_io(&opts, &mut r, w, &mut |_| {}).await;
        }
        conn.kill_child().await;
        let run = res.expect("drive_goose_io errored against real goose+minimax");
        eprintln!("REAL TURN outcome={} content={:?} tools={}", run.outcome, run.content, run.tool_calls);
        assert_ne!(run.outcome, "failed", "real goose+minimax turn failed outright");
        assert_ne!(run.outcome, "timeout", "real goose+minimax turn TIMED OUT (possible prompt-write-before-read deadlock!)");
    }
}
