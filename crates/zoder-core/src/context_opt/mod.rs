//! Context optimisation for metered, prompt-cached providers (DeepSeek,
//! MiniMax).
//!
//! Measured problem (DeepSeek console export, 30 days to 2026-10-25):
//! `$79.79` on 10.6B input tokens against 50.1M output — **212:1**. One `zoder
//! loop` on CERBERUS spent 32.9M input tokens for 110K output: roughly 250
//! model calls, every one of them resending the whole growing conversation at
//! a full 128K window. The only lever in the fleet was `max_context_tokens`
//! per profile, and nothing was known about what a call actually contained.
//!
//! This module is the measurement layer plus the four pure levers:
//!
//! 1. [`CacheTelemetry`] / [`classify`] — per-call prompt composition and the
//!    provider-side cache split (DeepSeek reports `prompt_cache_hit_tokens` /
//!    `prompt_cache_miss_tokens`; OpenAI-shaped gateways report
//!    `prompt_tokens_details.cached_tokens`; MiniMax reports neither, so its
//!    shape is [`ProviderCacheShape::None`] and its saving is prefix reuse the
//!    provider does not bill back).
//! 2. [`prefix`] — one byte-identical system prompt + tool-schema block per
//!    provider/model, so the provider's prefix cache keeps hitting.
//! 3. [`cap_tool_output`] — a per-call cap on a single tool result.
//! 4. [`elide_stale`] — replace the bodies of old, large tool outputs with a
//!    stub the agent can re-read.
//! 5. [`compact`] — fold older turns into a structured digest once the
//!    conversation passes a token threshold, keeping recent turns verbatim.
//!
//! Everything here is pure: no I/O, no clock, no provider. The CLI owns the
//! wiring (see `agentic.rs` and `agentic_worker.rs`) and the config surface
//! lives in [`crate::config::ContextOptConfig`].
//!
//! ## Provider cache shapes
//!
//! | provider | caches prompts | reported fields |
//! |---|---|---|
//! | DeepSeek direct | yes, disk KV prefix cache (64-token blocks) | `prompt_cache_hit_tokens`, `prompt_cache_miss_tokens` |
//! | OpenAI-compatible gateways (LiteLLM, OpenRouter, vLLM) | implementation-defined | `prompt_tokens_details.cached_tokens` |
//! | Anthropic-shaped gateways | yes | `cache_read_input_tokens` |
//! | MiniMax (`api.minimax.io`) | yes, server-side prefix reuse | **nothing in `usage`** — no hit/miss split to verify |
//!
//! A provider that reports nothing can still be *saving* (MiniMax bills reused
//! prompt tokens at a discount without telling the caller), but the saving is
//! unverifiable from the response, so the cache-hit share of a MiniMax call is
//! never asserted — [`CacheTelemetry::hit_share`] returns `None` rather than a
//! fabricated 0%.

pub mod measure;
pub mod prefix;
pub mod review;

use serde::{Deserialize, Serialize};

/// Provider-side prompt-cache reporting shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCacheShape {
    /// DeepSeek direct and anything else publishing
    /// `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens`.
    Split,
    /// OpenAI-shaped `usage.prompt_tokens_details.cached_tokens` (sometimes
    /// `usage.cache_read_input_tokens`): the hit count only, the miss is
    /// derived as `prompt - hit`.
    CachedSubset,
    /// Nothing reported. Truly cacheless providers and providers that reuse
    /// prefixes without exposing a counter both land here; the two are
    /// indistinguishable from the response, so neither is assumed.
    None,
}

/// The token cost of one model call, split the way a prompt cache bills it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheTelemetry {
    /// `usage.prompt_tokens` (total input, cached and not).
    pub prompt_tokens: u64,
    /// Tokens served from the provider's prefix cache, when reported.
    pub cache_hit_tokens: u64,
    /// `usage.completion_tokens`.
    pub completion_tokens: u64,
    /// `prompt_cache_miss_tokens` when the provider reports it directly.
    /// DeepSeek can report a miss LARGER than `prompt - hit` (a prefix break
    /// re-bills the tool-schema block), and the operator pays the larger
    /// number, so a reported value always wins over the derived one.
    pub reported_cache_miss: Option<u64>,
}

impl CacheTelemetry {
    /// Tokens billed at the cache-miss (full) input rate.
    ///
    /// A reported miss always wins over the derived one: DeepSeek re-bills the
    /// whole prefix when it breaks, and the miss it reports is the number the
    /// invoice is computed from. The derived value is the fallback for the
    /// shapes that publish only a hit counter.
    pub fn cache_miss_tokens(&self) -> u64 {
        match self.reported_cache_miss {
            Some(miss) => miss,
            None => self.prompt_tokens.saturating_sub(self.cache_hit_tokens),
        }
    }

    /// Tokens billed at the cache-read rate.
    pub fn cache_read_tokens(&self) -> u64 {
        self.cache_hit_tokens.min(self.prompt_tokens)
    }

