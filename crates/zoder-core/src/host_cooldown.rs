//! Host-level back-off marker: one file beside the ledger that says "this
//! host tripped its monthly spend cap — do not start another metered call".
//!
//! WHY this exists (decision D8 step 12)
//!
//! When the monthly cap trips, every worker on the host refuses the next
//! metered call in exactly the same way. One tripped worker therefore
//! produced N identical refusals (the incident report counted 49). The
//! refusal itself is correct — the cap is real — but each sibling re-derives
//! it, re-reads the ledger, re-prompts, and re-refuses. The fix is a
//! HOST-LOCAL signal: the first trip writes a marker, the siblings read it
//! and fall through to a cost-neutral (free / subscription) route instead of
//! walking into the same wall.
//!
//! The prerequisite for this feature is D8 step 1: the month-to-date total
//! sums only `billing = "metered"` rows. Without it the marker would encode
//! PHANTOM spend (a subscription row's usage telemetry priced at list), and
//! the host would back off when nothing was actually billed.
//!
//! MARKER FORMAT (stable, documented, shared with the Hive worker)
//!
//! WHO WRITES IT: [`crate::budget::Budget::evaluate_call_with_host_cooldown`]
//! (and its two-number sibling `evaluate_with_host_cooldown`) — the pre-call
//! budget gate every worker already runs. One tripped worker writes; every
//! later worker on the host reads it before it routes. `zoder status` reports
//! it through [`status_line`] / [`status_view`].
//!
//! The marker is a single JSON object written to a file whose name is
//! derived from the ledger path, so it always sits BESIDE the ledger and
//! moves with it:
//!
//! ```text
//! <ledger path>.cooldown.json
//! ```
//!
//! The JSON object carries exactly these keys:
//!
//! | key           | type            | meaning                                            |
//! |---------------|-----------------|----------------------------------------------------|
//! | `version`     | integer         | format version, currently `1`                      |
//! | `reason`      | string          | short machine-ish reason, `"monthly_cap"`          |
//! | `detail`      | string          | human-readable detail (cap, spend)                 |
//! | `limit_usd`   | number          | the configured monthly cap in USD                  |
//! | `spent_usd`   | number          | month-to-date BILLED spend in USD when it tripped  |
//! | `created_at`  | string (RFC3339)| UTC instant the marker was written                 |
//! | `expires_at`  | string (RFC3339)| UTC instant the back-off lapses                    |
//!
//! The Hive worker (FB64, worker-side budget back-off) reads the SAME
//! file: it is plain JSON with `created_at` / `expires_at` in RFC 3339, so
//! a Python reader needs no zoder code. A marker whose `expires_at` is in
//! the past is INERT (and is removed best-effort when read), so a stale file
//! can never park a host forever.
//!
//! The read path is FAIL-OPEN on a corrupt marker: a malformed file is
//! treated as "no cooldown" (the host routes as usual) rather than
//! "cooldown forever" (the host refuses everything). The write path is
//! fail-loud: a caller that cannot write the marker still gets its budget
//! refusal, it just does not get the host-wide signal.

use anyhow::Context;
use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Suffix of the cooldown marker written beside the ledger.
pub const COOLDOWN_SUFFIX: &str = ".cooldown.json";

/// Layout version of the marker file. A marker of another version is
/// ignored (treated as "no cooldown"), never misread.
pub const COOLDOWN_VERSION: u32 = 1;

/// The `reason` recorded when the monthly spend cap tripped.
pub const REASON_MONTHLY_CAP: &str = "monthly_cap";

/// Default back-off length when the caller does not pick one.
///
/// The monthly cap resets at the next UTC calendar month, so the back-off
/// covers the REST of the current month by default: one trip backs the host
/// off until the cap's own window rolls over. A caller that knows the cap's
/// reset instant should pass that instant explicitly.
pub const DEFAULT_COOLDOWN_SECS: i64 = 60 * 60;

/// A marker file larger than this was not written by this build and is not
/// parsed.
const MAX_COOLDOWN_BYTES: u64 = 64 * 1024;

/// The host-level back-off marker: "metered routes are off until
/// `expires_at`".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostCooldown {
    /// Format version. See [`COOLDOWN_VERSION`].
    pub version: u32,
    /// Why the host backed off. [`REASON_MONTHLY_CAP`] for a spend cap.
    pub reason: String,
    /// Human-readable detail (never parsed for a decision).
    #[serde(default)]
    pub detail: String,
    /// The configured cap in USD, when the reason was a spend cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_usd: Option<f64>,
    /// The month-to-date billed spend in USD when the cap tripped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spent_usd: Option<f64>,
    /// When the marker was written (UTC).
    pub created_at: DateTime<Utc>,
    /// When the back-off lapses (UTC). A marker past this instant is inert.
    pub expires_at: DateTime<Utc>,
}

