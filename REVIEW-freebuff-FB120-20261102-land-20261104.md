# Review + land — freebuff/FB120-20261102 (F5.4 fallback telemetry attribution)

- **Branch:** freebuff/FB120-20261102
- **Tip reviewed (baseline):** 07102100cfc6de2dc35928ddf8cde8cbe2be0e1f
- **Target:** master
- **Date:** 2026-11-04
- **Mode:** review-and-land — the change is reviewed, verified and landed on the
  same head branch. Nothing is merged into `master` here; the merge pass merges.

## What the branch claims (PLAN-FB120.md, commit 07102100)

1. **F5.4 (real):** per-turn quota/utilization telemetry was persisted against the
   chain HEAD provider even when a cross-provider FALLBACK answered the turn.
   Two call sites in `agentic_turn` are affected —
   `persist_agentic_utilization(&routed_provider, ..)` in the dispatch event
   closure and `persist_agentic_counter(&routed_provider, ..)` after the loop.
2. **F1r (premise false):** the fresh-create retry error is already preserved; no
   change made.
3. The change adds a `serving_provider(link, head)` helper, locks the winner's
   provider in via `used_provider`, and adds unit + store tests.

## Diff reviewed

```
PLAN-FB120.md                   |  54 +++++
crates/zoder-cli/src/agentic.rs |  31 ++
crates/zoder-cli/src/main.rs    | 127 ++++++++++++++++++++++++++++++++++++----
```

## Verification against source (not taken from the commit message)

- **F5.4 call sites.** `crates/zoder-cli/src/main.rs` now routes both telemetry
  call sites through `serving_provider`: the `AgentEvent::Utilization` arm passes
  `link_provider.as_ref()` (the candidate being dispatched) and the post-loop
  counter call passes `used_provider.as_ref()`. No other call site persists
  utilization or counter data; `grep -n 'persist_agentic_(utilization|counter)'`
  finds only these two in non-test code.
- **The remaining `routed_provider` uses are correct.** The ledger `Entry` and the
  health record both resolve `real_provider_for_model(&cfg, &model_used)` — i.e.
  the provider of the model that actually ran — and fall back to
  `routed_provider` only when that resolution returns `None`. They already
  followed the winner.
- **Head behaviour is unchanged.** `routed_provider` is resolved by
  `provider_for_resolved_model(&routing, &cfg, &primary, agent_provider_ref)`; at
  `idx == 0` `link_provider` is resolved by the *same* call with the same
  `agent_provider_ref`, so `serving_provider(link, head)` returns the head's own
  provider when the head answers.
- **`used_provider` is always set at the counter call.** The dispatch `loop` only
  `break r`s from the `Ok(run)` arm, which sets `used_provider = link_provider.clone()`
  immediately before the break. Every `Err` path either `return`s (before the
  counter statement) or `continue`s. The `unwrap_or(&routed_provider)` is therefore
  defensive only, never a misattribution path.
- **F1r.** `crates/acp-client/src/lib.rs` keeps the retry error in both `drive`
  (line 3077) and `drive_goose_io` (line 5018), pinned by the tests around line
  9294. No change was needed; the plan's "no code change" is accurate.

## Tests

- **Full workspace, before the review (`cargo test --workspace --locked`):**
  2486 passed, 0 failed, 2 ignored.
- The three `serving_provider_tests` in `main.rs` pass and pin the helper's
  selection rule (fallback wins over head; `None` keeps the head; head link
  selects the head).
- `agentic::agentic_utilization_tests::fallback_turn_tokens_accrue_only_to_answering_provider_bucket`
  passes and pins the store end-state (fallback bucket accrues 90 tokens; the
  head bucket is never created). It exercises the store helper, not
  `agentic_turn` itself — see "Unverified" below.
- `cargo fmt --check` reports differences only in pre-existing code
  (`acp-client/src/lib.rs`, and an unrelated block in `agentic.rs` at
  `CODE_LOOKALIKE_PREFIXES`); the hunks this branch adds are fmt-clean. That
  formatting drift predates the branch (`git show HEAD~1:...agentic.rs` shows the
  same diff), so it is left untouched.

## Structured review

Two independent non-author models reviewed the diff under a JSON contract:

- **tydeus-reviewer (gemma-4-26B-A4B)** — verdict **APPROVE**, no findings.
- **cerberus-reviewer (gpt-oss-20b)** — verdict **APPROVE**, no findings, after
  being asked to attack four specific risks (other misattributed call sites, head
  behaviour change, test strength, `used_provider` reachability). Each of the four
  was re-checked against source above; all hold.

No finding was raised, so nothing was changed to satisfy a reviewer.

## Unverified / caveats

- **No hermetic end-to-end test of `agentic_turn`.** Exercising the real wiring
  needs a live daemon binary; the store-level test plus the helper unit tests are
  the strongest checks available here. This was stated in the plan and is repeated
  here rather than papered over.
- **`AgenticCostScope::start` is still keyed by the head alias.** On a fallback to
  a different alias this can only cause a false local overlap, which fails safe to
  `cost_unknown` — never to wrong spend. Deliberately left unchanged (restructuring
  per-candidate scopes risks the overlap semantics); documented in the stale
  comment the branch already rewrites.
- **The branch is far behind `origin/master`** and the histories have diverged
  (the branch base `662ee2d8` is not an ancestor of master; a merge would produce
  ~2991 conflicts against a rewritten lineage). Rebasing this branch onto master
  is not a landing-pass action and the task reserves merging for the merge pass;
  the change is landed on the same head branch as instructed. The defect is still
  present on current master (`serving_provider` absent; both call sites still pass
  `&routed_provider`), so this branch is **not** superseded.

## Verdict

Worth landing. The fix is real, minimal, correct, and tested at the levels the
code allows. F1r correctly needs no change.
