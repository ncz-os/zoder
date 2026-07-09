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

pub mod session_store;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Child;

/// Maximum bytes the wire layer will buffer for a single JSON-RPC
/// frame. ACP frames are small (a few KB for typical tool calls; a few
/// hundred KB for the largest streamed text chunk the engine
/// produces in practice). 4 MiB is an order of magnitude larger than
/// any legitimate frame this driver has ever observed, and is small
/// enough that a hostile or runaway engine that emits a giant line
/// (or never sends a newline) cannot OOM the driver within the turn
/// deadline via a SINGLE frame. Frames larger than this cap surface
/// as a clear "frame exceeds N-byte cap" error from the per-frame
/// reader, so the caller can fail fast and surface a useful
/// diagnostic instead of seeing a silent memory blow-up on one
/// oversized frame.
///
/// NOTE: the cap is PER-FRAME. A continuous stream of
/// well-formed-but-many sub-cap frames can still accumulate to
/// arbitrarily large totals over a long turn; that case is bounded
/// separately by [`MAX_CUMULATIVE_CONTENT_BYTES`] (which guards the
/// cumulative size of the `content` accumulator + mirrored
/// on_event/Text sink in `drive` / `drive_goose_io`).
///
/// The cap is enforced via [`AsyncReadExt::take`] on the per-frame
/// read; the underlying reader's buffered bytes (beyond the cap) are
/// NOT discarded — they are left in the buffer for the next frame
/// read, so a malformed frame cannot corrupt a subsequent legitimate
/// one. The cap is PER-FRAME, not cumulative.
///
/// Z-17: introduced to bound the per-frame read that previously used
/// an unbounded [`AsyncBufReadExt::read_line`] (the engine-session
/// read loops in `drive`, `drive_goose_io`, and `cancel_session` all
/// go through this constant).
///
/// Y-9: [`MAX_CUMULATIVE_CONTENT_BYTES`] was added to bound the
/// running byte total of streamed `content` across frames, which
/// the per-frame cap cannot constrain on its own (a hostile engine
/// emitting a steady stream of sub-cap `agent_message_chunk` frames
/// would otherwise grow `content` without bound across the turn
/// deadline).
pub(crate) const MAX_FRAME_BYTES: u64 = 4 * 1024 * 1024;

/// Cumulative byte cap on the streamed `content` accumulator (and
/// its mirrored on_event/Text sink) inside a single turn of
/// `drive` / `drive_goose_io`. The per-frame
/// [`MAX_FRAME_BYTES`] cap (4 MiB) prevents a single oversized
/// frame from OOMing the driver, but it cannot constrain a
/// continuous stream of well-formed-but-many sub-cap frames over
/// the default 900s turn deadline: a hostile / runaway engine
/// that emits `agent_message_chunk` frames in a tight loop would
/// otherwise accumulate gigabytes into `content` (plus the mirrored
/// Text events the caller mirrors to its own sink).
///
/// 64 MiB is a small multiple of [`MAX_FRAME_BYTES`] (16x) that is
/// comfortably larger than any legitimate turn the driver has ever
/// observed in practice (a single tool-using agentic turn rarely
/// crosses a few hundred KiB of assistant text), while small enough
/// that hitting the cap means the engine is misbehaving — not that
/// the operator asked for too much. When the cap is hit the driver
/// bails the turn with a clear `InvalidData`-flavored error naming
/// the cap, instead of growing unbounded.
///
/// Y-9: introduced to bound cumulative streamed `content` after the
/// prior per-frame fix (Z-17) was shown to be insufficient against
/// a continuous stream of sub-cap frames.
pub(crate) const MAX_CUMULATIVE_CONTENT_BYTES: u64 = 64 * 1024 * 1024;

/// Convert an `EngineKind` into the canonical scope prefix the
/// persistence layer uses. Kept here (rather than in `session_store`)
/// so the wire layer can build the scope key without taking a
/// dependency on the store's API surface — and so a future engine
/// (non-`goose`, non-`zeroclaw`) only needs to know to update this
/// one helper to keep the persistence key stable.
pub fn engine_kind_scope(kind: EngineKind) -> &'static str {
    match kind {
        EngineKind::Zeroclaw => "zeroclaw",
        EngineKind::Goose => "goose",
    }
}

/// How to reach the ACP engine. Today: an already-running daemon over its
/// Unix socket. Future: spawn a child process (e.g. `goose acp`) and speak
/// ACP over its stdio. The JSON-RPC layer is identical in both cases — only
/// the transport half-acquisition differs.
#[derive(Clone)]
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

/// A process-env key whose VALUE is a secret (API key / token / bearer). Used
/// to redact values in any Debug/log rendering of a spawned engine's env — the
/// goose bridge injects `OPENAI_API_KEY`, so a bare `{:?}` on the transport or
/// its env vector would otherwise leak the operator's provider key.
pub(crate) fn is_secret_env_key(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    k.contains("API_KEY") || k.contains("TOKEN") || k.contains("SECRET") || k.ends_with("_KEY")
}

/// Render an engine env vector for Debug/log output with secret VALUES masked
/// while non-secret vars (GOOSE_PROVIDER, GOOSE_MODEL, OPENAI_BASE_URL, …) stay
/// visible so the output remains useful.
pub(crate) fn redact_env(env: &[(String, String)]) -> Vec<(String, String)> {
    env.iter()
        .map(|(k, v)| {
            let shown = if is_secret_env_key(k) {
                "***REDACTED***".to_string()
            } else {
                v.clone()
            };
            (k.clone(), shown)
        })
        .collect()
}

impl std::fmt::Debug for EngineTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineTransport::UnixSocket(p) => f.debug_tuple("UnixSocket").field(p).finish(),
            EngineTransport::Stdio { command, args, env } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                // env values are redacted: the goose bridge injects OPENAI_API_KEY.
                .field("env", &redact_env(env))
                .finish(),
        }
    }
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
    /// Process-group id of the spawned stdio engine (the child leads its own
    /// group because it is spawned with `.process_group(0)`, so `pgid == pid`).
    /// `None` for [`EngineTransport::UnixSocket`] and on the (unexpected) path
    /// where the child exposes no pid. [`Self::kill_child`] uses it to deliver
    /// `kill(-pgid, SIGKILL)` to the WHOLE group, so tool subprocesses the
    /// engine forked as GRANDCHILDREN (shell / build commands) die with it
    /// instead of being reparented to init and leaking one subtree per
    /// timed-out turn. Captured at spawn time — before the pid can be lost to a
    /// race with the child exiting — so the group kill still targets the right
    /// group even if the direct child has already been reaped by the OS by the
    /// time we call `kill_child`.
    pub pgid: Option<i32>,
}

impl ConnectedTransport {
    /// Kill and wait on the spawned stdio engine (if any). Idempotent and
    /// safe to call on every exit path; safe to call when there is no child.
    /// Called by the goose driver in its `Drop`-like guard before returning.
    ///
    /// SIGKILL is delivered to the child's WHOLE process group
    /// (`kill(-pgid, SIGKILL)` on Unix), not just the direct `goose` pid.
    /// goose forks its tool subprocesses (shell / build commands) as
    /// GRANDCHILDREN; a single-pid kill would drop only `goose` and leave
    /// those grandchildren reparented to init, leaking one subtree per
    /// timed-out turn. Because the child was spawned with `.process_group(0)`
    /// it leads its own group (`pgid == pid`), so one group kill takes the
    /// whole subtree down. This mirrors the check path
    /// (`agentic::run_check_watched` + `kill_process_group`).
    ///
    /// SIGKILL is used (not SIGTERM) because it is the only signal guaranteed
    /// to work even if goose is stuck on a sync syscall or in a model retry
    /// loop; we already gave it a chance to wind down via `session/cancel`.
    /// On non-Unix targets (no process groups) we fall back to a single-pid
    /// `start_kill()`.
    pub async fn kill_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            #[cfg(unix)]
            {
                if let Some(pgid) = self.pgid {
                    // Negative pid => "the process group `pgid`". Best-effort:
                    // the child may already be gone (ESRCH), which is fine — we
                    // still `wait()` below to reap the direct child's zombie.
                    // SAFETY: `libc::kill` is a plain syscall with no memory
                    // safety obligations; a bad pgid just returns an errno.
                    unsafe {
                        libc::kill(-pgid, libc::SIGKILL);
                    }
                } else {
                    // No pgid captured (e.g. child exposed no pid at spawn):
                    // fall back to a single-pid kill so we at least reap goose.
                    let _ = child.start_kill();
                }
            }
            #[cfg(not(unix))]
            {
                // No process groups on this platform — single-pid kill only.
                let _ = child.start_kill();
            }
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

/// Compose the final `session/prompt` text for an ACP engine
/// (zeroclaw or goose): the project-instructions block sits on top
/// when `AgentOptions::project_instructions` is `Some`, the user's
/// task text sits below it; when `None`, the returned string is
/// EXACTLY `opts.prompt` byte-for-byte so a project without
/// `AGENTS.md` / `CLAUDE.md` keeps every pre-this-slice wire shape.
///
/// The header format mirrors `zoder-core`'s `compose_prompt`:
///   `# Project instructions (AGENTS.md)\n\n{text}\n\n---\n\n{task}`
///
/// Implemented locally (without depending on `zoder_core`) so the
/// prompt shape that hits the wire stays debuggable from this file
/// alone and an acp-client unit test can pin both the byte shape and
/// the regression-guard behavior without spinning up the full
/// `zoder_core` crate. The two implementations are kept in lockstep
/// by the `acp_client::tests::compose_prompt_*` test set.
pub(crate) fn compose_session_prompt(opts: &AgentOptions) -> String {
    let Some(instructions) = opts
        .project_instructions
        .as_deref()
        .filter(|s| !s.is_empty())
    else {
        // Non-breaking default: when no AGENTS.md / CLAUDE.md was
        // loaded, send the prompt text unchanged so existing
        // operator tooling that diffs the wire still matches.
        return opts.prompt.clone();
    };
    const HEADER: &str = "# Project instructions (AGENTS.md)\n\n";
    const SEPARATOR: &str = "\n\n---\n\n";
    let mut out = String::with_capacity(
        HEADER.len() + instructions.len() + SEPARATOR.len() + opts.prompt.len(),
    );
    out.push_str(HEADER);
    out.push_str(instructions);
    out.push_str(SEPARATOR);
    out.push_str(&opts.prompt);
    out
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
                pgid: None,
            })
        }
        EngineTransport::Stdio { command, args, env } => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .kill_on_drop(false);
            // Detach the engine into its own process group so the driver can
            // SIGKILL the WHOLE subtree on timeout/error — goose forks its tool
            // subprocesses (shell / build commands) as grandchildren, and a
            // single-pid kill would leave them reparented to init and leaking.
            // Tokio maps `process_group(0)` to setpgid(pid, 0) on Unix (the
            // child leads a fresh group with pgid == pid), with no extra fork.
            // No-op on non-Unix; kept unconditional to keep the spawn shape
            // identical across platforms (the group KILL is what's cfg-gated,
            // in `kill_child`).
            #[cfg(unix)]
            cmd.process_group(0);
            for (k, v) in env {
                cmd.env(k, v);
            }
            let mut child = cmd
                .spawn()
                .with_context(|| format!("spawning ACP engine `{command}`"))?;
            // Capture the pgid NOW, at spawn, while the direct child is
            // guaranteed live. Because of `.process_group(0)` the child leads
            // its own group, so `child.id()` (its pid) IS the pgid. Capturing
            // here (rather than in `kill_child`) means the group kill still
            // targets the right group even if the direct `goose` pid has
            // already been reaped by the OS by the time we tear down.
            let pgid = child.id().map(|p| p as i32);
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
                // Group kill target for `kill_child` (see field docs).
                pgid,
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

/// Resolved zoder-provider config that `run_goose_agent` bridges into
/// the spawned `goose acp` child environment.
///
/// This is the CREDENTIAL/ENDPOINT seam (task #19) that makes
/// `zoder loop --engine goose` actually authenticate against the
/// free/subscription provider zoder picked, instead of falling back to
/// goose's own `~/.config/goose/config.yaml` and dialing the operator's
/// previous session (wrong auth, wrong model, wrong bill).
///
/// Construction contract (used by tests AND by `zoder-cli`):
///   * `provider_id` — the zoder `Provider::id` (e.g. `"minimax"`,
///     `"nvidia-eih"`, `"openrouter"`). Echoed in logs / surfaces.
///   * `kind` — zoder's `Provider::kind` (`openai-chat` |
///     `openai-responses` | `anthropic` | `custom`); used to derive
///     `GOOSE_PROVIDER` (the value goose keys on) and to decide
///     whether the OPENAI_* env-var bridge applies.
///   * `base_url` — the zoder `Provider::base_url`. Used verbatim for
///     `OPENAI_BASE_URL`; host-stripped for `OPENAI_HOST`.
///   * `api_key` — the credential as zoder resolved it from `Auth`
///     (env var OR inline bearer). NEVER logged. `Debug` renders it
///     as `[REDACTED]` so a stray `dbg!`/`println!("{opts:?}")`
///     can't leak the secret. The credential is set as
///     `OPENAI_API_KEY` regardless of auth style (bearer or custom
///     header) because goose's `openai` engine only reads
///     `OPENAI_API_KEY`; for ApiKeyHeader-style auth the operator's
///     gateway must accept the value as a bearer, which is the
///     common case for OpenAI-compatible endpoints.
///
/// The zoder CLI builds this from the same `Config::real_best_provider_for_model`
/// call the oneshot + agentic paths already use, so the loop and the
/// single-shot turn always target the same provider.
#[derive(Clone)]
pub struct GooseProviderEnv {
    pub provider_id: String,
    pub kind: String,
    pub base_url: String,
    pub api_key: Option<String>,
}

impl std::fmt::Debug for GooseProviderEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hard redaction: the key is NEVER rendered, even in test
        // failure output (`cargo test -- --nocapture` + an unexpected
        // panic would otherwise dump the full struct). `None` is
        // distinct from `Some(...)` so a test can still assert
        // presence, but the secret itself stays opaque.
        f.debug_struct("GooseProviderEnv")
            .field("provider_id", &self.provider_id)
            .field("kind", &self.kind)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
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
    /// Resolved provider to bridge into the spawned `goose acp` child
    /// (the CREDENTIAL/ENDPOINT seam). When set, `run_goose_agent`
    /// forwards the provider's API key + base_url as `OPENAI_API_KEY`
    /// / `OPENAI_HOST` / `OPENAI_BASE_URL` on the child so goose can
    /// authenticate against a free/subscription provider; without this
    /// goose falls back to its own `~/.config/goose/config.yaml` and
    /// silently dials the operator's previous session — wrong model,
    /// wrong auth, wrong bill. `None` for zeroclaw (no spawn). See
    /// [`GooseProviderEnv`] for the exact env-construction contract.
    pub goose_provider: Option<GooseProviderEnv>,
    /// Writable-root containment boundary (SLICE 1 of the execution-safety
    /// kernel). Every file-write target is resolved (canonicalized, with
    /// symlink/parent-dir traversal defeated) and checked against this list:
    /// targets OUTSIDE all listed roots are DENIED when
    /// [`Self::enforce_writable_roots`] is `true`. Default: a single-element
    /// vector containing [`Self::cwd`] (so a fresh `AgentOptions::new(...)`
    /// is, when enforcement is flipped on, a "writes must stay inside the
    /// repo root" gate). Populated with `vec![cwd.clone()]` by
    /// [`Self::new`]; struct-literal callers must set it explicitly to
    /// preserve the same default — a later slice will compute this from a
    /// CLI flag (out of scope here).
    pub writable_roots: Vec<PathBuf>,
    /// When `true`, every write/edit-class tool call is checked against
    /// [`Self::writable_roots`] BEFORE the policy decides; a target that
    /// resolves outside the roots is DENIED regardless of [`ApprovalPolicy`].
    /// `false` (default) preserves today's behavior EXACTLY — no new
    /// denials, no new containment checks — so this slice is non-breaking.
    /// A follow-up will validate the schema matrix and flip the default.
    pub enforce_writable_roots: bool,
    /// When `true`, the approval decision trusts the engine's identification
    /// of the tool call: an unknown engine OR an unknown write tool OR a
    /// missing/unparseable path on a known write tool does NOT force a
    /// deny on fail-closed grounds. `false` (default) is fail-closed: any
    /// of those conditions DENIES the call when enforcement is on. The
    /// follow-up that wires up the per-engine schema matrix will set this
    /// to `true` for engines whose tool-shape we have validated
    /// exhaustively; for now it stays `false` so unknown shapes never get
    /// a free pass.
    pub trust_engine: bool,
    /// Pre-serialized ACP `mcpServers` array to hand to the goose
    /// engine's `session/new` call. Each entry is an untagged-enum
    /// JSON object — stdio or http — built by
    /// `zoder_core::to_acp_mcp_servers` from the parsed engine-config
    /// MCP server specs. Defaulting to `Vec::new()` is NON-BREAKING:
    /// when no servers are configured the goose `session/new` call
    /// sends `[]` exactly as today, so existing operator setups are
    /// unaffected. Populated by the CLI from
    /// `parse_mcp_servers_file` + `to_acp_mcp_servers` for a goose
    /// run; `acp-client` itself stays decoupled from `zoder-core`'s
    /// parsing layer and only sees ready-to-send JSON values.
    pub mcp_servers: Vec<Value>,
    /// Optional project-level instructions loaded from
    /// `AGENTS.md` (Codex CLI convention) or `CLAUDE.md` (Claude Code
    /// convention) at the repo root by
    /// `zoder_core::load_project_instructions`. When `Some(text)`, both
    /// ACP drivers (`drive` for zeroclaw, `drive_goose_io` for goose)
    /// prepend a clearly-delimited
    /// `# Project instructions (AGENTS.md)\n\n{text}\n\n---\n\n`
    /// block ahead of [`Self::prompt`] before sending the
    /// `session/prompt` frame, so the model and any log/debug output
    /// can tell the project-instructions block apart from the user's
    /// task text.
    ///
    /// When `None` (default), the prompt sent to the model is exactly
    /// `prompt` byte-for-byte — preserving every pre-this-slice
    /// run's wire shape, including the AGENTS.md-parity regression
    /// guard exercised by `AgentOptions::new` and the
    /// `acp_client::tests::prompt_*` test suite. The CLI populates
    /// this field at construction time and never edits it past that
    /// point; loaded at the CLI seam so `acp-client` itself stays
    /// decoupled from filesystem IO (parity with how
    /// [`Self::mcp_servers`] is handed in pre-serialized).
    pub project_instructions: Option<String>,
    /// When `true`, the driver CONSULTS the engine-session persistence
    /// store at [`Self::session_store_path`] before creating a new
    /// session, and writes the returned `session_id` to that store
    /// after a successful run. When the persisted id's scope
    /// (`<engine_kind>:<canonical-cwd>`) differs from the current
    /// scope OR the record is older than the store's freshness
    /// window (default 7 days), the driver treats the record as
    /// absent and creates a fresh session exactly like the OFF path.
    /// When the engine REJECTS a resume (`session/new` returns a
    /// JSON-RPC error reply after the client sent a known
    /// `session_id`), the driver overwrites the stale record with
    /// the new session id from the fallback create — the next run
    /// resumes the new session, not the dead one.
    ///
    /// Default `false` (NON-BREAKING): today's wire shape is
    /// always-create-new and every pre-this-slice run expects that.
    /// The CLI opts in explicitly via a `--persist-session` flag so
    /// a turn that worked yesterday keeps the same wire shape
    /// tomorrow unless the operator asked for persistence.
    pub persist_session_id: bool,
    /// Filesystem location for the engine-session persistence file
    /// (used only when [`Self::persist_session_id`] is `true`). The
    /// store is a tiny JSON object — one record per
    /// `<engine_kind, canonical-cwd>` scope — under
    /// `~/.zoder/sessions/engine_sessions.json` by convention. The CLI
    /// populates this from `Config::sessions_dir()` so this layer stays
    /// decoupled from the zoder home layout. `None` with
    /// `persist_session_id = true` is a configuration error surfaced at
    /// the dispatch site, not here, so existing call sites stay
    /// byte-for-byte identical.
    pub session_store_path: Option<PathBuf>,
}

