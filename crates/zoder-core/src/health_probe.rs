//! Multi-provider model-health probe.
//!
//! Splits the `--probe --all` sweep into two layers so the heavy logic is
//! testable without a network:
//!
//! 1. The pure classification + iteration helpers in this module accept a
//!    list of providers and a [`ProbeResolver`]. They build the target list
//!    (live `list_models()` if available, otherwise the provider's declared
//!    model fallback), call `ping()` per model, and stamp the result into a
//!    [`HealthStore`] via [`HealthStore::record_classified_success`] /
//!    [`HealthStore::record_classified_failure`].
//! 2. The CLI wires `OpenAiProvider` to this trait; tests pass a mock.
//!
//! Classification is driven by [`Classification::from_status`] when the
//! provider surfaces an HTTP code and by [`classify_err`] when only the
//! typed `ProviderError` is available. Both produce a `Classification` that
//! the persisted store keeps on the model record so `consult` can show
//! freshness and skip Capacity/Unprovisioned entries without re-pinging.

use crate::config::Provider;
use crate::health::{Classification, HealthStore};
use crate::provider::{ChatRequest, ChatResult, ProviderError};
use std::time::Instant;

/// Abstract "ping a model on a provider". Production wires this to
/// `OpenAiProvider::stream_chat` with a tiny completion; tests inject a
/// mock so the iteration logic can be exercised without a network.
///
/// `list_models` is optional: a provider that can't be introspected (or
/// doesn't expose a `/models` route) returns `None`, and the probe falls
/// back to a configured-model set.
pub trait Probe: Send + Sync {
    /// Fetch the live model ids the provider currently serves. `None` when
    /// the provider doesn't expose a models endpoint or the call failed —
    /// the iteration helper then falls back to the provider's declared
    /// model fallback.
    fn list_models(&self) -> Option<Vec<String>>;
    /// Tiny completion ping; returns either the chat result or a typed
    /// `ProviderError` carrying the HTTP status code when known.
    fn ping(&self, model_id: &str) -> Result<ChatResult, ProviderError>;
}

/// Cheap default probe payload: 8 completion tokens is enough for the
/// provider to validate auth + model id + a streaming round-trip without
/// costing more than a fraction of a cent on a metered endpoint.
pub const PROBE_MAX_TOKENS: u32 = 8;

/// Default ping prompt. Kept tiny and neutral so it works for chat-tuned
/// models without provoking content filters.
pub const PROBE_PROMPT: &str = "ping";

/// Per-ping wall-clock cap for the `--probe --all` sweep. A single
/// misbehaving endpoint must never be able to wedge the whole daily
/// launchd/systemd job: when the timeout elapses the ping is recorded as
/// a classified `Error` and the sweep moves on to the next model.
pub const PROBE_PING_TIMEOUT_SECS: u64 = 20;

/// Upper bound on how many models one provider may ping in a single
/// `--probe --all` run. Endpoints like EIH expose hundreds of model ids;
/// a daily sweep has no business exercising all of them. When the cap
/// drops entries the operator sees a logged NOTE — the cap is never
/// silent.
pub const PROBE_MAX_MODELS_PER_PROVIDER: usize = 50;

/// Truncate a per-provider target list to at most `max` entries and
/// report how many were dropped. The pure helper is kept in the core
/// module (no I/O, no async) so the cap policy can be unit-tested without
/// touching a network.
///
/// Returns `(kept, dropped)`. When the input is shorter than `max` the
/// original vector is returned unchanged and `dropped` is 0 — i.e. no
/// truncation occurred and callers should NOT emit a "capped: …" note.
pub fn cap_targets(targets: Vec<String>, max: usize) -> (Vec<String>, usize) {
    if targets.len() <= max {
        return (targets, 0);
    }
    let dropped = targets.len() - max;
    (targets.into_iter().take(max).collect(), dropped)
}

/// Build a `ChatRequest` for the tiny probe ping.
pub fn probe_request(model_id: &str) -> ChatRequest {
    ChatRequest {
        model: model_id.to_string(),
        messages: vec![crate::provider::Message::new("user", PROBE_PROMPT)],
        max_tokens: PROBE_MAX_TOKENS,
        temperature: Some(0.0),
        stream: false,
        show_reasoning: false,
        reasoning_effort: None,
    }
}

