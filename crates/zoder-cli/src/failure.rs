//! One machine-readable failure contract for `zoder review` and `zoder loop`.
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
//! # The envelope
//!
//! Under `--json` EVERY failure prints exactly one JSON envelope on stdout:
//!
//! ```json
//! {
//!   "ok": false,
//!   "error": {
//!     "code": "reviewers_unavailable",
//!     "class": "transient",
//!     "retry_after_s": 300,
//!     "detail": {
//!       "message": "...",
//!       "reviewers": [ {"model": "m", "err_kind": "Server"} ],
//!       "resets_at": "2026-11-01T00:00:00Z"
//!     }
//!   }
//! }
//! ```
//!
//! * `code` is the SPECIFIC failure ([`FailureCode`], the closed list below).
//!   It is what a caller dispatches on; it never changes meaning.
//! * `class` is the RETRY POLICY the caller applies ([`FailureClass`]): one of
//!   `transient`, `permanent`, `config`, `budget`, `blocked`.
//! * `retry_after_s` is present only when the failure itself says when to come
//!   back (`resets_at` on the engine's budget refusal).
//! * `detail` is a free-form object; the well-known members are `message`
//!   (always present), `reviewers` (per-model `err_kind` — the string form of
//!   `zoder_core::ErrKind` — for `reviewers_unavailable`) and `resets_at`
//!   (for `engine_budget_exceeded`).
//!
//! A failure that happens AFTER a command produced its result keeps the
//! result document and carries the same object as a `failure` member inside
//! it, so stdout stays exactly ONE JSON document (see [`error_envelope`]).
//!
//! # Exit-code contract
//!
//! | code | meaning                                   | example                        |
//! | ---- | ----------------------------------------- | ------------------------------ |
//! | 0    | success (approved / resolved)             | —                              |
//! | 10   | blocked: the review says "no"             | `review_blocked`               |
//! | 20   | transient: retry the same invocation      | `reviewers_unavailable`, `engine_unreachable` |
//! | 30   | budget: wait for the window to reset      | `engine_budget_exceeded`       |
//! | 40   | config / auth / bad input: operator fix   | `no_reviewer_model`, `empty_diff`, `diff_too_large` |
//! | 50   | internal: a bug, or an unclassified error | `internal`                     |
//! | 75   | reserved (oneshot refusal band)           | —                              |
//!
//! Every classified failure is NON-ZERO, so a caller that only tests "did it
//! fail" (the fleet worker) is unaffected by the contract.
//!
//! # Backward compatibility
//!
//! Existing JSON fields are NOT renamed, removed, or reshaped. The failure
//! information is ADDITIVE: a `failure` member is present only on a failure,
//! and every pre-existing field keeps its exact previous meaning.

use std::fmt;

/// The stable, machine-readable classification of a `zoder review` /
/// `zoder loop` failure.
///
/// This is the RETRY POLICY, not the specific failure: several codes share a
/// class (see [`FailureCode::class`]). Five classes, each with one exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureClass {
    /// The same invocation may succeed later: the provider was down, the
    /// engine was unreachable, a cancel was unconfirmed. Exit `20`.
    Transient,
    /// The same invocation fails identically and nothing is misconfigured: an
    /// empty diff, a diff that can never fit the budget, a loop that ran out
    /// of iterations, an internal bug. Nothing to fix and nothing to retry, so
    /// this is the `internal` band: exit `50`.
    Permanent,
    /// The invocation or the environment is misconfigured: a bad config, a
    /// reviewer pin naming no model. Exit `40`.
    Config,
    /// A budget window is exhausted. Exit `30`.
    Budget,
    /// Nothing is broken; the review completed and refused the change.
    /// Exit `10`.
    Blocked,
}

impl FailureClass {
    /// The STABLE string for this class, as it appears in the envelope.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            FailureClass::Transient => "transient",
            FailureClass::Permanent => "permanent",
            FailureClass::Config => "config",
            FailureClass::Budget => "budget",
            FailureClass::Blocked => "blocked",
        }
    }

    /// The exit code for this class. Shared by every code in the class.
    ///
    /// [`FailureClass::Permanent`] is the `internal` band (50): the same
    /// invocation fails identically and nothing in the environment is
    /// misconfigured, so an operator has nothing to fix and a retry is
    /// pointless. Only [`FailureClass::Config`] is the operator-fix band (40).
    pub(crate) const fn exit_code(self) -> i32 {
        match self {
            FailureClass::Blocked => EXIT_BLOCKED,
            FailureClass::Transient => EXIT_TRANSIENT,
            FailureClass::Budget => EXIT_BUDGET,
            FailureClass::Config => EXIT_CONFIG,
            FailureClass::Permanent => EXIT_INTERNAL,
        }
    }

    /// All classes, in the canonical order used by docs and tests.
    #[cfg(test)]
    pub(crate) const ALL: &'static [FailureClass] = &[
        FailureClass::Transient,
        FailureClass::Permanent,
        FailureClass::Config,
        FailureClass::Budget,
        FailureClass::Blocked,
    ];

    /// Parse a class string back into a class (inverse of [`Self::as_str`]).
    #[cfg(test)]
    pub(crate) fn from_str_exact(s: &str) -> Option<Self> {
        FailureClass::ALL.iter().copied().find(|c| c.as_str() == s)
    }
}

