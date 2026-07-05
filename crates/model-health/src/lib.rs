//! Model health: a circuit breaker + EWMA latency per model, persisted to disk.
//! Mirrors the GRAEAE breaker model (closed -> open with failure counting).
//!
//! The `Classification` enum + `provider_id` + `checked_at_unix` fields on
//! [`ModelHealth`] were added to support the daily `--probe --all` sweep:
//! `consult` reads them to know whether a stale entry should still be trusted
//! or whether the model is `Capacity` / `Unprovisioned` and should be skipped.
//! Every new field has `#[serde(default)]` so existing on-disk stores load
//! unchanged.

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

/// Outcome class for a single model probe.
///
/// The classify-from-error mapping lives in [`Classification::from_status`]
/// (HTTP code) and [`Classification::from_err_kind`] (typed provider error).
/// New variants MUST stay cheap to persist (one lowercase token) and the
/// serde rename below keeps the on-disk shape stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// HTTP 2xx (or any non-error provider response). The model is callable.
    Reachable,
    /// HTTP 429 (rate limit / quota) or 503 (provider capacity). The model
    /// exists but is currently refused; consult should temporarily skip it.
    Capacity,
    /// HTTP 404 (no such model at this provider). The id is wrong for this
    /// endpoint; consult should permanently skip it.
    Unprovisioned,
    /// Anything else: timeouts, decode errors, network, 5xx-other, etc.
    Error,
}

impl Classification {
    /// Map an HTTP status code to a classification. Anything not in {2xx,
    /// 404, 429, 503} falls through to `Error`.
    pub fn from_status(status: u16) -> Self {
        match status {
            200..=299 => Classification::Reachable,
            404 => Classification::Unprovisioned,
            429 | 503 => Classification::Capacity,
            _ => Classification::Error,
        }
    }

    /// `true` for outcomes that should make consult SKIP this model
    /// regardless of breakder state: `Capacity` (transient) and
    /// `Unprovisioned` (permanent).
    pub fn skips_consult(self) -> bool {
        matches!(
            self,
            Classification::Capacity | Classification::Unprovisioned
        )
    }

    /// Lowercase tag for the on-disk JSON and CLI output.
    pub fn as_str(self) -> &'static str {
        match self {
            Classification::Reachable => "reachable",
            Classification::Capacity => "capacity",
            Classification::Unprovisioned => "unprovisioned",
            Classification::Error => "error",
        }
    }
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
    /// Id of the provider that owns the most recent probe (None for legacy
    /// records written before `--probe --all` shipped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// Outcome class of the most recent probe. Used by `consult` to skip
    /// Capacity/Unprovisioned entries without re-pinging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<Classification>,
    /// Unix seconds of the most recent probe (success OR failure). consult
    /// uses this to render "checked 3h ago" freshness in its report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at_unix: Option<i64>,
}

impl ModelHealth {
    /// Stamp the record as just-checked. Callers do this on every probe,
    /// success or failure, so consult can show freshness and so a model that
    /// flipped to Capacity is recognised even though its breaker didn't open.
    pub fn mark_checked(&mut self, provider_id: &str, classification: Classification) {
        self.checked_at_unix = Some(now_unix());
        self.provider_id = Some(provider_id.to_string());
        self.classification = Some(classification);
    }

