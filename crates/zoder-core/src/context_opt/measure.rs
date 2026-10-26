//! Prompt composition measurement at the level of one model call.
//!
//! [`super::PromptComposition`] answers "what fraction of what I sent was
//! history" from bytes. This module answers the question the operator actually
//! asked — *is the loop's prompt growing, and what is growing it* — per call,
//! from the messages that were really rendered.
//!
//! It exists because the 2026-10-25 study had to infer 250 full-window calls
//! from a monthly bill: no per-call record of prompt tokens, cache hits, cache
//! misses and composition survived a run. [`CallRecord`] is that record, and
//! [`summarise`] is the per-run roll-up whose `growing_share` names the lever.
//!
//! Everything here is pure and is fed the same message list that reaches the
//! provider, so a record can be produced offline from a transcript as well as
//! live from a loop turn. Token counts are estimates from [`estimate_tokens`]
//! unless the caller passes the provider's `usage` values, which always win.

use super::{estimate_tokens, PromptComposition, PromptSlice};
use serde::{Deserialize, Serialize};

/// Which part of a rendered message a byte range belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Part {
    /// The cache-stable prefix: system prompt and tool schemas.
    Prefix,
    /// The task brief (the first user turn).
    Task,
    /// A tool result.
    ToolOutput,
    /// Anything else in the conversation history.
    History,
}

impl Part {
    pub fn as_slice(self) -> PromptSlice {
        match self {
            Part::Prefix => PromptSlice::Prefix,
            Part::Task => PromptSlice::Task,
            Part::ToolOutput => PromptSlice::ToolOutput,
            Part::History => PromptSlice::History,
        }
    }
}

/// One rendered message and the part of the prompt it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub part: Part,
    /// The rendered text. Only its length is used by [`compose`].
    pub text: String,
}

impl Segment {
    pub fn new(part: Part, text: impl Into<String>) -> Self {
        Self {
            part,
            text: text.into(),
        }
    }
}

/// Byte attribution of one rendered prompt, as shares of the whole.
///
/// Same shape as [`PromptComposition`] so a per-call record can be summed into
/// the same reports, but built from explicit [`Segment`]s rather than a byte
/// offset.
pub fn compose(segments: &[Segment]) -> PromptComposition {
    let mut out = PromptComposition::default();
    for segment in segments {
        let bytes = segment.text.len() as u64;
        match segment.part {
            Part::Prefix => out.prefix_bytes += bytes,
            Part::Task => out.task_bytes += bytes,
            Part::ToolOutput => out.tool_output_bytes += bytes,
            Part::History => out.history_bytes += bytes,
        }
    }
    out
}

/// The composition of one call, split into the part that grows with the
/// conversation and the part that does not.
///
/// `growing_share` is the number that decides which lever to pull: a run whose
/// growth is nearly all [`Part::ToolOutput`] wants caps and elision; one whose
/// growth is [`Part::History`] wants compaction. The prefix never grows, which
/// is exactly why it must stay byte-identical.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Growth {
    pub tool_output_bytes: u64,
    pub history_bytes: u64,
    /// `tool_output + history`: the bytes a resend is paying for.
    pub growing_bytes: u64,
    /// Share of the whole prompt that is growth (0.0 for an empty prompt).
    pub growing_share: f64,
    pub tool_output_share: f64,
    pub history_share: f64,
}

pub fn growth(composition: &PromptComposition) -> Growth {
    let total = composition.total_bytes();
    let growing = composition
        .tool_output_bytes
        .saturating_add(composition.history_bytes);
    let share = |bytes: u64| {
        if total == 0 {
            0.0
        } else {
            bytes as f64 / total as f64
        }
    };
    Growth {
        tool_output_bytes: composition.tool_output_bytes,
        history_bytes: composition.history_bytes,
        growing_bytes: growing,
        growing_share: share(growing),
        tool_output_share: share(composition.tool_output_bytes),
        history_share: share(composition.history_bytes),
    }
}

/// One model call as it will be recorded in the run ledger.
///
/// `prompt_tokens`/`completion_tokens` are the provider's `usage` values when
/// they were reported and the byte estimate otherwise; `reported_usage` says
/// which, so a roll-up never mixes the two silently.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallRecord {
    /// 1-based turn number within the run.
    pub turn: u32,
    /// Chat surface that spent the call: `author`, `review`, `chunk:3/16`, …
    pub surface: String,
    pub prompt_tokens: u64,
    /// Provider-reported cache hits, `None` when the provider publishes none
    /// (MiniMax). Never guessed as 0.
    pub cache_hit_tokens: Option<u64>,
    /// `prompt - hit` unless the provider reported its own miss count.
    pub cache_miss_tokens: Option<u64>,
    pub completion_tokens: u64,
    pub reported_usage: bool,
    pub composition: PromptComposition,
    /// Estimated prompt tokens of the NEXT call on the same surface, when the
    /// caller knows it. `None` on the last call. The difference is what a
    /// resend is paying for; it is an estimate and is labelled as one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_prompt_tokens: Option<u64>,
}

