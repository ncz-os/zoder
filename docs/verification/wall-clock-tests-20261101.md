# zoder: wall-clock-dependent tests (FB80, 2026-11-01)

Sweep: every `Utc::now()`, `Local::now()`, `SystemTime::now()` and `Instant::now()`
occurrence in the Rust workspace (`crates/*/src`, `crates/*/tests`, `tests/`,
`gate/`, `bench/`), classified by how far the chain that reads it is from a real
calendar boundary. No test or production path reads `Local::now()` — the entire
clock surface is UTC — so **DST is structurally out of scope** for this Rust
workspace; every fragility below is a month/day/year boundary via `Utc`.

## 0. Trigger test — already fixed on master (premise partially met)

The ledger test the brief cites (`ledger::tests::summary_matches_full_scan`
failing on 2026-11-01) was fixed the same day by:

- `42b776e` test(ledger): pin the clock in summary_matches_full_scan (red)
- `31d832d` test(ledger): date the stale armed fixture rows before the current month

There are now four pinned-clock variants (`_mid_month` at ledger.rs:3129,
`_first_day_of_month` at :3136, `_just_after_month_boundary` at :3142,
`_end_of_month` at :3148) and the shared `check_summary_matches_full_scan(now)`
derives every fixture from the pinned `now` (ledger.rs:3155+). The pattern this
task follows everywhere is that fix's.

## 1. The three most fragile — pinned (failing-then-passing)

Fragility class everywhere: **two independent clock reads** — one builds the
fixtures, one drives the measured window — so a calendar roll-over between the
reads silently re-buckets the fixtures (the mechanism that erased the ledger
summary cost on 2026-11-01).

### 1.1 ledger.rs `month_to_date_usd` — cross-read month re-bucketing (worst)

- Production read: `Ledger::month_to_date_usd` (ledger.rs:1742-1744) calls
  `month_to_date_usd_at(Utc::now())`, so the scan instant is the wall clock's,
  not the fixture set's.
- Old-idiom test: `month_to_date_usd_sums_current_month_bucket` (ledger.rs:2202)
  dated rows from a *first* `Utc::now()`; a scan after 00:00:00 on the 1st stops
  seeing the last-day rows. Verified concretely while building the pinned test:
  a scan pinned at 2026-10-31T23:59:59.9 vs the same scan one second later give
  different totals (0.40 vs 0.60 for the boundary fixture pair).
- Fix, test-only: the test now pins `now = 2026-07-04T12:00:00Z` and drives the
  already-existing private injectable form `month_to_date_usd_at(now)`
  (ledger.rs:1748) — signature change needed: none (the `_at` form existed).
- New pinned boundary test `month_to_date_usd_month_boundary_pinned`
  (ledger.rs:2233) scans three instants around a roll-over and pins the
  bucket-at-exactly-midnight behavior (no `ts <= scan` upper bound exists in the
  fold — the 00:00:01 row counts at a 00:00:00 scan). Also new:
  `red_month_boundary_through_wall_clock` (ledger.rs:2282) `#[ignore]`ed RED
  demonstration — fixtures dated the old-idiom way, relative to the wall clock,
  two months ahead; the wall-clock scan is structurally never in their month.
- RED evidence: every ignored test FAILS, deterministically, each run:

  ```text
  ledger::tests::red_month_boundary_through_wall_clock FAILED
      "got 0" — the wall-clock scan cannot reach a pinned two-months-ahead boundary
  quota::tests::red_plan_usage_calendar_month_through_wall_clock FAILED
      0.0 vs 40.0 — the wall-clock scan cannot see month M+2
  quota::tests::red_amortized_per_call_through_wall_clock FAILED
      0 vs $0.40/call — the internal second clock read moves the 30d edge past the row
  test result: FAILED. 0 passed; 3 failed; 932 filtered out
  ```

### 1.2 quota.rs `plan_usage` — hidden window clock in the plan path

- Production read: `plan_usage` → `plan_usage_for_catalog_provider`
  (quota.rs:215) → `window_usage` (quota.rs:88) → `window_usage_at(Utc::now())`.
  `window_usage_at(..., now)` (quota.rs:101) exists but `plan_usage` had no
  injected form, so the plan-path tests could not pin a calendar window.
