//! The classified model corpus produced by the corpus builder/refresh job.
//!
//! Each [`ModelEntry`] carries free/paid classification, LMArena-derived
//! capability weights (overall + coding/SWE), and benched latency/throughput,
//! combined into a single `agentic_score` the router can sort on.
//!
//! # Alias overlay
//!
//! The builder's corpus is refreshed on a bench cadence, so ids that a vendor
//! ships *faster than the bench* (`gpt-6-sol`, `claude-opus-5-5`, ...) are
//! unknown to `zoder models`, `-m` pinning and pricing/health lookups until the
//! next refresh. An optional, provider-neutral alias overlay
//! (`$ZODER_MODEL_ALIASES`, default `~/.zoder/model-aliases.json`) closes that
//! window: it names current model ids and the short aliases an operator
//! actually types, and [`Corpus::load`] folds both into the corpus.
//!
//! Two invariants keep the overlay from becoming a back door into routing:
//!
//!   * an overlay-added row is METADATA ONLY — `route_candidate = false` and
//!     no invented pricing/Elo, so it can never be auto-selected or priced;
//!   * a model the corpus already knows keeps every corpus field and merely
//!     gains its aliases, so an overlay can never demote or re-score a
//!     benched model.
//!
//! Aliases translate only an EXPLICIT id an operator typed (exact match, exact
//! ids win over aliases) — they never feed the ranking or the fallback chain.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Monotonic per-writer nonce so two overlapping corpus saves in the same
/// process never collide on the temp path. Combined with `std::process::id()`
/// it makes the temp filename unique per writer (C5-4 / S19).
static SAVE_NONCE: AtomicU64 = AtomicU64::new(0);

/// Provenance tag stamped on rows the corpus builder never classified: entries
/// created from the alias overlay. Permanent — it records where an id came
/// from, not whether it is currently routable (a row promoted later by
/// `ingest_free_chat` keeps the tag).
pub const OVERLAY_SOURCE: &str = "overlay";

/// Env var that overrides where the alias overlay is read from. Unset (or
/// empty) falls back to `<zoder home>/model-aliases.json`.
pub const MODEL_ALIASES_ENV: &str = "ZODER_MODEL_ALIASES";

/// File name of the alias overlay inside the zoder home.
pub const MODEL_ALIASES_FILE: &str = "model-aliases.json";

/// Process-wide latch so a bad overlay warns ONCE per process: `Corpus::load`
/// runs in several commands (`config`, `models`, `route`, every dispatch), and
/// a per-call warning would repeat the same line on every invocation.
static OVERLAY_WARNED: AtomicBool = AtomicBool::new(false);

/// Emit `args` on stderr at most once per process. Returns whether THIS call
/// was the one that printed, which is what makes "warn once" testable.
fn warn_overlay_once(args: fmt::Arguments<'_>) -> bool {
    warn_overlay_once_flagged(&OVERLAY_WARNED, args)
}

/// The latch logic of [`warn_overlay_once`], with the flag passed in so a test
/// can prove the "exactly one of N" contract on its own fresh latch instead of
/// racing whatever the process-global one has already emitted.
fn warn_overlay_once_flagged(latch: &AtomicBool, args: fmt::Arguments<'_>) -> bool {
    if latch.swap(true, Ordering::Relaxed) {
        return false;
    }
    eprintln!("zoder: warning: {args}");
    true
}

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

    /// Operator-facing aliases this id also answers to (e.g. `codex:sol` ->
    /// `gpt-6-sol`), ingested from the optional alias overlay. Purely a
    /// name-translation layer for an EXPLICIT id: it is never a ranking,
    /// eligibility or pricing input.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Provenance of a row the corpus builder did not classify — `overlay`
    /// for alias-overlay entries ([`OVERLAY_SOURCE`]), `None` for a normal
    /// corpus row. Surfaced by `zoder models` so an operator can tell a
    /// metadata-only row from a benched one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

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

#[cfg(test)]
mod ingest_free_chat_tests {
    use super::*;

