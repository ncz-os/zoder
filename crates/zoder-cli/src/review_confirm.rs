//! Confirm-on-reject for blocking review findings (record shape v1).
//!
//! A reviewer that returns `request_changes` on a `high` or `critical`
//! finding proves nothing on its own: one reviewer family was measured at
//! 0.78 false blocking findings per review and 12 `request_changes` on 18
//! clean cases. Its `request_changes` is noise, and its `APPROVE` is not
//! evidence either.
//!
//! This module re-checks each blocking finding with a DIFFERENT model family
//! before it may block:
//!
//! 1. **Pre-check, no model call.** A blocking finding must carry a verbatim
//!    quote that is a substring of the text the reviewer was shown. An
//!    unquoted (or mis-quoted) finding is downgraded to
//!    `filtered_z3`-style unsupported — non-blocking, still reported — and
//!    costs nothing (`quote_found = false`, zero calls).
//! 2. **Confirmer choice.** The chain comes from `[review.confirm].models`
//!    (config roles, never hard-coded ids in code). A candidate is kept only
//!    when its family differs from BOTH the reviewer's family and the
//!    author's family. `gpt-oss` never confirms a `gpt-oss` finding, and a
//!    single non-independent family never demotes on its own.
//! 3. **Confirmer input.** The finding, the diff hunk it cites, the cited
//!    file at the reviewed head windowed around the cited lines, and the
//!    definition site of every identifier the finding names (reusing the Z3
//!    name extraction), all bounded in lines.
//! 4. **Confirmer output.** Strict JSON
//!    `{finding_id, disposition: confirm|reject|insufficient, quote, reason}`.
//!    A `confirm` or `reject` must quote the supplied context verbatim; no
//!    quote means `insufficient`.
//! 5. **Aggregation.** `confirm` keeps blocking; `reject` demotes with the
//!    reason; `critical` needs two INDEPENDENT rejects before demotion, `high`
//!    needs one. `insufficient`, timeouts, `429`/`503`, empty and unparsed
//!    output keep the finding blocking and flag it unconfirmed: **a failed
//!    confirmer is never a rejection.** If no finding remains blocking, the
//!    verdict becomes `comment`, never `request_changes`.
//! 6. **Limits.** At most [`ConfirmLimits::confirmations`] confirmations per
//!    review (default 8), one call per finding per confirmer, low reasoning
//!    effort, and a 60 s timeout per call.
//!
//! The module is split into a PURE core ([`precheck`], [`aggregate`],
//! [`record`], [`parse_confirmer_output`]) and an I/O driver ([`run`]), so
//! every aggregation branch is testable offline with no model call at all.

use serde_json::{json, Value};

use zoder_core::config::ReviewConfirmConfig;

/// Severities that reach confirmation. `medium` / `low` / `info` never do.
pub(crate) fn is_blocking_severity(severity: &str) -> bool {
    matches!(
        severity.trim().to_ascii_lowercase().as_str(),
        "critical" | "high"
    )
}

/// The closed disposition list of record shape v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// A confirmer re-checked the finding and it stands: still blocking.
    Confirmed,
    /// Enough INDEPENDENT confirmers rejected it: demoted, non-blocking.
    Rejected,
    /// A confirmer ran but could not decide: still blocking, unconfirmed.
    Insufficient,
    /// A confirmer was attempted and failed (timeout/429/503/empty/unparsed):
    /// still blocking, unconfirmed. NEVER a rejection.
    ConfirmerFailed,
    /// The deterministic filter dropped it as unsupported (unquoted claim).
    FilteredZ3,
    /// Not eligible for confirmation (an `approve` verdict, a `medium`/`low`
    /// finding, or a review that ran out of confirmation budget).
    NotRun,
}

impl Disposition {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Disposition::Confirmed => "confirmed",
            Disposition::Rejected => "rejected",
            Disposition::Insufficient => "insufficient",
            Disposition::ConfirmerFailed => "confirmer_failed",
            Disposition::FilteredZ3 => "filtered_z3",
            Disposition::NotRun => "not_run",
        }
    }

    /// Parse back from the record string. An unknown value is `not_run` (the
    /// safe default: it never implies a demotion), matching how an old record
    /// with no confirm data reads.
    #[cfg(test)]
    pub(crate) fn from_str(value: &str) -> Disposition {
        match value.trim() {
            "confirmed" => Disposition::Confirmed,
            "rejected" => Disposition::Rejected,
            "insufficient" => Disposition::Insufficient,
            "confirmer_failed" => Disposition::ConfirmerFailed,
            "filtered_z3" => Disposition::FilteredZ3,
            _ => Disposition::NotRun,
        }
    }
}

/// A confirmer attempt outcome, as recorded per reviewer entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmerOutcome {
    Ok,
    Timeout,
    TooManyRequests,
    Unavailable,
    Empty,
    Unparsed,
}

impl ConfirmerOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ConfirmerOutcome::Ok => "ok",
            ConfirmerOutcome::Timeout => "timeout",
            ConfirmerOutcome::TooManyRequests => "429",
            ConfirmerOutcome::Unavailable => "503",
            ConfirmerOutcome::Empty => "empty",
            ConfirmerOutcome::Unparsed => "unparsed",
        }
    }
}

/// Limits a review runs under: the confirmation budget, the per-call timeout
/// and the line bound on confirmer context. Pure data so tests can assert
/// enforcement without a clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfirmLimits {
    /// Maximum confirmation CALLS this review may make across all findings.
    pub confirmations: usize,
    /// Per-confirmation wall-clock timeout, in seconds.
    pub timeout_s: u64,
    /// Line bound on the cited-file window and definition context.
    pub max_context_lines: usize,
    /// Independent rejects needed to demote a `critical` finding.
    pub critical_rejects: usize,
    /// Independent rejects needed to demote a `high` finding.
    pub high_rejects: usize,
}

impl Default for ConfirmLimits {
    fn default() -> Self {
        Self::from_config(&ReviewConfirmConfig::default())
    }
}

impl ConfirmLimits {
    pub(crate) fn from_config(cfg: &ReviewConfirmConfig) -> Self {
        Self {
            confirmations: cfg.confirmations_per_review(),
            timeout_s: cfg.timeout_s.max(1),
            max_context_lines: cfg.context_lines(),
            critical_rejects: cfg.rejects_required("critical"),
            high_rejects: cfg.rejects_required("high"),
        }
    }

    /// The number of INDEPENDENT rejects `severity` needs before demotion.
    /// A `critical` never demotes below two independent rejects.
    pub(crate) fn rejects_required(&self, severity: &str) -> usize {
        match severity.trim().to_ascii_lowercase().as_str() {
            "critical" => self.critical_rejects.max(2),
            _ => self.high_rejects.max(1),
        }
    }
}

/// One claim a reviewer made, in the shape the confirmation layer needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingFinding {
    /// Stable id: hash of reviewer family + file + normalised claim.
    pub id: String,
    pub severity: String,
    pub location: String,
    /// The verbatim quote the reviewer supplied, if any.
    pub quote: Option<String>,
    /// The claim text, normalised, for hashing and the confirmer prompt.
    pub claim: String,
    /// Whether the reviewer's quote is a substring of the text it was shown.
    pub quote_found: bool,
}

/// Everything a single finding's confirmation needs, resolved once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FindingPlan {
    pub finding: PendingFinding,
    /// The reviewer that raised it, and its family.
    pub reviewer_model: String,
    pub reviewer_family: String,
    /// The ordered confirmer chain to try, already family-filtered.
    pub confirmer_chain: Vec<String>,
    /// The context blob sent to the confirmer (finding + hunk + windowed
    /// file + definition sites), bounded in lines.
    pub context: String,
}

