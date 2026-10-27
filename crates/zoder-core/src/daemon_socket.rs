//! The stale-socket cleanup: the one decision that may unlink a daemon's
//! socket.
//!
//! The socket is the daemon's only name. A Unix socket that is unlinked
//! while its daemon is alive leaves a daemon that nothing can reach any
//! more: the kernel keeps the listening socket, but the path no longer
//! names it, so every future `connect(2)` fails. That is why this policy
//! lives on its own, behind a name that says what it decides, and why every
//! arm of it fails towards "leave the socket alone".
//!
//! The decision is one decision about ONE directory entry:
//!
//! * the entry's identity (device + inode) is captured BEFORE the probe, so
//!   the removal is about the entry the probe actually examined;
//! * the daemon lock is held across the re-check and the removal, so two
//!   zoder processes cannot both decide to clean up the same path, and a
//!   client that is registering itself is not half-way through a lease
//!   write while this runs;
//! * the identity is re-checked immediately before the removal, so a daemon
//!   that bound the path after the probe is never unlinked;
//! * the probe's own answer decides the rest: only a socket that NOTHING
//!   answers on is removed.

use std::path::Path;
use std::time::Duration;

use crate::engine_rpc_impl::DaemonLock;
use crate::{probe_ready, DaemonReadiness};

/// The identity of the directory entry at `path`: its device and inode.
///
/// A Unix socket that is re-bound gets a NEW inode, so a change of identity
/// between two observations of the same path proves that somebody else took
/// it in between. That is what makes "remove the entry the probe saw"
/// expressible: the probe and the removal must be about the SAME entry, and
/// an inode is the only thing that says so.
#[cfg(unix)]
pub fn socket_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    // `symlink_metadata`: the identity of the DIRECTORY ENTRY, which is the
    // thing `remove_file` unlinks. Following a symlink would describe a
    // different file than the one that would be removed.
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
pub fn socket_identity(path: &Path) -> Option<(u64, u64)> {
    // No inode identity available: the guard degrades to "the entry is still
    // there", which is the behaviour this platform had before. The fleet runs
    // Linux, where the identity check is exact.
    let _ = path;
    None
}

/// What the stale-socket cleanup decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaleSocketOutcome {
    /// The daemon answered the handshake: the socket is live and stays.
    Ready,
    /// Something is bound to the socket but did not answer in time. The
    /// socket is NOT removed: an unresponsive daemon may still be serving.
    PossiblyAlive { reason: String },
    /// Nothing was listening and the stale entry was removed.
    Removed,
    /// Nothing was listening, but the entry was NOT removed: it changed
    /// under us (a daemon bound it in the meantime) or the daemon lock could
    /// not be taken. Either way nothing live was unlinked.
    NotRemoved,
}

/// The stale-socket cleanup, exactly as the product runs it.
///
/// The probe and the removal are ONE decision about ONE directory entry:
///
/// * the entry's identity is captured BEFORE the probe, so the removal is
///   about the entry the probe actually examined;
/// * the daemon lock is held across the re-check and the removal, so two
///   zoder processes cannot both decide to clean up the same path, and a
///   client that is registering itself is not half-way through a lease
///   write while this runs;
/// * the identity is re-checked immediately before the removal, so a daemon
///   that bound the path after the probe is never unlinked.
///
/// The function is the production entry point, so a test drives the same
/// code the product runs rather than a copy of its rules.
pub async fn cleanup_stale_socket_if_dead(socket: &Path) -> StaleSocketOutcome {
    let identity = socket_identity(socket);
    match probe_ready(socket, Duration::from_secs(2)).await {
        DaemonReadiness::Ready => StaleSocketOutcome::Ready,
        DaemonReadiness::PossiblyAlive { reason } => StaleSocketOutcome::PossiblyAlive { reason },
        DaemonReadiness::TrulyStale { reason } => {
            eprintln!(
                "zoder: daemon socket {} is stale ({reason}); removing before respawn",
                socket.display()
            );
            if remove_stale_socket(socket, identity) {
                StaleSocketOutcome::Removed
            } else {
                StaleSocketOutcome::NotRemoved
            }
        }
    }
}

/// Remove the daemon socket at `path`, but ONLY when it is still the entry
/// the stale probe saw.
///
/// `expected` is the identity captured before the probe. When the entry at
/// `path` no longer has that identity, somebody else owns the path now (a
/// daemon has just bound it) and NOTHING is removed: unlinking a live
/// daemon's socket severs every future `connect(2)` from it.
///
/// The daemon lock is held for the whole check-and-remove, so this cannot
/// interleave with another process's cleanup or with a client registering
/// itself. When the lock cannot be taken, nothing is removed either: a
/// stale socket left behind costs a respawn, a live daemon's socket removed
/// costs the daemon.
pub fn remove_stale_socket(socket: &Path, expected: Option<(u64, u64)>) -> bool {
    let Ok(_lock) = DaemonLock::exclusive(socket) else {
        eprintln!(
            "zoder: could not take the daemon lock for {}; leaving the socket in place",
            socket.display()
        );
        return false;
    };
    if socket_identity(socket) != expected {
        // The path changed under us: it is either gone already, or a daemon
        // has bound it and must not be unlinked.
        return false;
    }
    std::fs::remove_file(socket).is_ok()
}
