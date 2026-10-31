//! Machine-readable failure classification for `zoder review` and `zoder loop`.
//!
//! # Why this exists
//!
//! Until 2026-10-31 every failure of `zoder review` / `zoder loop` exited `1`
//! with free text on stderr. A fleet worker that drives those commands could
//! not tell a TRANSIENT failure (the provider was down, the engine was
//! unreachable, the budget reset is pending) from a PERMANENT one (the diff is
//! too large to ever review, the reviewer pin names a model that does not
//! exist, the command was misused). The only available signal was a substring
//! match against a human-readable message — brittle, and impossible to keep
//! stable while the prose improved.
//!
//! This module introduces the stable contract:
//!
//! * A [`FailureClass`] enum with a fixed set of variants. Each variant has a
//!   STABLE string code (`"config"`, `"budget_exceeded"`, ...) that callers
//!   may match on, and a documented process exit code.
//! * A `retryable` boolean, so a fleet worker can decide on its own whether
//!   to re-queue the job without parsing prose.
//! * A JSON envelope that carries the class, the retryability, and the human
//!   message, so a machine consumer never has to scrape stderr.
//!
//! # Exit-code contract
//!
//! * `0` — success (unchanged).
//! * `1` — ANY failure that is not classified. This is the pre-existing
//!   behaviour and it stays the default so every existing caller keeps
//!   working unchanged.
//! * A distinct, documented non-zero code per classified failure class. The
//!   codes live in the "64..=70" range (the `sysexits.h` convention) and are
//!   stable: a code is never reused for a different class.
//!
//! # Backward compatibility
//!
//! Existing JSON fields are NOT renamed, removed, or reshaped. The failure
//! information is ADDITIVE: a `failure` object is present only on a failure,
//! and every pre-existing field keeps its exact previous meaning.

use std::fmt;

/// The stable, machine-readable classification of a `zoder review` /
/// `zoder loop` failure.
///
/// Every variant maps to exactly one process exit code and one stable string
/// code. The string code is the CONTRACT: it is what a fleet worker matches
/// on, and it never changes meaning. The exit code is a convenience for
/// shell callers; `1` remains the code for anything unclassified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureClass {
    /// The invocation or the environment is misconfigured: an unreadable or
    /// invalid config file, a bad `--engine` value, a reviewer pin that names
    /// no configured model, conflicting CLI flags.
    ///
    /// NOT retryable without an operator change.
    Config,
    /// The command was used incorrectly: mutually exclusive flags, a missing
    /// required argument, a scope that cannot be satisfied.
    ///
    /// NOT retryable without an operator change.
    Usage,
    /// A budget was exhausted: the per-call cap, the month-to-date spend cap,
    /// or the EXTERNAL engine's own `budget_exceeded:` refusal.
    ///
    /// NOT retryable until the budget window resets.
    BudgetExceeded,
    /// No reviewer could produce a verdict: the chain was exhausted, the
    /// reviewer twice returned an answer with no verdict (budget exhausted
    /// twice), every panel member failed, or the provider kept the reviewer
    /// from answering.
    ///
    /// Retryable — a different reviewer or a later attempt often succeeds.
    ReviewerUnavailable,
    /// The review completed and the aggregate verdict BLOCKS the change
    /// (`request_changes` / `reject` / `block`). This is a real, successful
    /// review that says "no".
    ///
    /// NOT retryable — retrying the same diff produces the same verdict.
    ReviewBlocked,
    /// The diff is larger than the configured review budget (per-diff bytes,
    /// per-hunk bytes, or the number of chunks) and therefore cannot be
    /// reviewed as configured.
    ///
    /// NOT retryable without changing the caps or the exclusions.
    DiffTooLarge,
    /// The run was cancelled, or a required cancellation could not be
    /// confirmed by the engine.
    ///
    /// Retryable — the caller may re-drive the job.
    Cancelled,
    /// The agent engine (the `zeroclaw` daemon) could not be reached: it is
    /// not running, its socket is stale, or it died during startup.
    ///
    /// Retryable — the daemon can be restarted.
    EngineUnreachable,
    /// An unclassified internal failure: a bug, or a condition this
    /// classification does not cover yet.
    ///
    /// NOT retryable — the default is "something is wrong here".
    Internal,
}

