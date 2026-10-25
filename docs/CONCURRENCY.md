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

## The ACP idle watchdog (agentic turns)

The provider-level idle guard above bounds a single provider HTTP call. The
**ACP idle watchdog** bounds the agentic TURN: the gap between two
SUBSTANTIVE outputs from the engine session.

The watchdog has **TWO** windows, because the two waits have different
causes:

| window | default (self-hosted route) | default (hosted route) | measured from |
|---|---|---|---|
| `first_event` | **600s** | 60s | the start of the turn, until the first SUBSTANTIVE output |
| `between_events` | **180s** | 60s | the last SUBSTANTIVE output |

A **substantive** output is a token (a message chunk or a thought chunk) or a
tool call. Session metadata, usage reports and keepalives are **not**
substantive: they neither end the first phase nor restart any clock.

**Phase rules**

1. From the start of the turn until the first substantive output, the
   `first_event` window applies.
2. After the first substantive output, the shorter `between_events` window
   applies.
3. A pending tool call **suspends** the watchdog (that wait is bounded by the
   total turn budget). When the tool round **completes**, the phase resets
   to the `first_event` window, because the next model turn is a new cold
   start.

The first window is the long one on purpose: the wait for the first token of
a turn is a **cold start** (a local model loading its weights), not a wedge.
A single window could not express both — a 180s window kills a slow local
model before it has produced anything, while a 600s window hides a wedged
session for ten minutes.

The windows are measured between substantive events, never against the total
turn runtime (`--agent-timeout`, default 900s), so a long but steady stream
is never killed — only a session that produces NOTHING is. Such a turn now
ends with the distinct `idle_timeout` outcome and a structured reason (which
window tripped, the window length, the last-event age and the model) instead
of consuming the whole total budget:

```
zoder: ACP idle timeout: the session produced no substantive output for 181.2s
       (window=between_events, window_secs=180.000, last-event age 181.2s, model=qwen38)
```

**Configuration** — each window is set independently, and `0` disables that
window only:

| knob | first-event window | between-events window |
|---|---|---|
| CLI flag | `--agent-idle-timeout-first-event` | `--agent-idle-timeout-between-events` |
| CLI alias | `--agent-idle-timeout` (sets **both**) | `--agent-idle-timeout` (sets **both**) |
| config key | `agent_idle_timeout_first_event_s` | `agent_idle_timeout_between_events_s` |
| config alias | `agent_idle_timeout_s` (sets **both**) | `agent_idle_timeout_s` (sets **both**) |
| env var | `ZODER_AGENT_IDLE_FIRST_EVENT_S` | `ZODER_AGENT_IDLE_BETWEEN_EVENTS_S` |
| env alias | `ZODER_AGENT_IDLE_S` (sets **both**) | `ZODER_AGENT_IDLE_S` (sets **both**) |

Precedence **per window**, highest first: the specific CLI flag > the legacy
CLI alias > the specific config key > the legacy config key > the specific
env var > the legacy env var > the built-in default. The built-in default
follows the ROUTE: a self-hosted (local) route — a loopback or local-network endpoint (including a private LAN
address) or a
self-hosted engine family such as `vllm` / `sglang` / `llamacpp` — gets
600s/180s, and so does a hosted provider, which can queue or prefill a
request for minutes before its first token.
zoder derives the route from the endpoint, so the default is not a guess.

