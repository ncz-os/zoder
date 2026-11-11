# Refresh record — hv/zoder-mimo-usage-frame-20261105 → master 5a7fd588

**Date:** 2026-11-11
**Branch:** `hv/zoder-mimo-usage-frame-20261105`
**Target:** `origin/master` @ `5a7fd5884de6...` (fix(zoder): reaper exit-path
wording must not trip the failure classifier)
**Merge commit:** `094cc513`
**Merge parents:** `071c7e77` (branch) + `5a7fd588` (master)

## Why the refresh was needed

The branch had been refreshed onto `7e5e15c9` (see
`REFRESH-hv-zoder-mimo-usage-frame-20261110.md`) but master moved on since
through the exit-path wording work (`ec68c9ab`, `3f18d0bd`, `89132312`,
`0c53da90`, `452c5f58`, `5a7fd588`), so it was no longer a fast-forward.

## Conflicts

`git merge origin/master` produced **one conflict**, in
`crates/zoder-core/tests/provider.rs` (`choice_without_delta_is_decode_error`).
Both sides had already replaced the old
`malformed provider stream frame` assertion with the new
`no content delta` one — they diverged only in the explanatory comment:
the branch's comment (from its own refresh work) versus master's newer
comment (added by `452c5f58`, citing `25e1b10d`).

Resolution: kept **master's newer comment** (target's newer code — it cites
the exact commit that changed the behavior) with the **identical functional
assertion** (`assert!(error.message.contains("no content delta"))`), which is
what the branch's change required. No design decision was needed: the
assertions on both sides are byte-identical.

`crates/zoder-cli/src/agentic.rs` also appeared in both diffs but merged
**cleanly** — the branch's added F5.4 API-contract notes sit in
`agentic_utilization_tests` (around line 419), while master changed the
exit-path wording in the turn-completion path (around line 10999,
`cancel not confirmed` → `cancel not acknowledged`).

## Post-merge content

Against `origin/master`, the branch retains exactly its intended change set
(plus this record):

```
 crates/zoder-cli/src/agentic.rs                    | 18 +-
 crates/zoder-cli/src/main.rs                       | 14 +
 crates/zoder-core/src/utilization.rs               |  6 +
 crates/zoder-core/tests/provider_usage_after_finish.rs | 26 +
 docs/FAULT-TO-FIX.md                               |  1 +
```

Master's exit-path wording (`reaper.rs`, `failure.rs`, `docs/CANCEL-PROTOCOL.md`,
`agentic.rs` line ~10999, `provider.rs` comment/assertion) is present verbatim.
No new features, no unrelated files.

## Test evidence

Repository suite run as CI runs it (`cargo test --workspace --locked
--all-features --no-fail-fast`, cf. `.gitlab-ci.yml`), Rust 1.96.1 (pinned
toolchain), before and after this refresh. The new regression test
`mimo_usage_only_frame_after_content_is_delivered_and_recorded` is gated
behind `zoder-core`'s `integration-tests` feature, so `--all-features` is
used to make sure it actually executes:

| Tree | passed | failed | ignored |
|---|---|---|---|
| branch before merge @ `071c7e77` | 2757 | 0 | 7 |
| branch after merge @ `094cc513` | 2759 | 0 | 7 |

Failures do not grow versus the target: zero on both sides of the merge. The
post-merge head runs 2759 green tests — the 2757 of its first parent plus the
two new master tests (`the_exit_path_cleanup_wording_does_not_read_as_a_cancel_failure`
and the updated `choice_without_delta_is_decode_error` suite), with none lost
or broken.

## Delivery

Same head branch pushed to ARGONAS (`origin`,
`ssh://jasonperlow@192.168.207.101/mnt/datapool/git/zoder.git`) with upstream
tracking; remote SHA matches local HEAD.

## Post-refresh review round (fix round 1)

The refreshed head `6bd4d5a3` was reviewed read-only by two children of the
refresh job (`01a1c8fc`):

- `01a1c96a` (review ref
  `review/hv/zoder-mimo-usage-frame-20261105-01a1c96a-2e0d-7f65-8fee-17583b6c07ca`,
  reviewer `gemini-3.8-flash-medium` via `studio-agy-reviewer`): **approve**,
  `findings: []`.
- `01a1c91c` (review ref
  `review/hv/zoder-mimo-usage-frame-20261105-01a1c91c-f7d1-7d1d-aa04-d24abb3bd8d0`,
  reviewer `coder`): **approve**, `findings: []`.

Both verdicts verify the same change set documented above: the MiMo
usage-frame regression test, the contract notes, the `FAULT-TO-FIX.md` row,
and this refresh record. No blocking findings were reported by either child.

The branch was left unchanged by that round because there was no defect to
fix: the only open item is that the first review is an *advisory* approve
(the reviewer is below the tier review floor for this branch), so the merge
gate waits for a reviewer at or above that tier (`doctor_reason`: "advisory
approve... waiting for a reviewer at or above it"). That is a reviewer-tier
condition on the parent refresh job, not a code defect on this branch. This
commit records that verification so the delivery state is complete on the
branch itself.