    /// Share of the prompt served from cache, or `None` when the provider
    /// reports no hit counter.
    ///
    /// The shape decides: a [`ProviderCacheShape::None`] provider publishes
    /// nothing, so its hit share is unknown (a silent-but-caching provider and
    /// a cacheless one produce identical bytes on the wire and must not be
    /// reported as a 0% hit rate). This is the shape-aware entry point; the
    /// shape-free one is [`CacheTelemetry::hit_share_raw`].
    pub fn hit_share(&self, shape: ProviderCacheShape) -> Option<f64> {
        match shape {
            ProviderCacheShape::None => None,
            ProviderCacheShape::Split | ProviderCacheShape::CachedSubset => self.hit_share_raw(),
        }
    }

    /// Hit share with no shape in hand: an empty prompt is a full hit, a
    /// non-empty prompt with no hit counter reported is `None` (nothing was
    /// published), otherwise `hit / prompt`.
    pub fn hit_share_raw(&self) -> Option<f64> {
        if self.prompt_tokens == 0 {
            return Some(1.0);
        }
        // Nothing published at all: no claim to make.
        if self.cache_hit_tokens == 0 && self.reported_cache_miss.is_none() {
            return None;
        }
        // A split that does not add up (0 hits AND 0 misses for a non-empty
        // prompt) is not a 0% hit rate; it is an inconsistent report.
        if self.cache_hit_tokens == 0 && self.reported_cache_miss == Some(0) {
            return None;
        }
        Some(self.cache_read_tokens() as f64 / self.prompt_tokens as f64)
    }

    /// Cost in USD using the operator's rates. `hit_rate` applies to cached
    /// tokens, `miss_rate` to the rest, `output_rate` to the completion.
    ///
    /// `hit + miss` is never larger than `prompt_tokens`: the cached tokens are
    /// a SUBSET of the prompt (the provider bills `hit` at the cache rate and
    /// the remainder at the full rate), so a reported miss larger than the
    /// remainder is clamped. Billing both in full would charge the operator for
    /// tokens that were never sent.
    ///
    /// `reported_cache_miss` is deliberately NOT used here, and this is the one
    /// place it would change the answer. When DeepSeek breaks a prefix it
    /// re-bills the schema block and reports a miss larger than `prompt - hit`;
    /// the true cost of that break is the whole prompt at the miss rate. Rather
    /// than guess which of the two figures the invoice used, this bills the
    /// lower bound (`prompt - hit`) and the break is reported separately by
    /// [`classify`] as a [`CacheVerdict::PrefixBreak`]. Treat any call classified
    /// as a break as costing `prompt x miss_rate` when reconciling a month.
    pub fn cost_usd(&self, hit_rate: f64, miss_rate: f64, output_rate: f64) -> f64 {
        let hit = if self.cache_hit_tokens == 0 {
            0
        } else {
            self.cache_read_tokens()
        };
        // The exclusive remainder, not the reported figure: `prompt - hit` is
        // exactly the part of the prompt the cache did not serve.
        let miss = self.prompt_tokens.saturating_sub(hit);
        (hit as f64 * hit_rate
            + miss as f64 * miss_rate
            + self.completion_tokens as f64 * output_rate)
            / 1_000_000.0
    }
}

/// What a call's cache split says about prefix stability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheVerdict {
    /// Hit share at or above the configured floor.
    Cached,
    /// Hit share below the floor: something before the last change moved in
    /// the prefix (a timestamp, a session id, a reordered tool list) and the
    /// provider re-billed the block.
    PrefixBreak,
    /// The provider reports no cache counter, or the call carried no prompt
    /// tokens: nothing can be concluded.
    Unknown,
}

/// The measured cache-hit floor. DeepSeek's console reported 98.9% over 30
/// days; a healthy multi-turn agent loop sits at 95%+, and a prefix break
/// drops it to the system-prompt+schemas share (typically 5-15%).
pub const DEFAULT_CACHE_HIT_FLOOR: f64 = 0.98;

/// Classify one call's cache behaviour.
pub fn classify(shape: ProviderCacheShape, t: &CacheTelemetry, floor: f64) -> CacheVerdict {
    if shape == ProviderCacheShape::None || t.prompt_tokens == 0 {
        return CacheVerdict::Unknown;
    }
    match t.hit_share(shape) {
        Some(share) if share >= floor => CacheVerdict::Cached,
        Some(_) => CacheVerdict::PrefixBreak,
        None => CacheVerdict::Unknown,
    }
}

/// Which part of the prompt a byte slice is. Used by the composition
/// telemetry and by the byte-budget levers to decide what may shrink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptSlice {
    /// System prompt + tool schemas: cache-stable, never elided.
    Prefix,
    /// User/assistant turns that are not tool traffic.
    History,
    /// Tool results (file reads, test logs, diffs).
    ToolOutput,
    /// The task/brief or per-turn instruction at the tail.
    Task,
}

