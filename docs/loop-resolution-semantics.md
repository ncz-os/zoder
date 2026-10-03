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

## C. Unchanged diff + same blockers ⇒ three-sample majority

When the author produced no new diff since the previous review and the reviewer
is still blocking, the loop stops re-prompting the author for the same findings
a third time. It re-runs the reviewer ONCE more and decides by strict majority
of the three samples (approve vs not). All three verdicts are recorded in the
iteration record under `majority_samples`.

A located blocking finding (`critical`/`high` with a concrete location) in ANY
sample vetoes approval — with an unchanged diff it is by definition
unaddressed. The majority approval also still requires a satisfied check and a
substantive diff, so the objective gate is not bypassed.

## D. Ledger handoff

`mnemos::checkpoint` evidence now carries a bounded (≤1500 chars) sanitized
list of the iteration's located findings — severity, title, and location only,
no code bodies — plus the reviewer model. A resumed job can start from the last
findings.
