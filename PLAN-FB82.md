# PLAN-FB82 — zoder docs claim-by-claim verification (report only)

Task (brief): "Verify every concrete claim in zoder's docs/ (flags, env vars
and defaults, exit codes, config keys, file paths, cost and budget rules)
against the source. Write docs/verification/zoder-docs-claims-20261101.md
as a table (claim, doc file:line, source file:line, TRUE/FALSE/UNVERIFIABLE).
Fix nothing; list the false ones first."

Base: master `1cbe935`. Branch: freebuff/fb82-20261101. Nothing is modified
in any doc or source file; the only artifact is the verification report.
`1cbe935` is the branch point, not today's master: the report is a snapshot
audit of that tree, and the section "Re-verification against current master"
records which findings still reproduce on later master.

## Claim sources (docs/), in priority order

1. `docs/EXIT_CODES.md` (280 lines): the five classes and their exit codes;
   the frozen named-code table (18 codes); the exit ladder (0/10/20/30/40/50,
   75 reserved, 78 oneshot budget decline checked first); `UNCLASSIFIED_
   EXIT_CODE = 50`; both --json failure shapes; the failure object field
   table; the budget-refusal recognition ("budget_exceeded:" prefix +
   optional reset, runs in `turn_failure_error` and `classify_error`); the
   failure-site assignment table.
   Sources: crates/zoder-cli/src/failure.rs, main.rs (contract_exit_code,
   error_exit_code, emit_json_failure, turn_failure_error), agentic.rs
   (cmd_review, cmd_loop, aggregate_verdict, attach_failure_to_result),
   review_diff.rs.
2. `docs/CONFIG_REFERENCE.md` (180 lines): the key table (all 32 keys);
   deployment.mode default; profile fields; provider fields incl. kind enum,
   auth enum, engine_provider_ref, billing modes, rate_limit_group default;
   model-family validation errors; budget fields incl. est_output_tokens
   default 1024 and the paid-only caps; review fields; context defaults;
   routing presets (4, default balanced); engine config surface; all env
   vars incl. ZODER_UPDATE_CHECK_RETRY_S default 900; the three CLI route
   flags and the pool constraints.
   Sources: crates/zoder-core/src/config.rs, budget.rs; config.standalone.toml,
   config.fleet.toml; crates/zoder-cli/src/agentic.rs, main.rs, update.rs;
   scripts/test_config_reference.py.
3. `docs/PROVIDER-ROUTING.md` (159 lines): routing presets/scenarios, chain
   resolution and fallback rules, paid gating — verify against config.rs +
   routing code.
4. `docs/PROFILES.md` (99 lines): profile names and what each preset writes
   (verify against init/profile code and templates).
5. `docs/CONCURRENCY.md` (288 lines): lock names, queue bounds, rate-limit
   groups — verify against the concurrency code the doc names.
6. Time-box: README.md is outside docs/ and skip-worthy if budget gets tight
   (the brief scopes to zoder's docs/; note the exclusion in the report).

## Method

- One terminal pass per claim family: `grep -n` the concrete token in the
  cited source file(s); record source file:line for each verdict.
- Verdicts: TRUE (claim and value match source), FALSE (source disagrees —
  list these FIRST in the report), UNVERIFIABLE (needs a live service,
  daemon, or fleet runtime this host cannot stand up).
- Every FALSE entry gets a one-line evidence quote from the source.
- No source or doc file is edited by this task.

## Node definitions

- "Concrete claim" = a statement a greppable token or a numeric default
  could refute: identifiers, enum variants, defaults, exit numbers, file
  paths, precedence orders. Prose/nuance stays out of the table.
- Config keys are verified by locating the exact field in config.rs structs
  (or the standalone/fleet templates), not merely a mention.

## Risks

- Wrong-file drift: docs cite specific files/functions; if a claim moved to
  another file, cite where it DOES live rather than calling FALSE, and mark
  the doc line "stale but true at <file>".
- The self-test `scripts/test_config_reference.py` (referenced by
  CONFIG_REFERENCE.md) is run to confirm the key table exists as claimed.

## Deliverables

- docs/verification/zoder-docs-claims-20261101.md: FALSE rows first, then
  TRUE rows, then UNVERIFIABLE rows, each with doc file:line and source
  file:line.
- Summary entry + MNEMOS memory + coordination post after push.
