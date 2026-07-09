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

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
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
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo());
    let resp = client().get(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("GitHub API: HTTP {}", resp.status());
    }
    let rel: GhRelease = resp.json().await?;
    Ok(rel.tag_name.trim_start_matches('v').to_string())
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
pub async fn check_cached(home: &Path, ttl: Duration) -> Option<UpdateStatus> {
    if std::env::var_os("ZODER_NO_UPDATE_CHECK").is_some() {
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
    // Stale/absent: one network check, then cache it.
    let latest = latest_release().await.ok()?;
    let _ = std::fs::create_dir_all(home);
    if let Ok(json) = serde_json::to_string(&CacheFile {
        checked_at: now,
        latest: latest.clone(),
    }) {
        let _ = std::fs::write(&path, json);
    }
    let cur = current().to_string();
    let newer = is_newer(&latest, &cur);
    Some(UpdateStatus {
        current: cur,
        latest,
        newer,
    })
}

#[cfg(test)]
mod tests {
    use super::is_newer;

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