    fn scored(id: &str, agentic_score: f64, w_swe: f64) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            agentic_score: Some(agentic_score),
            w_swe: Some(w_swe),
            capability: Some(Capability {
                swe_verified: Some(BenchScore {
                    acc: Some(w_swe * 100.0),
                    source: "test".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            workflows: Some(Workflows {
                single_pass: Some(agentic_score),
                grind: Some(w_swe),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn bare_served_id_inherits_prefixed_quality() {
        let mut corpus = Corpus {
            models: vec![scored("minimax/MiniMax-M3:nitro", 0.647, 0.875)],
            ..Default::default()
        };

        assert_eq!(corpus.ingest_free_chat(&["MiniMax-M3:free".into()]), 1);
        let served = corpus.get("MiniMax-M3:free").unwrap();
        assert_eq!(served.agentic_score, Some(0.647));
        assert_eq!(served.w_swe, Some(0.875));
        assert_eq!(served.code_capability(), Some(87.5));
        assert_eq!(served.workflows.as_ref().unwrap().grind, Some(0.875));
    }

    #[test]
    fn normalized_match_has_no_substring_bleed() {
        let mut corpus = Corpus {
            models: vec![scored("minimax/minimax-m2.7", 0.670, 0.869)],
            ..Default::default()
        };

        corpus.ingest_free_chat(&["minimax-m2".into()]);
        let served = corpus.get("minimax-m2").unwrap();
        assert_eq!(served.agentic_score, Some(0.5));
        assert_eq!(served.w_swe, None);
        assert!(served.capability.is_none());
    }

    #[test]
    fn quality_inheritance_never_inherits_billing_and_paid_rows_stay_skipped() {
        let mut paid_twin = scored("vendor/model-x", 0.8, 0.9);
        paid_twin.free = false;
        paid_twin.paid = true;
        paid_twin.route_candidate = false;
        paid_twin.gated_reason = Some("paid feed row".into());
        paid_twin.economics = Some(Economics {
            input_usd_per_mtok: 1.0,
            output_usd_per_mtok: 2.0,
            source: "paid-feed".into(),
            ..Default::default()
        });
        let directly_paid = ModelEntry {
            id: "already-paid".into(),
            paid: true,
            economics: paid_twin.economics.clone(),
            kind: "chat".into(),
            ..Default::default()
        };
        let mut corpus = Corpus {
            models: vec![paid_twin, directly_paid],
            ..Default::default()
        };

        assert_eq!(
            corpus.ingest_free_chat(&["model-x".into(), "already-paid".into()]),
            1
        );
        let served = corpus.get("model-x").unwrap();
        assert_eq!(served.agentic_score, Some(0.8));
        assert!(served.free && served.route_candidate && !served.paid);
        assert!(served.economics.is_none());
        assert!(served.gated_reason.is_none());

        let skipped = corpus.get("already-paid").unwrap();
        assert!(skipped.paid && !skipped.free && !skipped.route_candidate);
        assert!(skipped.economics.is_some());
    }

    #[test]
    fn unmatched_model_keeps_unbenched_prior() {
        let mut corpus = Corpus::default();
        corpus.ingest_free_chat(&["brand-new-model".into()]);
        assert_eq!(
            corpus.get("brand-new-model").unwrap().agentic_score,
            Some(0.5)
        );
    }

    #[test]
    fn inherited_scores_fix_minimax_fallback_order() {
        let scores = [
            ("MiniMax-M3", 0.647, 0.875),
            ("MiniMax-M2.7", 0.670, 0.869),
            ("MiniMax-M2", 0.328, 0.664),
            ("MiniMax-M2.1", 0.30, 0.60),
            ("MiniMax-M2.1-highspeed", 0.29, 0.59),
            ("MiniMax-M2.5", 0.31, 0.61),
            ("MiniMax-M2.5-highspeed", 0.28, 0.58),
            ("MiniMax-M1", 0.20, 0.50),
        ];
        let mut corpus = Corpus {
            models: scores
                .iter()
                .map(|(id, score, swe)| scored(&format!("minimax/{id}"), *score, *swe))
                .collect(),
            ..Default::default()
        };
        let served_ids: Vec<String> = scores.iter().map(|(id, _, _)| (*id).into()).collect();
        corpus.ingest_free_chat(&served_ids);

        let mut ranked: Vec<&ModelEntry> = corpus.free_chat().collect();
        ranked.sort_by(|a, b| {
            b.agentic_score
                .partial_cmp(&a.agentic_score)
                .unwrap()
                .then_with(|| a.id.cmp(&b.id))
        });
        let fallback_cap: Vec<&str> = ranked.iter().take(4).map(|m| m.id.as_str()).collect();
        assert_eq!(fallback_cap[0..2], ["MiniMax-M2.7", "MiniMax-M3"]);
        let m2_position = ranked.iter().position(|m| m.id == "MiniMax-M2").unwrap();
        assert!(ranked.iter().position(|m| m.id == "MiniMax-M3").unwrap() < m2_position);
        assert!(ranked.iter().position(|m| m.id == "MiniMax-M2.7").unwrap() < m2_position);
    }

    #[test]
    fn update_path_replaces_unbenched_prior_with_inherited_quality() {
        let mut existing = ModelEntry::from_served_id("MiniMax-M3");
        existing.agentic_score = Some(0.5);
        let mut corpus = Corpus {
            models: vec![existing, scored("minimax/MiniMax-M3", 0.647, 0.875)],
            ..Default::default()
        };

        corpus.ingest_free_chat(&["MiniMax-M3".into()]);
        assert_eq!(corpus.get("MiniMax-M3").unwrap().agentic_score, Some(0.647));
    }

    #[test]
    fn duplicate_quality_rows_choose_highest_w_swe_deterministically() {
        let mut corpus = Corpus {
            models: vec![
                scored("z_feed/model-x", 0.9, 0.7),
                scored("a_feed/model-x", 0.6, 0.8),
            ],
            ..Default::default()
        };

        corpus.ingest_free_chat(&["model-x".into()]);
        let served = corpus.get("model-x").unwrap();
        assert_eq!(served.w_swe, Some(0.8));
        assert_eq!(served.agentic_score, Some(0.6));
    }
}

impl ModelEntry {
    /// Provider catalogs often expose a bare id while public feeds namespace
    /// the same model. Match only the final path component, case-insensitively,
    /// and discard a provider variant suffix such as `:free` or `:nitro`.
    fn normalized_model_key(id: &str) -> String {
        let leaf = id.rsplit('/').next().unwrap_or(id);
        leaf.split_once(':')
            .map_or(leaf, |(base, _)| base)
            .to_ascii_lowercase()
    }

    fn has_quality_signal(&self) -> bool {
        self.agentic_score.is_some()
            || self.w_swe.is_some()
            || self.code_capability().is_some()
            || self.workflows.is_some()
    }

    /// Copy model quality only. In particular, provider classification and
    /// billing fields must remain properties of the served row.
    fn inherit_quality_from(&mut self, source: &Self) {
        self.arena_overall_elo = source.arena_overall_elo;
        self.arena_coding_elo = source.arena_coding_elo;
        self.arena_webdev_elo = source.arena_webdev_elo;
        self.w_overall = source.w_overall;
        self.w_coding = source.w_coding;
        self.w_swe = source.w_swe;
        self.ttft_ms_p50 = source.ttft_ms_p50;
        self.tok_per_s_p50 = source.tok_per_s_p50;
        self.total_ms_p50 = source.total_ms_p50;
        self.latency_score = source.latency_score;
        self.latency_class = source.latency_class.clone();
        self.agentic_score = source.agentic_score;
        self.capability = source.capability.clone();
        self.preference = source.preference.clone();
        self.workflows = source.workflows.clone();
    }

    /// SWE capability ELO, preferring the text-coding arena then webdev.
    pub fn swe_elo(&self) -> Option<f64> {
        self.arena_coding_elo.or(self.arena_webdev_elo)
    }
    /// True when this is a free, general chat model we can route real work to.
    pub fn routable(&self) -> bool {
        self.free && self.route_candidate && self.kind == "chat"
    }
    /// True when the row came from the alias overlay rather than the corpus
    /// builder, i.e. it carries NO benchmark, pricing or routing evidence and
    /// exists so a current model id resolves. Display provenance only.
    pub fn is_overlay(&self) -> bool {
        self.source.as_deref() == Some(OVERLAY_SOURCE)
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

/// One model row in the provider-neutral alias overlay: a current model id,
/// the provider that serves it, and the short aliases an operator types for
/// it. Any other key the generator emits (`display_name`, `created_at`, ...) is
/// accepted and ignored — serde's default is to skip unknown fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OverlayModel {
    /// Canonical model id, e.g. `gpt-6-sol`.
    pub id: String,
    /// Provider that serves this id (e.g. `openai-codex`). Recorded as the
    /// entry's `family` for display; it decides neither routing nor billing.
    #[serde(default)]
    pub provider: String,
    /// Aliases that resolve to `id`, e.g. `["codex:sol"]`.
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// The optional provider-neutral alias overlay, generated outside zoder and
/// deployed to `<zoder home>/model-aliases.json`.
///
/// zoder reads exactly two things from it: the `models` rows (so an id a vendor
/// shipped after the last corpus refresh is at least *known*) and the `aliases`
/// map (so `-m codex:sol` reaches `gpt-6-sol`). The generator's
/// `codex_ladder`/`source`/`generated_at` fields are accepted and ignored on
/// purpose: zoder ranks on benched corpus data plus live provider health, never
/// on a static ladder, and never derives pricing from an alias file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AliasOverlay {
    #[serde(default)]
    pub models: Vec<OverlayModel>,
    /// alias -> model id. Resolved on EXACT match only.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
}

/// What [`Corpus::apply_alias_overlay`] ingested. Reported so an operator (or
/// a test) can see what the overlay contributed without diffing the corpus.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OverlayReport {
    /// The overlay file that was read. `None` when no overlay is deployed —
    /// the common case, since the overlay is optional.
    pub path: Option<PathBuf>,
    /// Overlay ids absent from the corpus, added as metadata-only rows.
    pub added: Vec<String>,
    /// Models the corpus already knew that gained at least one alias.
    pub aliased: Vec<String>,
    /// Size of the alias map after ingest (existing entries plus overlay ones).
    pub alias_count: usize,
}

/// Bounded read of the overlay file. Split out from [`AliasOverlay`]'s
/// consumer so "not deployed" stays distinguishable from "deployed but
/// broken": only the latter may warn.
enum OverlayRead {
    /// No overlay at this path (or it vanished between resolution and read).
    Missing,
    /// Deployed but not usable as a plain bounded JSON file.
    Broken(String),
    Body(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "CorpusWire")]
pub struct Corpus {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub arena_date: String,
    #[serde(default)]
    pub count: usize,
    pub models: Vec<ModelEntry>,
    /// Alias -> canonical model id, ingested from the optional alias overlay
    /// and consulted by [`Corpus::resolve_alias`] / [`Corpus::get`]. Persisted
    /// with the corpus so a refresh never loses the names an operator's config
    /// pins; omitted from the serialized form when empty so a corpus written by
    /// a build with no overlay is byte-identical to one written before.
    ///
    /// The persisted map is a CACHE: every construction path rebuilds it from
    /// the `aliases` list on the model rows, so a corpus saved while an overlay
    /// was deployed keeps resolving those names after the overlay file is gone.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub aliases: BTreeMap<String, String>,
}

/// Wire shape for [`Corpus`] deserialization. Separated from `Corpus` so
/// [`From<CorpusWire>`] can run [`Corpus::rebuild_aliases_from_models`] on
/// EVERY deserialization path, not just the file loader. Any top-level
/// `aliases` map in the JSON is deliberately not read: it is derived state and
/// would otherwise let a stale map disagree with the rows it was written from.
#[derive(Debug, Default, Deserialize)]
struct CorpusWire {
    #[serde(default)]
    source: String,
    #[serde(default)]
    arena_date: String,
    #[serde(default)]
    count: usize,
    #[serde(default)]
    models: Vec<ModelEntry>,
}

impl From<CorpusWire> for Corpus {
    fn from(wire: CorpusWire) -> Self {
        let mut corpus = Corpus {
            source: wire.source,
            arena_date: wire.arena_date,
            count: wire.count,
            models: wire.models,
            aliases: BTreeMap::new(),
        };
        corpus.rebuild_aliases_from_models();
        corpus
    }
}

impl Default for Corpus {
    fn default() -> Self {
        // Route the empty/default path through the same constructor so it has
        // exactly the same alias invariant as a loaded corpus.
        CorpusWire::default().into()
    }
}

impl Corpus {
    /// Maximum trusted model-corpus size. Larger files are rejected before the
    /// body is read so a misconfigured `corpus_path` pointing at a multi-GB JSON
    /// can't OOM `Engine::load()`. The actual bundled corpus is ~1.6 MiB
    /// (hundreds of models with descriptions and bench data); 8 MiB sits one
    /// order of magnitude above real usage while staying bounded -- a typo'd
    /// 64-bit `len()` would land in the `Err` path, not as an allocation.
    const MAX_CORPUS_BYTES: u64 = 8 * 1024 * 1024; // 8 MiB

    /// Maximum trusted alias-overlay size. The overlay is a small generated
    /// artifact (tens of models plus an alias map), so 1 MiB sits an order of
    /// magnitude above real usage while keeping a misconfigured
    /// `$ZODER_MODEL_ALIASES` pointing at some large file from being read into
    /// memory at all. Enforced at the stat and at the read, as above.
    const MAX_ALIAS_OVERLAY_BYTES: u64 = 1024 * 1024; // 1 MiB

    /// Load a corpus from `path`, rejecting non-regular files (FIFO, socket,
    /// device, directory) and any file larger than [`Self::MAX_CORPUS_BYTES`].
    /// The file is opened ONCE and validated + read through that same file
    /// descriptor so an attacker cannot swap a small, regular file for a huge
    /// one (or a FIFO) between the size check and the read -- the read itself
    /// is wrapped in `Read::take(MAX + 1)`, making the byte cap authoritative
    /// at the I/O layer regardless of stale metadata. On Unix, the file is
    /// opened with `O_NONBLOCK` so a FIFO without a writer fails fast
    /// (`ENXIO`) instead of blocking the loader thread forever, which was
    /// the original hang symptom the defect calls out. Mirrors the
    /// TOCTOU-safe `File::open` + `f.metadata()` pattern established in
    /// `crates/zoder-core/src/pricing.rs` (`PricingCatalog::load`).
    ///
    /// The optional alias overlay (`$ZODER_MODEL_ALIASES`, default
    /// `<zoder home>/model-aliases.json`) is folded in afterwards via
    /// [`Self::apply_alias_overlay`]. It can only ever ADD rows and alias
    /// names, and it is never fatal: a missing file is a no-op and a malformed
    /// one warns once and is ignored, so the `Err` contract of this function
    /// stays driven by the flat corpus file alone.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut corpus = Self::load_with_cap(path, Self::MAX_CORPUS_BYTES)?;
        // The overlay is optional, so a corpus saved while one was deployed
        // must still resolve its names from the persisted rows alone. Rebuild
        // before the overlay is folded in: a deployed overlay can then add or
        // override, never erase, what the corpus file already carries.
        corpus.rebuild_aliases_from_models();
        corpus.apply_alias_overlay();
        Ok(corpus)
    }

    /// Internal loader that takes the byte cap as a parameter so tests can
    /// override the production cap to prove the size guard fires without
    /// requiring a multi-GB file. Production callers should use [`Self::load`].
    ///
    /// The cap is enforced at TWO layers, both on the same file descriptor:
    /// 1. `f.metadata().len()` is checked before the read so a comfortably-
    ///    oversized file fails fast with a clear "X bytes exceeds Y cap" error.
    /// 2. `Read::take(max_bytes + 1)` caps the actual bytes read; if the
    ///    file grew between `metadata()` and the read (TOCTOU), the take
    ///    catches it and we still return an explicit `exceeds` error rather
    ///    than silently truncating or OOMing. The `+1` lets us distinguish
    ///    "exactly at the cap" (Ok) from "over the cap" (Err).
    ///
    /// `f.metadata()` rather than a separate `fs::metadata(path)` is what
    /// closes the original TOCTOU window: the same fd is used for stat,
    /// is_file check, size check, AND read. An attacker swapping the path
    /// after `File::open` cannot affect the bytes flowing out of `f`.
    fn load_with_cap(path: &Path, max_bytes: u64) -> anyhow::Result<Self> {
        use std::io::Read;

        // Open the file ONCE. On Unix, request `O_NONBLOCK` so a FIFO without
        // a connected writer returns `ENXIO` immediately instead of blocking
        // the loader thread forever (the original hang symptom). A regular
        // file is unaffected -- `O_NONBLOCK` on a regular file's read end
        // only changes behavior for files opened with `O_WRONLY` semantics,
        // which `O_RDONLY` is not.
        let mut f = Self::open_corpus_file(path)
            .map_err(|e| anyhow::anyhow!("open corpus {}: {e}", path.display()))?;

        // Stat through the same fd that will be read. This closes the
        // metadata-to-read TOCTOU window: a swap of the path on disk
        // after this point cannot affect what comes out of `f`.
        let meta = f
            .metadata()
            .map_err(|e| anyhow::anyhow!("stat corpus {}: {e}", path.display()))?;

        if !meta.is_file() {
            // The path is a FIFO / socket / device / directory (or a
            // symlink to one). Reading such a path would either hang
            // forever (FIFO without a writer even with O_NONBLOCK, on
            // some kernels) or yield non-JSON bytes (device). Reject
            // up-front with a clear error naming the path.
            anyhow::bail!(
                "corpus {} is not a regular file (FIFO/socket/device/directory are not supported)",
                path.display()
            );
        }

        // Layer 1: cheap pre-check from `f.metadata()` so a multi-GB file
        // fails fast with a clear "exceeds N bytes" message before any
        // bytes flow into a buffer. The authoritative check is layer 2
        // below; this is just the friendly early-out.
        if meta.len() > max_bytes {
            anyhow::bail!(
                "corpus {} is {} bytes, which exceeds the {} byte cap ({} MiB); refusing to read into memory",
                path.display(),
                meta.len(),
                max_bytes,
                max_bytes / (1024 * 1024)
            );
        }

        // Layer 2: authoritative byte-cap at the I/O layer. `take(N)` caps
        // the bytes read at N regardless of what `metadata().len()` said.
        // We pass `max_bytes + 1` so we can distinguish "exactly at the
        // cap" (read returns at EOF with `== max_bytes` bytes) from "over
        // the cap" (read returns `== max_bytes + 1` bytes, which means the
        // underlying file is larger than `max_bytes`). This is the SAME
        // idiom used by `ledger.rs` (`MAX_LEDGER_LINE_BYTES + 1`) and is
        // what makes the loader safe against a TOCTOU swap between the
        // metadata check and the read: the read cannot allocate more
        // than `max_bytes + 1` bytes into `buf`, regardless of how the
        // file on disk grew in between.
        let mut buf = String::new();
        let read = (&mut f)
            .take(max_bytes.saturating_add(1))
            .read_to_string(&mut buf)
            .map_err(|e| anyhow::anyhow!("read corpus {}: {e}", path.display()))?;

        // `read` is `buf.len()` after `read_to_string`. If it's `> max_bytes`,
        // the file is genuinely larger than the cap -- either the metadata
        // check was bypassed (TOCTOU) or `metadata().len()` reported stale.
        // Either way, refuse. We compare against `read` (the bytes we
        // actually got) rather than against `meta.len()` so the error
        // message reflects what's in the buffer.
        if read as u64 > max_bytes {
            anyhow::bail!(
                "corpus {} exceeds the {} byte cap ({} MiB) once read; refusing to parse (metadata reported {} bytes)",
                path.display(),
                max_bytes,
                max_bytes / (1024 * 1024),
                meta.len()
            );
        }

        // From here, `buf` is bounded at <= `max_bytes` bytes and the
        // rest of the parser operates on `&str`, so no further allocation
        // can run away. Hand off to the parse tail.
        Self::parse_raw(&buf, path)
    }

    /// Open the corpus file in a TOCTOU-safe way: the returned `File` is the
    /// SAME descriptor used later for `metadata()` and for the bounded read,
    /// so an attacker cannot swap the path between stat and read. On Unix,
    /// the file is opened with `O_NONBLOCK` so a FIFO without a connected
    /// writer fails fast (`ENXIO`) instead of blocking the loader forever.
    /// On non-Unix platforms, the standard blocking `File::open` is used
    /// (non-Unix platforms typically don't expose FIFOs / sockets at the
    /// corpus path, so the hang risk is correspondingly lower).
    fn open_corpus_file(path: &Path) -> std::io::Result<std::fs::File> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                // O_NONBLOCK: opening a FIFO without a writer returns ENXIO
                // immediately rather than blocking. Regular files are
                // unaffected. Constant comes from `libc` via the std
                // re-export; this avoids pulling the `libc` crate as a direct
                // dependency for a single flag.
                .custom_flags(libc::O_NONBLOCK)
                .open(path)
        }
        #[cfg(not(unix))]
        {
            std::fs::File::open(path)
        }
    }

    /// Tail of the loader: parse `raw` JSON and return a populated `Corpus`.
    /// Split out from `load_with_cap` so the TOCTOU-safe byte-cap path
    /// (which delivers bounded `&str`) shares its parser with any future
    /// callers that already have a vetted `String` in hand.
    fn parse_raw(raw: &str, path: &Path) -> anyhow::Result<Self> {
        // Lenient parse: deserialize the models array element-by-element so one
        // bad entry doesn't take down every command. `id` stays required.
        let root: serde_json::Value = serde_json::from_str(raw)
            .map_err(|e| anyhow::anyhow!("corpus {} is not valid JSON: {e}", path.display()))?;

        let str_field = |k: &str| {
            root.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };

        let mut models = Vec::new();
        let mut skipped = 0usize;
        let arr = match root.get("models").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => anyhow::bail!("corpus {}: missing `models` array", path.display()),
        };
        let input_len = arr.len();
        for item in arr {
            match serde_json::from_value::<ModelEntry>(item.clone()) {
                Ok(m) if !m.id.is_empty() => models.push(m),
                _ => skipped += 1,
            }
        }
        if skipped > 0 {
            eprintln!("zoder: warning: corpus skipped {skipped} invalid model entr{} (missing/invalid `id` or fields)",
                if skipped == 1 { "y" } else { "ies" });
        }

        // C5-3: a NON-EMPTY `models` array that yields zero usable entries (every
        // element failed to deserialize or had an empty `id`) is a corrupt/misshaped
        // corpus, not a legitimately-empty one. Fail loudly instead of returning a
        // silent empty corpus that later surfaces as a confusing "no healthy free
        // model" far downstream. A genuinely-empty `{"models":[]}` (input_len == 0)
        // still loads Ok as an empty corpus.
        if input_len > 0 && models.is_empty() {
            anyhow::bail!(
                "corpus {} parsed to 0 usable entries from {input_len} input entr{} (all had a missing/invalid `id` or failed to parse)",
                path.display(),
                if input_len == 1 { "y" } else { "ies" }
            );
        }

        // C5-2: de-duplicate by `id`, LAST-WINS. The corpus file itself can carry
        // two rows for one id (e.g. a stale/retired row plus a freshly-benched one);
        // load() previously pushed both, so get()/ingest_free_chat's `find` returned
        // the FIRST (stale) row and a doubly-listed id could appear twice in the
        // fallback chain. Keeping the last occurrence lets a later (fresher) row win
        // while preserving the original relative order of the survivors.
        Self::dedup_by_id_last_wins(&mut models, |m: &ModelEntry| m.id.as_str());

        let mut corpus = Corpus {
            source: str_field("source"),
            arena_date: str_field("arena_date"),
            count: models.len(),
            models,
            aliases: BTreeMap::new(),
        };
        // Includes the legitimately-empty `{"models":[]}` path: rebuilding an
        // empty set is a no-op, but every Corpus that leaves this parser has
        // been through the same invariant.
        corpus.rebuild_aliases_from_models();
        Ok(corpus)
    }

    /// De-duplicate `models` by `id`, keeping the LAST occurrence of each id and
    /// preserving the relative order of the surviving entries. Used by `load()`
    /// so a corpus file with duplicate ids collapses to one entry per id.
    fn dedup_by_id_last_wins<T>(models: &mut Vec<T>, id_of: fn(&T) -> &str) {
        // Index of the last occurrence of each id.
        let mut last: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for (i, m) in models.iter().enumerate() {
            last.insert(id_of(m), i);
        }
        if last.len() == models.len() {
            return; // no duplicates, avoid the allocation/rebuild
        }
        let mut keep = vec![false; models.len()];
        for &i in last.values() {
            keep[i] = true;
        }
        let mut idx = 0usize;
        models.retain(|_| {
            let k = keep[idx];
            idx += 1;
            k
        });
    }

    /// Look up a model by id, accepting an overlay alias for the id.
    ///
    /// An EXACT corpus id always wins; only when nothing in the corpus matches
    /// is the alias map consulted, and then only on an exact key match — never
    /// a prefix, substring, case-folded or fuzzy match, so `codex` never
    /// resolves to `codex:sol` and a typo never silently routes somewhere else.
    pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        if let Some(m) = self.models.iter().find(|m| m.id == id) {
            return Some(m);
        }
        let target = self.aliases.get(id)?;
        self.models.iter().find(|m| &m.id == target)
    }

    /// Resolve an operator-supplied model id through the alias overlay.
    ///
    /// Returns the canonical model id for an exact alias hit, and `id`
    /// unchanged in every other case — including when the corpus already knows
    /// a model by exactly that name (an exact id always wins over an alias) and
    /// when nothing matches at all. Callers that then need the entry can chain
    /// [`Self::get`], which applies the same rule.
    ///
    /// The fast path is the alias lookup itself: an id that is not an alias
    /// needs no scan of the model list at all.
    pub fn resolve_alias<'a>(&'a self, id: &'a str) -> &'a str {
        let Some(target) = self.aliases.get(id) else {
            return id;
        };
        if self.models.iter().any(|m| m.id == id) {
            // A real model id shadows an alias of the same name.
            return id;
        }
        target.as_str()
    }

    /// Rebuild the alias index from the `aliases` list persisted on each model
    /// row.
    ///
    /// The overlay file is optional, but the names it taught a row are not: a
    /// corpus saved while an overlay was deployed must keep resolving them
    /// after a refresh with no overlay present. Deriving the map from the rows
    /// (rather than trusting the serialized map) means the rows are the single
    /// source of truth and a hand-edited/older corpus works the same way.
    ///
    /// Names are taken verbatim after trimming: exact matches only, with no
    /// prefix, substring, case-fold or namespace stripping. On a collision the
    /// FIRST row that declares the alias wins, so a reload is deterministic.
    /// [`Self::resolve_alias`] and [`Self::get`] still give an exact model id
    /// precedence over an alias spelled the same way.
    fn rebuild_aliases_from_models(&mut self) {
        let mut aliases = BTreeMap::new();
        for model in &self.models {
            for alias in &model.aliases {
                let alias = alias.trim();
                if alias.is_empty() || alias == model.id {
                    continue;
                }
                aliases
                    .entry(alias.to_string())
                    .or_insert_with(|| model.id.clone());
            }
        }
        self.aliases = aliases;
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
    /// the caller, e.g. an Enterprise LLM Gateway serving `enterprise/* | provider-a/* | provider-b/* |
    /// mistralai/*` open-weight NIMs) is upserted as a free, routable chat
    /// candidate. Existing entries keep all benchmark/capability/latency scores
    /// — only the free/route flags are (re)asserted, so re-running a refresh is
    /// idempotent and never loses bench data. A served id inherits quality from
    /// an exact normalized match in another namespace; only a genuinely
    /// unbenched id gets the neutral prior. Returns the number of entries newly
    /// promoted into the routing pool.
    pub fn ingest_free_chat(&mut self, ids: &[String]) -> usize {
        const UNBENCHED_PRIOR: f64 = 0.5;
        let mut promoted = 0usize;
        for id in ids {
            let key = ModelEntry::normalized_model_key(id);
            // Multiple feeds can describe the same leaf id. Highest `w_swe`
            // wins because it is the routing-relevant quality signal; id ASC
            // is the deterministic tie-breaker so Vec/map ingestion order can
            // never change which quality row is inherited. Missing `w_swe`
            // sorts below a measured value.
            let quality_source = self
                .models
                .iter()
                .filter(|candidate| {
                    candidate.id != *id
                        && candidate.has_quality_signal()
                        && ModelEntry::normalized_model_key(&candidate.id) == key
                })
                .min_by(|a, b| {
                    match (a.w_swe, b.w_swe) {
                        (Some(a_score), Some(b_score)) => b_score.total_cmp(&a_score),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => std::cmp::Ordering::Equal,
                    }
                    .then_with(|| a.id.cmp(&b.id))
                })
                .cloned();

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
                let has_only_prior = m.code_capability().is_none()
                    && (m.agentic_score.is_none() || m.agentic_score == Some(UNBENCHED_PRIOR));
                if has_only_prior {
                    if let Some(source) = &quality_source {
                        m.inherit_quality_from(source);
                    } else if m.agentic_score.is_none() {
                        m.agentic_score = Some(UNBENCHED_PRIOR);
                    }
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
                if let Some(source) = &quality_source {
                    e.inherit_quality_from(source);
                } else {
                    e.agentic_score = Some(UNBENCHED_PRIOR);
                }
                self.models.push(e);
                promoted += 1;
            }
        }
        self.count = self.models.len();
        promoted
    }

    /// `id` accessor for [`Self::dedup_by_id_last_wins`], which is generic over
    /// the row type so the same last-wins rule serves the corpus file and the
    /// overlay file.
    fn overlay_id_of(m: &OverlayModel) -> &str {
        m.id.as_str()
    }

    /// Where the optional alias overlay lives: `$ZODER_MODEL_ALIASES` when set
    /// to a non-empty value, else `<zoder home>/model-aliases.json`. Reading
    /// the zoder home (not `dirs::home_dir()` directly) keeps a relocated
    /// `ZODER_HOME` authoritative for the overlay too.
    pub fn alias_overlay_path() -> PathBuf {
        match std::env::var_os(MODEL_ALIASES_ENV) {
            Some(p) if !p.is_empty() => PathBuf::from(p),
            _ => crate::config::Config::home().join(MODEL_ALIASES_FILE),
        }
    }

    /// Fold the optional alias overlay into this corpus.
    ///
    /// Never fails and never panics: no overlay deployed is a silent no-op, and
    /// a malformed/oversized/unreadable one warns once on stderr and is
    /// ignored, so a bad generator run can never take startup down. See the
    /// module docs for the two invariants (metadata-only additions, corpus
    /// data always wins).
    pub fn apply_alias_overlay(&mut self) -> OverlayReport {
        self.apply_alias_overlay_from(&Self::alias_overlay_path())
    }

    /// [`Self::apply_alias_overlay`] against an explicit `path`, so a caller
    /// (or a test) can point at a specific overlay without touching the process
    /// environment. Ingest is additive and idempotent: applying the same
    /// overlay twice adds nothing the second time.
    pub fn apply_alias_overlay_from(&mut self, path: &Path) -> OverlayReport {
        let mut report = OverlayReport::default();

        let body = match Self::read_overlay_body(path) {
            OverlayRead::Missing => return report,
            OverlayRead::Broken(why) => {
                warn_overlay_once(format_args!(
                    "model alias overlay {} is unusable ({why}); ignoring it — the corpus is \
                     unchanged and no alias resolves",
                    path.display()
                ));
                return report;
            }
            OverlayRead::Body(body) => body,
        };

        let mut overlay = match serde_json::from_str::<AliasOverlay>(&body) {
            Ok(overlay) => overlay,
            Err(e) => {
                warn_overlay_once(format_args!(
                    "model alias overlay {} is malformed ({e}); ignoring it — the corpus is \
                     unchanged and no alias resolves",
                    path.display()
                ));
                return report;
            }
        };
        report.path = Some(path.to_path_buf());

        // Lenient per-row handling, mirroring the corpus loader: one unusable
        // overlay row must not discard the whole file. A non-empty `models`
        // array that yields zero usable rows is a BROKEN file rather than a
        // legitimately empty one (same C5-3 reasoning as `parse_raw`).
        let input_rows = overlay.models.len();
        overlay.models.retain(|m| !m.id.trim().is_empty());
        Self::dedup_by_id_last_wins(&mut overlay.models, Self::overlay_id_of);
        if overlay.models.is_empty() && input_rows > 0 {
            warn_overlay_once(format_args!(
                "model alias overlay {} has {input_rows} model row{} but none carried a usable \
                 `id`; ignoring it — the corpus is unchanged",
                path.display(),
                if input_rows == 1 { "" } else { "s" },
            ));
            return OverlayReport::default();
        }
        if overlay.models.len() < input_rows {
            warn_overlay_once(format_args!(
                "model alias overlay {} skipped {} model row{} with a missing/empty `id`; the \
                 remaining rows were still ingested",
                path.display(),
                input_rows - overlay.models.len(),
                if input_rows - overlay.models.len() == 1 {
                    ""
                } else {
                    "s"
                },
            ));
        }

        // Alias map first: per-model `aliases` are the common case, and the
        // generator's explicit map is the resolution authority, so it is
        // applied last and overrides any collision.
        let mut aliases = std::mem::take(&mut self.aliases);

        for om in &overlay.models {
            let id = om.id.trim();
            let row_aliases: Vec<&str> = om
                .aliases
                .iter()
                .map(|a| a.trim())
                .filter(|a| !a.is_empty() && *a != id)
                .collect();
            for alias in &row_aliases {
                aliases
                    .entry((*alias).to_string())
                    .or_insert_with(|| id.to_string());
            }

            match self.models.iter_mut().find(|m| m.id == id) {
                Some(entry) => {
                    // Known model: the corpus keeps EVERY field it has (scores,
                    // economics, classification, routing flags). The overlay
                    // only teaches this row the extra names it answers to.
                    let before = entry.aliases.len();
                    for alias in &row_aliases {
                        if !entry.aliases.iter().any(|a| a.as_str() == *alias) {
                            entry.aliases.push((*alias).to_string());
                        }
                    }
                    if entry.aliases.len() > before {
                        report.aliased.push(id.to_string());
                    }
                }
                None => {
                    // Unknown id: add it as METADATA ONLY. `route_candidate`
                    // stays false, free/paid stay false and no score, Elo or
                    // price is invented, so the row is visible to `zoder
                    // models`, `-m`, and health/pricing lookups but can never be
                    // auto-selected, ranked or billed from an alias file.
                    let mut entry = ModelEntry::from_served_id(id);
                    entry.route_candidate = false;
                    entry.free = false;
                    entry.paid = false;
                    entry.aliases = row_aliases.iter().map(|a| (*a).to_string()).collect();
                    entry.source = Some(OVERLAY_SOURCE.into());
                    entry.gated_reason = Some(
                        "overlay: known id only — needs classification + bench + pricing before \
                         it can route (run corpus builder)"
                            .into(),
                    );
                    // The generator's provider id is the only provenance the
                    // overlay carries; surface it as the family for display.
                    // It is metadata, not a routing or billing decision.
                    let provider = om.provider.trim();
                    if !provider.is_empty() {
                        entry.family = provider.to_string();
                    }
                    self.models.push(entry);
                    report.added.push(id.to_string());
                }
            }
        }

        for (alias, target) in &overlay.aliases {
            let (alias, target) = (alias.trim(), target.trim());
            if alias.is_empty() || target.is_empty() {
                warn_overlay_once(format_args!(
                    "model alias overlay {} maps an empty name to an empty id; skipping that entry",
                    path.display(),
                ));
                continue;
            }
            aliases.insert(alias.to_string(), target.to_string());
        }

        report.alias_count = aliases.len();
        self.aliases = aliases;
        self.count = self.models.len();
        report
    }

    /// Bounded, non-blocking read of the overlay body, on the same posture as
    /// the corpus loader: regular files only (a FIFO must fail fast, not hang),
    /// capped at [`Self::MAX_ALIAS_OVERLAY_BYTES`] at BOTH the metadata layer
    /// and the `Read::take(cap + 1)` layer so a swap between stat and read
    /// cannot produce an unbounded allocation. `Missing` is deliberately a
    /// distinct outcome from `Broken`: no overlay deployed is the normal case
    /// and must stay silent.
    fn read_overlay_body(path: &Path) -> OverlayRead {
        use std::io::Read;

        let mut f = match Self::open_corpus_file(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return OverlayRead::Missing,
            Err(e) => return OverlayRead::Broken(e.to_string()),
        };
        let meta = match f.metadata() {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return OverlayRead::Missing,
            Err(e) => return OverlayRead::Broken(e.to_string()),
        };
        if !meta.is_file() {
            return OverlayRead::Broken("not a regular file".into());
        }
        if meta.len() > Self::MAX_ALIAS_OVERLAY_BYTES {
            return OverlayRead::Broken(format!(
                "{} bytes exceeds the {} byte cap",
                meta.len(),
                Self::MAX_ALIAS_OVERLAY_BYTES
            ));
        }
        let mut buf = String::new();
        match (&mut f)
            .take(Self::MAX_ALIAS_OVERLAY_BYTES + 1)
            .read_to_string(&mut buf)
        {
            Ok(_) if buf.len() as u64 > Self::MAX_ALIAS_OVERLAY_BYTES => OverlayRead::Broken(
                format!("grew past the {} byte cap", Self::MAX_ALIAS_OVERLAY_BYTES),
            ),
            Ok(_) => OverlayRead::Body(buf),
            Err(e) => OverlayRead::Broken(e.to_string()),
        }
    }

    /// Persist atomically (unique temp file + rename).
    ///
    /// C5-4 (= S19 for the corpus): the temp filename carries the process id AND
    /// a monotonic nonce (`<stem>.json.tmp.<pid>.<nonce>`) so two overlapping
    /// refreshes can never share a temp path — otherwise an interleaved
    /// write+rename could promote a torn or foreign temp file over the live
    /// corpus. The temp is removed on any error so a failed write never litters
    /// the dir with a half-written file a later reader could pick up. Mirrors the
    /// `write_atomic` pattern in `crates/model-health/src/lib.rs` (kept
    /// self-contained here — no cross-crate dependency).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let data = serde_json::to_vec_pretty(self)?;
        let nonce = SAVE_NONCE.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), nonce));
        // Write to the unique temp; on any failure remove it so it can never be
        // renamed over the live corpus or left behind torn.
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

