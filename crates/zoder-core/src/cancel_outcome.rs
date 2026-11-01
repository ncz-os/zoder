//! The ONE cancel policy, shared by the reaper and the loop.
//!
//! D11 step 10 / D12: the reaper (`crates/zoder-cli/src/reaper.rs`) and the
//! author phase (`crates/zoder-cli/src/agentic.rs`) used to answer "did the
//! cancel land?" in two different ways:
//!
//! * the reaper classified the transport error by walking the `io::ErrorKind`
//!   chain (`classify_cancel_error`), which distinguishes "there is no
//!   listener" from "the daemon did not answer";
//! * the loop keyed on a MARKER PREFIX in an error STRING
//!   (`AUTHOR_CANCEL_UNCONFIRMED_MARKER`), and separately on
//!   `msg.contains("timed out") || msg.contains("timeout")`.
//!
//! A string prefix is a control-flow channel built out of prose: any edit to
//! the message, any re-wording of an error, silently changes what the loop
//! does — and the failure mode is not a crash but a LOOP THAT REVIEWS A TREE
//! THE DAEMON MAY STILL BE WRITING. So the fact is carried by a TYPE here,
//! and the marker string stays only as the operator-facing rendering.
//!
//! [`CancelOutcome`] is that type. It is deliberately the smallest closed set
//! that answers the two questions the two callers actually ask:
//!
//! * did the daemon ACKNOWLEDGE the cancel? (`Confirmed`)
//! * if not, WHY NOT — is the engine gone, did it stay silent, or did our
//!   own budget expire before we could ask? (`EngineUnreachable`,
//!   `NotConfirmed`, `DeadlineExpired`)
//!
//! The three non-confirmed arms are different facts about the world and are
//! never logged, retried, or counted alike:
//!
//! * `EngineUnreachable` — the socket path is gone or nothing is listening
//!   on it. There is no engine to cancel against: the session died with the
//!   daemon that owned it, and retrying teaches us nothing.
//! * `NotConfirmed` — the engine WAS reached and did not confirm. The turn
//!   may still be running (a settled turn never acknowledges a cancel), so
//!   this is the arm that gets retried and eventually settles the session as
//!   "could not prove the turn stopped".
//! * `DeadlineExpired` — OUR budget ran out before the cancel could be
//!   delivered or confirmed. Nothing was learned about the daemon; this is
//!   about our clock, not about the engine.
//!
//! The reaper's own richer record ([`UnconfirmedCancelOutcome`]) stays in
//! `reaper.rs`: it carries the ATTEMPT COUNT, which is a fact about the
//! reaper's retry sequence and has no meaning in the loop. This module
//! carries the FACT; the reaper's type carries the count alongside it.

/// How a `session/cancel` attempt ended.
///
/// The closed set of facts the reaper and the author phase need in order to
/// decide what to do next. See the module docs for why this is a type and
/// not a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The daemon acknowledged the cancel. The turn is stopped.
    Confirmed,
    /// The engine was reached and did not acknowledge the cancel.
    ///
    /// This is the ordinary "a turn that already settled does not acknowledge
    /// a cancel" case, which may still be a live turn and is therefore
    /// retried. It is NOT proof that the turn is running, and it is NOT
    /// proof that it stopped.
    NotConfirmed,
    /// The socket path does not exist, or nothing is listening on it. There
    /// is no engine to cancel against, so the session is not running: it
    /// died with the daemon that owned it.
    ///
    /// Only ENOENT (the path is gone) and ECONNREFUSED (the entry is there
    /// but nobody is bound) prove this. Everything else — EACCES on
    /// somebody else's socket, a transport wrapper's own error, a connect
    /// timeout — leaves the engine unproven.
    EngineUnreachable,
    /// Our own budget expired before the cancel could be delivered or
    /// confirmed.
    ///
    /// Nothing was learned about the daemon: this is a fact about our clock,
    /// not about the engine. A cancel cut off before it reached the socket
    /// must never be reported as "the daemon stayed silent".
    DeadlineExpired,
}

impl CancelOutcome {
    /// True when the daemon acknowledged the cancel.
    pub fn is_confirmed(self) -> bool {
        matches!(self, CancelOutcome::Confirmed)
    }

    /// True when the cancel reached a daemon that did not acknowledge it.
    ///
    /// This is the ONLY arm that may be retried: the daemon is there, so a
    /// retry can change the answer. The other two are settled facts — the
    /// engine is gone, or our clock ran out.
    pub fn is_retryable(self) -> bool {
        matches!(self, CancelOutcome::NotConfirmed)
    }

