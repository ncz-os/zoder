//! Pre-query health gate for NVIDIA EIH models
//! (`https://integrate.api.nvidia.com/v1`).
//!
//! EIH rate-limits fleet-wide: a study run saw HTTP 429 on 7.5% of reviewer
//! calls, in bursts, even at one request in flight. Querying it blindly made
//! every zoder process on every host retry into the same limiter. This gate:
//!
//! * checks a model before the first query of a run, and again after the TTL
//!   (`ZODER_EIH_HEALTH_TTL_S`, default 300s) or after any 429/5xx/timeout,
//!   with a bounded `GET {base}/models` probe (5s) that must list the model.
//!   EIH exposes no per-model status endpoint; the "Healthy" tables in fleet
//!   logs come from zoder's own `~/.zoder/health.json`, which is also where
//!   this gate keeps its state, so every process on a host shares it;
//! * keeps a per-model circuit breaker in that store: it opens on 429, 5xx,
//!   timeouts and failed probes with exponential backoff (30s doubling to
//!   15 min), never shorter than the provider's Retry-After, and goes
//!   half-open afterwards with a single probe claimed by one process;
//! * skips a gated model immediately while its breaker is open, so the caller
//!   falls to the next configured route. A skipped reviewer is never counted
//!   as a pass: it is reported in `skipped_unhealthy`, and a panel missing its
//!   member stays incomplete.
//!
//! Routing rules are unchanged: Nemotron is served only by the EIH route.

use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use zoder_core::config::Provider;
use zoder_core::health::Precheck;
use zoder_core::{Classification, HealthStore, OpenAiProvider};

/// Host of the NVIDIA EIH gateway.
pub(crate) const EIH_HOST: &str = "integrate.api.nvidia.com";
const DEFAULT_TTL_SECS: i64 = 300;
const DEFAULT_PROBE_TIMEOUT_MS: u64 = 5000;

/// Skip records for this process, drained into `skipped_unhealthy`.
static SKIPPED: Mutex<Vec<Value>> = Mutex::new(Vec::new());

/// Providers that are health-gated before every query.
pub(crate) fn is_gated(provider: &Provider) -> bool {
    provider.base_url.contains(EIH_HOST) || provider.id == "nvidia-eih"
}

fn ttl_secs() -> i64 {
    std::env::var("ZODER_EIH_HEALTH_TTL_S")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_TTL_SECS)
}