#[cfg(test)]
mod corpus_io_tests {
    use super::*;
    use std::io::Write;

    /// Process-wide mutex around `$ZODER_MODEL_ALIASES`, which `Corpus::load`
    /// reads. EVERY test in this binary that mutates that variable — or that
    /// calls `Corpus::load` and then asserts exact model counts — must hold it,
    /// otherwise a developer's real `~/.zoder/model-aliases.json` (or a sibling
    /// test's overlay) can change what `load` returns. Same contract as
    /// `zoder-cli`'s `test_env::ENV_LOCK` for `ZODER_HOME`.
    static OVERLAY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// RAII override of `$ZODER_MODEL_ALIASES`. Holds [`OVERLAY_ENV_LOCK`] for
    /// its lifetime and restores the previous value on drop, so the env is only
    /// redirected for the duration of one test.
    struct AliasEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev: Option<std::ffi::OsString>,
    }

    impl AliasEnvGuard {
        /// Point the overlay lookup at `path`.
        fn new(path: &Path) -> Self {
            let lock = OVERLAY_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
            let prev = std::env::var_os(MODEL_ALIASES_ENV);
            std::env::set_var(MODEL_ALIASES_ENV, path);
            Self { _lock: lock, prev }
        }

        /// Pin the overlay to a path that does not exist — the "no overlay
        /// deployed" state, made explicit so a test asserting on the corpus
        /// shape can never pick up the machine's real overlay.
        fn none() -> Self {
            let mut missing = std::env::temp_dir();
            missing.push(format!(
                "zoder-absent-overlay-{}-{}",
                std::process::id(),
                SAVE_NONCE.fetch_add(1, Ordering::Relaxed)
            ));
            Self::new(&missing)
        }
    }

    impl Drop for AliasEnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(MODEL_ALIASES_ENV, v),
                None => std::env::remove_var(MODEL_ALIASES_ENV),
            }
        }
    }

    fn tmpdir() -> std::path::PathBuf {
        let mut d = std::env::temp_dir();
        let uniq = format!(
            "zoder-corpus-test-{}-{}",
            std::process::id(),
            SAVE_NONCE.fetch_add(1, Ordering::Relaxed)
        );
        d.push(uniq);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_file(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    // C5-2: two entries sharing an id load to ONE entry (last-wins), and get()
    // returns the surviving (last) one.
    #[test]
    fn duplicate_ids_collapse_last_wins() {
        let dir = tmpdir();
        // Two rows for "vendor/dup": the first is stale (low score), the second
        // is fresh (high score). Last-wins must keep the fresh one. A distinct
        // "vendor/other" row is kept to prove relative order survives.
        let body = r#"{
            "source": "test",
            "models": [
                {"id": "vendor/dup", "agentic_score": 0.10},
                {"id": "vendor/other", "agentic_score": 0.50},
                {"id": "vendor/dup", "agentic_score": 0.90}
            ]
        }"#;
        let p = write_file(&dir, "corpus.json", body);
        // `load` also folds in the optional alias overlay; pin it to "absent"
        // so this test measures the corpus file alone.
        let _env = AliasEnvGuard::none();
        let c = Corpus::load(&p).unwrap();

        // Exactly one entry for the duplicated id.
        let dups: Vec<_> = c.models.iter().filter(|m| m.id == "vendor/dup").collect();
        assert_eq!(
            dups.len(),
            1,
            "duplicate id must collapse to a single entry"
        );

        // The surviving entry is the LAST occurrence (the fresh score).
        assert_eq!(c.get("vendor/dup").unwrap().agentic_score, Some(0.90));
        // The other id is untouched and still present.
        assert_eq!(c.get("vendor/other").unwrap().agentic_score, Some(0.50));
        // Total count reflects de-dup.
        assert_eq!(c.models.len(), 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    // C5-3: a non-empty file where every entry is unusable -> load() returns Err.
    #[test]
    fn all_invalid_entries_is_err() {
        let dir = tmpdir();
        // Two entries, both unusable: one has an empty id, one is missing `id`
        // entirely (fails ModelEntry deser). Input was non-empty, usable == 0.
        let body = r#"{
            "models": [
                {"id": ""},
                {"host": "vendor", "leaf": "no-id"}
            ]
        }"#;
        let p = write_file(&dir, "corpus.json", body);
        let _env = AliasEnvGuard::none();
        let err = Corpus::load(&p).expect_err("all-invalid corpus must be an error");
        assert!(
            err.to_string().contains("0 usable entries"),
            "unexpected error: {err}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // C5-3: a legitimately-empty `{"models":[]}` still loads Ok as an empty corpus.
    #[test]
    fn legitimately_empty_models_is_ok() {
        let dir = tmpdir();
        let p = write_file(&dir, "corpus.json", r#"{"models": []}"#);
        let _env = AliasEnvGuard::none();
        let c = Corpus::load(&p).expect("empty models array must load Ok");
        assert_eq!(c.models.len(), 0);
        assert_eq!(c.count, 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    // C5-4: save() uses a unique temp filename and leaves no stray `.json.tmp`.
    #[test]
    fn save_uses_unique_temp_and_leaves_no_stray_tmp() {
        let dir = tmpdir();
        let path = dir.join("corpus.json");
        let c = Corpus {
            source: "test".into(),
            models: vec![ModelEntry {
                id: "vendor/m".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        c.save(&path).unwrap();

        // The corpus is present and reloads.
        assert!(path.exists());
        let _env = AliasEnvGuard::none();
        let reloaded = Corpus::load(&path).unwrap();
        assert_eq!(reloaded.models.len(), 1);

        // No deterministic `<stem>.json.tmp` and no per-writer temp remains: the
        // temp must be uniquely named AND renamed/removed, so only `corpus.json`
        // (plus any test artifacts) is left. Assert nothing matching `.json.tmp`
        // survives in the dir.
        for entry in std::fs::read_dir(&dir).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.contains(".json.tmp"),
                "stray temp file left behind: {name}"
            );
        }
        // Belt-and-suspenders: the legacy deterministic temp path never exists.
        assert!(!path.with_extension("json.tmp").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    // C5-1: a moderately-sized fixture (well under the production 8 MiB cap) is
    // rejected when the test uses a tighter cap, proving the size guard fires
    // BEFORE the read. The same fixture loads Ok under the production cap, so
    // we know the rejection is the cap firing and not content rejection. The
    // error message must name the path, the cap, and the byte counts so an
    // operator can diagnose a runaway corpus without re-running with --debug.
    #[test]
    fn load_rejects_oversized_under_test_cap_and_accepts_under_production_cap() {
        let dir = tmpdir();
        // Build a fixture with many small entries so the body is ~9 KiB -- well
        // under MAX_CORPUS_BYTES (8 MiB) so production `load` accepts it, but
        // over the 4 KiB test cap so the guard demonstrably fires. Using a
        // moderately-sized fixture (vs. a multi-GB file) keeps the test cheap
        // while still proving the cap-vs-content rejection.
        let mut body = String::with_capacity(8 * 1024);
        body.push_str(r#"{"source":"cap-test","models":["#);
        for i in 0..200 {
            if i > 0 {
                body.push(',');
            }
            // Each entry is ~45 bytes; 200 entries ~9 KiB.
            body.push_str(&format!(
                r#"{{"id":"vendor/m{i:04}","host":"vendor","leaf":"m{i:04}"}}"#
            ));
        }
        body.push_str("]}");
        let on_disk = write_file(&dir, "corpus.json", &body);
        let on_disk_size = std::fs::metadata(&on_disk).unwrap().len();
        assert!(
            on_disk_size > 4096 && on_disk_size < Corpus::MAX_CORPUS_BYTES,
            "fixture must be over the test cap (4 KiB) and under the production cap (8 MiB); got {on_disk_size} bytes"
        );

        // 1) Tight cap -> Err naming the size violation. The TOCTOU-safe
        //    loader fires either at the metadata() layer (fast path) or at
        //    the Read::take(N+1) layer (authoritative); both branches end in
        //    an anyhow error mentioning the cap, so we accept either wording.
        let err = Corpus::load_with_cap(&on_disk, 4096)
            .expect_err("oversized fixture must be rejected under a 4 KiB cap");
        let msg = err.to_string();
        assert!(
            msg.contains("exceeds") && msg.contains("byte cap"),
            "error must name the size cap, got: {msg}"
        );
        assert!(
            msg.contains(&on_disk.display().to_string()),
            "error must name the path, got: {msg}"
        );
        assert!(
            msg.contains("4096"),
            "error must name the cap (4096), got: {msg}"
        );

        // 2) Same fixture, production cap -> Ok with all 200 entries.
        let _env = AliasEnvGuard::none();
        let c = Corpus::load(&on_disk).expect("fixture under 8 MiB must load Ok");
        assert_eq!(c.models.len(), 200);
        assert_eq!(c.count, 200);

        std::fs::remove_dir_all(&dir).ok();
    }

    // C5-1: a non-regular path (directory) is rejected with a clear error
    // naming the path, BEFORE any read attempt. Without the is_file guard,
    // `read_to_string` on a directory would either fail with a noisy OS error
    // (Linux) or, in the FIFO case, hang forever -- the exact symptom the
    // defect calls out.
    #[test]
    fn load_rejects_non_regular_path_with_clear_error() {
        let dir = tmpdir();
        // Point the loader at the directory itself. `read_to_string` on a
        // directory would either error with EISDIR or, on some platforms,
        // read the directory's raw bytes -- neither is what we want. The
        // guard must trip on is_file() and bail with a clear message.
        let _env = AliasEnvGuard::none();
        let err = Corpus::load(&dir).expect_err("loading a directory as a corpus must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("not a regular file"),
            "error must name the path-type rejection reason, got: {msg}"
        );
        assert!(
            msg.contains(&dir.display().to_string()),
            "error must name the path, got: {msg}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // C5-1 (TOCTOU SAFETY): after `File::open`, the metadata check and the
    // bounded read BOTH operate on the same fd, so swapping the path on
    // disk between them cannot bypass the byte cap. The authoritative
    // cap is the `Read::take(max_bytes + 1)` wrapper around the read:
    // even if `f.metadata().len()` returned a stale small size (because
    // the file grew between stat and read), the take wrapper itself
    // refuses to read more than `max_bytes + 1` bytes, and the post-read
    // `read > max_bytes` check turns that into an explicit "exceeds"
    // error rather than a silent truncation or OOM.
    //
    // Two-part test:
    //   (a) DETERMINISTIC boundary: a file whose size at metadata time
    //       is *just* over the cap triggers the take-layer rejection
    //       even though the read-layer caps the read at `cap + 1`.
    //       This exercises the `read > cap` post-read check directly,
    //       without any race.
    //   (b) STATISTICAL race: a writer thread continuously grows a file
    //       while the loader runs. Across many iterations, the loader
    //       must never accept an over-cap body. This is not 100%
    //       deterministic on contended CI, so we only assert that
    //       every iteration where the file *was* over cap at read time
    //       (which we can check after the fact by re-stating the file)
    //       rejected.
    //
    // In practice (b) is observed to reject 100% of the time on a
    // single-threaded test runner; under heavy contention a small
    // fraction of iterations may legitimately see a small file at
    // both metadata and read times, which is correct behavior (the
    // file isn't actually over cap when the loader reads it).
    #[test]
    fn load_rejects_toctou_file_growth_via_read_take_cap() {
        use std::io::Write;
        use std::path::PathBuf;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::thread;
        use std::time::Duration;

        let dir = tmpdir();
        let target_path: PathBuf = dir.join("corpus.json");

        // ---------- (a) DETERMINISTIC BOUNDARY ----------
        // File is exactly `cap + 1` bytes long. `metadata().len()` returns
        // `cap + 1 > cap`, so the metadata layer fires (we still get
        // a cap-related error). Then we test the take-layer DIRECTLY by
        // manually opening the file and reading through `take(cap + 1)`:
        // the take wrapper delivers exactly `cap + 1` bytes, and our
        // post-read `read > cap` check turns that into an explicit
        // "exceeds" error. This is the canonical cap-vs-content test.
        let cap_a: u64 = 256;
        let boundary_body = {
            // `cap + 1` valid UTF-8 bytes (so read_to_string succeeds;
            // the cap layer is what rejects, not UTF-8 validation).
            let mut s = String::with_capacity((cap_a + 1) as usize);
            s.push_str(r#"{"models":["#);
            for _ in 0..((cap_a + 1) as usize - s.len() - 2) {
                s.push('A');
            }
            s.push_str("]}");
            assert_eq!(s.len() as u64, cap_a + 1, "fixture must be cap+1 bytes");
            s
        };
        let boundary_path = dir.join("boundary.json");
        std::fs::write(&boundary_path, &boundary_body).unwrap();
        assert_eq!(
            std::fs::metadata(&boundary_path).unwrap().len(),
            cap_a + 1,
            "boundary fixture must be exactly cap+1 bytes on disk"
        );

        // The production `load_with_cap` MUST reject this, regardless
        // of which cap-layer fires. Both error messages contain
        // "exceeds" and "byte cap".
        let err = Corpus::load_with_cap(&boundary_path, cap_a)
            .expect_err("file at cap+1 must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("exceeds") && msg.contains("byte cap"),
            "boundary: error must name the size cap, got: {msg}"
        );

        // ---------- (b) STATISTICAL RACE ----------
        // Continuous writer thread grows the file while the loader runs.
        // The writer writes 4 KiB chunks fast enough that the file is
        // reliably over the 4 KiB cap within the loader's metadata-to-read
        // window. Across all iterations the loader must reject any
        // file that is over cap at read time.
        let cap_b: u64 = 4 * 1024;
        let stop = Arc::new(AtomicBool::new(false));
        let iterations: usize = 100;
        for i in 0..iterations {
            // Reset the file to a small under-cap body.
            let small_body = r#"{"source":"toctou","models":[{"id":"vendor/ok","leaf":"x"}]}"#;
            std::fs::write(&target_path, small_body).unwrap();
            assert!(
                std::fs::metadata(&target_path).unwrap().len() < cap_b,
                "fixture must start under the cap on every iteration"
            );

            // Writer thread: keep appending 4 KiB chunks until the file
            // is well past the cap OR `stop` is signalled. Brief sleeps
            // (10µs) let the loader interleave between chunks, so some
            // iterations genuinely exercise the take-layer rejection.
            let stop_w = stop.clone();
            let target_w = target_path.clone();
            let writer = thread::spawn(move || {
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&target_w)
                    .expect("writer open");
                let chunk = vec![b'A'; 4 * 1024];
                let mut total = 0usize;
                while !stop_w.load(Ordering::Relaxed) && total < 64 * 1024 {
                    f.write_all(&chunk).expect("writer append");
                    total += chunk.len();
                    thread::sleep(Duration::from_micros(10));
                }
            });

            // Brief warmup so the writer has begun appending before we
            // start the load. After this, the file is reliably a few
            // KiB (still under cap) on every iteration; the file
            // crosses the cap during the loader's read phase.
            thread::sleep(Duration::from_micros(100));
            match Corpus::load_with_cap(&target_path, cap_b) {
                Err(e) => {
                    let msg = e.to_string();
                    // Two legitimate rejection shapes race here, both
                    // correct: (a) the take-layer cap catches the read
                    // once the file has grown past `cap_b` ("exceeds ...
                    // byte cap"), or (b) the read lands mid-append -- the
                    // writer's in-flight 4 KiB chunk is torn between the
                    // valid JSON prefix and raw `A` filler -- producing a
                    // syntactically invalid-but-still-under-cap read that
                    // `serde_json` rejects as malformed. Both are the
                    // guard correctly refusing a file that changed under
                    // it; only silently ACCEPTING torn/oversized content
                    // would be the real defect.
                    let named_cap = msg.contains("exceeds") && msg.contains("byte cap");
                    let torn_read = msg.contains("not valid JSON");
                    assert!(
                        named_cap || torn_read,
                        "iteration {i}: error must name the size cap or reject a torn/invalid \
                         read, got: {msg}"
                    );
                    if named_cap {
                        assert!(
                            msg.contains(&target_path.display().to_string()),
                            "iteration {i}: error must name the path, got: {msg}"
                        );
                    }
                }
                Ok(c) => {
                    // The loader accepted. The take-layer invariant is:
                    // the parsed body must contain <= cap bytes of
                    // serialized JSON. The writer may grow the file
                    // AFTER the loader's read completes (no TOCTOU bug
                    // there -- the loader already finished reading); we
                    // can't observe the file size at read time from
                    // outside, so we verify the byte cap via the
                    // loader's own output: serialize the parsed corpus
                    // back to JSON and assert it's <= cap bytes. If the
                    // take layer failed (e.g. cap wasn't enforced on the
                    // read), the loader would return a corpus whose
                    // serialized form is >> cap bytes.
                    let reencoded = serde_json::to_vec(&serde_json::json!({
                        "source": c.source,
                        "arena_date": c.arena_date,
                        "count": c.count,
                        "models": c.models,
                    }))
                    .unwrap_or_default();
                    if reencoded.len() as u64 > cap_b {
                        panic!(
                            "iteration {i}: BUG -- loader returned Ok with a body that \
                             serializes to {} bytes (cap is {cap_b}). TOCTOU regression in \
                             the take-layer cap.",
                            reencoded.len()
                        );
                    }
                }
            }

            // Signal the writer to stop, then join so the file handle
            // is closed before the next iteration resets the file.
            stop.store(true, Ordering::Relaxed);
            writer.join().expect("writer thread must not panic");
            stop.store(false, Ordering::Relaxed);
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    // C5-1 (FIFO HANG): a FIFO at the corpus path must fail fast, NOT
    // block the loader forever. With `O_NONBLOCK`, opening a FIFO without
    // a connected writer returns `ENXIO` immediately, which surfaces as
    // `io::ErrorKind::NotFound` or `WouldBlock` depending on the kernel.
    // Either way the loader must return `Err`, not hang.
    //
    // We use a timeout race: if the loader doesn't return within 5
    // seconds, we treat it as a hang and fail the test. The test is
    // skipped on non-Unix platforms (where `O_NONBLOCK` isn't applied).
    #[cfg(unix)]
    #[test]
    fn load_rejects_fifo_without_hanging() {
        use std::os::unix::fs::OpenOptionsExt;
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let dir = tmpdir();
        let fifo_path = dir.join("corpus.fifo");
        // mkfifo via libc -- std doesn't expose it directly.
        use std::os::unix::ffi::OsStrExt;
        let cstr = std::ffi::CString::new(fifo_path.as_os_str().as_bytes()).unwrap();
        let rc = unsafe { libc::mkfifo(cstr.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());

        // Open the FIFO read-end with O_NONBLOCK|O_RDONLY just to verify
        // it would normally block; we DON'T keep this fd open for the
        // loader (otherwise the loader's open-with-O_NONBLOCK would
        // succeed because a reader is connected). For the test, we want
        // the loader to encounter an "ENXIO / no writer" condition, so
        // we close this immediately.
        drop({
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo_path)
                .expect("nonblocking open of unread fifo for setup")
        });

        // Race the loader against a 5-second timeout. If the loader
        // hangs (the original defect symptom), the timeout fires and
        // the test fails with a clear "loader hung on FIFO" message.
        let path = fifo_path.clone();
        let (tx, rx) = mpsc::channel();
        let loader = thread::spawn(move || {
            let result = Corpus::load(&path);
            let _ = tx.send(result);
        });

        let result = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("loader must not hang on a FIFO without a writer (C5-1 fix)");
        // The loader may have already returned Err before we recv'd;
        // either way, we MUST have an Err here, never Ok.
        let err = result.expect_err("loading a FIFO must fail, never Ok");
        let msg = err.to_string();
        // The message will be the open() failure (ENXIO surfaces as
        // io::ErrorKind::NotFound on most Linux kernels for FIFO open
        // with O_NONBLOCK) wrapped in our "open corpus {}: ..." prefix.
        assert!(
            msg.contains("open corpus") || msg.contains("not a regular file"),
            "FIFO load error must mention the open-failure or path-type guard, got: {msg}"
        );
        assert!(
            msg.contains(&fifo_path.display().to_string()),
            "error must name the path, got: {msg}"
        );

        // Cleanup: unlink the FIFO so the temp dir can be removed.
        // (rm will fail if a writer is still attached, but in this test
        // there is no writer -- the loader was rejected on open.)
        let _ = loader.join();
        let cstr = std::ffi::CString::new(fifo_path.as_os_str().as_bytes()).unwrap();
        unsafe {
            libc::unlink(cstr.as_ptr());
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------------------------------------------------------------------
    // Provider-neutral alias overlay (see the module docs).
    //
    // The fixture deliberately carries every field the external generator
    // emits — `generated_at`, `source`, `display_name`, `created_at`,
    // `codex_ladder` — to pin that zoder tolerates (and ignores) the full
    // documented shape instead of only the subset it consumes.
    // ---------------------------------------------------------------------

    const OVERLAY: &str = r#"{
        "generated_at": "2026-10-17T00:00:00Z",
        "source": {"generator": "fleetops"},
        "models": [
            {
                "id": "gpt-6-sol",
                "provider": "openai-codex",
                "display_name": "GPT-6 Sol",
                "created_at": "2026-10-01",
                "aliases": ["codex:sol"]
            },
            {
                "id": "vendor/benched",
                "provider": "vendor-cloud",
                "display_name": "Benched",
                "aliases": ["vendor:benched"]
            }
        ],
        "aliases": {
            "codex:sol": "gpt-6-sol",
            "vendor:benched": "vendor/benched"
        },
        "codex_ladder": ["gpt-6-luna", "gpt-6-sol", "gpt-6-astra"]
    }"#;

    const CORPUS_WITH_ONE_BENCHED_ROW: &str =
        r#"{"source": "test", "models": [{"id": "vendor/benched", "agentic_score": 0.81}]}"#;

    // (1) No overlay deployed is a silent no-op: the flat corpus is the whole
    // model database, and nothing resolves through an alias.
    #[test]
    fn absent_overlay_leaves_the_corpus_untouched() {
        let dir = tmpdir();
        let p = write_file(&dir, "corpus.json", CORPUS_WITH_ONE_BENCHED_ROW);
        let missing = dir.join("no-such-overlay.json");

        let _env = AliasEnvGuard::new(&missing);
        // The env var is what selects the overlay; the default is only used
        // when it is unset.
        assert_eq!(Corpus::alias_overlay_path(), missing);

        let c = Corpus::load(&p).expect("a missing overlay must not fail the load");
        assert_eq!(c.models.len(), 1);
        assert_eq!(c.count, 1);
        assert!(c.aliases.is_empty());
        assert_eq!(c.resolve_alias("codex:sol"), "codex:sol");
        assert!(c.get("codex:sol").is_none());
        // The one known id still resolves to itself.
        assert_eq!(c.resolve_alias("vendor/benched"), "vendor/benched");
        assert_eq!(c.get("vendor/benched").unwrap().agentic_score, Some(0.81));

        std::fs::remove_dir_all(&dir).ok();
    }

    // (2) An overlay id the corpus has never seen becomes a KNOWN model, but
    // a metadata-only one: nothing about it may be invented, so it can never
    // be auto-routed, ranked or priced from an alias file.
    #[test]
    fn overlay_adds_an_unknown_model_as_metadata_only() {
        let dir = tmpdir();
        let p = write_file(&dir, "corpus.json", CORPUS_WITH_ONE_BENCHED_ROW);
        let overlay = write_file(&dir, "model-aliases.json", OVERLAY);

        let _env = AliasEnvGuard::new(&overlay);
        let c = Corpus::load(&p).unwrap();

        let sol = c.get("gpt-6-sol").expect("overlay id must become known");
        assert!(sol.is_overlay(), "the row must be marked overlay-sourced");
        assert_eq!(sol.leaf, "gpt-6-sol");
        assert_eq!(sol.kind, "chat");
        // The generator's provider is recorded as provenance for display.
        assert_eq!(sol.family, "openai-codex");
        assert_eq!(sol.aliases, vec!["codex:sol".to_string()]);
        assert!(sol
            .gated_reason
            .as_deref()
            .is_some_and(|g| g.starts_with("overlay:")));

        // Metadata only: no routing eligibility, and no invented evidence.
        assert!(!sol.route_candidate && !sol.routable() && !sol.free && !sol.paid);
        assert_eq!(sol.agentic_score, None);
        assert_eq!(sol.swe_elo(), None);
        assert_eq!(sol.code_capability(), None);
        assert_eq!(sol.value_score(), None);
        assert!(sol.economics.is_none());
        assert!(c.free_chat().all(|m| m.id != "gpt-6-sol"));
        assert_eq!(c.count, 2, "the overlay row must be counted");

        // A model the corpus already knew keeps EVERY corpus field and only
        // gains the names it answers to.
        let benched = c.get("vendor/benched").unwrap();
        assert!(!benched.is_overlay(), "a corpus row is not overlay-sourced");
        assert_eq!(benched.agentic_score, Some(0.81));
        assert_eq!(benched.aliases, vec!["vendor:benched".to_string()]);

        std::fs::remove_dir_all(&dir).ok();
    }

    // (3) An alias resolves to the canonical model id — exactly, never
    // approximately.
    #[test]
    fn overlay_alias_resolves_to_the_model_id() {
        let dir = tmpdir();
        let p = write_file(&dir, "corpus.json", CORPUS_WITH_ONE_BENCHED_ROW);
        let overlay = write_file(&dir, "model-aliases.json", OVERLAY);

        let _env = AliasEnvGuard::new(&overlay);
        let c = Corpus::load(&p).unwrap();

        assert_eq!(c.resolve_alias("codex:sol"), "gpt-6-sol");
        assert_eq!(
            c.get("codex:sol").map(|m| m.id.as_str()),
            Some("gpt-6-sol"),
            "an alias must find the entry it names"
        );
        // An alias listed only inside a model's own `aliases` (not in the
        // overlay's map) resolves the same way — the two spellings agree.
        assert_eq!(c.resolve_alias("vendor:benched"), "vendor/benched");
        assert_eq!(c.get("vendor:benched").unwrap().id, "vendor/benched");

        // Never fuzzy: a prefix, a superstring, a case-fold or a
        // namespace-stripped form must all resolve to themselves.
        for typo in [
            "codex",
            "codex:",
            "codex:sol ",
            "codex:sol:extra",
            "gpt-6-sol ",
            "GPT-6-SOL",
            "sol",
        ] {
            assert_eq!(
                c.resolve_alias(typo),
                typo,
                "alias resolution must be exact, never fuzzy: {typo:?}"
            );
            assert!(c.get(typo).is_none(), "{typo:?} must not match anything");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    // (4) An exact corpus id always beats an alias of the same name, so an
    // overlay can never shadow a model zoder already knows.
    #[test]
    fn exact_model_id_always_beats_an_alias() {
        let dir = tmpdir();
        let p = write_file(
            &dir,
            "corpus.json",
            r#"{"models": [{"id": "vendor/real", "agentic_score": 0.9,
                             "free": true, "route_candidate": true, "kind": "chat"}]}"#,
        );
        // A deliberately ambiguous overlay: it claims `vendor/real` as an
        // ALIAS of a different model.
        let overlay = write_file(
            &dir,
            "model-aliases.json",
            r#"{"models": [{"id": "gpt-6-sol", "provider": "openai-codex", "aliases": ["codex:sol"]}],
                "aliases": {"vendor/real": "gpt-6-sol", "codex:sol": "gpt-6-sol"}}"#,
        );

        let _env = AliasEnvGuard::new(&overlay);
        let c = Corpus::load(&p).unwrap();

        // The real id resolves to itself, not to the aliased model.
        assert_eq!(c.resolve_alias("vendor/real"), "vendor/real");
        assert_eq!(
            c.get("vendor/real").map(|m| m.agentic_score),
            Some(Some(0.9)),
            "the corpus row must win the lookup, not the alias"
        );
        // A genuine alias still resolves, and resolution is not transitive
        // through the shadowed name.
        assert_eq!(c.resolve_alias("codex:sol"), "gpt-6-sol");
        assert_eq!(c.get("codex:sol").unwrap().id, "gpt-6-sol");

        std::fs::remove_dir_all(&dir).ok();
    }

    // (5) A malformed overlay is warned about (once) and IGNORED. It must
    // never fail startup and must never half-apply.
    #[test]
    fn malformed_overlay_is_ignored_and_never_fails_the_load() {
        let dir = tmpdir();
        let p = write_file(&dir, "corpus.json", CORPUS_WITH_ONE_BENCHED_ROW);

        // Four flavours of broken overlay: not JSON, JSON of the wrong shape,
        // rows that carry no usable `id`, and a truncated body.
        let broken = [
            ("not-json.json", "this is not json at all"),
            ("wrong-shape.json", r#"{"models": "not-an-array"}"#),
            (
                "no-usable-rows.json",
                r#"{"models": [{"provider": "x"}, {"id": "  "}]}"#,
            ),
            (
                "truncated.json",
                r#"{"models": [{"id": "gpt-6-sol", "aliases": ["codex:sol"]}"#,
            ),
        ];
        for (name, body) in broken {
            let overlay = write_file(&dir, name, body);
            let _env = AliasEnvGuard::new(&overlay);

            let c = Corpus::load(&p)
                .unwrap_or_else(|e| panic!("{name}: a bad overlay must not fail startup: {e}"));
            assert_eq!(
                c.models.len(),
                1,
                "{name}: the corpus must be left exactly as it was"
            );
            assert_eq!(
                c.get("vendor/benched").map(|m| m.agentic_score),
                Some(Some(0.81)),
                "{name}"
            );
            assert!(c.aliases.is_empty(), "{name}: no alias may resolve");
            assert!(c.get("gpt-6-sol").is_none(), "{name}: no row may be added");
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    // (5b) The warning is latched per process, so a bad overlay produces one
    // stderr line no matter how many commands load the corpus.
    #[test]
    fn overlay_warning_is_emitted_once() {
        let latch = AtomicBool::new(false);
        assert!(
            warn_overlay_once_flagged(&latch, format_args!("first")),
            "the first warning must be printed"
        );
        assert!(
            !warn_overlay_once_flagged(&latch, format_args!("second")),
            "later warnings must be suppressed"
        );
        assert!(
            !warn_overlay_once_flagged(&latch, format_args!("third")),
            "the latch stays closed"
        );
    }

    // (5c) An overlay that is unreadable for a structural reason (a directory,
    // a FIFO, a file past the byte cap) is also warned about and ignored.
    #[test]
    fn structurally_unusable_overlay_is_ignored() {
        let dir = tmpdir();
        let p = write_file(&dir, "corpus.json", CORPUS_WITH_ONE_BENCHED_ROW);

        // A directory at the overlay path: refused by the is_file() guard, and
        // it must not hang the loader either.
        {
            let _env = AliasEnvGuard::new(&dir);
            let c = Corpus::load(&p).expect("a directory overlay must not fail the load");
            assert_eq!(c.models.len(), 1);
            assert!(c.aliases.is_empty());
        }

        // Past the overlay byte cap: refused at the stat layer, so an
        // over-large file is never buffered into memory at all.
        let big = dir.join("big-overlay.json");
        let mut big_body = String::from(r#"{"models":[{"id":"gpt-6-sol"}],"aliases":{"pad":"#);
        big_body.push_str(&"x".repeat(Corpus::MAX_ALIAS_OVERLAY_BYTES as usize));
        big_body.push_str(":gpt-6-sol\"}}\n");
        std::fs::write(&big, &big_body).unwrap();
        assert!(std::fs::metadata(&big).unwrap().len() > Corpus::MAX_ALIAS_OVERLAY_BYTES);
        {
            let _env = AliasEnvGuard::new(&big);
            let c = Corpus::load(&p).expect("an oversized overlay must not fail the load");
            assert_eq!(c.models.len(), 1);
            assert!(c.aliases.is_empty());
            assert!(c.get("gpt-6-sol").is_none());
        }

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let fifo = dir.join("overlay.fifo");
            let cstr = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(cstr.as_ptr(), 0o644) }, 0);

            let _env = AliasEnvGuard::new(&fifo);
            let c = Corpus::load(&p).expect("a FIFO overlay must not fail the load");
            assert_eq!(c.models.len(), 1);
            assert!(c.aliases.is_empty());
            unsafe {
                libc::unlink(cstr.as_ptr());
            }
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    // (6) Loading an overlay never changes routing eligibility in either
    // direction: an overlay row stays a non-candidate through a refresh, and a
    // corpus row the overlay also names keeps its scores and its routing flags.
    #[test]
    fn overlay_rows_never_become_route_candidates_and_corpus_rows_are_never_demoted() {
        let dir = tmpdir();
        let p = write_file(
            &dir,
            "corpus.json",
            r#"{"models": [{"id": "vendor/free", "agentic_score": 0.7, "free": true,
                             "route_candidate": true, "kind": "chat"}]}"#,
        );
        let overlay = write_file(
            &dir,
            "model-aliases.json",
            r#"{"models": [{"id": "gpt-6-sol", "provider": "openai-codex", "aliases": ["codex:sol"]},
                           {"id": "vendor/free", "aliases": ["vendor:free"]}],
                "aliases": {"codex:sol": "gpt-6-sol"}}"#,
        );

        let _env = AliasEnvGuard::new(&overlay);
        let mut c = Corpus::load(&p).unwrap();

        let sol = c.get("gpt-6-sol").unwrap();
        assert!(
            !sol.route_candidate,
            "an overlay-added model must never be a route candidate"
        );
        assert!(!sol.routable());
        assert_eq!(
            c.free_chat().count(),
            1,
            "only the pre-existing corpus row may route"
        );

        // A refresh that does not serve the overlay id must neither promote it
        // nor retire it — it is a known id, just not a route candidate.
        let report = c.reconcile(&["vendor/free".to_string()]);
        assert!(report.added.is_empty(), "{report:?}");
        assert!(report.retired.is_empty(), "{report:?}");
        assert_eq!(report.kept, 1, "{report:?}");
        assert!(!c.get("gpt-6-sol").unwrap().route_candidate);

        // The corpus row the overlay also named keeps its corpus data.
        let free = c.get("vendor/free").unwrap();
        assert!(free.free && free.route_candidate && free.routable());
        assert_eq!(free.agentic_score, Some(0.7));
        assert_eq!(free.aliases, vec!["vendor:free".to_string()]);

        std::fs::remove_dir_all(&dir).ok();
    }

    // (7) Ingest is idempotent, and the aliases it added persist through a
    // save/load round trip so a refresh cannot lose the names a pin uses.
    #[test]
    fn overlay_ingest_is_idempotent_and_survives_a_save_round_trip() {
        let dir = tmpdir();
        let p = write_file(&dir, "corpus.json", CORPUS_WITH_ONE_BENCHED_ROW);
        let overlay = write_file(&dir, "model-aliases.json", OVERLAY);

        let saved = dir.join("saved-corpus.json");
        {
            // Load with NO overlay and ingest explicitly, so the report asserted
            // here is this call's own: a load-time ingest would already have
            // consumed the first application.
            let _env = AliasEnvGuard::none();
            let mut c = Corpus::load(&p).unwrap();
            assert!(
                c.get("gpt-6-sol").is_none(),
                "precondition: nothing is known before the overlay is applied"
            );

            let first = c.apply_alias_overlay_from(&overlay);
            assert_eq!(first.added, vec!["gpt-6-sol".to_string()]);
            assert_eq!(first.aliased, vec!["vendor/benched".to_string()]);
            assert_eq!(first.alias_count, 2);
            assert_eq!(first.path.as_deref(), Some(overlay.as_path()));

            // Re-applying adds nothing and duplicates no alias.
            let second = c.apply_alias_overlay_from(&overlay);
            assert!(second.added.is_empty() && second.aliased.is_empty());
            assert_eq!(c.models.len(), 2);
            assert_eq!(c.count, 2);
            assert_eq!(c.get("gpt-6-sol").unwrap().aliases, ["codex:sol"]);

            c.save(&saved).unwrap();
        }

        // Reload with NO overlay deployed: the persisted rows and alias map
        // must still resolve, which is what keeps an existing `-m codex:sol`
        // working across a corpus refresh.
        let _env = AliasEnvGuard::none();
        let reloaded = Corpus::load(&saved).unwrap();
        assert_eq!(reloaded.models.len(), 2);
        assert_eq!(reloaded.resolve_alias("codex:sol"), "gpt-6-sol");
        assert!(reloaded.get("codex:sol").is_some());
        assert!(!reloaded.get("gpt-6-sol").unwrap().route_candidate);

        std::fs::remove_dir_all(&dir).ok();
    }

    // (8) Overlay location: the env var wins; unset falls back to the zoder
    // home, so a relocated `ZODER_HOME` carries the overlay with it.
    #[test]
    fn overlay_path_prefers_the_env_var_then_the_zoder_home() {
        let dir = tmpdir();
        let pinned = dir.join("pinned-aliases.json");

        let _env = AliasEnvGuard::new(&pinned);
        assert_eq!(Corpus::alias_overlay_path(), pinned);

        // Empty is treated as unset, not as "the current directory".
        std::env::set_var(MODEL_ALIASES_ENV, "");
        assert_eq!(
            Corpus::alias_overlay_path(),
            crate::config::Config::home().join(MODEL_ALIASES_FILE)
        );

        std::env::remove_var(MODEL_ALIASES_ENV);
        assert_eq!(
            Corpus::alias_overlay_path(),
            crate::config::Config::home().join(MODEL_ALIASES_FILE)
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
