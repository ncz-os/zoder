//! Selection for an explicitly configured secondary reviewer pool.
//! Eligibility is earned by a successful exact-provider health probe, never
//! inferred from the model catalog. Operators must refresh probes at least hourly.

use zoder_core::{Classification, HealthStore};

pub(crate) const MAX_HEALTH_AGE_SECS: i64 = 3600;

/// Keep only recently verified healthy routes, ordered by observed EWMA latency.
/// Stable ties retain the configured order. Unknown/stale/failed routes stay out
/// until an explicit health probe succeeds; no arbitrary fallback is introduced.
pub(crate) fn healthy_models(
    routes: &[(String, String)],
    health: &HealthStore,
    now: i64,
) -> Vec<String> {
    let mut eligible: Vec<(&str, f64)> = routes
        .iter()
        .filter_map(|(provider, model)| {
            let record = health.models.get(model)?;
            let checked = record.checked_at_unix?;
            let latency = record.ewma_latency_ms?;
            if record.provider_id.as_deref() != Some(provider.as_str())
                || record.classification != Some(Classification::Reachable)
                || record.consecutive_failures != 0
                || record.breaker_open()
                || record.is_skipped_by_classification()
                || checked > now
                || now.saturating_sub(checked) >= MAX_HEALTH_AGE_SECS
                || !latency.is_finite()
                || latency < 0.0
            {
                return None;
            }
            Some((model.as_str(), latency))
        })
        .collect();
    eligible.sort_by(|a, b| a.1.total_cmp(&b.1));
    eligible
        .into_iter()
        .map(|(model, _)| model.to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(health: &mut HealthStore, model: &str, latency: f64) {
        health.record_classified_success(
            model,
            latency,
            "review-provider",
            Classification::Reachable,
        );
        health.models.get_mut(model).unwrap().checked_at_unix = Some(1000);
    }

    #[test]
    fn healthy_pool_ranks_latency_with_stable_ties() {
        let mut health = HealthStore::default();
        record(&mut health, "slow", 200.0);
        record(&mut health, "fast", 20.0);
        record(&mut health, "tie", 20.0);
        let routes = ["slow", "fast", "tie"].map(|model| ("review-provider".into(), model.into()));
        assert_eq!(
            healthy_models(&routes, &health, 1001),
            ["fast", "tie", "slow"]
        );
    }

    #[test]
    fn unknown_stale_wrong_provider_and_failed_models_are_excluded() {
        let mut health = HealthStore::default();
        for model in [
            "stale",
            "wrong",
            "capacity",
            "broken",
            "unknown-class",
            "future",
            "nan",
        ] {
            record(&mut health, model, 20.0);
        }
        health.models.get_mut("stale").unwrap().checked_at_unix = Some(0);
        health.models.get_mut("wrong").unwrap().provider_id = Some("coder-provider".into());
        health.models.get_mut("capacity").unwrap().classification = Some(Classification::Capacity);
        health
            .models
            .get_mut("broken")
            .unwrap()
            .consecutive_failures = 3;
        health
            .models
            .get_mut("unknown-class")
            .unwrap()
            .classification = Some(Classification::Unknown);
        health.models.get_mut("future").unwrap().checked_at_unix = Some(5000);
        health.models.get_mut("nan").unwrap().ewma_latency_ms = Some(f64::NAN);
        let routes = [
            "missing",
            "stale",
            "wrong",
            "capacity",
            "broken",
            "unknown-class",
            "future",
            "nan",
        ]
        .map(|model| ("review-provider".into(), model.into()));
        assert!(healthy_models(&routes, &health, 4000).is_empty());
    }

    #[test]
    fn failed_pool_member_requires_successful_probe_to_recover() {
        let mut health = HealthStore::default();
        record(&mut health, "model", 20.0);
        health.record_classified_failure(
            "model",
            "503",
            "review-provider",
            Classification::Capacity,
        );
        let routes = [("review-provider".into(), "model".into())];
        assert!(healthy_models(&routes, &health, 1001).is_empty());
        record(&mut health, "model", 30.0);
        assert_eq!(healthy_models(&routes, &health, 1001), ["model"]);
    }
}
