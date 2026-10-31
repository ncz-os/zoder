# Overlay: local qwen3.x

**Family key:** `qwen`
**Matches:** `qwen38`, `qwen3.8-27b`, `qwen3.6-27b`, `qwen3-coder-next`,
`qwen3.5-122b-a10b`, and any id containing `qwen`.
**Status of the notes below:** see the footer.

---

## Known weaknesses this overlay addresses

**W1 — Batching tool calls.** The local qwen route is served with
`enable_thinking = false` and native tools. It is prone to putting two
independent tool calls in one assistant message, which the ACP bridge
rejects as an unparseable call.

**W2 — Stopping at the first plausible edit.** The route tends to make one
edit, see that it compiles, and stop, without running the test that covers
the behaviour it changed.

**W3 — Long tool arguments.** The route's patch payloads get truncated when
the change is large, which produces a half-applied file.

---

## Overlay text

<!-- zoder:prompt:start -->

## Model-specific notes (local qwen3.x)

- **One tool call per message.** Send a single tool call, wait for its
  result, then send the next one. Never put two calls in one message.
- **Run the test that covers your change.** A change that compiles is not a
  change that works. After editing, run the specific test for the code you
  touched, and fix it before you report.
- **Split large patches.** If a patch is long, make several smaller patches
  instead of one large one. A truncated patch leaves the file half-edited.
- **Read the file before you patch it.** The repository state you are
  patching against is the current one, not the one you remember.

<!-- zoder:prompt:end -->

---

## Evidence

| Claim | Kind | Source |
|-------|------|--------|
| The local coder route serves `qwen38` with `enable_thinking = false` and native tools | MEASURED | `~/.zoder/config.toml` `[providers.models.custom.coder]` |
| The `qwen38` runtime profile allows a 200 K prompt window | MEASURED | `~/.zoder/config.toml` `[runtime_profiles.zoder_coder]` (raised from 40 000 after the A/B replay of 2026-10-03) |
| qwen ids in the served corpus | MEASURED | `corpus/model_corpus.json` (`aiand/qwen/qwen3.8-27b`, `aihubmix/qwen3-coder-next`, …) |
| W1 (batched tool calls) | **UNVERIFIED** | observed family behaviour, not logged |
| W2 (stops at the first plausible edit) | **UNVERIFIED** | observed family behaviour, not logged |
| W3 (truncated long arguments) | **UNVERIFIED** | observed family behaviour, not logged |
