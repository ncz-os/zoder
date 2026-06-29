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
}

impl Tier {
    pub fn parse(s: &str) -> Tier {
        match s.to_ascii_lowercase().as_str() {
            "fast" => Tier::Fast,
            "strong" | "codex" => Tier::Strong,
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
}

impl<'a> Router<'a> {
    pub fn new(corpus: &'a Corpus, health: &'a HealthStore) -> Self {
        Self { corpus, health }
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
            .map(|m| (Self::rank_key(m, tier), m))
            .filter(|(k, _)| *k > 0.0)
            .collect();
        keyed.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        keyed.into_iter().map(|(_, m)| m).collect()
    }

    /// Pick a primary + a cross-family fallback chain.
    pub fn select(&self, tier: Tier) -> anyhow::Result<Route> {
        let ranked = self.candidates(tier);
        let primary = ranked
            .first()
            .ok_or_else(|| anyhow::anyhow!("no healthy free model available for tier {tier:?}"))?;

        // Fallback chain: highest-ranked models of a DIFFERENT family than the
        // primary (diversity dodges a family-wide outage), then same-family.
        let mut fallbacks: Vec<String> = Vec::new();
        for m in ranked.iter().skip(1) {
            if m.family != primary.family && fallbacks.len() < 3 {
                fallbacks.push(m.id.clone());
            }
        }
        for m in ranked.iter().skip(1) {
            if fallbacks.len() >= 4 {
                break;
            }
            if m.family == primary.family {
                fallbacks.push(m.id.clone());
            }
        }

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
