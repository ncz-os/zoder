# Loop resolution semantics (2026-10-03)

This note records the reviewer-verdict rules `zoder loop` enforces after the
`wip/studio/2026-10-03-loop-verdict` change. Everything here is **fail-closed**:
an unparseable, unlocated, or unavailable verdict must never become an approval.

## A. Prose is not a verdict

The loop's reviewer path accepts only a structured JSON verdict object
(`parse_review_json_only`). It never uses the keyword-recovery prose fallback
that the standalone `parse_review` still offers to non-loop callers.

When the reviewer answers in prose:

1. the loop re-asks that reviewer ONCE through the direct (non-agentic) reviewer
   path with the neutral `REVIEW_SYSTEM` prompt and a "return only JSON"
   directive. If the engine profile sets `provider_extra.response_format`,
   `dispatch_reviewer_for_model` forwards it on the request body (the same way
   `provider_extra.reasoning_effort` is already forwarded); the field is absent
   when unset;
2. if that retry is also not JSON, the loop advances to the next reviewer in the
   configured pool;
3. if no reviewer returns JSON, the iteration verdict is `reviewer_unavailable`
   and the loop stays unresolved with that reason. The prose is preserved as a
   `Finding` body for humans.

`reviewer_unavailable` is not a recognized verdict, so `loop_review_ok` fails
closed on it.

## B. `request_changes` must be located

A `request_changes` whose findings array is empty, or whose findings carry no
`location` (`path:line`), is an invalid review rather than a blocker. The loop
re-asks ONCE with a note that a located defect is required. If the reviewer
repeats the unlocated block, the iteration is downgraded to
`comment_no_evidence`: it can never resolve (unknown verdict ⇒ fail closed) and
it does **not** count toward the no-new-progress stall counter. Explicit blocks
with located findings keep the existing semantics.

## C. Unchanged diff + same blockers ⇒ three fresh-sample majority

When the author produced no new diff since the previous review, the reviewer is
still blocking, and the loop has already stalled at least once, the loop stops
re-prompting the author for the same findings a third time. It takes **two
additional, independent fresh reviews of the same unchanged diff** and decides
by a strict majority of the three samples `[current, fresh_1, fresh_2]`. All
three verdicts are recorded in the iteration record under `majority_samples`,
and the cost of both fresh reviews is added to the run total.

Because `current` is by construction a non-approve, resolution requires **both
fresh samples to approve**: a single noisy block is overruled only when two
independent fresh reviews both approve. The previous iteration's verdict is not
reused as one of the three samples.

A located blocking finding (`critical`/`high` with a concrete location) in ANY
of the three samples vetoes approval — with an unchanged diff it is by
definition unaddressed. An unparseable, unavailable, or synthesized-failure
sample is not an approval, so it counts as a non-approve and fails closed. The
majority approval also still requires a satisfied check and a substantive diff,
so the objective gate is not bypassed.

The pure decision is `majority_resolves_on_unchanged_diff` (which delegates the
vote to `majority_loop_approval`); the loop only calls it after one stalled
iteration on an identical diff.

## D. Ledger handoff

`mnemos::checkpoint` evidence now carries a bounded (≤1500 chars) sanitized
list of the iteration's located findings — severity, title, and location only,
no code bodies — plus the reviewer model. A resumed job can start from the last
findings.

## E. Review base and completeness (2026-10-21)

The reviewer must see the loop's OWN work and ALL of it.

* **Base.** Branch scope diffs from the HEAD sha captured when the loop started
  (`loop_review_base`), two-dot against the working tree so uncommitted author
  edits are included. An explicit `--base` still wins. The old default was
  `detect_base` (upstream/default-branch merge-base), which on 2026-10-21
  reported 5994 / 28388 diff lines for changes of ~880 / ~430.
