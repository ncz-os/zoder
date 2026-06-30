//! Model health: a circuit breaker + EWMA latency per model, persisted to disk.
//! Mirrors the GRAEAE breaker model (closed -> open with failure counting).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Consecutive failures that trip the breaker.
const BREAKER_THRESHOLD: u32 = 3;
/// Cooldown before a tripped breaker allows a single half-open probe (seconds).
const BREAKER_COOLDOWN_SECS: i64 = 300;

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Healthy,
    Degraded,
    Down,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelHealth {
    pub calls: u64,
    pub failures: u64,
    pub consecutive_failures: u32,
    pub ewma_latency_ms: Option<f64>,
    pub last_error: Option<String>,
    /// Unix seconds of the most recent failure (drives breaker cooldown).
    #[serde(default)]
    pub last_failure_unix: Option<i64>,
}

impl ModelHealth {
    pub fn state(&self) -> State {
        if self.consecutive_failures >= BREAKER_THRESHOLD {
            State::Down
        } else if self.consecutive_failures >= 1 {
            State::Degraded
        } else {
            State::Healthy
        }
    }
    /// Breaker open => skip in routing. After the cooldown elapses since the
    /// last failure the breaker goes half-open (returns false) to allow a
    /// single probe; a success resets it, a failure re-opens it.
    pub fn breaker_open(&self) -> bool {
        self.breaker_open_at(now_unix())
    }
    fn breaker_open_at(&self, now: i64) -> bool {
        if self.consecutive_failures < BREAKER_THRESHOLD {
            return false;
        }
        match self.last_failure_unix {
            // Open during cooldown; half-open (selectable) once it elapses.
            Some(ts) => now.saturating_sub(ts) < BREAKER_COOLDOWN_SECS,
            // No timestamp recorded: stay open (conservative).
            None => true,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HealthStore {
    #[serde(default)]
    pub models: HashMap<String, ModelHealth>,
    #[serde(skip)]
    path: PathBuf,
}

impl HealthStore {
    pub fn load(path: &Path) -> Self {
        let mut store: HealthStore = match std::fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(st) => st,
                Err(e) => {
                    // Do not silently wipe history: warn and preserve the bad
                    // file for inspection before starting fresh.
                    let backup = path.with_extension("json.corrupt");
                    eprintln!(
                        "zoder: warning: health store {} is corrupt ({e}); backing up to {}",
                        path.display(),
                        backup.display()
                    );
                    if let Err(be) = std::fs::rename(path, &backup) {
                        eprintln!("zoder: warning: could not back up corrupt health store: {be}");
                    }
                    HealthStore::default()
                }
            },
            // Not present yet: a fresh store is expected, no warning.
            Err(_) => HealthStore::default(),
        };
        store.path = path.to_path_buf();
        store
    }

    pub fn record_success(&mut self, model: &str, latency_ms: f64) {
        let h = self.models.entry(model.to_string()).or_default();
        h.calls += 1;
        h.consecutive_failures = 0;
        h.ewma_latency_ms = Some(match h.ewma_latency_ms {
            Some(prev) => 0.7 * prev + 0.3 * latency_ms,
            None => latency_ms,
        });
    }

    pub fn record_failure(&mut self, model: &str, err: &str) {
        let h = self.models.entry(model.to_string()).or_default();
        h.calls += 1;
        h.failures += 1;
        h.consecutive_failures += 1;
        h.last_error = Some(err.chars().take(160).collect());
        h.last_failure_unix = Some(now_unix());
    }

    pub fn breaker_open(&self, model: &str) -> bool {
        self.models
            .get(model)
            .map(|h| h.breaker_open())
            .unwrap_or(false)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        // Atomic write: serialize to a temp file then rename over the target so
        // a crash mid-write can never truncate the live store.
        let data = serde_json::to_string_pretty(self)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}
