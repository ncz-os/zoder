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

At the end of each recorded iteration of the `zoder` loop, zoder builds a small
evidence object (`crates/zoder-cli/src/agentic.rs`, the `mnemos::checkpoint`
call) from exactly these fields — no wrapper, no extra fields:

```json
{
  "job_id": "01a0f515-bf70-73ee-94d6-94f3b3dd7e46",
  "workspace": "/home/user/project",
  "iteration": 1,
  "head_sha": "71334bc",
  "files": ["src/lib.rs"],
  "check_passed": true,
  "verdict": "approve",
  "blocking_findings": 0
}
```

Field notes:

- `job_id` — the `HIVE_JOB_ID` environment value when set, otherwise `null`.
- `head_sha` — the repository HEAD SHA at that point (a Git SHA, **not** a job ID).
- `files` — the files touched this iteration.
- `check_passed` — `true`/`false`/`null`: the exit outcome of the caller's
  `--check` command, or `null` when no check was configured.
- `verdict` — the reviewer's verdict string as recorded by the loop:
  `approve`, `request_changes`, `comment`, `neutral`, or `reject`/`block`
  (the known set in `agentic.rs`), normalized to lowercase; anything
  unrecognized fail-closes as blocking.
- `blocking_findings` — an **integer** count of the reviewer's blocking
  findings (`0` means none).

The checkpoint is deliberately **factual**: it excludes prompts, model output,
and environment data. It records *what was worked on* (workspace, iteration,
HEAD SHA, touched files, check outcome, review verdict) — not *what the model
said*. The object above is stored **raw** in the local ledger; the remote
post wraps it (see *Automatic checkpoints* below).

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
- The automatic per-iteration checkpoint **still attempts the local ledger
  append** and then **skips the remote post** silently (it is optional and
  best-effort).

---

## CLI: `zoder mnemos` (manual, remote-only)

The `zoder mnemos` subcommand offers two **mutually exclusive** modes — exactly
one of `--search` or `--record` must be given. Both are **manual** and both go
**only to the remote MNEMOS server**; neither reads or writes the local `.git`
ledger, and neither automatically attaches job IDs or any other metadata.

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

Posts to the `/v1/memories` endpoint with **only**

`{"content": "<content>", "category": "projects", "subcategory": "zoder-work", "source_agent": "zoder"}`

and prints the full pretty JSON response on success. **That is everything that
is stored** — no job ID, timestamp, parent ID, or environment data is added
automatically by `--record`. If you want a job ID findable by later search,
write it into the `<content>` text yourself (see *Job context* below). Errors
from the server or network propagate as failures of the command.

### Errors and secrets

- A non-2xx HTTP response surfaces as `MNEMOS returned HTTP <status>` — the
  server's response body is **not** echoed, so sensitive payloads and
  credentials never leak into error output (this is covered by a unit test).
- Network/timeout failures surface as the generic `MNEMOS request failed`.
- The request uses a 10-second timeout and **does not follow redirects**.

---

## Automatic per-iteration checkpoints (inside the loop)

The automatic checkpoint (`mnemos::checkpoint`) runs **inside the loop after
each recorded loop iteration** — this is what the `zoder mnemos` CLI does *not*
do. It performs two distinct steps, in order:

### 1. Local `.git` ledger (attempted first)

The evidence object (the raw JSON shown in *What the ledger records*) is
appended as one line to a JSONL file inside the repository's Git directory:

```
<git dir>/zoder-work-ledger.jsonl
```

The git directory is discovered via `git rev-parse --absolute-git-dir` from the
loop's working directory; the append is attempted first, so the local record
exists even when no remote is configured. The append is a plain file open in
`append` mode plus a `write` — it is **not** an atomic write, and it is not
guaranteed to succeed: if the path is not a Git checkout, `git rev-parse`
fails, or the file cannot be opened or written, the append is **skipped** and a
`[zoder] local MNEMOS ledger …` note goes to stderr. These failures **do not**
abort the loop, and the remote attempt below still proceeds.

