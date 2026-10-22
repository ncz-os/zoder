//! Reap the processes an agentic `loop`/`rescue` leaked into the workspace.
//!
//! 2026-10-21 (HYDRA, hive job `01a1601b`): a `zoder loop` driven through the
//! zeroclaw ACP agent had the agent start 368 `yes` load generators in the
//! background. They ran for ~2 h at load average 376 and survived BOTH the end
//! of the loop and the Hive worker's 7200 s timeout, because the worker only
//! kills zoder's own process group (`os.killpg`) and those processes belong to
//! the zeroclaw daemon's tool shells — a different tree entirely.
//!
//! So the loop must own its blast radius: on EVERY exit path (success,
//! failure, `--loop-timeout`/`--agent-timeout`, SIGTERM, SIGINT, panic
//! unwind) cancel the agent session first, then kill what the agent left
//! behind, then sweep once more for anything the engine minted while we were
//! killing. `Drop` cannot await, so the panic path sends the cancel over a
//! blocking socket — thinner than the async path, but it lands before the
//! reap, which is the order that matters.
//!
//! Selection is deliberately narrow, because "kill stuff on exit" on a shared
//! host is how you take down somebody else's session:
//!
//! * same uid (`getuid()`) — never another user's processes,
//! * cwd inside the `-C` directory or zoder's own per-run scratch directory
//!   under the temp root — the workspace the agent was given. Never the temp
//!   root itself: on a shared host `/tmp` is everybody's cwd,
//! * not present when the loop began (a pid snapshot taken at scope open) and
//!   started after it (`/proc/<pid>/stat` `starttime`, which no pre-existing
//!   process can fake) — the two together are exact at USER_HZ granularity,
//! * not zoder and not one of zoder's ancestors — never our own tree.
//!
//! That is deliberately the whole ownership test, and it is a judgement call:
//! nothing in /proc says "the agent spawned this". The alternative is the bug
//! this file exists to fix — a leftover process nobody will ever clean up. On a
//! shared host the exposure is bounded to same-uid processes whose cwd is
//! this run's own workspace and that started during this run; `--no-reap` is
//! the escape hatch for anyone who needs that stricter.
//!
//! ## The shared daemon is exempt, by identity
//!
//! Every rule above describes the AGENT's processes, but the zeroclaw daemon
//! is not the agent's: it is one daemon per host that every zoder on the box
//! drives through `<data dir>/daemon.sock`. 2026-10-21 (ACHILLES): job
//! `01a16164`'s `zoder loop` spawned the daemon with no `current_dir` set, so
//! the daemon inherited the JOB's workspace as its cwd. The daemon matched
//! every rule in the list — same uid, cwd inside `-C`, started after the loop
//! began, not an ancestor — so this reaper SIGTERM'd it, killing the agent of
//! every other zoder on the host that was mid-turn.
//!
//! So the daemon is excluded outright, and the exclusion is deliberately made
//! of identity only, never of command-line text:
//!
//! * the process this zoder spawned (recorded by `note_daemon_identity` the
//!   instant the child exists, pid AND start time),
//! * the process recorded for the socket (`<pid> <start_ticks>` in
//!   `<data dir>/daemon.pid`, so a reaper in a *different* zoder process can
//!   spare it too — and re-checked against the live process, so a stale
//!   record whose pid the kernel has since reused spares nobody),
//! * `/proc/<pid>/exe` being the exact daemon binary zoder itself spawns
//!   (`locate_sibling("zeroclaw")`, compared by resolved file). A NAME check
//!   would spare anything called `zeroclaw`, including a copy an agent
//!   dropped in the very workspace being reaped.
//!
//! No argv matching anywhere: a process can be renamed into looking like the
//! daemon (`cp`, a shell called `zeroclaw`), and the daemon's own argv is not
//! a stable identity (`--ephemeral` is optional, the config dir varies). The
//! cost of that rule is stated plainly, because it is a real limit rather than
//! a rounding error: any process running the daemon BINARY is spared, even
//! one an agent started under a different subcommand. Every other arm is an
//! exact identity and does not have that reach; this one is the cross-process
//! fallback, and its failure direction is "spare something" rather than "kill
//! the shared daemon".
//!
//! The daemon's TOOL SHELLS are not exempt. They are what the agent starts —
//! the `yes` generators of the 01a1601b incident — they are not the daemon
//! binary, and they still match every ownership rule, so they are still
//! reaped. (A child that re-execs the daemon binary is, by definition, the
//! daemon, and is spared with it.) And processes the daemon runs for OTHER
//! sessions are still protected by the cwd rule above: another session's
//! leftovers live in another job's workspace, not in `-C`.
//!
//! The neutral spawn that keeps the daemon out of a job directory in the
//! first place lives in the CLI ([`crate::daemon_working_dir`]); the
//! exemption here is what makes that fix defence in depth rather than the
//! only thing standing between a reap and somebody else's agent.
//!
//! Selection is by exact PID only. There is no pattern matching anywhere here:
//! `pkill yes` would kill every `yes` on the host, including one a human
//! started. Where the kernel offers pidfd (5.3+), the selected process is
//! pinned before it is signalled, so a pid recycled mid-teardown cannot put a
//! stranger in front of the signal; older kernels fall back to the identity
//! re-check. Linux-only via `/proc`; a no-op elsewhere.
//!
//! `--no-reap` turns the whole thing off.
//!
//! ## An unreachable engine is not a busy agent
//!
//! 2026-10-22 (MINOS, hive job `01a163c9`): at the end of a `zoder loop` every
//! `session/cancel` failed with `connecting to engine at
//! ~/.zoder/data/daemon.sock: No such file or directory (os error 2)`, the same
//! "cancel not confirmed" line repeated for one session across the cancel
//! passes and all four teardown rounds, and teardown then printed `WARNING:
//! teardown stopped after 4 rounds with the agent still producing work;
//! leftovers may remain`. Nothing was producing work. The engine socket path
//! had ceased to exist — zeroclaw's `EndpointGuard` owns that directory entry
//! and unlinks it when the daemon that bound it stops, and `--ephemeral` (which
//! is how zoder spawns it) stops one second after its last client disconnects,
//! so on a host where a second daemon had also bound that path the entry can be
//! gone while a live daemon keeps listening on an inode no `connect(2)` can
//! name. The reap was correct the whole time; only the CANCEL was talking to
//! something that no longer existed.
//!
//! So the two failures are kept apart. A cancel that fails because the socket
//! does not exist (ENOENT) or nothing is listening on it (ECONNREFUSED) is
//! "engine unreachable": the session died with its daemon, there is nothing to
//! cancel, and it is certainly not work still being produced. It is said once
//! per session, the session is retired from the teardown instead of retried,
//! and it is never counted as evidence that the agent is still going. Every
//! other cancel failure is what "cancel not confirmed" has always meant: the
//! engine was reached and did not acknowledge.
//!
//! For the same reason the "still producing work" warning is now gated on the
//! final round's reap pass having actually found and signalled new processes.
//! Before this, a teardown that converged perfectly but could not reach the
//! engine still printed it, because the warning fired on "the round counter ran
//! out", not on "the reap found work".

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// SIGTERM -> settle -> SIGKILL grace on the normal exit path.
pub(crate) const REAP_GRACE: Duration = Duration::from_millis(1500);
/// Same, but tighter: this path runs inside a signal handler, and `zoder
/// cancel` only allows a worker `CANCEL_TERM_GRACE` to tear down before it
/// escalates to SIGKILL (see `cancel_background_process_group`).
const SIGNAL_REAP_GRACE: Duration = Duration::from_millis(750);
/// Budget for the ACP `session/cancel` handshake before we start killing.
pub(crate) const REAP_CANCEL_BUDGET: Duration = Duration::from_millis(750);
/// Same, on the signal path (see `SIGNAL_REAP_GRACE`).
const SIGNAL_CANCEL_BUDGET: Duration = Duration::from_millis(500);
/// Settle time between productive reap passes. A pass that killed something
/// has no reason to believe the work is finished: a shell that backgrounded a
/// child may fork the next one a moment later. Without this, the reap converges
/// on "nothing is alive right now" instead of "nothing more is coming".
const PASS_SETTLE: Duration = Duration::from_millis(250);
/// `TASK_COMM_LEN` from the kernel: 16 bytes, NUL included.
const TASK_COMM_LEN: usize = 16;
/// How many cancel/reap rounds the teardown may take before giving up. Two is
/// the normal case (something to clean, then confirmation there is nothing).
const MAX_TEARDOWN_ROUNDS: usize = 4;
/// How many drain passes the cancel phase may make before giving up.
const MAX_CANCEL_PASSES: usize = 8;
/// Read timeout for the blocking cancel used on the `Drop` path.
const BLOCKING_ACK_WAIT: Duration = Duration::from_millis(200);
/// How many scan/signall passes one reap may make before giving up. Each pass
/// only runs when it found something new, so a well-behaved agent converges in
/// two; the bound exists for an agent that forks in a loop.
const MAX_REAP_PASSES: usize = 8;
/// Poll interval while waiting for a signalled process to disappear.
const POLL: Duration = Duration::from_millis(20);
/// How long to wait for a SIGKILLed process to leave the table. SIGKILL is not
/// catchable, so this only covers reaping.
const KILL_WAIT: Duration = Duration::from_millis(500);

/// Session ids this process drove, so the exit path can cancel them even when
/// the loop never reached its own author-timeout cancel. A ghost turn from
/// iteration 1 keeps editing the tree just as hard as one from iteration 9.
static SESSIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The identity (pid -> start_ticks) of the shared zeroclaw daemon, when THIS
/// zoder process spawned it.
///
/// Recorded the instant `Command::spawn` returns (see
/// `crate::record_daemon_pid`), which is the moment a scope opened earlier
/// cannot yet be looking: its pid snapshot has no daemon in it and its
/// boundary predates the daemon, so every ownership rule in this file selects
/// the daemon. The daemon outlives zoder by design — it serves every
/// concurrent session on the host — so it must never be one of our reap
/// targets.
///
/// The value is the start time, not a bare pid, for the same reason the reap
/// re-checks it before every signal: the kernel recycles pids, and a daemon
/// that dies must not leave a standing permission to spare whoever inherits
/// its number.
static DAEMON_PIDS: std::sync::LazyLock<Mutex<HashMap<u32, u64>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Pids already reported as spared, so the exemption is announced once per
/// daemon per process instead of on every scan of every teardown round.
static REPORTED_DAEMONS: std::sync::LazyLock<Mutex<HashSet<u32>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

/// Remember the shared daemon's identity so no reap scope can select it.
/// Called by the CLI immediately after the daemon child is spawned.
pub(crate) fn note_daemon_identity(identity: crate::DaemonIdentity) {
    let (pid, start_ticks) = identity;
    if pid > 1 {
        DAEMON_PIDS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(pid, start_ticks);
    }
}

/// Retract an in-process daemon record, the counterpart of
/// [`note_daemon_identity`]. Correctness does not depend on it — the live
/// check rejects a process that is gone — but a failed spawn should not leave
/// the in-process map and the pid file disagreeing about what exists.
pub(crate) fn forget_daemon_identity(identity: crate::DaemonIdentity) {
    let (pid, start_ticks) = identity;
    let mut daemons = DAEMON_PIDS.lock().unwrap_or_else(|e| e.into_inner());
    // Compare BEFORE removing. `HashMap::remove` is unconditional: removing
    // first and inspecting the return value afterwards would retract a NEWER
    // daemon that inherited this pid, which is the opposite of what "forget
    // this daemon" means.
    if daemons.get(&pid) == Some(&start_ticks) {
        daemons.remove(&pid);
    }
}

/// Is `pid` exempt as "a daemon this process spawned", right now? Test-only
/// seam for the in-process arm of [`daemon_exemption`], which the scan calls
/// directly.
#[cfg(test)]
pub(crate) fn daemon_spared_by_this_process(pid: u32) -> bool {
    spawned_daemon_still_matches(pid)
}

/// The in-process arm: recorded for this pid AND still the same process.
/// The second half is what keeps a dead daemon from leaving a standing
/// exemption behind for whoever the kernel hands its pid to next.
fn spawned_daemon_still_matches(pid: u32) -> bool {
    let recorded = DAEMON_PIDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&pid)
        .copied();
    match recorded {
        Some(start_ticks) => live_start_ticks(pid) == Some(start_ticks),
        None => false,
    }
}

/// Why a process is exempt from the reap. Every arm is an identity the kernel
/// exposes for the process itself; none of them is its command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonExemption {
    /// The process this zoder process spawned, still running.
    Spawned,
    /// The `(pid, start_ticks)` pair recorded for the socket this reaper
    /// talks to, still matching the live process.
    Recorded,
    /// `/proc/<pid>/exe` resolves to the zeroclaw binary.
    Executable,
}

impl DaemonExemption {
    fn reason(self) -> &'static str {
        match self {
            DaemonExemption::Spawned => "the daemon this zoder spawned",
            DaemonExemption::Recorded => "the pid recorded for the daemon socket",
            DaemonExemption::Executable => "/proc/<pid>/exe is the zeroclaw binary",
        }
    }
}

/// Is this process the shared zeroclaw daemon? Checked for every candidate,
/// on every pass, because a daemon can appear at any time during a run.
///
/// Every arm is re-checked against the LIVE process before it spares
/// anything. A pid on its own is not an identity: the kernel recycles them,
/// and an exemption that outlives the process it was granted to is exactly
/// the "spare a stranger" bug. The pid-file arm cannot be cached in the plan
/// either — the daemon is usually spawned *after* the scope opened, which is
/// the case it exists for — so the record is read once per scan.
fn daemon_exemption(
    pid: u32,
    recorded: &mut Option<Option<crate::DaemonIdentity>>,
    pid_file: &Path,
    daemon_binary: &Path,
) -> Option<DaemonExemption> {
    if spawned_daemon_still_matches(pid) {
        return Some(DaemonExemption::Spawned);
    }
    if recorded.is_none() {
        *recorded = Some(crate::read_daemon_record(pid_file));
    }
    if let Some(Some((recorded_pid, recorded_ticks))) = *recorded {
        if recorded_pid == pid && live_start_ticks(pid) == Some(recorded_ticks) {
            return Some(DaemonExemption::Recorded);
        }
    }
    if let Some(exe) = process_executable(pid) {
        if executable_is_zeroclaw(&exe, daemon_binary) {
            return Some(DaemonExemption::Executable);
        }
    }
    None
}

/// `(pid, start_ticks)` for a live process, for the CLI to record as the
/// daemon's identity.
///
/// This is the crate's ONLY `/proc/<pid>/stat` reader: the CLI used to keep a
/// second, simpler one, which parsed from the last `)` and would have read the
/// wrong field on a kernel that renders a parenthesized symbol later in the
/// line. One parser means the daemon record and the reap agree on what an
/// identity is, and it inherits the tests that pin the tricky ones.
pub(crate) fn live_identity(pid: u32) -> Option<crate::DaemonIdentity> {
    live_start_ticks(pid).map(|start_ticks| (pid, start_ticks))
}

/// The live process's start time, or `None` if it is gone or unreadable. A
/// single `/proc` read: this is what turns "the record says 1234 started at
/// tick T" into "the process at 1234 still is that one".
///
/// A zombie reads as `None` for the same reason it reads as dead to
/// [`alive`]: it holds its pid and its start time but executes nothing, so
/// there is no daemon there for anybody to be spared from — and the scan
/// skips Z/X before selection anyway.
#[cfg(target_os = "linux")]
fn live_start_ticks(pid: u32) -> Option<u64> {
    let stat = read_stat(pid)?;
    if stat.state == "Z" || stat.state == "X" {
        return None;
    }
    Some(stat.start_ticks)
}

