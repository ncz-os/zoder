//! Cache-stable review chunks (lever e — the chunked review path).
//!
//! A chunked review is the cheapest place to lose the whole prompt cache. Every
//! chunk is a separate model call, an author's turn never follows it, and the
//! 16-chunk review measured on 2026-10-25 resent the system prompt, the
//! cross-chunk diff map, the focus line and the per-chunk file context on all
//! 16 calls. On a provider with a prefix cache that re-send is nearly free
//! *only if the leading bytes are identical*; on a provider without one it is
//! paid 16 times.
//!
//! The construction that keeps the leading bytes identical is a *baton*:
//!
//! 1. [`review_prefix`] — the constant block: the review system prompt, the
//!    focus line, the deterministic diff map and the label for any single-chunk
//!    review. Fixed for the whole review, so it either hits the cache on every
//!    chunk or (for the single chunk) is the whole prompt.
//! 2. [`review_chunk_message`] — the per-chunk variable block: part number, the
//!    chunk's own cross-file context header and the chunk itself. Nothing in
//!    it repeats the prefix, and no part number, byte count or timestamp
//!    appears anywhere before it.
//!
//! `review_prefix(..) + review_chunk_message(..)` is exactly the prompt,
//! `review_chunk_message` is exactly the suffix that grows per turn, and
//! [`chunk_prompt`] is the one function a call site should use. Nothing here is
//! random or clock-derived, so the prefix fingerprint of a review is stable
//! across sessions — [`review_fingerprint`] is the value to log to prove it.
//!
//! What this module deliberately does **not** do: drop the diff map or the
//! per-chunk context to save tokens. Those exist because small reviewers
//! reported names bound in another chunk as undefined (see
//! `zoder-cli/src/review_context.rs`); a review that is cheap because it has
//! less evidence is a worse review, not an optimised one.

use super::prefix::{fingerprint, Prefix};

/// Largest per-chunk file-context header that is still hoisted into the cache
/// prefix. `zoder-cli::review_context::CONTEXT_BUDGET_BYTES` is 3 000 bytes;
/// the margin absorbs the wrapper sentence the CLI adds around it.
pub const MAX_HOISTABLE_CONTEXT_BYTES: usize = 4_096;

/// The instruction that tells a reviewer its part is partial. Constant, so it
/// belongs to the prefix.
pub const PARTIAL_DIFF_NOTICE: &str = "Each part is a partial diff, not a complete source file. \
Definitions, imports and call sites may be unchanged or in other parts. Their absence here is \
not evidence of a defect. Report only defects established by the supplied code; do not invent \
missing integration or compiler failures from omitted context.";

/// The constant head of a review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPrefix {
    /// The review system prompt (`REVIEW_SYSTEM` / `ADVERSARIAL_SYSTEM`).
    pub system: String,
    /// The operator's `Focus especially on: …` line, already rendered (empty
    /// when there is none). Constant for the whole review.
    pub focus: String,
    /// The deterministic cross-chunk diff map, already wrapped in its
    /// explanatory sentence (empty when the review has one chunk).
    pub diff_map: String,
    /// The introducing sentence: the portion label, or — for a single-chunk
    /// review — the whole body instruction. It carries no part number, so it
    /// is constant for the review and belongs to the prefix; the banner that
    /// does carry the part number is in [`ChunkMessage::banner`].
    pub head: String,
    /// The shared cross-file context header, when *every* chunk of the review
    /// carries a byte-identical one (see [`hoistable_context`]). Hoisting it
    /// turns 16 per-call repeats into one cached block.
    pub context: String,
}

impl ReviewPrefix {
    /// The exact leading bytes of every chunk prompt in this review.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(self.system.trim_end());
        out.push('\n');
        if !self.focus.is_empty() {
            out.push_str(self.focus.trim_end());
            out.push('\n');
        }
        out.push_str(PARTIAL_DIFF_NOTICE);
        out.push('\n');
        out.push_str(self.head.trim_end());
        out.push('\n');
        if !self.context.is_empty() {
            out.push_str(self.context.trim_end());
            out.push('\n');
        }
        if !self.diff_map.is_empty() {
            out.push_str(self.diff_map.trim_end());
            out.push('\n');
        }
        out
    }

    /// The prefix as a [`Prefix`], so it can be fingerprinted, leak-checked
    /// against [`super::prefix::Volatile`] and stored alongside any other
    /// cache-stable block.
    pub fn as_prefix(&self) -> Prefix {
        Prefix::new(self.render())
    }
}

/// The per-chunk, variable block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkMessage {
    /// 1-based part number and total.
    pub part: usize,
    pub parts: usize,
    /// This chunk's own cross-file context header (empty when the header was
    /// hoisted into [`ReviewPrefix::context`] or when context is disabled).
    pub context: String,
    /// The chunk's diff text.
    pub chunk: String,
}

