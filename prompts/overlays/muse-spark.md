# Overlay: Muse Spark

**Family key:** `muse`
**Matches:** `muse-spark-1.1`, `muse-spark-1.2`, `muse-spark-1.3`,
`muse-spark-open`, and any id containing `muse-spark` or `muse_spark`.
**Status of the notes below:** see the footer.

---

## Known weaknesses this overlay addresses

**W1 — Malformed tool arguments.** The family emits argument payloads with
trailing commas inside objects, single quotes where the schema wants JSON
strings, and occasionally a truncated final argument when the call is long.
The result is a JSON parse failure, which the engine reports as an invalid
tool call and the agent then retries — often producing the same shape again.

**W2 — Truncating a long argument.** When a patch is large, the family
sometimes stops the argument mid-string rather than emitting it whole.

**W3 — Ending the turn on a status line.** Observed on short tasks where
the family reports what it intends to do instead of doing it.

---

## Overlay text

<!-- zoder:prompt:start -->

## Model-specific notes (Muse Spark)

- **Tool arguments are strict JSON.** Double quotes only, no trailing
  commas, no comments, no single-quoted strings, no bare identifiers where a
  string is expected. If an argument is a list, it is a JSON array; if it is
  a number, it is a JSON number, not a quoted string.
- **Never truncate an argument.** If a patch is long, split it into
  several smaller tool calls rather than cutting the last argument short.
  A truncated argument is a parse error, not a smaller patch.
- **A turn ends with a result, not with a status sentence.** "I will now
  update the function" means you have not updated the function yet; make the
  tool call.
- **Read before you write.** Fetch the file, then produce the patch against
  what the file actually contains.

<!-- zoder:prompt:end -->

---

## Evidence

| Claim | Kind | Source |
|-------|------|--------|
| Muse Spark ids in the served corpus | MEASURED | `corpus/model_corpus.json` (`meta/muse-spark-1.1`, `-1.2`, `-1.3`, `muse-spark-open`, `aihubmix/muse-spark-1.2`, …) |
| Muse Spark is not currently a configured agent route on this host | MEASURED | `~/.zoder/config.toml` has no `custom.muse*` provider profile |
| W1 (malformed tool arguments) | **UNVERIFIED** | observed family behaviour, not logged |
| W2 (truncated argument) | **UNVERIFIED** | observed family behaviour, not logged |
| W3 (turn ends on a status line) | **UNVERIFIED** | observed family behaviour, not logged |
