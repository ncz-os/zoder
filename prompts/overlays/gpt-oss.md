# Overlay: gpt-oss

**Family key:** `gpt-oss`
**Matches:** `gpt-oss-20b`, `gpt-oss-120b`, `openai/gpt-oss-*`, and any id
containing `gpt-oss`.
**Status of the notes below:** see the footer.

---

## Known weaknesses this overlay addresses

**W1 — Reasoning-token runaway.** The gpt-oss family uses a Harmony
reasoning format and can spend a very large number of reasoning tokens
before its first visible token. On a review chunk that meant the whole
output budget was consumed by reasoning and the JSON verdict never arrived,
which the strict parser reports as "invalid JSON".

**W2 — The reasoning block leaking into the answer.** The visible reply
sometimes begins with the reasoning channel's text, so a parser looking for
a JSON object at the start of the reply fails even though the JSON is
present later in the message.

**W3 — Truncated JSON.** When the output budget is consumed by reasoning,
the answer itself is cut off mid-object.

---

## Overlay text

<!-- zoder:prompt:start -->

## Model-specific notes (gpt-oss)

- **Keep the reasoning short.** Do not deliberate at length before acting.
  Read what you need, call the tool, and reason about the actual result.
  Long deliberation before the first tool call is the main way this model
  runs out of output budget.
- **Put the answer first.** When you are asked for a specific machine-
  readable format, the first characters of your reply are that format. Do
  not put a discussion, a restatement of the question, or a reasoning
  summary in front of it.
- **Finish the object.** If you are asked for JSON, close every brace and
  every bracket before you stop. A truncated object is not a valid answer.
- **One tool call per message.** Send a single call, wait for its result,
  then send the next one.

<!-- zoder:prompt:end -->

---

## Evidence

| Claim | Kind | Source |
|-------|------|--------|
| gpt-oss ids in the served corpus | MEASURED | `corpus/model_corpus.json` (`aiand/openai/gpt-oss-120b`, `aihubmix/gpt-oss-20b`, …) |
| `gpt-oss` is a distinct cross-family reviewer in this fleet | MEASURED | `crates/zoder-cli/src/agentic.rs` (`"gpt-oss-20b"` in the reviewer candidate set) and `crates/zoder-cli/src/review_confirm.rs` (the "never the reviewer's own family" rule) |
| The gpt-oss family uses a reasoning channel whose output can precede the visible answer | MEASURED | the `strip_leading_reasoning_block` helper exists in `crates/zoder-cli/src/agentic.rs` precisely because this happens |
| Reasoning models can spend the entire output budget before emitting the verdict | MEASURED | `crates/zoder-cli/src/agentic.rs` — `REVIEW_MIN_MAX_TOKENS` was raised from 2 048 to 8 192 after a measured 6 769-token reasoning prefix on a Nemotron chunk |
| W1 (runaway) frequency | **UNVERIFIED** | the budget exhaustion is measured; the per-turn rate is not |
| W2 (reasoning block leaking into the answer) | MEASURED | the helper exists for it; per-family attribution **UNVERIFIED** |
| W3 (truncated JSON) | **UNVERIFIED** | no logged incident |
