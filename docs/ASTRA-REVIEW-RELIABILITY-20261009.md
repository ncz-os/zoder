# Review reliability repair, 2026-10-09

Hive job: `01a12258-1f42-7331-9edc-8f5bfe928ba8`.
Canonical base: GitLab `ncz-os/zoder`, `c45483be38378b9a6fed2a1eed63a5d1db23aebb`.
Branch: `fix/astra-review-reliability-20261009`.

## Findings and changes

1. **The ceiling was bytes, not tokens.** Master rejected a single file/hunk
   above 9,000 bytes before contacting the reviewer. Prior commit `3b28425`
   implemented splitting but existed only on the October 5 work branch, not
   GitLab master. This repair carries that commit forward, including the
   cross-chunk map, explicit exclusions, configurable caps and `--dry-run`.
   Additional fixes reserve space for long path labels, repeat only contiguous
   context, exclude no-newline markers from line counts and stop parsing range
   coordinates before function-name text. Tests recover the exact added bytes
   from 40 KB and UTF-8 new-file diffs and verify the per-chunk bound.

2. **A 25-second idle timeout also acted as a first-token timeout.** A large
   prompt could fail during prefill despite a longer request timeout. Prefill
   now receives the total request budget; after answer output starts the idle
   guard still applies. The former timeout restarted after response headers,
   permitting headers and body to consume separate budgets. One outer deadline
   now covers both. A tracked output sink preserves `emitted=true` when that
   deadline interrupts a stream, preventing duplicate output through retry.

3. **Malformed JSON counted as a completed reviewer.** Standalone review
   synthesized `request_changes` from invalid output and still counted the slot
   as successful. It now makes at most one JSON-only retry on the same model,
   then returns an incomplete-review error. The loop uses the same whole-object
   schema validation. Incomplete objects, schema examples inside prose, invalid
   finding fields and contradictory approval/high-severity findings fail.
   A single enclosing code fence remains supported; prose/object extraction is
   not used at the approval boundary. The permissive October 4 parser change
   `28eea39` was inspected and deliberately not copied.

4. **Partial panels could advertise completion and approval.** They now report
   `complete=false`, cannot produce aggregate `approve`, and exit nonzero.
   Individual successful votes remain visible alongside failed slots.

5. **Requested identity was mistaken for served identity.** Direct completion
   parsers now retain the response body's model separately from a proxy's
   opaque deployment ID. Reviewer dispatch rejects an explicitly different
   served model, after recording the call's cost/policy evidence. Successful
   standalone review receipts include requested model, reported response model,
   provider, endpoint, proxy deployment/fallback telemetry and latency. A
   provider omitting its model remains explicitly unknown (`null`), not a
   fabricated match. Pool/chunk retries stay pinned to the selected reviewer.

6. **Outer cancellation could miss the first session and ignore failure.**
   The actual `session/new` result is now recorded before prompt dispatch in
   a task-scoped tracker. This covers fresh turns and replaced stale sessions.
   A socket-level regression times out a fresh turn, then verifies that the
   watchdog cancels that exact session and receives acknowledgement. Failed or
   timed-out cancellation stops before diff capture; it no longer waits a
   fixed grace and assumes the tree is safe. A daemon that refuses cancellation
   still requires operator/daemon recovery; the client does not claim it stopped.

7. **Invalid reviews caused further author rounds.** After the bounded
   formatting/location retry, unavailable or unlocated verdicts now stop the
   loop unresolved. They are not fed back as source defects. Existing bounded
   handling of real repeated findings remains in place.

8. **503 handling already existed on master.** HTTP 503 is a transient server
   failure with bounded same-model retries and backoff. Existing tests cover
   zero retries, transient recovery and exhausted retries before eligible
   fallback. An explicit reviewer pin is not silently substituted. The NCZ
   incident's 503 results are service failures, not successful reviews. This
   repair does not claim to fix the remote provider's capacity.

## Verification and scope

Rust compilation/tests run on idle ULTRA `192.168.207.88`, using btrfs under
`/home/jasonperlow/Projects`. HYDRA and all NCZ worktrees/boot settings are
excluded. The initial test scratch path exceeded Unix-domain socket pathname
limits; the final run uses the shorter disk-backed `Projects/za1009-tmp`.

Before the final session-tracking addition, 591 CLI tests and all four new
provider deadline regressions passed. Final workspace, Clippy and CLI dry-run
results are recorded in the delivery receipt. No Zoder or model review was
used to approve these changes; verification uses source inspection, controlled
HTTP/socket peers and the Rust test suite.

Source delivery does not update installed fleet binaries. The prior mechanical
log specifically found ULTRA running a stale binary despite fixed source on
other hosts. Deployment must identify the built source SHA and binary digest;
the old `zoder 0.2.1` version string alone cannot prove this repair is installed.

## Known limits

The 2026-10-10 follow-up (branch `fix/astra-review-followup-20261010`) reviewed
the following items and deliberately left them unchanged. They are not defects
in the code paths changed by that follow-up; each needs its own change with
independent evidence.

* **A strict served-model name match can reject a provider that echoes a
  resolved or dated model id.** The check fails closed. Confirm what CERBERUS
  `gemma4-31b` and EIH `nemotron` actually return before deploying any change.
* **`cancel_session` does not validate `session_id` on `turn_complete`.** A
  completion for a different session can be attributed to the cancelled one.
* **Goose loops abort on the first author timeout.** `record_active_session` is
  not recorded at the goose `session/new`, so the watchdog cannot cancel that
  session.
* **No idle-stall guard while only reasoning tokens stream.** A stream that
  emits reasoning tokens but no answer tokens never trips the idle timeout.
* **`drain_after_cancel` returning `timeout` lets `cmd_loop` proceed to review
  after an unacknowledged agent-timeout cancel.** Pre-existing; the fix is a
  separate change.