/// One line of the per-call prompt composition ledger.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptComposition {
    pub prefix_bytes: u64,
    pub history_bytes: u64,
    pub tool_output_bytes: u64,
    pub task_bytes: u64,
}

impl PromptComposition {
    pub fn total_bytes(&self) -> u64 {
        self.prefix_bytes
            .saturating_add(self.history_bytes)
            .saturating_add(self.tool_output_bytes)
            .saturating_add(self.task_bytes)
    }

    /// Share of the prompt that is each slice, in `[0, 1]`. An empty prompt
    /// reports zeros rather than a division by zero.
    pub fn shares(&self) -> [(&'static str, f64); 4] {
        let total = self.total_bytes();
        let share = |n: u64| {
            if total == 0 {
                0.0
            } else {
                n as f64 / total as f64
            }
        };
        [
            ("prefix", share(self.prefix_bytes)),
            ("history", share(self.history_bytes)),
            ("tool_output", share(self.tool_output_bytes)),
            ("task", share(self.task_bytes)),
        ]
    }
}

/// Bytes-per-token estimate for the cheap, deterministic prompt accounting.
/// Matches [`crate::budget::estimate_tokens`] so the two never disagree about
/// the same text (~4 bytes/token for English + code).
pub const BYTES_PER_TOKEN: u64 = 4;

/// Estimated tokens for a byte count.
pub fn estimate_tokens(bytes: u64) -> u64 {
    bytes.div_ceil(BYTES_PER_TOKEN)
}

/// One conversation turn as the levers see it. `content` is the full text the
/// provider would receive; `volatile` marks a turn that must never be elided
/// (the brief, a reviewer's findings, a digest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// `"system"`, `"user"`, `"assistant"`, `"tool"` — the wire role.
    pub role: String,
    pub content: String,
    /// A re-read hint recorded on a stub: the tool call and path that produced
    /// the original output.
    pub source: Option<String>,
}

impl Turn {
    pub fn new(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: content.into(),
            source: None,
        }
    }

    /// A tool result with a re-readable source reference.
    pub fn tool_result(source: &str, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.into(),
            source: Some(source.to_string()),
        }
    }

    pub fn bytes(&self) -> u64 {
        self.content.len() as u64
    }
}

/// Marker every elision stub starts with. Stable and greppable so an operator
/// (and the ledger) can tell an elided result from a real one, and so
/// [`is_elided`] never mistakes a genuine tool result for a stub.
pub const ELISION_MARKER: &str = "[elided:";

/// True when `turn` already carries an elision stub.
pub fn is_elided(turn: &Turn) -> bool {
    turn.content.trim_start().starts_with(ELISION_MARKER)
}

/// Build the stub that replaces an elided tool output.
///
/// The stub keeps the source reference so the agent can re-read the exact
/// file range instead of guessing, and names the byte count it dropped so it
/// knows whether re-reading is worth a call.
pub fn elision_stub(source: Option<&str>, original_bytes: u64) -> String {
    let what = source.unwrap_or("tool output");
    format!(
        "{ELISION_MARKER} {original_bytes} bytes dropped from {what} to save context. \
The output is unchanged on disk; re-read the file (or re-run the command) if it is needed again.]"
    )
}

