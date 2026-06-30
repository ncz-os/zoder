//! The classified model corpus produced by the corpus builder/refresh job.
//!
//! Each [`ModelEntry`] carries free/paid classification, LMArena-derived
//! capability weights (overall + coding/SWE), and benched latency/throughput,
//! combined into a single `agentic_score` the router can sort on.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub leaf: String,
    #[serde(default)]
    pub family: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub route_candidate: bool,
    #[serde(default)]
    pub free: bool,
    #[serde(default)]
    pub paid: bool,
    #[serde(default)]
    pub gated_reason: Option<String>,

    #[serde(default)]
    pub arena_overall_elo: Option<f64>,
    #[serde(default)]
    pub arena_coding_elo: Option<f64>,
    #[serde(default)]
    pub arena_webdev_elo: Option<f64>,
    #[serde(default)]
    pub w_overall: Option<f64>,
    #[serde(default)]
    pub w_coding: Option<f64>,
    #[serde(default)]
    pub w_swe: Option<f64>,

    // populated by the latency bench / merge
    #[serde(default)]
    pub ttft_ms_p50: Option<f64>,
    #[serde(default)]
    pub tok_per_s_p50: Option<f64>,
    #[serde(default)]
    pub total_ms_p50: Option<f64>,
    #[serde(default)]
    pub latency_score: Option<f64>,
    #[serde(default)]
    pub latency_class: Option<String>,
    #[serde(default)]
    pub agentic_score: Option<f64>,

    /// Multi-source coding-capability benchmarks (vals.ai SWE-bench Verified,
    /// Aider Polyglot, LiveCodeBench, Terminal-Bench, Scale SEAL). Each source
    /// writes only its own namespaced block, so feeds never clobber each other.
    #[serde(default)]
    pub capability: Option<Capability>,
    /// Crowd-preference rankings (arena.ai). Deliberately separate from
    /// `capability`: these are Elo/win-rate, NOT task solve-rate, so they are a
    /// distinct signal and never fold into the solve-rate composite.
    #[serde(default)]
    pub preference: Option<Preference>,
    /// Per-token economics, the source of truth projected into `pricing.json`.
    #[serde(default)]
    pub economics: Option<Economics>,
    /// Curated per-workflow suitability (single-pass authoring vs grind-loop
    /// convergence) from the known-good SWE list. No benchmark provides this, so
    /// it is the routing authority for `--tier single-pass|grind`.
    #[serde(default)]
    pub workflows: Option<Workflows>,
}

impl ModelEntry {
    /// SWE capability ELO, preferring the text-coding arena then webdev.
    pub fn swe_elo(&self) -> Option<f64> {
        self.arena_coding_elo.or(self.arena_webdev_elo)
    }
    /// True when this is a free, general chat model we can route real work to.
    pub fn routable(&self) -> bool {
        self.free && self.route_candidate && self.kind == "chat"
    }

    /// Unified coding-capability score: the straight mean of whatever benchmark
    /// scores are present (0..100). Missing sources are skipped, not penalized,
    /// so a model rated by 3 of 5 sources averages those 3. `None` when no
    /// coding benchmark has a score for this model.
    pub fn code_capability(&self) -> Option<f64> {
        let cap = self.capability.as_ref()?;
        let scores: Vec<f64> = cap
            .scores_in_authority_order()
            .iter()
            .filter_map(|(_, s)| s.acc)
            .collect();
        if scores.is_empty() {
            return None;
        }
        Some(scores.iter().sum::<f64>() / scores.len() as f64)
    }

    /// The most-authoritative source that contributed a coding score, for
    /// display next to the capability number (e.g. `82.4 (vals.ai)`). Authority
    /// order: vals.ai (independently audited) > Scale SEAL (locked harness) >
    /// Aider > LiveCodeBench > Terminal-Bench.
    pub fn code_capability_source(&self) -> Option<String> {
        let cap = self.capability.as_ref()?;
        cap.scores_in_authority_order()
            .into_iter()
            .find(|(_, s)| s.acc.is_some())
            .map(|(_, s)| {
                if s.source.is_empty() {
                    "?".to_string()
                } else {
                    s.source.clone()
                }
            })
    }

