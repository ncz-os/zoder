# MNEMOS Work Ledger

zoder records a small **factual work ledger** for its autonomous coding loop.
This page documents the zoder-specific surface — the `zoder mnemos` CLI, the
per-iteration local ledger, optional remote checkpoints, and the honest limits
of what a checkpoint does and does not prove.

> The general ncz-os **MNEMOS** integration (MCP recall, per-agent chat
> memory, direct DB persistence) is documented in the
> [Enterprise memory & persistence](README.md#enterprise-memory--persistence)
> section of the README. This page covers the *work ledger* — per-iteration
> progress evidence for the coding loop.

---

## What the ledger records

At the end of each iteration of the `zoder exec` coding loop (author turn,
review, objective check), zoder builds a small evidence object:

```json
{
  "job_id": "01a0f56a-857c-7de6-b743-c184007c7eee",
  "workspace": "/home/user/project",
  "iteration": 1,
  "head_sha": "a1b2c3d4",
  "files": ["src/lib.rs"],
  "check_passed": true,
  "verdict": "passed",
  "blocking_findings": []
}
```

The checkpoint is deliberately **factual**: it excludes prompts, model output,
and environment data. It records *what was worked on* (workspace, iteration,
HEAD SHA, touched files, check outcome, review verdict) — not *what the model
said*.

---

## Configuration: `MNEMOS_URL` and `MNEMOS_TOKEN`

The work ledger talks to a [MNEMOS](https://github.com/ncz-os/mnemos) server
using **two environment variables**, both read from the environment only:

| Variable | Purpose | Notes |
|---|---|---|
| `MNEMOS_URL` | Base URL of the MNEMOS HTTP API | Must be a valid `http://` or `https://` URL **without embedded credentials** (no `user:pass@` host). Invalid values fail fast with `MNEMOS_URL is required` / `MNEMOS_URL must be an HTTP(S) URL without embedded credentials`. |
| `MNEMOS_TOKEN` | Bearer token for API authentication | Sent as `Authorization: Bearer <token>`. Source it from the environment; **never commit the value to this repo or to code**. |

Example (values shown as placeholders, not real secrets):

```bash
export MNEMOS_URL="https://mnemos.internal"      # placeholder — no embedded credentials
export MNEMOS_TOKEN="${MNEMOS_TOKEN}"            # from your secret store; never hardcode
```

Behavior when either variable is unset:

- `zoder mnemos --search/--record` **fails** with `MNEMOS_URL is required` or
  `MNEMOS_TOKEN is required`.
- The automatic per-iteration checkpoint **still writes the local ledger** and
  then **skips the remote post** silently (it is optional and best-effort).

---

## CLI: `zoder mnemos`

The `zoder mnemos` subcommand offers two **mutually exclusive** modes — exactly
one of `--search` or `--record` must be given:

### Search — `--search <query>`

```bash
zoder mnemos --search "refactor authentication"
```

Posts to the MNEMOS server's `/v1/memories/search` endpoint with body
`{"query": "<query>", "limit": 5, "subcategory": "zoder-work"}` and prints the
pretty JSON response on stdout. Only the **top 5** `zoder-work` memories match.
The local `.git` ledger is **not** queried — search goes to the remote server.

### Record — `--record <content>`

```bash
zoder mnemos --record "Completed auth refactor: fixed token leak in session validation"
```

Posts to the `"/v1/memories"` endpoint with body
`{"content": "<content>", "category": "projects", "subcategory": "zoder-work", "source_agent": "zoder"}`
and prints the response. The **only** thing stored is the text you pass — no
job ID, timestamp, or environment data is added automatically by `--record`.

### Errors and secrets

- A non-2xx HTTP response surfaces as `MNEMOS returned HTTP <status>` — the
  server's response body is **not** echoed, so sensitive payloads and
  credentials never leak into error output (this is covered by a unit test).
- Network/timeout failures surface as the generic `MNEMOS request failed`.
- The request uses a 10-second timeout and **does not follow redirects**.

---

## Automatic per-iteration checkpoints

When the coding loop runs (e.g. `zoder exec` with an iteration budget), each
iteration ends with an automatic checkpoint — this is what the `zoder mnemos`
CLI does *not* do; it happens inside the loop.

### 1. Local `.git` ledger (always attempted, first)

The checkpoint is **first** appended to a JSONL file inside the repository's
`.git` directory:

```
<git dir>/zoder-work-ledger.jsonl
```

The git directory is discovered via `git rev-parse --absolute-git-dir` from the
loop's working directory. Each line is one checkpoint's evidence object (the
JSON shown above). This write is append-mode and is the **recovery path**:
network failure must never discard work or prevent local recovery.

Local-write failures (not a git checkout, git invocation fails, file cannot be
opened or written) are reported to stderr as `[zoder] local MNEMOS ledger …`
messages — they **do not** abort the loop.

### 2. Remote MNEMOS checkpoint (optional, second)

If **both** `MNEMOS_URL` and `MNEMOS_TOKEN` are set, the checkpoint is then
posted to `/v1/memories` with:

- `content` — the evidence object stringified
- `category`: `"projects"`, `subcategory`: `"zoder-work"`, `source_agent`: `"zoder"`
- `metadata.job_id` — the `HIVE_JOB_ID` value, when set

On success the loop logs `[zoder] MNEMOS checkpoint <memory-id>` to stderr; on
failure it logs `[zoder] MNEMOS checkpoint pending: <error>` and **moves on**
(the loop result is unaffected — see *Failure behavior*).

---

## Job context: `HIVE_JOB_ID` and retrieval by parent ID

- **`HIVE_JOB_ID`** — the hive job identifier for the *current* job, read from
  the environment by the worker. When set, it is stored as
  `metadata.job_id` on every remote checkpoint, and it is also included as a
  `job_id` field in the local ledger evidence. This lets you attribute
  checkpoints to one job's run.

- **Retrieval by parent job ID** — the checkpoint records only the *current*
  job's ID; a parent/supervisor job ID is **not** read or stored automatically.
  If your workflow parents jobs, include the parent ID **in the record text**
  when you log it manually so it stays searchable, then retrieve it by
  searching for that ID as free text:

  ```bash
  # Log so the parent ID is findable later (include it in the text)
  zoder mnemos --record "Parent 01a0f515-bf70-73ee-94d6-94f3b3dd7e46: iteration 3 review findings"

  # Retrieve by parent ID (free-text search over recorded content)
  zoder mnemos --search "01a0f515-bf70-73ee-94d6-94f3b3dd7e46"
  ```

  A job ID is retrievable by search **only because it appears in the recorded
  content** — search is free-text over `zoder-work` memories, not a structured
  ID lookup.

---

## Failure behavior

- **Local ledger write fails** (no git checkout / git error / I/O error): the
  loop continues; a `[zoder] local MNEMOS ledger …` note goes to stderr. The
  remote attempt (if configured) still proceeds.
- **Remote checkpoint fails** (network, timeout, auth, server error): the loop
  continues; you see `[zoder] MNEMOS checkpoint pending: <error>` on stderr.
  The local ledger entry remains as recorded.
- **Auth failure** (bad token): the server's HTTP 401 surfaces as
  `MNEMOS returned HTTP 401` — the response body is never echoed, so the token
  and any sensitive server output stay out of logs.
- **Missing env vars** in the CLI: the command fails with `MNEMOS_URL is
  required` / `MNEMOS_TOKEN is required` before any request is made.

The design rule throughout: **checkpointing is best-effort observability. It
never discards work, never blocks the loop, and never fails a run over a
network problem.**

---

## Limits (read this)

Be precise about what a checkpoint *is*:

- **A checkpoint is evidence, not proof of remote Git delivery.** It records
  what the agent worked on (iteration, HEAD SHA, touched files, check outcome,
  verdict). It does **not** prove a commit was pushed to a remote, that a
  delivery succeeded, or that the build/tests are green. Keep *checkpoint
  evidence*, *Git delivery*, and *correctness* as three separate claims —
  verify each independently.
- **Pending records do not currently auto-replay.** If a remote checkpoint
  fails and is logged as `pending`, there is **no automatic retry or replay**
  of that record. The local `.git` ledger keeps the evidence; re-logging a
  pending item to the remote is a manual step today.
- **Search is remote and shallow.** `--search` queries only the remote MNEMOS
  server, returns at most 5 `zoder-work` results, and does not search the
  local `.git` ledger.
- **`--record` stores only your text.** No automatic job ID, timestamp, or
  environment is attached to a manual record.
- **Local ledger is per-checkout.** It lives in the repository's `.git`
  directory, so it is scoped to that checkout (not shared across clones).