#[cfg(not(target_os = "linux"))]
fn live_start_ticks(_pid: u32) -> Option<u64> {
    None
}

/// Announce a spared daemon once. Silently skipping a process that matched
/// every ownership rule would make the reap log claim a clean workspace while
/// a shared daemon sat inside it; the honest form of that is one line naming
/// the pid and the reason.
fn report_spared_daemon(pid: u32, exemption: DaemonExemption) {
    if REPORTED_DAEMONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(pid)
    {
        eprintln!(
            "[zoder] left shared zeroclaw daemon pid={pid} running ({}); it serves every \
             zoder on this host, not just this run",
            exemption.reason()
        );
    }
}

/// Is `exe` the daemon binary this zoder spawns?
///
/// Not a name, and not a directory. The exemption matches the exact
/// executable zoder would have run (`locate_sibling("zeroclaw")`), so the two
/// ways a name check leaks are both closed: a copy an agent drops in the
/// workspace being reaped, and a different binary that merely happens to be
/// called `zeroclaw` and happens to sit in an install directory.
///
/// What is left is a process that replaced the daemon binary itself, which
/// stops every subsequent zoder run from finding an engine — a far larger
/// side effect than dodging one reap, and not worth defending against here.
fn executable_is_zeroclaw(exe: &Path, daemon_binary: &Path) -> bool {
    same_file(exe, daemon_binary)
}

/// Do two paths name the same file? Canonicalized when both resolve, so a
/// symlinked install (`~/.local/bin/zeroclaw` -> the real binary) and
/// `/proc/<pid>/exe`, which always names the resolved file, compare equal.
/// Falls back to the literal paths when either cannot be canonicalized — a
/// binary that was replaced under a running daemon, for instance, whose
/// `/proc` entry reads `<path> (deleted)`.
fn same_file(a: &Path, b: &Path) -> bool {
    if let (Ok(left), Ok(right)) = (a.canonicalize(), b.canonicalize()) {
        return left == right;
    }
    a == b
}

/// Sessions a cancel has already been sent for, so the async phase, the
/// blocking path and the final sweep never send the same one twice.
static CANCELLED: std::sync::LazyLock<Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

/// Sessions this process has already found the engine UNREACHABLE for, i.e.
/// whose cancel failed with ENOENT/ECONNREFUSED rather than going unconfirmed.
///
/// Membership is what makes the teardown converge. Retrying such a session is
/// not "being thorough", it is re-asking a socket that is not there: the 01a163c9
/// run burned eight cancel passes and four teardown rounds proving the engine
/// was gone, and then reported the agent as still working. The set is also what
/// keeps the operator's log to one line per session instead of one per attempt.
///
/// Not persisted and never cleared: an engine socket that is absent does not
/// reappear during a teardown, and a session id is unique per engine session.
static UNREACHABLE: std::sync::LazyLock<Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

/// Why a cancel could not be delivered. These are different facts about the
/// world and must never be logged, retried, or counted alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelFailure {
    /// The socket path does not exist, or nothing is listening on it. There is
    /// no engine to cancel against, so the session is not running: it died with
    /// the daemon that owned it.
    EngineUnreachable,
    /// The engine was reached and did not confirm the cancel — the ordinary
    /// "a turn that already settled does not acknowledge a cancel" case, which
    /// may still be a live turn and is therefore retried.
    NotConfirmed,
}

/// Does this connect error prove there is no listener, as opposed to merely
/// failing to reach one we can see?
///
/// Only ENOENT (the path is gone — zeroclaw's `EndpointGuard` unlinks it when
/// the daemon that bound it stops) and ECONNREFUSED (the entry is there but
/// nobody is bound) do. Everything else — EACCES on somebody else's socket, a
/// transport wrapper's own error, a connect timeout — leaves the engine
/// unproven, and the conservative answer is to keep treating the session as
/// possibly live.
fn io_error_proves_no_listener(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

/// Classify a `cancel_session` failure. The `io::Error` is inside the anyhow
/// chain (the transport adds "connecting to engine at <path>" as context), so
/// the chain is walked rather than the top-level message pattern-matched: a
/// message that merely mentions a missing path is not proof, while the errno is.
fn classify_cancel_error(error: &anyhow::Error) -> CancelFailure {
    for cause in error.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return if io_error_proves_no_listener(io) {
                CancelFailure::EngineUnreachable
            } else {
                CancelFailure::NotConfirmed
            };
        }
    }
    CancelFailure::NotConfirmed
}

/// Retire a session whose cancel could not be delivered because the engine was
/// unreachable. Returns true the FIRST time, which is the only time the caller
/// prints: "the engine is gone" is a fact about the run, not about the attempt.
fn mark_unreachable(id: &str) -> bool {
    UNREACHABLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.to_string())
}

/// Membership test, for the tests that assert a session was retired. The
/// production readers iterate the set inside a lock rather than pay for a
/// lookup per id.
#[cfg(test)]
fn engine_unreachable(id: &str) -> bool {
    UNREACHABLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(id)
}

/// Record a session id the agent is driving. Fed by the acp-client session
/// observer the moment the engine mints it; the exit path and the signal
/// handler read it back.
pub(crate) fn note_session(session_id: &str) {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return;
    }
    let mut seen = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    if seen.iter().any(|s| s == session_id) {
        return;
    }
    seen.push(session_id.to_string());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReapSignal {
    Term,
    Kill,
}

impl ReapSignal {
    fn name(self) -> &'static str {
        match self {
            ReapSignal::Term => "SIGTERM",
            ReapSignal::Kill => "SIGKILL",
        }
    }
}

/// One process the loop killed, for logging and for tests.
#[derive(Debug, Clone)]
pub(crate) struct Reaped {
    pub pid: u32,
    pub comm: String,
    pub cwd: PathBuf,
    pub signal: ReapSignal,
}

/// What "started after the loop began" means, plus where the agent was allowed
/// to work. Resolved once at `ReapGuard::begin` so the boundary cannot drift
/// while the loop runs.
#[derive(Debug, Clone)]
struct ReapPlan {
    roots: Vec<PathBuf>,
    /// The subset of `roots` zoder created, so it can be removed on the way
    /// out. `remove_dir` (not `remove_dir_all`) only ever succeeds on an
    /// empty directory.
    owned_roots: Vec<PathBuf>,
    started_ticks: u64,
    /// pid -> `starttime` for every process that existed when the run began.
    /// `starttime` is truncated to USER_HZ, so a process started a few
    /// milliseconds BEFORE the run can share the boundary tick exactly; this
    /// snapshot is the exact version of "started after the loop began", with
    /// no granularity window at all. The START TIME is stored, not just the
    /// pid, so a pid recycled onto a fresh process after the snapshot is
    /// judged on its own start time and stays eligible.
    preexisting: HashMap<u32, u64>,
    /// Where the shared daemon's pid is recorded (`<data dir>/daemon.pid`).
    /// Re-read on every scan rather than parsed once here, because the daemon
    /// is typically spawned *after* this plan exists.
    daemon_pid_file: PathBuf,
    /// The daemon executable THIS zoder would spawn (`locate_sibling
    /// ("zeroclaw")`). The executable exemption matches this exact file, not
    /// a name and not a directory: a copy an agent drops in the workspace
    /// being reaped is not the daemon, and neither is anything else that
    /// happens to be called `zeroclaw`.
    daemon_binary: PathBuf,
}

impl ReapPlan {
    /// `temp_base` is the system temp root (`TMPDIR`, usually `/tmp`). It is
    /// NOT itself a reap root: on a shared host every unrelated process has a
    /// cwd somewhere under `/tmp`, and "same uid + cwd in /tmp + started
    /// recently" describes all of them. Instead this run gets its own scratch
    /// directory inside it, and that directory is the boundary.
    ///
    /// `daemon_pid_file` and `daemon_binary` are carried in the plan rather
    /// than resolved from the environment here, so the exemption is testable
    /// against fixtures instead of the developer's real `~/.zoder`.
    fn new(
        cwd: &Path,
        temp_base: &Path,
        daemon_pid_file: PathBuf,
        daemon_binary: PathBuf,
    ) -> Option<Self> {
        // Order matters: the boot-tick boundary is read FIRST and the pid
        // snapshot second, so every process that existed before the boundary
        // is in the snapshot. (The other order leaves a window in which a
        // process started between the two reads is absent from the snapshot
        // AND newer than the boundary — i.e. looks like ours.)
        let started_ticks = boot_ticks()?;
        let preexisting = live_pids();
        let mut roots: Vec<PathBuf> = Vec::new();
        let mut owned_roots: Vec<PathBuf> = Vec::new();
        let workspace = absolute(cwd);
        let temp_abs = absolute(temp_base);
        // Refuse a workspace that IS the shared temp root or an ancestor of
        // it: `zoder loop -C /tmp` would otherwise make every same-uid process
        // on the host with a cwd under /tmp — every other build, every other
        // editor — a kill candidate. `/` and an empty path are refused for the
        // same reason. The run's own scratch dir stays in scope either way.
        let workspace_is_shared_temp = temp_abs.starts_with(&workspace);
        if !workspace.as_os_str().is_empty()
            && workspace.parent().is_some()
            && !workspace_is_shared_temp
        {
            roots.push(workspace);
        } else {
            eprintln!(
                "[zoder] not reaping -C {}: it is the shared temp root (or an ancestor of \
                 it), so a cwd there proves nothing about ownership; only this run's \
                 scratch directory is in scope. Pass --no-reap to silence this.",
                cwd.display()
            );
        }
        if let Some(scratch) = scratch_root(temp_base) {
            roots.push(scratch.clone());
            owned_roots.push(scratch);
        }
        if roots.is_empty() {
            return None;
        }
        Some(ReapPlan {
            roots,
            owned_roots,
            started_ticks,
            preexisting,
            daemon_pid_file,
            daemon_binary,
        })
    }

    fn owns_cwd(&self, cwd: &Path) -> bool {
        self.roots.iter().any(|root| cwd.starts_with(root))
    }