impl FailureClass {
    /// The STABLE string code for this class.
    ///
    /// This string is the contract a fleet worker matches on. It never
    /// changes meaning; new classes get new strings.
    pub(crate) const fn code(self) -> &'static str {
        match self {
            FailureClass::Config => "config",
            FailureClass::Usage => "usage",
            FailureClass::BudgetExceeded => "budget_exceeded",
            FailureClass::ReviewerUnavailable => "reviewer_unavailable",
            FailureClass::ReviewBlocked => "review_blocked",
            FailureClass::DiffTooLarge => "diff_too_large",
            FailureClass::Cancelled => "cancelled",
            FailureClass::EngineUnreachable => "engine_unreachable",
            FailureClass::Internal => "internal",
        }
    }

    /// The documented process exit code for this class.
    ///
    /// * `1` — unclassified failure (the pre-existing default).
    /// * `64..=72` — the "classified failure" band. `64` (`EX_USAGE`), `65`
    ///   (`EX_DATAERR`), `66` (`EX_NOINPUT`) and `69` (`EX_UNAVAILABLE`)
    ///   follow the `sysexits.h` convention; `67`, `68`, `70`, `71` and `72`
    ///   are zoder-specific extensions of the same band.
    ///
    /// Every code is stable AND unique: a code is never reused for another
    /// class, so a shell caller can dispatch on the exit code alone.
    pub(crate) const fn exit_code(self) -> i32 {
        match self {
            // EX_USAGE: the invocation itself is wrong.
            FailureClass::Usage => 64,
            // EX_DATAERR: the environment / config data is wrong.
            FailureClass::Config => 65,
            // EX_NOINPUT: the diff input cannot be processed as configured.
            FailureClass::DiffTooLarge => 66,
            // A budget window is exhausted; retrying now fails the same way.
            FailureClass::BudgetExceeded => 67,
            // No reviewer produced a verdict.
            FailureClass::ReviewerUnavailable => 68,
            // EX_UNAVAILABLE: the reviewed change is not available for
            // acceptance — the review completed and said "no".
            FailureClass::ReviewBlocked => 69,
            // The run was cancelled, or the cancellation was not confirmed.
            FailureClass::Cancelled => 70,
            // The agent engine is not reachable.
            FailureClass::EngineUnreachable => 71,
            // EX_SOFTWARE analogue: an internal error.
            FailureClass::Internal => 72,
        }
    }

    /// Whether a caller MAY usefully retry the same invocation as-is.
    ///
    /// This is a hint for fleet workers, not a guarantee: a retryable failure
    /// can still fail again. It answers exactly one question — "is this the
    /// same request failing, or is something in the environment broken?"
    pub(crate) const fn retryable(self) -> bool {
        match self {
            // The environment or the operator has to change something first.
            FailureClass::Config
            | FailureClass::Usage
            | FailureClass::BudgetExceeded
            | FailureClass::ReviewBlocked
            | FailureClass::DiffTooLarge
            | FailureClass::Internal => false,
            // A different reviewer, a later attempt, a restarted daemon, or a
            // re-driven job can plausibly succeed unchanged.
            FailureClass::ReviewerUnavailable
            | FailureClass::Cancelled
            | FailureClass::EngineUnreachable => true,
        }
    }

    /// All classes, in the canonical order used by the documentation and the
    /// tests. Keeps the docs and the code from drifting apart.
    pub(crate) const ALL: &'static [FailureClass] = &[
        FailureClass::Config,
        FailureClass::Usage,
        FailureClass::BudgetExceeded,
        FailureClass::ReviewerUnavailable,
        FailureClass::ReviewBlocked,
        FailureClass::DiffTooLarge,
        FailureClass::Cancelled,
        FailureClass::EngineUnreachable,
        FailureClass::Internal,
    ];

    /// Parse a stable class code back into a class.
    ///
    /// This is the inverse of [`FailureClass::code`], so a consumer that
    /// persisted a class code (a fleet worker recording a job outcome, a
    /// test fixture) can read it back without a hand-maintained mapping.
    /// An unknown code is `None`: the vocabulary is closed, so an
    /// unrecognized string is a contract violation, not a class.
    pub(crate) fn from_code(code: &str) -> Option<Self> {
        FailureClass::ALL.iter().copied().find(|c| c.code() == code)
    }
}

/// The stable code of every class, in the canonical order.
///
/// This is the machine-readable half of the contract, exposed so a caller
/// (or the documentation test) can enumerate the vocabulary without
/// duplicating it.
pub(crate) fn all_class_codes() -> Vec<&'static str> {
    FailureClass::ALL.iter().map(|c| c.code()).collect()
}

