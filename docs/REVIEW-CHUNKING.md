# Review chunk sizing per reviewer

`zoder review` splits a diff into chunks no larger than `max_hunk_bytes` and
sends each chunk to the reviewer. The historical 9000-byte cap is a default,
not a model limit: some reviewers are more accurate with fewer, larger chunks
(fewer chunks means fewer cross-chunk false positives), others lose recall or
truncate. Sizing is therefore resolved **per reviewer**.

## Precedence

For each reviewer slot (the default reviewer and every `--panel` member):

1. CLI flag (`--max-hunk-bytes`, `--max-diff-bytes`)
2. `[review.route_defaults.<model>]`
3. `[review]` (`max_hunk_bytes`, `max_diff_bytes`)
4. built-in default: 9000 per chunk, 120000 total, chunk count derived

`<model>` matches the exact served id first, then the part after the last
`/` (`nemotron-3-ultra-550b-a55b` matches `nvidia/nemotron-3-ultra-550b-a55b`).
Agent aliases (`-m reviewer`) resolve to their configured model first. A
reviewer with no entry keeps 9000.

Per-route keys:

| Key | Meaning |
|---|---|
| `max_hunk_bytes` | chunk byte cap for this reviewer |
| `max_diff_bytes` | total diff byte cap for this reviewer |
| `max_chunks` | chunk-count bound (default derived from the two caps) |
| `max_tokens` | output-token floor for this reviewer's verdict calls (raises, never lowers, the 8192 review floor) |

A panel can chunk differently per member. If the default reviewer falls back
to another model on the first chunk, the review re-plans its chunks for the
model that is actually serving (once); a fallback after the first chunk is
refused as a mixed-model review, as before.

`zoder review --dry-run` prints the plan per reviewer:

```
[zoder] chunk plan per reviewer:
[zoder]   gemma4-31b: 4 chunk(s) [8850, 8361, 8799, 2325] max-hunk-bytes=9000 ... (general default)
[zoder]   qwen38: 1 chunk(s) [27626] max-hunk-bytes=64000 ... (route default)
```

Consecutive hunks of one file in one chunk share a single file header, so a
fragment-heavy diff is no longer inflated by a repeated header per hunk.

## Where to put it: `~/.zoder/review.toml`

```toml
[review]
max_diff_bytes = 400000

[review.route_defaults.qwen38]
max_hunk_bytes = 64000
```

Use `review.toml`, not the vendor overlay. A `[review]` table inside
`config.<vendor>.toml` is read by current builds, but builds that predate it
**ignore the whole overlay file** (every provider in it) when they meet the
unknown table, while `config --validate` still says VALID. `review.toml` is
never opened by older builds, so it can be deployed independently of the
binary. It is merged after the overlays and wins over an overlay `[review]`.
A malformed `review.toml` is ignored with a warning.

The loop's review phase sends one prompt and is not chunked; these settings
apply to `zoder review` and `zoder adversarial-review`.

## Measured recommendations (fleet routes, 2026-10-10)

From the chunk-size study (seeded 96,835-byte diff with 8 known defects;
recall / false positives per cap):

| Reviewer | Recommended | Evidence |
|---|---|---|
| qwen38 (TYDEUS) | `max_hunk_bytes = 64000` | 0 false positives at every cap 9k-131k; 7/8 recall |
| deepseek-flash | `max_hunk_bytes = 48000` | 8/8 at 32k and 96k, 7/8 at 48-64k; FP 6 -> 0-2 as cap rises; half the wall time of 9k |
| nvidia/nemotron-3-ultra-550b-a55b (EIH) | `max_hunk_bytes = 48000`, `max_tokens = 16384` | FP 12 (9k) -> 2 (48k); reasoning-heavy |
| MiniMax-M3 | `max_hunk_bytes = 48000`, `max_tokens = 32768` | FP 21 (9k) -> 4 (48k); truncates below 32768 on real diffs |
| MiniMax-M3.1-Flash-Preview | `max_hunk_bytes = 64000` | 7/8, 0 FP |
| gemma4-31b (CERBERUS) | leave at 9000 | 8/8 and FP-free at 9000; 16k lost a defect |
