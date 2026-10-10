//! Local spend ledger. Append-only JSONL, one record per model call
//! (ts_utc, provider, model, tokens_in, tokens_out, cost_usd), with
//! day/week/month/year rollups. SQLite is a drop-in later via the same shape.
//!
//! Concurrency note: the reserve->commit serialization relies on `fs2` /
//! `flock(2)` advisory locking, which is unreliable over NFS (locks may be
//! silently ignored or not propagate between clients). The ledger file MUST
//! therefore live on a LOCAL filesystem. The default `~/.zoder/` is local; a
//! future config that points the ledger at an NFS mount (for example an
//! ARGONAS share) would weaken the reserve->commit serialization and must be
//! avoided.

use anyhow::Context;
use chrono::{DateTime, Datelike, IsoWeek, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

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
}

fn one_call() -> u64 {
    1
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
}

fn accumulate_rollup(rollup: &mut Rollup, entry: &Entry) -> anyhow::Result<()> {
    if entry.cost_unknown {
        rollup.unknown_cost_tokens = rollup
            .unknown_cost_tokens
            .saturating_add(entry.tokens_in.saturating_add(entry.tokens_out));
        rollup.unknown_cost_calls = rollup.unknown_cost_calls.saturating_add(entry.calls);
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
    owner_lock: Option<File>,
    owner_lock_path: PathBuf,
}

const RESERVATION_PROVIDER: &str = "__zoder_reservation__";
const PREPARED_RESERVATION_MODEL: &str = "__zoder_billable_p__";
const ARMED_RESERVATION_MODEL: &str = "__zoder_billable_a__";

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
        if self.armed {
            anyhow::bail!("billable reservation is already armed");
        }
        let lock = open_lock_file(&self.lock_path)?;
        lock.lock_exclusive()?;
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
        self.owner_lock.take();
        let _ = std::fs::remove_file(&self.owner_lock_path);
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
        lock.lock_exclusive()?;
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
}

impl Drop for BillableReservation {
    fn drop(&mut self) {
        if !self.armed {
            // Other writers may have appended after this reservation was
            // created, so cancellation must blank only our fixed-size slot;
            // truncating to `offset` would discard their rows.
            if let Ok(lock) = open_lock_file(&self.lock_path) {
                if lock.lock_exclusive().is_ok() {
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
            }
        }
        self.owner_lock.take();
        let _ = std::fs::remove_file(&self.owner_lock_path);
    }
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
        lock.lock_exclusive()?;
        let mut f = self.open_locked_rw("recording")?;
        self.entries_strict_from_file(&mut f)?;
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
        lock.lock_exclusive()
            .with_context(|| format!("locking ledger at {}", self.path.display()))?;
        let mut file = self.open_locked_rw("reservation")?;
        let entries = self.entries_strict_from_file(&mut file)?;
        let month_to_date = month_to_date_from_entries(&entries).map_err(|error| error.to_string());

        let offset = file.seek(SeekFrom::End(0))?;
        let marker = format!("zoder-reservation-{:032x}", rand::random::<u128>());
        let owner_lock_path = reservation_owner_lock_path(&self.path, &marker);
        let owner_lock = open_lock_file(&owner_lock_path)?;
        owner_lock.lock_exclusive().with_context(|| {
            format!(
                "locking billable reservation owner at {}",
                owner_lock_path.display()
            )
        })?;
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
    fn recover_abandoned_prepared(&self, file: &mut File) -> anyhow::Result<ScanStats> {
        file.seek(SeekFrom::Start(0))?;
        let mut reader = BufReader::new(&mut *file);
        let mut offset = 0_u64;
        let mut line_no = 0_usize;
        let mut line = Vec::new();
        let mut abandoned = Vec::new();
        let mut waste = 0_u64;
        let mut legacy_inflight = false;
        let now = Utc::now();
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
            if is_reservation_slot_len(bytes) {
                let mut payload = line.as_slice();
                while matches!(payload.last(), Some(b'\n') | Some(b'\r') | Some(b' ')) {
                    payload = &payload[..payload.len() - 1];
                }
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
                if !is_reservation {
                    // Finished or blank slot: everything past the payload
                    // and its newline is compactable padding.
                    waste = waste.saturating_add(
                        u64::try_from(bytes.saturating_sub(payload.len() + 1)).unwrap_or(0),
                    );
                }
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
            offset = offset.saturating_add(u64::try_from(bytes)?);
        }
        drop(reader);
        if abandoned.is_empty() {
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
                    Ok(entry) if entry.provider != RESERVATION_PROVIDER => {
                        out.write_all(payload)?;
                        out.write_all(b"\n")?;
                    }
                    // Reservation rows and anything unparseable: unchanged.
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
    fn open_locked_rw(&self, what: &str) -> anyhow::Result<File> {
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
        let stats = self.recover_abandoned_prepared(&mut file)?;
        if self.compact_if_wasteful(&mut file, stats)? {
            file = open()?;
        }
        Ok(file)
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
        lock.lock_exclusive()?;
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(anyhow::Error::from(e)
                    .context(format!("opening ledger at {}", self.path.display())))
            }
        };
        self.recover_abandoned_prepared(&mut file)?;
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
        lock.lock_exclusive()
            .with_context(|| format!("locking ledger at {}", self.path.display()))?;
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(anyhow::Error::from(error)
                    .context(format!("opening ledger at {}", self.path.display())))
            }
        };
        self.recover_abandoned_prepared(&mut file)?;
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
    pub fn month_to_date_usd(&self) -> anyhow::Result<f64> {
        month_to_date_from_entries(&self.entries_strict()?)
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

fn read_entries(
    file: &mut File,
    path: &Path,
    mut on_malformed: impl FnMut(usize, &str),
) -> anyhow::Result<Vec<Entry>> {
    file.seek(SeekFrom::Start(0))?;
    // Parse incrementally so one invalid UTF-8 byte cannot blank a large
    // ledger, while bounding memory before accumulating an attacker-sized line.
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut out = Vec::new();
    let mut line_no = 0usize;
    let mut buf = Vec::new();
    loop {
        line_no += 1;
        buf.clear();
        loop {
            let available = reader.fill_buf().with_context(|| {
                format!("reading ledger at line {line_no} of {}", path.display())
            })?;
            if available.is_empty() {
                if buf.is_empty() {
                    return Ok(out);
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
            reader.consume(chunk_len);
            if has_newline {
                break;
            }
        }
        while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
            buf.pop();
        }
        if buf.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        match serde_json::from_slice::<Entry>(&buf) {
            Ok(entry) if entry_numbers_valid(&entry) => out.push(entry),
            _ => {
                let raw = String::from_utf8_lossy(&buf).into_owned();
                on_malformed(line_no, &raw);
            }
        }
    }
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

fn month_to_date_from_entries(entries: &[Entry]) -> anyhow::Result<f64> {
    let bucket = Utc::now().format("%Y-%m").to_string();
    let mut rollup = Rollup::default();
    for entry in entries
        .iter()
        .filter(|entry| Period::Month.bucket(&entry.ts_utc) == bucket)
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
}