/// The exit code of every class, paired with its stable code.
///
/// Exposed so the documentation test can assert that `docs/EXIT_CODES.md`
/// matches the code — the doc and the implementation cannot drift.
pub(crate) fn class_exit_codes() -> Vec<(&'static str, i32)> {
    FailureClass::ALL
        .iter()
        .map(|c| (c.code(), c.exit_code()))
        .collect()
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// A classified failure: the class, the human message, and (when the external
/// engine said so) the moment its budget window resets.
///
/// This is the value every `zoder review` / `zoder loop` failure site
/// returns. It carries no secrets: the message is the same operator-facing
/// text that was already printed to stderr before this contract existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClassifiedFailure {
    pub class: FailureClass,
    /// The operator-facing message. Identical to (or a superset of) the text
    /// that was printed before this contract existed, so no diagnostic is
    /// lost.
    pub message: String,
    /// The external engine's budget-reset timestamp, when it reported one.
    /// Present only for [`FailureClass::BudgetExceeded`] and only when the
    /// engine's message carried a reset time.
    pub budget_reset_at: Option<String>,
}

impl ClassifiedFailure {
    /// A classified failure with no reset timestamp.
    pub(crate) fn new(class: FailureClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            budget_reset_at: None,
        }
    }

    /// A classified failure that carries a budget-reset timestamp.
    pub(crate) fn with_budget_reset(
        class: FailureClass,
        message: impl Into<String>,
        reset_at: Option<String>,
    ) -> Self {
        Self {
            class,
            message: message.into(),
            budget_reset_at: reset_at,
        }
    }

    /// The process exit code for this failure.
    pub(crate) fn exit_code(&self) -> i32 {
        self.class.exit_code()
    }

    /// Whether the same invocation may usefully be retried as-is.
    pub(crate) fn retryable(&self) -> bool {
        self.class.retryable()
    }

    /// The JSON object a machine consumer reads.
    ///
    /// Shape (stable):
    /// ```json
    /// {
    ///   "class": "budget_exceeded",
    ///   "code": "budget_exceeded",
    ///   "retryable": false,
    ///   "message": "...",
    ///   "budget_reset_at": "2026-11-01T00:00:00Z"
    /// }
    /// ```
    /// `budget_reset_at` is present ONLY when the engine reported a reset
    /// timestamp; it is never fabricated.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::json!({
            "class": self.class.code(),
            "code": self.class.code(),
            "retryable": self.retryable(),
            "message": self.message,
        });
        if let Some(reset) = &self.budget_reset_at {
            obj["budget_reset_at"] = serde_json::Value::String(reset.clone());
        }
        obj
    }
}

/// The exit code the process uses when the failure was never classified.
///
/// `1` is the pre-existing behaviour for every failure, so a caller that
/// predates this contract keeps seeing exactly what it saw before.
pub(crate) const UNCLASSIFIED_EXIT_CODE: i32 = 1;

/// A [`ClassifiedFailure`] carried inside an `anyhow::Error` chain.
///
/// This is the transport: a failure site returns
/// `Err(ClassifiedFailure::new(class, msg).into())` and the dispatcher in
/// `main` recovers the class by walking the error chain. The chain walk is
/// what keeps the classification attached to the error even when an
/// intermediate frame adds `.context(...)` on top of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClassifiedFailureError(ClassifiedFailure);

impl ClassifiedFailureError {
    pub(crate) fn new(class: FailureClass, message: impl Into<String>) -> Self {
        Self(ClassifiedFailure::new(class, message))
    }

    pub(crate) fn with_budget_reset(
        class: FailureClass,
        message: impl Into<String>,
        reset_at: Option<String>,
    ) -> Self {
        Self(ClassifiedFailure::with_budget_reset(
            class, message, reset_at,
        ))
    }

    pub(crate) fn failure(&self) -> &ClassifiedFailure {
        &self.0
    }
}

impl std::fmt::Display for ClassifiedFailureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.message)
    }
}

impl std::error::Error for ClassifiedFailureError {}