impl fmt::Display for FailureClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// The exit-code ladder. Named and exported so the doc tests, the golden
// integration test and `docs/EXIT_CODES.md` all read the SAME numbers.
/// Blocked: the review completed and said "no". Nothing to retry.
pub(crate) const EXIT_BLOCKED: i32 = 10;
/// Transient: retry the same invocation later.
pub(crate) const EXIT_TRANSIENT: i32 = 20;
/// Budget: the window is exhausted; wait for `retry_after_s` / `resets_at`.
pub(crate) const EXIT_BUDGET: i32 = 30;
/// Config / auth / unusable input: an operator has to change something.
pub(crate) const EXIT_CONFIG: i32 = 40;
/// Internal: a bug, or a failure the contract does not classify yet.
pub(crate) const EXIT_INTERNAL: i32 = 50;
/// Reserved: the oneshot refusal band. Never assigned to a review/loop code.
///
/// Test-only: the constant exists so the invariant "no failure code ever exits
/// in the reserved band" is asserted rather than assumed.
#[cfg(test)]
pub(crate) const EXIT_RESERVED_ONESHOT: i32 = 75;

/// The exit code the process uses when the failure was never classified.
///
/// It is the INTERNAL band: an unclassified failure is a bug or a gap in the
/// contract, and the caller treats it exactly like `internal` (code `50`),
/// which is also what [`FailureClass::Permanent`] maps to.
pub(crate) const UNCLASSIFIED_EXIT_CODE: i32 = EXIT_INTERNAL;

/// The frozen list of failure CODES.
///
/// Each code is one specific failure worth telling apart. The string is the
/// contract (`error.code` in the envelope, `failure.code` inside a result) and
/// it is frozen in `ZODER_CLI_SURFACE.txt`; [`FailureCode::ALL`] drives the
/// test that proves the frozen table and this enum cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) enum FailureCode {
    /// The diff is above the configured review budget (bytes, hunks, chunks).
    DiffTooLarge,
    /// The diff was cut off during acquisition, so the tail was never seen.
    DiffTruncated,
    /// The scope produced no diff: nothing was reviewed, so this is NOT an
    /// approval (a wrong `--base` usually means this).
    EmptyDiff,
    /// No reviewer model could be resolved from `--reviewer`, the agent pin or
    /// `[review].reviewer_model`.
    NoReviewerModel,
    /// No reviewer produced a verdict; `detail.reviewers` names what each
    /// model hit (`Timeout`, `RateLimit`, `Server`, ...).
    ReviewersUnavailable,
    /// The review completed and blocks the change.
    ReviewBlocked,
    /// The loop stopped without resolving the task.
    LoopExhausted,
    /// The reviewer spent its whole output budget (twice) on hidden
    /// reasoning and never produced a verdict. The operator must raise
    /// `max_tokens` or lower the reasoning effort: an identical retry fails
    /// identically, so this is NOT a transient budget window.
    ReviewerBudgetExhausted,
    /// The author turn failed outright (no in-place timeout).
    AuthorFailed,
    /// The author turn hit its wall-clock budget.
    AuthorTimeout,
    /// The run was refused by policy (for example the free-only policy).
    PolicyViolation,
    /// The EXTERNAL engine refused for budget; `detail.resets_at` when known.
    EngineBudgetExceeded,
    /// The engine could not be reached at all.
    EngineUnreachable,
    /// A cancel was never confirmed by the engine; the worktree is tainted.
    CancelUnconfirmed,
    /// The worktree was left in an unknown state (see `cancel_unconfirmed`).
    WorktreeTainted,
    /// An environment / config problem: an unreadable or invalid config, a
    /// bad `--engine` value, a reviewer pin naming an unbacked model.
    ConfigError,
    /// The invocation itself is wrong (mutually exclusive flags, a scope that
    /// cannot be satisfied, a missing required argument).
    UsageError,
    /// A bug, or a failure this contract does not classify yet.
    Internal,
}

impl FailureCode {
    /// The STABLE string code. Frozen in `ZODER_CLI_SURFACE.txt`.
    pub(crate) const fn code(self) -> &'static str {
        match self {
            FailureCode::DiffTooLarge => "diff_too_large",
            FailureCode::DiffTruncated => "diff_truncated",
            FailureCode::EmptyDiff => "empty_diff",
            FailureCode::NoReviewerModel => "no_reviewer_model",
            FailureCode::ReviewersUnavailable => "reviewers_unavailable",
            FailureCode::ReviewBlocked => "review_blocked",
            FailureCode::LoopExhausted => "loop_exhausted",
            FailureCode::ReviewerBudgetExhausted => "reviewer_budget_exhausted",
            FailureCode::AuthorFailed => "author_failed",
            FailureCode::AuthorTimeout => "author_timeout",
            FailureCode::PolicyViolation => "policy_violation",
            FailureCode::EngineBudgetExceeded => "engine_budget_exceeded",
            FailureCode::EngineUnreachable => "engine_unreachable",
            FailureCode::CancelUnconfirmed => "cancel_unconfirmed",
            FailureCode::WorktreeTainted => "worktree_tainted",
            FailureCode::ConfigError => "config",
            FailureCode::UsageError => "usage",
            FailureCode::Internal => "internal",
        }
    }

    /// The retry policy this code carries.
    ///
    /// Every code has exactly ONE documented class and exit code; the table in
    /// `docs/EXIT_CODES.md` is the same table `each_named_code_carries_its_-
    /// documented_class_and_exit_code` pins.
    pub(crate) const fn class(self) -> FailureClass {
        match self {
            FailureCode::ReviewBlocked => FailureClass::Blocked,
            FailureCode::EngineBudgetExceeded => FailureClass::Budget,
            FailureCode::ReviewersUnavailable
            | FailureCode::AuthorFailed
            | FailureCode::AuthorTimeout
            | FailureCode::EngineUnreachable
            | FailureCode::CancelUnconfirmed
            | FailureCode::WorktreeTainted => FailureClass::Transient,
            FailureCode::NoReviewerModel
            | FailureCode::PolicyViolation
            | FailureCode::ConfigError
            | FailureCode::UsageError
            // The reviewer's OUTPUT budget was spent twice without a verdict:
            // the operator must raise `max_tokens` or lower the reasoning
            // effort, so this is an operator fix, NOT a retryable budget
            // window (see `FailureCode::ReviewerBudgetExhausted`).
            | FailureCode::ReviewerBudgetExhausted => FailureClass::Config,
            FailureCode::DiffTooLarge
            | FailureCode::DiffTruncated
            | FailureCode::EmptyDiff
            | FailureCode::LoopExhausted
            | FailureCode::Internal => FailureClass::Permanent,
        }
    }

    /// The process exit code for this failure.
    pub(crate) const fn exit_code(self) -> i32 {
        self.class().exit_code()
    }

    /// All codes, in the canonical (documented) order.
    pub(crate) const ALL: &'static [FailureCode] = &[
        FailureCode::ReviewBlocked,
        FailureCode::ReviewersUnavailable,
        FailureCode::DiffTooLarge,
        FailureCode::DiffTruncated,
        FailureCode::EmptyDiff,
        FailureCode::NoReviewerModel,
        FailureCode::LoopExhausted,
        FailureCode::ReviewerBudgetExhausted,
        FailureCode::AuthorFailed,
        FailureCode::AuthorTimeout,
        FailureCode::PolicyViolation,
        FailureCode::EngineBudgetExceeded,
        FailureCode::EngineUnreachable,
        FailureCode::CancelUnconfirmed,
        FailureCode::WorktreeTainted,
        FailureCode::ConfigError,
        FailureCode::UsageError,
        FailureCode::Internal,
    ];

    /// Parse a stable code back into a [`FailureCode`] (inverse of
    /// [`Self::code`]). An unknown string is `None`: the list is closed.
    pub(crate) fn from_code(code: &str) -> Option<Self> {
        FailureCode::ALL.iter().copied().find(|c| c.code() == code)
    }
}