    /// Best-effort cleanup of the per-run scratch directory. Non-recursive, so
    /// this can never delete agent output: if anything is in there the call
    /// simply fails and the directory stays.
    fn cleanup_owned_roots(&self) {
        for dir in &self.owned_roots {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

fn absolute(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    abs.canonicalize().unwrap_or(abs)
}

/// A directory this run owns, inside the system temp root. Refuses a temp base
/// that is the filesystem root or has no parent: creating `//zoder-reap-...`
/// (or writing anywhere at all) is not an acceptable side effect of a reap
/// scope.
fn scratch_root(temp_base: &Path) -> Option<PathBuf> {
    let base = absolute(temp_base);
    if base.as_os_str().is_empty() || base.parent().is_none() {
        return None;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let dir = base.join(format!("zoder-reap-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    Some(absolute(&dir))
}

/// A process selected for reaping.
#[derive(Debug, Clone)]
struct Candidate {
    pid: u32,
    comm: String,
    cwd: PathBuf,
    /// The `starttime` the selection was made on. Re-checked immediately
    /// before every signal: a pid on its own is not an identity, because the
    /// kernel recycles pids and the grace window is exactly long enough for a
    /// busy host to recycle one.
    start_ticks: u64,
}

/// The loop's reaping scope. Created by `loop`/`rescue` right after the
/// workspace is known and dropped (or finished) on every exit path, including
/// a panic unwinding through it.
pub(crate) struct ReapGuard {
    plan: Option<ReapPlan>,
    finished: bool,
}

/// Translate the negative CLI flag into the guard's positive `enabled`
/// argument. Both loop and rescue use this single decision.
pub(crate) fn enabled_for_cli(cli: &crate::Cli) -> bool {
    !cli.no_reap
}

impl ReapGuard {
    /// Begin reaping for a run rooted at `cwd` (the `-C` directory) whose
    /// scratch space is `tmp`. `enabled=false` (`--no-reap`) yields a guard
    /// that does nothing at all, including no signal handling.
    pub(crate) fn begin(cwd: &Path, tmp: &Path, enabled: bool) -> Self {
        Self::begin_with(
            cwd,
            tmp,
            enabled,
            crate::daemon_pid_file_path(),
            crate::daemon_binary_path(),
        )
    }

    /// [`ReapGuard::begin`] with the daemon-identity inputs supplied by the
    /// caller. Only the tests need this: they must not read (or worse, write)
    /// a developer's real `~/.zoder/data/daemon.pid`, and they need the
    /// executable arm pointed at a fixture binary.
    pub(crate) fn begin_with(
        cwd: &Path,
        tmp: &Path,
        enabled: bool,
        daemon_pid_file: PathBuf,
        daemon_binary: PathBuf,
    ) -> Self {
        let plan = enabled
            .then(|| ReapPlan::new(cwd, tmp, daemon_pid_file, daemon_binary))
            .flatten();
        if let Some(plan) = plan.as_ref() {
            // Every session the engine mints is recorded the moment it exists,
            // not when its turn returns: a SIGTERM in the middle of a 900 s
            // author turn must be able to cancel THAT turn, and it has not
            // returned yet.
            zoder_core::engine_rpc::set_session_observer(note_session);
            install_signal_handler(plan.clone());
        }
        ReapGuard {
            plan,
            finished: false,
        }
    }

    /// True when this guard will (or did) kill processes.
    #[cfg(test)]
    fn armed(&self) -> bool {
        self.plan.is_some()
    }

    /// Cancel the agent sessions this run drove, THEN reap. Awaiting here is
    /// the point: the cancel stops the daemon from spawning more work while
    /// the scan decides what to kill.
    pub(crate) async fn cancel_then_reap(&mut self, budget: Duration) {
        if self.plan.is_none() || self.finished {
            return;
        }
        cancel_recorded_sessions(&crate::engine_socket_path(), budget).await;
        self.finish();
    }

    /// Reap now. Idempotent, and called from `Drop` so an unwinding panic
    /// still cleans up. See [`teardown_rounds`] for the cancel/reap alternation.
    pub(crate) fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let Some(plan) = self.plan.clone() else {
            return;
        };
        teardown_rounds(&plan, REAP_GRACE, &crate::engine_socket_path());
    }
}

/// Log every process we killed, by exact pid and command name, so an operator
/// can tell an agent's leftovers apart from anything else on the box. Failures
/// get their own lines: a cleanup that silently did not happen is the failure
/// this whole change exists to stop repeating.
fn log_outcome(outcome: &ReapOutcome) {
    for entry in &outcome.reaped {
        eprintln!(
            "[zoder] reaped orphan pid={} comm={} cwd={} via {}",
            entry.pid,
            entry.comm,
            entry.cwd.display(),
            entry.signal.name()
        );
    }
    for entry in &outcome.skipped {
        eprintln!(
            "[zoder] skipped pid={} comm={}: it exited or the pid was recycled before \
             any signal was sent",
            entry.pid, entry.comm
        );
    }
    for entry in &outcome.survivors {
        eprintln!(
            "[zoder] WARNING pid={} comm={} cwd={} survived SIGKILL (uninterruptible \
             sleep?); it was NOT reaped",
            entry.pid,
            entry.comm,
            entry.cwd.display()
        );
    }
    if !outcome.reaped.is_empty() {
        eprintln!(
            "[zoder] reaped {} process(es) left in the workspace by the agent",
            outcome.reaped.len()
        );
    }
}

impl Drop for ReapGuard {
    fn drop(&mut self) {
        if self.plan.is_none() {
            // `--no-reap` means NO cleanup, including this one. A user who
            // asked for the scope to be off must not get a surprise
            // `session/cancel` on the way out.
            return;
        }
        // `Drop` cannot await, so the cancel is sent the blocking way. The
        // ordering the module promises — stop the agent, then kill what it
        // left — holds on this path too, which matters because `Drop` is the
        // path a panic takes and a panic is exactly when a half-finished turn
        // is most likely to still be editing.
        cancel_sessions_blocking(&crate::engine_socket_path(), &pending_sessions());
        self.finish();
    }
}

/// Send `session/cancel` for every session this process drove, so a ghost
/// turn stops editing before we start killing. Best-effort: a turn that
/// already completed never acknowledges a cancel, and that is fine — we reap
/// either way.
///
/// `socket` is passed in rather than resolved from the environment so a test
/// can point this at a path that does not exist, which is the 01a163c9 case.
async fn cancel_recorded_sessions(socket: &Path, budget: Duration) {
    // The budget covers the WHOLE phase, not each session: a loop that drove
    // nine sessions must not spend nine budgets before the first kill.
    let deadline = Instant::now() + budget;
    // Drain repeatedly: the engine can mint a session while we are cancelling
    // (a fallback transition, a fresh turn), and that session is exactly the
    // one still editing the tree. One snapshot would leave it uncancelled.
    // Bounded as well as time-bounded: a deadline alone does not stop a loop
    // whose cancels all fail instantly while the engine keeps minting.
    for _ in 0..MAX_CANCEL_PASSES {
        // One lock per pass; a pass covers every session that appeared since
        // the last one.
        let (pending, still_pending) = {
            let cancelled = CANCELLED.lock().unwrap_or_else(|e| e.into_inner());
            let unreachable = UNREACHABLE.lock().unwrap_or_else(|e| e.into_inner());
            let sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
            let pending: Vec<String> = sessions
                .iter()
                .filter(|id| !cancelled.contains(*id) && !unreachable.contains(*id))
                .cloned()
                .collect();
            (pending.clone(), pending.len())
        };
        if pending.is_empty() {
            return;
        }
        for id in pending {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if still_pending > 0 {
                    eprintln!(
                        "[zoder] cancel budget exhausted with {still_pending} session(s) \
                         left to the reap"
                    );
                }
                return;
            }
            match zoder_core::cancel_session(socket, &id, remaining).await {
                Ok(()) => {
                    eprintln!("[zoder] cancelled agent session {id}");
                    // Only a cancel that actually went out is marked. Marking a
                    // failed one hides it from the blocking path and the later
                    // teardown rounds, and a session nobody cancelled is a
                    // session still editing the tree.
                    mark_cancelled(&id);
                }
                Err(e) => match classify_cancel_error(&e) {
                    CancelFailure::EngineUnreachable => {
                        // No engine, no cancel, and above all no live turn: the
                        // session died with the daemon that owned it. Retire it
                        // so no later pass or teardown round asks again, and so
                        // the reap is free to conclude the workspace is quiet.
                        // Said once — an absent socket is a fact about the run,
                        // not about the attempt.
                        if mark_unreachable(&id) {
                            eprintln!(
                                "[zoder] session {id} not cancelled: the engine at {} is \
                                 unreachable ({e:#}); the session ended with it and is not \
                                 treated as work still running",
                                socket.display()
                            );
                        }
                    }
                    CancelFailure::NotConfirmed => eprintln!(
                        "[zoder] session {id} cancel not confirmed ({e:#}); \
                         a turn that already settled does not acknowledge a cancel"
                    ),
                },
            }
        }
    }
}

/// Session ids that were never cancelled AND are not already known to be
/// unreachable — used for the final sweep and for the report, so "we cancelled
/// everything" is never claimed when it is not true, and so an unreachable
/// engine cannot keep the teardown spinning. Newest first: the session minted
/// last is the one still editing.
fn pending_sessions() -> Vec<String> {
    let cancelled = CANCELLED.lock().unwrap_or_else(|e| e.into_inner());
    let unreachable = UNREACHABLE.lock().unwrap_or_else(|e| e.into_inner());
    SESSIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .rev()
        .filter(|id| !cancelled.contains(*id) && !unreachable.contains(*id))
        .cloned()
        .collect()
}

fn mark_cancelled(id: &str) {
    CANCELLED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.to_string());
}

/// Fire-and-forget `session/cancel` over a blocking socket, for the one exit
/// path that cannot await: `Drop` during a panic unwind. It sends the cancel
/// notification for every session this run drove, reads briefly for the ack,
/// and returns. Ordering (cancel, then reap) is preserved because both happen
/// here, in order, on the same thread.
///
/// This is deliberately a thinner operation than [`zoder_core::cancel_session`]:
/// on a panicking path the goal is to make the daemon stop editing, not to
/// prove it acknowledged.
///
/// Returns the sessions this call found the engine unreachable for, so the
/// teardown can report "the engine was gone" from data instead of from the log.
/// A missing socket is no longer a silent `continue`: it is the 01a163c9
/// condition, and the operator needs to see it stated once rather than inferred
/// from an absence. A session that was already retired is not even attempted,
/// so this list holds each id at most once per process.
fn cancel_sessions_blocking(socket: &Path, ids: &[String]) -> Vec<String> {
    let mut unreachable: Vec<String> = Vec::new();
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::net::UnixStream;
        for id in ids {
            {
                // A session already cancelled OR already retired as
                // unreachable is settled: asking again can only re-print a
                // verdict this process has already reached. `pending_sessions`
                // filters both out; this is the backstop for a caller holding a
                // stale list, and it is what keeps a second report impossible.
                let cancelled = CANCELLED.lock().unwrap_or_else(|e| e.into_inner());
                let unreachable = UNREACHABLE.lock().unwrap_or_else(|e| e.into_inner());
                if cancelled.contains(id) || unreachable.contains(id) {
                    continue;
                }
            }
            let mut stream = match UnixStream::connect(socket) {
                Ok(stream) => stream,
                Err(error) => {
                    // No daemon, no cancel: nothing is left editing. ENOENT /
                    // ECONNREFUSED PROVE that and retire the session, so no
                    // later round asks again; anything else leaves the engine
                    // unproven and the session stays pending.
                    if io_error_proves_no_listener(&error) && mark_unreachable(id) {
                        eprintln!(
                            "[zoder] session {id} not cancelled: the engine at {} is \
                             unreachable ({error}); the session ended with it and is not \
                             treated as work still running",
                            socket.display()
                        );
                        unreachable.push(id.clone());
                    }
                    continue;
                }
            };
            let _ = stream.set_read_timeout(Some(BLOCKING_ACK_WAIT));
            let _ = stream.set_write_timeout(Some(BLOCKING_ACK_WAIT));
            let initialized = send_frame(
                &mut stream,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": "init",
                    "method": "initialize",
                    "params": { "protocol_version": zoder_core::engine_rpc::ACP_PROTOCOL_VERSION },
                }),
            )
            .is_ok();
            if !initialized {
                continue;
            }
            // The daemon wants an initialize round trip before anything else.
            let mut handshake = [0u8; 4096];
            let _ = stream.read(&mut handshake);
            let sent = send_frame(
                &mut stream,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/cancel",
                    "params": { "session_id": id },
                }),
            )
            .is_ok();
            if sent {
                let mut ack = [0u8; 4096];
                let _ = stream.read(&mut ack);
                eprintln!("[zoder] cancelled agent session {id} (blocking, best effort)");
                // Only a cancel that reached the socket counts. A session we
                // failed to tell about stays pending, so a later round tries
                // again rather than pretending the agent was stopped.
                mark_cancelled(id);
            } else {
                eprintln!("[zoder] could not deliver a cancel for session {id}");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, ids);
    }
    unreachable
}

/// Write one NDJSON frame. Used by the blocking cancel only.
#[cfg(unix)]
fn send_frame(
    stream: &mut std::os::unix::net::UnixStream,
    frame: serde_json::Value,
) -> std::io::Result<()> {
    use std::io::Write;
    // Serialization of a fixed literal cannot realistically fail, but if it
    // did, sending an empty line would hand the daemon a malformed frame. Fail
    // the write instead.
    let mut line = serde_json::to_string(&frame)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    stream.write_all(line.as_bytes())
}

/// What one reap pass did. `survivors` and `skipped` exist so the log tells
/// the truth: a process we failed to kill, or one whose pid was recycled under
/// us, is reported as such instead of being counted as reaped.
#[derive(Debug, Default)]
struct ReapOutcome {
    reaped: Vec<Reaped>,
    /// Signalled and still running when the pass ended.
    survivors: Vec<Reaped>,
    /// Selected, then found gone (or replaced) before we could signal.
    skipped: Vec<Reaped>,
}

impl ReapOutcome {
    /// Nothing selected, nothing signalled, nothing left: the workspace is
    /// clean as far as this reaper can tell.
    fn quiet(&self) -> bool {
        self.reaped.is_empty() && self.skipped.is_empty() && self.survivors.is_empty()
    }

    /// This pass found new processes AND signalled them. That is the only
    /// evidence the reap has that the agent is still producing work: a pass
    /// that selected nothing found nothing, no matter what the cancel phase
    /// was doing. `skipped` alone does not count — those processes were gone
    /// before any signal, so they are not work being produced.
    fn signalled_new_work(&self) -> bool {
        !self.reaped.is_empty() || !self.survivors.is_empty()
    }
}

/// What a teardown concluded, kept as data so the log and the tests read the
/// same thing instead of the tests scraping stderr.
#[derive(Debug, Default, PartialEq, Eq)]
struct TeardownReport {
    /// How many cancel/reap rounds actually ran. The 01a163c9 bug cost four of
    /// them proving an absent socket was absent.
    rounds: usize,
    /// Sessions whose cancel could not be delivered because the engine socket
    /// was not there. Deduplicated by the callers, so this is one entry per
    /// session and never per attempt.
    unreachable_sessions: Vec<String>,
    /// The final round's reap pass found and signalled new processes, i.e. the
    /// agent really was still producing work when teardown ran out of rounds.
    /// False for a teardown whose only unfinished business was a cancel the
    /// engine could not receive.
    producing_work: bool,
}

/// Cancel and reap, alternating, until a round does neither.
///
/// Every exit path funnels through here — the normal return, the `Drop` during
/// a panic unwind, and the SIGTERM/SIGINT handler — because they must not be
/// allowed to drift apart. The alternation exists because reaping takes
/// seconds: the engine can mint a session during it and that session can fork
/// children after the last scan, so one cancel followed by one reap is not a
/// teardown.
///
/// `socket` is a parameter for the same reason [`cancel_sessions_blocking`]'s
/// is: the absent-socket path (01a163c9) is only reachable by pointing a test
/// at a path that does not exist, and a test must not mutate `ZEROCLAW_SOCKET`
/// to arrange that.
fn teardown_rounds(plan: &ReapPlan, grace: Duration, socket: &Path) -> TeardownReport {
    let mut report = TeardownReport::default();
    for round in 0..MAX_TEARDOWN_ROUNDS {
        let cancelled = pending_sessions();
        let unreachable = cancel_sessions_blocking(socket, &cancelled);
        let outcome = reap(plan, grace);
        log_outcome(&outcome);
        report.rounds += 1;
        report.unreachable_sessions.extend(unreachable);
        // Convergence is "nothing left to cancel AND nothing left to kill",
        // and both halves are read AFTER this round's work rather than from the
        // snapshot taken before it: a cancel this round delivered and a session
        // the engine minted during the reap both have to count, or the loop
        // either runs a pointless round or exits without seeing a new session.
        // The first half is what 01a163c9 got wrong: an engine that is not there
        // is not a session still running, and treating it as one is what kept
        // this loop going for four rounds.
        if pending_sessions().is_empty() && outcome.quiet() {
            plan.cleanup_owned_roots();
            return report;
        }
        // The warning is about WORK, so it is gated on work: the final round's
        // reap pass having found and signalled something new. Running out of
        // rounds while the cancel was talking to a dead socket is not evidence
        // of anything, and saying so was the second half of the 01a163c9
        // mis-report.
        if round + 1 == MAX_TEARDOWN_ROUNDS {
            report.producing_work = outcome.signalled_new_work();
            if report.producing_work {
                eprintln!(
                    "[zoder] WARNING: teardown stopped after {MAX_TEARDOWN_ROUNDS} rounds \
                     with the agent still producing work; leftovers may remain"
                );
            }
        }
    }
    plan.cleanup_owned_roots();
    report
}

/// A pidfd pins ONE process identity. Once it is open, every signal sent
/// through it reaches that process and nothing else — if the pid is recycled
/// in the meantime, the signal is simply refused (ESRCH) instead of landing on
/// a stranger. This is what makes "check identity, then signal" atomic
/// instead of merely adjacent. Kernels without pidfd (pre-5.3) fall back to
/// plain pid signalling, where the identity re-check is all we have.
struct PidFd {
    fd: libc::c_int,
}

