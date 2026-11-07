# ADR D-ZD2 — Slot lease scope (ZD-05)

Status: **Proposed** · Scope: zoder CLI (review dispatcher) · Owner: Jason Perlow
For: the operator. Read time: five minutes.

## 1. DECISION NEEDED

zoder caps how many reviewer calls may run at once against one provider, and
today one zoder process counts as one holder of that cap for its whole life.
We have to choose whether a holder should instead end when an author turn ends
and come back at the next one — freeing the cap while the loop is reviewing,
but starting every turn without the earlier conversation. We need to decide now
because the review phase runs inside the same process that holds the cap, and
every round the loop spends reviewing is time other jobs on the host wait. Doing
nothing keeps today's behaviour: the cap is held across the whole job and one
long session can starve the others.

## 2. CONTEXT (verified against this repository)

**What the "slot" actually is.** It is not a lease object. It is a set of
`flock`-ed files, `N` of them per provider, created under
`<zoder home>/state/slots/<provider>/`, and a caller holds one by keeping the
file open (`crates/zoder-cli/src/provider_slots.rs:89-105` `try_take`;
`provider_slots.rs:53-55` `SlotGuard` with `_file`). Locking is exclusive and
non-blocking per file; the wait loop tries `0..limit` on every pass
(`provider_slots.rs:90-104`) and sleeps 200 ms between passes
(`provider_slots.rs:27`, `:154`).

**How long a holder lives.** Exactly as long as the `File` lives: the lock is
released when the guard is dropped, and the module doc states a dead process
releases its slot (`provider_slots.rs:1-25`; `docs/CONCURRENCY.md:220-221`).
There is no TTL, no heartbeat, and no owner id written anywhere — the slot is
"held" purely by an open file descriptor.

**Where it is taken.** Inside `dispatch_reviewer_for_model`
(`crates/zoder-cli/src/agentic.rs:2184`), the slot is acquired at
`agentic.rs:2372-2379` and held in `slot` for the rest of the function. A
second, group-level slot is taken at `agentic.rs:2397-2417`, and a rate turn at
`agentic.rs:2421-2433`. All three guards live until the function returns
(`agentic.rs:2795`), i.e. until that one provider call — including retries
inside the ledger loop — has finished.

**The lease is already per call, not per job.** A zoder process holds nothing
between reviewer calls: the guard is created and dropped inside one dispatch.
What spans the job is not a slot at all, but:

- **the process**, which stays alive for the whole `zoder loop` run and
  re-takes slots on every review (`agentic.rs:2354-2379`); and
- **the provider concurrency limit**, which is a host-wide number shared by
  every zoder process of that user (`docs/CONCURRENCY.md:216-222`).

**Where the real waiting lives — inside each call, not between them.** The loop
runs chunk 0 alone and then up to `DEFAULT_CHUNK_CONCURRENCY` = 4 later chunks
at once through `buffer_unordered` (`agentic.rs:5209`,
`agentic.rs:4963-4973`, `agentic.rs:5578-5588`). Each chunk re-enters
`dispatch_reviewer_for_model` and queues for its own slot
(`agentic.rs:5164-5166`). With `ZODER_PROVIDER_CONCURRENCY` unset the limit is
0 and no slot is ever taken (`provider_slots.rs:118-125`), and both deployment
profiles default to 0 (`crates/zoder-core/src/config.rs:2484-2488`; the
per-provider `max_concurrent` default in `config.rs:4336-4344` is also 0), so
the queueing in this tree only happens when an operator sets a cap.

**What is lost if the session is not resumed.** The author turn resumes one
engine session across iterations, keyed to the agent alias
(`agentic.rs:10264`, `:10592-10595`; `crates/zoder-cli/src/main.rs:12511-12535`,
`:12590-12592`), and `zoder loop` documents that continuity as a feature
(`crates/zoder-cli/src/main.rs:1392-1395`). `agentic_turn` itself takes the
continuity choice as a parameter (`main.rs:12684-12691`). Whether a fresh
session costs prompt-cache reuse or extra tokens is **not established in this
tree**: the cache-stable prompt work here is for the *review* path
(`crates/zoder-cli/src/review_baton.rs:1-30`, `agentic.rs:1229-1233`), and the
engine's own session caching is outside this repository.