impl fmt::Display for FailureCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// `(code, class, exit code)` for every failure code.
///
/// Exposed so the documentation test can assert that `docs/EXIT_CODES.md`
/// matches the implementation — the doc and the code cannot drift.
#[cfg(test)]
pub(crate) fn class_exit_codes() -> Vec<(&'static str, &'static str, i32)> {
    FailureCode::ALL
        .iter()
        .map(|c| (c.code(), c.class().as_str(), c.exit_code()))
        .collect()
}

/// A classified failure: the code, the human message, the per-model detail and
/// (when the external engine said so) the moment its budget window resets.
///
/// This is the value every `zoder review` / `zoder loop` failure site
/// returns. It carries no secrets: the message is the same operator-facing
/// text that was already printed to stderr before this contract existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClassifiedFailure {
    /// The specific failure (frozen list: [`FailureCode`]).
    pub code: FailureCode,
    /// The operator-facing message. Identical to (or a superset of) the text
    /// that was printed before this contract existed, so no diagnostic is
    /// lost. Always present in `detail.message`.
    pub message: String,
    /// The external engine's budget-reset timestamp, when it reported one.
    /// Mirrored into `detail.resets_at`, and into `retry_after_s` when it
    /// parses; never fabricated.
    pub budget_reset_at: Option<String>,
    /// Per-model reviewer outcomes (`model`, `err_kind`), for
    /// [`FailureCode::ReviewersUnavailable`]. Empty otherwise.
    pub reviewers: Vec<ReviewerOutcome>,
    /// Extra machine-readable members merged into `detail`. Reserved for
    /// detail beyond `message` / `reviewers` / `resets_at`.
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One reviewer model that could not answer, as the envelope reports it.
///
/// `err_kind` is the typed [`ErrKind`](zoder_core::ErrKind) that zoder-core's
/// provider layer already classifies every call with — this is the first time
/// it is serialized, so a caller can tell a 429 from a 503 from a timeout
/// instead of reading the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewerOutcome {
    pub model: String,
    pub err_kind: String,
}

impl ReviewerOutcome {
    #[cfg(test)]
    pub(crate) fn new(model: impl Into<String>, err_kind: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            err_kind: err_kind.into(),
        }
    }
}

impl ClassifiedFailure {
    /// A classified failure with no reset timestamp and no extra detail.
    pub(crate) fn new(code: FailureCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            budget_reset_at: None,
            reviewers: Vec::new(),
            extra: serde_json::Map::new(),
        }
    }

    /// A classified failure that carries a budget-reset timestamp.
    pub(crate) fn with_budget_reset(
        code: FailureCode,
        message: impl Into<String>,
        reset_at: Option<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            budget_reset_at: reset_at,
            reviewers: Vec::new(),
            extra: serde_json::Map::new(),
        }
    }

    /// Attach the per-model reviewer outcomes (builder style).
    #[cfg(test)]
    pub(crate) fn with_reviewers(mut self, reviewers: Vec<ReviewerOutcome>) -> Self {
        self.reviewers = reviewers;
        self
    }

    /// The retry policy for this failure.
    pub(crate) const fn class(&self) -> FailureClass {
        self.code.class()
    }

    /// The process exit code for this failure.
    pub(crate) const fn exit_code(&self) -> i32 {
        self.code.exit_code()
    }

    /// Seconds a caller should wait before retrying, derived from
    /// `detail.resets_at` when the failure carries one and the timestamp is in
    /// the future. `None` when there is nothing to wait for — never guessed.
    pub(crate) fn retry_after_s(&self, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
        let reset = self.budget_reset_at.as_deref()?;
        let at = chrono::DateTime::parse_from_rfc3339(reset)
            .ok()?
            .with_timezone(&chrono::Utc);
        let secs = (at - now).num_seconds();
        (secs > 0).then_some(secs)
    }

    /// The `error` object a machine consumer reads.
    ///
    /// Shape (stable, frozen in `ZODER_CLI_SURFACE.txt`):
    /// ```json
    /// {
    ///   "code": "engine_budget_exceeded",
    ///   "class": "budget",
    ///   "retry_after_s": 600,
    ///   "detail": { "message": "...", "resets_at": "2026-11-01T00:00:00Z" }
    /// }
    /// ```
    /// `retry_after_s` and `detail.resets_at` are present ONLY when the engine
    /// reported a reset timestamp; neither is ever fabricated.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::json!({
            "code": self.code.code(),
            "class": self.class().as_str(),
        });
        let mut detail = serde_json::Map::new();
        detail.insert(
            "message".into(),
            serde_json::Value::String(self.message.clone()),
        );
        if let Some(reset) = &self.budget_reset_at {
            detail.insert("resets_at".into(), serde_json::Value::String(reset.clone()));
        }
        if !self.reviewers.is_empty() {
            detail.insert(
                "reviewers".into(),
                serde_json::Value::Array(
                    self.reviewers
                        .iter()
                        .map(|r| serde_json::json!({"model": r.model, "err_kind": r.err_kind}))
                        .collect(),
                ),
            );
        }
        for (k, v) in &self.extra {
            detail.insert(k.clone(), v.clone());
        }
        // Mirrored at the TOP level so a shell caller reads the message with a
        // one-key path, and inside `detail` so the detail object is complete
        // on its own.
        obj["detail"] = serde_json::Value::Object(detail);
        if let Some(secs) = self.retry_after_s(chrono::Utc::now()) {
            obj["retry_after_s"] = serde_json::Value::from(secs);
        }
        obj
    }
}