    /// True when nothing of the session can still be running, because the
    /// engine that owned it is gone.
    pub fn is_engine_gone(self) -> bool {
        matches!(self, CancelOutcome::EngineUnreachable)
    }
}

impl std::fmt::Display for CancelOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            CancelOutcome::Confirmed => "confirmed",
            CancelOutcome::NotConfirmed => "not confirmed",
            CancelOutcome::EngineUnreachable => "engine unreachable",
            CancelOutcome::DeadlineExpired => "cancel deadline expired",
        };
        f.write_str(text)
    }
}

/// Does this connect error prove there is no listener, as opposed to merely
/// failing to reach one we can see?
///
/// Only ENOENT (the path is gone — the daemon's `EndpointGuard` unlinks it
/// when the daemon that bound it stops) and ECONNREFUSED (the entry is there
/// but nobody is bound) do. Everything else — EACCES on somebody else's
/// socket, a transport wrapper's own error, a connect timeout — leaves the
/// engine unproven, and the conservative answer is to keep treating the
/// session as possibly live.
pub fn io_error_proves_no_listener(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

/// Classify a `cancel_session` failure into a [`CancelOutcome`].
///
/// The `io::Error` is inside the anyhow chain (the transport adds
/// "connecting to engine at <path>" as context), so the chain is walked
/// rather than the top-level message pattern-matched: a message that merely
/// mentions a missing path is not proof, while the errno is.
///
/// This is the ONE place that decides "the engine is gone" — the reaper
/// used to have a private copy of it, and the loop had none at all.
pub fn classify_cancel_error(error: &anyhow::Error) -> CancelOutcome {
    for cause in error.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return if io_error_proves_no_listener(io) {
                CancelOutcome::EngineUnreachable
            } else {
                CancelOutcome::NotConfirmed
            };
        }
    }
    CancelOutcome::NotConfirmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_socket_proves_the_engine_is_gone() {
        let error = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(
            classify_cancel_error(&anyhow::Error::new(error)),
            CancelOutcome::EngineUnreachable
        );
    }

    #[test]
    fn a_refused_connection_proves_the_engine_is_gone() {
        let error = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert_eq!(
            classify_cancel_error(&anyhow::Error::new(error)),
            CancelOutcome::EngineUnreachable
        );
    }

    /// A permission error on somebody else's socket is NOT proof that there
    /// is no engine: the conservative answer keeps the session possibly live.
    #[test]
    fn a_permission_error_does_not_prove_the_engine_is_gone() {
        let error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            classify_cancel_error(&anyhow::Error::new(error)),
            CancelOutcome::NotConfirmed
        );
    }

    /// The io error is inside the anyhow CHAIN (the transport adds context),
    /// so the chain must be walked rather than the top-level message
    /// pattern-matched.
    #[test]
    fn the_errno_is_found_through_the_anyhow_chain() {
        let io = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let wrapped = anyhow::Error::new(io).context("connecting to engine at /tmp/x.sock");
        assert_eq!(
            classify_cancel_error(&wrapped),
            CancelOutcome::EngineUnreachable
        );
    }

    /// A message that merely MENTIONS a missing path is not proof. The
    /// errno is the proof.
    #[test]
    fn a_message_mentioning_a_missing_path_is_not_proof() {
        let error = anyhow::anyhow!("connecting to engine at /tmp/gone.sock failed");
        assert_eq!(
            classify_cancel_error(&error),
            CancelOutcome::NotConfirmed
        );
    }

    /// Only the "reached the daemon but it stayed silent" arm may be retried:
    /// the engine is there, so asking again can change the answer.
    #[test]
    fn only_not_confirmed_is_retryable() {
        assert!(CancelOutcome::NotConfirmed.is_retryable());
        assert!(!CancelOutcome::Confirmed.is_retryable());
        assert!(!CancelOutcome::EngineUnreachable.is_retryable());
        assert!(!CancelOutcome::DeadlineExpired.is_retryable());
    }

    #[test]
    fn only_confirmed_is_confirmed() {
        assert!(CancelOutcome::Confirmed.is_confirmed());
        assert!(!CancelOutcome::NotConfirmed.is_confirmed());
        assert!(!CancelOutcome::EngineUnreachable.is_confirmed());
        assert!(!CancelOutcome::DeadlineExpired.is_confirmed());
    }
}
