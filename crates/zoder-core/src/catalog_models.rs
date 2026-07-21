//! Client for the local zeroclaw **model catalog** over its Unix-socket JSON-RPC
//! (`config/catalog-models`). The zeroclaw engine exposes its own currently-
//! configured, live model catalog via this RPC method; this module is the
//! `zoder` side of the call: it sends the request, decodes the response into
//! strongly-typed rows, and exposes a pure-function [`merge_into_corpus`]
//! helper that enriches a static [`crate::corpus::Corpus`] with the live data.
//!
//! Why this exists
//! ---------------
//! `zoder`'s corpus is a static, benched-at-build-time list of model entries
//! (capability / ELO / latency / pricing). When the operator changes a
//! `config.toml` provider block (rotates a key, adds a new model, retires a
//! stale one), `zoder` historically had to wait for the next corpus-builder
//! refresh to see the change. The `config/catalog-models` RPC closes that gap:
//! it asks the running zeroclaw daemon "what models are you actually
//! configured for right now?" and folds the answer in as **additive
//! enrichment**. The static corpus is NEVER replaced — when the RPC is
//! unavailable, errors out, or returns an empty/unknown shape, the existing
//! corpus behavior is preserved bit-for-bit. This is the same additive
//! posture the corpus builder's `reconcile()` + `ingest_free_chat()` already
//! use to fold a provider's `serves` allowlist in: live state, local
//! classification.
//!
//! Wire shape
//! ----------
//! The daemon is expected to return a JSON object with the following shape
//! (matches the field names the zeroclaw engine uses elsewhere in its
//! `pricing.json` and cost tracker; we accept extra fields without
//! rejecting, so a future zeroclaw release can add columns without breaking
//! `zoder`):
//!
//! ```json
//! {
//!   "models": [
//!     {
//!       "id": "minimax/MiniMax-M3",
//!       "host": "minimax",
//!       "kind": "chat",
//!       "free": true,
//!       "source": "subscription",
//!       "input_usd_per_mtok": 0.0,
//!       "output_usd_per_mtok": 0.0,
//!       "context_window": 200000,
//!       "tags": ["fast", "code"]
//!     },
//!     ...
//!   ]
//! }
//! ```
//!
//! Transport is the same NDJSON JSON-RPC the `cost/query` client uses
//! ([`crate::engine_cost::fetch_engine_cost`]): connect, `initialize`, then
//! one request/response. A missing or unreachable daemon yields an error the
//! caller degrades on (enrichment skipped, corpus used as-is), never a
//! panic.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::corpus::{Corpus, Economics, ModelEntry};

/// ACP protocol version the daemon's `initialize` expects. Mirrors
/// `zeroclaw_api::jsonrpc::ACP_PROTOCOL_VERSION` and the value used in
/// [`crate::engine_cost`]. Kept in sync with the rest of the wire layer.
const ACP_PROTOCOL_VERSION: u64 = 1;

/// How long to wait for the whole connect → initialize → query exchange.
/// Matches the cost engine's `QUERY_TIMEOUT` so a hung daemon cannot stall the
/// CLI for arbitrarily long.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// One catalog row, mirroring the daemon's `config/catalog-models` result
/// entry. Every field is `#[serde(default)]` so a future daemon that omits
/// `context_window` or `tags` does not break `zoder` — missing fields are
/// just treated as "unknown", which is exactly the posture the corpus has
/// always taken for absent metadata.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct CatalogModel {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub kind: String,
    /// Whether the daemon considers the model free to call (subscription,
    /// free-tier, internal). `None` when the daemon didn't classify it.
    #[serde(default)]
    pub free: Option<bool>,
    /// Where the daemon learned about this row (`subscription` | `serves` |
    /// `pricing` | `manual` | ...). Free-form string — `zoder` does not
    /// branch on it; it is preserved on the enriched entry so operators can
    /// tell why a row was injected.
    #[serde(default)]
    pub source: String,
    /// Per-1M-token input cost, USD. `0.0` for free models.
    #[serde(default)]
    pub input_usd_per_mtok: Option<f64>,
    /// Per-1M-token output cost, USD. `0.0` for free models.
    #[serde(default)]
    pub output_usd_per_mtok: Option<f64>,
    /// Optional context window the daemon knows about. Preserved for display;
    /// not currently consumed by the router.
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Free-form operator tags (e.g. `["fast", "code"]`). Preserved verbatim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// The full `config/catalog-models` response. The daemon's wire shape is
/// `{ "models": [...] }`; extra top-level fields are accepted but not
/// consumed (forward-compatible).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CatalogResponse {
    #[serde(default)]
    pub models: Vec<CatalogModel>,
}

