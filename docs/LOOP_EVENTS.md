# zoder loop events

Doc-only reference for the Hive-visible progress trail and the
`ZODER_LOOP_RESULT` line. Every statement cites the file and function it
comes from. No code is changed by this document.

## Sources

- `crates/zoder-cli/src/mnemos.rs`: `event_body`, `event`, `event_blocking`.
- `crates/zoder-cli/src/agentic.rs`: `loop_result_line_enabled`,
  `LoopEventGuard` (+ `Drop`), `cmd_loop_run` (all `mnemos::event` call
  sites and the `ZODER_LOOP_RESULT` print).
- `crates/zoder-core/src/config.rs`: `fleet_job_workspaces`
  (fleet-profile gate for the job id).

No docs index file exists in `docs/` (directory listing 2026-10-29 shows
only topical `.md` files, no `SUMMARY.md`/`README.md`/`INDEX.md`), so no
index link is added.

## Event envelope (`mnemos.rs:event_body`)

`mnemos.rs:event_body(job_id, seq, phase, payload)` builds one MNEMOS
memory per event (Source: `mnemos.rs:event_body`).

- `content`: `payload.to_string()`, with `phase` inserted into the payload
  object when the payload is a JSON object (Source:
  `mnemos.rs:event_body`).
- `category`: `"projects"` (Source: `mnemos.rs:event_body`).
- `subcategory`: `"zoder-loop-events"` (Source: `mnemos.rs:event_body`).
- `source_agent`: `"zoder"` (Source: `mnemos.rs:event_body`).
- `metadata`: `{ job_id, seq, phase, ts }` where `ts` is
  `SystemTime::now` seconds since `UNIX_EPOCH` (0 on clock error)
  (Source: `mnemos.rs:event_body`).
- `seq` comes from the caller; loop callers use a process-wide
  `EVENT_SEQ: AtomicU32` incremented with `fetch_add(1, SeqCst)` in both
  `event` and `event_blocking` (Source: `mnemos.rs:EVENT_SEQ`, `event`,
  `event_blocking`).

Payloads are small factual summaries, never prompts, diffs, model output,
or credentials (Source: `mnemos.rs:event_body` doc comment).

## When events are emitted (`agentic.rs:cmd_loop_run`)

`cmd_loop_run` computes `event_job_id` once (Source:
`agentic.rs:cmd_loop_run`):

```rust
let fleet_job_workspaces = engine_cfg.profile_defaults().fleet_job_workspaces();
let event_job_id: Option<String> = if fleet_job_workspaces {
    std::env::var("HIVE_JOB_ID").ok()
} else {
    None
};
```

`fleet_job_workspaces()` is true only in fleet deployment mode
(`self.mode.is_fleet()`) (Source: `config.rs:fleet_job_workspaces`).

Then:

- `mnemos.rs:event(job_id, phase, payload)` returns immediately when
  `job_id` is `None` (Source: `mnemos.rs:event` — `let Some(job_id) ...
  else { return }`).
- It also returns immediately when `MNEMOS_URL`/`MNEMOS_TOKEN` are not both
  set (Source: `mnemos.rs:event`).
- `mnemos.rs:event_blocking` has the same two early returns, plus URL
  validation (must parse, scheme `http`/`https`, no embedded credentials)
  (Source: `mnemos.rs:event_blocking`).

Net effect: loop-progress events exist only on a fleet-profile run with a
`HIVE_JOB_ID` and MNEMOS credentials configured; standalone runs and runs
without credentials emit nothing (Source: `agentic.rs:cmd_loop_run`,
`mnemos.rs:event`, `mnemos.rs:event_blocking`).

## Event phases (`agentic.rs:cmd_loop_run`)

All sites pass `event_job_id.as_deref()` (Source: every call below).