/// Cap one tool result at `cap_bytes`, keeping the HEAD and the TAIL.
///
/// The head carries the signature/imports, the tail carries the failure of a
/// test log or the end of a diff; the middle is the least informative part of
/// both. A capped result names the source so the agent can re-read the middle
/// instead of assuming it (the pre-fix behaviour silently truncated, and a
/// reviewer then approved an approve-on-what-fit prompt).
///
/// `cap_bytes` below [`MIN_CAP_BYTES`] is raised to it: a cap smaller than the
/// stub it has to carry would consume the whole result to say it dropped
/// nothing. A result no longer than the effective cap — including one between
/// the requested cap and `MIN_CAP_BYTES` — is returned untouched, because
/// honouring a sub-floor cap is not possible without losing more to the stub
/// than is saved. When a cut IS made, the kept head+tail is at most `cap` (it can
/// be a few bytes less when a multi-byte character straddles a boundary, since
/// both cuts snap inward on a char boundary), and the stub's omitted count is the
/// exact size of the dropped MIDDLE, so `kept + omitted == original len` holds.
pub fn cap_tool_output(turn: &Turn, cap_bytes: usize) -> Turn {
    // One comparison against the EFFECTIVE cap. `cap_bytes` is raised to the
    // floor before the length test, so a result between the requested cap and
    // `MIN_CAP_BYTES` is returned untouched by the same rule that keeps a short
    // result; two separate length checks (one against the raw cap, one against
    // the raised one) invited changing one and forgetting the other.
    if cap_bytes == 0 {
        return turn.clone();
    }
    let cap = cap_bytes.max(MIN_CAP_BYTES);
    if turn.content.len() <= cap {
        return turn.clone();
    }
    // Keep a third of the budget for the tail; a log's last lines carry the
    // failure, a source file's tail carries the closing brace.
    let tail_bytes = cap / 3;
    let head_bytes = cap.saturating_sub(tail_bytes);
    let content = &turn.content;
    let head = floor_char_boundary(content, head_bytes);
    let tail_start = ceil_char_boundary(content, content.len().saturating_sub(tail_bytes));
    // Defensive: a multi-byte character straddling both boundaries can make
    // the two cuts meet or cross. There is nothing left to elide then, so the
    // turn is returned untouched rather than emitting a stub that claims
    // bytes it did not drop.
    if tail_start <= head {
        return turn.clone();
    }
    let omitted = tail_start.saturating_sub(head);
    // A stub that drops fewer bytes than it costs is a net loss: replacing the
    // middle with "1 bytes omitted" (cap 599 on a 600-byte body) shrinks nothing
    // meaningful and only spends tokens. Below this floor the turn is left
    // alone.
    if omitted < MIN_ELISION_BYTES {
        return turn.clone();
    }
    let stub = format!(
        "\n\n{ELISION_MARKER} {omitted} bytes omitted from the middle to fit the per-tool-output \
cap of {cap} bytes. The full output is unchanged on disk; re-read {} for the omitted part.]\n\n",
        turn.source.as_deref().unwrap_or("the file")
    );
    Turn {
        role: turn.role.clone(),
        content: format!("{}{stub}{}", &content[..head], &content[tail_start..]),
        source: turn.source.clone(),
    }
}

/// Smallest cap [`cap_tool_output`] will honour. Below this the elision stub
/// itself would be most of the result.
pub const MIN_CAP_BYTES: usize = 512;

/// Smallest number of bytes worth eliding. A stub costs ~150 bytes; dropping
/// less than this from a tool result is a net loss in context and tokens.
pub const MIN_ELISION_BYTES: usize = 1024;

fn floor_char_boundary(s: &str, mut n: usize) -> usize {
    n = n.min(s.len());
    while n > 0 && !s.is_char_boundary(n) {
        n -= 1;
    }
    n
}

fn ceil_char_boundary(s: &str, mut n: usize) -> usize {
    n = n.min(s.len());
    while n < s.len() && !s.is_char_boundary(n) {
        n += 1;
    }
    n
}

/// Replace the bodies of tool results older than `keep_turns` turns whose
/// content is at least `min_bytes`, newest-first ordering preserved.
///
/// `turns` is the conversation in wire order. The last `keep_turns` turns are
/// never touched (the agent is still working with them); neither is a turn
/// already elided, a non-tool turn, or one below `min_bytes` (a `git status`
/// line is not costing anything).
pub fn elide_stale(turns: &mut [Turn], keep_turns: usize, min_bytes: usize) -> usize {
    if turns.is_empty() {
        return 0;
    }
    let cutoff = turns.len().saturating_sub(keep_turns);
    let mut elided = 0usize;
    for turn in turns.iter_mut().take(cutoff) {
        if turn.role != "tool" || is_elided(turn) || turn.content.len() < min_bytes {
            continue;
        }
        let original = turn.bytes();
        turn.content = elision_stub(turn.source.as_deref(), original);
        elided += 1;
    }
    elided
}

/// One file the conversation touched, as the digest reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Digest {
    pub files: Vec<String>,
    pub decisions: Vec<String>,
    pub open_todos: Vec<String>,
    /// Turns folded into this digest.
    pub turns: usize,
    /// Bytes those turns occupied before folding.
    pub bytes_before: u64,
    /// Earlier digests absorbed by this one (0 on a first pass). Reported so a
    /// reader knows the lists also describe work folded in previous passes.
    pub absorbed_digests: usize,
}

impl Digest {
    /// Render the digest as the system-role message that replaces the folded
    /// turns. Marked so [`compact`] never folds it again.
    pub fn render(&self) -> String {
        let mut out = String::from(COMPACT_MARKER);
        out.push_str(&format!(
            " A structured digest of {} earlier turn(s) ({:.1} KiB) that were folded to keep the \
prompt small. The work they describe is already on disk; re-read a file if you need its text \
again. Do not redo what is listed as decided.",
            self.turns,
            self.bytes_before as f64 / 1024.0
        ));
        if self.absorbed_digests > 0 {
            out.push_str(&format!(
                " It absorbs {} earlier digest(s); their file, decision and todo lists are \
merged below.",
                self.absorbed_digests
            ));
        }
        let mut list = |label: &str, items: &[String]| {
            if items.is_empty() {
                return;
            }
            out.push_str(&format!("\n{label}: "));
            out.push_str(&items.join("; "));
        };
        list("Files touched", &self.files);
        list("Decisions", &self.decisions);
        list("Open todos", &self.open_todos);
        out.push('\n');
        out
    }
}