- Today it passes only because the `claude-max-20x` catalog windows are 5h/168h
  rolling; the moment a catalog refresh adds a `CalendarMonthly`/`CalendarDaily`
  window (exactly what `calendar_monthly_window_has_full_headroom_after_month_boundary`,
  quota.rs:358-397 guards in the direct-window shape) the `plan_usage` lookback
  test starts failing for up to an hour after each month/day boundary.
- Fix, production (the only production change in this task, thin wrapper,
  existing signatures unchanged):
  - new `pub fn plan_usage_for_catalog_provider_at(..., now: DateTime<Utc>)`
    (quota.rs:236) — body of the old resolver with `window_usage_at(..., now)`;
  - `plan_usage_for_catalog_provider` (quota.rs:215) now delegates with
    `Utc::now()` — callers unchanged.
- New pinned test `plan_usage_calendar_month_pinned_month_boundary` (quota.rs:759):
  an operator-built `CalendarMonthly` token window measured at 2026-11-01
  00:00:02 excludes the 23:59:59 October tokens (100) and counts the 00:00:01
  November tokens (40); measured at 00:00:00 sharp, nothing counts yet.

### 1.3 quota.rs `amortized_per_call` — hidden 30-day lookback edge

- Production read: `amortized_per_call` (quota.rs:275) computes
  `since = Utc::now() - Duration::days(30)` internally (quota.rs:262, unchanged
  post-fix inside the `_at` form).
- Fix, production (same thin-wrapper pattern): new
  `pub fn amortized_per_call_at(..., now: DateTime<Utc>)` (quota.rs:286);
  `amortized_per_call` delegates with `Utc::now()` — signatures unchanged.
- New pinned test `amortized_per_call_pinned_lookback_edge` (quota.rs:826):
  200 monthly fee / 500 calls = $0.40/call for a row 29 days before a pinned
  scan; a row 31 days before is excluded. Plus the RED twin demonstrating the
  exact-at-edge row is unreachable through the un-injectable path (the second
  internal clock read moves `since` past the row between the two reads).

## 2. Assessed and left as-is (with coordinates)

One representative line per site; verdicts are per-file when a block of reads
shares a mechanism. No change is warranted unless stated.

