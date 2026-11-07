# ADR D-ZD2 — Slot lease scope (ZD-05)

Status: **Proposed** · Scope: zoder CLI + ACP client (daemon session slots) · Owner: Jason Perlow
For: the operator. Read time: five minutes.

## 1. DECISION NEEDED

The Zeroclaw daemon caps how many sessions may be open at once, and today a
worker keeps its session — and so its share of that cap — from the first author
turn to the end of the job, including the review phase that never talks to the
daemon. We must choose whether a session is given back when each author turn
ends (holding the cap only while a turn runs) or kept for the whole job
(today's behaviour). We need to decide now because the code for both is already
written on an unmerged branch, and whichever we pick becomes the default every
worker on a shared host gets. Doing nothing leaves the current behaviour: the
cap is consumed for the whole job and long jobs starve the others.

## 2. CONTEXT (verified against this repository)

**Scope note.** This lease is **not** the reviewer concurrency semaphore
(`ZODER_PROVIDER_CONCURRENCY`, `crates/zoder-cli/src/provider_slots.rs:89-105`),
which is already held only for one provider call and is untouched here.
"Slot" in ZD-05 means a **daemon session slot** (`acp.max_sessions`).

**Where the code lives.** Not on this branch. It is on
`origin/feat/slot-lease-20261103`, five commits past the merge base, and does
**not** exist on `origin/master` (`slot_lease.rs`, `ticket.rs` and
`backoff.rs` are all absent there). Every citation below is from that branch,
by file:line as it exists there.

**The problem it addresses.** A daemon session is the capacity unit, but the
client used to send `session/new` with no admission step, hold the session for
its whole job — including the review phase — and give it back only with a
fire-and-forget `session/cancel` (`crates/acp-client/src/slot_lease.rs:1-13`).
Sixty workers against a cap of ten therefore produced fifty refusals on the
first try, and each unacknowledged cancel shrank the usable cap until the
daemon restarted (`slot_lease.rs:3-9`).

**The lease itself is one enum with two values.**

- `LeaseScope::Turn` — "release the session as soon as the turn is over
  (default). The next turn queues for a fresh session; continuity across turns
  is the prompt's job, not the daemon session's" (`slot_lease.rs:83-86`).
- `LeaseScope::Job` — "keep the session after the turn (single-tenant hosts
  that want the continuity hint to keep working)" (`slot_lease.rs:87-90`).

The default is `turn`, selected by `ZODER_SLOT_LEASE_SCOPE`, and `job` is only
set by an explicit, case-insensitive `job` value (`slot_lease.rs:194-198`).
Leasing as a whole is off unless `ZODER_SLOT_LEASE` is `1`/`true`/`on`/`yes`
(`slot_lease.rs:152-175`).

**What "per turn" does to the session.** With `scope=turn` the previous
turn's continuity id is *dropped before the turn starts* — no resume round
trip is even spent on it (`lib.rs:3016-3022`). A persisted or explicit
session is never treated this way (`lib.rs:3021`, `:2822-2836`). So "loses
session continuity" in the backlog text is accurate and is a deliberate,
implemented behaviour, not a side effect.

**What "per job" costs.** The session stays in the daemon's session map after
the turn ends, so the slot stays occupied across check, review and commit, and
until the daemon drops the session itself. `scope=job` marks the lease
`Retained`, which is explicitly not a leak (`slot_lease.rs:674-678`;
test `a_retained_lease_is_not_a_leak`, `tests.rs:502-515`).

**Release is provable, or it is reported as a leak.** A lease is `Released`
only when the daemon answered `session/close` (or said the session is already
gone); anything else — no answer in budget, a daemon without the method
(`-32601`), a transport error — is `Abandoned` with `leak_suspected`
(`slot_lease.rs:662-697`). After an unacknowledged cancel the loop asks for
that release on a fresh connection (`slot_lease.rs:1136`). This is the pairing
with ADR D-ZD1 (abort authority): D-ZD1 stops the turn, this releases the
slot.

**Observability already exists.** One JSON line per transition (`slot.queued`,
`slot.held`, `slot.released`, `slot.abandoned`, …) and process counters
`zoder_slot_*` (`docs/CONCURRENCY.md` on the branch, "Observability"). Any
`leak_suspected` increment means a slot may stay occupied.

**What is not in this repository.** The daemon's own source. That it serves
`session/close` with a `{session_id, closed}` reply is asserted from
Zeroclaw 0.8.x behaviour (`slot_lease.rs:30-36`), not verified here. Whether
the daemon keeps a session's prompt cache across a close is likewise
unverified, and it is the number that decides what continuity is worth.

