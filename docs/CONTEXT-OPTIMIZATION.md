# Context optimization for DeepSeek and MiniMax

Operator brief, 2026-10-25: *"lets context optimize for deepseek. We should
probably also do it for minimax."*

This document records the evidence, the levers, what each one is expected to
save, and the fleet config deltas to apply after review. It is deliberately
honest about what is measured and what is still estimated: the benchmark runs
that would turn every number below into a measured one cost real money and are
called out as **pending** where that is the case.

## The problem, in one number

From the DeepSeek console export for the 30 days to 2026-10-25:

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
remainder is rounding in the export plus the output rate's exact value. The
rates used are the ones the operator's pricing config carries for
deepseek-flash ($0.003 / $0.15 / $0.60 per M). If a review finds the miss rate
is not $0.15/M, the derived dollar figures above are the lines to correct.

One `zoder` loop on one host used **32.9 M input tokens for 110 K output** —
roughly 250 calls each resending a full 128 K window. Every agent turn resends
the whole growing conversation, and the only lever in play today is
`max_context_tokens` per profile in `gitops/fleet/zoder-config/config.toml`.

The useful way to read that table: 99% of the prompt is *re-sent* text, and
almost all of it is already in the provider's cache. The bill is therefore
dominated by (a) how many tokens the cache misses on and (b) how many times the
whole conversation is re-sent at all. Both are addressable without touching
`max_context_tokens`.

## What was built

`zoder_core::context_opt`. Every lever is a pure function over a `Turn` list, so
it is testable without a provider and its saving is attributable rather than
aggregate.

### Telemetry

`CacheTelemetry` normalizes the three provider reporting shapes:

| Shape | Provider | Fields |
|---|---|---|
| `Split` | DeepSeek | `prompt_cache_hit_tokens`, `prompt_cache_miss_tokens` |
| `CachedSubset` | OpenAI-shaped gateways | `prompt_tokens_details.cached_tokens` |
| `None` | MiniMax | nothing published |

`hit_share` is **shape-aware**. For `None` it returns `None`, never `0.0`: a
provider that caches silently and a provider that does not cache at all produce
identical bytes in the response, so asserting either would be inventing data.
`classify` then reports `Unknown` rather than a fabricated prefix break.

`cost_usd` bills `hit` at the cache-read rate and `prompt - hit` at the miss
rate. These are **exclusive**: cached tokens are a subset of the prompt, so a
reported `prompt_cache_miss_tokens` larger than `prompt - hit` (which DeepSeek
returns when it re-bills a broken prefix, and which includes the re-read schema
block) must not be *added* to the hit count — that would charge for tokens the
request never carried.

That makes `cost_usd` a **lower bound**: it does not use `reported_cache_miss`
at all, because the two candidate readings (the prompt was re-billed in full, or
the reported figure is the truth) cannot be told apart from the response. A
prefix break is instead reported as `CacheVerdict::PrefixBreak` by `classify`,
and when reconciling a month every call classified as a break should be costed
at `prompt × miss_rate`. This is the open cost-model item below.

Pinned by `context_opt::tests::cost_never_bills_more_tokens_than_the_prompt_carried`:
1 000 prompt tokens with 900 hits and a reported miss of 250 bills
900 × cache rate + 100 × miss rate, not 1 150 tokens. The uncached case is
pinned by `cost_with_no_cache_counter_bills_every_token_at_the_miss_rate`, and
the 30-day totals by `cost_bills_the_cached_tokens_once_at_the_cache_rate`.

`PromptComposition` reports what fraction of the prompt is prefix, history, tool
output and task. That is the number that says *which* lever to pull: a run at
90% tool output needs caps, a run at 80% history needs compaction.

### Lever 1 — cache-stable prefix (`prefix`)

DeepSeek's prefix cache is keyed on the exact leading bytes up to the last
64-token block. Anything volatile early in the prompt — a cwd, a session id, a
timestamp — invalidates the entire block from that point on, forever, while the
console still shows a plausible-looking total.

`Prefix` renders the system prompt and the tool schemas as one byte-identical
block: section markers are constants, the tool list is name-sorted (so
registration order can never reorder the bytes), and there is no trailing
whitespace variation. `render_first_message` is the only supported way to
assemble a request, and it places volatile values *after* the prefix.
`check_stability` rejects a prefix containing any of them.

One subtlety worth recording: the leak check ignores values shorter than 6
bytes. A bare iteration number (`"3"`) appears in any prompt by chance, and the
constant marker itself contains `v1` — flagging those made every check fail, so
the detector now only treats distinctive values as evidence.

**Target:** cache-hit share stays at or above **98%** (the measured status quo),
with no drop for runs that previously had none.

### Lever 2 — tool-output caps and stale elision

`cap_tool_output` keeps the head (signatures, imports) and the tail (the failure
in a test log, the closing brace of a file) and replaces the middle with a stub
naming the source and the exact number of bytes dropped, so the agent re-reads
instead of guessing. Silent truncation is what produced an approve-on-what-fits
review.

Two guards came out of review and are pinned by tests:

- A cap below `MIN_CAP_BYTES` (512) is raised to it. A cap of 1 byte used to
  round both boundaries to 0 and `len`, emit a stub claiming a huge omission,
  and keep every byte.
- A middle smaller than `MIN_ELISION_BYTES` (1 024) is **not** elided. Replacing
  1 byte with a 150-byte stub is a net loss in both context and tokens.

`elide_stale` replaces tool results older than N turns with a re-readable stub.
It is idempotent, skips non-tool turns and turns already stubbed, and leaves
results below a byte floor alone (a `git status` line costs nothing).

**Defaults:** 16 KiB cap / elide after 8 turns (DeepSeek); 12 KiB / 6 turns
(MiniMax).

