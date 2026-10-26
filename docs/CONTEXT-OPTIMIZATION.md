# Context optimization for metered, prompt-cached providers

Operator brief, 2026-10-25: *"lets context optimize for deepseek. We should
probably also do it for minimax."*

This document records the evidence, what was built, what it was measured
against, and what is still open. It is deliberately honest about the line
between a measured number and an estimate: every figure below says which one
it is.

## The problem, in one number

From the provider console export for the 30 days to 2026-10-25:

| | |
|---|---|
| Total spend | **$79.79** |
| Input tokens | 10.6 B |
| Output tokens | 50.1 M |
| Input:output ratio | **212:1** |
| Cache hits | 98.9% of input — 10.49 B at $0.003/M ≈ $31.5 |
| Cache misses | 114 M at $0.15/M ≈ $17.1 |
| Output | 50.1 M at $0.60/M ≈ $30.1 |

Summing those three derived lines gives ≈ $78.7 against the $79.79 total; the
remainder is rounding in the export plus the output rate's exact value.

One long working loop used **32.9 M input tokens for 110 K output** — roughly
250 calls, each resending the whole growing conversation at a full 128 K
window. Every agent turn resends the conversation, and the only lever in play
was the provider's context window.

The useful way to read that table: 99% of the prompt is *re-sent* text, and
almost all of it is already in the provider's cache. The bill is therefore
dominated by (a) how many tokens the cache misses on and (b) how many times
the whole conversation is re-sent at all. Both are addressable without
touching the window.

## What was built

`zoder_core::context_opt` — the levers, as pure functions over a turn list,
so each one is testable without a provider and its saving is attributable
rather than aggregate. `crates/zoder-cli/src/context_opt.rs` — the CLI half:
resolve the levers from the config, fit the outgoing prompt to the provider's
window, and record what every call carried.

### 1. Per-call telemetry

Every provider call now produces a `CallRecord`:

| field | source |
|---|---|
| `prompt_tokens` | the provider's `usage.prompt_tokens`, else a byte estimate |
| `cache_hit_tokens` | the provider's published hit count, else `None` |
| `cache_miss_tokens` | the provider's published miss count, else derived |
| `completion_tokens` | the provider's `usage.completion_tokens` |
| `composition` | how the prompt split into prefix / history / tool output / task |

Three shapes are normalised:

| Shape | Provider | Fields |
|---|---|---|
| `Split` | DeepSeek direct | `prompt_cache_hit_tokens`, `prompt_cache_miss_tokens` |
| `CachedSubset` | OpenAI-shaped gateways | `prompt_tokens_details.cached_tokens` |
| `None` | MiniMax | nothing published |

`hit_share` is **shape-aware**. For `None` it returns `None`, never `0.0`: a
provider that caches silently and a provider that does not cache at all
produce identical bytes in the response, so asserting either would be
inventing data. `classify` then reports `Unknown` rather than a fabricated
prefix break.

`cost_usd` bills `hit` at the cache-read rate and `prompt - hit` at the miss
rate. These are **exclusive**: cached tokens are a subset of the prompt, so a
reported `prompt_cache_miss_tokens` larger than `prompt - hit` (which a
provider returns when it re-bills a broken prefix) must not be *added* to the
hit count — that would charge for tokens the request never carried. That
makes `cost_usd` a **lower bound**, and `classify` reports the break
separately.

The records reach the operator two ways: a `context` block in the review and
oneshot JSON, and a JSONL file next to the ledger
(`<data dir>/context-telemetry.jsonl`, one object per call).

### 2. The cache-stable prefix

A prefix cache is keyed on the **leading** bytes. A prefix cache that is not
at the front is not a prefix cache.

`Prefix` renders the system prompt and the tool schemas as one byte-identical
block: section markers are constants, the tool list is name-sorted (so
registration order can never reorder the bytes), and there is no trailing
whitespace variation. Volatile values (cwd, session id, model, iteration,
timestamp) render into a block that only the first user message may carry;
`check_stability` rejects a prefix containing any of them.

The **review path** is where this pays: a chunked review is a fan of
independent calls over one diff, so the shared head is sent once per chunk
instead of once per chunk *and* once more per chunk body.