/// A single confirmer's parsed answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfirmerVerdict {
    pub finding_id: String,
    pub disposition: ParsedDisposition,
    pub quote: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParsedDisposition {
    Confirm,
    Reject,
    Insufficient,
}

/// One confirmer attempt, recorded verbatim in the review payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfirmerAttempt {
    pub model: String,
    pub family: String,
    pub outcome: ConfirmerOutcome,
    pub latency_ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// The parsed answer when `outcome == Ok`.
    pub verdict: Option<ConfirmerVerdict>,
}

/// A reviewer entry in the v1 record: the reviewer itself or one confirmer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewerEntry {
    pub model: String,
    pub family: String,
    /// `reviewer` or `confirmer`.
    pub role: &'static str,
    pub outcome: ConfirmerOutcome,
    pub latency_ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

impl ReviewerEntry {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "model": self.model,
            "family": self.family,
            "role": self.role,
            "outcome": self.outcome.as_str(),
            "latency_ms": self.latency_ms,
            "tokens_in": self.tokens_in,
            "tokens_out": self.tokens_out,
        })
    }
}

/// The aggregated decision for one finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FindingDecision {
    pub finding_id: String,
    pub severity: String,
    pub location: String,
    pub quote: Option<String>,
    pub quote_found: bool,
    pub disposition: Disposition,
    /// Still blocking after confirmation.
    pub blocking: bool,
    /// The reason recorded when a finding was demoted (empty otherwise).
    pub reason: String,
    /// Every confirmer attempt made for this finding, in order.
    pub attempts: Vec<ConfirmerAttempt>,
}

/// Which confirmers demoted a finding (record-shape v1 `demoted` entry). For
/// a `critical` this carries BOTH confirmer ids: the independent rejects that
/// were required.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Demoted {
    pub finding_id: String,
    pub severity: String,
    pub reason: String,
    /// The confirmer model ids whose rejects demoted it (both, for criticals).
    pub confirmer_ids: Vec<String>,
}

/// The confirmed verdict of a review: whether anything still blocks, what was
/// demoted, and how the verdict was reached.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConfirmedReview {
    pub decisions: Vec<FindingDecision>,
    pub demoted: Vec<Demoted>,
    /// True when at least one `high`/`critical` finding still blocks.
    pub effective_blocking: bool,
    /// Recalled (demoted) blocking findings as a share of those that reached
    /// confirmation. `None` when nothing reached confirmation.
    pub reviewer_family_recall: Option<f64>,
}

impl ConfirmedReview {
    /// `reviewer_plus_confirmer` when any finding was actually confirmed or
    /// demoted (i.e. at least one confirmer ran, or the deterministic
    /// pre-check dropped a finding), else `reviewer_only`.
    pub(crate) fn verdict_basis(&self) -> &'static str {
        let touched = self
            .decisions
            .iter()
            .any(|d| d.disposition != Disposition::NotRun || !d.attempts.is_empty());
        if touched {
            "reviewer_plus_confirmer"
        } else {
            "reviewer_only"
        }
    }

    /// The verdict implied by the confirmed findings. A review whose findings
    /// all demoted (or which raised none) is `comment`, never
    /// `request_changes`; only a finding that still blocks keeps
    /// `request_changes`.
    pub(crate) fn effective_verdict(&self) -> &'static str {
        if self.effective_blocking {
            "request_changes"
        } else {
            "comment"
        }
    }
}

// ---------------------------------------------------------------------------
// Normalisation and the stable finding id.
// ---------------------------------------------------------------------------

/// Whitespace-collapsed, case-folded claim text, used for the finding id and
/// for the confirmer prompt. Deleting or adding whitespace in the same claim
/// must not change the id.
pub(crate) fn normalise_claim(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// FNV-1a 64 over the family + file + normalised claim. A stable, dependency
/// free hash: the record must name the SAME finding for the same input across
/// processes, so a re-raise is recognisable and a demoted finding is auditable.
pub(crate) fn finding_id(family: &str, path: &str, claim: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let parts: [&str; 3] = [family.trim(), path.trim(), ""];
    let normalised = normalise_claim(claim);
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash ^= 0x1f;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for part in parts.iter().take(2) {
        feed(part.as_bytes());
    }
    feed(normalised.as_bytes());
    format!("{hash:016x}")
}

// ---------------------------------------------------------------------------
// Pre-check: the verbatim-quote requirement, with no model call.
// ---------------------------------------------------------------------------

/// Whether `quote` appears verbatim in `shown`. Whitespace at the edges is
/// trimmed; internal bytes must match exactly, so a paraphrase is not a quote.
pub(crate) fn quote_in(quote: &str, shown: &str) -> bool {
    let needle = quote.trim();
    if needle.is_empty() {
        return false;
    }
    shown.contains(needle)
}

/// The pre-check result for one finding: whether it may block at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Precheck {
    pub finding_id: String,
    pub quote_found: bool,
    /// `true` when the finding is dropped from blocking (unquoted), i.e. the
    /// Z3-style unsupported downgrade. NOT a confirmer result.
    pub dropped: bool,
}

/// Pre-check every finding that would otherwise block against the text the
/// reviewer was shown.
///
/// A finding whose quote is absent or is not a substring is dropped from
/// blocking and recorded as `filtered_z3`; **no model call is ever made for
/// it** (it is absent from the returned pending set). A finding with a valid
/// quote is returned as still pending confirmation.
pub(crate) fn precheck(findings: &[PendingFinding]) -> (Vec<Precheck>, Vec<PendingFinding>) {
    let mut checks = Vec::with_capacity(findings.len());
    let mut pending = Vec::with_capacity(findings.len());
    for finding in findings {
        let dropped = !finding.quote_found;
        checks.push(Precheck {
            finding_id: finding.id.clone(),
            quote_found: finding.quote_found,
            dropped,
        });
        if !dropped {
            pending.push(finding.clone());
        }
    }
    (checks, pending)
}

// ---------------------------------------------------------------------------
// Confirmer output parsing.
// ---------------------------------------------------------------------------

/// Extract the outermost `{...}` object from a confirmer reply and parse it.
fn parse_object(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<Value>(&trimmed[start..=end]).ok()
}

/// Parse a confirmer's raw output into its disposition, enforcing the "no
/// quote means insufficient" rule.
///
/// Returns `None` when the reply is not structured JSON at all (the caller
/// records `unparsed`). A structurally valid reply with an empty or unknown
/// disposition, an empty quote, or a quote that is not in the context reads
/// as [`ParsedDisposition::Insufficient`] — the safe default that keeps the
/// finding blocking. A reply about a DIFFERENT `finding_id` is also
/// insufficient: it can never demote the wrong claim.
pub(crate) fn parse_confirmer_output(
    raw: &str,
    expected_id: &str,
    context: &str,
) -> Option<ConfirmerVerdict> {
    let value = parse_object(raw)?;
    let finding_id = value
        .get("finding_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let object_id_ok = finding_id.is_empty() || finding_id == expected_id;
    let disposition_raw = value
        .get("disposition")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let quote = value
        .get("quote")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_string);
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    let disposition = match disposition_raw.as_str() {
        "confirm" | "reject" => match quote.as_deref() {
            // A confirm or reject must quote the context verbatim; without a
            // quote (or with a quote that is not in the context) the answer
            // is insufficient, and the finding keeps blocking.
            Some(q) if object_id_ok && quote_in(q, context) => {
                if disposition_raw == "confirm" {
                    ParsedDisposition::Confirm
                } else {
                    ParsedDisposition::Reject
                }
            }
            _ => ParsedDisposition::Insufficient,
        },
        // An explicit `insufficient`, or any unrecognised disposition, keeps
        // the finding blocking.
        _ => ParsedDisposition::Insufficient,
    };
    Some(ConfirmerVerdict {
        finding_id: if finding_id.is_empty() {
            expected_id.to_string()
        } else {
            finding_id.to_string()
        },
        disposition,
        quote,
        reason,
    })
}