impl ChunkMessage {
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.context.len() + self.chunk.len() + 128);
        if !self.context.is_empty() {
            out.push_str(self.context.trim_end());
            out.push_str("\n\n");
        }
        out.push_str("```diff\n");
        out.push_str(&self.chunk);
        if !self.chunk.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n");
        out
    }

    /// The part banner a reviewer sees. It lives in the variable block, not the
    /// prefix, precisely because it changes per chunk.
    pub fn banner(&self) -> String {
        format!("Part {}/{} of this review.", self.part, self.parts)
    }
}

/// Build the constant prefix from the parts the CLI already computes.
///
/// `contexts` is the per-chunk context header list. When every header is
/// byte-identical and within [`MAX_HOISTABLE_CONTEXT_BYTES`] it is hoisted out
/// of the per-chunk blocks; otherwise the headers stay where they are and the
/// prefix keeps only the truly constant parts.
pub fn review_prefix(
    system: &str,
    focus: &str,
    diff_map: &str,
    head: &str,
    contexts: &[String],
) -> ReviewPrefix {
    ReviewPrefix {
        system: system.to_string(),
        focus: focus.to_string(),
        diff_map: diff_map.to_string(),
        head: head.to_string(),
        context: hoistable_context(contexts).unwrap_or_default(),
    }
}

/// The shared header when every chunk carries the same non-empty one and it is
/// small enough to be worth hoisting.
pub fn hoistable_context(contexts: &[String]) -> Option<String> {
    hoistable_context_with(contexts, |c| c.to_string())
}

/// [`hoistable_context`] over a raw per-chunk value list, rendering the shared
/// value once.
///
/// The CLI observes each chunk's context through a closure, which both memoises
/// and keeps `Fn`. Chunk ids are unique by construction, so a fallback
/// "observed chunk 7" value can never *equal* another chunk's and be hoisted by
/// accident — only the chunk the CLI actually looked at can be shared, and a
/// shared observation is by definition the same header for both.
pub fn hoistable_context_with<T: PartialEq, F: Fn(&T) -> String>(
    contexts: &[T],
    render: F,
) -> Option<String> {
    let first = contexts.first()?;
    if contexts.len() < 2 {
        return None;
    }
    // Adjacent-pair comparison: the raw-value form is only ever asked "are
    // these all the same value", and pairs avoid holding a `first` borrow
    // across the whole scan.
    let same = contexts.windows(2).all(|w| w[0] == w[1]);
    if !same {
        return None;
    }
    let rendered = render(first);
    if rendered.trim().is_empty() || rendered.len() > MAX_HOISTABLE_CONTEXT_BYTES {
        return None;
    }
    Some(rendered)
}

/// Assemble one chunk prompt: `prefix + banner + (context) + chunk`.
pub fn chunk_prompt(prefix: &ReviewPrefix, msg: &ChunkMessage) -> String {
    let mut out = prefix.render();
    out.push('\n');
    out.push_str(&msg.banner());
    out.push('\n');
    out.push_str(&msg.render());
    out
}

/// Assemble every chunk of a review, wiring the hoisted context through.
///
/// `contexts[i]` is chunk `i`'s header. A header that was hoisted is not
/// repeated in the chunk block; the prompt it produces is byte-for-byte the
/// same review, minus the duplication.
pub fn review_prompts(
    prefix: &ReviewPrefix,
    chunks: &[String],
    contexts: &[String],
) -> Vec<String> {
    let hoisted = !prefix.context.is_empty();
    chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let context = match contexts.get(i) {
                Some(c) if !hoisted => c.clone(),
                _ => String::new(),
            };
            chunk_prompt(
                prefix,
                &ChunkMessage {
                    part: i + 1,
                    parts: chunks.len(),
                    context,
                    chunk: chunk.clone(),
                },
            )
        })
        .collect()
}

/// Fingerprint of a review's constant prefix. Two reviews over the same diff,
/// system prompt and focus share this value across sessions and hosts; a value
/// that changes between chunks of the same review is a prefix break. 0 means
/// the prefix is empty.
pub fn review_fingerprint(prefix: &ReviewPrefix) -> u64 {
    fingerprint(&prefix.as_prefix())
}

/// Split a prompt at `boundary` for the assertion that the leading bytes are
/// exactly the prefix. Returns `(head, tail)`; a boundary past the end yields
/// the whole string and an empty tail rather than panicking on a short prompt.
pub fn split_at(prompt: &str, boundary: usize) -> (&str, &str) {
    let at = boundary.min(prompt.len());
    // Never split a UTF-8 code point: the diff may be non-ASCII.
    let at = (0..=at)
        .rev()
        .find(|&i| prompt.is_char_boundary(i))
        .unwrap_or(0);
    prompt.split_at(at)
}

#[cfg(test)]
mod tests;