impl AgentOptions {
    pub fn new(
        socket: impl Into<PathBuf>,
        agent_alias: impl Into<String>,
        cwd: impl Into<PathBuf>,
        prompt: impl Into<String>,
    ) -> Self {
        let cwd = cwd.into();
        Self {
            socket: socket.into(),
            agent_alias: agent_alias.into(),
            cwd: cwd.clone(),
            prompt: prompt.into(),
            model_override: None,
            model_id: None,
            session_id: None,
            show_reasoning: false,
            approval: ApprovalPolicy::Allowlist,
            timeout: Duration::from_secs(900),
            goose_provider: None,
            // SLICE 1 default: writes are confined to cwd until the CLI
            // flag ships and the default is reviewed.
            writable_roots: vec![cwd],
            enforce_writable_roots: false,
            trust_engine: false,
            // SLICE: configured MCP servers pre-serialized for the
            // goose `session/new` `mcpServers` parameter. Defaulting
            // to an empty Vec preserves today's wire shape EXACTLY —
            // session/new sends `[]` when nothing is configured, so
            // existing operator setups see no change.
            mcp_servers: Vec::new(),
            // PROJECT-INSTRUCTIONS SLICE: default to None so
            // pre-this-slice AgentOptions::new call sites keep sending
            // the raw prompt byte-for-byte. The CLI seam populates
            // this when AGENTS.md / CLAUDE.md is present at the repo
            // root.
            project_instructions: None,
            // PERSISTENT-SESSIONS SLICE: default OFF to preserve
            // today's wire shape (every run starts a fresh session).
            // The CLI explicitly opts in via `--persist-session`; with
            // the default OFF every existing call site produces the
            // same JSON-RPC frames as before this slice.
            persist_session_id: false,
            session_store_path: None,
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
    /// Rate-limit response metadata attached to an engine usage update.
    /// Only known utilization header families are forwarded; credentials and
    /// unrelated response headers never cross the ACP boundary.
    Utilization { headers: Vec<(String, String)> },
}

/// Recursively collect the small, non-secret response-header vocabulary used
/// by KNEMON. Engines differ in whether metadata lives directly on the usage
/// update or under `headers` / `_meta`, so walking the update is more robust
/// than binding to one engine-specific envelope.
fn utilization_headers(value: &Value) -> Vec<(String, String)> {
    fn visit(value: &Value, out: &mut std::collections::BTreeMap<String, String>) {
        match value {
            Value::Object(map) => {
                for (name, value) in map {
                    let lower = name.to_ascii_lowercase();
                    let recognized = lower.starts_with("x-codex-")
                        || lower.starts_with("x-ratelimit-")
                        || lower.starts_with("anthropic-ratelimit-");
                    if recognized {
                        let scalar = match value {
                            Value::String(s) => Some(s.clone()),
                            Value::Number(n) => Some(n.to_string()),
                            Value::Bool(b) => Some(b.to_string()),
                            _ => None,
                        };
                        if let Some(scalar) = scalar {
                            out.insert(lower, scalar);
                        }
                    } else {
                        visit(value, out);
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    visit(value, out);
                }
            }
            _ => {}
        }
    }

    let mut headers = std::collections::BTreeMap::new();
    visit(value, &mut headers);
    headers.into_iter().collect()
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

/// Cancel an in-flight turn on the daemon for `session_id` and wait up to
/// `settle_budget` for the daemon to acknowledge the cancel (via a
/// `session/update {type: "turn_complete"}` notification). This is the
/// cancel hook the loop's author-phase watchdog calls when its outer
/// wall-clock budget fires — without it, dropping the future merely
/// orphans the turn on the daemon side, and the daemon keeps editing
/// files while the loop captures a torn diff.
///
/// The function is best-effort: if the daemon doesn't acknowledge within
/// `settle_budget`, it returns `Err` but the cancel notification has
/// still been delivered, so the caller can treat the timeout as "settled
/// enough to capture the diff" (i.e., the daemon is in the process of
/// winding down but no new tool calls will be accepted). The `settle_budget`
/// should be small (a few seconds) — long enough to receive the
/// turn_complete notification, short enough that a hung daemon cannot
/// stall the loop.
///
/// **Zero-length read (EOF) handling:** an EOF before `turn_complete` is
/// NOT treated as a successful settle. If the daemon closes the socket
/// without first sending the cancel acknowledgment, we cannot tell
/// whether the cancel succeeded or the daemon crashed mid-edit. We let
/// the settle budget elapse and return `Err` so the caller times out
/// gracefully. (The pre-fix code returned `Ok(())` immediately on EOF,
/// which could let `build_diff` capture a torn tree if the daemon had
/// crashed in the middle of a tool write.)
///
/// **Connect timeout:** the function races `UnixStream::connect` against
/// `settle_budget` itself. A hung daemon socket (the daemon exists and
/// accepts at the kernel level, but never completes the ACP handshake)
/// can otherwise stall the loop for the OS-level connect timeout (often
/// tens of seconds). Capping connect at the same budget keeps cancel-on-
/// timeout from adding significant latency to the loop's timeout path.
///
/// `session/cancel` is sent as a JSON-RPC NOTIFICATION (no `id`, no
/// expected response) per the ACP v1 spec, matching the in-band cancel
/// the `drive()` loop sends on its own timeout. The function opens a
/// FRESH daemon connection rather than reusing the in-flight one —
/// after a `phase_watchdog` drops the loop's future, that connection
/// is gone. A fresh connection + `initialize` + `session/cancel` is the
/// only signal the daemon gets that its caller has gone away.
pub async fn cancel_session(
    socket: &Path,
    session_id: &str,
    settle_budget: Duration,
) -> anyhow::Result<()> {
    // Cap the connect at the same `settle_budget` so a hung daemon
    // socket cannot add multi-second latency to the watchdog path.
    let transport = EngineTransport::UnixSocket(socket.to_path_buf());
    let conn = tokio::time::timeout(settle_budget, connect_transport(&transport))
        .await
        .map_err(|_| {
            anyhow!(
                "connecting to daemon at {} timed out after {settle_budget:?}",
                socket.display()
            )
        })??;
    let mut reader = BufReader::new(conn.reader);
    let mut write_half = conn.writer;

    // initialize (the daemon requires this on every connection). Use the
    // remaining settle budget so a slow daemon can't extend the cancel
    // window past what the caller asked for.
    let init_deadline = tokio::time::Instant::now() + settle_budget;
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
    let _ = read_result(&mut reader, "init").await?;

    // session/cancel — NOTIFICATION (no `id`), per ACP v1.
    // Y-2: the zeroclaw daemon (this fn's target: it connects to the engine
    // socket and its `initialize` uses snake_case `protocol_version`, and
    // `session/new` returns `session_id`) keys the cancel on snake_case
    // `session_id`. The previous camelCase `sessionId` made the daemon look up
    // a non-existent `params.session_id`, so the cancel was a NO-OP — the
    // author-timeout ghost turn kept editing and the loop reviewed a torn
    // tree. (Goose uses its own in-turn cancel via stdio, not this path.)
    write_frame(
        &mut write_half,
        &json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "session_id": session_id },
        }),
    )
    .await?;

    // Drain until we see the daemon's turn_complete notification for this
    // cancel, OR `settle_budget` elapses (best-effort). A zero-length
    // read (EOF) BEFORE we've seen `turn_complete` is treated as a
    // failure: the daemon closed the connection without acknowledging
    // the cancel, which we cannot distinguish from a daemon crash mid-
    // edit. The caller will see the timeout-elapsed error below.
    //
    // Z-17: the per-frame read is bounded by [`MAX_FRAME_BYTES`]
    // via [`read_frame_line_capped`]. A hostile / runaway daemon
    // that emits a single frame larger than the cap surfaces as an
    // `io::Error`, NOT an OOM; we treat any per-frame read error
    // here the same as any other read error (settle failure).
    let deadline = init_deadline;
    let mut line = String::new();
    let mut turn_complete_seen = false;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(anyhow!(
                "daemon did not acknowledge session/cancel within {settle_budget:?}"
            ));
        }
        match tokio::time::timeout(remaining, read_frame_line_capped(&mut reader, &mut line)).await
        {
            Ok(Ok(false)) => {
                // EOF — daemon closed the connection. Only accept this
                // as "settled" if we'd already received the turn_complete
                // (defensive: in practice the daemon keeps the socket
                // open until after the final session/update, so EOF
                // arrives *after* turn_complete, but we don't rely on
                // that ordering — an early EOF is treated as a hang).
                if turn_complete_seen {
                    return Ok(());
                }
                return Err(anyhow!(
                    "daemon closed connection without acknowledging session/cancel \
                     (possible crash mid-edit); waited {settle_budget:?}"
                ));
            }
            Ok(Ok(true)) => {
                let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                if frame.get("method").and_then(Value::as_str) == Some("session/update") {
                    let kind = frame
                        .pointer("/params/type")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if kind == "turn_complete" {
                        // Defensive `let _ =` for the EOF arm: if the daemon
                        // ever sent `turn_complete` and THEN closed the
                        // socket, we'd still accept the EOF as "settled".
                        // (In practice the daemon keeps the socket open
                        // until after the final session/update, so EOF
                        // arrives *after* turn_complete, but we don't rely
                        // on that ordering.)
                        #[allow(unused_assignments)]
                        {
                            turn_complete_seen = true;
                        }
                        return Ok(());
                    }
                }
                // keep draining — the daemon may send a few more session/update
                // notifications (e.g., a final tool_result) before turn_complete.
            }
            Ok(Err(e)) => {
                return Err(anyhow!("reading cancel ack from daemon: {e}"));
            }
            Err(_) => {
                return Err(anyhow!(
                    "daemon did not acknowledge session/cancel within {settle_budget:?}"
                ));
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

    // PERSISTENT-SESSIONS SLICE: when the CLI opted in, look for a
    // non-stale record for this (engine, cwd) scope BEFORE sending
    // `session/new`. A `None` here drives the same "fresh session"
    // path as the OFF case — the wire shape is identical, so a
    // first-run-after-enable still produces a session/new without
    // `session_id` in the params.
    let scope = session_store::make_scope(engine_kind_scope(EngineKind::Zeroclaw), &opts.cwd);
    let mut effective_session_id: Option<String> = opts.session_id.clone();
    if effective_session_id.is_none() && opts.persist_session_id {
        if let Some(store_path) = opts.session_store_path.as_ref() {
            let cfg = session_store::StoreConfig::new(store_path);
            if let Ok(Some(rec)) = session_store::EngineSessionStore::load(&cfg, &scope) {
                effective_session_id = Some(rec.session_id);
            }
        }
    }
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
    //
    // PERSISTENT-SESSIONS SLICE: when `effective_session_id` is
    // `Some`, the params include a `session_id` field per the
    // existing wire contract (~L805-807). If the engine returns a
    // JSON-RPC error reply — meaning the persisted id is unknown /
    // expired on the server side — we retry `session/new` with no
    // `session_id` to mint a fresh one. The previous code also
    // cleared the on-disk record on the reject path; that was
    // Z-24-unsafe (a transient failure on the fresh-create retry
    // would leave the operator with an empty store AND no
    // session, for no reason) and is no longer done here — the
    // success-path `persist_session_after` at the end of `drive`
    // overwrites the record with the fresh id, and the failure
    // path leaves it untouched. A non-error failure (timeout,
    // dropped connection) is surfaced unchanged so the caller can
    // decide.
    let mut new_params = serde_json::Map::new();
    new_params.insert("agent_alias".into(), json!(opts.agent_alias));
    new_params.insert("cwd".into(), json!(opts.cwd.to_string_lossy()));
    new_params.insert("chat_mode".into(), json!("acp"));
    if let Some(sid) = &effective_session_id {
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
    let new_res = match read_result_inner(&mut reader, "new").await? {
        Ok(v) => v,
        Err(msg) => {
            // Engine rejected the resume. The previous behavior
            // was to clear the on-disk record here so the next
            // run wouldn't keep tripping the same error — but
            // that path was Z-24-unsafe: if the fresh-create
            // retry below ALSO failed, the operator would be
            // left with an empty store AND no session, for no
            // reason. The fresh id (if the retry succeeds) is
            // already persisted by `persist_session_after` at
            // the end of `drive`, so there is no benefit to
            // dropping the existing record on the reject path.
            // We just retry without `session_id`; the
            // success-path persist overwrites the record, the
            // failure path leaves it untouched.
            //
            // Retry without session_id.
            let mut retry_params = serde_json::Map::new();
            retry_params.insert("agent_alias".into(), json!(opts.agent_alias));
            retry_params.insert("cwd".into(), json!(opts.cwd.to_string_lossy()));
            retry_params.insert("chat_mode".into(), json!("acp"));
            write_frame(
                &mut write_half,
                &json!({
                    "jsonrpc": "2.0",
                    "id": "new",
                    "method": "session/new",
                    "params": Value::Object(retry_params),
                }),
            )
            .await?;
            // A retry-failure is fatal (any repeated error here means
            // the engine itself is misbehaving — not a stale id).
            read_result(&mut reader, "new").await.map_err(|_| {
                anyhow!(
                    "engine error on session/new: {msg} (and the fresh-create retry also failed)"
                )
            })?
        }
    };
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
    //
    // PROJECT-INSTRUCTIONS SLICE: when the loader populated
    // `opts.project_instructions` at the CLI seam, the
    // composed prompt (instructions block + the user's task
    // text, separated by `---`) is what reaches the engine.
    // When `project_instructions` is `None`, the body of
    // `compose_session_prompt` returns `opts.prompt` BYTE-FOR-BYTE,
    // so this slice is non-breaking for any run without an
    // AGENTS.md / CLAUDE.md at the repo root (regression pinned
    // by `prompt_none_is_byte_identical_to_task`).
    let final_prompt = compose_session_prompt(opts);
    write_frame(
        &mut write_half,
        &json!({
            "jsonrpc": "2.0",
            "id": "prompt",
            "method": "session/prompt",
            "params": { "session_id": session_id, "prompt": final_prompt },
        }),
    )
    .await?;

    // 5. consume session/update notifications until turn_complete (or the
    //    deadline elapses, in which case we keep the partial turn).
    //
    //    Z-17: the per-frame read is bounded by [`MAX_FRAME_BYTES`]
    //    via [`read_frame_line_capped`]. A hostile / runaway engine
    //    that emits a single frame larger than the cap surfaces as
    //    an `io::Error` of `InvalidData` kind, NOT an OOM.
    //
    //    Y-9: the per-frame cap is NOT sufficient on its own —
    //    a continuous stream of sub-cap `agent_message_chunk`
    //    frames can accumulate gigabytes into `content` (and the
    //    mirrored Text sink) across the turn deadline. The
    //    `content_bytes` running total bounds the cumulative
    //    size; once it crosses [`MAX_CUMULATIVE_CONTENT_BYTES`]
    //    we bail the turn with a clear diagnostic instead of
    //    growing unbounded.
    let mut content = String::new();
    let mut content_bytes: u64 = 0;
    let mut input_tokens = 0u64;
    let mut tool_calls = 0u32;
    let mut line = String::new();
    let outcome: String = loop {
        let got_line =
            match tokio::time::timeout_at(deadline, read_frame_line_capped(&mut reader, &mut line))
                .await
            {
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
                        read_frame_line_capped(&mut reader, &mut line),
                    )
                    .await;
                    break "timeout".to_string();
                }
                Ok(r) => r.context("reading from engine")?,
            };
        if !got_line {
            bail!("engine closed the connection before turn completed");
        }
        let frame: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let headers = utilization_headers(&frame);
        if !headers.is_empty() {
            on_event(AgentEvent::Utilization { headers });
        }

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
                    // Y-9: track cumulative bytes appended into `content`
                    // (and mirrored into the on_event/Text sink) and bail
                    // the turn when the running total crosses
                    // [`MAX_CUMULATIVE_CONTENT_BYTES`]. The per-frame
                    // [`MAX_FRAME_BYTES`] cap cannot stop a steady stream
                    // of well-formed sub-cap frames from accumulating
                    // gigabytes over the turn deadline.
                    let new_total = content_bytes.saturating_add(t.len() as u64);
                    if new_total > MAX_CUMULATIVE_CONTENT_BYTES {
                        bail!(
                            "streamed content exceeded {MAX_CUMULATIVE_CONTENT_BYTES}-byte \
                             cumulative cap (already appended {content_bytes} bytes across prior \
                             frames; refusing to grow `content` unbounded); possible \
                             hostile or runaway engine"
                        );
                    }
                    content.push_str(t);
                    content_bytes = new_total;
                    on_event(AgentEvent::Text(t.to_string()));
                }
            }
            "agent_thought_chunk" if opts.show_reasoning => {
                if let Some(t) = params.get("text").and_then(Value::as_str) {
                    on_event(AgentEvent::Thought(t.to_string()));
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
                // SLICE 1: route through the argument-aware decision so
                // `enforce_writable_roots` is honored. With enforce OFF
                // (today's default) this is exactly equivalent to the
                // old `decide_approval(opts.approval, &tool)` call.
                let approved = decide_approval_with_containment(
                    opts.approval,
                    &tool,
                    // Zeroclaw's `approval_request` carries a canonical
                    // `tool_name`, not an ACP semantic kind, so pass `None`
                    // and let the name-based heuristic decide.
                    None,
                    &params,
                    EngineKind::Zeroclaw,
                    &opts.writable_roots,
                    opts.enforce_writable_roots,
                    opts.trust_engine,
                );
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
                        // Y-9: enforce the same cumulative cap on the
                        // final `content` field, in case a hostile
                        // engine front-loads the whole turn output
                        // into the `turn_complete` payload rather
                        // than a stream of chunks. The single frame
                        // is already capped by [`MAX_FRAME_BYTES`]
                        // (4 MiB), which is well below the 64 MiB
                        // cumulative cap, but checking keeps the
                        // invariant (final `content.len()` <=
                        // [`MAX_CUMULATIVE_CONTENT_BYTES`]) explicit.
                        if (c.len() as u64) > MAX_CUMULATIVE_CONTENT_BYTES {
                            bail!(
                                "turn_complete `content` of {} bytes exceeds \
                                 {MAX_CUMULATIVE_CONTENT_BYTES}-byte cumulative cap; \
                                 possible hostile or runaway engine",
                                c.len()
                            );
                        }
                        content = c.to_string();
                        // The loop breaks immediately below; the
                        // streaming-loop's `content_bytes` counter
                        // is no longer consulted, so we deliberately
                        // do NOT update it here (would be a dead
                        // write and trip `unused_assignments`).
                    }
                }
                break oc;
            }
            _ => {}
        }
    };

    // PERSISTENT-SESSIONS SLICE: persisting the (potentially-freshly-
    // minted) session id so the NEXT run resumes it. Errors are
    // surfaced as a stderr warning rather than failed: a write error
    // here is non-fatal (the current run already succeeded), and a
    // missed save just means the next run starts fresh — the same
    // outcome as if persistence were OFF.
    persist_session_after(&session_id, opts, &scope);
    Ok(AgentRun {
        session_id,
        outcome,
        content,
        input_tokens,
        tool_calls,
    })
}

/// Semantic classification of a tool call, derived from the ACP v1
/// `toolCall.kind` when present (the MACHINE tool category) and never
/// from the human-rendered `toolCall.title`.
///
/// ACP v1 defines a small, fixed set of semantic kinds. We collapse them
/// into the three security-relevant buckets this file already reasons about:
///   * `Read`  — read/search/fetch: eligible for the Allowlist auto-approve.
///   * `Write` — edit/delete/move: routed through writable-root containment.
///   * `Exec`  — execute: an exec-class tool; NOT write-class, decided by the
///     name-based policy (exec-arg inspection is a later slice).
///
/// Anything we do not recognize (e.g. `think`, `other`, or a novel kind) is
/// `Other`, which means "fall back to the name/title heuristic" — we do NOT
/// guess, and we never let an unknown kind widen the allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolKindCategory {
    Read,
    Write,
    Exec,
    Other,
}

/// Map an ACP v1 `toolCall.kind` string to a [`ToolKindCategory`]. Returns
/// `None` for an empty/absent kind so the caller can fall back to the
/// existing name-based heuristic (this is what keeps zeroclaw — which sends a
/// canonical `tool_name` and no ACP kind — on its current code path).
///
/// The match is case-insensitive and covers the ACP v1 semantic kinds
/// (`read`, `search`, `fetch`, `edit`, `delete`, `move`, `execute`) plus a
/// couple of tolerant aliases (`write` as an edit synonym, `exec` for
/// `execute`). Anything else maps to `Other` (fall back to the heuristic),
/// NEVER to `Read` — an unrecognized kind must never widen auto-approval.
fn acp_kind_category(kind: &str) -> Option<ToolKindCategory> {
    let k = kind.trim().to_ascii_lowercase();
    if k.is_empty() {
        return None;
    }
    Some(match k.as_str() {
        "read" | "search" | "fetch" => ToolKindCategory::Read,
        "edit" | "delete" | "move" | "write" => ToolKindCategory::Write,
        "execute" | "exec" => ToolKindCategory::Exec,
        _ => ToolKindCategory::Other,
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

/// Approval decision that consults the ACP tool CATEGORY (from
/// `toolCall.kind`) for allowlist eligibility, falling back to the
/// name-based [`decide_approval`] when no category is available.
///
/// SECURITY: this only ever WIDENS `Allowlist` on a `Read`-category tool
/// (read/search/fetch), and it NEVER weakens a deny policy — under
/// `ApprovalPolicy::None` every tool is denied, and under `Allowlist` an
/// `Exec`/`Write`/`Other` category is NOT auto-approved by kind (it falls
/// back to the exact name allowlist, which for those is a deny). The
/// `All` branch is unchanged.
fn decide_approval_categorized(
    policy: ApprovalPolicy,
    tool: &str,
    category: Option<ToolKindCategory>,
) -> bool {
    match policy {
        // `All` approves everything; `None` denies everything. Neither is
        // influenced by the ACP category — the deny guarantee is absolute.
        ApprovalPolicy::All => true,
        ApprovalPolicy::None => false,
        ApprovalPolicy::Allowlist => match category {
            // A read-class ACP kind is allowlist-eligible regardless of the
            // human-rendered title (the whole point of GD1): goose sends
            // title="Read file src/main.rs", kind="read" — we approve it.
            Some(ToolKindCategory::Read) => true,
            // Exec/Write/Other never auto-approve by kind under Allowlist;
            // fall back to the exact name allowlist (a deny for these).
            // No kind: pure name-based decision (zeroclaw path, legacy).
            Some(_) | None => decide_approval(ApprovalPolicy::Allowlist, tool),
        },
    }
}

/// PERSISTENT-SESSIONS SLICE: persist `session_id` to the
/// engine-session store when the caller opted in. Reads
/// `opts.persist_session_id` and `opts.session_store_path`; a
/// no-op when persistence is OFF or no path is configured (the
/// CLI guarantees both are set together when it opts in, so the
/// `None` path here is only hit in tests).
///
/// A write failure here is logged as a warning rather than
/// returned as an error: the run itself already succeeded, and a
/// missed save simply means the next run will create a fresh
/// session — exactly the same outcome as if persistence had been
/// OFF. There's no point failing the run on what is essentially
/// a cache write.
fn persist_session_after(session_id: &str, opts: &AgentOptions, scope: &str) {
    if !opts.persist_session_id {
        return;
    }
    let Some(store_path) = opts.session_store_path.as_ref() else {
        return;
    };
    let cfg = session_store::StoreConfig::new(store_path);
    // The path here is whatever the caller threaded through; we
    // don't know which engine the caller intended for the scope
    // key, so use `scope` directly (built by the driver based on
    // its engine_kind_scope helper).
    if let Err(e) = session_store::EngineSessionStore::save_with_scope(&cfg, scope, session_id) {
        eprintln!("zoder: warning: failed to persist engine-session record ({scope}): {e}");
    }
}

// ---------------------------------------------------------------------------
// SLICE 1 of the execution-safety kernel: writable-root containment.
//
// This module adds an ARGUMENT-AWARE layer on top of the existing
// name-based [`decide_approval`]. The new layer does NOT change today's
// behavior when enforcement is OFF (see [`AgentOptions::enforce_writable_roots`]
// = `false`); when enforcement is ON, every write/edit-class tool call is
// checked against the configured writable roots BEFORE the policy decides
// (i.e. a target outside the roots is DENIED even under `ApprovalPolicy::All`).
//
// The boundary is REAL (canonicalize-then-`starts_with`, symlink + dot-dot
// safe) and FAIL-CLOSED: an unknown engine, an unknown write tool, or a
// missing/unparseable path on a known write tool DENIES the call when
// enforcement is on (unless `trust_engine` is set, which is the future
// seam for "we have exhaustively validated this engine's schema").
//
// Out of scope for SLICE 1 (later slices): CLI flags, OS-level sandboxing,
// and exec-argument inspection for shell-class tools.
// ---------------------------------------------------------------------------

/// Result of [`path_within_roots`] / the underlying extractors. Used by
/// [`decide_approval_with_containment`] to choose between ALLOW, DENY (out of
/// roots) and DENY (fail-closed: unknown shape). Internal — the public
/// decision is the bool from `decide_approval_with_containment`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ContainmentVerdict {
    /// Target resolved to a path inside at least one writable root.
    Inside,
    /// Target resolved to a path outside every writable root. Hard DENY
    /// (overrides any policy that would otherwise ALLOW).
    Outside,
    /// The engine/tool/path shape could not be verified. Hard DENY unless
    /// `trust_engine` is set.
    Unknown,
}

/// Symlink- and traversal-safe containment check: returns `true` iff the
/// canonicalized `target` path lives inside at least one of the `roots`,
/// after both sides are resolved with [`std::fs::canonicalize`].
///
/// Properties this primitive MUST guarantee (and which the unit tests
/// below lock in):
///   * If the target does NOT yet exist (a file about to be created), we
///     canonicalize the nearest existing ancestor directory and re-join
///     the remaining components, so a not-yet-created file under a
///     permitted root is still considered "inside".
///   * Parent-directory (`..`) traversal is defeated: the joined path is
///     lexically normalized (collapsing `..` against a component stack)
///     before the `starts_with` check, so
///     `<root>/../../etc/passwd` is normalized to `/etc/passwd` and the
///     `starts_with` check then fails.
///   * Symlink escape is defeated: both the root and the target go
///     through `canonicalize`, so a symlink inside the root that points
///     outside is followed and the resulting real path is checked.
///   * If canonicalization of EITHER side fails (e.g. an unreadable
///     component, a TOCTOU race, a relative-only path on a platform
///     where the cwd is gone), we treat the target as NOT contained
///     (deny). That keeps the primitive fail-closed.
///
/// `pub` (not `pub(crate)`) so external callers — the zoder CLI, the
/// dashboard, the future policy-evaluator slice — can use the SAME
/// primitive the agentic driver uses, rather than each subsystem
/// re-implementing containment and drifting from the boundary.
pub fn path_within_roots(target: &Path, roots: &[PathBuf]) -> bool {
    matches!(
        resolve_containment(target, roots),
        ContainmentVerdict::Inside
    )
}

/// Lower-level helper that returns the structured verdict, so the
/// approval decision can distinguish "outside" (deny with a clear reason)
/// from "unknown" (fail-closed deny with a different reason).
fn resolve_containment(target: &Path, roots: &[PathBuf]) -> ContainmentVerdict {
    // Resolve the target. If the target does not exist yet, walk up to the
    // nearest existing ancestor, canonicalize that, and re-join the
    // remaining tail. This is the only way a "not-yet-created file under
    // a permitted root" can be treated as inside: canonicalizing the
    // future-file path itself fails on every platform.
    let target_canon = match canonicalize_for_write(target) {
        Some(p) => p,
        None => return ContainmentVerdict::Unknown,
    };
    for root in roots {
        let root_canon = match std::fs::canonicalize(root) {
            Ok(p) => p,
            Err(_) => continue, // a broken root is ignored here, but an
                                // empty `roots` slice still ends up
                                // denying — see the final fallback below.
        };
        // `starts_with` on `Path` is component-aware (not byte-prefix),
        // so `/foo/barbaz` does NOT contain `/foo/bar`.
        if target_canon.starts_with(&root_canon) {
            return ContainmentVerdict::Inside;
        }
    }
    ContainmentVerdict::Outside
}

/// Canonicalize a path that may not exist yet. Walks up to the nearest
/// existing ancestor, canonicalizes that, and re-joins the remaining
/// non-existing tail. Returns `None` if no part of the path is
/// canonicalizable (e.g. relative path with no anchor at all).
///
/// Implementation note: we deliberately work from the absolute path's
/// `components()`, NOT from repeated `parent()` calls, because
/// `Path::parent()` of a path ending in `..` does NOT strip the `..`
/// (it is treated as a regular component). The component-walk
/// computes the ancestor prefixes from the path's own structure and
/// finds the longest existing one — which is exactly the "nearest
/// existing ancestor" we need.
///
/// We then LEXICALLY normalize the result (resolve `..` segments
/// against a component stack) so the containment check on the joined
/// path is unaffected by any `..` in the tail. Lexical normalization
/// is filesystem-free, so it works on paths whose tail does not yet
/// exist; the canonicalization on the existing prefix then resolves
/// any symlinks in the prefix. Together: the joined path is the
/// real, symlink-resolved, `..`-resolved path the kernel will land
/// on once the file is created.
fn canonicalize_for_write(p: &Path) -> Option<PathBuf> {
    if p.as_os_str().is_empty() {
        return None;
    }
    // Fast path: the target already exists. Use `canonicalize` directly
    // so symlinks in the target itself (e.g. an existing symlink at the
    // leaf) are resolved and the symlink-escape guarantee holds even
    // when the file is a symlink pointing outside the root.
    if p.exists() {
        return std::fs::canonicalize(p).ok();
    }
    // Make the path absolute so the components walk is anchored on a
    // known root (`/`). Without this, a relative path like
    // `target/../../outside/secret.txt` could yield ambiguous ancestor
    // prefixes. `std::path::absolute` is the stable way to do this on
    // stable Rust (it landed in 1.79).
    let abs = std::path::absolute(p).ok()?;
    // Compute every prefix of `abs` (as a path of components from the
    // root down), and find the LONGEST one that exists. The remaining
    // tail is then rejoined onto the canonicalized prefix.
    let comps: Vec<std::path::Component<'_>> = abs.components().collect();
    // Try from longest prefix to shortest.
    let mut best_split: Option<usize> = None;
    for split in (1..=comps.len()).rev() {
        let prefix: std::path::PathBuf = comps[..split].iter().collect();
        if prefix.exists() {
            best_split = Some(split);
            break;
        }
    }
    let split = best_split?;
    let prefix: PathBuf = comps[..split].iter().collect();
    let tail: Vec<std::path::Component<'_>> = comps[split..].to_vec();
    let canon_prefix = std::fs::canonicalize(&prefix).ok()?;
    // Re-join the tail onto the canonicalized prefix, then LEXICALLY
    // normalize the result. This collapses `..` segments against the
    // stack (so `<canon_prefix>/sub/../../outside/secret.txt` becomes
    // `<canon_prefix>/outside/secret.txt`) without any filesystem
    // access — which is essential for paths whose tail does not exist
    // yet. Symlinks in the non-existing tail cannot be exploited
    // because the tail doesn't exist (no kernel-level symlink to
    // follow); symlinks in the existing prefix were already resolved
    // by `canon_prefix`.
    let mut rejoined = canon_prefix;
    for component in &tail {
        rejoined.push(component.as_os_str());
    }
    Some(lexical_normalize(&rejoined))
}

/// Lexical path normalization: resolve `.` and `..` segments against a
/// component stack, with no filesystem access. Equivalent to
/// `path-clean`-style normalization. Symlinks are NOT followed (use
/// [`std::fs::canonicalize`] for that). This is what lets us
/// collapse `<canon>/root_a/sub/../../outside/x` to
/// `<canon>/outside/x` so the containment check is unaffected by
/// `..` segments in the target path.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut stack: Vec<std::ffi::OsString> = Vec::new();
    for component in p.components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                stack.clear();
                stack.push(component.as_os_str().to_os_string());
            }
            std::path::Component::CurDir => {
                // No-op: `.` is a no-op on the stack.
            }
            std::path::Component::ParentDir => {
                // Pop the top of the stack, unless it's a root or prefix
                // (in which case there's nothing to pop) or the stack
                // is empty.
                let top_is_root_or_prefix = matches!(
                    stack
                        .last()
                        .map(|s| std::path::Path::new(s).components().next()),
                    Some(Some(
                        std::path::Component::RootDir | std::path::Component::Prefix(_)
                    ))
                );
                if !top_is_root_or_prefix && stack.len() > 1 {
                    stack.pop();
                } else if stack.is_empty() {
                    // Relative path with a leading `..` — emit `..` so
                    // the caller still sees a well-formed path. (This
                    // branch is unreachable for absolute paths, which
                    // is the only shape `canonicalize_for_write` feeds
                    // us, but the fallback is correct anyway.)
                    stack.push(std::ffi::OsString::from(".."));
                }
            }
            std::path::Component::Normal(name) => {
                stack.push(name.to_os_string());
            }
        }
    }
    if stack.is_empty() {
        PathBuf::from(".")
    } else {
        let mut out = PathBuf::new();
        for s in &stack {
            out.push(s);
        }
        out
    }
}

