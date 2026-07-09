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
use std::sync::atomic::{AtomicU64, Ordering};

/// Consecutive failures that trip the breaker.
const BREAKER_THRESHOLD: u32 = 3;
/// Cooldown before a tripped breaker allows a single half-open probe (seconds).
const BREAKER_COOLDOWN_SECS: i64 = 300;

fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Monotonic per-process nonce so two concurrent `save()` calls in the same
/// process never collide on the temp path. Combined with `std::process::id()`
/// this makes every in-flight temp file unique across processes and threads,
/// so a half-written temp from one writer can never be renamed over the live
/// store by another (C4-MH1: torn file).
static SAVE_NONCE: AtomicU64 = AtomicU64::new(0);

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
/// (HTTP code), [`Classification::from_anthropic_error_body`] (typed
/// Anthropic Messages API error envelope), and
/// [`Classification::from_err_kind`] (typed provider error).
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
    /// HTTP 401/403 (missing/invalid/expired credential for the provider).
    /// The provider is rejecting the caller's API key — that is a
    /// configuration/account problem, not model ill-health, so it MUST NOT
    /// trip the circuit breaker and MUST be skipped by consult until the
    /// operator fixes the key. Persisted as the lowercase token
    /// "unauthorized".
    Unauthorized,
    /// Anything else: timeouts, decode errors, network, 5xx-other, etc.
    Error,
    /// Forward-compat catch-all (C7-M1). A store written by a NEWER binary
    /// may carry a classification token this build does not know (e.g. a
    /// future "overloaded"). `#[serde(other)]` makes any unrecognized token
    /// deserialize to `Unknown` instead of raising an "unknown variant"
    /// serde error — which, on the whole-store `serde_json::from_str` in
    /// [`HealthStore::load`], would otherwise be treated as corruption and
    /// WIPE every model's history. `Unknown` is deliberately NEUTRAL: it is
    /// not `Reachable` (so it never counts as healthy), it does NOT
    /// `skips_consult` (so it never forces a skip on its own), and it does
    /// NOT trip the breaker. It renders as the lowercase token "unknown".
    /// Note: serde applies `other` only on the DESERIALIZE side; if this
    /// build re-saves an `Unknown` it serializes as "unknown" (the original
    /// future token is not preserved) — acceptable, because the required
    /// property is that the surrounding store survives the round-trip, not
    /// that the future token itself does.
    #[serde(other)]
    Unknown,
}

impl Classification {
    /// Map an HTTP status code to a classification. Anything not in {2xx,
    /// 404, 429, 503, 401, 403, 529} falls through to `Error`.
    pub fn from_status(status: u16) -> Self {
        match status {
            200..=299 => Classification::Reachable,
            404 => Classification::Unprovisioned,
            // 401/403 are the provider REJECTING THE CREDENTIAL (missing /
            // invalid / expired API key). That is a configuration problem
            // for the operator, not model ill-health — classify as
            // Unauthorized so consult skips the model and the circuit
            // breaker does not open on the (perfectly fine) model behind it.
            401 | 403 => Classification::Unauthorized,
            // 429 (rate limit), 503 (service unavailable), and 529
            // ("site is overloaded" — Cloudflare / Anthropic-style
            // capacity signal) are all transient capacity outcomes:
            // consult should temporarily skip the model and the breaker
            // should back off rather than trip. Mapping 529 here
            // guarantees the same bucket regardless of body shape — a
            // non-JSON 529 body (HTML gateway error page, plain text)
            // still lands on Capacity instead of falling through to
            // Error and tripping the breaker on a perfectly fine model.
            429 | 503 | 529 => Classification::Capacity,
            _ => Classification::Error,
        }
    }

    /// Classify an Anthropic Messages API error response. Anthropic
    /// surfaces typed errors in a single envelope of the shape
    ///
    /// ```text
    /// {
    ///   "type": "error",
    ///   "error": {
    ///     "type": "authentication_error" | "rate_limit_error" | "overloaded_error" | ...,
    ///     "message": "..."
    ///   }
    /// }
    /// ```
    ///
    /// mirroring the `{"error":{"code":..., "message":...}}` envelope
    /// OpenAI / OpenAI-compatible backends use. The mapping is
    /// deliberately identical to what `Classification::from_status`
    /// already produces for the matching HTTP code, so a credential
    /// rejection lands on `Unauthorized` regardless of whether it was
    /// carried by `type: "authentication_error"` (the body) or
    /// `status: 401` (the headers), a rate-limit lands on `Capacity`,
    /// and an `overloaded_error` (Anthropic's 529) lands on `Capacity`
    /// the same way 429 / 503 do. An unparseable body, or an
    /// unrecognized `error.type`, falls through to `from_status(status)`
    /// so the HTTP-code path remains the source of truth for everything
    /// the typed envelope does not cover.
    ///
    /// This is the Anthropic counterpart to the OpenAI-style
    /// `from_status`-driven classification already used in
    /// `zoder-core::health_probe::classify_err`. Both backends now land
    /// on the same `Classification` variants for the same operational
    /// outcomes — credential rejection, capacity, model-missing,
    /// generic error — so the breaker + consult paths do not need a
    /// per-provider branch.
    pub fn from_anthropic_error_body(body: &str, status: u16) -> Self {
        // Defensive: every malformed body falls back to the status-only
        // path so an upstream caller that hands us an empty string, an
        // HTML gateway error page, or a truncated body still gets a
        // well-defined Classification instead of a silent default.
        let parsed = match serde_json::from_str::<serde_json::Value>(body) {
            Ok(v) => v,
            Err(_) => return Classification::from_status(status),
        };
        // Anthropic error envelope: top-level `type: "error"` and a
        // nested `error.type` string naming the typed rejection. Both
        // fields must agree for the body to be considered a typed
        // Anthropic error envelope; anything else (e.g. an OpenAI-style
        // `{"error":{"code":"...",...}}` body that a future gateway
        // might proxy through) defers to the status code.
        let outer_type = parsed.get("type").and_then(|t| t.as_str());
        let inner_type = parsed
            .get("error")
            .and_then(|e| e.get("type"))
            .and_then(|t| t.as_str());
        let Some(inner) = inner_type else {
            return Classification::from_status(status);
        };
        if outer_type != Some("error") {
            return Classification::from_status(status);
        }
        match inner {
            // Auth/credential rejection — never trip the breaker (the
            // model itself is fine; the operator's API key is wrong).
            "authentication_error" | "permission_error" => Classification::Unauthorized,
            // Rate-limit + Anthropic's 529 overloaded_error are both
            // transient capacity signals — consult should skip until
            // the next probe, the same way 429 / 503 do.
            "rate_limit_error" | "overloaded_error" => Classification::Capacity,
            // Typed but unknown error name: defer to the status code so
            // 401/403/404/429/503 still map onto the right variant.
            _ => Classification::from_status(status),
        }
    }