/// Marker every compaction digest starts with.
pub const COMPACT_MARKER: &str = "[digest of earlier turns:";

/// True when `turn` is a compaction digest.
pub fn is_digest(turn: &Turn) -> bool {
    turn.content.starts_with(COMPACT_MARKER)
}

/// Extract the digest from a folded conversation.
///
/// Deliberately deterministic and extractive, not a model call: summarising
/// with a model would itself spend the tokens compaction exists to save, and
/// a lossy summary of a lossy summary is how an agent forgets its own
/// decision. It walks the folded turns and records:
///
/// * **files** — every path mentioned in a `diff --git a/X b/X`, `+++ b/X`,
///   or a `re-read … file` stub, deduplicated, capped;
/// * **decisions** — assistant sentences containing a decision verb
///   (`decided`, `will`, `must`, `because`, `instead`) — the closest thing a
///   transcript has to "what we settled on";
/// * **open todos** — `TODO`, `FIXME`, `XXX` markers and "still failing"
///   phrases from tool output, plus reviewer next-steps lines.
pub fn digest_turns(turns: &[Turn], max_files: usize, max_lines: usize) -> Digest {
    let mut digest = Digest {
        turns: turns.len(),
        bytes_before: turns.iter().map(Turn::bytes).sum(),
        ..Default::default()
    };
    let push = |v: &mut Vec<String>, item: &str, cap: usize| {
        let item = item.trim();
        if item.is_empty() || v.len() >= cap || v.iter().any(|x| x == item) {
            return;
        }
        v.push(item.to_string());
    };
    for turn in turns {
        for line in turn.content.lines() {
            let t = line.trim();
            if let Some(path) = diff_path(t) {
                push(&mut digest.files, &path, max_files);
            } else if let Some(path) = reread_path(t) {
                push(&mut digest.files, &path, max_files);
            }
            let lower = t.to_ascii_lowercase();
            // Todo markers and "still broken" phrases are what the next agent
            // must pick up. Assistant lines that merely *mention* a TODO while
            // recording a decision belong in `decisions`, not here.
            // A TODO the agent wrote itself is exactly what must survive, so
            // assistant lines count too; only the decision text has its todo
            // tail stripped so the same sentence is not recorded twice.
            let is_todo_line = lower.contains("todo")
                || lower.contains("fixme")
                || lower.contains("xxx")
                || lower.contains("still failing")
                || lower.contains("does not compile")
                || t.starts_with("Required next steps:")
                || t.starts_with("- [");
            if is_todo_line && t.len() < 200 {
                push(&mut digest.open_todos, &squash(t), max_lines);
            }
            if turn.role == "assistant" && is_decision_line(&lower) {
                push(
                    &mut digest.decisions,
                    &squash(&strip_todo_tail(t)),
                    max_lines,
                );
            }
        }
    }
    digest
}

/// Drop a trailing "TODO: …" clause from a decision sentence so the same text
/// is not recorded as both a decision and an open todo.
///
/// `to_ascii_lowercase` maps bytes 1:1 (A-Z only) so an index found in `lower`
/// IS a valid index in `s` — but only if `s` is not sliced on a byte inside a
/// multi-byte character that happens to share a byte with a marker. The index is
/// re-validated against `s` itself rather than trusted: a marker match that does
/// not land on a char boundary of the ORIGINAL string is skipped, so a CJK or
/// emoji sentence before the marker can never panic the fold.
fn strip_todo_tail(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    for marker in ["todo:", "todo ", "fixme:", "fixme "] {
        if let Some(i) = lower.find(marker) {
            if i > 0 && s.is_char_boundary(i) {
                return s[..i]
                    .trim_end_matches([' ', '.', ',', ';', '-'])
                    .to_string();
            }
        }
    }
    s.to_string()
}

/// Longest a squashed digest line may be, INCLUDING the ellipsis.
const SQUASH_MAX: usize = 160;

