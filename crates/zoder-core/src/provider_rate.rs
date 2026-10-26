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

use std::collections::BTreeMap;
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
    home.join("state")
        .join("slots")
        .join(safe_group(group))
}

fn rate_file(home: &Path, group: &str) -> PathBuf {
    home
        .join("state")
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
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.lock_exclusive()?;
    let raw = std::io::read_to_string(&file).unwrap_or_default();
    let mut stamps: BTreeMap<u128, ()> = serde_json::from_str(&raw).unwrap_or_default();
    let cutoff = now_ms.saturating_sub(WINDOW.as_millis());
    stamps.retain(|ts, _| *ts >= cutoff);
    let admit = (stamps.len() as u32) < limit;
    if admit {
        stamps.insert(now_ms, ());
    }
    drop(file);
    if admit {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)?;
        file.lock_exclusive()?;
        use std::io::Write;
        file.set_len(0)?;
        file.write_all(serde_json::to_string(&stamps).unwrap().as_bytes())?;
    }
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
    let guard =
        wait_for_slot(home, group, max_concurrent, queue_timeout, quiet).await?;
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
        // ...until the window slides past the first stamp.
        assert!(rate_admit(&path, 3, now + 61_000).unwrap());
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
        assert!(rate_admit(&path, 1, now + 61_000).unwrap());
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

    /// Zero limit is a no-op.
    #[tokio::test]
    async fn zero_limits_are_noops() {
        let dir = tempfile::tempdir().unwrap();
        let a = acquire(
            dir.path(),
            "g",
            0,
            0,
            Duration::from_secs(1),
            true,
        )
        .await
        .unwrap();
        assert!(!a.is_held());
        assert_eq!(a.limit, 0);
        assert_eq!(a.rate_limit, 0);
        assert!(a.waited.is_zero());
    }
}
