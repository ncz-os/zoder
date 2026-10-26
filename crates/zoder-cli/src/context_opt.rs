//! The CLI half of the context-optimisation work.
//!
//! Three jobs, all of them about what a model call actually carried:
//!
//! 1. **Resolve the levers** for a provider from the config
//!    ([`zoder_core::config::ContextConfig`]). A config with no `[context]`
//!    block resolves to no levers at all, so nothing changes for an operator
//!    who never asked for any of this.
//! 2. **Fit the outgoing prompt** to the provider's context window
//!    (`max_context_tokens`). A prompt that cannot fit is REFUSED, never
//!    truncated: a review that is cheap because it looked at less evidence
//!    is a worse review, not an optimised one.
//! 3. **Record what each call carried** — prompt tokens, cache hits, cache
//!    misses, completion tokens, and how the prompt was composed. This is
//!    the per-call record the 2026-10-25 study had to infer from a monthly
//!    bill.
//!
//! The recorder is process-global (the same shape as
//! `crate::provider_health::take_skipped`): the review path fans out over
//! chunks and over a fallback chain, and threading a `&mut` recorder through
//! every layer of that would be a large, invasive change for no benefit.
//! One process makes one set of calls, so one global is one run's worth of
//! records.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};

use zoder_core::config::Config;
use zoder_core::context_opt::measure::{summarise, CallRecord, Part, Segment};
use zoder_core::context_opt::{estimate_tokens, fit_to_window, Levers, WindowAction};

/// The per-call records of this process.
static RECORDS: Mutex<Vec<CallRecord>> = Mutex::new(Vec::new());

/// The levers for `model`, or `None` when the `[context]` block is off (or
/// absent).
pub(crate) fn levers_for(cfg: &Config, model: &str) -> Option<Levers> {
    cfg.context.levers_for(model)
}

/// The context window for a (provider, model), in tokens.
///
/// The per-model entry wins over the provider's, and the lever profile's own
/// window is the last resort. `None` means "no window stated": the caller
/// must not trim a prompt to fit a window nobody published.
pub(crate) fn window_for(
    cfg: &Config,
    provider_id: &str,
    model: &str,
    levers: Option<&Levers>,
) -> Option<u64> {
    cfg.max_context_tokens_for(provider_id, model)
        .or_else(|| levers.map(|l| l.max_context_tokens))
        .filter(|w| *w > 0)
}

/// The outcome of fitting one prompt to the provider's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowOutcome {
    /// No window was stated, or the prompt already fitted.
    Fits,
    /// The prompt was brought under the window.
    Fitted(WindowAction),
}

/// Fit one rendered prompt to the provider's context window.
///
/// `system` travels in the provider's `system` slot and `user` in the `user`
/// slot; both count against the window. The system prompt is pinned (it is
/// the contract with the reviewer) and the user message is the brief, so a
/// prompt that does not fit is REFUSED: there is nothing to drop that would
/// leave the reviewer with the same task.
pub(crate) fn fit_prompt(
    system: &str,
    user: &str,
    window: Option<u64>,
) -> anyhow::Result<WindowOutcome> {
    let Some(window) = window else {
        return Ok(WindowOutcome::Fits);
    };
    let turns = vec![
        zoder_core::context_opt::Turn::new("system", system),
        zoder_core::context_opt::Turn::new("user", user),
    ];
    let fit = fit_to_window(&turns, window);
    match fit.action {
        WindowAction::AlreadyFits => Ok(WindowOutcome::Fits),
        WindowAction::TooLarge => anyhow::bail!(
            "the prompt is {} tokens (about {} bytes) and does not fit this provider's \
             {}-token context window. Raise the provider's max_context_tokens, lower the \
             review chunk size, or pick a model with a larger window. The prompt is NOT \
             truncated: a review that saw less of the diff would be a worse review.",
            estimate_tokens((system.len() + user.len()) as u64),
            system.len() + user.len(),
            window
        ),
        action => Ok(WindowOutcome::Fitted(action)),
    }
}