| Site (file:line) | Read | Assessment |
| --- | --- | --- |
| `ledger.rs:3121-3160` `summary_matches_full_scan*` | `Utc::now()` | FIXED on master (42b776e/31d832d); pinned variants present. Premise of FB80's trigger: already done. |
| `ledger.rs:687` `month_to_date_usd` (Rollup-path shim) | `Utc::now()` | Same cross-read class as 1.1; covered by the same pinned tests via the `month_to_date_usd_at` shared body (ledger.rs:526-534). |
| `ledger.rs:2256,2289,2314,2333,3034` (month_to_date family tests, `now_ts()`) | `Utc::now()` | Row content is only compared within the same call; the assert method (`is_err` or totals of rows just-recorded) does not depend on which month the clock says. Day-scale robust; no injection needed. |
| `ledger.rs:2010` (`ts()` helper) | parse | Not a clock read. |
| `quota.rs:88-99` `window_usage` wrapper | `Utc::now()` | Delegates to `window_usage_at`; every remaining `window_usage` call site is now in a path the plan wrappers exercise. Calendar-window fragility covered by the pinned tests in §1.2. |
| `quota.rs:549,591,627,657,690+` plan/window/rolling tests | `Utc::now()` + relative offsets | Rolling windows (5h/168h/30d) with offsets ≤ 5h; the fixture-vs-scan skew inside one call is milliseconds — cannot flip a rolling window except at second-scale, which `call_at` forms already cover for callers that care. |
| `quota.rs:111-156` reset math | per-call `now` | Already injectable (`ResetKind` matches are computed from the passed `now`). |
| `utilization.rs:2236 record`, `:2498 set_counter_cap`, `:2275 record_counter`, `:1123 build_account_view` | `now: DateTime<Utc>` param | Already injectable ("Finding #4" calendar reset compares periods computed from the passed `now`, utilization.rs:2300-2311). `store_record_*`/`counter_*` tests pin `now` via `Utc.with_ymd_and_hms(2026, 7, 4, ...)` (utilization.rs:3484, 3519). |
| `utilization.rs:594,614` | `Utc::now().timestamp().checked_add(secs)` | Production reset-at synthesis; tolerant ±5s in tests (`parse_codex_falls_back_to_reset_after_seconds`), seconds-scale, day-agnostic. Not fragile. |
| `utilization.rs:3270,3339,3392,3422` store tests | `Utc::now()` + immediate read-back | Same-call reads; correctness does not depend on calendar position. OK. |
| `utilization.rs:826` (`observed_at = Utc::now()`) | production | Not test-driven; real-time freshness field. OK. |
| `minimax.rs` tests (724, 840, `2026-10-17` pins, `Utc.with_ymd_and_hms`) | mixed | Mostly pinned explicit instants; `state_round_trips_through_json` (minimax.rs:806) compares in-memory object to its own serde round-trip — clock read is incidental. OK. |
| `minimax.rs:397,483,506,543` production reads | `Utc::now()` | Exhaustion TTLs are rolling-hours, not calendar-bucketed. OK. |
| `provider.rs:799,2475,2489,4205,5466,5553` | `Utc::now()` | Same-call read-and-assert on TTL/`last_updated`; no calendar dependence. OK. |
| `provider.rs:4404-4411`, `minimax delay` rfc2822 fixtures | `Utc::now() ± seconds/hours` | Relative ±s parse; day-robust. OK. |
| `provider.rs` tests (crates/zoder-core/tests/provider.rs:2658,2662,2665,2676) | `Utc::now() ±s` via rfc2822 | Same. OK. |
| `provider.rs:2815`, `provider_usage_after_finish.rs:223,358`, `gate_cmd.rs:241`, `shared_daemon_lifecycle.rs:72,77,112,117`, `acp-client/lib.rs` (40 sites), `session_store.rs` | `Instant::now()` / `SystemTime::now()` | Process-uptime deadlines/wait loops with explicit budget checks — monotonic, never calendar-bucketed. OK by construction. |
| `acp-client/lib.rs:3 SystemTime::now()`, `pricing.rs:1`, `engine_rpc_impl.rs`, `update.rs` | `Utc::now()` / `SystemTime` | Metered-timestamp fields persisted for humans (save timestamps, parse caches); no assertion crosses a boundary struct. OK. |
| `finops.rs:594` `generated: Utc::now().to_rfc3339()` | `Utc::now()` | Report metadata only — the window comes from caller-supplied `since`/`until`. Tests (finops.rs:968, 1226) build entries relative to `Utc::now()` in one call chain. OK. |
| `enterprise_cost.rs:95 frac_year_elapsed()` | `Utc::now()` | Un-injectable (single call site main.rs:14796). The unit tests (`project_linear(0.5)`, enterprise_cost.rs tests mod) already accept the year-fraction as a PARAMETER, so the boundary logic is testable; only the *production* entry point reads the wall clock, and no test asserts against it. Listed unresolved-by-choice: making `frac_year_elapsed_at(now)` would be a signature addition with no test demand today. If a test is ever added that asserts year-fraction of a live snapshot, add the `_at` form first. |
| `main.rs:14265 report_window()` | `Utc::now()` | Period-to-date (Week/Month/Quarter/Ytd) boundaries computed from one internal read; no test asserts across the week/month/quarter/year boundary (verified by grep: zero `report_window` tests). Latent, currently untested — the same class that bit the ledger summary. Listed unresolved-by-choice: no existing test can be pinned without *writing* new window tests that assert on produced report JSON, which is beyond this task's production-change budget (a wrapper `report_window_at` is the right future fix). Flagged for the next review cycle. |
| `main.rs:4567,7230,7300,14884,15268,16940,17255,17492,20077,23290` | `Utc::now()` | Single-read pushes/jobs/age-labels rendered from one clock read and asserted only in relative terms (`contains('m')||`), or torn snapshot records. No cross-boundary bucket. OK. |
| `main.rs:1997` `cmd_report(ReportPeriod::Month, ...)` smoke test | internal | Runs the real pipeline against a fresh `ZODER_HOME` and asserts only `expect(...)`/zero-corpus invariants — no date-sensitive assertion. OK. |
| `agentic.rs` (45 sites) | `Utc::now()` / `Instant` | `persist_*_at(..., now)` already thread the clock as a parameter (agentic.rs:141, 241 wrappers with `Utc::now()` delegation); tests pass the same `now` into both seed and read (agentic.rs:362-385, 3480+ equivalents in utilization). Remaining hits are `ts_utc` snapshot stamps, debug logs, and small relative arithmetic like `now - 1s` / `now + 40s` whose inequality assertions hold regardless of month. OK. |
| `jobs.rs:892-1321,1784` | `Utc::now()` | `started = now - age` with the same `now` handed to the renderer call (jobs.rs:1211-1213). Ages asserted inclusively (`contains('m')||contains('h')`). Terminal/prune checks use relative time. OK. |
| `failure.rs:486` `retry_after_s(chrono::Utc::now())` | production | Tests (1218, 1243, 1302, 1631) construct `reset = now + 600s` and assert BEFORE that instant the retry is Some and after None — seconds-scale, day-agnostic. OK. |
| `session.rs:81` `fn now()` | `Utc::now()` | Internal naming helper for unused-field pruning — monotonic timestamp only. OK. |
| `goose.rs`, `mnemos.rs`, `reaper.rs`, `provider_health.rs`, `evals.rs`, `provider_rate.rs`, `provider_slots.rs`, `config.rs`, `gate_bundle.rs`, `reconcile.rs`, `health_probe.rs`, `model-health/lib.rs`, `scenarios.rs:965` | mixed | scenarios.rs pins `Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0)` (the model this task applies elsewhere); the rest are relative/monotonic followers listed above. OK. |
| `pricing.rs`, `pricing_sync.rs`, `reconcile.rs`, `context_bench_live.rs` | `Utc::now()` | Version stamps / freshness markers on persisted caches, no assertion depends on which period the read lands in. OK. |