impl PidFd {
    #[cfg(target_os = "linux")]
    fn open(pid: u32) -> Option<Self> {
        // SAFETY: pidfd_open takes (pid, flags) and returns a new fd or -1.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_pidfd_open as libc::c_long,
                pid as libc::pid_t,
                0 as libc::c_uint,
            )
        };
        if fd < 0 {
            return None;
        }
        Some(PidFd {
            fd: fd as libc::c_int,
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn open(_pid: u32) -> Option<Self> {
        None
    }

    /// Send `sig` to the pinned process. `sig == 0` is the liveness probe: it
    /// succeeds while the process is alive and fails with ESRCH once it is
    /// gone, which is exactly the check the reap loop needs.
    #[cfg(target_os = "linux")]
    fn signal(&self, sig: i32) -> bool {
        // SAFETY: pidfd_send_signal takes (pidfd, sig, info, flags).
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal as libc::c_long,
                self.fd as libc::c_long,
                sig as libc::c_int,
                std::ptr::null::<libc::siginfo_t>(),
                0 as libc::c_uint,
            ) == 0
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn signal(&self, _sig: i32) -> bool {
        false
    }

    fn alive(&self) -> bool {
        self.signal(0)
    }
}

#[cfg(target_os = "linux")]
impl Drop for PidFd {
    fn drop(&mut self) {
        // SAFETY: `fd` is owned by this struct and closed exactly once.
        unsafe {
            libc::close(self.fd);
        }
    }
}

/// A selected process plus, when the kernel supports it, a pinned identity.
struct Target {
    candidate: Candidate,
    pin: Option<PidFd>,
}

impl Target {
    fn deliver(&self, sig: i32) {
        match self.pin.as_ref() {
            Some(pin) => {
                pin.signal(sig);
            }
            // No pidfd: fall back to the pid, having just confirmed identity.
            None => signal(self.candidate.pid, sig),
        }
    }

    /// "Still able to run". The pin is authoritative about WHICH process this
    /// is; `/proc` is authoritative about whether a task can still run at all
    /// (a zombie holds both its pid and its pidfd but executes nothing).
    fn alive(&self) -> bool {
        match self.pin.as_ref() {
            Some(pin) => pin.alive() && alive(self.candidate.pid),
            None => alive(self.candidate.pid),
        }
    }
}

/// SIGTERM, grace, SIGKILL — by exact PID, never by name or pattern.
fn reap(plan: &ReapPlan, grace: Duration) -> ReapOutcome {
    let uid = current_uid();
    let mut outcome = ReapOutcome::default();
    // Keyed by pid AND start time: a pid recycled onto a process that appeared
    // during this reap is a different process and must not be written off as
    // "already handled".
    let mut seen: HashSet<(u32, u64)> = HashSet::new();
    // Keep scanning until a pass finds nothing new: a parent can fork again
    // while the previous pass is still signalling, and a fixed two-pass loop
    // leaves the last descendants behind. Bounded so an agent that forks
    // forever cannot stall teardown.
    let mut bounded_out = false;
    for _ in 0..MAX_REAP_PASSES {
        let candidates: Vec<Candidate> = scan(plan)
            .into_iter()
            .filter(|c| seen.insert((c.pid, c.start_ticks)))
            .collect();
        if candidates.is_empty() {
            break;
        }
        let mut targets: Vec<Target> = Vec::new();
        for candidate in &candidates {
            // Pin FIRST, then validate. The order is the whole point: if the
            // pid was recycled between the scan and here, the pin now holds the
            // REPLACEMENT, and the identity check below reads /proc for that
            // same pid, sees a start time that is not ours, and drops the pin
            // without signalling. Pinning after the check would leave exactly
            // that window open.
            let pin = PidFd::open(candidate.pid);
            if confirm_identity(candidate, uid) != Identity::Same {
                // Gone, or the pid belongs to somebody else. No signal was sent,
                // and the log must not claim otherwise.
                drop(pin);
                outcome.skipped.push(entry(candidate, ReapSignal::Term));
                continue;
            }
            targets.push(Target {
                candidate: candidate.clone(),
                pin,
            });
        }
        if targets.is_empty() {
            continue;
        }
        for target in &targets {
            target.deliver(libc_sigterm());
        }
        // Counts targets we signalled, not targets we killed: a survivor is
        // still evidence that the agent is producing work.
        let mut signalled_this_pass = 0usize;
        let deadline = Instant::now() + grace;
        while targets.iter().any(Target::alive) && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        for target in &targets {
            let mut record = entry(&target.candidate, ReapSignal::Term);
            // "Gone" and "its pid now belongs to somebody else" mean the same
            // thing here: the process we selected is no longer running, and it
            // died because we asked it to. A process that obeyed SIGTERM is
            // gone from /proc, which must NOT be reported as skipped.
            signalled_this_pass += 1;
            if !target.alive() || confirm_identity(&target.candidate, uid) != Identity::Same {
                // "Gone" includes "the pid was recycled while we waited": either
                // way the process we selected is no longer running. Without a
                // pidfd, this identity check is what stops the SIGKILL below
                // from landing on the replacement.
                outcome.reaped.push(record);
                continue;
            }
            target.deliver(libc_sigkill());
            let kill_deadline = Instant::now() + KILL_WAIT;
            while target.alive() && Instant::now() < kill_deadline {
                std::thread::sleep(POLL);
            }
            record.signal = ReapSignal::Kill;
            // SIGKILL is uncatchable, so "still here" means the kernel is
            // holding it (uninterruptible sleep, a wedged D-state task) — a
            // real, visible failure rather than a silent success claim.
            if target.alive() {
                outcome.survivors.push(record);
            } else {
                outcome.reaped.push(record);
            }
        }
        if signalled_this_pass > 0 {
            // Let whatever the agent forks next appear before we decide the
            // workspace is clean.
            std::thread::sleep(PASS_SETTLE);
        }
        bounded_out = !scan(plan)
            .iter()
            .all(|c| seen.contains(&(c.pid, c.start_ticks)));
    }
    if bounded_out {
        // Say so rather than pretending the workspace is clean: something is
        // still forking faster than the passes can catch it.
        eprintln!(
            "[zoder] WARNING: stopped after {MAX_REAP_PASSES} reap passes with candidates \
             still appearing; the agent may still be forking new processes"
        );
    }
    outcome
}

fn entry(target: &Candidate, signal: ReapSignal) -> Reaped {
    Reaped {
        pid: target.pid,
        comm: target.comm.clone(),
        cwd: target.cwd.clone(),
        signal,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Identity {
    /// The process we selected is still the process at this pid.
    Same,
    /// It exited (nothing to signal) or the pid was recycled onto another
    /// process, which is NOT ours to kill.
    Recycled,
}

/// Re-check, immediately before signalling, that the pid still belongs to the
/// process the scan selected. `starttime` is the identity: no process can
/// inherit another one's start time, so a mismatch means the pid was reused
/// and the new tenant must not be signalled on the old one's behalf.
fn confirm_identity(target: &Candidate, uid: u32) -> Identity {
    match read_stat(target.pid) {
        Some(stat) if stat.start_ticks == target.start_ticks => {
            if stat.state == "Z" || stat.state == "X" || proc_uid(target.pid) != Some(uid) {
                Identity::Recycled
            } else {
                Identity::Same
            }
        }
        // No stat at all: it exited. Signalling a pid that may since have been
        // recycled is exactly the mistake this check exists to prevent.
        _ => Identity::Recycled,
    }
}

#[cfg(target_os = "linux")]
fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(not(target_os = "linux"))]
fn current_uid() -> u32 {
    0
}

/// SIGTERM/SIGINT while a loop is running: cancel the session, reap, exit.
/// Without this the signal kills zoder outright — no cancel, no reap, and the
/// agent's children are reparented to init (ppid 1) exactly as in the 01a1601b
/// incident.
fn install_signal_handler(plan: ReapPlan) {
    use std::sync::Arc;
    use tokio::signal::unix::{signal, SignalKind};
    let plan = Arc::new(plan);
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    // Exactly one handler per process. Two armed guards would otherwise leave
    // two tasks racing on the same signal, and the second `std::process::exit`
    // would truncate the first one's teardown. A zoder process runs one
    // loop/rescue, so the first armed scope owns the signal path.
    static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if INSTALLED.set(()).is_err() {
        return;
    }
    // Register the streams HERE, not inside the spawned task: a task that has
    // not been polled yet has not installed the handler, and a SIGTERM landing
    // in that window would take the default disposition — die immediately, no
    // cancel, no reap, children reparented to init. That window is exactly the
    // failure this change exists to close.
    // A half-installed handler is worse than none: once tokio has claimed a
    // signal, the default disposition is gone, so returning early would leave
    // SIGTERM swallowed with nothing behind it. Whatever registers gets a
    // task.
    let term = match signal(SignalKind::terminate()) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("[zoder] SIGTERM handler unavailable ({e}); a signalled run will not reap");
            None
        }
    };
    let intr = match signal(SignalKind::interrupt()) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("[zoder] SIGINT handler unavailable ({e}); Ctrl-C will not reap");
            None
        }
    };
    if term.is_none() && intr.is_none() {
        eprintln!("[zoder] no signal handler could be installed; reaping is signal-blind");
        return;
    }
    tokio::spawn(async move {
        let (name, code) = match (term, intr) {
            (Some(mut term), Some(mut intr)) => {
                tokio::select! {
                    _ = term.recv() => ("SIGTERM", 143),
                    _ = intr.recv() => ("SIGINT", 130),
                }
            }
            (Some(mut term), None) => {
                term.recv().await;
                ("SIGTERM", 143)
            }
            (None, Some(mut intr)) => {
                intr.recv().await;
                ("SIGINT", 130)
            }
            (None, None) => return,
        };
        eprintln!(
            "[zoder] {name} received: cancelling the agent session and reaping \
             workspace processes before exit"
        );
        // Same teardown as every other exit path, with the shorter budgets a
        // signalled run can afford. One cancel followed by one scan is not a
        // teardown: the engine can mint a session while the reap is running.
        cancel_recorded_sessions(&crate::engine_socket_path(), SIGNAL_CANCEL_BUDGET).await;
        teardown_rounds(&plan, SIGNAL_REAP_GRACE, &crate::engine_socket_path());
        std::process::exit(code);
    });
}

// ---------------------------------------------------------------------------
// /proc access. Linux only; everything above is a no-op elsewhere.
// ---------------------------------------------------------------------------

struct ProcStat {
    state: String,
    comm: String,
    ppid: u32,
    start_ticks: u64,
}

/// Parse `/proc/<pid>/stat`. Field 2 (`comm`) is parenthesized and may itself
/// contain spaces AND parentheses, so the closing paren cannot be found with
/// `rfind(')')`: a process named `foo)bar` would end the parse early. The
/// closing paren is the first one followed by the state field — a single
/// letter from `/proc`'s state set followed by whitespace.
/// Parse `/proc/<pid>/stat`. Field 2 (`comm`) is parenthesized and may contain
/// spaces AND parentheses, so the closing paren has to be found without
/// assuming a well-behaved command name:
///
/// * the LAST `)` followed by the state field wins. That is correct when comm
///   itself contains `)` (`42 (worker) R) S 7 ...` — the embedded `)` is
///   followed by `) S`, which is not a state field, and the real close is the
///   final candidate.
/// * the FIRST such `)` is the fallback, for the other direction: a kernel that
///   renders `wchan` symbolically puts a paren later in the line.
/// * a candidate must have the state field (one letter from /proc's state set)
///   followed by whitespace AND enough remaining fields to reach `starttime`.
fn parse_stat(raw: &str) -> Option<ProcStat> {
    let open = raw.find('(')?;
    let after_open = raw.get(open + 1..)?;
    let close = after_open
        .char_indices()
        .rev()
        .find(|(i, c)| *c == ')' && valid_stat_tail(&after_open[i + 1..]))
        .map(|(i, _)| i)?;
    let comm = after_open.get(..close)?.to_string();
    // The kernel caps a command name at TASK_COMM_LEN (16 bytes including the
    // NUL), i.e. 15 visible characters. That bound is what makes the backward
    // search safe: a comm could only fool it by containing `) S ` plus twenty
    // numeric fields, which cannot fit. A longer "comm" means the line is not
    // a stat line at all.
    if comm.len() > TASK_COMM_LEN - 1 {
        return None;
    }
    let rest = after_open.get(close + 1..)?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `fields[0]` is stat field 3 (state), so stat field N is `fields[N - 3]`.
    let state = fields.first()?.to_string();
    let ppid = fields.get(1)?.parse().ok()?;
    let start_ticks = fields.get(19)?.parse().ok()?;
    Some(ProcStat {
        state,
        comm,
        ppid,
        start_ticks,
    })
}

/// Does the text after a candidate `)` start a plausible stat tail?
fn valid_stat_tail(tail: &str) -> bool {
    const STATES: &str = "RSDZTtWXxKPI";
    let tail = tail.trim_start();
    let mut chars = tail.chars();
    let Some(state) = chars.next() else {
        return false;
    };
    STATES.contains(state)
        && chars.as_str().starts_with(' ')
        && tail.split_whitespace().count() >= 20
}

#[cfg(target_os = "linux")]
fn read_stat(pid: u32) -> Option<ProcStat> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(&raw)
}

/// Real uid of `pid`, from `/proc/<pid>/status`. Compared against `getuid()`
/// so we never touch another user's processes — including root's.
#[cfg(target_os = "linux")]
fn proc_uid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("Uid:") else {
            continue;
        };
        return rest.split_whitespace().next()?.parse().ok();
    }
    None
}

/// Our own pid and every ancestor, up to pid 1. A reaper that cannot exclude
/// itself is a foot-gun: `/proc/<self>/cwd` is the workspace.
#[cfg(target_os = "linux")]
fn protected_pids() -> HashSet<u32> {
    let mut protected = HashSet::new();
    let mut cursor = std::process::id();
    loop {
        if cursor <= 1 || !protected.insert(cursor) {
            break;
        }
        let Some(stat) = read_stat(cursor) else {
            break;
        };
        if stat.ppid == cursor {
            break;
        }
        cursor = stat.ppid;
    }
    protected
}

/// The boot-relative clock-tick instant of "now". `/proc/uptime` is seconds
/// since boot and `USER_HZ` ticks per second, so this is the same clock as
/// `/proc/<pid>/stat`'s `starttime` and the two compare directly — no boot
/// time, no wall clock, nothing to drift.
#[cfg(target_os = "linux")]
fn boot_ticks() -> Option<u64> {
    let uptime: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return None;
    }
    let ticks = uptime * hz as f64;
    if ticks <= 0.0 {
        return None;
    }
    Some(ticks as u64)
}

/// Every live process right now, as pid -> `starttime`. Read once per run.
#[cfg(target_os = "linux")]
fn live_pids() -> HashMap<u32, u64> {
    let mut pids = HashMap::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            if pid <= 1 {
                continue;
            }
            if let Some(stat) = read_stat(pid) {
                pids.insert(pid, stat.start_ticks);
            }
        }
    }
    pids
}

#[cfg(not(target_os = "linux"))]
fn live_pids() -> HashMap<u32, u64> {
    HashMap::new()
}

/// Every process the loop may kill. Ordered by pid so the log is stable.
#[cfg(target_os = "linux")]
fn scan(plan: &ReapPlan) -> Vec<Candidate> {
    let uid = current_uid();
    let protected = protected_pids();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found: Vec<Candidate> = Vec::new();
    // Resolved once per scan, not once per pid: the pid file is a single read
    // for the whole table, and only when something could actually match.
    let mut recorded_daemon = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        // pid 1 is init; `protected` covers zoder itself and its ancestors.
        if pid <= 1 || protected.contains(&pid) {
            continue;
        }
        // The shared zeroclaw daemon is not this run's to kill, however it
        // came to match the rules below: it serves every zoder on the host.
        // Checked by identity only (spawned pid, recorded pid, executable).
        if let Some(exemption) = daemon_exemption(
            pid,
            &mut recorded_daemon,
            &plan.daemon_pid_file,
            &plan.daemon_binary,
        ) {
            report_spared_daemon(pid, exemption);
            continue;
        }
        let Some(stat) = read_stat(pid) else {
            continue;
        };
        // A zombie is already dead and only holds a pid slot until its parent
        // reaps it. Signalling one does nothing.
        if stat.state == "Z" || stat.state == "X" {
            continue;
        }
        // Started before the loop did: not ours to kill, however it got a cwd
        // inside the workspace. Both checks are needed, and neither alone is
        // exact: `starttime` is truncated to USER_HZ (so a process started a
        // few ms before the run can share the boundary tick), and the pid
        // snapshot is taken once (so a pid recycled onto a fresh process after
        // the snapshot is caught by the tick test).
        // The snapshot only speaks for processes that were ALREADY OLD when
        // the boundary was read. A process the snapshot caught after that read
        // belongs to this run and stays eligible.
        //
        // The boundary tick itself is the ambiguous case: `starttime` has
        // 10 ms granularity, so a process that started microseconds before the
        // run and one that started microseconds after it share a tick. The tie
        // breaks toward INCLUSION, because the failure mode being fixed is a
        // leak nobody will ever clean up, while the cost of the other side is
        // one same-uid process in this workspace that started within 10 ms of
        // the run - which, in practice, is an agent child anyway.
        let same_process_as_at_scope_open = plan
            .preexisting
            .get(&pid)
            .is_some_and(|seen| *seen == stat.start_ticks && *seen < plan.started_ticks);
        if stat.start_ticks < plan.started_ticks || same_process_as_at_scope_open {
            continue;
        }
        if proc_uid(pid) != Some(uid) {
            continue;
        }
        // An unreadable cwd means we cannot prove it is ours. Never guess.
        let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) else {
            continue;
        };
        let cwd = strip_deleted_suffix(&cwd);
        if !plan.owns_cwd(&cwd) {
            continue;
        }
        found.push(Candidate {
            pid,
            comm: stat.comm,
            cwd,
            start_ticks: stat.start_ticks,
        });
    }
    found.sort_by_key(|c| c.pid);
    found
}