**The measurement the recommendation depends on.** The loop records one entry
per iteration carrying `"resolved"` (false until the run converges; the
top-level payload sets `"resolved": resolved` at `agentic.rs:11876`) and stops
after `--max-iters`, default 6 (`main.rs:1406-1408`), or after `STALL_LIMIT` = 3
consecutive no-progress turns (`agentic.rs:10360`, `:11771-11785`).
"Iterations to resolve" therefore already exists in the loop's JSON; no new
instrumentation is needed to define it, only a corpus run to collect it.

**Backlog text I could NOT verify.** I could not read
`refactor-research-20261031/BACKLOG-FROM-FABLE-20261107.md`; it is not in this
repository and I could not find it on this host. The D-ZD2 / ZD-05 text above
is transcribed from the task, not checked against the backlog. Also
unverified: (i) that `acp.slot_lease.enabled` is an agreed key name anywhere —
no `slot_lease` key exists in this tree, and the only `[acp]` key the engine
registry reads is `default_agent` (`crates/zoder-core/src/config.rs:734`,
`:830-833`); (ii) that any other backlog item depends on ZD-05; (iii) what
"per author turn" means at the daemon — the daemon's source is not in this
repository.

## 3. OPTIONS

### (a) Per job — keep today's behaviour, make it explicit

- **Benefit:** zero code. The cap is a property of the running process, which
  is what `ZODER_PROVIDER_CONCURRENCY` was built to bound
  (`docs/CONCURRENCY.md:211-222`). Session continuity across iterations is
  preserved as documented (`main.rs:1392-1395`).
- **Cost:** the cap does not move when a job stops reviewing. A job sitting in
  a long author turn, or a wedged one, still holds nothing *between* calls —
  but it re-takes a slot immediately on its next chunk, competing with every
  other job at the same priority.
- **Risk:** starvation is unfairness, not deadlock: the poll loop is FIFO-ish
  at best, and a large review that fans out 4 chunks takes 4 slots while a
  small one takes 1 (`agentic.rs:4966-4977`).
- **Reversibility:** n/a — this is the status quo.

### (b) Per author turn — free the cap between turns

- **Benefit:** a job's claim on the host's reviewer capacity follows its
  active work; a job that is thinking (author turn) or finished stops
  occupying capacity another job could use.
- **Cost:** one flag plus a rule for turning "one process" into "one turn".
  The difficulty is that **a slot is a file lock held by a file descriptor**,
  so the only ways to release it between turns are (i) re-open the slot files
  at the start of each review and close them at the end — which is already
  what happens, so the change is a no-op — or (ii) stop the review phase from
  sharing the job's capacity accounting at all, which is a different change.
- **Risk:** the option as written cannot deliver the promised continuity
  *and* the release: dropping and re-opening the lock ends exactly the state
  that would carry over, and the loop already re-acquires per call. Implemented
  literally it buys nothing measurable and adds a configuration flag.
- **Reversibility:** high — one flag, default off.

### (c) Per turn, but at the daemon session (a real lease with an owner)

- **Benefit:** only option that can address a job that stops *replying*
  rather than stops *reviewing* — a lease with a TTL and an owner id can
  expire. It is also the only shape that would make `acp.slot_lease` mean
  anything.
- **Cost:** new state (owner, TTL, expiry sweeper) in the client or the
  daemon, and the daemon's source is outside this repository.
- **Risk:** a TTL cheaper than the work it protects evicts a healthy holder
  mid-call; the existing wait notice and queue timeout
  (`provider_slots.rs:139-166`) already give an operator a bound. It also
  duplicates the process-death release the file lock gives for free
  (`provider_slots.rs:1-25`).
- **Reversibility:** medium — removing lease state is harder than setting a
  flag to false.

## 4. RECOMMENDATION

**Take (a) now, and refuse to ship the flag until (b) can be shown to change a
number.** The verified problem — a job holding capacity it is not using — does
not exist in this code: slots are taken and released inside a single provider
call (`agentic.rs:2372` guard → `agentic.rs:2795` return), never across a turn.
A flag that "makes the lease per turn" would therefore be a flag that changes
nothing, and shipping configuration that does nothing is worse than shipping
none: it invites an operator to believe a knob works.

If the operator still wants the behaviour, the honest path is the one in §5:
measure iterations-to-resolve on the corpus first, and only then decide whether
anything needs to move. The task's own recommendation already says "measure
before making it the default"; this ADR says measure before writing it, because
the release half is already in place.

**The strongest argument against this:** it leaves the operator with a knob
they asked for and did not get, and if the real complaint is that review-phase
fan-out (`agentic.rs:5205-5207`, up to 4 concurrent chunks) crowds siblings, the
complaint is about **fan-out width**, not lease scope — and an ADR that answers
"no change" without proposing the width fix is answering a different question
than the one that was asked.