* **Dirty start.** A commit base is not a working-tree base. `zoder loop -C
  <tree>` is routinely pointed at a checkout that already carries uncommitted
  and untracked work, and `git diff <start_head>` charges every byte of it to
  the author. The loop now snapshots the working tree BEFORE the first author
  turn (`loop_start_state`) into a real git tree object through a throwaway
  `GIT_INDEX_FILE` (`git add -A` into the temporary index, `git write-tree`) --
  tracked and non-ignored untracked files, honouring `.gitignore`, without
  touching the worktree, the real index, or any ref. The reviewed delta is
  `git diff <start tree> <current tree>`: loop work of every kind (committed,
  uncommitted, new, deleted) is in scope and pre-existing state is not. The
  snapshot tree id and `start_dirty` are recorded per iteration
  (`review_base_tree`, `start_dirty`), a dirty start prints a one-line warning
  even under `--quiet`, and a snapshot that cannot be taken makes branch scope
  refuse the review rather than fall back to a base that misattributes. An
  explicit `--base` keeps its own three-dot contract.
* **Chunking.** The loop review now uses the same per-route chunk planner as
  `zoder review` (`plan_loop_review`). Every changed line is in exactly one
  chunk; a chunk over `max_hunk_bytes` is split. Nothing is truncated.
* **Fail closed.** A diff over `max_diff_bytes`, an exclusion that removes
  every changed file, a provider error, a chunk with no structured JSON after
  one repair, a mixed-model chunk sequence, or a review-phase timeout makes
  `run_loop_review` return an error. The iteration is recorded
  `reviewer_unavailable` with `review_error` set and stops unresolved. An
  unfinished review is never an approval, even when every finding heuristic
  would return zero blockers.
* **Evidence.** Each iteration records `review_base`, `review_base_tree`,
  `start_dirty`, `review_chunks`, and `review_error`; the human summary prints
  chunks and the error.

## F. Review completeness and finding identity (2026-10-23)

The per-iteration record in the loop's JSON (`result.json` / stdout, under
`log[]`) now says whether the review actually finished, and gives every
finding a stable identity and an explicit disposition. All three are
**additive**: every pre-existing field keeps its name and meaning, and an
old-schema record still parses with the new fields absent.

* **`review_complete` (bool, per iteration).** `true` ONLY when every
  requested reviewer returned a structured JSON verdict for every chunk of
  the diff. `false` for a PARTIAL review (a chunk that never returned a
  structured verdict, even after its repair retry), a reviewer timeout or
  provider failure (the `Err` path, `reviewer_unavailable`), and a mid-review
  fallback to a different model — zoder refuses mixed-model approval, and the
  record surfaces that refusal as incomplete, never as complete. When the
  review errored, `reviewer_verdicts` is absent (empty) and `review_complete`
  is `false`. Downstream triage (Hive's shadow-mode loop triage) keys on this
  field to decide whether a non-converging loop can be judged.
* **`reviewer_verdicts` (array, per iteration).** One entry per requested
  reviewer: `{model, verdict, complete}`. `model` is the model that served the
  review, `verdict` is the aggregated review verdict, and `complete` is the
  per-reviewer structured-verdict evidence (every chunk structured, same
  serving model). The iteration's `review_complete` is `true` iff the array is
  non-empty and every entry is `complete: true`.
* **`findings` (array, per iteration and at the top level for the last
  iteration).** Each finding carries its existing fields (`severity`,
  `title`, `body`, `location`) plus two new ones:
  * `fingerprint` — a stable string: the finding's normalized file path (the
    path part of its `location`; a pathless finding uses no path) plus its
    normalized title (lowercased, runs of whitespace/hyphen/punctuation
    collapsed to a single space). The same defect re-raised in the next round
    — after line shifts, case changes, or re-hyphenation — maps to the same
    fingerprint; the same title in a different file does not.
  * `disposition` — one of `open | real | disproved | intended`, default
    `open`. Only an EXPLICIT value recorded by an author or reviewer in the
    review payload sets a non-`open` disposition; an unknown or missing value
    stays `open`. zoder never infers a disposition from severity, verdict, or
    prose.

The human (non-`--json`) summary now prints the per-iteration finding count
and a `review=complete|INCOMPLETE|unknown` marker alongside the existing
fields.
