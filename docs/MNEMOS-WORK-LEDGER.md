# MNEMOS Work Ledger

*Part of `zoder`. Content-free checkpoints: no prompts, no model output, no
environment variables are ever recorded. See `crates/zoder-cli/src/mnemos.rs`.*

## What it is

When zoder runs an agentic `zoder loop` inside a Git checkout, it writes one
small **factual checkpoint per iteration** recording what was attempted — not
what was proved. The checkpoint has two legs:

1. **Local (always attempted):** one JSON line appended to
   `.git/zoder-work-ledger.jsonl` inside the current Git directory. This is the
   recovery path — work is never lost because a remote was unreachable.
2. **Remote (optional):** the same evidence object posted to a
   [MNEMOS](https://github.com/ncz-os/mnemos) server so a fleet of workers can
   find prior work. Skipped silently unless both environment variables below
   are set.

Both legs are **evidence, not proof**: a checkpoint says "the loop reached this
iteration on this HEAD", not "the change is correct" and not "it was pushed to
a remote Git server".

## Configuration — `MNEMOS_URL` and `MNEMOS_TOKEN`

| Variable | Purpose |
|---|---|
| `MNEMOS_URL` | Base URL of the MNEMOS HTTP API (e.g. `https://mnemos.example.com`). Must be a valid `http://` or `https://` URL with **no embedded credentials** (`user:pass@` is rejected). |
| `MNEMOS_TOKEN` | Bearer token sent as `Authorization: Bearer …`. Read from the environment only — never put it in config files, source, or the ledger. |

Both are optional. If either is unset, the local ledger is still written and
the remote post is skipped with no error. If `MNEMOS_URL` is set to a
non-HTTP(S) URL or one with embedded credentials, the remote post fails with
`MNEMOS_URL must be an HTTP(S) URL without embedded credentials`.

Credentials are read only from the environment; the CLI and the checkpoint
helper never take them from flags or config.

## `zoder mnemos` — search and record

The CLI exposes the MNEMOS API directly. It requires both `MNEMOS_URL` and
`MNEMOS_TOKEN`, and exactly one of `--search` / `--record`:

```bash
# Search prior work (free text; filtered to subcategory "zoder-work", limit 5).
zoder mnemos --search "retry after host routing moved"

# Search for a specific job's prior work — the job ID must appear in recorded
# content for the search to hit.
zoder mnemos --search "01a0f56a-857c-7de6-b743-c184007c7eee"

# Record a note as a memory (category "projects", subcategory "zoder-work").
zoder mnemos --record "Parent 01a0f515-bf70-73ee-94d6-94f3b3dd7e46: iteration 3 review findings"
```

Behavior worth knowing:

- `--search` posts to `/v1/memories/search` with
  `{"query": …, "limit": 5, "subcategory": "zoder-work"}`. It is free text
  against the **remote** server — the local `.git/zoder-work-ledger.jsonl` file
  is not queried.
- `--record` posts to `/v1/memories` with
  `{"content": …, "category": "projects", "subcategory": "zoder-work",
  "source_agent": "zoder"}`. It adds no environment metadata — to make a job ID
  retrievable, include it in the record text yourself.
- On success the full pretty-printed JSON response is printed to stdout. On
  request failure the command **fails** (nonzero, error on stderr) — there is
  no "pending" state for manual records. The 10-second request timeout and
  "no redirects" policy apply; a 401 surfaces as `MNEMOS returned HTTP 401`
  with the token and response body never leaked into the error.

## Automatic per-iteration local ledger

After each `zoder loop` iteration, `checkpoint()` appends one line to
`.git/zoder-work-ledger.jsonl` (path: the Git directory from
`git rev-parse --absolute-git-dir`, joined with `zoder-work-ledger.jsonl`,
opened in append mode). The line is the raw evidence object the loop supplies —
no wrapper, no timestamps added by the ledger:

```json
{"job_id":"01a0f56a-857c-7de6-b743-c184007c7eee","workspace":"/home/user/project","iteration":1,"head_sha":"a1b2c3d4…","files":["src/lib.rs"],"check_passed":true,"verdict":"approve","blocking_findings":[]}
```

- Written only when `cwd` is a Git checkout; otherwise zoder prints
  `[zoder] local MNEMOS ledger unavailable: not a Git checkout` and continues.
- The append is best-effort. Open or write failures print
  `[zoder] local MNEMOS ledger could not be opened` /
  `[zoder] local MNEMOS ledger write failed` to stderr and never fail the loop.
- Appends are not atomic across processes; two workers writing to the same
  `.git` directory interleave at line granularity.

## Optional remote checkpoints

If `MNEMOS_URL` and `MNEMOS_TOKEN` are set, the same evidence object is posted
to `/v1/memories` after the local append, with:

- `content` — the evidence object, stringified;
- `category` — `"projects"`;
- `subcategory` — `"zoder-work"`;
- `source_agent` — `"zoder"`;
- `metadata.job_id` — the `HIVE_JOB_ID` environment value **only if set**
  (omitted otherwise).

On success: `[zoder] MNEMOS checkpoint <memory-id>`. On failure:
`[zoder] MNEMOS checkpoint pending: <error>` — and the loop moves on.

### Job context — `HIVE_JOB_ID` and retrieval by parent ID

- `HIVE_JOB_ID` identifies the current job. Set in the worker environment, it
  is recorded in the remote checkpoint's `metadata.job_id`, so a job's
  checkpoints are attributable when searched.
- **Retrieval by parent ID is a search-time convention, not a stored field.**
  The checkpoint never reads or records a parent job ID (no
  `HIVE_PARENT_JOB_ID` is consulted). To retrieve a child job's prior work by
  its parent, include the parent ID in record text and search for it as free
  text:

  ```bash
  zoder mnemos --search "01a0f515-bf70-73ee-94d6-94f3b3dd7e46"
  ```

## Failure behavior

- **Local write failure** — the ledger line is missing (stderr message above),
  the loop continues, and the remote attempt still proceeds independently. A
  local failure never stops a remote post, and vice versa.
- **Remote failure** (network, timeout, HTTP error, invalid URL) — the local
  entry stands as recorded; the failure is logged as `MNEMOS checkpoint
  pending: …` and the iteration continues. Error text is deliberately minimal
  (`MNEMOS request failed`, `MNEMOS returned HTTP <n>`) — no tokens, URLs with
  credentials, or server response bodies are surfaced.
- **Neither failure kind aborts work.** The design rule is: *network failure
  must not discard work or prevent local recovery.*

## Limits — read this before relying on the ledger

- **A checkpoint is evidence, not proof of remote Git delivery.** It records
  iteration progress (HEAD SHA, files touched, check/verdict) at the moment the
  loop reached it. It does not verify that commits were pushed to any remote,
  that the push was accepted, or that the change is correct. Keep *checkpoint
  evidence*, *Git delivery*, and *correctness* as three separate claims.
- **Pending records do not currently auto-replay.** A remote post that logs
  `MNEMOS checkpoint pending` is not retried by zoder. The local line is the
  durable copy; replaying it to MNEMOS is a manual act (e.g. `zoder mnemos
  --record "<line>"`).
- **Checkpoints fire from `zoder loop` only**, once per completed iteration.
  One-shot `zoder exec` turns and aborted turns produce no automatic
  checkpoints.
- **The ledger is content-free by construction** — no prompts, model output, or
  environment dumps are recorded. Treat it as an audit trail of *progress*,
  and pair it with Git refs/objects for *content*.

## Local recovery

When the remote is down, `.git/zoder-work-ledger.jsonl` in the workspace's Git
directory is the source of truth: read it to see which iterations landed, on
which HEAD, with which check/verdict. Each line is self-describing JSON and
safe to `jq` (`jq -c '.head_sha, .iteration' .git/zoder-work-ledger.jsonl`).