/// Outcome of probing a single model, returned by [`probe_all`]. The CLI
/// prints one row per outcome; tests assert against the per-row fields.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProbeOutcome {
    pub provider_id: String,
    pub model_id: String,
    pub latency_ms: Option<f64>,
    pub classification: Classification,
    pub note: Option<String>,
}

/// Map a typed `ProviderError` to the classification we'll record. The HTTP
/// status (when present) wins; the `ErrKind` decides only for the
/// status-less cases (e.g. `RateLimit` shows up without a status code on
/// some backends).
pub fn classify_err(err: &ProviderError) -> Classification {
    if let Some(code) = err.status {
        let from_status = Classification::from_status(code);
        if from_status != Classification::Error {
            return from_status;
        }
    }
    classify_err_kind(err.kind)
}

/// Map a bare `ErrKind` to a classification. Kept separate so the CLI can
/// classify the result of `list_models()` the same way it classifies a
/// chat ping.
pub fn classify_err_kind(kind: crate::provider::ErrKind) -> Classification {
    use crate::provider::ErrKind;
    match kind {
        // The breaker's signal: timeouts / decode / network / generic
        // server. These should NOT cause consult to skip a model — the
        // breaker already handles repeated failures. Recorded as `Error`.
        ErrKind::Timeout | ErrKind::Network | ErrKind::Decode | ErrKind::Server | ErrKind::Http => {
            Classification::Error
        }
        // No-status RateLimit: treat as Capacity so consult skips it until
        // the next probe.
        ErrKind::RateLimit => Classification::Capacity,
    }
}

/// A row of the per-provider report. `targets` is the live model id set
/// the iteration chose for the provider (live `list_models()` or fallback).
pub struct ProbePlan<'a> {
    pub provider_id: &'a str,
    pub targets: Vec<String>,
}

/// Build the per-provider target lists. Iterates every non-placeholder
/// provider in `providers`, calls `list_models()` (skipped silently on
/// failure), and falls back to the provider's declared `model_ids`
/// alternative when introspection isn't available. This is the "what
/// should we ping?" half of the sweep.
pub fn build_probe_plan<'a>(
    providers: &'a [Provider],
    prober: &dyn Fn(&Provider) -> anyhow::Result<Box<dyn Probe>>,
) -> Vec<ProbePlan<'a>> {
    let mut plans = Vec::new();
    for p in providers {
        if p.base_url
            .contains(crate::config::PLACEHOLDER_PROVIDER_HOST)
        {
            continue;
        }
        // Prober failure (e.g. invalid URL) -> skip with no targets so the
        // caller doesn't try to ping anything.
        let probe = match prober(p) {
            Ok(pr) => pr,
            Err(_) => continue,
        };
        let targets = probe
            .list_models()
            .filter(|v| !v.is_empty())
            // Fallback: a provider that doesn't expose a models route still
            // gets its `id` itself as a probe target — useful for catching
            // misconfigured endpoints where the only "model" is a test id.
            .unwrap_or_else(|| vec![p.id.clone()]);
        plans.push(ProbePlan {
            provider_id: &p.id,
            targets,
        });
    }
    plans
}

/// Abstract resolver: given a provider id, return the `Probe` to use.
/// Production holds a `HashMap<String, OpenAiProvider>` and returns
/// `Some(&dyn_probe)` (where `dyn_probe` borrows from the map). Tests
/// hold a `HashMap<String, Box<dyn Probe>>` and return `Some(&*boxed)`.
/// Returning `None` means the prober can't be built for this provider
/// right now — every target gets recorded as `Error`.
pub trait ProbeResolver {
    fn resolve<'a>(&'a self, provider_id: &'a str) -> Option<&'a dyn Probe>;
}

