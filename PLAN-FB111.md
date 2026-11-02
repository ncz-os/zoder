# PLAN-FB111 — zoder: prose with inline approve must never approve (audit F4)

Base: master f30f581. Branch: freebuff/FB111-20261102.
Source: ~/Projects/refactor-research-20261031/audit-zoder-session-limit-20261102.md (F4).

## Finding verification (reproduces at head)

- `cmd_review` parses each reviewer's completion with lenient `parse_review`
  (agentic.rs:6427). `parse_review_json_only` extracts a verdict object from
  ANYWHERE in the text; W3 most-blocking only helps with 2+ objects. A lone
  schema example `{"verdict":"approve",...}` in prose decodes (ReviewOutput
  and Finding are all-defaults) and returns approve.
- Loop paths already strict (`strict_review` at 1366, 5234, 5291). Only the
  `zoder review` slot parse is lenient. "I would approve" prose already
  fails closed (D4.6 removed prose recovery).

## Change (one commit, tests green after)

1. New `review_gate_parse(raw)`: `strict_review` success returns as-is
   (bare/fenced/think-wrapped whole-reply verdicts keep working); otherwise
   the `parse_review` extraction is consulted but an `approve` from it fails
   closed (request_changes with the raw body preserved); blocking extractions
   pass through unchanged (fail-closed direction preserved).
2. `cmd_review`:6427 uses `review_gate_parse`.
3. Extract the fail-closed constructor shared by the gate (dedup with
   `parse_review`'s fallback; no behavior change to the fallback itself).
4. Tests: quoted-schema example, think-tagged inline approve, "I would
   approve" prose all fail closed; strict bare/fenced/think-wrapped approves
   still approve; prose-embedded request_changes still blocks.

## Risks

- Reviewers that chat around a fenced verdict now fail closed (blocking).
  Intended per brief (structured gate is the only way to approve); real
  reviewers are instructed to emit bare JSON, and fenced-only still passes.
- `parse_review` keeps its lenient behavior for any future caller; only the
  review gate changes. Dead-code risk: none (parse_review still used by the
  gate + tests).

## Non-goals

F4 residual (prose-only reply maps to review_blocked instead of
reviewers_unavailable) — brief does not ask; reported as limitation.
Daemon-side changes, loop-path changes (already strict).
