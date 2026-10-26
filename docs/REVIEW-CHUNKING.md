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

## Single-review budget from the context window

When the reviewer route has `max_context_tokens`, zoder first tries the whole
diff as one review. Its byte budget is the configured token window minus the
review's output-token allowance and a fixed 2,048-token margin for the system
prompt and message framing, converted at four bytes per estimated token (the
same conservative estimate used by the context-window check). The budget is
also limited by `max_diff_bytes`. Definition context is included when deciding
whether the whole review fits. If it does not, zoder returns to the configured
`max_hunk_bytes` chunk cap. With no context window configured, the single-review
budget remains `max_hunk_bytes`, preserving the default behavior.

Set `max_context_tokens` on the reviewer provider or, more specifically, the
reviewer model entry to enable whole-diff sizing:

```toml
[[providers]]
id = "provider-a"
serves = ["provider-a/"]
max_context_tokens = 131072

[providers.models.custom.model-x]
model = "provider-a/model-x"
max_context_tokens = 131072
```

The model entry takes precedence over the provider value. The emitted chunking
metadata reports `window_tokens`, `budget_bytes`, and `chunk_cap_bytes` so the
chosen mode can be inspected in review output.

`zoder review --dry-run` prints the plan per reviewer:

```
[zoder] chunk plan per reviewer:
[zoder]   gemma4-31b: 4 chunk(s) [8850, 8361, 8799, 2325] max-hunk-bytes=9000 ... (general default)
[zoder]   qwen38: 1 chunk(s) [27626] max-hunk-bytes=64000 ... (route default)
```

Consecutive hunks of one file in one chunk share a single file header, so a
fragment-heavy diff is no longer inflated by a repeated header per hunk.

## Definition context and false undefined findings

When chunk context is enabled (the default; set `ZODER_REVIEW_CONTEXT=0` to
disable it), each chunk gets a bounded post-change context tail. For each
changed file this includes imports, top-level definitions/constants, the
enclosing definition for each changed hunk, and defining lines for referenced
symbols that are outside the chunk. Files below the small-file threshold are
shown whole. The context is appended after the cache-stable review head and is
charged against the route's `max_hunk_bytes` budget; it is never placed in the
shared head.

If a structured finding says a symbol is undefined, missing, or not imported,
the deterministic post-filter checks the reviewed file and the sibling modules
named by its imports. A disproved finding is removed from `findings` and
recorded in `filtered_findings` with its symbol, reason, and evidence line. A
lone blocking finding of this kind becomes `comment`, not `approve`, because
disproving a finding is not proof that the change is correct. Findings without
source evidence remain unchanged. Missing or rejected CLI options are disproved only
by exact positional string literals in `add_argument`, `add_option`, or
`click.option` calls in the same file. Help text, comments, and longer option
names are not evidence, and every claimed symbol or option must be disproved.

The regression fixtures cover both sides: a constant defined in an earlier
chunk is supplied to the later chunk and is filtered if falsely reported, while
a genuinely absent name remains a finding. They also cover definitions in an
imported sibling module.

## Context cost measurement

The following deterministic sizing measurements use three multi-file commits
from this repository's history, the built-in 9,000-byte route cap, and an
approximately four-characters-per-token estimate for input tokens. “Before”
counts a 9,000-byte diff slice; “after” adds the maximum reserved context tail
and wrapper. They are an upper-bound budget measurement (the actual context
builder often emits less), not provider billing telemetry.

| History diff | Chunks | Before input tokens/chunk | After input tokens/chunk | Change |
|---|---:|---:|---:|---:|
| `7b4d4ea` (11 files, 57,102 diff bytes) | 7 | 776–2,250 | 1,050–2,893 | up to +29% |
| `f965acb` (3 files, 21,544 diff bytes) | 3 | 886–2,250 | 1,188–2,893 | up to +34% |
| `4fea8b7` (5 files, 11,363 diff bytes) | 2 | 591–2,250 | 819–2,893 | up to +39% |

The per-chunk route cap remains the hard ceiling: the implementation reserves
at most a quarter for definition context (plus the fixed wrapper allowance),
reducing the diff portion when necessary.

## Default reviewer benchmark