impl HostCooldown {
    /// `true` when the marker is still in force at `now`.
    pub fn is_active_at(&self, now: DateTime<Utc>) -> bool {
        now < self.expires_at
    }

    /// `true` when the marker is still in force right now.
    pub fn is_active(&self) -> bool {
        self.is_active_at(Utc::now())
    }
}

/// The marker path beside `ledger_path`.
///
/// Derived from the ledger path (like the `.lock` companion and the
/// `.checkpoint` sidecar), so the marker travels with the ledger when an
/// operator points the ledger somewhere else.
pub fn cooldown_path_for(ledger_path: &Path) -> PathBuf {
    let mut path = ledger_path.as_os_str().to_os_string();
    path.push(COOLDOWN_SUFFIX);
    PathBuf::from(path)
}

/// Read the host cooldown marker beside `ledger_path`.
///
/// Returns `Ok(None)` when there is no marker, when the marker is expired,
/// or when the marker is corrupt/unreadable — the fail-open contract: a
/// broken marker must never park a host. An expired marker is removed
/// best-effort so the directory does not accumulate dead files; a removal
/// failure is NOT an error (the marker is inert either way).
pub fn read_cooldown(ledger_path: &Path) -> anyhow::Result<Option<HostCooldown>> {
    read_cooldown_at(ledger_path, Utc::now())
}

/// [`read_cooldown`] with an explicit clock, so tests can pin "now".
pub fn read_cooldown_at(
    ledger_path: &Path,
    now: DateTime<Utc>,
) -> anyhow::Result<Option<HostCooldown>> {
    read_cooldown_at_with(ledger_path, now, true)
}

/// [`read_cooldown_at`], with the expired-marker cleanup optional.
///
/// The write path reads WITHOUT cleanup: a writer that is extending a marker
/// must see the file that is there, not delete it and then re-create a
/// shorter one (two trips racing would otherwise be able to lose the longer
/// hold-off). Every read that DECIDES something uses the cleanup form.
fn read_cooldown_at_with(
    ledger_path: &Path,
    now: DateTime<Utc>,
    remove_expired: bool,
) -> anyhow::Result<Option<HostCooldown>> {
    let path = cooldown_path_for(ledger_path);
    let metadata = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e)
                .context(format!("reading host cooldown marker {}", path.display())))
        }
    };
    if metadata.len() > MAX_COOLDOWN_BYTES {
        // Too large to be ours: treat as absent (fail open) and do not
        // parse it.
        return Ok(None);
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // Not UTF-8 (binary garbage, a half-written file, something else's
        // file at this path): corrupt, so INERT, not fatal — same fail-open
        // arm as malformed JSON below. Propagating `InvalidData` here would
        // make a stray binary file park or crash every worker on the host.
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e)
                .context(format!("reading host cooldown marker {}", path.display())))
        }
    };
    let marker: HostCooldown = match serde_json::from_str(&raw) {
        Ok(marker) => marker,
        // A corrupt marker is INERT, not fatal: the host routes as usual.
        Err(_) => return Ok(None),
    };
    if marker.version != COOLDOWN_VERSION {
        // A marker written by a future/other build is not misread.
        return Ok(None);
    }
    if !marker.is_active_at(now) {
        // Expired: inert. Remove it best-effort.
        if remove_expired {
            let _ = std::fs::remove_file(&path);
        }
        return Ok(None);
    }
    Ok(Some(marker))
}

/// `true` when a live host cooldown marker sits beside `ledger_path`.
pub fn cooldown_active(ledger_path: &Path) -> bool {
    matches!(read_cooldown(ledger_path), Ok(Some(_)))
}

/// Write the host cooldown marker beside `ledger_path`.
///
/// The write is atomic (temp file + rename) so a concurrent reader never
/// sees a half-written marker, and it never SHORTENS a live marker: a
/// second trip on the same host extends the hold-off instead of cutting it
/// short. Returns the marker as written.
pub fn write_cooldown(ledger_path: &Path, marker: &HostCooldown) -> anyhow::Result<HostCooldown> {
    let path = cooldown_path_for(ledger_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating host cooldown directory {}", parent.display()))?;
    }
    let mut out = marker.clone();
    if out.version == 0 {
        out.version = COOLDOWN_VERSION;
    }
    // Never shorten a live marker: another trip on this host must not cut
    // the existing hold-off short.
    if let Ok(Some(current)) = read_cooldown_at_with(ledger_path, Utc::now(), false) {
        if current.expires_at > out.expires_at {
            out.expires_at = current.expires_at;
        }
        if current.created_at < out.created_at {
            out.created_at = current.created_at;
        }
    }
    let body = serde_json::to_vec_pretty(&out)?;
    // The temp name is APPENDED to the marker path (not derived with
    // `with_extension`, which would REPLACE the ledger's own extension and
    // produce a confusing `ledger.jsonl.cooldown.cooldown.json.tmp`).
    // Appending keeps the marker path recognizable in a directory listing.
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)
            .with_context(|| format!("creating host cooldown temp file {}", tmp.display()))?;
        file.write_all(&body)?;
        file.sync_data()?;
    }
    std::fs::rename(&tmp, &path).with_context(|| {
        format!(
            "publishing host cooldown marker {} -> {}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(out)
}

/// Remove the host cooldown marker beside `ledger_path`.
///
/// Best-effort: a missing marker is already "no cooldown", so a removal
/// failure is reported as success (there is nothing to clear).
pub fn clear_cooldown(ledger_path: &Path) -> anyhow::Result<()> {
    let path = cooldown_path_for(ledger_path);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::Error::new(e)
            .context(format!("removing host cooldown marker {}", path.display()))),
    }
}