/// Outcome of a `config/catalog-models` enrichment. Lets callers (CLI,
/// tests) tell apart "the daemon returned a real catalog" from "the daemon
/// was unreachable / errored" from "the daemon returned an empty catalog" —
/// all three are valid end-states for the additive enrichment path.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnrichmentOutcome {
    /// How many rows the daemon returned. `0` is a valid answer (no models
    /// configured), distinct from "RPC failed" below.
    pub rows: usize,
    /// How many corpus entries were *added* by the merge (a row that wasn't
    /// in the static corpus). Existing entries are *enriched* in place, not
    /// counted here.
    pub added: usize,
    /// How many corpus entries were *enriched* in place (a row whose `id`
    /// was already in the static corpus and the daemon supplied new
    /// metadata for).
    pub enriched: usize,
    /// How many daemon rows were skipped because they had an empty `id`
    /// (the loader-style strict-`id` invariant the corpus enforces).
    pub skipped: usize,
    /// The error string, if the RPC failed and we degraded silently. `None`
    /// when the RPC succeeded (or when the caller never tried, in which
    /// case `rows == 0` and `added == 0` and `enriched == 0`).
    pub error: Option<String>,
}

impl EnrichmentOutcome {
    /// True when the daemon supplied at least one usable row.
    pub fn has_live_data(&self) -> bool {
        self.rows > 0 && self.error.is_none()
    }
}

/// Query the local zeroclaw engine for its current model catalog via the
/// `config/catalog-models` JSON-RPC method. Same transport conventions as
/// [`crate::engine_cost::fetch_engine_cost`]: NDJSON over a Unix socket,
/// `initialize` first, then a single request/response, all under
/// [`QUERY_TIMEOUT`]. Returns the raw [`CatalogResponse`] so callers can
/// inspect/merge it however they want; most callers will pass the result
/// straight to [`merge_into_corpus`].
pub async fn fetch_catalog_models(socket: &Path) -> anyhow::Result<CatalogResponse> {
    let exchange = async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .with_context(|| format!("connecting to catalog engine at {}", socket.display()))?;
        let (read_half, mut write_half) = tokio::io::split(stream);
        let mut reader = BufReader::new(read_half);

        // First frame must be `initialize` (the daemon rejects everything
        // else until the handshake completes).
        write_frame(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": "nvz-init",
                "method": "initialize",
                "params": { "protocol_version": ACP_PROTOCOL_VERSION },
            }),
        )
        .await?;
        read_response(&mut reader, "nvz-init").await?;

        // The actual `config/catalog-models` request. The daemon returns
        // `{ "models": [...] }`; any `params` is reserved for future
        // filters (provider id, kind, free-only, …) and intentionally
        // omitted here so this client stays forward-compatible with old
        // and new daemon builds alike.
        write_frame(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": "nvz-catalog",
                "method": "config/catalog-models",
                "params": {},
            }),
        )
        .await?;
        let result = read_response(&mut reader, "nvz-catalog").await?;
        let resp: CatalogResponse =
            serde_json::from_value(result).context("decoding config/catalog-models result")?;
        Ok::<CatalogResponse, anyhow::Error>(resp)
    };

    tokio::time::timeout(QUERY_TIMEOUT, exchange)
        .await
        .map_err(|_| anyhow!("catalog engine query timed out after {QUERY_TIMEOUT:?}"))?
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Value) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Read NDJSON frames until the response with `want_id` arrives, skipping
/// interleaved notifications (no `id`) and unrelated responses. Mirrors
/// `engine_cost::read_response` — duplicated here instead of exported so
/// each module keeps its own copy of the wire-shape contract (the cost and
/// catalog engines could diverge in notification behavior over time).
async fn read_response<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    want_id: &str,
) -> anyhow::Result<Value> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("reading from catalog engine")?;
        if n == 0 {
            bail!("catalog engine closed the connection before responding");
        }
        let frame: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if frame.get("id").and_then(Value::as_str) != Some(want_id) {
            continue;
        }
        if let Some(err) = frame.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("catalog engine returned an error: {msg}");
        }
        return Ok(frame.get("result").cloned().unwrap_or(Value::Null));
    }
}