    /// `true` for outcomes that should make consult SKIP this model
    /// regardless of breakder state: `Capacity` (transient),
    /// `Unprovisioned` (permanent), and `Unauthorized` (key rejected — keep
    /// retrying it does not help, the operator must fix the credential).
    pub fn skips_consult(self) -> bool {
        matches!(
            self,
            Classification::Capacity | Classification::Unprovisioned | Classification::Unauthorized
        )
    }

    /// Lowercase tag for the on-disk JSON and CLI output.
    pub fn as_str(self) -> &'static str {
        match self {
            Classification::Reachable => "reachable",
            Classification::Capacity => "capacity",
            Classification::Unprovisioned => "unprovisioned",
            Classification::Unauthorized => "unauthorized",
            Classification::Error => "error",
            // Forward-compat catch-all (C7-M1): a token from a newer binary
            // we don't recognize. Neutral display; never treated as healthy.
            Classification::Unknown => "unknown",
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

    /// `true` when the most recent classification is `Capacity`,
    /// `Unprovisioned`, or `Unauthorized`. consult treats these as "skip
    /// for now" independent of the breaker.
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
            // No timestamp recorded: allow a probe (half-open). A
            // legacy or partially-populated record carries no cooldown
            // anchor, so "permanently open" would mean "this model is
            // permanently unusable"; the SSE spec / breaker contract is
            // to instead let a probe through on the next routing pass so
            // a healthy model can self-recover. The model still has to
            // pass BREAKER_THRESHOLD consecutive failures before this
            // branch fires, so the breaker keeps its safety net.
            None => false,
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
                    //
                    // Unique suffix (C7-M1 bonus): a FIXED `health.json.corrupt`
                    // name means a second downgrade clobbers the first backup.
                    // Stamp the backup with unix-secs + pid + a monotonic nonce
                    // so repeated corrupt loads each keep their own copy.
                    let nonce = SAVE_NONCE.fetch_add(1, Ordering::Relaxed);
                    let stamp = format!(
                        "json.corrupt.{}.{}.{}",
                        now_unix(),
                        std::process::id(),
                        nonce
                    );
                    let backup = path.with_extension(stamp);
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
    ///
    /// SPECIAL CASE: when `classification.skips_consult()` is true — i.e.
    /// `Capacity` (429/503/529 / `overloaded_error` / `rate_limit_error`),
    /// `Unprovisioned` (404), or `Unauthorized` (401/403) — this is NOT a
    /// model-health failure. In all three cases the model itself is fine:
    /// the provider is transiently overloaded, the model isn't provisioned
    /// for this key, or the credential was rejected. consult already skips
    /// these by classification (`is_skipped_by_classification`), so counting
    /// them against the breaker is both redundant and harmful — a healthy
    /// model behind a temporarily overloaded provider would get bench-binned
    /// for the full `BREAKER_THRESHOLD` cooldown after a handful of 529s.
    /// We must therefore NOT increment `consecutive_failures`, NOT bump
    /// `failures`, and NOT stamp `last_failure_unix` (which would otherwise
    /// pin the breaker open) for any skip-class outcome. We still call
    /// `mark_checked` so the on-disk store records the probe attempt, the
    /// provider, and the classification — consult uses that to skip the
    /// model until the transient/credential/provisioning condition clears.
    /// Only a genuine `Error` (500 / network / timeout / decode) trips the
    /// breaker. This matches the `skips_consult` doc contract: those classes
    /// are skipped "regardless of breaker state".
    pub fn record_classified_failure(
        &mut self,
        model: &str,
        err: &str,
        provider_id: &str,
        classification: Classification,
    ) {
        let h = self.models.entry(model.to_string()).or_default();
        h.calls = h.calls.saturating_add(1);
        if classification.skips_consult() {
            // Skip-class outcome (Capacity / Unprovisioned / Unauthorized) —
            // the model is unknown-healthy; the condition is transient
            // (overload), provisioning, or an operator-side credential. Stamp
            // the probe (so consult can render freshness + the skip tag) but
            // leave breaker state untouched. Truncate the error string the
            // same way the failure path does so legacy readers see a
            // consistent last_error length cap (still records WHAT went
            // wrong, just does not count it as a model failure).
            h.last_error = Some(err.chars().take(160).collect());
            if !provider_id.is_empty() {
                h.mark_checked(provider_id, classification);
            } else {
                h.checked_at_unix = Some(now_unix());
                h.classification = Some(classification);
            }
            return;
        }
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

    /// `true` when the model's most recent classification marks it "skip for
    /// now" (Unauthorized/Unprovisioned/Capacity). Mirrors `breaker_open`:
    /// these classifications are breaker-neutral (W1), so the breaker stays
    /// closed forever for a 401/404 model — routing must consult this too or
    /// it will keep selecting a guaranteed-failed model. Unknown model =>
    /// false (nothing recorded means nothing to skip).
    pub fn is_skipped_by_classification(&self, model: &str) -> bool {
        self.models
            .get(model)
            .map(|h| h.is_skipped_by_classification())
            .unwrap_or(false)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        Self::write_atomic(&self.path, self)?;
        Ok(())
    }

    /// Serialize `store` to `path` via a UNIQUE temp file then rename over the
    /// target. The temp name carries the process id AND a monotonic nonce
    /// (`<stem>.json.tmp.<pid>.<nonce>`) so two concurrent writers can never
    /// share a temp path — that is what makes the rename atomic-swap safe
    /// under real fan-out (C4-MH1: torn file). The temp is removed if the
    /// write or rename fails, so a crash mid-write never litters the dir with
    /// a stale half-written temp that a later reader could pick up.
    fn write_atomic(path: &Path, store: &HealthStore) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let data = serde_json::to_string_pretty(store)?;
        let nonce = SAVE_NONCE.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), nonce));
        // Write to the unique temp; on any failure remove it so it can never
        // be renamed over the live store or left behind torn.
        if let Err(e) = std::fs::write(&tmp, &data) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// Atomic locked read-modify-write. Takes an exclusive advisory lock on a
    /// sibling lockfile (`<stem>.lock`) for the WHOLE load -> apply(f) -> save
    /// critical section, so concurrent writers (the reviewer-panel `join_all`
    /// fan-out, and daemon+CLI both doing load -> record -> save) serialize
    /// and each one observes the latest on-disk state BEFORE applying its own
    /// delta. This is the fix for C4-MH1's lost-update: without it P2 loads a
    /// snapshot, P1 saves a recorded failure, then P2's save clobbers P1's
    /// failure (a real failure is dropped and the breaker under-counts).
    ///
    /// The lock is an MSRV-safe **lockfile**: exclusive ownership is claimed by
    /// `OpenOptions::create_new(true)` on `<stem>.lock` (stable since Rust 1.9,
    /// no MSRV gap — unlike `File::lock`, which needs 1.89). `create_new` is an
    /// atomic O_CREAT|O_EXCL create: exactly one racer wins; the losers see
    /// `AlreadyExists` and spin-retry with a short sleep up to a bounded
    /// timeout, returning an `io::Error` rather than hanging. A very old lock
    /// (older than `LOCK_STALE_SECS`) is treated as abandoned and force-broken
    /// so a crashed holder can't wedge the store forever. The lockfile is
    /// removed by a `LockGuard` Drop, so it is cleaned up even on panic or
    /// early return. The closure sees a store loaded from `path` UNDER the
    /// lock; its mutation is persisted by `write_atomic` before the guard
    /// drops. I/O errors from the load path are non-fatal (a fresh store is
    /// used, mirroring `load`); lock-acquire and save failures propagate.
    pub fn mutate_locked(path: &Path, f: impl FnOnce(&mut HealthStore)) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let lock_path = path.with_extension("lock");
        // Acquire the lockfile (RAII: released on Drop, incl. panic / early
        // return) BEFORE loading, so the whole read-modify-write is exclusive.
        let _guard = LockGuard::acquire(&lock_path)?;
        // Load the freshest on-disk state UNDER the lock so we merge onto
        // whatever the previous holder just wrote.
        let mut store = HealthStore::load(path);
        f(&mut store);
        HealthStore::write_atomic(path, &store).map_err(|e| std::io::Error::other(e.to_string()))
        // `_guard` drops here, removing the lockfile.
    }
}