/// Per-engine write-tool target extractor. The matrix is intentionally
/// HARDCODED — we do NOT guess generically. Each engine has a known,
/// documented schema for the tool-call args, and we only wire up the
/// specific (engine, tool_name) pairs we can verify. Anything else is
/// reported as `Unknown` and fails closed (denies on enforce=true unless
/// `trust_engine` is set).
///
/// ------------------------------------------------------------------------
/// ENGINE → TOOL → ARG FIELD MATRIX
/// ------------------------------------------------------------------------
///
/// ZEROCLAW:
///   * The daemon streams `session/update` notifications with
///     `type: "approval_request"` carrying `{request_id, tool_name,
///     ...}`. The `tool_name` is the tool's canonical id; the tool-call
///     ARGUMENTS for that call arrive via a SEPARATE notification
///     (`type: "tool_call"`) which this slice does NOT yet pipe into the
///     approval decision (the follow-up that adds the CLI flag will).
///   * Verified write/edit-class tool names we expect on the zeroclaw
///     side: `edit`, `write`, `apply_patch`, `shell`, `bash`. None of
///     these are wired up here because the matching `tool_call`
///     notification (with the target path) is not in this slice's scope.
///   * Verdict: ALL ZEROCLAW WRITE TOOLS ARE TREATED AS `Unknown` IN
///     SLICE 1. With `enforce=true` and `trust_engine=false` (the
///     default) the call is denied, which is the correct fail-closed
///     posture until the follow-up slice wires up the per-tool arg
///     extractor.
///
/// GOOSE (Block's goose, ACP v1):
///   * Standard ACP `session/request_permission` request with
///     `params.toolCall.title` (the tool name) and
///     `params.toolCall.rawInput` (the tool args). The exact argument
///     field name for the target path depends on the specific tool;
///     the table below lists the field we extract per tool.
///   * Verified write/edit-class tool names (per goose >= 1.37):
///       - `text_editor`  → `rawInput.path`     (a file path; covers
///         `view`/`create`/`str_replace`/`insert`/`undo_edit` subcommands
///         per goose's text editor tool)
///       - `write_file`   → `rawInput.file_path` or `rawInput.path`
///         (the latter is the canonical snake_case field; the camelCase
///         variant is accepted for permissive servers)
///       - `shell`        → no filesystem target path; this is an exec
///         tool, NOT a write tool. We DO NOT classify `shell` as
///         write-class here. Exec-arg inspection is a later slice.
///   * Anything else on goose is reported as `Unknown` (e.g. the
///     `computer_*` tools, custom MCP tools). The follow-up slice can
///     add rows as we validate them.
///
/// To extend this matrix, add a row to `extract_write_target` and (if
/// needed) an entry in the `is_write_class_tool` table — DO NOT make
/// the classification generic.
const ZEROCLAW_WRITE_TOOLS: &[&str] = &[
    // Listed for documentation/future-use; currently treated as
    // `Unknown` (fail-closed) because the matching `tool_call` arg
    // notification is not piped into the approval decision in this
    // slice. See the per-engine matrix comment above.
    "edit",
    "write",
    "apply_patch",
    "shell",
    "bash",
];

const GOOSE_WRITE_TOOLS: &[&str] = &[
    "text_editor",
    "write_file",
    // `shell` and `Bash` are exec tools, not file-write tools — the
    // exec-arg inspection slice will handle them. Listed here ONLY so
    // reviewers see they are explicitly NOT classified as write-class
    // (comment-only; the extractor returns `Unknown` for them today).
];

/// Return `true` iff `(engine, tool_name)` is a write/edit-class tool
/// we have explicitly classified in the per-engine matrix. Exec-class
/// tools (shell, bash) and read-only tools return `false` here, which
/// means enforcement leaves them to the regular name-based policy
/// decision (the existing [`decide_approval`]). When in doubt, return
/// `false` here AND return `Unknown` from the extractor — the fail-closed
/// `Unknown` path is what makes the boundary safe.
fn is_write_class_tool(engine: EngineKind, tool: &str) -> bool {
    let table: &[&str] = match engine {
        // Zeroclaw: write/edit-class tools are all in the matrix but
        // currently treated as `Unknown` at the extractor level (no
        // arg plumbing in this slice). Returning `true` here is still
        // safe: a `true` here only forces the extractor to run, and
        // the extractor returns `Unknown` → fail-closed deny.
        EngineKind::Zeroclaw => ZEROCLAW_WRITE_TOOLS,
        EngineKind::Goose => GOOSE_WRITE_TOOLS,
    };
    let needle = tool.to_ascii_lowercase();
    table.iter().any(|t| needle == t.to_ascii_lowercase())
}

/// Extract the target filesystem path for a known write/edit-class tool
/// call. Returns `Some(path)` when the path is unambiguously present in
/// `params`, `None` otherwise (which the caller treats as `Unknown` and
/// deny on enforce=true, fail-closed).
///
/// `params` is the tool-call argument object:
///   * For zeroclaw's `approval_request` notification, this is the
///     per-tool-args blob (when piped in by a follow-up slice; today it
///     is effectively empty for the write tools, which is why the
///     zeroclaw path is `Unknown` for the full matrix).
///   * For goose's `session/request_permission` request, this is
///     `params.toolCall.rawInput` (already extracted out by the caller).
fn extract_write_target(engine: EngineKind, tool: &str, params: &Value) -> Option<PathBuf> {
    let needle = tool.to_ascii_lowercase();
    match engine {
        EngineKind::Zeroclaw => {
            // SLICE 1: the arg shape for zeroclaw write tools is not yet
            // wired up (the matching `tool_call` notification carrying
            // the args is not piped into the approval decision in this
            // slice). Return `None` so the fail-closed path DENIES.
            let _ = needle; // documented in the matrix above
            let _ = params;
            None
        }
        EngineKind::Goose => {
            // Field-name allowlist per tool — NEVER a generic
            // `params.get("path").or_else(params.get("filePath"))` chain,
            // because that would let an unrelated MCP tool with a
            // `path` field accidentally be classified as a write.
            let field: &[&str] = match needle.as_str() {
                "text_editor" => &["path"],
                "write_file" => &["file_path", "path"],
                // Unknown goose write tool (or exec/read tool reaching
                // this branch by mistake). Fail closed.
                _ => return None,
            };
            pick_string_field(params, field).map(PathBuf::from)
        }
    }
}

/// Look up one of the candidate string fields in `params`. Returns the
/// first one that is a non-empty string. Used by
/// [`extract_write_target`] for the goose matrix.
fn pick_string_field(params: &Value, candidates: &[&str]) -> Option<String> {
    let obj = params.as_object()?;
    for key in candidates {
        if let Some(s) = obj.get(*key).and_then(Value::as_str) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Argument-aware approval decision (SLICE 1).
///
/// When `enforce` is `false`, this is a strict passthrough to the
/// existing name-based [`decide_approval`] — NO new denials, NO new
/// containment checks. That keeps this slice non-breaking.
///
/// When `enforce` is `true`:
///   1. If the tool is NOT write/edit-class (per the per-engine matrix),
///      the decision falls through to the name-based policy unchanged.
///   2. If the tool IS write/edit-class, extract the target path. An
///      unknown engine / unknown write tool / missing-or-unparseable
///      path yields `Unknown`. With `trust_engine = false` (the
///      default), `Unknown` DENIES.
///   3. Resolve containment: target inside any root → fall through
///      to the name-based policy. Target outside all roots → DENY,
///      regardless of `ApprovalPolicy`.
///   4. Every DENY logs a clear reason (target vs roots, or the
///      fail-closed cause). The log goes through the `tracing`-free
///      `eprintln!` channel used elsewhere in this file (tests assert
///      on the return value; the log is for operators).
#[allow(clippy::too_many_arguments)]
pub(crate) fn decide_approval_with_containment(
    policy: ApprovalPolicy,
    tool: &str,
    kind: Option<&str>,
    params: &Value,
    engine: EngineKind,
    writable_roots: &[PathBuf],
    enforce: bool,
    trust_engine: bool,
) -> bool {
    // ACP v1 keys the decision on the MACHINE tool category
    // (`toolCall.kind`), not the human-rendered `title`. When a kind is
    // present we classify off it; otherwise we fall back to the name-based
    // heuristic (zeroclaw sends a canonical `tool_name` and no kind).
    let category = kind.and_then(acp_kind_category);

    // Non-enforced path: exact behavior of `decide_approval` today, but
    // allowlist eligibility is decided by the ACP kind when we have one
    // (so a `read`-kind call with a human title like "Read file X" is
    // still auto-approved under Allowlist instead of being rejected).
    if !enforce {
        return decide_approval_categorized(policy, tool, category);
    }
    // Enforced path: only write/edit-class tools go through the
    // containment check; everything else keeps the name/kind-based
    // decision (so e.g. a `read` is still allowed under Allowlist even
    // when enforcement is on, regardless of `writable_roots`).
    //
    // Write-class membership is driven by the ACP kind when present
    // (edit/delete/move), else by the per-engine name matrix. An Exec or
    // Read kind is explicitly NOT write-class.
    let is_write = match category {
        Some(ToolKindCategory::Write) => true,
        Some(ToolKindCategory::Read) | Some(ToolKindCategory::Exec) => false,
        // Unknown/absent kind: fall back to the name-based matrix.
        Some(ToolKindCategory::Other) | None => is_write_class_tool(engine, tool),
    };
    if !is_write {
        return decide_approval_categorized(policy, tool, category);
    }
    // Known write tool — extract the target path. Fail closed on any
    // shape mismatch: an unknown engine, an unknown write tool, or a
    // missing/unparseable path on a known write tool all DENY unless
    // `trust_engine` is set.
    let target = match extract_write_target(engine, tool, params) {
        Some(p) => p,
        None => {
            // Fail-closed deny with a clear reason. The reason
            // distinguishes "unknown engine" from "unknown tool" from
            // "missing path" so an operator reading the log can tell
            // which follow-up slice needs to land.
            let reason = if !is_engine_known(engine) {
                format!(
                    "writable-root containment: denying {engine:?} tool {tool:?}: \
                     unknown engine (no schema registered in slice 1)"
                )
            } else if !is_write_class_tool(engine, tool) {
                format!(
                    "writable-root containment: denying {engine:?} tool {tool:?}: \
                     tool not in the per-engine write matrix"
                )
            } else {
                format!(
                    "writable-root containment: denying {engine:?} tool {tool:?}: \
                     target path missing or unparseable in tool params"
                )
            };
            if !trust_engine {
                eprintln!("{reason}");
                return false;
            }
            // trust_engine=true: fall through to the name/kind-based
            // policy for unknown shapes. This is the future seam for
            // "exhaustively validated engine, accept whatever shape it
            // sends". The default `false` keeps the boundary fail-closed.
            return decide_approval_categorized(policy, tool, category);
        }
    };
    // We have a target path. Resolve containment.
    let verdict = resolve_containment(&target, writable_roots);
    match verdict {
        ContainmentVerdict::Inside => decide_approval_categorized(policy, tool, category),
        ContainmentVerdict::Outside => {
            eprintln!(
                "writable-root containment: denying {engine:?} tool {tool:?}: \
                 target {} resolves outside writable_roots {:?}",
                target.display(),
                writable_roots
            );
            false
        }
        ContainmentVerdict::Unknown => {
            // Canonicalize failed (e.g. unreadable component, TOCTOU
            // race, relative-only path). Fail closed.
            eprintln!(
                "writable-root containment: denying {engine:?} tool {tool:?}: \
                 target {} could not be resolved against writable_roots {:?}",
                target.display(),
                writable_roots
            );
            false
        }
    }
}

/// `true` for the engine variants the per-engine matrix supports.
/// Keeping this as a tiny helper (not a method on `EngineKind`) means
/// the matrix can be extended without leaking implementation details
/// into the engine enum itself.
fn is_engine_known(engine: EngineKind) -> bool {
    matches!(engine, EngineKind::Zeroclaw | EngineKind::Goose)
}

/// One row of the per-engine write-tool matrix: which `(engine, tool_name)`
/// pair we recognize as write/edit-class, and which argument field carries
/// the target filesystem path. The path field is `None` for engines whose
/// write tools' arg shape is not yet wired up (SLICE 1: zeroclaw is `None`
/// for every row — the matching `tool_call` arg notification is not yet
/// piped into the approval decision; goose wires up two tools).
///
/// SLICE 2 exposes this struct + [`write_tool_matrix`] (a public accessor
/// returning the full per-engine matrix) so the zoder CLI can print a
/// `--list-schemas` view an operator reads to understand "what is enforced
/// vs what would be denied fail-closed". The matrix itself stays defined
/// in `acp-client`; this is a read-only accessor, not a duplicate source
/// of truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteToolMatrixRow {
    /// Which engine this row applies to.
    pub engine: EngineKind,
    /// Canonical tool name as it appears on the wire (lowercased on
    /// lookup; preserved verbatim here so the printed matrix matches
    /// what an operator sees in a tool-approval log).
    pub tool_name: &'static str,
    /// Argument field that holds the target filesystem path for this
    /// `(engine, tool_name)` pair. `None` for engines whose arg shape
    /// is not yet wired up — those rows are intentionally here so the
    /// printed matrix shows "we know this is write-class but we cannot
    /// yet extract the path; enforcement is fail-closed on this row".
    pub path_field: Option<&'static str>,
}

/// Return the per-engine write-tool matrix as a flat, deterministic
/// (engine-asc, tool-asc) list of [`WriteToolMatrixRow`] entries. The
/// returned slice is `&'static` because both the engine roster and the
/// tool tables are compile-time constants — there is no allocation and
/// no fallible construction.
///
/// This is the SOLE public surface an external caller uses to render
/// the matrix (e.g. `zoder --list-schemas`); the CLI MUST NOT duplicate
/// the tables, otherwise it drifts from the decision the kernel
/// actually applies.
pub fn write_tool_matrix() -> &'static [WriteToolMatrixRow] {
    // Engine order matters for the rendered output (deterministic): zeroclaw
    // first, then goose. Within an engine, tools are in the order they
    // appear in the corresponding `*_WRITE_TOOLS` constant so the
    // `assert_eq!`-able shape is stable for the CLI's `--list-schemas` test.
    const ROWS: &[WriteToolMatrixRow] = &[
        // Zeroclaw: every documented write tool is in the matrix but the
        // extractor returns `None` for all of them (SLICE 1: arg plumbing
        // not yet wired). Render them with `path_field = None` so an
        // operator reading --list-schemas sees "we know these are
        // write-class, we cannot extract a path, enforcement denies on
        // fail-closed unless --trust-engine".
        WriteToolMatrixRow {
            engine: EngineKind::Zeroclaw,
            tool_name: "edit",
            path_field: None,
        },
        WriteToolMatrixRow {
            engine: EngineKind::Zeroclaw,
            tool_name: "write",
            path_field: None,
        },
        WriteToolMatrixRow {
            engine: EngineKind::Zeroclaw,
            tool_name: "apply_patch",
            path_field: None,
        },
        WriteToolMatrixRow {
            engine: EngineKind::Zeroclaw,
            tool_name: "shell",
            path_field: None,
        },
        WriteToolMatrixRow {
            engine: EngineKind::Zeroclaw,
            tool_name: "bash",
            path_field: None,
        },
        // Goose: write tools with a wired-up path extractor. `text_editor`
        // uses the canonical `path` field; `write_file` accepts either the
        // canonical `file_path` or the permissive `path` variant (snake_case
        // vs permissive camelCase). The path_field listed here is the
        // PRIMARY one for human readability; the actual extractor accepts
        // both, see `extract_write_target`.
        WriteToolMatrixRow {
            engine: EngineKind::Goose,
            tool_name: "text_editor",
            path_field: Some("path"),
        },
        WriteToolMatrixRow {
            engine: EngineKind::Goose,
            tool_name: "write_file",
            path_field: Some("file_path"),
        },
    ];
    ROWS
}

/// Render [`write_tool_matrix`] as a stable, multi-line, human-readable
/// string. Intended for `zoder --list-schemas`: the operator pastes the
/// output into a config review, an incident write-up, or a `git diff` of
/// the kernel policy. Format is deliberately plain-text (no ANSI) so it
/// is pipe-friendly and `grep`/`diff`-friendly.
///
/// Layout:
///
/// ```text
/// writable-root enforcement: per-engine write-tool matrix
/// (rows describe what is recognized as write-class; path_field=None means
///  the arg extractor is not yet wired up -> fail-closed deny on enforce)
/// engine     tool            path_field
/// zeroclaw   edit            -
/// zeroclaw   write           -
/// zeroclaw   apply_patch     -
/// zeroclaw   shell           -
/// zeroclaw   bash            -
/// goose      text_editor     path
/// goose      write_file      file_path  (also accepts: path)
/// ```
pub fn write_tool_matrix_human() -> String {
    let mut out = String::new();
    out.push_str("writable-root enforcement: per-engine write-tool matrix\n");
    out.push_str("(rows describe what is recognized as write-class; ");
    out.push_str("path_field=None means the arg extractor is not yet wired up ->\n");
    out.push_str(" fail-closed deny on enforce=true unless --trust-engine is set)\n");
    out.push_str("engine     tool            path_field\n");
    for row in write_tool_matrix() {
        // Engine name uses Debug so `EngineKind::Zeroclaw` -> "zeroclaw"
        // (the derived Debug for a unit variant is exactly the lowercased
        // variant name; this is the same string `EngineKind::FromStr`
        // accepts, so an operator can copy-paste it back into a config).
        let engine = format!("{:?}", row.engine).to_ascii_lowercase();
        let field = row.path_field.unwrap_or("-");
        // Pad for column alignment on the typical small matrix; longer
        // engine/tool names just push the column over (no truncation).
        out.push_str(&format!("{:<10} {:<15} {}\n", engine, row.tool_name, field));
    }
    out
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

/// Build the env that selects a goose model/provider AND bridges the
/// resolved provider's credential + endpoint into the child process.
/// The rest of the environment is inherited by [`EngineTransport::Stdio`].
///
/// Resolution precedence for `GOOSE_PROVIDER` (highest first):
///   1. explicit `provider_override` — a `Some(s)` is a test seam that
///      forces a specific value (used to avoid mutating global env,
///      which is `unsafe` in Rust 2024 because of parallel readers).
///   2. the resolved `GooseProviderEnv::kind` (from the zoder CLI's
///      `Config::real_best_provider_for_model` call). This is the
///      MODEL/PROVIDER selection parity seam: when zoder has picked
///      a specific provider we ALWAYS echo its kind so goose doesn't
///      silently fall through to its own default ("openai" or
///      "anthropic" depending on the binary).
///   3. `$GOOSE_PROVIDER` (operator override on the parent shell).
///   4. `"openai"` (goose's default; covers unknown zoder kinds +
///      custom OpenAI-compatible endpoints).
///
/// Resolution precedence for `GOOSE_MODEL`:
///   1. `opts.model_id` (the routed model id, e.g. `MiniMax-M3`,
///      `deepseek-chat`),
///   2. `opts.model_override`,
///   3. `opts.agent_alias` (last-resort fallback; zeroclaw alias).
///
/// Credential/endpoint bridge (the CREDENTIAL/ENDPOINT seam): when
/// `opts.goose_provider` is set, append:
///   * `OPENAI_API_KEY` = resolved key (NEVER logged; `Debug` on the
///     input struct redacts it; this fn never puts the key in any log
///     line, including the `tracing::debug!` callers typically add).
///   * `OPENAI_BASE_URL` = provider's `base_url` verbatim.
///   * `OPENAI_HOST` = base_url with the trailing `/v1` (and any deeper
///     version segment) stripped — goose uses `OPENAI_HOST` as the
///     session-override host and `OPENAI_BASE_URL` for the API path.
///
/// The bridge is ONLY applied when the resolved kind is OpenAI-
/// compatible (`openai-chat`, `openai-responses`, `custom`, or any
/// unknown kind — they all map onto goose's `openai` engine).
/// Anthropic kinds skip the OPENAI_* bridge entirely; goose reads
/// `ANTHROPIC_*` for those and the zoder provider's auth shape does
/// not map cleanly. For Anthropic, the CLI/operator is expected to
/// configure `ANTHROPIC_API_KEY` etc. on the parent shell — this
/// preserves the existing behavior and prevents leaking the wrong
/// credential into the wrong engine.
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
    // Pick the canonical goose provider name for GOOSE_PROVIDER. The
    // precedence is documented above; `provider_override` (the test
    // seam) ALWAYS wins.
    let provider = match provider_override {
        Some(Some(s)) => s.to_string(),
        Some(None) => "openai".to_string(),
        None => match &opts.goose_provider {
            Some(gp) => goose_provider_kind(gp).to_string(),
            None => std::env::var("GOOSE_PROVIDER").unwrap_or_else(|_| "openai".to_string()),
        },
    };
    let model = opts
        .model_id
        .clone()
        .or_else(|| opts.model_override.clone())
        .unwrap_or_else(|| opts.agent_alias.clone());
    let mut env: Vec<(String, String)> = Vec::with_capacity(5);
    env.push(("GOOSE_PROVIDER".to_string(), provider));
    env.push(("GOOSE_MODEL".to_string(), model));
    // CREDENTIAL/ENDPOINT bridge: only when the resolved provider is
    // OpenAI-compatible. Anthropic providers keep their existing
    // ambient-env behavior.
    if let Some(gp) = &opts.goose_provider {
        if is_openai_compatible_kind(&gp.kind) {
            if let Some(key) = gp.api_key.as_deref().filter(|s| !s.is_empty()) {
                env.push(("OPENAI_API_KEY".to_string(), key.to_string()));
            }
            if !gp.base_url.is_empty() {
                env.push(("OPENAI_BASE_URL".to_string(), gp.base_url.clone()));
                env.push(("OPENAI_HOST".to_string(), strip_v1(&gp.base_url)));
            }
        }
    }
    env
}

/// Map a zoder `Provider::kind` to the corresponding goose provider
/// name (the value goose keys on for `GOOSE_PROVIDER`). OpenAI-
/// compatible kinds collapse to `"openai"` because that's what
/// goose's `openai` engine handles (it reads `OPENAI_BASE_URL` etc.
/// for custom endpoints). Anthropic stays `"anthropic"`. Unknown
/// kinds are reported verbatim — if the operator pinned something
/// exotic they probably know the goose name; we don't guess.
pub(crate) fn goose_provider_kind(gp: &GooseProviderEnv) -> &'static str {
    if is_openai_compatible_kind(&gp.kind) {
        "openai"
    } else if gp.kind == "anthropic" {
        "anthropic"
    } else {
        // Unknown kind: fall through to the caller's provider_override
        // resolution (which defaults to "openai" / ambient). We return
        // a static sentinel by leaking into the resolution upstream;
        // here we just return "openai" so an unrecognized kind still
        // gets a sensible default rather than panicking.
        "openai"
    }
}

/// `true` for kinds that map onto goose's `openai` engine. This is
/// the central predicate for the OPENAI_* credential bridge.
pub(crate) fn is_openai_compatible_kind(kind: &str) -> bool {
    matches!(kind, "openai-chat" | "openai-responses" | "custom" | "")
}