/// Merge a live catalog response into a static corpus, returning a per-row
/// diff summary so callers can surface "N added / M enriched" in their
/// output. **Pure function** — does not perform any I/O, so it is trivially
/// testable from a `#[test]` without spinning up a daemon.
///
/// Merge contract (additive enrichment, NEVER destructive):
///   1. Rows with an empty `id` are dropped (matches the corpus loader's
///      strict-`id` invariant; we never promote a nameless row into routing).
///   2. For a row whose `id` already exists in the corpus: enrich in place
///      with whichever of (free, kind, host, family, economics) the daemon
///      supplied. Existing bench scores, capability, preference, and
///      workflows fields are NEVER overwritten (the corpus builder is the
///      authority for those, not the daemon).
///   3. For a new id: append a minimal entry, marked `route_candidate =
///      false` (so the corpus's free-classification is preserved and a
///      daemon that misclassifies a paid model as free can never silently
///      promote it into routing — exactly the same fail-closed posture
///      `from_served_id` enforces for the corpus builder's reconcile path).
///   4. The corpus's `count` is updated to match the new `models.len()`.
///
/// Returns the outcome so CLI/test code can log / assert on the merge
/// shape. The function takes `&mut Corpus` so the static corpus's existing
/// ordering is preserved (new rows are appended at the end, matching the
/// `reconcile` and `ingest_free_chat` patterns).
pub fn merge_into_corpus(corpus: &mut Corpus, resp: &CatalogResponse) -> EnrichmentOutcome {
    let mut outcome = EnrichmentOutcome {
        rows: resp.models.len(),
        ..Default::default()
    };
    // Track which daemon rows actually carried a usable id so the
    // `skipped` count is exact, not "rows - added - enriched".
    let mut seen_ids: HashSet<String> = HashSet::new();
    for row in &resp.models {
        if row.id.trim().is_empty() {
            outcome.skipped += 1;
            continue;
        }
        if !seen_ids.insert(row.id.clone()) {
            // Duplicate id within the same daemon response: treat the
            // first occurrence as authoritative and skip the rest, so a
            // buggy daemon emitting the same id twice can't promote a
            // single row into two corpus entries.
            outcome.skipped += 1;
            continue;
        }
        match corpus.models.iter_mut().find(|m| m.id == row.id) {
            Some(existing) => {
                if enrich_existing(existing, row) {
                    outcome.enriched += 1;
                }
            }
            None => {
                corpus.models.push(entry_from_catalog(row));
                outcome.added += 1;
            }
        }
    }
    corpus.count = corpus.models.len();
    outcome
}

/// Apply daemon-supplied metadata to an EXISTING corpus entry. Returns
/// `true` if any field was actually changed (so the caller can count
/// meaningful enrichments separately from no-ops).
///
/// Existing benchmark, capability, preference, and workflows fields are
/// never touched — those belong to the corpus builder, not the daemon.
/// The daemon is only authoritative for the model-selection surface
/// fields: free/paid/kind/host/family/economics (and a free-form `source`
/// annotation for diagnostic display).
fn enrich_existing(existing: &mut ModelEntry, row: &CatalogModel) -> bool {
    let mut changed = false;
    if let Some(free) = row.free {
        // Daemon-supplied `free` is authoritative when the corpus's local
        // classification disagrees: the corpus's free flag is set by the
        // corpus builder (a periodic, offline job) and the daemon's is
        // live (provider config is the source of truth for "is this
        // subscription / metered right now"). A new daemon that flipped a
        // model from free to paid (provider rotation, plan change) should
        // be reflected immediately, not on the next refresh.
        //
        // SAFETY: we still never promote a paid-flagged-in-corpus model
        // to free based on the daemon alone — the corpus's `paid` flag
        // is harder to flip than its `free` flag (it requires explicit
        // pricing knowledge), so we trust it. A new daemon saying
        // "free=true" for a model the corpus already classifies as paid
        // is a misconfiguration, not a license to spend on it.
        if !existing.paid && existing.free != free {
            existing.free = free;
            changed = true;
        }
    }
    if !row.kind.is_empty() && existing.kind != row.kind {
        existing.kind = row.kind.clone();
        changed = true;
    }
    if !row.host.is_empty() && existing.host != row.host {
        existing.host = row.host.clone();
        // family follows host, by the same convention `from_served_id`
        // uses, so a daemon that moves a model between providers (e.g.
        // re-hosting a free model on a different NIM) keeps the corpus
        // consistent.
        existing.family = row.host.clone();
        changed = true;
    }
    // Economics: only ever write the daemon's number when the corpus has
    // no number yet. The corpus builder's pricing source is
    // intentionally more authoritative than a daemon-provided price
    // (the pricing feed handles multi-source reconciliation; the
    // catalog RPC is a snapshot). If both say "$0" the result is
    // unchanged; if the corpus has a real price and the daemon says
    // "$0", the corpus wins.
    if let (Some(in_p), Some(out_p)) = (row.input_usd_per_mtok, row.output_usd_per_mtok) {
        if existing.economics.is_none() {
            existing.economics = Some(Economics {
                input_usd_per_mtok: in_p,
                output_usd_per_mtok: out_p,
                cache_read_usd_per_mtok: 0.0,
                source: if row.source.is_empty() {
                    "daemon".to_string()
                } else {
                    row.source.clone()
                },
                date: None,
            });
            changed = true;
        }
    }
    changed
}

