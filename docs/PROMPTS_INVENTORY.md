# zoder prompt inventory

Every place zoder builds, loads, or ships a system / developer / role prompt
today, before the master-prompt work landed. Written from the tree at
`hv/zc-master-prompt-20261031`.

**Scope.** "Prompt" here means a text block that is sent to a model in a
`system` slot, or that establishes the agent's role/contract for a turn. It
does not include: the operator's task text (data, not prompt), the JSON schemas
of tool calls, the MCP tool descriptions (a protocol surface, not a role
prompt), or documentation prose.

**Sizes** are the byte length of the literal prompt text as it exists today,
measured from the source literal (string-literal escapes resolved, line
continuation `\` + newline removed). Where a prompt is assembled from a
constant plus a variable tail, both numbers are given.

**Model targeting** is expressed in zoder's own vocabulary:
`author` = the model that writes code in the loop / `exec` / `run` /
`rescue`; `reviewer` = the model that judges a diff (`zoder review`,
`zoder adversarial-review`, the loop's review phase); `confirmer` = the
second-opinion model in the confirm-on-reject path; `probe` = the health
probe.

---

## 1. Author-side prompts (the "you are an agent" prompts)

| # | File : function / constant | Purpose | Size (bytes) | Sent to | Notes |
|---|---------------------------|---------|--------------|----------|-------|
| A1 | `crates/zoder-cli/src/agentic.rs:8633` — `cmd_loop_run`, iteration-1 author prompt | Tells the author it is "the AUTHOR in an autonomous fix loop", requires a complete correct fix, minimal focused changes, tests where appropriate, and forbids stopping until coherent. Appends the `check` command when the operator supplied one. | 316 (prefix) + task + check | author (every model the router/chain selects) | **The closest thing to a master prompt that exists today.** It is a `format!` inline in the loop body, not a named asset, so it cannot be reviewed, diffed, versioned, or unit-tested. |
| A2 | `crates/zoder-cli/src/agentic.rs:8643` — `cmd_loop_run`, iteration-2+ author prompt | "Continue the SAME fix … The previous attempt was NOT accepted", embeds reviewer feedback and re-states the original task. | 261 (prefix) + feedback + task | author | A **continuation** prompt, not a role prompt. The two are the same role split across two literals that can drift apart. |
| A3 | `crates/zoder-cli/src/codex.rs:792` — `cmd_loop_run`, iteration-1 author prompt | Byte-for-byte copy of A1. | 316 (prefix) + task + check | **nothing — dead code** | **Dead file.** `crates/zoder-cli/src/codex.rs` is present in the tree but is **not declared as a module** in `crates/zoder-cli/src/main.rs`, so it is not compiled and none of its prompts reach a model. Verified: `strings target/debug/zoder` contains the `agentic.rs` reviewer wording and **not** the `codex.rs` wording. Listed so the duplication is visible (it is a trap for the next reader) rather than silently carried. |
| A4 | `crates/zoder-cli/src/codex.rs:802` — `cmd_loop_run`, iteration-2+ author prompt | Byte-for-byte copy of A2. | 261 (prefix) + feedback + task | **nothing — dead code** | Dead file, as A3. |
| A5 | `crates/zoder-cli/src/agentic.rs:6572` — `cmd_rescue_run` | RESCUE mode: reproduce, find the root cause, implement a minimal fix, verify with build/tests, explain root cause and fix. | 247 + task | author | A **mode** overlay on top of the author role. Its duplicate in `crates/zoder-cli/src/codex.rs:577` (**A6**) is dead code. |
| A6 | `crates/zoder-cli/src/codex.rs:577` — `cmd_rescue_run` | Byte-for-byte copy of A5. | 247 + task | **nothing — dead code** | Dead file, as A3. |
| A7 | `crates/zoder-cli/src/main.rs` — `cmd_exec_agentic` / `agentic_turn` | **No author system prompt at all.** The operator's task text is sent verbatim as the user turn. | 0 | author | The `exec` / `run` / `zoder "prompt"` path — the single most-used author path — has **no** role prompt. Whatever role text exists arrives only through the operator's own text or through `AGENTS.md`. |
| A8 | `crates/zoder-core/src/project_instructions.rs:load_project_instructions` → `compose_prompt` | Loads `AGENTS.md` (Codex convention) then `CLAUDE.md` (Claude Code convention) from the repo root and prepends them to the task under the header `# Project instructions (AGENTS.md)`. Capped at 32 KiB. | 0 (operator-authored) + 29-byte header + 8-byte separator | author | The only *repository-supplied* instruction channel today. First match wins; the files are **not** concatenated, so a nested `AGENTS.md` is invisible to zoder. |
| A9 | `crates/acp-client/src/lib.rs:compose_session_prompt` | The ACP-side twin of A8: same header, same separator, same `AGENTS.md`-then-`CLAUDE.md` source. | 0 (operator-authored) + header + separator | author | **Duplication of A8's wire format**, implemented twice on purpose (so `acp-client` stays decoupled from `zoder-core`) and kept in lockstep by parallel test suites. |

## 2. Reviewer-side prompts

| # | File : function / constant | Purpose | Size (bytes) | Sent to | Notes |
|---|---------------------------|---------|--------------|----------|-------|
| R1 | `crates/zoder-cli/src/agentic.rs:4618` — `REVIEW_SYSTEM` | Reviewer role + strict JSON verdict contract (`verdict` / `summary` / `findings` / `next_steps`), with an explicit instruction never to copy the instructions and never to emit prose outside the JSON object. | 791 | reviewer | The canonical reviewer system prompt. |
| R2 | `crates/zoder-cli/src/agentic.rs:4626` — `ADVERSARIAL_SYSTEM` | Adversarial reviewer role: pressure-test logic, name and locate defects only, approve a sound change. Same JSON contract, plus the "withholding approval from correct work is itself a review failure" clause. | 1 186 | reviewer | |
| R3 | `crates/zoder-cli/src/codex.rs:376` — `REVIEW_SYSTEM` | A **shorter, differently-worded** reviewer prompt with the same JSON schema but expressed as a literal schema string rather than prose rules. | 444 | **nothing — dead code** | Dead file, as A3. A live divergence risk only if the file is ever re-wired into the build. |
| R4 | `crates/zoder-cli/src/codex.rs:381` — `ADVERSARIAL_SYSTEM` | The codex-driver twin of R3. | 579 | **nothing — dead code** | Dead file, as A3. |
| R5 | `crates/zoder-cli/src/agentic.rs:6045` — `dispatch_confirmer` | "You are a second-opinion confirmer for a code review finding." | 61 | confirmer | One line. The real instruction is in the user turn (R6). |
| R6 | `crates/zoder-cli/src/review_confirm.rs:1614` — `confirmer_instruction` | The strict-JSON confirmer instruction, with the finding id substituted (never a placeholder), the verbatim-quote requirement, and the `insufficient` disposition. | 539 + finding id | confirmer | Carries the substantive contract; R5 is the role label. |
| R7 | `crates/zoder-core/src/context_opt/review.rs:PARTIAL_DIFF_NOTICE` | Tells a chunked reviewer that each part is a partial diff, so an absent definition is not a defect. | 307 | reviewer | Constant, and hoisted into the cache-stable prefix (see §5). |

## 3. Loop / harness prompts (the "continue the work" prompts)

| # | File : function / constant | Purpose | Size (bytes) | Sent to | Notes |
|---|---------------------------|---------|--------------|----------|-------|
| L1 | `crates/zoder-cli/src/agentic.rs:8633` / `:8643` | The author turn inside `zoder loop` — see A1 / A2. | — | author | The loop is the only place where a *system-level* author instruction exists, and it exists only for the loop. |
| L2 | `crates/zoder-cli/src/agentic.rs:9052` — loop review preamble | "Task under review (context, not part of the diff): …" — the task text handed to the reviewer as context. | 62 + task | reviewer | Not a role prompt; it is the review's *subject*. Listed for completeness because it is assembled text that reaches the model. |
| L3 | `crates/zoder-cli/src/agentic.rs` — `run_loop_review` preamble | The validation-command output and task text that precede the diff in a loop review. | variable | reviewer | Same category as L2. |
| L4 | `crates/zoder-cli/src/agentic.rs:17610` — test fixture | `"You are a reviewer."` | 19 | reviewer | **Test-only literal.** Not production. Listed so the inventory is complete and so a future grep does not mistake it for a live prompt. |
| L5 | `crates/zoder-core/src/bin/context_bench_live.rs:62` — `SYSTEM` | The benchmark's author system prompt. | 167 | author (bench only) | A sixth copy of the "AUTHOR in an autonomous fix loop" idea, in its own wording. |
| L6 | `crates/zoder-core/src/bin/context_bench.rs:169` — bench fixture | `"SYSTEM-PROMPT: You are the AUTHOR in an autonomous fix loop. …"` | 133 | author (bench only) | Bench-only. |

## 4. Operational / machine prompts

| # | File : function / constant | Purpose | Size (bytes) | Sent to | Notes |
|---|---------------------------|---------|--------------|----------|-------|
| O1 | `crates/zoder-core/src/health_probe.rs:50` — `PROBE_PROMPT` | `"ping"` — the liveness payload for the provider health probe. | 4 | probe | Deliberately minimal: the probe measures reachability, not capability. |
| O2 | `crates/zoder-core/src/context_opt/review.rs` — `ChunkMessage::banner` | "This is review part N of M." | ~28 | reviewer | Per-chunk, so it is a *variable* tail, never in the cache prefix. |
| O3 | `crates/zoder-cli/src/review_context.rs` — chunk context header | Tells a chunked reviewer that the supplied file context is not part of the diff and that names in it must not be reported as undefined. | ~200 + context | reviewer | Built per chunk from the diff. |
| O4 | `crates/zoder-cli/src/review_baton.rs` — `focus_sentence` / `head_sentence` | The operator's focus line and the "this is a review of <label>" head, rendered into the cache-stable prefix. | variable | reviewer | |
| O5 | `crates/zoder-mcp-server/src/lib.rs` — tool `description` fields | The MCP tool descriptions (`zoder_route`, `zoder_sop_*`). | ~100 each | MCP client | A protocol surface for a tool-calling client, not an agent role prompt. Listed so the inventory is honest about what does **not** count. |
| O6 | `zoder-plugin-cc/plugins/zoder/agents/zoder-rescue.md` | The Claude Code plugin's forwarder agent: a single Bash call to the companion script, returned verbatim. | 839 | Claude Code (plugin host) | A different product's agent descriptor, not a zoder model prompt. |

## 5. The engine-config side (where a prompt *could* live, and does not)

- `[agents.<alias>]` blocks in the engine config (`~/.zoder/config.toml`, and
  `gitops/fleet/zoder-context.toml` for the fleet) carry `model_provider`,
  `runtime_profile`, `risk_profile`, and `[agents.<alias>.identity]` /
  `.memory` / `.workspace` sub-blocks. **None of them carries a system
  prompt, a persona, or an instruction string.** The identity block is a
  *format* selector (`format = "openclaw"`), not text.
- `crates/zoder-core/src/config.rs` — `Config` (`deny_unknown_fields`)
  therefore has **no** key that could accept a prompt today. Adding one
  requires a new serde field.
- `crates/agent-descriptor/schema/zoder.descriptor.json` describes the
  config surface for external consumers; it lists `knobs` (providers,
  `primary_model`, budgets, …) and **no** prompt knob.
- `crates/zoder-core/src/scenarios.rs` — routing scenarios select *models*,
  not prompts.

**Consequence:** a per-agent prompt is not expressible in the current
config, and a master prompt has to be introduced as a new asset plus a new
config key.

---

## 6. Duplication summary

| Duplicated thing | Copies | Where |
|------------------|--------|-------|
| "AUTHOR in an autonomous fix loop" author prompt | 4 (1 live) | `agentic.rs:8633` (live), `codex.rs:792` (dead), `context_bench_live.rs:62` (bench), `context_bench.rs:169` (bench) |
| Loop iteration-2 continuation prompt | 2 (1 live) | `agentic.rs:8643` (live), `codex.rs:802` (dead) |
| RESCUE-mode prompt | 2 (1 live) | `agentic.rs:6572` (live), `codex.rs:577` (dead) |
| Reviewer system prompt | 2 (divergent wording; 1 live) | `agentic.rs:4618` (live), `codex.rs:376` (dead) |
| Adversarial reviewer system prompt | 2 (divergent wording; 1 live) | `agentic.rs:4626` (live), `codex.rs:381` (dead) |
| AGENTS.md prepend format | 2 | `zoder-core/src/project_instructions.rs::compose_prompt`, `acp-client/src/lib.rs::compose_session_prompt` |
| Confirmer role label | 1 + 1 split | `agentic.rs:6045` (label) + `review_confirm.rs:1614` (contract) |
| JSON verdict schema, restated | 4 (2 live) | R1, R2 (live), R3, R4 (dead) |

### The two structural problems

1. **The author role has no prompt on the main path.** A1 exists only inside
   `zoder loop`. `zoder exec`, `zoder run`, `zoder "<prompt>"` and
   `zoder rescue` send the operator's text with no role instruction at all
   (A7). The behaviour that the fleet relies on — plan briefly, preamble
   before tool calls, `rg` over `grep`, run the tests, do not revert other
   people's changes, escalate destructive commands — is **nowhere** written
   down; it is hoped for.
2. **Nothing is a versioned asset.** Every prompt is an inline literal in a
   `.rs` file. There is no `prompts/` directory, no `v1`/`v2` naming, no
   snapshot test over a prompt, and no per-model overlay. A behavioural
   change to the reviewer contract requires a Rust edit, a rebuild, and a
   fleet rollout; there is no way to A/B two prompt versions.

---

## 7. What the master-prompt work changes

| Change | Effect |
|--------|--------|
| `prompts/master/v1.md` + `prompts/overlays/*.md` as **versioned assets**, loaded by one function (`zoder_core::prompt::assemble`) | The prompt becomes a reviewable, diffable, testable artifact. |
| Per-family overlays for the six families this fleet runs | The known per-model weaknesses get a targeted fix instead of a generic instruction. |
| A config flag (`[prompt].master = true`) that **defaults to the current behaviour** | Nothing changes for an operator who has not flipped it. |
| The author role prompt gains a home outside `zoder loop` | The behaviour contract applies to `exec` / `run` / `rescue` too, once the operator opts in. |

See `docs/PROMPT_EVAL.md` for how the assembled prompts are measured, and
`prompts/README.md` for the asset layout and the overlay contract.