/// Fit a whole conversation to the provider's context window.
///
/// The levers have already bounded the working set by the time this is
/// called, so the remaining case is a conversation whose pinned head and
/// brief alone exceed the window. That is REFUSED: a conversation that has
/// lost the task is not a smaller task.
pub(crate) fn fit_messages(
    messages: &[zoder_core::provider::Message],
    window: Option<u64>,
) -> anyhow::Result<WindowOutcome> {
    let Some(window) = window else {
        return Ok(WindowOutcome::Fits);
    };
    let bytes: u64 = messages.iter().map(|m| m.content.len() as u64).sum();
    let tokens = estimate_tokens(bytes);
    if tokens <= window {
        return Ok(WindowOutcome::Fits);
    }
    let turns: Vec<zoder_core::context_opt::Turn> = messages
        .iter()
        .map(|m| zoder_core::context_opt::Turn::new(&m.role, m.content.clone()))
        .collect();
    let fit = fit_to_window(&turns, window);
    match fit.action {
        WindowAction::AlreadyFits | WindowAction::Elided | WindowAction::Compacted => {
            Ok(WindowOutcome::Fitted(fit.action))
        }
        WindowAction::TooLarge => anyhow::bail!(
            "the conversation is {tokens} tokens (about {bytes} bytes) and does not fit this \
             provider's {window}-token context window, even after the context levers bounded \
             it. Raise the provider's max_context_tokens or pick a model with a larger \
             window. The conversation is NOT truncated: cutting it would silently drop the \
             brief."
        ),
    }
}

/// Record one model call.
///
/// The provider's own `usage` wins over the byte estimate; when the provider
/// publishes no cache counter the cache fields stay `None` rather than
/// becoming a fabricated zero.
pub(crate) fn record_call(
    surface: &str,
    segments: &[Segment],
    res: &zoder_core::provider::ChatResult,
) {
    let record = CallRecord::new(
        next_turn(),
        surface,
        segments,
        res.prompt_tokens,
        res.cached_prompt_tokens,
        res.cache_miss_tokens,
        res.completion_tokens.unwrap_or(res.tokens_out),
    );
    if let Ok(mut records) = RECORDS.lock() {
        records.push(record);
    }
}

/// The records of this process, in call order.
pub(crate) fn records() -> Vec<CallRecord> {
    RECORDS.lock().map(|r| r.clone()).unwrap_or_default()
}

/// The per-call telemetry as a JSON block, for a command's structured output.
///
/// Always present once the `[context]` block is configured, so a consumer can
/// tell "measured, and the provider published nothing" from "not measured".
pub(crate) fn payload() -> Value {
    let records = records();
    let summary = summarise(&records);
    json!({
        "calls": summary.calls,
        "prompt_tokens": summary.prompt_tokens,
        "completion_tokens": summary.completion_tokens,
        "cache_hit_tokens": summary.cache_hit_tokens,
        "cache_miss_tokens": summary.cache_miss_tokens,
        "cache_reported_calls": summary.cache_reported_calls,
        "hit_share": summary.hit_share(),
        "peak_prompt_tokens": summary.peak_prompt_tokens,
        "final_prompt_tokens": summary.final_prompt_tokens,
        "resent_prompt_tokens": summary.resent_prompt_tokens,
        "per_call": records,
    })
}

/// Where the per-call records are appended, one JSON object per line.
///
/// Next to the ledger, so the two are found together and the ledger's
/// directory already exists (and is created if it does not).
pub(crate) fn telemetry_path(cfg: &Config) -> PathBuf {
    let dir = cfg
        .ledger_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| Config::home().join("data"));
    dir.join("context-telemetry.jsonl")
}

/// Append this process's records to the telemetry file.
///
/// Best effort: a telemetry file that cannot be written must never fail the
/// command that produced the records. The records are still in the command's
/// structured output.
pub(crate) fn flush(cfg: &Config) -> Option<PathBuf> {
    let records = records();
    if records.is_empty() {
        return None;
    }
    let path = telemetry_path(cfg);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    for record in &records {
        if let Ok(line) = serde_json::to_string(record) {
            let _ = writeln!(file, "{line}");
        }
    }
    Some(path)
}