### 2. Remote MNEMOS checkpoint (optional, second)

If **both** `MNEMOS_URL` and `MNEMOS_TOKEN` are set, the checkpoint is then
posted to `/v1/memories` with a **wrapper** around the same evidence:

- `content` — the evidence object **stringified** (`evidence.to_string()`)
- `category`: `"projects"`, `subcategory`: `"zoder-work"`, `source_agent`: `"zoder"`
- `metadata.job_id` — the `HIVE_JOB_ID` value, when set (the **only** metadata
  key sent)

On success the loop logs `[zoder] MNEMOS checkpoint <memory-id>` to stderr; on
failure it logs `[zoder] MNEMOS checkpoint pending: <error>` and **moves on**
(the loop result is unaffected — see *Failure behavior*). The automatic
checkpoint helper is the one that **catches** network errors and logs
`pending`; the manual `--record` command does not — it propagates them.

---

## Job context: `HIVE_JOB_ID`, parent IDs, and search

- **`HIVE_JOB_ID`** — the hive job identifier for the *current* job. The loop
  reads it to populate `job_id` in the local evidence and `metadata.job_id` in
  the remote post. This is the only job metadata the code reads or records.
- **Parent job IDs are not read or recorded.** `HIVE_PARENT_JOB_ID` is **not**
  read or recorded anywhere in `mnemos.rs`. If your workflow parents jobs, the
  worker provides the parent ID only as a **manual lookup hint**: include it
  explicitly in a manual record's text so it stays searchable, then retrieve
  it by searching for that ID as free text:

  ```bash
  # Log so the parent ID is findable later (include it in the text)
  zoder mnemos --record "Parent 01a0f515-bf70-73ee-94d6-94f3b3dd7e46: iteration 3 review findings"

  # Retrieve by parent ID (free-text search over recorded content)
  zoder mnemos --search "$HIVE_PARENT_JOB_ID"
  ```

  An ID is retrievable by search **only because it appears in the recorded
  content** — search is free-text over the remote `zoder-work` memories, not a
  structured ID lookup, and not a search of the local ledger.

---

## Failure behavior

- **Local ledger append fails** (no git checkout / git error / I/O error): the
  loop continues; a `[zoder] local MNEMOS ledger …` note goes to stderr. The
  remote attempt (if configured) still proceeds.
- **Remote checkpoint fails** (network, timeout, auth, server error): the loop
  continues; you see `[zoder] MNEMOS checkpoint pending: <error>` on stderr.
  A `pending` record is **not** retried or replayed automatically.
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
- **`--check` is not a Git-delivery verifier.** `--check` is a zoder loop
  option: it executes the *caller-supplied* shell check after each author turn
  (the loop's `check_passed` just records that command's exit outcome). A Hive
  worker may choose its own Git-delivery verifier as that check; other callers
  may choose tests or builds. zoder itself does not inspect commits or
  worktree cleanliness on your behalf — a passing `--check` proves only what
  that specific command checks.
- **Pending records do not currently auto-replay.** If a remote checkpoint
  fails and is logged as `pending`, there is **no automatic retry or replay**
  of that record. The local `.git` ledger keeps the evidence; re-logging a
  pending item to the remote is a manual step today.
- **Search is remote and shallow.** `--search` queries only the remote MNEMOS
  server, returns at most 5 `zoder-work` results, and does not search the
  local `.git` ledger.
- **`--record` stores only your text.** No automatic job ID, timestamp, or
  environment is attached to a manual record.
- **Local ledger is per-checkout.** It lives in the repository's Git directory,
  so it is scoped to that checkout (not shared across clones).
- **Local appends are best-effort, not atomic.** A single append is a file
  open in append mode plus a write; there is no cross-process atomic
  guarantee, and local Git or I/O errors can prevent an append (see
  *Automatic checkpoints* and *Failure behavior*).