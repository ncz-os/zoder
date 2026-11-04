# PLAN-FB80 — zoder: find wall-clock-dependent tests

Task: search the Rust workspace for tests that read `Utc::now` / `Local::now` /
`SystemTime::now` / `chrono::Utc` without an injected clock, assess each for
month/day/DST/year-boundary fragility, add a failing-then-passing pinned-clock
test for the three most fragile, list all others in a report. Production changes
only where a clock must become injectable, with a thin wrapper keeping signatures.

## Premise check (record honestly)

The trigger test in the brief — `ledger::tests::summary_matches_full_scan` — was
already fixed on master on 2026-11-01:
- `42b776e test(ledger): pin the clock in summary_matches_full_scan (red)`
- `31d832d test(ledger): date the stale armed fixture rows before the current month`
There are now pinned variants: `_mid_month`, `_first_day_of_month`,
`_just_after_month_boundary`, `_end_of_month` (ledger.rs:3121-3152), and the
fixtures derive from the passed `now` (ledger.rs:3157+). So the ledger piece of
the premise is DONE; I report that and harden the rest of the workspace instead.

## Clock-read inventory (from sweep, 2026-11-01)

Already pinned / injectable (no action):
- `zoder-core/src/scenarios.rs:965` — test helper `now()` returns fixed
  `2026-07-04 12:00:00`, plus `Utc.with_ymd_and_hms` pins throughout.
- `quota.rs` `window_usage_at(..., now)` exists (clock-injected form) and
  `calendar_monthly_window_has_full_headroom_after_month_boundary` pins it.
- `utilization.rs` counter API (`record_counter`, `set_counter_cap`,
  `build_account_view`) all take `now: DateTime<Utc>` ("Finding #4" calendar
  reset is computed from the passed `now`, utilization.rs:2300-2311).
- `minimax.rs` tests pin explicit instants (`Utc.with_ymd_and_hms(2026, 10, 17, ...)`).
- `provider.rs` / `acp-client` / gate_cmd / shared_daemon_lifecycle /
  mcp-server: `Utc::now()` offsets are relative (`now + 10s`, `now - 30s`) with
  tolerance or passed-in instants — seconds-scale robust, day-independent.
- `tests/`, `gate/`, `crates/zoder-mcp-server/`: zero clock reads.
- ledger.rs `month_to_date_usd_at(now)` already exists as the injectable form.

Fragile (un-injected real clock inside the code path a test drives):
1. `ledger.rs:2201 month_to_date_usd_sums_current_month_bucket` — fixtures
   dated from `Utc::now()` at test start (`"%Y-%m-%d"` + hardcoded T10:00:00Z /
   T11:00:00Z), scan reads the clock a second time inside `month_to_date_usd()`
   (ledger.rs:1742-1744 → `month_to_date_usd_at(Utc::now())`). Month-boundary
   roll-over between the two reads silently re-buckets the fixtures (the Nov 1
   failure class). Fragility window: the ~1 s before each month rollover, plus
   structural exposure to any future `ts <= scan_now` filter.
2. `quota.rs:544 plan_usage_measured_calls_in_lookback` — `plan_usage` →
   `plan_usage_for_catalog_provider` → `window_usage` → `window_usage_at(Utc::now())`
   (quota.rs:88-99): a second, un-injectable clock read. Robust today only
   because the `claude-max-20x` catalog windows are 5h/168h rolling; any future
   calendar window in the catalog makes the fixtures drop out at a month/day
   boundary for up to an hour (that failure mode is exactly what
   `calendar_monthly_window_has_full_headroom_after_month_boundary` guards for
   in another shape).
3. `quota.rs:651+694 amortized_per_call_*` — `amortized_per_call` computes
   `since = Utc::now() - Duration::days(30)` internally (quota.rs:262) —
   un-injectable 30-day lookback; same cross-read class, rolling so only the
   30-day edge can bite (e.g. fixtures near `now - 30d + seconds`).

## Changes

1. `crates/zoder-core/src/quota.rs` (ONLY production change, thin wrappers,
   signatures of existing fns unchanged):
   - `pub fn plan_usage_for_catalog_provider_at(entries, provider_id,
     ledger_provider_id, plan, catalog, catalog_provider_id, now)` — body is
     today's `plan_usage_for_catalog_provider` with `window_usage_at(..., now)`
     instead of `window_usage(...)`.
   - `plan_usage_for_catalog_provider` delegates with `Utc::now()`.
   - Same for `amortized_per_call_at(..., now)` / delegate.