/// Max time to wait for the health-store lockfile before giving up with a
/// `TimedOut` error (rather than hanging a review or daemon flush forever).
const LOCK_TIMEOUT_MS: u64 = 5_000;
/// Poll interval while another writer holds the lock.
const LOCK_RETRY_MS: u64 = 5;
/// A lockfile older than this is treated as abandoned (a crashed holder that
/// never removed it) and force-broken. Generous relative to the critical
/// section (a load + serialize + rename is sub-millisecond), so a live holder
/// is never mistaken for a stale one.
const LOCK_STALE_SECS: u64 = 30;

/// RAII guard for the `<stem>.lock` lockfile: owns exclusive access for the
/// critical section and removes the file on Drop (so a panic or early return
/// still releases the lock). Acquisition is an atomic `create_new` with a
/// bounded spin-retry; a very old lock is treated as stale and broken.
struct LockGuard {
    path: PathBuf,
}

impl LockGuard {
    fn acquire(lock_path: &Path) -> std::io::Result<Self> {
        let start = std::time::Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(lock_path)
            {
                // Won the race: we own the lock.
                Ok(_file) => {
                    return Ok(LockGuard {
                        path: lock_path.to_path_buf(),
                    });
                }
                // Someone else holds it — wait, break-if-stale, or time out.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Self::is_stale(lock_path) {
                        // Best-effort break of an abandoned lock. If removal
                        // races another breaker, the next create_new attempt
                        // sorts out the single winner.
                        let _ = std::fs::remove_file(lock_path);
                        continue;
                    }
                    if start.elapsed().as_millis() as u64 >= LOCK_TIMEOUT_MS {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            format!(
                                "timed out after {LOCK_TIMEOUT_MS}ms waiting for health-store lock {}",
                                lock_path.display()
                            ),
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(LOCK_RETRY_MS));
                }
                // Any other error (e.g. permission) is fatal for acquisition.
                Err(e) => return Err(e),
            }
        }
    }

    /// True when the lockfile's mtime is older than `LOCK_STALE_SECS`, i.e. a
    /// crashed holder likely never cleaned it up. Metadata/time errors return
    /// false (treat as fresh) so a transient stat error can't cause us to
    /// break a live lock.
    fn is_stale(lock_path: &Path) -> bool {
        let Ok(meta) = std::fs::metadata(lock_path) else {
            return false;
        };
        let Ok(modified) = meta.modified() else {
            return false;
        };
        match modified.elapsed() {
            Ok(age) => age.as_secs() >= LOCK_STALE_SECS,
            // Clock moved backwards / mtime in the future: not stale.
            Err(_) => false,
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Best-effort release. If it's already gone (e.g. a stale-breaker
        // removed it), that's fine.
        let _ = std::fs::remove_file(&self.path);
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
        // 401/403 => Unauthorized (credential rejection — must NOT trip the
        // breaker; see record_classified_failure_preserves_breaker_on_auth).
        assert_eq!(
            Classification::from_status(401),
            Classification::Unauthorized
        );
        assert_eq!(
            Classification::from_status(403),
            Classification::Unauthorized
        );
        // 429/503 => Capacity (transient skip).
        assert_eq!(Classification::from_status(429), Classification::Capacity);
        assert_eq!(Classification::from_status(503), Classification::Capacity);
        // Everything else => Error.
        for s in [400u16, 500, 502, 504] {
            assert_eq!(
                Classification::from_status(s),
                Classification::Error,
                "{s} must classify as Error"
            );
        }
    }

    /// Z-22: HTTP 529 ("Site is overloaded", Cloudflare / Anthropic /
    /// OpenAI-style capacity signal) MUST classify as `Capacity` -- same
    /// bucket as 503 -- regardless of whether the response body is JSON,
    /// HTML, or plain text. Pre-fix the `from_status` match had
    /// `429 | 503 => Capacity` and `_ => Error`, so a 529 response with a
    /// non-JSON body (e.g. an HTML gateway error page) fell through to
    /// `Error`, which trips the breaker on a perfectly fine model that's
    /// just being rate-limited by its own provider. Post-fix 529 joins
    /// the Capacity bucket so the breaker backs off instead of opening.
    ///
    /// The assertion exercises the REAL `Classification::from_status` --
    /// the same surface the HTTP-error branch in `OpenAiProvider` feeds
    /// into `classify_err`, and the same surface the Anthropic typed
    /// envelope (`from_anthropic_error_body`) defers to for unrecognized
    /// error types. So a single pin covers both providers.
    ///
    /// Companion regression: the existing `from_anthropic_error_body`
    /// already routes the TYPED `overloaded_error` envelope to Capacity
    /// (see `anthropic_error_body_maps_typed_envelope_onto_classification`),
    /// but a misbehaving gateway that emits a 529 with a NON-typed body
    /// (HTML gateway error page, empty string, truncated body) used to
    /// fall through to `from_status` and land on `Error` -- the fix
    /// closes that gap.
    #[test]
    fn http_529_classifies_as_capacity_not_error() {
        // The task-pinned assertion: 529 is a transient capacity signal,
        // not a hard error. Skips-consult too -- the model is fine, the
        // provider is just overloaded.
        assert_eq!(
            Classification::from_status(529),
            Classification::Capacity,
            "HTTP 529 (site overloaded) must classify as Capacity, same as 503, \
             so the breaker backs off rather than trips on a perfectly fine model"
        );
        assert!(
            Classification::from_status(529).skips_consult(),
            "529 must behave like 503 for consult (transient skip, no breaker trip)"
        );
        // Sanity: 503 / 429 / 529 all land on the SAME variant. A future
        // refactor that splits the bucket (e.g. adds a new `Overloaded`
        // variant) is caught by this matrix pin.
        assert_eq!(
            Classification::from_status(529),
            Classification::from_status(503),
            "529 must match 503 -- both are transient capacity signals"
        );
        assert_eq!(
            Classification::from_status(529),
            Classification::from_status(429),
            "529 must match 429 -- both are transient capacity signals"
        );
        // And 529 MUST NOT regress to Error: a tripped breaker on a
        // perfectly fine model is exactly the failure mode Z-22 names.
        assert_ne!(
            Classification::from_status(529),
            Classification::Error,
            "529 must NOT classify as Error (would trip breaker on transient overload)"
        );
    }

    #[test]
    fn unauthorized_has_lowercase_token_and_round_trips_through_serde() {
        // snake_case rename must produce the lowercase token "unauthorized"
        // and it must round-trip through the on-disk JSON shape so a future
        // enum tweak can't silently rename a persisted record.
        assert_eq!(Classification::Unauthorized.as_str(), "unauthorized");
        assert_eq!(
            serde_json::to_string(&Classification::Unauthorized).unwrap(),
            "\"unauthorized\""
        );
        let back: Classification = serde_json::from_str("\"unauthorized\"").expect("deserialize");
        assert_eq!(back, Classification::Unauthorized);
    }

    /// Anthropic error-body classification. Mirrors the `from_status`
    /// mapping for the same operational outcomes but recognizes the
    /// typed Anthropic envelope:
    /// `{"type":"error","error":{"type":"authentication_error"|...}}`.
    /// Every cell of the contract is pinned: the typed envelope wins
    /// over the status code (so a 200 with an inline error body still
    /// classifies correctly — Anthropic never does this, but a
    /// misbehaving gateway might), an unparseable body falls through
    /// to `from_status`, and an OpenAI-style envelope (no top-level
    /// `type: "error"`) also defers to the status code so the two
    /// backends share the same downstream classification pipeline.
    #[test]
    fn anthropic_error_body_maps_typed_envelope_onto_classification() {
        // The task-pinned case: a 401 carrying `authentication_error`
        // MUST classify as Unauthorized (credential rejection — never
        // trip the breaker; consult skips the model).
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        assert_eq!(
            Classification::from_anthropic_error_body(body, 401),
            Classification::Unauthorized,
            "Anthropic 401 + authentication_error must classify as Unauthorized"
        );
        // permission_error is Anthropic's name for a 403 with the same
        // operational meaning — same outcome, same classification.
        assert_eq!(
            Classification::from_anthropic_error_body(
                r#"{"type":"error","error":{"type":"permission_error","message":"key lacks scope"}}"#,
                403
            ),
            Classification::Unauthorized
        );
        // Rate-limit envelopes (HTTP 429) land on Capacity — consult
        // skips until the next probe, the same way 429 already does in
        // `from_status`.
        assert_eq!(
            Classification::from_anthropic_error_body(
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
                429
            ),
            Classification::Capacity
        );
        // Anthropic's overloaded_error (HTTP 529) is also a transient
        // capacity signal — same outcome as 503. We mirror the existing
        // 429/503 -> Capacity mapping so the breaker + consult paths
        // treat both backends the same.
        assert_eq!(
            Classification::from_anthropic_error_body(
                r#"{"type":"error","error":{"type":"overloaded_error","message":"try again"}}"#,
                529
            ),
            Classification::Capacity
        );
        // Typed envelope wins over the status code when the two
        // disagree — a 500 with an authentication_error body is still
        // a credential rejection and must NOT be lumped into Error
        // (which would trip the breaker on a perfectly fine model).
        assert_eq!(
            Classification::from_anthropic_error_body(
                r#"{"type":"error","error":{"type":"authentication_error","message":"x"}}"#,
                500
            ),
            Classification::Unauthorized
        );
        // An unknown typed error name defers to the status code so
        // the well-known mappings still apply — e.g. a future
        // `not_found_error` carrying HTTP 404 still classifies as
        // Unprovisioned.
        assert_eq!(
            Classification::from_anthropic_error_body(
                r#"{"type":"error","error":{"type":"not_found_error","message":"x"}}"#,
                404
            ),
            Classification::Unprovisioned
        );
        // OpenAI-style envelope (no top-level `type: "error"`) defers
        // to the status code — a 401 with an OpenAI body still lands
        // on Unauthorized through `from_status`.
        assert_eq!(
            Classification::from_anthropic_error_body(
                r#"{"error":{"code":"invalid_api_key","message":"x"}}"#,
                401
            ),
            Classification::Unauthorized
        );
        // Unparseable body + 401 still lands on Unauthorized via the
        // status fallback (the caller can always trust
        // `from_status` to do the right thing).
        assert_eq!(
            Classification::from_anthropic_error_body("not-json", 401),
            Classification::Unauthorized
        );
        // Unparseable body + 500 lands on Error (status fallback).
        assert_eq!(
            Classification::from_anthropic_error_body("garbage", 500),
            Classification::Error
        );
    }

    #[test]
    fn anthropic_authentication_error_classifies_as_unauthorized() {
        // Task-pinned assertion (adversarial-review finding #5): an
        // Anthropic 401 with a typed `authentication_error` body
        // classifies as Classification::Unauthorized — identical to
        // the OpenAI 401 path so a credential rejection never trips
        // the breaker on a perfectly fine model behind the bad key.
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        let cls = Classification::from_anthropic_error_body(body, 401);
        assert_eq!(cls, Classification::Unauthorized);
        // And the same classification survives the
        // record_classified_failure -> "breaker stays closed" contract
        // — i.e. the Anthropic envelope feeds into the same store
        // path the OpenAI 401 already exercises.
        let mut s = HealthStore::default();
        let model = "anthropic/claude-3.7";
        for _ in 0..(BREAKER_THRESHOLD * 2) {
            s.record_classified_failure(model, body, "anthropic", cls);
        }
        assert_eq!(
            s.models[model].consecutive_failures, 0,
            "Anthropic authentication_error must not increment the breaker (Unauthorized special-case)"
        );
        assert!(s.models[model].classification == Some(Classification::Unauthorized));
        assert!(!s.breaker_open(model));
    }

    #[test]
    fn skips_consult_predicate_includes_unauthorized() {
        // AuthenticatED models must be routable as normal; rejected credentials
        // must make consult skip the model without re-trying it.
        assert!(!Classification::Reachable.skips_consult());
        assert!(Classification::Capacity.skips_consult());
        assert!(Classification::Unprovisioned.skips_consult());
        assert!(Classification::Unauthorized.skips_consult());
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
        assert_eq!(
            serde_json::to_string(&Classification::Unauthorized).unwrap(),
            "\"unauthorized\""
        );
        assert_eq!(Classification::Reachable.as_str(), "reachable");
        assert_eq!(Classification::Capacity.as_str(), "capacity");
        assert_eq!(Classification::Unprovisioned.as_str(), "unprovisioned");
        assert_eq!(Classification::Unauthorized.as_str(), "unauthorized");
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
        // Unprovisioned is a skip-class outcome (skips_consult) — it must NOT
        // count against the breaker (W1): consult already skips the model by
        // classification, so failures/consecutive_failures/last_failure_unix
        // stay untouched. Only a genuine Error trips the breaker.
        assert_eq!(h.failures, 0, "Unprovisioned must not count as a failure");
        assert_eq!(
            h.consecutive_failures, 0,
            "Unprovisioned must not increment consecutive_failures"
        );
        assert!(
            h.last_failure_unix.is_none(),
            "Unprovisioned must not stamp last_failure_unix (would arm breaker)"
        );
        // Diagnostics + classification are still recorded so consult can
        // render the skip tag.
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

    // ---------------------------------------------------------------------
    // Regression tests for the "401/403 trips the breaker" defect.
    //
    // Symptom: a provider rejecting the API key with HTTP 401/403 used to
    // classify as Error and increment consecutive_failures; after the third
    // rejection the circuit breaker opened on a perfectly fine model whose
    // only problem was the operator's credential. These tests pin the
    // corrected behavior: credential rejections NEVER trip the breaker,
    // while genuine Errors still do. They live here (not in
    // zoder-core/src/health_probe.rs) because the fix is inside the
    // model-health crate's record_classified_failure path and we want a
    // focused unit that exercises the public surface in isolation.
    // ---------------------------------------------------------------------

    /// A single 401/403 probe stamps the model as checked (so consult can
    /// render the unauthorized tag) but it must NOT bump
    /// `consecutive_failures`, must NOT bump `failures`, and must NOT stamp
    /// `last_failure_unix` (which would otherwise arm the breaker cooldown).
    #[test]
    fn record_classified_failure_does_not_trip_breaker_on_auth() {
        let mut s = HealthStore::default();
        s.record_classified_failure(
            "openai/gpt-4o",
            "provider HTTP 401 Unauthorized: invalid api key",
            "openrouter",
            Classification::Unauthorized,
        );
        let h = &s.models["openai/gpt-4o"];
        // calls IS counted (a probe happened) but the breaker-relevant
        // counters are untouched.
        assert_eq!(h.calls, 1);
        assert_eq!(
            h.failures, 0,
            "Unauthorized must not be counted as a failure"
        );
        assert_eq!(
            h.consecutive_failures, 0,
            "Unauthorized must not increment consecutive_failures"
        );
        assert!(
            h.last_failure_unix.is_none(),
            "Unauthorized must not stamp last_failure_unix (would arm breaker)"
        );
        // The diagnostic string IS preserved so an operator inspecting the
        // store can see why consult is skipping the model.
        assert_eq!(
            h.last_error.as_deref(),
            Some("provider HTTP 401 Unauthorized: invalid api key")
        );
        // And the consult path sees the unauthorized classification and
        // skips the model — exactly what the task wants.
        assert_eq!(h.classification, Some(Classification::Unauthorized));
        assert!(h.is_skipped_by_classification());
        assert!(!s.breaker_open("openai/gpt-4o"));
    }

    /// Even after MORE than BREAKER_THRESHOLD auth-rejected probes, the
    /// breaker MUST stay closed. This is the test that fails on the old
    /// code (where 401/403 mapped to Error and record_classified_failure
    /// incremented consecutive_failures every time).
    #[test]
    fn many_unauthorized_probes_never_open_breaker() {
        let mut s = HealthStore::default();
        let model = "anthropic/claude-3.7";
        // Hammer the probe with >= BREAKER_THRESHOLD 401/403 outcomes.
        for i in 0..(BREAKER_THRESHOLD * 5) {
            s.record_classified_failure(
                model,
                &format!("provider HTTP 401 Unauthorized: probe {i}"),
                "openrouter",
                Classification::Unauthorized,
            );
        }
        let h = &s.models[model];
        assert_eq!(h.calls, (BREAKER_THRESHOLD * 5) as u64);
        assert_eq!(h.failures, 0);
        assert_eq!(h.consecutive_failures, 0);
        assert!(h.last_failure_unix.is_none());
        assert!(
            !s.breaker_open(model),
            "breaker must NEVER open on auth rejection"
        );
        // state() must still report Healthy — the model is fine.
        assert_eq!(h.state(), State::Healthy);
    }

    /// Sanity check: the SAME count of genuine Errors DOES open the
    /// breaker. Without this, the previous test could be passing only
    /// because record_classified_failure is broken in some other way.
    #[test]
    fn many_error_probes_open_breaker() {
        let mut s = HealthStore::default();
        let model = "broken/model";
        for _ in 0..BREAKER_THRESHOLD {
            s.record_classified_failure(model, "boom", "openrouter", Classification::Error);
        }
        let h = &s.models[model];
        assert_eq!(h.consecutive_failures, BREAKER_THRESHOLD);
        assert_eq!(h.failures, BREAKER_THRESHOLD as u64);
        assert!(s.breaker_open(model), "Errors must still trip the breaker");
        assert_eq!(h.state(), State::Down);
    }

    /// After an Unauthorized-stamped probe, a subsequent SUCCESS resets the
    /// breaker-clean state we just preserved — proving the two code paths
    /// remain consistent (no leftover counter corruption).
    #[test]
    fn auth_rejection_then_success_keeps_record_consistent() {
        let mut s = HealthStore::default();
        let model = "x/y";
        for _ in 0..(BREAKER_THRESHOLD * 2) {
            s.record_classified_failure(model, "401", "openrouter", Classification::Unauthorized);
        }
        assert_eq!(s.models[model].consecutive_failures, 0);
        // A later success should land cleanly with no leftover breaker
        // debt to clear.
        s.record_classified_success(model, 100.0, "openrouter", Classification::Reachable);
        assert_eq!(s.models[model].consecutive_failures, 0);
        assert_eq!(s.models[model].failures, 0);
        assert_eq!(
            s.models[model].classification,
            Some(Classification::Reachable)
        );
        assert!(!s.breaker_open(model));
    }

    // ---------------------------------------------------------------------
    // Z-21: legacy breaker record with consecutive_failures >= threshold but
    // NO last_failure_unix must be PROBE-ELIGIBLE (half-open), not stuck
    // OPEN forever. Pre-fix the `breaker_open_at` path keyed exclusively on
    // the elapsed-since-failure cooldown and fell through to `None => true`,
    // pinning the breaker open permanently — a partially-populated record
    // written before the timestamp field shipped (or hand-edited, or
    // populated by an older `record_failure` that did not stamp the unix
    // field) would never recover. Post-fix the absence of a timestamp is
    // treated as "we don't know when the failure happened, so let a probe
    // through" rather than "never let a probe through".
    //
    // The test exercises the REAL `ModelHealth::breaker_open` path (the same
    // surface `zoder-cli` and `zoder-core::router` consult) on a directly
    // constructed `ModelHealth`, so a future refactor that moves the
    // half-open gate stays covered. Companion assertions pin the
    // non-regression contract: a healthy model stays closed, a fully-populated
    // tripped model behaves exactly as before.
    // ---------------------------------------------------------------------

    /// The bug. A `ModelHealth` written before the `last_failure_unix`
    /// field existed (or written with `None` by some legacy path) MUST NOT
    /// be stuck OPEN forever -- the absence of a timestamp means "we don't
    /// know when the failure was", not "never recover". The breaker should
    /// treat this as half-open / probe-eligible, i.e. `breaker_open()`
    /// returns `false`. Pre-fix this assertion fails because the
    /// `last_failure_unix = None` branch returned `true` unconditionally.
    #[test]
    fn breaker_recovers_on_legacy_record_without_timestamp() {
        let h = ModelHealth {
            consecutive_failures: BREAKER_THRESHOLD,
            last_failure_unix: None,
            ..Default::default()
        };
        assert!(
            !h.breaker_open(),
            "legacy timestampless record with consecutive_failures >= threshold \
             must be probe-eligible (half-open), not permanently OPEN"
        );
    }

    /// Even when the legacy record is "really" tripped (failures well above
    /// the threshold, no other fields populated), the absence of a
    /// timestamp must STILL allow a probe. Pinned separately so a future
    /// tweak that only fires for the boundary value is caught.
    #[test]
    fn breaker_recovers_on_legacy_record_far_above_threshold_without_timestamp() {
        let h = ModelHealth {
            consecutive_failures: BREAKER_THRESHOLD * 10,
            last_failure_unix: None,
            ..Default::default()
        };
        assert!(
            !h.breaker_open(),
            "a deeply tripped timestampless record must still be probe-eligible"
        );
    }

    /// Non-regression: a healthy model (no failures at all) stays closed.
    /// Without this, the new "None => false" branch could be passing only
    /// because `breaker_open_at` short-circuits before the timestamp check.
    #[test]
    fn breaker_stays_closed_on_healthy_record_without_timestamp() {
        let h = ModelHealth::default();
        assert!(!h.breaker_open());
    }

    /// Non-regression: a properly-timestamped tripped record stays OPEN
    /// during cooldown (the original behavior we MUST preserve). This is
    /// the contract `zoder-cli` and `zoder-core::router` already rely on;
    /// weakening it would silently re-introduce the original "breaker
    // always open" failure mode on every record that does have a
    /// timestamp.
    #[test]
    fn breaker_stays_open_on_tripped_record_within_cooldown() {
        let now = now_unix();
        let h = ModelHealth {
            consecutive_failures: BREAKER_THRESHOLD,
            // Recorded just now -- well within the cooldown window.
            last_failure_unix: Some(now),
            ..Default::default()
        };
        assert!(
            h.breaker_open_at(now),
            "a tripped model whose last failure is within the cooldown must stay OPEN"
        );
    }

    /// Non-regression: a properly-timestamped tripped record whose
    /// cooldown has elapsed transitions to half-open (selectable). This
    /// is the OTHER half of the original contract and pins the existing
    /// recovery path so the Z-21 fix doesn't accidentally re-pin the
    /// breaker open on every call.
    #[test]
    fn breaker_transitions_to_half_open_after_cooldown_with_timestamp() {
        let now = now_unix();
        let h = ModelHealth {
            consecutive_failures: BREAKER_THRESHOLD,
            // Recorded BREAKER_COOLDOWN_SECS + 1 seconds ago -- outside the
            // cooldown window.
            last_failure_unix: Some(now - BREAKER_COOLDOWN_SECS - 1),
            ..Default::default()
        };
        assert!(
            !h.breaker_open_at(now),
            "a tripped model past the cooldown must transition to half-open (probe-eligible)"
        );
    }

    // ---- C4-MH1: torn file + lost update ----

    /// `save()` must never leave the deterministic legacy temp path
    /// (`<stem>.json.tmp`) behind, and each writer's temp name must be unique
    /// per process + nonce so two concurrent saves cannot share a temp file
    /// (which is what allowed a torn temp to be renamed over the live store).
    #[test]
    fn save_uses_unique_temp_and_leaves_no_stray_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        let mut store = HealthStore::load(&path);
        store.record_failure("m1", "boom");
        store.save().unwrap();

        // The live store exists and round-trips.
        let reloaded = HealthStore::load(&path);
        assert_eq!(reloaded.models.get("m1").map(|h| h.failures), Some(1));

        // No leftover temp files of ANY shape (deterministic or unique) and no
        // stray legacy `.json.tmp`.
        let deterministic = path.with_extension("json.tmp");
        assert!(
            !deterministic.exists(),
            "the deterministic legacy temp path must never be used/left behind"
        );
        let mut stray_tmps = Vec::new();
        for entry in std::fs::read_dir(dir.path()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            if name.contains(".json.tmp") {
                stray_tmps.push(name);
            }
        }
        assert!(
            stray_tmps.is_empty(),
            "save() must clean up its unique temp; found strays: {stray_tmps:?}"
        );

        // Two saves in the same process pick DISTINCT nonces, so their temp
        // paths differ -- the property that prevents a torn shared temp.
        let n0 = SAVE_NONCE.load(Ordering::Relaxed);
        let t0 = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), n0));
        let t1 = path.with_extension(format!(
            "json.tmp.{}.{}",
            std::process::id(),
            n0.wrapping_add(1)
        ));
        assert_ne!(t0, t1, "consecutive save temp paths must be unique");
    }

    /// LOST UPDATE (the real C4-MH1 bug): two sequential `mutate_locked` calls
    /// on the same path, each recording a DIFFERENT model's failure, must BOTH
    /// survive on reload. The pre-fix pattern (load snapshot -> record -> save)
    /// would let the second writer's save clobber the first writer's recorded
    /// failure because it loaded a stale snapshot. `mutate_locked` reloads the
    /// freshest on-disk state under the lock before applying, so both persist.
    #[test]
    fn mutate_locked_serializes_and_does_not_lose_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");

        // Seed an on-disk store so both mutations merge onto real prior state.
        HealthStore::mutate_locked(&path, |h| {
            h.record_failure("seed", "seed");
        })
        .unwrap();

        // P1 records model A's failure.
        HealthStore::mutate_locked(&path, |h| {
            h.record_classified_failure("model-a", "a down", "prov", Classification::Error);
        })
        .unwrap();
        // P2 records a DIFFERENT model's failure. If P2 had loaded a stale
        // snapshot and saved it, model-a's failure would be dropped.
        HealthStore::mutate_locked(&path, |h| {
            h.record_classified_failure("model-b", "b down", "prov", Classification::Error);
        })
        .unwrap();

        let reloaded = HealthStore::load(&path);
        assert_eq!(
            reloaded.models.get("seed").map(|h| h.failures),
            Some(1),
            "the seed failure must survive both later mutations"
        );
        assert_eq!(
            reloaded
                .models
                .get("model-a")
                .map(|h| h.consecutive_failures),
            Some(1),
            "P1's failure must NOT be lost by P2's save (lost-update bug)"
        );
        assert_eq!(
            reloaded
                .models
                .get("model-b")
                .map(|h| h.consecutive_failures),
            Some(1),
            "P2's failure must persist"
        );
        // The lockfile is a create_new lockfile released by a Drop guard, so
        // after each mutate_locked returns it MUST be gone -- a leftover
        // lockfile would wedge the next writer until the stale timeout.
        assert!(
            !path.with_extension("lock").exists(),
            "the lockfile must be removed by the Drop guard after mutate_locked returns"
        );
    }

    /// `mutate_locked` on a fresh path (no prior store) still persists the
    /// applied delta -- the load-under-lock tolerates a missing file exactly
    /// like `HealthStore::load`.
    #[test]
    fn mutate_locked_creates_store_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("health.json");
        HealthStore::mutate_locked(&path, |h| {
            h.record_success("fresh", 12.0);
        })
        .unwrap();
        let reloaded = HealthStore::load(&path);
        assert_eq!(reloaded.models.get("fresh").map(|h| h.calls), Some(1));
    }

    // ---- C7-M1: forward-compat, no whole-store wipe on unknown token ----

    #[test]
    fn unknown_classification_token_round_trips_to_unknown_variant() {
        // A token a newer binary might write ("overloaded") must NOT raise
        // an "unknown variant" serde error; it must land on `Unknown`.
        let back: Classification =
            serde_json::from_str("\"overloaded\"").expect("unknown token must deserialize");
        assert_eq!(back, Classification::Unknown);
        // Unknown is neutral: not healthy, does not skip, has a display tag.
        assert_ne!(Classification::Unknown, Classification::Reachable);
        assert!(!Classification::Unknown.skips_consult());
        assert_eq!(Classification::Unknown.as_str(), "unknown");
    }

    #[test]
    fn load_does_not_wipe_store_on_future_classification_token() {
        // A health.json written by a NEWER binary carrying a future
        // classification token ("overloaded") must load WITHOUT wiping the
        // rest of the store. Before the `#[serde(other)]` fix this triggered
        // "unknown variant `overloaded`" -> whole-store rename+default (all
        // model history lost).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        // Two models: one with a known classification, one carrying the
        // future/unknown token. Both must survive the load.
        let json = r#"{
            "models": {
                "known-model": {
                    "calls": 7, "failures": 1, "consecutive_failures": 0,
                    "ewma_latency_ms": 42.0, "last_error": null,
                    "classification": "reachable"
                },
                "future-model": {
                    "calls": 3, "failures": 0, "consecutive_failures": 0,
                    "ewma_latency_ms": null, "last_error": null,
                    "classification": "overloaded"
                }
            }
        }"#;
        std::fs::write(&path, json).unwrap();

        let store = HealthStore::load(&path);
        // Store NOT wiped: both models survived with their counters intact.
        assert_eq!(
            store.models.len(),
            2,
            "future token must not wipe the whole store"
        );
        let known = store.models.get("known-model").expect("known survives");
        assert_eq!(known.calls, 7);
        assert_eq!(known.classification, Some(Classification::Reachable));

        let future = store.models.get("future-model").expect("future survives");
        assert_eq!(future.calls, 3);
        // The unrecognized token mapped to the neutral Unknown variant.
        assert_eq!(future.classification, Some(Classification::Unknown));

        // Breaker/skip semantics unaffected by Unknown: it is not skipped,
        // and with zero consecutive failures the breaker stays closed.
        assert!(!future.is_skipped_by_classification());
        assert!(!future.breaker_open());
        assert_eq!(future.state(), State::Healthy);

        // The original file must still be present (no rename-to-.corrupt).
        assert!(
            path.exists(),
            "load must not have renamed the store away as corrupt"
        );
    }
}