/// A deleted directory shows up as `/path/to/dir (deleted)` in
/// `/proc/<pid>/cwd`; strip the suffix so the path still compares as itself.
#[cfg(target_os = "linux")]
fn strip_deleted_suffix(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    match text.strip_suffix(" (deleted)") {
        Some(stripped) => PathBuf::from(stripped),
        None => path.to_path_buf(),
    }
}

/// `/proc/<pid>/exe`, with the "(deleted)" suffix a replaced binary leaves
/// behind stripped — an install that swaps the file out from under a running
/// daemon must not stop that daemon from being recognised.
///
/// An unreadable link (a process that dropped `dumpable`, a pid mid-exit)
/// reads as "no executable", which is the safe answer here only because the
/// other two arms still apply.
#[cfg(target_os = "linux")]
fn process_executable(pid: u32) -> Option<PathBuf> {
    let raw = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    Some(strip_deleted_suffix(&raw))
}

#[cfg(not(target_os = "linux"))]
fn process_executable(_pid: u32) -> Option<PathBuf> {
    None
}

/// Alive means "can still run": a missing pid, or one in Z/X state, is dead.
/// `kill(pid, 0)` says a zombie is alive, which would make every reap burn
/// the full grace waiting for a reap that may never come.
#[cfg(target_os = "linux")]
fn alive(pid: u32) -> bool {
    match read_stat(pid) {
        Some(stat) => stat.state != "Z" && stat.state != "X",
        None => false,
    }
}

#[cfg(target_os = "linux")]
fn libc_sigterm() -> i32 {
    libc::SIGTERM
}

#[cfg(target_os = "linux")]
fn libc_sigkill() -> i32 {
    libc::SIGKILL
}

