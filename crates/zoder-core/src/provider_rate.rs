//! Host-wide, per-endpoint-group call budgets for provider requests.
//!
//! A free shared endpoint is one scarce resource. Several zoder processes
//! on one host — and several `[[providers]]` entries pointing at the same
//! hosted endpoint — each burst their own copy of the requests and push the
//! endpoint into 503/429 territory, which the clients then retry into
//! harder. This module enforces two host-wide budgets per
//! [`crate::config::Provider::rate_limit_group`]:
//!
//! * **Concurrency** — at most `max_concurrent` in-flight requests at once.
//!   Implemented as `flock`-ed slot files under
//!   `<zoder home>/state/slots/<group>/`, so it works across processes and a
//!   crashed process cannot leak a slot (the kernel releases the lock on
//!   process exit). This extends the reviewer-only
//!   `ZODER_PROVIDER_CONCURRENCY` slot files (same mechanism, same
//!   directory) to every call path and to shared groups.
//! * **Request rate** — at most `max_requests_per_minute` requests started
//!   within any sliding 60-second window. Implemented as one `flock`-ed
//!   JSON file of start timestamps under `<zoder home>/state/rate/<group>/`,
//!   so the window is shared across processes too.
//!
//! Both are PER HOST. A cap across several hosts is not possible without a
//! shared coordinator: divide the account's limit by the number of hosts.
//!
//! A waiting call SLEEPS for its slot / turn (bounded by its queue timeout,
//! `ZODER_QUEUE_TIMEOUT_S`, default 900 s — the same bound the reviewer
//! slots use) and prints one stderr notice after ~10 s, instead of failing.
//! `0` or absent limits mean unlimited: the acquisition is a no-op that
//! returns immediately, so a config without these keys behaves exactly as
//! before.

use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt;

const POLL: Duration = Duration::from_millis(200);
const NOTICE_AFTER: Duration = Duration::from_secs(10);
const WINDOW: Duration = Duration::from_secs(60);

/// Queue timeout from `ZODER_QUEUE_TIMEOUT_S` (default 900 s) — the same
/// variable and default the reviewer slot queue uses, so one operator knob
/// bounds every wait in this file.
pub fn queue_timeout_from_env() -> Duration {
    Duration::from_secs(
        std::env::var("ZODER_QUEUE_TIMEOUT_S")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(900),
    )
}

/// A held concurrency slot; released (unlocked) on drop.
#[derive(Debug)]
pub struct SlotGuard {
    _file: File,
}

/// The outcome of waiting for a group's budgets: a held concurrency slot
/// (`None` when unlimited), the time spent waiting, and the resolved limits.
#[derive(Debug)]
pub struct Acquired {
    guard: Option<SlotGuard>,
    /// Time spent waiting for a concurrency slot or a rate window.
    pub waited: Duration,
    /// Resolved concurrency limit (0 = unlimited).
    pub limit: usize,
    /// Resolved request-rate limit (0 = unlimited).
    pub rate_limit: u32,
}

impl Acquired {
    /// True when a concurrency slot is held (the group has a cap).
    pub fn is_held(&self) -> bool {
        self.guard.is_some()
    }
}