impl CallRecord {
    /// Build a record from the rendered prompt segments and the provider's
    /// usage values (all `None` when the provider reported nothing).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        turn: u32,
        surface: impl Into<String>,
        segments: &[Segment],
        usage_prompt: Option<u64>,
        usage_hit: Option<u64>,
        usage_reported_miss: Option<u64>,
        completion_tokens: u64,
    ) -> Self {
        let composition = compose(segments);
        let estimated = estimate_tokens(composition.total_bytes());
        let prompt_tokens = usage_prompt.unwrap_or(estimated);
        let (hit, miss) = match usage_hit {
            Some(hit) => {
                let miss = usage_reported_miss.unwrap_or_else(|| prompt_tokens.saturating_sub(hit));
                (Some(hit), Some(miss))
            }
            None => (None, None),
        };
        Self {
            turn,
            surface: surface.into(),
            prompt_tokens,
            cache_hit_tokens: hit,
            cache_miss_tokens: miss,
            completion_tokens,
            reported_usage: usage_prompt.is_some(),
            composition,
            next_prompt_tokens: None,
        }
    }

    /// Cache-hit share, `None` when the provider reported no cache counter.
    ///
    /// A call with no prompt tokens has no ratio to report: `Some(1.0)` here
    /// is the vacuous "everything that was sent was cached" reading, and
    /// [`classify`](super::classify) treats the same case as
    /// [`CacheVerdict::Unknown`](super::CacheVerdict::Unknown) rather than as
    /// a prefix break.
    pub fn hit_share(&self) -> Option<f64> {
        let hit = self.cache_hit_tokens?;
        if self.prompt_tokens == 0 {
            return Some(1.0);
        }
        Some((hit as f64 / self.prompt_tokens as f64).clamp(0.0, 1.0))
    }

    /// Estimated tokens the NEXT call on this surface spends re-sending text
    /// that was already sent.
    ///
    /// `next_prompt_tokens` is the next call's PROMPT size, so the overlap
    /// with this call is the smaller of the two: the next call cannot re-send
    /// more than this call sent, and a next prompt that is SMALLER than this
    /// one (a compaction) re-sends at most itself. That is why the value is
    /// clamped, and why it is an estimate and not a measurement.
    pub fn resend_tokens(&self) -> Option<u64> {
        self.next_prompt_tokens
            .map(|next| next.min(self.prompt_tokens))
    }
}

/// Per-run roll-up of [`CallRecord`]s.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Summed only over calls whose provider reported a counter.
    pub cache_hit_tokens: u64,
    pub cache_miss_tokens: u64,
    /// Calls that carried a provider cache counter.
    pub cache_reported_calls: u64,
    /// Sum of each call's estimated resend of the previous prompt.
    pub resent_prompt_tokens: u64,
    /// Prompt tokens of the LAST call: the window the run ended at.
    pub final_prompt_tokens: u64,
    /// Prompt tokens of the largest call.
    pub peak_prompt_tokens: u64,
}

impl RunSummary {
    /// Cache-hit share over the reported calls, `None` if none reported.
    pub fn hit_share(&self) -> Option<f64> {
        if self.cache_reported_calls == 0 {
            return None;
        }
        let total = self.cache_hit_tokens + self.cache_miss_tokens;
        if total == 0 {
            return Some(1.0);
        }
        Some(self.cache_hit_tokens as f64 / total as f64)
    }

    /// Share of prompt tokens that bought a resend of text already sent.
    pub fn resend_share(&self) -> f64 {
        if self.prompt_tokens == 0 {
            0.0
        } else {
            self.resent_prompt_tokens as f64 / self.prompt_tokens as f64
        }
    }
}

pub fn summarise(records: &[CallRecord]) -> RunSummary {
    let mut out = RunSummary::default();
    for record in records {
        out.calls += 1;
        out.prompt_tokens += record.prompt_tokens;
        out.completion_tokens += record.completion_tokens;
        out.final_prompt_tokens = record.prompt_tokens;
        out.peak_prompt_tokens = out.peak_prompt_tokens.max(record.prompt_tokens);
        if let (Some(hit), Some(miss)) = (record.cache_hit_tokens, record.cache_miss_tokens) {
            out.cache_hit_tokens += hit;
            out.cache_miss_tokens += miss;
            out.cache_reported_calls += 1;
        }
        out.resent_prompt_tokens += record.resend_tokens().unwrap_or(0);
    }
    out
}

/// A one-line ledger rendering: turn, surface, tokens, cache split, growth.
pub fn ledger_line(record: &CallRecord) -> String {
    let hit = match (record.cache_hit_tokens, record.cache_miss_tokens) {
        (Some(hit), Some(miss)) => format!("cache {hit}/{}", hit + miss),
        _ => "cache -".to_string(),
    };
    let g = growth(&record.composition);
    format!(
        "t{} {} prompt={} {hit} out={} growth={:.0}% (tool {:.0}%, history {:.0}%)",
        record.turn,
        record.surface,
        record.prompt_tokens,
        record.completion_tokens,
        g.growing_share * 100.0,
        g.tool_output_share * 100.0,
        g.history_share * 100.0,
    )
}

#[cfg(test)]
mod tests;
