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
//! failure, `--loop-timeout`/`--agent-timeout`, SIGTERM, SIGINT) cancel the
//! agent session first, then kill what the agent left behind.
//!
//! Selection is deliberately narrow, because "kill stuff on exit" on a shared
//! host is how you take down somebody else's session:
//!
//! * same uid (`getuid()`) — never another user's processes,
//! * cwd inside the `-C` directory or zoder's own per-run scratch directory
//!   under the temp root — the workspace the agent was given. Never the temp
//!   root itself: on a shared host `/tmp` is everybody's cwd,
//! * started after the loop began (`/proc/<pid>/stat` `starttime`, which no
//!   pre-existing process can fake),
//! * not zoder and not one of zoder's ancestors — never our own tree.
//!
//! Selection is by exact PID only. There is no pattern matching anywhere here:
//! `pkill yes` would kill every `yes` on the host, including one a human
//! started. Linux-only via `/proc`; a no-op elsewhere.
//!
//! `--no-reap` turns the whole thing off.

use std::collections::HashSet;
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
/// Poll interval while waiting for a signalled process to disappear.
const POLL: Duration = Duration::from_millis(20);
/// How long to wait for a SIGKILLed process to leave the table. SIGKILL is not
/// catchable, so this only covers reaping.
const KILL_WAIT: Duration = Duration::from_millis(500);
/// Slack, in `/proc` clock ticks, subtracted from the loop-start boundary.
/// `starttime` has `USER_HZ` (normally 100 Hz = 10 ms) granularity, so the
/// boundary has to be rounded DOWN: a process the agent started 3 ms after
/// the loop began must still be caught. Erring the other way (reaping
/// something that predates the loop) needs a 10 ms overlap, which cannot
/// matter for a process whose cwd is the agent's own workspace.
const START_TICK_SLACK: u64 = 2;

/// Session ids this process drove, so the exit path can cancel them even when
/// the loop never reached its own author-timeout cancel. A ghost turn from
/// iteration 1 keeps editing the tree just as hard as one from iteration 9.
static SESSIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Record a session id the agent is driving. Called from the turn paths; the
/// exit path and the signal handler read it back.
pub(crate) fn note_session(session_id: &str) {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return;
    }
    let mut seen = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.iter().any(|s| s == session_id) {
        seen.push(session_id.to_string());
    }
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
}