## 5. IMPLEMENTATION PLAN

No step here touches production code; this is what an implementing ADR would
execute.

1. **Confirm the key name and the owner.** Decide whether
   `acp.slot_lease.enabled` is really the config key (nothing in this tree
   defines it) and whether it belongs to the daemon's `[acp]` section or a
   zoder-side `[review]` section. Stop here until §6 Q1 is answered.
2. **Instrument the baseline first, under today's behaviour.** Run the loop
   over the corpus and record, per run, the iteration count and whether it
   resolved — both already in the loop JSON (`agentic.rs:11876`) — plus
   the existing `slot_wait_ms` / `slot_held` receipts (`agentic.rs:2784-2790`).
   Define "iterations to resolve" as the run's `iterations` count when
   `resolved` is true, and "unresolved" (no number) when it is false.
3. **Write the flag as a no-op only if step 1 says the daemon owns it.**
   If the lease is a daemon concept, the flag belongs there and this ADR is
   blocked on that repository. If it is a zoder concept, the implementing ADR
   must first name the state that survives a turn — the slot files do not
   (`provider_slots.rs:87-105`).
4. **Do not move the concurrency defaults.** `provider_concurrency_default` is
   0 on both profiles (`config.rs:2484-2488`) and the per-provider
   `max_concurrent` defaults to 0 (`config.rs:4336-4344`); making per-turn
   leasing the default before the corpus number exists would change behaviour
   on a host that has never set a cap.
5. **Review fan-out is the real lever if the complaint is crowding.** The
   bounded fan-out is one constant (`DEFAULT_CHUNK_CONCURRENCY`,
   `agentic.rs:4977`) and one env override (`agentic.rs:4963-4973`). Any change
   there should be its own ADR.
6. **Rollout.** Flag default off; enable per host; compare the corpus
   iteration count against the step-2 baseline for a week.
7. **Rollback.** Flip the flag off (one setting). If the flag was written as a
   no-op, rollback is also a no-op — which is the point of deciding first.
8. **Backlog items unblocked:** ZD-05, by the id in the task text.
   **Not verified:** I could not read the backlog file on this host, so I can
   name no other item this releases and cannot check ZD-05's own members.

**Tests** (each is a documentation-level invariant until code exists; if code
is written, each must fail before and pass after):

- `unlimited_provider_takes_no_slot_and_reports_not_held` — already pinned
  (`provider_slots.rs:183-192`); it is the guard that makes "leased per turn"
  a no-op when no cap is configured.
- `a_slot_is_released_when_the_dispatch_returns` — the guard's lifetime is the
  dispatch's (`agentic.rs:2372` → `agentic.rs:2795`); a test that re-acquires
  immediately after a dispatch in the same process must succeed.
- `iteration_count_is_recorded_per_run_and_sums_unresolved_plus_one` — pins
  the measurement definition in step 2: a run reports `iterations` as
  `iterations.len()` (`agentic.rs:11877`) with `resolved` from the run
  (`agentic.rs:11876`), so a resolved 4-iteration run reads 4 and a stalled one
  reads its cap.
- If the flag is implemented: `slot_lease_disabled_keeps_one_holder_per_process`
  and `slot_lease_enabled_releases_between_calls` — the second must assert on
  observable slot state, not on a log line.

## 6. OPEN QUESTIONS FOR THE OPERATOR

1. Is `acp.slot_lease.enabled` a real agreed key, and does it configure the
   daemon or zoder? Nothing in this tree defines it (§2).
2. What problem is ZD-05 trying to fix: jobs queuing behind each other on a
   capped host, or something about the author session itself? The verified code
   already releases between calls, so the answer decides whether this is a
   capacity question (option a/c) or a session question (option b/c).
3. If the goal is fairness on a capped host, is the acceptable fix to lower
   `ZODER_REVIEW_CHUNK_CONCURRENCY` (`agentic.rs:4977`) rather than to change
   lease scope? That is the setting that decides how many slots one job holds
   at once.
4. Who owns the corpus run, and what is the success threshold for
   iterations-to-resolve? No number exists in this tree today.
5. Does the daemon keep a session's prompt cache across a reconnect? If it
   does not, "loses session continuity" costs less than the backlog text
   implies, and the whole option is cheaper. I could not verify this: the
   daemon is not in this repository.

## 7. STATUS

**Proposed.** No production code changed by this ADR. It needs answers to §6
Q1 and Q2 before an implementation ADR is written.