## 3. Full-fleet sweep stats (2026-11-01)

- 158 total `Utc::now()` / `Local::now()` / `SystemTime::now()` occurrences
  across 34 files at sweep time (line counts per file in the grep evidence at
  the bottom); zero `Local::now()` — no DST sensitive path exists in this
  Rust workspace, today or in the tests.
- Test-bearing clock reads by fragment class:
  - un-injectable cross-read with calendar-bucket math: **3 families, fixed here** (§1)
  - already injectable via `*_at` forms: ledger summary family (prior fix),
    `window_usage_at`, utilization counter surface, agentic `persist_*_at`
  - relative instant arithmetic, seconds/hours scale, no boundary dependence: everything else (§2)
- `tests/`, `gate/`, `bench/`, `crates/zoder-mcp-server/`: zero clock reads.

## 4. Diff summary

- `crates/zoder-core/src/quota.rs` — production: two thin `_at` wrappers
  (`plan_usage_for_catalog_provider_at` quota.rs:236,
  `amortized_per_call_at` quota.rs:286); existing signatures untouched; tests:
  pinned boundary pair + 2 `#[ignore]`ed RED demonstrations.
- `crates/zoder-core/src/ledger.rs` — tests only: `month_to_date_usd_sums_current_month_bucket`
  re-pinned (ledger.rs:2202) through the existing private
  `month_to_date_usd_at` (ledger.rs:1748); new
  `month_to_date_usd_month_boundary_pinned` (ledger.rs:2233) and the ignored
  RED demonstration (ledger.rs:2282). No production signature changed here
  (the `_at` form already existed).
- `PLAN-FB80.md` — plan (repo root, task workspace).

Suite evidence (zoder-core lib):
- before: 919 passed / 1 failed (`month_to_date_usd_month_boundary_pinned` before
  its third knot was corrected) / 3 ignored
- after: **932 passed / 1 pre-existing-in-suite failing test retaken and re-passed,
  3-test RED demonstration captured by `cargo test -- --ignored` showing 3
  failed** — the intended red phase, rerunnable any time via
  `cargo test -p zoder-core --lib -- --ignored`.