A pending tool call suspends the watchdog (that wait is bounded by the total
turn budget), and the turn is wound down through the existing teardown path
(`session/cancel` + the spawned child's reap), so an idle timeout never
leaves an orphan process behind.

## Per-model request budget

The request budget is a property of the **model**, not only of the host. One
provider routinely serves a fast local reviewer and a slow frontier model, and
the two need opposite budgets: measured 2026-10-21 (HYDRA),
`zoder review --panel MiniMax-M3 ...` on a ~650-line diff was cut twice at the
120s default ("[zoder] still waiting for minimax-m3 after 90s ... request budget
120s" -> "review incomplete") while `gemma4-31b` and
`MiniMax-M3.1-Flash-Preview` answered the same diff. Raising the budget
globally instead would have delayed failure detection for every fast model.

`request_timeout_s` can therefore be set per model. Highest precedence first:

1. `--request-timeout` (per invocation)
2. `request_timeout_s` on the zoder `models` entry serving the model
   (`config.json`)
3. `request_timeout_s` on the engine provider profile serving the model
   (`[providers.models.<family>.<alias>]` in the engine's `config.toml`)
4. the global `request_timeout_s` (`config.json`, or `[profile]` in a
   `config.<vendor>.toml` overlay)
5. `ZODER_TIMEOUT_S`
6. the built-in default, 120s

```toml
# engine config.toml — a real operator knob, per model.
[providers.models.custom.minimax]
model = "MiniMax-M3"
request_timeout_s = 600      # this model alone gets 10 minutes

[providers.models.custom.reviewer]
model = "gemma4-31b"         # no key: keeps the global budget
```

```json
// config.json — the same knob for the zoder-side models map.
{
  "models": {
    "custom.minimax": { "model": "MiniMax-M3", "request_timeout_s": 600 }
  }
}
```

It applies to every model-scoped call: the `zoder review` panel (including the
loop's review phase) and the `zoder exec` / `zoder exec --oneshot` author
chain. A model with no per-model value is completely unaffected, which is the
point — the fast routes keep failing fast.

Two things deliberately keep the global budget: provider catalog fetches
(`list_models`, `zoder refresh`) carry no model, and health probes
(`zoder probe`) are `max_tokens = 1` liveness pings already bounded by
`PROBE_PING_TIMEOUT_S` (20s), which a larger per-model budget could not extend.

A `request_timeout_s` of `0` is rejected at load — on a vendor overlay /
`config.json` as a validation error, on an engine profile as a config parse
error — because a zero budget can never complete a call and silently replacing
it would hide the mistake until the next slow review.

The zoder-side `models` map matches on the model id, so an alias that carries a
budget for a model id another alias also serves is a load-time error — neither
a coin flip between two budgets nor one alias silently hiding the other's.

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
- Pin `request_timeout_s = 600` on the `MiniMax-M3` profile (see "Per-model
  request budget" above) instead of raising the budget host-wide: MiniMax-M3 is
  the one route measured past 120s on a large chunk, and a host-wide bump would
  also delay failure detection for gemma and the local coder.
- Watch `first_token_ms` in review receipts: values of tens of seconds mean
  the reviewer server is saturated; spread sessions across routes rather than
  raising timeouts.

These are recommendations; nothing is applied automatically.

## EIH health gate (before any EIH query)

NVIDIA EIH (`https://integrate.api.nvidia.com/v1`) rate-limits fleet-wide:
a study run saw HTTP 429 on 7.5% of reviewer calls, in bursts, even at one
request in flight. zoder therefore never queries an EIH model blindly:

- Before the first query of a run, and again once the last check is older
  than `ZODER_EIH_HEALTH_TTL_S` (default 300s) or after any 429/5xx/timeout,
  it runs a bounded `GET {base}/models` (`ZODER_EIH_PROBE_TIMEOUT_MS`,
  default 5000) that must list the model. EIH has no per-model status
  endpoint. The "Healthy" table seen in fleet logs is zoder's own
  `~/.zoder/health.json` (`zoder health`), which is where the gate keeps its
  state, so all zoder processes on a host share it.
- A per-model circuit breaker in that store opens on 429, 5xx, timeouts and
  failed probes: 30s backoff doubling to 15 min, never shorter than the
  provider's Retry-After. After the backoff one process claims a half-open
  probe; success closes the breaker.
- An EIH 429 is not retried in-process (that only hammers the shared
  limiter): the breaker opens and the reviewer chain falls to the next
  configured route.
- While the breaker is open the model is skipped without any request. The
  review JSON lists it in `skipped_unhealthy` (model, provider, reason,
  `retry_not_before_unix`), and each EIH receipt carries a `health` object
  (`source`: `cache`, `probe` or `half_open_probe`). A skipped reviewer is
  never a pass: the slot either falls back (its receipt names the serving
  model) or fails, and a panel missing a member stays incomplete.
- `zoder exec --oneshot` skips an unhealthy EIH link in its chain the same
  way; an agentic `zoder exec` whose route is EIH stops with the reason
  (the engine does not switch models by itself).

Routing rules are unchanged: Nemotron is served only by the EIH route.