#[cfg(target_os = "linux")]
fn signal(pid: u32, sig: i32) {
    // Exact pid, always. There is no `pkill`/`pgrep` fallback anywhere here:
    // a name pattern would match processes this loop never started.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

// --------------------------------------------------------------------------
// Non-Linux: no /proc, so nothing is ever selected and `reap` finds no
// candidates. The module still has to COMPILE there — every helper the shared
// code path calls needs an arm, or the crate breaks the moment someone builds
// zoder on a mac. `confirm_identity` returning `Recycled` is the safe answer:
// with no candidates there is nothing to signal anyway.
// --------------------------------------------------------------------------

#[cfg(not(target_os = "linux"))]
fn read_stat(_pid: u32) -> Option<ProcStat> {
    None
}

#[cfg(not(target_os = "linux"))]
fn proc_uid(_pid: u32) -> Option<u32> {
    None
}

#[cfg(not(target_os = "linux"))]
fn boot_ticks() -> Option<u64> {
    None
}

#[cfg(not(target_os = "linux"))]
fn scan(_plan: &ReapPlan) -> Vec<Candidate> {
    Vec::new()
}

#[cfg(not(target_os = "linux"))]
fn alive(_pid: u32) -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
fn libc_sigterm() -> i32 {
    0
}

#[cfg(not(target_os = "linux"))]
fn libc_sigkill() -> i32 {
    0
}

#[cfg(not(target_os = "linux"))]
fn signal(_pid: u32, _sig: i32) {}
// ---------------------------------------------------------------------------
// Regression tests: the 2026-10-21 HYDRA orphan incident (hive job 01a1601b).
// All Linux-only: they read /proc and spawn real processes.
// ---------------------------------------------------------------------------

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::Command;

    /// These tests spawn real processes under real roots, and `cargo test`
    /// runs a crate's tests in parallel threads inside ONE process: without
    /// this lock one test's reap scope would select another test's fixture.
    static REAP_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct Workspace {
        _dir: tempfile::TempDir,
        cd: PathBuf,
        tmp_root: PathBuf,
    }

    impl Workspace {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let cd = dir.path().join("workspace");
            let tmp_root = dir.path().join("scratch");
            let outside = dir.path().join("elsewhere");
            std::fs::create_dir_all(&cd).expect("workspace dir");
            std::fs::create_dir_all(&tmp_root).expect("scratch dir");
            std::fs::create_dir_all(&outside).expect("outside dir");
            Workspace {
                _dir: dir,
                cd,
                tmp_root,
            }
        }

        fn outside(&self) -> PathBuf {
            self._dir.path().join("elsewhere")
        }

        /// A daemon-pid file that does NOT exist, for plans whose tests are
        /// about something else. The developer's real `~/.zoder/data/
        /// daemon.pid` must never decide whether a fixture is a candidate.
        fn daemon_pid_file(&self) -> PathBuf {
            self._dir.path().join("absent-daemon.pid")
        }

        /// The tempdir root itself — where fixture binaries are installed. It
        /// is deliberately NOT a reap root, so a fixture executable parked
        /// here can never be mistaken for the workspace. It doubles as the
        /// fixture "install directory" the executable exemption trusts.
        fn root(&self) -> PathBuf {
            self._dir.path().to_path_buf()
        }

        /// The daemon binary the fixture installs, which the executable arm
        /// is pointed at. Pointing the plan at the tempdir copy keeps the
        /// test hermetic: nothing is written to, or read from, the
        /// developer's actual `~/.local/bin`.
        fn daemon_binary(&self) -> PathBuf {
            self.root().join("zeroclaw")
        }

        /// Open a guard whose daemon-identity inputs are fixtures rather than
        /// this machine's real `~/.zoder`.
        fn begin(&self) -> ReapGuard {
            ReapGuard::begin_with(
                &self.cd,
                &self.tmp_root,
                true,
                self.daemon_pid_file(),
                self.daemon_binary(),
            )
        }
    }

    /// Install a REAL executable named `zeroclaw`.
    ///
    /// A symlink would not do: `/proc/<pid>/exe` resolves to the file the
    /// kernel actually executed, so a link called `zeroclaw` still reads as
    /// whatever it points at. Copying the binary is what makes the
    /// executable-identity arm see in a test exactly what it sees in
    /// production.
    ///
    /// The source is `/bin/sh` on purpose. `/bin/sleep` on some hosts is a
    /// symlink into a multi-call `coreutils` binary that dispatches on
    /// argv[0]: copied to `zeroclaw` it prints "unknown program" and exits,
    /// which is a fixture that dies on its own and tests nothing. `dash` runs
    /// the same whatever it is called.
    fn install_fake_zeroclaw(root: &Path, source: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let exe = root.join("zeroclaw");
        std::fs::copy(source, &exe).unwrap_or_else(|e| panic!("copy {source} to {exe:?}: {e}"));
        let mut perms = std::fs::metadata(&exe)
            .expect("stat the copy")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&exe, perms).expect("make the copy executable");
        exe
    }

    /// A shell body that backgrounds one long-lived child, records its pid, and
    /// then stays alive — so the "daemon" is still running (and still the
    /// parent of an agent leftover) when the reap does its scan.
    fn daemon_script(pidfile: &Path) -> String {
        format!("sleep 600 & echo $! > {}; wait", pidfile.display())
    }

    /// A daemon fixture that has NO child of its own inside the reap roots: the
    /// one helper it runs is moved to `helper_cwd` (outside `-C`) before it
    /// execs. That is what a shared daemon actually looks like — its own
    /// machinery lives outside the job workspace — and it keeps the fixture
    /// alive across the reap, so a test can assert the daemon SURVIVED rather
    /// than infer it from a selection list.
    fn neutral_daemon_script(pidfile: &Path, helper_cwd: &Path) -> String {
        format!(
            "( cd '{}' && exec sleep 600 ) & echo $! > {}; wait",
            helper_cwd.display(),
            pidfile.display()
        )
    }

    /// Spawn the daemon fixture with its cwd in `cwd`, and wait until the whole
    /// shape is up: the daemon running in `cwd`, its helper recorded, the
    /// helper out in `helper_cwd`. Returns (daemon, helper).
    fn spawn_fake_daemon(
        exe: &Path,
        cwd: &Path,
        helper_cwd: &Path,
        pidfile: &Path,
    ) -> (std::process::Child, u32) {
        let daemon = Command::new(exe)
            .arg("-c")
            .arg(neutral_daemon_script(pidfile, helper_cwd))
            .current_dir(cwd)
            .spawn()
            .expect("spawn the fake daemon");
        let daemon_pid = daemon.id();
        let helper = read_pidfile(pidfile);
        wait_until(
            || {
                alive(daemon_pid)
                    && alive(helper)
                    && proc_cwd(helper).as_deref() == Some(helper_cwd)
            },
            "the fake daemon and its out-of-scope helper to settle",
        );
        (daemon, helper)
    }

    fn read_pidfile(path: &Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(raw) = std::fs::read_to_string(path) {
                if let Ok(pid) = raw.trim().parse::<u32>() {
                    return pid;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {} to name a pid",
                path.display()
            );
            std::thread::sleep(POLL);
        }
    }

    /// Spawn a long-lived child with its cwd inside `cwd` and return its pid.
    /// The caller owns it and must `kill_exact` it unless the reaper took it.
    // The fixtures are deliberately NOT waited on: a reaped child stays a
    // zombie until its parent reaps it, and this test process is about to
    // exit. Waiting here would race the reaper for the very process the test
    // is trying to observe.
    #[allow(clippy::zombie_processes)]
    fn spawn_sleeper(cwd: &Path) -> u32 {
        let child = Command::new("/bin/sleep")
            .arg("600")
            .current_dir(cwd)
            .spawn()
            .expect("spawn sleep fixture");
        child.id()
    }

    /// A process that refuses to die politely: the reaper must escalate.
    #[allow(clippy::zombie_processes)]
    fn spawn_sigterm_ignoring(cwd: &Path) -> u32 {
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("trap '' TERM; exec sleep 600")
            .current_dir(cwd)
            .spawn()
            .expect("spawn SIGTERM-ignoring fixture");
        child.id()
    }

    fn wait_until<F: FnMut() -> bool>(mut cond: F, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if cond() {
                return;
            }
            std::thread::sleep(POLL);
        }
        panic!("timed out waiting for {what}");
    }

    /// `/proc/<pid>/cwd`, the way the reaper and the incident both see it.
    fn proc_cwd(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd"))
            .ok()
            .map(|cwd| strip_deleted_suffix(&cwd))
    }

    /// Kill a fixture this test created, by exact pid. Never by name.
    fn kill_exact(pid: u32) {
        signal(pid, libc_sigkill());
        let deadline = Instant::now() + KILL_WAIT;
        while alive(pid) && Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
    }

    /// The incident, end to end: a "fake agent" starts a detached child in the
    /// `-C` workspace and exits, so the child is orphaned (reparented away
    /// from the run) exactly like the 368 `yes` generators on HYDRA. It must
    /// be SELECTED while it is still running (i.e. nothing else would have
    /// killed it) and GONE once the loop's reap scope closes.
    #[test]
    fn fake_agent_detached_child_in_cd_is_reaped_and_would_otherwise_leak() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let mut guard = ws.begin();
        assert!(guard.armed(), "Linux plan should be armed");

        // The fake agent: spawn a detached child in the workspace, record it,
        // then exit — leaving the child orphaned.
        let pidfile = ws.cd.join("agent-child.pid");
        let script = format!("sleep 600 & echo $! > {}; exit 0", pidfile.display());
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .current_dir(&ws.cd)
            .status()
            .expect("run fake agent");
        assert!(status.success(), "fake agent should exit cleanly");

        let raw = std::fs::read_to_string(&pidfile).expect("fake agent wrote its child pid");
        let orphan: u32 = raw.trim().parse().expect("pid file holds a pid");
        wait_until(|| alive(orphan), "the orphaned child to be running");
        let Some(stat) = read_stat(orphan) else {
            kill_exact(orphan);
            panic!("orphan {orphan} disappeared");
        };
        assert_ne!(
            stat.ppid,
            std::process::id(),
            "fixture is meant to be orphaned, not a direct child of the test"
        );

        let plan = guard.plan.clone().expect("armed guard has a plan");
        let selected: Vec<u32> = scan(&plan).iter().map(|c| c.pid).collect();
        assert!(
            selected.contains(&orphan),
            "the leaked child must be selectable while it still runs (before the fix \
             nothing killed it and it survived the loop)"
        );

        guard.finish();

        assert!(
            !alive(orphan),
            "reap must SIGTERM/SIGKILL the orphaned child by exact pid"
        );
        assert!(
            !scan(&plan).iter().any(|c| c.pid == orphan),
            "the reaped pid must leave the candidate set"
        );
    }

    /// A process whose cwd is outside the loop's roots is never ours, however
    /// it got there: another agent's session, an editor, a shell on this host.
    #[test]
    fn process_outside_the_cd_is_never_touched() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let mut guard = ws.begin();
        let outsider = spawn_sleeper(&ws.outside());
        wait_until(|| alive(outsider), "the outsider fixture to be running");

        let plan = guard.plan.clone().expect("armed guard has a plan");
        assert!(
            !scan(&plan).iter().any(|c| c.pid == outsider),
            "a cwd outside the -C dir and the temp root must not be selected"
        );
        guard.finish();
        assert!(alive(outsider), "an outside process must survive the reap");

        kill_exact(outsider);
    }

    /// A process that started before the loop did is not the loop's to kill,
    /// even with its cwd inside the workspace.
    #[test]
    fn process_started_before_the_loop_is_never_touched() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let predates = spawn_sleeper(&ws.cd);
        wait_until(|| alive(predates), "the pre-loop fixture to be running");
        // `starttime` has USER_HZ granularity (10 ms); clear it so the
        // boundary cannot be mistaken for "started after".
        std::thread::sleep(Duration::from_millis(150));

        let mut guard = ws.begin();
        let plan = guard.plan.clone().expect("armed guard has a plan");
        assert!(
            !scan(&plan).iter().any(|c| c.pid == predates),
            "a process that predates the loop must not be selected"
        );
        guard.finish();
        assert!(
            alive(predates),
            "a pre-loop process must survive the reap even with its cwd in -C"
        );

        kill_exact(predates);
    }

    /// The run's OWN scratch directory under the temp root is in scope: an
    /// agent that scatters work into zoder's scratch area leaves processes
    /// there too.
    #[test]
    fn process_in_this_runs_scratch_dir_is_reaped() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let mut guard = ws.begin();
        let scratch = guard
            .plan
            .as_ref()
            .and_then(|p| p.owned_roots.first().cloned())
            .expect("the plan owns a scratch dir");
        let in_scratch = spawn_sleeper(&scratch);
        wait_until(|| alive(in_scratch), "the scratch fixture to be running");

        guard.finish();
        assert!(
            !alive(in_scratch),
            "a scratch-dir process from this run is ours"
        );

        if alive(in_scratch) {
            kill_exact(in_scratch);
        }
    }

    /// The SYSTEM temp root is NOT a reap root. On a shared host every
    /// unrelated process has a cwd somewhere under `/tmp`; selecting on that
    /// would make "someone else's build" a kill candidate.
    #[test]
    fn a_process_in_the_shared_temp_root_is_not_reaped() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let mut guard = ws.begin();
        let bystander = spawn_sleeper(&ws.tmp_root);
        wait_until(|| alive(bystander), "the bystander fixture to be running");

        let plan = guard.plan.clone().expect("armed guard has a plan");
        assert!(
            !scan(&plan).iter().any(|c| c.pid == bystander),
            "a cwd in the shared temp root is not evidence of ownership"
        );
        guard.finish();
        assert!(alive(bystander), "a temp-root bystander must survive");

        kill_exact(bystander);
    }

    /// SIGTERM is a request. A process that traps it must still die.
    #[test]
    fn sigterm_ignoring_process_is_escalated_to_sigkill() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let plan = ReapPlan::new(
            &ws.cd,
            &ws.tmp_root,
            ws.daemon_pid_file(),
            ws.daemon_binary(),
        )
        .expect("plan");
        let stubborn = spawn_sigterm_ignoring(&ws.cd);
        wait_until(
            || read_stat(stubborn).is_some_and(|stat| stat.comm == "sleep"),
            "the SIGTERM-ignoring fixture to exec sleep",
        );
        assert_eq!(
            read_stat(stubborn).map(|stat| stat.comm),
            Some("sleep".to_string()),
            "the fixture must have no child that can outlive a killed shell"
        );

        let outcome = reap(&plan, Duration::from_millis(200));
        let entry = outcome
            .reaped
            .iter()
            .find(|r| r.pid == stubborn)
            .expect("stubborn process is reported as reaped");
        assert_eq!(
            entry.signal,
            ReapSignal::Kill,
            "a process that ignores SIGTERM must be escalated to SIGKILL"
        );
        assert!(
            entry.comm.contains("sh") || entry.comm.contains("sleep"),
            "comm is logged so an operator can identify the victim: {}",
            entry.comm
        );
        assert!(!alive(stubborn), "the stubborn process must be gone");

        if alive(stubborn) {
            kill_exact(stubborn);
        }
    }

    /// `--no-reap` is a real opt-out, not a no-op flag.
    #[test]
    fn no_reap_leaves_the_agent_children_running() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let mut guard = ReapGuard::begin_with(
            &ws.cd,
            &ws.tmp_root,
            false,
            ws.daemon_pid_file(),
            ws.daemon_binary(),
        );
        assert!(!guard.armed(), "--no-reap must not arm a plan");
        let leaked = spawn_sleeper(&ws.cd);
        wait_until(|| alive(leaked), "the fixture to be running");

        guard.finish();
        assert!(alive(leaked), "--no-reap must leave the child alone");

        kill_exact(leaked);
    }

    /// Every exit path funnels through `Drop`, including an early `?` or a
    /// panic unwinding out of the loop body.
    #[test]
    fn dropping_the_guard_reaps_without_an_explicit_finish() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let leaked = {
            let _guard = ws.begin();
            let leaked = spawn_sleeper(&ws.cd);
            wait_until(|| alive(leaked), "the fixture to be running");
            // `_guard` leaves scope here with no `finish()` call.
            leaked
        };
        assert!(
            !alive(leaked),
            "Drop must reap so a panic/early-return path cannot leak"
        );
    }

    /// A reaper that can select itself is a foot-gun; `/proc/self/cwd` is the
    /// workspace. zoder and every ancestor must be excluded outright.
    #[test]
    fn scan_never_selects_zoder_itself_or_its_ancestors() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let guard = ws.begin();
        let plan = guard.plan.clone().expect("armed guard has a plan");
        let selected: Vec<u32> = scan(&plan).iter().map(|c| c.pid).collect();
        let protected = protected_pids();

        assert!(
            !selected.contains(&std::process::id()),
            "zoder itself must never be a reap candidate"
        );
        assert!(
            !selected.contains(&1),
            "init must never be a reap candidate"
        );
        for pid in &protected {
            assert!(
                !selected.contains(pid),
                "ancestor {pid} must never be a reap candidate"
            );
        }
    }

    /// `comm` may contain spaces AND parentheses, so the closing paren of
    /// field 2 cannot be found with `rfind(')')`: a process named `foo)bar`
    /// would end the parse early and every later field would be garbage —
    /// including `starttime`, which is what decides what gets reaped.
    ///
    /// `od d) dxx (yy` is deliberately not a fixture reused anywhere else in
    /// the crate: an expected value that appears once makes a failing assert
    /// unambiguous.
    #[test]
    fn stat_parser_handles_a_comm_with_spaces_and_parens() {
        // Fields 3..22 of a real line: state ppid pgrp session tty_nr tpgid
        // flags minflt cminflt majflt cmajflt utime stime cutime cstime
        // priority nice num_threads itrealvalue starttime.
        let raw = "42 (od d) dxx (yy) S 7 7 7 0 -1 4194304 100 0 0 0 1 2 3 4 20 0 1 0 424242 1 2 3";
        let stat = parse_stat(raw).expect("parse");
        let expected = "od d) dxx (yy";
        assert_eq!(stat.comm, expected, "comm keeps its embedded parens");
        assert_eq!(stat.state, "S", "state is field 3");
        assert_eq!(stat.ppid, 7, "ppid is field 4");
        assert_eq!(stat.start_ticks, 424242, "starttime is field 22");
        // A plain comm must still parse, and a comm holding a paren must not
        // truncate the line.
        let plain = parse_stat("77 (sleep) S 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20")
            .expect("parse plain");
        assert_eq!(plain.comm, "sleep");
        assert_eq!(plain.start_ticks, 20, "starttime still lines up");
    }

    /// A pid is not an identity: the kernel recycles them. Selection records
    /// `starttime`, and every signal re-checks it, so a pid that changed hands
    /// between the scan and the signal is SKIPPED, never signalled.
    #[test]
    fn a_recycled_pid_is_skipped_instead_of_signalled() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let bystander = spawn_sleeper(&ws.cd);
        wait_until(|| alive(bystander), "the fixture to be running");

        // A candidate that claims this pid but a DIFFERENT start time is what
        // a recycled pid looks like from the reaper's side.
        let real = read_stat(bystander).expect("stat");
        let impostor = Candidate {
            pid: bystander,
            comm: real.comm.clone(),
            cwd: ws.cd.clone(),
            start_ticks: real.start_ticks.wrapping_add(1),
        };
        assert_eq!(
            confirm_identity(&impostor, current_uid()),
            Identity::Recycled,
            "a pid whose start time changed is NOT the process we selected"
        );
        let genuine = Candidate {
            start_ticks: real.start_ticks,
            ..impostor.clone()
        };
        assert_eq!(
            confirm_identity(&genuine, current_uid()),
            Identity::Same,
            "the same start time is the same process"
        );
        // A wrong uid must not pass either: uid is re-read, not remembered.
        assert_eq!(
            confirm_identity(&genuine, current_uid().wrapping_add(1)),
            Identity::Recycled,
            "the uid is re-validated from /proc, not trusted from the scan"
        );

        assert!(
            alive(bystander),
            "no signal may be sent to a mismatched pid"
        );
        kill_exact(bystander);
    }

    /// The per-run scratch dir zoder created must not be left behind in the
    /// system temp root.
    #[test]
    fn the_scratch_dir_is_removed_when_the_scope_closes() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let scratch = {
            let mut guard = ws.begin();
            let scratch = guard
                .plan
                .as_ref()
                .and_then(|p| p.owned_roots.first().cloned())
                .expect("a scratch dir");
            assert!(
                scratch.is_dir(),
                "the scratch dir exists while the run does"
            );
            guard.finish();
            scratch
        };
        assert!(
            !scratch.exists(),
            "zoder's own scratch dir must not outlive the run"
        );
    }

    /// `zoder loop -C /tmp` must not turn every other process on the host with
    /// a cwd in /tmp into a kill candidate: the shared temp root is refused as
    /// a workspace root, and only this run's scratch dir remains in scope.
    #[test]
    fn a_workspace_that_is_the_shared_temp_root_is_refused() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let bystander = spawn_sleeper(&ws.tmp_root);
        wait_until(|| alive(bystander), "the bystander fixture to be running");

        // Same directory passed as both the workspace and the temp base.
        let plan = ReapPlan::new(
            &ws.tmp_root,
            &ws.tmp_root,
            ws.daemon_pid_file(),
            ws.daemon_binary(),
        )
        .expect("plan");
        assert!(
            !plan.owns_cwd(&ws.tmp_root),
            "-C <temp root> must not make the shared temp root a reap root"
        );
        assert!(
            plan.roots.len() == 1 && plan.owned_roots.len() == 1,
            "only the run's own scratch dir may remain in scope"
        );
        let outcome = reap(&plan, Duration::from_millis(50));
        assert!(
            !outcome.reaped.iter().any(|r| r.pid == bystander),
            "an unrelated process in /tmp must not be reaped"
        );
        assert!(alive(bystander), "the bystander survives");

        kill_exact(bystander);
    }

    /// The cancel budget is for the PHASE, not per session. A loop that drove
    /// nine sessions must not spend nine budgets before the first kill; with
    /// nothing left in the budget it stops and lets the reap run.
    #[tokio::test]
    async fn the_cancel_budget_covers_every_session_not_each_one() {
        let socket = PathBuf::from("/nonexistent/zoder-cancel-budget-probe.sock");
        {
            let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            note_session("budget-probe-a");
            note_session("budget-probe-b");
            note_session("budget-probe-c");
        }
        let started = Instant::now();
        // A zero budget must send nothing at all: not even a connect attempt
        // to a daemon that happens to be running on this host.
        cancel_recorded_sessions(&socket, Duration::ZERO).await;
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "an exhausted budget must stop immediately, not cancel session by session"
        );
        for id in ["budget-probe-a", "budget-probe-b", "budget-probe-c"] {
            let mut seen = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
            seen.retain(|s| s != id);
        }
    }

    /// The panic-unwind and signal teardown use this list to send blocking
    /// cancels. A pass limit must not silently become a session limit.
    #[test]
    fn pending_sessions_includes_every_uncancelled_session() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ids: Vec<String> = (0..65).map(|i| format!("pending-cap-probe-{i}")).collect();
        for id in &ids {
            note_session(id);
        }
        let pending = pending_sessions();
        for id in &ids {
            assert!(pending.contains(id), "teardown omitted session {id}");
        }
        SESSIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|id| !ids.contains(id));
    }

    /// A process from a tick BEFORE the run began is never ours, even with its
    /// cwd inside the workspace. This is the boundary the pid snapshot draws,
    /// and it is the half of the granularity problem that can be decided.
    #[test]
    fn a_process_from_an_earlier_tick_is_never_a_candidate() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let older = spawn_sleeper(&ws.cd);
        wait_until(|| alive(older), "the fixture to be running");
        // Clear the tick granularity so the fixture is unambiguously older
        // than the boundary the guard is about to take.
        std::thread::sleep(Duration::from_millis(150));
        let mut guard = ws.begin();
        let plan = guard.plan.clone().expect("armed guard has a plan");
        let start = read_stat(older).map(|s| s.start_ticks).unwrap_or(0);
        assert!(
            start < plan.started_ticks,
            "the fixture must be strictly older than the boundary for this test to mean anything"
        );
        assert!(
            plan.preexisting.contains_key(&older),
            "the snapshot records pid -> start time, not just the pid"
        );
        assert!(
            !scan(&plan).iter().any(|c| c.pid == older),
            "a process from an earlier tick is never selected"
        );
        guard.finish();
        assert!(alive(older), "and it must survive the reap");

        kill_exact(older);
    }

    /// When the kernel supports pidfd, a selected process is pinned before it
    /// is signalled: the fd survives the process's death, refuses signals once
    /// it is gone, and reports liveness without consulting /proc. That is what
    /// makes "check identity then signal" atomic instead of merely adjacent.
    #[test]
    fn a_pinned_target_reports_its_own_lifecycle() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        // Spawned by a shell that exits, so the fixture is reparented to init:
        // when it dies it disappears instead of waiting as a zombie for this
        // test to reap it.
        let pidfile = ws.cd.join("pin-target.pid");
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "sleep 600 & echo $! > {}; exit 0",
                pidfile.display()
            ))
            .current_dir(&ws.cd)
            .status()
            .expect("run the fixture shell");
        assert!(status.success(), "the fixture shell exits cleanly");
        let victim: u32 = std::fs::read_to_string(&pidfile)
            .expect("fixture pid file")
            .trim()
            .parse()
            .expect("fixture pid is a number");
        wait_until(|| alive(victim), "the fixture to be running");
        let Some(pin) = PidFd::open(victim) else {
            // Kernel without pidfd (pre-5.3): the fallback path is pid
            // signalling, which the other tests already cover.
            eprintln!("[test] pidfd unsupported on this kernel; skipping");
            kill_exact(victim);
            return;
        };
        assert!(pin.alive(), "a pinned live process reports alive");
        assert!(pin.signal(0), "signal 0 is the liveness probe");
        assert!(
            pin.signal(libc::SIGTERM),
            "the pinned process is signallable"
        );
        wait_until(|| !pin.alive(), "the pinned process to die from SIGTERM");
        assert!(
            !pin.alive(),
            "the pin keeps reporting the truth after the process is gone"
        );
        assert!(
            !pin.signal(libc::SIGTERM),
            "a pin for a dead process refuses further signals"
        );

        if alive(victim) {
            kill_exact(victim);
        }
    }

    /// A command name may contain a paren followed by a state-looking token
    /// (`worker) R`). Splitting on the FIRST such paren shifts every later
    /// field by one and reads `starttime` from the wrong column — which
    /// silently changes WHICH processes the reaper is allowed to kill.
    #[test]
    fn stat_parser_survives_a_comm_that_ends_with_a_state_like_token() {
        // comm is `worker) R`: the line is `42 (worker) R) S 7 7 ...`.
        let raw = "42 (worker) R) S 7 7 7 0 -1 4194304 100 0 0 0 1 2 3 4 20 0 1 0 424242 1 2 3";
        let stat = parse_stat(raw).expect("parse");
        assert_eq!(stat.comm, "worker) R", "comm keeps the embedded paren");
        assert_eq!(
            stat.state, "S",
            "state is the REAL state field, not the comm's"
        );
        assert_eq!(stat.ppid, 7, "ppid is field 4");
        assert_eq!(stat.start_ticks, 424242, "starttime is field 22");
    }

    /// A parent can fork again while the reap is still signalling the previous
    /// generation. The reap keeps scanning until a pass finds nothing new, so
    /// the late descendant is not left behind.
    // The fixture is deliberately not waited on: the point is to reap it
    // mid-flight, and waiting would race the reap for its own subject.
    #[allow(clippy::zombie_processes)]
    #[test]
    fn a_descendant_forked_during_the_reap_is_still_reaped() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let plan = ReapPlan::new(
            &ws.cd,
            &ws.tmp_root,
            ws.daemon_pid_file(),
            ws.daemon_binary(),
        )
        .expect("plan");
        // A parent that forks one child, then forks another ~200ms later —
        // after the first child has been signalled, i.e. mid-pass.
        let pidfile = ws.cd.join("forker-pids");
        // The parent runs OUTSIDE the reap roots so the reap cannot kill it
        // before it forks; its children run inside -C, which is what makes
        // them candidates. Child two appears 150 ms in, i.e. while the reap's
        // grace window is open.
        // The second child is forked only AFTER the first one is gone, i.e.
        // after the reap has killed it: "forked during the reap" becomes a
        // fact rather than a hope about poll timing.
        let script = format!(
            "(cd {ws}; exec sleep 600) & echo $! >> {f}; first=$!; \
             while kill -0 $first 2>/dev/null; do sleep 0.02; done; \
             (cd {ws}; exec sleep 600) & echo $! >> {f}; wait",
            ws = ws.cd.display(),
            f = pidfile.display()
        );
        let forker = Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .current_dir(ws.outside())
            .spawn()
            .expect("run the forking fixture");
        let forker_pid = forker.id();
        // Wait for the FIRST child only: the fixture forks its second child
        // 150 ms later, which lands inside the reap's grace window below. A
        // test that waited for both children up front would pass even if the
        // reaper only ever scanned once.
        let mut first_pids: Vec<u32> = Vec::new();
        wait_until(
            || {
                first_pids = std::fs::read_to_string(&pidfile)
                    .map(|text| {
                        text.lines()
                            .filter_map(|line| line.trim().parse().ok())
                            .collect::<Vec<u32>>()
                    })
                    .unwrap_or_default();
                !first_pids.is_empty()
            },
            "the fixture to fork its first child",
        );
        let outcome = reap(&plan, Duration::from_millis(400));
        // Both generations, including the one forked mid-reap.
        let mut pids = first_pids;
        wait_until(
            || {
                pids = std::fs::read_to_string(&pidfile)
                    .map(|text| {
                        text.lines()
                            .filter_map(|line| line.trim().parse().ok())
                            .collect::<Vec<u32>>()
                    })
                    .unwrap_or_default();
                pids.len() >= 2
            },
            "the fixture to fork its second child",
        );
        assert_eq!(pids.len(), 2, "the fixture forked two children");
        let reaped: Vec<u32> = outcome.reaped.iter().map(|r| r.pid).collect();
        for pid in &pids {
            assert!(
                reaped.contains(pid),
                "child {pid} must be reaped (reaped: {reaped:?})"
            );
            assert!(!alive(*pid), "child {pid} must be gone");
        }
        for pid in pids.iter().copied().chain([forker_pid]) {
            if alive(pid) {
                kill_exact(pid);
            }
        }
    }

    /// The `Drop` path cannot await, so it sends the cancel over a blocking
    /// socket. Prove the frame that lands on the wire is a real
    /// `session/cancel` for the session we drove — a broken hand-rolled
    /// handshake would silently cancel nothing.
    #[test]
    fn the_blocking_cancel_sends_a_real_cancel_frame() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("engine.sock");
        let listener = UnixListener::bind(&socket).expect("bind fake daemon");
        let seen: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let server = std::thread::spawn(move || {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let mut writer = stream.try_clone().expect("clone stream");
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            // initialize -> reply; session/cancel -> record.
            for _ in 0..2 {
                line.clear();
                if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                    return;
                }
                let frame: serde_json::Value = match serde_json::from_str(line.trim()) {
                    Ok(f) => f,
                    Err(_) => return,
                };
                let method = frame.get("method").and_then(|m| m.as_str()).unwrap_or("");
                if method == "initialize" {
                    let _ =
                        writer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"init\",\"result\":{}}\n");
                } else {
                    sink.lock().unwrap_or_else(|e| e.into_inner()).push(frame);
                    let _ = writer.write_all(
                        b"{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"type\":\"turn_complete\",\"session_id\":\"blocking-probe\"}}\n",
                    );
                }
            }
        });

        note_session("blocking-probe");
        // The socket is passed in rather than reached through the environment:
        // a test that mutates ZEROCLAW_SOCKET races every other test in the
        // binary that reads it.
        cancel_sessions_blocking(&socket, &pending_sessions());
        server.join().expect("fake daemon thread");

        let seen = seen.lock().unwrap_or_else(|e| e.into_inner());
        let cancel = seen
            .iter()
            .find(|f| f.get("method").and_then(|m| m.as_str()) == Some("session/cancel"))
            .expect("a session/cancel frame must reach the daemon");
        assert_eq!(
            cancel
                .pointer("/params/session_id")
                .and_then(|v| v.as_str()),
            Some("blocking-probe"),
            "the cancel must name the session this run drove"
        );
        // Leave the shared session list clean for the other tests.
        SESSIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|id| id != "blocking-probe");
        CANCELLED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove("blocking-probe");
    }

    /// A socket path under a tempdir that was never created. The 01a163c9
    /// failure had no listener AND no entry, so both "no engine" branches of
    /// the classifier have to be exercised; this is the ENOENT one.
    fn absent_socket(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    /// Forget everything a test told the global registries, so one test's
    /// unreachable session cannot leak into the next one's teardown.
    fn forget_session(id: &str) {
        SESSIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|s| s != id);
        CANCELLED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        UNREACHABLE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    /// Only a "no listener exists" errno proves the engine is gone. Every
    /// other failure leaves the engine unproven and the session live.
    #[test]
    fn only_no_listener_errnos_count_as_engine_unreachable() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        for (error, expected) in [
            (missing, CancelFailure::EngineUnreachable),
            (refused, CancelFailure::EngineUnreachable),
            (
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
                CancelFailure::NotConfirmed,
            ),
            (
                std::io::Error::from(std::io::ErrorKind::TimedOut),
                CancelFailure::NotConfirmed,
            ),
        ] {
            assert_eq!(classify_cancel_error(&anyhow::Error::new(error)), expected);
        }
        // The `io::Error` is wrapped by transport context in production, so
        // the classifier must read the CHAIN, not the top-level message.
        let wrapped = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::NotFound))
            .context("connecting to engine at /nowhere/daemon.sock");
        assert_eq!(
            classify_cancel_error(&wrapped),
            CancelFailure::EngineUnreachable,
            "a transport-wrapped ENOENT must still read as an unreachable engine"
        );
        assert_eq!(
            classify_cancel_error(&anyhow::anyhow!("daemon did not acknowledge")),
            CancelFailure::NotConfirmed,
            "a handshake failure is the ordinary unconfirmed cancel"
        );
    }

    /// 2026-10-22 (MINOS, hive job `01a163c9`): the engine socket path no
    /// longer existed, yet the teardown ran all four rounds, repeated the same
    /// "cancel not confirmed" line for one session, and then reported the agent
    /// as still producing work. Three separate failures, three assertions:
    /// one round, one unreachable line per session, no producing-work warning.
    #[test]
    fn a_missing_engine_socket_ends_the_teardown_in_one_round_without_a_work_warning() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = absent_socket(&dir, "engine.sock");
        assert!(
            !socket.exists(),
            "the fixture must be a path that does not exist"
        );
        let ids = ["unreachable-probe-a", "unreachable-probe-b"];
        for id in ids {
            note_session(id);
        }

        let plan = ReapPlan::new(
            &ws.cd,
            &ws.tmp_root,
            ws.daemon_pid_file(),
            ws.daemon_binary(),
        )
        .expect("a reap plan for a real workspace");
        let report = teardown_rounds(&plan, Duration::from_millis(50), &socket);

        assert_eq!(
            report.rounds, 1,
            "an engine that is not there must not cost four teardown rounds"
        );
        for id in ids {
            assert!(
                report.unreachable_sessions.contains(&id.to_string()),
                "session {id} must be reported as engine-unreachable, not as a live turn"
            );
            assert!(
                !pending_sessions().contains(&id.to_string()),
                "session {id} must not be offered to a later round"
            );
        }
        assert!(
            !report.producing_work,
            "an absent engine socket is not the agent still producing work"
        );
        for id in ids {
            forget_session(id);
        }
    }

    /// The unreachable verdict is a fact about the run, not about the attempt:
    /// one line per session however many rounds, passes or paths ask. (The
    /// 01a163c9 log carried the same session line once per round.)
    #[test]
    fn an_unreachable_session_is_said_once_no_matter_how_often_it_is_asked() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = absent_socket(&dir, "engine.sock");
        note_session("unreachable-once-probe");

        assert!(
            mark_unreachable("unreachable-once-probe"),
            "the first unreachable failure is the one worth reporting"
        );
        for _ in 0..5 {
            assert!(
                !mark_unreachable("unreachable-once-probe"),
                "a second report of the same absent socket is noise, not news"
            );
        }
        // Handed the id DIRECTLY, not through `pending_sessions()`: the point
        // is that even a caller which still holds a stale id gets no second
        // report out of it.
        let reported = cancel_sessions_blocking(&socket, &["unreachable-once-probe".to_string()]);
        assert!(
            !reported.contains(&"unreachable-once-probe".to_string()),
            "a session already reported must not be reported again"
        );
        forget_session("unreachable-once-probe");
    }

    /// The async cancel phase retires an unreachable session too, or the signal
    /// path and the panic path would still ask it once each.
    ///
    /// The lock is deliberately held across the await, against the usual lint:
    /// `cancel_recorded_sessions` reads the PROCESS-WIDE session list, so
    /// without the lock this test could pick up a concurrent reaper test's
    /// session and retire it, failing that test at random. `#[tokio::test]` is
    /// a current-thread runtime and this guard never moves between threads, so
    /// holding a `std::sync::Mutex` across an await is sound here; the lint
    /// exists for `Send` futures on multi-threaded runtimes, which this is not.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn the_async_cancel_phase_retires_an_unreachable_session() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = absent_socket(&dir, "engine.sock");
        note_session("async-unreachable-probe");

        cancel_recorded_sessions(&socket, REAP_CANCEL_BUDGET).await;

        assert!(
            engine_unreachable("async-unreachable-probe"),
            "an ENOENT cancel must retire the session"
        );
        assert!(
            !pending_sessions().contains(&"async-unreachable-probe".to_string()),
            "a retired session must not be offered to the blocking path or a later round"
        );
        forget_session("async-unreachable-probe");
    }

    /// The warning still fires when it is true, and only for what a signal
    /// proves. A pass that killed something, or signalled something that
    /// outlived the grace, is work the agent was still producing; a pass that
    /// selected nothing found nothing, and a pass whose targets were already
    /// gone signalled nothing. The fix removes a false positive; it does not
    /// disarm the alarm.
    #[test]
    fn only_a_pass_that_signalled_new_processes_counts_as_producing_work() {
        let entry = Reaped {
            pid: 4242,
            comm: "sleep".to_string(),
            cwd: PathBuf::from("/tmp/nowhere-in-particular"),
            signal: ReapSignal::Term,
        };
        let killed = ReapOutcome {
            reaped: vec![entry.clone()],
            ..Default::default()
        };
        assert!(
            killed.signalled_new_work(),
            "a pass that killed the agent's leftover IS the agent producing work"
        );
        let survived = ReapOutcome {
            survivors: vec![entry.clone()],
            ..Default::default()
        };
        assert!(
            survived.signalled_new_work(),
            "a process we signalled and could not kill is still producing work"
        );
        assert!(
            !ReapOutcome::default().signalled_new_work(),
            "an empty pass found nothing"
        );
        let vanished = ReapOutcome {
            skipped: vec![entry],
            ..Default::default()
        };
        assert!(
            !vanished.signalled_new_work(),
            "a target that was gone before any signal is not work being produced"
        );
    }

    /// A converged teardown that really did kill something must still reap it,
    /// and must not dress the kill up as an unfinished teardown. This is the
    /// positive control for the missing-socket test above: the reap half of the
    /// teardown is untouched by the change.
    #[test]
    fn a_teardown_with_a_leftover_still_reaps_it_and_converges_in_one_round() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = absent_socket(&dir, "engine.sock");
        // The plan FIRST: a fixture that existed when the scope opened is not
        // ours, which is the whole ownership test.
        let plan = ReapPlan::new(
            &ws.cd,
            &ws.tmp_root,
            ws.daemon_pid_file(),
            ws.daemon_binary(),
        )
        .expect("a reap plan for a real workspace");
        let leftover = spawn_sleeper(&ws.cd);
        wait_until(|| alive(leftover), "the fixture to be running");

        let report = teardown_rounds(&plan, Duration::from_millis(50), &socket);

        assert_eq!(
            report.rounds, 2,
            "kill, then a confirming round that finds nothing: the normal two rounds"
        );
        assert!(
            !report.producing_work,
            "a teardown that converged did not run out of rounds while working"
        );
        assert!(
            !alive(leftover),
            "the leftover must actually be reaped on the way past"
        );
    }

    /// `--no-reap` really means off, including on the `Drop` path: no cancel,
    /// no reap, no signal handlers. A user who asked for the scope to be off
    /// must not get a surprise `session/cancel` on the way out.
    #[test]
    fn no_reap_sends_no_cancel_on_drop() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("engine.sock");
        // Anything that opens this socket proves a cancel went out.
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        note_session("no-reap-probe");
        {
            let guard = ReapGuard::begin(&ws.cd, &ws.tmp_root, false);
            assert!(!guard.armed(), "--no-reap leaves the guard disarmed");
            // Dropping here must not touch the engine.
        }
        listener.set_nonblocking(true).expect("nonblocking");
        assert!(
            listener.accept().is_err(),
            "--no-reap must not open a connection to the engine on Drop"
        );
        SESSIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|id| id != "no-reap-probe");
    }

    /// A root of `/` would select every process on the host. Refuse it.
    #[test]
    fn a_root_of_slash_arms_nothing() {
        assert!(
            ReapPlan::new(
                Path::new("/"),
                Path::new("/"),
                PathBuf::from("/nonexistent/daemon.pid"),
                PathBuf::from("/nonexistent/zeroclaw"),
            )
            .is_none(),
            "a / root must not produce a reap plan"
        );
    }

    /// The workspace and this run's scratch dir are in scope; the shared temp
    /// root, anything else, and `/` are not.
    #[test]
    fn plan_covers_the_cd_directory_and_this_runs_scratch_dir_only() {
        let ws = Workspace::new();
        let plan = ReapPlan::new(
            &ws.cd,
            &ws.tmp_root,
            ws.daemon_pid_file(),
            ws.daemon_binary(),
        )
        .expect("plan");
        assert!(plan.owns_cwd(&ws.cd.join("src/lib.rs")));
        let scratch = plan
            .owned_roots
            .first()
            .expect("a dedicated scratch dir under the temp root");
        assert!(
            scratch.starts_with(&ws.tmp_root),
            "the scratch dir belongs inside the temp base"
        );
        assert!(
            plan.owns_cwd(&scratch.join("scratch.index")),
            "the run's own scratch dir is in scope"
        );
        assert!(
            !plan.owns_cwd(&ws.tmp_root),
            "the shared temp root itself must NOT be a reap root"
        );
        assert!(!plan.owns_cwd(&ws.outside()));
        assert!(!plan.owns_cwd(Path::new("/")));
    }

    /// The CLI flag has to exist on the subcommand, not just on the root.
    #[test]
    fn no_reap_is_a_real_cli_flag() {
        use clap::Parser;
        let on_loop = crate::Cli::try_parse_from(["zoder", "loop", "--no-reap"])
            .expect("loop accepts --no-reap");
        assert!(on_loop.no_reap, "--no-reap must set the flag");
        let on_rescue = crate::Cli::try_parse_from(["zoder", "rescue", "fix", "--no-reap"])
            .expect("rescue accepts --no-reap");
        assert!(on_rescue.no_reap, "global flag reaches rescue too");
        let default =
            crate::Cli::try_parse_from(["zoder", "loop", "task"]).expect("loop without the flag");
        assert!(!default.no_reap, "reaping is ON by default");
        assert!(enabled_for_cli(&default), "loop must reap by default");
        assert!(
            !enabled_for_cli(&on_loop),
            "--no-reap must disable loop reaping"
        );
        assert!(
            !enabled_for_cli(&on_rescue),
            "--no-reap must disable rescue reaping"
        );
    }

    /// The reap is silent when there is nothing to reap: no noise on a normal
    /// run that cleaned up after itself.
    #[test]
    fn a_clean_run_reaps_nothing() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let plan = ReapPlan::new(
            &ws.cd,
            &ws.tmp_root,
            ws.daemon_pid_file(),
            ws.daemon_binary(),
        )
        .expect("plan");
        let outcome = reap(&plan, Duration::from_millis(50));
        assert!(
            outcome.reaped.is_empty() && outcome.skipped.is_empty() && outcome.survivors.is_empty(),
            "an empty workspace must produce no reaped, skipped or surviving processes"
        );
    }

    // -----------------------------------------------------------------------
    // The shared zeroclaw daemon (2026-10-21, ACHILLES hive job 01a16164).
    // -----------------------------------------------------------------------

    /// The incident, at the reap scope. A `zeroclaw` daemon sits inside `-C`,
    /// same uid, started after the loop began — so every ownership rule in
    /// this file selects it — and an ordinary leftover sits right beside it.
    /// The daemon must survive; the leftover must not.
    ///
    /// Both fixtures are spawned AFTER the scope opens on purpose: a daemon
    /// that predated the loop would be spared by the start-time rule alone,
    /// which is exactly the loophole that let this through in the first
    /// place.
    #[test]
    fn a_shared_daemon_in_the_workspace_is_spared_and_an_ordinary_leftover_is_still_reaped() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let mut guard = ws.begin();
        let plan = guard.plan.clone().expect("armed guard has a plan");

        let daemon_exe = install_fake_zeroclaw(&ws.root(), "/bin/sh");
        let (mut daemon, helper) = spawn_fake_daemon(
            &daemon_exe,
            &ws.cd,
            &ws.outside(),
            &ws.cd.join("daemon-helper.pid"),
        );
        let daemon_pid = daemon.id();
        let leftover = spawn_sleeper(&ws.cd);
        wait_until(|| alive(leftover), "the ordinary fixture to be running");
        assert!(
            process_executable(daemon_pid)
                .is_some_and(|exe| executable_is_zeroclaw(&exe, &ws.daemon_binary())),
            "precondition: /proc/<pid>/exe must name the daemon, or this test proves nothing"
        );
        assert_eq!(
            proc_cwd(daemon_pid).as_deref(),
            Some(ws.cd.as_path()),
            "precondition: the daemon sits INSIDE -C, which is the whole incident"
        );

        let selected: Vec<u32> = scan(&plan).iter().map(|c| c.pid).collect();
        assert!(
            selected.contains(&leftover),
            "an ordinary leftover in -C started during the loop is ours to reap"
        );
        assert!(
            !selected.contains(&daemon_pid),
            "the shared daemon matches every ownership rule and must still be spared"
        );

        guard.finish();
        assert!(
            alive(daemon_pid),
            "SIGTERM'ing the shared daemon kills the agent of every other zoder on this host"
        );
        assert!(
            !alive(leftover),
            "the exemption is the daemon itself, not everything in the workspace"
        );

        kill_exact(daemon_pid);
        kill_exact(helper);
        let _ = daemon.wait();
        kill_exact(leftover);
    }

    /// Identity beats evidence. A daemon this process spawned is spared even
    /// when nothing about it looks like zeroclaw — which is the window
    /// between `spawn` returning and the executable being inspectable, and the
    /// window that matters, because that is when the reaper's first scan runs
    /// in a signal teardown.
    #[test]
    fn the_pid_this_zoder_spawned_is_spared_whatever_its_executable_is() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let mut guard = ws.begin();
        let plan = guard.plan.clone().expect("armed guard has a plan");
        let daemon = spawn_sleeper(&ws.cd);
        wait_until(|| alive(daemon), "the spawned fixture to be running");
        assert!(
            !process_executable(daemon)
                .is_some_and(|exe| executable_is_zeroclaw(&exe, &ws.daemon_binary())),
            "precondition: this fixture is /bin/sleep, not the zeroclaw binary"
        );

        let identity = (
            daemon,
            live_start_ticks(daemon).expect("a live fixture has a start time"),
        );
        note_daemon_identity(identity);
        assert!(daemon_spared_by_this_process(daemon));
        assert!(
            !scan(&plan).iter().any(|c| c.pid == daemon),
            "the pid this process handed out from spawn() is exempt by identity"
        );

        guard.finish();
        assert!(
            alive(daemon),
            "our own daemon child must survive our own reap"
        );

        // …and the exemption dies with it: a record for a pid whose process is
        // gone must not spare whoever the kernel hands that number to next.
        kill_exact(daemon);
        assert!(
            !daemon_spared_by_this_process(daemon),
            "a dead daemon must not leave a standing exemption behind"
        );
    }

    /// The cross-process arm: zoder records the daemon's identity next to the
    /// socket, so a reap scope in a DIFFERENT zoder process (job 01a1619b's
    /// loop, while 01a16164's daemon is serving it) can spare it too. Only
    /// the record is involved here — the fixture's executable is `sleep`.
    #[test]
    fn the_daemon_recorded_for_the_socket_is_spared_in_another_zoder_process() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let pidfile = ws.root().join("daemon.pid");
        // Scope FIRST, daemon second — twice over: `plan` reads the record
        // that will name the daemon, `without_record` reads a pid file that
        // does not exist. A fixture that predates a scope is excluded by the
        // start-time rule whatever the record says, so both scopes have to be
        // open before the fixture is spawned or the assertions prove nothing.
        let plan =
            ReapPlan::new(&ws.cd, &ws.tmp_root, pidfile.clone(), ws.daemon_binary()).expect("plan");
        let without_record = ReapPlan::new(
            &ws.cd,
            &ws.tmp_root,
            ws.daemon_pid_file(),
            ws.daemon_binary(),
        )
        .expect("plan");
        let daemon = spawn_sleeper(&ws.cd);
        wait_until(|| alive(daemon), "the recorded fixture to be running");
        let identity = (
            daemon,
            live_start_ticks(daemon).expect("a live fixture has a start time"),
        );
        std::fs::write(&pidfile, format!("{} {}\n", identity.0, identity.1))
            .expect("record the daemon identity");
        assert_eq!(crate::read_daemon_record(&pidfile), Some(identity));

        assert!(
            !scan(&plan).iter().any(|c| c.pid == daemon),
            "the daemon recorded for the socket must be spared by another process's reap"
        );
        // …and the fixture is genuinely eligible, so the assertion above is
        // about the record and not about a process nobody would have reaped:
        // a scope that never saw a record treats it as an ordinary candidate.
        assert!(
            scan(&without_record).iter().any(|c| c.pid == daemon),
            "precondition: with no record at all this fixture is an ordinary reap candidate"
        );

        kill_exact(daemon);
    }

    /// A record the kernel has outgrown must spare nobody. The daemon died,
    /// nobody cleared `<data dir>/daemon.pid`, and the pid has since been
    /// handed to an ordinary agent leftover in this workspace: honouring the
    /// stale record would spare a stranger, which is the one thing an
    /// identity-based exemption must never do.
    #[test]
    fn a_stale_daemon_record_spares_nobody_after_the_pid_is_reused() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let pidfile = ws.root().join("daemon.pid");
        // The scope opens FIRST: a process that already existed when it did
        // would be spared by the start-time rule, and this test would prove
        // nothing about the stale record.
        let plan =
            ReapPlan::new(&ws.cd, &ws.tmp_root, pidfile.clone(), ws.daemon_binary()).expect("plan");
        let stranger = spawn_sleeper(&ws.cd);
        wait_until(|| alive(stranger), "the fixture to be running");
        let live = live_start_ticks(stranger).expect("a live fixture has a start time");
        // Same pid, a start time that is demonstrably not this process's.
        std::fs::write(&pidfile, format!("{} {}\n", stranger, live + 1))
            .expect("record a stale identity");

        assert!(
            scan(&plan).iter().any(|c| c.pid == stranger),
            "a record whose start time does not match the live process is a recycled pid, \
             not the daemon: the process it now names must stay reapable"
        );
        reap(&plan, Duration::from_millis(50));
        assert!(!alive(stranger), "the recycled-pid stranger is reaped");

        if alive(stranger) {
            kill_exact(stranger);
        }
    }

    /// A pid file that is empty, non-numeric, names init/pid 0, or carries a
    /// pid with no start time spares nobody: it must not be able to silence
    /// the reap wholesale, and a pid-only record is refused for the same
    /// reason a stale one is — it cannot be checked against anything.
    #[test]
    fn a_garbage_daemon_record_spares_nobody() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let pidfile = ws.root().join("daemon.pid");
        // Scope first, fixture second, so the fixture is reapable for the
        // reason under test and not because it predates the run.
        let plan =
            ReapPlan::new(&ws.cd, &ws.tmp_root, pidfile.clone(), ws.daemon_binary()).expect("plan");
        let leftover = spawn_sleeper(&ws.cd);
        wait_until(|| alive(leftover), "the fixture to be running");
        let live = live_start_ticks(leftover).expect("a live fixture has a start time");
        for junk in [
            "",
            "   ",
            "not-a-pid",
            "0",
            "1",
            "-5",
            "4294967296",
            &format!("{leftover}"),
            &format!("{leftover} not-a-tick"),
            &format!("{leftover} 0 1 2 3"),
        ] {
            std::fs::write(&pidfile, format!("{junk}\n")).expect("write junk");
            assert_eq!(
                crate::read_daemon_record(&pidfile),
                None,
                "{junk:?} must not parse as a daemon identity"
            );
        }
        // A record naming a live pid with its REAL start time does parse —
        // and is honoured, which is what makes the refusals above meaningful.
        std::fs::write(&pidfile, format!("{leftover} {live}\n")).expect("write junk");
        assert_eq!(crate::read_daemon_record(&pidfile), Some((leftover, live)));

        std::fs::write(&pidfile, "0 12345\n").expect("write junk");
        assert!(
            scan(&plan).iter().any(|c| c.pid == leftover),
            "an unusable daemon record must not turn the reap off"
        );

        reap(&plan, Duration::from_millis(50));
        assert!(!alive(leftover), "an ordinary leftover is still reaped");
        kill_exact(leftover);
    }

    /// The daemon's CHILDREN are not exempt. They are the tool shells the
    /// agent starts — the 368 `yes` generators of the 01a1601b incident — so a
    /// blanket "skip the daemon's subtree" exemption would undo the reaper
    /// while fixing this.
    #[test]
    fn the_shared_daemons_own_children_in_the_workspace_are_still_reaped() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let guard = ws.begin();
        let plan = guard.plan.clone().expect("armed guard has a plan");

        let daemon_exe = install_fake_zeroclaw(&ws.root(), "/bin/sh");
        let pidfile = ws.cd.join("daemon-child.pid");
        let mut daemon = Command::new(&daemon_exe)
            .arg("-c")
            .arg(daemon_script(&pidfile))
            .current_dir(&ws.cd)
            .spawn()
            .expect("spawn the fake daemon");
        let daemon_pid = daemon.id();
        let agent_child = read_pidfile(&pidfile);
        wait_until(
            || alive(daemon_pid) && alive(agent_child),
            "the daemon and its agent child to be running",
        );

        let selected: Vec<u32> = scan(&plan).iter().map(|c| c.pid).collect();
        assert!(
            !selected.contains(&daemon_pid),
            "the daemon process itself is spared"
        );
        assert!(
            selected.contains(&agent_child),
            "the tool shell the agent started is an ordinary orphan and must be reaped"
        );

        let outcome = reap(&plan, Duration::from_millis(50));
        assert!(
            !alive(agent_child),
            "the daemon's leftovers in the workspace are exactly what the reap is for"
        );
        // The daemon is not merely spared from selection: it appears in no
        // reap outcome at all, not even as a survivor or a recycled skip.
        for entry in outcome
            .reaped
            .iter()
            .chain(outcome.skipped.iter())
            .chain(outcome.survivors.iter())
        {
            assert_ne!(
                entry.pid, daemon_pid,
                "the daemon must never be signalled, not even to find out it is gone"
            );
        }
        // The fixture daemon exits of its own accord once its last child is
        // gone, which is why its survival is asserted in
        // `a_shared_daemon_in_the_workspace_is_spared_and_...` instead.

        kill_exact(daemon_pid);
        let _ = daemon.wait();
    }

    /// …and the other direction: a process the daemon runs for SOMEONE ELSE
    /// is not ours. Its cwd is another job's workspace, which is outside
    /// `-C`, so the ownership test never reaches it — the exemption is not
    /// what protects it, and the reap must not start treating the daemon's
    /// whole subtree as fair game because of this fix.
    #[test]
    fn a_leftover_of_another_session_served_by_the_same_daemon_is_untouched() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let mut guard = ws.begin();
        let plan = guard.plan.clone().expect("armed guard has a plan");

        let daemon_exe = install_fake_zeroclaw(&ws.root(), "/bin/sh");
        let pidfile = ws.outside().join("other-session.pid");
        let mut daemon = Command::new(&daemon_exe)
            .arg("-c")
            .arg(daemon_script(&pidfile))
            .current_dir(ws.outside())
            .spawn()
            .expect("spawn the fake daemon");
        let daemon_pid = daemon.id();
        let other_sessions_leftover = read_pidfile(&pidfile);
        // …and one ordinary leftover of OUR OWN, so the run proves both halves
        // at once: the reap still works (this one dies) and it stops at the
        // boundary of this run's workspace (the other session's survives).
        // Without it, an implementation that blanket-spared the daemon's whole
        // subtree would pass this test.
        let ours = spawn_sleeper(&ws.cd);
        wait_until(
            || alive(daemon_pid) && alive(other_sessions_leftover) && alive(ours),
            "the daemon, the other session's leftover and our own to be running",
        );

        let selected: Vec<u32> = scan(&plan).iter().map(|c| c.pid).collect();
        assert!(
            !selected.contains(&daemon_pid) && !selected.contains(&other_sessions_leftover),
            "neither the daemon nor another session's leftover is in this run's scope"
        );
        assert!(
            selected.contains(&ours),
            "precondition: our own leftover in -C is a candidate, so the reap is armed"
        );

        guard.finish();
        assert!(
            alive(other_sessions_leftover),
            "a concurrent session's agent must not be killed by our teardown"
        );
        assert!(!alive(ours), "and our own workspace is still reaped");

        kill_exact(daemon_pid);
        kill_exact(other_sessions_leftover);
        kill_exact(ours);
        let _ = daemon.wait();
    }

    /// Executable identity is the exact binary, not a name and not a directory.
    /// A copy an agent drops in the workspace being reaped is the realistic
    /// leak for a name check, and it is exactly the process that must be reaped.
    #[test]
    fn the_executable_arm_matches_the_daemon_binary_and_not_its_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let installed = dir.path().join("zeroclaw");
        let link = dir.path().join("zeroclaw-link");
        let decoy_dir = dir.path().join("workspace");
        let decoy = decoy_dir.join("zeroclaw");
        let other = dir.path().join("zeroclaw.bak-20261011");
        std::fs::create_dir_all(&decoy_dir).expect("decoy dir");
        for path in [&installed, &decoy, &other] {
            std::fs::write(path, b"#!/bin/sh\nexit 0\n").expect("fixture binary");
        }
        std::os::unix::fs::symlink(&installed, &link).expect("symlinked install");

        assert!(executable_is_zeroclaw(&installed, &installed));
        // A symlinked install (`~/.local/bin/zeroclaw` -> the real binary) names
        // the same FILE, and /proc/<pid>/exe always names the resolved file.
        assert!(executable_is_zeroclaw(&link, &installed));
        // Right name, different file: an agent's copy in its own workspace.
        assert!(!executable_is_zeroclaw(&decoy, &installed));
        assert!(!executable_is_zeroclaw(&other, &installed));
        assert!(!executable_is_zeroclaw(Path::new("zeroclaw"), &installed));
        assert!(!executable_is_zeroclaw(Path::new("/"), &installed));
    }
}
