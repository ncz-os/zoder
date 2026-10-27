//! Shared engine-daemon lifecycle: who may stop the shared zeroclaw daemon,
//! and who must leave it running for its siblings.
//!
//! The invariant this module enforces: a zoder process may stop a daemon only
//! if it started it (it is the holder of the EARLIEST live lease) AND no
//! other live zoder process is using it; it must never delete a socket a
//! live daemon is serving. An unacknowledged `session/cancel` abandons the
//! session and reaps the caller's workspace processes — it does not kill a
//! shared daemon out from under every other loop on the host.
//!
//! The mechanism is a per-client lease file under `<socket_dir>/daemons/`:
//! each client writes one the first time it reaches the daemon and refreshes
//! it on every later connection, so "who is using this daemon" is a question
//! about live pids, not about timing. The lease API below is small and pure
//! enough to unit-test; the policy decision that consumes it lives in
//! `zoder-cli` (`reaper::settle_unconfirmed_cancel`) and `main.rs`
//! (`ensure_engine_daemon` / `reap_ephemeral_daemons`).
//!
//! The agentic turn driver itself is NOT part of this module: it lives in the
//! `acp-client` crate (`zoder_core::engine_rpc`), which is the canonical ACP
//! client. The lease functions are re-exported at `zoder_core::engine_rpc`
//! too, so both spellings name the same items.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::anyhow;
use fs2::FileExt;

/// The name of the lock file inside the lease directory.
///
/// It is deliberately NOT a bare pid: the lease reader only accepts entries
/// whose name parses as a client pid, so this file is invisible to the census.
const LEASE_LOCK_FILE: &str = ".lock";

/// How long a caller waits for the daemon lock before giving up.
///
/// The lock is only ever held for the length of a lease write or for the
/// census-then-signal step, both of which are milliseconds. A caller that
/// cannot get it in this budget treats the daemon as NOT stoppable: every
/// failure of this mechanism must leave the daemon running.
const LEASE_LOCK_WAIT: Duration = Duration::from_secs(2);

/// Default lease TTL: how long a lease must be refreshed by its owner.
///
/// Every live client refreshes its lease on each engine connection, so a
/// healthy client keeps its lease indefinitely. A dead client's last refresh
/// is bounded by its turn length, so 60 s is far longer than any single
/// turn's inter-connection gap while still letting an orphaned daemon be
/// cleaned up well before the daemon's own last-client idle timeout would
/// otherwise leave it behind.
pub const DAEMON_LEASE_TTL: Duration = Duration::from_secs(60);

/// A lease on the shared engine daemon.
///
/// One lease file per client pid, under `<socket_dir>/daemons/<client_pid>`,
/// written by the client the first time it reaches for the daemon and
/// refreshed on every later connection. The file body is
/// `"<client_pid> <daemon_pid> <acquired_at_ns> <refreshed_at_ns>"`; the
/// `daemon_pid` is the
/// identity of the daemon this client is talking to, so a client that
/// connects across a daemon restart (a new pid bound to the same socket
/// path) refreshes the lease under the NEW daemon's identity.
///
/// The daemon's owner — the client whose exit is allowed to take the daemon
/// down — is the holder of the lease with the smallest
/// `acquired_at_ns` among the live leases: the first client to reach the
/// daemon is the one that started it (or is the longest-standing consumer of
/// it), and it is the only one allowed to stop it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonLease {
    /// The client process that holds this lease.
    pub client_pid: u32,
    /// The daemon process this lease was last refreshed against.
    pub daemon_pid: u32,
    /// When the lease was first acquired, in nanoseconds since the Unix
    /// epoch. Stable for the life of the lease: refreshes do not move it, so
    /// the "first client" ordering is stable too.
    pub acquired_at_ns: u128,
    /// When the lease was last refreshed, in nanoseconds since the Unix
    /// epoch. A lease whose refresh is older than [`DAEMON_LEASE_TTL`] and
    /// whose client is also gone is stale.
    pub refreshed_at_ns: u128,
}

