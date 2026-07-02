//! Smart router: pick the best FREE model for a task by capability x latency x
//! live health, with a deterministic cross-family fallback chain.

use crate::corpus::{Corpus, ModelEntry};
use crate::health::HealthStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// High-frequency agentic loops: optimize latency/throughput first.
    Fast,
    /// Hard reasoning/codegen: optimize capability (SWE) first.
    Strong,
    /// Balanced default: the composite agentic score.
    Auto,
    /// One-shot agentic authoring (`zoder oneshot`/`exec`): prefer models the
    /// known-good list rates well for single-pass code generation.
    SinglePass,
    /// Adversarial grind-until-green loop (`zoder loop`): prefer models proven to
    /// CONVERGE on iterative runtime-failure debugging — single-pass quality does
    /// not imply it (a strong author can still stall in a grind).
    Grind,
}

impl Tier {
    pub fn parse(s: &str) -> Tier {
        match s.to_ascii_lowercase().as_str() {
            "fast" => Tier::Fast,
            "strong" | "codex" => Tier::Strong,
            "single-pass" | "singlepass" | "single" | "oneshot" => Tier::SinglePass,
            "grind" | "loop" => Tier::Grind,
            _ => Tier::Auto,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Route {
    pub primary: String,
    pub fallbacks: Vec<String>,
    pub reason: String,
}

pub struct Router<'a> {
    corpus: &'a Corpus,
    health: &'a HealthStore,
    /// Optional operator-pinned primary model id (`Config.primary_model`). When
    /// set, it always leads the chain and the capability/health-ranked free
    /// pool becomes the fallback chain behind it.
    pinned_primary: Option<String>,
    /// Optional set of model ids that a REAL (non-placeholder) provider serves
    /// on this host (`Config::model_has_real_provider`). When present, the
    /// auto-pick pool is filtered to these so the router never selects a
    /// free-pool model that would fall through to the `api.example.com`
    /// placeholder default and fail cryptically. `None` = no filter (legacy
    /// behavior for callers without a config in hand).
    backed: Option<std::collections::HashSet<String>>,
}

impl<'a> Router<'a> {
    pub fn new(corpus: &'a Corpus, health: &'a HealthStore) -> Self {
        Self {
            corpus,
            health,
            pinned_primary: None,
            backed: None,
        }
    }

    /// Pin a primary model (from `Config.primary_model`) to lead the chain.
    pub fn with_primary(mut self, primary: Option<String>) -> Self {
        self.pinned_primary = primary.filter(|s| !s.trim().is_empty());
        self
    }

    /// Restrict the auto-pick pool to models a real provider serves on this
    /// host. Pass `None` to disable filtering. The pinned primary is exempt
    /// (an operator pin is honored even if it is a subscription model outside
    /// the free pool); a bad pin still surfaces at call-site provider
    /// resolution as a clear error, not a silent `example.com` dial.
    pub fn with_backed(mut self, backed: Option<std::collections::HashSet<String>>) -> Self {
        // NOTE: an *empty* Some(set) is a real signal ("no free model has a
        // configured provider on this host") and MUST filter the pool to empty
        // — do NOT collapse it to None. None means "caller has no config info,
        // don't filter" (legacy). The distinction is what makes an unconfigured
        // host fail cleanly instead of auto-picking an example.com-bound model.
        self.backed = backed;
        self
    }

    fn rank_key(m: &ModelEntry, tier: Tier) -> f64 {
        // A measured multi-source coding score (0..100 -> 0..1) is more
        // trustworthy than the arena-derived `w_swe`/`agentic_score` weights
        // (anchored 0.5..1.0), but the two are not on the same scale. So we
        // *band*: any model with a real benchmark (+1.0) outranks one with only
        // an inferred weight, and within each band they sort on their own metric.
        let cap = m.code_capability().map(|c| c / 100.0);
        match tier {
            // Latency-first; capability only breaks ties (tiny nudge).
            Tier::Fast => {
                let base = m.latency_score.or(m.agentic_score).unwrap_or(0.0);
                base + cap.map(|c| c * 1e-3).unwrap_or(0.0)
            }
            // Capability-first: real bench band, else arena weight.
            Tier::Strong => match cap {
                Some(c) => 1.0 + c,
                None => m.w_swe.or(m.agentic_score).unwrap_or(0.0),
            },
            // Balanced: blend real capability with latency inside the bench band.
            Tier::Auto => match (cap, m.latency_score) {
                (Some(c), Some(l)) => 1.0 + 0.6 * c + 0.4 * l,
                (Some(c), None) => 1.0 + c,
                (None, _) => m.agentic_score.or(m.w_swe).unwrap_or(0.0),
            },
            // Workflow-first: a model the known-good list rates for THIS workflow
            // (top band) outranks one with only a measured capability, which
            // outranks one with only an inferred weight.
            Tier::SinglePass => {
                Self::workflow_rank(m, m.workflows.as_ref().and_then(|w| w.single_pass), cap)
            }
            Tier::Grind => Self::workflow_rank(m, m.workflows.as_ref().and_then(|w| w.grind), cap),
        }
    }

    /// Banded rank for the workflow tiers: curated workflow score (2.0+) beats a
    /// measured capability (1.0+) beats an inferred agentic/arena weight.
    fn workflow_rank(m: &ModelEntry, wf: Option<f64>, cap: Option<f64>) -> f64 {
        match wf {
            Some(w) => 2.0 + w,
            None => match cap {
                Some(c) => 1.0 + c,
                None => m.agentic_score.or(m.w_swe).unwrap_or(0.0),
            },
        }
    }

    /// Ordered free candidates for a tier, skipping open circuit breakers.
    /// A half-open breaker (cooldown elapsed) is selectable so models recover.
    fn candidates(&self, tier: Tier) -> Vec<&ModelEntry> {
        // Precompute the rank key once per model instead of 3x per comparison.
        let mut keyed: Vec<(f64, &ModelEntry)> = self
            .corpus
            .free_chat()
            .filter(|m| !self.health.breaker_open(&m.id))
            // Only models a real provider serves on this host (when known):
            // keeps auto-pick from selecting a free-pool model that would fall
            // through to the api.example.com placeholder default and fail.
            .filter(|m| self.backed.as_ref().is_none_or(|b| b.contains(&m.id)))
            .map(|m| (Self::rank_key(m, tier), m))
            .filter(|(k, _)| *k > 0.0)
            .collect();
        keyed.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        keyed.into_iter().map(|(_, m)| m).collect()
    }

    /// Build a fallback chain from the ranked pool, excluding `primary_id`:
    /// highest-ranked models of a DIFFERENT family first (diversity dodges a
    /// family-wide outage), then same-family, capped at 4.
    fn build_fallbacks(
        ranked: &[&ModelEntry],
        primary_id: &str,
        primary_family: &str,
    ) -> Vec<String> {
        let mut fallbacks: Vec<String> = Vec::new();
        for m in ranked.iter().filter(|m| m.id != primary_id) {
            if m.family != primary_family && fallbacks.len() < 3 {
                fallbacks.push(m.id.clone());
            }
        }
        for m in ranked.iter().filter(|m| m.id != primary_id) {
            if fallbacks.len() >= 4 {
                break;
            }
            if m.family == primary_family {
                fallbacks.push(m.id.clone());
            }
        }
        fallbacks
    }

    /// Pick a primary + a cross-family fallback chain.
    pub fn select(&self, tier: Tier) -> anyhow::Result<Route> {
        let ranked = self.candidates(tier);

        // Operator-pinned primary: it always leads, and the ranked free pool
        // (capability x latency x live health) becomes the fallback chain
        // behind it. The pin is honored even when it is not itself in the free
        // pool (e.g. a flat-rate subscription model the operator wants first),
        // so an empty pool is not an error in this path.
        if let Some(pin) = &self.pinned_primary {
            let pin_entry = self.corpus.get(pin);
            let pin_family = pin_entry.map(|e| e.family.as_str()).unwrap_or("");
            let fallbacks = Self::build_fallbacks(&ranked, pin, pin_family);
            let cap_str =
                match pin_entry.and_then(|e| e.code_capability().zip(e.code_capability_source())) {
                    Some((c, src)) => format!("{c:.1} ({src})"),
                    None => "pinned".to_string(),
                };
            let reason = format!(
                "tier={tier:?} pick={pin} (PINNED primary; code_cap={cap_str}) then {} ranked free fallback(s)",
                fallbacks.len()
            );
            return Ok(Route {
                primary: pin.clone(),
                fallbacks,
                reason,
            });
        }

        let primary = ranked.first().ok_or_else(|| {
            match &self.backed {
                // A backed model exists in the pool but every one is filtered by
                // an open circuit breaker (all unhealthy right now) — distinct
                // from "none configured", so the operator retries rather than
                // reconfigures.
                Some(b) if self.corpus.free_chat().any(|m| b.contains(&m.id)) => anyhow::anyhow!(
                    "all backed free models are currently unhealthy (circuit breaker open) — \
                     retry shortly, or pass `-m <backed-model>` to force one"
                ),
                // The backed filter emptied the pool: this host has no free
                // model served by a real (non-placeholder) provider. Auto-pick
                // would otherwise have dialed the api.example.com placeholder.
                Some(_) => anyhow::anyhow!(
                    "no free model has a configured provider on this host — configure a provider \
                     (e.g. in ~/.zoder/config.toml), pin a backed model via [profile].primary_model, \
                     or pass `-m <backed-model>`"
                ),
                None => anyhow::anyhow!("no healthy free model available for tier {tier:?}"),
            }
        })?;

        let fallbacks = Self::build_fallbacks(&ranked, &primary.id, &primary.family);

        let cap_str = match (primary.code_capability(), primary.code_capability_source()) {
            (Some(c), Some(src)) => format!("{c:.1} ({src})"),
            _ => "n/a".to_string(),
        };
        let reason = format!(
            "tier={:?} pick={} (code_cap={} swe_elo={:?} ttft={:?}ms tok/s={:?} agentic={:?}) free=$0",
            tier,
            primary.id,
            cap_str,
            primary.swe_elo(),
            primary.ttft_ms_p50,
            primary.tok_per_s_p50,
            primary.agentic_score,
        );
        Ok(Route {
            primary: primary.id.clone(),
            fallbacks,
            reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{BenchScore, Capability};

    fn benched(id: &str, swe: f64) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            capability: Some(Capability {
                swe_verified: Some(BenchScore {
                    acc: Some(swe),
                    source: "vals.ai".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn strong_prefers_real_benchmark_over_arena_weight() {
        // A model with a *measured* 40% SWE score must outrank one with only a
        // high arena-derived weight (no real bench), because real data wins.
        let real = benched("real", 40.0);
        let arena_only = ModelEntry {
            id: "arena".into(),
            w_swe: Some(0.95),
            ..Default::default()
        };
        assert!(
            Router::rank_key(&real, Tier::Strong) > Router::rank_key(&arena_only, Tier::Strong)
        );
    }

    #[test]
    fn strong_sorts_benched_models_by_composite() {
        let hi = benched("hi", 80.0);
        let lo = benched("lo", 50.0);
        assert!(Router::rank_key(&hi, Tier::Strong) > Router::rank_key(&lo, Tier::Strong));
    }

    #[test]
    fn pinned_primary_leads_and_ranked_pool_falls_back() {
        // A pinned primary must lead the chain even though `hi` outranks it in
        // the free pool; the ranked pool then forms the fallbacks behind it.
        let health = HealthStore::default();
        let corpus = Corpus {
            models: vec![
                ModelEntry {
                    family: "minimax".into(),
                    ..benched("MiniMax-M3", 60.0)
                },
                ModelEntry {
                    family: "alpha".into(),
                    ..benched("hi", 90.0)
                },
                ModelEntry {
                    family: "beta".into(),
                    ..benched("lo", 50.0)
                },
            ]
            .into_iter()
            .map(|mut m| {
                m.free = true;
                m.route_candidate = true;
                m.kind = "chat".into();
                m
            })
            .collect(),
            ..Default::default()
        };
        let router = Router::new(&corpus, &health).with_primary(Some("MiniMax-M3".to_string()));
        let route = router.select(Tier::Auto).unwrap();
        assert_eq!(route.primary, "MiniMax-M3");
        // `hi` (higher SWE) leads the fallbacks, proving the rest stay ranked.
        assert_eq!(route.fallbacks.first().map(String::as_str), Some("hi"));
        assert!(!route.fallbacks.contains(&"MiniMax-M3".to_string()));
    }

    fn three_free_model_corpus() -> Corpus {
        Corpus {
            models: vec![
                ModelEntry {
                    family: "alpha".into(),
                    ..benched("backed-hi", 90.0)
                },
                ModelEntry {
                    family: "beta".into(),
                    ..benched("unbacked-top", 95.0)
                },
                ModelEntry {
                    family: "gamma".into(),
                    ..benched("backed-lo", 50.0)
                },
            ]
            .into_iter()
            .map(|mut m| {
                m.free = true;
                m.route_candidate = true;
                m.kind = "chat".into();
                m
            })
            .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn backed_filter_excludes_unbacked_models_from_autopick() {
        // The highest-ranked model (`unbacked-top`, SWE 95) has NO configured
        // provider on this host, so the backed filter must skip it — auto-pick
        // lands on the top BACKED model instead of a model that would fall
        // through to the api.example.com placeholder default.
        let health = HealthStore::default();
        let corpus = three_free_model_corpus();
        let backed: std::collections::HashSet<String> =
            ["backed-hi".to_string(), "backed-lo".to_string()].into();
        let router = Router::new(&corpus, &health).with_backed(Some(backed));
        let route = router.select(Tier::Auto).unwrap();
        assert_eq!(
            route.primary, "backed-hi",
            "unbacked-top must be filtered out"
        );
        assert!(!route.fallbacks.contains(&"unbacked-top".to_string()));
    }

    #[test]
    fn empty_backed_set_errors_instead_of_dialing_placeholder() {
        // An empty Some(backed) means "no free model has a real provider" — the
        // pool must filter to empty and select() must error (a legible failure),
        // NOT be treated as "no filter" and auto-pick an example.com-bound model.
        let health = HealthStore::default();
        let corpus = three_free_model_corpus();
        let router =
            Router::new(&corpus, &health).with_backed(Some(std::collections::HashSet::new()));
        let err = router.select(Tier::Auto).unwrap_err().to_string();
        assert!(
            err.contains("no free model has a configured provider"),
            "expected the actionable no-provider error, got: {err}"
        );
    }

    #[test]
    fn none_backed_preserves_legacy_unfiltered_routing() {
        // A caller without config info (None) must not filter — legacy behavior.
        let health = HealthStore::default();
        let corpus = three_free_model_corpus();
        let router = Router::new(&corpus, &health).with_backed(None);
        let route = router.select(Tier::Auto).unwrap();
        assert_eq!(
            route.primary, "unbacked-top",
            "None must not filter the pool"
        );
    }

    #[test]
    fn fast_stays_latency_first() {
        // Lower-capability but faster model wins the Fast tier.
        let fast_lowcap = ModelEntry {
            id: "fast".into(),
            latency_score: Some(0.9),
            capability: Some(Capability {
                aider_polyglot: Some(BenchScore {
                    acc: Some(20.0),
                    source: "aider".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let slow_hicap = ModelEntry {
            id: "slow".into(),
            latency_score: Some(0.2),
            ..benched("slow", 90.0)
        };
        assert!(
            Router::rank_key(&fast_lowcap, Tier::Fast) > Router::rank_key(&slow_hicap, Tier::Fast)
        );
    }
}
