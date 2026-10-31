//! Release-update check against the zoder GitHub releases.
//!
//! Two surfaces:
//!   * `zoder update --check` reports whether a newer release exists;
//!     `zoder update` re-runs the official installer to self-replace the binary
//!     (reusing its platform detection + SHA256 verification + atomic install).
//!   * A throttled daily check (cached under `$ZODER_HOME`) lets the CLI print a
//!     one-line "a new release is available" hint on normal startup, without an
//!     explicit command. Opt out with `ZODER_NO_UPDATE_CHECK=1`.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// `owner/repo` the binary updates from; override with `$ZODER_REPO`.
pub fn repo() -> String {
    std::env::var("ZODER_REPO").unwrap_or_else(|_| "ncz-os/zoder".to_string())
}

/// The version compiled into this binary (workspace version).
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The one-liner that installs/updates to the latest release.
pub fn install_command() -> String {
    format!(
        "curl -fsSL https://raw.githubusercontent.com/{}/master/install.sh | sh",
        repo()
    )
}

/// URL of the latest release's notes page.
pub fn release_url() -> String {
    format!("https://github.com/{}/releases/latest", repo())
}

#[derive(Debug, Clone)]
pub struct UpdateStatus {
    pub current: String,
    pub latest: String,
    /// True when `latest` is strictly newer than `current`.
    pub newer: bool,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    checked_at: u64,
    latest: String,
}

/// On-disk shape written when a check FAILED (offline, HTTP error, timeout).
/// `failed_at` is the wall-clock second of the newest failed attempt; any run
/// that sees it inside the failure retry interval stays offline.
#[derive(Serialize, Deserialize)]
struct FailureFile {
    failed_at: u64,
}

/// Whether a failed check is still inside its retry interval.
fn failure_is_fresh(home: &Path, now: u64, retry: Duration) -> bool {
    let Ok(txt) = std::fs::read_to_string(home.join(FAILURE_FILE)) else {
        return false;
    };
    match serde_json::from_str::<FailureFile>(&txt) {
        Ok(ff) => now.saturating_sub(ff.failed_at) < retry.as_secs(),
        // An unreadable negative cache is treated as stale: one retry is
        // cheaper than honouring a corrupt marker forever.
        Err(_) => false,
    }
}

fn record_failure(home: &Path, now: u64) {
    let _ = std::fs::create_dir_all(home);
    if let Ok(json) = serde_json::to_string(&FailureFile { failed_at: now }) {
        let _ = std::fs::write(home.join(FAILURE_FILE), json);
    }
}

/// File name of the negative (failure) cache, next to `.update_check.json`.
const FAILURE_FILE: &str = ".update_check_failed.json";

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
}

/// Default bound on how long a caller waits on a background update check
/// before giving up on the hint: short commands must never pay the update
/// check's network latency, so the CLI is willing to lose the hint.
pub const BACKGROUND_BUDGET: Duration = Duration::from_millis(200);

/// Default delay before a FAILED check is retried. Without this, an offline or
/// air-gapped host re-attempted the 8 s network check on every single command
/// (including every fleet subprocess): thousands of doomed requests a day.
/// Override with `$ZODER_UPDATE_CHECK_RETRY_S`.
pub const DEFAULT_FAILURE_RETRY: Duration = Duration::from_secs(900);

/// Environment variable that overrides the failed-check retry interval.
pub const FAILURE_RETRY_ENV: &str = "ZODER_UPDATE_CHECK_RETRY_S";

/// The configurable interval before a failed check is retried.
pub fn failure_retry_interval() -> Duration {
    std::env::var(FAILURE_RETRY_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_FAILURE_RETRY)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("zoder/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(8))
        .build()
        .unwrap_or_default()
}

/// Fetch the latest release tag, leading `v` stripped (e.g. `0.2.2`).
pub async fn latest_release() -> anyhow::Result<String> {
    latest_release_from(&releases_url()).await
}

/// The release-notes URL the check reads, from `$ZODER_REPO`.
fn releases_url() -> String {
    format!("https://api.github.com/repos/{}/releases/latest", repo())
}

/// [`latest_release`] against an explicit URL, so a test can point the check at
/// a local (unreachable or fake) endpoint instead of the real API.
async fn latest_release_from(url: &str) -> anyhow::Result<String> {
    let resp = client().get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("GitHub API: HTTP {}", resp.status());
    }
    let rel: GhRelease = resp.json().await?;
    Ok(rel.tag_name.trim_start_matches('v').to_string())
}

