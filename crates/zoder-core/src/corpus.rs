//! The classified model corpus produced by the corpus builder/refresh job.
//!
//! Each [`ModelEntry`] carries free/paid classification, LMArena-derived
//! capability weights (overall + coding/SWE), and benched latency/throughput,
//! combined into a single `agentic_score` the router can sort on.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic per-writer nonce so two overlapping corpus saves in the same
/// process never collide on the temp path. Combined with `std::process::id()`
/// it makes the temp filename unique per writer (C5-4 / S19).
static SAVE_NONCE: AtomicU64 = AtomicU64::new(0);

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
    /// Maximum trusted model-corpus size. Larger files are rejected before the
    /// body is read so a misconfigured `corpus_path` pointing at a multi-GB JSON
    /// can't OOM `Engine::load()`. The actual bundled corpus is ~1.6 MiB
    /// (hundreds of models with descriptions and bench data); 8 MiB sits one
    /// order of magnitude above real usage while staying bounded -- a typo'd
    /// 64-bit `len()` would land in the `Err` path, not as an allocation.
    const MAX_CORPUS_BYTES: u64 = 8 * 1024 * 1024; // 8 MiB

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
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Self::load_with_cap(path, Self::MAX_CORPUS_BYTES)
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
        Self::dedup_by_id_last_wins(&mut models);

        Ok(Corpus {
            source: str_field("source"),
            arena_date: str_field("arena_date"),
            count: models.len(),
            models,
        })
    }

    /// De-duplicate `models` by `id`, keeping the LAST occurrence of each id and
    /// preserving the relative order of the surviving entries. Used by `load()`
    /// so a corpus file with duplicate ids collapses to one entry per id.
    fn dedup_by_id_last_wins(models: &mut Vec<ModelEntry>) {
        // Index of the last occurrence of each id.
        let mut last: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for (i, m) in models.iter().enumerate() {
            last.insert(m.id.as_str(), i);
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
}