/// Run the sweep. For each plan: ping every target, classify the result,
/// stamp it into `store`, and return one [`ProbeOutcome`] per ping. The CLI
/// renders the outcome list as the per-provider report.
pub fn probe_all<'a>(
    plans: Vec<ProbePlan<'a>>,
    resolver: &dyn ProbeResolver,
    store: &mut HealthStore,
) -> Vec<ProbeOutcome> {
    let mut out = Vec::new();
    for plan in plans {
        match resolver.resolve(plan.provider_id) {
            None => {
                // The provider is in the config but the prober rejected it
                // (bad URL, missing auth, etc.). Record every target as
                // Error so the operator can see why nothing was pinged.
                for model_id in &plan.targets {
                    store.record_classified_failure(
                        model_id,
                        "prober unavailable",
                        plan.provider_id,
                        Classification::Error,
                    );
                    out.push(ProbeOutcome {
                        provider_id: plan.provider_id.to_string(),
                        model_id: model_id.clone(),
                        latency_ms: None,
                        classification: Classification::Error,
                        note: Some("prober unavailable".into()),
                    });
                }
            }
            Some(probe) => {
                for model_id in &plan.targets {
                    let t = Instant::now();
                    let result = probe.ping(model_id);
                    let elapsed_ms = t.elapsed().as_millis() as f64;
                    let outcome = match result {
                        Ok(_) => {
                            store.record_classified_success(
                                model_id,
                                elapsed_ms,
                                plan.provider_id,
                                Classification::Reachable,
                            );
                            ProbeOutcome {
                                provider_id: plan.provider_id.to_string(),
                                model_id: model_id.clone(),
                                latency_ms: Some(elapsed_ms),
                                classification: Classification::Reachable,
                                note: None,
                            }
                        }
                        Err(err) => {
                            let cls = classify_err(&err);
                            let note = err.message.clone();
                            store.record_classified_failure(
                                model_id,
                                &err.message,
                                plan.provider_id,
                                cls,
                            );
                            ProbeOutcome {
                                provider_id: plan.provider_id.to_string(),
                                model_id: model_id.clone(),
                                latency_ms: None,
                                classification: cls,
                                note: Some(note),
                            }
                        }
                    };
                    out.push(outcome);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Auth, BillingMode, Provider};
    use crate::provider::{ChatRequest, ChatResult, ErrKind, ProviderError};
    use std::sync::Mutex;

    fn provider(id: &str, base_url: &str) -> Provider {
        Provider {
            id: id.into(),
            base_url: base_url.into(),
            kind: "openai-chat".into(),
            auth: Auth::None,
            paid: false,
            billing: BillingMode::Free,
            subscription: None,
            serves: Vec::new(),
        }
    }

    /// Mock probe with a configurable per-model outcome queue.
    struct MockProbe {
        models: Option<Vec<String>>,
        outcomes: Mutex<Vec<(String, Result<ChatResult, ProviderError>)>>,
    }

    impl MockProbe {
        fn new(models: Option<Vec<String>>) -> Self {
            Self {
                models,
                outcomes: Mutex::new(Vec::new()),
            }
        }
        fn enqueue(&self, model: &str, outcome: Result<ChatResult, ProviderError>) {
            self.outcomes.lock().unwrap().push((model.into(), outcome));
        }
    }

    impl Probe for MockProbe {
        fn list_models(&self) -> Option<Vec<String>> {
            self.models.clone()
        }
        fn ping(&self, model_id: &str) -> Result<ChatResult, ProviderError> {
            let mut q = self.outcomes.lock().unwrap();
            // Pop the first queued outcome whose model matches. If the
            // queue is empty or no entry matches, return a benign Ok so a
            // single enqueue can drive a single assertion without each
            // test having to enqueue every model.
            let pos = match q.iter().position(|(m, _)| m == model_id) {
                Some(i) => i,
                None => return Ok(ChatResult::default()),
            };
            let (_, outcome) = q.remove(pos);
            outcome
        }
    }

    fn provider_to_mock(p: &Provider) -> anyhow::Result<Box<dyn Probe>> {
        Ok(match p.id.as_str() {
            "openrouter" => Box::new(MockProbe::new(Some(vec![
                "openai/gpt-4o".into(),
                "anthropic/claude-3.7".into(),
            ]))),
            "nvidia-eih" => Box::new(MockProbe::new(Some(vec!["nvidia/llama-3.3".into()]))),
            "unsupported" => Box::new(MockProbe::new(None)),
            _ => return Err(anyhow::anyhow!("unknown provider in fixture")),
        })
    }

    fn store() -> HealthStore {
        HealthStore::default()
    }

    #[test]
    fn build_probe_plan_skips_placeholder_provider() {
        let providers = vec![
            provider("openrouter", "https://api.real.example/v1"),
            // PLACEHOLDER_PROVIDER_HOST must be skipped, regardless of
            // whether the prober can introspect it.
            provider("placeholder", "https://api.example.com/v1"),
        ];
        let plans = build_probe_plan(&providers, &provider_to_mock);
        assert_eq!(plans.len(), 1, "placeholder must be skipped");
        assert_eq!(plans[0].provider_id, "openrouter");
    }

    #[test]
    fn build_probe_plan_uses_fallback_when_list_models_returns_none() {
        // A provider that can't introspect still gets at least one probe
        // target (its own id) so a misconfigured endpoint surfaces as a
        // ping failure instead of silently passing.
        let providers = vec![provider("unsupported", "https://gw.example/v1")];
        let plans = build_probe_plan(&providers, &provider_to_mock);
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].provider_id, "unsupported");
        assert_eq!(plans[0].targets, vec!["unsupported".to_string()]);
    }

    #[test]
    fn build_probe_plan_uses_fallback_when_list_models_returns_empty() {
        struct EmptyListProbe;
        impl Probe for EmptyListProbe {
            fn list_models(&self) -> Option<Vec<String>> {
                Some(vec![])
            }
            fn ping(&self, _: &str) -> Result<ChatResult, ProviderError> {
                Ok(ChatResult::default())
            }
        }
        let prober =
            |_: &Provider| -> anyhow::Result<Box<dyn Probe>> { Ok(Box::new(EmptyListProbe)) };
        let providers = vec![provider("p", "https://gw.example/v1")];
        let plans = build_probe_plan(&providers, &prober);
        assert_eq!(plans[0].targets, vec!["p".to_string()]);
    }

    #[test]
    fn build_probe_plan_skips_provider_when_prober_returns_err() {
        let providers = vec![provider("broken", "https://gw.example/v1")];
        let prober =
            |_: &Provider| -> anyhow::Result<Box<dyn Probe>> { Err(anyhow::anyhow!("bad url")) };
        let plans = build_probe_plan(&providers, &prober);
        assert!(plans.is_empty(), "prober failure -> skip silently");
    }

    #[test]
    fn probe_all_iterates_every_provider_and_every_target() {
        let providers = vec![
            provider("openrouter", "https://openrouter/v1"),
            provider("nvidia-eih", "https://nvidia/v1"),
        ];
        let plans = build_probe_plan(&providers, &provider_to_mock);

        // Build fresh mocks that own their outcome queues so we can inject
        // a 404, an Ok, and a 429 across the three targets.
        let openrouter = MockProbe::new(Some(vec![
            "openai/gpt-4o".into(),
            "anthropic/claude-3.7".into(),
        ]));
        openrouter.enqueue(
            "openai/gpt-4o",
            Err(ProviderError {
                message: "404".into(),
                kind: ErrKind::Http,
                status: Some(404),
                retry_after: None,
                emitted: false,
            }),
        );
        openrouter.enqueue("anthropic/claude-3.7", Ok(ChatResult::default()));

        let nvidia = MockProbe::new(Some(vec!["nvidia/llama-3.3".into()]));
        nvidia.enqueue(
            "nvidia/llama-3.3",
            Err(ProviderError {
                message: "429".into(),
                kind: ErrKind::RateLimit,
                status: Some(429),
                retry_after: None,
                emitted: false,
            }),
        );

        let map: std::collections::HashMap<String, Box<dyn Probe>> = [
            (
                "openrouter".to_string(),
                Box::new(openrouter) as Box<dyn Probe>,
            ),
            ("nvidia-eih".to_string(), Box::new(nvidia) as Box<dyn Probe>),
        ]
        .into_iter()
        .collect();
        struct MapResolver(std::collections::HashMap<String, Box<dyn Probe>>);
        impl ProbeResolver for MapResolver {
            fn resolve<'a>(&'a self, provider_id: &'a str) -> Option<&'a dyn Probe> {
                self.0.get(provider_id).map(|b| b.as_ref() as &dyn Probe)
            }
        }
        let resolver = MapResolver(map);

        let mut s = store();
        let outcomes = probe_all(plans, &resolver, &mut s);

        // Every (provider, model) pair produced an outcome.
        assert_eq!(outcomes.len(), 3);
        let by_model: std::collections::HashMap<(&str, &str), Classification> = outcomes
            .iter()
            .map(|o| {
                (
                    (o.provider_id.as_str(), o.model_id.as_str()),
                    o.classification,
                )
            })
            .collect();
        assert_eq!(
            by_model[&("openrouter", "openai/gpt-4o")],
            Classification::Unprovisioned
        );
        assert_eq!(
            by_model[&("openrouter", "anthropic/claude-3.7")],
            Classification::Reachable
        );
        assert_eq!(
            by_model[&("nvidia-eih", "nvidia/llama-3.3")],
            Classification::Capacity
        );

        // The store now stamps every model with provider_id + classification.
        let gpt = &s.models["openai/gpt-4o"];
        assert_eq!(gpt.provider_id.as_deref(), Some("openrouter"));
        assert_eq!(gpt.classification, Some(Classification::Unprovisioned));
        assert!(gpt.is_skipped_by_classification());
        assert!(gpt.checked_at_unix.is_some());

        let nvidia_llama = &s.models["nvidia/llama-3.3"];
        assert_eq!(nvidia_llama.provider_id.as_deref(), Some("nvidia-eih"));
        assert_eq!(nvidia_llama.classification, Some(Classification::Capacity));
        assert!(nvidia_llama.is_skipped_by_classification());
        assert_eq!(nvidia_llama.failures, 1);

        let claude = &s.models["anthropic/claude-3.7"];
        assert_eq!(claude.classification, Some(Classification::Reachable));
        assert!(!claude.is_skipped_by_classification());
    }

    #[test]
    fn probe_all_records_error_when_provider_prober_unavailable() {
        // Plans built for a provider whose runtime prober can't be
        // constructed (e.g. auth missing at probe time). The loop must
        // still surface every target as an Error outcome.
        struct PingOk;
        impl Probe for PingOk {
            fn list_models(&self) -> Option<Vec<String>> {
                Some(vec!["m1".into()])
            }
            fn ping(&self, _: &str) -> Result<ChatResult, ProviderError> {
                Ok(ChatResult::default())
            }
        }
        let providers = vec![provider("p", "https://gw.example/v1")];
        let plans = build_probe_plan(&providers, &|_| Ok(Box::new(PingOk)));
        // Now pretend the runtime lookup fails (auth gone, etc.).
        struct NoneResolver;
        impl ProbeResolver for NoneResolver {
            fn resolve<'a>(&'a self, _: &'a str) -> Option<&'a dyn Probe> {
                None
            }
        }
        let resolver = NoneResolver;
        let mut s = store();
        let outcomes = probe_all(plans, &resolver, &mut s);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].classification, Classification::Error);
        assert_eq!(
            outcomes[0].note.as_deref(),
            Some("prober unavailable"),
            "the failure reason must round-trip through the outcome"
        );
        let h = &s.models["m1"];
        assert_eq!(h.classification, Some(Classification::Error));
    }

    #[test]
    fn classify_err_prefers_status_over_kind() {
        // A status-bearing RateLimit (429) classifies as Capacity via the
        // status, not the kind's RateLimit branch.
        let e = ProviderError {
            message: "boom".into(),
            kind: ErrKind::RateLimit,
            status: Some(429),
            retry_after: None,
            emitted: false,
        };
        assert_eq!(classify_err(&e), Classification::Capacity);

        // Without a status, RateLimit still resolves to Capacity.
        let e = ProviderError {
            message: "boom".into(),
            kind: ErrKind::RateLimit,
            status: None,
            retry_after: None,
            emitted: false,
        };
        assert_eq!(classify_err(&e), Classification::Capacity);

        // Network error without status -> generic Error (breaker handles).
        let e = ProviderError {
            message: "boom".into(),
            kind: ErrKind::Network,
            status: None,
            retry_after: None,
            emitted: false,
        };
        assert_eq!(classify_err(&e), Classification::Error);

        // 500 server error with Server kind -> Error (not Capacity).
        let e = ProviderError {
            message: "boom".into(),
            kind: ErrKind::Server,
            status: Some(500),
            retry_after: None,
            emitted: false,
        };
        assert_eq!(classify_err(&e), Classification::Error);
    }

    #[test]
    fn probe_request_is_tiny_and_neutral() {
        // The probe payload must be tiny (8 tokens) so a daily sweep
        // across hundreds of models costs ~zero on a metered endpoint,
        // and the prompt must be neutral so a content-filtered model
        // doesn't refuse it.
        let req = probe_request("test-model");
        assert_eq!(req.max_tokens, 8);
        assert_eq!(req.model, "test-model");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].content, "ping");
        assert!(!req.stream);
        assert_eq!(req.temperature, Some(0.0));
    }

    // Suppress unused warnings for items used only in some tests.
    #[allow(dead_code)]
    fn _suppress_unused(_: &ChatRequest) {}

    #[test]
    fn cap_targets_truncates_and_reports_dropped() {
        // Build an 11-entry target list (more than the 5 we will ask for)
        // and confirm cap_targets returns exactly the first 5 + drops 6.
        let targets: Vec<String> = (0..11).map(|i| format!("m{i}")).collect();
        let (kept, dropped) = cap_targets(targets, 5);
        assert_eq!(kept.len(), 5, "must cap to max=5");
        assert_eq!(dropped, 6, "must report the 6 entries that were dropped");
        assert_eq!(
            kept,
            vec![
                "m0".to_string(),
                "m1".to_string(),
                "m2".to_string(),
                "m3".to_string(),
                "m4".to_string(),
            ],
            "cap must keep the prefix, not arbitrary entries"
        );

        // Asking for the same size as the input (max == len) is a no-op
        // and must report dropped == 0 so the operator never sees a
        // misleading "capped: …" note for an under-cap run.
        let targets: Vec<String> = (0..11).map(|i| format!("m{i}")).collect();
        let (kept, dropped) = cap_targets(targets.clone(), 50);
        assert_eq!(kept, targets, "max larger than input must be a no-op");
        assert_eq!(dropped, 0, "no truncation -> dropped must be 0");
    }

    #[test]
    #[allow(clippy::assertions_on_constants)] // deliberate range guards on config constants
    fn probe_ping_timeout_is_bounded() {
        // The per-ping cap must be a positive number of seconds and must
        // not exceed a minute — anything larger risks undoing the
        // hardening and re-introducing the original "one hanging
        // endpoint blocks the whole sweep" failure mode for the daily
        // launchd/systemd job.
        assert!(
            PROBE_PING_TIMEOUT_SECS > 0,
            "PROBE_PING_TIMEOUT_SECS must be > 0"
        );
        assert!(
            PROBE_PING_TIMEOUT_SECS <= 60,
            "PROBE_PING_TIMEOUT_SECS must be <= 60 (got {})",
            PROBE_PING_TIMEOUT_SECS
        );

        // The per-provider model cap must be positive (otherwise the
        // sweep pings nothing) and must not exceed 200 — otherwise EIH
        // and similar broad catalogs still dominate the daily run.
        assert!(
            PROBE_MAX_MODELS_PER_PROVIDER > 0,
            "PROBE_MAX_MODELS_PER_PROVIDER must be > 0"
        );
        assert!(
            PROBE_MAX_MODELS_PER_PROVIDER <= 200,
            "PROBE_MAX_MODELS_PER_PROVIDER must be <= 200 (got {})",
            PROBE_MAX_MODELS_PER_PROVIDER
        );
    }
}
