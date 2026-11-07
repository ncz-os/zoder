//! How long a `session/cancel` is given to be acknowledged.
//!
//! 2026-11-04 (three fleet runs in one day): a `zoder loop` whose author turn
//! hit the 900 s phase watchdog in the middle of a long tool step (a build or
//! a test run) sent `session/cancel` and waited the flat
//! [`crate::CANCEL_SETTLE_BUDGET`] (5 s) for the daemon's `turn_complete`,
//! three times over. The daemon does not interrupt a running tool: it marks
//! the turn cancelled and emits `turn_complete` when the step returns, which
//! for a build is tens of seconds later. Every attempt timed out, the loop
//! stopped on a shared daemon after the first unconfirmed cancel, and the
//! exit path then asked the same session again and printed the
//! `cancel not confirmed after 2 attempt(s)` line as the run's last words.
//!
//! The ack budget therefore scales with the step the daemon is known to be
//! in. The loop records when the in-flight tool call started (the ACP
//! `tool_call` / tool-result events it already streams), and the FIRST cancel
//! attempt waits at least as long as that step has already run, capped by
//! [`DEFAULT_CANCEL_ACK_CAP`] (`ZODER_CANCEL_ACK_CAP_S` overrides it; `0`
//! disables the scaling). A turn with no tool call in flight keeps today's
//! 5 s: a daemon that is only streaming text answers a cancel promptly, and
//! a healthy run never reaches this path at all.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Environment override for the ceiling of the step-scaled ack budget, in
/// whole seconds. `0` disables the scaling (every cancel gets the flat
/// settle budget, which is the pre-fix behaviour).
pub const CANCEL_ACK_CAP_ENV: &str = "ZODER_CANCEL_ACK_CAP_S";

/// The most a cancel waits for its ack when the daemon was mid-step, however
/// long that step has already run. One minute covers a build or a test run
/// without letting a wedged daemon hold the loop for the step's whole length.
pub const DEFAULT_CANCEL_ACK_CAP: Duration = Duration::from_secs(60);

/// The ceiling from the environment, or [`DEFAULT_CANCEL_ACK_CAP`].
pub fn cancel_ack_cap_from_env() -> Duration {
    std::env::var(CANCEL_ACK_CAP_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_CANCEL_ACK_CAP)
}

/// The ack budget for one `session/cancel` given how long the daemon's
/// in-flight tool step has been running (`None`: no tool call is pending).
///
/// * no pending step -> [`crate::CANCEL_SETTLE_BUDGET`], unchanged;
/// * a pending step -> at least the settle budget, at most `cap`, and
///   otherwise as long as the step has already taken: a step that has run
///   for 20 s is the kind of step that takes 20 s, and the daemon only
///   acknowledges once it returns.
///
/// A `cap` below the settle budget (including `0`) disables the scaling.
pub fn cancel_ack_budget(active_step: Option<Duration>, cap: Duration) -> Duration {
    let floor = crate::CANCEL_SETTLE_BUDGET;
    match active_step {
        None => floor,
        Some(elapsed) => elapsed.clamp(floor, cap.max(floor)),
    }
}

/// The tool step the current author turn is in, if any.
///
/// Process-wide, like the reaper's session list: the loop drives one author
/// turn at a time, and the watchdog that needs this value runs after the
/// turn's future has been dropped, so nothing else could carry it.
struct ToolStep {
    /// Tool calls streamed without a result yet.
    pending: u32,
    /// When the oldest still-pending tool call was streamed.
    started: Option<Instant>,
}

static TOOL_STEP: Mutex<ToolStep> = Mutex::new(ToolStep {
    pending: 0,
    started: None,
});

/// A new author turn is starting: nothing is pending yet.
pub fn turn_started() {
    let mut step = TOOL_STEP.lock().unwrap_or_else(|e| e.into_inner());
    step.pending = 0;
    step.started = None;
}

/// The engine streamed a `tool_call`: a step is now in flight.
pub fn tool_call_started() {
    let mut step = TOOL_STEP.lock().unwrap_or_else(|e| e.into_inner());
    step.pending = step.pending.saturating_add(1);
    if step.started.is_none() {
        step.started = Some(Instant::now());
    }
}

/// The engine streamed a tool result: one fewer step in flight. The clock is
/// cleared only when nothing is pending, so overlapping calls keep the age
/// of the oldest one.
pub fn tool_result_seen() {
    let mut step = TOOL_STEP.lock().unwrap_or_else(|e| e.into_inner());
    step.pending = step.pending.saturating_sub(1);
    if step.pending == 0 {
        step.started = None;
    }
}

/// How long the in-flight tool step has been running, or `None` when no
/// tool call is pending.
pub fn active_step_elapsed() -> Option<Duration> {
    let step = TOOL_STEP.lock().unwrap_or_else(|e| e.into_inner());
    if step.pending == 0 {
        return None;
    }
    step.started.map(|s| s.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_pending_step_keeps_the_flat_settle_budget() {
        assert_eq!(
            cancel_ack_budget(None, DEFAULT_CANCEL_ACK_CAP),
            crate::CANCEL_SETTLE_BUDGET
        );
    }

    /// The incident shape: the watchdog fired while a tool step had already
    /// run for 20 s. The flat 5 s budget is shorter than the step, so the
    /// daemon's `turn_complete` (sent when the step returns) could never
    /// land inside it; the scaled budget waits as long as the step has run.
    #[test]
    fn a_long_running_step_scales_the_budget_past_the_flat_one() {
        let step = Duration::from_secs(20);
        let budget = cancel_ack_budget(Some(step), DEFAULT_CANCEL_ACK_CAP);
        assert!(
            crate::CANCEL_SETTLE_BUDGET < step,
            "the pre-fix flat budget is shorter than the step"
        );
        assert_eq!(budget, step);
    }

    #[test]
    fn a_short_step_never_goes_below_the_settle_budget() {
        assert_eq!(
            cancel_ack_budget(Some(Duration::from_millis(800)), DEFAULT_CANCEL_ACK_CAP),
            crate::CANCEL_SETTLE_BUDGET
        );
    }

    #[test]
    fn the_cap_bounds_a_step_that_has_run_for_minutes() {
        assert_eq!(
            cancel_ack_budget(Some(Duration::from_secs(600)), DEFAULT_CANCEL_ACK_CAP),
            DEFAULT_CANCEL_ACK_CAP
        );
    }

    #[test]
    fn a_zero_cap_disables_the_scaling() {
        assert_eq!(
            cancel_ack_budget(Some(Duration::from_secs(600)), Duration::ZERO),
            crate::CANCEL_SETTLE_BUDGET
        );
    }

    /// The tracker follows the streamed events: a call opens a step, its
    /// result closes it, overlapping calls keep the oldest one's clock, and
    /// a new turn starts clean.
    #[test]
    fn the_tool_step_tracker_follows_calls_and_results() {
        turn_started();
        assert_eq!(active_step_elapsed(), None);
        tool_call_started();
        assert!(active_step_elapsed().is_some());
        tool_call_started();
        tool_result_seen();
        assert!(
            active_step_elapsed().is_some(),
            "one of two overlapping calls is still pending"
        );
        tool_result_seen();
        assert_eq!(active_step_elapsed(), None);
        tool_call_started();
        turn_started();
        assert_eq!(active_step_elapsed(), None, "a new turn starts clean");
    }
}