    /// Compact crowd-preference label for display (e.g. `1654 (arena.ai)`),
    /// preferring the coding-relevant webdev Elo, then the agent score. Separate
    /// from the solve-rate composite by design.
    pub fn arena_label(&self) -> Option<String> {
        let p = self.preference.as_ref()?;
        if let Some(s) = &p.arena_webdev {
            if let Some(r) = s.rating {
                return Some(format!("{r:.0} ({})", s.source));
            }
        }
        if let Some(s) = &p.arena_agent {
            if let Some(sc) = s.score {
                return Some(format!("{sc:.3} ({})", s.source));
            }
        }
        None
    }

    /// Capability per paid dollar: `code_capability / ($/Mtok + ε)`. Free models
    /// (cost 0) naturally dominate (ε keeps it finite). `None` without a
    /// capability score. Used by the router for SWE-aware, cost-aware selection.
    pub fn value_score(&self) -> Option<f64> {
        let cap = self.code_capability()?;
        let cost = self
            .economics
            .as_ref()
            .map(|e| e.blended_usd_per_mtok())
            .unwrap_or(0.0);
        Some(cap / (cost + 0.01))
    }

    /// Minimal entry for a newly-observed served model id. Deliberately
    /// conservative: it is NOT classified free and NOT route-eligible until the
    /// full corpus builder classifies + benches it, so a refresh can never
    /// silently promote an unknown (possibly paid) model into routing.
    fn from_served_id(id: &str) -> Self {
        let (host, leaf) = match id.split_once('/') {
            Some((h, l)) => (h.to_string(), l.to_string()),
            None => (String::new(), id.to_string()),
        };
        let family = if host.is_empty() {
            id.to_string()
        } else {
            host.clone()
        };
        ModelEntry {
            id: id.to_string(),
            host,
            leaf,
            family,
            kind: "chat".into(),
            route_candidate: false,
            free: false,
            paid: false,
            gated_reason: Some("new: needs classification + bench (run corpus builder)".into()),
            ..Default::default()
        }
    }
}

/// One coding benchmark's result for a model, with provenance so a
/// scaffold-inflated or stale row is always visible.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BenchScore {
    /// Accuracy / pass-rate on a 0..100 scale.
    #[serde(default)]
    pub acc: Option<f64>,
    /// Where the score came from (e.g. `vals.ai`, `aider`, `artificialanalysis`).
    #[serde(default)]
    pub source: String,
    /// ISO date the score was ingested/published.
    #[serde(default)]
    pub date: Option<String>,
    /// Scaffold/harness, when disclosed (SWE-bench scores swing 10-20pts by
    /// harness, so this is recorded when known).
    #[serde(default)]
    pub harness: Option<String>,
    /// Extra SWE axes (mainly from vals.ai): $/test and wall-clock latency.
    #[serde(default)]
    pub cost_per_test: Option<f64>,
    #[serde(default)]
    pub latency_s: Option<f64>,
}

/// Multi-source coding-capability block. Each field is one benchmark source;
/// all are optional so coverage can be partial.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capability {
    /// vals.ai SWE-bench Verified (independently audited; multi-axis).
    #[serde(default)]
    pub swe_verified: Option<BenchScore>,
    /// Aider Polyglot (multi-language edit/diff quality).
    #[serde(default)]
    pub aider_polyglot: Option<BenchScore>,
    /// LiveCodeBench via Artificial Analysis (contamination-resistant).
    #[serde(default)]
    pub livecodebench: Option<BenchScore>,
    /// Terminal-Bench 2.0 (agentic shell).
    #[serde(default)]
    pub terminal_bench: Option<BenchScore>,
    /// Scale SEAL (locked, standardized harness).
    #[serde(default)]
    pub scale_seal: Option<BenchScore>,
}