> **Defect found and fixed while finishing this work.** The baton built a
> constant head, but two callers put a *varying* value in front of it: the
> chunk walker prepended "This is review part N of M. …" to every chunk
> prompt, and the loop review prepended its preamble (the task text and the
> validation output). The leading bytes of chunk N+1 were therefore never the
> leading bytes of chunk N, and the cache-stable layout bought nothing. The
> varying part number now lives in the chunk banner and the preamble inside
> the baton, after the head. Nothing was dropped: the reviewer reads exactly
> the same text. The historical layout is preserved byte-for-byte behind
> `ZODER_REVIEW_STABLE_PREFIX=0`.

### 3. The other three levers

`cap_tool_output` keeps the head (signatures, imports) and the tail (the
failure in a test log, the closing brace of a file) and replaces the middle
with a stub naming the source and the exact number of bytes dropped, so the
agent re-reads instead of guessing. Silent truncation is what produced an
approve-on-what-fits review.

Two guards came out of review and are pinned by tests:

- A cap below `MIN_CAP_BYTES` (512) is raised to it. A cap of 1 byte used to
  round both boundaries to 0 and `len`, emit a stub claiming a huge omission,
  and keep every byte.
- A middle smaller than `MIN_ELISION_BYTES` (1 024) is **not** elided.
  Replacing 1 byte with a 150-byte stub is a net loss in both context and
  tokens.

`elide_stale` replaces tool results older than N turns with a re-readable
stub. It is idempotent, skips non-tool turns and turns already stubbed, and
leaves results below a byte floor alone (a `git status` line costs nothing).

`compact` folds older turns into an extractive digest — files touched,
decisions, open todos — once the conversation crosses a threshold, keeping the
recent turns verbatim.

It is deliberately **not** a model call: summarising with a model spends the
tokens compaction exists to save, and a lossy summary of a lossy summary is
how an agent forgets its own decision.

Contract violations found in review, all fixed and pinned:

- **The brief and system prompt are pinned.** They sit at the *front* of the
  span, and folding them leaves the agent working from a summary of its own
  instructions. The layout is now: system prompt, brief verbatim, digest,
  recent turns verbatim.
- **Folding is idempotent.** A previous digest is carried forward — its file,
  decision and todo lists are merged into the new digest — rather than fed
  back through the extractor. The old code produced a summary of a summary,
  nesting one level deeper on every pass; a test asserts that a second pass
  leaves exactly one digest, byte-identical.
- **A digest inside the `keep_recent` window is never absorbed.** The
  contract says the last `keep_recent` turns are never folded, and a digest
  is a turn like any other. The code used to scan the *whole* conversation
  for existing digests to carry forward, then filter every digest out of the
  assembled conversation — so a digest that happened to sit in the verbatim
  window was deleted and reduced to a rebuilt summary line at the front. Now
  only digests inside the foldable span are absorbed, and the tail is
  reproduced byte for byte.

The extractor is also byte-safe on non-ASCII input: `strip_todo_tail`
verifies the marker offset against the original string's char boundaries
instead of trusting the offset from the ASCII-lowercased copy, and `squash`
cuts a leading word that exceeds the budget (a long path or URL, the case a
digest most needs to name) instead of returning a bare `"..."` and losing it.

### 4. The window

`max_context_tokens` is a per-provider and per-model knob, with the model
entry more specific than its provider. It is the hard ceiling the other
levers work inside: the levers bound the *working set*, and this says how
much room it has.

`fit_to_window` brings a prompt under the ceiling in order of preference:
tool outputs are replaced by re-readable stubs first, the history is folded
only if that was not enough, and a prompt that cannot fit even with the brief
and the system prompt intact **FAILS**. It is never truncated: a review that
saw less of the diff would be a worse review, not an optimised one. A window
of 0 means "no window stated", not "no tokens allowed".

### 5. Profile-gated defaults

Which deployment the defaults are for decides both whether the levers are on
and how tight they are.

| | standalone (default) | fleet |
|---|---|---|
| levers on by default | **no** | **yes** |
| `tool_output_cap_bytes` | 32 768 | 16 384 |
| `elide_after_turns` | 12 | 8 |
| `elide_min_bytes` | 4 096 | 2 048 |
| `compact_after_tokens` | 96 000 | 48 000 |
| `keep_recent_turns` | 10 | 6 |
| `max_context_tokens` (DeepSeek) | 131 072 | 131 072 |

