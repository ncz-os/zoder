# The cancel protocol

`session/cancel` is an ACP notification (no `id`, no reply). zoder sends it
in three places: the in-band `drive()` loop when `--agent-timeout` fires, the
loop's author-phase watchdog when `--loop-timeout` fires (fresh connection,
`initialize` + `session/cancel`), and the exit path (`reaper`) for every
session the run drove. The daemon acknowledges by sending
`session/update {type: "turn_complete"}` for that session; a turn that has
already ended is never acknowledged.

The daemon does not interrupt a running tool step: it marks the turn
cancelled and emits `turn_complete` when the step returns. A build or a test
run therefore answers tens of seconds after the cancel, which the flat 5 s
settle budget (`CANCEL_SETTLE_BUDGET`) could never see.

What changed (2026-11-04):

* The loop records the in-flight tool step from the `tool_call` /
  tool-result events it streams (`zoder_core::cancel_budget`). The FIRST
  author-phase cancel waits at least as long as that step has already run,
  between 5 s and `ZODER_CANCEL_ACK_CAP_S` (default 60; `0` disables the
  scaling). With no tool call pending the budget is 5 s as before. The
  outer cancel deadline grows with it, so the wait is never cut off.
* A JSON-RPC error reply saying the session is unknown, finished or already
  cancelled settles the cancel: there is no turn left to stop.
* A cancel the loop had acknowledged is recorded for the exit path, which no
  longer re-cancels that session (one settle budget and one misleading
  `cancel not confirmed` line per session).
* A per-session window the first attempt spent entirely no longer grants a
  1 ms "retry": the record reports the one attempt that was made.
