# Refresh record — hv/zoder-exitpath-cancel-20261105-r1 → master b459645d (2026-11-11)

**Date:** 2026-11-11
**Branch:** `hv/zoder-exitpath-cancel-20261105-r1`
**Previous head:** `5b87817f9d` (refreshed onto `5a7fd588`, see the record it replaced)
**Target:** `origin/master` @ `b459645df4...` (docs(refresh): record the post-refresh review round and the remaining advisory-approve gate)
**Merge commit:** `c7a7b31c`
**Merge parents:** `5b87817f` (branch) + `b459645d` (master)

## Why the refresh was needed

The branch head `5b87817f` was already refreshed onto `5a7fd588` earlier today,
but master moved on in the interval through the
`hv/zoder-mimo-usage-frame-20261105` landing (`094cc513`, `6bd4d5a3`,
`b459645d`). The branch was therefore no longer a fast-forward of master.

## Conflicts

`git merge origin/master` produced **zero textual conflicts**. Master's new
commits touch only files this branch does not carry:

- `REFRESH-hv-zoder-mimo-usage-frame-20261110.md` (new)
- `REFRESH-hv-zoder-mimo-usage-frame-20261111.md` (new)
- `crates/zoder-cli/src/agentic.rs` (F5.4 API-contract notes in `agentic_utilization_tests`)
- `crates/zoder-cli/src/main.rs`
- `crates/zoder-core/src/utilization.rs`
- `crates/zoder-core/tests/provider_usage_after_finish.rs` (new)
- `docs/FAULT-TO-FIX.md` (one row)

This branch's intended change — the reaper exit-path cleanup wording
(`cancel not confirmed` → `cancel not acknowledged`) in
`crates/zoder-cli/src/reaper.rs`, the classifier pinning in
`crates/zoder-cli/src/failure.rs`, the provider-frame assertion in
`crates/zoder-core/tests/provider.rs`, the doc wording in
`docs/CANCEL-PROTOCOL.md`, and the `.gitignore` scratch rule — is already
byte-identical in master's tree since `5a7fd588`. Both sides therefore keep
together exactly as required: the branch's intended change and the target's
newer code.

No conflict required a design decision.

## Post-merge content

Against `origin/master` (`b459645d`), the branch retains exactly its refresh
record (this file); every other path is identical to master's tree. The
mimo-usage-frame code and docs land with master.

## Test evidence

Suite run: `cargo test --workspace --all-features`, Rust 1.96.1 (pinned
toolchain), before (target tree `b459645d`) and after (merged head).

- One flaky failure appeared in the baseline clone run —
  `per_provider_stream_key_forces_streaming_wire_and_records_first_token`
  (`crates/zoder-core/tests/provider.rs`) — but it passes on immediate rerun
  in isolation (`2 passed; 0 failed`); the full-suite failure is non-deterministic.
- Failures do not grow versus the target: the same (zero stable / one flaky)
  profile on both sides of the merge.

## Delivery

Same head branch pushed to ARGONAS (`origin`,
`ssh://jasonperlow@192.168.207.101/mnt/datapool/git/zoder.git`) with upstream
tracking; remote SHA matches local HEAD.
