# Concurrency, queueing and shared hosts

How zoder behaves when many sessions share one host, where time goes while a
call "waits", and the settings recommended for a busy shared build host.

## Engine hierarchy

zoder's engine is **Zeroclaw**. `zoder exec` and the author turn of
`zoder loop` run through the Zeroclaw daemon (`zeroclaw daemon`, reached over
its Unix socket). **goose is an alternative engine only**: it is used when it
is selected explicitly with `--engine goose`, typically because Zeroclaw is
broken on that host. zoder never switches to goose by itself. Every default,
route and fallback in this document assumes Zeroclaw.

## What goes through the daemon and what does not

| Command | Path |
|---|---|
| `zoder review`, `zoder adversarial-review`, loop review phase | **direct** to the reviewer provider endpoint (no daemon) |
| `zoder exec` (agentic, default) and loop author turns | Zeroclaw daemon over ACP; falls back to a oneshot call when no daemon answers |
| `zoder exec --oneshot` | direct to the provider endpoint |

So a review that "queues" is waiting on one of three things: a local zoder
slot (only when a limit is configured), the local spend ledger lock, or the
provider server itself. It is never waiting on the Zeroclaw daemon.

## Where the time goes (`--json`)

Every reviewer receipt in `zoder review --json` (`provenance[]`) reports:

| Field | Meaning |
|---|---|
| `slot_wait_ms` | time waiting for a local per-provider slot (`ZODER_PROVIDER_CONCURRENCY`) |
| `slot_limit`, `slot_held` | the configured limit (0 = unlimited) and whether a slot was taken |
| `ledger_wait_ms` | time to reserve the spend-ledger row (host-wide lock + scan) |
| `headers_ms` | request sent to response headers |
| `first_token_ms` | request sent to the first generated token (streamed calls only) |
| `latency_ms` | total wall time for this reviewer, including retries |

Reviewer calls are non-streaming, so for them `headers_ms` is the server's
whole queue + prefill + generation time and `first_token_ms` is empty;
`zoder exec --oneshot` streams and reports both.

`zoder exec --oneshot --json` reports `headers_ms` and `first_token_ms`.

While a call is waiting on the server (a non-streaming reviewer call that has
no response yet, or a streamed call with no token yet), zoder prints
`[zoder] still waiting for <provider> after Ns ...` every 30 seconds
(`ZODER_WAIT_NOTICE_S`) unless `--quiet`. The same request keeps running; it
is never re-sent. The overall request
budget (`--request-timeout`, default 120s) still bounds the wait; once tokens
start, the idle guard (`ZODER_IDLE_S`, default 25s) applies, including while a
model is only streaming reasoning.

## The ledger (fixed 2026-10-10)

Before this fix every billable call left a 64 KiB slot in
`~/.zoder/ledger.jsonl` and every reservation re-read the whole file under a
host-wide lock. Measured: ACHILLES 506 MB / 7,725 calls, HYDRA 328 MB. One
review chunk then spent seconds re-reading hundreds of MB while holding the
lock, and concurrent sessions serialized silently. Slots are now 4 KiB and a
mostly-padding ledger is compacted on the next write (see the ledger module
docs); `ledger_wait_ms` shows what is left.

Compaction is postponed while an older zoder build may be mid-dispatch on the
same host (a legacy-size reservation row younger than an hour), so a
mixed-version rollout is safe; the first write after the old processes finish
compacts the file.

## Per-provider concurrency limit

Optional, off by default. Configured by environment only, so an older binary
reading the same shared config never meets an unknown key:

```sh
# at most 2 concurrent reviewer calls to the CERBERUS gemma route from this
# host, 4 to EIH, unlimited for everything else
export ZODER_PROVIDER_CONCURRENCY="cerberus-reviewer=2,nvidia-eih=4"
# fail (and let the next reviewer candidate take over) after 15 minutes
export ZODER_QUEUE_TIMEOUT_S=900
```

The limit is a counting semaphore of `flock`-ed files under
`~/.zoder/state/slots/<provider>/`, shared by every zoder process of that user
on the host. A process that dies releases its slot automatically. A queue
timeout is a capacity failure, so the reviewer chain moves to the next
configured reviewer instead of failing the review.

## Recommended settings for a shared host (HYDRA)

HYDRA runs about ten agent sessions against one Zeroclaw daemon, and their
reviews all go to the same two reviewer routes.

- Deploy a zoder build that contains the ledger fix first; it removes the
  dominant serialization.
- `ZODER_PROVIDER_CONCURRENCY="cerberus-reviewer=2,nvidia-eih=4"`: CERBERUS
  serves gemma4-31b from one GPU, so more than two concurrent chunks from one
  host only lengthens everyone's queue; EIH rate-limits (HTTP 429) under
  bursts.
- `ZODER_QUEUE_TIMEOUT_S=900` (the default).
- Keep `--request-timeout` at 300 for gemma reviews of large chunks.
- Watch `first_token_ms` in review receipts: values of tens of seconds mean
  the reviewer server is saturated; spread sessions across routes rather than
  raising timeouts.

These are recommendations; nothing is applied automatically.
