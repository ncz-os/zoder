# The CI-parity gate — zoder's compliance-first stance

> **Status:** decided + being built. Slice 1 (the gate-planning core) has landed
> (`crates/zoder-core/src/gate.rs`); the runner, CI-file derivation, and loop/review
> wiring are in progress (see [Roadmap](#roadmap)). This document is the design of
> record.

## The position

zoder runs a **full, fail-closed CI simulation** on **both** authoring and code
review. Before a change can converge in the `zoder loop`, and before an adversarial
reviewer can approve it, it must pass the *same* checks the upstream project's CI
will run — plus a baseline of universal open-source hygiene. A change that passes
zoder's gate should not surprise GitHub / GitLab / Codeberg CI, and should already
meet the community's norms.

This is a deliberate stance, and it has a cost: **it slows authoring down.** We
think that trade is worth it, for three reasons:

1. **It's a real differentiator.** General-purpose coding agents (Codex, Cursor,
   and friends) don't simulate the target repo's full CI before proposing a change.
   zoder does. "It passed my gate" becomes an honest, load-bearing claim.
2. **It de-slops the work.** The fingerprints of careless automation — red CI, a
   failing license/audit gate, unformatted code, a missing sign-off — are exactly
   what makes maintainers distrust AI contributions. A change that arrives already
   green and compliant removes those tells. This directly addresses the
   maintainer-friction problem that AI-authored PRs routinely hit.
3. **It respects the community.** Being a good citizen of GitHub, GitLab, and
   Codeberg means running *their* declared CI and *their* hygiene expectations, not
   a tool's own idea of "good enough."

## What the gate is

The gate for a given repository is the **union** of two sources:

### (a) Derived from the repo's own CI
zoder parses the target repo's actual CI configuration and runs the real commands
locally, so **local == upstream CI**:

| Forge | Config parsed |
|---|---|
| GitHub | `.github/workflows/*.yml` (Actions) |
| GitLab | `.gitlab-ci.yml` |
| Codeberg | `.woodpecker.yml` / `.woodpecker/*.yml` (Woodpecker) |

### (b) Baseline open-source hygiene
Applied even when a repo has **no** CI config, ecosystem-detected. Multi-language
by design — not Rust-only:

| Ecosystem | Format | Lint | Build | Test | Security / supply-chain |
|---|---|---|---|---|---|
| Rust | `cargo fmt --all --check` | `cargo clippy -D warnings` | `cargo build` | `cargo test` | `cargo deny check` (req) + `cargo audit` (advisory) |
| Node / TS | `prettier --check` | `npm run lint` | `npm run build` | `npm test` | `npm audit` / `osv-scanner` |
| Python | `ruff format --check` | `ruff check` | `python -m build` | `pytest` | `pip-audit` |
| Go | `gofmt -l` | `go vet` | `go build ./...` | `go test ./...` | `govulncheck` |
| _(more ecosystems + framework detectors to follow)_ | | | | | |

Cross-cutting baseline checks (language-agnostic): **license / SPDX** compliance,
**secret scanning** (gitleaks), **conventional-commits** + **DCO sign-off** (when
the repo requires it), and **SBOM** generation (CycloneDX / Syft) on release gates.

## Two modes

- **`strict` (default, fail-closed).** The loop cannot converge and a reviewer
  cannot approve over a red gate. A **missing required tool is a hard error** with
  an install hint — never a silent skip. This is the mode that earns the "it passed
  my gate" claim.
- **`local-iterate` (fail-open, for inner-loop speed only).** Runs what's present,
  **records every skip in an audit log**, and is **disabled before push**. Never a
  way to sneak past the gate — just a way to iterate fast before the strict gate
  runs for real.

## Honest degradation — Green / Yellow / Red

Some CI jobs genuinely cannot run locally: they need cloud secrets, a GPU, a
self-hosted runner, or service containers that aren't present. The gate never
*silently* passes those — that would be security theater and would break the trust
the gate exists to build. Instead it reports three states:

- **🟢 Green** — every required check ran and passed; nothing was skipped. "Safe to
  merge, within known local scope."
- **🟡 Yellow** — all required checks that *could* run passed, but some were skipped.
  The report names each skip **and the risk it leaves unverified** (e.g. "SKIPPED:
  integration test needs cloud credentials — upstream CI verifies; risk: storage
  layer not exercised locally").
- **🔴 Red** — a required, runnable check failed. Cannot converge / cannot approve.

The claim is **"CI parity within local compute/network scope"** — never false total
parity. A gate report always attaches the compatibility breakdown (runnable / added
baseline / skipped-with-reason). Optional/advisory step failures do **not** turn the
gate red — they're surfaced but don't block.

## Tooling

zoder **pins and manages** the gate tools (cargo-deny, cargo-audit, osv-scanner,
gitleaks, cyclonedx, govulncheck, pip-audit, …) reproducibly — the same way it pins
the goose and zeroclaw engines from source. Strict mode requires them; the managed
install keeps "fail-closed" from becoming "can't work because a tool is missing."

## Roadmap

1. ✅ **Gate planning core** — ecosystem detection, `GateStep` model, baseline
   plans, Green/Yellow/Red aggregation (`crates/zoder-core/src/gate.rs`).
2. **CI-file derivation** — parse Actions / GitLab CI / Woodpecker into `GateStep`s
   with a runnable-vs-skipped compatibility report.
3. **The runner** — execute steps, detect missing tools (hard-error in strict),
   produce `StepResult`s and the aggregated status.
4. **Wiring** — make the gate the default `zoder loop --check`, and ground the
   adversarial reviewer on the gate report (no approve over red).
5. **Managed tool bundle** — pinned install of the gate tools.