/// Recover the classification from an error chain, if it carries one.
///
/// The chain is walked in order and the FIRST `ClassifiedFailureError` wins:
/// the outermost classification is the one the failure site chose, and any
/// `.context(...)` frame above it is only prose. When no frame carries a
/// classification, the message is matched against the known markers
/// ([`classify_message`]) so an un-annotated failure site still lands in the
/// right class instead of the unclassified bucket.
///
/// Returns `None` when the failure is genuinely unclassifiable; the caller
/// then uses [`UNCLASSIFIED_EXIT_CODE`].
pub(crate) fn classify_error(error: &anyhow::Error) -> Option<ClassifiedFailure> {
    for cause in error.chain() {
        if let Some(classified) = cause.downcast_ref::<ClassifiedFailureError>() {
            return Some(classified.failure().clone());
        }
    }
    // No explicit classification: try to infer one from the message. The
    // whole chain is searched so a context frame that carries the real
    // diagnostic is not missed.
    let chain_text = error
        .chain()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(": ");
    classify_message(&chain_text).map(|(class, reset)| {
        ClassifiedFailure::with_budget_reset(class, chain_text.clone(), reset)
    })
}

/// Build an `anyhow::Error` that carries a classification.
pub(crate) fn classified_err(class: FailureClass, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ClassifiedFailureError::new(class, message))
}

/// Build an `anyhow::Error` that carries a classification AND a budget-reset
/// timestamp.
pub(crate) fn classified_budget_err(
    message: impl Into<String>,
    reset_at: Option<String>,
) -> anyhow::Error {
    anyhow::Error::new(ClassifiedFailureError::with_budget_reset(
        FailureClass::BudgetExceeded,
        message,
        reset_at,
    ))
}

/// The JSON document a `--json` failure prints when there is NO result
/// object to attach the failure to.
///
/// The shape mirrors the in-result envelope so a consumer reads the same
/// keys in both places:
///
/// ```json
/// {
///   "error": {
///     "class": "reviewer_unavailable",
///     "code": "reviewer_unavailable",
///     "retryable": true,
///     "message": "..."
///   }
/// }
/// ```
pub(crate) fn error_envelope(failure: &ClassifiedFailure) -> serde_json::Value {
    serde_json::json!({ "error": failure.to_json() })
}