/// What a single check attempt should read: live GitHub, or a test override.
#[derive(Debug, Clone)]
pub struct Endpoint {
    url: String,
}

impl Endpoint {
    /// The live releases endpoint (`$ZODER_REPO` aware).
    pub fn live() -> Self {
        Self {
            url: releases_url(),
        }
    }

    /// An explicit endpoint (tests: a closed local port).
    pub fn url(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }
}

impl Default for Endpoint {
    fn default() -> Self {
        Self::live()
    }
}

/// Semantic-ish compare: true when `latest` is strictly newer than `current`.
/// Parses leading dotted integers; a non-numeric/pre-release tail parses to 0, so
/// a parse miss is treated conservatively as "not newer" (never nags wrongly).
pub fn is_newer(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        v.split(['.', '-', '+'])
            .map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<u64>()
                    .unwrap_or(0)
            })
            .collect()
    }
    let (l, c) = (parts(latest), parts(current));
    for i in 0..l.len().max(c.len()) {
        let lv = l.get(i).copied().unwrap_or(0);
        let cv = c.get(i).copied().unwrap_or(0);
        if lv != cv {
            return lv > cv;
        }
    }
    false
}

/// Live check (one network call). Compares current vs latest.
pub async fn check() -> anyhow::Result<UpdateStatus> {
    let latest = latest_release().await?;
    let cur = current().to_string();
    let newer = is_newer(&latest, &cur);
    Ok(UpdateStatus {
        current: cur,
        latest,
        newer,
    })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Throttled, best-effort check for the startup hint. Reads/writes a cache under
/// `home`; only hits the network when the cache is older than `ttl`. Any failure
/// (offline, parse error, opt-out) yields `None` — it must never break a run.
///
/// A FAILED check is remembered for [`failure_retry_interval`], so an offline
/// or air-gapped host does not re-attempt the network on every command.
pub async fn check_cached(home: &Path, ttl: Duration) -> Option<UpdateStatus> {
    check_cached_at(&Endpoint::live(), home, ttl, failure_retry_interval()).await
}

/// [`check_cached`] with an explicit failed-check retry interval (tests pin it).
///
/// This is the single network attempt behind the startup hint. Callers that
/// print the hint must run it concurrently with a short budget
/// ([`BACKGROUND_BUDGET`]) so a slow network can never delay a command; this
/// function itself keeps the original 8 s request timeout so a check that runs
/// to completion (and lands in the cache) still gets to finish.
pub async fn check_cached_with(
    home: &Path,
    ttl: Duration,
    failure_retry: Duration,
) -> Option<UpdateStatus> {
    check_cached_at(&Endpoint::live(), home, ttl, failure_retry).await
}

/// [`check_cached_with`] against an explicit [`Endpoint`] (tests use a local
/// closed port as the "unreachable network").
pub async fn check_cached_at(
    endpoint: &Endpoint,
    home: &Path,
    ttl: Duration,
    failure_retry: Duration,
) -> Option<UpdateStatus> {
    if update_check_disabled() {
        return None;
    }
    let path = home.join(".update_check.json");
    let now = now_secs();
    // Fast path: fresh cache, no network.
    if let Ok(txt) = std::fs::read_to_string(&path) {
        if let Ok(cf) = serde_json::from_str::<CacheFile>(&txt) {
            if now.saturating_sub(cf.checked_at) < ttl.as_secs() {
                let cur = current().to_string();
                let newer = is_newer(&cf.latest, &cur);
                return Some(UpdateStatus {
                    current: cur,
                    latest: cf.latest,
                    newer,
                });
            }
        }
    }
    // Negative cache: a recent failure suppresses the network entirely.
    if failure_is_fresh(home, now, failure_retry) {
        return None;
    }
    // Stale/absent: one network check, then cache the outcome either way.
    let latest = match latest_release_from(&endpoint.url).await {
        Ok(latest) => latest,
        Err(_) => {
            record_failure(home, now);
            return None;
        }
    };
    let _ = std::fs::create_dir_all(home);
    if let Ok(json) = serde_json::to_string(&CacheFile {
        checked_at: now,
        latest: latest.clone(),
    }) {
        let _ = std::fs::write(&path, json);
    }
    // A success clears any stale failure marker.
    let _ = std::fs::remove_file(home.join(FAILURE_FILE));
    let cur = current().to_string();
    let newer = is_newer(&latest, &cur);
    Some(UpdateStatus {
        current: cur,
        latest,
        newer,
    })
}

/// Fresh-cache-only lookup: the startup hint's display path.
///
/// Returns the cached [`UpdateStatus`] only when the last SUCCESSFUL check is
/// younger than `ttl` (no network, no blocking). The cache read is such small
/// local IO that a missing or unreadable file just means "no hint". This is
/// what [`spawn_background`] returns, so a command that waits past the budget
/// still gets whatever was already known without touching the network.
pub fn cached_only(home: &Path, ttl: Duration) -> Option<UpdateStatus> {
    if update_check_disabled() {
        return None;
    }
    let txt = std::fs::read_to_string(home.join(".update_check.json")).ok()?;
    let cf = serde_json::from_str::<CacheFile>(&txt).ok()?;
    if now_secs().saturating_sub(cf.checked_at) >= ttl.as_secs() {
        return None;
    }
    let cur = current().to_string();
    let newer = is_newer(&cf.latest, &cur);
    Some(UpdateStatus {
        current: cur,
        latest: cf.latest,
        newer,
    })
}

/// Run the throttled check on its own blocking thread and wait at most
/// [`BACKGROUND_BUDGET`] for it.
///
/// Short commands (`zoder --help`, `zoder config`, every fleet subprocess)
/// must not wait on the update check's network latency at all, so this is
/// what the CLI's startup hint calls: the network check runs concurrently on
/// a background thread while the command's own work completes. If the check
/// finishes inside the budget its result is used; otherwise the caller gets
/// the fresh cache (if any) immediately and the background thread — which is
/// detached, not cancelled — finishes the check and records the outcome
/// (positive or negative) for the next invocation.
pub fn spawn_background(home: &Path, ttl: Duration) -> Option<UpdateStatus> {
    spawn_background_at(
        &Endpoint::live(),
        home,
        ttl,
        failure_retry_interval(),
        BACKGROUND_BUDGET,
    )
}

/// [`spawn_background`] with explicit retry interval and wait budget (tests
/// pin both, so no process-wide environment is mutated).
pub fn spawn_background_with(
    home: &Path,
    ttl: Duration,
    failure_retry: Duration,
    budget: Duration,
) -> Option<UpdateStatus> {
    spawn_background_at(&Endpoint::live(), home, ttl, failure_retry, budget)
}

/// [`spawn_background_with`] against an explicit [`Endpoint`].
pub fn spawn_background_at(
    endpoint: &Endpoint,
    home: &Path,
    ttl: Duration,
    failure_retry: Duration,
    budget: Duration,
) -> Option<UpdateStatus> {
    if update_check_disabled() {
        return None;
    }
    // Fast path: a fresh cache answers without any thread at all.
    if let Some(st) = cached_only(home, ttl) {
        return Some(st);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let thread_home = home.to_path_buf();
    let thread_endpoint = endpoint.clone();
    let spawned = std::thread::Builder::new()
        .name("zoder-update-check".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            if let Ok(runtime) = runtime {
                let st = runtime.block_on(check_cached_at(
                    &thread_endpoint,
                    &thread_home,
                    ttl,
                    failure_retry,
                ));
                // A send failure means the caller already gave up: the cache
                // write above is all that mattered.
                let _ = tx.send(st);
            }
        });
    match spawned {
        Ok(_handle) => {
            // Dropping the JoinHandle detaches the thread: when the budget
            // expires the check keeps running (bounded by its own 8 s request
            // timeout) and records its result for the next command.
            rx.recv_timeout(budget).ok().flatten()
        }
        // Could not spawn a thread: fall back to the fresh cache only. The
        // hint is never worth blocking a command for.
        Err(_) => cached_only(home, ttl),
    }
}

fn update_check_disabled() -> bool {
    std::env::var_os("ZODER_NO_UPDATE_CHECK").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An endpoint that refuses instantly: a closed loopback port. Every
    /// attempt fails fast the way an offline / air-gapped host does, without
    /// depending on DNS or a real network.
    fn unreachable_endpoint() -> Endpoint {
        // Bind then drop: the port was free a moment ago and nothing local
        // listens there, so `connect` gets ECONNREFUSED at once.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);
        Endpoint::url(format!("http://127.0.0.1:{port}/latest"))
    }

    fn temp_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zoder-update-{tag}-{}-{}",
            std::process::id(),
            now_secs()
        ));
        std::fs::create_dir_all(&dir).expect("temp home");
        dir
    }

    fn failure_marker(home: &std::path::Path) -> Option<FailureFile> {
        let txt = std::fs::read_to_string(home.join(FAILURE_FILE)).ok()?;
        serde_json::from_str(&txt).ok()
    }

    /// D5.3 regression: a failed check is remembered, so the next run inside
    /// the retry interval does not touch the network again.
    #[tokio::test]
    async fn failed_check_is_cached_and_not_retried_within_the_interval() {
        let home = temp_home("negative");
        let endpoint = unreachable_endpoint();
        let ttl = Duration::from_secs(86_400);
        let retry = Duration::from_secs(3600);

        // First run: the network is attempted and fails; the failure lands in
        // the negative cache.
        let first = check_cached_at(&endpoint, &home, ttl, retry).await;
        assert!(first.is_none(), "a failed check yields no status");
        let marker = failure_marker(&home).expect("a failed check must be recorded");
        assert!(
            now_secs().saturating_sub(marker.failed_at) < 5,
            "failure timestamp must be the attempt time"
        );

        // Second run inside the interval: the negative cache answers BEFORE
        // any request, so this returns with no network attempt at all. Point
        // at a listener that counts connections; it must stay at zero.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind counting port");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let port = listener.local_addr().expect("local addr").port();
        let counting = Endpoint::url(format!("http://127.0.0.1:{port}/latest"));
        let second = check_cached_at(&counting, &home, ttl, retry).await;
        assert!(second.is_none());
        assert!(
            listener.accept().is_err(),
            "a check inside the failure retry interval must not open a connection"
        );
    }

    /// D5.3 regression: the check must not make a short command wait on the
    /// network. Against an unreachable endpoint and a 200 ms budget the call
    /// returns (empty-handed) promptly, and the failure is recorded later by
    /// the detached thread so the next run stays offline.
    #[test]
    fn background_check_is_bounded_and_records_its_failure() {
        let home = temp_home("bounded");
        let endpoint = unreachable_endpoint();
        let ttl = Duration::from_secs(86_400);
        let budget = Duration::from_millis(200);

        let started = std::time::Instant::now();
        let st = spawn_background_at(&endpoint, &home, ttl, Duration::from_secs(3600), budget);
        let waited = started.elapsed();
        assert!(st.is_none(), "an unreachable endpoint yields no hint");
        assert!(
            waited <= Duration::from_millis(600),
            "the update check must not delay a command beyond its budget (waited {waited:?})"
        );

        // The detached thread finishes the attempt and leaves the negative
        // cache behind, so the next invocation does not retry.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while failure_marker(&home).is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            failure_marker(&home).is_some(),
            "the background check must record its failure for the next run"
        );
    }

    /// A fresh POSITIVE cache is returned by the display path with no network
    /// and no thread, and `spawn_background_at` short-circuits on it.
    #[test]
    fn fresh_cache_is_served_without_a_check() {
        let home = temp_home("fresh");
        let ttl = Duration::from_secs(86_400);
        let json = serde_json::to_string(&CacheFile {
            checked_at: now_secs(),
            latest: "99.0.0".to_string(),
        })
        .expect("serialize cache");
        std::fs::write(home.join(".update_check.json"), json).expect("write cache");

        let cached = cached_only(&home, ttl).expect("fresh cache answers");
        assert_eq!(cached.latest, "99.0.0");
        assert!(cached.newer);

        let st = spawn_background_at(
            &unreachable_endpoint(),
            &home,
            ttl,
            Duration::from_secs(3600),
            Duration::from_millis(200),
        )
        .expect("fresh cache answers without a network check");
        assert_eq!(st.latest, "99.0.0");
    }

    /// The retry interval is configurable through the documented env var.
    #[test]
    fn failure_retry_interval_is_configurable() {
        assert_eq!(FAILURE_RETRY_ENV, "ZODER_UPDATE_CHECK_RETRY_S");
        assert!(DEFAULT_FAILURE_RETRY.as_secs() > 0);
        assert!(BACKGROUND_BUDGET <= Duration::from_millis(500));
    }

    #[test]
    fn newer_detection() {
        assert!(is_newer("0.2.2", "0.2.1"));
        assert!(is_newer("0.3.0", "0.2.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.2.1", "0.2.1"));
        assert!(!is_newer("0.2.0", "0.2.1"));
        assert!(!is_newer("0.2.1", "0.2.2"));
        // pre-release / non-numeric tail parses conservatively
        assert!(is_newer("0.2.2-rc1", "0.2.1"));
        assert!(!is_newer("garbage", "0.2.1"));
    }
}