/// Test-only probe bound override (milliseconds; 0 = unset), so tests do not
/// mutate the process environment from a multi-threaded runtime.
#[cfg(test)]
pub(crate) static PROBE_TIMEOUT_OVERRIDE_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Health-probe bound (`ZODER_EIH_PROBE_TIMEOUT_MS`, default 5000).
fn probe_timeout() -> Duration {
    #[cfg(test)]
    {
        let ms = PROBE_TIMEOUT_OVERRIDE_MS.load(std::sync::atomic::Ordering::SeqCst);
        if ms > 0 {
            return Duration::from_millis(ms);
        }
    }
    Duration::from_millis(
        std::env::var("ZODER_EIH_PROBE_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(DEFAULT_PROBE_TIMEOUT_MS),
    )
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Every model this process skipped as unhealthy (drained).
pub(crate) fn take_skipped() -> Vec<Value> {
    SKIPPED
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default()
}

fn note_skip(model: &str, provider: &str, until_unix: i64, reason: &str) -> String {
    if let Ok(mut v) = SKIPPED.lock() {
        v.push(json!({
            "model": model,
            "provider": provider,
            "reason": reason,
            "retry_not_before_unix": until_unix,
        }));
    }
    let wait = until_unix.saturating_sub(now()).max(0);
    format!(
        "skipped unhealthy EIH model {model} (provider {provider}): {reason}; not querying it for \
         another {wait}s (circuit breaker); falling back to the next configured route"
    )
}

fn decide(health_path: &std::path::Path, model: &str, at: i64) -> Precheck {
    let ttl = ttl_secs();
    let mut decision = None;
    let locked = HealthStore::mutate_locked(health_path, |h| {
        let m = h.models.entry(model.to_string()).or_default();
        let d = m.precheck_at(at, ttl);
        if matches!(d, Precheck::NeedsProbe { half_open: true }) {
            m.claim_half_open_at(at);
        }
        decision = Some(d);
    });
    match (locked, decision) {
        (Ok(()), Some(d)) => d,
        // Lock unavailable: decide from a read-only snapshot (no claim).
        _ => HealthStore::load(health_path)
            .models
            .get(model)
            .map(|m| m.precheck_at(at, ttl))
            .unwrap_or(Precheck::NeedsProbe { half_open: false }),
    }
}

fn classify(error: &zoder_core::ProviderError) -> Classification {
    error
        .status
        .map(Classification::from_status)
        .unwrap_or_else(|| zoder_core::classify_err_kind(error.kind))
}

/// Record a gate failure for `model`.
pub(crate) fn record_failure(
    health_path: &std::path::Path,
    provider_id: &str,
    model: &str,
    classification: Classification,
    retry_after: Option<Duration>,
    reason: &str,
) {
    let at = now();
    let _ = HealthStore::mutate_locked(health_path, |h| {
        h.models
            .entry(model.to_string())
            .or_default()
            .record_gate_failure_at(
                at,
                provider_id,
                classification,
                retry_after.map(|d| d.as_secs()),
                reason,
            );
    });
}

/// Record a healthy check / successful call for `model`.
pub(crate) fn record_success(health_path: &std::path::Path, provider_id: &str, model: &str) {
    let at = now();
    let _ = HealthStore::mutate_locked(health_path, |h| {
        h.models
            .entry(model.to_string())
            .or_default()
            .record_gate_success_at(at, provider_id);
    });
}

/// Record the outcome of a real call to a gated model. Only failures that
/// mean "the model is not answering" open the breaker (429, 5xx, timeout,
/// transport); a 4xx caused by the request itself does not.
pub(crate) fn record_call_error(
    health_path: &std::path::Path,
    provider_id: &str,
    model: &str,
    error: &zoder_core::ProviderError,
) {
    use zoder_core::ErrKind;
    if matches!(
        error.kind,
        ErrKind::RateLimit | ErrKind::Server | ErrKind::Timeout | ErrKind::Network
    ) {
        record_failure(
            health_path,
            provider_id,
            model,
            classify(error),
            error.retry_after,
            &error.message,
        );
    }
}

/// Decide whether `model` may be queried now. `Ok` carries the `health`
/// object for receipts; `Err` carries the skip message (already recorded in
/// `skipped_unhealthy`).
pub(crate) async fn admit(
    health_path: &std::path::Path,
    provider_cfg: &Provider,
    provider: &OpenAiProvider,
    model: &str,
) -> Result<Value, String> {
    let at = now();
    match decide(health_path, model, at) {
        Precheck::Fresh { checked_at_unix } => Ok(json!({
            "state": "healthy",
            "source": "cache",
            "checked_at_unix": checked_at_unix,
        })),
        Precheck::Skip { until_unix, reason } => {
            Err(note_skip(model, &provider_cfg.id, until_unix, &reason))
        }
        Precheck::NeedsProbe { half_open } => {
            let bound = probe_timeout();
            let outcome = tokio::time::timeout(bound, provider.list_models()).await;
            let failure: Option<(Classification, Option<Duration>, String)> = match outcome {
                Ok(Ok(ids)) if ids.iter().any(|id| id == model) => None,
                Ok(Ok(_)) => Some((
                    Classification::Unprovisioned,
                    None,
                    format!("health probe: {model} is not listed by the gateway's /models"),
                )),
                Ok(Err(e)) => Some((
                    classify(&e),
                    e.retry_after,
                    format!("health probe failed: {}", e.message),
                )),
                Err(_) => Some((
                    Classification::Error,
                    None,
                    format!("health probe timed out after {}ms", bound.as_millis()),
                )),
            };
            match failure {
                None => {
                    record_success(health_path, &provider_cfg.id, model);
                    Ok(json!({
                        "state": "healthy",
                        "source": if half_open { "half_open_probe" } else { "probe" },
                        "checked_at_unix": now(),
                    }))
                }
                Some((class, retry_after, reason)) => {
                    record_failure(
                        health_path,
                        &provider_cfg.id,
                        model,
                        class,
                        retry_after,
                        &reason,
                    );
                    let until = HealthStore::load(health_path)
                        .models
                        .get(model)
                        .and_then(|m| m.gate.as_ref())
                        .and_then(|g| g.retry_not_before_unix)
                        .unwrap_or(at);
                    Err(note_skip(model, &provider_cfg.id, until, &reason))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, base_url: &str) -> Provider {
        Provider {
            id: id.into(),
            engine_provider_ref: None,
            base_url: base_url.into(),
            kind: "openai-chat".into(),
            auth: zoder_core::config::Auth::None,
            paid: false,
            billing: zoder_core::BillingMode::Free,
            subscription: None,
            serves: vec![],
            azure_api_version: None,
        }
    }

    /// Only the EIH route is gated; the local routes (TYDEUS qwen38,
    /// CERBERUS gemma) and other providers are not.
    #[test]
    fn only_eih_routes_are_gated() {
        assert!(is_gated(&provider(
            "nvidia-eih",
            "https://integrate.api.nvidia.com/v1"
        )));
        assert!(is_gated(&provider(
            "eih-other-id",
            "https://integrate.api.nvidia.com/v1"
        )));
        assert!(!is_gated(&provider(
            "tydeus-coder",
            "http://192.168.207.73:8006/v1"
        )));
        assert!(!is_gated(&provider(
            "cerberus-reviewer",
            "http://192.168.207.96:8080/v1"
        )));
        assert!(!is_gated(&provider(
            "minimax-m3",
            "https://api.minimax.io/v1"
        )));
    }
}