impl Capability {
    /// All present benchmark blocks, ordered most- to least-authoritative for
    /// the display label. The composite (`code_capability`) averages these
    /// regardless of order; the order only decides which source is shown.
    fn scores_in_authority_order(&self) -> Vec<(&'static str, &BenchScore)> {
        let mut out = Vec::with_capacity(5);
        if let Some(s) = &self.swe_verified {
            out.push(("swe_verified", s));
        }
        if let Some(s) = &self.scale_seal {
            out.push(("scale_seal", s));
        }
        if let Some(s) = &self.aider_polyglot {
            out.push(("aider_polyglot", s));
        }
        if let Some(s) = &self.livecodebench {
            out.push(("livecodebench", s));
        }
        if let Some(s) = &self.terminal_bench {
            out.push(("terminal_bench", s));
        }
        out
    }
}

/// One crowd-preference ranking for a model. `rating` is Elo-style (webdev),
/// `score` is a 0..1 win-rate (agent); a row carries whichever its surface uses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrefScore {
    #[serde(default)]
    pub rating: Option<f64>,
    #[serde(default)]
    pub score: Option<f64>,
    #[serde(default)]
    pub rank: Option<u32>,
    #[serde(default)]
    pub votes: Option<u64>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub date: Option<String>,
}

/// Crowd-preference signal (arena.ai). Kept apart from `Capability` because
/// preference Elo/win-rate is not comparable to task solve-rate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Preference {
    /// arena.ai webdev/code Elo.
    #[serde(default)]
    pub arena_webdev: Option<PrefScore>,
    /// arena.ai agent win-rate (0..1).
    #[serde(default)]
    pub arena_agent: Option<PrefScore>,
}

/// Curated per-workflow suitability scores (0..1) from the known-good SWE list.
/// `single_pass` = one-shot agentic authoring quality; `grind` = adversarial
/// loop convergence (multi-step runtime-failure debugging). The two are distinct
/// by design: a strong single-pass author can still stall in a grind loop when a
/// check fails on runtime behavior it must iteratively debug (observed: Kimi-K2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Workflows {
    #[serde(default)]
    pub single_pass: Option<f64>,
    #[serde(default)]
    pub grind: Option<f64>,
}

/// Per-token economics for a model (per 1M tokens). Source of truth that the
/// corpus builder projects into the daemon's `pricing.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Economics {
    #[serde(default)]
    pub input_usd_per_mtok: f64,
    #[serde(default)]
    pub output_usd_per_mtok: f64,
    #[serde(default)]
    pub cache_read_usd_per_mtok: f64,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub date: Option<String>,
}

impl Economics {
    /// A single representative $/Mtok for value scoring: the input/output mean
    /// when both are set, else whichever is non-zero (0.0 = free).
    pub fn blended_usd_per_mtok(&self) -> f64 {
        match (self.input_usd_per_mtok, self.output_usd_per_mtok) {
            (i, o) if i > 0.0 && o > 0.0 => (i + o) / 2.0,
            (i, o) => i.max(o),
        }
    }
}

/// Outcome of reconciling the corpus against the live served-model list.
#[derive(Debug, Default)]
pub struct RefreshReport {
    pub added: Vec<String>,
    pub retired: Vec<String>,
    pub kept: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Corpus {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub arena_date: String,
    #[serde(default)]
    pub count: usize,
    pub models: Vec<ModelEntry>,
}

impl Corpus {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read corpus {}: {e}", path.display()))?;
        // Lenient parse: deserialize the models array element-by-element so one
        // bad entry doesn't take down every command. `id` stays required.
        let root: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("corpus {} is not valid JSON: {e}", path.display()))?;

        let str_field = |k: &str| {
            root.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };

        let mut models = Vec::new();
        let mut skipped = 0usize;
        match root.get("models").and_then(|v| v.as_array()) {
            Some(arr) => {
                for item in arr {
                    match serde_json::from_value::<ModelEntry>(item.clone()) {
                        Ok(m) if !m.id.is_empty() => models.push(m),
                        _ => skipped += 1,
                    }
                }
            }
            None => anyhow::bail!("corpus {}: missing `models` array", path.display()),
        }
        if skipped > 0 {
            eprintln!("zoder: warning: corpus skipped {skipped} invalid model entr{} (missing/invalid `id` or fields)",
                if skipped == 1 { "y" } else { "ies" });
        }

