//! Host-wide, per-provider concurrency slots for reviewer calls.
//!
//! Every `zoder review` chunk goes straight to its provider endpoint. With
//! many zoder sessions on one host (ten agent sessions sharing HYDRA), a
//! single-GPU server such as the CERBERUS gemma4-31b route received every
//! session's chunk at once and queued them server-side, while each client
//! waited silently for its whole request budget. This module lets an operator
//! cap concurrent reviewer calls per provider on a host and makes the wait
//! visible: a counting semaphore made of `flock`-ed slot files under
//! `<zoder home>/state/slots/<provider>/`, so it works across processes.
//!
//! Configuration is environment-only (so an older binary reading the same
//! shared config never trips over an unknown key):
//!
//! - `ZODER_PROVIDER_CONCURRENCY` -- `provider=N` pairs, comma separated,
//!   with `*=N` as the default for unlisted providers. `0` (or unset) means
//!   unlimited, which is the default behaviour.
//! - `ZODER_QUEUE_TIMEOUT_S` -- how long to wait for a slot before failing
//!   with a clear error (default 900).

use fs2::FileExt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEFAULT_QUEUE_TIMEOUT_S: u64 = 900;
const POLL: Duration = Duration::from_millis(200);
const NOTICE_EVERY: Duration = Duration::from_secs(30);

/// Parse `ZODER_PROVIDER_CONCURRENCY` and return the limit for `provider`.
/// Malformed entries are ignored (a typo must not block reviews).
pub(crate) fn limit_for(spec: Option<&str>, provider: &str) -> usize {
    let Some(spec) = spec else { return 0 };
    let mut default = 0usize;
    for part in spec.split(',') {
        let Some((name, n)) = part.trim().split_once('=') else {
            continue;
        };
        let Ok(n) = n.trim().parse::<usize>() else {
            continue;
        };
        match name.trim() {
            "*" => default = n,
            name if name == provider => return n,
            _ => {}
        }
    }
    default
}

/// A held slot; released (unlocked) on drop.
#[derive(Debug)]
pub(crate) struct SlotGuard {
    _file: File,
}

/// Outcome of acquiring a slot.
#[derive(Debug)]
pub(crate) struct Acquired {
    /// `None` when the provider is unlimited.
    guard: Option<SlotGuard>,
    /// Time spent waiting for a free slot.
    pub waited: Duration,
    /// Configured limit (0 = unlimited).
    pub limit: usize,
}

impl Acquired {
    /// True when a slot is held (the provider has a limit).
    pub(crate) fn is_held(&self) -> bool {
        self.guard.is_some()
    }
}

fn slot_dir(home: &Path, provider: &str) -> PathBuf {
    let safe: String = provider
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    home.join("state").join("slots").join(safe)
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

/// Wait for one of `limit` slots for `provider`. Prints a notice every 30s
/// while waiting (unless `quiet`) and fails with an actionable error after
/// `queue_timeout`.
pub(crate) async fn acquire(
    home: &Path,
    provider: &str,
    limit: usize,
    queue_timeout: Duration,
    quiet: bool,
) -> anyhow::Result<Acquired> {
    if limit == 0 {
        return Ok(Acquired {
            guard: None,
            waited: Duration::ZERO,
            limit,
        });
    }
    let dir = slot_dir(home, provider);
    std::fs::create_dir_all(&dir)?;
    let started = Instant::now();
    let mut next_notice = NOTICE_EVERY;
    loop {
        if let Some(file) = try_take(&dir, limit)? {
            return Ok(Acquired {
                guard: Some(SlotGuard { _file: file }),
                waited: started.elapsed(),
                limit,
            });
        }
        let waited = started.elapsed();
        if waited >= queue_timeout {
            anyhow::bail!(
                "queued {}s waiting for one of {limit} local slot(s) for provider {provider}: \
                 other zoder processes on this host hold them all \
                 (ZODER_PROVIDER_CONCURRENCY). Retry later, raise the limit, or raise \
                 ZODER_QUEUE_TIMEOUT_S",
                waited.as_secs()
            );
        }
        if !quiet && waited >= next_notice {
            eprintln!(
                "[zoder] waiting {}s for a local {provider} slot ({limit} in use by other zoder \
                 processes; ZODER_PROVIDER_CONCURRENCY)",
                waited.as_secs()
            );
            next_notice += NOTICE_EVERY;
        }
        tokio::time::sleep(POLL.min(queue_timeout.saturating_sub(waited))).await;
    }
}

/// Queue timeout from `ZODER_QUEUE_TIMEOUT_S` (default 900s).
pub(crate) fn queue_timeout_from_env() -> Duration {
    Duration::from_secs(
        std::env::var("ZODER_QUEUE_TIMEOUT_S")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_QUEUE_TIMEOUT_S),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_spec_parses_pairs_default_and_garbage() {
        assert_eq!(limit_for(None, "p"), 0);
        assert_eq!(limit_for(Some(""), "p"), 0);
        let spec = Some("cerberus-reviewer=2, *=4 ,bad, x=y, nvidia-eih=0");
        assert_eq!(limit_for(spec, "cerberus-reviewer"), 2);
        assert_eq!(limit_for(spec, "nvidia-eih"), 0);
        assert_eq!(limit_for(spec, "other"), 4);
        assert_eq!(limit_for(Some("a=1"), "b"), 0);
    }

    #[tokio::test]
    async fn unlimited_never_waits() {
        let dir = tempfile::tempdir().unwrap();
        let got = acquire(dir.path(), "p", 0, Duration::from_millis(10), true)
            .await
            .unwrap();
        assert!(!got.is_held());
        assert_eq!(got.waited, Duration::ZERO);
    }

    /// The second caller waits for the first to release, and the wait is
    /// measured; with every slot held past the queue timeout the error says
    /// why instead of hanging silently.
    #[tokio::test]
    async fn slots_serialize_measure_wait_and_time_out_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let first = acquire(&home, "cerberus-reviewer", 1, Duration::from_secs(5), true)
            .await
            .unwrap();
        assert!(first.is_held());

        let err = acquire(
            &home,
            "cerberus-reviewer",
            1,
            Duration::from_millis(300),
            true,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("local slot(s) for provider cerberus-reviewer"),
            "{err}"
        );
        assert!(err.contains("ZODER_QUEUE_TIMEOUT_S"), "{err}");

        let home2 = home.clone();
        let waiter = tokio::spawn(async move {
            acquire(&home2, "cerberus-reviewer", 1, Duration::from_secs(5), true)
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(400)).await;
        drop(first);
        let second = waiter.await.unwrap();
        assert!(second.is_held());
        assert!(
            second.waited >= Duration::from_millis(300),
            "{:?}",
            second.waited
        );
        // A different provider has its own slots.
        let other = acquire(&home, "nvidia-eih", 1, Duration::from_millis(50), true)
            .await
            .unwrap();
        assert!(other.is_held());
    }
}