`zoder review` over two synthetic fixtures, reviewed by gpt-oss-20b (local,
reasoning effort low), 2026-10-26. The seeded fixture is a ~97 KB multi-file
diff (Python, C, Rust) with 8 planted defects: SQL injection, use-after-free,
off-by-one, fail-open auth, an undefined Rust function, a sign error, a
check-then-act race and an unchecked return. The clean fixture is a ~24 KB
correct diff whose names are defined in other chunks. A defect counts as found
when a finding names its file and a defect keyword. FP = findings that match
no planted defect; "false undefined" = FPs that call a name undefined.
`--max-diff-bytes 120000`; one run per cell, so a difference of one or two
defects is within run-to-run noise.

| fixture | chunk cap | definition context | chunks | recall | FP | false undefined |
|---|---:|---|---:|---:|---:|---:|
| seeded | 9,000 | off | 12 | 7/8 | 0 | 0 |
| seeded | 9,000 | on | 16 | 8/8 | 0 | 0 |
| seeded | 16,000 | off | 7 | 7/8 | 0 | 0 |
| seeded | 16,000 | on | 8 | 6/8 | 0 | 0 |
| seeded | 32,000 | off | 4 | 6/8 | 0 | 0 |
| seeded | 32,000 | on | 4 | 6/8 | 0 | 0 |
| seeded | single review (131,072-token window) | on | 1 | 6/8 | 0 | 0 |
| clean | 9,000 | off | 4 | n/a | 1 | 0 |
| clean | 9,000 | on | 5 | n/a | 4 | 1 |
| clean | single review (131,072-token window) | on | 1 | n/a | 1 | 1 |

What the numbers say:

- Recall is 6-8 of 8 at every setting; no chunk size or context setting is
  clearly better on this fixture at one run per cell.
- A single review (the route window set, so the whole diff went in one call)
  found as much as the chunked runs in one call instead of 4-16.
- The surviving "false undefined" findings were kept on purpose: in one, the
  sentence making the claim names no symbol; in the other the named constant
  really is absent and the claim is about intent. The filter drops a finding
  only when every name it claims is undefined is shown to be defined.
- The first version of the post-filter treated every identifier in a finding
  as a claimed name and dropped the finding when any one was defined. On this
  fixture it removed the real undefined-function defect in most runs (the
  path stem `main` is defined) and some use-after-free, off-by-one and race
  findings ("undefined behaviour", "does not exist"). The regression tests in
  `review_filter.rs` reproduce those findings.

Recommendation: keep 32,000 bytes as the chunk cap for this reviewer (same
recall within noise, fewest calls), and set `max_context_tokens` on its
provider entry so that a diff that fits its window is reviewed whole.

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
unknown table. Current builds record that skip, so `config --validate` reports
the ignored file instead of saying VALID. `review.toml` is never opened by
older builds, so it can be deployed independently of the binary. It is merged
after the overlays and wins over an overlay `[review]`. A malformed
`review.toml` is ignored with a warning at load, and `config --validate`
reports it as a problem.

`zoder loop` uses the same planner for its review phase (per-route caps are
resolved for the reviewer that will serve the head slot), so a loop review is
chunked too. Unlike the standalone command, the loop has no
`--max-hunk-bytes` / `--max-diff-bytes` flags yet: it reads `[review]` and
`[review.route_defaults.<model>]` from the config. An over-cap loop diff is an
error — the iteration is recorded `reviewer_unavailable` and stays unresolved —
never a truncated prompt that a reviewer can approve.

## Measured recommendations (reference routes, 2026-10-10)

From the chunk-size study (seeded 96,835-byte diff with 8 known defects;
recall / false positives per cap):

| Reviewer | Recommended | Evidence |
|---|---|---|
| qwen38 (provider-a) | `max_hunk_bytes = 64000` | 0 false positives at every cap 9k-131k; 7/8 recall |
| deepseek-flash | `max_hunk_bytes = 48000` | 8/8 at 32k and 96k, 7/8 at 48-64k; FP 6 -> 0-2 as cap rises; half the wall time of 9k |
| nvidia/nemotron-3-ultra-550b-a55b (hosted provider) | `max_hunk_bytes = 48000`, `max_tokens = 16384` | FP 12 (9k) -> 2 (48k); reasoning-heavy |
| MiniMax-M3 | `max_hunk_bytes = 48000`, `max_tokens = 32768` | FP 21 (9k) -> 4 (48k); truncates below 32768 on real diffs |
| MiniMax-M3.1-Flash-Preview | `max_hunk_bytes = 64000` | 7/8, 0 FP |
| gemma4-31b (provider-a) | leave at 9000 | 8/8 and FP-free at 9000; 16k lost a defect |
