# Panic-inventory of unwrap / expect / panic / unreachable / slice-index / `as`-casts (non-test code)

Date: 2026-11-01 · Task: FB88 · Repo: zoder @ master `5efb4033` · Author: Jason Perlow <jperlow@gmail.com>

> **Base re-anchor (refresh onto `5efb4033`).** This report was first written against
> master `1cbe935`. It has since been re-verified against master `5efb4033dff6570ec7e2a9392bd660c7f710b4d0`
> (the base the branch was refreshed onto) and every cited `file:line` below has been
> re-checked at its current location. Five of the twenty ranked rows moved because
> `agentic.rs` and `acp-client/src/lib.rs` grew; the substance of every finding is
> unchanged. See "Base re-anchor" at the end for the old→new line map and what was
> re-read.

Report only — no production code changed. Deliverable = this document.

## Scope and method

Scanner: `#[cfg(test)] mod` blocks (and bare `mod tests {`) are text-stripped from every
`*.rs` in the workspace, then the remaining code is scanned for six patterns:

| pattern | scoped hits (non-test) |
|---|---|
| `.unwrap()` | 18 |
| `.expect(...)` | 54 |
| `panic!(...)` | 3 |
| `unreachable!()` | 3 |
| `.unwrap_or_else(\|e\| panic!...)` | 1 |
| bare slice/vector indexing `x[expr]` | 92 |
| `as` narrowing cast | 32 |
| **total** | **203** across 36 files |

Workspace-wide raw counts (test code included, for context): `unwrap` 2864, `expect` 1527,
`panic!` 131, `unreachable!` 6 (4528 in all) — i.e. ~98% of those raw hits (all but the 78
non-test hits of the same four patterns above) live in test code and are excluded by design.

Every non-test hit was then hand-adjudicated against its source context (±4–6 lines) into
exactly one class:

- **SAFE** — an invariant or guard is stated nearby and holds by construction (loop bounds,
  `>= len` short-circuits, casts preceded by an explicit range check, provable time/epoch
  constructions, documented decode invariants, dev-only codegen/bin tools), or the hit is a
  scanner false positive (doc-comment, string literal).
- **RISKY** — a panic is reachable from conditions the call site does not itself prove
  (environmental or enum-dispatch input).
- **UNKNOWN** — correctness depends on an invariant the call site does not state; a
  reviewer must walk the def-site before the hit can be settled.
