# ADR 0002 — zoder as a codex replacement: behavior-emulation spec + roadmap

Status: Accepted (2026-07-02) · Scope: zoder / zerocode · Owner: Jason Perlow
Supersedes the ad-hoc "loop guardrails" framing — the guardrails ARE codex's execution
model, so we emulate it deliberately rather than re-deriving it.

## Decision

zoder **replaces** `codex exec` for the autonomous author→validate→review→fix loop on
open-weight/subscription models at ~$0. It must **emulate codex's functions and behavior**,
not approximate them. The junk both 12h supervision runs produced (git add -A sweeping
locks/backups, a hallucinated `object_detection` module, syntax-only checks, 1-iteration
"RESOLVED" on broken/unwired code) is exactly the set of guardrails codex HAS and zoder
LACKS. So: port codex's execution-safety model faithfully.

## Codex behavior spec (characterized from codex-cli 0.133.0)

Execution-safety model (why codex doesn't produce junk):
- **Sandbox policy** on every model-generated shell command: `-s {read-only | workspace-write
  | danger-full-access}` (seatbelt on macOS, landlock/seccomp on Linux). Default posture is
  sandboxed; power users opt into `danger-full-access` on trusted boxes.
- **Approval policy**: `-a {untrusted | on-request | never}` — when a proposed command needs
  human approval; escalates on out-of-policy commands. (`~/.codex/config.toml` here:
  `approval_policy="never"`, `sandbox_mode="danger-full-access"` — the trusted-box opt-out.)
- **Writable-dir scoping**: `-C/--cd` working root + `--add-dir` allowlist; writes confined.
- **Diff-based change surface**: the agent's change is a reviewable diff; `codex apply
  <task_id>` applies it as `git apply`. Codex NEVER does `git add -A`. Edits in
  workspace-write are working-tree edits, reviewable before commit.

Interface / functions:
- **Config**: `~/.codex/config.toml` + `-c key=value` dotted TOML overrides + profiles
  (`-p`, layered `--profile-v2`) + `--strict-config`.
- **Model/provider**: `-m`, `--oss` (open-source provider), `--local-provider
  {lmstudio|ollama}`.
- **Output**: `--json` (JSONL events), `--output-schema FILE` (structured final),
  `-o/--output-last-message FILE`.
- **Sessions**: `resume` / `fork`, `--ephemeral` (no session persistence).
- **Hooks**: hook system with per-source trust (`--dangerously-bypass-hook-trust`).
- Subcommands: `exec`, `review`, `apply`, `sandbox`, `mcp-server`.

## Gap analysis (codex behavior → zoder today)

| Codex behavior | zoder today | Gap |
|---|---|---|
| Sandboxed command exec (read-only/workspace-write) | shells `sh -c` / spawns w/ FULL access, no sandbox | **MISSING (P0)** |
| Diff-based change + explicit apply (no git add -A) | loop `git add -A` sweeps everything | **MISSING (P0)** — junk root cause |
| Writable-dir scoping (--add-dir, -C) | none | **MISSING (P0)** |
| Fail-closed validate before "done" | `check=None` counts as RESOLVED (syntax-only) | **MISSING (P0)** — recreates the "green on broken" bug |
| Approval policy (untrusted/on-request/never) | auto-approve allowlist only | PARTIAL (P1) |
| Config + dotted overrides + profiles | config.toml + vendor overlays | PARTIAL (P1) |
| Output --json / --output-schema | text + some json | PARTIAL (P1) |
| Sessions resume/fork/ephemeral | background jobs; engine session continuity | PARTIAL (P2) |
| Hooks + trust | none | MISSING (P2) |

## Roadmap (build order)

**P0 — execution safety (stops the junk; the immediate need):**
1. **Diff-based change surface + scoped commit**: the loop stages only files its own edit
   tool-calls touched, through a reject-list (lockfiles, `*.bak`/`*.orig`, `target/`,
   binaries, untracked-outside-scope). Kills git-add-A sweep. *(Pure-Rust loop change.)*
2. **Fail-closed validate**: `check=None ≡ FAILED`; the loop cannot reach RESOLVED without an
   actually-executed green build+test. Default the loop's check to the #16 `zoder gate`
   engine (dogfood the fail-closed gate we already shipped). *(Pure-Rust loop change.)*
3. **Sandboxed command execution**: run model-generated commands + checks under an OS sandbox
   (macOS `sandbox-exec`/seatbelt; Linux bubblewrap/landlock) with a `--sandbox
   {read-only|workspace-write|danger-full-access}` flag mirroring codex, default
   workspace-write. *(Platform-specific; larger.)*
4. **Writable-dir scoping**: `-C/--root` + `--add-dir`, wired to the sandbox.

**P1:** approval policy (`-a untrusted|on-request|never`); config profiles + `-c` dotted
overrides; `--json`/`--output-schema` output parity.

**P2:** sessions (resume/fork/ephemeral); hooks + trust.

## Bootstrap note (critical)
P0.1 + P0.2 must be built by a RELIABLE path (orchestrator-direct or heavily real-host
Codex-gated) — NOT fire-and-forget through the current unsafe loop, since that's the very
behavior being fixed (fixing the loop with the broken loop is circular). Once P0.1+P0.2 land,
zoder's loop can no longer converge on junk (it refuses git-add-A + refuses non-green), and it
becomes safely self-improving for P0.3+ onward.

## Consequences
- zoder becomes a faithful open-weight `codex exec` replacement, not a worse copy.
- The two 12h-supervision junk failure modes are structurally impossible after P0.
- Ties to ADR 0001: the same execution-safety model is engine-agnostic, so it applies whether
  the loop drives zeroclaw or goose.