| Phase | Payload fields | Call site |
| --- | --- | --- |
| `loop_start` | `brief_bytes`, `max_iters`, `reviewer`, `workspace` | `agentic.rs:cmd_loop_run` (~line 8504) |
| `author_start` | `iteration` | `agentic.rs:cmd_loop_run` (~line 8656) |
| `author_end` | `iteration`, `model`, `cost_usd`, `duration_s`, `status` (`ok`/`timeout`/`error`), `error` (≤300 chars) | `agentic.rs:cmd_loop_run` (~line 8830) |
| `check_end` | `iteration`, `passed` (`check_passed: Option<bool>`) | `agentic.rs:cmd_loop_run` (~line 9229) |
| `review_response` | `iteration`, `reviewer_model`, `chunks`, `bytes`, `status` (`ok`/`unavailable`), `error` (≤300 chars), `verdict`, `blocking_findings` | `agentic.rs:cmd_loop_run` (~line 9305) |
| `teardown` (normal) | `reason` (`resolved`/`author_failed`/`exhausted`, chosen from `resolved`), `resolved`, `iterations`, `max_iters`, `final_verdict`, `cost_usd`, `duration_s`, `last_good_head_sha`, `reviewer_model` | `agentic.rs:cmd_loop_run` (~line 9822) |
| `teardown` (crash) | `reason: "crash"` | `agentic.rs:LoopEventGuard::drop` via `event_blocking` |

`teardown` reason selection: `resolved` when the loop resolved, else
`author_failed` vs `exhausted` (Source: `agentic.rs:cmd_loop_run`
teardown block).

## Fail-soft contract (`mnemos.rs:event`, `event_blocking`)

- Async `event` wraps the POST in `tokio::time::timeout(4s)`; any
  non-`Ok(Ok(_))` prints `eprintln!("[zoder] loop event {phase} not
  recorded")` and returns (Source: `mnemos.rs:event`).
- Blocking `event_blocking` spawns a thread with a 4 s client timeout plus
  `Policy::none()` redirects; `join().unwrap_or(false)` false prints the
  same line (Source: `mnemos.rs:event_blocking`).
- Neither function returns a `Result`; callers ignore the outcome, so an
  event failure never changes the loop result (Source:
  `mnemos.rs:event`/`event_blocking` signatures return `()`, and
  `agentic.rs:cmd_loop_run` never branches on them).
- Tests pin this: `loop_event_is_recorded_with_job_metadata` proves the
  envelope/metadata, and `loop_event_is_fail_soft_when_mnemos_is_unreachable`
  proves an unreachable MNEMOS returns within ~6 s without panic (Source:
  `mnemos.rs:tests`).

`request` itself uses a 10 s client timeout, no redirects, bearer auth, and
never echoes credentials or response bodies in errors (`"MNEMOS request
failed"`, `"MNEMOS returned HTTP {code}"`, URL must be HTTP(S) without
embedded credentials) (Source: `mnemos.rs:request` and its tests
`authenticated_record_and_search`,
`auth_error_does_not_expose_credentials_or_response_body`).

## Crash teardown (`agentic.rs:LoopEventGuard`)

`struct LoopEventGuard { job_id: Option<String>, armed: bool }` (Source:
`agentic.rs:LoopEventGuard`).

- Created armed at loop start with `event_job_id.clone()` (Source:
  `agentic.rs:cmd_loop_run` — `let mut event_guard = LoopEventGuard {
  job_id: event_job_id.clone(), armed: true }`).
- `Drop` writes a `teardown` event with `reason: "crash"` via the blocking
  (non-async, `Drop`-safe) path when still armed (Source:
  `agentic.rs:Drop for LoopEventGuard` calls
  `mnemos::event_blocking(job_id, "teardown", json!({"reason":"crash"}))`).
- Normal teardown disarms first (`event_guard.armed = false` right after
  the async `teardown` event) so the guard does not double-emit (Source:
  `agentic.rs:cmd_loop_run` teardown tail).

## `ZODER_LOOP_RESULT` line (`agentic.rs:loop_result_line_enabled`)

Gating function (Source: `agentic.rs:loop_result_line_enabled`):

```rust
fn loop_result_line_enabled(job_id: Option<&str>, env: Option<&str>) -> bool {
    match env.map(str::trim) {
        Some("0") | Some("false") => false,
        Some("1") | Some("true") => true,
        _ => job_id.is_some(),
    }
}
```

- Emitted when the run has a Hive job id, or when `ZODER_LOOP_RESULT=1`
  (or `true`) asks for it; `ZODER_LOOP_RESULT=0` (or `false`) suppresses it
  even under Hive; standalone runs print nothing extra (Source:
  `agentic.rs:loop_result_line_enabled` doc comment + body).
