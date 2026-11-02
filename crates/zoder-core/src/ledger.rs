//! Local spend ledger. Append-only JSONL, one record per model call
//! (ts_utc, provider, model, tokens_in, tokens_out, cost_usd), with
//! day/week/month/year rollups. SQLite is a drop-in later via the same shape.
//!
//! Concurrency note: the reserve->commit serialization relies on `fs2` /
//! `flock(2)` advisory locking, which is unreliable over NFS (locks may be
//! silently ignored or not propagate between clients). The ledger file MUST
//! therefore live on a LOCAL filesystem. The default `~/.zoder/` is local; a
//! future config that points the ledger at an NFS mount (for example a shared
//! release volume) would weaken the reserve->commit serialization and must be
//! avoided.

use anyhow::Context;
use chrono::{DateTime, Datelike, IsoWeek, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Optional FinOps tags attached to a ledger entry at ingestion time.
/// Mirrors the TypeScript `FinOpsTags` interface (snake_case wire format).
/// Persisted on [`Entry`] via `#[serde(default)]` so legacy entries written
/// before this field existed still deserialize — Finding #22. The fields
/// are intentionally `Option<..>` so a JSON `null` and an absent field are
/// indistinguishable at the rollup layer (both mean "no tag").
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FinOpsTags {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_ratio: Option<f64>,
    /// The input tokens the provider served from its prompt cache. A subset
    /// of the call's total input count, which includes the hits. Recorded
    /// next to the ratio because a ratio cannot be turned back into the
    /// exact hit count, and the hit count is what a row is re-priced with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_tokens: Option<u64>,
}

/// Maximum number of consecutive non-UTF-8 bytes we'll tolerate in a
/// single line before giving up. Used as the upper bound for
/// incremental `BufRead::fill_buf` consumption so an attacker can't pin the parser on a
/// multi-gigabyte garbage line. This is a distinct, larger ceiling from
/// `provider.rs`'s `MAX_LINE_BYTES` (1 MiB) for streaming SSE lines: a
/// persisted ledger record can legitimately be larger than a single SSE
/// frame, so this bound is set to 16 MiB rather than matching that constant.
const MAX_LEDGER_LINE_BYTES: usize = 16 * 1024 * 1024; // 16 MiB
/// Space allocated before a billable call. The completed entry is written into
/// this already-allocated region, so a full filesystem cannot strand spend
/// after dispatch. Ledger entries contain metadata, not prompts/responses
/// (a final row is ~300-400 bytes); 4 KiB leaves ample reconciliation
/// headroom, and an oversized `violation` note is truncated to fit.
///
/// Was 64 KiB. Every slot stayed in the file forever, so the ledger grew by
/// 64 KiB per call (506 MB after 7,725 calls on one fleet host) and every
/// reservation re-read the whole file under the exclusive ledger lock --
/// concurrent sessions on one host serialized behind multi-second scans.
const BILLABLE_RESERVATION_BYTES: usize = 4 * 1024;
/// Slot size written by older zoder builds; still recognized so abandoned
/// legacy reservations are recovered and legacy rows are compacted.
const LEGACY_RESERVATION_BYTES: usize = 64 * 1024;
/// Compact the ledger (drop slot padding and blank rows) once it is at least
/// this large AND more than half of it is padding.
const COMPACT_MIN_BYTES: u64 = 4 * 1024 * 1024;
/// Older builds cannot find a reservation row that a compaction moved, so a
/// legacy-size reservation row younger than this postpones compaction (its
/// owner may still be mid-dispatch). Rows written by this build relocate by
/// marker and never block.
const LEGACY_INFLIGHT_GRACE_S: i64 = 3600;

/// An armed reservation row older than this can no longer be reconciled by
/// its owner (every dispatch is bounded far below it), so compaction may trim
/// it to its JSON. It stays in the ledger as unknown-cost spend: trimming
/// only drops the slot padding that let the owner write its final entry.
const STALE_RESERVATION_TRIM_S: i64 = 6 * 3600;

fn is_stale_armed_reservation(entry: &Entry, now: DateTime<Utc>) -> bool {
    entry.provider == RESERVATION_PROVIDER
        && entry.model == ARMED_RESERVATION_MODEL
        && (now - entry.ts_utc).num_seconds() >= STALE_RESERVATION_TRIM_S
}

/// True when the row is a reservation that a live process may still own: a
/// prepared reservation, or an armed one that is not yet stale.
///
/// A row that is not a live reservation holds finished spend, even when its
/// cost is unknown, and its padding is reclaimable.
fn is_live_reservation(entry: &Entry, now: DateTime<Utc>) -> bool {
    if entry.provider != RESERVATION_PROVIDER {
        return false;
    }
    match entry.model.as_str() {
        PREPARED_RESERVATION_MODEL => true,
        ARMED_RESERVATION_MODEL => !is_stale_armed_reservation(entry, now),
        _ => false,
    }
}

/// What the recovery scan learned about compaction.
#[derive(Debug, Default, Clone, Copy)]
struct ScanStats {
    /// Padding bytes held by finished rows and blank slots.
    waste: u64,
    /// A recent legacy-size reservation row exists (an older build may be
    /// mid-dispatch on it), so compaction must wait.
    legacy_inflight: bool,
}

fn is_reservation_slot_len(bytes: usize) -> bool {
    bytes == BILLABLE_RESERVATION_BYTES || bytes == LEGACY_RESERVATION_BYTES
}

/// True when `haystack` contains `needle`.
///
/// The recovery scan runs on every ledger write, so it must not parse every
/// row. The UTF-8 path uses the standard library's substring search; the
/// fallback covers a row that is not valid UTF-8.
fn contains_bytes(haystack: &[u8], needle: &str) -> bool {
    match std::str::from_utf8(haystack) {
        Ok(text) => text.contains(needle),
        Err(_) => {
            let needle = needle.as_bytes();
            haystack
                .windows(needle.len())
                .any(|window| window == needle)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub ts_utc: DateTime<Utc>,
    pub provider: String,
    pub model: String,
    /// Publisher/host of the model — the segment before `/` in the model id
    /// (e.g. `meta` for `meta/llama-3.3-70b-instruct`). This is the *publisher*
    /// scope, distinct from `provider` (who served the call): the same model
    /// (`meta/...`) can be served by `enterprise-gw` and by `openrouter`, and a
    /// `--host meta` view counts both while `--vendor` counts one. Empty for
    /// un-prefixed model ids and for legacy entries written before this field
    /// existed; `#[serde(default)]` keeps those entries deserializable.
    #[serde(default)]
    pub host: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    /// True when no authoritative telemetry or catalog price was available.
    /// The numeric field remains for wire compatibility, but reports must not
    /// interpret its placeholder zero as a verified-free call. Missing on
    /// historical rows means the recorded numeric cost was known.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cost_unknown: bool,
    /// Underlying calls this row represents (1 = per-call; >1 = rollup). Legacy = 1.
    #[serde(default = "one_call")]
    pub calls: u64,
    /// Set when the post-call free-policy guard flagged this spend (e.g. a
    /// "free" model that was actually billed or served from a paid backend).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub violation: Option<String>,
    /// Optional FinOps tags attached at ingestion time (Finding #22).
    /// `#[serde(default)]` keeps legacy entries deserializable: a line that
    /// pre-dates this field still produces a valid `Entry` with empty tags.
    #[serde(default)]
    pub tags: FinOpsTags,
    /// The serving provider's billing posture at write time: `"free"`,
    /// `"subscription"` or `"metered"`. Set at every write site that knows
    /// the provider. `#[serde(default, skip_serializing_if =
    /// "Option::is_none")]` keeps lines written before this field existed
    /// parsing unchanged — an old row still deserializes with
    /// `billing: None` and is re-serialized without the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing: Option<String>,
    /// The engine's own cost figure for this turn, when it reported a
    /// positive one. It is an ESTIMATE — the engine prices from the catalog
    /// zoder published, not from a provider bill — so it is kept beside the
    /// billed `cost_usd` rather than inside it (Z3/Incident B, D8 step 1).
    ///
    /// On a cost-neutral posture (a free route or a flat plan) `cost_usd`
    /// is a known $0 and this field is the only trace of what the engine
    /// would have charged: a local model the engine priced from a cloud
    /// listing shows its estimate without billing the host for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
}

fn one_call() -> u64 {
    1
}

impl Entry {
    /// True when this row was BILLED (pay-as-you-go), as opposed to served by
    /// a flat-fee plan or a free route, whose marginal cost is $0.
    ///
    /// `billing: None` is a row written before the field existed: metered
    /// history. Every unrecognised value is also treated as metered, so a
    /// future posture can never silently stop counting toward the cap.
    pub fn is_metered(&self) -> bool {
        match self.billing.as_deref() {
            Some("free") | Some("subscription") => false,
            _ => true,
        }
    }
}

impl Entry {
    /// The effective publisher host for rollups/filters: the stored `host` if
    /// present, otherwise derived from the model id on the fly so historical
    /// entries (written before `host` existed) still bucket by publisher.
    /// Returns "" only for un-prefixed model ids.
    pub fn effective_host(&self) -> String {
        if self.host.is_empty() {
            host_of_model(&self.model)
        } else {
            self.host.clone()
        }
    }
}

fn entry_numbers_valid(e: &Entry) -> bool {
    e.calls > 0
        && e.cost_usd.is_finite()
        && e.cost_usd >= 0.0
        && e.tags
            .cache_hit_ratio
            .is_none_or(|v| v.is_finite() && (0.0..=1.0).contains(&v))
}

fn add_cost(total: &mut f64, cost: f64) -> anyhow::Result<()> {
    let next = *total + cost;
    if !next.is_finite() {
        anyhow::bail!("ledger cost rollup overflowed; refusing to report a misleading total");
    }
    *total = next;
    Ok(())
}

/// Derive the publisher host from a model id: the segment before the first `/`
/// (`meta/llama-3.3-70b-instruct` -> `meta`). Returns "" for un-prefixed ids.
pub fn host_of_model(model: &str) -> String {
    model
        .split_once('/')
        .map(|(h, _)| h.to_string())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy)]
pub enum Period {
    Day,
    Week,
    Month,
    Year,
}

impl Period {
    pub fn parse(s: &str) -> Option<Period> {
        match s.to_ascii_lowercase().as_str() {
            "day" | "daily" => Some(Period::Day),
            "week" | "weekly" => Some(Period::Week),
            "month" | "monthly" => Some(Period::Month),
            "year" | "yearly" => Some(Period::Year),
            _ => None,
        }
    }
    fn bucket(&self, ts: &DateTime<Utc>) -> String {
        match self {
            Period::Day => ts.format("%Y-%m-%d").to_string(),
            Period::Week => {
                let w: IsoWeek = ts.iso_week();
                format!("{}-W{:02}", w.year(), w.week())
            }
            Period::Month => ts.format("%Y-%m").to_string(),
            Period::Year => ts.format("%Y").to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Rollup {
    pub cost_usd: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub calls: u64,
    /// Usage whose price was unknown. It is deliberately segregated from the
    /// known-cost token/call denominator so `$0` is never inferred.
    pub unknown_cost_tokens: u64,
    pub unknown_cost_calls: u64,
    /// How many LEDGER ROWS were folded into the unknown-cost fields above.
    ///
    /// Separate from both of them: a row's token count is not its row count,
    /// and `calls = 0` is a legal row (so the call counter cannot stand in
    /// either). Only a bundle that folds unpriced rows sets this.
    pub unknown_cost_rows: u64,
}

fn accumulate_rollup(rollup: &mut Rollup, entry: &Entry) -> anyhow::Result<()> {
    if entry.cost_unknown {
        rollup.unknown_cost_tokens = rollup
            .unknown_cost_tokens
            .saturating_add(entry.tokens_in.saturating_add(entry.tokens_out));
        rollup.unknown_cost_calls = rollup.unknown_cost_calls.saturating_add(entry.calls);
        rollup.unknown_cost_rows = rollup.unknown_cost_rows.saturating_add(1);
        return Ok(());
    }
    add_cost(&mut rollup.cost_usd, entry.cost_usd)?;
    rollup.tokens_in = rollup.tokens_in.saturating_add(entry.tokens_in);
    rollup.tokens_out = rollup.tokens_out.saturating_add(entry.tokens_out);
    rollup.calls = rollup.calls.saturating_add(entry.calls);
    Ok(())
}

pub struct Ledger {
    path: PathBuf,
}

/// Suffix of the sidecar checkpoint kept beside the ledger. Like the `.lock`
/// companion it is derived from the ledger path, so the two move together.
const CHECKPOINT_SUFFIX: &str = ".checkpoint";
/// Layout version of the checkpoint. A checkpoint of another version is
/// ignored (the full scan rebuilds it), never misread.
///
/// Version 2: month totals hold only metered rows (D8 step 1). A version-1
/// checkpoint also summed `subscription`/`free` rows, so it is rebuilt.
const CHECKPOINT_VERSION: u32 = 2;
/// A checkpoint holds a few numbers per calendar month; a file this large was
/// not written by this build and is not parsed.
const MAX_CHECKPOINT_BYTES: u64 = 1024 * 1024;
/// Bytes hashed at each end of the checkpointed prefix. A ledger truncated in
/// place and regrown past the old offset keeps its inode, so the inode alone
/// cannot prove that the prefix still holds the bytes that were folded.
const CHECKPOINT_FINGERPRINT_BYTES: u64 = 64 * 1024;

/// What one calendar month's METERED rows cost, accumulated in file order exactly as
/// `month_to_date_from_entries` accumulates it, so continuing a checkpointed
/// total with later rows gives the same bits as summing every row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
struct MonthTotal {
    /// Known-cost spend (USD), the running sum before any overflow. Stored as
    /// its IEEE-754 bits: JSON's decimal text does not promise to parse back
    /// to the same double, and the total must match the full scan exactly.
    #[serde(with = "f64_bits")]
    cost_usd: f64,
    /// The running known-cost sum stopped being finite. Kept as a flag because
    /// JSON cannot carry an infinite number.
    overflowed: bool,
    /// Calls in the month whose cost is unknown.
    unknown_cost_calls: u64,
}

/// (De)serialize an `f64` as its exact bit pattern.
mod f64_bits {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(value.to_bits())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
        u64::deserialize(deserializer).map(f64::from_bits)
    }
}

impl MonthTotal {
    fn add(&mut self, entry: &Entry) {
        // The cap counts only BILLED spend: a `subscription` or `free` row is
        // $0 marginal, exactly as `month_to_date_from_entries` filters it.
        if !entry.is_metered() {
            return;
        }
        if entry.cost_unknown {
            self.unknown_cost_calls = self.unknown_cost_calls.saturating_add(entry.calls);
            return;
        }
        if self.overflowed {
            return;
        }
        let next = self.cost_usd + entry.cost_usd;
        if next.is_finite() {
            self.cost_usd = next;
        } else {
            self.overflowed = true;
        }
    }

    /// The same answer, and the same fail-closed errors in the same order, as
    /// `month_to_date_from_entries` over the month's rows.
    fn month_to_date(&self) -> anyhow::Result<f64> {
        if self.overflowed {
            anyhow::bail!("ledger cost rollup overflowed; refusing to report a misleading total");
        }
        if self.unknown_cost_calls > 0 {
            anyhow::bail!(
                "month-to-date spend contains {} call(s) with unknown cost",
                self.unknown_cost_calls
            );
        }
        Ok(self.cost_usd)
    }
}

/// Sidecar checkpoint: every byte of the ledger before `offset` has been
/// validated and folded into `months`, so only the rows after it are parsed
/// again.
///
/// `offset` only ever covers *settled* rows -- rows no code path rewrites in
/// place. A prepared or armed reservation slot is rewritten when it is armed,
/// reconciled, cancelled or recovered, so the prefix stops at the first such
/// row; a malformed or unterminated row stops it too, so a strict read keeps
/// failing on it. The identity, fingerprints and checksum make any other
/// change (rotation, compaction, truncation, a damaged checkpoint) discard the
/// whole checkpoint: it is never partially believed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct LedgerCheckpoint {
    version: u32,
    /// Inode and device of the ledger the offset belongs to (0 off unix).
    inode: u64,
    device: u64,
    /// Row boundary after the last settled row folded into `months`.
    offset: u64,
    /// Lines before `offset`, so a malformed row after it is still reported
    /// by its line number in the file.
    lines: usize,
    /// FNV-1a of the first and of the last [`CHECKPOINT_FINGERPRINT_BYTES`]
    /// bytes before `offset`.
    head_fingerprint: u64,
    tail_fingerprint: u64,
    /// Per-month totals of the rows before `offset`, keyed `YYYY-MM`.
    months: BTreeMap<String, MonthTotal>,
    /// Compactable padding before `offset`, as the recovery scan counts it.
    waste: u64,
    /// Newest legacy-size reservation-provider row before `offset`; recovery
    /// postpones compaction while it is younger than
    /// [`LEGACY_INFLIGHT_GRACE_S`].
    legacy_reservation_newest: Option<DateTime<Utc>>,
    /// FNV-1a of this checkpoint serialized with `checksum = 0`.
    checksum: u64,
}

/// FNV-1a, 64 bit. Stable across builds and platforms, which a fingerprint
/// stored on disk needs (`DefaultHasher` promises neither).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

impl LedgerCheckpoint {
    fn compute_checksum(&self) -> anyhow::Result<u64> {
        let mut unsigned = self.clone();
        unsigned.checksum = 0;
        Ok(fnv1a(&serde_json::to_vec(&unsigned)?))
    }

    /// True when this checkpoint describes the bytes `file` holds now.
    fn is_current_for(&self, file: &mut File) -> bool {
        if self.version != CHECKPOINT_VERSION || self.compute_checksum().ok() != Some(self.checksum)
        {
            return false;
        }
        let Ok(meta) = file.metadata() else {
            return false;
        };
        if file_identity(&meta) != (self.inode, self.device) || meta.len() < self.offset {
            return false;
        }
        prefix_fingerprints(file, self.offset).ok()
            == Some((self.head_fingerprint, self.tail_fingerprint))
    }
}

#[cfg(unix)]
fn file_identity(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.ino(), meta.dev())
}

#[cfg(not(unix))]
fn file_identity(_meta: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// Fingerprints of the first and the last [`CHECKPOINT_FINGERPRINT_BYTES`]
/// bytes before `offset`.
fn prefix_fingerprints(file: &mut File, offset: u64) -> std::io::Result<(u64, u64)> {
    let window = offset.min(CHECKPOINT_FINGERPRINT_BYTES);
    let mut read = |start: u64| -> std::io::Result<u64> {
        let mut buf = vec![0; usize::try_from(window).unwrap_or(0)];
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf)?;
        Ok(fnv1a(&buf))
    };
    Ok((read(0)?, read(offset - window)?))
}

fn checkpoint_path_for(ledger_path: &Path) -> PathBuf {
    let mut path = ledger_path.as_os_str().to_os_string();
    path.push(CHECKPOINT_SUFFIX);
    PathBuf::from(path)
}

/// Result of a strict read that starts from a checkpoint.
struct Fold {
    /// Per-month totals of every valid row in the file, prefix included.
    months: BTreeMap<String, MonthTotal>,
    /// The checkpoint describing the settled prefix after this read.
    next: LedgerCheckpoint,
    /// Lines actually read (not covered by the starting checkpoint).
    #[cfg_attr(not(test), allow(dead_code))]
    lines_read: usize,
    /// Offset of the checkpoint the read started from, if any.
    base_offset: Option<u64>,
}

impl Fold {
    /// The settled prefix grew (or there was no checkpoint), so the sidecar
    /// is worth rewriting.
    fn advanced(&self) -> bool {
        self.base_offset != Some(self.next.offset)
    }

    fn month_to_date(&self) -> anyhow::Result<f64> {
        self.month_to_date_at(Utc::now())
    }

    /// Month-to-date for the UTC calendar month containing `now`.
    fn month_to_date_at(&self, now: DateTime<Utc>) -> anyhow::Result<f64> {
        let bucket = Period::Month.bucket(&now);
        self.months
            .get(&bucket)
            .map_or(Ok(0.0), MonthTotal::month_to_date)
    }
}

/// Compactable padding of a settled row, counted exactly as
/// [`Ledger::recover_abandoned_prepared`] counts it: everything in a
/// reservation-sized row past its payload and newline.
fn slot_padding(line: &LedgerLine) -> u64 {
    if !is_reservation_slot_len(line.raw_len) {
        return 0;
    }
    let mut kept = line.payload.as_slice();
    while matches!(kept.last(), Some(b'\n') | Some(b'\r') | Some(b' ')) {
        kept = &kept[..kept.len() - 1];
    }
    line.raw_len.saturating_sub(kept.len() + 1) as u64
}

/// True for a reservation row that some code path may still rewrite in place.
fn is_unsettled_reservation(entry: &Entry) -> bool {
    entry.provider == RESERVATION_PROVIDER
        && (entry.model == PREPARED_RESERVATION_MODEL || entry.model == ARMED_RESERVATION_MODEL)
}

/// Strictly read the ledger from `base` (or from the start) and fold every
/// valid row into per-month totals.
///
/// `skip_prepared(entry, raw_len)` says whether a prepared reservation row is
/// abandoned and must be left out of the totals, as the recovery scan would
/// have blanked it; callers that already ran recovery pass `|_, _| Ok(false)`.
/// Malformed rows fail the read with the same error as
/// [`Ledger::entries_strict`], numbered by their line in the whole file.
fn fold_ledger(
    file: &mut File,
    path: &Path,
    base: Option<LedgerCheckpoint>,
    mut skip_prepared: impl FnMut(&Entry, usize) -> anyhow::Result<bool>,
) -> anyhow::Result<Fold> {
    let base_offset = base.as_ref().map(|cp| cp.offset);
    let mut next = base.unwrap_or_default();
    let mut months = next.months.clone();
    let mut settled = true;
    let mut malformed = Vec::new();
    let mut lines_read = 0usize;
    let mut lines = LedgerLines::new(file, path, next.offset, next.lines)
        .with_context(|| format!("reading ledger from {}", path.display()))?;
    for line in &mut lines {
        let line = line.with_context(|| format!("reading ledger from {}", path.display()))?;
        lines_read += 1;
        settled &= line.terminated;
        let end = line.start.saturating_add(line.raw_len as u64);
        if line.payload.iter().all(u8::is_ascii_whitespace) {
            if settled {
                next.waste = next.waste.saturating_add(slot_padding(&line));
                next.offset = end;
                next.lines = line.line_no;
            }
            continue;
        }
        let entry = match serde_json::from_slice::<Entry>(&line.payload) {
            Ok(entry) if entry_numbers_valid(&entry) => entry,
            _ => {
                malformed.push(line.line_no);
                settled = false;
                continue;
            }
        };
        let bucket = Period::Month.bucket(&entry.ts_utc);
        if is_unsettled_reservation(&entry) {
            settled = false;
            if entry.model == PREPARED_RESERVATION_MODEL && skip_prepared(&entry, line.raw_len)? {
                continue;
            }
            months.entry(bucket).or_default().add(&entry);
            continue;
        }
        months.entry(bucket.clone()).or_default().add(&entry);
        if settled {
            next.months.entry(bucket).or_default().add(&entry);
            next.waste = next.waste.saturating_add(slot_padding(&line));
            if line.raw_len == LEGACY_RESERVATION_BYTES && entry.provider == RESERVATION_PROVIDER {
                next.legacy_reservation_newest = Some(
                    next.legacy_reservation_newest
                        .map_or(entry.ts_utc, |newest| newest.max(entry.ts_utc)),
                );
            }
            next.offset = end;
            next.lines = line.line_no;
        }
    }
    if !malformed.is_empty() {
        anyhow::bail!(
            "cannot establish ledger integrity: {} contains malformed non-empty row(s) at line(s) {}",
            path.display(),
            malformed
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(Fold {
        months,
        next,
        lines_read,
        base_offset,
    })
}

/// Durable, preallocated accounting transaction for one billable dispatch.
/// If dropped without reconciliation, its valid unknown-cost row remains in
/// the ledger, forcing subsequent budget/reporting decisions to fail closed.
pub struct BillableReservation {
    path: PathBuf,
    lock_path: PathBuf,
    offset: u64,
    slot_bytes: usize,
    marker: String,
    month_to_date: Result<f64, String>,
    armed: bool,
    /// True once the slot holds a final entry, so the drop guard knows not to
    /// overwrite it.
    reconciled: bool,
    /// The provider and model the call was dispatched to, when the caller
    /// knows them. A reservation that never gets a final entry is reconciled
    /// into an unknown-cost row that names them, so the unknown spend is
    /// attributable to the model that caused it.
    dispatched_provider: String,
    dispatched_model: String,
    /// When the reservation was created. The reconciled unknown-cost row keeps
    /// it, so the row is dated at the dispatch and not at the failure.
    ts_utc: DateTime<Utc>,
    owner_lock: Option<File>,
    owner_lock_path: PathBuf,
}

const RESERVATION_PROVIDER: &str = "__zoder_reservation__";
const PREPARED_RESERVATION_MODEL: &str = "__zoder_billable_p__";
const ARMED_RESERVATION_MODEL: &str = "__zoder_billable_a__";
/// A reservation that was dispatched, whose cost could not be determined,
/// and that was reconciled: the dispatch is over and the money is unknown.
/// It is distinct from [`ARMED_RESERVATION_MODEL`], which marks a dispatch
/// that may still be in flight.
const UNKNOWN_COST_RESERVATION_MODEL: &str = "__zoder_billable_u__";
/// The prefix the reservation model names share. It is a cheap pre-filter:
/// a ledger row that does not contain it cannot be a reservation, so it does
/// not have to be parsed.
const RESERVATION_MODEL_PREFIX: &str = "__zoder_billable_";

impl BillableReservation {
    /// Month-to-date known spend from the strict snapshot taken while this
    /// reservation was created under the exclusive sidecar lock.
    pub fn month_to_date_usd(&self) -> anyhow::Result<f64> {
        self.month_to_date
            .as_ref()
            .copied()
            .map_err(|message| anyhow::anyhow!(message.clone()))
    }

    /// Confirm this reservation's slot is still at `offset`; if a compaction
    /// moved it, find it again by its unique marker and update `offset`.
    /// Fails closed (as before) when the marker is nowhere in the ledger, e.g.
    /// the canonical file was replaced. Caller holds the ledger lock.
    fn relocate(&mut self, file: &mut File) -> anyhow::Result<()> {
        if verify_reservation_slot(file, self.offset, self.slot_bytes, &self.marker, &self.path)
            .is_ok()
        {
            return Ok(());
        }
        match find_reservation_slot(file, self.slot_bytes, &self.marker)? {
            Some(offset) => {
                self.offset = offset;
                Ok(())
            }
            None => verify_reservation_slot(
                file,
                self.offset,
                self.slot_bytes,
                &self.marker,
                &self.path,
            ),
        }
    }

    /// Mark the reservation as immediately preceding dispatch. Before this is
    /// called, dropping the guard cancels the slot (for example when a user
    /// declines a budget prompt); afterward, an unreconciled slot is retained
    /// as unknown spend so failures cannot become unaccounted retries.
    pub fn arm(&mut self) -> anyhow::Result<()> {
        self.arm_for("", "")
    }

    /// [`Self::arm`], recording who the call is dispatched to.
    ///
    /// The provider and model are used by the drop guard only: if the call
    /// fails, times out, is cancelled, or the process unwinds, the
    /// reservation is reconciled into an unknown-cost row that names the
    /// model, so the unknown spend is reported against the model that caused
    /// it instead of against a reservation placeholder.
    pub fn arm_for(&mut self, provider: &str, model: &str) -> anyhow::Result<()> {
        if self.armed {
            anyhow::bail!("billable reservation is already armed");
        }
        let lock = open_lock_file(&self.lock_path)?;
        lock_exclusive_bounded(&lock, &self.lock_path)?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .with_context(|| {
                format!("opening ledger at {} before dispatch", self.path.display())
            })?;
        self.relocate(&mut file)?;
        transition_reservation_to_armed(
            &mut file,
            self.offset,
            self.slot_bytes,
            &self.marker,
            &self.path,
        )?;
        self.armed = true;
        self.dispatched_provider = provider.to_string();
        self.dispatched_model = model.to_string();
        self.owner_lock.take();
        let _ = std::fs::remove_file(&self.owner_lock_path);
        Ok(())
    }

    /// Replace the reservation with an entry that records the truth: the call
    /// was dispatched, its cost is unknown, and it is NOT an unreconciled
    /// reservation.
    ///
    /// The row says the same thing about the money as the armed reservation
    /// did — the cost is unknown — without claiming a reconciliation that
    /// never happened. When the caller told the reservation who the call was
    /// dispatched to, the row carries that model, so the unknown spend shows
    /// up against the model that caused it.
    pub fn reconcile_unknown(&mut self) -> anyhow::Result<()> {
        let entry = self.unknown_cost_entry();
        self.write_slot(&entry)?;
        self.reconciled = true;
        Ok(())
    }

    /// The entry that records a dispatched call whose cost is unknown.
    fn unknown_cost_entry(&self) -> Entry {
        Entry {
            ts_utc: self.ts_utc,
            provider: if self.dispatched_provider.is_empty() {
                RESERVATION_PROVIDER.to_string()
            } else {
                self.dispatched_provider.clone()
            },
            model: if self.dispatched_model.is_empty() {
                ARMED_RESERVATION_MODEL.to_string()
            } else {
                self.dispatched_model.clone()
            },
            host: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            cost_unknown: true,
            calls: 1,
            violation: None,
            tags: FinOpsTags::default(),
            billing: None,
            estimated_cost_usd: None,
        }
    }

    /// Overwrite the reservation slot with `entry`, keeping the slot's fixed
    /// size so the rows after it do not move.
    fn write_slot(&mut self, entry: &Entry) -> anyhow::Result<()> {
        let json = fit_entry_json(entry, self.slot_bytes)?;
        if json.len() + 1 > self.slot_bytes {
            anyhow::bail!(
                "ledger entry is {} bytes, exceeding the {}-byte reservation slot",
                json.len() + 1,
                self.slot_bytes
            );
        }
        let mut line = vec![b' '; self.slot_bytes];
        line[..json.len()].copy_from_slice(&json);
        line[self.slot_bytes - 1] = b'\n';
        let lock = open_lock_file(&self.lock_path)?;
        lock_exclusive_bounded(&lock, &self.lock_path)?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .with_context(|| {
                format!(
                    "opening ledger at {} for reconciliation",
                    self.path.display()
                )
            })?;
        self.relocate(&mut file)?;
        file.seek(SeekFrom::Start(self.offset))?;
        file.write_all(&line)?;
        file.sync_data()?;
        Ok(())
    }

    /// Replace the preallocated unknown-cost reservation with the final entry.
    /// No allocation or append is needed after the provider has been called.
    pub fn reconcile(mut self, entry: &Entry) -> anyhow::Result<()> {
        self.armed = true;
        if !entry_numbers_valid(entry) {
            anyhow::bail!("ledger entry contains invalid cost, calls, or cache-hit telemetry");
        }
        let json = fit_entry_json(entry, self.slot_bytes)?;
        if json.len() + 1 > self.slot_bytes {
            anyhow::bail!(
                "ledger entry is {} bytes, exceeding the {}-byte preallocated reconciliation slot",
                json.len() + 1,
                self.slot_bytes
            );
        }
        let mut line = vec![b' '; self.slot_bytes];
        line[..json.len()].copy_from_slice(&json);
        line[self.slot_bytes - 1] = b'\n';
        let lock = open_lock_file(&self.lock_path)?;
        lock_exclusive_bounded(&lock, &self.lock_path)?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .with_context(|| {
                format!(
                    "opening ledger at {} for reconciliation",
                    self.path.display()
                )
            })?;
        self.relocate(&mut file)?;
        file.seek(SeekFrom::Start(self.offset))?;
        file.write_all(&line)?;
        file.sync_data()?;
        // The slot holds the final entry now. This is set only after the
        // write succeeded: when the write fails, the drop guard reconciles
        // the slot into an unknown-cost row instead of leaving it armed.
        self.reconciled = true;
        Ok(())
    }
}

/// # Lock deadline in `Drop`
///
/// Both branches below take the exclusive ledger lock through
/// [`lock_exclusive_bounded`], so a held lock cannot wedge the process on
/// the way out of a reservation.
///
/// On timeout, `Drop` LOGS AND GIVES UP — it never panics and never blocks
/// past the budget:
///
/// - an ARMED, unreconciled reservation (the call reached the provider, so
///   its cost is real) leaves `reconciled` false when the write fails, so
///   the in-memory state stays honest: nothing was written. The on-disk row
///   stays the armed reservation row, and a later writer's recovery scan
///   (`recover_abandoned_prepared` + the stale-armed pass) finds it once
///   this process's owner lock is gone and reconciles it into an
///   unknown-cost row. Retrying here would only extend a hang that has
///   already blocked for the whole budget.
/// - an UNARMED reservation (nothing was dispatched) leaves its prepared row
///   for the same recovery scan, which blanks it because the owner lock is
///   released by the code that follows.
///
/// `Drop` cannot report an error (it runs on unwinding and at scope exit),
/// so `tracing::warn!` names the lock and the budget: that is the only
/// signal the operator gets that the ledger lock is stuck.
///
/// Note that `Drop` uses the SAME budget as every other site: a drop that
/// meets a held lock waits up to the configured budget before giving up.
/// That is deliberate. Giving up on the first `try_lock` would skip the
/// cancel/reconcile of the common case — a peer releasing the lock moments
/// later — and leave rows to the recovery scan that this code can settle
/// now. The budget is what bounds the wait; `ZODER_LEDGER_LOCK_TIMEOUT_S`
/// lowers it for a process that must never stall on exit.
impl Drop for BillableReservation {
    fn drop(&mut self) {
        if self.armed && !self.reconciled {
            // The drop guard. The call was dispatched and its cost is
            // unknown, so the slot is reconciled into an unknown-cost row.
            // Without this the row stayed an armed reservation for ever and
            // every failed dispatch left a "reserved but not reconciled"
            // violation behind: thousands of them, fleet-wide.
            //
            // A lock deadline is not fatal to the accounting: the row on
            // disk is still the armed reservation row, and the next writer's
            // recovery scan turns it into an unknown-cost row once this
            // process's owner lock is gone. Give up, keep the row, say so.
            if let Err(error) = self.reconcile_unknown() {
                tracing::warn!(
                    "could not reconcile the armed ledger reservation at {}: {error:#}",
                    self.path.display()
                );
            }
        } else if !self.armed {
            // Other writers may have appended after this reservation was
            // created, so cancellation must blank only our fixed-size slot;
            // truncating to `offset` would discard their rows.
            //
            // On a lock deadline the prepared row is left alone: it is
            // already unknown-cost, and the recovery scan blanks it because
            // the owner lock is released below. Never panic, never wait
            // past the budget.
            if let Ok(lock) = open_lock_file(&self.lock_path) {
                match lock_exclusive_bounded(&lock, &self.lock_path) {
                    Ok(()) => {
                        if let Ok(mut file) = std::fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .open(&self.path)
                        {
                            if self.relocate(&mut file).is_ok() {
                                let mut blank = vec![b' '; self.slot_bytes];
                                blank[self.slot_bytes - 1] = b'\n';
                                let _ = file.seek(SeekFrom::Start(self.offset));
                                let _ = file.write_all(&blank);
                                let _ = file.sync_data();
                            }
                        }
                    }
                    Err(error) => tracing::warn!(
                        "could not cancel the ledger reservation at {}: {error:#}",
                        self.path.display()
                    ),
                }
            }
        }
        self.owner_lock.take();
        let _ = std::fs::remove_file(&self.owner_lock_path);
    }
}

/// Environment variable naming the ledger lock budget in seconds.
///
/// `fs2::FileExt::lock_exclusive` is a KERNEL WAIT with no deadline: it
/// blocks until the holder releases the `flock(2)`. A wedged holder — or an
/// orphaned flock — therefore hung every ledger writer for ever, and with it
/// `zoder review`/`zoder loop`, BEFORE the first provider call (zero tokens
/// spent, nothing to kill but the process). Reported as a 30 s-plus hang in
/// `zoder review` with 0 provider calls.
///
/// `ZODER_LEDGER_LOCK_TIMEOUT_S` bounds that wait:
///
/// - unset or empty: [`DEFAULT_LEDGER_LOCK_TIMEOUT_SECS`] (30 s);
/// - a positive number: that many seconds (fractions allowed, e.g. `0.5`);
/// - `0`: UNBOUNDED — the documented opt-out that restores the old
///   wait-for-ever behaviour, for callers that would rather queue behind a
///   slow but healthy holder than fail;
/// - anything else (negative, NaN, unparseable): the default, and a warning
///   naming the rejected value, because a typo must not silently disable the
///   bound.
pub const LEDGER_LOCK_TIMEOUT_ENV: &str = "ZODER_LEDGER_LOCK_TIMEOUT_S";

/// Default ledger lock budget: 30 s. Long enough to queue behind a healthy
/// holder doing a recovery scan of a large ledger, short enough that a
/// wedged lock surfaces as an error instead of an indefinite hang.
pub const DEFAULT_LEDGER_LOCK_TIMEOUT_SECS: f64 = 30.0;

/// First backoff step: 5 ms.
const LOCK_BACKOFF_START: Duration = Duration::from_millis(5);
/// Backoff ceiling: 100 ms, reached after five doublings.
const LOCK_BACKOFF_MAX: Duration = Duration::from_millis(100);

/// The configured ledger lock budget. `None` means unbounded.
fn ledger_lock_budget() -> Option<Duration> {
    let raw = std::env::var(LEDGER_LOCK_TIMEOUT_ENV).unwrap_or_default();
    let raw = raw.trim();
    if raw.is_empty() {
        return Some(Duration::from_secs_f64(DEFAULT_LEDGER_LOCK_TIMEOUT_SECS));
    }
    match raw.parse::<f64>() {
        Ok(0.0) => None,
        // `Duration::from_secs_f64` PANICS for a value it cannot represent
        // (e.g. 1e300), so a typo priced in the wrong unit would abort the
        // process instead of falling back. Only a value the duration can
        // actually hold counts as valid.
        Ok(secs) if secs > 0.0 => match Duration::try_from_secs_f64(secs) {
            Ok(budget) => Some(budget),
            Err(_) => {
                tracing::warn!(
                    "{LEDGER_LOCK_TIMEOUT_ENV}={raw:?} is too large to be a lock budget; \
                     using the {DEFAULT_LEDGER_LOCK_TIMEOUT_SECS} s default"
                );
                Some(Duration::from_secs_f64(DEFAULT_LEDGER_LOCK_TIMEOUT_SECS))
            }
        },
        _ => {
            tracing::warn!(
                "{LEDGER_LOCK_TIMEOUT_ENV}={raw:?} is not a number of seconds (or 0 for \
                 unbounded); using the {DEFAULT_LEDGER_LOCK_TIMEOUT_SECS} s default"
            );
            Some(Duration::from_secs_f64(DEFAULT_LEDGER_LOCK_TIMEOUT_SECS))
        }
    }
}

/// Human-readable budget for the timeout message: `30` for whole seconds,
/// `0.5` otherwise.
fn format_budget(budget: Duration) -> String {
    let secs = budget.as_secs_f64();
    // Test the Duration itself for whole seconds rather than `secs.fract()`:
    // the f64 can be non-integral through rounding while the Duration still
    // prints as an integer, and vice versa.
    if budget.subsec_nanos() == 0 {
        // `as u64` would saturate for an absurdly large configured budget
        // (e.g. 1e30 s), printing a number the operator never set. The
        // Duration is an exact whole number of seconds here, so render that.
        format!("{}", budget.as_secs())
    } else {
        format!("{secs}")
    }
}

/// Which side of the lock a bounded acquisition takes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LockMode {
    Exclusive,
    Shared,
}

impl LockMode {
    /// The mode's name in a timeout message.
    fn describe(self) -> &'static str {
        match self {
            LockMode::Exclusive => "exclusive",
            LockMode::Shared => "shared",
        }
    }
}

/// Take the ledger lock in `mode`, polling `try_lock_exclusive` /
/// `try_lock_shared` with a 5 ms backoff that doubles to a 100 ms ceiling
/// and giving up when the budget from [`LEDGER_LOCK_TIMEOUT_ENV`] expires.
///
/// Polling (not a blocking `lock_exclusive`/`lock_shared`) is what makes the
/// bound possible: `flock(2)` has no timeout, so the only way to stop
/// waiting is to not wait indefinitely in the first place. The first
/// attempt is immediate, so an uncontended lock costs nothing — keeping the
/// granularity of the ledger sidecar unchanged.
///
/// On timeout the error names the budget, the lock path, and the holder
/// situation, so an operator can tell a wedged peer from a slow one.
fn lock_bounded(file: &File, path: &Path, mode: LockMode, holder: &str) -> anyhow::Result<()> {
    let Some(budget) = ledger_lock_budget() else {
        return match mode {
            LockMode::Exclusive => FileExt::lock_exclusive(file),
            LockMode::Shared => FileExt::lock_shared(file),
        }
        .with_context(|| format!("locking ledger at {}", path.display()));
    };
    // `checked_add` rather than `+`: a representable but absurd budget must
    // degrade to unbounded, not overflow-panic on a platform whose Instant
    // is narrower than the Duration. The parsers already reject values the
    // Duration cannot hold; this closes the last way a typo could abort.
    let Some(deadline) = std::time::Instant::now().checked_add(budget) else {
        return match mode {
            LockMode::Exclusive => FileExt::lock_exclusive(file),
            LockMode::Shared => FileExt::lock_shared(file),
        }
        .with_context(|| format!("locking ledger at {}", path.display()));
    };
    let mut wait = LOCK_BACKOFF_START;
    let mut attempt: u64 = 0;
    loop {
        // fs2's `flock(2)` wrappers, named explicitly: std's inherent
        // `File::try_lock_*` are newer than this crate's MSRV (1.88) and
        // return a different error type.
        let result = match mode {
            LockMode::Exclusive => FileExt::try_lock_exclusive(file),
            LockMode::Shared => FileExt::try_lock_shared(file),
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                return Err(anyhow::Error::from(error)
                    .context(format!("locking ledger at {}", path.display())))
            }
        }
        attempt += 1;
        let now = std::time::Instant::now();
        if now >= deadline {
            anyhow::bail!(
                "timed out after {}s waiting for the ledger lock at {} ({} lock; {holder}; \
                 waited {attempt} time(s) up to this budget — set \
                 {LEDGER_LOCK_TIMEOUT_ENV} to a larger number of seconds, or 0 to wait \
                 indefinitely)",
                format_budget(budget),
                path.display(),
                mode.describe(),
            );
        }
        std::thread::sleep(wait.min(deadline.saturating_duration_since(now)));
        wait = (wait * 2).min(LOCK_BACKOFF_MAX);
    }
}

/// [`lock_bounded`] for the exclusive side: the ledger lock every writer
/// and every strict reader takes.
fn lock_exclusive_bounded(file: &File, path: &Path) -> anyhow::Result<()> {
    lock_bounded(file, path, LockMode::Exclusive, "another process holds it")
}

/// [`lock_exclusive_bounded`] for the per-reservation OWNER sidecar, whose
/// path carries a random per-reservation marker: only a thread of THIS
/// process can hold it, so the shared "another process holds it" hint would
/// send an operator hunting for a phantom peer.
fn lock_owner_sidecar_bounded(file: &File, path: &Path) -> anyhow::Result<()> {
    lock_bounded(
        file,
        path,
        LockMode::Exclusive,
        "another thread of this process holds it",
    )
}

/// [`lock_bounded`] for the shared side: the routing snapshot's read.
fn lock_shared_bounded(file: &File, path: &Path) -> anyhow::Result<()> {
    lock_bounded(file, path, LockMode::Shared, "another process holds it")
}

fn open_lock_file(path: &Path) -> anyhow::Result<File> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("opening ledger lock at {}", path.display()))
}