/// Convenience wrapper: call [`merge_into_corpus`] but degrade cleanly
/// when the RPC itself failed. The CLI uses this so a missing daemon
/// socket never aborts the surface command — it just records the error
/// in the outcome (which the operator can see) and proceeds with the
/// static corpus unchanged.
///
/// This is the additive-enrichment posture the task requires: the static
/// corpus is NEVER replaced; the daemon is an optional enrichment source
/// that fails soft. The corpus's bench/capability/preference/workflows
/// fields are never touched on either the Ok or the Err path.
pub fn merge_or_degrade(
    corpus: Option<&mut Corpus>,
    resp: anyhow::Result<CatalogResponse>,
) -> EnrichmentOutcome {
    let Some(corpus) = corpus else {
        return EnrichmentOutcome {
            error: Some("cannot enrich catalog: corpus is unavailable".to_string()),
            ..Default::default()
        };
    };
    match resp {
        Ok(c) => merge_into_corpus(corpus, &c),
        Err(e) => {
            let msg = e.to_string();
            EnrichmentOutcome {
                error: Some(msg),
                ..Default::default()
            }
        }
    }
}

/// Build a minimal `ModelEntry` for a daemon-supplied id that wasn't in the
/// static corpus. Deliberately conservative — same fail-closed posture as
/// `ModelEntry::from_served_id`: NOT classified free, NOT route-eligible,
/// gated with a clear "needs classification" reason so a future corpus
/// refresh picks it up and the router never sees it until then.
fn entry_from_catalog(row: &CatalogModel) -> ModelEntry {
    let host = if row.host.is_empty() {
        // Synthesize the host from the id's prefix when the daemon
        // didn't supply one explicitly — mirrors what
        // `ModelEntry::from_served_id` does so the corpus's host
        // family is consistent regardless of which path added the row.
        match row.id.split_once('/') {
            Some((h, _)) => h.to_string(),
            None => String::new(),
        }
    } else {
        row.host.clone()
    };
    let leaf = match row.id.split_once('/') {
        Some((_, l)) => l.to_string(),
        None => row.id.clone(),
    };
    let family = if host.is_empty() {
        row.id.clone()
    } else {
        host.clone()
    };
    let kind = if row.kind.is_empty() {
        "chat".to_string()
    } else {
        row.kind.clone()
    };
    // Daemon-supplied `free=true` is honored for the *display* flag (so
    // `zoder models` can correctly show "free" against the daemon's
    // authority), but route_candidate stays false. Routing is gated on
    // a corpus-builder pass that produces latency + capability
    // numbers, so a daemon-only "free" claim can never silently
    // promote an unbenched model into the live router.
    let free_display = row.free.unwrap_or(false);
    let mut entry = ModelEntry {
        id: row.id.clone(),
        host,
        leaf,
        family,
        kind,
        route_candidate: false,
        free: free_display,
        paid: !free_display && row.free.is_some(),
        gated_reason: Some(
            "from daemon catalog: needs corpus-builder pass (capability + latency) before routing"
                .to_string(),
        ),
        ..Default::default()
    };
    if let (Some(in_p), Some(out_p)) = (row.input_usd_per_mtok, row.output_usd_per_mtok) {
        entry.economics = Some(Economics {
            input_usd_per_mtok: in_p,
            output_usd_per_mtok: out_p,
            cache_read_usd_per_mtok: 0.0,
            source: if row.source.is_empty() {
                "daemon".to_string()
            } else {
                row.source.clone()
            },
            date: None,
        });
    }
    entry
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pure merge/enrichment layer. The RPC client
    //! itself is exercised by the integration tests in
    //! `crates/zoder-core/tests/catalog_models_rpc.rs`, which spin up a
    //! real Unix-socket daemon (so the wire shape, the `initialize`
    //! handshake, the `config/catalog-models` request, and the
    //! failure-mode degradation are all covered end-to-end).
    use super::*;
    use crate::corpus::BenchScore;
    use crate::corpus::Capability;

    fn minimal_corpus() -> Corpus {
        Corpus {
            source: "test".into(),
            models: vec![ModelEntry {
                id: "minimax/MiniMax-M3".into(),
                host: "minimax".into(),
                leaf: "MiniMax-M3".into(),
                family: "minimax".into(),
                kind: "chat".into(),
                free: true,
                route_candidate: true,
                capability: Some(Capability {
                    swe_verified: Some(BenchScore {
                        acc: Some(85.0),
                        source: "vals.ai".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                agentic_score: Some(0.9),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn empty_catalog_is_a_noop() {
        let mut corpus = minimal_corpus();
        let before = corpus.models.len();
        let outcome = merge_into_corpus(&mut corpus, &CatalogResponse::default());
        assert_eq!(outcome.rows, 0);
        assert_eq!(outcome.added, 0);
        assert_eq!(outcome.enriched, 0);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(
            corpus.models.len(),
            before,
            "empty catalog must not mutate the corpus"
        );
    }

    #[test]
    fn empty_id_rows_are_skipped() {
        let mut corpus = minimal_corpus();
        let resp = CatalogResponse {
            models: vec![
                CatalogModel {
                    id: "".into(),
                    ..Default::default()
                },
                CatalogModel {
                    id: "  ".into(),
                    ..Default::default()
                }, // whitespace = empty
                CatalogModel {
                    id: "minimax/MiniMax-M3".into(),
                    free: Some(false),
                    ..Default::default()
                },
            ],
        };
        let outcome = merge_into_corpus(&mut corpus, &resp);
        assert_eq!(outcome.rows, 3);
        assert_eq!(outcome.skipped, 2, "two empty-id rows must be skipped");
        // The valid row lands as an enrichment (free flipped to false).
        assert_eq!(outcome.enriched, 1);
        assert_eq!(outcome.added, 0);
        // The model is no longer free because the daemon says so.
        let e = corpus
            .models
            .iter()
            .find(|m| m.id == "minimax/MiniMax-M3")
            .unwrap();
        assert!(
            !e.free,
            "daemon's free=false must be honored on an existing row"
        );
    }

    #[test]
    fn new_id_is_appended_as_unrouted() {
        let mut corpus = minimal_corpus();
        let resp = CatalogResponse {
            models: vec![CatalogModel {
                id: "newco/newmodel".into(),
                host: "newco".into(),
                kind: "chat".into(),
                free: Some(true),
                source: "serves".into(),
                input_usd_per_mtok: Some(0.0),
                output_usd_per_mtok: Some(0.0),
                ..Default::default()
            }],
        };
        let outcome = merge_into_corpus(&mut corpus, &resp);
        assert_eq!(outcome.added, 1);
        assert_eq!(outcome.enriched, 0);
        assert_eq!(corpus.models.len(), 2);
        // The new row is a fail-closed stub: not route-eligible, gated with
        // a clear reason, even when the daemon said free=true. The
        // corpus-builder pass is the authority for "safe to route".
        let added = corpus
            .models
            .iter()
            .find(|m| m.id == "newco/newmodel")
            .unwrap();
        assert!(
            !added.route_candidate,
            "new daemon rows must NOT be route-eligible"
        );
        assert!(
            added.gated_reason.is_some(),
            "new daemon rows must be gated"
        );
        assert!(added
            .gated_reason
            .as_deref()
            .unwrap()
            .contains("corpus-builder"));
        // host / family / kind filled from the daemon.
        assert_eq!(added.host, "newco");
        assert_eq!(added.family, "newco");
        assert_eq!(added.kind, "chat");
        // economics populated from the daemon so `zoder report` /
        // `zoder models` can show the price.
        let ec = added
            .economics
            .as_ref()
            .expect("daemon economics preserved");
        assert_eq!(ec.input_usd_per_mtok, 0.0);
        assert_eq!(ec.source, "serves");
    }

    #[test]
    fn existing_id_is_enriched_in_place_and_bench_scores_preserved() {
        // Corpus builder's bench numbers are the corpus builder's; the
        // daemon's free/host/kind metadata must NOT clobber them.
        let mut corpus = minimal_corpus();
        let resp = CatalogResponse {
            models: vec![CatalogModel {
                id: "minimax/MiniMax-M3".into(),
                host: "minimax-rotated".into(), // simulate a provider rotation
                kind: "chat".into(),
                free: Some(true),
                ..Default::default()
            }],
        };
        let outcome = merge_into_corpus(&mut corpus, &resp);
        assert_eq!(outcome.enriched, 1);
        assert_eq!(outcome.added, 0);
        let e = corpus
            .models
            .iter()
            .find(|m| m.id == "minimax/MiniMax-M3")
            .unwrap();
        // Host rotates, family follows (per the enrich_existing contract).
        assert_eq!(e.host, "minimax-rotated");
        assert_eq!(e.family, "minimax-rotated");
        // Bench numbers are untouched — the corpus builder's authority.
        assert_eq!(
            e.code_capability(),
            Some(85.0),
            "existing bench scores must be preserved through enrichment"
        );
        assert_eq!(e.agentic_score, Some(0.9));
    }

    #[test]
    fn daemon_paid_claim_does_not_flip_paid_corpus_row_to_free() {
        // FAIL-CLOSED: a row the corpus classifies as paid stays paid even
        // if the daemon says free=true. The corpus's paid flag is the
        // hard gate; the daemon's free flag is a soft hint.
        let mut corpus = Corpus {
            models: vec![ModelEntry {
                id: "openai/gpt-5".into(),
                host: "openai".into(),
                leaf: "gpt-5".into(),
                family: "openai".into(),
                kind: "chat".into(),
                free: false,
                paid: true,
                route_candidate: false,
                ..Default::default()
            }],
            ..Default::default()
        };
        let resp = CatalogResponse {
            models: vec![CatalogModel {
                id: "openai/gpt-5".into(),
                host: "openai".into(),
                kind: "chat".into(),
                free: Some(true), // daemon says free
                ..Default::default()
            }],
        };
        let outcome = merge_into_corpus(&mut corpus, &resp);
        // The free flag is NOT changed (existing.paid blocks the flip).
        // So no field actually changed -> enriched count = 0.
        assert_eq!(
            outcome.enriched, 0,
            "paid row stays paid; nothing was actually enriched"
        );
        let e = corpus
            .models
            .iter()
            .find(|m| m.id == "openai/gpt-5")
            .unwrap();
        assert!(e.paid, "corpus's paid flag is the hard gate");
        assert!(
            !e.free,
            "daemon's free=true is ignored when corpus says paid"
        );
    }

    #[test]
    fn daemon_free_claim_does_flip_free_corpus_row() {
        // The other direction: a row the corpus classified as free but
        // that the daemon has just started treating as paid (provider
        // plan rotation) must be updated immediately, not on the next
        // corpus refresh. This is the "additive enrichment" half of
        // the contract: the daemon is the source of truth for "is
        // this still free right now".
        let mut corpus = minimal_corpus();
        let resp = CatalogResponse {
            models: vec![CatalogModel {
                id: "minimax/MiniMax-M3".into(),
                host: "minimax".into(),
                kind: "chat".into(),
                free: Some(false), // daemon says no longer free
                ..Default::default()
            }],
        };
        let outcome = merge_into_corpus(&mut corpus, &resp);
        assert_eq!(outcome.enriched, 1);
        let e = corpus
            .models
            .iter()
            .find(|m| m.id == "minimax/MiniMax-M3")
            .unwrap();
        assert!(
            !e.free,
            "daemon's free=false must be honored on a non-paid row"
        );
    }

    #[test]
    fn duplicate_daemon_ids_are_deduplicated() {
        let mut corpus = minimal_corpus();
        let resp = CatalogResponse {
            models: vec![
                CatalogModel {
                    id: "dup/model".into(),
                    host: "dup".into(),
                    ..Default::default()
                },
                CatalogModel {
                    id: "dup/model".into(),
                    host: "dup-other".into(),
                    ..Default::default()
                },
            ],
        };
        let outcome = merge_into_corpus(&mut corpus, &resp);
        assert_eq!(outcome.rows, 2);
        assert_eq!(outcome.skipped, 1, "the duplicate id must be skipped");
        assert_eq!(outcome.added, 1, "the first occurrence is the one appended");
        // Only one row for `dup/model` exists, and it carries the FIRST
        // occurrence's host (so a buggy daemon that emits two different
        // hosts for one id can't make the corpus flip-flop).
        let dup = corpus.models.iter().filter(|m| m.id == "dup/model").count();
        assert_eq!(dup, 1);
        let dup = corpus.models.iter().find(|m| m.id == "dup/model").unwrap();
        assert_eq!(dup.host, "dup");
    }

    #[test]
    fn corpus_count_is_updated_after_merge() {
        let mut corpus = minimal_corpus();
        // The minimal corpus has one model; `count` follows `models.len()`
        // by construction (so test starts consistent).
        corpus.count = corpus.models.len();
        let before = corpus.count;
        let resp = CatalogResponse {
            models: vec![
                CatalogModel {
                    id: "a/x".into(),
                    host: "a".into(),
                    ..Default::default()
                },
                CatalogModel {
                    id: "b/y".into(),
                    host: "b".into(),
                    ..Default::default()
                },
            ],
        };
        merge_into_corpus(&mut corpus, &resp);
        assert_eq!(
            corpus.count,
            corpus.models.len(),
            "count must follow models.len()"
        );
        assert_eq!(corpus.count, before + 2);
    }

    #[test]
    fn outcome_reports_error_string_when_rpc_fails() {
        // The merge itself can't fail (pure function), but the outcome's
        // error field is populated by the convenience wrapper
        // `merge_or_degrade` below. Verify the wrapper degrades cleanly.
        let mut corpus = minimal_corpus();
        let outcome = merge_or_degrade(
            Some(&mut corpus),
            Err(anyhow!("daemon socket not found: /tmp/nope.sock")),
        );
        assert_eq!(outcome.rows, 0);
        assert_eq!(outcome.added, 0);
        assert_eq!(outcome.enriched, 0);
        assert!(outcome.error.is_some(), "error path must surface a message");
        // And the corpus is unchanged.
        assert_eq!(corpus.models.len(), 1);
    }

    #[test]
    fn outcome_distinguishes_empty_from_error() {
        // Empty success (no models configured) and RPC failure are
        // different end-states; the outcome makes them distinguishable.
        let mut corpus_a = minimal_corpus();
        let out_a = merge_or_degrade(Some(&mut corpus_a), Ok(CatalogResponse::default()));
        assert_eq!(out_a.rows, 0);
        assert!(out_a.error.is_none(), "empty success is not an error");
        assert!(!out_a.has_live_data());

        let mut corpus_b = minimal_corpus();
        let out_b = merge_or_degrade(Some(&mut corpus_b), Err(anyhow!("boom")));
        assert!(out_b.error.is_some());
        assert!(!out_b.has_live_data());
    }

    #[test]
    fn merge_or_degrade_passes_through_success() {
        let mut corpus = minimal_corpus();
        let resp = CatalogResponse {
            models: vec![CatalogModel {
                id: "newco/newmodel".into(),
                host: "newco".into(),
                kind: "chat".into(),
                free: Some(true),
                ..Default::default()
            }],
        };
        let outcome = merge_or_degrade(Some(&mut corpus), Ok(resp));
        assert_eq!(outcome.added, 1);
        assert!(outcome.error.is_none());
        assert!(outcome.has_live_data());
    }

    #[test]
    fn entry_from_catalog_synthesizes_host_from_id() {
        // When the daemon doesn't supply a host, fall back to the id's
        // `host/leaf` split — same convention `ModelEntry::from_served_id`
        // uses. Keeps the corpus's host/family consistent regardless of
        // which path added the row.
        let row = CatalogModel {
            id: "minimax/MiniMax-M3".into(),
            host: "".into(),
            ..Default::default()
        };
        let e = entry_from_catalog(&row);
        assert_eq!(e.host, "minimax");
        assert_eq!(e.leaf, "MiniMax-M3");
        assert_eq!(e.family, "minimax");
        assert_eq!(e.kind, "chat");
    }
}