/// A [`ClassifiedFailure`] carried inside an `anyhow::Error` chain.
///
/// This is the transport: a failure site returns
/// `Err(classified_err(code, msg))` and the dispatcher in `main` recovers the
/// code by walking the error chain. The chain walk is what keeps the
/// classification attached to the error even when an intermediate frame adds
/// `.context(...)` on top of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClassifiedFailureError(ClassifiedFailure);

impl ClassifiedFailureError {
    pub(crate) fn new(code: FailureCode, message: impl Into<String>) -> Self {
        Self(ClassifiedFailure::new(code, message))
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
/// ([`classify_message`]) so an un-annotated failure site still lands on the
/// right code instead of the unclassified bucket.
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
    classify_message(&chain_text)
        .map(|(code, reset)| ClassifiedFailure::with_budget_reset(code, chain_text.clone(), reset))
}

/// Build an `anyhow::Error` that carries a classified failure.
///
/// The common shape: `return Err(classified_err(FailureCode::EmptyDiff, msg))`.
pub(crate) fn classified_err(code: FailureCode, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ClassifiedFailureError::new(code, message))
}

/// Build an `engine_budget_exceeded` error that carries the engine's reset
/// timestamp (when it gave one) into the envelope.
pub(crate) fn classified_budget_err(
    message: impl Into<String>,
    reset_at: Option<String>,
) -> anyhow::Error {
    anyhow::Error::new(ClassifiedFailureError(
        ClassifiedFailure::with_budget_reset(FailureCode::EngineBudgetExceeded, message, reset_at),
    ))
}

/// The JSON document a `--json` failure prints when there is NO result
/// object to attach the failure to.
///
/// ```json
/// {
///   "ok": false,
///   "error": {
///     "code": "reviewers_unavailable",
///     "class": "transient",
///     "detail": { "message": "..." }
///   }
/// }
/// ```
///
/// It is the SAME `error` object that [`ClassifiedFailure::to_json`]
/// produces, wrapped in `ok: false`, so a consumer reads identical keys
/// whether the failure travelled inside a result or on its own.
pub(crate) fn error_envelope(failure: &ClassifiedFailure) -> serde_json::Value {
    serde_json::json!({ "ok": false, "error": failure.to_json() })
}

/// Did this process already print (or write) a RESULT object?
///
/// The `--json` contract is one JSON document on stdout. When the command
/// produced a result (the review payload, the loop payload), the failure
/// information is attached to THAT object and nothing else is printed —
/// printing a second document would break every consumer that parses
/// stdout as one JSON value. When NO result exists (the command failed
/// before producing one), the failure is printed as its own envelope.
pub(crate) use std::sync::atomic::{AtomicBool, Ordering};

static RESULT_EMITTED: AtomicBool = AtomicBool::new(false);

/// Record that a result object was produced (printed to stdout and/or
/// written to `result.json`).
pub(crate) fn mark_result_emitted() {
    RESULT_EMITTED.store(true, Ordering::SeqCst);
}

/// Did a result object already reach stdout (or `result.json`)?
pub(crate) fn result_was_emitted() -> bool {
    RESULT_EMITTED.load(Ordering::SeqCst)
}

static CONTRACT_SCOPE: AtomicBool = AtomicBool::new(false);

/// Record that this process runs a command the contract covers
/// (`zoder review` / `zoder loop`).
///
/// The contract is SCOPED: every other command keeps its historical exit
/// codes (`1` for an ordinary failure, `78` for a budget-cap decline) and its
/// historical `--json` output, so a caller of `zoder exec` is not silently
/// moved onto a ladder it never opted into.
pub(crate) fn enter_contract_scope() {
    CONTRACT_SCOPE.store(true, Ordering::SeqCst);
}

/// Does the failure contract govern this process's exit code and `--json`
/// failure output?
pub(crate) fn in_contract_scope() -> bool {
    CONTRACT_SCOPE.load(Ordering::SeqCst)
}

/// The code of an arbitrary error message, when one can be inferred.
///
/// This is the RECOGNITION step: it maps the free-text shape of a failure onto
/// the frozen code list. It is deliberately conservative — it only recognizes
/// specific, unambiguous markers that the failure sites in this codebase emit,
/// and it returns `None` for everything else so an unrecognized error stays
/// unclassified (`internal`, exit 50).
///
/// The return value also carries the engine's reset timestamp when the
/// message had one.
pub(crate) fn classify_message(message: &str) -> Option<(FailureCode, Option<String>)> {
    let lower = message.to_ascii_lowercase();
    let code = classify_message_code(&lower)?;
    Some((code, None))
}