/// Serialize a final entry so it fits a `slot_bytes` slot (with its newline):
/// an over-long `violation` note is truncated rather than failing the
/// reconciliation and leaving the call recorded as unknown spend.
fn fit_entry_json(entry: &Entry, slot_bytes: usize) -> anyhow::Result<Vec<u8>> {
    let json = serde_json::to_vec(entry)?;
    if json.len() < slot_bytes {
        return Ok(json);
    }
    let Some(violation) = entry.violation.as_deref() else {
        return Ok(json);
    };
    let over = json.len() + 1 - slot_bytes;
    let suffix = " [truncated]";
    let keep = violation.len().saturating_sub(over + suffix.len() + 8);
    let mut cut = keep;
    while cut > 0 && !violation.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut trimmed = entry.clone();
    trimmed.violation = Some(format!("{}{suffix}", &violation[..cut]));
    Ok(serde_json::to_vec(&trimmed)?)
}

/// Scan for the line of exactly `slot_bytes` bytes that carries `marker`.
fn find_reservation_slot(
    file: &mut File,
    slot_bytes: usize,
    marker: &str,
) -> anyhow::Result<Option<u64>> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(&mut *file);
    let mut offset = 0_u64;
    let mut line = Vec::new();
    loop {
        line.clear();
        let bytes = (&mut reader)
            .take((MAX_LEDGER_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)?;
        if bytes == 0 {
            return Ok(None);
        }
        if bytes == slot_bytes
            && line
                .windows(marker.len())
                .any(|window| window == marker.as_bytes())
        {
            return Ok(Some(offset));
        }
        offset = offset.saturating_add(u64::try_from(bytes)?);
    }
}

