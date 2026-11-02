# PLAN-FB110 — zoder: -32001 session limit is retryable infrastructure (audit F1+F2)

Base: master 9c80049 (post-FB108). Branch: freebuff/FB110-20261102.
Source: ~/Projects/refactor-research-20261031/audit-zoder-session-limit-20261102.md

## Finding verification (all reproduce at head)

- F1: `drive` (~lib.rs:3020) and `drive_goose_io` (~lib.rs:4985) bail on the
  FIRST `session/new` error (`may_replace_rejected_session` only knows -32000
  and -32602-ownership). No retry, no `session_limit` code anywhere in the repo.
- Scoping trap: -32001 is REUSED for "user denied approval" on approve replies
  (lib.rs:13862 test). Matcher must require code AND "session limit" text, and
  retry must apply ONLY to `session/new` responses.
- F2: `loop_failure` evidence = `review_error.or(author_error)` (agentic.rs:9513);
  incident shape (turn None, empty diff, review fails) yields
  `reviewer_unavailable` with null reviewer. Teardown (11599) reports
  `reviewer_unavailable` whenever final_verdict says so, even when the author
  failed; `author_failed` arm (`iterations.is_empty()`) is near-unreachable.
- F5.1: chain-fallback Err arm (main.rs:13290) strikes the breaker for EVERY
  setup error and falls through to the next model — a full daemon trips every
  model's breaker and spams one session/new per model into it.

## Changes (one commit each, tests green after each)

1. acp-client: `SESSION_LIMIT_REACHED=-32001`, `is_session_limit_message`
   (pub, code+message predicate), `session_limit_retry_delay(attempt)` pure
   schedule 2,4,8,16,30s + time-seeded jitter (no new deps), retry helper with
   injectable `delays: &[Duration]` wired into both drive paths (fresh +
   resume-reject fresh-create). Final error carries `session_limit:` marker.
2. failure.rs: `FailureClass::Infrastructure` ("infrastructure"),
   `EXIT_INFRASTRUCTURE=60`; `FailureCode::SessionLimit`; classify rule;
   update ALL + frozen-list test + doc-table test + D3 comment.
   Infrastructure is NOT transient (`only_transient_is_retryable` stays green).
3. main.rs chain fallback: session-limit setup error -> no breaker strike, no
   fallback, immediate Err (factored predicate, unit-tested).
4. agentic.rs loop: `author_session_limit` flag -> skip check (None), review
   third skip arm (same record shape, reviewer_unavailable verdict +
   session-limit review_error text); existing break fires. `loop_failure`
   author-priority when author produced no model + author_error present.
   `loop_teardown_reason` extracted with failure-code priority for author-side
   codes. Loop payload gains `failure_class: "<class>:<code>"` next to `failure`.
5. docs/EXIT_CODES.md: class row, code row, 60 rung, worker case, assignment
   rows, `failure_class` field. Frozen contract tests updated in (2).

## Tests

- acp-client: schedule unit test; matcher +/- tests (approval-denial -32001
  NOT matched; session-limit text with other code NOT matched); fake-daemon
  1x-32001-then-success; always--32001 with `&[]` -> immediate marker error;
  with 2 short delays -> 3 attempts server-side.
- failure.rs: frozen list, doc table, to_json, classify_message cases.
- agentic.rs: loop_failure author-priority (2 cases + preserved review-wins
  case); loop_teardown_reason matrix.
- Full: `cargo test -p acp-client`, `-p zoder-cli`, then workspace.

## Risks

- Golden tests asserting exact loop payload keys (adding `failure_class`) —
  run suite, evaluate if red.
- `has_word("session_limit")` vs underscore splitting — check `word_matches`
  before relying on single-token marker; dual rule ("session limit" |
  "session_limit") covers it.
- Exit 60 consumers: non-zero stays failure; worker case statement updated.

## Non-goals

`new_session` transfer helper, F3 close-protocol, F4 prose-approve (FB111),
F5.4 billing scope, daemon-side changes.