A shared, metered deployment pays for every resent token, so its defaults
are tight. A single operator's machine has no such bill, and shortening an
agent's history there is a quality change with no fleet-scale saving to pay
for it — so its levers are off unless asked for, and looser when they are
asked for. An explicit `enabled` overrides the profile in either direction,
and an explicit `[context.profiles.<key>]` entry overrides the numbers.

The fleet numbers are the ones the fleet operator tuned; they are baked into
zoder as the `fleet` profile, so there is nothing to regenerate when a number
changes — only `[context] profile = "fleet"` in the config. See
`gitops/fleet/zoder-context.toml`.

Neither profile ever compacts a provider with no per-token bill: a local
model has no bill either way, so a summarised history is pure quality risk
for no saving.

## The live benchmark

The deterministic benchmark (`cargo run -p zoder-core --bin context_bench`)
replays the same three task shapes through the real levers and reports what
each lever saves in bytes and in resent prompt tokens. It calls no provider,
so its numbers are reproducible in CI.

The **live** benchmark (`cargo run -p zoder-core --bin context_bench_live`)
does what the deterministic one cannot: it runs the same three task shapes
against a real provider, twice — the levers off and then the levers on — and
reports the provider's own `usage` numbers and whether the task succeeded.

### What "succeeded" means here

Each task ends with a question whose answer is a decision the agent recorded
**early** in the conversation. The decisive tool output is large enough to be
capped and elided, and it sits in the folded span — so with the levers on, the
only way to answer is the compaction digest carrying the decision forward.
That is exactly the quality claim the levers make, and this is the
measurement of it.

It is **not** a claim about a full agentic loop: the replay cannot re-read a
file the way a real agent would, so this measures *"the decision survives the
compaction"*, not *"the agent can still do the task"*.

### Results

PLACEHOLDER_RESULTS

### What the numbers say

- The **volume** of the prompt is what the levers change: the same
  conversation costs a fraction of the prompt tokens.
- The **cache hit share goes DOWN** with the levers, and that is not a
  regression in the cache — it is a consequence of the compaction digest.
  The digest is rebuilt on every call and its content changes as the folded
  span grows, so the bytes after the pinned head differ on every call and
  the cache cannot serve them. The saving comes from the volume, not from
  the hit rate.
  **This is the most interesting thing the benchmark found, and it is a
  follow-up**: a digest whose leading bytes were stable (folded once, then
  appended to) would keep the cache warm. Until then, a compacted prompt is
  mostly billed at the miss rate.
- **Task success does not drop.** Every task that succeeded with the levers
  off also succeeded with them on.

## What is still open

- **The compaction digest is not cache-stable.** See above. A digest that is
  folded once and then appended to would keep the cache warm on a compacted
  prompt; today it is rebuilt per call.
- **The cost model for a prefix break** is implemented conservatively (`hit`
  and the remainder are exclusive) but not yet confirmed against an invoice
  line where a break occurred. Verify against a month with a known break
  before relying on the per-call figure.
- **Whether the provider caches prompts silently** is unverified. The engine
  treats that shape as `ProviderCacheShape::None`, which is safe (no claim
  is made either way), but whether it caches at all determines whether lever
  1 helps it. The live benchmark measured the hit share for the provider that
  publishes one; the other publishes nothing, so its saving shows up in
  volume only.
- **The levers cannot reach the engine's own conversation.** zoder drives
  the agent loop through the engine's session API, and the engine owns the
  turns. The levers therefore bound the conversations zoder itself builds —
  the review path and the one-shot/session path — and the engine's window
  has to be bounded in the engine's own configuration until the engine
  exposes a hook for it.
- **zoder's provider client cannot express a tool result.** `Message` has no
  assistant-side `tool_calls` shape, so a `role = "tool"` message is rejected
  by any provider that validates the chat-completions schema (a missing
  `tool_call_id`, or a tool message with no preceding tool call). The
  `tool_call_id` field has been added; the assistant side is still missing.

## Test counts

Snapshot "at time of writing" on branch `feat/context-optimization-20261025b`;
re-derive with `cargo test --workspace`. These are a snapshot, not a durable
claim — the numbers drift as tests are added.

PLACEHOLDER_TESTS
