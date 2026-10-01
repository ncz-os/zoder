# MNEMOS Work Ledger

## Overview

zoder records factual progress checkpoints to a MNEMOS work ledger. These checkpoints are **evidence**, not proof of Git delivery. They record what the agent worked on, enabling lookup of prior work and supporting local recovery.

The ledger is maintained as `.git/zoder-work-ledger.jsonl` (local file) and optionally mirrored to a remote MNEMOS server via `MNEMOS_URL` / `MNEMOS_TOKEN`.

## MNEMOS_URL and MNEMOS_TOKEN

- **MNEMOS_URL** — The base URL for the MNEMOS HTTP API (e.g. `https://mnemos.internal`). Must be a valid HTTP(S) URL without embedded credentials.
- **MNEMOS_TOKEN** — Bearer token for MNEMOS API authentication. Sourced from the environment; never store credentials in source.

Both are required for remote checkpoint posting. If either is missing, the local `.git/zoder-work-ledger.jsonl` file is still written, but the remote POST is skipped.

## Local Checkpoint Format

Each local checkpoint is a single line in `.git/zoder-work-ledger.jsonl`, appended after discovery of the Git directory via `git rev-parse --absolute-git-dir`. The entry is the raw evidence object as supplied by the checkpoint caller — no wrapper.

```json
{
  "job_id": "01a0f56a-857c-7de6-b743-c184007c7eee",
  "workspace": "/home/user/project",
  "iteration": 1,
  "head_sha": "a1b2c3d4",
  "files": ["src/lib.rs", "docs/README.md"],
  "check_passed": true,
  "verdict": "passed",
  "blocking_findings": []
}
```

The `job_id` field reflects the `HIVE_JOB_ID` environment value if set; otherwise it may be `null`. Local write errors (Git unavailable, disk full, permission denied) can prevent entry — the operation is not guaranteed across processes.

## Remote Checkpoint Posting

After the local write attempt, zoder POSTs the checkpoint to the MNEMOS server at `/v1/memories`. The remote payload includes:

- `content`: The evidence stringified from the local checkpoint object
- `category`: `"projects"`
- `subcategory`: `"zoder-work"`
- `source_agent`: `"zoder"`
- `metadata.job_id`: `HIVE_JOB_ID` environment value (optional — recorded only if set)

**`HIVE_PARENT_JOB_ID` is not read or recorded by the current checkpoint implementation.** The worker may supply a parent job ID as a manual lookup hint (e.g. `zoder mnemos --search "$HIVE_PARENT_JOB_ID"`), but it is not stored in the checkpoint metadata.

**Important**: The remote POST may fail even when the local write succeeds. Network errors, auth failures, or server unavailability will result in a "pending" status — the local ledger entry stands as-recorded.

## CLI Commands

### `zoder mnemos --search`

Search the remote MNEMOS server for prior work (the local `.git` ledger is a JSONL file and is not queried by this command):

```bash
zoder mnemos --search "query string"
```

- Uses `/v1/memories/search` endpoint
- Filters by `subcategory: zoder-work` and `limit: 5`
- Returns matching memories with their IDs and content
- The query is free text. There is no structured field syntax: a job ID is retrievable only because it appears in recorded content.

### `zoder mnemos --record`

Record a new checkpoint to the remote MNEMOS server:

```bash
zoder mnemos --record "Completed iteration 3: fixed parser crash on malformed input"
```

- Posts with `category: projects`, `subcategory: zoder-work`, `source_agent: zoder`
- **Does NOT automatically include `metadata.parent_job_id`** — parent job ID must be supplied explicitly if needed
- Prints the full pretty JSON response on success; a failed request propagates as an error rather than a "pending" notice
- The automatic checkpoint helper (internal `checkpoint()` function), not manual `--record`, catches network errors and logs `MNEMOS checkpoint pending`

## HIVE_JOB_ID Context

- `HIVE_JOB_ID` — The current job's identifier. When set, it is recorded in the `metadata.job_id` field of remote checkpoints, enabling job-scoped lookup of prior work.
- Supply this via the hive worker environment; it distinguishes the current job's checkpoints from others.

## Parent Job ID Lookup

`HIVE_PARENT_JOB_ID` is never read or recorded. To retrieve prior work by parent job ID, include it in the record text and search for it as free text:

```bash
zoder mnemos --search "01a0f515-bf70-73ee-94d6-94f3b3dd7e46"
```

## Failure Behavior and Limits

- **Local write failure**: If `.git/` is not available or `git rev-parse` fails, the local `.git/zoder-work-ledger.jsonl` write is skipped. No error is propagated beyond the console message. Automatic remote attempt may still proceed.
- **Network failure**: The local checkpoint write is attempted first. If the remote POST fails, the user sees `MNEMOS checkpoint pending: <error>`. The local ledger entry stands as-recorded; the remote absence is informational. No retry or replay is triggered.
- **Auth failure**: If `MNEMOS_TOKEN` is invalid, the remote POST returns HTTP 401 and the error is reported as pending. Credentials are never exposed in error messages.
- **Concurrent writes**: The local ledger uses append-mode `OpenOptions`, which is safe for process-local use but not guaranteed atomic across processes.
- **Pending checkpoints do not automatically replay**: A failed remote POST does not trigger a retry or replay. The local ledger entry stands as-recorded; the remote absence is informational.
- **Checkpoint is evidence, not proof of Git delivery**: A checkpoint records what work was attempted, not that Git operations succeeded. `--check` is a zoder loop option that executes a caller-supplied shell check; it is not a Git-delivery verifier. Keep checkpoint evidence, Git delivery and correctness as three separate claims.
- **Automatic checkpoints occur at the checkpoint call after recorded loop iterations**, not every failed or aborted turn. Pending remote records do not auto-replay.

## Search and Record Examples

### Search by query text

```bash
zoder mnemos --search "agent review feedback"
```

### Search for a job's prior work

```bash
zoder mnemos --search "01a0f56a-857c-7de6-b743-c184007c7eee"
```

### Record a checkpoint

```bash
zoder mnemos --record "Completed iteration 3: fixed parser crash on malformed input"
```

Neither `HIVE_JOB_ID` nor `HIVE_PARENT_JOB_ID` is added automatically: only the text passed to `--record` is stored. To make a job ID searchable, include it in that text.

### Record a parent job ID so it stays searchable

```bash
zoder mnemos --record "Parent 01a0f515-bf70-73ee-94d6-94f3b3dd7e46: iteration 3 review findings"
```

`HIVE_PARENT_JOB_ID` is never read by the checkpoint implementation. Put the parent ID in the record text if you need to find it later.

## Relationship to Git Delivery

- MNEMOS checkpoints are **independent** of Git delivery. A checkpoint records work progress; it does not guarantee or imply that Git operations succeeded.
- `--check` is a zoder loop option that runs the caller-supplied shell check. It is not a Git-delivery command, and zoder does not itself assert commit cleanliness or reachability. A Hive worker may choose a Git-delivery verifier; other callers may choose tests or builds.
- Never represent MNEMOS checkpointing as proof of Git delivery or build/test correctness.

## Local Recovery

If the remote MNEMOS server is unavailable, the local `.git/zoder-work-ledger.jsonl` file contains all checkpoints recorded in the current session. These can be reviewed or replayed manually, or used as the source of truth until the remote server recovers.