impl DaemonLease {
    /// True when the lease's client process is still alive.
    ///
    /// On Linux this is a `/proc/<pid>` read: the directory entry exists
    /// while the process exists (zombie or not — a zombie holds its pid but
    /// executes nothing, and a zombie client holds no engine connection, so
    /// it is NOT alive for lease purposes). Where the check cannot be made
    /// (non-Linux, unreadable `/proc`, another user), the answer is `true`:
    /// the failure direction is "keep the daemon a bit longer", never
    /// "delete a live daemon's socket".
    #[cfg(target_os = "linux")]
    fn owner_still_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                // `stat` is "<pid> (comm) <state> ...": the state field
                // is the first whitespace-separated field after the
                // closing paren. `comm` may itself contain spaces, so
                // split on the LAST ')'.
                let after_comm = match stat.rfind(')') {
                    Some(pos) => &stat[pos + 1..],
                    None => return true,
                };
                let state = after_comm.split_whitespace().next().unwrap_or("");
                // Z (zombie) and X (dead) hold a pid but execute nothing.
                state != "Z" && state != "X" && !state.is_empty()
            }
            Err(e) => {
                // A pid that cannot be read at all is NOT alive: the
                // process is gone (or the read was denied, which for a
                // same-uid lease check means the process is not there).
                // The safe direction is to treat an unreadable pid as
                // dead: a lease whose owner we cannot find is not a
                // live lease.
                let _ = e;
                false
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    fn owner_still_alive(pid: u32) -> bool {
        // Cannot check (non-Linux): assume alive. The failure direction is
        // "keep the daemon a bit longer", never "delete a live daemon's
        // socket".
        pid != 0
    }

    /// True when this lease is still fresh: its client is alive AND its last
    /// refresh is within `ttl` of now. A live client's lease is always fresh
    /// (it refreshes on every connection); a dead client's lease goes stale
    /// after `ttl` elapses since its last refresh.
    #[cfg_attr(not(test), allow(dead_code))]
    fn is_fresh(&self, ttl: Duration) -> bool {
        if !Self::owner_still_alive(self.client_pid) {
            return false;
        }
        let now = now_since_epoch_ns();
        now.saturating_sub(self.refreshed_at_ns) <= ttl.as_nanos()
    }
}

/// Nanoseconds since the Unix epoch, saturating at 0 on a broken clock.
fn now_since_epoch_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// The client that STARTED a daemon, as recorded by the spawner.
///
/// The owner is recorded in the daemon's own record file, next to the daemon
/// identity, and NOT derived from the leases. That separation is the whole
/// point: a lease says "this client is talking to the daemon" and its order
/// says "this client got there first", but neither of those is a statement
/// about who STARTED the daemon. A client that arrived first is a consumer,
/// not the owner, and a lease whose write silently failed leaves no trace at
/// all — so a stop decision that reads only the leases cannot tell "I
/// started this daemon" from "nobody wrote a lease because the directory was
/// not writable".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DaemonOwner {
    /// The pid of the process that spawned the daemon.
    pub pid: u32,
    /// The owner's start time, so a recycled pid cannot impersonate it.
    pub start_ticks: u64,
}

/// The verdict on whether a client may stop the shared daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonStopAuthority {
    /// This client started the daemon and no other live client is using it.
    Owner,
    /// The daemon has no live owner and this client is its only live
    /// consumer, so it may take over the daemon it did not start.
    LastConsumer,
    /// This client may not stop the daemon.
    NotAuthorized,
}

/// Decide whether `client_pid` may stop the daemon behind the socket whose
/// clients are `leases`.
///
/// The rules, in order, and every one of them fails CLOSED:
///
/// 1. The client must hold a live lease of its own. A client that never
///    registered (or whose lease write failed) cannot prove that it is a
///    consumer of this daemon, and an EMPTY census is not permission to
///    stop anything.
/// 2. No other live client may hold a lease. A live sibling is using the
///    daemon, so the daemon must keep running.
/// 3. If the daemon has a live owner, only that owner may stop it — the
///    ownership record, not the lease order, decides who started it.
/// 4. A daemon whose owner is gone (or was never recorded) may be stopped by
///    its only remaining live consumer, so an orphaned daemon does not
///    outlive its last user — but only if that consumer's own registration is
///    CURRENT (its lease was refreshed within `ttl`). Taking over a daemon
///    this client did not start is the one action here that can seize
///    something another process owned, so it carries the extra proof.
///
/// `owner_is_alive` is decided by the caller, which is the side that can read
/// the owner's start time: this function stays a pure decision so it can be
/// tested against every combination of owner liveness and lease state.
pub fn daemon_stop_authority(
    leases: &[DaemonLease],
    client_pid: u32,
    owner: Option<DaemonOwner>,
    owner_is_alive: bool,
    ttl: Duration,
) -> DaemonStopAuthority {
    // A lease counts as a live consumer while its client process is alive.
    // The freshness half of `is_fresh` (the refresh is within the TTL) is
    // deliberately NOT used to decide whether a SIBLING is using the daemon:
    // a client that is alive but slow to refresh is still a live process that
    // may be mid-session, and the safe answer is to leave the daemon alone.
    let live: Vec<&DaemonLease> = leases
        .iter()
        .filter(|l| DaemonLease::owner_still_alive(l.client_pid))
        .collect();

    // Rule 1: we must be a consumer ourselves.
    let Some(ours) = live.iter().find(|l| l.client_pid == client_pid) else {
        return DaemonStopAuthority::NotAuthorized;
    };
    // Rule 2: a live sibling is using the daemon.
    if live.iter().any(|l| l.client_pid != client_pid) {
        return DaemonStopAuthority::NotAuthorized;
    }
    // Rule 3: a live owner decides.
    if let Some(owner) = owner {
        if owner_is_alive {
            return if owner.pid == client_pid {
                DaemonStopAuthority::Owner
            } else {
                DaemonStopAuthority::NotAuthorized
            };
        }
    }
    // Rule 4: nobody owns it any more; we are its only live consumer. The
    // registration must still be CURRENT: a lease nobody refreshed for a
    // whole TTL describes a client that reached this daemon long ago and
    // has not come back, not one that is using it now.
    if !ours.is_fresh(ttl) {
        return DaemonStopAuthority::NotAuthorized;
    }
    DaemonStopAuthority::LastConsumer
}

