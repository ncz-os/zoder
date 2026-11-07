# ADR D-ZD6 — Exit-60 envelope for Hive (ZD-19)

Status: **Proposed** · Scope: zoder failure contract + Hive job runner · Owner: Jason Perlow
For: the operator. Read time: five minutes.

## 1. DECISION NEEDED

When a zoder run cannot get a daemon session, it now exits `60` and says
`infrastructure:session_limit` in its JSON, but the Hive job runner never
reads that number — it fans `60` out to the oldest host with a free slot,
a full daemon, again. We need to decide now whether the `60` envelope becomes
the runner's stable contract, so that a capacity refusal stops costing the
job a retry and stops looking like a rejected review. If we do nothing, the
runner keeps counting every capacity wall against `retry_count` and keeps
queueing review work for changes nobody authored, until the job dead-letters.

## 2. CONTEXT (verified against this repository)

**zoder already emits the envelope.** `FailureCode::SessionLimit` maps to
`FailureClass::Infrastructure`, whose exit code is `EXIT_INFRASTRUCTURE = 60`
(`crates/zoder-cli/src/failure.rs:172`; class table `failure.rs:126-130`,
code table `failure.rs:279-281`). The `--json` failure object is
`{code, class, detail{message}}` (`failure.rs:474-509`) and the wrapper is
`{"ok": false, "error": {..}}` (`failure.rs:610-612`). In the `zoder loop`
payload the same object travels as `failure` *next to* the loop fields, plus a
one-string handle `failure_class: "infrastructure:session_limit"`
(`crates/zoder-cli/src/agentic.rs:11913-11922`).

**The exit code and the JSON cannot disagree.** `main` derives the process
code from the same classified failure: `contract_exit_code` →
`classify_error(&e)` → `classified.exit_code()`, else `50`
(`crates/zoder-cli/src/main.rs:14134-14140`; used at `main.rs:2748-2755`).
A missing number is therefore not a risk: the JSON is authoritative, `60` is
its convenience rung.

**The contract is scoped to two commands.** `failure::begin_contract_run` is
called with `in_scope` true only for `Cmd::Review` and `Cmd::Loop`
(`main.rs:2764-2767`; `failure.rs:672-679`). Every other command still exits
`78` for a budget decline and `1` otherwise (`main.rs:14117-14121`). The
runner must not read `60` from anything but `review`/`loop`.

**`60` means "nothing ran", and only that.** The retry only applies to
`session/new`, and only when the reply carries both `-32001` and the text
"session limit", because the daemon reuses `-32001` for approval denials
(`crates/acp-client/src/lib.rs:5861-5885`). The budget is 6 tries, 2 s
doubling to a 30 s cap plus 0-25% jitter (`lib.rs:5895-5932`), so by the time
`60` is emitted the client has already waited ~60 s. On that failure the chain
records no breaker strike and does not fall back to another model
(`main.rs:13475-13490`), and the loop records the refusal, skips
check/review/commit, and leaves the worktree untouched
(`agentic.rs:10691-10704`, `:10825-10829`). So the change under review was
never authored: nothing is owed a review.

**The documentation contract already says all of this.**
`docs/EXIT_CODES.md` freezes `session_limit | infrastructure | 60` (line 81),
declares `infrastructure` NOT retryable precisely because the fast retries are
already spent (lines 51 and 54-60), gives the loop payload example with
`failure_class: "infrastructure:session_limit"` (lines 179-190), and ends with
a worker `case "$rc"` example whose `60` branch says "cool down, do not charge
the retry budget" (line 323).

**The Hive runner reads none of it.** It is `/opt/graeae/hive_worker.py` on
the job host — outside this repository, so that file is evidence, not source
we own. Verified there: `verify_delivery` (`hive_worker.py:6516`) returns
delivery evidence only; `run_job` prints `exit={exit_code}`
(`hive_worker.py:~10512`) but chooses the disposition through
`resolve_delivery_status` (`hive_worker.py:5813-5854`), which switches on
"exit 0 or not" and never on `60`; and the only free re-queue path is
`release_without_retry` (`hive_worker.py:4764-4790`), today wired to exactly
one condition, the engine spend cap (`hive_worker.py:10539-10560`). A search
for `60` in that file finds no exit-code branch.

**What I could NOT verify.** (a) The backlog text itself:
`refactor-research-20261031/BACKLOG-FROM-FABLE-20261107.md` does not exist in
this tree, on `origin/master`, or on any local branch, so the ZD-19 wording
and its item ids are unreadable from here — items below are named, not
numbered. (b) "Confirm the Hive runner requeues on 60 without spending an
attempt": the runner does **not** do this today (see above); a claim that it
already does is false against `hive_worker.py`. (c) Whether the bus charges
`retry_count` on a `claimed -> queued` release: the function's own docstring
asserts it does (`hive_worker.py:4770-4787`) and says only the admin requeue
zeroes the counter; I could not read the bus source, so that is reported, not
verified. (d) `DECLINE_REASON_PREFIXES` is referenced at
`hive_worker.py:4785` but defined in the bus, which is not on this host.
(e) **`requeue_without_review` does not exist in this repository's source
yet.** It is implemented only on the unmerged branch
`origin/feat/slot-lease-20261103` (commit `de789c33`, plus
`SlotQueueTimeout` and `FailurePhase`/`Attribution` in
`crates/zoder-cli/src/failure.rs` and its tests). On this HEAD the envelope
has no such key.

## 3. OPTIONS

**A. Adopt the envelope as-is; runner treats `60` as "requeue, no review".**
- Benefit: zero production code; the contract is already frozen and tested;
  a full daemon stops burning `retry_count`; no reviewer is asked to read a
  diff that does not exist.