- Unit tests pin the matrix: `(None,None)->false`, `(Some(job),None)->true`,
  `(None,Some("1"))->true`, `(Some(job),Some("0"))->false`,
  `(None,Some(""))->false` (Source: `agentic.rs:tests` near line 10947).
- Line format: `ZODER_LOOP_RESULT {json}` where json is
  `{"teardown": <teardown object>, "verdicts": [<verdict per iter>]}`
  (Source: `agentic.rs:cmd_loop_run` `result_line`).
- Destination: `eprintln!` (stderr) under `--json` to keep stdout a single
  JSON document, else `println!` (stdout) (Source:
  `agentic.rs:cmd_loop_run` — `if cli.json { eprintln! } else { println! }`).

## How an orchestrator reads the events

Query MNEMOS for `category: "projects"`, `subcategory:
"zoder-loop-events"`, filtering on `metadata.job_id == <HIVE_JOB_ID>`,
ordered by `metadata.seq`; `metadata.phase` and `metadata.ts` give the
timeline (Source: envelope contract in `mnemos.rs:event_body`).

Worked example (placeholder host/token; never commit real values):

```bash
: "${MNEMOS_URL:?set}"; : "${MNEMOS_TOKEN:?set}"; : "${HIVE_JOB_ID:?set}"
curl -sS -m 15 "$MNEMOS_URL/v1/memories/search" \
  -H "Authorization: Bearer $MNEMOS_TOKEN" \
  -H 'Content-Type: application/json' \
  -d "{\"query\":\"$HIVE_JOB_ID\",\"limit\":50,\"subcategory\":\"zoder-loop-events\"}" \
| python3 -c '
import json,sys
d=json.load(sys.stdin)
rows=d if isinstance(d,list) else d.get("memories",d.get("results",[]))
ev=[r for r in rows if (r.get("metadata") or {}).get("job_id")=="'"$HIVE_JOB_ID"'"]
ev.sort(key=lambda r:(r.get("metadata") or {}).get("seq",0))
for r in ev:
    m=r.get("metadata") or {}
    print(m.get("seq"), m.get("phase"), m.get("ts"), r.get("content","")[:160])
'
```

Expected order for a one-iteration success: `loop_start` → `author_start`
→ `author_end(status ok)` → `check_end` → `review_response(verdict
approve)` → `teardown(reason resolved)` (Source: call-site order in
`agentic.rs:cmd_loop_run`). A crash shows only the prefix plus a final
`teardown(reason crash)` from the drop guard (Source:
`agentic.rs:LoopEventGuard::drop`).

## Troubleshooting

| Symptom | Likely cause | What to check |
| --- | --- | --- |
| No `zoder-loop-events` memories | Standalone profile or no job id/creds | `config.rs:fleet_job_workspaces` must be fleet; `HIVE_JOB_ID`, `MNEMOS_URL`, `MNEMOS_TOKEN` set (Source: `agentic.rs:cmd_loop_run`, `mnemos.rs:event`) |
| `[zoder] loop event <phase> not recorded` on stderr | MNEMOS unreachable, 401/5xx, or >4 s | Check URL/scheme/creds, server health; loop result is unaffected (Source: `mnemos.rs:event`, `event_blocking`) |
| Only `teardown(reason crash)` after a prefix | Early `?`/`bail!`/panic before disarm | Fix the crash; the guard trail is working as designed (Source: `agentic.rs:LoopEventGuard::drop`) |
| `ZODER_LOOP_RESULT` missing | Standalone run, or suppressed | Needs `HIVE_JOB_ID` or `ZODER_LOOP_RESULT=1`; `=0`/`false` always suppresses (Source: `agentic.rs:loop_result_line_enabled`, tests) |
| `ZODER_LOOP_RESULT` on stderr not stdout | `--json` mode | By design to keep stdout parseable (Source: `agentic.rs:cmd_loop_run`) |
| `seq` gaps or repeats across processes | `EVENT_SEQ` is per-process `AtomicU32` starting at 0 | Order by `(ts, seq)` per writer; do not assume global contiguity (Source: `mnemos.rs:EVENT_SEQ`) |