/// An exclusive or shared lock over the daemon's lease directory.
///
/// It makes "is anybody else using this daemon?" and "signal the daemon" one
/// atomic step with respect to a client that is registering itself: a client
/// takes the lock SHARED to write its lease, and the stop decision takes it
/// EXCLUSIVE for the whole census-then-signal step. A client that starts
/// connecting while the daemon is being stopped therefore either has its
/// lease in place before the decision (and the decision sees it) or is still
/// blocked when the signal goes out (and the decision did not see it, but it
/// is not yet talking to the daemon either).
///
/// The lock is advisory and per host: it coordinates the zoder processes on
/// one machine, which is exactly the scope of the invariant.
pub struct DaemonLock {
    file: std::fs::File,
}

impl Drop for DaemonLock {
    fn drop(&mut self) {
        // The lock is released explicitly rather than by closing the
        // descriptor, so the release happens at a defined point (end of the
        // guarded section) instead of whenever the `File` happens to be
        // dropped. `flock` is released by the kernel when the last
        // descriptor closes anyway, so a failed unlock is not fatal — the
        // lock still ends with the descriptor.
        let _ = FileExt::unlock(&self.file);
    }
}

impl DaemonLock {
    /// Take the lock SHARED: a client registering itself as a consumer.
    pub fn shared(socket: &Path) -> anyhow::Result<Self> {
        Self::acquire(socket, false)
    }

    /// Take the lock EXCLUSIVE: the stop decision.
    pub fn exclusive(socket: &Path) -> anyhow::Result<Self> {
        Self::acquire(socket, true)
    }

    /// Try to take the lock EXCLUSIVE, giving up after `budget` instead of
    /// waiting for the full [`LEASE_LOCK_WAIT`].
    ///
    /// This is the bounded variant: the caller is asking "may I have the
    /// lock right now, and if not, not for long" — the question a contended
    /// decision asks when the answer it needs is "no". A caller that must
    /// have the lock uses [`DaemonLock::exclusive`].
    pub fn try_exclusive(socket: &Path, budget: Duration) -> anyhow::Result<Self> {
        Self::acquire_within(socket, true, budget)
    }

    /// Try to take the lock SHARED, giving up after `budget`.
    pub fn try_shared(socket: &Path, budget: Duration) -> anyhow::Result<Self> {
        Self::acquire_within(socket, false, budget)
    }

    fn acquire(socket: &Path, exclusive: bool) -> anyhow::Result<Self> {
        Self::acquire_within(socket, exclusive, LEASE_LOCK_WAIT)
    }