// ---------------------------------------------------------------------------
// Confirmer selection: one call per finding per confirmer.
// ---------------------------------------------------------------------------

/// Filter the configured confirmer chain down to the candidates allowed to
/// confirm a given finding, EXCLUDING confirmers already used on it.
///
/// Rules (spec §3, §7):
/// * a candidate in the reviewer's family is never used — this is what stops
///   `gpt-oss` confirming a `gpt-oss` finding;
/// * a candidate in any author family is never used;
/// * a candidate already used on THIS finding (in `already_used`) is dropped,
///   so a finding gets at most ONE call per confirmer across all attempts.
///
/// Order is preserved from config. `family_of` resolves a configured model id
/// (or role) to its family; `None` means the candidate cannot be resolved and
/// is skipped. An empty result means no confirmer can run, and the finding
/// keeps blocking (unconfirmed).
///
/// The family isolation is UNCONDITIONAL. There is deliberately no flag to
/// relax it: a confirmer from the reviewer's family, or from an author's
/// family, is never a valid second opinion, so the rule cannot be switched
/// off by a caller.
pub(crate) fn select_confirmers(
    chain: &[String],
    reviewer_family: &str,
    author_families: &[String],
    already_used: &[String],
    family_of: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for candidate in chain {
        // One call per finding per confirmer: never re-offer a model that has
        // already run for this finding, and never list it twice.
        if out.iter().any(|m| m == candidate) || already_used.iter().any(|m| m == candidate) {
            continue;
        }
        let Some(family) = family_of(candidate) else {
            continue;
        };
        // Never the reviewer's own family (this is what stops `gpt-oss`
        // confirming a `gpt-oss` finding), never an author family.
        if family.eq_ignore_ascii_case(reviewer_family)
            || author_families
                .iter()
                .any(|a| a.eq_ignore_ascii_case(&family))
        {
            continue;
        }
        out.push(candidate.clone());
    }
    out
}

// ---------------------------------------------------------------------------
// Aggregation.
// ---------------------------------------------------------------------------

/// Aggregate the confirmation results into a review decision.
///
/// `jobs` are processed in order; `limits.confirmations` bounds the total
/// number of confirmer CALLS (not findings). A confirmer already attempted on
/// a finding is never offered again (one call per finding per confirmer),
/// until either the finding is decided or the budget runs out. A blocking
/// finding with no call left is `not_run` and stays blocking.
///
/// `run_confirmer` is the injected I/O: given a confirmer model and the
/// finding plan it returns one attempt. The pure core never calls a model
/// itself, so the whole matrix is unit-testable offline.
pub(crate) fn aggregate(
    jobs: &[ConfirmationJob],
    limits: &ConfirmLimits,
    run_confirmer: &mut dyn FnMut(&str, &FindingPlan) -> ConfirmerAttempt,
) -> ConfirmedReview {
    let mut decisions = Vec::with_capacity(jobs.len());
    let mut demoted = Vec::new();
    let mut budget = limits.confirmations;
    let mut reached_confirmation = 0usize;
    let mut demoted_count = 0usize;

    for job in jobs {
        let severity = job.plan.finding.severity.clone();
        let requires = limits.rejects_required(&severity);
        let mut attempts: Vec<ConfirmerAttempt> = Vec::new();
        let mut used: Vec<String> = Vec::new();

        for confirmer in &job.confirmers {
            // Decide as soon as the finding is settled, and never overrun the
            // per-review budget.
            if budget == 0 {
                break;
            }
            // One call per finding per confirmer. `select_confirmers` is the
            // single source of truth for that rule: a confirmer already used
            // on THIS finding is never offered again.
            let remaining = select_confirmers(
                std::slice::from_ref(confirmer),
                &job.plan.reviewer_family,
                &[],
                &used,
                &|_m| Some(String::new()),
            );
            if remaining.is_empty() {
                continue;
            }
            // Stop once the finding is decided: a confirm settles it, and
            // enough independent rejects demote it.
            let decided = attempts.iter().any(|a| {
                a.outcome == ConfirmerOutcome::Ok
                    && a.verdict
                        .as_ref()
                        .is_some_and(|v| v.disposition == ParsedDisposition::Confirm)
            }) || independent_rejects(&attempts) >= requires;
            if decided {
                break;
            }
            budget -= 1;
            used.push(confirmer.clone());
            attempts.push(run_confirmer(confirmer, &job.plan));
        }

        // Walk the recorded attempts (exactly what the record shows) to
        // derive the disposition.
        let mut confirmed = false;
        let mut insufficient_seen = false;
        let mut failure_seen = false;
        for attempt in &attempts {
            match attempt.outcome {
                ConfirmerOutcome::Ok => match attempt.verdict.as_ref().map(|v| v.disposition) {
                    Some(ParsedDisposition::Confirm) => confirmed = true,
                    Some(ParsedDisposition::Reject) => {}
                    Some(ParsedDisposition::Insufficient) | None => insufficient_seen = true,
                },
                // timeout / 429 / 503 / empty / unparsed: NEVER a rejection.
                _ => failure_seen = true,
            }
        }
        let reject_ids = reject_models(&attempts);
        let enough_rejects = independent_rejects(&attempts) >= requires;
        // Only a finding that a confirmer actually looked at "reached
        // confirmation"; a finding with no attempt (empty chain, budget
        // exhausted) stays out of the recall denominator.
        if !attempts.is_empty() {
            reached_confirmation += 1;
        }

        let (disposition, blocking) = if confirmed {
            (Disposition::Confirmed, true)
        } else if enough_rejects {
            (Disposition::Rejected, false)
        } else if failure_seen {
            // A failed confirmer keeps the finding blocking and flags it
            // unconfirmed. It is NEVER a rejection.
            (Disposition::ConfirmerFailed, true)
        } else if insufficient_seen || !attempts.is_empty() {
            // A confirmer ran but the thresholds were not met (e.g. one reject
            // on a critical). Undecided, so it keeps blocking and is recorded
            // insufficient — a confirmer DID run, so this is not `not_run`.
            (Disposition::Insufficient, true)
        } else {
            // Nothing ran: no confirmer could be resolved, or the review ran
            // out of confirmation budget. The finding keeps blocking and is
            // unconfirmed — never demoted.
            (Disposition::NotRun, true)
        };

        let reason = if disposition == Disposition::Rejected {
            demoted_count += 1;
            let reason = attempts
                .iter()
                .filter_map(|a| a.verdict.as_ref())
                .map(|v| v.reason.clone())
                .find(|r| !r.is_empty())
                .unwrap_or_else(|| "confirmer rejected the finding".to_string());
            demoted.push(Demoted {
                finding_id: job.plan.finding.id.clone(),
                severity: severity.clone(),
                reason: reason.clone(),
                confirmer_ids: reject_ids,
            });
            reason
        } else {
            String::new()
        };

        decisions.push(FindingDecision {
            finding_id: job.plan.finding.id.clone(),
            severity: job.plan.finding.severity.clone(),
            location: job.plan.finding.location.clone(),
            quote: job.plan.finding.quote.clone(),
            quote_found: job.plan.finding.quote_found,
            disposition,
            blocking,
            reason,
            attempts,
        });
    }

    // A review blocks only when a finding STILL BLOCKS. Every blocking
    // disposition (`confirmed`, `insufficient`, `confirmer_failed`, `not_run`)
    // sets `blocking = true`; a demoted (`rejected`) finding does not. The
    // flag is the plain OR of the per-finding booleans, so there is no second
    // rule that could disagree with the record.
    let effective_blocking = decisions.iter().any(|d| d.blocking);
    let reviewer_family_recall = if reached_confirmation == 0 {
        None
    } else {
        Some(demoted_count as f64 / reached_confirmation as f64)
    };
    ConfirmedReview {
        decisions,
        demoted,
        effective_blocking,
        reviewer_family_recall,
    }
}