- Cost: the runner change is in `/opt/graeae`, reviewed and deployed
  separately from this repo.
- Risk: if the runner keys on the exit code alone, a future code that reuses
  `60` for a non-capacity cause would be requeued silently.
- Reversibility: high — revert the runner's condition; zoder is untouched.

**B. Add the additive fields (`requeue_without_review`, `phase`,
`attribution`) to zoder first, then change the runner.**
- Benefit: the runner keys on a boolean written by the code that knows the
  phase, not on a number; the change is additive so every existing consumer
  is unaffected; it also covers `slot_queue_timeout`.
- Cost: a real code change (docs-only job cannot land it) plus tests; two
  releases to coordinate.
- Risk: it lands `feat/slot-lease-20261103`'s scope, which is unmerged and
  carries far more than this decision.
- Reversibility: high — extra keys are ignored by old readers.

**C. Keep `60` fatal (do nothing).**
- Benefit: no coordination, no new surface.
- Cost: every capacity wall charges the job a retry, and `needs-review` rows
  accumulate for changes nobody wrote.
- Risk: jobs dead-letter on infrastructure noise; operators learn to ignore
  the queue.
- Reversibility: trivial, which is the problem — it is the status quo.

## 4. RECOMMENDATION

Take **A now**, and treat **B** as the follow-up that arrives with the slot
lease. Reason: the contract is already frozen, documented and tested on this
tree; what is missing is a consumer that reads it. Requiring a new zoder field
before the runner can behave correctly inverts the dependency, and makes the
fleet wait on an unmerged branch for a fix whose evidence is already shipped.
Key the runner on `error.class == "infrastructure"` (with
`error.code == "session_limit"` as the narrow first cut), never on `60` alone,
so the decision survives a future code joining the class.

**Strongest argument against:** the class is a string, and a runner that
parses model-visible JSON is coupled to prose — if `class` were ever renamed,
the runner silently falls back to today's failing behaviour, which a numeric
exit code would not do.

## 5. IMPLEMENTATION PLAN

Ordered steps (steps 1-2 are this repository, docs-only, in this ADR's
branch; steps 3-6 are the runner, a separate change):

1. **Freeze the handle in the docs.** Add the `infrastructure:session_limit`
   handle and the "do not charge the retry" sentence to the worker-facing
   section of `docs/EXIT_CODES.md` (already present at ~line 323; make it the
   named contract rather than an example). *(this ADR)*
2. **Record the decision** as Status `Proposed` here; the operator accepts.
3. **Runner: parse the envelope.** Read the last JSON document from the
   driver's stdout/stderr; if `failure.class == "infrastructure"` (or the
   loop payload's `failure_class` starts with `infrastructure:`), set
   `failure_reason = "infrastructure:<code>"` and route to
   `release_without_retry` instead of `patch_job(status)`.
4. **Runner: narrow first cut.** Accept only `session_limit`; widen to the
   whole class when `SlotQueueTimeout` lands.
5. **Runner: log one line** naming the host, the code and the reset/backoff,
   the way `budget_exceeded` already does (`hive_worker.py:10544-10554`).
6. **Runner: keep the no-review proof.** On this path, assert the evidence is
   empty (no commits above base). If a commit exists, `60` is not a capacity
   refusal: fall through to today's failure path. Fail closed.
7. **Follow-up (option B)**, when the slot lease merges: read
   `requeue_without_review: true` instead of the class string, and use
   `phase`/`attribution` in the job row.

Backlog items this unblocks (by name; ids unreadable, see §2):
"Add `requeue_without_review` and the attribution block" — this ADR decides
*yes, additive only, with `infrastructure:session_limit` as the stable
contract*; the slot-lease `slot_queue_timeout` work; and the "not_reviewed"
attribution item that stops a null-reviewer `reviewer_unavailable` from
reaching the review queue.

Tests (for the follow-up code change, not this docs job):
- `zoder`: the existing
  `session_limit_envelope_carries_infrastructure_class_and_exit_60`
  (`failure.rs:1551`) already pins code/class/`60`; add a pin that
  `error_envelope` contains no `requeue_without_review` key on this HEAD, so
  the runner cannot assume it.
- runner: `60` + empty evidence → `queued`; `60` + one commit → `failed`;
  `20` → `failed` as today; a non-JSON stdout → `failed` as today.

Rollout: runner change behind a host env flag (`HIVE_INFRA_ENVELOPE=1`),
observed on one host for a day, then default. zoder needs no rollout.

Rollback: set `HIVE_INFRA_ENVELOPE=0`; the runner returns to `exit 60 ->
failed`, exactly today's behaviour. No data migration: `retry_count` is only
compared before and after, never rewritten.

## 6. OPEN QUESTIONS FOR THE OPERATOR

1. Does the bus actually increment `retry_count` on `claimed -> queued`? If it
   does not, option A's main benefit shrinks to "no bogus review", and the
   urgency drops.
2. Is a `60` ever produced by a host whose daemon is *not* full — e.g. a
   misconfigured daemon that always answers -32001? If so, requeueing will
   walk the whole fleet before anyone looks.
3. Should the cooldown be per daemon socket (per host) or global? The runner
   already has a per-host state dir; `60` is a host fact, not a job fact.
4. Do we accept option B's scope (slot lease, `slot_queue_timeout`,
   `phase`/`attribution`) as one landing, or split it?
5. Who owns the `/opt/graeae` change — is it under this repo's review rules
   at all?

## 7. Status

**Proposed.** No production code was changed by this ADR. The recommendation
is option A now, option B with the slot lease. Acceptance needs answers to
question 1 and question 2; the rest can be settled during implementation.