fn verify_reservation_slot(
    file: &mut File,
    offset: u64,
    slot_bytes: usize,
    marker: &str,
    path: &Path,
) -> anyhow::Result<()> {
    let mut slot = vec![0; slot_bytes];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut slot).with_context(|| {
        format!(
            "reading reserved ledger slot at byte {offset} of {}",
            path.display()
        )
    })?;
    if !slot
        .windows(marker.len())
        .any(|window| window == marker.as_bytes())
    {
        anyhow::bail!(
            "reserved ledger slot at byte {offset} of {} is missing or was replaced",
            path.display()
        );
    }
    Ok(())
}

fn transition_reservation_to_armed(
    file: &mut File,
    offset: u64,
    slot_bytes: usize,
    marker: &str,
    path: &Path,
) -> anyhow::Result<()> {
    verify_reservation_slot(file, offset, slot_bytes, marker, path)?;
    let mut slot = vec![0; slot_bytes];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut slot)?;
    let prepared = PREPARED_RESERVATION_MODEL.as_bytes();
    let model_offset = slot
        .windows(prepared.len())
        .position(|window| window == prepared)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "reserved ledger slot at byte {offset} of {} is not in prepared phase",
                path.display()
            )
        })?;
    let phase_offset = PREPARED_RESERVATION_MODEL
        .rfind('p')
        .expect("prepared reservation model has a phase byte");
    debug_assert_eq!(
        &PREPARED_RESERVATION_MODEL.as_bytes()[..phase_offset],
        &ARMED_RESERVATION_MODEL.as_bytes()[..phase_offset]
    );
    debug_assert_eq!(
        &PREPARED_RESERVATION_MODEL.as_bytes()[phase_offset + 1..],
        &ARMED_RESERVATION_MODEL.as_bytes()[phase_offset + 1..]
    );
    file.seek(SeekFrom::Start(
        offset + u64::try_from(model_offset + phase_offset)?,
    ))?;
    file.write_all(b"a")?;
    file.sync_data()?;
    Ok(())
}