/// The distinct reject confirmer model ids, in attempt order.
fn reject_models(attempts: &[ConfirmerAttempt]) -> Vec<String> {
    let mut out = Vec::new();
    for attempt in attempts {
        if attempt.outcome == ConfirmerOutcome::Ok
            && attempt
                .verdict
                .as_ref()
                .is_some_and(|v| v.disposition == ParsedDisposition::Reject)
        {
            out.push(attempt.model.clone());
        }
    }
    out
}

/// The number of INDEPENDENT rejects: a reject whose family has not already
/// rejected this finding. Two rejects from the SAME family count once, so a
/// `critical` is only demoted by two DIFFERENT families.
fn independent_rejects(attempts: &[ConfirmerAttempt]) -> usize {
    let mut families: Vec<String> = Vec::new();
    for attempt in attempts {
        let is_reject = attempt.outcome == ConfirmerOutcome::Ok
            && attempt
                .verdict
                .as_ref()
                .is_some_and(|v| v.disposition == ParsedDisposition::Reject);
        if is_reject && !families.iter().any(|f| f == &attempt.family) {
            families.push(attempt.family.clone());
        }
    }
    families.len()
}

/// One planned confirmation: the finding plus the confirmers to run for it.
#[derive(Debug, Clone)]
pub(crate) struct ConfirmationJob {
    pub plan: FindingPlan,
    /// The confirmers to try, in order, already filtered to a different
    /// family from the reviewer's and the authors'.
    pub confirmers: Vec<String>,
}

// ---------------------------------------------------------------------------
// Record shape v1.
// ---------------------------------------------------------------------------

/// Render the confirm-on-reject section of the review payload (record shape
/// v1). Additive: an old record with no `confirm` key still parses
/// ([`parse_record`]), and `reviewers[0]` is always the first reviewer so the
/// dashboard's view is unchanged.
///
/// Per finding the record carries `id`, `severity`, `location`, `quote`,
/// `quote_found` and `disposition` (the closed list). Per reviewer entry it
/// carries `model`, `family`, `role`, `outcome`, `latency_ms`, `tokens_in`,
/// `tokens_out`. At review level: `effective_blocking`, `demoted` (with both
/// confirmer ids for a critical), `verdict_basis`, `verdict`, and
/// `reviewer_family_recall_ref`.
pub(crate) fn record(
    confirmed: &ConfirmedReview,
    reviewer_model: &str,
    reviewer_family: &str,
    verdict: &str,
    reviewer_latency_ms: u64,
    reviewer_tokens_in: u64,
    reviewer_tokens_out: u64,
) -> Value {
    // reviewers[0] is the reviewer, so the dashboard keeps its anchor. Every
    // confirmer attempt follows, in finding order.
    let mut reviewers: Vec<Value> = Vec::new();
    reviewers.push(
        ReviewerEntry {
            model: reviewer_model.to_string(),
            family: reviewer_family.to_string(),
            role: "reviewer",
            outcome: ConfirmerOutcome::Ok,
            latency_ms: reviewer_latency_ms,
            tokens_in: reviewer_tokens_in,
            tokens_out: reviewer_tokens_out,
        }
        .to_json(),
    );
    for decision in &confirmed.decisions {
        for attempt in &decision.attempts {
            reviewers.push(
                ReviewerEntry {
                    model: attempt.model.clone(),
                    family: attempt.family.clone(),
                    role: "confirmer",
                    outcome: attempt.outcome,
                    latency_ms: attempt.latency_ms,
                    tokens_in: attempt.tokens_in,
                    tokens_out: attempt.tokens_out,
                }
                .to_json(),
            );
        }
    }

    let findings: Vec<Value> = confirmed
        .decisions
        .iter()
        .map(|decision| {
            json!({
                "id": decision.finding_id,
                "severity": decision.severity,
                "location": decision.location,
                "quote": decision.quote,
                "quote_found": decision.quote_found,
                "disposition": decision.disposition.as_str(),
                "blocking": decision.blocking,
                "reason": decision.reason,
            })
        })
        .collect();

    let demoted: Vec<Value> = confirmed
        .demoted
        .iter()
        .map(|d| {
            json!({
                "id": d.finding_id,
                "severity": d.severity,
                "reason": d.reason,
                "confirmer_ids": d.confirmer_ids,
            })
        })
        .collect();

    json!({
        "version": 1,
        "effective_blocking": confirmed.effective_blocking,
        "demoted": demoted,
        "verdict_basis": confirmed.verdict_basis(),
        "verdict": verdict,
        "reviewer_family_recall_ref": confirmed.reviewer_family_recall,
        "findings": findings,
        "reviewers": reviewers,
    })
}