/// The 1-based number of the next recorded call.
fn next_turn() -> u32 {
    RECORDS.lock().map(|r| r.len() as u32 + 1).unwrap_or(1)
}

/// The segments of a two-message provider call: the system prompt is the
/// cache-stable head, the user message is the task.
pub(crate) fn segments_for(system: &str, user: &str) -> Vec<Segment> {
    vec![
        Segment::new(Part::Prefix, system),
        Segment::new(Part::Task, user),
    ]
}

/// The segments of a whole conversation, attributed the way the levers see
/// it: the system prompt is the cache-stable prefix, a tool result is tool
/// output, and everything else is history.
pub(crate) fn segments_for_messages(messages: &[zoder_core::provider::Message]) -> Vec<Segment> {
    messages
        .iter()
        .map(|m| {
            let part = match m.role.as_str() {
                "system" => Part::Prefix,
                "tool" => Part::ToolOutput,
                _ => Part::History,
            };
            Segment::new(part, m.content.clone())
        })
        .collect()
}

/// Apply the context levers to a conversation the CLI owns.
///
/// This is the CLI's own multi-turn conversation: the prior session
/// transcript plus the new prompt. The levers are pure functions over a turn
/// list, so this adapts a provider message list to and from that shape.
///
/// Nothing is dropped that a lever did not explicitly replace: a capped tool
/// output keeps its head and its tail and names what it dropped, an elided
/// one names its source so it can be re-read, and a compacted span becomes a
/// digest that lists the files, the decisions and the open todos. The brief
/// and the system prompt are never folded.
pub(crate) fn apply_levers(
    messages: &[zoder_core::provider::Message],
    levers: &Levers,
) -> Vec<zoder_core::provider::Message> {
    let turns: Vec<zoder_core::context_opt::Turn> = messages
        .iter()
        .map(|m| zoder_core::context_opt::Turn::new(&m.role, m.content.clone()))
        .collect();
    let compacted = levers.apply(&turns);
    compacted
        .turns
        .iter()
        .map(|t| zoder_core::provider::Message::new(&t.role, t.content.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_window(window: u64) -> Config {
        let mut cfg = Config::default_provider(std::path::Path::new("/nonexistent"));
        if let Some(provider) = cfg.providers.first_mut() {
            provider.max_context_tokens = Some(window);
        }
        cfg
    }

    #[test]
    fn no_window_means_no_trimming() {
        let cfg = Config::default_provider(std::path::Path::new("/nonexistent"));
        assert_eq!(window_for(&cfg, "p", "m", None), None);
        assert_eq!(
            fit_prompt("sys", "user", None).unwrap(),
            WindowOutcome::Fits
        );
    }

    #[test]
    fn a_prompt_that_fits_is_left_alone() {
        let out = fit_prompt("sys", "user", Some(1_000)).unwrap();
        assert_eq!(out, WindowOutcome::Fits);
    }

    #[test]
    fn a_prompt_that_cannot_fit_is_refused_not_truncated() {
        let err = fit_prompt("sys", &"x".repeat(100_000), Some(10)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("does not fit"), "{msg}");
        assert!(msg.contains("NOT"), "{msg}");
    }

    #[test]
    fn the_window_comes_from_the_provider_then_the_levers() {
        let cfg = cfg_with_window(131_072);
        let pid = cfg.providers[0].id.clone();
        assert_eq!(window_for(&cfg, &pid, "m", None), Some(131_072));
        let levers = Levers::FLEET_MINIMAX;
        assert_eq!(
            window_for(&cfg, &pid, "m", Some(&levers)),
            Some(131_072),
            "the stated window wins over the profile's"
        );
        // A provider that states no window falls back to the lever profile's
        // own window.
        let bare = Config::default_provider(std::path::Path::new("/nonexistent"));
        let bare_pid = bare.providers[0].id.clone();
        assert_eq!(
            window_for(&bare, &bare_pid, "m", Some(&levers)),
            Some(levers.max_context_tokens)
        );
    }

    #[test]
    fn a_zero_window_is_treated_as_no_window() {
        let cfg = cfg_with_window(0);
        assert_eq!(window_for(&cfg, "p", "m", None), None);
    }
}