/// Build the marker for a tripped monthly cap and write it beside
/// `ledger_path`.
///
/// `limit_usd` is the configured monthly cap, `spent_usd` the month-to-date
/// BILLED spend that tripped it. The back-off runs until the end of the
/// current UTC calendar month by default — the cap's own window — so the
/// host stops spending until the cap actually resets.
pub fn trip_monthly_cap_cooldown(
    ledger_path: &Path,
    limit_usd: f64,
    spent_usd: f64,
    now: DateTime<Utc>,
) -> anyhow::Result<HostCooldown> {
    let marker = HostCooldown {
        version: COOLDOWN_VERSION,
        reason: REASON_MONTHLY_CAP.to_string(),
        detail: format!("monthly spend cap reached: ${spent_usd:.2} spent of ${limit_usd:.2} cap"),
        limit_usd: Some(limit_usd),
        spent_usd: Some(spent_usd),
        created_at: now,
        expires_at: month_end(now),
    };
    write_cooldown(ledger_path, &marker)
}

/// The first instant of the NEXT UTC calendar month — when a monthly cap
/// resets.
fn month_end(now: DateTime<Utc>) -> DateTime<Utc> {
    use chrono::Duration;
    let (year, month) = (now.year(), now.month());
    let next = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    Utc.with_ymd_and_hms(next.0, next.1, 1, 0, 0, 0)
        .single()
        .unwrap_or_else(|| now + Duration::days(1))
}

/// The host-level back-off hook the budget gate calls after it decides.
///
/// This is the ONE function that turns a budget decision into host state, so
/// every writer of the marker agrees on what "the monthly cap tripped" means:
///
/// * a MONTHLY cap trip writes/extend the marker beside `ledger_path` (the
///   host-wide signal the sibling workers and the Hive worker read);
/// * any other decline is NOT host-wide and never writes (a per-call cap, an
///   unpriceable model, an unreadable ledger — D8 step 12 exists to stop the
///   PHANTOM back-off, so the marker must not be written on a guess).
///
/// Returns `Ok(Some(marker))` when the trip left a marker in force, and
/// `Ok(None)` when the host was not backed off. The call is FAIL-LOUD on I/O
/// (the caller still has its budget refusal either way) and is silent when
/// `ledger_path` is `None` (a caller with no ledger has no host to signal).
pub fn note_budget_decline(
    ledger_path: Option<&Path>,
    decline: Option<&crate::budget::BudgetDecline>,
    now: DateTime<Utc>,
) -> anyhow::Result<Option<HostCooldown>> {
    let Some(ledger_path) = ledger_path else {
        return Ok(None);
    };
    let Some(decline) = decline.filter(|d| d.monthly_cap_tripped()) else {
        return Ok(None);
    };
    let limit_usd = decline.limit_usd.unwrap_or_default();
    let spent_usd = decline.spent_usd.unwrap_or_default();
    trip_monthly_cap_cooldown(ledger_path, limit_usd, spent_usd, now).map(Some)
}

/// A live cooldown marker rendered for `zoder status` / `--json`.
///
/// Reduces the marker to the fields an operator (or a sibling worker reading
/// the JSON) needs, and states the ONE consequence in words: metered routes
/// are off, cost-neutral routes still run.
pub fn status_view(marker: &HostCooldown) -> serde_json::Value {
    serde_json::json!({
        "active": marker.is_active(),
        "reason": marker.reason,
        "detail": marker.detail,
        "limit_usd": marker.limit_usd,
        "spent_usd": marker.spent_usd,
        "created_at": marker.created_at.to_rfc3339(),
        "expires_at": marker.expires_at.to_rfc3339(),
        "effect": "metered routes skipped; falling through to cost-neutral routes",
    })
}

