# Overlay: DeepSeek V4.x flash

**Family key:** `deepseek`
**Matches:** `deepseek-flash`, `deepseek-v4-flash`, `deepseek-v4.1-flash`,
`deepseek-v4-pro`, and any id containing `deepseek` (see
`zoder_core::prompt::family_for_model`).
**Status of the notes below:** UNVERIFIED for the behaviour claims (see the
footer); the deployment facts are MEASURED and cited.

---

## Known weaknesses this overlay addresses

**W1 — Batching tool calls into a single message.** DeepSeek V4 flash
emits several independent tool calls in one assistant message, and the
zeroclaw bridge occasionally surfaces them as one merged call whose
arguments contain more than one tool. When that happens the arguments do not
parse against the declared schema and the call is dropped, which silently
costs the agent a whole turn.

> **Note: UNVERIFIED.** No incident record ties a dropped DeepSeek call to
> merged arguments. The guard below is cheap and harmless if the behaviour
> never occurs, and it removes a failure mode that would otherwise be
> invisible (a dropped call looks exactly like a model that gave up).

**W2 — Ending the turn on a status line.** This family is prone to closing
a turn with a progress sentence ("I have updated the configuration file")
instead of a tool call or a result. On a long turn that reads as completion.

**W3 — Reasoning-token runaway.** DeepSeek's thinking blocks are billed as
output and can be very long relative to the answer. A turn that thinks for
thousands of tokens before its first tool call burns the operator's budget
without producing work.

---

## Overlay text

<!-- zoder:prompt:start -->

## Model-specific notes (DeepSeek V4.x flash)

- **One tool call per message.** Emit exactly one tool call in each
  assistant message, then wait for its result. If you believe two calls are
  independent, still send them one at a time. A merged or concatenated
  argument blob is a hard error and the call will be dropped.
- **Never end a turn on a status sentence.** A turn ends with a tool call or
  with the final report. "I have made the changes" is not an ending; it is
  the point at which you make the next tool call.
- **Reasoning discipline.** Do not restate the problem, the plan, and the
  file contents in your thinking before acting. Read what you need, then
  call the tool. Reason about the *result* of a call, not about what you
  expect it to contain.
- **Tool arguments must be complete and literal.** Every argument is a
  finished value of the declared type — a full path, a complete string, an
  explicit integer. No placeholders, no `...`, no "the rest of the file".

<!-- zoder:prompt:end -->

---

## Evidence

| Claim | Kind | Source |
|-------|------|--------|
| DeepSeek V4 flash is served as `deepseek-flash` on `custom.deepseek` | MEASURED | `~/.zoder/config.toml` `[providers.models.custom.deepseek]` |
| `reasoning_effort = "none"` yields 0 reasoning tokens on that route | MEASURED | operator note in the same config block, 2026-10-03 |
| W1 (merged tool arguments) | **UNVERIFIED** | no incident record; defensive guard only |
| W2 (turn ends on a status line) | **UNVERIFIED** | observed family behaviour, not logged |
| W3 (reasoning-token runaway) | **UNVERIFIED** | observed family behaviour, not logged |
