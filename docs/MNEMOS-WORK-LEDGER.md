# MNEMOS Work Ledger

## Overview

zoder records factual progress checkpoints to a MNEMOS work ledger. These checkpoints are **evidence**, not proof of Git delivery. They record what the agent worked on, enabling lookup of prior work and supporting local recovery.

The ledger is maintained as `.git/zoder-work-ledger.jsonl` (local file) and optionally mirrored to a remote MNEMOS server via `MNEMOS_URL` / `MNEMOS_TOKEN`.

## MNEMOS_URL and MNEMOS_TOKEN

- **MNEMOS_URL** — The base URL for the MNEMOS HTTP API (e.g. `https://mnemos.internal`). Must be a valid HTTP(S) URL without embedded credentials.
- **MNEMOS_TOKEN** — Bearer token for MNEMOS API authentication. Sourced from the environment; never store credentials in source.

Both are required for remote checkpoint posting. If either is missing, the local `.git/zoder-work-ledger.jsonl` file is still written, but the remote POST is skipped.

## Local Checkpoint Format

Each local checkpoint is a line in `.git/zoder-work-ledger.jsonl`, written atomically via `git rev-parse --absolute-git-dir` discovery:

```json
{
  "content": "Human-readable summary of work performed",
  "category": "projects",
  "subcategory": "zoder-work",
  "source_agent": "zoder",
  "metadata": {
    "job_id": "hive job identifier if available",
    "parent_job_id": "parent job identifier for lookup context"
  }
}
```

The checkpoint is **always** written to the local ledger first, regardless of network success. Network failure must not discard work or prevent local recovery.

## Remote Checkpoint Posting

After the local write, zoder attempts to POST the checkpoint to the MNEMOS server at `/v1/memories`. The remote payload includes:

- `content`: The evidence stringified
- `category`: `"projects"`
- `subcategory`: `"zoder-work"`
- `source_agent`: `"zoder"`
- `metadata.job_id`: `HIVE_JOB_ID` environment value (optional)
- `metadata.parent_job_id`: `HIVE_PARENT_JOB_ID` environment value (optional — **this is the lookup context, not a field recorded by the current checkpoint implementation**)

**Important**: The `parent_job_id` field in metadata serves as a *lookup context* — it lets you search prior work by parent job ID supplied by the worker. It is **not** the same as the `job_id` field, which identifies the current job's checkpoint. The checkpoint records `HIVE_JOB_ID` as the primary job identifier and optionally `HIVE_PARENT_JOB_ID` as contextual metadata for search.

## CLI Commands

### `zoder mnemos --search`

Search the MNEMOS ledger for prior work:

```bash
zoder mnemos --search "query string"
```

- Uses `/v1/memories/search` endpoint
- Filters by `subcategory: zoder-work` and `limit: 5`
- Returns matching memories with their IDs and content
- Can supply either a free-text query or a `HIVE_JOB_ID` / parent job ID to retrieve prior work evidence

### `zoder mnemos --record`

Record a new checkpoint:

```bash
zoder mnemos --record "content summary"
```

- Uses `/v1/memories` endpoint
- Posts with `category: projects`, `subcategory: zoder-work`, `source_agent: zoder`
- Includes `metadata.job_id` from `HIVE_JOB_ID` if set
- Includes `metadata.parent_job_id` from `HIVE_PARENT_JOB_ID` if set
- Outputs the remote MNEMOS record ID on success, or "pending" with the error on failure

## HIVE_JOB_ID Context

- `HIVE_JOB_ID` — The current job's identifier. When set, it is recorded in every MNEMOS checkpoint's `metadata.job_id` field, enabling job-scoped lookup of prior work.
- Supply this via the hive worker environment; it distinguishes the current job's checkpoints from others.

## Parent Job ID Lookup

- `HIVE_PARENT_JOB_ID` — An optional identifier supplied by the worker. When present, it is recorded in checkpoint metadata as a **lookup context** (not as a recorded field of the current checkpoint itself).
- To retrieve prior work by parent job ID, use:

```bash
zoder mnemos --search "job_id:PARENT_ID"
```

or equivalently, the search UI may filter by the parent job ID context.

## Failure Behavior and Limits

- **Local failure**: If `.git/` is not available or `git rev-parse` fails, the local `.git/zoder-work-ledger.jsonl` write is skipped. No error is propagated to the user beyond the console message.
- **Network failure**: The local checkpoint is **still written**. The remote POST may fail; in that case the user sees `MNEMOS checkpoint pending: <error>`. Work is not discarded.
- **Auth failure**: If `MNEMOS_TOKEN` is invalid, the remote POST returns HTTP 401 and the error is reported as pending. Credentials are never exposed in error messages.
- **Concurrent writes**: The local ledger uses append-mode `OpenOptions`, which is safe for process-local use but not guaranteed atomic across processes.
- **Pending checkpoints do not automatically replay**: A failed remote POST does not trigger a retry or replay. The local ledger entry stands as-recorded; the remote absence is informational.
- **Checkpoint is evidence, not proof of Git delivery**: A checkpoint records what work was attempted, not that Git successfully delivered it. The `--check` CLI command verifies Git delivery separately; do not represent MNEMOS checkpoints as proof of build/test correctness.

## Search and Record Examples

### Search by query text

```bash
zoder mnemos --search "agent review feedback"
```

### Search by HIVE_JOB_ID

```bash
zoder mnemos --search "job_id:fb30400fb491527619293e2cdf1e868565d4eeab"
```

### Record a checkpoint

```bash
zoder mnemos --record "Completed iteration 3: fixed parser crash on malformed input"
```

### Record with HIVE_JOB_ID and parent context

```bash
zoder mnemos --record "Completed iteration 3: fixed parser crash on malformed input"
```

(Both `HIVE_JOB_ID` and `HIVE_PARENT_JOB_ID` are picked up from the environment if set.)

## Relationship to Git Delivery

- MNEMOS checkpoints are **independent** of Git delivery. A checkpoint records work progress; it does not guarantee or imply that Git operations succeeded.
- The `zoder --check` command verifies Git delivery (e.g., whether the working tree is clean, whether commits are reachable). That verification is separate from MNEMOS checkpointing.
- Never represent MNEMOS checkpointing as proof of Git delivery or build/test correctness.

## Local Recovery

If the remote MNEMOS server is unavailable, the local `.git/zoder-work-ledger.jsonl` file contains all checkpoints recorded in the current session. These can be reviewed or replayed manually, or used as the source of truth until the remote server recovers.