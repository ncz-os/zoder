# PLAN-FB133 — a full daemon must not trip model breakers fleet-wide

Audit F5.1 (High): capacity errors from a full daemon counted against the model
and were written to the shared health file, taking the route out of rotation on
every host.

## Premise check on master
- The SETUP path is already fixed (FB110): `is_capacity_setup_error` (main.rs
  ~12584) scans the whole error chain for the session-limit marker; the chain
  dispatch loop (~13350) returns Err immediately with no breaker strike and no
  fallback.
- The POST-RUN path is still reachable: `run_agent_dispatch` returns
  `Ok(AgentRun)` once any frame is emitted, and a mid-turn session-capacity
  refusal can end the as `AgentOutcome::Failed` with the daemon's own
  diagnosis in `run.failure_detail` ("turn failed: Session limit reached (code
  -32001)" / "session_limit: ..."). The post-run else-arm (~13563) records
  `record_classified_failure(..., Classification::Error)` for that — a real
  breaker strike derived from a LOCAL daemon being full, written to the
  SHARED health_path (fleet-wide bleed). That is the remaining hole.

## Change (crates/zoder-cli/src/main.rs only)
1. New pure helper near the other turn helpers:
   `enum TurnFailureHealth { Record { reason: String }, SkipCapacity }`
   and `fn turn_failure_health(outcome: &zoder_core::AgentOutcome,
   detail: Option<&str>) -> TurnFailureHealth` —
   detail carries a session-limit marker (`zoder_core::engine_rpc::
   is_session_limit_message`, same predicate FB110 trusts) -> SkipCapacity;
   otherwise Record with reason "turn did not complete: <detail|outcome>"
   (detail included exactly as the terminal-error path surfaces it).
2. Post-run else-arm uses the helper:
   - SkipCapacity -> record NOTHING, do NOT call save_health (the shared
     file must not change for a local daemon-capacity outcome), print a
     short diagnostic line (suppressed by --quiet).
   - Record -> `record_classified_failure(..., Classification::Error)` then
     `save_health` as before.
   - The success arm now saves explicitly (same end state as today).
3. Tests (main.rs test module, next to the FB110 setup-capacity test):
   - `capacity_turn_detail_skips_the_health_strike` (helper: session_limit
     marker + raw daemon shape -> SkipCapacity; approval-denial -32001 and
     generic failures -> Record).
   - `genuine_turn_failures_still_strike` (helper: Record reasons.
   - `daemon_capacity_turn_never_writes_the_shared_health_file`
     (store-level both-directions pin: capacity arm writes nothing to a
     tempdir health.json — no file, no entries; genuine arm records
     failures=1 / consecutive_failures=1).

## Risks
- `failure_detail` only mines the FIRST non-empty line via
  `turn_failure_summary` for display; the health reason now embeds the raw
  trimmed detail (store truncates at 160 chars). No downstream consumer
  pattern-matches the old "turn did not complete: failed" shape except as
  `last_error` prose.
- save_health moved from unconditional into the three arms: identical
  behavior on success/genuine-failure paths; only the new skip arm differs.

## Test loop (no .py files change; ruff n/a)
`cargo test -p zoder-cli` (unit tests, includes the new ones) in one batched
command; structured non-author review: `zoder --json review --scope branch
--base master -m nvidia/nemotron-3-ultra-550b-a55b`.

Branch: `freebuff/FB133-20261103` from origin/master.