/// Collapse whitespace and bound a line to [`SQUASH_MAX`] bytes on a char
/// boundary. The ellipsis is reserved out of the budget, so the result never
/// exceeds the cap.
///
/// The FIRST word is allowed to overflow the budget into the cut branch: a line
/// that begins with a long path or URL (`/home/x/very/long/…`, a diff header) is
/// the informative case, and the pre-fix loop broke before writing anything and
/// returned just `"..."`, silently dropping exactly the file the digest needed
/// to name for the agent to re-read it.
fn squash(s: &str) -> String {
    let mut out = String::with_capacity(SQUASH_MAX.min(s.len()));
    let budget = SQUASH_MAX - 3; // room for "..."
    for w in s.split_whitespace() {
        let extra = if out.is_empty() { w.len() } else { w.len() + 1 };
        if out.len() + extra > budget {
            if !out.is_empty() {
                out.push_str("...");
                break;
            }
            // A leading word longer than the budget is cut, not dropped. Only a
            // zero-width cut (the whole budget inside one multi-byte character)
            // falls through to the ellipsis-only result below.
            let cut = floor_char_boundary(w, budget);
            if cut == 0 {
                break;
            }
            out.push_str(&w[..cut]);
            out.push_str("...");
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    // A line whose leading word could not be cut at all still reports that
    // something was dropped rather than squashing to an empty string, which
    // `push` would then discard as if the line had never been there.
    if out.is_empty() && !s.trim().is_empty() {
        out.push_str("...");
    }
    out
}

fn diff_path(line: &str) -> Option<String> {
    if let Some(rest) = line.strip_prefix("diff --git ") {
        let b = rest.split_whitespace().find(|t| t.starts_with("b/"))?;
        return Some(b.trim_start_matches("b/").to_string());
    }
    let rest = line.strip_prefix("+++ ")?;
    if rest.trim() == "/dev/null" {
        return None;
    }
    Some(rest.trim().trim_start_matches("b/").to_string())
}

fn reread_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("[elided:")?;
    let tail = rest.split(" from ").nth(1)?;
    let path = tail.split(" to save context").next()?.trim();
    (!path.is_empty() && path != "tool output").then(|| path.to_string())
}

fn is_decision_line(lower: &str) -> bool {
    // Phrases that read as a settled decision. Matched on word boundaries
    // (`" will "` alone would miss "Will refactor…" at the start of a line and
    // "…we must" at the end, silently dropping those from the digest).
    const PHRASES: [&str; 7] = [
        "we decided",
        "i decided",
        " will ",
        " must ",
        "because",
        "instead of",
        "plan:",
    ];
    let padded = format!(" {lower} ");
    PHRASES.iter().any(|k| padded.contains(k))
        || lower.starts_with("will ")
        || lower.ends_with(" must")
}

/// Result of one compaction pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Compacted {
    pub turns: Vec<Turn>,
    /// Turns folded into the digest (0 when compaction did not run).
    pub folded: usize,
    /// Bytes in, bytes out — the saving the operator can book.
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Fold older turns into a digest once the conversation crosses
/// `threshold_tokens`, keeping `keep_recent` turns verbatim.
///
/// Contract:
/// * the LAST `keep_recent` turns are never folded (the agent needs the live
///   tool results it is editing against), and are reproduced byte for byte —
///   including a digest that happens to sit in that window;
/// * a system turn or the first user turn (the brief) is never folded — the
///   brief is the task, and losing it is how a loop forgets what it was asked
///   to do;
/// * below the threshold nothing moves, byte for byte;
/// * folding is idempotent: an existing digest INSIDE THE FOLDED SPAN is
///   REUSED (its `files`/`decisions`/`open_todos` carry forward and the new
///   turns are merged into it), never nested inside a fresh one. A second pass
///   over an already-compacted conversation therefore produces exactly one
///   digest. A digest outside that span is not absorbed: absorbing the one in
///   the tail would delete a turn the agent is still working against.
pub fn compact(
    turns: &[Turn],
    threshold_tokens: u64,
    keep_recent: usize,
    max_files: usize,
    max_lines: usize,
) -> Compacted {
    let bytes_before: u64 = turns.iter().map(Turn::bytes).sum();
    let tokens = estimate_tokens(bytes_before);
    let unchanged = Compacted {
        turns: turns.to_vec(),
        folded: 0,
        bytes_before,
        bytes_after: bytes_before,
    };
    if threshold_tokens == 0 || tokens <= threshold_tokens {
        return unchanged;
    }
    let cutoff = turns.len().saturating_sub(keep_recent);
    // Find the last foldable candidate: not the brief, not a system prompt,
    // not a digest.
    let mut end = cutoff;
    while end > 0 {
        let t = &turns[end - 1];
        if t.role == "system" || is_digest(t) || is_brief(turns, end - 1) {
            end -= 1;
        } else {
            break;
        }
    }
    if end == 0 {
        return unchanged;
    }
    let (old, recent) = turns.split_at(end);
    // The brief and the system prompt are never folded even though they sit at
    // the FRONT of the span: they are the task and the contract, and a digest
    // that swallows them leaves the agent working from a summary of its own
    // instructions. Everything before the brief is pinned too, since nothing
    // meaningful precedes it.
    let pinned_end = {
        let mut n = 0;
        while n < old.len() {
            let t = &old[n];
            if t.role == "system" || is_brief(old, n) {
                n += 1;
            } else {
                break;
            }
        }
        n
    };
    let (pinned, foldable) = old.split_at(pinned_end);
    if foldable.is_empty() {
        return unchanged;
    }
    // Carry an earlier digest forward instead of summarising it: `digest_turns`
    // would otherwise fold "a summary of the earlier turns" into a summary of a
    // summary, and every later pass would nest one level deeper.
    //
    // Only the digests that are actually being REPLACED are absorbed: every one
    // inside the foldable span. A digest that sits in the pinned head or in the
    // verbatim `keep_recent` tail is NOT — the head digest and the tail are part
    // of the conversation contract (`the last keep_recent turns are never
    // folded`), and absorbing a tail digest while also filtering it out of the
    // tail deleted turns the agent was still working against. The pre-fix code
    // scanned the whole conversation and then dropped every digest from `out`,
    // so a digest inside the window vanished with only a summary line left.
    let prior: Vec<&Turn> = foldable.iter().filter(|t| is_digest(t)).collect();
    let foldable_fresh: Vec<Turn> = foldable.iter().filter(|t| !is_digest(t)).cloned().collect();
    // The digest describes the turns it replaces: the foldable span, not the
    // pinned head it is rendered alongside.
    let mut digest = digest_turns(&foldable_fresh, max_files, max_lines);
    // Carry a previous digest forward. Its LISTS are merged, but its
    // `turns`/`bytes_before` are its own historical accounting for turns that
    // are no longer present and are already reflected in the rendered header of
    // the digest this one replaces. Adding them to the fresh counts would make
    // the header claim more turns and bytes than this pass ever saw, growing
    // without bound across passes. The merged digest reports this pass's span
    // plus the number of earlier digests it absorbed.
    let mut absorbed = 0usize;
    for prior_digest in prior {
        let carried = digest_from_render(&prior_digest.content);
        digest.files = merge_capped(digest.files, carried.files, max_files);
        digest.decisions = merge_capped(digest.decisions, carried.decisions, max_lines);
        digest.open_todos = merge_capped(digest.open_todos, carried.open_todos, max_lines);
        absorbed += 1;
    }
    if absorbed > 0 {
        // The rendered header notes the absorption so the count is honest
        // rather than silently omitting work that really did happen.
        digest.absorbed_digests = absorbed;
    }
    // Order: pinned system prompt and brief, then the digest, then the recent
    // turns verbatim. The pinned head is reproduced VERBATIM, digest or not: a
    // digest sitting there was never absorbed (only the foldable span is), so
    // filtering it out would delete its carried lists without merging them and
    // without counting it in `folded`. The tail is verbatim for the same reason.
    let mut out: Vec<Turn> = Vec::with_capacity(pinned.len() + recent.len() + 1);
    out.extend(pinned.iter().cloned());
    out.push(Turn::new("system", digest.render()));
    out.extend(recent.iter().cloned());
    let bytes_after: u64 = out.iter().map(Turn::bytes).sum();
    Compacted {
        turns: out,
        // Turns actually REMOVED from the conversation and replaced by the
        // digest: the foldable span only. The pinned system prompt and brief are
        // still present verbatim, and an absorbed digest was replaced rather
        // than folded, so neither is counted.
        folded: foldable.iter().filter(|t| !is_digest(t)).count(),
        bytes_before,
        bytes_after,
    }
}

fn merge_capped(mut into: Vec<String>, from: Vec<String>, cap: usize) -> Vec<String> {
    for item in from {
        if into.len() >= cap {
            break;
        }
        if !into.contains(&item) {
            into.push(item);
        }
    }
    into
}

/// Re-read a rendered digest's lists. Only used when carrying an earlier digest
/// forward, so the field order and separators must match
/// [`Digest::render`]. A line that does not parse is skipped rather than
/// guessed at.
fn digest_from_render(text: &str) -> Digest {
    let mut out = Digest::default();
    let field = |line: &str, label: &str| -> Option<Vec<String>> {
        let rest = line.strip_prefix(label)?;
        Some(
            rest.split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        )
    };
    if let Some(head) = text.strip_prefix(COMPACT_MARKER) {
        // " A structured digest of N earlier turn(s) (x KiB) that were ..."
        // split_whitespace yields ["A","structured","digest","of","N",...], so
        // the count is the FIRST numeric word. Finding it rather than indexing a
        // fixed position keeps this working if the sentence is reworded.
        let words: Vec<&str> = head.split_whitespace().collect();
        if let Some(n) = words.iter().find_map(|w| w.parse::<usize>().ok()) {
            out.turns = n;
        }
    }
    for line in text.lines().map(str::trim) {
        if let Some(v) = field(line, "Files touched: ") {
            out.files = v;
        } else if let Some(v) = field(line, "Decisions: ") {
            out.decisions = v;
        } else if let Some(v) = field(line, "Open todos: ") {
            out.open_todos = v;
        }
    }
    // " It absorbs N earlier digest(s); ..."
    if let Some(rest) = text.split("It absorbs ").nth(1) {
        if let Some(n) = rest
            .split_whitespace()
            .next()
            .and_then(|w| w.parse::<usize>().ok())
        {
            out.absorbed_digests = n;
        }
    }
    out
}

/// The conversation's brief: the first non-system user turn.
fn is_brief(turns: &[Turn], idx: usize) -> bool {
    match turns.get(idx) {
        Some(t) if t.role == "user" && !is_digest(t) => !turns[..idx]
            .iter()
            .any(|t| t.role == "user" && !is_digest(t)),
        _ => false,
    }
}

/// Provider-model tunables resolved from config. Numbers here are the
/// measured defaults from the 2026-10-25 DeepSeek study; see
/// `docs/CONTEXT-OPTIMIZATION.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Levers {
    /// Per-tool-output byte cap. 0 disables the cap.
    pub tool_output_cap_bytes: usize,
    /// Elide tool outputs older than this many turns. 0 disables elision.
    pub elide_after_turns: usize,
    /// Never elide a tool output smaller than this.
    pub elide_min_bytes: usize,
    /// History-compaction trigger in estimated prompt tokens. 0 disables.
    pub compact_after_tokens: u64,
    /// Turns kept verbatim on the tail of a compaction.
    pub keep_recent_turns: usize,
}

