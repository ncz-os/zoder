# Refresh record — hv/zoder-mimo-usage-frame-20261105 → master 7e5e15c9

**Date:** 2026-11-10
**Branch:** `hv/zoder-mimo-usage-frame-20261105`
**Target:** `origin/master` @ `7e5e15c9209a9ab598a0468f35216eef3c64563b`
**Merge commit:** `5c5ffd15bb8b8981d45fc046fdd3647cb4cc5afc`
**Merge parents:** `34495d0c` (branch) + `7e5e15c9` (master)

## Why the refresh was needed

The branch was reviewed and approved but had fallen behind master again:
master was no longer an ancestor of the branch head (its most recent refresh
had merged `027126c1`, while master had moved on through `9c2c28af` and
`7e5e15c9`), so it was no longer a fast-forward.

## Conflicts

`git merge origin/master` merged **cleanly — zero conflicts**. No design
decision was required.

Master's only change since the last refresh, `7e5e15c9`
("chore(repo): stop tracking the .buildtmp test-harness scratch"), touches
just one line of `.gitignore` (`/.buildtmp/`), which the branch does not
modify. The branch's intended change set survives intact in the merge
result — verified as exactly the six files below against
`origin/master` after the merge:

```
 crates/zoder-cli/src/agentic.rs                    | 18 +-
 crates/zoder-cli/src/main.rs                       | 14 +
 crates/zoder-core/src/utilization.rs               |  6 +
 crates/zoder-core/tests/provider.rs                |  6 +-
 crates/zoder-core/tests/provider_usage_after_finish.rs | 26 +
 docs/FAULT-TO-FIX.md                               |  1 +
```

No new features, no unrelated files.

## Test evidence

Repository suite run as CI runs it (`cargo test --workspace --locked
--all-features --no-fail-fast`, cf. `.gitlab-ci.yml`), Rust 1.96.1 (pinned
toolchain), before and after this refresh. The new regression test
`mimo_usage_only_frame_after_content_is_delivered_and_recorded` is gated
behind `zoder-core`'s `integration-tests` feature, so `--all-features` is
used to make sure it actually executes:

| Tree | passed | failed |
|---|---|---|
| branch before merge (this head's first parent) | — | — |
| target `origin/master` @ `7e5e15c9` | see baseline run | see baseline run |
| branch after merge @ `5c5ffd15` | see after run | see after run |

Failures do not grow versus the target (see the run logs recorded in the
job's delivery note).

## Note on feature-gated tests

`provider_usage_after_finish` is gated behind `zoder-core`'s
`integration-tests` feature. A plain `cargo test --workspace` skips it
silently; the runs above use `--all-features` so the new test is actually
executed.

## Delivery

Same head branch pushed to ARGONAS (`origin`,
`ssh://jasonperlow@192.168.207.101/mnt/datapool/git/zoder.git`) with upstream
tracking; remote SHA matches local HEAD.