/// One human-readable line for `zoder status`, or `None` when `ledger_path`
/// has no live marker (or the marker cannot be read — a status command must
/// never fail because a host-local sidecar is unreadable).
pub fn status_line(ledger_path: &Path) -> Option<String> {
    let marker = read_cooldown(ledger_path).ok().flatten()?;
    let detail = if marker.detail.is_empty() {
        String::new()
    } else {
        format!(" — {}", marker.detail)
    };
    Some(format!(
        "host cooldown ACTIVE ({}){}; metered routes skipped until {}, falling through to \
         cost-neutral routes [{}]",
        marker.reason,
        detail,
        marker.expires_at.to_rfc3339(),
        cooldown_path_for(ledger_path).display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(now: DateTime<Utc>, expires: DateTime<Utc>) -> HostCooldown {
        HostCooldown {
            version: COOLDOWN_VERSION,
            reason: REASON_MONTHLY_CAP.to_string(),
            detail: "test".to_string(),
            limit_usd: Some(10.0),
            spent_usd: Some(12.0),
            created_at: now,
            expires_at: expires,
        }
    }

    #[test]
    fn marker_path_sits_beside_the_ledger() {
        let path = cooldown_path_for(Path::new("/home/u/.zoder/ledger.jsonl"));
        assert_eq!(
            path,
            PathBuf::from("/home/u/.zoder/ledger.jsonl.cooldown.json")
        );
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let now = Utc::now();
        write_cooldown(&ledger, &marker(now, now + chrono::Duration::hours(1))).unwrap();
        let got = read_cooldown(&ledger).unwrap().expect("marker is live");
        assert_eq!(got.reason, REASON_MONTHLY_CAP);
        assert_eq!(got.limit_usd, Some(10.0));
        assert_eq!(got.spent_usd, Some(12.0));
    }

    #[test]
    fn expired_marker_is_inert_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let now = Utc::now();
        write_cooldown(&ledger, &marker(now, now - chrono::Duration::seconds(1))).unwrap();
        assert!(read_cooldown(&ledger).unwrap().is_none());
        assert!(
            !cooldown_path_for(&ledger).exists(),
            "an expired marker must be removed best-effort"
        );
    }

    #[test]
    fn corrupt_marker_fails_open() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        std::fs::write(cooldown_path_for(&ledger), b"{not json").unwrap();
        assert!(
            read_cooldown(&ledger).unwrap().is_none(),
            "a corrupt marker must never park a host"
        );
    }

    /// A marker that is not even valid UTF-8 (binary garbage, a truncated
    /// write, an unrelated file at that path) must ALSO read as absent. The
    /// read used `read_to_string`, whose `InvalidData` error was propagated,
    /// so a stray binary file returned `Err` instead of `Ok(None)` — breaking
    /// the documented fail-open contract and failing every worker on the host.
    #[test]
    fn non_utf8_marker_fails_open() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        // Valid JSON prefix followed by a lone 0xFF: parses as bytes, not UTF-8.
        std::fs::write(
            cooldown_path_for(&ledger),
            b"{\"version\":1,\"reason\":\"\xff\xfe\"}",
        )
        .unwrap();
        assert!(
            read_cooldown(&ledger)
                .expect("a non-UTF8 marker must be INERT, not an error")
                .is_none(),
            "a non-UTF8 marker must never park or fail a host"
        );
    }

    #[test]
    fn unknown_version_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let now = Utc::now();
        let mut m = marker(now, now + chrono::Duration::hours(1));
        m.version = COOLDOWN_VERSION + 1;
        write_cooldown(&ledger, &m).unwrap();
        assert!(read_cooldown(&ledger).unwrap().is_none());
    }

    #[test]
    fn a_second_trip_never_shortens_a_live_marker() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let now = Utc::now();
        let long = now + chrono::Duration::hours(6);
        write_cooldown(&ledger, &marker(now, long)).unwrap();
        let shorter = now + chrono::Duration::minutes(5);
        let written = write_cooldown(&ledger, &marker(now, shorter)).unwrap();
        assert_eq!(
            written.expires_at, long,
            "a later trip must not cut the existing hold-off short"
        );
    }

    #[test]
    fn monthly_cap_trip_expires_at_the_next_month_start() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let now = Utc.with_ymd_and_hms(2026, 11, 1, 13, 50, 32).unwrap();
        let written = trip_monthly_cap_cooldown(&ledger, 10.0, 12.5, now).unwrap();
        assert_eq!(
            written.expires_at,
            Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap(),
            "a monthly cap resets at the next UTC month start"
        );
        assert_eq!(written.reason, REASON_MONTHLY_CAP);
    }

    #[test]
    fn clear_removes_the_marker() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let now = Utc::now();
        write_cooldown(&ledger, &marker(now, now + chrono::Duration::hours(1))).unwrap();
        clear_cooldown(&ledger).unwrap();
        assert!(read_cooldown(&ledger).unwrap().is_none());
        // Clearing an absent marker is not an error.
        clear_cooldown(&ledger).unwrap();
    }
}