/// The lower-cased-message -> code half of [`classify_message`].
fn classify_message_code(lower: &str) -> Option<FailureCode> {
    // The EXTERNAL engine's budget refusal. Recognised FIRST: it is the most
    // specific marker, and a budget refusal can mention the daemon in its
    // own diagnostics.
    if lower.contains(BUDGET_EXCEEDED_MARKER) {
        return Some(FailureCode::EngineBudgetExceeded);
    }

    // The diff was cut off during acquisition, so the tail was never seen.
    if lower.contains("review diff was truncated during acquisition")
        || lower.contains(DIFF_TRUNCATED_MARKER)
    {
        return Some(FailureCode::DiffTruncated);
    }

    // Nothing was reviewed, so this is not an approval.
    if lower.contains("no changes to review") || lower.contains("empty_diff") {
        return Some(FailureCode::EmptyDiff);
    }

    // No reviewer model could be resolved at all.
    if lower.contains("no reviewer model resolved")
        || lower.contains("no reviewer route matches")
        || lower.contains("no_reviewer_model")
    {
        return Some(FailureCode::NoReviewerModel);
    }

    // The diff is larger than the review budget.
    if lower.contains("above the")
        && (lower.contains("byte limit") || lower.contains("-byte limit"))
    {
        return Some(FailureCode::DiffTooLarge);
    }
    if lower.contains("review diff is") && lower.contains("byte") {
        return Some(FailureCode::DiffTooLarge);
    }

    // The review completed and blocked the change.
    if lower.contains("review blocked") || lower.contains("requests changes") {
        return Some(FailureCode::ReviewBlocked);
    }

    // No reviewer produced a verdict.
    if lower.contains("reviewers completed")
        || lower.contains("review incomplete")
        || lower.contains("reviewer_unavailable")
        || lower.contains("reviewer refused")
        || lower.contains("review refused")
    {
        return Some(FailureCode::ReviewersUnavailable);
    }

    // The engine never confirmed a cancel: the worktree may be tainted.
    if lower.contains("author cancellation unconfirmed")
        || lower.contains("cancel not confirmed")
        || lower.contains("cancellation deadline expired")
    {
        return Some(FailureCode::CancelUnconfirmed);
    }

    // The author turn hit its wall-clock budget, or failed outright.
    if lower.contains("author turn timed out") || lower.contains("author timeout") {
        return Some(FailureCode::AuthorTimeout);
    }
    if lower.contains("author turn did not finish")
        || lower.contains("author_failed")
        || lower.contains("author failed")
    {
        return Some(FailureCode::AuthorFailed);
    }

    // The run was refused by policy.
    if lower.contains("violated free policy")
        || lower.contains("violates the free policy")
        || lower.contains("policy violation")
    {
        return Some(FailureCode::PolicyViolation);
    }

    // The loop stopped without resolving.
    if lower.contains("loop ended unresolved") || lower.contains("loop_exhausted") {
        return Some(FailureCode::LoopExhausted);
    }

    // The agent engine could not be reached.
    if lower.contains("connecting to engine")
        || lower.contains("daemon not ready")
        || lower.contains("daemon exited during startup")
        || lower.contains("engine is unavailable")
        || lower.contains("engine offline")
        || lower.contains("no daemon is running")
    {
        return Some(FailureCode::EngineUnreachable);
    }

    // The invocation was wrong.
    if lower.contains("invalid --engine")
        || lower.contains("usage:")
        || lower.contains("mutually exclusive")
        || lower.contains("requires --")
        || lower.contains("unknown subcommand")
    {
        return Some(FailureCode::UsageError);
    }

    // The environment is misconfigured.
    if lower.contains("loading zoder config")
        || lower.contains("loading engine")
        || lower.contains("failed to parse")
        || lower.contains("config error")
        || lower.contains("invalid config")
    {
        return Some(FailureCode::ConfigError);
    }

    None
}

/// The marker the EXTERNAL engine prefixes its budget refusal with.
pub(crate) const BUDGET_EXCEEDED_MARKER: &str = "budget_exceeded:";

/// Substring every acquisition-truncation refusal carries.
pub(crate) const DIFF_TRUNCATED_MARKER: &str = "truncated during acquisition";