**Backlog text I could NOT verify.** I could not read
`refactor-research-20261031/BACKLOG-FROM-FABLE-20261107.md`; it is not in this
repository and I could not find it on this host. The ZD-05 text ("per author
turn (frees the slot during review, loses session continuity) or per job
(today's behaviour); **Rec:** turn behind `acp.slot_lease.enabled`; measure
iterations-to-resolve on the corpus before making it the default") is
transcribed from the task. Two parts of it I could check do **not** match the
branch: the flag is `ZODER_SLOT_LEASE` with `ZODER_SLOT_LEASE_SCOPE`, **not**
`acp.slot_lease.enabled` (no key of that name exists anywhere on the branch),
and `turn` is already the default rather than merely a recommendation. Also
unverified: that any other backlog item depends on ZD-05, and that a corpus
run measuring iterations-to-resolve exists — the loop does report `iterations`
and `resolved` per run (`crates/zoder-cli/src/agentic.rs:11876-11877`), so the
measurement is *possible*, but I found no corpus harness for it.

## 3. OPTIONS

### (a) Per job — today's behaviour, keep the session for the whole job

- **Benefit:** session continuity across iterations is real, so the author
  keeps the daemon's conversation context without relying on the prompt;
  no queue round trip per turn. `Retained` is a clean, accounted state.
- **Cost:** the slot is occupied through check, review and commit — exactly
  the phases that never talk to the daemon — so a host at its cap queues
  workers that could otherwise run.
- **Risk:** the cap is the scarce resource on a shared host, and holding it
  through non-daemon work is what produced the fifty-refusals-from-sixty-
  workers incident in the first place (`slot_lease.rs:7-10`). On a
  single-tenant host with a generous cap the risk is negligible.
- **Reversibility:** high — set `ZODER_SLOT_LEASE_SCOPE=turn`.

### (b) Per turn — release at the end of each author turn

- **Benefit:** a worker holds a slot only while a turn is in flight, so the
  cap tracks actual daemon work rather than job wall time. On a host at its
  cap this is the difference between N jobs succeeding and N jobs queuing.
- **Cost:** continuity is deliberately dropped (`lib.rs:3016-3022`), so the
  next turn cannot resume the session and the loop must carry the findings in
  the prompt. It already does: the continuation prompt is built from
  `feedback` and the original task (`agentic.rs:10429-10434`).
- **Risk:** more `session/new` calls, one admission round trip per turn, and a
  measurable cost if the daemon's prompt cache is per-session. That cost is
  **unquantified** — the daemon is outside this tree.
- **Reversibility:** high — set `ZODER_SLOT_LEASE_SCOPE=job`.

### (c) Ship neither yet — keep leasing off and fix admission only

- **Benefit:** the always-on bounded retry on `-32001` already removes the
  thundering herd; scope can wait for data.
- **Cost:** the slot still spans review and commit, so the capacity complaint
  that motivated ZD-05 is untouched.
- **Risk:** the code sits unmerged and rots.
- **Reversibility:** n/a.

## 4. RECOMMENDATION

**Take (b) per turn, behind `ZODER_SLOT_LEASE`, but do not change the default
until the corpus number exists.** Concretely: merge the branch with leasing
off, run `turn` and `job` against the corpus for a week, and only then make
`turn` the default.

1. **The recommendation in the backlog and the branch disagree, and the branch
   should win on the mechanism.** The backlog says "turn behind
   `acp.slot_lease.enabled`"; the branch implements `ZODER_SLOT_LEASE` +
   `ZODER_SLOT_LEASE_SCOPE`, default `turn`. Adopting the backlog's key name
   would mean inventing a second flag for behaviour that already has one.
2. **`turn` is the better default on the evidence we have.** The slot is the
   scarce resource; holding it through review is the behaviour that caused the
   incident the branch was written to fix (`slot_lease.rs:3-10`).
3. **Continuity is not lost silently.** The drop is explicit and documented
   (`lib.rs:3016-3019`), the loop prompt already carries the findings forward
   (`agentic.rs:10429-10434`), and a persisted or explicit session is exempt
   (`lib.rs:3021`). The one honest gap is the daemon's prompt cache, which we
   cannot measure from here — which is exactly what step 2 measures
   indirectly, as iterations-to-resolve and cost.
4. **Leasing stays opt-in until measured.** `from_lookup` returns `None` for
   anything but an explicit on-value (`slot_lease.rs:152-175`), so merging the
   branch changes nothing for an operator who does not set the flag.
5. **`job` remains the right answer for single-tenant hosts** and stays one
   environment variable away.

**The strongest argument against this:** on a host that is *not* at its
session cap, per-turn releases buy nothing and cost a `session/close` round
trip plus a `session/new` per iteration — and if the daemon's prompt cache is
keyed on the session, per-turn leasing could make every iteration pay full
prefill, turning a capacity fix into a latency and cost regression. That is
the risk the corpus measurement exists to settle, and it is a real reason not
to change the default before the numbers are in.

## 5. IMPLEMENTATION PLAN

No step here touches production code; this is what an implementing branch
would execute.

1. **Merge the branch with leasing off.** `origin/feat/slot-lease-20261103`
   onto master. Because `ZODER_SLOT_LEASE` defaults to off
   (`slot_lease.rs:152-175`) this is a no-op at runtime; the always-on part
   (bounded `-32001` retry, `session_limit`/`slot_queue_timeout` classified as
   infrastructure, exit 60, no breaker strike) is already in the branch's
   `crates/acp-client/src/backoff.rs` and `crates/zoder-cli/src/failure.rs`.
2. **Measure both scopes on the corpus.** Run the same corpus task set twice,
   `ZODER_SLOT_LEASE=1 ZODER_SLOT_LEASE_SCOPE=turn` and `...=job`, recording
   per run the `iterations` count and `resolved` flag the loop already emits
   (`agentic.rs:11876-11877`), plus `zoder_slot_*` counters and cost. Define
   "iterations to resolve" as the `iterations` value of a run whose `resolved`
   is true; unresolved runs are recorded as such, not as a number.
3. **Set the default from the data.** If `turn` is within an agreed margin of
   `job` on iterations-to-resolve and does not raise cost per resolved run,
   flip the default to `turn` for the fleet profile. If it is worse, keep
   `job` and revisit only if the daemon's prompt cache is fixed.
4. **Do not change the flag names** and **keep the persisted/explicit
exemption** (`lib.rs:3021`): `ZODER_SLOT_LEASE` + `ZODER_SLOT_LEASE_SCOPE`
exist, and an operator who chose a session id gets it back.
5. **Rollout.** Leasing off in the merged commit; enable per host; watch
   `zoder_slot_leak_suspected_total` — any nonzero value means the daemon did
   not confirm a close and the cap is drifting down.
6. **Rollback.** Unset `ZODER_SLOT_LEASE` (pre-merge behaviour, no revert) or
   set `ZODER_SLOT_LEASE_SCOPE=job` (keep leasing, restore continuity). Both
   are environment changes.
7. **Backlog items unblocked:** ZD-05, by the id in the task text.
   **Not verified:** the backlog file could not be read on this host, so I can
   name no other item this releases and cannot check ZD-05's members.

**Tests.** All of these already exist on the branch
(`crates/acp-client/src/slot_lease/tests.rs`) and must keep passing; they are
the acceptance criteria for this ADR, not new work:

- `a_leased_turn_is_admitted_through_the_queue_and_closed_after_the_turn`
  (`tests.rs:527`) — per-turn scope releases after the turn.
- `a_job_scoped_lease_keeps_its_session` (`tests.rs:546`) — per-job scope
  retains, and `a_retained_lease_is_not_a_leak` (`tests.rs:502`) pins that it
  is not counted as a leak.
- `with_leasing_off_the_turn_is_unchanged` (`tests.rs:560`) — the default-off
  contract that makes the merge safe.
- `on_a_daemon_without_release_the_slot_is_abandoned_and_the_leak_is_counted`
  (`tests.rs:674`) — the unverifiable daemon behaviour is reported, never
  assumed.
- `an_unacknowledged_cancel_is_released_on_a_fresh_connection`
  (`tests.rs:621`) — the D-ZD1 pairing.
- **One test this decision needs and the branch does not have:** a per-turn
  run whose iteration count is recorded end to end, so step 2's measurement
  is pinned by code rather than by a manual run.

## 6. OPEN QUESTIONS FOR THE OPERATOR

1. Is `acp.slot_lease.enabled` a real agreed key, or a placeholder in the
   backlog? The branch uses `ZODER_SLOT_LEASE` /
   `ZODER_SLOT_LEASE_SCOPE`, and no `acp.slot_lease` key exists on it.
2. Which hosts run at the daemon's session cap today? On a host well under
   `acp.max_sessions`, per-turn leasing costs round trips and buys nothing.
3. Does the daemon keep a session's prompt cache across `session/close`? If
   yes, per-turn scope has a real cost we cannot measure from this repository,
   and the corpus run in step 2 becomes decisive rather than confirmatory.
4. Who owns the corpus run, and what is the success threshold for
   iterations-to-resolve? No number exists in this tree today.
5. Should `turn` become the default on the fleet profile only, or on both
   profiles? Standalone is typically single-tenant, where `job` is defensible.
6. The branch is five commits past the merge base and master has moved 1,151
   commits since; who rebases it, and do the `session_limit` failure-code
   changes (`de789c33`) still match master's `failure.rs`?

## 7. STATUS

**Proposed.** No production code changed by this ADR. It needs answers to §6
Q1 and Q2 before the implementation branch is merged and before any default
is changed.