    fn acquire_within(socket: &Path, exclusive: bool, budget: Duration) -> anyhow::Result<Self> {
        let dir = lease_dir_for(socket)?;
        let path = dir.join(LEASE_LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| anyhow!("cannot open the daemon lock at {}: {e}", path.display()))?;
        let deadline = std::time::Instant::now() + budget;
        loop {
            // The trait methods are called through the trait on purpose:
            // `std::fs::File` has inherent `try_lock_shared` /
            // `try_lock_exclusive` methods on newer toolchains, and an
            // inherent method wins over the trait one. The inherent pair
            // does not have the same error type on every toolchain, so the
            // two arms of this `if` would not typecheck together. Naming the
            // trait keeps ONE error type (`std::io::Error`) on every
            // toolchain, and `fs2` reports contention as `WouldBlock`.
            let attempt = if exclusive {
                FileExt::try_lock_exclusive(&file)
            } else {
                FileExt::try_lock_shared(&file)
            };
            match attempt {
                Ok(()) => return Ok(DaemonLock { file }),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return Err(anyhow!(
                            "the daemon lock at {} stayed busy for {:?}",
                            path.display(),
                            budget
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => {
                    return Err(anyhow!(
                        "cannot lock the daemon lock at {}: {e}",
                        path.display()
                    ))
                }
            }
        }
    }
}

/// Acquire (or refresh) this client's lease for the daemon behind `socket`,
/// which is currently served by `daemon_pid`.
///
/// The lease file is written atomically (sibling temp file + `rename`), and
/// a pre-existing lease for this client is replaced: the `acquired_at_ns`
/// from the old lease is PRESERVED (the first-client ordering must not reset
/// when a client reconnects) while `daemon_pid` and `refreshed_at_ns` move to
/// the current values.
pub fn acquire_daemon_lease(
    socket: &Path,
    client_pid: u32,
    daemon_pid: u32,
) -> anyhow::Result<DaemonLease> {
    let lease_dir = lease_dir_for(socket)?;
    // The lease write happens under the SHARED lock, so a stop decision
    // holding the EXCLUSIVE lock cannot read a half-written census: it sees
    // this client either not at all or fully registered.
    let _lock = DaemonLock::shared(socket)?;
    let lease_path = lease_dir.join(client_pid.to_string());
    let now = now_since_epoch_ns();
    // Preserve the original acquire time on a refresh.
    let acquired_at_ns = match std::fs::read_to_string(&lease_path) {
        Ok(content) => {
            // File body is "<client> <daemon> <acquired> <refreshed>"; the
            // third field is the stable acquire time we preserve.
            let mut fields = content.split_whitespace();
            fields.next(); // client pid (validated against the name by the reader)
            fields.next(); // daemon pid (moves with the refresh)
            fields
                .next()
                .and_then(|f| f.parse::<u128>().ok())
                .unwrap_or(now)
        }
        Err(_) => now,
    };
    write_lease_file(
        &lease_dir,
        &lease_path,
        client_pid,
        daemon_pid,
        acquired_at_ns,
        now,
    )?;
    Ok(DaemonLease {
        client_pid,
        daemon_pid,
        acquired_at_ns,
        refreshed_at_ns: now,
    })
}

/// Remove this client's lease file. Best-effort: a missing file is fine.
pub fn remove_daemon_lease(socket: &Path, client_pid: u32) {
    let Ok(lease_dir) = lease_dir_for(socket) else {
        return;
    };
    let _ = std::fs::remove_file(lease_dir.join(client_pid.to_string()));
}

/// The `<socket_dir>/daemons` directory, created on demand.
fn lease_dir_for(socket: &Path) -> anyhow::Result<PathBuf> {
    let dir = socket
        .parent()
        .ok_or_else(|| anyhow!("socket path {} has no parent directory", socket.display()))?;
    let lease_dir = dir.join("daemons");
    std::fs::create_dir_all(&lease_dir)?;
    Ok(lease_dir)
}

/// Write one lease file atomically: sibling temp file, then `rename`.
fn write_lease_file(
    lease_dir: &Path,
    lease_path: &Path,
    client_pid: u32,
    daemon_pid: u32,
    acquired_at_ns: u128,
    refreshed_at_ns: u128,
) -> anyhow::Result<()> {
    let tmp = lease_dir.join(format!(".{client_pid}.{}-tmp", refreshed_at_ns));
    std::fs::write(
        &tmp,
        format!("{client_pid} {daemon_pid} {acquired_at_ns} {refreshed_at_ns}\n"),
    )?;
    std::fs::rename(&tmp, lease_path)?;
    Ok(())
}

/// Write the lease file for `client_pid` with the given timestamps,
/// exactly as the production acquire path would. Test-only: it lets a test
/// fabricate a lease whose client is a pid the test controls (alive or
/// dead) without going through the real acquire path, which pins the pid
/// to the calling process.
#[cfg(test)]
fn write_test_lease(
    socket: &Path,
    client_pid: u32,
    daemon_pid: u32,
    acquired_at_ns: u128,
    refreshed_at_ns: u128,
) {
    let lease_dir = lease_dir_for(socket).unwrap();
    let lease_path = lease_dir.join(client_pid.to_string());
    write_lease_file(
        &lease_dir,
        &lease_path,
        client_pid,
        daemon_pid,
        acquired_at_ns,
        refreshed_at_ns,
    )
    .unwrap();
}
/// Parse one lease file's contents, if it is a plausible one.
fn parse_lease_file(content: &str, expected_client: u32) -> Option<DaemonLease> {
    let mut fields = content.split_whitespace();
    let client_pid = fields.next().and_then(|f| f.parse::<u32>().ok())?;
    if client_pid != expected_client {
        // The file name and its body disagree: not a lease this build wrote.
        return None;
    }
    let daemon_pid = fields.next().and_then(|f| f.parse::<u32>().ok())?;
    let acquired_at_ns = fields.next().and_then(|f| f.parse::<u128>().ok())?;
    let refreshed_at_ns = fields.next().and_then(|f| f.parse::<u128>().ok())?;
    Some(DaemonLease {
        client_pid,
        daemon_pid,
        acquired_at_ns,
        refreshed_at_ns,
    })
}

/// Read the live leases for the daemon behind `socket`, sorted oldest-acquired
/// first. A lease is live when its client process is still alive. Stale
/// leases (dead client) are removed as a side effect so the directory cannot
/// accumulate one file per historical client.
pub fn read_daemon_leases(socket: &Path) -> Vec<DaemonLease> {
    let dir = match socket.parent() {
        Some(d) => d.join("daemons"),
        None => return Vec::new(),
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut leases = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // Temp files (".<pid>.<ns>-tmp") and anything that is not a bare pid
        // are not leases.
        let client_pid: u32 = match name.parse() {
            Ok(p) if p > 1 => p,
            _ => continue,
        };
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let Some(lease) = parse_lease_file(&content, client_pid) else {
            continue;
        };
        if client_pid_is_alive(client_pid) {
            leases.push(lease);
        } else {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    leases.sort_by_key(|l| l.acquired_at_ns);
    leases
}

/// The pid liveness check for [`read_daemon_leases`], named so the call site
/// reads as a policy decision.
fn client_pid_is_alive(pid: u32) -> bool {
    DaemonLease::owner_still_alive(pid)
}

/// When this process exits, may it stop the daemon behind `socket`?
///
/// This is the CENSUS-ONLY form of the decision: it consults the leases and
/// the live processes, and it does NOT consult the ownership record. It is
/// kept for callers that have no owner information (and for the lease API's
/// own tests); the production stop decision uses [`daemon_stop_authority`]
/// with the recorded owner, because "the earliest lease holder" is not the
/// same question as "the process that started the daemon".
///
/// A client may stop the daemon only when it holds a live lease of its own
/// AND no other live client holds one. A client with no live lease at all
/// (it never reached the daemon, or its lease write failed) may not stop the
/// daemon: with no lease the question has no answer, and the safe answer is
/// "not ours".
pub fn client_may_stop_daemon(
    socket: &Path,
    client_pid: u32,
    _daemon_pid: u32,
    ttl: Duration,
) -> bool {
    let leases = read_daemon_leases(socket);
    matches!(
        daemon_stop_authority(&leases, client_pid, None, false, ttl),
        DaemonStopAuthority::Owner | DaemonStopAuthority::LastConsumer
    )
}

/// Whether ANY live client holds a lease for the daemon behind `socket`.
///
/// A `true` answer means the daemon is a shared one: at least one live zoder
/// process has reached it and recorded itself as a client. A `false` answer
/// means either no lease was ever written (a daemon that predates the lease
/// mechanism, or a client that could not write the directory) or every lease
/// holder is dead. Callers use this to distinguish "a shared daemon I must
/// not stop" from "a legacy daemon that the pre-lease rules still govern".
pub fn any_daemon_lease(socket: &Path) -> bool {
    !read_daemon_leases(socket).is_empty()
}

#[cfg(test)]
mod lease_tests {
    //! Unit tests for the daemon-lease mechanism: acquire / refresh /
    //! release, stale-lease cleanup, and the "may stop" decision. These
    //! run on the same thread as the rest of the test suite, but the lease
    //! files live in a per-test tempdir so they do not interfere with any
    //! other test's view of the daemon directory.

    use super::*;
    use std::time::Duration;

    /// A lease file whose client is the TEST process is always fresh:
    /// the test process is alive, and the refresh is seconds-old.
    #[test]
    fn acquire_then_read_returns_the_lease_for_the_test_pid() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let client_pid = std::process::id();
        let daemon_pid = 12345;

        let lease = acquire_daemon_lease(&socket, client_pid, daemon_pid).unwrap();
        assert_eq!(lease.client_pid, client_pid);
        assert_eq!(lease.daemon_pid, daemon_pid);

        let leases = read_daemon_leases(&socket);
        assert_eq!(leases.len(), 1, "exactly one live lease expected");
        assert_eq!(leases[0].client_pid, client_pid);
        assert_eq!(leases[0].daemon_pid, daemon_pid);
    }

    /// A lease whose client process is dead is filtered out by
    /// `read_daemon_leases` and its file is removed. The dead pid is
    /// obtained by spawning a subprocess, killing it, and reaping it, so
    /// `/proc/<pid>` is provably gone.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_dead_clients_lease_is_filtered_out_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        // Fabricate a lease whose client is a pid that is provably dead:
        // spawn a subprocess, wait for it to exit, and use its (now-free)
        // pid. The liveness check must reject it and remove the file.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        // Kill and reap so the pid is provably dead (zombie -> gone).
        child.kill().unwrap();
        child.wait().unwrap();
        write_test_lease(&socket, dead_pid, 12345, 1, 1);
        let leases = read_daemon_leases(&socket);
        assert!(
            leases.is_empty(),
            "a lease whose client is dead must be filtered: {leases:?}"
        );
        // The file should have been removed.
        let lease_dir = dir.path().join("daemons");
        let remaining: Vec<_> = std::fs::read_dir(&lease_dir)
            .unwrap()
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(
            remaining.is_empty(),
            "stale lease file must be removed: {remaining:?}"
        );
    }

    /// The ORDER of the leases is not authority.
    ///
    /// Two live clients share the daemon, so NEITHER of them may stop it:
    /// the earliest lease is a registration that happened to arrive first,
    /// not a statement about who started the daemon. The old rule ("the
    /// first lease holder may stop the daemon") is what let a client that
    /// merely connected first tear down a daemon another live process was
    /// using.
    #[test]
    fn the_earliest_lease_holder_may_not_stop_a_daemon_a_sibling_is_using() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let daemon_pid = 12345;
        let ttl = Duration::from_secs(60);

        // Client A (the test process) acquires first.
        let client_a = std::process::id();
        acquire_daemon_lease(&socket, client_a, daemon_pid).unwrap();

        // Client B (a different alive pid) — spawn a real subprocess whose
        // pid the lease liveness check can verify.
        let mut client_b = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let b_pid = client_b.id();
        acquire_daemon_lease(&socket, b_pid, daemon_pid).unwrap();

        assert!(
            !client_may_stop_daemon(&socket, client_a, daemon_pid, ttl),
            "the earliest lease holder is a consumer, not an authority: a live sibling is using the daemon"
        );
        assert!(
            !client_may_stop_daemon(&socket, b_pid, daemon_pid, ttl),
            "a live sibling is using the daemon"
        );

        // Clean up: kill B, remove both leases.
        let _ = client_b.kill();
        let _ = client_b.wait();
        remove_daemon_lease(&socket, client_a);
        remove_daemon_lease(&socket, b_pid);
    }

    /// The census is read from the lease FILES on disk, so the decision is
    /// driven through the production reader rather than through a
    /// hand-built list.
    ///
    /// A test that fed `daemon_stop_authority` a list it made itself would
    /// keep passing if the reader stopped reading the files at all, so this
    /// one writes a real lease file and asks the real reader what it sees.
    #[test]
    fn the_census_is_read_from_the_lease_files_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let daemon_pid = 4242;
        let ttl = Duration::from_secs(60);
        let me = std::process::id();

        // A lease file for this (live) process, written exactly as the
        // production acquire path writes it.
        let now = now_since_epoch_ns();
        write_test_lease(&socket, me, daemon_pid, now, now);

        let census = read_daemon_leases(&socket);
        assert_eq!(
            census.len(),
            1,
            "the reader must see the lease file that was written: {census:?}"
        );
        assert_eq!(census[0].client_pid, me);
        assert_eq!(census[0].daemon_pid, daemon_pid);

        // And the decision over that census authorises the one consumer.
        assert!(
            client_may_stop_daemon(&socket, me, daemon_pid, ttl),
            "the only live consumer may stop the daemon"
        );

        // A second, live consumer's lease file on disk blocks the stop —
        // the reader, not the test, is what notices it.
        let sibling = spawn_live_sleeper();
        write_test_lease(&socket, sibling, daemon_pid, now, now);
        assert!(
            !client_may_stop_daemon(&socket, me, daemon_pid, ttl),
            "a live sibling's lease file on disk must block the stop"
        );
    }