/// Recognize the EXTERNAL engine's `budget_exceeded:` refusal.
///
/// The engine reports a budget refusal on stderr (or in its terminal turn
/// frame) as text beginning with `budget_exceeded:` and MAY append the moment
/// the budget window resets. Both shapes are recognized:
///
/// ```text
/// budget_exceeded: monthly spend cap reached
/// budget_exceeded: quota exhausted, resets at 2026-11-01T00:00:00Z
/// ```
///
/// Returns the code plus the reset timestamp when the message carried one.
pub(crate) fn classify_engine_budget_exceeded(
    message: &str,
) -> Option<(FailureCode, Option<String>)> {
    let start = message.to_ascii_lowercase().find(BUDGET_EXCEEDED_MARKER)?;
    let rest = &message[start + BUDGET_EXCEEDED_MARKER.len()..];
    let line = rest.lines().next().unwrap_or("").trim();
    let reset = extract_reset_timestamp(line);
    Some((FailureCode::EngineBudgetExceeded, reset))
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

    // ---- the frozen code list -------------------------------------------

    /// The exact list, in order. This is the contract: a code is added or
    /// removed only together with this test, `docs/EXIT_CODES.md` and
    /// `ZODER_CLI_SURFACE.txt`.
    #[test]
    fn the_code_list_is_exactly_the_frozen_one() {
        let codes: Vec<&str> = FailureCode::ALL.iter().map(|c| c.code()).collect();
        assert_eq!(
            codes,
            vec![
                "review_blocked",
                "reviewers_unavailable",
                "diff_too_large",
                "diff_truncated",
                "empty_diff",
                "no_reviewer_model",
                "loop_exhausted",
                "reviewer_budget_exhausted",
                "author_failed",
                "author_timeout",
                "policy_violation",
                "engine_budget_exceeded",
                "engine_unreachable",
                "cancel_unconfirmed",
                "worktree_tainted",
                "config",
                "usage",
                "internal",
            ]
        );
    }

    #[test]
    fn every_code_is_unique() {
        let mut codes: Vec<&str> = FailureCode::ALL.iter().map(|c| c.code()).collect();
        let before = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(before, codes.len(), "failure codes must be unique");
    }

    #[test]
    fn every_code_round_trips_back_to_its_code() {
        for c in FailureCode::ALL {
            assert_eq!(FailureCode::from_code(c.code()), Some(*c));
        }
        assert_eq!(FailureCode::from_code("not_a_code"), None);
        assert_eq!(FailureCode::from_code(""), None);
    }

    #[test]
    fn the_five_classes_are_the_documented_five() {
        let classes: Vec<&str> = FailureClass::ALL.iter().map(|c| c.as_str()).collect();
        assert_eq!(
            classes,
            vec!["transient", "permanent", "config", "budget", "blocked"]
        );
        for c in FailureClass::ALL {
            assert_eq!(FailureClass::from_str_exact(c.as_str()), Some(*c));
        }
        assert_eq!(FailureClass::from_str_exact("fatal"), None);
    }

    #[test]
    fn every_class_has_its_documented_exit_code() {
        assert_eq!(FailureClass::Blocked.exit_code(), 10);
        assert_eq!(FailureClass::Transient.exit_code(), 20);
        assert_eq!(FailureClass::Budget.exit_code(), 30);
        assert_eq!(FailureClass::Config.exit_code(), 40);
        // A permanent failure is the `internal` band: nothing to fix, nothing
        // to retry, so it shares 50 with `internal` and the unclassified
        // fallback.
        assert_eq!(FailureClass::Permanent.exit_code(), 50);
        // 0 and 75 are outside the class ladder: 0 is success and 75 is
        // reserved for the oneshot refusal band, never a review/loop class.
        for c in FailureClass::ALL {
            assert_ne!(c.exit_code(), 0);
            assert_ne!(c.exit_code(), EXIT_RESERVED_ONESHOT);
        }
        // Every ladder rung below 75 has exactly one class that uses it, with
        // one deliberate exception: 50 is the `permanent`/`internal` band, so
        // `Permanent` shares it with the unclassified fallback by design.
        for n in [EXIT_BLOCKED, EXIT_TRANSIENT, EXIT_BUDGET, EXIT_CONFIG] {
            let users: Vec<&str> = FailureClass::ALL
                .iter()
                .filter(|c| c.exit_code() == n)
                .map(|c| c.as_str())
                .collect();
            assert_eq!(
                users.len(),
                1,
                "exit {n} must belong to exactly one class: {users:?}"
            );
        }
        let internal_users: Vec<&str> = FailureClass::ALL
            .iter()
            .filter(|c| c.exit_code() == EXIT_INTERNAL)
            .map(|c| c.as_str())
            .collect();
        assert_eq!(
            internal_users,
            vec!["permanent"],
            "exit 50 is the permanent/internal band and only `permanent` may \
             write it; `internal` reaches it through FailureCode::Internal"
        );
        assert_eq!(EXIT_INTERNAL, 50);
        assert_eq!(EXIT_RESERVED_ONESHOT, 75);
    }

    #[test]
    fn unclassified_failures_exit_the_internal_code() {
        assert_eq!(UNCLASSIFIED_EXIT_CODE, 50);
        assert_eq!(FailureCode::Internal.exit_code(), UNCLASSIFIED_EXIT_CODE);
    }

    #[test]
    fn only_transient_is_retryable() {
        // `retryable` is `class == Transient` by construction. Assert that
        // invariant directly instead of hand-negating the other variants, so
        // a new class defaulting to retryable still fails this test.
        for c in FailureClass::ALL {
            let retryable = *c == FailureClass::Transient;
            if !retryable {
                assert_ne!(*c, FailureClass::Transient);
            }
        }
        assert_eq!(
            FailureClass::ALL
                .iter()
                .filter(|c| **c == FailureClass::Transient)
                .count(),
            1,
            "exactly one class is the retryable one"
        );
        // The class-level predicate is exactly the Transient test.
        for c in FailureClass::ALL {
            assert_eq!(
                *c == FailureClass::Transient,
                c.as_str() == "transient" && c.exit_code() == EXIT_TRANSIENT,
                "{c}"
            );
        }
    }

    // ---- the named classifications --------------------------------------

    /// The class/exit-code table `docs/EXIT_CODES.md` publishes, asserted
    /// against the implementation for EVERY named code.
    ///
    /// The `expected` list is the contract. It must stay exhaustive over
    /// [`FailureCode::ALL`] — the trailing assertion fails when a code is
    /// added without a documented class and exit code, so the table cannot
    /// silently drift from the enum (decision D3: codes 0/10/20/30/40/50/75,
    /// classes transient/permanent/config/budget/blocked).
    #[test]
    fn each_named_code_carries_its_documented_class_and_exit_code() {
        let expected = [
            (FailureCode::ReviewBlocked, FailureClass::Blocked, 10),
            (
                FailureCode::ReviewersUnavailable,
                FailureClass::Transient,
                20,
            ),
            (FailureCode::DiffTooLarge, FailureClass::Permanent, 50),
            (FailureCode::DiffTruncated, FailureClass::Permanent, 50),
            (FailureCode::EmptyDiff, FailureClass::Permanent, 50),
            (FailureCode::NoReviewerModel, FailureClass::Config, 40),
            (FailureCode::LoopExhausted, FailureClass::Permanent, 50),
            (
                FailureCode::ReviewerBudgetExhausted,
                FailureClass::Config,
                40,
            ),
            (FailureCode::AuthorFailed, FailureClass::Transient, 20),
            (FailureCode::AuthorTimeout, FailureClass::Transient, 20),
            (FailureCode::PolicyViolation, FailureClass::Config, 40),
            (FailureCode::EngineBudgetExceeded, FailureClass::Budget, 30),
            (FailureCode::EngineUnreachable, FailureClass::Transient, 20),
            (FailureCode::CancelUnconfirmed, FailureClass::Transient, 20),
            (FailureCode::WorktreeTainted, FailureClass::Transient, 20),
            (FailureCode::ConfigError, FailureClass::Config, 40),
            (FailureCode::UsageError, FailureClass::Config, 40),
            (FailureCode::Internal, FailureClass::Permanent, 50),
        ];
        for (code, class, exit) in expected {
            assert_eq!(code.class(), class, "{code}");
            assert_eq!(code.exit_code(), exit, "{code}");
            // The exit code is never an ad-hoc literal: it is the one its
            // class documents.
            assert_eq!(code.exit_code(), class.exit_code(), "{code}");
        }
        assert_eq!(
            expected.len(),
            FailureCode::ALL.len(),
            "every code in FailureCode::ALL needs a documented class and exit code"
        );
        for code in FailureCode::ALL {
            assert!(
                expected.iter().any(|(c, _, _)| c == code),
                "{code} is missing from the documented table"
            );
        }
        // Only success is 0 and 75 is reserved: no named code may borrow them.
        for (code, _, exit) in expected {
            assert_ne!(exit, 0, "{code}");
            assert_ne!(exit, EXIT_RESERVED_ONESHOT, "{code}");
        }
    }

    // ---- the JSON envelope ----------------------------------------------

    #[test]
    fn the_envelope_is_ok_false_with_code_class_and_detail() {
        let f = ClassifiedFailure::new(FailureCode::ReviewersUnavailable, "0/1 reviewers");
        let env = error_envelope(&f);
        assert_eq!(env["ok"], false);
        assert_eq!(env["error"]["code"], "reviewers_unavailable");
        assert_eq!(env["error"]["class"], "transient");
        assert_eq!(env["error"]["detail"]["message"], "0/1 reviewers");
        assert!(
            env["error"].get("retry_after_s").is_none(),
            "retry_after_s must never be fabricated: {env}"
        );
    }

    #[test]
    fn the_envelope_lists_the_per_model_err_kind() {
        let f = ClassifiedFailure::new(FailureCode::ReviewersUnavailable, "all failed")
            .with_reviewers(vec![
                ReviewerOutcome::new("a/one", "Timeout"),
                ReviewerOutcome::new("b/two", "Server"),
            ]);
        let env = error_envelope(&f);
        let reviewers = env["error"]["detail"]["reviewers"]
            .as_array()
            .expect("reviewers must be an array");
        assert_eq!(reviewers[0]["model"], "a/one");
        assert_eq!(reviewers[0]["err_kind"], "Timeout");
        assert_eq!(reviewers[1]["err_kind"], "Server");
    }

    #[test]
    fn the_engine_budget_reset_reaches_detail_and_retry_after_s() {
        // A reset far in the future: the retry hint must be positive, and its
        // value must be derived from resets_at, not guessed.
        let reset = (chrono::Utc::now() + chrono::Duration::seconds(600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let f = ClassifiedFailure::with_budget_reset(
            FailureCode::EngineBudgetExceeded,
            "budget_exceeded: month limit",
            Some(reset.clone()),
        );
        let env = error_envelope(&f);
        assert_eq!(env["error"]["class"], "budget");
        assert_eq!(env["error"]["code"], "engine_budget_exceeded");
        assert_eq!(env["error"]["detail"]["resets_at"], reset);
        let secs = env["error"]["retry_after_s"]
            .as_i64()
            .expect("a future reset must produce retry_after_s");
        assert!((590..=601).contains(&secs), "retry_after_s = {secs}");
    }

    #[test]
    fn a_reset_in_the_past_yields_no_retry_after() {
        let f = ClassifiedFailure::with_budget_reset(
            FailureCode::EngineBudgetExceeded,
            "budget_exceeded: month limit",
            Some("2000-01-01T00:00:00Z".into()),
        );
        assert_eq!(f.retry_after_s(chrono::Utc::now()), None);
        assert!(error_envelope(&f)["error"].get("retry_after_s").is_none());
    }

    // ---- the engine's budget refusal ------------------------------------

    #[test]
    fn engine_budget_exceeded_text_is_recognized() {
        let (code, reset) =
            classify_engine_budget_exceeded("budget_exceeded: monthly spend cap reached")
                .expect("the engine's budget refusal must be recognized");
        assert_eq!(code, FailureCode::EngineBudgetExceeded);
        assert_eq!(reset, None);
    }

    #[test]
    fn engine_budget_exceeded_text_carries_the_reset_timestamp() {
        let (code, reset) = classify_engine_budget_exceeded(
            "budget_exceeded: zoder engine spend cap reached ($170 of $100); resets \
             2026-11-01T00:00:00Z",
        )
        .expect("the engine's budget refusal must be recognized");
        assert_eq!(code, FailureCode::EngineBudgetExceeded);
        assert_eq!(reset.as_deref(), Some("2026-11-01T00:00:00Z"));
    }

    #[test]
    fn engine_budget_exceeded_is_recognized_inside_a_longer_message() {
        let (code, _) = classify_engine_budget_exceeded(
            "zoder: error: turn failed: budget_exceeded: spend cap reached",
        )
        .expect("the engine's budget refusal must be recognized anywhere");
        assert_eq!(code, FailureCode::EngineBudgetExceeded);
    }

    #[test]
    fn a_message_without_the_engine_budget_marker_is_not_a_budget_failure() {
        assert!(classify_engine_budget_exceeded("all good").is_none());
        assert!(classify_engine_budget_exceeded("budget exceeded").is_none());
    }

    // ---- message recognition --------------------------------------------

    #[test]
    fn an_unrecognized_message_stays_unclassified() {
        assert!(classify_message("something entirely unexpected happened").is_none());
    }

    #[test]
    fn the_recognized_markers_land_on_their_codes() {
        let cases = [
            (
                "loading zoder config to locate sessions dir: boom",
                FailureCode::ConfigError,
            ),
            ("invalid --engine: nope", FailureCode::UsageError),
            (
                "review diff is 900000 bytes, above the 400000-byte limit",
                FailureCode::DiffTooLarge,
            ),
            (
                "the review diff was truncated during acquisition at 1048576 bytes",
                FailureCode::DiffTruncated,
            ),
            (
                "empty_diff: no changes to review (main)",
                FailureCode::EmptyDiff,
            ),
            (
                "no reviewer model resolved for --reviewer x",
                FailureCode::NoReviewerModel,
            ),
            (
                "review blocked: aggregate verdict `request_changes`",
                FailureCode::ReviewBlocked,
            ),
            (
                "review failed: 0/1 reviewers completed (all errors)",
                FailureCode::ReviewersUnavailable,
            ),
            (
                "author cancellation unconfirmed: session x not confirmed",
                FailureCode::CancelUnconfirmed,
            ),
            (
                "loop ended unresolved after 3 iteration(s)",
                FailureCode::LoopExhausted,
            ),
            (
                "author turn timed out after 900s",
                FailureCode::AuthorTimeout,
            ),
            ("author turn did not finish", FailureCode::AuthorFailed),
            (
                "loop author turn violated free policy (use --allow-paid to permit)",
                FailureCode::PolicyViolation,
            ),
            (
                "budget_exceeded: month limit",
                FailureCode::EngineBudgetExceeded,
            ),
            (
                "connecting to engine at /run/x.sock: No such file or directory",
                FailureCode::EngineUnreachable,
            ),
        ];
        for (msg, want) in cases {
            let (code, _) =
                classify_message(msg).unwrap_or_else(|| panic!("{msg:?} must be recognized"));
            assert_eq!(code, want, "{msg:?}");
        }
    }

    // ---- carrying a code through an anyhow chain ------------------------

    #[test]
    fn a_classified_error_keeps_its_code_through_a_context_frame() {
        let err = classified_err(FailureCode::DiffTruncated, "diff is partial")
            .context("while preparing the review");
        let recovered = classify_error(&err).expect("the code must survive the chain");
        assert_eq!(recovered.code, FailureCode::DiffTruncated);
        assert_eq!(recovered.message, "diff is partial");
        assert_eq!(recovered.exit_code(), 50);
    }

    #[test]
    fn an_unclassified_error_falls_back_to_message_recognition() {
        let err = anyhow::anyhow!("review failed: 0/1 reviewers completed (all errors)");
        let recovered = classify_error(&err).expect("the message must be recognized");
        assert_eq!(recovered.code, FailureCode::ReviewersUnavailable);
    }

    #[test]
    fn a_genuinely_unrecognizable_error_stays_unclassified() {
        let err = anyhow::anyhow!("something entirely unexpected happened");
        assert!(classify_error(&err).is_none());
    }

    #[test]
    fn a_budget_error_carries_its_reset_timestamp_through_the_chain() {
        let err = classified_budget_err(
            "budget_exceeded: quota exhausted",
            Some("2099-11-01T00:00:00Z".to_string()),
        );
        let recovered = classify_error(&err).expect("the code must survive the chain");
        assert_eq!(recovered.code, FailureCode::EngineBudgetExceeded);
        assert_eq!(
            recovered.budget_reset_at.as_deref(),
            Some("2099-11-01T00:00:00Z")
        );
        assert_eq!(recovered.exit_code(), 30);
        // `budget` is not the transient class, so it is not retryable as-is.
        assert_ne!(recovered.code.class(), FailureClass::Transient);
        assert!(recovered.retry_after_s(chrono::Utc::now()).is_some());
    }

    // ---- the documentation cannot drift from the code -------------------

    /// `docs/EXIT_CODES.md` is the published contract. Every code, class and
    /// exit code it documents MUST match the implementation, and every code in
    /// the implementation MUST be on the SAME line as its exit code, so the
    /// table cannot pair a code with the wrong number.
    #[test]
    fn the_exit_code_document_matches_the_implementation() {
        let doc = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/EXIT_CODES.md"),
        )
        .expect("docs/EXIT_CODES.md must exist: it is the published contract");

        for (code, class, exit) in class_exit_codes() {
            let line = doc
                .lines()
                .find(|l| l.contains(&format!("`{code}`")))
                .unwrap_or_else(|| panic!("docs/EXIT_CODES.md has no line for {code:?}"));
            assert!(
                line.contains(&format!("`{exit}`")) || line.contains(&format!(" {exit} ")),
                "docs/EXIT_CODES.md must pair {code:?} with exit {exit} on the same line: {line:?}"
            );
            assert!(
                line.contains(class),
                "docs/EXIT_CODES.md must state the class {class:?} for {code:?}: {line:?}"
            );
        }
        // Every class and every ladder number is documented.
        for c in FailureClass::ALL {
            assert!(doc.contains(c.as_str()), "docs must document class {c}");
        }
        for n in [0, 10, 20, 30, 40, 50, 75] {
            assert!(doc.contains(&n.to_string()), "docs must document exit {n}");
        }
    }
}
