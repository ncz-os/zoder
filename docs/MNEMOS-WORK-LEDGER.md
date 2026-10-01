# MNEMOS Work Ledger

**zoder** integrates with [MNEMOS](https://github.com/ncz-os/mnemos), ncz-os's open-source agent memory system, to persist and recall work evidence across runs. This document explains the zoder-specific MNEMOS workflow, configuration, and limits.

---

## Configuration

Two environment variables must be set before using MNEMOS features:

| Variable | Description |
|---|---|
| `MNEMOS_URL` | HTTP(S) URL of the MNEMOS server, e.g. `https://mnemos.internal`. Must not contain embedded credentials (username/password). |
| `MNEMOS_TOKEN` | Bearer token for MNEMOS API authentication. Read from the environment only; never stored in code. |

Both are required by the `zoder mnemos` CLI subcommand and the `checkpoint` function.

---

## CLI: `zoder mnemos`

The `zoder mnemos` subcommand offers two mutually exclusive modes:

### `--search`

Search existing MNEMOS memories for a given query, limited to 5 results filtered by the `zoder-work` subcategory.

```bash
zoder mnemos --search "refactor authentication"
```

This sends a POST to `/v1/memories/search` with body `{"query":"<query>","limit":5,"subcategory":"zoder-work"}` using the `MNEMOS_URL` and `MNEMOS_TOKEN`.

### `--record`

Record a new memory entry to MNEMOS. The content, category `"projects"`, subcategory `"zoder-work"`, and source agent `"zoder"` are set automatically.

```bash
zoder mnemos --record "Completed authentication refactor — fixed token leak in session validation"
```

This sends a POST to `/v1/memories` with body `{"content":"<content>","category":"projects","subcategory":"zoder-work","source_agent":"zoder"}`.

---

## Automatic Per-Iteration Local Ledger

Every zoder run automatically writes a local JSONL ledger inside the Git checkout:

**.git/zoder-work-ledger.jsonl**

Each line is a JSON object (the `evidence` payload) passed to `zoder mnemos checkpoint`. This is **written before any network attempt**, so:

- If the network call fails, the local ledger entry remains as evidence of the attempted checkpoint.
- The ledger is appended to on every iteration; it is not a replacement.
- It is scoped per-repository (`.git/` directory), so different projects have independent ledgers.

This local ledger is the **primary recovery mechanism**. It survives network outages, server restarts, or credential changes.

---

## Remote Checkpoints (Optional)

When `MNEMOS_URL` and `MNEMOS_TOKEN` are set, the checkpoint function also attempts a remote POST to `/v1/memories`. The remote write:

- Includes `metadata.job_id` if the `HIVE_JOB_ID` environment variable is set.
- Emits `[zoder] MNEMOS checkpoint <id>` on success, or `[zoder] MNEMOS checkpoint pending: <error>` on failure.
- **Is evidence, not proof of remote Git delivery.** The remote MNEMOS entry is a separate system from the local Git ledger; it does not guarantee the content arrived in a Git remote. Use the local `.git/zoder-work-ledger.jsonl` as the authoritative record.

---

## Job Context: `HIVE_JOB_ID`

The `HIVE_JOB_ID` environment variable, when set, is included as `metadata.job_id` in the remote MNEMOS checkpoint payload. This enables retrieval of all checkpoints belonging to the same job:

- **Retrieve by parent ID:** Use `zoder mnemos --search` with queries that filter by job context, or search the local `.git/zoder-work-ledger.jsonl` for entries whose `metadata.job_id` matches.
- This is particularly useful in hive/worker scenarios where multiple iterations or agents contribute to a single job's evidence trail.

---

## Failure Behavior

| Situation | Behavior |
|---|---|
| **Network failure during checkpoint** | Local `.git/zoder-work-ledger.jsonl` entry is preserved. The remote MNEMOS write is retried on the next iteration, but the local record is not discarded. |
| **MNEMOS_URL/MNEMOS_TOKEN missing** | Checkpoint skips the remote write and only updates the local ledger. An informational message is printed: `[zoder] MNEMOS_URL/MNEMOS_TOKEN not set; checkpoint local only`. |
| **Git checkout not detected** | If the working directory is not a Git checkout, the local ledger is skipped with: `[zoder] local MNEMOS ledger unavailable: not a Git checkout`. |
| **Git command fails** | The local ledger is skipped with: `[zoder] local MNEMOS ledger unavailable: Git failed`. |

In all cases, **work is never discarded** — the local ledger is the authoritative fallback.

---

## Limits and Design Notes

| Limit / Design Note | Description |
|---|---|
| **Checkpoint is evidence, not proof of remote Git delivery** | The remote MNEMOS entry and the local `.git/zoder-work-ledger.jsonl` are independent. A successful remote write does not guarantee the content is in any Git remote. |
| **Pending records do not currently auto-replay** | If a previous checkpoint's remote write was pending (failed), the system does **not** automatically replay/rerun that record on the next iteration. The local ledger preserves it, but manual action is required to re-post it. |
| **Search limit** | `zoder mnemos --search` returns a maximum of 5 results, filtered by `subcategory:"zoder-work"`. |
| **Per-iteration ledger** | Each iteration appends one line to `.git/zoder-work-ledger.jsonl`; the file grows monotonically and is never truncated automatically. |
| **No credentials in code** | `MNEMOS_URL` and `MNEMOS_TOKEN` are read exclusively from the environment. The code explicitly rejects URLs with embedded credentials. |
| **HIVE_JOB_ID is optional** | When not set, the remote checkpoint omits `metadata.job_id`; retrieval by parent ID will return no matches for that filter. |

---

## Related

- [MNEMOS project](https://github.com/ncz-os/mnemos) — the memory system backend
- `docs/KNEMON.md` — subscription-utilization intelligence (different from MNEMOS)
- `zoder mnemos --help` — CLI help output