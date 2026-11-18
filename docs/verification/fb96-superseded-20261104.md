# FB96 collector review — branch is superseded, nothing to land (2026-11-04)

Scope: owner review of `freebuff/FB96-20261102` (tip `deee2af5e12c`) against
`master` (`608332f4`). Verdict: **superseded — do not land**. This report is
the only artifact; no source, test or doc file was modified. The branch itself
is left untouched because its content is already on `master`.

## What the branch carries

Three finding commits, all confined to `crates/zoder-cli/src/agentic.rs`
(190 lines changed, `2ac9d7e6^..deee2af5`):

| commit | finding |
|---|---|
| `2ac9d7e6` | F2.2 — never judge a truncated diff on its visible prefix |
| `a3c4fa65` | F2.4 — keep `retry_after` / `token_plan_resets_at` on reviewer fallback |
| `deee2af5` | F2.5 — read the skip reason from the diagnostic head only |

`56088d6b` ("build: repair the master merge break in `turn_failure_error`") sits
below them on the same branch. It is **not** part of the fix set and is not
claimable here: `master` already carries an equivalent repair (`c22acd68` and
`44420226`, same subject), and `56088d6b` is itself a cherry-pick of
`e1552957` whose parent (`788d35ea`) is not on `master`.

## Why it is superseded (evidence)

`master` already contains each of the three findings as a **cherry-pick of the
branch's own commit SHA** — the trailer names the exact source commit:

| master commit | cherry-picked from |
|---|---|
| `b21531d0` (F2.2) | `2ac9d7e68f2c` (+ `c7fceb54`, FB95's twin) |
| `9d0fe573` (F2.4) | `a3c4fa65f31c` (+ `b141885d`, FB95's twin) |
| `dc1ba563` (F2.5) | `deee2af5e12c` (+ `3d2bdeee`, FB95's twin) |

`git merge-base --is-ancestor` confirms all three are ancestors of
`origin/master`. The re-application was deliberate and is documented on
`master` itself: FB96's 56088d6 build-repair commit was already on master and was not cherry-picked.

Content check, not just commit identity:

- All five FB96 regression tests exist on `master` and their function bodies
  are whitespace-normalised **identical** to the branch's:
  `classify_diff_substance_truncated_diff_is_not_judged_on_prefix`,
  `fallback_worthy_keeps_reset_hint`,
  `fallback_reset_wall_uses_retry_after_for_non_minimax_providers`,
  `fallback_reset_wall_prefers_plan_reset_and_keeps_no_wall_without_a_hint`,
  `skip_reason_does_not_match_timeout_inside_error_body`. All five pass there.
- `reviewer_skip_reason` and `fallback_reset_wall_from` are function-body
  identical; `enum ReviewerError` is identical; the `complete_once_tracked`
  dispatch region (`agentic.rs:940-1060` on the branch) is identical,
  including the `FallbackWorthy { …, retry_after, token_plan_resets_at }` arm
  and the three `retry_after: None` constructor sites.
- `classify_diff_substance` opens on both sides with the same guard and the
  same comment block:
  `if diff.contains(DIFF_ACQUISITION_TRUNCATION_MARKER) { return DiffSubstance::Substantive; }`
  (`DIFF_ACQUISITION_TRUNCATION_MARKER` has the same value on both sides).

The one behavioural divergence is **additive and non-regressing**: `master`
(through FB95, finding F2.1) routes `CommentOnly`/`TestOnly` detection through
`is_comment_line()` instead of the branch's inline
`COMMENT_MARKERS.iter().any(…)`. The truncation guard is the **first**
statement of `classify_diff_substance` on `master`, so it short-circuits before
comment detection runs. Verified empirically (throwaway probe, since removed):
a pure comment-only payload with the truncation marker appended classifies
`Substantive`, and the same payload without the marker classifies
`CommentOnly` — so the result is caused by the guard, not by the payload.

## Why a rebase cannot land it

The branch is not merely "behind" — it is a **divergent lineage**:
1041 commits on the branch are not on `master` and 1097 on `master` are not on
the branch. A fast-forward is impossible, and a plain
`git rebase origin/master` replays 1000+ unrelated historical commits and
immediately hits add/add conflicts on `docs/DAILY-BUILDS.md` and
`scripts/daily-build.sh` — neither of which FB96 touches. Forcing that replay
would rewrite the branch's history with unrelated content and risk silently
dropping other landed work, to produce zero net change against `master`.
Merging `master` in (the other permitted conflict resolution) has the same
outcome: the three finding commits remain in history but `git diff
master...branch` for `agentic.rs` reduces to master's own newer state, so
landing it is a no-op at best and a duplicate-history risk at worst.

## Test counts

| tree | tests | passed | skipped | failed |
|---|---|---|---|---|
| branch tip `deee2af5` | 2448 | 2446 | 2 | 0 |
| `master` `608332f4` | 2548 | 2546 | 2 | 0 |

`cargo nextest run --workspace --no-fail-fast` on both. Failures do not grow
in either direction; the 100-test delta is master-only work unrelated to this
branch.

## Structured review

Non-author model: **`claude-sonnet-4-5-20250929`** (returned in the structured
`reviewers[].model` field). Verdict: **APPROVE**. Two low-severity notes, both
addressed here:

- *"evidence was not re-executed by the reviewer"* — every claim above was
  re-run by the author against live git objects (`merge-base --is-ancestor`,
  cherry-pick trailers, function-body diffs, nextest). The reviewer saw the
  evidence payload, not a shell.
- *"the truncation × comment-detection interaction has no dedicated test"* —
  correct: no checked-in test pins the guard's precedence over F2.1's
  `is_comment_line`. It is proven above by the guard's position (first
  statement) and by a throwaway probe. No test was added: the interaction is
  already un-shadowable by construction, and this branch's job is to land (or
  decline) FB96, not to extend FB95's tests.

## Unverified

- `freebuff/FB96-20261102`'s entry in `FREEBUFF-BATCH10-SUMMARY.md` could not
  be read: no `FREEBUFF-BATCH*` summary file exists on the branch, on `master`,
  or anywhere in the repository history (`git ls-tree -r origin/master`
  matches nothing; `git log --all --diff-filter=A` matches nothing). The
  branch's task/tests/claims were taken from the three commit messages and
  their diffs instead, which is the only in-repo record.
- Whether the Freebuff seat intended `56088d6b` as part of the deliverable is
  not recorded in the branch; it is treated as out of scope for the reasons
  above.

## Conclusion

FB96's findings F2.2, F2.4 and F2.5 are fully present on `master` as
cherry-picks of this branch's own commits, with identical production code and
identical regression tests, on a tree with more tests and no failures. There is
no branch-only hunk worth landing and no defect on `master` that this branch
would fix. The branch was **not** modified or rebased, and nothing was
re-landed.