impl Ledger {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
        }
    }

    pub fn record(&self, e: &Entry) -> anyhow::Result<()> {
        if !entry_numbers_valid(e) {
            anyhow::bail!("ledger entry contains invalid cost, calls, or cache-hit telemetry");
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating ledger directory {}", parent.display()))?;
        }
        // Serialize record + newline into one buffer and emit it with a single
        // write_all. With O_APPEND this is one syscall, so concurrent writers
        // can't interleave a partial line. (writeln! issues two writes.)
        let mut line = serde_json::to_string(e)?;
        line.push('\n');
        let lock_path = self.lock_path();
        let lock = open_lock_file(&lock_path)?;
        lock_exclusive_bounded(&lock, &lock_path)?;
        let (mut f, base) = self.open_locked_rw("recording")?;
        let fold = fold_ledger(&mut f, &self.path, base, |_, _| Ok(false))?;
        self.publish_checkpoint(&mut f, fold);
        f.seek(SeekFrom::End(0))?;
        f.write_all(line.as_bytes())?;
        f.sync_data()?;
        Ok(())
    }

    /// Lock, strictly validate, and preallocate the durable row for one external
    /// billable dispatch. The stable sidecar lock is released before this
    /// returns; the persisted pending row makes concurrent budget decisions
    /// fail closed without holding a lock while arbitrary provider/tool code runs.
    pub fn reserve_billable(&self) -> anyhow::Result<BillableReservation> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating ledger directory {}", parent.display()))?;
        }
        let lock_path = self.lock_path();
        let lock = open_lock_file(&lock_path)?;
        lock_exclusive_bounded(&lock, &lock_path)?;
        let (mut file, base) = self.open_locked_rw("reservation")?;
        // Recovery ran under the exclusive lock, so no abandoned prepared row
        // is left to skip.
        let fold = fold_ledger(&mut file, &self.path, base, |_, _| Ok(false))?;
        let month_to_date = fold.month_to_date().map_err(|error| error.to_string());
        self.publish_checkpoint(&mut file, fold);

        let offset = file.seek(SeekFrom::End(0))?;
        let marker = format!("zoder-reservation-{:032x}", rand::random::<u128>());
        let owner_lock_path = reservation_owner_lock_path(&self.path, &marker);
        let owner_lock = open_lock_file(&owner_lock_path)?;
        // A per-reservation sidecar, so its only holder is a thread of this
        // process. Bounded like the ledger lock (never behind another
        // process, so it cannot be the 30 s hang) and named in its error.
        lock_owner_sidecar_bounded(&owner_lock, &owner_lock_path)?;
        let pending = Entry {
            ts_utc: Utc::now(),
            provider: RESERVATION_PROVIDER.to_string(),
            model: PREPARED_RESERVATION_MODEL.to_string(),
            host: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            cost_unknown: true,
            calls: 1,
            violation: Some(format!(
                "billable call reserved but not reconciled ({marker})"
            )),
            tags: FinOpsTags::default(),
            billing: None,
            estimated_cost_usd: None,
        };
        let json = serde_json::to_vec(&pending)?;
        debug_assert!(json.len() < BILLABLE_RESERVATION_BYTES);
        let mut slot = vec![b' '; BILLABLE_RESERVATION_BYTES];
        slot[..json.len()].copy_from_slice(&json);
        slot[BILLABLE_RESERVATION_BYTES - 1] = b'\n';
        let preallocate = file
            .write_all(&slot)
            .with_context(|| {
                format!(
                    "preallocating billable ledger entry at {}",
                    self.path.display()
                )
            })
            .and_then(|()| {
                file.sync_data().with_context(|| {
                    format!(
                        "syncing billable ledger reservation at {}",
                        self.path.display()
                    )
                })
            });
        if let Err(error) = preallocate {
            let rollback = file
                .set_len(offset)
                .and_then(|()| file.sync_data())
                .with_context(|| {
                    format!(
                        "rolling back failed ledger reservation at {}",
                        self.path.display()
                    )
                });
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(error.context(rollback_error)),
            };
        }
        Ok(BillableReservation {
            path: self.path.clone(),
            lock_path,
            offset,
            slot_bytes: BILLABLE_RESERVATION_BYTES,
            marker,
            month_to_date,
            armed: false,
            reconciled: false,
            dispatched_provider: String::new(),
            dispatched_model: String::new(),
            ts_utc: pending.ts_utc,
            owner_lock: Some(owner_lock),
            owner_lock_path,
        })
    }

    fn lock_path(&self) -> PathBuf {
        let mut lock = self.path.as_os_str().to_os_string();
        lock.push(".lock");
        PathBuf::from(lock)
    }

    /// Recover abandoned prepared reservations and return the number of
    /// padding bytes held by finished (non-reservation) rows and blank rows,
    /// which [`Self::compact_if_wasteful`] uses to decide on compaction.
    ///
    /// With a current checkpoint the scan starts at its offset: the settled
    /// prefix holds no prepared or armed row, and its padding and newest
    /// legacy reservation row are carried by the checkpoint.
    fn recover_abandoned_prepared(
        &self,
        file: &mut File,
        base: Option<&LedgerCheckpoint>,
    ) -> anyhow::Result<ScanStats> {
        let now = Utc::now();
        let mut offset = base.map_or(0, |cp| cp.offset);
        let mut line_no = base.map_or(0, |cp| cp.lines);
        let mut waste = base.map_or(0, |cp| cp.waste);
        let mut legacy_inflight = base
            .and_then(|cp| cp.legacy_reservation_newest)
            .is_some_and(|newest| (now - newest).num_seconds() < LEGACY_INFLIGHT_GRACE_S);
        file.seek(SeekFrom::Start(offset))?;
        let mut reader = BufReader::new(&mut *file);
        let mut line = Vec::new();
        let mut abandoned = Vec::new();
        let mut stale_armed_rows = Vec::new();
        loop {
            line_no += 1;
            line.clear();
            let bytes = (&mut reader)
                .take((MAX_LEDGER_LINE_BYTES + 1) as u64)
                .read_until(b'\n', &mut line)?;
            if bytes == 0 {
                break;
            }
            if bytes > MAX_LEDGER_LINE_BYTES {
                anyhow::bail!(
                    "ledger line {line_no} exceeds {MAX_LEDGER_LINE_BYTES} bytes; truncating early"
                );
            }
            let mut payload = line.as_slice();
            while matches!(payload.last(), Some(b'\n') | Some(b'\r') | Some(b' ')) {
                payload = &payload[..payload.len() - 1];
            }
            // Cheap pre-filter: only a row that mentions a reservation model
            // can be a reservation, so a large ledger is not parsed line by
            // line.
            let mentions_reservation = contains_bytes(payload, RESERVATION_MODEL_PREFIX);
            let is_slot = is_reservation_slot_len(bytes);
            if is_slot || mentions_reservation {
                let parsed = serde_json::from_slice::<Entry>(payload).ok();
                let is_reservation = parsed
                    .as_ref()
                    .is_some_and(|entry| entry.provider == RESERVATION_PROVIDER);
                if is_reservation
                    && bytes == LEGACY_RESERVATION_BYTES
                    && parsed.as_ref().is_some_and(|entry| {
                        (now - entry.ts_utc).num_seconds() < LEGACY_INFLIGHT_GRACE_S
                    })
                {
                    legacy_inflight = true;
                }
                let stale_armed = parsed
                    .as_ref()
                    .is_some_and(|entry| is_stale_armed_reservation(entry, now));
                let live_reservation = parsed
                    .as_ref()
                    .is_some_and(|entry| is_live_reservation(entry, now));
                if is_slot && !live_reservation {
                    // Finished, blank or stale-armed slot: everything past
                    // the payload and its newline is compactable padding.
                    waste = waste.saturating_add(
                        u64::try_from(bytes.saturating_sub(payload.len() + 1)).unwrap_or(0),
                    );
                }
                if stale_armed {
                    // A stale armed reservation can never be reconciled by the
                    // process that armed it: that process is gone, and every
                    // dispatch is bounded far below the age at which a
                    // reservation goes stale. It is reconciled here instead,
                    // so the ledger stops reporting a reservation that was
                    // never reconciled.
                    if let Some(entry) = parsed.as_ref() {
                        stale_armed_rows.push((offset, bytes, entry.ts_utc));
                    }
                }
                if is_slot {
                    if let Some(entry) = parsed {
                        if entry.provider == RESERVATION_PROVIDER
                            && entry.model == PREPARED_RESERVATION_MODEL
                        {
                            let marker = reservation_marker(&entry).ok_or_else(|| {
                                anyhow::anyhow!(
                                    "prepared reservation at byte {offset} has invalid owner metadata"
                                )
                            })?;
                            let owner_path = reservation_owner_lock_path(&self.path, marker);
                            let owner = open_lock_file(&owner_path)?;
                            match owner.try_lock_exclusive() {
                                Ok(()) => abandoned.push((offset, bytes, owner_path, owner)),
                                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                                Err(error) => return Err(error.into()),
                            }
                        }
                    }
                }
            }
            offset = offset.saturating_add(u64::try_from(bytes)?);
        }
        drop(reader);
        if abandoned.is_empty() && stale_armed_rows.is_empty() {
            return Ok(ScanStats {
                waste,
                legacy_inflight,
            });
        }
        let mut writable = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .with_context(|| {
                format!(
                    "opening ledger at {} to recover abandoned reservations",
                    self.path.display()
                )
            })?;
        for (offset, bytes, owner_path, owner) in abandoned {
            let mut blank = vec![b' '; bytes];
            blank[bytes - 1] = b'\n';
            writable.seek(SeekFrom::Start(offset))?;
            writable.write_all(&blank)?;
            waste = waste.saturating_add(u64::try_from(bytes).unwrap_or(0));
            drop(owner);
            let _ = std::fs::remove_file(owner_path);
        }
        for (offset, bytes, ts_utc) in stale_armed_rows {
            let entry = Entry {
                ts_utc,
                provider: RESERVATION_PROVIDER.to_string(),
                model: UNKNOWN_COST_RESERVATION_MODEL.to_string(),
                host: String::new(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                cost_unknown: true,
                calls: 1,
                violation: None,
                tags: FinOpsTags::default(),
                billing: None,
                estimated_cost_usd: None,
            };
            let json = fit_entry_json(&entry, bytes)?;
            if json.len() + 1 > bytes {
                // The row is too small to hold the reconciled entry. Leave it
                // armed: a stale armed reservation is reported as
                // unreconciled, which is honest, and the ledger stays usable.
                continue;
            }
            let mut line = vec![b' '; bytes];
            line[..json.len()].copy_from_slice(&json);
            line[bytes - 1] = b'\n';
            writable.seek(SeekFrom::Start(offset))?;
            writable.write_all(&line)?;
        }
        writable.sync_data()?;
        Ok(ScanStats {
            waste,
            legacy_inflight,
        })
    }

    /// Rewrite the ledger without slot padding and blank rows when it is at
    /// least [`COMPACT_MIN_BYTES`] and more than half padding, and no recent
    /// legacy-size reservation row exists (see [`LEGACY_INFLIGHT_GRACE_S`]).
    /// Caller holds the
    /// exclusive ledger lock. Reservation rows (prepared or armed, possibly
    /// owned by a live process mid-dispatch) are copied byte-for-byte so their
    /// owners can find them again by marker; finished rows keep their exact
    /// JSON; malformed rows are copied unchanged so strict reads still see
    /// them. The new file replaces the old one by an atomic rename. Returns
    /// true when the file was replaced (the caller must reopen it).
    fn compact_if_wasteful(&self, file: &mut File, stats: ScanStats) -> anyhow::Result<bool> {
        let now = Utc::now();
        let len = file.metadata()?.len();
        if len < COMPACT_MIN_BYTES || stats.waste.saturating_mul(2) <= len || stats.legacy_inflight
        {
            return Ok(false);
        }
        let dir = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut tmp_name = self.path.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(format!(".compact-{:016x}", rand::random::<u64>()));
        let tmp_path = dir.join(tmp_name);
        let result = (|| -> anyhow::Result<()> {
            let mut out = std::io::BufWriter::new(
                std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&tmp_path)
                    .with_context(|| format!("creating {}", tmp_path.display()))?,
            );
            file.seek(SeekFrom::Start(0))?;
            let mut reader = BufReader::new(&mut *file);
            let mut line = Vec::new();
            loop {
                line.clear();
                let bytes = (&mut reader)
                    .take((MAX_LEDGER_LINE_BYTES + 1) as u64)
                    .read_until(b'\n', &mut line)?;
                if bytes == 0 {
                    break;
                }
                if bytes > MAX_LEDGER_LINE_BYTES {
                    anyhow::bail!(
                        "ledger line exceeds {MAX_LEDGER_LINE_BYTES} bytes; not compacting"
                    );
                }
                let mut payload = line.as_slice();
                while matches!(payload.last(), Some(b'\n') | Some(b'\r') | Some(b' ')) {
                    payload = &payload[..payload.len() - 1];
                }
                if payload.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                match serde_json::from_slice::<Entry>(payload) {
                    // A live reservation row (prepared, or armed and not yet
                    // stale) is copied byte-for-byte so its owner can find it
                    // again by marker. Every other row is finished spend and
                    // its padding is reclaimable.
                    Ok(entry) if !is_live_reservation(&entry, now) => {
                        out.write_all(payload)?;
                        out.write_all(b"\n")?;
                    }
                    // Live reservation rows and anything unparseable:
                    // unchanged.
                    _ => {
                        out.write_all(&line)?;
                        if line.last() != Some(&b'\n') {
                            out.write_all(b"\n")?;
                        }
                    }
                }
            }
            let out = out.into_inner().map_err(|e| e.into_error())?;
            if let Ok(meta) = file.metadata() {
                let _ = out.set_permissions(meta.permissions());
            }
            out.sync_all()?;
            std::fs::rename(&tmp_path, &self.path).with_context(|| {
                format!("replacing {} with its compacted copy", self.path.display())
            })?;
            if let Ok(d) = File::open(dir) {
                let _ = d.sync_all();
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(error);
        }
        Ok(true)
    }

    /// Open the ledger for a locked read/write, recover abandoned reservations
    /// and compact it if it is mostly padding. Caller holds the ledger lock.
    /// Also returns the checkpoint when it is current for the returned file.
    fn open_locked_rw(&self, what: &str) -> anyhow::Result<(File, Option<LedgerCheckpoint>)> {
        let open = || {
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&self.path)
                .with_context(|| format!("opening ledger for {what} at {}", self.path.display()))
        };
        let mut file = open()?;
        let mut base = self.load_checkpoint(&mut file);
        let stats = self.recover_abandoned_prepared(&mut file, base.as_ref())?;
        if self.compact_if_wasteful(&mut file, stats)? {
            file = open()?;
            base = None;
        }
        Ok((file, base))
    }

    /// The sidecar checkpoint, when it describes the bytes `file` holds now.
    fn load_checkpoint(&self, file: &mut File) -> Option<LedgerCheckpoint> {
        let path = checkpoint_path_for(&self.path);
        if std::fs::metadata(&path).ok()?.len() > MAX_CHECKPOINT_BYTES {
            return None;
        }
        let checkpoint: LedgerCheckpoint =
            serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
        checkpoint.is_current_for(file).then_some(checkpoint)
    }

    /// Stamp `checkpoint` with `file`'s identity and fingerprints and replace
    /// the sidecar atomically (temporary file, then rename). The caller holds
    /// the ledger lock, shared or exclusive, so no writer changes the prefix
    /// meanwhile; concurrent shared holders publish identical content.
    ///
    /// Best effort: the checkpoint is a cache, so a read-only directory or a
    /// full disk only costs the next reader a longer scan, never a decision.
    fn publish_checkpoint(&self, file: &mut File, fold: Fold) {
        if !fold.advanced() {
            return;
        }
        let mut checkpoint = fold.next;
        let path = checkpoint_path_for(&self.path);
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(format!(".tmp-{:016x}", rand::random::<u64>()));
        let tmp = PathBuf::from(tmp);
        let result = (|| -> anyhow::Result<()> {
            let meta = file.metadata()?;
            (checkpoint.inode, checkpoint.device) = file_identity(&meta);
            (checkpoint.head_fingerprint, checkpoint.tail_fingerprint) =
                prefix_fingerprints(file, checkpoint.offset)?;
            checkpoint.version = CHECKPOINT_VERSION;
            checkpoint.checksum = checkpoint.compute_checksum()?;
            let mut out = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp)?;
            let _ = out.set_permissions(meta.permissions());
            out.write_all(&serde_json::to_vec(&checkpoint)?)?;
            drop(out);
            std::fs::rename(&tmp, &path)?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_file(&tmp);
            tracing::debug!(
                "not updating ledger checkpoint {}: {error:#}",
                path.display()
            );
        }
    }

    /// For a reader that may not run recovery (it holds only the shared lock):
    /// true when a prepared row is one recovery would blank, i.e. a
    /// reservation-sized row whose owner no longer holds its owner lock.
    ///
    /// The owner cannot arm or cancel the row while the shared ledger lock is
    /// held (both need the exclusive lock), so the answer stays true until
    /// the reader is done. Anything unclear keeps the row, which keeps its
    /// unknown cost in the totals and fails the month closed.
    fn prepared_owner_is_gone(&self, entry: &Entry, raw_len: usize) -> bool {
        if !is_reservation_slot_len(raw_len) {
            return false;
        }
        let Some(marker) = reservation_marker(entry) else {
            return false;
        };
        let owner = match File::open(reservation_owner_lock_path(&self.path, marker)) {
            Ok(owner) => owner,
            Err(error) => return error.kind() == std::io::ErrorKind::NotFound,
        };
        // Released when `owner` is dropped.
        owner.try_lock_exclusive().is_ok()
    }

    /// Open the ledger for a strict read under the SHARED sidecar lock. Readers
    /// do not serialize behind one another; writers, which take the exclusive
    /// lock, are still excluded, so the reader sees whole rows. `None` means
    /// there is no ledger yet.
    ///
    /// The SHARED acquisition is bounded too, for the same reason as the
    /// exclusive one: `flock(2)` has no timeout, so a holder that never
    /// releases — the case this whole change exists for — blocked the
    /// routing snapshot before the reviewer's first provider call exactly
    /// as it blocked the writers. "Only writers hold it exclusively" was
    /// true of well-behaved processes and false of the wedged one. Polling
    /// `try_lock_shared` (the shared mirror of
    /// [`lock_exclusive_bounded`]) keeps the shared read shared while
    /// giving it the same deadline.
    fn open_shared(&self) -> anyhow::Result<Option<(File, File)>> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating ledger directory {}", parent.display()))?;
        }
        let lock_path = self.lock_path();
        let lock = open_lock_file(&lock_path)?;
        lock_shared_bounded(&lock, &lock_path)?;
        match File::open(&self.path) {
            Ok(file) => Ok(Some((lock, file))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(anyhow::Error::from(error)
                .context(format!("opening ledger at {}", self.path.display()))),
        }
    }

    /// [`Self::entries_strict`] for routing: the same strict, fail-closed
    /// view, read under the shared lock so concurrent routing decisions do
    /// not queue behind one another.
    ///
    /// It does not run recovery (that writes), so an abandoned prepared or a
    /// stale armed reservation row is returned as it stands. Both are
    /// unknown-cost rows of the reservation pseudo-provider, which no quota
    /// window counts -- the same rows dry-run routing
    /// ([`Self::entries_readonly`]) has always seen.
    pub fn entries_strict_shared(&self) -> anyhow::Result<Vec<Entry>> {
        let Some((_lock, mut file)) = self.open_shared()? else {
            return Ok(Vec::new());
        };
        self.entries_strict_from_file(&mut file)
    }

    /// Parse all entries, invoking `on_malformed(line_no, raw_line)` for every
    /// non-empty line that fails to parse. A single mangled/half-written line
    /// (e.g. from an interrupted append) is skipped rather than aborting the
    /// rollup, but — unlike a silent drop — the caller can observe and surface
    /// dropped spend. Mirrors the TS `Ledger.entries({ onMalformed })`.
    ///
    /// Read errors are NOT swallowed — `NotFound` returns an empty vector
    /// (the canonical "no ledger yet" signal), every other I/O error
    /// propagates as an `Err`. The old behavior of collapsing any
    /// permission denial / invalid UTF-8 / partial write into an empty
    /// success result is what let a corrupted ledger under-report spend
    /// to $0 and bypass the pre-call budget gate (Finding #10).
    pub fn entries_observed(
        &self,
        on_malformed: impl FnMut(usize, &str),
    ) -> anyhow::Result<Vec<Entry>> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating ledger directory {}", parent.display()))?;
        }
        let lock_path = self.lock_path();
        let lock = open_lock_file(&lock_path)?;
        lock_exclusive_bounded(&lock, &lock_path)?;
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(anyhow::Error::from(e)
                    .context(format!("opening ledger at {}", self.path.display())))
            }
        };
        self.recover_abandoned_prepared(&mut file, None)?;
        read_entries(&mut file, &self.path, on_malformed)
    }

    /// All entries, silently skipping malformed lines. For visibility into
    /// dropped lines use [`Ledger::entries_observed`].
    ///
    /// I/O errors OTHER than "file does not exist" are propagated, not
    /// silently reported as an empty ledger — Finding #10.
    pub fn entries(&self) -> anyhow::Result<Vec<Entry>> {
        self.entries_observed(|_, _| {})
    }

    /// Read every valid entry and reject the ledger if any non-empty row is
    /// malformed. Quota and budget decisions must use this stricter view: a
    /// skipped row may contain spend, so continuing would be fail-open.
    pub fn entries_strict(&self) -> anyhow::Result<Vec<Entry>> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating ledger directory {}", parent.display()))?;
        }
        let lock_path = self.lock_path();
        let lock = open_lock_file(&lock_path)?;
        lock_exclusive_bounded(&lock, &lock_path)?;
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(anyhow::Error::from(error)
                    .context(format!("opening ledger at {}", self.path.display())))
            }
        };
        self.recover_abandoned_prepared(&mut file, None)?;
        self.entries_strict_from_file(&mut file)
    }

    /// Read entries without acquiring the lock. Used for dry-run routing
    /// where the ledger is read but no usage is recorded — the lock file
    /// need not be created or opened for writing.
    pub fn entries_readonly(&self) -> anyhow::Result<Vec<Entry>> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(anyhow::Error::from(error)
                    .context(format!("opening ledger at {}", self.path.display())))
            }
        };
        self.entries_strict_from_file(&mut file)
    }

    fn entries_strict_from_file(&self, file: &mut File) -> anyhow::Result<Vec<Entry>> {
        let mut malformed_lines = Vec::new();
        let entries = read_entries(file, &self.path, |line_no, _| {
            malformed_lines.push(line_no);
        })
        .with_context(|| format!("reading ledger from {}", self.path.display()))?;
        if !malformed_lines.is_empty() {
            anyhow::bail!(
                "cannot establish ledger integrity: {} contains malformed non-empty row(s) at line(s) {}",
                self.path.display(),
                malformed_lines
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Ok(entries)
    }

    /// Entries within an optional [since, until] window (inclusive).
    pub fn entries_in(
        &self,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Vec<Entry>> {
        Ok(self
            .entries_strict()?
            .into_iter()
            .filter(|e| since.map(|s| e.ts_utc >= s).unwrap_or(true))
            .filter(|e| until.map(|u| e.ts_utc <= u).unwrap_or(true))
            .collect())
    }

    /// Entries within an optional [since, until] window that also satisfy
    /// `keep`. Used by `zoder report --vendor <name>` to scope the report to
    /// a vendor's providers without rewriting the ledger. `keep` receives a
    /// borrow of each entry and returns `true` to keep it.
    pub fn entries_in_filtered(
        &self,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        mut keep: impl FnMut(&Entry) -> bool,
    ) -> anyhow::Result<Vec<Entry>> {
        Ok(self
            .entries_in(since, until)?
            .into_iter()
            .filter(|e| keep(e))
            .collect())
    }

    /// Spend rolled up by period bucket (sorted by bucket key).
    pub fn rollup(&self, period: Period) -> anyhow::Result<BTreeMap<String, Rollup>> {
        self.rollup_in(period, None, None)
    }

    /// Total spend (USD) recorded in the current UTC calendar month. Used by the
    /// pre-call budget gate to check a projected call against the monthly cap.
    ///
    /// Returns 0.0 ONLY when:
    /// - the ledger file does not exist (the canonical "no spend yet" signal), or
    /// - the file is empty.
    ///
    /// Returns `Err(_)` for every other failure (permission denied, invalid
    /// UTF-8, partial write). The caller (the pre-call budget gate) must
    /// treat `Err` as "could not read spend → fail CLOSED, request
    /// confirmation" rather than as $0 — the old behavior of collapsing
    /// any read failure into `0.0` is what let a corrupted ledger under-
    /// report spend to $0 and bypass the monthly cap (Finding #10).
    ///
    /// Reads under the shared lock and parses only the rows after the sidecar
    /// checkpoint. Recovery does not run here, so a prepared reservation whose
    /// owner is gone -- one recovery would blank -- is left out of the total,
    /// as it would be after recovery.
    pub fn month_to_date_usd(&self) -> anyhow::Result<f64> {
        self.month_to_date_usd_at(Utc::now())
    }

    /// [`Self::month_to_date_usd`] for the UTC calendar month containing
    /// `now`, so the month boundary can be tested with a pinned clock.
    fn month_to_date_usd_at(&self, now: DateTime<Utc>) -> anyhow::Result<f64> {
        let Some((_lock, mut file)) = self.open_shared()? else {
            return Ok(0.0);
        };
        let base = self.load_checkpoint(&mut file);
        let fold = fold_ledger(&mut file, &self.path, base, |entry, raw_len| {
            Ok(self.prepared_owner_is_gone(entry, raw_len))
        })?;
        let month_to_date = fold.month_to_date_at(now);
        self.publish_checkpoint(&mut file, fold);
        month_to_date
    }

    /// Spend rolled up by period bucket within an optional date window.
    pub fn rollup_in(
        &self,
        period: Period,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> anyhow::Result<BTreeMap<String, Rollup>> {
        let mut out: BTreeMap<String, Rollup> = BTreeMap::new();
        for e in self.entries_in(since, until)? {
            let r = out.entry(period.bucket(&e.ts_utc)).or_default();
            // `+= e.calls` honors legacy rollup entries (Finding #22).
            // A pre-fix ledger had every entry with `calls: 1`, so the
            // old `+= 1` happened to be correct — but a rollup row with
            // `calls: 10` (a legitimate aggregate) was counted as one.
            accumulate_rollup(r, &e)?;
        }
        Ok(out)
    }

    /// Spend rolled up by period bucket within a window, keeping only entries
    /// for which `keep` returns true (e.g. a `--host` publisher predicate).
    pub fn rollup_in_filtered(
        &self,
        period: Period,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        keep: impl FnMut(&Entry) -> bool,
    ) -> anyhow::Result<BTreeMap<String, Rollup>> {
        let mut out: BTreeMap<String, Rollup> = BTreeMap::new();
        for e in self.entries_in_filtered(since, until, keep)? {
            let r = out.entry(period.bucket(&e.ts_utc)).or_default();
            accumulate_rollup(r, &e)?;
        }
        Ok(out)
    }

    /// Spend grouped by model within an optional date window.
    pub fn by_model(
        &self,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> anyhow::Result<BTreeMap<String, Rollup>> {
        self.by_model_filtered(since, until, |_| true)
    }

    /// Spend grouped by model within a window, keeping only entries for which
    /// `keep` returns true (e.g. a `--host` publisher predicate).
    pub fn by_model_filtered(
        &self,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        keep: impl FnMut(&Entry) -> bool,
    ) -> anyhow::Result<BTreeMap<String, Rollup>> {
        let mut out: BTreeMap<String, Rollup> = BTreeMap::new();
        for e in self.entries_in_filtered(since, until, keep)? {
            let r = out.entry(e.model.clone()).or_default();
            accumulate_rollup(r, &e)?;
        }
        Ok(out)
    }
}

/// One line of the ledger as [`LedgerLines`] yields it.
struct LedgerLine {
    /// 1-based line number in the whole file.
    line_no: usize,
    /// Byte offset of the line's first byte.
    start: u64,
    /// Bytes the line occupies, terminator included.
    raw_len: usize,
    /// The line ends with `\n` (only the last line of a file may not).
    terminated: bool,
    /// The line without its trailing `\n`/`\r` bytes.
    payload: Vec<u8>,
}

/// Incremental line reader over the ledger, starting at a row boundary.
///
/// Reading incrementally keeps one invalid UTF-8 byte from blanking a large
/// ledger, and bounds memory before an attacker-sized line is accumulated.
struct LedgerLines<'a> {
    reader: BufReader<&'a mut File>,
    path: &'a Path,
    offset: u64,
    line_no: usize,
    done: bool,
}

impl<'a> LedgerLines<'a> {
    /// Read from byte `start`, numbering the first line `lines_before + 1`.
    fn new(
        file: &'a mut File,
        path: &'a Path,
        start: u64,
        lines_before: usize,
    ) -> anyhow::Result<Self> {
        file.seek(SeekFrom::Start(start))?;
        Ok(Self {
            reader: BufReader::with_capacity(64 * 1024, file),
            path,
            offset: start,
            line_no: lines_before,
            done: false,
        })
    }

    fn read_line(&mut self) -> anyhow::Result<Option<LedgerLine>> {
        self.line_no += 1;
        let line_no = self.line_no;
        let mut buf = Vec::new();
        loop {
            let available = self.reader.fill_buf().with_context(|| {
                format!(
                    "reading ledger at line {line_no} of {}",
                    self.path.display()
                )
            })?;
            if available.is_empty() {
                if buf.is_empty() {
                    return Ok(None);
                }
                break;
            }
            let chunk_len = available
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(available.len(), |position| position + 1);
            if buf.len().saturating_add(chunk_len) > MAX_LEDGER_LINE_BYTES {
                anyhow::bail!(
                    "ledger line {line_no} exceeds {} bytes; truncating early",
                    MAX_LEDGER_LINE_BYTES
                );
            }
            let has_newline = available[chunk_len - 1] == b'\n';
            buf.extend_from_slice(&available[..chunk_len]);
            self.reader.consume(chunk_len);
            if has_newline {
                break;
            }
        }
        let start = self.offset;
        let raw_len = buf.len();
        self.offset = self.offset.saturating_add(raw_len as u64);
        let terminated = buf.last() == Some(&b'\n');
        while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
            buf.pop();
        }
        Ok(Some(LedgerLine {
            line_no,
            start,
            raw_len,
            terminated,
            payload: buf,
        }))
    }
}

impl Iterator for LedgerLines<'_> {
    type Item = anyhow::Result<LedgerLine>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let item = self.read_line().transpose();
        if !matches!(item, Some(Ok(_))) {
            self.done = true;
        }
        item
    }
}

