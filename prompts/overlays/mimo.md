# Overlay: Xiaomi MiMo

**Family key:** `mimo`
**Matches:** `mimo-v2.6-flash`, `mimo-v2.6-pro`, `mimo-v2.5`, `mimo-v2-pro`,
`coding-xiaomi-mimo-v2.5`, and any id containing `mimo`.
**Status of the notes below:** see the footer.

---

## Known weaknesses this overlay addresses

**W1 — Reasoning tokens consuming the whole completion budget.** The MiMo
routes are reasoning models. A small reply has been measured spending 11–12
of 16–17 completion tokens on thinking before emitting the answer; at higher
efforts the reasoning grows again. A turn that reasons for the entire
completion budget produces no answer at all, and the engine reports an empty
response.

**W2 — Truncation at the completion limit.** Because the reasoning shares
the completion budget, a turn that reasons too long is cut off mid-answer.

**W3 — Ending the turn on a status line.** Observed on short tasks.

---

## Overlay text

<!-- zoder:prompt:start -->

## Model-specific notes (Xiaomi MiMo)

- **Spend your reasoning budget on the answer, not on the setup.** Do not
  restate the task, the plan, or the file contents before acting. Read what
  you need with a tool call, then think about the actual result.
- **Keep the answer short enough to fit.** This model has a small
  completion budget shared with its reasoning. A long final message can be
  truncated mid-sentence. Make the final report brief — a few lines — and
  put the detail in the code and the test output instead.
- **A turn ends with a result, not with a status sentence.** If you have
  not run the test, you have not verified anything; say so rather than
  implying otherwise.
- **Tool arguments must be complete and literal.** A full path, a complete
  string, an explicit integer. No placeholders, no truncation.

<!-- zoder:prompt:end -->

---

## Evidence

| Claim | Kind | Source |
|-------|------|--------|
| MiMo flash is `mimo-v2.6-flash` and pro is `mimo-v2.6-pro` on `custom.mimo_flash` / `custom.mimo_pro` | MEASURED | `~/.zoder/config.toml` |
| `reasoning_effort = "none"` yields 0 reasoning tokens and the right answer; `"low"` yields ~35 reasoning tokens; `"minimal"` returns HTTP 400 | MEASURED | operator note in the `custom.mimo_flash` block, 2026-10-26 |
| A tiny reply spends 11–12 of 16–17 completion tokens thinking at default effort | MEASURED | operator note in the same block, 2026-10-26 |
| W1 (reasoning-token runaway) | MEASURED at the route level; per-turn frequency **UNVERIFIED** | as above |
| W2 (truncation at the completion limit) | **UNVERIFIED** | no logged incident |
| W3 (turn ends on a status line) | **UNVERIFIED** | observed family behaviour, not logged |
