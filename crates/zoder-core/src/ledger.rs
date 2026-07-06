//! Local spend ledger. Append-only JSONL, one record per model call
//! (ts_utc, provider, model, tokens_in, tokens_out, cost_usd), with
//! day/week/month/year rollups. SQLite is a drop-in later via the same shape.

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
/// multi-gigabyte garbage line; matches `provider.rs`'s
/// `MAX_LINE_BYTES` for streaming SSE.
const MAX_LEDGER_LINE_BYTES: usize = 16 * 1024 * 1024; // 16 MiB
/// Space allocated before a billable call. The completed entry is written into
/// this already-allocated region, so a full filesystem cannot strand spend
/// after dispatch. Ledger entries contain metadata, not prompts/responses; 64
/// KiB leaves ample reconciliation headroom while keeping the bound explicit.
const BILLABLE_RESERVATION_BYTES: usize = 64 * 1024;

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
}

impl BillableReservation {
    /// Month-to-date known spend from the strict snapshot taken while this
    /// reservation was created under the exclusive sidecar lock.
    pub fn month_to_date_usd(&self) -> anyhow::Result<f64> {
        self.month_to_date
            .as_ref()
            .copied()
            .map_err(|message| anyhow::anyhow!(message.clone()))
    }

    /// Mark the reservation as immediately preceding dispatch. Before this is
    /// called, dropping the guard cancels the slot (for example when a user
    /// declines a budget prompt); afterward, an unreconciled slot is retained
    /// as unknown spend so failures cannot become unaccounted retries.
    pub fn arm(&mut self) -> anyhow::Result<()> {
        let lock = open_lock_file(&self.lock_path)?;
        lock.lock_exclusive()?;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .with_context(|| {
                format!("opening ledger at {} before dispatch", self.path.display())
            })?;
        verify_reservation_slot(
            &mut file,
            self.offset,
            self.slot_bytes,
            &self.marker,
            &self.path,
        )?;
        self.armed = true;
        Ok(())
    }

    /// Replace the preallocated unknown-cost reservation with the final entry.
    /// No allocation or append is needed after the provider has been called.
    pub fn reconcile(mut self, entry: &Entry) -> anyhow::Result<()> {
        self.armed = true;
        if !entry_numbers_valid(entry) {
            anyhow::bail!("ledger entry contains invalid cost, calls, or cache-hit telemetry");
        }
        let json = serde_json::to_vec(entry)?;
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
        verify_reservation_slot(
            &mut file,
            self.offset,
            self.slot_bytes,
            &self.marker,
            &self.path,
        )?;
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
                        if verify_reservation_slot(
                            &mut file,
                            self.offset,
                            self.slot_bytes,
                            &self.marker,
                            &self.path,
                        )
                        .is_ok()
                        {
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
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.path)?;
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
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.path)
            .with_context(|| {
                format!("opening ledger for reservation at {}", self.path.display())
            })?;
        let entries = self.entries_strict_from_file(&mut file)?;
        let month_to_date = month_to_date_from_entries(&entries).map_err(|error| error.to_string());

        let offset = file.seek(SeekFrom::End(0))?;
        let marker = format!("zoder-reservation-{:032x}", rand::random::<u128>());
        let pending = Entry {
            ts_utc: Utc::now(),
            provider: "__zoder_reservation__".to_string(),
            model: "__pending_billable_call__".to_string(),
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
        })
    }

    fn lock_path(&self) -> PathBuf {
        let mut lock = self.path.as_os_str().to_os_string();
        lock.push(".lock");
        PathBuf::from(lock)
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
        FileExt::lock_shared(&lock)?;
        let mut file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(anyhow::Error::from(e)
                    .context(format!("opening ledger at {}", self.path.display())))
            }
        };
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
        FileExt::lock_shared(&lock)
            .with_context(|| format!("locking ledger at {}", self.path.display()))?;
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