fn read_entries(
    file: &mut File,
    path: &Path,
    mut on_malformed: impl FnMut(usize, &str),
) -> anyhow::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for line in LedgerLines::new(file, path, 0, 0)? {
        let line = line?;
        if line.payload.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Entry>(&line.payload) {
            Ok(entry) if entry_numbers_valid(&entry) => out.push(entry),
            _ => {
                let raw = String::from_utf8_lossy(&line.payload).into_owned();
                on_malformed(line.line_no, &raw);
            }
        }
    }
    Ok(out)
}

fn reservation_marker(entry: &Entry) -> Option<&str> {
    let violation = entry.violation.as_deref()?;
    let marker = violation.strip_prefix("billable call reserved but not reconciled (")?;
    let marker = marker.strip_suffix(')')?;
    let hex = marker.strip_prefix("zoder-reservation-")?;
    (hex.len() == 32 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(marker)
}

fn reservation_owner_lock_path(ledger_path: &Path, marker: &str) -> PathBuf {
    let mut path = ledger_path.as_os_str().to_os_string();
    path.push(format!(".{marker}.owner"));
    PathBuf::from(path)
}

/// The BILLED spend of the UTC calendar month containing `now`, for the
/// pre-call cap.
///
/// Only `billing = "metered"` rows count. A `subscription` or `free` row is
/// $0 marginal by construction (the plan is paid for monthly; the free route
/// costs nothing to serve), so including it lets a provider's usage
/// telemetry drive the cap toward a limit nothing was billed against — the
/// phantom `budget_exceeded` that stopped every job on one host at $170 while
/// real billed usage was under $1 (Z3, Incident B).
///
/// A row written before the `billing` field existed carries `None` and counts
/// as metered: it is metered history, and dropping it would forget real spend.
///
/// This full scan is the oracle the checkpointed totals must reproduce; the
/// production read is [`Fold::month_to_date`], whose [`MonthTotal::add`]
/// applies the same metered filter.
#[cfg(test)]
fn month_to_date_from_entries(entries: &[Entry], now: DateTime<Utc>) -> anyhow::Result<f64> {
    let bucket = Period::Month.bucket(&now);
    let mut rollup = Rollup::default();
    for entry in entries
        .iter()
        .filter(|entry| Period::Month.bucket(&entry.ts_utc) == bucket)
        .filter(|entry| entry.is_metered())
    {
        accumulate_rollup(&mut rollup, entry)?;
    }
    if rollup.unknown_cost_calls > 0 {
        anyhow::bail!(
            "month-to-date spend contains {} call(s) with unknown cost",
            rollup.unknown_cost_calls
        );
    }
    Ok(rollup.cost_usd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .unwrap()
            .and_utc()
    }

    fn entry(t: &str, model: &str, cost: f64, tin: u64, tout: u64, calls: u64) -> Entry {
        Entry {
            ts_utc: ts(t),
            provider: "test".into(),
            model: model.into(),
            host: model
                .split_once('/')
                .map(|(h, _)| h.to_string())
                .unwrap_or_default(),
            tokens_in: tin,
            tokens_out: tout,
            cost_usd: cost,
            cost_unknown: false,
            calls,
            violation: None,
            tags: FinOpsTags::default(),
            billing: None,
            estimated_cost_usd: None,
        }
    }

    /// Finding #22: a rollup row with `calls: 10` must contribute 10 to the
    /// bucket's `calls` total — the old `r.calls += 1` collapsed every row
    /// to a count of one.
    #[test]
    fn rollup_honors_e_calls_not_assumes_one() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        // Distinct days so the test isn't sensitive to Period::Day
        // bucketing collapsing all three rows into one key.
        led.record(&entry("2026-07-01 10:00:00", "m1", 1.0, 100, 200, 10))
            .unwrap();
        led.record(&entry("2026-07-02 10:00:00", "m1", 1.0, 100, 200, 1))
            .unwrap();
        led.record(&entry("2026-07-03 10:00:00", "m2", 0.5, 50, 50, 3))
            .unwrap();
        let r = led.rollup(Period::Day).unwrap();
        // Three distinct day-buckets.
        assert_eq!(r.len(), 3, "three distinct days => three buckets");
        let total_calls: u64 = r.values().map(|r| r.calls).sum();
        assert_eq!(
            total_calls, 14,
            "rollup must sum e.calls across buckets (10+1+3 = 14), not assume 1"
        );
        let total_cost: f64 = r.values().map(|r| r.cost_usd).sum();
        assert!((total_cost - 2.5).abs() < 1e-9);
    }

    #[test]
    fn rollup_segregates_unknown_cost_usage() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        led.record(&entry("2026-07-01 10:00:00", "m", 2.0, 100, 50, 1))
            .unwrap();
        let mut unknown = entry("2026-07-01 11:00:00", "m", 0.0, 400, 100, 2);
        unknown.cost_unknown = true;
        led.record(&unknown).unwrap();
        let bucket = led.rollup(Period::Day).unwrap();
        let row = bucket.values().next().unwrap();
        assert_eq!(row.cost_usd, 2.0);
        assert_eq!(row.tokens_in, 100);
        assert_eq!(row.tokens_out, 50);
        assert_eq!(row.calls, 1);
        assert_eq!(row.unknown_cost_tokens, 500);
        assert_eq!(row.unknown_cost_calls, 2);
    }

    /// Finding #22: a JSONL entry carrying `tags` deserializes and
    /// reserializes with those tags preserved (the legacy `parse_tags`
    /// hack that round-tripped through `Entry` only ever saw empty
    /// fields because `Entry` had no `tags`).
    #[test]
    fn entry_persists_and_round_trips_finops_tags() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        let mut e = entry("2026-07-01 10:00:00", "m1", 1.0, 100, 200, 1);
        e.tags = FinOpsTags {
            cache_hit_tokens: None,
            caller: Some("ci-job-42".into()),
            task: Some("summarize".into()),
            tier: Some("explicit".into()),
            cache_hit_ratio: Some(0.42),
        };
        led.record(&e).unwrap();
        let loaded = led.entries().unwrap();
        assert_eq!(loaded.len(), 1);
        let got = &loaded[0].tags;
        assert_eq!(got.caller.as_deref(), Some("ci-job-42"));
        assert_eq!(got.task.as_deref(), Some("summarize"));
        assert_eq!(got.tier.as_deref(), Some("explicit"));
        assert!((got.cache_hit_ratio.unwrap() - 0.42).abs() < 1e-9);
    }

    /// Finding #10: a missing ledger file is the canonical "no spend"
    /// signal — `entries()` returns an empty Vec and `month_to_date_usd`
    /// returns Ok(0.0). The old behavior of returning 0.0 from
    /// `month_to_date_usd` on ANY read error was the bug.
    #[test]
    fn missing_ledger_is_empty_ok_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("missing.jsonl"));
        assert!(led.entries().unwrap().is_empty());
        assert_eq!(led.month_to_date_usd().unwrap(), 0.0);
    }

    /// Finding #10: a permission-denied read propagates as an error
    /// instead of silently reporting $0 (the old "any read error → 0"
    /// path was what let the budget gate approve a call that would have
    /// exceeded the cap). We don't rely on `chmod 000` here because root
    /// bypasses DAC and the test would silently pass on a privileged
    /// runner; instead we point the ledger at a *directory*, which is
    /// guaranteed to fail with `IsADirectory`/`PermissionDenied` for any
    /// non-privileged reader.
    #[cfg(unix)]
    #[test]
    fn permission_denied_is_propagated_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        // Point the ledger at the tempdir itself: `File::open` on a
        // directory returns an error (PermissionDenied or IsADirectory,
        // platform-dependent). The fix is that this error reaches the
        // caller as `Err`, not as a silent empty Vec.
        let led = Ledger::new(dir.path());
        let res = led.entries();
        assert!(
            res.is_err(),
            "read failure on a directory must propagate, not return empty"
        );
        let month = led.month_to_date_usd();
        assert!(
            month.is_err(),
            "month_to_date_usd must propagate read errors so the budget gate can fail closed"
        );
    }

    /// Finding #10: a single invalid UTF-8 byte inside one line does NOT
    /// blank the whole ledger — the per-line parser recovers the rest
    /// and surfaces the bad line via `on_malformed`.
    #[test]
    fn invalid_utf8_line_does_not_blank_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // Good line.
        f.write_all(b"{\"ts_utc\":\"2026-07-01T10:00:00Z\",\"provider\":\"p\",\"model\":\"m\",\"host\":\"\",\"tokens_in\":1,\"tokens_out\":1,\"cost_usd\":0.10,\"calls\":1}\n").unwrap();
        // A line with an invalid UTF-8 byte. The per-line parser will
        // hand it to on_malformed and keep going; the OLD `read_to_string`
        // path would have failed the whole file as InvalidData.
        f.write_all(b"{not-json-line\n").unwrap();
        // Another good line.
        f.write_all(b"{\"ts_utc\":\"2026-07-01T10:01:00Z\",\"provider\":\"p\",\"model\":\"m\",\"host\":\"\",\"tokens_in\":1,\"tokens_out\":1,\"cost_usd\":0.20,\"calls\":1}\n").unwrap();
        drop(f);

        let led = Ledger::new(&path);
        let mut dropped = Vec::<(usize, String)>::new();
        let entries = led
            .entries_observed(|n, raw| dropped.push((n, raw.to_string())))
            .unwrap();
        assert_eq!(
            entries.len(),
            2,
            "two valid lines must survive a bad line in the middle"
        );
        assert_eq!(
            dropped.len(),
            1,
            "exactly one line should be reported as malformed"
        );
        assert_eq!(dropped[0].0, 2, "the malformed line is line 2");
    }

    #[test]
    fn oversized_line_is_rejected_at_the_streaming_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_LEDGER_LINE_BYTES as u64 + 1).unwrap();
        drop(file);

        let err = Ledger::new(&path).entries().unwrap_err().to_string();
        assert!(err.contains("line 1 exceeds"), "{err}");
        assert!(err.contains(&MAX_LEDGER_LINE_BYTES.to_string()), "{err}");
    }

    /// Finding #10: month_to_date_usd on a healthy ledger returns the
    /// bucket's summed cost.
    #[test]
    fn month_to_date_usd_sums_current_month_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        let now = Utc::now();
        let month = now.format("%Y-%m").to_string();
        // Two entries in the current month.
        let line_a = format!(
            "{{\"ts_utc\":\"{}T10:00:00Z\",\"provider\":\"p\",\"model\":\"m\",\"host\":\"\",\"tokens_in\":1,\"tokens_out\":1,\"cost_usd\":0.40,\"calls\":1}}\n",
            now.format("%Y-%m-%d")
        );
        let line_b = format!(
            "{{\"ts_utc\":\"{}T11:00:00Z\",\"provider\":\"p\",\"model\":\"m\",\"host\":\"\",\"tokens_in\":1,\"tokens_out\":1,\"cost_usd\":0.60,\"calls\":1}}\n",
            now.format("%Y-%m-%d")
        );
        f.write_all(line_a.as_bytes()).unwrap();
        f.write_all(line_b.as_bytes()).unwrap();
        drop(f);

        let led = Ledger::new(&path);
        let got = led.month_to_date_usd().unwrap();
        assert!((got - 1.0).abs() < 1e-9, "month total {got}");
        // Spot-check the bucket exists under the expected key.
        let _ = month;
    }

    #[test]
    fn month_to_date_fails_closed_when_current_cost_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        let unknown = Entry {
            ts_utc: Utc::now(),
            provider: "p".into(),
            model: "uncatalogued".into(),
            host: String::new(),
            tokens_in: 100,
            tokens_out: 20,
            cost_usd: 0.0,
            cost_unknown: true,
            calls: 1,
            violation: None,
            tags: FinOpsTags::default(),
            billing: None,
            estimated_cost_usd: None,
        };
        led.record(&unknown).unwrap();
        assert!(led.month_to_date_usd().is_err());
    }

    #[test]
    fn month_to_date_fails_closed_on_malformed_non_empty_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let led = Ledger::new(&path);
        led.record(&entry(
            &Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            "m",
            1.0,
            10,
            10,
            1,
        ))
        .unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"ts_utc\":\"truncated\"\n").unwrap();

        let err = led.month_to_date_usd().unwrap_err().to_string();
        assert!(err.contains("malformed"), "{err}");
        assert!(err.contains("line(s) 2"), "{err}");
    }

    // ---------------------------------------------------------------------
    // D8 step 1: the month-to-date cap sums only what was actually BILLED.
    // ---------------------------------------------------------------------

    /// A `billing = "subscription"` or `billing = "free"` row is $0 marginal
    /// by construction. If a provider reports usage telemetry on such a row
    /// the numeric cost can be non-zero (an overage estimate, a plan-included
    /// call priced at list), and summing it drives the monthly cap toward a
    /// limit nothing was billed against — the phantom `budget_exceeded` that
    /// stopped every job on one host (Z3, Incident B).
    #[test]
    fn month_to_date_ignores_free_and_subscription_rows() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        let now = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let mut metered = entry(&now, "cloud/m", 0.50, 10, 10, 1);
        metered.billing = Some("metered".into());
        let mut subscription = entry(&now, "plan/m", 5.00, 10, 10, 1);
        subscription.billing = Some("subscription".into());
        let mut free = entry(&now, "local/m", 3.00, 10, 10, 1);
        free.billing = Some("free".into());
        for e in [&metered, &subscription, &free] {
            led.record(e).unwrap();
        }
        let got = led.month_to_date_usd().unwrap();
        assert!(
            (got - 0.50).abs() < 1e-9,
            "only the metered row counts toward the cap, got {got}"
        );
    }

    /// Rows written before the `billing` field existed carry `billing: None`.
    /// Every one of them is metered history (the posture field was added
    /// later, and the pre-field writer only ever recorded metered routes), so
    /// they must keep counting — otherwise the cap would forget real spend.
    #[test]
    fn old_rows_without_billing_count_as_metered() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        let now = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        // `entry()` leaves billing = None, i.e. an old row.
        led.record(&entry(&now, "cloud/m", 0.75, 10, 10, 1))
            .unwrap();
        let got = led.month_to_date_usd().unwrap();
        assert!(
            (got - 0.75).abs() < 1e-9,
            "a row without `billing` is metered history and must count, got {got}"
        );
    }

    /// Unknown-cost row semantics are unchanged by the metered filter: a
    /// metered row whose price is unknown still fails the read closed, so the
    /// gate asks instead of silently treating it as $0.
    #[test]
    fn unknown_cost_metered_row_still_fails_the_cap_read_closed() {
        let dir = tempfile::tempdir().unwrap();
        let led = Ledger::new(&dir.path().join("ledger.jsonl"));
        let mut unknown = entry(
            &Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            "cloud/uncatalogued",
            0.0,
            10,
            10,
            1,
        );
        unknown.cost_unknown = true;
        unknown.billing = Some("metered".into());
        led.record(&unknown).unwrap();
        assert!(led.month_to_date_usd().is_err());
    }

    #[test]
    fn zero_call_entry_is_rejected_by_record_and_strict_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let mut invalid = entry("2026-07-01 10:00:00", "m", 0.0, 10, 5, 0);
        invalid.cost_unknown = true;
        assert!(Ledger::new(&path).record(&invalid).is_err());

        std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        let error = Ledger::new(&path).entries_strict().unwrap_err().to_string();
        assert!(error.contains("malformed"), "{error}");
    }

    #[test]
    fn reservation_reconciles_into_one_final_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);
        let reservation = ledger.reserve_billable().unwrap();
        reservation
            .reconcile(&entry("2026-07-01 10:00:00", "m", 0.25, 10, 5, 1))
            .unwrap();
        let entries = ledger.entries_strict().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cost_usd, 0.25);
        assert!(!entries[0].cost_unknown);
    }

    #[test]
    fn unarmed_reservation_is_cancelled_without_a_ledger_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);
        drop(ledger.reserve_billable().unwrap());
        assert!(ledger.entries_strict().unwrap().is_empty());
    }

    #[test]
    fn abandoned_prepared_reservation_is_recovered_after_owner_death() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);
        let mut abandoned = ledger.reserve_billable().unwrap();
        let owner = abandoned.owner_lock.take().unwrap();
        FileExt::unlock(&owner).unwrap();
        drop(owner);
        std::mem::forget(abandoned);

        assert!(ledger.entries_strict().unwrap().is_empty());
    }

    #[test]
    fn live_prepared_reservation_is_visible_to_concurrent_budget_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let first = Ledger::new(&path).reserve_billable().unwrap();

        let error = Ledger::new(&path)
            .month_to_date_usd()
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown cost"), "{error}");
        drop(first);
    }

    #[test]
    fn persisted_reservation_does_not_block_nested_ledger_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);
        let mut reservation = ledger.reserve_billable().unwrap();
        reservation.arm().unwrap();

        let nested_path = path.clone();
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = Ledger::new(&nested_path).entries_strict();
            send.send(result).unwrap();
        });
        let nested = receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("nested ledger read deadlocked behind reservation");
        let entries = nested.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].cost_unknown);

        reservation
            .reconcile(&entry("2026-07-01 10:00:00", "m", 0.25, 10, 5, 1))
            .unwrap();
    }

    #[test]
    fn concurrent_reservation_observes_pending_row_and_fails_budget_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let mut first = Ledger::new(&path).reserve_billable().unwrap();
        first.arm().unwrap();

        let (send, receive) = std::sync::mpsc::channel();
        let nested_path = path.clone();
        std::thread::spawn(move || {
            send.send(Ledger::new(&nested_path).reserve_billable())
                .unwrap();
        });
        let second = receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("concurrent reservation deadlocked")
            .unwrap();
        let error = second.month_to_date_usd().unwrap_err().to_string();
        assert!(error.contains("unknown cost"), "{error}");
        drop(second);
        drop(first);
    }

    #[test]
    fn uncertain_attempt_is_retained_when_later_attempt_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);

        let mut timed_out_attempt = ledger.reserve_billable().unwrap();
        timed_out_attempt.arm().unwrap();
        drop(timed_out_attempt);

        let mut winning_attempt = ledger.reserve_billable().unwrap();
        winning_attempt.arm().unwrap();
        winning_attempt
            .reconcile(&entry("2026-07-01 10:00:00", "winner", 0.25, 10, 5, 1))
            .unwrap();

        let entries = ledger.entries_strict().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].cost_unknown);
        assert_eq!(entries[1].model, "winner");
        assert_eq!(entries[1].cost_usd, 0.25);
    }

    #[test]
    fn cancelling_older_reservation_preserves_later_reconciliation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);
        let older = ledger.reserve_billable().unwrap();
        let newer = ledger.reserve_billable().unwrap();
        newer
            .reconcile(&entry("2026-07-01 10:00:00", "newer", 0.5, 2, 1, 1))
            .unwrap();
        drop(older);

        let entries = ledger.entries_strict().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].model, "newer");
    }

    /// Write `n` finished rows padded to `slot` bytes (what older builds left
    /// behind: every reconciled call kept its whole preallocated slot).
    fn write_padded_rows(path: &Path, n: usize, slot: usize) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for i in 0..n {
            let json = serde_json::to_vec(&entry(
                "2026-07-01 10:00:00",
                &format!("m{i}"),
                0.01,
                1,
                1,
                1,
            ))
            .unwrap();
            let mut line = vec![b' '; slot];
            line[..json.len()].copy_from_slice(&json);
            line[slot - 1] = b'\n';
            f.write_all(&line).unwrap();
        }
    }

    #[test]
    fn new_reservation_slots_are_small() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);
        ledger
            .reserve_billable()
            .unwrap()
            .reconcile(&entry("2026-07-01 10:00:00", "m", 0.25, 10, 5, 1))
            .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            BILLABLE_RESERVATION_BYTES as u64
        );
    }

    /// Throughput regression: a ledger of legacy 64 KiB padded rows is
    /// compacted on the next locked write, with every entry preserved.
    #[test]
    fn legacy_padded_ledger_is_compacted_without_losing_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        write_padded_rows(&path, 80, LEGACY_RESERVATION_BYTES);
        let before_len = std::fs::metadata(&path).unwrap().len();
        assert!(before_len >= COMPACT_MIN_BYTES);
        let ledger = Ledger::new(&path);
        let before = ledger.entries_strict().unwrap();
        ledger
            .record(&entry("2026-07-02 10:00:00", "after", 0.5, 1, 1, 1))
            .unwrap();
        let after_len = std::fs::metadata(&path).unwrap().len();
        assert!(after_len < before_len / 20, "{after_len} vs {before_len}");
        let after = ledger.entries_strict().unwrap();
        assert_eq!(after.len(), before.len() + 1);
        for (a, b) in before.iter().zip(after.iter()) {
            assert_eq!(a.model, b.model);
            assert_eq!(a.cost_usd, b.cost_usd);
        }
        assert_eq!(after.last().unwrap().model, "after");
        // Below the threshold nothing is rewritten.
        let small_len = std::fs::metadata(&path).unwrap().len();
        ledger
            .record(&entry("2026-07-02 11:00:00", "again", 0.5, 1, 1, 1))
            .unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() > small_len);
    }

    /// An armed reservation whose row a compaction moved still reconciles in
    /// place (found again by its marker) -- in-flight calls survive.
    #[test]
    fn armed_reservation_survives_compaction_and_reconciles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        write_padded_rows(&path, 5, LEGACY_RESERVATION_BYTES);
        let ledger = Ledger::new(&path);
        // Reserved while the ledger is still below the compaction threshold
        // (its slot sits after 5 legacy rows) ...
        let mut inflight = ledger.reserve_billable().unwrap();
        let reserved_at = inflight.offset;
        assert_eq!(reserved_at, 5 * LEGACY_RESERVATION_BYTES as u64);
        inflight.arm().unwrap();
        // ... older writers keep appending padded rows after it ...
        write_padded_rows(&path, 65, LEGACY_RESERVATION_BYTES);
        // ... then another process's write compacts the ledger.
        ledger
            .record(&entry("2026-07-02 10:00:00", "other", 0.5, 1, 1, 1))
            .unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < reserved_at);
        inflight
            .reconcile(&entry("2026-07-02 10:01:00", "inflight", 0.25, 1, 1, 1))
            .unwrap();
        let entries = ledger.entries_strict().unwrap();
        assert_eq!(entries.len(), 72);
        assert!(entries.iter().all(|e| !e.cost_unknown));
        assert!(entries.iter().any(|e| e.model == "inflight"));
    }

    /// Mixed-version safety: a recent reservation row written by an older
    /// build (legacy 64 KiB slot, cannot relocate by marker) postpones
    /// compaction; an old one does not.
    #[test]
    fn recent_legacy_reservation_postpones_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        write_padded_rows(&path, 70, LEGACY_RESERVATION_BYTES);
        let legacy_armed = |ts: DateTime<Utc>| {
            let row = Entry {
                ts_utc: ts,
                provider: RESERVATION_PROVIDER.into(),
                model: ARMED_RESERVATION_MODEL.into(),
                host: String::new(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                cost_unknown: true,
                calls: 1,
                violation: Some(format!(
                    "billable call reserved but not reconciled (zoder-reservation-{:032x})",
                    9_u128
                )),
                tags: FinOpsTags::default(),
                billing: None,
                estimated_cost_usd: None,
            };
            let json = serde_json::to_vec(&row).unwrap();
            let mut line = vec![b' '; LEGACY_RESERVATION_BYTES];
            line[..json.len()].copy_from_slice(&json);
            line[LEGACY_RESERVATION_BYTES - 1] = b'\n';
            line
        };
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(&legacy_armed(Utc::now())).unwrap();
        drop(f);
        let ledger = Ledger::new(&path);
        let before = std::fs::metadata(&path).unwrap().len();
        ledger
            .record(&entry("2026-07-02 10:00:00", "x", 0.5, 1, 1, 1))
            .unwrap();
        assert!(
            std::fs::metadata(&path).unwrap().len() > before,
            "must not compact"
        );

        let dir2 = tempfile::tempdir().unwrap();
        let path2 = dir2.path().join("ledger.jsonl");
        write_padded_rows(&path2, 70, LEGACY_RESERVATION_BYTES);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path2)
            .unwrap();
        f.write_all(&legacy_armed(Utc::now() - chrono::Duration::hours(3)))
            .unwrap();
        drop(f);
        let ledger2 = Ledger::new(&path2);
        let before2 = std::fs::metadata(&path2).unwrap().len();
        ledger2
            .record(&entry("2026-07-02 10:00:00", "x", 0.5, 1, 1, 1))
            .unwrap();
        assert!(std::fs::metadata(&path2).unwrap().len() < before2 / 10);
        // The stale armed row survives compaction as unknown spend.
        assert!(ledger2
            .entries_strict()
            .unwrap()
            .iter()
            .any(|e| e.cost_unknown));
    }

    /// Measured on a fleet host: 4,337 of 7,746 rows were stale armed
    /// reservations (failed/abandoned calls, days old) kept at 64 KiB each,
    /// so compaction never reached its threshold. Stale armed rows are
    /// trimmed (still unknown-cost spend); fresh ones keep their slot.
    #[test]
    fn stale_armed_reservations_are_trimmed_but_kept_as_unknown_spend() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let armed_row = |ts: DateTime<Utc>, n: u128, slot: usize| {
            let row = Entry {
                ts_utc: ts,
                provider: RESERVATION_PROVIDER.into(),
                model: ARMED_RESERVATION_MODEL.into(),
                host: String::new(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                cost_unknown: true,
                calls: 1,
                violation: Some(format!(
                    "billable call reserved but not reconciled (zoder-reservation-{n:032x})"
                )),
                tags: FinOpsTags::default(),
                billing: None,
                estimated_cost_usd: None,
            };
            let json = serde_json::to_vec(&row).unwrap();
            let mut line = vec![b' '; slot];
            line[..json.len()].copy_from_slice(&json);
            line[slot - 1] = b'\n';
            line
        };
        let old = Utc::now() - chrono::Duration::hours(7);
        let mut f = std::fs::File::create(&path).unwrap();
        for n in 0..70 {
            f.write_all(&armed_row(old, n, LEGACY_RESERVATION_BYTES))
                .unwrap();
        }
        // A fresh armed row (this build, mid-dispatch) must keep its slot.
        f.write_all(&armed_row(Utc::now(), 999, BILLABLE_RESERVATION_BYTES))
            .unwrap();
        drop(f);
        let ledger = Ledger::new(&path);
        let before = ledger.entries_strict().unwrap();
        assert_eq!(before.iter().filter(|e| e.cost_unknown).count(), 71);
        ledger
            .record(&entry("2026-07-02 10:00:00", "x", 0.5, 1, 1, 1))
            .unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(
            len < 70 * 400 + 2 * BILLABLE_RESERVATION_BYTES as u64,
            "{len}"
        );
        let after = ledger.entries_strict().unwrap();
        assert_eq!(
            after.iter().filter(|e| e.cost_unknown).count(),
            71,
            "spend kept"
        );
        let fresh = format!("zoder-reservation-{:032x}", 999_u128);
        let mut file = std::fs::File::open(&path).unwrap();
        assert!(
            find_reservation_slot(&mut file, BILLABLE_RESERVATION_BYTES, &fresh)
                .unwrap()
                .is_some(),
            "fresh reservation keeps a full slot"
        );
    }

    /// A prepared (unarmed) reservation moved by compaction is still cancelled
    /// cleanly when dropped.
    #[test]
    fn prepared_reservation_moved_by_compaction_is_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        write_padded_rows(&path, 5, LEGACY_RESERVATION_BYTES);
        let ledger = Ledger::new(&path);
        let pending = ledger.reserve_billable().unwrap();
        assert_eq!(pending.offset, 5 * LEGACY_RESERVATION_BYTES as u64);
        write_padded_rows(&path, 65, LEGACY_RESERVATION_BYTES);
        ledger
            .record(&entry("2026-07-02 10:00:00", "other", 0.5, 1, 1, 1))
            .unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < 5 * LEGACY_RESERVATION_BYTES as u64);
        drop(pending);
        let entries = ledger.entries_strict().unwrap();
        assert_eq!(entries.len(), 71);
        assert!(entries.iter().all(|e| !e.cost_unknown));
    }

    /// Abandoned prepared reservations written by older builds (64 KiB slots)
    /// are still recovered.
    #[test]
    fn abandoned_legacy_size_prepared_slot_is_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let marker = format!("zoder-reservation-{:032x}", 7_u128);
        let pending = Entry {
            ts_utc: Utc::now(),
            provider: RESERVATION_PROVIDER.into(),
            model: PREPARED_RESERVATION_MODEL.into(),
            host: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            cost_unknown: true,
            calls: 1,
            violation: Some(format!(
                "billable call reserved but not reconciled ({marker})"
            )),
            tags: FinOpsTags::default(),
            billing: None,
            estimated_cost_usd: None,
        };
        let json = serde_json::to_vec(&pending).unwrap();
        let mut line = vec![b' '; LEGACY_RESERVATION_BYTES];
        line[..json.len()].copy_from_slice(&json);
        line[LEGACY_RESERVATION_BYTES - 1] = b'\n';
        std::fs::write(&path, &line).unwrap();
        assert!(Ledger::new(&path).entries_strict().unwrap().is_empty());
    }

    #[test]
    fn oversized_violation_is_truncated_to_fit_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);
        let mut final_entry = entry("2026-07-01 10:00:00", "m", 0.25, 10, 5, 1);
        final_entry.violation = Some("é".repeat(BILLABLE_RESERVATION_BYTES));
        ledger
            .reserve_billable()
            .unwrap()
            .reconcile(&final_entry)
            .unwrap();
        let entries = ledger.entries_strict().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0]
            .violation
            .as_deref()
            .unwrap()
            .ends_with("[truncated]"));
        assert_eq!(entries[0].cost_usd, 0.25);
    }

    #[test]
    fn dispatch_refuses_replaced_canonical_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let rotated = dir.path().join("ledger.rotated.jsonl");
        let ledger = Ledger::new(&path);
        let mut reservation = ledger.reserve_billable().unwrap();
        std::fs::rename(&path, &rotated).unwrap();
        let replacement = entry("2026-07-01 10:00:00", "replacement", 1.0, 1, 1, 1);
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&replacement).unwrap()),
        )
        .unwrap();

        let error = reservation.arm().unwrap_err().to_string();
        assert!(error.contains("reserved ledger slot"), "{error}");
        let entries = ledger.entries_strict().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].model, "replacement");
    }

    /// An armed reservation whose owner is gone: dispatched, never
    /// reconciled, and older than the stale threshold.
    fn stale_armed_entry() -> Entry {
        stale_armed_entry_at(Utc::now() - chrono::Duration::hours(7))
    }

    /// [`stale_armed_entry`] armed at `ts_utc`, which must be at least
    /// [`STALE_RESERVATION_TRIM_S`] before the wall clock.
    fn stale_armed_entry_at(ts_utc: DateTime<Utc>) -> Entry {
        Entry {
            ts_utc,
            provider: RESERVATION_PROVIDER.to_string(),
            model: ARMED_RESERVATION_MODEL.to_string(),
            host: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            cost_unknown: true,
            calls: 1,
            violation: Some(
                "billable call reserved but not reconciled \
                 (zoder-reservation-00000000000000000000000000000000)"
                    .to_string(),
            ),
            tags: FinOpsTags::default(),
            billing: None,
            estimated_cost_usd: None,
        }
    }

    /// A reservation that was armed and then dropped without a final entry --
    /// the dispatch failed, timed out, was cancelled, or the process unwound
    /// -- must leave an unknown-cost row that no longer claims to be an
    /// unreconciled reservation.
    ///
    /// Before the drop guard the slot stayed armed for ever, so every failed
    /// dispatch left a "billable call reserved but not reconciled" row behind
    /// and the ledger reported the reservation as still in flight.
    #[test]
    fn an_armed_reservation_dropped_without_a_final_entry_is_reconciled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);
        {
            let mut reservation = ledger.reserve_billable().unwrap();
            reservation.arm_for("provider-a", "model-a").unwrap();
            // The dispatch failed: the reservation is dropped with no final
            // entry.
        }
        let rows = Ledger::new(&path).entries_strict().unwrap();
        assert_eq!(rows.len(), 1, "the reservation leaves exactly one row");
        assert!(
            rows[0].cost_unknown,
            "a failed dispatch has an unknown cost"
        );
        assert!(
            rows[0].violation.is_none(),
            "a reconciled reservation must not report an unreconciled one: {:?}",
            rows[0].violation
        );
        assert_eq!(rows[0].provider, "provider-a");
        assert_eq!(rows[0].model, "model-a");
    }

    /// A panic unwinding through an armed reservation reconciles it too: the
    /// drop guard runs while the stack unwinds.
    #[test]
    fn a_panicking_dispatch_reconciles_the_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome: std::thread::Result<()> = std::panic::catch_unwind(|| {
            let mut reservation = ledger.reserve_billable().unwrap();
            reservation.arm_for("provider-a", "model-a").unwrap();
            panic!("the dispatch exploded");
        });
        std::panic::set_hook(previous_hook);
        assert!(outcome.is_err(), "the panic must propagate");
        let rows = Ledger::new(&path).entries_strict().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].cost_unknown);
        assert!(
            rows[0].violation.is_none(),
            "a reconciled reservation must not report an unreconciled one: {:?}",
            rows[0].violation
        );
    }

    /// A reservation dropped before it was armed was never dispatched: its
    /// slot is blanked instead of becoming an unknown-cost row.
    #[test]
    fn a_reservation_dropped_before_dispatch_is_blanked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);
        {
            let _reservation = ledger.reserve_billable().unwrap();
        }
        let rows = Ledger::new(&path).entries_strict().unwrap();
        assert!(
            rows.is_empty(),
            "an undispatched reservation leaves no row: {rows:?}"
        );
    }

    /// A reconciled reservation keeps its final entry: the drop guard must not
    /// overwrite a successful reconciliation with an unknown-cost row.
    #[test]
    fn a_reconciled_reservation_keeps_its_final_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);
        let mut reservation = ledger.reserve_billable().unwrap();
        reservation.arm_for("provider-a", "model-a").unwrap();
        let final_entry = entry("2026-10-25 10:00:00", "model-a", 0.25, 10, 5, 1);
        reservation.reconcile(&final_entry).unwrap();
        let rows = Ledger::new(&path).entries_strict().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].cost_unknown);
        assert!((rows[0].cost_usd - 0.25).abs() < 1e-9);
        assert!(rows[0].violation.is_none());
    }

    /// A stale armed reservation is reconciled when the ledger is opened: its
    /// owner is gone, so no other code path can ever reconcile it.
    #[test]
    fn a_stale_armed_reservation_is_reconciled_when_the_ledger_is_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let stale = stale_armed_entry();
        let mut file = std::fs::File::create(&path).unwrap();
        let json = serde_json::to_vec(&stale).unwrap();
        let mut slot = vec![b' '; BILLABLE_RESERVATION_BYTES];
        slot[..json.len()].copy_from_slice(&json);
        slot[BILLABLE_RESERVATION_BYTES - 1] = b'\n';
        file.write_all(&slot).unwrap();
        drop(file);

        // Opening the ledger for a write runs the recovery scan.
        Ledger::new(&path)
            .record(&entry("2026-10-25 10:00:00", "model-a", 0.0, 0, 0, 1))
            .unwrap();

        let rows = Ledger::new(&path).entries_strict().unwrap();
        let reconciled = rows
            .iter()
            .find(|row| row.ts_utc == stale.ts_utc)
            .expect("the stale row is still in the ledger");
        assert!(
            reconciled.violation.is_none(),
            "a stale reservation must not report an unreconciled one: {:?}",
            reconciled.violation
        );
        assert!(reconciled.cost_unknown);
        assert_eq!(reconciled.model, UNKNOWN_COST_RESERVATION_MODEL);
    }

    /// The same reconciliation applies to a stale armed reservation that the
    /// compaction already rewrote as a normal-length row.
    #[test]
    fn a_compacted_stale_armed_reservation_is_reconciled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let stale = stale_armed_entry();
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&serde_json::to_vec(&stale).unwrap())
            .unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);

        Ledger::new(&path)
            .record(&entry("2026-10-25 10:00:00", "model-a", 0.0, 0, 0, 1))
            .unwrap();

        let rows = Ledger::new(&path).entries_strict().unwrap();
        let reconciled = rows
            .iter()
            .find(|row| row.ts_utc == stale.ts_utc)
            .expect("the stale row is still in the ledger");
        assert!(
            reconciled.violation.is_none(),
            "a stale reservation must not report an unreconciled one: {:?}",
            reconciled.violation
        );
        assert!(reconciled.cost_unknown);
        assert_eq!(reconciled.model, UNKNOWN_COST_RESERVATION_MODEL);
    }

    // ---- sidecar checkpoint -------------------------------------------------

    fn now_ts() -> String {
        Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
    }

    /// Append rows the way a bulk writer would, bypassing `record` so a large
    /// ledger can be built quickly.
    fn append_lines(path: &Path, lines: &[Vec<u8>]) {
        let mut file = std::io::BufWriter::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap(),
        );
        for line in lines {
            file.write_all(line).unwrap();
        }
        file.flush().unwrap();
    }

    fn row_line(entry: &Entry) -> Vec<u8> {
        let mut line = serde_json::to_vec(entry).unwrap();
        line.push(b'\n');
        line
    }

    /// `entry` written into a reservation-sized slot, as a reconciled slot is.
    fn slot_line(entry: &Entry, slot_bytes: usize) -> Vec<u8> {
        let json = serde_json::to_vec(entry).unwrap();
        let mut line = vec![b' '; slot_bytes];
        line[..json.len()].copy_from_slice(&json);
        line[slot_bytes - 1] = b'\n';
        line
    }

    /// Per-month totals of a full strict scan: the oracle. Like the cap, it
    /// sums only metered rows; a month holding only `subscription`/`free`
    /// rows still appears, with nothing in it.
    fn full_scan_months(entries: &[Entry]) -> BTreeMap<String, Rollup> {
        let mut out: BTreeMap<String, Rollup> = BTreeMap::new();
        for entry in entries {
            let month = out.entry(Period::Month.bucket(&entry.ts_utc)).or_default();
            if entry.is_metered() {
                accumulate_rollup(month, entry).unwrap();
            }
        }
        out
    }

    fn assert_months_match(fold: &Fold, oracle: &BTreeMap<String, Rollup>, context: &str) {
        let got: Vec<_> = fold.months.keys().collect();
        let want: Vec<_> = oracle.keys().collect();
        assert_eq!(got, want, "{context}: month keys");
        for (month, total) in &fold.months {
            let full = &oracle[month];
            assert!(!total.overflowed, "{context}: {month} overflowed");
            assert_eq!(
                total.cost_usd.to_bits(),
                full.cost_usd.to_bits(),
                "{context}: {month} cost {} vs full scan {}",
                total.cost_usd,
                full.cost_usd
            );
            assert_eq!(
                total.unknown_cost_calls, full.unknown_cost_calls,
                "{context}: {month} unknown-cost calls"
            );
        }
    }

    fn fold_from_checkpoint(ledger: &Ledger) -> (Option<LedgerCheckpoint>, Fold) {
        let mut file = File::open(&ledger.path).unwrap();
        let base = ledger.load_checkpoint(&mut file);
        let fold = fold_ledger(&mut file, &ledger.path, base.clone(), |entry, raw_len| {
            Ok(ledger.prepared_owner_is_gone(entry, raw_len))
        })
        .unwrap();
        (base, fold)
    }

    /// Property test: on a synthetic 100k-row ledger grown in batches -- with
    /// unknown-cost rows, blank rows, padded slots, legacy slots, live,
    /// abandoned, armed and stale-armed reservations, and reservations that
    /// are reconciled or cancelled in place after later rows were appended --
    /// the checkpointed totals equal the full scan bit for bit after every
    /// batch, and so do month-to-date and the recovery scan's compaction
    /// statistics. `ZODER_LEDGER_SEED` replays another seed.
    #[test]
    fn summary_matches_full_scan() {
        check_summary_matches_full_scan(Utc::now());
    }

    // The same property with the clock pinned, so the outcome does not depend
    // on when the suite runs. Each instant must not be later than the wall
    // clock: recovery judges reservation staleness by the wall clock.

    /// Mid-month, nowhere near a boundary.
    #[test]
    fn summary_matches_full_scan_mid_month() {
        check_summary_matches_full_scan(ts("2026-10-15 12:00:00"));
    }

    /// The first day of a month, past the first seven hours (the instant the
    /// failure was reported on master).
    #[test]
    fn summary_matches_full_scan_first_day_of_month() {
        check_summary_matches_full_scan(ts("2026-11-01 17:50:00"));
    }

    /// Within seven hours after a month boundary (when it was verified green).
    #[test]
    fn summary_matches_full_scan_just_after_month_boundary() {
        check_summary_matches_full_scan(ts("2026-11-01 03:30:00"));
    }

    /// The last second of a month.
    #[test]
    fn summary_matches_full_scan_end_of_month() {
        check_summary_matches_full_scan(ts("2026-10-31 23:59:59"));
    }

    /// [`summary_matches_full_scan`] with the clock pinned at `now`: fixture
    /// timestamps and the month the month-to-date figures read both derive
    /// from `now`.
    fn check_summary_matches_full_scan(now: DateTime<Utc>) {
        use rand::{Rng, SeedableRng};
        let seed = std::env::var("ZODER_LEDGER_SEED")
            .ok()
            .and_then(|seed| seed.parse().ok())
            .unwrap_or(0x5eed_0000_1ed9_e700_u64);
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);
        let now_text = now.format("%Y-%m-%d %H:%M:%S").to_string();
        let month_start = chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let elapsed = (now - month_start).num_seconds().max(1);
        const BATCHES: usize = 8;
        const ROWS_PER_BATCH: usize = 12_500;
        let mut live: Vec<BillableReservation> = Vec::new();
        let mut armed: Vec<BillableReservation> = Vec::new();
        let mut total_rows = 0usize;
        for batch in 0..BATCHES {
            let last = batch == BATCHES - 1;
            // Rewrite rows that are already behind later rows: reconcile the
            // armed reservations with a known cost, cancel the live ones.
            if batch % 2 == 1 || last {
                for reservation in armed.drain(..) {
                    let mut done = entry(&now_text, "vendor/m", 0.0, 5, 5, 1);
                    done.cost_usd = rng.gen_range(0.0..2.0);
                    reservation.reconcile(&done).unwrap();
                }
                live.clear();
            }
            let mut lines = Vec::with_capacity(ROWS_PER_BATCH);
            for _ in 0..ROWS_PER_BATCH {
                let pick: u32 = rng.gen_range(0..1000);
                if pick < 5 {
                    lines.push(b"\n".to_vec());
                    continue;
                }
                let ts = match rng.gen_range(0..3) {
                    0 => now - chrono::Duration::seconds(rng.gen_range(0..elapsed)),
                    1 => month_start - chrono::Duration::days(rng.gen_range(1..28)),
                    _ => month_start - chrono::Duration::days(rng.gen_range(32..58)),
                };
                let mut row = entry("2026-01-01 00:00:00", "vendor/model", 0.0, 0, 0, 1);
                row.ts_utc = ts;
                row.provider = format!("p{}", rng.gen_range(0..7));
                row.model = format!("vendor/model-{}", rng.gen_range(0..53));
                row.tokens_in = rng.gen_range(0..5000);
                row.tokens_out = rng.gen_range(0..2000);
                row.calls = rng.gen_range(1..4);
                // Magnitudes spread over six decades, so a different
                // summation order would change the bits.
                row.cost_usd = rng.gen_range(0.0..5.0) / 10f64.powi(rng.gen_range(0..6));
                // Unknown cost only in past months: the current month must be
                // able to come out as a figure.
                if ts < month_start && pick < 20 {
                    row.cost_unknown = true;
                    row.cost_usd = 0.0;
                }
                // Every billing posture, so the checkpoint must leave out the
                // same $0-marginal rows the full scan leaves out.
                row.billing = match rng.gen_range(0..4) {
                    0 => None,
                    1 => Some("metered".into()),
                    2 => Some("subscription".into()),
                    _ => Some("free".into()),
                };
                if pick < 23 {
                    lines.push(slot_line(&row, BILLABLE_RESERVATION_BYTES));
                } else if pick == 23 {
                    // A finished row in a legacy-size slot, some of them young
                    // enough to postpone compaction.
                    row.provider = RESERVATION_PROVIDER.to_string();
                    row.model = UNKNOWN_COST_RESERVATION_MODEL.to_string();
                    row.ts_utc = now - chrono::Duration::seconds(rng.gen_range(0..7200));
                    lines.push(slot_line(&row, LEGACY_RESERVATION_BYTES));
                } else {
                    lines.push(row_line(&row));
                }
            }
            total_rows += lines.len();
            append_lines(&path, &lines);
            if !last {
                // A live prepared row, an armed one, an abandoned one (owner
                // gone, never cancelled) and a stale armed row from a crash.
                live.push(ledger.reserve_billable().unwrap());
                let mut reservation = ledger.reserve_billable().unwrap();
                reservation.arm_for("p1", "vendor/model-1").unwrap();
                armed.push(reservation);
                let mut abandoned = ledger.reserve_billable().unwrap();
                drop(abandoned.owner_lock.take());
                std::mem::forget(abandoned);
                append_lines(
                    &path,
                    &[slot_line(
                        // Recovery reconciles it into an unknown-cost row,
                        // which must not fall in the current month (see the
                        // unknown-cost rows above). Dated before the month,
                        // it is also stale by the wall clock (>= `now`).
                        &stale_armed_entry_at(month_start - chrono::Duration::hours(7)),
                        BILLABLE_RESERVATION_BYTES,
                    )],
                );
                total_rows += 4;
            }

            let context = format!("seed {seed:#x}, batch {batch}, ~{total_rows} rows");
            let fast = ledger.month_to_date_usd_at(now);
            let (base, fold) = fold_from_checkpoint(&ledger);
            let base = base.unwrap_or_else(|| panic!("{context}: checkpoint was not published"));
            assert!(
                fold.lines_read < total_rows,
                "{context}: the read started over ({} lines)",
                fold.lines_read
            );
            // The oracle: the strict full scan, after recovery.
            let oracle = ledger.entries_strict().unwrap();
            assert_months_match(&fold, &full_scan_months(&oracle), &context);
            match (&fast, month_to_date_from_entries(&oracle, now)) {
                (Ok(fast), Ok(full)) => assert_eq!(fast.to_bits(), full.to_bits(), "{context}"),
                (Err(fast), Err(full)) => {
                    assert_eq!(fast.to_string(), full.to_string(), "{context}")
                }
                (fast, full) => panic!("{context}: checkpoint {fast:?} vs full scan {full:?}"),
            }
            // Recovery from the checkpoint sees the same padding and legacy
            // in-flight state as a full recovery scan.
            let mut file = File::open(&path).unwrap();
            let current = ledger
                .load_checkpoint(&mut file)
                .unwrap_or_else(|| panic!("{context}: recovery invalidated the checkpoint"));
            assert!(current.offset >= base.offset, "{context}");
            let full = ledger.recover_abandoned_prepared(&mut file, None).unwrap();
            let incremental = ledger
                .recover_abandoned_prepared(&mut file, Some(&current))
                .unwrap();
            assert_eq!(full.waste, incremental.waste, "{context}: waste");
            assert_eq!(
                full.legacy_inflight, incremental.legacy_inflight,
                "{context}: legacy in-flight"
            );
            if last {
                assert!(fast.is_ok(), "{context}: everything settled, yet {fast:?}");
            }
        }
        // Everything is settled: the checkpoint covers the whole file and a
        // read parses nothing.
        ledger.month_to_date_usd_at(now).unwrap();
        let (base, fold) = fold_from_checkpoint(&ledger);
        assert_eq!(
            base.unwrap().offset,
            std::fs::metadata(&path).unwrap().len()
        );
        assert_eq!(fold.lines_read, 0);
    }

    #[test]
    fn checkpoint_parses_only_new_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let rows: Vec<_> = (0..50)
            .map(|_| row_line(&entry(&now_ts(), "m", 0.5, 1, 1, 1)))
            .collect();
        append_lines(&path, &rows);
        let ledger = Ledger::new(&path);
        assert_eq!(ledger.month_to_date_usd().unwrap(), 25.0);
        append_lines(&path, &[row_line(&entry(&now_ts(), "m", 0.25, 1, 1, 1))]);

        let (base, fold) = fold_from_checkpoint(&ledger);
        assert_eq!(base.unwrap().lines, 50);
        assert_eq!(fold.lines_read, 1, "only the appended row is parsed");
        assert_eq!(fold.month_to_date().unwrap(), 25.25);
        assert_eq!(ledger.month_to_date_usd().unwrap(), 25.25);
    }

    /// A reservation slot is rewritten in place after later rows were
    /// appended. The checkpoint must not have folded it while it was armed:
    /// after reconciliation the month is a known figure again.
    #[test]
    fn reservation_rewritten_behind_later_rows_is_recounted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);
        ledger.record(&entry(&now_ts(), "m", 1.0, 1, 1, 1)).unwrap();
        let mut reservation = ledger.reserve_billable().unwrap();
        reservation.arm_for("p", "m").unwrap();
        for _ in 0..3 {
            ledger.record(&entry(&now_ts(), "m", 0.5, 1, 1, 1)).unwrap();
        }
        let err = ledger.month_to_date_usd().unwrap_err().to_string();
        assert!(err.contains("1 call(s) with unknown cost"), "{err}");

        reservation
            .reconcile(&entry(&now_ts(), "m", 0.25, 1, 1, 1))
            .unwrap();
        assert_eq!(ledger.month_to_date_usd().unwrap(), 2.75);
        assert_eq!(
            ledger
                .reserve_billable()
                .unwrap()
                .month_to_date_usd()
                .unwrap(),
            2.75
        );
    }

    /// A ledger truncated in place and regrown past the checkpoint keeps its
    /// inode and its size; only the fingerprints show it is another file.
    #[test]
    fn truncated_and_regrown_ledger_falls_back_to_full_scan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ts = now_ts();
        let rows = |cost: f64| -> Vec<Vec<u8>> {
            (0..20)
                .map(|_| row_line(&entry(&ts, "m", cost, 1, 1, 1)))
                .collect()
        };
        append_lines(&path, &rows(0.25));
        let ledger = Ledger::new(&path);
        assert_eq!(ledger.month_to_date_usd().unwrap(), 5.0);
        let size = std::fs::metadata(&path).unwrap().len();

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        for line in rows(0.75) {
            file.write_all(&line).unwrap();
        }
        drop(file);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), size);

        assert_eq!(ledger.month_to_date_usd().unwrap(), 15.0);
    }

    #[test]
    fn rotated_ledger_falls_back_to_full_scan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);
        for _ in 0..10 {
            ledger.record(&entry(&now_ts(), "m", 1.0, 1, 1, 1)).unwrap();
        }
        assert_eq!(ledger.month_to_date_usd().unwrap(), 10.0);

        std::fs::rename(&path, dir.path().join("ledger.jsonl.1")).unwrap();
        let rows: Vec<_> = (0..30)
            .map(|_| row_line(&entry(&now_ts(), "m", 0.5, 1, 1, 1)))
            .collect();
        append_lines(&path, &rows);
        assert_eq!(ledger.month_to_date_usd().unwrap(), 15.0);

        // Rotated to an empty file: nothing this month.
        std::fs::rename(&path, dir.path().join("ledger.jsonl.2")).unwrap();
        std::fs::write(&path, b"").unwrap();
        assert_eq!(ledger.month_to_date_usd().unwrap(), 0.0);
    }

    /// A damaged checkpoint -- unparseable, edited, or from another layout
    /// version -- is discarded and the full scan answers.
    #[test]
    fn corrupt_checkpoint_falls_back_to_full_scan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let rows: Vec<_> = (0..10)
            .map(|_| row_line(&entry(&now_ts(), "m", 1.0, 1, 1, 1)))
            .collect();
        append_lines(&path, &rows);
        let ledger = Ledger::new(&path);
        assert_eq!(ledger.month_to_date_usd().unwrap(), 10.0);
        let checkpoint_path = checkpoint_path_for(&path);
        let good = std::fs::read(&checkpoint_path).unwrap();
        let bucket = Utc::now().format("%Y-%m").to_string();

        std::fs::write(&checkpoint_path, b"{\"version\":1,\"offs").unwrap();
        assert_eq!(ledger.month_to_date_usd().unwrap(), 10.0);

        let mut edited: serde_json::Value = serde_json::from_slice(&good).unwrap();
        edited["months"][&bucket]["cost_usd"] = serde_json::json!(0.0_f64.to_bits());
        std::fs::write(&checkpoint_path, serde_json::to_vec(&edited).unwrap()).unwrap();
        assert_eq!(ledger.month_to_date_usd().unwrap(), 10.0);

        let mut other: LedgerCheckpoint = serde_json::from_slice(&good).unwrap();
        other.version = CHECKPOINT_VERSION + 1;
        other.months.clear();
        other.checksum = other.compute_checksum().unwrap();
        std::fs::write(&checkpoint_path, serde_json::to_vec(&other).unwrap()).unwrap();
        assert_eq!(ledger.month_to_date_usd().unwrap(), 10.0);

        std::fs::write(
            &checkpoint_path,
            vec![b' '; MAX_CHECKPOINT_BYTES as usize + 1],
        )
        .unwrap();
        assert_eq!(ledger.month_to_date_usd().unwrap(), 10.0);
        // Every fallback rebuilt it.
        let mut file = File::open(&path).unwrap();
        assert!(ledger.load_checkpoint(&mut file).is_some());
    }

    /// A version-1 checkpoint was written before the cap counted only metered
    /// rows, so its month total may include `subscription`/`free` spend. It
    /// must be rebuilt, never continued: otherwise a host upgraded from v1
    /// keeps the phantom spend behind the checkpoint offset forever.
    #[test]
    fn version_one_checkpoint_with_unmetered_spend_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let mut metered = entry(&now_ts(), "cloud/m", 0.5, 1, 1, 1);
        metered.billing = Some("metered".into());
        let mut plan = entry(&now_ts(), "plan/m", 170.0, 1, 1, 1);
        plan.billing = Some("subscription".into());
        append_lines(&path, &[row_line(&metered), row_line(&plan)]);
        let ledger = Ledger::new(&path);
        assert_eq!(ledger.month_to_date_usd().unwrap(), 0.5);

        // What a v1 build left behind: the same prefix, the plan row summed.
        let checkpoint_path = checkpoint_path_for(&path);
        let mut v1: LedgerCheckpoint =
            serde_json::from_slice(&std::fs::read(&checkpoint_path).unwrap()).unwrap();
        let bucket = Utc::now().format("%Y-%m").to_string();
        v1.version = 1;
        v1.months.get_mut(&bucket).unwrap().cost_usd = 170.5;
        v1.checksum = v1.compute_checksum().unwrap();
        std::fs::write(&checkpoint_path, serde_json::to_vec(&v1).unwrap()).unwrap();

        assert_eq!(ledger.month_to_date_usd().unwrap(), 0.5);
        let mut file = File::open(&path).unwrap();
        assert_eq!(
            ledger.load_checkpoint(&mut file).unwrap().version,
            CHECKPOINT_VERSION
        );
    }

    /// Rows after the checkpoint are still reported by their line in the file.
    #[test]
    fn malformed_row_after_checkpoint_keeps_its_file_line_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let mut rows: Vec<_> = (0..10)
            .map(|_| row_line(&entry(&now_ts(), "m", 1.0, 1, 1, 1)))
            .collect();
        rows.push(b"\n".to_vec());
        append_lines(&path, &rows);
        let ledger = Ledger::new(&path);
        assert_eq!(ledger.month_to_date_usd().unwrap(), 10.0);
        append_lines(&path, &[b"{\"ts_utc\":\"truncated\"\n".to_vec()]);

        for err in [
            ledger.month_to_date_usd().unwrap_err(),
            ledger.reserve_billable().err().unwrap(),
            ledger
                .record(&entry(&now_ts(), "m", 1.0, 1, 1, 1))
                .unwrap_err(),
        ] {
            let err = format!("{err:#}");
            assert!(err.contains("malformed"), "{err}");
            assert!(err.contains("line(s) 12"), "{err}");
        }
    }

    /// An unterminated last row may be an interrupted write; it is read (as
    /// the full scan reads it) but never checkpointed, so completing it later
    /// is seen.
    #[test]
    fn unterminated_last_row_is_not_checkpointed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let line = row_line(&entry(&now_ts(), "m", 1.0, 1, 1, 1));
        append_lines(&path, &[line.clone(), line[..line.len() - 1].to_vec()]);
        let ledger = Ledger::new(&path);
        assert_eq!(ledger.month_to_date_usd().unwrap(), 2.0);
        let (base, _) = fold_from_checkpoint(&ledger);
        assert_eq!(base.unwrap().offset, line.len() as u64);
        append_lines(&path, &[b"\n".to_vec(), line.clone()]);
        assert_eq!(ledger.month_to_date_usd().unwrap(), 3.0);
    }

    /// Hold the ledger lock SHARED from another descriptor (as a concurrent
    /// reader does) and run `read` on a thread: an exclusive lock would block
    /// until the timeout.
    fn completes_while_ledger_lock_is_held_shared<T: Send + 'static>(
        path: &Path,
        read: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock = open_lock_file(Path::new(&lock_path)).unwrap();
        FileExt::lock_shared(&lock).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = tx.send(read());
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(10)).ok();
        FileExt::unlock(&lock).unwrap();
        worker.join().unwrap();
        result
    }

    #[test]
    fn month_to_date_does_not_take_exclusive_ledger_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        Ledger::new(&path)
            .record(&entry(&now_ts(), "m", 1.5, 1, 1, 1))
            .unwrap();
        let reader = path.clone();
        let got = completes_while_ledger_lock_is_held_shared(&path, move || {
            Ledger::new(&reader).month_to_date_usd().unwrap()
        });
        assert_eq!(got, Some(1.5), "month-to-date queued behind a reader");
    }

    #[test]
    fn strict_shared_read_does_not_take_exclusive_ledger_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        Ledger::new(&path)
            .record(&entry(&now_ts(), "m", 1.5, 1, 1, 1))
            .unwrap();
        let reader = path.clone();
        let got = completes_while_ledger_lock_is_held_shared(&path, move || {
            Ledger::new(&reader).entries_strict_shared().unwrap().len()
        });
        assert_eq!(got, Some(1), "the strict read queued behind a reader");
    }

    /// The shared read cannot run recovery, so it leaves out a prepared row
    /// whose owner is gone (recovery would blank it) and keeps one whose owner
    /// is alive -- without writing to the ledger.
    #[test]
    fn shared_month_to_date_skips_only_abandoned_prepared_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let ledger = Ledger::new(&path);
        ledger.record(&entry(&now_ts(), "m", 1.0, 1, 1, 1)).unwrap();
        let mut abandoned = ledger.reserve_billable().unwrap();
        drop(abandoned.owner_lock.take());
        std::mem::forget(abandoned);
        let before = std::fs::read(&path).unwrap();

        assert_eq!(ledger.month_to_date_usd().unwrap(), 1.0);
        assert_eq!(std::fs::read(&path).unwrap(), before, "a read wrote");

        let live = ledger.reserve_billable().unwrap();
        let err = ledger.month_to_date_usd().unwrap_err().to_string();
        assert!(err.contains("1 call(s) with unknown cost"), "{err}");
        drop(live);
        assert_eq!(ledger.month_to_date_usd().unwrap(), 1.0);
    }
}