/// Did this process already print (or write) a RESULT object?
///
/// The `--json` contract is one JSON document on stdout. When the command
/// produced a result (the review payload, the loop payload), the failure
/// information is attached to THAT object and nothing else is printed —
/// printing a second document would break every consumer that parses
/// stdout as one JSON value. When NO result exists (the command failed
/// before producing one), the failure is printed as its own `error` object.
static RESULT_EMITTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record that a result object was produced (printed to stdout and/or
/// written to `result.json`).
pub(crate) fn mark_result_emitted() {
    RESULT_EMITTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// The class of an arbitrary error message, when one can be inferred.
///
/// This is the RECOGNITION step: it maps the free-text shape of a failure onto
/// the stable class vocabulary. It is deliberately conservative — it only
/// recognizes specific, unambiguous markers that the failure sites in this
/// codebase emit, and it returns `None` for everything else so an
/// unrecognized error stays unclassified (exit `1`).
///
/// Recognized markers:
///
/// * `budget_exceeded:` — the external engine's own budget refusal. The
///   optional trailing reset timestamp is extracted when present.
/// * the diff-size refusals (`above the N-byte limit`, `above the N-byte
///   review limit`, `review diff is N bytes`) — the diff cannot be reviewed
///   as configured.
/// * the reviewer-unavailable markers (`0/N reviewers completed`,
///   `review incomplete`, `reviewer_unavailable`) — no reviewer produced a
///   verdict.
/// * `review blocked:` / `requests changes` — the review completed and said
///   "no".
/// * the cancellation markers (`author cancellation unconfirmed:`,
///   `cancel not confirmed`) — the run was cancelled, or the engine did not
///   confirm the cancellation.
/// * the engine-unreachable markers (`connecting to engine`,
///   `daemon not ready`, `daemon exited during startup`,
///   `engine is unavailable`) — the agent engine could not be reached.
/// * the usage markers (`invalid --engine`, `usage:`) — the invocation was
///   wrong.
/// * the config markers (`loading zoder config`, `config`) — the environment
///   is misconfigured.
pub(crate) fn classify_message(message: &str) -> Option<(FailureClass, Option<String>)> {
    let lower = message.to_ascii_lowercase();

    // The EXTERNAL engine's budget refusal. Recognised FIRST: it is the most
    // specific marker, and a budget refusal can mention the daemon in its
    // own diagnostics.
    if let Some((class, reset)) = classify_engine_budget_exceeded(message) {
        return Some((class, reset));
    }

    // The diff is larger than the review budget.
    if lower.contains("above the")
        && (lower.contains("byte limit") || lower.contains("-byte limit"))
    {
        return Some((FailureClass::DiffTooLarge, None));
    }
    if lower.contains("review diff is") && lower.contains("byte") {
        return Some((FailureClass::DiffTooLarge, None));
    }
    if lower.contains("review diff was truncated during acquisition") {
        // A truncated diff is a diff that cannot be reviewed in full.
        return Some((FailureClass::DiffTooLarge, None));
    }

    // The review completed and blocked the change.
    if lower.contains("review blocked") || lower.contains("requests changes") {
        return Some((FailureClass::ReviewBlocked, None));
    }

    // No reviewer produced a verdict.
    if lower.contains("reviewers completed")
        || lower.contains("review incomplete")
        || lower.contains("reviewer_unavailable")
        || lower.contains("reviewer refused")
        || lower.contains("review refused")
    {
        return Some((FailureClass::ReviewerUnavailable, None));
    }

    // The cancellation could not be confirmed, or the run was cancelled.
    if lower.contains("author cancellation unconfirmed")
        || lower.contains("cancel not confirmed")
        || lower.contains("cancellation deadline expired")
        || lower.contains("cancelled")
        || lower.contains("canceled")
    {
        return Some((FailureClass::Cancelled, None));
    }

    // The agent engine could not be reached.
    if lower.contains("connecting to engine")
        || lower.contains("daemon not ready")
        || lower.contains("daemon exited during startup")
        || lower.contains("engine is unavailable")
        || lower.contains("engine offline")
        || lower.contains("no daemon is running")
    {
        return Some((FailureClass::EngineUnreachable, None));
    }

    // The invocation was wrong.
    if lower.contains("invalid --engine")
        || lower.contains("usage:")
        || lower.contains("mutually exclusive")
        || lower.contains("requires --")
        || lower.contains("unknown subcommand")
    {
        return Some((FailureClass::Usage, None));
    }

    // The environment is misconfigured.
    if lower.contains("loading zoder config")
        || lower.contains("loading engine")
        || lower.contains("failed to parse")
        || lower.contains("config error")
        || lower.contains("invalid config")
    {
        return Some((FailureClass::Config, None));
    }

    None
}

/// Recognize the EXTERNAL engine's `budget_exceeded:` refusal.
///
/// The engine reports a budget refusal on stderr as a line beginning with
/// `budget_exceeded:` and MAY append the moment the budget window resets.
/// Both shapes are recognized:
///
/// ```text
/// budget_exceeded: monthly spend cap reached
/// budget_exceeded: quota exhausted, resets at 2026-11-01T00:00:00Z
/// ```
///
/// Returns the class plus the reset timestamp when the message carried one.
pub(crate) fn classify_engine_budget_exceeded(
    message: &str,
) -> Option<(FailureClass, Option<String>)> {
    const MARKER: &str = "budget_exceeded:";
    let start = message.to_ascii_lowercase().find(MARKER)?;
    let rest = &message[start + MARKER.len()..];
    let line = rest.lines().next().unwrap_or("").trim();
    let reset = extract_reset_timestamp(line);
    Some((FailureClass::BudgetExceeded, reset))
}

/// Pull a reset timestamp out of an engine budget line.
///
/// Recognizes an RFC 3339 timestamp (`2026-11-01T00:00:00Z`,
/// `2026-11-01T00:00:00+00:00`) and a bare date (`2026-11-01`) that follows
/// a `reset`/`resets`/`until` marker. Returns `None` when the line carries
/// no timestamp — the timestamp is never fabricated.
fn extract_reset_timestamp(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let marker_at = ["reset", "resets", "until"]
        .iter()
        .filter_map(|m| lower.find(m))
        .min()?;
    let tail = &line[marker_at..];
    let token: String = tail
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| {
            c.is_ascii_digit()
                || *c == '-'
                || *c == ':'
                || *c == 'T'
                || *c == 'Z'
                || *c == '+'
                || *c == '.'
        })
        .collect();
    if token.len() < 8 {
        return None;
    }
    // A bare date is the common shape; keep it verbatim so the consumer
    // decides how to parse it. A full RFC 3339 instant is kept verbatim too.
    Some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the vocabulary -------------------------------------------------

    #[test]
    fn every_class_has_a_unique_stable_code() {
        let mut codes: Vec<&str> = FailureClass::ALL.iter().map(|c| c.code()).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(before, codes.len(), "failure class codes must be unique");
    }

    #[test]
    fn every_class_has_a_unique_exit_code() {
        let mut codes: Vec<i32> = FailureClass::ALL.iter().map(|c| c.exit_code()).collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(
            before,
            codes.len(),
            "failure class exit codes must be unique"
        );
    }

    #[test]
    fn every_class_exit_code_is_non_zero() {
        for c in FailureClass::ALL {
            assert_ne!(c.exit_code(), 0, "{c} must not exit 0");
        }
    }

    #[test]
    fn the_class_vocabulary_is_exactly_the_documented_nine() {
        let codes: Vec<&str> = FailureClass::ALL.iter().map(|c| c.code()).collect();
        assert_eq!(
            codes,
            vec![
                "config",
                "usage",
                "budget_exceeded",
                "reviewer_unavailable",
                "review_blocked",
                "diff_too_large",
                "cancelled",
                "engine_unreachable",
                "internal",
            ]
        );
    }

    #[test]
    fn every_code_round_trips_back_to_its_class() {
        for c in FailureClass::ALL {
            assert_eq!(
                FailureClass::from_code(c.code()),
                Some(*c),
                "the code {:?} must round-trip",
                c.code()
            );
        }
    }

    #[test]
    fn an_unknown_code_is_not_a_class() {
        assert_eq!(FailureClass::from_code("not_a_class"), None);
        assert_eq!(FailureClass::from_code(""), None);
    }

    // ---- one test per class --------------------------------------------

    #[test]
    fn config_failure_is_not_retryable_and_exits_65() {
        let c = FailureClass::Config;
        assert_eq!(c.code(), "config");
        assert_eq!(c.exit_code(), 65);
        assert!(!c.retryable());
    }

    #[test]
    fn usage_failure_is_not_retryable_and_exits_64() {
        let c = FailureClass::Usage;
        assert_eq!(c.code(), "usage");
        assert_eq!(c.exit_code(), 64);
        assert!(!c.retryable());
    }

    #[test]
    fn budget_exceeded_failure_is_not_retryable_and_exits_67() {
        let c = FailureClass::BudgetExceeded;
        assert_eq!(c.code(), "budget_exceeded");
        assert_eq!(c.exit_code(), 67);
        assert!(!c.retryable());
    }

    #[test]
    fn reviewer_unavailable_failure_is_retryable_and_exits_68() {
        let c = FailureClass::ReviewerUnavailable;
        assert_eq!(c.code(), "reviewer_unavailable");
        assert_eq!(c.exit_code(), 68);
        assert!(c.retryable());
    }

    #[test]
    fn review_blocked_failure_is_not_retryable_and_exits_69() {
        let c = FailureClass::ReviewBlocked;
        assert_eq!(c.code(), "review_blocked");
        assert_eq!(c.exit_code(), 69);
        assert!(!c.retryable());
    }

    #[test]
    fn diff_too_large_failure_is_not_retryable_and_exits_66() {
        let c = FailureClass::DiffTooLarge;
        assert_eq!(c.code(), "diff_too_large");
        assert_eq!(c.exit_code(), 66);
        assert!(!c.retryable());
    }

    #[test]
    fn cancelled_failure_is_retryable_and_exits_70() {
        let c = FailureClass::Cancelled;
        assert_eq!(c.code(), "cancelled");
        assert_eq!(c.exit_code(), 70);
        assert!(c.retryable());
    }

    #[test]
    fn engine_unreachable_failure_is_retryable_and_exits_71() {
        let c = FailureClass::EngineUnreachable;
        assert_eq!(c.code(), "engine_unreachable");
        assert_eq!(c.exit_code(), 71);
        assert!(c.retryable());
    }

    #[test]
    fn internal_failure_is_not_retryable_and_exits_72() {
        let c = FailureClass::Internal;
        assert_eq!(c.code(), "internal");
        assert_eq!(c.exit_code(), 72);
        assert!(!c.retryable());
    }

    // ---- the unclassified default --------------------------------------

    #[test]
    fn unclassified_failures_keep_exit_code_one() {
        assert_eq!(UNCLASSIFIED_EXIT_CODE, 1);
        // Every existing caller that checks "non-zero means failure" keeps
        // working: no classified code is 1, and 1 stays the fallback.
        for c in FailureClass::ALL {
            assert_ne!(c.exit_code(), UNCLASSIFIED_EXIT_CODE);
        }
    }

    // ---- the JSON envelope ---------------------------------------------

    #[test]
    fn json_envelope_carries_class_code_retryable_and_message() {
        let f = ClassifiedFailure::new(
            FailureClass::ReviewerUnavailable,
            "review failed: 0/1 reviewers completed",
        );
        let j = f.to_json();
        assert_eq!(j["class"], "reviewer_unavailable");
        assert_eq!(j["code"], "reviewer_unavailable");
        assert_eq!(j["retryable"], true);
        assert_eq!(j["message"], "review failed: 0/1 reviewers completed");
    }

    #[test]
    fn json_envelope_omits_the_reset_timestamp_when_there_is_none() {
        let f = ClassifiedFailure::new(FailureClass::Internal, "boom");
        let j = f.to_json();
        assert!(
            j.get("budget_reset_at").is_none(),
            "a reset timestamp must never be fabricated: {j}"
        );
    }

    #[test]
    fn json_envelope_carries_the_reset_timestamp_when_the_engine_gave_one() {
        let f = ClassifiedFailure::with_budget_reset(
            FailureClass::BudgetExceeded,
            "budget_exceeded: quota exhausted",
            Some("2026-11-01T00:00:00Z".to_string()),
        );
        let j = f.to_json();
        assert_eq!(j["class"], "budget_exceeded");
        assert_eq!(j["retryable"], false);
        assert_eq!(j["budget_reset_at"], "2026-11-01T00:00:00Z");
    }

    // ---- the engine's budget refusal -----------------------------------

    #[test]
    fn engine_budget_exceeded_text_is_recognized() {
        let (class, reset) =
            classify_engine_budget_exceeded("budget_exceeded: monthly spend cap reached")
                .expect("the engine's budget refusal must be recognized");
        assert_eq!(class, FailureClass::BudgetExceeded);
        assert_eq!(reset, None);
    }

    #[test]
    fn engine_budget_exceeded_text_carries_the_reset_timestamp_when_present() {
        let (class, reset) = classify_engine_budget_exceeded(
            "budget_exceeded: quota exhausted, resets at 2026-11-01T00:00:00Z",
        )
        .expect("the engine's budget refusal must be recognized");
        assert_eq!(class, FailureClass::BudgetExceeded);
        assert_eq!(reset.as_deref(), Some("2026-11-01T00:00:00Z"));
    }

    #[test]
    fn engine_budget_exceeded_text_is_recognized_inside_a_longer_message() {
        let (class, _) = classify_engine_budget_exceeded(
            "zoder: error: turn failed: budget_exceeded: spend cap reached",
        )
        .expect("the engine's budget refusal must be recognized anywhere");
        assert_eq!(class, FailureClass::BudgetExceeded);
    }

    #[test]
    fn a_message_without_the_engine_budget_marker_is_not_a_budget_failure() {
        assert!(classify_engine_budget_exceeded("all good").is_none());
        assert!(classify_engine_budget_exceeded("budget exceeded").is_none());
    }

    // ---- message recognition -------------------------------------------

    #[test]
    fn an_unrecognized_message_stays_unclassified() {
        assert!(classify_message("something entirely unexpected happened").is_none());
    }

    #[test]
    fn a_config_error_message_is_recognized() {
        let (class, _) = classify_message("loading zoder config to locate sessions dir: boom")
            .expect("a config error must be recognized");
        assert_eq!(class, FailureClass::Config);
    }

    #[test]
    fn a_usage_error_message_is_recognized() {
        let (class, _) =
            classify_message("invalid --engine: nope").expect("a usage error must be recognized");
        assert_eq!(class, FailureClass::Usage);
    }

    #[test]
    fn a_diff_too_large_message_is_recognized() {
        let (class, _) = classify_message(
            "review diff is 900000 bytes, above the 400000-byte limit; pass --max-diff-bytes N",
        )
        .expect("a diff-size refusal must be recognized");
        assert_eq!(class, FailureClass::DiffTooLarge);
    }

    #[test]
    fn a_review_blocked_message_is_recognized() {
        let (class, _) = classify_message(
            "review blocked: aggregate verdict `request_changes` requests changes (a blocking review must not exit 0)",
        )
        .expect("a blocking verdict must be recognized");
        assert_eq!(class, FailureClass::ReviewBlocked);
    }

    #[test]
    fn a_reviewer_unavailable_message_is_recognized() {
        let (class, _) = classify_message(
            "review failed: 0/1 reviewers completed (all errors; see diagnostics above).",
        )
        .expect("a total reviewer failure must be recognized");
        assert_eq!(class, FailureClass::ReviewerUnavailable);
    }

    #[test]
    fn an_unconfirmed_cancel_message_is_recognized() {
        let (class, _) =
            classify_message("author cancellation unconfirmed: session x not confirmed")
                .expect("an unconfirmed cancel must be recognized");
        assert_eq!(class, FailureClass::Cancelled);
    }

    #[test]
    fn an_engine_unreachable_message_is_recognized() {
        let (class, _) = classify_message(
            "connecting to engine at /run/zeroclaw/daemon.sock: No such file or directory",
        )
        .expect("an unreachable engine must be recognized");
        assert_eq!(class, FailureClass::EngineUnreachable);
    }

    // ---- carrying a class through an anyhow chain -----------------------

    #[test]
    fn a_classified_error_keeps_its_class_through_a_context_frame() {
        let err = classified_err(FailureClass::DiffTooLarge, "diff is 900000 bytes")
            .context("while preparing the review");
        let recovered = classify_error(&err).expect("the class must survive the chain");
        assert_eq!(recovered.class, FailureClass::DiffTooLarge);
        assert_eq!(recovered.message, "diff is 900000 bytes");
    }

    #[test]
    fn an_unclassified_error_falls_back_to_message_recognition() {
        let err = anyhow::anyhow!("review failed: 0/1 reviewers completed (all errors)");
        let recovered = classify_error(&err).expect("the message must be recognized");
        assert_eq!(recovered.class, FailureClass::ReviewerUnavailable);
    }

    #[test]
    fn a_genuinely_unrecognizable_error_stays_unclassified() {
        let err = anyhow::anyhow!("something entirely unexpected happened");
        assert!(classify_error(&err).is_none());
    }

    #[test]
    fn the_error_envelope_carries_the_class_under_the_error_key() {
        let f = ClassifiedFailure::new(FailureClass::EngineUnreachable, "daemon is down");
        let env = error_envelope(&f);
        assert_eq!(env["error"]["class"], "engine_unreachable");
        assert_eq!(env["error"]["code"], "engine_unreachable");
        assert_eq!(env["error"]["retryable"], true);
        assert_eq!(env["error"]["message"], "daemon is down");
    }

    #[test]
    fn a_budget_error_carries_its_reset_timestamp_through_the_chain() {
        let err = classified_budget_err(
            "budget_exceeded: quota exhausted",
            Some("2026-11-01T00:00:00Z".to_string()),
        );
        let recovered = classify_error(&err).expect("the class must survive the chain");
        assert_eq!(recovered.class, FailureClass::BudgetExceeded);
        assert_eq!(
            recovered.budget_reset_at.as_deref(),
            Some("2026-11-01T00:00:00Z")
        );
        assert_eq!(recovered.exit_code(), 67);
        assert!(!recovered.retryable());
    }

    // ---- the documentation cannot drift from the code -------------------

    /// `docs/EXIT_CODES.md` is the published contract. Every class and every
    /// exit code it documents MUST match the implementation, and every class
    /// in the implementation MUST be documented. A change to the vocabulary
    /// without a documentation update fails here.
    #[test]
    fn the_exit_code_document_matches_the_implementation() {
        let doc = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/EXIT_CODES.md"),
        )
        .expect("docs/EXIT_CODES.md must exist: it is the published contract");

        for (code, exit) in class_exit_codes() {
            // The class code must appear in the document.
            assert!(
                doc.contains(code),
                "docs/EXIT_CODES.md does not document the failure class {code:?}"
            );
            // The exit code must appear on the SAME line as the class code,
            // so the table cannot pair a class with the wrong code.
            let line = doc
                .lines()
                .find(|l| l.contains(&format!("`{code}`")))
                .unwrap_or_else(|| panic!("docs/EXIT_CODES.md has no line for {code:?}"));
            assert!(
                line.contains(&format!("{exit}")),
                "docs/EXIT_CODES.md pairs {code:?} with the wrong exit code; expected {exit} in: {line:?}"
            );
        }
        // The unclassified fallback must be documented too.
        assert!(
            doc.contains("`1`") && doc.contains("not classified"),
            "docs/EXIT_CODES.md must document the unclassified exit code 1"
        );
    }
}