impl Levers {
    /// DeepSeek direct (1M window, disk prefix cache, `$0.003/M` cache read).
    /// Compaction at 48K: the measured loop averaged a ~28K prompt before its
    /// first edit, and 48K is 1.7x that — high enough that a legitimately
    /// large single-file task is never folded, low enough that a 128K working
    /// set is folded instead of resent on every one of ~250 calls.
    pub const DEEPSEEK: Levers = Levers {
        tool_output_cap_bytes: 16_384,
        elide_after_turns: 8,
        elide_min_bytes: 2_048,
        compact_after_tokens: 48_000,
        keep_recent_turns: 6,
    };
    /// MiniMax-M3 / M3.1-Flash (subscription-metered, no usage cache counter).
    /// A tighter working set: the plan meters by tokens, and the M3 window is
    /// 200K while a review chunk is 48K, so mid-30K keeps author turns off
    /// the top of the window.
    pub const MINIMAX: Levers = Levers {
        tool_output_cap_bytes: 12_288,
        elide_after_turns: 6,
        elide_min_bytes: 2_048,
        compact_after_tokens: 32_000,
        keep_recent_turns: 6,
    };
    /// Every other provider (local vLLM, EIH, gateways whose cache shape is
    /// `CachedSubset`). Caps and elision on, compaction off: a local model has
    /// no per-token bill and a summarised history is pure quality risk.
    pub const OTHER: Levers = Levers {
        tool_output_cap_bytes: 16_384,
        elide_after_turns: 8,
        elide_min_bytes: 2_048,
        compact_after_tokens: 0,
        keep_recent_turns: 6,
    };