/// Bounded ledger lock acquisition.
///
/// `fs2::FileExt::lock_exclusive` is a kernel wait: it blocks until the
/// holder releases the `flock(2)`, with no deadline. A second process that
/// is wedged (or a lock orphaned on a filesystem where the flock outlives
/// its writer) therefore hung `zoder review`/`zoder loop` forever BEFORE the
/// first provider call — a zero-token run that never finished. These tests
/// hold the lock from a second open file description and pin the bounded
/// acquisition that replaces it.
///
/// The deadline is a process-wide environment variable, so every test in
/// this module serializes on `env_lock` to keep one test's budget from
/// leaking into another's.
#[cfg(test)]
mod lock_deadline_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Serializes tests that set `ZODER_LEDGER_LOCK_TIMEOUT_S`.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `body` with `ZODER_LEDGER_LOCK_TIMEOUT_S` set to `seconds`, then
    /// restore the previous value. Holding the `ENV_LOCK` guard for the
    /// duration of `body` (rather than storing it in a struct) keeps the
    /// guard's lifetime tied to the call, not to `'static`.
    fn with_budget<T>(seconds: f64, body: impl FnOnce() -> T) -> T {
        let _env = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        let previous = std::env::var_os("ZODER_LEDGER_LOCK_TIMEOUT_S");
        std::env::set_var(LEDGER_LOCK_TIMEOUT_ENV, format!("{seconds}"));
        // Restore on the way out of the scope even if `body` panics (an
        // assertion failure in a test does), so a failed test cannot leak
        // its budget into the ones that run after it.
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var(LEDGER_LOCK_TIMEOUT_ENV, value),
                    None => std::env::remove_var(LEDGER_LOCK_TIMEOUT_ENV),
                }
            }
        }
        let _restore = Restore(previous);
        body()
    }

    /// One valid metered row, timestamped now.
    fn row() -> Entry {
        Entry {
            ts_utc: Utc::now(),
            provider: "test".into(),
            model: "test/model".into(),
            host: String::new(),
            tokens_in: 1,
            tokens_out: 1,
            cost_usd: 1.0,
            cost_unknown: false,
            calls: 1,
            violation: None,
            tags: FinOpsTags::default(),
            billing: None,
            estimated_cost_usd: None,
        }
    }

    fn ledger_lock(ledger: &Ledger) -> File {
        open_lock_file(&ledger.lock_path()).unwrap()
    }

    /// A second open file description holds the exclusive flock, exactly as
    /// a concurrent `zoder` process would. `flock(2)` is per open file
    /// description, so this FD's lock genuinely excludes other opens of the
    /// same path even from this process.
    fn hold_from_another_open(path: &Path) -> File {
        let holder = open_lock_file(path).unwrap();
        holder.lock_exclusive().unwrap();
        holder
    }

    /// The per-reservation owner sidecar carries a random marker in its
    /// path, so only a thread of THIS process can hold it. Its timeout
    /// message must say so instead of blaming an imaginary peer process.
    #[test]
    fn the_owner_sidecar_timeout_blames_this_process_not_another() {
        with_budget(0.3, || {
            let dir = tempfile::tempdir().unwrap();
            let lock_path = dir.path().join("ledger.jsonl.lock.owner");
            let holder = hold_from_another_open(&lock_path);

            let file = open_lock_file(&lock_path).unwrap();
            let error = lock_owner_sidecar_bounded(&file, &lock_path)
                .expect_err("a held sidecar must time out");
            drop(holder);

            let message = format!("{error:#}");
            assert!(
                message.contains("another thread of this process holds it"),
                "the owner-sidecar message must name the real holder: {message}"
            );
            assert!(
                !message.contains("another process holds it"),
                "the owner sidecar cannot be held by another process: {message}"
            );
        });
    }

    /// Regression: with the lock held elsewhere and a 1 s budget, `record()`
    /// must FAIL within the budget instead of blocking for ever. On the
    /// unbounded implementation the worker thread never returns and the
    /// 10 s `recv_timeout` below is what ends the test.
    #[test]
    fn a_contended_ledger_lock_fails_within_the_deadline_instead_of_hanging() {
        with_budget(1.0, || {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ledger.jsonl");
            let ledger = Ledger::new(&path);
            let lock_path = ledger.lock_path();
            let holder = hold_from_another_open(&lock_path);

            let (tx, rx) = mpsc::channel();
            let started = Instant::now();
            let worker = std::thread::spawn({
                let path = path.clone();
                move || {
                    let ledger = Ledger::new(&path);
                    let _ = tx.send(ledger.record(&row()));
                }
            });

            let result = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("record() must give up on the ledger lock, not block for ever");
            let elapsed = started.elapsed();
            worker.join().unwrap();
            drop(holder);

            let error = result.expect_err("record() must fail when the lock budget expires");
            let message = format!("{error:#}");
            assert!(
                message.contains("timed out after 1s waiting for the ledger lock at"),
                "the deadline error must name the timeout, the budget and the lock path: {message}"
            );
            assert!(
                message.contains(&lock_path.display().to_string()),
                "the deadline error must name the lock path: {message}"
            );
            assert!(
                message.contains("another process holds it"),
                "the deadline error must say the lock is held elsewhere: {message}"
            );
            assert!(
                // Bound close to the 1 s budget: a 10 s bound would pass even
                // if the deadline arithmetic or backoff were wrong.
                elapsed < Duration::from_secs(3),
                "record() took {elapsed:?}, not bounded by its 1 s budget"
            );
            assert!(
                !path.exists() || std::fs::metadata(&path).unwrap().len() == 0,
                "a failed acquisition must not write a ledger row"
            );
        });
    }

    /// REGRESSION: the routing snapshot's SHARED read hung the same way.
    /// A wedged process holding the lock exclusively blocked
    /// `entries_strict_shared` (the QUOTA-ROUTING read `zoder review` takes
    /// before it dispatches) for ever, because a plain `lock_shared` is just
    /// as unbounded as `lock_exclusive`. Bounded like the writers.
    #[test]
    fn a_contended_shared_read_fails_within_the_deadline_instead_of_hanging() {
        with_budget(1.0, || {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ledger.jsonl");
            let ledger = Ledger::new(&path);
            ledger.record(&row()).unwrap();
            let holder = hold_from_another_open(&ledger.lock_path());

            let (tx, rx) = mpsc::channel();
            let started = Instant::now();
            let worker = std::thread::spawn({
                let path = path.clone();
                move || {
                    let _ = tx.send(Ledger::new(&path).entries_strict_shared());
                }
            });

            let result = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("the shared read must give up on the ledger lock, not block for ever");
            let elapsed = started.elapsed();
            worker.join().unwrap();
            drop(holder);

            let error = result.expect_err("the shared read must fail when the lock budget expires");
            let message = format!("{error:#}");
            assert!(
                message.contains("timed out after 1s waiting for the ledger lock at")
                    && message.contains("(shared lock"),
                "the deadline error must name the timeout and the shared lock: {message}"
            );
            assert!(
                // Bound close to the 1 s budget: a 10 s bound would pass even
                // if the deadline arithmetic or backoff were wrong.
                elapsed < Duration::from_secs(3),
                "the shared read took {elapsed:?}, not bounded by its 1 s budget"
            );
        });
    }

    /// A budget too large for a `Duration` must fall back to the default
    /// instead of panicking: `Duration::from_secs_f64` aborts on values it
    /// cannot represent, so a typo priced in the wrong unit would have
    /// blown up the process rather than taking the documented fallback.
    #[test]
    fn an_unrepresentable_budget_falls_back_instead_of_panicking() {
        with_budget(1e300, || {
            let budget = ledger_lock_budget().expect("a huge budget must not mean unbounded");
            assert_eq!(
                budget,
                Duration::from_secs_f64(DEFAULT_LEDGER_LOCK_TIMEOUT_SECS),
                "an unrepresentable budget must fall back to the default"
            );
            // And the ledger still works under the fallback, rather than aborting.
            let dir = tempfile::tempdir().unwrap();
            let ledger = Ledger::new(&dir.path().join("ledger.jsonl"));
            ledger
                .record(&row())
                .expect("a fallback budget must still record");
        });
    }

    /// A budget that is a VALID `Duration` but far too large to add to an
    /// `Instant` must degrade to unbounded rather than overflow-panic. Pins
    /// the `checked_add` guard around the deadline computation.
    #[test]
    fn an_absurd_but_representable_budget_does_not_overflow_the_deadline() {
        with_budget(1e18, || {
            let budget = ledger_lock_budget().expect("a huge budget must not mean unbounded");
            assert!(
                std::time::Instant::now().checked_add(budget).is_some(),
                "the platform Instant must tolerate this budget, so the guard is never hit \
                 here; the assertion documents the real limit"
            );
            // And an uncontended ledger still acquires under it, no panic.
            let dir = tempfile::tempdir().unwrap();
            let ledger = Ledger::new(&dir.path().join("ledger.jsonl"));
            ledger
                .record(&row())
                .expect("an absurd budget must still record");
        });
    }

    /// The bound must not cost anything in the common case: an uncontended
    /// lock is taken on the first `try_lock_exclusive`, with no backoff
    /// sleep at all.
    #[test]
    fn an_uncontended_ledger_lock_is_taken_immediately() {
        with_budget(30.0, || {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ledger.jsonl");
            let ledger = Ledger::new(&path);
            let lock_path = ledger.lock_path();

            let started = Instant::now();
            let lock = ledger_lock(&ledger);
            lock_exclusive_bounded(&lock, &lock_path).unwrap();
            let elapsed = started.elapsed();
            FileExt::unlock(&lock).unwrap();

            assert!(
                elapsed < Duration::from_millis(100),
                "an uncontended lock must be granted on the first attempt, took {elapsed:?}"
            );
            assert!(
                lock_path.exists(),
                "the lock sidecar is created by the open, as before"
            );
        });
    }

    /// `ZODER_LEDGER_LOCK_TIMEOUT_S=0` means unbounded (documented opt-out
    /// for callers that would rather wait than fail). Pinned with a short
    /// hold: the acquisition succeeds, so the budget did not fire.
    #[test]
    fn a_zero_budget_means_unbounded_and_still_acquires() {
        with_budget(0.0, || {
            let dir = tempfile::tempdir().unwrap();
            let ledger = Ledger::new(&dir.path().join("ledger.jsonl"));
            let lock_path = ledger.lock_path();
            let holder = hold_from_another_open(&lock_path);

            let (tx, rx) = mpsc::channel();
            let worker = std::thread::spawn({
                let lock_path = lock_path.clone();
                move || {
                    let lock = open_lock_file(&lock_path).unwrap();
                    let result = lock_exclusive_bounded(&lock, &lock_path);
                    if result.is_ok() {
                        let _ = FileExt::unlock(&lock);
                    }
                    let _ = tx.send(result);
                }
            });
            assert!(
                rx.recv_timeout(Duration::from_millis(300)).is_err(),
                "with a zero budget the acquisition must still be waiting on the holder"
            );
            drop(holder);
            let result = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the unbounded acquisition must proceed once the holder releases");
            result.expect("the unbounded acquisition must succeed after the release");
            worker.join().unwrap();
        });
    }

    /// The Drop path must never panic and must never block: an armed
    /// reservation whose lock is held elsewhere gives up after the budget,
    /// leaving the row for recovery instead of wedging the process exit.
    #[test]
    fn the_drop_guard_gives_up_on_a_contended_lock_without_panicking() {
        with_budget(0.3, || {
            let dir = tempfile::tempdir().unwrap();
            let ledger = Ledger::new(&dir.path().join("ledger.jsonl"));
            let lock_path = ledger.lock_path();
            let mut reservation = ledger.reserve_billable().unwrap();
            reservation.arm_for("test", "test/model").unwrap();
            let holder = hold_from_another_open(&lock_path);

            let started = Instant::now();
            drop(reservation);
            let elapsed = started.elapsed();
            drop(holder);

            assert!(
                elapsed < Duration::from_secs(5),
                "dropping an armed reservation must be bounded, took {elapsed:?}"
            );
        });
    }
}
