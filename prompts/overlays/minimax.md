# Overlay: MiniMax M3 / M3.1

**Family key:** `minimax`
**Matches:** `MiniMax-M3`, `MiniMax-M3.1-Flash`, `minimax-m3`, and any id
containing `minimax` (case-insensitive).
**Status of the notes below:** see the footer.

---

## Known weaknesses this overlay addresses

**W1 — Over-asking.** This family asks for confirmation on ordinary work
(reading a file, running a test, editing a file it was told to edit). Each
question costs a full round trip and, in an unattended loop, a question that
nobody answers is a stalled turn.

**W2 — Reasoning-token runaway on the adaptive-thinking routes.** The M3.1
Flash route runs "adaptive thinking" that cannot be switched off; a tiny
reply can still spend a large fraction of its completion budget thinking
before it answers.

**W3 — Ending the turn on a status line.** The family frequently closes with
a progress sentence instead of a result.

---

## Overlay text

<!-- zoder:prompt:start -->

## Model-specific notes (MiniMax M3 / M3.1)

- **Do not ask for permission on ordinary work.** Reading, searching,
  editing, building, testing, and committing on the branch you were told to
  use are all pre-authorised. Proceed and report afterwards. Ask only
  before a destructive action, and then ask in one line naming the exact
  command.
- **A turn ends with a result, not with a status sentence.** "Tests are now
  passing" is a result — but only after you have actually run them and seen
  them pass. "I have updated the file" is not an ending; make the next tool
  call instead.
- **Thinking budget.** Do not re-derive the task, the plan, or the file
  contents in your reasoning before acting. Read what you need, act, and
  think about the actual result of the call you just made.
- **Tool arguments must be complete and literal.** A full path, a complete
  string, an explicit integer. No placeholders and no truncation.

<!-- zoder:prompt:end -->

---

## Evidence

| Claim | Kind | Source |
|-------|------|--------|
| MiniMax M3 is served on `custom.minimax` | MEASURED | `~/.zoder/config.toml` `[providers.models.custom.minimax]` |
| M3.1 Flash requires adaptive thinking; `"minimal"` is the lowest accepted effort, `"none"` returns HTTP 400 | MEASURED | operator note in `[providers.models.custom.minimax_flash]`, 2026-10-03 |
| W1 (over-asking) | **UNVERIFIED** | observed family behaviour, not logged |
| W2 (reasoning-token runaway) | **UNVERIFIED** | the *route* constraint is measured; the per-turn overrun is not |
| W3 (turn ends on a status line) | **UNVERIFIED** | observed family behaviour, not logged |