impl ReapPlan {
    /// `temp_base` is the system temp root (`TMPDIR`, usually `/tmp`). It is
    /// NOT itself a reap root: on a shared host every unrelated process has a
    /// cwd somewhere under `/tmp`, and "same uid + cwd in /tmp + started
    /// recently" describes all of them. Instead this run gets its own scratch
    /// directory inside it, and that directory is the boundary.
    fn new(cwd: &Path, temp_base: &Path) -> Option<Self> {
        let started_ticks = boot_ticks()?;
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

impl ReapGuard {
    /// Begin reaping for a run rooted at `cwd` (the `-C` directory) whose
    /// scratch space is `tmp`. `enabled=false` (`--no-reap`) yields a guard
    /// that does nothing at all, including no signal handling.
    pub(crate) fn begin(cwd: &Path, tmp: &Path, enabled: bool) -> Self {
        let plan = enabled.then(|| ReapPlan::new(cwd, tmp)).flatten();
        if let Some(plan) = plan.as_ref() {
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
        cancel_recorded_sessions(budget).await;
        self.finish();
    }

    /// Reap now. Idempotent, and called from `Drop` so an unwinding panic
    /// still cleans up.
    pub(crate) fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let Some(plan) = self.plan.as_ref() else {
            return;
        };
        log_outcome(&reap(plan, REAP_GRACE));
        plan.cleanup_owned_roots();
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
             the signal, so no signal was sent",
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
        self.finish();
    }
}

/// Send `session/cancel` for every session this process drove, so a ghost
/// turn stops editing before we start killing. Best-effort: a turn that
/// already completed never acknowledges a cancel, and that is fine — we reap
/// either way.
async fn cancel_recorded_sessions(budget: Duration) {
    let ids = SESSIONS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if ids.is_empty() {
        return;
    }
    let socket = crate::engine_socket_path();
    for id in ids {
        match zoder_core::cancel_session(&socket, &id, budget).await {
            Ok(()) => eprintln!("[zoder] cancelled agent session {id}"),
            Err(e) => eprintln!(
                "[zoder] session {id} cancel not confirmed ({e:#}); \
                 a turn that already settled does not acknowledge a cancel"
            ),
        }
    }
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

/// SIGTERM, grace, SIGKILL — by exact PID, never by name or pattern.
fn reap(plan: &ReapPlan, grace: Duration) -> ReapOutcome {
    let uid = current_uid();
    let mut outcome = ReapOutcome::default();
    let mut seen: HashSet<u32> = HashSet::new();
    // Two passes: the second catches anything the agent spawned while the
    // first pass was still signalling (a parent that forks again before it
    // dies). `seen` keeps a pid from being reported twice.
    for _ in 0..2 {
        let targets: Vec<Candidate> = scan(plan)
            .into_iter()
            .filter(|c| seen.insert(c.pid))
            .collect();
        if targets.is_empty() {
            break;
        }
        for target in &targets {
            if confirm_identity(target, uid) != Identity::Same {
                // Gone, or the pid now belongs to somebody else.
                outcome.skipped.push(entry(target, ReapSignal::Term));
                continue;
            }
            signal(target.pid, libc_sigterm());
        }
        let deadline = Instant::now() + grace;
        while targets
            .iter()
            .any(|c| alive(c.pid) && confirm_identity(c, uid) == Identity::Same)
            && Instant::now() < deadline
        {
            std::thread::sleep(POLL);
        }
        for target in &targets {
            if outcome.skipped.iter().any(|s| s.pid == target.pid) {
                continue;
            }
            if confirm_identity(target, uid) != Identity::Same {
                outcome.skipped.push(entry(target, ReapSignal::Term));
                continue;
            }
            let entry = Reaped {
                pid: target.pid,
                comm: target.comm.clone(),
                cwd: target.cwd.clone(),
                signal: ReapSignal::Term,
            };
            if !alive(target.pid) {
                outcome.reaped.push(entry);
                continue;
            }
            signal(target.pid, libc_sigkill());
            let kill_deadline = Instant::now() + KILL_WAIT;
            while alive(target.pid) && Instant::now() < kill_deadline {
                std::thread::sleep(POLL);
            }
            let mut killed = entry;
            killed.signal = ReapSignal::Kill;
            // SIGKILL is uncatchable, so "still here" means the kernel is
            // holding it (uninterruptible sleep, a wedged D-state task) — a
            // real, visible failure rather than a silent success claim.
            if alive(target.pid) && confirm_identity(target, uid) == Identity::Same {
                outcome.survivors.push(killed);
            } else {
                outcome.reaped.push(killed);
            }
        }
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
    let plan = Arc::new(plan);
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[zoder] SIGTERM handler unavailable ({e}); reaping is signal-blind");
                return;
            }
        };
        let mut intr = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[zoder] SIGINT handler unavailable ({e}); reaping is signal-blind");
                return;
            }
        };
        let (name, code) = tokio::select! {
            _ = term.recv() => ("SIGTERM", 143),
            _ = intr.recv() => ("SIGINT", 130),
        };
        eprintln!(
            "[zoder] {name} received: cancelling the agent session and reaping \
             workspace processes before exit"
        );
        cancel_recorded_sessions(SIGNAL_CANCEL_BUDGET).await;
        log_outcome(&reap(&plan, SIGNAL_REAP_GRACE));
        plan.cleanup_owned_roots();
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
        .or_else(|| {
            after_open
                .char_indices()
                .find(|(i, c)| *c == ')' && valid_stat_tail(&after_open[i + 1..]))
        })
        .map(|(i, _)| i)?;
    let comm = after_open.get(..close)?.to_string();
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

/// Every process the loop may kill. Ordered by pid so the log is stable.
#[cfg(target_os = "linux")]
fn scan(plan: &ReapPlan) -> Vec<Candidate> {
    let uid = current_uid();
    let protected = protected_pids();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found: Vec<Candidate> = Vec::new();
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
        let Some(stat) = read_stat(pid) else {
            continue;
        };
        // A zombie is already dead and only holds a pid slot until its parent
        // reaps it. Signalling one does nothing.
        if stat.state == "Z" || stat.state == "X" {
            continue;
        }
        // Started before the loop did: not ours to kill, however it got a cwd
        // inside the workspace.
        if stat.start_ticks.saturating_add(START_TICK_SLACK) < plan.started_ticks {
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
            .arg("trap '' TERM; sleep 600")
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
        let mut guard = ReapGuard::begin(&ws.cd, &ws.tmp_root, true);
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
        let mut guard = ReapGuard::begin(&ws.cd, &ws.tmp_root, true);
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

        let mut guard = ReapGuard::begin(&ws.cd, &ws.tmp_root, true);
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
        let mut guard = ReapGuard::begin(&ws.cd, &ws.tmp_root, true);
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
        let mut guard = ReapGuard::begin(&ws.cd, &ws.tmp_root, true);
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
        let plan = ReapPlan::new(&ws.cd, &ws.tmp_root).expect("plan");
        let stubborn = spawn_sigterm_ignoring(&ws.cd);
        wait_until(
            || alive(stubborn),
            "the SIGTERM-ignoring fixture to be running",
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
        let mut guard = ReapGuard::begin(&ws.cd, &ws.tmp_root, false);
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
            let _guard = ReapGuard::begin(&ws.cd, &ws.tmp_root, true);
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
        let guard = ReapGuard::begin(&ws.cd, &ws.tmp_root, true);
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
            let mut guard = ReapGuard::begin(&ws.cd, &ws.tmp_root, true);
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
        let plan = ReapPlan::new(&ws.tmp_root, &ws.tmp_root).expect("plan");
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

    /// A root of `/` would select every process on the host. Refuse it.
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

    /// A root of `/` would select every process on the host. Refuse it.
    #[test]
    fn a_root_of_slash_arms_nothing() {
        assert!(
            ReapPlan::new(Path::new("/"), Path::new("/")).is_none(),
            "a / root must not produce a reap plan"
        );
    }

    /// The workspace and this run's scratch dir are in scope; the shared temp
    /// root, anything else, and `/` are not.
    #[test]
    fn plan_covers_the_cd_directory_and_this_runs_scratch_dir_only() {
        let ws = Workspace::new();
        let plan = ReapPlan::new(&ws.cd, &ws.tmp_root).expect("plan");
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
    }

    /// The reap is silent when there is nothing to reap: no noise on a normal
    /// run that cleaned up after itself.
    #[test]
    fn a_clean_run_reaps_nothing() {
        let _lock = REAP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ws = Workspace::new();
        let plan = ReapPlan::new(&ws.cd, &ws.tmp_root).expect("plan");
        let outcome = reap(&plan, Duration::from_millis(50));
        assert!(
            outcome.reaped.is_empty() && outcome.skipped.is_empty() && outcome.survivors.is_empty(),
            "an empty workspace must produce no reaped, skipped or surviving processes"
        );
    }
}