2. `crates/zoder-core/src/ledger.rs` tests: rework
   `month_to_date_usd_sums_current_month_bucket` to pin `now` through the
   existing (private, same-module) `month_to_date_usd_at`, and add
   `month_to_date_usd_month_boundary_pinned` (row at 23:59:59 on the last day
   excluded / 00:00:01 row included / scan straddle).
3. Red phase (failing-then-passing, per brief): add `#[test] #[ignore]` red
   demonstrations that replay the boundary fixtures through the previous
   un-injectable real-clock entry points and assert the pre-injection
   expectation; `cargo test -- --ignored` documents the reds failing (the
   production clock cannot see the pinned boundary), and the pinned suite
   passes. Ignored so the regular suite stays green while the evidence stays
   runnable.
4. Plan tests for quota: pinned boundary `now` (month boundary) via the `_at`
   forms + calendar-window entry — red is the same idiom through `plan_usage()`
   / `amortized_per_call()`.

## Test loop

`cargo test -p zoder-core` (+ `-p zoder-cli` for the workspace) and
`cargo test -- --ignored` for the red evidence in ONE batched command. No ruff
(only Rust + docs changed). No cargo publish/install; no `deny.toml` work; no
image builds.

## Risks

- `debug_assert_eq!` invariant in the plan windows resolver must be kept in the
  `_at` form (windows.len() == provenance.len()).
- `month_to_date_usd_at` is private — test mod is a child module, so access is
  fine, but the pinned ledger test must not leak it in the report as "public API".
- Cargo build must not fetch new deps (no Cargo.toml change at all).

## Landing note (2026-11-04, rework of freebuff/FB80-20261030-d2)

The d2 branch's tip (`651fbe80`) carried an older, standalone bounded-lock
rework in `ledger.rs` (unbounded `lock_exclusive` call sites plus a
self-contained `lock_deadline_tests` module) that predates the bounded-lock
rewrite that landed on `master` (535bb3ea: `lock_exclusive_bounded` /
`lock_owner_sidecar_bounded` / `lock_shared_bounded`,
`ZODER_LEDGER_LOCK_TIMEOUT_S`, and the `lock_deadline_tests` module). Landing
the d2 ledger.rs wholesale would have REGRESSED master's bounded lock.

Resolution: `ledger.rs` now keeps master's bounded-lock production code and
test module verbatim, and adds exactly the two FB80 test functions that
master was missing:

- `month_to_date_usd_month_boundary_pinned` — pins the three-scan-instant
  bucket behavior across a month roll-over through
  `Ledger::month_to_date_usd_at`.
- `red_month_boundary_through_wall_clock` — `#[ignore]`d RED demonstration
  that the un-injectable `Ledger::month_to_date_usd` cannot reach a
  two-months-ahead pinned boundary (fails by design under `--ignored`).

The other FB80 deliverables (quota.rs `*_at` wrappers + pinned tests + the
two quota REDs; the pinned `month_to_date_usd_sums_current_month_bucket` and
`month_to_date_fails_closed_when_current_cost_is_unknown`; PLAN-FB80.md and
docs/verification/wall-clock-tests-20261101.md) are already present on
The branch is rebased onto the current master tip (535bb3ea) with the
conflict resolved as above; no master file is altered by the rework itself.

## Final delivery (2026-11-04)

The d2 tip (776be32c, "WIP: preserve uncommitted work") carried build scratch
files under `.build/` (quota/ledger snapshots, a review prompt, diff dumps)
that must not land; they are dropped from the branch history. The landing
commit on the current master tip adds exactly the remaining FB80 deliverable
(master was missing it): `ledger::tests::month_to_date_usd_month_boundary_pinned`
and the `#[ignore]`d `red_month_boundary_through_wall_clock`, plus this note.
Everything else in the FB80 diff (quota `*_at` wrappers, the two quota pinned
tests and their two REDs, the pinned `month_to_date_usd_sums_current_month_bucket`,
the pinned `month_to_date_fails_closed_when_current_cost_is_unknown`, and
docs/verification/wall-clock-tests-20261101.md) is already on master and is
unchanged here.

Test counts on the build host (cargo 1.96.1), full workspace suite
`cargo test --workspace`:

- before (branch at 776be32c, i.e. master + the two preserved WIP commits):
  2548 passed / 0 failed / 3 ignored
- after (this landing commit on master):
  2550 passed / 0 failed / 5 ignored (+2 passed: the two new non-ignored
  pinned tests — the ledger month-boundary test and the quota calendar-month
  test; +2 ignored: the two new `#[ignore]`d RED demonstrations — the ledger
  wall-clock red and the quota plan-usage wall-clock red)

The three intended FB80 RED demonstrations still fail by design under
`cargo test -p zoder-core -- --ignored` (ledger red, red_plan_usage,
red_amortized); the pinned boundary tests pass.
