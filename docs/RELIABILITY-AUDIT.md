# Reliability Audit — agentic + provider loop

Codex-driven reliability audit of the zoder binary and the zeroclaw agentic
engine it drives, focused on **loop reliability**, **provider connectivity**, and
**tool reliability**. Each finding was verified against the source before action.

Two surfaces were audited:

1. **zoder** (this repo) — the cost-aware CLI: provider HTTP layer, the
   engine-RPC turn driver, model routing/health, daemon lifecycle, policy/ledger.
   Findings here are **fixed in this repo** (see status column).
2. **zeroclaw engine** (`ncz-os/zeroclaw`, the vendored `zoder-integration`
   branch) — the agent turn loop, provider adapters, tool execution, transport.
   Findings here are **upstream of this repo**; they are tracked for fixes on the
   fork and re-vendored via `scripts/package.sh` (see `docs/VENDORING.md`).

---

## Part 1 — zoder (fixed in this repo)

| # | Sev | Finding | Status |
|---|-----|---------|--------|
| 1 | BLOCKER | Streaming EOF before any terminal marker (`[DONE]`/`finish_reason`/usage) returned `Ok` — a proxy dropping the SSE connection mid-answer was booked as a successful partial, poisoning health and suppressing fallback. | **Fixed** — terminal-marker tracking + truncation error (`provider.rs`); regression tests added. |
| 2 | BLOCKER | `agentic_cost` swallowed engine `cost/query` failures and returned `$0` on the primary, so a paid alias/fallback escaped the free gate. | **Fixed** — returns `Option`; `None` (after retry) → policy violation under the free guard (`main.rs`). |
| 3 | HIGH | Mid-turn socket disconnect `bail!`ed, discarding streamed text/tool-count/session-id and skipping health. | **Fixed** — returns a partial `AgentRun` (`disconnected`); caller records a health failure (`engine_rpc.rs`). |
| 4 | HIGH | Agentic path built a fallback chain but only ever ran `chain.first()`. | **Fixed** — pre-side-effect fallback loop: falls back only on `run_agent` `Err` (before any streamed output/tool), never after side effects (`main.rs`). |
| 5 | HIGH | Timeout-cancel sent `session/cancel` then read one line and returned — a ghost agent could keep editing; a completed-at-deadline turn was mislabeled. | **Fixed** — bounded drain that parses final frames and waits for engine wind-down (`engine_rpc.rs`). |
| 6 | HIGH | Only a `prompt`-id JSON-RPC error was fatal; a failed `session/approve` was ignored and stalled the turn to the wall-clock budget. | **Fixed** — every error response handled; approval failure ends the turn with preserved partial (`engine_rpc.rs`). |
| 7 | HIGH | Agentic setup/prompt failures returned early via `?` without feeding the breaker — a broken alias stayed "healthy". | **Fixed** — health failure recorded before propagating (`main.rs`). |
| 8 | HIGH | Daemon readiness was connect-only; a socket that accepts but never answers `initialize` was treated as ready; daemon stderr discarded; early child exit waited the full budget. | **Fixed** — `probe_ready` initialize handshake, stderr→`daemon.log`, retained `Child` + `try_wait`, stale-socket unlink (`main.rs`/`engine_rpc.rs`). |
| 9 | HIGH | Reviewer/panel completions bypassed health and returned `Ok` after an unverified paid fallback. | **Fixed** — health recorded; free-policy violation now fails the call unless `--allow-paid` (`codex.rs`). |
| 10 | HIGH | `health --probe` sent through a possibly-metered provider with no gate, no post-call free verification, and no ledger — spending untracked money and marking paid models healthy. | **Fixed** — paid-provider opt-in gate, per-probe `verify_free`, ledger of any nonzero-cost/violating probe (`main.rs`). |
| 11 | MED | `read_result` / `new_session` had no deadline — a daemon that accepts the socket but never answers could hang the loop (only the 900s+ turn guard, or nothing). | **Fixed** — 30s setup-RPC budget; `new_session` bounded (`engine_rpc.rs`). |
| 12 | MED | Agentic health recorded success even on a policy violation. | **Fixed** — violation recorded as a health failure (`main.rs`). |
| 13 | MED | HTTP error-body read (chat) and `list_models` body read were unbounded — a stalled body pinned the call. | **Fixed** — both timeout-bounded (`provider.rs`). |
| 14 | LOW | FinOps `--since`/`--until` silently defaulted a typo'd date to *now*. | **Fixed** — invalid date is now a hard CLI error (`finops.rs`). |