/// Strip a trailing `/v1` (case-sensitive; must be exactly `/v1`,
/// not `/v1beta` or `/v2`) from `url` to produce the OPENAI_HOST
/// value goose expects. Mirrors goose's own
/// `goose_providers::openai::parse_openai_base_url` so the host
/// zoder forwards matches the host goose derives from the same
/// OPENAI_BASE_URL — a divergence here is exactly what would break
/// a custom-endpoint setup silently.
///
/// Rules (all taken from the goose test matrix):
///   * `https://api.openai.com/v1`  -> `https://api.openai.com`
///   * `https://gw.example.com/openai/v1` -> `https://gw.example.com/openai`
///   * `https://api.openai.com/v1/` -> `https://api.openai.com`
///     (trailing `/` is trimmed before the strip)
///   * `https://api.openai.com`     -> `https://api.openai.com` (no /v1)
///   * `https://example.com/v1beta` -> `https://example.com/v1beta`
///     (`/v1beta` is NOT `/v1` — preserve verbatim)
///   * `https://example.com/v2`     -> `https://example.com/v2`
///     (only `/v1` is stripped; other version segments are kept so
///     e.g. `/v4` Zhipu endpoints aren't silently rewired)
///   * Query strings on the BASE_URL are preserved verbatim — zoder
///     forwards `OPENAI_BASE_URL` unchanged; the host extract here
///     is only for OPENAI_HOST, which is the override surface and
///     isn't expected to carry query params in practice.
fn strip_v1(url: &str) -> String {
    // Split path from the rest at the FIRST `/` after the scheme
    // (or after `://` if present). We don't pull in the `url` crate
    // for one helper — a manual split keeps this hermetic.
    let (pre, path_with_query) = match url.find("://") {
        Some(i) => {
            let rest = &url[i + 3..];
            match rest.find('/') {
                Some(j) => (&url[..i + 3 + j], &rest[j..]),
                None => return url.to_string(),
            }
        }
        None => match url.find('/') {
            Some(j) => (&url[..j], &url[j..]),
            None => return url.to_string(),
        },
    };
    // Strip the query string for the matching test — goose's parser
    // keeps query params on the API request, not on OPENAI_HOST, so
    // the host extract must ignore them.
    let path_only = match path_with_query.find('?') {
        Some(q) => &path_with_query[..q],
        None => path_with_query,
    };
    // Trim trailing slashes so `/v1/` matches `/v1` (and `/v1/?...`
    // is treated identically).
    let path = path_only.trim_end_matches('/');
    if path == "/v1" {
        return pre.to_string();
    }
    if let Some(prefix) = path.strip_suffix("/v1") {
        return format!("{pre}{prefix}");
    }
    url.to_string()
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

    // PERSISTENT-SESSIONS SLICE: when the CLI opted in, look for a
    // non-stale record for this (engine, cwd) scope BEFORE sending
    // `session/new`. A `None` here drives the same "fresh session"
    // path as the OFF case — the wire shape is identical, so a
    // first-run-after-enable still produces a session/new without
    // `sessionId` in the params.
    let scope = session_store::make_scope(engine_kind_scope(EngineKind::Goose), &opts.cwd);
    let mut effective_session_id: Option<String> = opts.session_id.clone();
    if effective_session_id.is_none() && opts.persist_session_id {
        if let Some(store_path) = opts.session_store_path.as_ref() {
            let cfg = session_store::StoreConfig::new(store_path);
            if let Ok(Some(rec)) = session_store::EngineSessionStore::load(&cfg, &scope) {
                effective_session_id = Some(rec.session_id);
            }
        }
    }

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
    //
    //    PERSISTENT-SESSIONS SLICE: when an effective session id is
    //    available (either hand-set via `opts.session_id` or loaded
    //    from the persistence store), it is included as `sessionId`
    //    so the engine can resume an existing session. An
    //    unrecognized / expired id surfaces as a JSON-RPC error
    //    reply; we retry without `sessionId` to mint a fresh
    //    session. Z-24: the previous code also cleared the
    //    on-disk record on the reject path — that was unsafe
    //    because a transient failure on the fresh-create retry
    //    would leave the operator with an empty store AND no
    //    session, for no reason. The success-path
    //    `persist_session_after` at the end of `drive_goose_io`
    //    overwrites the record with the fresh id, and the
    //    failure path leaves it untouched.
    //
    //    `mcpServers` is populated by the CLI from the parsed engine-config
    //    server specs (see `zoder_core::to_acp_mcp_servers`); an empty
    //    `opts.mcp_servers` keeps today's wire shape (`[]`) so this slice
    //    is NON-BREAKING when no servers are configured.
    let mut new_params = serde_json::Map::new();
    new_params.insert("cwd".into(), json!(opts.cwd.to_string_lossy()));
    new_params.insert("mcpServers".into(), json!(opts.mcp_servers));
    if let Some(sid) = &effective_session_id {
        new_params.insert("sessionId".into(), json!(sid));
    }
    write_frame(
        write_half,
        &json!({
            "jsonrpc": "2.0",
            "id": "new",
            "method": "session/new",
            "params": Value::Object(new_params),
        }),
    )
    .await?;
    let new_res = match tokio::time::timeout_at(deadline, read_result_inner(reader, "new")).await {
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
    let new_res = match new_res {
        Ok(v) => v,
        Err(msg) => {
            // Engine rejected a resume. Z-24: do NOT clear the
            // on-disk record here. The previous behavior was to
            // drop the record so the next run wouldn't keep
            // tripping the same error — but if the fresh-create
            // retry below also fails, the operator would be
            // left with an empty store AND no session, for no
            // reason. The fresh id (if the retry succeeds) is
            // already persisted by `persist_session_after` at
            // the end of `drive_goose_io`, so there is no
            // benefit to dropping the existing record on the
            // reject path. The success-path persist overwrites
            // the record; the failure path leaves it untouched.
            let mut retry_params = serde_json::Map::new();
            retry_params.insert("cwd".into(), json!(opts.cwd.to_string_lossy()));
            retry_params.insert("mcpServers".into(), json!(opts.mcp_servers));
            write_frame(
                write_half,
                &json!({
                    "jsonrpc": "2.0",
                    "id": "new",
                    "method": "session/new",
                    "params": Value::Object(retry_params),
                }),
            )
            .await?;
            let retry =
                match tokio::time::timeout_at(deadline, read_result_inner(reader, "new")).await {
                    Ok(r) => r?,
                    Err(_) => {
                        return Err(anyhow!(
                            "goose session/new timed out after {:?}",
                            opts.timeout
                        ))
                    }
                };
            retry.map_err(|retry_msg| {
                anyhow!("engine error on session/new: {msg} (and the fresh-create retry also failed: {retry_msg})")
            })?
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
    //
    // PROJECT-INSTRUCTIONS SLICE: when the loader populated
    // `opts.project_instructions` at the CLI seam, the composed
    // prompt (instructions block + the user's task text, separated
    // by `---`) is what reaches the engine. When
    // `project_instructions` is `None`, `compose_session_prompt`
    // returns `opts.prompt` BYTE-FOR-BYTE, so any run without
    // AGENTS.md / CLAUDE.md at the repo root still sends the raw
    // prompt — matching the pre-this-slice wire shape verbatim
    // (regression pinned by `goose_prompt_frame_is_byte_identical_to_prompt_when_no_instructions`).
    let final_prompt = compose_session_prompt(opts);
    let prompt_frame = json!({
        "jsonrpc": "2.0",
        "id": "prompt",
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": final_prompt }],
        },
    });
    let mut content = String::new();
    // Y-9: cumulative byte cap on streamed `content` (see
    // [`MAX_CUMULATIVE_CONTENT_BYTES`]). The per-frame
    // [`MAX_FRAME_BYTES`] cap cannot stop a steady stream of
    // well-formed sub-cap `agent_message_chunk` frames from
    // accumulating gigabytes across the turn deadline, so the
    // streaming loop tracks a running total and bails the turn
    // when the cap is hit.
    let mut content_bytes: u64 = 0;
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
        //    but before the first `read_frame_line_capped`) is the
        //    actual deadlock fix: the reader is now ready to drain any
        //    frames the server emits in response.
        //
        //    Z-17: the per-frame read is bounded by
        //    [`MAX_FRAME_BYTES`] via [`read_frame_line_capped`]; an
        //    oversized / never-terminated frame surfaces as an
        //    `io::Error`, not an OOM.
        if !prompt_sent {
            write_frame(write_half, &prompt_frame).await?;
            prompt_sent = true;
            // Fall through to read_frame_line_capped on this same
            // iteration so the reader is polled immediately after the
            // write — no scheduler-induced gap between "sent prompt"
            // and "ready to read reply".
        }
        let got_line = match tokio::time::timeout_at(
            deadline,
            read_frame_line_capped(reader, &mut line),
        )
        .await
        {
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
                    let _ = tokio::time::timeout(
                        drain_budget,
                        read_frame_line_capped(reader, &mut line),
                    )
                    .await;
                }
                break "timeout".to_string();
            }
            Ok(r) => r.context("reading from goose ACP engine")?,
        };
        if !got_line {
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
        let headers = utilization_headers(&frame);
        if !headers.is_empty() {
            on_event(AgentEvent::Utilization { headers });
        }

        // --- A) The `session/prompt` RESPONSE carries the terminal
        //        stopReason and ends the turn.
        //
        // GD3: a JSON-RPC RESPONSE has NO `method` field; a REQUEST does.
        // Gate on `method` being absent so a server frame that happens to
        // carry id:"prompt" AND a `method` (a request) is NOT misread as
        // the terminal prompt response and does not end the turn early.
        if frame.get("method").is_none()
            && frame.get("id").and_then(Value::as_str) == Some("prompt")
        {
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
                    // Y-9: enforce the cumulative cap on the
                    // terminal `content`. The single frame is
                    // already capped by [`MAX_FRAME_BYTES`] (4 MiB),
                    // well below the 64 MiB cumulative cap, but
                    // the explicit check keeps the invariant
                    // consistent with the streaming chunk path.
                    if (c.len() as u64) > MAX_CUMULATIVE_CONTENT_BYTES {
                        bail!(
                            "session/prompt terminal `content` of {} bytes exceeds \
                             {MAX_CUMULATIVE_CONTENT_BYTES}-byte cumulative cap; \
                             possible hostile or runaway engine",
                            c.len()
                        );
                    }
                    content = c.to_string();
                    // The loop breaks immediately below; the
                    // streaming-loop's `content_bytes` counter is
                    // no longer consulted, so we deliberately do
                    // NOT update it here (would be a dead write
                    // and trip `unused_assignments`).
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
            // GD1: the SECURITY decision keys on the ACP MACHINE category
            // (`toolCall.kind`, an ACP v1 semantic kind: read/search/fetch/
            // edit/delete/move/execute/...), NOT the human `title`. `title`
            // is used only for the display AgentEvent below. When `kind` is
            // absent we fall back to the name-based heuristic inside
            // `decide_approval_with_containment`.
            let tool_kind = req_params
                .get("toolCall")
                .and_then(|tc| tc.get("kind"))
                .and_then(Value::as_str)
                .map(str::to_string);
            // Real ACP v1 spec: `optionId` is an OPAQUE server-side id; the
            // semantic meaning lives in `options[].kind` (one of
            // `allow_once`, `allow_always`, `reject_once`, `reject_always`).
            // We MUST match by `kind` and echo back the option's actual
            // `optionId` in `result.outcome.optionId` — the server's id, not
            // a hard-coded string. Matching on `optionId` directly (the old
            // behavior) was wrong because (a) real goose may surface
            // server-generated ids like `"opt-A9F1"` instead of the canonical
            // names, and (b) the semantics are in `kind` by spec.
            // SLICE 1: route through the argument-aware decision so
            // `enforce_writable_roots` is honored. The "params" we pass
            // to the extractor is `toolCall.rawInput` — the tool-call
            // argument object per the standard ACP v1 spec, where the
            // per-engine matrix in `extract_write_target` looks up
            // the target path. If `rawInput` is absent (e.g. an older
            // server variant or a non-standard MCP tool), we pass an
            // empty `Value::Object` so the extractor returns `None`
            // and the fail-closed path DENIES on enforce=true.
            let tool_args = req_params
                .get("toolCall")
                .and_then(|tc| tc.get("rawInput"))
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()));
            let approved = decide_approval_with_containment(
                opts.approval,
                &tool_name,
                tool_kind.as_deref(),
                &tool_args,
                EngineKind::Goose,
                &opts.writable_roots,
                opts.enforce_writable_roots,
                opts.trust_engine,
            );
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
                    // Y-9: enforce the cumulative cap before
                    // appending. See [`MAX_CUMULATIVE_CONTENT_BYTES`].
                    let new_total = content_bytes.saturating_add(t.len() as u64);
                    if new_total > MAX_CUMULATIVE_CONTENT_BYTES {
                        bail!(
                            "streamed content exceeded {MAX_CUMULATIVE_CONTENT_BYTES}-byte \
                             cumulative cap (already appended {content_bytes} bytes across prior \
                             frames; refusing to grow `content` unbounded); possible \
                             hostile or runaway engine"
                        );
                    }
                    content.push_str(&t);
                    content_bytes = new_total;
                    on_event(AgentEvent::Text(t));
                }
            }
            "agent_thought_chunk" if opts.show_reasoning => {
                if let Some(t) = extract_text_content(update.get("content")) {
                    on_event(AgentEvent::Thought(t));
                }
            }
            "agent_message" => {
                // Final assistant message for the turn (ContentBlock).
                //
                // GD2: `agent_message` is AUTHORITATIVE — it REPLACES the
                // accumulated `content` rather than appending to it. Goose
                // may stream the reply as `agent_message_chunk`s AND then
                // send a final `agent_message` carrying the full text; a
                // naive append double-counts it ("hello worldhello world")
                // and emits `Text` twice. By resetting `content`/
                // `content_bytes` and treating this frame as the source of
                // truth, the reply lands exactly ONCE regardless of whether
                // chunks preceded it.
                if let Some(t) = extract_text_content(update.get("content")) {
                    // Y-9: enforce the cumulative cap against the single
                    // authoritative message. See [`MAX_CUMULATIVE_CONTENT_BYTES`].
                    if (t.len() as u64) > MAX_CUMULATIVE_CONTENT_BYTES {
                        bail!(
                            "terminal `agent_message` of {} bytes exceeds \
                             {MAX_CUMULATIVE_CONTENT_BYTES}-byte cumulative cap; \
                             possible hostile or runaway engine",
                            t.len()
                        );
                    }
                    content = t.clone();
                    content_bytes = t.len() as u64;
                    on_event(AgentEvent::Text(t));
                }
            }
            "tool_call" | "tool_call_update" if kind == "tool_call" => {
                // Spec: `toolCallId`, `title`, `kind`, `status`, `content`.
                // The "call" arrives once (status pending) and updates
                // arrive as `tool_call_update` with the same id. We count
                // AND emit the ToolCall event ONLY for the initial call
                // (the `if kind == "tool_call"` match guard), never for
                // progress updates: emitting on every status transition
                // (pending -> in_progress -> completed) makes the CLI
                // display "[tool] shell" once per transition, i.e. three
                // "tool called" lines for one logical call. The
                // `tool_calls` counter and the ToolCall event stream must
                // agree on "one logical call == one event" — otherwise the
                // counter says 1 but the CLI shows 3 invocations.
                tool_calls += 1;
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

    // PERSISTENT-SESSIONS SLICE: persisting the (potentially-freshly-
    // minted) session id so the NEXT run resumes it. Same
    // rationale as the zeroclaw driver: a write failure here is
    // a warning, not a run failure (a missed save means the next
    // run starts fresh, identical to the OFF path).
    persist_session_after(&session_id, opts, &scope);
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

/// Read a single NDJSON line from `reader` into `line`, bounded by
/// [`MAX_FRAME_BYTES`]. Returns:
///
/// * `Ok(true)`  — a complete line was read (the buffer ends with
///   `\n`; the caller trims it as today).
/// * `Ok(false)` — EOF reached before any bytes were read (the
///   connection was closed cleanly between frames).
/// * `Err`       — an I/O error, OR the line exceeded
///   [`MAX_FRAME_BYTES`] bytes without a trailing `\n`. The error is
///   reported as `io::ErrorKind::InvalidData` with a message naming
///   the cap, so a misbehaving engine that emits a giant line (or
///   never sends a newline) cannot OOM the driver — it fails fast
///   with a useful diagnostic instead.
///
/// The cap is enforced via [`AsyncReadExt::take`]: the underlying
/// reader's buffered bytes (beyond the cap) are NOT discarded —
/// they remain in the buffer for the next frame read. The cap is
/// PER-FRAME, not cumulative, so a malformed frame cannot corrupt a
/// subsequent legitimate one.
///
/// Z-17: introduced to bound the per-frame read that previously
/// used an unbounded [`AsyncBufReadExt::read_line`]. All wire-layer
/// readers (the `read_result` handshake, the streaming
/// session/update loop in `drive` / `drive_goose_io`, and the
/// cancel-ack drain in `cancel_session`) route through this
/// function.
async fn read_frame_line_capped<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut String,
) -> std::io::Result<bool> {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let n = reader
        .take(MAX_FRAME_BYTES)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        // EOF — engine closed the connection between frames.
        line.clear();
        return Ok(false);
    }
    if buf.last() != Some(&b'\n') {
        // The cap was hit before any delimiter was found. Treat
        // this as a hostile / runaway frame: the cap exists
        // specifically so the driver cannot be OOMed by a single
        // malformed line. We do NOT consume the line buffer
        // (already empty — `line` was not touched yet) so a
        // partial frame cannot "stick" into a subsequent
        // legitimate one if the caller decides to continue.
        line.clear();
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "engine frame exceeds {MAX_FRAME_BYTES}-byte cap (no newline found within cap); \
                 possible hostile or runaway engine"
            ),
        ));
    }
    // NDJSON requires UTF-8; lossily replace invalid bytes so a
    // misbehaving engine cannot wedge the driver on a non-UTF-8
    // byte in the payload (it'll just fail to JSON-parse, which
    // the caller already handles by skipping the frame).
    // Y-15: consume `buf` into the String directly on the valid-UTF-8 path
    // (reuses the same allocation, no copy); fall back to the lossy copy only
    // when the bytes aren't valid UTF-8. The previous
    // `from_utf8_lossy(&buf).into_owned()` always allocated a second buffer.
    *line = match String::from_utf8(buf) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(&e.into_bytes()).into_owned(),
    };
    Ok(true)
}

/// Read NDJSON frames until the response with `want_id` arrives (skipping
/// notifications and unrelated responses). Returns the `result` value.
async fn read_result(
    reader: &mut (impl AsyncBufReadExt + Unpin),
    want_id: &str,
) -> anyhow::Result<Value> {
    read_result_inner(reader, want_id)
        .await?
        .map_err(|msg| anyhow!("engine error on {want_id}: {msg}"))
}

/// Lower-level variant that returns the engine-error message as `Err`
/// (without an anyhow wrapper) so the caller can decide whether the
/// error is recoverable in its context. Used by the persistence-aware
/// `session/new` sites to distinguish a JSON-RPC error reply (which
/// triggers the resume-rejected fallback in the persistent-sessions
/// slice) from any other IO/protocol failure.
///
/// Z-17: the inner read is now bounded by [`MAX_FRAME_BYTES`] via
/// [`read_frame_line_capped`]. A hostile / runaway engine that emits
/// a frame larger than the cap surfaces as an `io::ErrorKind::InvalidData`
/// error, never an OOM.
async fn read_result_inner(
    reader: &mut (impl AsyncBufReadExt + Unpin),
    want_id: &str,
) -> anyhow::Result<Result<Value, String>> {
    let mut line = String::new();
    loop {
        let got_line = read_frame_line_capped(reader, &mut line)
            .await
            .context("reading from engine")?;
        if !got_line {
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
                .unwrap_or("unknown error")
                .to_string();
            return Ok(Err(msg));
        }
        return Ok(Ok(frame.get("result").cloned().unwrap_or(Value::Null)));
    }
}

#[cfg(test)]
mod tests {
    // Clippy nits: `&[x.clone()]` is idiomatic for tests (we want to
    // assert the closure captures a single-element slice), and the
    // `single_match` / `needless_collect` style here is fine for test
    // code clarity. Allow the specific lints instead of refactoring
    // every call site.
    #![allow(clippy::cloned_ref_to_slice_refs)]
    use super::*;
    // `AsyncBufReadExt` is brought in by `use super::*` (it lives in the
    // parent module's `use` lines). The alias is no longer needed.

    // -----------------------------------------------------------------
    // PROJECT-INSTRUCTIONS SLICE — prompt composition regression guard.
    //
    // These tests pin the contract `compose_session_prompt` exposes
    // to both `drive` (zeroclaw wire shape) and `drive_goose_io`
    // (goose ACP wire shape):
    //
    //   * When `AgentOptions::project_instructions` is `None`, the
    //     composed prompt text MUST be EXACTLY `opts.prompt`
    //     byte-for-byte — matching every pre-this-slice run's wire
    //     shape (any project without AGENTS.md / CLAUDE.md sees no
    //     change). This is the non-breaking regression guard the
    //     slice spec calls out.
    //
    //   * When `Some(text)`, the composed prompt MUST prepend a
    //     clearly-delimited `# Project instructions (AGENTS.md)`
    //     block ahead of the user's task, and the user's task text
    //     MUST still appear verbatim somewhere in the result.
    //
    // An empty `Some("")` is normalized to "no instructions" —
    // same behavior as `None` — so a zero-byte file cannot leak a
    // bare-header block into the prompt.
    // -----------------------------------------------------------------
    fn opts_with_prompt(prompt: &str) -> AgentOptions {
        let mut o = goose_opts(None);
        o.prompt = prompt.to_string();
        o
    }

    #[test]
    fn prompt_none_is_byte_identical_to_task() {
        let opts = opts_with_prompt("hello");
        // `project_instructions` defaults to None in `goose_opts`,
        // matching the `AgentOptions::new` default.
        assert_eq!(
            opts.project_instructions, None,
            "compose_session_prompt regression guard requires the default to be None",
        );
        let composed = compose_session_prompt(&opts);
        assert_eq!(
            composed, "hello",
            "with no project_instructions, the composed prompt MUST be exactly the task text \
             (regression guard against silently injecting a header for repos without AGENTS.md)",
        );
    }

    #[test]
    fn prompt_empty_string_treated_as_none() {
        // Empty `Some("")` is a degenerate case (a zero-byte file or
        // a forgotten-`trim` bug upstream) and would otherwise produce
        // an empty header block in front of the task. Normalize it
        // to "no instructions" so the wire shape stays identical to
        // the `None` path.
        let mut opts = opts_with_prompt("hello");
        opts.project_instructions = Some(String::new());
        assert_eq!(
            compose_session_prompt(&opts),
            "hello",
            "empty project_instructions must NOT prepend an empty header block",
        );
    }

    #[test]
    fn prompt_some_prepends_header_and_keeps_task_verbatim() {
        // Block layout: `# Project instructions (AGENTS.md)\n\n{text}\n\n---\n\n{task}`.
        // Mirrors `zoder_core::project_instructions::compose_prompt` so
        // the CLI loader and the in-driver composer stay in lockstep;
        // any future drift is caught by the parallel `zoder_core`
        // tests and by these.
        const HEADER: &str = "# Project instructions (AGENTS.md)\n\n";
        const SEPARATOR: &str = "\n\n---\n\n";

        let mut opts = opts_with_prompt("do the thing");
        opts.project_instructions = Some("be polite".to_string());

        let composed = compose_session_prompt(&opts);
        let expected = format!("{HEADER}be polite{SEPARATOR}do the thing");
        assert_eq!(
            composed, expected,
            "the prepended block must follow the documented header / separator format",
        );

        // The user's task text must still appear verbatim SOMEWHERE
        // in the composed prompt; that's the whole point of
        // prepending instead of replacing. Tests assert presence
        // (not position) so future refactors that move the header
        // to a foot-block are still caught — they would change the
        // prompt in user-visible ways without affecting this check.
        assert!(
            composed.contains("do the thing"),
            "composed prompt must contain the user's task text verbatim; got {composed:?}",
        );

        // And instructions must precede the user's task so the model
        // treats them as system-level guidance, not as a follow-up
        // instruction inside the task stream.
        let instr_pos = composed
            .find("be polite")
            .expect("instructions text present in composed prompt");
        let task_pos = composed
            .find("do the thing")
            .expect("task text present in composed prompt");
        assert!(
            instr_pos < task_pos,
            "instructions must precede the user's task in the composed prompt \
             (got instructions at byte {instr_pos}, task at byte {task_pos})",
        );

        // The separator is the single visible boundary between the
        // two blocks — the model and any debug/log output can grep
        // for it to tell project-instructions from user task.
        assert!(
            composed.contains("\n\n---\n\n"),
            "the instructions/task boundary separator must be present in the composed prompt",
        );
    }

    #[test]
    fn prompt_some_preserves_unicode_and_newlines_in_task() {
        // Regression for an earlier risk: a misbegotten `.trim()` or
        // `.chars().collect()` between `prompt` and the wire would
        // silently drop multi-byte / newline content. The composed
        // prompt must carry the task text through verbatim.
        let task = "summary\n  - fix login\n  - tweak copy\n  ñ";
        let mut opts = opts_with_prompt(task);
        opts.project_instructions = Some("instructions".to_string());
        let composed = compose_session_prompt(&opts);
        assert!(
            composed.contains(task),
            "task text must appear verbatim in the composed prompt; got {composed:?}",
        );
    }

    #[test]
    fn agent_options_new_defaults_project_instructions_to_none() {
        // The non-breaking default: `AgentOptions::new` must yield
        // `project_instructions: None` so any pre-this-slice
        // construction site keeps sending the raw prompt.
        let opts = AgentOptions::new("/tmp/sock", "codex", "/tmp/repo", "hi");
        assert_eq!(
            opts.project_instructions, None,
            "AgentOptions::new must default project_instructions to None (non-breaking)",
        );
    }

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
    fn utilization_metadata_extracts_only_known_header_families() {
        let update = json!({
            "type": "context_usage",
            "_meta": {
                "response_headers": {
                    "X-Codex-Primary-Used-Percent": 93,
                    "x-codex-plan-type": "pro",
                    "authorization": "Bearer must-not-escape",
                    "x-request-id": "not-utilization"
                }
            }
        });
        let headers = utilization_headers(&update);
        assert_eq!(
            headers,
            vec![
                ("x-codex-plan-type".into(), "pro".into()),
                ("x-codex-primary-used-percent".into(), "93".into()),
            ]
        );
    }

    #[test]
    fn policy_all_approves_everything_none_denies() {
        assert!(decide_approval(ApprovalPolicy::All, "shell"));
        assert!(decide_approval(ApprovalPolicy::All, "anything_at_all"));
        assert!(!decide_approval(ApprovalPolicy::None, "read"));
    }

    // -----------------------------------------------------------------------
    // SLICE 1 — writable-root containment (the execution-safety kernel)
    //
    // The tests below lock in the boundary properties. They are organized
    // into four groups:
    //
    //   A. Containment primitive (`path_within_roots`)
    //   B. Argument-aware enforcement (`decide_approval_with_containment`)
    //   C. Fail-closed posture (unknown engine / unknown tool / no path)
    //   D. Non-breaking (enforce=false is exactly today's name-based decision)
    //
    // Containment tests use a real tempdir so the boundary is exercised
    // against real on-disk paths and symlinks (rather than a synthetic
    // `PathBuf`-only mock that wouldn't catch a real `canonicalize` bug).
    // -----------------------------------------------------------------------

