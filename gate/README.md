# zerocode pre-submission gate

Local subsystem that derives a destination project's contributor bar from the
project's own CI + contributing docs, runs that full bar on the contributor's
machine, adds the beyond-CI layers (independent adversarial review, runtime
exercise, regression proof), and refuses to open/update a PR/MR until it passes.

Provider-idempotent (GitHub, GitLab, Codeberg/Gitea, Bitbucket, plain git) and
stack-idempotent (Rust, Python, JS/TS, Go, JVM, C/C++, Ruby, …), with a
CI-derived fallback for the rest. Enforces the maintainers' existing standard
earlier — on the workstation, before any shared CI minute or human reviewer.

## Where the bar lives (YAML, not code)

| File | Role |
|------|------|
| `stacks.yaml` | **Stack adapter catalog** — per-language `{fmt,lint,typecheck,test,build}` defaults (the hardened bar). Declarative. |
| `forges.yaml` | **Forge adapter catalog** — per-provider `{open,update,template,labels,request,read,status}` + where each forge's CI lives. Declarative. |
| `gate.toml` (per repo) | **Derived spec** — synthesized from the destination repo's CI + CONTRIBUTING/AGENTS.md, pins the exact commands + toolchain, overrides catalog defaults. |
| `zerocode-gate.py` | **Thin runner** — detects stack(s)+forge, executes the selected phases for the active risk tier, captures validation evidence. The only code; the bar itself is YAML. |
| `hooks/pre-push` | **Enforcement backstop** — blocks `git push` (any caller) until the gate is green. Server-side required checks remain the authoritative floor. |

To add a language or change a command, edit YAML — no recompile.

## Usage

```bash
# Detect stack/forge only:
python3 gate/zerocode-gate.py --repo /path/to/repo --list

# Run the hardened gate (default tier=medium):
python3 gate/zerocode-gate.py --repo /path/to/repo --tier high --evidence ev.json

# Install the push backstop:
ln -sf ../../gate/hooks/pre-push /path/to/repo/.git/hooks/pre-push
```

## Invariant

`local gate green` ⇒ `destination CI green` for the covered legs. Any
local-green/CI-red divergence is a **gate defect to reconcile**, not tolerate
(use the per-repo `gate.toml` to pin the exact CI commands + toolchain). The
optional CI-parity container closes the residual environment gap.

See `RFC` (zerocode local pre-submission gate) for full rationale, enforcement
model, and acceptance criteria.