/// A parsed confirm section, for the round-trip test and any consumer that
/// reads the record back.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedRecord {
    pub version: u64,
    pub effective_blocking: bool,
    pub verdict_basis: String,
    pub verdict: String,
    pub findings: Vec<ParsedFinding>,
    pub demoted: Vec<ParsedDemoted>,
    pub reviewer_count: usize,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedFinding {
    pub id: String,
    pub severity: String,
    pub location: String,
    pub quote: Option<String>,
    pub quote_found: bool,
    pub disposition: Disposition,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedDemoted {
    pub id: String,
    pub severity: String,
    pub confirmer_ids: Vec<String>,
}

/// Parse a confirm record produced by [`record`].
///
/// An OLD record (a review payload with no `confirm` section, i.e. no
/// `version` field) parses to an empty, `reviewer_only` record rather than
/// failing: `version` defaults to 0, `findings`/`demoted` to empty, and the
/// verdict is taken from the payload if present.
#[cfg(test)]
pub(crate) fn parse_record(value: &Value) -> ParsedRecord {
    let version = value.get("version").and_then(Value::as_u64).unwrap_or(0);
    let effective_blocking = value
        .get("effective_blocking")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let verdict_basis = value
        .get("verdict_basis")
        .and_then(Value::as_str)
        .unwrap_or("reviewer_only")
        .to_string();
    let verdict = value
        .get("verdict")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let findings = value
        .get("findings")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|f| ParsedFinding {
                    id: f
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    severity: f
                        .get("severity")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    location: f
                        .get("location")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    quote: f.get("quote").and_then(Value::as_str).map(str::to_string),
                    quote_found: f
                        .get("quote_found")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    disposition: f
                        .get("disposition")
                        .and_then(Value::as_str)
                        .map(Disposition::from_str)
                        .unwrap_or(Disposition::NotRun),
                })
                .collect()
        })
        .unwrap_or_default();
    let demoted = value
        .get("demoted")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .map(|d| ParsedDemoted {
                    id: d
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    severity: d
                        .get("severity")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    confirmer_ids: d
                        .get("confirmer_ids")
                        .and_then(Value::as_array)
                        .map(|ids| {
                            ids.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();
    let reviewer_count = value
        .get("reviewers")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    ParsedRecord {
        version,
        effective_blocking,
        verdict_basis,
        verdict,
        findings,
        demoted,
        reviewer_count,
    }
}

// ---------------------------------------------------------------------------
// Confirmer context.
// ---------------------------------------------------------------------------

/// Build the bounded context blob sent to a confirmer: the finding, the diff
/// hunk it cites, the cited file at the reviewed head windowed around the
/// cited lines, and the definition site of every identifier the claim names.
///
/// `read` returns the post-change text of a repo-relative path. `hunk` is the
/// diff text the reviewer saw. Everything is bounded to `max_lines` lines so
/// one pathological finding can never blow up the prompt or the budget.
pub(crate) fn build_context(
    plan: &PendingFinding,
    hunk: &str,
    read: &dyn Fn(&str) -> Option<String>,
    max_lines: usize,
) -> String {
    let max_lines = max_lines.max(1);
    let mut out = String::new();
    out.push_str("Finding under confirmation:\n");
    out.push_str(&format!("- severity: {}\n", plan.severity));
    if !plan.location.is_empty() {
        out.push_str(&format!("- location: {}\n", plan.location));
    }
    if let Some(quote) = plan.quote.as_deref().filter(|q| !q.trim().is_empty()) {
        out.push_str(&format!("- reviewer quote: {}\n", quote.trim()));
    }
    out.push_str(&format!("- claim: {}\n", plan.claim));

    out.push_str("\nDiff hunk (the reviewer saw this):\n");
    for line in hunk.lines().take(max_lines) {
        out.push_str(line);
        out.push('\n');
    }

    if let Some(window) = cited_window(plan, read, max_lines) {
        out.push_str("\nCited file at the reviewed head (windowed around the cited line):\n");
        out.push_str(&window);
    }
    if let Some(defs) = definition_sites(plan, read, max_lines) {
        out.push_str("\nDefinition sites of identifiers the finding names:\n");
        out.push_str(&defs);
    }
    out
}

/// The cited file window: `max_lines` lines centred on the cited line number.
fn cited_window(
    plan: &PendingFinding,
    read: &dyn Fn(&str) -> Option<String>,
    max_lines: usize,
) -> Option<String> {
    let (path, line) = split_location(&plan.location)?;
    let text = read(&path)?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let centre = line.unwrap_or(1).clamp(1, lines.len());
    let half = (max_lines / 2).max(1);
    let start = centre.saturating_sub(half).saturating_sub(1);
    let end = (start + max_lines).min(lines.len());
    let mut out = String::new();
    for (idx, text) in lines.iter().enumerate().take(end).skip(start) {
        out.push_str(&format!("{:>6}: {}\n", idx + 1, text));
    }
    Some(out)
}

/// Definition site of every identifier the claim names, reusing the Z3 name
/// extraction ([`crate::review_filter::claimed_symbols`]).
fn definition_sites(
    plan: &PendingFinding,
    read: &dyn Fn(&str) -> Option<String>,
    max_lines: usize,
) -> Option<String> {
    let (path, _) = split_location(&plan.location)?;
    let text = read(&path)?;
    let symbols = crate::review_filter::claimed_symbols(&plan.claim, &plan.claim);
    if symbols.is_empty() {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::new();
    for symbol in symbols.iter().take(8) {
        for (idx, line) in lines.iter().enumerate().take(max_lines) {
            if defines_or_imports(line, symbol) {
                out.push_str(&format!("- {}:{}: {}\n", path, idx + 1, line.trim()));
                break;
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Whether `line` defines or imports `symbol`.
fn defines_or_imports(line: &str, symbol: &str) -> bool {
    if crate::review_context::imported_names(line)
        .iter()
        .any(|name| name == symbol)
    {
        return true;
    }
    if crate::review_diff::extract_defs(line)
        .into_iter()
        .any(|name| name == symbol)
    {
        return true;
    }
    crate::review_context::const_name(line).is_some_and(|name| name == symbol)
}

/// Split a `path:line` location into the repo-relative path and the line.
pub(crate) fn split_location(location: &str) -> Option<(String, Option<usize>)> {
    let trimmed = location.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rsplit_once(':') {
        Some((path, tail)) => match tail.trim().split('-').next().and_then(|n| n.parse().ok()) {
            Some(line) if !path.trim().is_empty() => Some((path.trim().to_string(), Some(line))),
            _ => Some((trimmed.to_string(), None)),
        },
        None => Some((trimmed.to_string(), None)),
    }
}

/// The strict-JSON instruction appended to a confirmer prompt.
pub(crate) const CONFIRMER_INSTRUCTION: &str = "You are a second-opinion confirmer. A reviewer raised the finding above. \
Decide whether it is real. Answer with ONE JSON object only, no prose, of the form \
{\"finding_id\": \"<the id above>\", \"disposition\": \"confirm\" | \"reject\" | \"insufficient\", \"quote\": \"<verbatim text copied from the context above>\", \"reason\": \"<one sentence>\"}. \
A confirm or reject MUST include a quote copied verbatim from the context; without one your answer counts as insufficient. \
Use \"insufficient\" when the context does not let you decide. \\
finding-id: {FINDING_ID}";

// ---------------------------------------------------------------------------
// I/O driver: perform the async confirmer calls the aggregator plans.
// ---------------------------------------------------------------------------

/// One planned confirmer call: (finding id, confirmer model, finding plan).
pub(crate) fn plan_calls(
    jobs: &[ConfirmationJob],
    limits: &ConfirmLimits,
) -> Vec<(String, String, FindingPlan)> {
    let mut calls = Vec::new();
    let mut budget = limits.confirmations;
    for job in jobs {
        let requires = limits.rejects_required(&job.plan.finding.severity);
        let mut used: Vec<String> = Vec::new();
        let mut attempts: Vec<ConfirmerAttempt> = Vec::new();
        for confirmer in &job.confirmers {
            if budget == 0 {
                break;
            }
            if used.iter().any(|m| m == confirmer) {
                continue;
            }
            let decided = attempts.iter().any(|a| {
                a.outcome == ConfirmerOutcome::Ok
                    && a.verdict
                        .as_ref()
                        .is_some_and(|v| v.disposition == ParsedDisposition::Confirm)
            }) || independent_rejects(&attempts) >= requires;
            if decided {
                break;
            }
            budget -= 1;
            used.push(confirmer.clone());
            attempts.push(ConfirmerAttempt {
                model: confirmer.clone(),
                family: String::new(),
                outcome: ConfirmerOutcome::Empty,
                latency_ms: 0,
                tokens_in: 0,
                tokens_out: 0,
                verdict: None,
            });
            calls.push((
                job.plan.finding.id.clone(),
                confirmer.clone(),
                job.plan.clone(),
            ));
        }
    }
    calls
}

/// Run the planned confirmer calls concurrently through `call`, honouring the
/// per-call timeout, then aggregate. A call that exceeds `limits.timeout_s`
/// records a `Timeout` attempt; the finding keeps blocking.
pub(crate) async fn run<F, Fut>(
    jobs: &[ConfirmationJob],
    limits: &ConfirmLimits,
    mut call: F,
) -> ConfirmedReview
where
    F: FnMut(String, FindingPlan) -> Fut,
    Fut: std::future::Future<Output = ConfirmerAttempt>,
{
    use std::collections::HashMap;
    let calls = plan_calls(jobs, limits);
    let mut results: Vec<(String, String, ConfirmerAttempt)> = Vec::with_capacity(calls.len());
    for (finding_id, model, plan) in calls {
        let attempt = match tokio::time::timeout(
            std::time::Duration::from_secs(limits.timeout_s.max(1)),
            call(model.clone(), plan),
        )
        .await
        {
            Ok(attempt) => attempt,
            Err(_) => ConfirmerAttempt {
                model: model.clone(),
                family: String::new(),
                outcome: ConfirmerOutcome::Timeout,
                latency_ms: limits.timeout_s.saturating_mul(1_000),
                tokens_in: 0,
                tokens_out: 0,
                verdict: None,
            },
        };
        results.push((finding_id, model, attempt));
    }

    let mut by_key: HashMap<(String, String), ConfirmerAttempt> = HashMap::new();
    for (finding_id, model, attempt) in results {
        by_key.insert((finding_id, model), attempt);
    }
    aggregate(jobs, limits, &mut |model, plan| {
        by_key
            .remove(&(plan.finding.id.clone(), model.to_string()))
            .unwrap_or_else(|| ConfirmerAttempt {
                model: model.to_string(),
                family: String::new(),
                outcome: ConfirmerOutcome::Timeout,
                latency_ms: 0,
                tokens_in: 0,
                tokens_out: 0,
                verdict: None,
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- helpers ---------------------------------------------------------

    const CTX: &str = "fn f() { let x = 1; }";

    fn finding(sev: &str, claim: &str, quote: Option<&str>, shown: &str) -> PendingFinding {
        let quote = quote.map(str::to_string);
        let quote_found = quote_in(quote.as_deref().unwrap_or(""), shown);
        PendingFinding {
            id: finding_id("fam", "src/a.rs", claim),
            severity: sev.to_string(),
            location: "src/a.rs:1".to_string(),
            quote,
            claim: claim.to_string(),
            quote_found,
        }
    }

    fn plan(f: PendingFinding, chain: &[&str]) -> FindingPlan {
        FindingPlan {
            finding: f,
            reviewer_model: "rev".into(),
            reviewer_family: "fam".into(),
            confirmer_chain: chain.iter().map(|s| s.to_string()).collect(),
            context: CTX.to_string(),
        }
    }

    fn job(f: PendingFinding, chain: &[&str]) -> ConfirmationJob {
        ConfirmationJob {
            plan: plan(f, chain),
            confirmers: chain.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// An attempt that answered `disposition` with a verbatim quote.
    fn attempt(model: &str, family: &str, d: ParsedDisposition) -> ConfirmerAttempt {
        ConfirmerAttempt {
            model: model.into(),
            family: family.into(),
            outcome: ConfirmerOutcome::Ok,
            latency_ms: 5,
            tokens_in: 10,
            tokens_out: 3,
            verdict: Some(ConfirmerVerdict {
                finding_id: String::new(),
                disposition: d,
                quote: Some("let x".into()),
                reason: format!("{model} says {:?}", d).to_lowercase(),
            }),
        }
    }

    fn failed(model: &str, family: &str, outcome: ConfirmerOutcome) -> ConfirmerAttempt {
        ConfirmerAttempt {
            model: model.into(),
            family: family.into(),
            outcome,
            latency_ms: 60_000,
            tokens_in: 10,
            tokens_out: 0,
            verdict: None,
        }
    }

    fn limits() -> ConfirmLimits {
        ConfirmLimits::default()
    }

    // ---- pre-check: unquoted finding drops with ZERO model calls ---------

    #[test]
    fn precheck_drops_unquoted_finding_without_a_model_call() {
        let quoted = finding("high", "x is unused", Some("let x"), CTX);
        let unquoted = finding("critical", "y is wrong", Some("not in the diff"), CTX);
        let (checks, pending) = precheck(&[quoted.clone(), unquoted.clone()]);
        assert!(checks[0].quote_found);
        assert!(!checks[0].dropped);
        assert!(!checks[1].quote_found);
        assert!(checks[1].dropped, "an unquoted finding must drop");
        // Only the quoted finding may reach a confirmer.
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, quoted.id);

        // And end to end: building jobs from the pending set and aggregating
        // makes NO call for the dropped one.
        let mut calls = 0usize;
        let jobs: Vec<ConfirmationJob> = pending.iter().map(|p| job(p.clone(), &["c1"])).collect();
        let review = aggregate(&jobs, &limits(), &mut |m, _p| {
            calls += 1;
            attempt(m, "other", ParsedDisposition::Confirm)
        });
        assert_eq!(calls, 1, "zero calls may be made for the dropped finding");
        assert!(review.effective_blocking);
        let _ = unquoted;
    }

    #[test]
    fn precheck_missing_quote_also_drops() {
        let f = finding("high", "no quote at all", None, CTX);
        let (checks, pending) = precheck(&[f]);
        assert!(checks[0].dropped);
        assert!(pending.is_empty());
    }

    // ---- aggregation branches -------------------------------------------

    #[test]
    fn confirm_keeps_blocking() {
        let j = job(finding("high", "c", Some("let x"), CTX), &["c1", "c2"]);
        let review = aggregate(&[j], &limits(), &mut |m, _| {
            attempt(m, "other", ParsedDisposition::Confirm)
        });
        assert!(review.effective_blocking);
        assert_eq!(review.decisions[0].disposition, Disposition::Confirmed);
        assert_eq!(review.effective_verdict(), "request_changes");
    }

    #[test]
    fn one_high_reject_demotes() {
        let j = job(finding("high", "h", Some("let x"), CTX), &["c1"]);
        let review = aggregate(&[j], &limits(), &mut |m, _| {
            attempt(m, "other", ParsedDisposition::Reject)
        });
        assert!(!review.effective_blocking);
        assert_eq!(review.decisions[0].disposition, Disposition::Rejected);
        assert_eq!(review.effective_verdict(), "comment");
        assert_eq!(review.demoted.len(), 1);
        assert_eq!(review.demoted[0].confirmer_ids, vec!["c1".to_string()]);
    }

    #[test]
    fn one_critical_reject_does_not_demote() {
        let j = job(finding("critical", "cr", Some("let x"), CTX), &["c1"]);
        let review = aggregate(&[j], &limits(), &mut |m, _| {
            attempt(m, "other", ParsedDisposition::Reject)
        });
        // Only one reject: a critical needs two independent ones.
        assert!(review.effective_blocking);
        assert_eq!(review.decisions[0].disposition, Disposition::Insufficient);
        assert_eq!(review.effective_verdict(), "request_changes");
    }

    #[test]
    fn two_independent_critical_rejects_demote() {
        let j = job(
            finding("critical", "cr2", Some("let x"), CTX),
            &["c1", "c2"],
        );
        let review = aggregate(&[j], &limits(), &mut |m, _| {
            let fam = if m == "c1" { "famA" } else { "famB" };
            attempt(m, fam, ParsedDisposition::Reject)
        });
        assert!(!review.effective_blocking);
        assert_eq!(review.decisions[0].disposition, Disposition::Rejected);
        assert_eq!(review.demoted.len(), 1);
        // Both confirmer ids are recorded for a critical.
        assert_eq!(
            review.demoted[0].confirmer_ids,
            vec!["c1".to_string(), "c2".to_string()]
        );
    }

    #[test]
    fn second_critical_reject_from_same_family_does_not_count() {
        // Two rejects, but the same family: not independent, so no demotion.
        let j = job(
            finding("critical", "cr3", Some("let x"), CTX),
            &["c1", "c2"],
        );
        let review = aggregate(&[j], &limits(), &mut |m, _| {
            attempt(m, "same", ParsedDisposition::Reject)
        });
        assert!(review.effective_blocking);
        assert_ne!(review.decisions[0].disposition, Disposition::Rejected);
    }

    #[test]
    fn insufficient_timeout_429_503_empty_unparsed_all_keep_blocking() {
        for outcome in [
            ConfirmerOutcome::Timeout,
            ConfirmerOutcome::TooManyRequests,
            ConfirmerOutcome::Unavailable,
            ConfirmerOutcome::Empty,
            ConfirmerOutcome::Unparsed,
        ] {
            let j = job(finding("high", "f", Some("let x"), CTX), &["c1"]);
            let review = aggregate(&[j], &limits(), &mut |m, _| failed(m, "other", outcome));
            assert!(
                review.effective_blocking,
                "{:?} must keep the finding blocking",
                outcome
            );
            assert_eq!(
                review.decisions[0].disposition,
                Disposition::ConfirmerFailed,
                "{:?} must be recorded confirmer_failed",
                outcome
            );
            // A failed confirmer is NEVER a demotion.
            assert!(review.demoted.is_empty());
            assert_eq!(review.effective_verdict(), "request_changes");
        }
    }

    #[test]
    fn insufficient_verdict_keeps_blocking() {
        let j = job(finding("high", "i", Some("let x"), CTX), &["c1"]);
        let review = aggregate(&[j], &limits(), &mut |m, _| {
            attempt(m, "other", ParsedDisposition::Insufficient)
        });
        assert!(review.effective_blocking);
        assert_eq!(review.decisions[0].disposition, Disposition::Insufficient);
    }

    #[test]
    fn unconfirmed_findings_never_yield_non_request_changes() {
        // A mix: one confirmed, one failed. The review must still block.
        let jobs = vec![
            job(finding("high", "a", Some("let x"), CTX), &["c1"]),
            job(finding("critical", "b", Some("let x"), CTX), &["c1", "c2"]),
        ];
        let review = aggregate(&jobs, &limits(), &mut |m, _| {
            if m == "c1" {
                attempt(m, "f1", ParsedDisposition::Confirm)
            } else {
                failed(m, "f2", ConfirmerOutcome::Timeout)
            }
        });
        assert!(review.effective_blocking);
        assert_eq!(review.effective_verdict(), "request_changes");
    }

    // ---- limits ---------------------------------------------------------

    #[test]
    fn eight_finding_cap_is_enforced() {
        // Nine distinct findings, one confirmer each: only eight calls.
        let jobs: Vec<ConfirmationJob> = (0..9)
            .map(|i| {
                let claim = format!("claim number {i}");
                job(finding("high", &claim, Some("let x"), CTX), &["c1"])
            })
            .collect();
        let mut calls = 0usize;
        let review = aggregate(&jobs, &limits(), &mut |m, _| {
            calls += 1;
            attempt(m, "other", ParsedDisposition::Confirm)
        });
        assert_eq!(limits().confirmations, 8);
        assert_eq!(calls, 8, "at most 8 confirmations per review");
        // The ninth finding was never confirmed: recorded not_run, still blocks.
        assert_eq!(review.decisions[8].disposition, Disposition::NotRun);
        assert!(review.decisions[8].blocking);
    }

    #[test]
    fn recall_denominator_counts_only_findings_that_reached_confirmation() {
        // Two findings: one has a confirmer (rejects -> demoted), one has an
        // empty chain (never reached a confirmer). The recall denominator must
        // be 1, not 2, so recall == 1.0 rather than 0.5.
        let demoted_job = job(finding("high", "reached", Some("let x"), CTX), &["c1"]);
        let unreached_job = ConfirmationJob {
            plan: plan(finding("high", "unreached", Some("let x"), CTX), &[]),
            confirmers: Vec::new(),
        };
        let review = aggregate(&[demoted_job, unreached_job], &limits(), &mut |m, _| {
            attempt(m, "other", ParsedDisposition::Reject)
        });
        assert_eq!(review.demoted.len(), 1);
        assert_eq!(
            review.reviewer_family_recall,
            Some(1.0),
            "recall denominator must only count findings that reached a confirmer"
        );
        // The unreached finding never demoted, so the review still blocks.
        assert!(review.effective_blocking);
    }

    #[test]
    fn budget_exhausted_finding_is_not_run_and_still_blocks() {
        // The spec (§7): unconfirmed findings keep blocking. A finding that
        // never got a confirmation call because the budget ran out is
        // `not_run` and MUST still block — it is not a demotion.
        let jobs: Vec<ConfirmationJob> = (0..9)
            .map(|i| {
                job(
                    finding("high", &format!("budget claim {i}"), Some("let x"), CTX),
                    &["c1"],
                )
            })
            .collect();
        let review = aggregate(&jobs, &limits(), &mut |m, _| {
            attempt(m, "other", ParsedDisposition::Confirm)
        });
        let last = review.decisions.last().unwrap();
        assert_eq!(last.disposition, Disposition::NotRun);
        assert!(
            last.blocking,
            "an unconfirmed, budget-exhausted finding keeps blocking"
        );
        assert!(review.effective_blocking);
    }

    #[test]
    fn confirmed_low_finding_never_blocks() {
        // A medium/low finding recorded by the caller as not_run must not
        // contribute to effective_blocking. `aggregate` only sees blocking
        // candidates, so this pins the caller-side contract: a NotRun
        // decision added with blocking=false does not block.
        use crate::review_confirm::ConfirmedReview;
        let decisions: Vec<FindingDecision> = vec![FindingDecision {
            finding_id: "x".into(),
            severity: "low".into(),
            location: "src/a.rs:1".into(),
            quote: None,
            quote_found: false,
            disposition: Disposition::NotRun,
            blocking: false,
            reason: "medium/low findings never reach confirmation".into(),
            attempts: Vec::new(),
        }];
        let review = ConfirmedReview {
            decisions,
            demoted: Vec::new(),
            effective_blocking: false,
            reviewer_family_recall: None,
        };
        assert!(!review.effective_blocking);
        assert_eq!(review.effective_verdict(), "comment");
    }

    #[test]
    fn one_call_per_finding_per_confirmer() {
        // The same confirmer repeated in the chain must be called once.
        let j = ConfirmationJob {
            plan: plan(
                finding("critical", "dup", Some("let x"), CTX),
                &["c1", "c1", "c1"],
            ),
            confirmers: vec!["c1".into(), "c1".into(), "c1".into()],
        };
        let mut calls = 0usize;
        let review = aggregate(&[j], &limits(), &mut |m, _| {
            calls += 1;
            attempt(m, "famA", ParsedDisposition::Reject)
        });
        assert_eq!(calls, 1, "one call per finding per confirmer");
        // One critical reject is not enough, so the finding still blocks.
        assert!(review.effective_blocking);
    }

    #[test]
    fn default_timeout_is_sixty_seconds() {
        assert_eq!(limits().timeout_s, 60);
        assert_eq!(
            ConfirmLimits::from_config(&ReviewConfirmConfig::default()).timeout_s,
            60
        );
    }

    // ---- second-reject independence via selection -----------------------

    #[test]
    fn second_reject_requires_a_different_family() {
        // Chain has two confirmers from the reviewer's own family and one
        // from another; selection must drop the same-family pair.
        let chain = vec![
            "samefam-a".to_string(),
            "samefam-b".to_string(),
            "otherfam".to_string(),
        ];
        let family_of = |m: &str| {
            if m.starts_with("samefam") {
                Some("fam".to_string())
            } else {
                Some("other".to_string())
            }
        };
        let selected = select_confirmers(&chain, "fam", &[], &[], &family_of);
        assert_eq!(selected, vec!["otherfam".to_string()]);
    }

    #[test]
    fn selection_excludes_already_used_confirmers_for_the_finding() {
        let chain = vec!["a".to_string(), "b".to_string()];
        let used = vec!["a".to_string()];
        let family_of = |_: &str| Some("x".to_string());
        let selected = select_confirmers(&chain, "rev", &[], &used, &family_of);
        assert_eq!(selected, vec!["b".to_string()]);
    }

    // ---- parsing --------------------------------------------------------

    #[test]
    fn unparseable_reply_is_none() {
        assert!(parse_confirmer_output("not json at all", "id", CTX).is_none());
    }

    #[test]
    fn reject_without_quote_is_insufficient() {
        let raw = r#"{"finding_id":"id","disposition":"reject","reason":"bad"}"#;
        let v = parse_confirmer_output(raw, "id", CTX).unwrap();
        assert_eq!(v.disposition, ParsedDisposition::Insufficient);
    }

    #[test]
    fn reject_with_verbatim_quote_parses() {
        let raw = r#"{"finding_id":"id","disposition":"reject","quote":"let x","reason":"fine"}"#;
        let v = parse_confirmer_output(raw, "id", CTX).unwrap();
        assert_eq!(v.disposition, ParsedDisposition::Reject);
    }

    #[test]
    fn reply_about_a_different_finding_cannot_demote() {
        let raw = r#"{"finding_id":"other","disposition":"reject","quote":"let x","reason":"r"}"#;
        let v = parse_confirmer_output(raw, "id", CTX).unwrap();
        assert_eq!(v.disposition, ParsedDisposition::Insufficient);
    }

    // ---- record shape v1 ------------------------------------------------

    #[test]
    fn record_shape_v1_round_trips() {
        let j = job(finding("high", "rt", Some("let x"), CTX), &["c1"]);
        let review = aggregate(&[j], &limits(), &mut |m, _| {
            attempt(m, "other", ParsedDisposition::Reject)
        });
        let value = record(&review, "rev-model", "revfam", "comment", 12, 100, 40);
        let parsed = parse_record(&value);
        assert_eq!(parsed.version, 1);
        assert!(!parsed.effective_blocking);
        assert_eq!(parsed.verdict, "comment");
        assert_eq!(parsed.verdict_basis, "reviewer_plus_confirmer");
        assert_eq!(parsed.findings.len(), 1);
        let f = &parsed.findings[0];
        assert_eq!(f.id, review.decisions[0].finding_id);
        assert_eq!(f.severity, "high");
        assert_eq!(f.location, "src/a.rs:1");
        assert_eq!(f.quote.as_deref(), Some("let x"));
        assert!(f.quote_found);
        assert_eq!(f.disposition, Disposition::Rejected);
        // reviewers[0] is the reviewer; the confirmer follows.
        assert_eq!(parsed.reviewer_count, 2);
        let reviewers = value.get("reviewers").unwrap().as_array().unwrap();
        assert_eq!(reviewers[0]["role"], "reviewer");
        assert_eq!(reviewers[0]["model"], "rev-model");
        assert_eq!(reviewers[1]["role"], "confirmer");
    }

    #[test]
    fn old_shape_record_still_parses() {
        // A pre-confirm review payload has no confirm fields at all.
        let value = serde_json::json!({ "verdict": "request_changes" });
        let parsed = parse_record(&value);
        assert_eq!(parsed.version, 0);
        assert!(!parsed.effective_blocking);
        assert_eq!(parsed.verdict_basis, "reviewer_only");
        assert!(parsed.findings.is_empty());
        assert!(parsed.demoted.is_empty());
    }

    #[test]
    fn record_includes_all_required_finding_fields() {
        let j = job(
            finding("critical", "fields", Some("let x"), CTX),
            &["c1", "c2"],
        );
        let review = aggregate(&[j], &limits(), &mut |m, _| {
            let fam = if m == "c1" { "a" } else { "b" };
            attempt(m, fam, ParsedDisposition::Reject)
        });
        let value = record(&review, "rev", "revfam", "comment", 0, 0, 0);
        let f = &value["findings"][0];
        for key in [
            "id",
            "severity",
            "location",
            "quote",
            "quote_found",
            "disposition",
        ] {
            assert!(f.get(key).is_some(), "finding must carry `{key}`");
        }
        // Both confirmer ids recorded for a critical demotion.
        assert_eq!(
            value["demoted"][0]["confirmer_ids"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(value["version"], 1);
        assert_eq!(value["verdict_basis"], "reviewer_plus_confirmer");
    }

    // ---- context builder ------------------------------------------------

    #[test]
    fn context_is_bounded_and_includes_the_cited_window() {
        let file = (1..=500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let read = move |_: &str| Some(file.clone());
        let f = finding("high", "something about foo", Some("let x"), CTX);
        let ctx = build_context(&f, "diff body", &read, 20);
        // The hunk and the windowed file are present, but bounded.
        assert!(ctx.contains("diff body"));
        let windowed_lines = ctx.lines().count();
        // hunk(1) + headers + at most 20 window lines + a couple of def lines.
        assert!(
            windowed_lines < 60,
            "context must stay bounded: {windowed_lines}"
        );
    }

    #[test]
    fn reviewer_family_is_never_a_confirmer() {
        // The reviewer's own family can never confirm its finding — there is no
        // flag that relaxes this.
        let chain = vec!["rev-sibling".to_string(), "other".to_string()];
        let family_of = |m: &str| {
            if m == "rev-sibling" {
                Some("revfam".to_string())
            } else {
                Some("otherfam".to_string())
            }
        };
        let selected = select_confirmers(&chain, "revfam", &[], &[], &family_of);
        assert_eq!(selected, vec!["other".to_string()]);
    }

    #[test]
    fn author_family_is_never_a_confirmer() {
        // A confirmer from any author family is excluded, unconditionally.
        let chain = vec![
            "reviewer-ok".to_string(),
            "author-sibling".to_string(),
            "free".to_string(),
        ];
        let family_of = |m: &str| {
            if m == "author-sibling" {
                Some("authorfam".to_string())
            } else {
                Some(m.trim_end_matches("-ok").to_string())
            }
        };
        let authors = vec!["authorfam".to_string()];
        let selected = select_confirmers(&chain, "revfam", &authors, &[], &family_of);
        assert_eq!(
            selected,
            vec!["reviewer-ok".to_string(), "free".to_string()]
        );
    }

    #[test]
    fn finding_with_no_confirmers_stays_blocking() {
        // No confirmer could be resolved (empty chain). The finding is not_run
        // but MUST still block, and the review must still request changes.
        let j = ConfirmationJob {
            plan: plan(finding("critical", "no chain", Some("let x"), CTX), &[]),
            confirmers: Vec::new(),
        };
        let mut calls = 0usize;
        let review = aggregate(&[j], &limits(), &mut |m, _| {
            calls += 1;
            attempt(m, "other", ParsedDisposition::Confirm)
        });
        assert_eq!(calls, 0, "no confirmer may be called for an empty chain");
        assert_eq!(review.decisions[0].disposition, Disposition::NotRun);
        assert!(review.decisions[0].blocking);
        assert!(
            review.effective_blocking,
            "a finding with no available confirmer must stay blocking"
        );
        assert_eq!(review.effective_verdict(), "request_changes");
    }

    #[test]
    fn already_used_confirmer_is_excluded_even_if_listed_again() {
        // The used-set is honoured on every selection, so a caller that retries
        // cannot re-call a confirmer that already ran for this finding.
        let chain = vec!["a".to_string(), "b".to_string()];
        let used = vec!["a".to_string()];
        let family_of = |_: &str| Some("x".to_string());
        let selected = select_confirmers(&chain, "rev", &[], &used, &family_of);
        assert_eq!(selected, vec!["b".to_string()]);
    }

    #[test]
    fn finding_id_is_stable_and_whitespace_insensitive() {
        let a = finding_id("fam", "src/a.rs", "Foo  bar\tbaz");
        let b = finding_id("fam", "src/a.rs", "foo bar baz");
        assert_eq!(a, b);
        let c = finding_id("other", "src/a.rs", "foo bar baz");
        assert_ne!(a, c);
    }
}