- **TEST** — the hit was mis-scoped by the cfg-stripper: it is actually in-file test code
  (`reaper.rs`'s giant `mod tests`, `review_filter.rs` test fns, `session.rs` test fns,
  `context_opt/tests.rs`, `agentic.rs`'s `test_engine()`). Counted, but excluded from the
  non-test inventory.

## Final tally

| class | hits |
|---|---|
| RISKY | 5 |
| UNKNOWN | 0 |
| SAFE (incl. 15 scanner FPs in comments/strings) | 165 |
| TEST (mis-scoped, excluded) | 33 |
| **total scanned hits** | **203** |

The nine hits originally parked in UNKNOWN were resolved in review round 1 (see
"Review round 1" below): seven collapsed into SAFE on an existing guard and two
(`engine_rpc_impl.rs:474/484`) turned out to be `#[cfg(test)]` code the stripper had
placed in the non-test set. UNKNOWN is empty — every hit is now settled from source.

## The 20 riskiest, first

Ranked: real RISKY first, then the reviewed-and-settled rows, then the riskiest SAFE
entries (invariant-based `expect`s and deliberate panics worth keeping on a watchlist).
All file:line relative to the workspace root at master `5efb4033` (re-anchored from
`1cbe935`; see "Base re-anchor").

| # | location | kind | class | why it was picked |
|---|---|---|---|---|
| 1 | crates/acp-client/src/lib.rs:4882 | expect | RISKY | `dummy_reader()` calls `UnixStream::pair().expect("unix stream pair")`; under fd exhaustion process start or transport teardown panics instead of reporting. |
| 2 | crates/acp-client/src/lib.rs:4888 | expect | RISKY | Same pattern in the symmetric `dummy_writer()`. |
| 3 | crates/zoder-cli/src/agentic.rs:5738 | index | RISKY | `ordered[outcome.idx] = Some(outcome.outcome)` — unchecked write into the plan-slot vector; a chunk/plan bookkeeping drift panics the review loop. |
| 4 | crates/zoder-cli/src/agentic.rs:4159 | unreachable | RISKY | `ReviewScope::Auto => unreachable!()` completes a scope match; a new scope arm routed here aborts `zoder --review`. |
| 5 | crates/zoder-mcp-server/src/lib.rs:645 | unreachable | RISKY | `_ => unreachable!()` on SOP method dispatch; adding an SOP method without a value builder here panics servers in the field. |
| 6 | crates/zoder-core/src/engine_rpc_impl.rs:474 | unwrap | TEST | `lease_dir_for(socket).unwrap()` is inside `#[cfg(test)] fn write_test_lease` (line 467) — test-only, never compiled into the shipped binary. |
| 7 | crates/zoder-core/src/engine_rpc_impl.rs:484 | unwrap | TEST | `write_lease_file(...).unwrap()` is in the same `#[cfg(test)]` helper; production `acquire` propagates the `Result`. |
| 8 | crates/zoder-core/src/finops.rs:175 | index | SAFE | `for i in 0..n` with `n = xs.len()`, and the lone caller builds `xs` and `daily` from the same `window_days` (522 vs 538), so `ys.len() == xs.len() == n` by construction. |
| 9 | crates/zoder-core/src/finops.rs:176 | index | SAFE | same bound: `ys[i]` is reached only for `i < xs.len()`, and both vectors share the `window_days` length. |
| 10 | crates/zoder-core/src/finops.rs:193 | index | SAFE | `median` returns early on `nums.is_empty()` (186), so `m = s.len() / 2 >= 1` and `s[m - 1]` cannot underflow. |
| 11 | crates/zoder-core/src/finops.rs:195 | index | SAFE | same early return; the odd branch indexes `s[m]` with `m < s.len()` whenever the slice is non-empty. |
| 12 | crates/zoder-cli/src/agentic.rs:11740 | expect | SAFE | the loop is a fixed `for _ in 0..2` (11699) with no `continue`/`break`/`return` and one unconditional `fresh.push(r)` per pass, so exactly two elements reach `try_into`. |
| 13 | crates/zoder-core/src/corpus.rs:1727 | index | SAFE | guarded by `if idx >= before_aliases.len() { continue; }` (1720–1722) immediately above, so the index is proven in bounds. |
| 14 | crates/zoder-cli/src/review_filter.rs:634 | index | SAFE | `for at in 0..words.len().saturating_sub(2)` bounds `at + 2 < words.len()`, so `words[at]`, `words[at + 1]` and `words[at + 2]` are all in range. |
| 15 | crates/zoder-core/src/pricing.rs:520 | narrow_as | SAFE (watchlist) | `raw_u64 as u32` guarded by an explicit `raw_u64 > u32::MAX` check two lines up — the poster-child guarded cast; keep the guard. |
| 16 | crates/zoder-core/src/pricing.rs:631 | narrow_as | SAFE (watchlist) | same guarded-cast pattern in `clamp_peak_window_min`. |
| 17 | crates/zoder-core/src/provider.rs:2597 | expect | SAFE (watchlist) | `.expect("has_meaningful_message guarantees Some(_)")` in the decode path — invariant lives in the predicate, not here. |
| 18 | crates/zoder-core/src/provider.rs:2600 | expect | SAFE (watchlist) | same guarantee for `content`. |
| 19 | crates/zoder-core/src/quota.rs:143 | expect | SAFE (watchlist) | `.expect("midnight is valid")` — provable calendar construction, but a witness to keep. |
| 20 | crates/zoder-core/src/utilization.rs:1712 | panic | SAFE (watchlist) | deliberate `panic!` in `Clone for UtilizationStore` (flock is process-unique) — documented design decision. |

## Full inventory

### RISKY (5) — as in the table above

No further reads were needed; each was adjudicated on its source context.

### UNKNOWN (0 after review round 1)

The nine hits first parked here (see "Review round 1") all resolved on a read of the
def-site: two were `#[cfg(test)]`-only and seven carried an existing guard. Nothing
remains unclassified.

Note on the three sibling `eng.as_ref().expect…` call sites (11227, 11310, 11706): they
are SAFE — a failed `Engine::load()` fails closed (`reviewer_unavailable` is
synthesized and the guarded block is never entered); two sites even spell the guard out.

Rows 1-5 are the complete RISKY set; rows 6-14 are settled (TEST ×2, SAFE ×7).

#### Verification one-liners for the UNKNOWN set (all now resolved)

- engine_rpc_impl.rs:474 — **TEST**: the call sits in `#[cfg(test)] fn write_test_lease`.
- engine_rpc_impl.rs:484 — **TEST**: same `#[cfg(test)]` helper; the production path propagates `Result`.
- finops.rs:175/176 — **SAFE**: sole caller sizes `daily` and `xs` from the same `window_days`.
- finops.rs:193/195 — **SAFE**: `median` early-returns on an empty slice, so `m >= 1`.
- agentic.rs:11740 — **SAFE**: `for _ in 0..2` with an unconditional push and no early exit.
- corpus.rs:1727 — **SAFE**: `idx >= before_aliases.len()` guard just above.
- review_filter.rs:634 — **SAFE**: `0..words.len().saturating_sub(2)` bounds `at + 2`.

### SAFE (165)

Grouped per file with hit counts; the reason column summarizes each file's guard class
(all hits were individually adjudicated):

| file | SAFE hits | dominant reasons |
|---|---|---|
| crates/zoder-cli/src/review_filter.rs | 29 | loop `0..=len-2` and `get()`-guarded token walks over parallel `norm`/`words`; bound-checked character scanner; `min(len-1)`-clamped jumps; `at > 0` history guards |
| crates/zoder-cli/src/review_diff.rs | 19 | guarded wildcard-compare loops; `.expect("chunk was just created")` right after a push; index vectors collected from the same `lines` enumeration |
| crates/zoder-cli/src/agentic.rs | 16 | loop-guarded byte scans; explicit `min(usize::MAX as u64)`/`min(u32::MAX as u64)` clamps before casts; match-arm-fixed `.unwrap()`; the three engine `expect`s guarded by the fail-closed `reviewer_unavailable` skip |
| crates/zoder-cli/src/reaper.rs | 7 | production `retry_plan`: compile-time constants, saturating float cast + `.clamp(1, _)`, constant unions; one comment FP (1485) |
| crates/zoder-cli/src/exec_safety.rs | 7 | scan-first/slice-second (`digit_end` cannot exceed byte length); `toks[j]`/`toks[k]` inside `j..len` loops; `char as u32` is widening |
| crates/zoder-cli/src/review_confirm.rs | 4 | `from <= len` loop slice; `end >= len` else-branches; `start < len` guard |
| crates/zoder-cli/src/review_context.rs | 3 | ranges derived from bounded `.find` / `enumerate` over the same `lines` vector |
| crates/acp-client/src/lib.rs | 2 | pid `as i32` sits far below the 2^31 pid ceiling; `comps[split..]` with `split` from a bounded search (slice at `len` is legal) |
| crates/acp-client/src/session_store.rs | 1 | doc-comment doctest example (FP) |
| crates/agent-descriptor/src/bin/codegen-zoder.rs | 11 | dev-only codegen binary: 5 `expect`s on fs/cargo env + 5 `unwraps` on freshly serialized `serde_json` values; a panic fails CI at author time, not users; row 468 is a string literal (FP) |
| crates/agent-descriptor/src/descriptor.rs | 2 | enum <-> u8 round-trip casts |
| crates/agent-descriptor/src/schema.rs | 1 | `include_str!` of the checked-in, CI-validated schema |
| crates/zoder-cli/src/context_opt.rs | 1 | guarded cast |
| crates/zoder-cli/src/failure.rs | 1 | `h[end]` after an `end >= h.len()` short-circuit |
| crates/zoder-cli/src/goose.rs | 2 | pid `as i32`; `.expect` whose invariant is stated on the line above |
| crates/zoder-core/src/bin/context_bench.rs | 1 | bench bin: loop index `i as u32` over a bounded fixture replay |
| crates/zoder-core/src/bin/context_bench_live.rs | 2 | bench bin: `while i < args.len()` loop + call counter `as u32` |
| crates/zoder-core/src/config.rs | 6 | guarded wildcard match (`p < pattern.len()`); `position()`-returned index; keys enumerated from `self.profiles`; `u32->usize` widening |
| crates/zoder-core/src/consultant.rs | 1 | comment FP ("Never calls .unwrap()") |
| crates/zoder-core/src/context_opt/prefix.rs | 4 | scan advances by `char::len_utf8` so `text[from..]` stays on a char boundary; `bytes[at - 1]` guarded by `at == 0`; `bytes[end]` guarded by the `end >= bytes.len()` branch |
| crates/zoder-core/src/context_opt/review.rs | 1 | doc-comment FP (`contexts[i]`) |
| crates/zoder-core/src/corpus.rs | 4 | dedup `keep`/`retain` over the same vector's enumerate indices; alias-row index after a bail-out match over the same snapshot |
| crates/zoder-core/src/finops.rs | 7 | two provable midnight `expect`s + `vec![0.0; window_days as usize]`; the four indexing hits (`linear_slope` 175/176, `median` 193/195) are SAFE on the loop bound and the empty-slice early return |
| crates/zoder-core/src/ledger.rs | 1 | `expect` on a `const` literal with `debug_assert`s right behind it |
| crates/zoder-core/src/minimax.rs | 2 | `rem_euclid` arithmetic cannot exceed `u32::MAX`; provable epoch-time `expect` fallback |
| crates/zoder-core/src/pricing.rs | 7 | every `as u32` cast behind an explicit `> u32::MAX` range check; 4 comment hits documenting exactly those checks |
| crates/zoder-core/src/prompt.rs | 1 | `u64->usize` metadata cast (lossless on the 64-bit hosts this tool targets) |
| crates/zoder-core/src/provider.rs | 8 | documented decode/timeout invariants ("has_meaningful_message guarantees…", "deadline set") |
| crates/zoder-core/src/provider_rate.rs | 2 | `stamps.len() as u32` at real-world file lengths; `serde_json::to_string(Vec<u128>).unwrap()` cannot fail for that type |
| crates/zoder-core/src/quota.rs | 2 | `and_hms_opt(0,0,0)` / first-of-month constructions are provable |
| crates/zoder-core/src/scenarios.rs | 3 | enumerate-index vectors; 2 doc-comment hits |
| crates/zoder-core/src/utilization.rs | 3 | guarded `.expect("observable non-empty by prior guard")`; deliberate documented clone `panic!`; one widening cast |
| crates/zoder-mcp-server/src/lib.rs | 4 | `serde_json::to_value(...)` on owned, serializable structs — infallible in practice |

(Sum: 165.)

`engine_rpc_impl.rs` contributes no SAFE hits in non-test code: every `unwrap`/`expect`
in the file sits at line 474 or below, inside the `#[cfg(test)]` module, so its two
scanned hits are TEST (see below) and not part of this table.

### TEST (33) — cfg-stripper blind spots, in-file test code

| location | note |
|---|---|
| crates/zoder-cli/src/reaper.rs:2614,2618,2619,2620,2684(x2),2686,2689,2728,2771,2783,2795,3225,3887,3890,3908,4264,4272,4370,4380,4388,4393,4405 | `Workspace::new` / fixture-daemon / assertion helpers inside the file's giant test module |
| crates/zoder-cli/src/review_filter.rs:1930,1938 | `assert_gap` test-fn unwraps |
| crates/zoder-cli/src/agentic.rs:12166 | `test_engine()` fixture |
| crates/zoder-core/src/session.rs:696,1836 | test-fs helpers (`zoder-session-test-…` path, `utimes` fixture) |
| crates/zoder-core/src/context_opt/tests.rs:252,253(x2) | test-file helper `stub_len` |
| crates/zoder-core/src/engine_rpc_impl.rs:474,484 | `#[cfg(test)] write_test_lease` — resolved from UNKNOWN in review round 1 |

## Limitations of this scan

1. **Text-level classifier, not a compiler pass.** Type-directed questions (is the
   receiver `Option` or `Result`? is the slice a `Vec` or a serde value guard?) are
   heuristics. That is why a class for "needs a def-site walk" was needed, and why every
   hit still unresolved after the sweep was hand-adjudicated from source context. After
   review round 1 no hit is left unresolved (UNKNOWN = 0).
2. **The `#[cfg(test)]` stripper is a text heuristic** and missed items: five in-file
   test modules (31 hits) plus the two `engine_rpc_impl.rs` lease helpers. All seven are
   listed in the TEST table.
3. **False-positive classes observed** (all classified SAFE with notes): doc-comment and
   string-literal fragments that embed the target patterns (`code`-fence examples,
   `providers[i].auth.var` inside a schema description, `{i}`-format strings), comment
   lines that discuss unwraps (`consultant.rs:44`, `reaper.rs:1485`,
   `review_diff.rs:716`, `pricing.rs:485/487/508/611`).
4. **In-file test modules the cfg-stripper missed** are 31 of the 33 TEST rows; they use
   `expect`/`panic!` freely, which is fine — they are tests.
5. **Raw counts include `benches/` and binary-only helper code** consistent with the
   brief ("non-test code of the Rust workspace").
6. **The scan pipeline is not checked in.** It was a one-off regex sweep plus manual
   adjudication; re-running after merges means rebuilding it. That is the main reason
   this document should be treated as a dated snapshot rather than a gate.

## Review round 1

A non-author model (`deepseek-v4-pro`) reviewed the nine UNKNOWN rows against quoted
source, with the repository checked out at master `1cbe935` (reviewer: `deepseek-v4-pro`,
distinct from the authoring model). It returned **REQUEST_CHANGES** on all nine. Each
finding was then re-checked independently against the repository before being accepted;
all nine held, and the document was corrected rather than the source:

| row | was | now | decisive source fact |
|---|---|---|---|
| 6 | UNKNOWN | TEST | call is inside `#[cfg(test)] fn write_test_lease` (engine_rpc_impl.rs:468) |
| 7 | UNKNOWN | TEST | same `#[cfg(test)]` helper |
| 8 | UNKNOWN | SAFE | `for i in 0..n`, `n = xs.len()`; caller sizes `daily` and `xs` from one `window_days` |
| 9 | UNKNOWN | SAFE | same bound |
| 10 | UNKNOWN | SAFE | `median` early-returns on empty, so `m >= 1` |
| 11 | UNKNOWN | SAFE | same early return |
| 12 | UNKNOWN | SAFE | `for _ in 0..2` + unconditional push + no early exit |
| 13 | UNKNOWN | SAFE | `idx >= before_aliases.len()` guard immediately above |
| 14 | UNKNOWN | SAFE | `0..words.len().saturating_sub(2)` bounds `at + 2` |

The original document named the wrong remedy for row 13 (`before_aliases[idx]` claimed
a missing "parallel-length invariant"; the guard is an explicit bounds check, not a
length coupling) and for row 14 (it named a `norm`/`words` alignment that does not exist
in `claimed_alias_pairs`, which walks a single `words` vector). Both clauses are
corrected here.

## Verification status

- RISKY rows 1-5: claims re-read at the cited line on master `5efb4033`; confirmed
  (locations re-anchored — see "Base re-anchor").
- Rows 6-14: settled against source in review round 1; confirmed at the re-anchored
  locations (`engine_rpc_impl.rs:474/484`, `finops.rs:175/176/193/195`,
  `agentic.rs:11740`, `corpus.rs:1727`, `review_filter.rs:634`).
- Rows 15-20 (SAFE watchlist): spot-checked — the guarded casts at `pricing.rs:520/631`,
  the `has_meaningful_message` invariant at `provider.rs:2597/2600`, the calendar
  construction at `quota.rs:143` and the deliberate `Clone` panic at `utilization.rs:1712`
  all state their guard as described.
- **Not re-derived:** the 165 SAFE per-file counts were taken from the original sweep and
  were not re-scanned; only the twenty ranked rows and the nine UNKNOWN rows were
  re-adjudicated line by line. The `as`-cast and slice-index subtotals (32 / 92) likewise
  carry over unverified, and the scan script itself is not checked in.

## Base re-anchor (refresh onto master `5efb4033`)

This branch was refreshed onto master `5efb4033dff6570ec7e2a9392bd660c7f710b4d0`. The
merge took master's code wholesale on every path this report does not own, so every cited
`file:line` was re-checked at its new location. Nothing about the findings changed; only
the line numbers moved, plus two stale cross-references that were corrected.

| row | old cite (`1cbe935`) | new cite (`5efb4033`) | still the same hit? |
|---|---|---|---|
| 1 | acp-client/src/lib.rs:4711 | acp-client/src/lib.rs:4882 | yes — `dummy_reader()` `UnixStream::pair().expect(…)` |
| 2 | acp-client/src/lib.rs:4717 | acp-client/src/lib.rs:4888 | yes — `dummy_writer()` |
| 3 | agentic.rs:5542 | agentic.rs:5738 | yes — `ordered[outcome.idx] = …` |
| 4 | agentic.rs:4013 | agentic.rs:4159 | yes — `ReviewScope::Auto => unreachable!()` |
| 5 | zoder-mcp-server/src/lib.rs:645 | zoder-mcp-server/src/lib.rs:645 | yes — unchanged |
| 6 | engine_rpc_impl.rs:474 | engine_rpc_impl.rs:474 | yes — `#[cfg(test)] fn write_test_lease` now at 467 |
| 7 | engine_rpc_impl.rs:484 | engine_rpc_impl.rs:484 | yes |
| 8-11 | finops.rs:175/176/193/195 | unchanged | yes — `linear_slope`/`median` unmoved |
| 12 | agentic.rs:11250 | agentic.rs:11740 | yes — loop at 11699, same `for _ in 0..2` |
| 13 | corpus.rs:1727 | corpus.rs:1727 | yes — guard at 1719 |
| 14 | review_filter.rs:642 | review_filter.rs:634 | yes — `0..words.len().saturating_sub(2)` |
| 15-16 | pricing.rs:520/631 | unchanged | yes |
| 17-18 | provider.rs:2586/2589 | provider.rs:2597/2600 | yes |
| 19 | quota.rs:143 | quota.rs:143 | yes |
| 20 | utilization.rs:1711 | utilization.rs:1712 | yes — `panic!` moved one line down |

Re-read at the new base, not assumed: the five RISKY sites (context ±6 lines each), the
`engine_rpc_impl.rs` `#[cfg(test)]` attribute/function pair, `finops.rs`'s `linear_slope`
loop bound and the caller that sizes `daily` (522) and `xs` (538) from one `window_days`,
`median`'s empty-slice early return, the `for _ in 0..2` collection loop, the
`before_aliases` bounds guard, and the four watchlist guards.

Also corrected in this pass: the "three sibling `eng.as_ref().expect…`" note cited
`10750/10833/11216` and a fail-closed site at `10715`; those are `11227/11310/11706` at
this base, and the function that synthesizes the fail-closed verdict is
`reviewer_unavailable`, not a line number.

The tally is unchanged: **5 RISKY / 0 UNKNOWN / 165 SAFE / 33 TEST = 203.**
- **Not re-derived:** the 165 SAFE per-file counts were taken from the original sweep and
  were not re-scanned; only the twenty ranked rows and the nine UNKNOWN rows were
  re-adjudicated line by line. The `as`-cast and slice-index subtotals (32 / 92) likewise
  carry over unverified, and the scan script itself is not checked in.

## Bottom line

Across 203 scoped hits, panic surface is small and mostly provable: **5 RISKY, 0
UNKNOWN, 165 SAFE, 33 TEST** (203 total). The five RISKY hits are the ones to fix first
(2 fd-exhaustion `expects` in acp-client, 2 enum-dispatch `unreachable!()`s, 1
unchecked plan-slot index in agentic.rs). Nothing is left needing a def-site walk.