        Ok(Corpus {
            source: str_field("source"),
            arena_date: str_field("arena_date"),
            count: models.len(),
            models,
        })
    }

    pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        self.models.iter().find(|m| m.id == id)
    }

    pub fn free_chat(&self) -> impl Iterator<Item = &ModelEntry> {
        self.models.iter().filter(|m| m.routable())
    }

    /// Reconcile the corpus with the live set of served model ids. New ids are
    /// added as unclassified/non-routable; ids no longer served are retired
    /// (kept for history but removed from routing). Existing classification and
    /// scores are preserved so a refresh never loses bench data.
    pub fn reconcile(&mut self, served: &[String]) -> RefreshReport {
        let mut report = RefreshReport::default();
        let served_set: std::collections::HashSet<&str> =
            served.iter().map(|s| s.as_str()).collect();
        let existing: std::collections::HashSet<String> =
            self.models.iter().map(|m| m.id.clone()).collect();

        for id in served {
            if !existing.contains(id) {
                self.models.push(ModelEntry::from_served_id(id));
                report.added.push(id.clone());
            } else {
                report.kept += 1;
            }
        }
        for m in self.models.iter_mut() {
            // Only models still in the routing pool (route_candidate) are
            // retired. Keying off `route_candidate` (the field we clear) keeps
            // this idempotent: once retired, a later refresh won't re-report it.
            if !served_set.contains(m.id.as_str()) && m.route_candidate {
                m.route_candidate = false;
                // Also drop the free flag: a retired model is no longer a
                // proven free route, so it must not linger as routable-free if
                // re-added to a different (possibly paid) catalog later.
                m.free = false;
                m.gated_reason = Some("retired: not currently served".into());
                report.retired.push(m.id.clone());
            }
        }
        self.count = self.models.len();
        report
    }

    /// Fold a free provider's live catalog into the routing pool. Each id in
    /// `ids` (already prefix-filtered to the provider's `serves` allowlist by
    /// the caller, e.g. NVIDIA EIH's `nvidia/* | deepseek-ai/* | meta/llama-* |
    /// mistralai/*` open-weight NIMs) is upserted as a free, routable chat
    /// candidate. Existing entries keep all benchmark/capability/latency scores
    /// — only the free/route flags are (re)asserted, so re-running a refresh is
    /// idempotent and never loses bench data. A new (unbenched) entry gets a
    /// neutral agentic prior so it is selectable as a fallback until the corpus
    /// builder benches it; a benched entry's real score always wins (the prior
    /// is only set when no capability/agentic signal exists). Returns the number
    /// of entries newly promoted into the routing pool.
    pub fn ingest_free_chat(&mut self, ids: &[String]) -> usize {
        const UNBENCHED_PRIOR: f64 = 0.5;
        let mut promoted = 0usize;
        for id in ids {
            if let Some(m) = self.models.iter_mut().find(|m| &m.id == id) {
                // Never silently flip a model the corpus already classifies as
                // paid (or with nonzero per-token economics) into free — an
                // overbroad `serves` prefix must not launder a paid model into
                // the free pool. Leave such an entry untouched.
                let priced = m
                    .economics
                    .as_ref()
                    .map(|e| e.input_usd_per_mtok > 0.0 || e.output_usd_per_mtok > 0.0)
                    .unwrap_or(false);
                if m.paid || priced {
                    continue;
                }
                let was_routable = m.routable();
                m.free = true;
                m.route_candidate = true;
                m.kind = "chat".into();
                m.gated_reason = None;
                if m.agentic_score.is_none() && m.code_capability().is_none() {
                    m.agentic_score = Some(UNBENCHED_PRIOR);
                }
                if !was_routable {
                    promoted += 1;
                }
            } else {
                let mut e = ModelEntry::from_served_id(id);
                e.free = true;
                e.paid = false;
                e.route_candidate = true;
                e.kind = "chat".into();
                e.gated_reason = None;
                e.agentic_score = Some(UNBENCHED_PRIOR);
                self.models.push(e);
                promoted += 1;
            }
        }
        self.count = self.models.len();
        promoted
    }

    /// Persist atomically (temp file + rename).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    fn bench(source: &str, acc: f64) -> BenchScore {
        BenchScore {
            acc: Some(acc),
            source: source.into(),
            ..Default::default()
        }
    }

    #[test]
    fn composite_is_mean_of_present_scores() {
        let m = ModelEntry {
            id: "x".into(),
            capability: Some(Capability {
                swe_verified: Some(bench("vals.ai", 90.0)),
                aider_polyglot: Some(bench("aider", 80.0)),
                livecodebench: Some(bench("artificialanalysis", 70.0)),
                ..Default::default()
            }),
            ..Default::default()
        };
        // mean(90, 80, 70) == 80; missing terminal_bench/scale_seal are skipped.
        assert_eq!(m.code_capability(), Some(80.0));
    }

    #[test]
    fn missing_sources_do_not_penalize() {
        // Only one source present -> composite is exactly that score, not diluted.
        let m = ModelEntry {
            id: "x".into(),
            capability: Some(Capability {
                aider_polyglot: Some(bench("aider", 88.0)),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(m.code_capability(), Some(88.0));
    }

    #[test]
    fn no_capability_is_none() {
        let m = ModelEntry {
            id: "x".into(),
            ..Default::default()
        };
        assert_eq!(m.code_capability(), None);
        assert_eq!(m.code_capability_source(), None);
        assert_eq!(m.value_score(), None);
    }

    #[test]
    fn authority_label_prefers_vals_then_seal_then_aider() {
        // vals.ai wins when present.
        let m = ModelEntry {
            id: "x".into(),
            capability: Some(Capability {
                swe_verified: Some(bench("vals.ai", 90.0)),
                aider_polyglot: Some(bench("aider", 80.0)),
                scale_seal: Some(bench("scale-seal", 60.0)),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(m.code_capability_source().as_deref(), Some("vals.ai"));

        // Without vals.ai, Scale SEAL outranks Aider.
        let m2 = ModelEntry {
            id: "x".into(),
            capability: Some(Capability {
                aider_polyglot: Some(bench("aider", 80.0)),
                scale_seal: Some(bench("scale-seal", 60.0)),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(m2.code_capability_source().as_deref(), Some("scale-seal"));
    }

    #[test]
    fn free_models_dominate_value_score() {
        let cap = Capability {
            swe_verified: Some(bench("vals.ai", 70.0)),
            ..Default::default()
        };
        let free = ModelEntry {
            id: "free".into(),
            capability: Some(cap.clone()),
            economics: Some(Economics::default()), // $0
            ..Default::default()
        };
        let paid = ModelEntry {
            id: "paid".into(),
            capability: Some(cap),
            economics: Some(Economics {
                input_usd_per_mtok: 3.0,
                output_usd_per_mtok: 15.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        // Same capability, but the free model's value score is far higher.
        assert!(free.value_score().unwrap() > paid.value_score().unwrap() * 10.0);
    }

    #[test]
    fn preference_is_separate_from_composite() {
        // arena.ai preference must NOT change the solve-rate composite, and must
        // surface via its own label.
        let m = ModelEntry {
            id: "x".into(),
            capability: Some(Capability {
                aider_polyglot: Some(bench("aider", 70.0)),
                ..Default::default()
            }),
            preference: Some(Preference {
                arena_webdev: Some(PrefScore {
                    rating: Some(1654.0),
                    source: "arena.ai".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(m.code_capability(), Some(70.0)); // unchanged by the Elo
        assert_eq!(m.arena_label().as_deref(), Some("1654 (arena.ai)"));
    }

    #[test]
    fn capability_block_round_trips_json() {
        let json = r#"{
            "id": "vendor/some-model",
            "capability": {
                "swe_verified": {"acc": 82.6, "source": "vals.ai", "cost_per_test": 1.23},
                "livecodebench": {"acc": 91.7, "source": "artificialanalysis"}
            },
            "economics": {"input_usd_per_mtok": 1.25, "output_usd_per_mtok": 10.0, "source": "litellm"}
        }"#;
        let m: ModelEntry = serde_json::from_str(json).unwrap();
        assert_eq!(m.code_capability(), Some((82.6 + 91.7) / 2.0));
        assert_eq!(m.code_capability_source().as_deref(), Some("vals.ai"));
        assert!(m.value_score().is_some());
    }
}
