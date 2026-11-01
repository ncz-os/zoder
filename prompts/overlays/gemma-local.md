# Overlay: local gemma

**Family key:** `gemma`
**Matches:** `gemma-4-31b-it`, `gemma-4-26b-a4b-it`, `gemma-7b-it`,
`google/gemma-*`, and any id containing `gemma`.
**Status of the notes below:** see the footer.

---

## Known weaknesses this overlay addresses

**W1 — Ending the turn on a status line.** This is the family's most
consistent failure: it narrates what it is about to do, then stops, and the
turn is scored as a non-result even though nothing was wrong with its
reasoning.

**W2 — Over-asking.** It asks whether to proceed before steps that were
already authorised.

**W3 — Malformed tool arguments.** It occasionally emits an argument object
with a trailing comma, which fails JSON parsing and costs a retry.

---

## Overlay text

<!-- zoder:prompt:start -->

## Model-specific notes (local gemma)

- **A turn ends with a tool call or with the final report.** Never end a
  turn with an intention. "Now I will update the parser" is not an ending —
  the tool call that updates the parser is. If you have described what you
  are about to do, the next thing in the message must be that tool call.
- **Do not ask before ordinary work.** Reading, searching, editing,
  building, and testing are already authorised. Ask only before a
  destructive action, in one line, naming the exact command.
- **Tool arguments are strict JSON.** No trailing commas, no single quotes,
  no comments. A trailing comma makes the whole call fail.
- **Finish the task.** If a test fails, fix it and re-run it before you
  report. Do not stop at the first edit that looks right.

<!-- zoder:prompt:end -->

---

## Evidence

| Claim | Kind | Source |
|-------|------|--------|
| gemma ids in the served corpus | MEASURED | `corpus/model_corpus.json` (`aiand/google/gemma-4-31b-it`, `aihubmix/gemma-4-26b-a4b-it`, …) |
| gemma is a known local reviewer route on this host (`custom.reviewer` serves `cerberus-reviewer` on the gemma-family vLLM box) | MEASURED | `~/.zoder/config.toml` `[providers.models.custom.reviewer]` + the fleet topology |
| W1 (turn ends on a status line) | **UNVERIFIED** | observed family behaviour, not logged |
| W2 (over-asking) | **UNVERIFIED** | observed family behaviour, not logged |
| W3 (malformed tool arguments) | **UNVERIFIED** | observed family behaviour, not logged |