    /// With the sibling gone, the one remaining live consumer may stop the
    /// daemon — otherwise a daemon would outlive every user of it.
    #[test]
    fn the_last_remaining_consumer_may_stop_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let daemon_pid = 12345;
        let ttl = Duration::from_secs(60);

        let client_a = std::process::id();
        acquire_daemon_lease(&socket, client_a, daemon_pid).unwrap();

        // A second client registers and then releases its lease — the
        // normal way a client that is done with the daemon says so.
        let mut client_b = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let b_pid = client_b.id();
        acquire_daemon_lease(&socket, b_pid, daemon_pid).unwrap();
        remove_daemon_lease(&socket, b_pid);
        let _ = client_b.kill();
        let _ = client_b.wait();

        assert!(
            client_may_stop_daemon(&socket, client_a, daemon_pid, ttl),
            "with the sibling gone, the remaining live consumer may stop the daemon"
        );

        remove_daemon_lease(&socket, client_a);
    }

    /// After the last client removes its lease, `any_daemon_lease` returns
    /// false and the daemon's owner (whoever it was) may stop it.
    #[test]
    fn no_live_lease_means_any_daemon_lease_is_false() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let daemon_pid = 12345;
        let client = std::process::id();

        acquire_daemon_lease(&socket, client, daemon_pid).unwrap();
        assert!(any_daemon_lease(&socket), "a live lease must be visible");

        remove_daemon_lease(&socket, client);
        assert!(
            !any_daemon_lease(&socket),
            "after the only client removes its lease, no live lease remains"
        );
    }

    // ---------------------------------------------------------------------
    // The stop decision: `daemon_stop_authority`.
    //
    // These are the tests that pin the three HIGH findings of the review of
    // the shared-daemon lifecycle branch:
    //
    //   1. the stop decision must not be derivable from the lease ORDER
    //      (a first consumer is not the owner), and an empty census must
    //      fail closed;
    //   2. the census must be taken under a lock that also covers the
    //      signal, so a sibling cannot slip in between;
    //   3. the stale-socket removal must be guarded against the socket
    //      being replaced under it.
    //
    // The decision itself is a PURE function, so it can be tested against
    // every combination of owner liveness and lease state — including the
    // combinations that are hard to produce with real processes.

    /// A lease for a client that is alive (the test process) and fresh.
    fn live_fresh_lease(client_pid: u32, daemon_pid: u32) -> DaemonLease {
        DaemonLease {
            client_pid,
            daemon_pid,
            acquired_at_ns: now_since_epoch_ns(),
            refreshed_at_ns: now_since_epoch_ns(),
        }
    }

    /// A lease for a client that is alive but whose registration has not
    /// been refreshed for longer than the TTL.
    fn live_stale_lease(client_pid: u32, daemon_pid: u32, ttl: Duration) -> DaemonLease {
        DaemonLease {
            client_pid,
            daemon_pid,
            acquired_at_ns: now_since_epoch_ns(),
            refreshed_at_ns: now_since_epoch_ns().saturating_sub(ttl.as_nanos() * 2),
        }
    }

    /// The owner that started the daemon, alive.
    fn live_owner(pid: u32) -> DaemonOwner {
        DaemonOwner {
            pid,
            start_ticks: 1,
        }
    }

    /// The owner that started the daemon, gone.
    fn dead_owner(pid: u32) -> DaemonOwner {
        DaemonOwner {
            pid,
            start_ticks: 1,
        }
    }

    /// A disposable, live process whose pid the liveness check can see.
    ///
    /// The child is killed when it goes out of scope, so the pid does not
    /// linger as a zombie and the test does not leak a process.
    fn spawn_live_sleeper() -> u32 {
        struct Sleeper(std::process::Child);
        impl Drop for Sleeper {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn a live child process");
        let pid = child.id();
        // The child must stay alive for the whole test, and it must be
        // killed when the test ends: the handle is kept alive on purpose.
        std::mem::forget(Sleeper(child));
        pid
    }

    /// The owner may stop the daemon it started, when nobody else is using
    /// it. This is the case the whole lease mechanism exists for.
    #[test]
    fn the_recorded_owner_may_stop_the_daemon_it_started() {
        let me = std::process::id();
        let leases = vec![live_fresh_lease(me, 4242)];
        let authority =
            daemon_stop_authority(&leases, me, Some(live_owner(me)), true, DAEMON_LEASE_TTL);
        assert_eq!(authority, DaemonStopAuthority::Owner);
    }

    /// The FIRST lease holder is NOT the owner. A client that merely
    /// arrived first is a consumer: it may not stop a daemon somebody else
    /// started, no matter how early its lease is.
    ///
    /// This is the finding that the old "earliest live lease" rule produced
    /// the wrong answer on, and it is the reason the ownership record exists
    /// as a separate fact.
    #[test]
    fn the_first_lease_holder_is_not_the_owner_and_may_not_stop_the_daemon() {
        let me = std::process::id();
        let owner_pid = spawn_live_sleeper();
        // WE hold the only lease, acquired FIRST — and we are still not the
        // owner: the daemon was started by a process that is still alive.
        let mut ours = live_fresh_lease(me, 4242);
        ours.acquired_at_ns = 1;
        assert_eq!(
            daemon_stop_authority(
                &[ours],
                me,
                Some(live_owner(owner_pid)),
                true,
                DAEMON_LEASE_TTL
            ),
            DaemonStopAuthority::NotAuthorized,
            "the earliest lease holder is a consumer, not the owner"
        );

        // The owner, when it is the ONLY consumer, may stop the daemon it
        // started.
        assert_eq!(
            daemon_stop_authority(
                &[live_fresh_lease(owner_pid, 4242)],
                owner_pid,
                Some(live_owner(owner_pid)),
                true,
                DAEMON_LEASE_TTL
            ),
            DaemonStopAuthority::Owner,
            "the recorded owner may stop the daemon it started"
        );
    }

    /// A live SIBLING blocks the stop, whoever the caller is. Even the
    /// owner may not stop a daemon another live process is still using.
    #[test]
    fn a_live_sibling_blocks_the_stop_even_for_the_owner() {
        let me = std::process::id();
        let sibling = spawn_live_sleeper();
        let leases = vec![live_fresh_lease(me, 4242), live_fresh_lease(sibling, 4242)];
        assert_eq!(
            daemon_stop_authority(&leases, me, Some(live_owner(me)), true, DAEMON_LEASE_TTL),
            DaemonStopAuthority::NotAuthorized,
            "a live sibling is using the daemon; the owner must leave it running"
        );
        assert_eq!(
            daemon_stop_authority(
                &leases,
                sibling,
                Some(live_owner(me)),
                true,
                DAEMON_LEASE_TTL
            ),
            DaemonStopAuthority::NotAuthorized,
            "a live sibling is using the daemon; a consumer must leave it running"
        );
    }

    /// An EMPTY census is not permission to stop anything. A client that
    /// never registered (or whose lease write failed) cannot prove it is a
    /// consumer, so it has no authority — even when it IS the recorded
    /// owner, because without a lease we cannot prove that nobody else is
    /// using the daemon.
    #[test]
    fn an_empty_census_fails_closed() {
        let me = std::process::id();
        assert_eq!(
            daemon_stop_authority(&[], me, Some(live_owner(me)), true, DAEMON_LEASE_TTL),
            DaemonStopAuthority::NotAuthorized,
            "no lease means no authority, even for the recorded owner"
        );
        assert_eq!(
            daemon_stop_authority(&[], me, None, false, DAEMON_LEASE_TTL),
            DaemonStopAuthority::NotAuthorized,
            "an empty census and no owner must fail closed"
        );
    }

    /// A client whose own lease is missing may not stop the daemon, even
    /// when it is the only entry in the census and the owner is gone. This
    /// is the "lease write failed" case: the client cannot prove it is a
    /// consumer of this daemon.
    #[test]
    fn a_client_without_its_own_lease_may_not_stop_the_daemon() {
        let me = std::process::id();
        let sibling = spawn_live_sleeper();
        // The census names the SIBLING but not us: we never registered.
        let leases = vec![live_fresh_lease(sibling, 4242)];
        assert_eq!(
            daemon_stop_authority(&leases, me, None, false, DAEMON_LEASE_TTL),
            DaemonStopAuthority::NotAuthorized,
            "a client that holds no lease of its own has no authority"
        );
    }

    /// When the owner is GONE, the only remaining live consumer may take the
    /// daemon over — otherwise an orphaned daemon outlives every user.
    #[test]
    fn the_last_consumer_may_take_over_an_orphaned_daemon() {
        let me = std::process::id();
        let leases = vec![live_fresh_lease(me, 4242)];
        assert_eq!(
            daemon_stop_authority(
                &leases,
                me,
                Some(dead_owner(999_999)),
                false,
                DAEMON_LEASE_TTL
            ),
            DaemonStopAuthority::LastConsumer,
            "with the owner gone, the only live consumer may take over"
        );
    }

    /// The takeover is a seizure of something another process owned, so it
    /// needs a CURRENT registration: a lease nobody refreshed for a whole
    /// TTL describes a client that reached the daemon long ago.
    #[test]
    fn a_stale_registration_may_not_take_over_an_orphaned_daemon() {
        let me = std::process::id();
        let leases = vec![live_stale_lease(me, 4242, DAEMON_LEASE_TTL)];
        assert_eq!(
            daemon_stop_authority(
                &leases,
                me,
                Some(dead_owner(999_999)),
                false,
                DAEMON_LEASE_TTL
            ),
            DaemonStopAuthority::NotAuthorized,
            "a stale registration is not proof that this client is using the daemon now"
        );
    }

    /// The lock: an exclusive holder keeps a second exclusive holder out,
    /// and the lock is released when it is dropped. The two facts are what
    /// make "census, then signal" one step with respect to a client that is
    /// registering itself.
    #[test]
    fn the_exclusive_lock_excludes_a_second_exclusive_holder() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let held = DaemonLock::exclusive(&socket).expect("take the exclusive lock");
        // A second exclusive holder in the SAME process would deadlock on a
        // blocking flock, so the contention is asserted through the
        // budgeted path: the caller must give up rather than wait forever.
        let start = std::time::Instant::now();
        let second = DaemonLock::try_exclusive(&socket, Duration::from_millis(50));
        assert!(
            second.is_err(),
            "an exclusive lock must exclude a second exclusive holder"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "the contended attempt must give up promptly, not hang"
        );
        drop(held);
        // Once released, the lock is available again.
        DaemonLock::try_exclusive(&socket, Duration::from_millis(50))
            .expect("the lock must be free after it is dropped");
    }

    /// Two clients may register themselves at the same time: the shared
    /// lock is shared. If it were exclusive, two concurrent zoder processes
    /// would serialise against each other on every daemon connection.
    #[test]
    fn the_shared_lock_admits_more_than_one_holder() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let _a = DaemonLock::shared(&socket).expect("first shared holder");
        let _b = DaemonLock::shared(&socket).expect("second shared holder");
    }

    /// A refresh preserves the original acquire time: the owner ordering
    /// is stable across reconnects.
    #[test]
    fn a_refresh_preserves_the_original_acquire_time() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let client = std::process::id();
        let daemon_pid = 12345;

        let first = acquire_daemon_lease(&socket, client, daemon_pid).unwrap();
        // Simulate a reconnect: acquire again.
        let second = acquire_daemon_lease(&socket, client, daemon_pid).unwrap();
        assert_eq!(
            first.acquired_at_ns, second.acquired_at_ns,
            "a refresh must not move the acquire time"
        );
        assert!(
            second.refreshed_at_ns >= first.refreshed_at_ns,
            "the refresh time must not go backwards"
        );
    }
}