    /// `true` when the most recent classification is `Capacity` or
    /// `Unprovisioned`. consult treats these as "skip for now" independent
    /// of the breaker.
    pub fn is_skipped_by_classification(&self) -> bool {
        self.classification
            .map(Classification::skips_consult)
            .unwrap_or(false)
    }

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
        h.calls = h.calls.saturating_add(1);
        h.consecutive_failures = 0;
        if latency_ms.is_finite() && latency_ms >= 0.0 {
            h.ewma_latency_ms = Some(match h.ewma_latency_ms.filter(|v| v.is_finite()) {
                Some(prev) => 0.7 * prev + 0.3 * latency_ms,
                None => latency_ms,
            });
        }
        h.checked_at_unix = Some(now_unix());
    }

    /// Success + the provider that served the probe + the classified outcome.
    /// Used by `zoder health --probe --all` so the persisted record carries
    /// enough context for `consult` to show "last checked by provider X" and
    /// to skip Capacity/Unprovisioned models.
    pub fn record_classified_success(
        &mut self,
        model: &str,
        latency_ms: f64,
        provider_id: &str,
        classification: Classification,
    ) {
        let h = self.models.entry(model.to_string()).or_default();
        h.calls = h.calls.saturating_add(1);
        h.consecutive_failures = 0;
        if latency_ms.is_finite() && latency_ms >= 0.0 {
            h.ewma_latency_ms = Some(match h.ewma_latency_ms.filter(|v| v.is_finite()) {
                Some(prev) => 0.7 * prev + 0.3 * latency_ms,
                None => latency_ms,
            });
        }
        h.mark_checked(provider_id, classification);
    }

    pub fn record_failure(&mut self, model: &str, err: &str) {
        let h = self.models.entry(model.to_string()).or_default();
        h.calls = h.calls.saturating_add(1);
        h.failures = h.failures.saturating_add(1);
        h.consecutive_failures = h.consecutive_failures.saturating_add(1);
        h.last_error = Some(err.chars().take(160).collect());
        h.last_failure_unix = Some(now_unix());
        h.checked_at_unix = Some(now_unix());
    }

    /// Failure + classified outcome. Same shape as `record_classified_success`
    /// but resets the breaker state in the opposite direction. `provider_id`
    /// may be empty for legacy callers; the new field is left `None` in that
    /// case so the JSON stays minimal.
    pub fn record_classified_failure(
        &mut self,
        model: &str,
        err: &str,
        provider_id: &str,
        classification: Classification,
    ) {
        let h = self.models.entry(model.to_string()).or_default();
        h.calls = h.calls.saturating_add(1);
        h.failures = h.failures.saturating_add(1);
        h.consecutive_failures = h.consecutive_failures.saturating_add(1);
        h.last_error = Some(err.chars().take(160).collect());
        h.last_failure_unix = Some(now_unix());
        if !provider_id.is_empty() {
            h.mark_checked(provider_id, classification);
        } else {
            h.checked_at_unix = Some(now_unix());
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// outcome -> classification mapping. The matrix is small but every cell
    /// of the public contract is asserted: HTTP code first, the
    /// `skips_consult` predicate, and the on-disk JSON tag.
    #[test]
    fn classification_from_status_matrix() {
        // 2xx => Reachable.
        for s in [200u16, 201, 202, 204, 299] {
            assert_eq!(
                Classification::from_status(s),
                Classification::Reachable,
                "2xx {s} must classify as Reachable"
            );
        }
        // 404 => Unprovisioned (permanent skip).
        assert_eq!(
            Classification::from_status(404),
            Classification::Unprovisioned
        );
        // 429/503 => Capacity (transient skip).
        assert_eq!(Classification::from_status(429), Classification::Capacity);
        assert_eq!(Classification::from_status(503), Classification::Capacity);
        // Everything else => Error.
        for s in [400u16, 401, 403, 500, 502, 504] {
            assert_eq!(
                Classification::from_status(s),
                Classification::Error,
                "{s} must classify as Error"
            );
        }
    }

    #[test]
    fn skips_consult_predicate_matches_only_capacity_and_unprovisioned() {
        assert!(!Classification::Reachable.skips_consult());
        assert!(Classification::Capacity.skips_consult());
        assert!(Classification::Unprovisioned.skips_consult());
        assert!(!Classification::Error.skips_consult());
    }

    #[test]
    fn classification_serializes_as_lowercase_token() {
        // The on-disk JSON uses the snake_case rename — ensure it's stable so
        // a future change to the enum can't silently rename persisted records.
        assert_eq!(
            serde_json::to_string(&Classification::Reachable).unwrap(),
            "\"reachable\""
        );
        assert_eq!(
            serde_json::to_string(&Classification::Unprovisioned).unwrap(),
            "\"unprovisioned\""
        );
        assert_eq!(Classification::Reachable.as_str(), "reachable");
        assert_eq!(Classification::Capacity.as_str(), "capacity");
        assert_eq!(Classification::Unprovisioned.as_str(), "unprovisioned");
        assert_eq!(Classification::Error.as_str(), "error");
    }

    #[test]
    fn mark_checked_sets_provider_classification_and_timestamp() {
        let mut h = ModelHealth::default();
        assert!(h.classification.is_none());
        h.mark_checked("openrouter", Classification::Reachable);
        assert_eq!(h.provider_id.as_deref(), Some("openrouter"));
        assert_eq!(h.classification, Some(Classification::Reachable));
        assert!(h.checked_at_unix.is_some());
        // and the skippable predicate flips for Capacity/Unprovisioned.
        h.mark_checked("openrouter", Classification::Unprovisioned);
        assert!(h.is_skipped_by_classification());
    }

    #[test]
    fn legacy_store_without_new_fields_loads_with_defaults() {
        // A file written by the pre-Classification version of the store must
        // still load: every new field is `#[serde(default)]` and the per-key
        // record deserializes as Option::None / 0.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("legacy.json");
        let mut f = std::fs::File::create(&path).expect("create");
        writeln!(
            f,
            r#"{{
  "models": {{
    "openai/gpt-4o": {{
      "calls": 7,
      "failures": 1,
      "consecutive_failures": 0,
      "ewma_latency_ms": 412.5,
      "last_error": null,
      "last_failure_unix": 1700000000
    }}
  }}
}}"#
        )
        .unwrap();
        let store = HealthStore::load(&path);
        let h = store
            .models
            .get("openai/gpt-4o")
            .expect("legacy model loads");
        assert_eq!(h.calls, 7);
        assert_eq!(h.ewma_latency_ms, Some(412.5));
        // New fields default cleanly.
        assert!(h.provider_id.is_none());
        assert!(h.classification.is_none());
        assert!(h.checked_at_unix.is_none());
        assert!(!h.is_skipped_by_classification());
        // Re-saving keeps the record and adds the new fields as null.
        store.save().expect("save");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"provider_id\": null") || !raw.contains("provider_id"),
            "skip_serializing_if=Option::is_none must not emit provider_id when absent: {raw}"
        );
        assert!(raw.contains("\"calls\": 7"));
    }

    #[test]
    fn record_classified_success_records_latency_and_stamps_metadata() {
        let mut s = HealthStore::default();
        s.record_classified_success("model-a", 250.0, "openrouter", Classification::Reachable);
        let h = &s.models["model-a"];
        assert_eq!(h.calls, 1);
        assert_eq!(h.consecutive_failures, 0);
        assert_eq!(h.ewma_latency_ms, Some(250.0));
        assert_eq!(h.provider_id.as_deref(), Some("openrouter"));
        assert_eq!(h.classification, Some(Classification::Reachable));
        assert!(h.checked_at_unix.is_some());
    }

    #[test]
    fn record_classified_failure_keeps_breaker_signal_and_classification() {
        let mut s = HealthStore::default();
        s.record_classified_failure(
            "model-b",
            "boom",
            "openrouter",
            Classification::Unprovisioned,
        );
        let h = &s.models["model-b"];
        assert_eq!(h.calls, 1);
        assert_eq!(h.failures, 1);
        assert_eq!(h.consecutive_failures, 1);
        assert_eq!(h.last_error.as_deref(), Some("boom"));
        assert_eq!(h.classification, Some(Classification::Unprovisioned));
        assert!(h.is_skipped_by_classification());
        // Round-trip through the persistence path.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.json");
        // bind path through save() trick: re-load from a written copy.
        let json = serde_json::to_string(&s).unwrap();
        std::fs::write(&path, &json).unwrap();
        let reloaded = HealthStore::load(&path);
        let h = &reloaded.models["model-b"];
        assert_eq!(h.classification, Some(Classification::Unprovisioned));
        assert_eq!(h.provider_id.as_deref(), Some("openrouter"));
        assert!(h.is_skipped_by_classification());
    }
}