### Tracked for follow-up (zoder)

- **Engine JSON-RPC frame reads are line-unbounded** (`engine_rpc.rs`): a daemon
  streaming bytes without a newline grows the line buffer until the deadline. A
  truly bounded framed reader (cap ~4 MiB, error on exceed) is the proper fix.
- **Health/session/ledger writes are not concurrent-safe** (fixed temp paths,
  read-modify-write): concurrent CLI runs can clobber each other. Wants unique
  temp files + advisory locks (or SQLite).
- **String-based base-URL normalization** edge cases (`/v1/` substring,
  query/fragment): move to `url::Url` parsing.
- **Budget enforcement** is one-shot-only and uses the configured output
  estimate rather than `--max-tokens`; agentic paid runs aren't budget-gated.

---

## Part 2 — zeroclaw engine (upstream; tracked for the fork)

These live in `ncz-os/zeroclaw` (`zoder-integration`). They mirror the same
class of defects found on the zoder side — notably truncated-stream-as-success
and unbounded provider waits — confirming the pattern is systemic across the
RPC boundary.

| # | Sev | Finding | File (zeroclaw) |
|---|-----|---------|-----------------|
| 1 | BLOCKER | Streaming provider calls have no idle guard (only `connect_timeout`); a stalled SSE endpoint hangs the turn forever, never reaching max-iteration/`TurnComplete`. | `agent/turn/provider_call.rs`, `agent/turn/stream_consume.rs`, `providers/compatible.rs` |
| 2 | HIGH | Clean EOF / truncated streams accepted as the final answer (compatible + codex providers). | `providers/compatible.rs`, `providers/openai_codex.rs`, `agent/turn/stream_consume.rs` |
| 3 | HIGH | Stream→non-stream fallback path doesn't apply `step_timeout_secs` — a hung fallback stalls the turn. | `agent/turn/provider_call.rs` |
| 4 | HIGH | RPC outbound backpressure (64-slot queue) can stall turn draining and terminal `session/update` when the client stops reading. | `rpc/local.rs`, `rpc/wss.rs`, `rpc/dispatch.rs`, `api/jsonrpc.rs` |
| 5 | HIGH | Non-MCP tools have no per-tool execution timeout; a hung tool blocks the turn (and `join_all` withholds sibling results). | `agent/tool_execution.rs` |
| 6 | HIGH | Browser `agent-browser` subprocesses can hang forever (no timeout / kill-on-drop). | `tools/browser.rs` |
| 7 | HIGH | OpenAI Codex OAuth 401 not force-refreshed + retried (revoked-but-unexpired token wedges the session). | `providers/auth/mod.rs`, `providers/openai_codex.rs`, `providers/reliable.rs` |
| 8 | MED | MCP reconnect can replay non-idempotent `tools/call` side effects after ambiguous transport close. | `tools/mcp_client.rs`, `tools/mcp_transport.rs` |
| 9 | MED | Codex streaming→non-streaming fallback can double-charge inference. | `providers/openai_codex.rs` |
| 10 | MED | `Retry-After` headers dropped (adapters read only the body); error bodies unbounded. | `providers/reliable.rs`, `providers/lib.rs`, `providers/compatible.rs` |
| 11 | MED | Rate-limit backoff is deterministic, no jitter, caps truncate server cooldowns → thundering herd. | `providers/reliable.rs` |
| 12 | MED | API-key rotation on rate limits is advertised but not applied (retries the exhausted key). | `providers/lib.rs`, `providers/reliable.rs` |
| 13 | MED | **The zoder-facing streamed/RPC path hard-codes `loop_detection_enabled: false`** — a model that ping-pongs tools burns the full iteration cap. Directly degrades zoder agentic loops. | `agent/agent.rs`, `agent/turn/mod.rs`, `agent/turn/results_collect.rs` |
| 14 | LOW | WSS TLS/WebSocket handshake has no deadline (stalled client holds an fd, blocks ephemeral shutdown). | `rpc/wss.rs` |
| 15 | LOW | Local IPC frame reads unbounded. | `rpc/local.rs`, `rpc/dispatch.rs` |

Highest-value fork fixes for zoder users: **#2** (truncation-as-success, same as
zoder #1), **#13** (loop detection disabled on the RPC path), **#1/#5/#6**
(missing stream/tool/subprocess timeouts), **#7** (OAuth 401 refresh).

---

*Generated from codex `gpt-5.5` adversarial audits; each finding verified against
source. Fix commits are on `fix/loop-reliability-and-ci`.*