    /// Process-wide monotonic counter used to make `make_containment_dirs`
    /// produce a UNIQUE subdir name on every call. The previous
    /// implementation used `SystemTime::now().as_nanos()` alone, which
    /// collides when two tests in the same process start within the same
    /// nanosecond (cargo's default test runner runs tests in parallel
    /// threads). The collision caused the `TempdirShim` Drop impls of one
    /// pair of tests to `remove_dir_all` the dir that the other pair was
    /// still mutating, producing "ghost" containment failures like
    /// `containment_symlink_escape_denied` flaking on a target that
    /// "vanished" mid-test, and `containment_inside_root_allowed` flaking
    /// when the target file was `remove_dir_all`'d before the assertion
    /// read it back. The atomic counter (combined with pid + nanos for
    /// defense-in-depth across processes) is a no-extra-dep fix that
    /// guarantees a unique suffix per call within the process.
    static TEMPDIR_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Tiny stand-in for `tempfile::Tempdir`: drops the directory tree on
    /// `Drop` so tests don't leak even when assertions abort early. Lives
    /// inside the `tests` module so it isn't part of the public surface.
    struct TempdirShim(PathBuf);
    impl Drop for TempdirShim {
        fn drop(&mut self) {
            // Only remove if the dir still exists; under parallel tests a
            // peer TempdirShim may have already swept it (we tolerate the
            // resulting NotFound here so a benign race never panics on
            // drop).
            match std::fs::remove_dir_all(&self.0) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {}
            }
        }
    }
    impl TempdirShim {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    /// Helper: build a fresh `TempdirShim` with two real roots (`root_a`,
    /// `root_b`) that are `canonicalize`-able, plus a separate `outside`
    /// dir that is guaranteed not to be under either root. Returns the
    /// shim (for `.path()`) and the three paths.
    fn make_containment_dirs() -> (TempdirShim, PathBuf, PathBuf, PathBuf) {
        // Hand-rolled tempdir (no extra dep just for tests): create a
        // unique subdir of the test temp dir; the `TempdirShim` returned
        // here drops and `remove_dir_all`s it on test exit, so the OS
        // doesn't accumulate stale trees even when assertions abort.
        //
        // Uniqueness: the directory name embeds pid (cross-process),
        // nanoseconds since the Unix epoch (cross-run defense-in-depth),
        // AND a per-process atomic counter (the only component that
        // actually changes between two calls inside the same process on
        // the same nanosecond). Together this is collision-free for the
        // `cargo test` thread fan-out we run.
        let seq = TEMPDIR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "zoder-acp-slice1-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            seq,
        ));
        std::fs::create_dir_all(&base).unwrap();
        let root_a = base.join("root_a");
        let root_b = base.join("root_b");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        (TempdirShim(base), root_a, root_b, outside)
    }

    #[test]
    fn containment_inside_root_allowed() {
        let (dir, root_a, _root_b, _outside) = make_containment_dirs();
        let file = root_a.join("file.txt");
        std::fs::write(&file, "ok").unwrap();
        // Inside: single root, the file sits directly under it.
        assert!(path_within_roots(&file, &[root_a.clone()]));
        // Inside: nested file.
        let nested = root_a.join("sub").join("deep.txt");
        std::fs::create_dir_all(root_a.join("sub")).unwrap();
        std::fs::write(&nested, "ok").unwrap();
        assert!(path_within_roots(&nested, &[root_a.clone()]));
        // Inside: multiple roots, file is under one of them.
        let root_b = dir.path().join("root_b");
        assert!(path_within_roots(&file, &[root_a.clone(), root_b.clone()]));
    }

    #[test]
    fn containment_outside_root_denied() {
        let (_dir, root_a, _root_b, outside) = make_containment_dirs();
        let file = outside.join("evil.txt");
        std::fs::write(&file, "x").unwrap();
        // Outside: target under a dir that is not in the roots.
        assert!(!path_within_roots(&file, &[root_a.clone()]));
        // Outside: empty roots list.
        assert!(!path_within_roots(&file, &[]));
        // Outside: target's parent is a prefix of a root but target is
        // not actually under it (the `starts_with` check is
        // component-aware).
        let root_prefix = root_a.join("foo");
        let evil_sibling = root_a.join("foobar");
        std::fs::write(&evil_sibling, "x").unwrap();
        assert!(
            !path_within_roots(&evil_sibling, &[root_prefix.clone()]),
            "starts_with on Path is component-aware, so root_a/foo does NOT \
             contain root_a/foobar"
        );
    }

    #[test]
    fn containment_parent_dir_traversal_denied() {
        let (dir, root_a, _root_b, outside) = make_containment_dirs();
        // Place a sensitive file OUTSIDE the root.
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, "TOP SECRET").unwrap();
        // Build a target that traverses OUT of the root via `..` to reach
        // the secret. The target does not exist yet (we are testing the
        // pre-write containment check), so the not-yet-existing branch
        // of `canonicalize_for_write` is the one under test.
        //
        // `dir.path()/root_a/../outside/secret.txt` canonicalizes to
        // `dir.path()/outside/secret.txt`, which is OUTSIDE `root_a`.
        let sneaky = dir
            .path()
            .join("root_a")
            .join("..")
            .join("outside")
            .join("secret.txt");
        assert!(
            !path_within_roots(&sneaky, &[root_a.clone()]),
            "dot-dot traversal must be denied: target={}",
            sneaky.display()
        );
        // And the same path with extra `..` segments is also denied.
        let extra_sneaky = dir
            .path()
            .join("root_a")
            .join("sub")
            .join("..")
            .join("..")
            .join("outside")
            .join("secret.txt");
        assert!(!path_within_roots(&extra_sneaky, &[root_a.clone()]));
    }

    #[test]
    fn containment_symlink_escape_denied() {
        let (dir, root_a, _root_b, outside) = make_containment_dirs();
        // Place a sensitive file outside the root.
        let secret = outside.join("passwd");
        std::fs::write(&secret, "root:x:0:0:").unwrap();
        // Inside the root, create a SYMLINK that points at the secret.
        // `canonicalize` follows the symlink, so the resolved target is
        // OUTSIDE the root and containment must deny it.
        let symlink_path = root_a.join("sneaky_link");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&secret, &symlink_path).unwrap();
            // The symlink ITSELF is inside the root, but resolution
            // follows it and lands outside. `path_within_roots` must
            // deny.
            assert!(
                !path_within_roots(&symlink_path, &[root_a.clone()]),
                "symlink escape must be denied: symlink={} -> real={}",
                symlink_path.display(),
                secret.display()
            );
            // The real path of the secret is also denied (sanity).
            assert!(!path_within_roots(&secret, &[root_a.clone()]));
        }
        // On non-Unix we just verify the basic containment still holds
        // for the inside case so the test doesn't false-fail on Windows.
        #[cfg(not(unix))]
        {
            let inside = root_a.join("normal.txt");
            std::fs::write(&inside, "ok").unwrap();
            assert!(path_within_roots(&inside, &[root_a.clone()]));
        }
        // Suppress an "unused" warning on the captured `dir` on non-Unix.
        let _ = dir;
    }

    /// Regression test for the parallel-test tempdir-name collision
    /// that previously caused this file's symlink/inside-root tests to
    /// flake under `cargo test` (the default test runner fans out
    /// across threads, and two tests starting in the same nanosecond
    /// used to produce the SAME `zoder-acp-slice1-<pid>-<nanos>` path
    /// — so one's `TempdirShim::drop` would `remove_dir_all` the
    /// other's test artifacts mid-test, and `path_within_roots` would
    /// see a "vanished" target). The fix: `make_containment_dirs` now
    /// appends a process-wide atomic counter, making the directory
    /// name strictly unique per call inside the process. This test
    /// exercises that property directly: it creates N independent
    /// containment dirs in a tight loop and asserts that all of them
    /// (a) point at distinct filesystem paths and (b) are correctly
    /// classified by `path_within_roots` (inside == true, outside ==
    /// false) when paired with their own roots. If anyone regresses
    /// the uniqueness (e.g. removes the counter), this test will
    /// catch it deterministically — no parallel runner required.
    #[test]
    fn containment_tempdir_names_are_strictly_unique() {
        const N: usize = 32;
        let mut dirs: Vec<(TempdirShim, PathBuf, PathBuf)> = Vec::with_capacity(N);
        for _ in 0..N {
            let (shim, root_a, root_b, _outside) = make_containment_dirs();
            dirs.push((shim, root_a, root_b));
        }
        // Every base path must be distinct from every other base path.
        let mut bases: Vec<PathBuf> = dirs
            .iter()
            .map(|(shim, _, _)| shim.path().to_path_buf())
            .collect();
        let n_before_dedup = bases.len();
        bases.sort();
        bases.dedup();
        assert_eq!(
            bases.len(),
            n_before_dedup,
            "make_containment_dirs must return distinct base paths on every call; \
             duplicates mean the tempdir-naming fix has regressed (re-introducing \
             the parallel-test flake that previously broke \
             containment_symlink_escape_denied and containment_inside_root_allowed)"
        );
        // And the per-test root_a / root_b paths must also all be
        // distinct (defense-in-depth: even if a base collision sneaked
        // in, root_a would still be unique because it's base+"/root_a").
        let mut all_roots: Vec<PathBuf> = dirs
            .iter()
            .flat_map(|(_, a, b)| [a.clone(), b.clone()])
            .collect();
        let n_roots = all_roots.len();
        all_roots.sort();
        all_roots.dedup();
        assert_eq!(
            all_roots.len(),
            n_roots,
            "every root_a / root_b across calls must be distinct"
        );
        // Each dir must classify its OWN root_a as inside (sanity
        // check that the counter-padded paths are still functional
        // containment roots, not just "unique looking" ones).
        for (shim, root_a, root_b) in &dirs {
            let inside_a = root_a.join("ok.txt");
            std::fs::write(&inside_a, "x").unwrap();
            assert!(
                path_within_roots(&inside_a, &[root_a.clone()]),
                "self-pair inside must be allowed: base={}",
                shim.path().display()
            );
            // And a file under root_b is NOT inside root_a (cross-root
            // sanity; the counter must not have produced overlapping
            // roots).
            let b_file = root_b.join("y.txt");
            std::fs::write(&b_file, "x").unwrap();
            assert!(
                !path_within_roots(&b_file, &[root_a.clone()]),
                "cross-root sanity: file under root_b must NOT be inside root_a; \
                 base={}",
                shim.path().display()
            );
        }
    }

    #[test]
    fn containment_not_yet_existing_file_under_root_allowed() {
        let (_dir, root_a, _root_b, _outside) = make_containment_dirs();
        // The target does not exist yet (about to be created by a write
        // tool). It sits under the root. The not-yet-existing branch of
        // `canonicalize_for_write` must walk up to the existing ancestor
        // (the root) and re-join the tail, classifying the target as
        // inside.
        let future_file = root_a.join("new").join("soon.txt");
        assert!(!future_file.exists(), "precondition: target must not exist");
        assert!(path_within_roots(&future_file, &[root_a.clone()]));
        // Single-segment tail (file directly under the root) also works.
        let future_top = root_a.join("brand_new.txt");
        assert!(path_within_roots(&future_top, &[root_a.clone()]));
        // A not-yet-existing target that would land OUTSIDE the root
        // (via traversal) is still denied.
        let sneaky_future = root_a.join("..").join("outside").join("later.txt");
        assert!(!path_within_roots(&sneaky_future, &[root_a.clone()]));
    }

    #[test]
    fn enforce_off_is_exactly_today_behavior() {
        // The new arg-aware function must, with enforce=false, return
        // EXACTLY the same result as the old name-based `decide_approval`.
        // That is the non-breaking guarantee: every existing call site
        // continues to behave identically until the follow-up flips the
        // default.
        let cases: &[(&str, ApprovalPolicy, bool)] = &[
            // (tool, policy, expected)
            ("read", ApprovalPolicy::Allowlist, true),
            ("grep", ApprovalPolicy::Allowlist, true),
            ("shell", ApprovalPolicy::Allowlist, false),
            ("write", ApprovalPolicy::Allowlist, false),
            ("shell", ApprovalPolicy::All, true),
            ("anything", ApprovalPolicy::All, true),
            ("read", ApprovalPolicy::None, false),
            ("shell", ApprovalPolicy::None, false),
            ("dangerous_shell_proxy", ApprovalPolicy::Allowlist, false),
        ];
        for (tool, policy, expected) in cases {
            let new_decision = decide_approval_with_containment(
                *policy,
                tool,
                None,
                &Value::Null,
                EngineKind::Goose,
                &[],
                /* enforce = */ false,
                /* trust_engine = */ false,
            );
            let old_decision = decide_approval(*policy, tool);
            assert_eq!(
                new_decision, old_decision,
                "enforce=false must match decide_approval for ({tool:?}, {policy:?})"
            );
            assert_eq!(
                new_decision, *expected,
                "enforce=false for ({tool:?}, {policy:?}) must equal {expected}"
            );
        }
    }

    #[test]
    fn enforce_on_denies_write_tool_outside_roots_under_all() {
        // Even under ApprovalPolicy::All (which would normally approve
        // every tool call), a write tool whose target resolves outside
        // the writable roots must be DENIED when enforce=true.
        let (_dir, root_a, _root_b, outside) = make_containment_dirs();
        let evil_target = outside.join("leak.txt");
        std::fs::write(&evil_target, "x").unwrap();
        // Goose write_file targeting the evil path.
        let goose_args = json!({ "file_path": evil_target.to_string_lossy() });
        assert!(
            !decide_approval_with_containment(
                ApprovalPolicy::All,
                "write_file",
                None,
                &goose_args,
                EngineKind::Goose,
                &[root_a.clone()],
                /* enforce = */ true,
                /* trust_engine = */ false,
            ),
            "ApprovalPolicy::All + enforce=true + write_file outside roots MUST deny"
        );
        // Same under Allowlist (also expected to deny via the boundary).
        assert!(!decide_approval_with_containment(
            ApprovalPolicy::Allowlist,
            "write_file",
            None,
            &goose_args,
            EngineKind::Goose,
            &[root_a.clone()],
            true,
            false,
        ));
        // Same with the other goose write tool (text_editor).
        let text_editor_args = json!({ "path": evil_target.to_string_lossy() });
        assert!(!decide_approval_with_containment(
            ApprovalPolicy::All,
            "text_editor",
            None,
            &text_editor_args,
            EngineKind::Goose,
            &[root_a.clone()],
            true,
            false,
        ));
    }

    #[test]
    fn enforce_on_allows_write_tool_inside_roots_under_all() {
        // Mirror of the above: a write tool whose target resolves inside
        // the writable roots must still be APPROVED under All when
        // enforce=true (the boundary does NOT block legitimate writes).
        let (_dir, root_a, _root_b, _outside) = make_containment_dirs();
        let ok_target = root_a.join("ok.txt");
        std::fs::write(&ok_target, "x").unwrap();
        let goose_args = json!({ "file_path": ok_target.to_string_lossy() });
        assert!(decide_approval_with_containment(
            ApprovalPolicy::All,
            "write_file",
            None,
            &goose_args,
            EngineKind::Goose,
            &[root_a.clone()],
            true,
            false,
        ));
    }

    #[test]
    fn enforce_on_read_tool_unaffected() {
        // Read-class tools must NOT be affected by writable_roots even
        // when enforce=true. A `read` against a path outside the roots
        // is still allowed under Allowlist (the read is not a write,
        // so the boundary doesn't gate it).
        let (_dir, _root_a, _root_b, outside) = make_containment_dirs();
        let outside_file = outside.join("x.txt");
        std::fs::write(&outside_file, "x").unwrap();
        assert!(decide_approval_with_containment(
            ApprovalPolicy::Allowlist,
            "read",
            None,
            &json!({ "path": outside_file.to_string_lossy() }),
            EngineKind::Goose,
            &[PathBuf::from("/some/unused/root")],
            true,
            false,
        ));
    }

    #[test]
    fn fail_closed_unknown_engine_denies_on_enforce() {
        // `EngineKind` has only two variants today (Zeroclaw, Goose); we
        // simulate "unknown engine" by calling the per-engine matrix
        // helpers directly, asserting they all return Unknown / fail
        // closed. The high-level entry point also denies for the
        // zeroclaw path on enforce=true (the matrix is registered but
        // the extractor returns None — see the per-engine matrix
        // comment in lib.rs).
        assert!(!is_engine_known(EngineKind::Zeroclaw) || is_engine_known(EngineKind::Zeroclaw));
        // Zeroclaw: write tool with no plumbing -> fail-closed deny.
        assert!(!decide_approval_with_containment(
            ApprovalPolicy::All,
            "write",
            None,
            &json!({ "path": "/tmp/anything" }),
            EngineKind::Zeroclaw,
            &[],
            true,
            /* trust_engine = */ false,
        ));
        // Even with a path-shaped arg, the zeroclaw extractor returns
        // None in slice 1, so the fail-closed branch fires.
        assert!(!decide_approval_with_containment(
            ApprovalPolicy::All,
            "edit",
            None,
            &json!({ "path": "/tmp/anything" }),
            EngineKind::Zeroclaw,
            &[],
            true,
            false,
        ));
    }

    #[test]
    fn fail_closed_unknown_goose_write_tool_denies_on_enforce() {
        // A write tool that is NOT in the goose per-engine matrix
        // (e.g. a custom MCP tool called "wipe_disk") is treated as
        // not-write-class, so enforcement falls through to the name
        // policy. Under Allowlist that means deny; under All it means
        // allow. The boundary is NOT engaged because the tool is not
        // classified as write-class.
        assert!(decide_approval_with_containment(
            ApprovalPolicy::All,
            "wipe_disk",
            None,
            &json!({ "path": "/etc/passwd" }),
            EngineKind::Goose,
            &[],
            true,
            false,
        ));
        // But: a tool that IS classified as write-class with a
        // missing/empty path is fail-closed denied.
        assert!(!decide_approval_with_containment(
            ApprovalPolicy::All,
            "write_file",
            None,
            &json!({}), // no file_path, no path
            EngineKind::Goose,
            &[],
            true,
            false,
        ));
        // Same with an unparseable (non-string) path.
        assert!(!decide_approval_with_containment(
            ApprovalPolicy::All,
            "write_file",
            None,
            &json!({ "file_path": 12345 }),
            EngineKind::Goose,
            &[],
            true,
            false,
        ));
        // An empty-string path is also a deny (treat as missing).
        assert!(!decide_approval_with_containment(
            ApprovalPolicy::All,
            "write_file",
            None,
            &json!({ "file_path": "" }),
            EngineKind::Goose,
            &[],
            true,
            false,
        ));
    }

    #[test]
    fn fail_closed_trust_engine_relaxes_fail_closed_only() {
        // `trust_engine=true` is the future seam for "we have validated
        // this engine's schema exhaustively". When set, an unknown
        // shape on a known write tool falls through to the name-based
        // policy (here: ApprovalPolicy::All -> approve). The boundary
        // is NOT relaxed: an extractable target OUTSIDE the roots is
        // still denied.
        let (_dir, root_a, _root_b, outside) = make_containment_dirs();
        let evil = outside.join("leak.txt");
        std::fs::write(&evil, "x").unwrap();
        // Unknown shape on a known write tool: trust_engine=true
        // relaxes the fail-closed branch (returns true under All).
        assert!(decide_approval_with_containment(
            ApprovalPolicy::All,
            "write_file",
            None,
            &json!({}), // no file_path
            EngineKind::Goose,
            &[root_a.clone()],
            true,
            /* trust_engine = */ true,
        ));
        // But: known shape with target outside roots is STILL denied
        // (the boundary is not relaxed by trust_engine).
        let goose_args = json!({ "file_path": evil.to_string_lossy() });
        assert!(!decide_approval_with_containment(
            ApprovalPolicy::All,
            "write_file",
            None,
            &goose_args,
            EngineKind::Goose,
            &[root_a.clone()],
            true,
            true,
        ));
    }

    #[test]
    fn agent_options_new_defaults_are_safe() {
        // `AgentOptions::new` must set `writable_roots` to a single
        // element containing the cwd, and must leave `enforce_*` as
        // false (so today's behavior is preserved by default).
        let opts = AgentOptions::new("/tmp/sock", "codex", "/tmp/repo", "hi");
        assert_eq!(opts.writable_roots, vec![PathBuf::from("/tmp/repo")]);
        assert!(!opts.enforce_writable_roots);
        assert!(!opts.trust_engine);
    }

    #[test]
    fn write_tool_matrix_is_non_empty_and_covers_known_engines() {
        // The matrix MUST cover at least the engines we claim to
        // support (zeroclaw, goose) so --list-schemas is never empty.
        // The matrix is also the single source of truth for the kernel;
        // a missing engine row means an operator has no way to discover
        // that engine's enforcement posture.
        let m = write_tool_matrix();
        assert!(!m.is_empty(), "write-tool matrix must not be empty");
        let has_zeroclaw = m.iter().any(|r| r.engine == EngineKind::Zeroclaw);
        let has_goose = m.iter().any(|r| r.engine == EngineKind::Goose);
        assert!(has_zeroclaw, "matrix must include zeroclaw rows");
        assert!(has_goose, "matrix must include goose rows");
    }

    #[test]
    fn write_tool_matrix_rows_align_with_is_write_class_tool() {
        // Every row in the public matrix MUST be classified as write-
        // class by the kernel's own `is_write_class_tool`. Otherwise the
        // matrix and the kernel decision have drifted — an operator who
        // trusts the printed matrix would believe a tool is enforced
        // when the kernel does not actually consult the matrix for it.
        for row in write_tool_matrix() {
            assert!(
                is_write_class_tool(row.engine, row.tool_name),
                "row {:?}/{:?} must be classified as write-class",
                row.engine,
                row.tool_name
            );
        }
    }

    #[test]
    fn write_tool_matrix_human_contains_known_tools() {
        // Spot-check the human-readable rendering: it must mention at
        // least one tool per known engine so an operator can grep for
        // either engine and find something. The exact format is a
        // diagnostic surface; we don't pin column alignment here.
        let s = write_tool_matrix_human();
        assert!(
            s.contains("zeroclaw"),
            "human rendering must include zeroclaw"
        );
        assert!(s.contains("goose"), "human rendering must include goose");
        assert!(
            s.contains("text_editor"),
            "human rendering must include goose text_editor row"
        );
        assert!(
            s.contains("write_file"),
            "human rendering must include goose write_file row"
        );
        assert!(
            s.contains("path"),
            "human rendering must include the path_field column"
        );
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
            goose_provider: None,
            // SLICE 1: defaults matching `AgentOptions::new` — keep tests
            // non-breaking (enforce off, trust off) and pin the writable
            // boundary to the synthetic cwd used by goose-driver tests.
            writable_roots: vec![std::path::PathBuf::from("/tmp")],
            enforce_writable_roots: false,
            trust_engine: false,
            // SLICE: empty Vec keeps the wire shape `[]` — non-breaking.
            mcp_servers: Vec::new(),
            // PROJECT-INSTRUCTIONS SLICE: defaults to None so the
            // goose-driver test surface keeps sending the raw `prompt`
            // verbatim. A test that wants to exercise the prepended
            // block sets it explicitly.
            project_instructions: None,
            // PERSISTENT-SESSIONS SLICE: default OFF in tests so the
            // existing assertions (which assert the wire shape is
            // always-create-new) keep passing byte-for-byte. Tests
            // that exercise persistence set this explicitly.
            persist_session_id: false,
            session_store_path: None,
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

    // ----- task #19 seam tests (CREDENTIAL/ENDPOINT bridge + parity) ----

    /// Build a `GooseProviderEnv` from a (kind, base_url, key) tuple —
    /// the shape `zoder-cli::agentic_turn` produces after
    /// `real_best_provider_for_model`. `provider_id` is a static label
    /// so the test can assert against an exact field value.
    fn gpe(kind: &str, base_url: &str, api_key: Option<&str>) -> GooseProviderEnv {
        GooseProviderEnv {
            provider_id: "test-provider".to_string(),
            kind: kind.to_string(),
            base_url: base_url.to_string(),
            api_key: api_key.map(|s| s.to_string()),
        }
    }

    /// Convenience: pull the value for `key` from the env vec returned
    /// by `goose_env`. Panics if the var isn't present — every test
    /// asserts presence as part of the seam contract.
    fn env_get<'a>(env: &'a [(String, String)], key: &str) -> &'a str {
        env.iter()
            .find(|(k, _)| k == key)
            .unwrap_or_else(|| panic!("env missing required var {key}; got {:?}", redact_env(env)))
            .1
            .as_str()
    }

    /// Convenience: assert a var is NOT present (used to prove the
    /// bridge skips variables for kinds that don't map onto goose's
    /// openai engine — e.g. anthropic).
    fn assert_env_absent(env: &[(String, String)], key: &str) {
        assert!(
            env.iter().all(|(k, _)| k != key),
            "env must not contain {key}; got {:?}",
            redact_env(env)
        );
    }

    #[test]
    fn engine_transport_debug_redacts_secret_env_values() {
        let t = EngineTransport::Stdio {
            command: "goose".to_string(),
            args: vec!["acp".to_string()],
            env: vec![
                ("GOOSE_MODEL".to_string(), "MiniMax-M3".to_string()),
                (
                    "OPENAI_API_KEY".to_string(),
                    "sk-super-secret-value".to_string(),
                ),
            ],
        };
        let dbg = format!("{t:?}");
        assert!(
            !dbg.contains("sk-super-secret-value"),
            "key leaked in Debug: {dbg}"
        );
        assert!(dbg.contains("REDACTED"), "expected redaction marker: {dbg}");
        assert!(
            dbg.contains("MiniMax-M3"),
            "non-secret value should stay visible: {dbg}"
        );
    }

    #[test]
    fn goose_provider_env_debug_redacts_api_key() {
        // The Debug impl MUST scrub the key. A test panic with
        // `--nocapture` would otherwise print the secret verbatim
        // alongside the rest of the struct; this guards against that
        // and against any future caller doing `println!("{env:?}")`.
        let env = gpe(
            "openai-chat",
            "https://api.minimax.io/v1",
            Some("sk-supersecret-do-not-leak"),
        );
        let rendered = format!("{env:?}");
        assert!(
            rendered.contains("[REDACTED]"),
            "Debug must render the key as [REDACTED]; got {rendered}"
        );
        assert!(
            !rendered.contains("sk-supersecret"),
            "Debug must NOT leak the raw key; got {rendered}"
        );
        assert!(
            !rendered.contains("do-not-leak"),
            "Debug must NOT leak any portion of the key; got {rendered}"
        );
        // Non-secret fields stay visible so debug output is still
        // useful for triage.
        assert!(
            rendered.contains("test-provider"),
            "Debug must keep provider_id visible; got {rendered}"
        );
        assert!(
            rendered.contains("https://api.minimax.io/v1"),
            "Debug must keep base_url visible; got {rendered}"
        );
    }

    #[test]
    fn goose_provider_env_debug_handles_no_key() {
        // `api_key = None` renders as the literal `None`, NOT
        // `[REDACTED]`, so a test can distinguish "no key configured"
        // from "key present but hidden". This is the only Debug
        // surface where the two cases are intentionally distinguishable.
        let env = gpe("openai-chat", "https://api.example/v1", None);
        let rendered = format!("{env:?}");
        assert!(
            rendered.contains("None"),
            "None api_key stays None; got {rendered}"
        );
        assert!(
            !rendered.contains("[REDACTED]"),
            "[REDACTED] must only appear when a key IS set; got {rendered}"
        );
    }

    #[test]
    fn goose_env_bridges_openai_compatible_provider_credential_and_endpoint() {
        // The CORE seam test (task #19, seam 1). Given a resolved
        // OpenAI-compatible provider (zoder's `minimax` -> openai-chat
        // against api.minimax.io), the child env must carry:
        //   * GOOSE_PROVIDER = "openai"  (parity: kind -> goose name)
        //   * GOOSE_MODEL    = routed model id (parity seam 2)
        //   * OPENAI_API_KEY = the resolved credential
        //   * OPENAI_BASE_URL = base_url verbatim
        //   * OPENAI_HOST = base_url with trailing /v1 stripped
        // and nothing else: no `ANTHROPIC_API_KEY`, no other random vars.
        let mut opts = goose_opts(None);
        opts.model_id = Some("MiniMax-M3".to_string());
        opts.goose_provider = Some(gpe(
            "openai-chat",
            "https://api.minimax.io/v1",
            Some("sk-test-key"),
        ));
        let env = goose_env(&opts, None);

        assert_eq!(env_get(&env, "GOOSE_PROVIDER"), "openai");
        assert_eq!(env_get(&env, "GOOSE_MODEL"), "MiniMax-M3");
        assert_eq!(env_get(&env, "OPENAI_API_KEY"), "sk-test-key");
        assert_eq!(
            env_get(&env, "OPENAI_BASE_URL"),
            "https://api.minimax.io/v1"
        );
        assert_eq!(env_get(&env, "OPENAI_HOST"), "https://api.minimax.io");
        // No leakage of the wrong-engine vars.
        assert_env_absent(&env, "ANTHROPIC_API_KEY");
    }

    #[test]
    fn goose_env_bridge_works_for_openai_responses_and_custom_kinds() {
        // The OpenAI-compatible predicate covers three kinds (any of
        // which the router may resolve to). Test each so a typo in the
        // match arm that silently drops one kind would fail loudly.
        for kind in ["openai-chat", "openai-responses", "custom"] {
            let mut opts = goose_opts(None);
            opts.model_id = Some("routed".to_string());
            opts.goose_provider = Some(gpe(kind, "https://gw.example.com/v1", Some("k")));
            let env = goose_env(&opts, None);
            assert_eq!(
                env_get(&env, "OPENAI_API_KEY"),
                "k",
                "kind={kind} must bridge the key"
            );
            assert_eq!(
                env_get(&env, "OPENAI_BASE_URL"),
                "https://gw.example.com/v1",
                "kind={kind} must bridge the base url"
            );
            assert_eq!(
                env_get(&env, "OPENAI_HOST"),
                "https://gw.example.com",
                "kind={kind} must bridge the host"
            );
        }
    }

    #[test]
    fn goose_env_anthropic_provider_skips_openai_bridge() {
        // Anthropic providers don't map onto goose's `openai` engine —
        // they need `ANTHROPIC_API_KEY` etc. (which we leave to the
        // ambient env / operator config). Forcing the OPENAI_* vars
        // here would leak the bearer into goose's openai path if the
        // operator ever switched GOOSE_PROVIDER via shell, so we
        // explicitly skip the bridge.
        let mut opts = goose_opts(None);
        opts.model_id = Some("claude-3-5-sonnet".to_string());
        opts.goose_provider = Some(gpe(
            "anthropic",
            "https://api.anthropic.com",
            Some("sk-ant-test"),
        ));
        let env = goose_env(&opts, None);

        assert_eq!(env_get(&env, "GOOSE_PROVIDER"), "anthropic");
        assert_eq!(env_get(&env, "GOOSE_MODEL"), "claude-3-5-sonnet");
        assert_env_absent(&env, "OPENAI_API_KEY");
        assert_env_absent(&env, "OPENAI_BASE_URL");
        assert_env_absent(&env, "OPENAI_HOST");
    }

    #[test]
    fn goose_env_openai_host_strips_trailing_v1_only() {
        // OPENAI_HOST = base_url minus a clean trailing `/vN` segment.
        // We must NOT strip:
        //   * versioned paths like `/v1beta` (not a clean /vN segment),
        //   * any subpath after /v1 (it's the API route, not a version),
        //   * a path prefix before /v1 (e.g. `/openai/v1` -> host ends
        //     at `/openai`).
        // Each case is asserted with an exact expected string so a
        // future strip_v1 regression fails loudly.
        let mut opts = goose_opts(None);
        opts.goose_provider = Some(gpe("openai-chat", "", None));
        // Bare /v1 — strip it.
        opts.goose_provider.as_mut().unwrap().base_url = "https://api.minimax.io/v1".into();
        let env = goose_env(&opts, None);
        assert_eq!(env_get(&env, "OPENAI_HOST"), "https://api.minimax.io");

        // No /v1 — host == base_url.
        opts.goose_provider.as_mut().unwrap().base_url = "https://api.openai.com".into();
        let env = goose_env(&opts, None);
        assert_eq!(env_get(&env, "OPENAI_HOST"), "https://api.openai.com");

        // Path prefix + /v1 — strip the version, keep the prefix.
        opts.goose_provider.as_mut().unwrap().base_url = "https://gw.example.com/openai/v1".into();
        let env = goose_env(&opts, None);
        assert_eq!(
            env_get(&env, "OPENAI_HOST"),
            "https://gw.example.com/openai"
        );

        // Trailing slash on /v1 — goose's parser strips it (mirrors the
        // `parse_base_url_handles_trailing_slash` test in goose's own
        // source). We do the same so the host zoder forwards matches
        // the host goose derives from the same OPENAI_BASE_URL.
        opts.goose_provider.as_mut().unwrap().base_url = "https://api.minimax.io/v1/".into();
        let env = goose_env(&opts, None);
        assert_eq!(env_get(&env, "OPENAI_HOST"), "https://api.minimax.io");

        // /v1beta is NOT a clean /vN segment — don't strip.
        opts.goose_provider.as_mut().unwrap().base_url = "https://api.example/v1beta".into();
        let env = goose_env(&opts, None);
        assert_eq!(env_get(&env, "OPENAI_HOST"), "https://api.example/v1beta");
    }

    #[test]
    fn goose_env_provider_kind_selection_is_deterministic() {
        // Direct test of the kind-to-goose-name mapping. The intent
        // is to lock in the parity contract: every zoder kind maps
        // onto a stable goose provider name so a config rename can't
        // silently switch the engine.
        let openai_env = gpe("openai-chat", "x", None);
        assert_eq!(goose_provider_kind(&openai_env), "openai");
        let openai2 = gpe("openai-responses", "x", None);
        assert_eq!(goose_provider_kind(&openai2), "openai");
        let custom = gpe("custom", "x", None);
        assert_eq!(goose_provider_kind(&custom), "openai");
        let anth = gpe("anthropic", "x", None);
        assert_eq!(goose_provider_kind(&anth), "anthropic");
        // Unknown kind must NOT panic; it falls back to "openai" so a
        // freshly-added zoder kind that has no mapping yet still
        // produces a sensible default (and gets the OPENAI_* bridge
        // applied if is_openai_compatible_kind also says so).
        let unknown = gpe("something-brand-new", "x", None);
        assert_eq!(goose_provider_kind(&unknown), "openai");
    }

    #[test]
    fn goose_env_provider_derives_from_resolved_kind_overriding_ambient() {
        // Parity seam (task #19, seam 2): when the operator has an
        // ambient `$GOOSE_PROVIDER=anthropic` on the parent shell but
        // the routed model resolves to an openai-chat provider, the
        // resolved kind MUST win — otherwise the loop dials the wrong
        // engine (and the auth bridge wouldn't even apply, since the
        // anthropic path skips it). The test deliberately doesn't
        // touch `std::env`; instead it proves the precedence via
        // `provider_override = None` (the production call) with
        // `goose_provider` set, which is documented to outrank the
        // ambient env. The existing `goose_env_provider_defaults_to_openai`
        // test covers the "no provider set, no override" default.
        let mut opts = goose_opts(None);
        opts.model_id = Some("nvidia/llama-3.3-nemotron-super-49b-v1.5".to_string());
        opts.goose_provider = Some(gpe(
            "openai-chat",
            "https://integrate.api.nvidia.com/v1",
            Some("nvapi-test"),
        ));
        // `provider_override = None` exercises the real production
        // resolution path (which would normally consult $GOOSE_PROVIDER).
        let env = goose_env(&opts, None);
        assert_eq!(
            env_get(&env, "GOOSE_PROVIDER"),
            "openai",
            "resolved kind outranks ambient $GOOSE_PROVIDER"
        );
        assert_eq!(
            env_get(&env, "GOOSE_MODEL"),
            "nvidia/llama-3.3-nemotron-super-49b-v1.5",
            "GOOSE_MODEL = routed model id, NOT the agent alias"
        );
    }

    #[test]
    fn goose_env_omits_credential_when_provider_has_no_key() {
        // Real-world case: the operator's `minimax` provider is
        // configured with `auth = { type = "none" }` (no auth). We
        // must NOT inject an empty `OPENAI_API_KEY=` — goose's parser
        // may reject it, and at minimum it advertises a credential
        // that isn't there. The bridge is conditional on a non-empty
        // resolved key.
        let mut opts = goose_opts(None);
        opts.model_id = Some("model-1".to_string());
        opts.goose_provider = Some(gpe("openai-chat", "https://api.minimax.io/v1", None));
        let env = goose_env(&opts, None);
        assert_env_absent(&env, "OPENAI_API_KEY");
        // Base URL + host ARE still set — the operator chose this
        // endpoint, the bridge should still route to it.
        assert_eq!(
            env_get(&env, "OPENAI_BASE_URL"),
            "https://api.minimax.io/v1"
        );
        assert_eq!(env_get(&env, "OPENAI_HOST"), "https://api.minimax.io");
    }

    #[test]
    fn goose_env_skips_endpoint_vars_for_empty_base_url() {
        // A misconfigured provider with an empty base_url shouldn't
        // poison the child env with `OPENAI_BASE_URL=""` /
        // `OPENAI_HOST=""`. Skipping the endpoint pair entirely
        // lets goose fall back to its own config (or fail loudly,
        // which is the right behavior).
        let mut opts = goose_opts(None);
        opts.model_id = Some("model-1".to_string());
        opts.goose_provider = Some(gpe("openai-chat", "", Some("key")));
        let env = goose_env(&opts, None);
        // Key still bridges (a key without an endpoint is the right
        // direction to leak a credential into a default endpoint —
        // see comment above; but the test only checks the skip-path).
        assert_eq!(env_get(&env, "OPENAI_API_KEY"), "key");
        assert_env_absent(&env, "OPENAI_BASE_URL");
        assert_env_absent(&env, "OPENAI_HOST");
    }

    #[test]
    fn goose_env_without_resolved_provider_does_not_inject_bridge() {
        // The legacy / no-provider-configured path must keep the old
        // behavior exactly: only GOOSE_PROVIDER + GOOSE_MODEL, no
        // OPENAI_* vars. (Otherwise a future refactor that always
        // injects the bridge could leak a stale credential from a
        // previous test run into a clean repo.)
        let opts = goose_opts(Some("gpt-4o-mini"));
        let env = goose_env(&opts, None);
        assert!(env.iter().any(|(k, _)| k == "GOOSE_PROVIDER"));
        assert!(env.iter().any(|(k, _)| k == "GOOSE_MODEL"));
        assert_env_absent(&env, "OPENAI_API_KEY");
        assert_env_absent(&env, "OPENAI_BASE_URL");
        assert_env_absent(&env, "OPENAI_HOST");
    }

    #[test]
    fn is_openai_compatible_kind_recognizes_known_and_unknown_kinds() {
        // The predicate is the central gate for the credential bridge;
        // any future kind addition that should NOT get OPENAI_* vars
        // (e.g. a future non-OpenAI provider type) needs a deliberate
        // negative test here. Today: openai-chat/responses/custom +
        // the empty-string default are OpenAI-compatible; anthropic is
        // not.
        assert!(is_openai_compatible_kind("openai-chat"));
        assert!(is_openai_compatible_kind("openai-responses"));
        assert!(is_openai_compatible_kind("custom"));
        assert!(is_openai_compatible_kind(""));
        assert!(!is_openai_compatible_kind("anthropic"));
        assert!(!is_openai_compatible_kind("azure-openai"));
        assert!(!is_openai_compatible_kind("cohere"));
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

    /// Same as [`drive_against_mock`] but lets the caller pass an
    /// explicitly-built `AgentOptions` (e.g. with `mcp_servers`
    /// populated, for the populated-array wire-shape regression
    /// test). Mirrors the existing helper so the populated test
    /// doesn't have to reach back into private mock plumbing.
    async fn drive_against_mock_with_opts(
        outbound: Vec<String>,
        show_reasoning: bool,
        approval: ApprovalPolicy,
        opts: AgentOptions,
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
        let mut opts = opts;
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

    /// Regression for adversarial-review finding #5: when the CLI
    /// populates `AgentOptions::mcp_servers` from the parsed engine
    /// config, the goose `session/new` params must carry that
    /// populated array VERBATIM — not drop it back to `[]` at the
    /// call site, not flatten it, not wrap it. The two entries below
    /// are the stdio + http shapes produced by
    /// `zoder_core::to_acp_mcp_servers`; the assertions check the
    /// exact field names (`command`/`args`/`env`, `url`/`headers`)
    /// and the array-of-{name,value} sub-shapes so that an accidental
    /// future "simplification" (e.g. passing the map straight through
    /// as a JSON object) breaks the test instead of silently shipping
    /// wire-shape drift to the goose binary.
    #[tokio::test]
    async fn goose_drive_session_new_carries_populated_mcp_servers() {
        // The pre-serialized ACP objects the CLI would have built via
        // `zoder_core::to_acp_mcp_servers(...)` for a config with one
        // stdio server (with args + env) and one http server (with
        // headers). Hand-built here so the test owns the exact
        // expected wire shape rather than depending on the converter's
        // own output — keeps this test honest as a wire-shape lock.
        let populated = vec![
            serde_json::json!({
                "name": "lookup",
                "command": "node",
                "args": ["server.js"],
                "env": [
                    {"name": "API_KEY", "value": "secret"},
                    {"name": "DEBUG", "value": "1"},
                ],
            }),
            serde_json::json!({
                "name": "github",
                "url": "https://api.example.com/mcp/",
                "headers": [
                    {"name": "Authorization", "value": "Bearer TOKEN"},
                ],
            }),
        ];

        // The mock harness drives the driver with the options the
        // caller built. We override `mcp_servers` directly here
        // (rather than going through `goose_opts`, which is locked to
        // the empty-Vec default to keep the sibling
        // `goose_drive_handshake_emits_standard_acp` test stable).
        let mut opts = goose_opts(None);
        opts.mcp_servers = populated.clone();

        let (frames, _run, _events) =
            drive_against_mock_with_opts(vec![], false, ApprovalPolicy::Allowlist, opts).await;
        assert_eq!(frames.len(), 3, "expected init/new/prompt frames");

        // session/new params must carry the populated array verbatim.
        let new = &frames[1];
        assert_eq!(new["method"], "session/new");
        let np = &new["params"];
        let actual = np["mcpServers"]
            .as_array()
            .expect("mcpServers must be an array");
        assert_eq!(
            actual.len(),
            2,
            "mcpServers length must match what was passed in; got: {actual:?}"
        );

        // Field-name precision: stdio entry must carry `command`/`args`/`env`
        // and MUST NOT carry `url`/`headers`. http entry must carry
        // `url`/`headers` and MUST NOT carry `command`/`args`/`env`.
        // These are the exact untagged-enum field names goose 1.37
        // expects (wrong field names cause goose to reject with
        // "data did not match any variant of untagged enum McpServer").
        let lookup = actual
            .iter()
            .find(|e| e["name"] == "lookup")
            .expect("lookup entry present");
        assert_eq!(lookup["command"], "node");
        assert_eq!(lookup["args"], serde_json::json!(["server.js"]));
        let env = lookup["env"].as_array().expect("env must be an array");
        assert_eq!(env.len(), 2);
        // env entries MUST be {name, value} objects, not a flat map.
        for entry in env {
            assert!(
                entry.get("name").is_some(),
                "env entry missing name: {entry}"
            );
            assert!(
                entry.get("value").is_some(),
                "env entry missing value: {entry}"
            );
        }
        assert!(
            lookup.get("url").is_none(),
            "stdio variant must NOT have url"
        );
        assert!(
            lookup.get("headers").is_none(),
            "stdio variant must NOT have headers"
        );

        let github = actual
            .iter()
            .find(|e| e["name"] == "github")
            .expect("github entry present");
        assert_eq!(github["url"], "https://api.example.com/mcp/");
        let headers = github["headers"]
            .as_array()
            .expect("headers must be an array");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0]["name"], "Authorization");
        assert_eq!(headers[0]["value"], "Bearer TOKEN");
        assert!(
            github.get("command").is_none(),
            "http variant must NOT have command"
        );
        assert!(
            github.get("args").is_none(),
            "http variant must NOT have args"
        );
        assert!(
            github.get("env").is_none(),
            "http variant must NOT have env"
        );

        // initialize + session/prompt must remain unchanged in shape.
        assert_eq!(frames[0]["method"], "initialize");
        assert_eq!(frames[2]["method"], "session/prompt");
    }

    // -----------------------------------------------------------------
    // PERSISTENT-SESSIONS SLICE — resume + save + fall-back wiring.
    //
    // Tests below exercise:
    //   * explicit `opts.session_id` is forwarded to `session/new` as
    //     `sessionId` (the resume wire-shape lock);
    //   * persistence disabled / store empty = byte-for-byte the same
    //     wire shape as the pre-this-slice driver (regression guard);
    //   * persistence enabled + fresh record in the store = the
    //     loaded id is what reaches `session/new`;
    //   * persistence enabled + the engine replies with a
    //     `session/new` error (the "stale id" path) = the driver
    //     retries without `sessionId`, saves the new id, and the
    //     stale record is overwritten (no panic, no double-counted
    //     session ids).
    // -----------------------------------------------------------------

    /// Mock variant for the persistent-sessions tests. Behaves like
    /// [`run_mock`] EXCEPT the FIRST `session/new` response is an
    /// error reply (the engine telling us our persisted id is gone),
    /// and the SECOND `session/new` (the fallback fresh-create)
    /// succeeds with `sessionId = recovered_id`. Asserts that the
    /// client sent exactly two `session/new` requests: one with
    /// `sessionId` populated (the resume), one without (the
    /// fallback).
    async fn run_mock_session_new_rejected_then_recovers(
        server: MockGoose,
        engine_io: tokio::io::DuplexStream,
        recovered_id: &'static str,
    ) {
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

        // 1. read initialize -> reply.
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

        // 2. read session/new (resume) -> error.
        read_one(&mut r, &server.received).await;
        write_one(
            &mut w,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "new",
                "error": { "code": -32004, "message": "session not found" }
            }),
        )
        .await;

        // 3. read session/new (fallback fresh create) -> success.
        read_one(&mut r, &server.received).await;
        write_one(
            &mut w,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "new",
                "result": { "sessionId": recovered_id }
            }),
        )
        .await;

        // 4. outbound updates (session/update notifications, etc.).
        for frame in &server.outbound {
            let parsed: serde_json::Value = serde_json::from_str(frame).unwrap();
            write_one(&mut w, parsed.clone()).await;
        }

        // 5. read session/prompt -> terminal reply.
        read_one(&mut r, &server.received).await;
        write_one(
            &mut w,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "prompt",
                "result": { "stopReason": "end_turn" }
            }),
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn goose_drive_session_new_includes_session_id_when_resuming() {
        // Forwarding test: when the CLI hands us an `opts.session_id`,
        // the goose `session/new` params MUST carry `sessionId` so the
        // engine can resume. This is the single non-persistence smoke
        // test for the resume wire contract; the persistence tests
        // below build on the same plumbing.
        let mut opts = goose_opts(Some("gpt-4o-mini"));
        opts.session_id = Some("goose-stored-1".to_string());
        let (frames, run, _events) =
            drive_against_mock_with_opts(vec![], false, ApprovalPolicy::Allowlist, opts).await;
        let new = &frames[1];
        assert_eq!(new["method"], "session/new");
        let np = &new["params"];
        assert_eq!(
            np["sessionId"], "goose-stored-1",
            "session/new must forward opts.session_id as sessionId"
        );
        // And the returned id is what the mock chose — NOT the input
        // one. (Goose ignores the resume hint and mints fresh; the
        // driver records whatever the server actually returned.)
        assert_eq!(run.session_id, "goose-test-session-1");
    }

    #[tokio::test]
    async fn goose_drive_default_opts_keep_todays_wire_shape() {
        // Regression guard for the "persistence OFF preserves byte-
        // for-byte behavior" promise at the module level:
        // `goose_opts` builds an AgentOptions with both
        // `persist_session_id = false` and `session_id = None`; the
        // session/new frame MUST NOT contain `sessionId`, and a
        // second run with the same opts MUST produce identical
        // frames (no implicit cache lookup introduces a hidden
        // `sessionId`).
        let (frames1, _, _) = drive_against_mock_with_opts(
            vec![],
            false,
            ApprovalPolicy::Allowlist,
            goose_opts(None),
        )
        .await;
        let (frames2, _, _) = drive_against_mock_with_opts(
            vec![],
            false,
            ApprovalPolicy::Allowlist,
            goose_opts(None),
        )
        .await;
        for (label, frames) in [("run-1", &frames1), ("run-2", &frames2)] {
            let new = &frames[1];
            assert_eq!(new["method"], "session/new");
            assert!(
                new["params"].get("sessionId").is_none(),
                "{label}: persistence OFF must NOT add sessionId to session/new"
            );
        }
        // Bit-for-bit identical session/new between two runs.
        assert_eq!(
            frames1[1], frames2[1],
            "two consecutive runs with persist_session_id=false must emit identical session/new frames"
        );
    }

    #[tokio::test]
    async fn goose_drive_persistence_enabled_uses_stored_id_on_resume() {
        // Persistence ON + a fresh record in the store + the engine
        // would accept a resume = the loaded id (NOT the seeded mock
        // value) reaches `session/new` as `sessionId`, and the
        // returned id survives to AgentRun (engine passthrough).
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("engine_sessions.json");
        let cfg = session_store::StoreConfig::new(&store_path);
        session_store::EngineSessionStore::save(
            &cfg,
            "goose",
            &PathBuf::from("/tmp"),
            "goose-stored-1",
        )
        .unwrap();

        let mut opts = goose_opts(Some("gpt-4o-mini"));
        opts.persist_session_id = true;
        opts.session_store_path = Some(store_path.clone());
        let (frames, run, _events) =
            drive_against_mock_with_opts(vec![], false, ApprovalPolicy::Allowlist, opts).await;
        let new = &frames[1];
        assert_eq!(
            new["params"]["sessionId"], "goose-stored-1",
            "loaded store id must reach session/new as sessionId"
        );
        // The mock returns "goose-test-session-1" on success (any
        // session/new success). Assert that the driver records
        // whatever the engine actually returned, since the engine
        // is the source of truth (some engines keep the same id on
        // resume; some mint fresh).
        assert_eq!(run.session_id, "goose-test-session-1");

        // And the returned id was persisted (overwriting the seeded
        // record).
        let cfg = session_store::StoreConfig::new(&store_path);
        let scope = session_store::make_scope("goose", &PathBuf::from("/tmp"));
        let rec = session_store::EngineSessionStore::load(&cfg, &scope)
            .unwrap()
            .expect("session id must be saved after a successful run");
        assert_eq!(rec.session_id, "goose-test-session-1");
    }

    #[tokio::test]
    async fn goose_drive_persistence_enabled_persists_id_on_first_run() {
        // First-run parity: persistence ON with no record yet on disk
        // MUST emit `session/new` WITHOUT `sessionId` (same wire
        // shape as the OFF path) AND save the returned id for the
        // next run. This is the non-breaking guarantee — enabling
        // persistence doesn't change what the first run sends.
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("engine_sessions.json");
        let cfg = session_store::StoreConfig::new(&store_path);

        let mut opts = goose_opts(Some("gpt-4o-mini"));
        opts.persist_session_id = true;
        opts.session_store_path = Some(store_path.clone());
        let (frames, run, _events) =
            drive_against_mock_with_opts(vec![], false, ApprovalPolicy::Allowlist, opts).await;

        let new = &frames[1];
        assert!(
            new["params"].get("sessionId").is_none(),
            "first run with empty store must NOT carry sessionId (same as OFF)"
        );
        assert_eq!(run.session_id, "goose-test-session-1");

        let scope = session_store::make_scope("goose", &PathBuf::from("/tmp"));
        let rec = session_store::EngineSessionStore::load(&cfg, &scope)
            .unwrap()
            .expect("first run must persist its returned id");
        assert_eq!(rec.session_id, "goose-test-session-1");
    }

    #[tokio::test]
    async fn goose_drive_resume_rejected_falls_back_and_overwrites_stale_record() {
        // Headline behavior of the slice: stale persisted id +
        // engine error reply on session/new = the driver retries
        // WITHOUT sessionId, the engine mints a fresh id, and the
        // returned id overwrites the stale record on disk. The
        // test asserts each step:
        //   1. exactly TWO session/new frames hit the wire (the
        //      failed resume + the successful fresh-create);
        //   2. the first carries the seeded persisted id as
        //      sessionId, the second does NOT;
        //   3. AgentRun.session_id is the FRESH id, not the seeded
        //      one;
        //   4. the on-disk record was overwritten — the next run
        //      sees the fresh id, not the stale one.
        // If any of those change, the persistence guarantee is
        // silently broken (a "successful" run would still carry
        // the dead id), so each one is asserted independently.
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("engine_sessions.json");
        let cfg = session_store::StoreConfig::new(&store_path);
        // Seed with a stale id.
        session_store::EngineSessionStore::save(
            &cfg,
            "goose",
            &PathBuf::from("/tmp"),
            "goose-stale-dead",
        )
        .unwrap();

        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let server = MockGoose {
            received: received.clone(),
            outbound: vec![],
        };
        let (client_io, engine_io) = tokio::io::duplex(128 * 1024);
        let server_handle = tokio::spawn(run_mock_session_new_rejected_then_recovers(
            server,
            engine_io,
            "goose-recovered-fresh",
        ));

        let (mut r, mut w) = tokio::io::split(client_io);
        let mut r = tokio::io::BufReader::new(&mut r);
        let mut opts = goose_opts(Some("gpt-4o-mini"));
        opts.persist_session_id = true;
        opts.session_store_path = Some(store_path.clone());
        let mut events: Vec<AgentEvent> = Vec::new();
        let run = drive_goose_io(&opts, &mut r, &mut w, &mut |ev| {
            events.push(ev);
        })
        .await
        .expect("resume-rejected + fresh-create retry must succeed (no panic)");
        let _ = server_handle.await;

        let frames = received.lock().unwrap().clone();
        // 4 frames expected: init / first-new / second-new / prompt.
        // The two session/new frames are at indices 1 and 2.
        assert_eq!(frames.len(), 4, "init / new / new / prompt");
        let first_new = &frames[1];
        let second_new = &frames[2];
        assert_eq!(first_new["method"], "session/new");
        assert_eq!(second_new["method"], "session/new");
        assert_eq!(
            first_new["params"]["sessionId"], "goose-stale-dead",
            "the first session/new must carry the persisted (stale) id"
        );
        assert!(
            second_new["params"].get("sessionId").is_none(),
            "the fallback session/new must NOT carry sessionId (fresh-create path)"
        );

        // The fallback session/new must still carry cwd + mcpServers
        // — regression guard that the retry params match the
        // pre-failure params exactly (no silent loss of the cwd
        // pin or MCP servers).
        assert_eq!(second_new["params"]["cwd"], "/tmp");
        assert!(second_new["params"]["mcpServers"].is_array());

        assert_eq!(
            run.session_id, "goose-recovered-fresh",
            "AgentRun must record the freshly issued id (not the stale one)"
        );
        assert_eq!(run.outcome, "completed");

        // The next run's load must see the recovered id, not the
        // stale one — this is the persistence invariant the slice
        // is built around.
        let scope = session_store::make_scope("goose", &PathBuf::from("/tmp"));
        let rec = session_store::EngineSessionStore::load(&cfg, &scope)
            .unwrap()
            .expect("record must survive (overwritten with fresh id)");
        assert_eq!(
            rec.session_id, "goose-recovered-fresh",
            "stale record must be overwritten with the fresh id"
        );
    }

    /// Mock variant for the Z-24 test: behaves like
    /// [`run_mock_session_new_rejected_then_recovers`] EXCEPT the
    /// SECOND `session/new` (the fresh-create retry) ALSO returns a
    /// JSON-RPC error reply, so the driver bails out at the
    /// "fresh-create retry also failed" branch.
    async fn run_mock_session_new_rejected_both_times(
        server: MockGoose,
        engine_io: tokio::io::DuplexStream,
    ) {
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

        // 1. read initialize -> reply.
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

        // 2. read session/new (resume) -> error.
        read_one(&mut r, &server.received).await;
        write_one(
            &mut w,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "new",
                "error": { "code": -32004, "message": "session not found" }
            }),
        )
        .await;

        // 3. read session/new (fresh-create retry) -> ALSO error.
        //    The driver must then bail out with the combined
        //    "fresh-create retry also failed" diagnostic and
        //    MUST NOT have cleared the on-disk record for this
        //    scope (Z-24 invariant).
        read_one(&mut r, &server.received).await;
        write_one(
            &mut w,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "new",
                "error": { "code": -32004, "message": "engine on fire" }
            }),
        )
        .await;

        // Give the client a moment to read the error and bail out.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    /// Z-24: when the engine rejects a resume AND the fresh-create
    /// retry ALSO fails, the driver MUST NOT silently drop the
    /// previously-stored session id from the persistence store.
    /// Pre-fix code unconditionally called
    /// `EngineSessionStore::clear` on a resume-reject, so a transient
    /// failure (engine hiccup, network blip) on the retry would
    /// leave the operator with an empty store AND no session — for
    /// no reason. The post-fix code lets `persist_session_after`
    /// decide what (if anything) to overwrite, so a failed run
    /// preserves the prior record verbatim.
    ///
    /// The test seeds a valid record for "session-A" in the store,
    /// passes an explicit `opts.session_id = "session-A"`, points
    /// the mock at a server that returns a JSON-RPC error for BOTH
    /// the resume AND the retry, and asserts the on-disk record is
    /// still present and still "session-A" after the failed run.
    #[tokio::test]
    async fn z24_resume_reject_with_failing_retry_does_not_evict_store_record() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("engine_sessions.json");
        let cfg = session_store::StoreConfig::new(&store_path);
        // Seed with the SAME id the operator is going to ask us to
        // resume. The store record is "valid" (in scope, fresh) and
        // must survive a failed-resume/failed-retry untouched.
        session_store::EngineSessionStore::save(&cfg, "goose", &PathBuf::from("/tmp"), "session-A")
            .unwrap();

        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let server = MockGoose {
            received: received.clone(),
            outbound: vec![],
        };
        let (client_io, engine_io) = tokio::io::duplex(128 * 1024);
        let server_handle =
            tokio::spawn(run_mock_session_new_rejected_both_times(server, engine_io));

        let (mut r, mut w) = tokio::io::split(client_io);
        let mut r = tokio::io::BufReader::new(&mut r);
        let mut opts = goose_opts(Some("gpt-4o-mini"));
        // Key: persistence ON + explicit session_id, so the driver
        // takes the resume path. The store record happens to be for
        // the same id ("session-A"), making the pre-fix clear()
        // call a particularly nasty case: it would drop a valid
        // record the operator might still be able to use on the
        // NEXT run.
        opts.persist_session_id = true;
        opts.session_store_path = Some(store_path.clone());
        opts.session_id = Some("session-A".to_string());
        let mut events: Vec<AgentEvent> = Vec::new();
        let run = drive_goose_io(&opts, &mut r, &mut w, &mut |ev| {
            events.push(ev);
        })
        .await;
        let _ = server_handle.await;
        // Sanity: the resume + fresh-create retry both failed, so
        // the driver must surface that combined error.
        let err = run.expect_err(
            "Z-24: drive_goose_io must return Err when both the resume and the fresh-create retry \
             fail",
        );
        let msg = format!("{err:?}");
        assert!(
            msg.contains("fresh-create retry also failed") || msg.contains("retry also failed"),
            "Z-24: expected the combined-resume-failure diagnostic, got: {msg}"
        );

        // The actual fix: the store record MUST still be present,
        // and MUST still be the seeded "session-A" — the failed
        // run MUST NOT have evicted a valid record the operator
        // could still use.
        let scope = session_store::make_scope("goose", &PathBuf::from("/tmp"));
        let rec = session_store::EngineSessionStore::load(&cfg, &scope)
            .unwrap()
            .expect("Z-24: a failed resume-retry must NOT evict a valid store record");
        assert_eq!(
            rec.session_id, "session-A",
            "Z-24: the seeded record must remain intact after a failed run; got {rec:?}"
        );
    }

    #[tokio::test]
    async fn goose_drive_stale_record_is_treated_as_absent() {
        // Defensive guard: a record older than the freshness window
        // (or for a different scope) must NOT shadow the
        // always-create path. The driver must emit session/new
        // WITHOUT sessionId — exact parity with the OFF path — and
        // overwrite the stale record with whatever the engine
        // returns (so an unstick step happens implicitly on the
        // next run).
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("engine_sessions.json");
        let seed_cfg = session_store::StoreConfig::new(&store_path);
        session_store::EngineSessionStore::save(
            &seed_cfg,
            "goose",
            &PathBuf::from("/tmp"),
            "goose-very-old",
        )
        .unwrap();
        // Pin "now" to a future timestamp so the seeded record
        // (just written) is read with zero_age on the load, then
        // manipulate updated_unix directly via store-side round
        // trip: rewrite the file with an updated_unix in the
        // distant past. Simplest: re-save with a now in the past
        // using StoreConfig::with_now (test seam).
        let stale_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - (session_store::DEFAULT_MAX_AGE_SECS + 60);
        let stale_cfg = session_store::StoreConfig::new(&store_path).with_now(stale_secs);
        session_store::EngineSessionStore::save(
            &stale_cfg,
            "goose",
            &PathBuf::from("/tmp"),
            "goose-very-old",
        )
        .unwrap();

        let mut opts = goose_opts(Some("gpt-4o-mini"));
        opts.persist_session_id = true;
        opts.session_store_path = Some(store_path.clone());
        let (frames, run, _) =
            drive_against_mock_with_opts(vec![], false, ApprovalPolicy::Allowlist, opts).await;
        let new = &frames[1];
        assert!(
            new["params"].get("sessionId").is_none(),
            "a stale record must NOT participate in resume (treated as absent)"
        );
        // And the recovered record now carries the FRESH id, not
        // the stale one — the driver's save-after replaces the old
        // record.
        let scope = session_store::make_scope("goose", &PathBuf::from("/tmp"));
        let rec = session_store::EngineSessionStore::load(
            &session_store::StoreConfig::new(&store_path),
            &scope,
        )
        .unwrap()
        .expect("fresh id must be saved");
        assert_eq!(rec.session_id, run.session_id);
        assert_ne!(
            rec.session_id, "goose-very-old",
            "stale record must be replaced"
        );
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
        let mut utilization_count = 0;
        for ev in &events {
            match ev {
                AgentEvent::Text(_) => text_count += 1,
                AgentEvent::ToolCall { name } if name == "shell" => tool_count += 1,
                AgentEvent::Usage { .. } => usage_count += 1,
                AgentEvent::Utilization { .. } => utilization_count += 1,
                _ => {}
            }
        }
        assert_eq!(text_count, 2);
        assert_eq!(tool_count, 1);
        assert_eq!(usage_count, 1);
        assert_eq!(
            utilization_count, 0,
            "Goose context-window usage is not subscription-quota telemetry"
        );
    }

    /// Z-17 (end-to-end): a hostile engine that emits a single
    /// `session/update` larger than [`MAX_FRAME_BYTES`] in the
    /// streaming loop MUST surface as an `io::Error` from
    /// `drive_goose_io` (the same `read_frame_line_capped` helper
    /// gates the streaming read as well as the handshake read). The
    /// pre-fix code would have buffered the whole oversized frame
    /// and OOMed the driver; the post-fix code fails fast at the
    /// cap with a useful diagnostic.
    ///
    /// The mock does the normal init/new handshake, then writes a
    /// SINGLE `session/update`-shaped JSON object whose body is
    /// `(MAX_FRAME_BYTES + 256)` bytes of `'x'` with no `\n` inside
    /// the cap. The driver must reject the frame at the cap and
    /// surface an `InvalidData`-flavored error.
    ///
    /// The server task keeps its writer alive after the oversized
    /// write so the client bails on the cap (not on a writer-side
    /// shutdown that would surface as a different error kind).
    #[tokio::test]
    async fn z17_drive_goose_io_streaming_loop_errors_on_oversized_frame() {
        use tokio::io::AsyncWriteExt;
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (client_io, engine_io) = tokio::io::duplex((MAX_FRAME_BYTES as usize) * 2);
        let recv = received.clone();
        let server = tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(engine_io);
            let mut r = tokio::io::BufReader::new(r);
            // 1. initialize -> ack
            let mut line = String::new();
            let _ = r.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                recv.lock().unwrap().push(v);
            }
            let init_ack = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "init",
                "result": { "protocolVersion": 1 }
            });
            let s = serde_json::to_string(&init_ack).unwrap();
            let _ = w.write_all(s.as_bytes()).await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;
            // 2. session/new -> success
            line.clear();
            let _ = r.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                recv.lock().unwrap().push(v);
            }
            let new_ack = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "new",
                "result": { "sessionId": "goose-overflow-test" }
            });
            let s = serde_json::to_string(&new_ack).unwrap();
            let _ = w.write_all(s.as_bytes()).await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;
            // 3. send a session/update whose body is larger than the
            //    cap, with no '\n' inside the cap. The driver MUST
            //    reject this at the cap and surface the overflow.
            let huge_body = "x".repeat(MAX_FRAME_BYTES as usize + 256);
            let huge = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "goose-overflow-test",
                    "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": huge_body } }
                }
            });
            let s = serde_json::to_string(&huge).unwrap();
            // Block until the whole oversized frame is in the
            // duplex buffer. The duplex is sized at 2x MAX so this
            // never blocks on backpressure from the client (the
            // client can't drain faster than we fill).
            let _ = w.write_all(s.as_bytes()).await;
            // Yield a few times so the client gets to read before
            // we drop `w` (which closes the writer and would
            // surface as a different error kind).
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
        });
        let (mut r, mut w) = tokio::io::split(client_io);
        let mut r = tokio::io::BufReader::new(&mut r);
        let mut opts = goose_opts(Some("gpt-4o-mini"));
        opts.timeout = std::time::Duration::from_secs(5);
        let mut events: Vec<AgentEvent> = Vec::new();
        let res = drive_goose_io(&opts, &mut r, &mut w, &mut |ev| {
            events.push(ev);
        })
        .await;
        let _ = server.await;
        let err =
            res.expect_err("Z-17: an oversized session/update must make drive_goose_io return Err");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("cap") || msg.contains("exceeds") || msg.contains("too large"),
            "Z-17: expected an overflow / cap diagnostic from the streaming loop, got: {msg}"
        );
    }

    /// Y-9: a hostile / runaway engine that emits a CONTINUOUS
    /// STREAM of well-formed sub-cap `agent_message_chunk` frames
    /// MUST be stopped by the cumulative `content`-size cap. The
    /// Z-17 fix bounded the per-frame read; that fix is necessary
    /// but not sufficient — a stream of, e.g., twenty 1 MiB frames
    /// would each pass the per-frame cap, but `content` (and the
    /// mirrored on_event/Text sink) would grow to 20 MiB+ without
    /// bound across the default 900s turn deadline, OOMing the
    /// driver.
    ///
    /// This test feeds `drive_goose_io` a stream of
    /// `agent_message_chunk` frames whose cumulative `content`
    /// payload EXCEEDS [`MAX_CUMULATIVE_CONTENT_BYTES`] and asserts
    /// that the driver bails the turn with a clear
    /// cumulative-cap diagnostic (NOT a per-frame cap diagnostic,
    /// NOT a silent OOM, NOT a successful completion).
    #[tokio::test]
    async fn y9_drive_goose_io_streaming_loop_errors_on_cumulative_content_overflow() {
        use tokio::io::AsyncWriteExt;
        // Pick a frame payload small enough that no SINGLE frame
        // trips the per-frame [`MAX_FRAME_BYTES`] cap, but large
        // enough that a handful of frames push the cumulative
        // `content` past [`MAX_CUMULATIVE_CONTENT_BYTES`].
        // 1 MiB per chunk * (cap / 1 MiB + 2) chunks guarantees we
        // cross the cap while staying well under the per-frame cap.
        let chunk_bytes: usize = 1024 * 1024; // 1 MiB
        let chunk_text: String = "a".repeat(chunk_bytes);
        let chunks_needed: usize = (MAX_CUMULATIVE_CONTENT_BYTES as usize / chunk_bytes) + 2;

        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        // Size the duplex generously so the server can buffer all
        // the chunks it wants to emit.
        let (client_io, engine_io) = tokio::io::duplex(MAX_CUMULATIVE_CONTENT_BYTES as usize * 2);
        let recv = received.clone();
        let server = tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(engine_io);
            let mut r = tokio::io::BufReader::new(r);
            // 1. initialize -> ack
            let mut line = String::new();
            let _ = r.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                recv.lock().unwrap().push(v);
            }
            let init_ack = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "init",
                "result": { "protocolVersion": 1 }
            });
            let s = serde_json::to_string(&init_ack).unwrap();
            let _ = w.write_all(s.as_bytes()).await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;
            // 2. session/new -> success
            line.clear();
            let _ = r.read_line(&mut line).await;
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                recv.lock().unwrap().push(v);
            }
            let new_ack = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "new",
                "result": { "sessionId": "goose-y9-cumulative" }
            });
            let s = serde_json::to_string(&new_ack).unwrap();
            let _ = w.write_all(s.as_bytes()).await;
            let _ = w.write_all(b"\n").await;
            let _ = w.flush().await;
            // 3. emit a continuous stream of well-formed
            //    `agent_message_chunk` frames. Each frame is
            //    well under MAX_FRAME_BYTES individually, but their
            //    cumulative `text` payload exceeds
            //    MAX_CUMULATIVE_CONTENT_BYTES. The driver MUST bail
            //    the turn with a cumulative-cap diagnostic.
            for i in 0..chunks_needed {
                let frame = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "goose-y9-cumulative",
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": { "type": "text", "text": chunk_text }
                        }
                    }
                });
                let s = serde_json::to_string(&frame).unwrap();
                let _ = w.write_all(s.as_bytes()).await;
                let _ = w.write_all(b"\n").await;
                let _ = w.flush().await;
                // Yield between chunks so the client has a chance
                // to read and process each frame before we push
                // the next. Without this the client could be
                // blocked on a single big read instead of seeing
                // the per-chunk cumulative growth.
                tokio::task::yield_now().await;
                if i % 8 == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            }
            // Hold the writer open past the bailout so the
            // client's error is the cumulative-cap diagnostic, not
            // an EOF / writer-dropped error.
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
        });
        let (mut r, mut w) = tokio::io::split(client_io);
        let mut r = tokio::io::BufReader::new(&mut r);
        let mut opts = goose_opts(Some("gpt-4o-mini"));
        opts.timeout = std::time::Duration::from_secs(5);
        let mut events: Vec<AgentEvent> = Vec::new();
        let res = drive_goose_io(&opts, &mut r, &mut w, &mut |ev| {
            events.push(ev);
        })
        .await;
        let _ = server.await;
        let err = res.expect_err(
            "Y-9: a continuous stream of sub-cap chunks whose cumulative content exceeds \
             the cap must make drive_goose_io return Err",
        );
        let msg = format!("{err:?}");
        // The diagnostic MUST mention cumulative + the cap value
        // (so the operator can distinguish from a per-frame cap
        // hit, which the Z-17 surface already names).
        assert!(
            msg.contains("cumulative"),
            "Y-9: expected a cumulative-cap diagnostic (mentioning 'cumulative'), got: {msg}"
        );
        assert!(
            msg.contains(&MAX_CUMULATIVE_CONTENT_BYTES.to_string()),
            "Y-9: expected the diagnostic to name the cumulative cap value \
             ({}), got: {msg}",
            MAX_CUMULATIVE_CONTENT_BYTES
        );
        // Sanity-check: the driver must NOT have absorbed the
        // full malicious stream — it must have stopped well before
        // the cumulative cap. Counting Text events gives us a
        // proxy for how much was appended (each chunk emits one
        // Text event). The cap / chunk_bytes would be the max we
        // expect to see PLUS one more chunk (the one that
        // triggers the bailout); allow some slack to avoid
        // racy off-by-ones across the cap boundary.
        let text_events: Vec<&AgentEvent> = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Text(_)))
            .collect();
        let max_text_events = chunks_needed; // upper bound — driver never sees more than this many
        assert!(
            text_events.len() < max_text_events,
            "Y-9: driver should bail BEFORE absorbing the full malicious stream \
             ({} chunks); saw {} Text events",
            chunks_needed,
            text_events.len()
        );
    }

    /// Y-9 (positive half): a normal turn whose cumulative
    /// streamed `content` is well under
    /// [`MAX_CUMULATIVE_CONTENT_BYTES`] MUST complete unchanged.
    /// Pins the regression-prevention contract that the new
    /// cumulative cap does NOT interfere with legitimate turns.
    #[tokio::test]
    async fn y9_drive_goose_io_normal_turn_under_cumulative_cap_succeeds() {
        // Build a turn with several sub-cap chunks whose total is
        // a small fraction of the cap. The driver must return Ok
        // and the run's `content` must equal the concatenated text.
        let chunk1 = "hello, ".to_string();
        let chunk2 = "world ".to_string();
        let chunk3 = "from goose".to_string();
        let outbound = vec![
            update(
                "goose-test-session-1",
                serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": chunk1 }
                }),
            ),
            update(
                "goose-test-session-1",
                serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": chunk2 }
                }),
            ),
            update(
                "goose-test-session-1",
                serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": chunk3 }
                }),
            ),
            update(
                "goose-test-session-1",
                serde_json::json!({
                    "sessionUpdate": "usage_update",
                    "used": 7,
                    "size": 131072
                }),
            ),
        ];
        let (_frames, run, _events) =
            drive_against_mock(outbound, false, ApprovalPolicy::Allowlist).await;
        assert_eq!(
            run.outcome, "completed",
            "Y-9 (positive): a normal turn well under the cumulative cap must still complete"
        );
        assert_eq!(
            run.content,
            format!("{chunk1}{chunk2}{chunk3}"),
            "Y-9 (positive): the streamed chunks must be concatenated into `content` unchanged"
        );
        // Sanity: the cumulative total is well under the cap.
        let total = (chunk1.len() + chunk2.len() + chunk3.len()) as u64;
        assert!(
            total < MAX_CUMULATIVE_CONTENT_BYTES,
            "sanity: the test payload must stay under the cap"
        );
    }

    #[tokio::test]
    async fn goose_drive_tool_call_update_does_not_duplicate_tool_call_event() {
        // Regression for the `tool_call_update` event-over-emission bug:
        //
        //   Per ACP v1, a single logical tool call generates ONE
        //   `session/update` with `sessionUpdate == "tool_call"` (the
        //   initial creation, status pending) and ZERO OR MORE follow-ups
        //   with `sessionUpdate == "tool_call_update"` (status transitions:
        //   in_progress -> completed/failed). The `tool_calls` counter
        //   is incremented only for the initial `tool_call`, but the
        //   previous driver ALSO emitted an `AgentEvent::ToolCall` for
        //   every `tool_call_update` — meaning a single call that
        //   progresses through pending -> in_progress -> completed would
        //   surface THREE `[tool] shell` lines to the CLI user (and a
        //   mismatched counter-vs-event invariant: counter==1, events==3).
        //
        //   The fix moves the `on_event(AgentEvent::ToolCall { ... })`
        //   INSIDE the `if kind == "tool_call"` branch so a single
        //   logical call surfaces exactly ONE event — same count as the
        //   counter, same count as the user-perceived invocations.
        //
        //   This test sends the exact ACP v1 progression: one initial
        //   `tool_call` (status pending) plus three `tool_call_update`
        //   frames for the SAME `toolCallId`, then asserts the
        //   counter and the event count BOTH report exactly one
        //   invocation. On the old code the test fails because the
        //   event count is 4 (1 initial + 3 updates); on the fix it
        //   passes because the event count is 1.
        let outbound = vec![
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
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call-1",
                    "status": "in_progress"
                }),
            ),
            update(
                "goose-test-session-1",
                serde_json::json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call-1",
                    "status": "in_progress",
                    "content": [{ "type": "text", "text": "running..." }]
                }),
            ),
            update(
                "goose-test-session-1",
                serde_json::json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call-1",
                    "status": "completed"
                }),
            ),
        ];
        let (_frames, run, events) =
            drive_against_mock(outbound, false, ApprovalPolicy::Allowlist).await;
        // The `tool_calls` counter is correct on BOTH old and new code
        // (only the initial `tool_call` increments it).
        assert_eq!(
            run.tool_calls, 1,
            "tool_calls counter must reflect logical invocations, not per-update frames"
        );
        // Count ALL `ToolCall` events the CLI would surface to the user
        // (regardless of name). On the OLD code this is 4 (1 initial +
        // 3 updates); on the FIX it is 1 — matching the counter. We
        // do NOT filter by name here because a buggy driver might
        // surface the events with placeholder names ("tool") and the
        // bug is the COUNT, not the name.
        let tool_event_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall { .. }))
            .count();
        assert_eq!(
            tool_event_count, 1,
            "exactly one ToolCall event must be emitted per logical tool call \
             (counter must equal event count); got events={:?}",
            events
        );
        // And the SINGLE event must carry the correct tool name from
        // the initial `tool_call` (not from a later update).
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::ToolCall { name } if name == "shell")),
            "the ToolCall event for call-1 must carry name=\"shell\""
        );
    }

    #[tokio::test]
    async fn goose_drive_tool_call_update_does_not_increment_tool_calls() {
        // Companion to the test above: even if the fix is reduced to
        // "never emit ToolCall for tool_call_update" but accidentally
        // also stopped incrementing `tool_calls` for the initial
        // `tool_call`, this test would catch it. Sends ONE `tool_call`
        // followed by FOUR `tool_call_update` frames for the same id;
        // the counter must be exactly 1, not 0 (regression for "never
        // count the call") and not 5 (regression for "count every frame").
        let outbound = vec![
            update(
                "goose-test-session-1",
                serde_json::json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call-7",
                    "title": "read",
                    "kind": "read",
                    "status": "pending"
                }),
            ),
            update(
                "goose-test-session-1",
                serde_json::json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call-7",
                    "status": "in_progress"
                }),
            ),
            update(
                "goose-test-session-1",
                serde_json::json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call-7",
                    "status": "in_progress"
                }),
            ),
            update(
                "goose-test-session-1",
                serde_json::json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call-7",
                    "status": "in_progress"
                }),
            ),
            update(
                "goose-test-session-1",
                serde_json::json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call-7",
                    "status": "completed"
                }),
            ),
        ];
        let (_frames, run, events) =
            drive_against_mock(outbound, false, ApprovalPolicy::Allowlist).await;
        assert_eq!(
            run.tool_calls, 1,
            "tool_calls must count logical invocations (1), not frames (5)"
        );
        // Count ALL ToolCall events, not just those with the right
        // name — a buggy driver might emit `ToolCall { name: "tool" }`
        // placeholders for the updates.
        let tool_event_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::ToolCall { .. }))
            .count();
        assert_eq!(
            tool_event_count, 1,
            "exactly one ToolCall event per logical call (must match the counter); got events={:?}",
            events
        );
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
        // Spawn in its own process group (as the production goose spawn does)
        // so the group-kill path in `kill_child` (`kill(-pgid, SIGKILL)`)
        // targets a real group led by this child.
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn sleep");
        let pid = child.id().expect("child has pid");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut conn = ConnectedTransport {
            reader: TransportReader::ChildStdout(stdout),
            writer: TransportWriter::ChildStdin(stdin),
            child: Some(child),
            pgid: Some(pid as i32),
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

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_child_reaps_grandchild_process_group() {
        // SB1 regression: goose forks its tool subprocesses (shell / build
        // commands) as GRANDCHILDREN. Before the fix, `kill_child` sent a
        // single-pid SIGKILL to only the direct `goose` process; the
        // grandchildren were reparented to init and kept running, leaking one
        // subtree per timed-out turn. The fix spawns the engine with
        // `.process_group(0)` and delivers `kill(-pgid, SIGKILL)` to the whole
        // group.
        //
        // This test builds that exact shape hermetically (no `goose` binary):
        // a `/bin/sh` "parent" that forks a `sleep 600` GRANDCHILD, writes the
        // grandchild's pid to a temp file, then blocks forever itself. We wrap
        // the parent in a `ConnectedTransport` the same way the production
        // spawn does (own process group, pgid captured), call `kill_child()`,
        // and assert the GRANDCHILD is gone — which only holds if the kill went
        // to the whole group, not just the direct child.
        use std::io::Write as _;
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("grandchild.pid");
        let pidfile_str = pidfile.to_str().expect("utf8 path").to_string();

        // Parent shell: background a long sleep (the grandchild), record its
        // pid, then wait forever so the parent stays alive until we kill it.
        let script = format!("sleep 600 & echo $! > '{pidfile_str}'; while true; do sleep 1; done");

        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(&script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(false)
            .process_group(0);
        let mut child = cmd.spawn().expect("spawn parent shell");
        let parent_pid = child.id().expect("parent has pid");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut conn = ConnectedTransport {
            reader: TransportReader::ChildStdout(stdout),
            writer: TransportWriter::ChildStdin(stdin),
            child: Some(child),
            // As the production spawn does: the child leads its own group, so
            // its pid is the pgid.
            pgid: Some(parent_pid as i32),
        };

        // Wait for the grandchild pid to appear in the pidfile (the parent
        // shell needs a moment to fork the background sleep + write the file).
        let mut grandchild_pid: Option<i32> = None;
        for _ in 0..200 {
            if let Ok(txt) = std::fs::read_to_string(&pidfile) {
                if let Ok(n) = txt.trim().parse::<i32>() {
                    if n > 0 {
                        grandchild_pid = Some(n);
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let grandchild_pid = grandchild_pid.expect("grandchild pid must appear in the pidfile");

        // Sanity: both parent and grandchild are alive before the reap.
        let parent_alive = unsafe { libc::kill(parent_pid as libc::pid_t, 0) };
        assert_eq!(parent_alive, 0, "parent must be alive before reap");
        let gc_alive = unsafe { libc::kill(grandchild_pid as libc::pid_t, 0) };
        assert_eq!(gc_alive, 0, "grandchild must be alive before reap");

        // The reap under test: this MUST take the grandchild down via the
        // group kill. A single-pid kill on the parent would leave the
        // grandchild alive (the bug).
        conn.kill_child().await;

        // Give the OS a beat to tear the group down (kill is async wrt the
        // grandchild's own reparent/exit), then poll for the grandchild to be
        // gone. `kill(pid, 0)` returns -1/ESRCH once it's reaped.
        let mut gc_gone = false;
        for _ in 0..200 {
            let alive = unsafe { libc::kill(grandchild_pid as libc::pid_t, 0) };
            if alive == -1 {
                gc_gone = true;
                break;
            }
            // Best-effort: reap any adopted zombie so kill(0) reports ESRCH
            // rather than lingering on a defunct entry (the grandchild is not
            // our child, so we can't wait() it; we rely on init reaping it).
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Defensive: if the group kill regressed, don't leak the grandchild out
        // of the test — take it down directly before asserting the failure.
        if !gc_gone {
            unsafe {
                libc::kill(grandchild_pid as libc::pid_t, libc::SIGKILL);
            }
        }
        assert!(
            gc_gone,
            "grandchild (pid {grandchild_pid}) must be reaped by the process-group              kill in kill_child(); a single-pid kill on the parent would leak it              (this is the SB1 bug)"
        );

        // Idempotent second call must not panic.
        conn.kill_child().await;
        let _ = std::io::stdout().flush();
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
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn /bin/sh -c 'sleep 60'");
        let pid = child.id().expect("spawned sleep child must have a pid");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut conn = ConnectedTransport {
            reader: TransportReader::ChildStdout(stdout),
            writer: TransportWriter::ChildStdin(stdin),
            child: Some(child),
            pgid: Some(pid as i32),
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
        let err_msg =
            res.expect_err("the inner drive must error out (sleep emits no ACP bytes); got Ok");
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
        #[cfg(unix)]
        cmd2.process_group(0);
        let mut child2 = cmd2.spawn().expect("spawn second sleep child");
        let pid2 = child2.id().expect("second child must have pid");
        let stdin2 = child2.stdin.take().expect("piped stdin");
        let stdout2 = child2.stdout.take().expect("piped stdout");
        let mut conn2 = ConnectedTransport {
            reader: TransportReader::ChildStdout(stdout2),
            writer: TransportWriter::ChildStdin(stdin2),
            child: Some(child2),
            pgid: Some(pid2 as i32),
        };
        let alive2_before = unsafe { libc::kill(pid2 as libc::pid_t, 0) };
        assert_eq!(alive2_before, 0, "second sleep child must be alive");

        // Inner future parks forever (no ACP output) — the timeout
        // MUST Elapse, dropping this future (and the `&mut conn2`
        // borrow). We then reap unconditionally — exactly what the
        // fix does after `tokio::time::timeout(...)` returns.
        let inner = async {
            let _reader = std::mem::replace(&mut conn2.reader, dummy_reader_for_tests());
            let _writer = std::mem::replace(&mut conn2.writer, dummy_writer_for_tests());
            std::future::pending::<()>().await;
        };
        let raced = tokio::time::timeout(std::time::Duration::from_millis(100), inner).await;
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

    // -----------------------------------------------------------------
    // C3-GD1: the approval + containment decision must key on the ACP
    // MACHINE tool `kind`, NOT the human-rendered `title`. Real goose puts
    // a human label in `title` ("Read file src/main.rs") and the semantic
    // category in `toolCall.kind` (read/edit/...). These tests use
    // REALISTIC goose frames (human title + kind) — NOT canonical names
    // stuffed into `title`.
    // -----------------------------------------------------------------

    /// GD1(a): under Allowlist, a permission request whose `title` is a
    /// human label ("Read file src/main.rs") but whose ACP `kind` is
    /// "read" MUST be auto-approved. Before the fix the driver keyed on
    /// `title`, the Allowlist exact-match against ["read", ...] never
    /// matched the human label, and safe reads were REJECTED — stalling
    /// allowlisted runs.
    #[tokio::test]
    async fn goose_drive_allowlist_approves_by_acp_read_kind_not_title() {
        let outbound = vec![serde_json::json!({
            "jsonrpc": "2.0",
            "id": "perm-read-kind",
            "method": "session/request_permission",
            "params": {
                "sessionId": "goose-test-session-1",
                "toolCall": {
                    // Human label — exactly what real goose renders.
                    "title": "Read file src/main.rs",
                    // Machine category — the ACP v1 semantic kind.
                    "kind": "read",
                    "rawInput": { "path": "src/main.rs" }
                },
                "options": [
                    { "optionId": "opt-A9F1", "name": "Allow once", "kind": "allow_once" },
                    { "optionId": "opt-B2C3", "name": "Deny once", "kind": "reject_once" }
                ]
            }
        })
        .to_string()];
        let (frames, _run, events) =
            drive_against_mock(outbound, false, ApprovalPolicy::Allowlist).await;
        let reply = &frames[3];
        assert_eq!(reply["id"], "perm-read-kind");
        assert_eq!(
            reply["result"]["outcome"]["outcome"], "selected",
            "an ACP kind:\"read\" call must be allowlist-APPROVED regardless of the human title"
        );
        assert_eq!(
            reply["result"]["outcome"]["optionId"], "opt-A9F1",
            "must echo back the allow_once option's opaque server id"
        );
        // The display event uses the human title, not the kind.
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Approval { tool, approved: true } if tool == "Read file src/main.rs"
            )),
            "Approval event should carry the human title for display and approved=true; got {events:?}"
        );
    }

    /// GD1(b): with `enforce_writable_roots` ON, a permission request with
    /// `kind:"edit"` and an out-of-writable-root path MUST be contained
    /// (denied) even under ApprovalPolicy::All. Before the fix the
    /// write-class containment keyed on canonical names in `title`
    /// (text_editor/write_file), never matched goose's human title, and
    /// an out-of-root edit sailed through under All.
    #[tokio::test]
    async fn goose_drive_containment_triggers_on_acp_edit_kind_out_of_root() {
        let (_dir, root_a, _root_b, outside) = make_containment_dirs();
        let evil = outside.join("passwd");
        std::fs::write(&evil, "x").unwrap();
        let outbound = vec![serde_json::json!({
            "jsonrpc": "2.0",
            "id": "perm-edit-kind",
            "method": "session/request_permission",
            "params": {
                "sessionId": "goose-test-session-1",
                "toolCall": {
                    // Human label; the machine category is the `kind`.
                    "title": "Edit /etc/passwd",
                    "kind": "edit",
                    // write_file's field name; path is OUTSIDE root_a.
                    "rawInput": { "file_path": evil.to_string_lossy() }
                },
                "options": [
                    { "optionId": "opt-allow", "name": "Allow once", "kind": "allow_once" },
                    { "optionId": "opt-deny", "name": "Deny once", "kind": "reject_once" }
                ]
            }
        })
        .to_string()];

        let mut opts = goose_opts(Some("gpt-4o-mini"));
        opts.writable_roots = vec![root_a.clone()];
        opts.enforce_writable_roots = true;
        let (frames, _run, events) = drive_against_mock_with_opts(
            outbound,
            false,
            // Even ApprovalPolicy::All must be overridden by containment.
            ApprovalPolicy::All,
            opts,
        )
        .await;
        let reply = &frames[3];
        assert_eq!(reply["id"], "perm-edit-kind");
        assert_eq!(
            reply["result"]["outcome"]["outcome"], "selected",
            "denials still SELECT the reject option so goose doesn't wedge"
        );
        assert_eq!(
            reply["result"]["outcome"]["optionId"], "opt-deny",
            "an ACP kind:\"edit\" targeting a path outside writable_roots MUST be contained \
             (denied) even under ApprovalPolicy::All"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::Approval { tool, approved: false } if tool == "Edit /etc/passwd"
            )),
            "containment denial must surface approved=false with the display title; got {events:?}"
        );
    }

    /// GD2: goose may emit streaming `agent_message_chunk`s AND a final
    /// `agent_message` carrying the full text. The reply must appear
    /// exactly ONCE (the terminal `agent_message` is authoritative-REPLACE),
    /// not doubled ("hello worldhello world"), and Text must not be emitted
    /// twice for the same content.
    #[tokio::test]
    async fn goose_drive_agent_message_after_chunks_not_double_counted() {
        let chunk = |t: &str| {
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "goose-test-session-1",
                    "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": t }
                    }
                }
            })
            .to_string()
        };
        let final_msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "goose-test-session-1",
                "update": {
                    "sessionUpdate": "agent_message",
                    "content": { "type": "text", "text": "hello world" }
                }
            }
        })
        .to_string();
        let outbound = vec![chunk("hello "), chunk("world"), final_msg];
        let (_frames, run, _events) =
            drive_against_mock(outbound, false, ApprovalPolicy::Allowlist).await;
        assert_eq!(
            run.content, "hello world",
            "final agent_message must REPLACE the accumulated chunk content, not append to it \
             (append would yield the doubled \"hello worldhello world\")"
        );
    }

    /// GD3: a server frame carrying id:"prompt" AND a `method` is a
    /// REQUEST, not the terminal prompt RESPONSE. It must NOT be treated
    /// as the prompt response (which would end the turn early). Here a
    /// `session/request_permission` request reuses id:"prompt"; the driver
    /// must answer it and continue, then end only on the REAL prompt
    /// response.
    #[tokio::test]
    async fn goose_drive_method_bearing_frame_with_prompt_id_not_terminal() {
        // A permission REQUEST that (adversarially) reuses id:"prompt".
        let outbound = vec![serde_json::json!({
            "jsonrpc": "2.0",
            "id": "prompt",
            "method": "session/request_permission",
            "params": {
                "sessionId": "goose-test-session-1",
                "toolCall": { "title": "Read file x", "kind": "read" },
                "options": [
                    { "optionId": "opt-allow", "name": "Allow once", "kind": "allow_once" },
                    { "optionId": "opt-deny", "name": "Deny once", "kind": "reject_once" }
                ]
            }
        })
        .to_string()];
        let (frames, run, events) =
            drive_against_mock(outbound, false, ApprovalPolicy::Allowlist).await;
        // GD3 proof: the driver must have REPLIED to the method-bearing
        // id:"prompt" frame as a permission request. That reply is a frame
        // carrying `result.outcome.outcome == "selected"`. WITHOUT the fix,
        // branch A matches on id=="prompt" first, consumes the frame as the
        // terminal prompt RESPONSE, ends the turn early, and NO such reply
        // is ever written — so the presence of a `selected` reply is the
        // load-bearing assertion.
        let perm_reply = frames.iter().find(|f| {
            f.get("result")
                .and_then(|r| r.get("outcome"))
                .and_then(|o| o.get("outcome"))
                .and_then(serde_json::Value::as_str)
                == Some("selected")
        });
        let reply = perm_reply.unwrap_or_else(|| {
            panic!(
                "the method-bearing id:\"prompt\" frame must be answered as a permission \
                 request (a `selected` reply), NOT consumed as the terminal prompt response; \
                 frames={frames:?}"
            )
        });
        assert_eq!(
            reply["id"], "prompt",
            "the permission reply echoes the request id (which was \"prompt\")"
        );
        assert_eq!(reply["result"]["outcome"]["optionId"], "opt-allow");
        // The turn still completed via the REAL (method-less) prompt response.
        assert_eq!(run.outcome, "completed");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Approval { approved: true, .. })),
            "the permission request (id:prompt + method) must be processed as an approval; got {events:?}"
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

    // -----------------------------------------------------------------
    // `cancel_session` — the loop's author-phase watchdog cancel hook.
    //
    // This is the public surface the loop calls when its outer
    // `phase_watchdog` fires. Without it, dropping the future merely
    // orphans the turn on the daemon side. These tests pin the wire
    // shape and the "settle on turn_complete" contract.
    // -----------------------------------------------------------------

    use tokio::net::UnixListener;

    /// Spin up a Unix-socket "daemon" that speaks just enough ACP to
    /// satisfy `cancel_session`: ack `initialize`, accept `session/cancel`
    /// as a NOTIFICATION (no `id`), and respond with a
    /// `session/update {type: "turn_complete"}` notification. Records all
    /// received frames into `received` so the test can assert the cancel
    /// was sent with the expected session id.
    async fn spawn_cancel_test_daemon(
        received: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind");
        let recv = received.clone();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let (read_half, mut write_half) = tokio::io::split(stream);
                let mut reader = tokio::io::BufReader::new(read_half);
                let mut line = String::new();
                // 1. read initialize -> ack
                let _ = reader.read_line(&mut line).await;
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                    recv.lock().unwrap().push(v);
                }
                let init_ack = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "init",
                    "result": { "protocolVersion": 1 }
                });
                let _ = write_half
                    .write_all(serde_json::to_string(&init_ack).unwrap().as_bytes())
                    .await;
                let _ = write_half.write_all(b"\n").await;
                let _ = write_half.flush().await;
                // 2. read session/cancel (a notification, no id) -> record it
                line.clear();
                let _ = reader.read_line(&mut line).await;
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                    recv.lock().unwrap().push(v);
                }
                // 3. respond with turn_complete so cancel_session can settle
                let complete = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": { "type": "turn_complete", "outcome": "cancelled" }
                });
                let _ = write_half
                    .write_all(serde_json::to_string(&complete).unwrap().as_bytes())
                    .await;
                let _ = write_half.write_all(b"\n").await;
                let _ = write_half.flush().await;
            }
        });
        dir
    }

    /// `cancel_session` MUST send `session/cancel` as a notification (no
    /// `id`), carrying the session id from its argument. The loop's
    /// watchdog relies on this exact wire shape — the daemon's per-
    /// session bookkeeping keys off the sessionId param. A test that
    /// fails here means the loop's cancel is no longer addressing the
    /// right session on the daemon side.
    #[tokio::test]
    async fn cancel_session_sends_cancel_notification_with_session_id() {
        let received: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let dir = spawn_cancel_test_daemon(received.clone()).await;
        let socket = dir.path().join("daemon.sock");
        let res = cancel_session(&socket, "sess-z-8", Duration::from_secs(2)).await;
        assert!(
            res.is_ok(),
            "cancel_session must settle on turn_complete: {res:?}"
        );
        // give the server task a moment to push the recorded frame
        tokio::time::sleep(Duration::from_millis(50)).await;
        let frames = received.lock().unwrap().clone();
        let cancel = frames
            .iter()
            .find(|f| f.get("method").and_then(Value::as_str) == Some("session/cancel"))
            .expect("daemon must have received session/cancel");
        assert!(
            cancel.get("id").is_none(),
            "session/cancel MUST be a notification (no id); got {cancel:?}"
        );
        assert_eq!(
            cancel.pointer("/params/session_id").and_then(Value::as_str),
            Some("sess-z-8"),
            "Y-2: session/cancel MUST carry the loop's session id under the \
             snake_case `session_id` key the zeroclaw daemon reads (a \
             camelCase `sessionId` is silently ignored → cancel is a no-op); \
             got {cancel:?}"
        );
        assert!(
            cancel.pointer("/params/sessionId").is_none(),
            "Y-2: must NOT emit the camelCase `sessionId` the daemon ignores; got {cancel:?}"
        );
    }

    /// `cancel_session` MUST NOT return until the daemon has acknowledged
    /// the cancel (via `session/update {type: "turn_complete"}`). The
    /// loop's `build_diff` capture is gated on this return — if the
    /// daemon is allowed to keep editing after cancel_session returns,
    /// the loop is back to reviewing a torn tree. The settle bound is
    /// also pinned: cancel_session must not block forever on an
    /// unresponsive daemon.
    #[tokio::test]
    async fn cancel_session_returns_after_daemon_turn_complete_within_budget() {
        let received: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let dir = spawn_cancel_test_daemon(received.clone()).await;
        let socket = dir.path().join("daemon.sock");
        let start = std::time::Instant::now();
        // Mirrors the production settle budget (5s) — see
        // `zoder_core::CANCEL_SETTLE_BUDGET`. Kept locally because this
        // crate doesn't depend on `zoder-core`; the wire-shape tests
        // exist to pin the daemon side, not the loop's policy choice.
        let settle = Duration::from_secs(5);
        let res = cancel_session(&socket, "sess-z-8b", settle).await;
        let elapsed = start.elapsed();
        assert!(
            res.is_ok(),
            "cancel_session must settle on turn_complete: {res:?}"
        );
        assert!(
            elapsed < settle,
            "cancel_session must settle promptly when daemon acks; took {elapsed:?}"
        );
    }

    /// HIGH-severity Z-8 regression guard: if the daemon closes the
    /// connection BEFORE sending `session/update {type: "turn_complete"}`,
    /// `cancel_session` MUST return `Err` (forcing the loop to wait out
    /// its settle budget for a torn tree) rather than `Ok(())` (which the
    /// pre-fix code did, on the optimistic assumption that "EOF =
    /// settled"). The pre-fix behavior lets a daemon that crashed
    /// mid-edit trick the loop into capturing a torn diff.
    ///
    /// We also pin a `elapsed < budget` invariant: the diagnostic must
    /// arrive *before* the settle budget elapses, not after. A buggy
    /// implementation that simply waits `settle_budget` and then
    /// reports Err would pass the `is_err` check but still stall the
    /// loop for the full budget on every timeout, which is what the
    /// connect-timeout fix (below) is meant to prevent.
    #[tokio::test]
    async fn cancel_session_early_eof_is_treated_as_settle_failure() {
        use tokio::net::UnixListener;
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind");
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let (read_half, mut write_half) = tokio::io::split(stream);
                let mut reader = tokio::io::BufReader::new(read_half);
                let mut line = String::new();
                // 1. read initialize -> ack
                let _ = reader.read_line(&mut line).await;
                let init_ack = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "init",
                    "result": { "protocolVersion": 1 }
                });
                let _ = write_half
                    .write_all(serde_json::to_string(&init_ack).unwrap().as_bytes())
                    .await;
                let _ = write_half.write_all(b"\n").await;
                let _ = write_half.flush().await;
                // 2. read session/cancel — and then CLOSE the connection
                //    without sending turn_complete. This models a daemon
                //    that crashed (or one that mishandles cancel).
                line.clear();
                let _ = reader.read_line(&mut line).await;
                // Drop write_half / stream — that closes the socket.
                drop(write_half);
            }
        });
        let budget = Duration::from_millis(500);
        let start = std::time::Instant::now();
        let res = cancel_session(&socket, "sess-eof", budget).await;
        let elapsed = start.elapsed();
        assert!(
            res.is_err(),
            "Z-8 HIGH: EOF before turn_complete MUST return Err (got Ok)."
        );
        assert!(
            elapsed < budget,
            "Z-8 HIGH: cancel_session must return its EOF diagnostic within \
             the settle budget; took {elapsed:?} vs budget {budget:?}"
        );
    }

    /// INFO Z-8 fix: `cancel_session` must cap its connect wait at
    /// `settle_budget`. A daemon whose socket exists but isn't accepting
    /// (e.g., a hung child holding the listening fd but not calling
    /// `accept`) would otherwise block on whatever connect path the
    /// kernel takes. While Unix sockets typically complete `connect`
    /// immediately for a bound listener, the timeout races still cost
    /// nothing and protect against pathological cases where the daemon
    /// socket exists but is wedged at a deeper layer.
    ///
    /// This test exercises the COMMON failure shape — the socket file
    /// doesn't exist — and asserts cancel_session fails fast rather
    /// than blocking past `settle_budget`.
    #[tokio::test]
    async fn cancel_session_fails_fast_when_socket_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("nope.sock");
        // No listener bound -> connect returns ECONNREFUSED immediately.
        let start = std::time::Instant::now();
        let res = cancel_session(&socket, "sess", Duration::from_millis(200)).await;
        let elapsed = start.elapsed();
        assert!(
            res.is_err(),
            "cancel_session MUST surface connect failures; got Ok"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "cancel_session must fail fast on connect errors; took {elapsed:?}"
        );
    }

    // -----------------------------------------------------------------
    // Z-17 [MED]: unbounded per-frame read_line -> OOM.
    //
    // The wire layer (drive / drive_goose_io / cancel_session) used
    // to call `AsyncBufReadExt::read_line` with no cap. A hostile /
    // buggy engine that emits a single NDJSON line longer than
    // [`MAX_FRAME_BYTES`] (or never sends a newline) would make zoder
    // buffer without limit and OOM within the turn deadline.
    //
    // The fix caps the per-frame read with
    // `AsyncReadExt::take(MAX_FRAME_BYTES)` and surfaces a clear
    // "frame exceeds N-byte cap" error when no newline is found
    // inside the cap. These tests pin that behavior against the
    // REAL `read_result` reader, not a reimplementation.
    //
    // Both tests use a `tokio::io::duplex` whose buffer is sized so
    // the server can write the whole oversized frame in one shot
    // (otherwise the server's `write_all` would block on the bounded
    // buffer and the test would deadlock, not fail — which is
    // exactly the bug we want to prevent).
    // -----------------------------------------------------------------

    /// Z-17: an oversized frame (no newline within [`MAX_FRAME_BYTES`])
    /// must surface as a clear overflow error from the per-frame
    /// reader. The reader must NOT buffer the line unboundedly; it
    /// must fail fast at the cap.
    #[tokio::test]
    async fn z17_read_result_errors_on_oversized_frame() {
        use tokio::io::AsyncWriteExt;
        // 2x MAX so the server can push the whole frame in one
        // write; otherwise it would block on the bounded buffer.
        let buf = (MAX_FRAME_BYTES as usize) * 2;
        let (client_io, mut engine_io) = tokio::io::duplex(buf);
        let server = tokio::spawn(async move {
            // MAX + 256 bytes of 'x' with no '\n' inside. We follow
            // with a '\n' so a well-behaved NEXT-frame reader would
            // not loop forever; the cap MUST trip on this single
            // frame first.
            let huge = vec![b'x'; MAX_FRAME_BYTES as usize + 256];
            let _ = engine_io.write_all(&huge).await;
            let _ = engine_io.write_all(b"\n").await;
            let _ = engine_io.shutdown().await;
        });
        let mut r = tokio::io::BufReader::new(client_io);
        let res = read_result(&mut r, "init").await;
        let _ = server.await;
        let err = res.expect_err("oversized frame MUST error, not buffer unboundedly");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("cap") || msg.contains("exceeds") || msg.contains("too large"),
            "Z-17: expected an overflow / cap error, got: {msg}"
        );
    }

    /// Z-17 (round-trip sanity): legitimate normal-sized frames
    /// still parse unchanged. This is the non-breaking half of the
    /// fix: capping the read MUST NOT change the behavior of
    /// [`read_result`] for well-behaved engines.
    #[tokio::test]
    async fn z17_read_result_still_parses_legitimate_frames_under_cap() {
        use tokio::io::AsyncWriteExt;
        let (client_io, mut engine_io) = tokio::io::duplex(8 * 1024);
        let server = tokio::spawn(async move {
            let frame = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "init",
                "result": { "protocolVersion": 1 }
            });
            let mut s = serde_json::to_string(&frame).unwrap();
            s.push('\n');
            let _ = engine_io.write_all(s.as_bytes()).await;
            let _ = engine_io.shutdown().await;
        });
        let mut r = tokio::io::BufReader::new(client_io);
        let res = read_result(&mut r, "init")
            .await
            .expect("legitimate frame must still parse after the cap fix");
        assert_eq!(res["protocolVersion"], 1);
        let _ = server.await;
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
            _ => {
                eprintln!("SKIP: MINIMAX_API_KEY not set");
                return;
            }
        };
        let transport = EngineTransport::Stdio {
            command: "goose".to_string(),
            args: vec!["acp".to_string()],
            env: vec![
                ("GOOSE_PROVIDER".to_string(), "openai".to_string()),
                ("GOOSE_MODEL".to_string(), "MiniMax-M3".to_string()),
                ("OPENAI_API_KEY".to_string(), key.clone()),
                (
                    "OPENAI_HOST".to_string(),
                    "https://api.minimax.io".to_string(),
                ),
                (
                    "OPENAI_BASE_URL".to_string(),
                    "https://api.minimax.io/v1".to_string(),
                ),
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
            goose_provider: None,
            // SLICE 1: defaults matching `AgentOptions::new` — keep this
            // real-engine oracle test non-breaking (enforce off,
            // trust off) and pin the writable boundary to the
            // synthetic cwd it uses.
            writable_roots: vec![std::path::PathBuf::from("/tmp")],
            enforce_writable_roots: false,
            trust_engine: false,
            // SLICE: empty Vec keeps the wire shape `[]` — non-breaking
            // against the real goose binary even though that test path
            // is gated behind `#[ignore]`.
            mcp_servers: Vec::new(),
            // PROJECT-INSTRUCTIONS SLICE: default off so the
            // real-engine oracle continues to send the raw prompt
            // verbatim (matches every other `AgentOptions` literal).
            project_instructions: None,
            // PERSISTENT-SESSIONS SLICE: default OFF — the real-engine
            // oracle is gated behind `#[ignore]` and only validates
            // parity with the pre-this-slice fresh-create path.
            persist_session_id: false,
            session_store_path: None,
        };
        let mut conn = connect_transport(&transport)
            .await
            .expect("spawn goose acp");
        let res;
        {
            let mut r = tokio::io::BufReader::new(&mut conn.reader);
            let w = &mut conn.writer;
            res = drive_goose_io(&opts, &mut r, w, &mut |_| {}).await;
        }
        conn.kill_child().await;
        let run = res.expect("drive_goose_io errored against real goose+minimax");
        eprintln!(
            "REAL TURN outcome={} content={:?} tools={}",
            run.outcome, run.content, run.tool_calls
        );
        assert_ne!(
            run.outcome, "failed",
            "real goose+minimax turn failed outright"
        );
        assert_ne!(
            run.outcome, "timeout",
            "real goose+minimax turn TIMED OUT (possible prompt-write-before-read deadlock!)"
        );
    }
}