/// Sanitise a group name into a safe directory component.
pub fn safe_group(group: &str) -> String {
    group
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn slots_dir(home: &Path, group: &str) -> PathBuf {
    home.join("state").join("slots").join(safe_group(group))
}

fn rate_file(home: &Path, group: &str) -> PathBuf {
    home.join("state")
        .join("rate")
        .join(format!("{}.json", safe_group(group)))
}

fn try_take(dir: &Path, limit: usize) -> std::io::Result<Option<File>> {
    for i in 0..limit {
        let path = dir.join(format!("{i}.lock"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(Some(file)),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// Wait for a free concurrency slot for `group` under `home`. `limit == 0`
/// is a no-op. Prints one stderr notice after ~10 s of waiting (unless
/// `quiet`) and fails with an actionable error after `queue_timeout`.
pub async fn wait_for_slot(
    home: &Path,
    group: &str,
    limit: usize,
    queue_timeout: Duration,
    quiet: bool,
) -> anyhow::Result<Option<SlotGuard>> {
    if limit == 0 {
        return Ok(None);
    }
    let dir = slots_dir(home, group);
    std::fs::create_dir_all(&dir)?;
    let started = Instant::now();
    let mut noticed = false;
    loop {
        if let Some(file) = try_take(&dir, limit)? {
            return Ok(Some(SlotGuard { _file: file }));
        }
        let waited = started.elapsed();
        if waited >= queue_timeout {
            anyhow::bail!(
                "queued {}s waiting for one of {limit} local slot(s) for endpoint group {group}: \
                 other zoder processes on this host hold them all \
                 (provider max_concurrent). Retry later, raise the limit, or raise \
                 ZODER_QUEUE_TIMEOUT_S",
                waited.as_secs()
            );
        }
        if !quiet && !noticed && waited >= NOTICE_AFTER {
            eprintln!(
                "[zoder] waiting {}s for a local {group} slot ({limit} in use by other zoder \
                 processes; provider max_concurrent)",
                waited.as_secs()
            );
            noticed = true;
        }
        tokio::time::sleep(POLL.min(queue_timeout.saturating_sub(waited))).await;
    }
}

/// Read + prune + append the sliding-window rate ledger under an exclusive
/// flock. Returns `true` when a request may start NOW. `now_ms` is injectable
/// for tests.
fn rate_admit(path: &Path, limit: u32, now_ms: u128) -> std::io::Result<bool> {
    // The ledger: one exclusive flock, read + prune + (maybe) append +
    // rewrite, all under the lock, so concurrent processes (separate
    // fds) always see a consistent sliding window.
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)?;
    file.lock_exclusive()?;
    let mut raw = String::new();
    std::io::Read::read_to_string(&mut file, &mut raw).unwrap_or_default();
    let mut stamps: BTreeSet<u128> = serde_json::from_str(&raw).unwrap_or_default();
    let cutoff = now_ms.saturating_sub(WINDOW.as_millis());
    // A stamp is fresh while it is strictly inside the window; exactly one
    // window old it has just expired.
    stamps.retain(|ts| *ts > cutoff);
    let admit = (stamps.len() as u32) < limit;
    if admit {
        stamps.insert(now_ms);
    }
    use std::io::{Seek, Write};
    file.set_len(0)?;
    file.seek(std::io::SeekFrom::Start(0))?;
    file.write_all(serde_json::to_string(&stamps).unwrap().as_bytes())?;
    file.flush()?;
    file.unlock()?;
    Ok(admit)
}

/// Sleep until the sliding-window rate ledger admits this request, then
/// record it. `rate_limit == 0` is a no-op. `now_ms` defaults to the real
/// clock (tests inject their own via [`rate_admit`]).
pub async fn wait_for_rate_turn(
    home: &Path,
    group: &str,
    rate_limit: u32,
    queue_timeout: Duration,
    quiet: bool,
) -> anyhow::Result<()> {
    if rate_limit == 0 {
        return Ok(());
    }
    let path = rate_file(home, group);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let started = Instant::now();
    let mut noticed = false;
    loop {
        let path = path.clone();
        let limit = rate_limit;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let admitted = tokio::task::spawn_blocking(move || rate_admit(&path, limit, now_ms))
            .await
            .unwrap_or(Ok(false))?;
        if admitted {
            return Ok(());
        }
        let waited = started.elapsed();
        if waited >= queue_timeout {
            anyhow::bail!(
                "queued {}s waiting for a request slot in the {rate_limit}/min budget of endpoint \
                 group {group}: other zoder processes on this host are using the budget \
                 (provider max_requests_per_minute). Retry later, raise the limit, or raise \
                 ZODER_QUEUE_TIMEOUT_S",
                waited.as_secs()
            );
        }
        if !quiet && !noticed && waited >= NOTICE_AFTER {
            eprintln!(
                "[zoder] waiting {}s for a request slot in the {rate_limit}/min budget of \
                 endpoint group {group} (provider max_requests_per_minute)",
                waited.as_secs()
            );
            noticed = true;
        }
        tokio::time::sleep(POLL.min(queue_timeout.saturating_sub(waited))).await;
    }
}

/// Wait for BOTH of a group's budgets before starting a request: first the
/// concurrency slot, then the rate turn (a rate slot held while no request
/// is in flight would waste the budget). Drop the returned [`Acquired`] when
/// the request finishes to release the slot; the rate stamp stays in the
/// sliding window either way.
pub async fn acquire(
    home: &Path,
    group: &str,
    max_concurrent: usize,
    max_requests_per_minute: u32,
    queue_timeout: Duration,
    quiet: bool,
) -> anyhow::Result<Acquired> {
    let slot_started = Instant::now();
    let guard = wait_for_slot(home, group, max_concurrent, queue_timeout, quiet).await?;
    let slot_waited = slot_started.elapsed();
    let rate_started = Instant::now();
    wait_for_rate_turn(home, group, max_requests_per_minute, queue_timeout, quiet).await?;
    Ok(Acquired {
        guard,
        waited: slot_waited + rate_started.elapsed(),
        limit: max_concurrent,
        rate_limit: max_requests_per_minute,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Admission logic of the sliding window, with an injected clock.
    #[test]
    fn sliding_window_admits_up_to_limit_then_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.json");
        let now = 1_000_000u128;
        assert!(rate_admit(&path, 3, now).unwrap());
        assert!(rate_admit(&path, 3, now + 1_000).unwrap());
        assert!(rate_admit(&path, 3, now + 2_000).unwrap());
        // Fourth within the window: blocked, and NOT recorded.
        assert!(!rate_admit(&path, 3, now + 3_000).unwrap());
        // ...until the window slides past the first stamp (60 s after it).
        assert!(rate_admit(&path, 3, now + 60_500).unwrap());
    }

    /// A rejected admission must not consume budget.
    #[test]
    fn rejected_admission_does_not_consume_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g.json");
        let now = 2_000_000u128;
        assert!(rate_admit(&path, 1, now).unwrap());
        assert!(!rate_admit(&path, 1, now + 500).unwrap());
        // Budget still has exactly one stamp: after the window slides, the
        // next call admits without waiting for a second empty slot.
        assert!(rate_admit(&path, 1, now + 60_500).unwrap());
    }

    /// Slot files are shared across independent lock users (no in-process
    /// state): two users contend for one slot; a dropped user frees it.
    #[tokio::test]
    async fn slot_freed_when_holder_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let group_dir = dir.path().join("g");
        std::fs::create_dir_all(&group_dir).unwrap();
        let guard = wait_for_slot(dir.path(), "g", 1, Duration::from_secs(5), true)
            .await
            .unwrap();
        assert!(guard.is_some());
        // A second user cannot take the one slot: it times out.
        let err = wait_for_slot(dir.path(), "g", 1, Duration::from_millis(500), true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("queued"), "{}", err);
        // Drop the holder: the slot is released (flock on close/drop).
        drop(guard);
        let again = wait_for_slot(dir.path(), "g", 1, Duration::from_secs(5), true)
            .await
            .unwrap();
        assert!(again.is_some());
    }

    /// Two INDEPENDENT in-process users (no shared state) cap at 1: while
    /// one holds the slot, the other times out; after the holder drops, the
    /// slot is free again. Exercises the same flock files a second process
    /// would use.
    #[tokio::test]
    async fn independent_users_contend_for_one_slot() {
        let dir = tempfile::tempdir().unwrap();
        let a = wait_for_slot(dir.path(), "g", 1, Duration::from_secs(2), true)
            .await
            .unwrap();
        assert!(a.is_some());
        // A second, independent waiter sees the slot held and times out.
        let err = wait_for_slot(dir.path(), "g", 1, Duration::from_millis(400), true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("slot"), "{}", err);
        drop(a);
        let b = wait_for_slot(dir.path(), "g", 1, Duration::from_secs(2), true)
            .await
            .unwrap();
        assert!(b.is_some());
        drop(b);
    }

    /// A real second PROCESS holding the slot blocks this process's waiter;
    /// when the child dies (kill), the flock is released by the kernel and
    /// the waiter proceeds — a crashed process cannot leak a slot.
    #[tokio::test]
    async fn second_process_holder_blocks_then_killed_holder_frees_slot() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let slots = home.join("state").join("slots").join("g");
        std::fs::create_dir_all(&slots).unwrap();
        let slot0 = slots.join("0.lock");
        // A real second PROCESS holds the slot file's flock: while it lives,
        // this process's slot waiter must time out.
        // A real second PROCESS holds the slot file's flock: while it lives,
        // this process's slot waiter must time out. The child script writes a
        // temp file (a separate, safe path under /tmp) so this test can print
        // its own diagnostics if the platform's flock behaves unexpectedly.
        // The child takes the flock before its 60 s sleep; write its stderr
        // to a temp file so this test can print diagnostics if the platform
        // flock behaves unexpectedly.
        let dbg_path = dir.path().join("child.stderr");
        let slot0_s = slot0.to_string_lossy().into_owned();
        let py = "import fcntl,os,time,sys\n".to_string()
            + &format!("fd=os.open('{slot0_s}',os.O_CREAT|os.O_RDWR)\n")
            + "fcntl.flock(fd,fcntl.LOCK_EX)\n"
            + "sys.stderr.write('HELD ' + str(os.getpid()) + '\\n')\n"
            + "sys.stderr.flush()\n"
            + "time.sleep(60)\n";
        let dbg_file = std::fs::File::create(&dbg_path).unwrap();
        let mut child = match std::process::Command::new("python3")
            .args(["-c", &py])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(dbg_file))
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("skipping cross-process slot test (python3 unavailable: {e})");
                return;
            }
        };
        // The child takes the flock before its 60 s sleep; wait for the
        // lock to actually be held (non-blocking probe) instead of guessing.
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(100));
            let probe = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&slot0)
            {
                Ok(f) => f,
                Err(_) => continue,
            };
            use fs2::FileExt;
            match probe.try_lock_exclusive() {
                // Child holds it: stop waiting.
                Err(_) => break,
                // We grabbed it: release and keep waiting for the child.
                Ok(()) => {
                    let _ = probe.unlock();
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        // While the child holds it, a slot waiter must time out (it sees a
        // held lock owned by ANOTHER process).
        let err = match wait_for_slot(home, "g", 1, Duration::from_millis(700), true).await {
            Err(e) => e,
            Ok(guard) => {
                let child_err = std::fs::read_to_string(&dbg_path).unwrap_or_default();
                panic!(
                    "cross-process slot must be held by the child process; got a slot                      (fd sharing / platform flock anomaly? guard={guard:?} child stderr: {child_err:?})"
                );
            }
        };
        assert!(err.to_string().contains("slot"), "{}", err);
        // Kill the holder: the kernel releases the flock on process death.
        let _ = child.kill();
        let _ = child.wait();
        let guard = wait_for_slot(home, "g", 1, Duration::from_secs(5), true)
            .await
            .unwrap();
        assert!(guard.is_some());
    }

    /// Cross-process sliding-window rate: a child process stamps N turns in
    /// the shared window file; this process's waiter is admitted only after
    /// the window slides (here: the child's stamps age out via the injected
    /// clock path is not cross-process, so we assert the admission count
    /// from the file instead: N stamps recorded, and a (N+1)th stamp by
    /// THIS process is refused until the window moves).
    #[test]
    fn second_process_rate_window_is_shared_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("rate").join("g.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // "Process 1": admit 3 turns at t=10s.
        assert!(rate_admit(&path, 3, 10_000).unwrap());
        assert!(rate_admit(&path, 3, 10_100).unwrap());
        assert!(rate_admit(&path, 3, 10_200).unwrap());
        // "Process 2" reads the SAME file at t=10.5s: the window is full.
        assert!(!rate_admit(&path, 3, 10_500).unwrap());
        // ...and after the window slides past the first stamps, it admits.
        assert!(rate_admit(&path, 3, 70_500).unwrap());
    }

    /// Two providers in ONE rate_limit_group share the on-disk budget;
    /// different groups do not.
    #[test]
    fn rate_budget_is_per_group_not_per_provider() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("state").join("rate");
        let shared_a = base.join("shared.json");
        let shared_b = base.join("shared.json"); // same group name
        let other = base.join("other.json");
        std::fs::create_dir_all(&base).unwrap();
        assert!(rate_admit(&shared_a, 2, 20_000).unwrap());
        assert!(rate_admit(&shared_a, 2, 20_100).unwrap());
        assert!(
            !rate_admit(&shared_b, 2, 20_500).unwrap(),
            "shared group budget exhausted"
        );
        assert!(
            rate_admit(&other, 2, 20_500).unwrap(),
            "a different group must have its own budget"
        );
    }

    /// Zero limit is a no-op.
    #[tokio::test]
    async fn zero_limits_are_noops() {
        let dir = tempfile::tempdir().unwrap();
        let a = acquire(dir.path(), "g", 0, 0, Duration::from_secs(1), true)
            .await
            .unwrap();
        assert!(!a.is_held());
        assert_eq!(a.limit, 0);
        assert_eq!(a.rate_limit, 0);
        assert!(a.waited < Duration::from_millis(50));
    }
}
