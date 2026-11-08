# ADR D-ZD1 — Abort authority (ZD-04, ZD-14)

Status: **Proposed** · Scope: zeroclaw daemon + zoder CLI · Owner: Jason Perlow
For: the operator. Read time: five minutes.

## 1. DECISION NEEDED

Something has to be allowed to stop a turn on the shared zeroclaw daemon when
its client is gone, and today zoder cannot: it can ask the daemon to stop a
turn and wait five seconds for the answer, but it cannot make the daemon forget
the session, and the daemon has no idle timeout of its own. We need to decide
now because a worker killed at its 7,200 s limit can leave a turn running in a
daemon shared with other jobs, and every later job on that host queues behind
it. Doing nothing keeps that stall: the turn stays, zoder reports "cancel not
confirmed", and nothing on the host reclaims it.

## 2. CONTEXT (verified against this repository)

**What zoder does today when it wants a turn stopped.** It sends
`session/cancel`, an ACP *notification* with no reply and no `id`
(`crates/acp-client/src/lib.rs:2262` `cancel_session`; frame at
`lib.rs:2315`), then waits for the daemon's `session/update`
`{type: "turn_complete"}` for that same session and gives up after
`CANCEL_SETTLE_BUDGET`, 5 s (`crates/zoder-core/src/lib.rs:88`; used at
`lib.rs:2340`). A `turn_complete` for a *different* session is not an ack
(`lib.rs:174`, `:219`). An EOF before the ack is a failure, not a success
(`lib.rs:1763-1772`, `:2345-2360`).

**The cancel is best-effort, and already survives failing.** After the first
failure the loop re-sends it `AUTHOR_CANCEL_RETRY_ATTEMPTS` (= 2) times
(`crates/zoder-cli/src/agentic.rs:7848`, `:8011`). If all attempts fail the
iteration is marked unresolved and the author's work is kept, committed and
reported (`agentic.rs:8046`) — the loop does not abort on one miss. It stops
only after `UNCONFIRMED_CANCEL_ABORT_THRESHOLD` (= 2) consecutive misses
(`agentic.rs:7859`), or after the *first* miss on a daemon shared with other
jobs (`agentic.rs:7871`).

**The daemon does not interrupt a running tool step.** The documented contract
(`docs/CANCEL-PROTOCOL.md:9-14`) is that it marks the turn cancelled and emits
`turn_complete` when the current step returns, so a long build answers tens of
seconds late. The settle budget is therefore scaled to the step in flight
(`docs/CANCEL-PROTOCOL.md:18-24`; `crates/zoder-core/src/cancel_budget.rs`).
An error reply saying the session is unknown, finished or already cancelled
settles the cancel (`docs/CANCEL-PROTOCOL.md:25-27`).

**The limit that matters.** A session is durable state on disk — one JSON
transcript per session under `$ZODER_HOME/sessions/<id>.json`
(`crates/zoder-core/src/session.rs:1-5`, `:109-111`, `:271`), listed by
`zoder sessions` (`crates/zoder-cli/src/main.rs:17920`). Nothing in this
repository removes or releases a session: `Session` exposes `load_or_new`,
`latest`, `push`, `save`, `mutate_locked` and `list` (`session.rs:140`, `:200`,
`:234`, `:271`, `:385`, `:414`), and no delete.