### Lever 3 — history compaction

`compact` folds older turns into an extractive digest — files touched,
decisions, open todos — once the conversation crosses a threshold, keeping the
recent turns verbatim.

It is deliberately **not** a model call: summarising with a model spends the
tokens compaction exists to save, and a lossy summary of a lossy summary is how
an agent forgets its own decision.

Two contract violations found in review are fixed and pinned:

- **The brief and system prompt are pinned.** They sit at the *front* of the
  span, and folding them leaves the agent working from a summary of its own
  instructions. The layout is now: system prompt, brief verbatim, digest,
  recent turns verbatim.
- **Folding is idempotent.** A previous digest is carried forward — its file,
  decision and todo lists are merged into the new digest — rather than fed back
  through the extractor. The old code produced a summary of a summary, nesting
  one level deeper on every pass; a test asserts that a second pass leaves
  exactly one digest, byte-identical.

A third, found in the next review round and fixed:

- **A digest inside the `keep_recent` window is never absorbed.** The contract
  says the last `keep_recent` turns are never folded, and a digest is a turn
  like any other. The code used to scan the *whole* conversation for existing
  digests to carry forward, then filter every digest out of the assembled
  conversation — so a digest that happened to sit in the verbatim window was
  deleted and reduced to a rebuilt summary line at the front. Now only digests
  inside the foldable span are absorbed, and the tail is reproduced byte for
  byte. Two tests pin it: an all-in-window conversation comes back unchanged,
  and a tail digest survives a pass that folds earlier turns.

The extractor is also byte-safe on non-ASCII input: `strip_todo_tail` verifies
the marker offset against the original string's char boundaries instead of
trusting the offset from the ASCII-lowercased copy, and `squash` cuts a
leading word that exceeds the budget (a long path or URL, the case a digest
most needs to name) instead of returning a bare `"..."` and losing it.

**Thresholds:** 48 K tokens for DeepSeek — the measured loop averaged a ~28 K
prompt before its first edit, and 48 K is 1.7x that: high enough that a
legitimately large single-file task is never folded, low enough that a 128 K
working set is folded instead of re-sent on ~250 calls. 32 K for MiniMax, whose
window is 200 K and whose metering is by tokens.

### Profiles

| | `tool_output_cap_bytes` | `elide_after_turns` | `elide_min_bytes` | `compact_after_tokens` | `keep_recent_turns` |
|---|---|---|---|---|---|
| `Levers::DEEPSEEK` | 16 384 | 8 | 2 048 | 48 000 | 6 |
| `Levers::MINIMAX` | 12 288 | 6 | 2 048 | 32 000 | 6 |
| `Levers::OTHER` | 16 384 | 8 | 2 048 | 0 (disabled) | 6 |

`elide_min_bytes` is the floor below which a stale tool result is not worth
replacing: a `git status` line costs nothing and a stub would cost more.

`OTHER` caps and elides but never compacts. A local vLLM model has no
per-token bill, so a summarised history is pure quality risk for no saving.

## Measured savings in the unit tests

The levers are pure, so the unit tests can assert the saving directly rather
than through a bill. At the DeepSeek profile a 120-turn conversation (60 tool
results at 8 KiB each, each carrying a real diff) is cut to **under a quarter of
its bytes**. A 60-turn one is cut below the 48 K compaction threshold by caps
and elision alone and is therefore *not* compacted — folding a prompt that is
already small only risks quality, and a test pins that.

## What is still pending

Be explicit about this when reviewing:

- **The benchmark runs** (3 past job briefs × deepseek-flash and MiniMax-M3,
  recording tokens, turns, cost, wall time and success) have **not** been run.
  They cost real money against a $100/month ceiling that the 30 days to 10-25
  already consumed $79.79 of, and the brief caps benchmark spend at $5 with bulk
  runs off-peak (DeepSeek peak is 01:00–04:00Z). The before/after table and the
  quality gate ("no drop in task success") need those runs; this document should
  not claim them until they exist.
- **The cost model for DeepSeek's prefix break** is implemented conservatively
  (`hit` and the remainder are exclusive) but not yet confirmed against an
  invoice line where a break occurred. Verify against a month with a known
  break before relying on the per-call figure.
- **MiniMax's caching behaviour** is unverified. The engine treats it as
  `ProviderCacheShape::None`, which is safe (no claim is made either way), but
  whether MiniMax caches prompts at all determines whether lever 1 helps it.
  Measure before assuming it does.
- **The chunked review path** (16 chunks observed in one run) has not been
  checked for per-chunk full-context resends. If it resends, it is the largest
  remaining single win and belongs in a follow-up.

## Fleet config deltas

`gitops/fleet/zoder-config.patch` in this repository adds the per-provider lever
profiles to the fleet's own gitops config. It is **not** applied and **not**
pushed; the operator deploys after review. The hunk is a real unified diff
with explicit line ranges. It was dry-run clean against the fleet's
`config.toml` as of 2026-10-25; if that file has changed in its first lines the
patch will need regenerating (re-run `diff -u` against the current file).

`max_context_tokens` is deliberately left unchanged in this patch. The levers
make a large window affordable by keeping the *working set* small; shrinking the
window as well would change behaviour before the benchmark says it is safe.
Right-sizing it is the final step of the brief, after the measurements exist.

## Test counts

Snapshot "at time of writing" on branch `feat/context-optimization-20261025`;
re-derive with `cargo test -p zoder-core --lib`. These are a snapshot, not a
durable claim — the numbers drift as tests are added.

- `zoder-core` lib: **697 passed, 0 failed**.
- `context_opt` alone: **60 tests**.
- `cargo clippy -p zoder-core --all-targets`: **0 warnings**.
