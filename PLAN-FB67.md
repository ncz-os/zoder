# PLAN-FB67 — zoder golden tests for the CLI surface (done)

## Brief
`crates/zoder-cli/tests/` integration tests driving the built `zoder`
binary with fake providers, pinning exit code + stdout/stderr shape for
each D3 failure class: empty diff, no reviewer model, diff over
max_diff_bytes, 0/N reviewers complete, loop unresolved, unconfirmed
cancel. No production changes; new files only.

## What was built
- New: `crates/zoder-cli/tests/failure_contract_golden.rs` (11 tests).
  - `empty_diff_review_is_stable`: clean repo → exit 1,
    `zoder: error: empty_diff:…`, JSON `result == "empty_diff"`,
    `verdict == "request_changes"`, `complete == true`.
  - `no_reviewer_model_is_stable`: provider serves another model →
    exit 1, 0/N marker, summary names the unrouted model.
  - 0/N provider failures (wiremock on the `openai-chat` wire path
    `{base}/v1/chat/completions`): 503, 401, invalid verdict
    (`"invalid JSON twice"` + `invalid_verdict` code), timeout
    (`"request timeout: no response in 2s"`), plus a 503→200
    recovery pinning the success side.
  - `diff_over_max_bytes_is_stable`: exit 1, empty stdout, marker.
  - Loop: `loop_without_engine_config_is_stable` (exit 1, empty
    stdout, `open corpus` marker) and
    `loop_without_agent_route_is_stable` (exit 1,
    `final_verdict == reviewer_unavailable`, author error names the
    missing agent route). True `unresolved` needs a live engine
    daemon (author turns) — reported, not driven.
  - `unconfirmed_cancel_rescue_is_stable`: silent-cancel stand-in
    daemon + `rescue -m … --agent-timeout 2` → exit 1,
    `rescue ended: failed`, daemon-side `cancel_count >= 1`.

## Recon findings (shapes the pins, kept here so the next reader
## doesn't re-derive them)
- `Config::load` schema: provider needs `id` (not `name`),
  `kind: "openai-chat"`, `auth: {"type": "none"}`, plus
  `default_provider`, `corpus_path`, `ledger_path`, `health_path`.
  Engine routes come from `$ZEROCLAW_CONFIG_DIR/config.toml`
  (`[agents.<alias>]` + `[providers.models.<family>.<alias>]`),
  NOT `$HOME/engine/config.toml`. Family `custom` resolves transport;
  unknown families fail the mapping check.
- Corpus routing needs `free + route_candidate + kind == "chat"`.
- A verdict body without `next_steps` fails validation and is
  retried — the recovery 200 MUST include it.
- `rescue`/`loop` author turns need an engine agent route; without a
  daemon they fail with `reviewer_unavailable`, never `unresolved`.
- The reaper (`cancel not confirmed` line) is Linux-only:
  `boot_ticks()` reads `/proc/uptime`, returns `None` off Linux, so
  `ReapPlan::new` is `None` on macOS and the scope is inert. The
  golden pins the portable subset (fail-closed exit + delivered
  cancel); the reaper line is covered by `reaper.rs` unit tests on
  Linux CI.
- Test-harness gotchas: `locate_sibling` checks `~/.local/bin`
  BEFORE `$PATH`, so the test must override `HOME` or a real
  installed `zeroclaw` shadows the stand-in. Socket paths must stay
  short (AF_UNIX limit) — scratch prefix kept tiny.

## Risks
- Timing: cancel test takes ~7–25 s (agent-timeout + settle); no
  exact durations pinned, only shapes and counts.
- Linux CI will execute the reaper path too (extra cancels,
  additional stderr line) — assertions use `>= 1` / `contains` so
  they hold on both platforms.

## Verification
- `cargo test --test failure_contract_golden`: 11 passed.
- `cargo fmt --check` clean. Production code untouched
  (`git status` shows only the two new files).

## Rework on landing (D3 exit ladder, 2026-11-03)
The branch was cut before `b640bc5a` ("give every named code its
documented class and exit code") rewrote the exit ladder, so every
`assert_eq!(…, Some(1))` here pinned the PRE-contract code. Rebased onto
`master` and re-pinned against the published table in `docs/EXIT_CODES.md`:

| Test | Was | Now |
| --- | --- | --- |
| `empty_diff_review_is_stable` | 1 | 50 (`empty_diff` / permanent) |
| `no_reviewer_model_is_stable` | 1 | 20 (`reviewers_unavailable`) |
| 0/N provider tests (`assert_zero_of_n`) | 1 | 20 (transient) |
| `diff_over_max_bytes_is_stable` | 1 | 50 (`diff_too_large` / permanent) |
| `loop_without_engine_config_is_stable` | 1 | 40 (`config`) |
| `loop_without_agent_route_is_stable` | 1 | 20 (`reviewers_unavailable`) |
| `unconfirmed_cancel_rescue_is_stable` | 1 | 1 (unchanged — `rescue` is outside the contract scope, `failure::in_contract_scope`) |

Three assertions pinned host/version state rather than the contract and
were re-anchored:

- `loop_without_engine_config_is_stable` asserted `stderr.starts_with("zoder:
  error: open corpus")`. Two problems: the reap scope may print `[zoder] left
  shared zeroclaw daemon pid=…` BEFORE the error (so the assertion depended on
  whether a host-wide daemon happened to be running), and the error is now
  wrapped as `loading engine config: open corpus …` (`agentic.rs`). It now
  matches the `zoder: error:` LINE and the current message.
- The oversize and loop-config tests asserted `stdout.is_empty()`. Since the
  `emit_json_failure` fix (same commit) a `--json` failure prints exactly ONE
  envelope, so they now pin that envelope's `code`/`class` instead — a
  strictly stronger claim.
- The empty-diff, oversize, loop-config and every 0/N test now also pin the
  typed `failure.code`/`failure.class` handle the fleet worker dispatches on,
  with the exit rung derived from the same class.

Exit codes use named constants (`EXIT_TRANSIENT`, `EXIT_CONFIG`,
`EXIT_PERMANENT`, `EXIT_UNCONTRACTED`) carrying the rung, so the next ladder
change is a one-line edit instead of nine.

`cargo test --workspace` on the rebased branch: 11/11 golden tests pass and
the workspace is green.

