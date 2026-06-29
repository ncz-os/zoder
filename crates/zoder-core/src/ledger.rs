//! Local spend ledger. Append-only JSONL, one record per model call
//! (ts_utc, provider, model, tokens_in, tokens_out, cost_usd), with
//! day/week/month/year rollups. SQLite is a drop-in later via the same shape.

use chrono::{DateTime, Datelike, IsoWeek, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

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
    /// Set when the post-call free-policy guard flagged this spend (e.g. a
    /// "free" model that was actually billed or served from a paid backend).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub violation: Option<String>,
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
}

pub struct Ledger {
    path: PathBuf,
}

impl Ledger {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
        }
    }

    pub fn record(&self, e: &Entry) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        // Serialize record + newline into one buffer and emit it with a single
        // write_all. With O_APPEND this is one syscall, so concurrent writers
        // can't interleave a partial line. (writeln! issues two writes.)
        let mut line = serde_json::to_string(e)?;
        line.push('\n');
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Parse all entries, invoking `on_malformed(line_no, raw_line)` for every
    /// non-empty line that fails to parse. A single mangled/half-written line
    /// (e.g. from an interrupted append) is skipped rather than aborting the
    /// rollup, but — unlike a silent drop — the caller can observe and surface
    /// dropped spend. Mirrors the TS `Ledger.entries({ onMalformed })`.
    pub fn entries_observed(
        &self,
        mut on_malformed: impl FnMut(usize, &str),
    ) -> anyhow::Result<Vec<Entry>> {
        let Ok(raw) = std::fs::read_to_string(&self.path) else {
            return Ok(vec![]);
        };
        let mut out = Vec::new();
        for (i, l) in raw.lines().enumerate() {
            if l.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Entry>(l) {
                Ok(e) => out.push(e),
                Err(_) => on_malformed(i + 1, l),
            }
        }
        Ok(out)
    }

    /// All entries, silently skipping malformed lines. For visibility into
    /// dropped lines use [`Ledger::entries_observed`].
    pub fn entries(&self) -> anyhow::Result<Vec<Entry>> {
        self.entries_observed(|_, _| {})
    }

    /// Entries within an optional [since, until] window (inclusive).
    pub fn entries_in(
        &self,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Vec<Entry>> {
        Ok(self
            .entries()?
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
    /// Returns 0.0 on any read error (the gate fails open on a missing ledger).
    pub fn month_to_date_usd(&self) -> f64 {
        let bucket = Utc::now().format("%Y-%m").to_string();
        self.rollup(Period::Month)
            .ok()
            .and_then(|m| m.get(&bucket).map(|r| r.cost_usd))
            .unwrap_or(0.0)
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
            r.cost_usd += e.cost_usd;
            r.tokens_in += e.tokens_in;
            r.tokens_out += e.tokens_out;
            r.calls += 1;
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
            r.cost_usd += e.cost_usd;
            r.tokens_in += e.tokens_in;
            r.tokens_out += e.tokens_out;
            r.calls += 1;
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
            r.cost_usd += e.cost_usd;
            r.tokens_in += e.tokens_in;
            r.tokens_out += e.tokens_out;
            r.calls += 1;
        }
        Ok(out)
    }
}