    /// Bytes a conversation would occupy after every lever ran, in order.
    /// Used by the benchmark's dry-run mode and the tests; the CLI applies the
    /// levers one at a time so each saving is attributable.
    pub fn apply(&self, turns: &[Turn]) -> Compacted {
        // The saving is measured against the conversation as it arrived, not
        // against the already-capped intermediate, or the reported win understates
        // the caps and elision by their own savings.
        let original_bytes: u64 = turns.iter().map(Turn::bytes).sum();
        let mut capped: Vec<Turn> = turns
            .iter()
            .map(|t| {
                if t.role == "tool" {
                    cap_tool_output(t, self.tool_output_cap_bytes)
                } else {
                    t.clone()
                }
            })
            .collect();
        elide_stale(&mut capped, self.elide_after_turns, self.elide_min_bytes);
        let mut out = compact(
            &capped,
            self.compact_after_tokens,
            self.keep_recent_turns,
            64,
            40,
        );
        out.bytes_before = original_bytes;
        out
    }
}

impl Levers {
    /// Look a profile up by name, case-insensitively. `None` for a name that
    /// is not a built-in, so a config typo falls back to [`Levers::OTHER`]
    /// rather than to an arbitrary provider's numbers.
    pub fn named(name: &str) -> Option<Levers> {
        match name.trim().to_ascii_lowercase().as_str() {
            "deepseek" => Some(Levers::DEEPSEEK),
            "minimax" => Some(Levers::MINIMAX),
            "other" => Some(Levers::OTHER),
            _ => None,
        }
    }

    /// Sniff a model id for its provider family (`deepseek-flash`,
    /// `MiniMax-M3`, `azure_ai/FW-DeepSeek-V4-Flash`) and return that profile.
    /// Anything else is [`Levers::OTHER`] — caps and elision, no compaction,
    /// because a local model has no per-token bill and a summarised history is
    /// pure quality risk for no saving.
    pub fn for_model(model: &str) -> Levers {
        let needle = model.to_ascii_lowercase();
        if needle.contains("deepseek") {
            Levers::DEEPSEEK
        } else if needle.contains("minimax") {
            Levers::MINIMAX
        } else {
            Levers::OTHER
        }
    }
}

impl Default for Levers {
    fn default() -> Self {
        Self::OTHER
    }
}

#[cfg(test)]
mod tests;