**What zoder already does about a daemon that stops answering.** On an
unacknowledged cancel the reaper will, as a last resort, stop the daemon
*process* by exact pid — only under a lease census that fails closed
(`crates/zoder-cli/src/reaper.rs:1410`). `daemon_stop_authority`
(`crates/zoder-core/src/engine_rpc_impl.rs:221`) answers `NotAuthorized`
unless this client holds a live lease, no other live client holds one, and
either this client is the recorded owner or (owner gone) its own lease was
refreshed within `DAEMON_LEASE_TTL` = 60 s (`engine_rpc_impl.rs:62`). Census
and signal share one exclusive lock (`reaper.rs:1470-1503`;
`engine_rpc_impl.rs:279`). This exists because one unacked cancel once killed
the daemon out from under sibling loops (`docs/FAULT-TO-FIX.md`, "Concurrent
loops on one host lost the shared engine").

**What the daemon does when a client disappears mid-turn: nothing is proven.**
zoder spawns it with `--ephemeral` (`crates/zoder-cli/src/main.rs:8898`). A
comment at `crates/zoder-core/src/engine_rpc_impl.rs:55-61` asserts the daemon
has its own "last-client idle timeout"; that is a comment, not a setting, and
**the daemon's source is not in this repository**, so daemon behaviour here is
reported, never verified. Verified: zoder has no daemon idle-timeout setting.
The only idle timeout in this tree is the per-turn `IdleWatchdog` in the ACP
client (`crates/acp-client/src/lib.rs:854`, `:1213`; configured from
`agent_idle_timeout_*`, `crates/zoder-cli/src/main.rs:437-449`), which fires
while the *client* is alive and watching.

**Guards this must not break.** The reaper spares any process running the
daemon binary, by pid + start time + resolved executable, never by name
(`reaper.rs:49-70`), and selects only same-uid processes under the run's own
cwd or scratch dir (`reaper.rs:20-33`). Sibling sessions and other tenants stay
untouched.

**Backlog text I could NOT verify.** I could not read
`refactor-research-20261031/BACKLOG-FROM-FABLE-20261107.md`, or find the ZD-04
/ ZD-14 entries anywhere on this host, so the three options (a), (b), (c) and
the recommendation are transcribed from the task text, not checked against the
backlog. Also unverified: (i) that the daemon has a permission-gated
`sessions:delete` capability; (ii) that it has any idle-TTL setting (ZC-04);
(iii) that it reaps a turn when the last client disconnects. All three are
properties of a codebase outside this repository.

## 3. OPTIONS

Judged on what it buys, what it costs, what can go wrong, and how hard it is to
undo.

### (a) Grant the worker `sessions:delete` and use the existing `session/abort`

- **Benefit:** the only option that can *prove* a turn is dead rather than
  infer it. Cheap in zoder: `cancel_session` is ~80 lines
  (`lib.rs:2262-2340`) and the ACP client already owns the socket.
- **Cost:** one client call; a worker-side grant (config, not code); socket-level
  tests, which acp-client already has (`lib.rs:11523`, `:11566`, `:11711`).
- **Risk:** the authority is wider than the problem. A client that can delete
  another client's session on a shared daemon is a cross-tenant hazard, and
  whether `session/abort` checks ownership is in the daemon's source, which I
  could not read. If it silently ignores an unowned id we are back to waiting
  out a budget; if it honours it we have removed a sibling session nobody asked
  us to remove.
- **Reversibility:** high. Revoke the permission; zoder falls back to
  `session/cancel` alone.

### (b) Wait for an idempotent `session/release` in zeroclaw

- **Benefit:** the right home for the state. Idempotent, so a retry after a
  timeout is harmless; session-scoped rather than turn-scoped; the daemon can
  apply its own ownership rules.
- **Cost:** zero here today, everything later — it is a change in a codebase
  outside this tree, on that team's schedule.
- **Risk:** an indeterminate wait. Meanwhile workers do (a) or (c) anyway and
  we carry two mechanisms. The cost of waiting is paid every time a worker is
  killed mid-turn, which is the steady state (`agentic.rs:7722`).
- **Reversibility:** not applicable — a dependency, not a change.

### (c) Rely on a daemon idle TTL (ZC-04)

- **Benefit:** no new zoder authority, and it cannot delete a session another
  client is watching. The only option that covers a client killed with no chance
  to send anything — SIGKILL, or the worker's 7,200 s timeout.
- **Cost:** nothing in zoder; one setting plus its expiry logic in the daemon.
- **Risk:** a TTL is a guess about progress, and the cancel contract says a
  legitimate tool step runs for tens of seconds (`docs/CANCEL-PROTOCOL.md:9-14`).
  Too short kills a healthy build; too long outlives the job it was meant to
  outlive. It is also the option I can verify least — no daemon idle-TTL
  setting exists in this tree, so ZC-04 is entirely a daemon change.
- **Reversibility:** high. Raise the TTL, or disable it.

**Not an option:** stopping the whole daemon by pid. It exists
(`engine_rpc_impl.rs:221`) and stays as restricted as it is; widening it puts
every other job on the host at risk to reclaim one turn.

## 4. RECOMMENDATION

**Take (a) now, with a denied-permission fallback; take (c) on the daemon side
as ZC-04; take (b) later, when zeroclaw ships it.**

1. **The fallback is already correct, so (a) is an improvement, not a
   prerequisite.** A failed cancel no longer throws work away: it is retried
   twice, the iteration is marked unresolved, the work is committed
   (`agentic.rs:8011`, `:8046`). Granting `sessions:delete` cannot make the
   loop *less* safe; it only lets a run *finish* instead of spending its budget
   queued behind a turn nobody can stop.
2. **"Denied permission" must be a supported answer, not an error.** Older
   daemons and locked-down deployments will refuse. The client tries abort and,
   on a permission or unknown-method refusal, falls back to the existing
   `session/cancel` path with a reason naming which was refused. That keeps the
   change additive and (a) reversible on a host whose daemon lacks it.
3. **(c) covers what (a) structurally cannot.** A client killed by the worker
   sends nothing. Only the daemon can notice nobody is coming back. (c) is a
   complement, not a competitor.
4. **(b) is the right end state and the wrong thing to wait for.** Idempotent
   and daemon-scoped is exactly what retried cleanup needs — but it is outside
   this repository, so waiting means the stall persists indefinitely.

**The strongest argument against this:** it gives a worker on a shared daemon
the power to end sessions it does not own, and the only thing between two
tenants is a capability check whose ownership rules live in code we cannot read
— one check that silently succeeds on another client's id is cross-tenant data
loss, which is a worse failure than a slow job.

## 5. IMPLEMENTATION PLAN

No step here touches production code; this is what an implementation ADR
would execute.

1. **Confirm the daemon contract first.** Read the zeroclaw source for
   `session/abort`: exact method name, parameter keys, whether it requires a
   capability, whether it rejects an id the caller does not own, and whether it
   is idempotent on a finished session. If any answer is unclear, stop and take
   (b) instead.
2. **Client:** add `abort_session(socket, session_id, budget)` beside
   `cancel_session` in `crates/acp-client/src/lib.rs`, same shape (fresh
   connection, `initialize`, JSON-RPC *request* with an `id` so a refusal is
   visible rather than a timeout).
3. **Loop:** in `agentic.rs`, call abort before the first `session/cancel`
   retry; on refusal record it and drop into `cancel_author_session_best_effort`
   unchanged. Do not move either abort threshold (`agentic.rs:7859`, `:7871`)
   in the same step.
4. **Exit path:** the reaper's teardown (`reaper.rs:1410`) applies the same
   rule, so a settled session is never re-asked.
5. **Worker grant:** add `sessions:delete` to the worker's daemon configuration
   only, and only after step 1 answers the ownership question. This is the
   worker's grant, not a default for every zoder.
6. **Daemon side (ZC-04):** an idle TTL that stops a turn with no attached
   client. It must exceed the longest legitimate tool step; the loop already
   assumes a step can outlive 5 s (`docs/CANCEL-PROTOCOL.md:18-24`,
   `crates/zoder-core/src/cancel_budget.rs`).
7. **Backlog items unblocked:** ZD-04 and ZD-14, by id from the task text.
   **Not verified:** the backlog file could not be read on this host, so I can
   name no *other* item this releases and cannot check ZD-04 / ZD-14 members.
   ZC-04 is referenced but likewise unread.

**Tests** (each fails before, passes after; socket-level, in the style of
`lib.rs:11523+`):

- `abort_session_sends_a_request_with_an_id_and_the_session_id` — the frame
  carries `session_id` (snake_case; the Y-2 lesson at `lib.rs:2307-2315`) and an
  `id`, so a refusal is delivered rather than waited out.
- `abort_session_reports_a_permission_refusal_as_denied_not_slow` — an error
  reply naming a missing capability comes back as `Denied`, not a timeout.
- `a_denied_abort_falls_back_to_cancel_and_never_marks_the_loop_aborted` — on a
  shared daemon (`unconfirmed_cancel_abort_threshold(true) == 1`) a denial must
  not become the "first unconfirmed cancel".
- `an_aborted_turn_never_reviews_a_torn_tree` — the tree is snapshotted only
  after the ack, matching the EOF-is-not-an-ack rule (`lib.rs:1763-1772`).
- `an_unknown_session_abort_is_settled_not_retried` — parity with
  `docs/CANCEL-PROTOCOL.md:25-27`.
- Daemon: `an_idle_turn_with_no_client_is_stopped_after_the_ttl`,
  `a_turn_with_a_connected_client_is_never_stopped_by_the_ttl`.

**Rollout.** Deny-first: attempt abort only when the daemon advertises the
capability; enable the worker grant per host; watch the `abort_reason` /
`cancel_unconfirmed` fields the loop already emits (`agentic.rs:10940`) for a
week before enabling everywhere.

**Rollback.** Remove `sessions:delete` from the worker config. The client sees
a refusal on the first attempt and takes the unchanged `session/cancel` path —
no code revert. Disabling ZC-04's TTL is one daemon setting.

## 6. OPEN QUESTIONS FOR THE OPERATOR

1. Does `session/abort` exist under that name, and does the daemon refuse ids
   the caller does not own? (Blocks step 1 and all of (a).)
2. If jobs A and B share a daemon and A aborts a session B is driving (a reused
   id), is losing B's turn acceptable, or must the daemon refuse any abort whose
   caller did not *create* the session?
3. How long should the ZC-04 idle TTL be? The one measured number in this tree
   is `ZODER_CANCEL_ACK_CAP_S` (default 60 s, `docs/CANCEL-PROTOCOL.md:20-22`).
   Anything shorter than the longest tool step the fleet runs kills good work.
4. Should the *worker* send abort on SIGTERM before it kills zoder? Today it
   kills zoder's group and zoder's `Drop` sends a blocking cancel
   (`reaper.rs:1-14`); nobody covers SIGKILL.
5. Should (a) apply to `exec` and `rescue` too, or only `loop`? Loop-only keeps
   the blast radius smallest.

## 7. STATUS

**Proposed.** No production code changed by this ADR. It needs answers to §6
Q1 and Q2 before any implementation ADR is written.
