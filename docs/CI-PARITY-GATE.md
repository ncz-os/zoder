# The CI-parity gate — zoder's compliance-first stance

> **Status:** slices 1-5 of the gate engine have landed (planning core,
> CI derivation, runner, language/framework detectors + honest-
> degradation reporting, and the managed tool bundle + `zoder gate`
> CLI wiring). The `zoder gate` subcommand is operational: `zoder gate
> [--root DIR] [--strict|--local-iterate] [--tools-only|--plan-only]
> [--json]` runs the gate end-to-end against the current repo. Next
> is CI YAML adapters (Actions / GitLab / Woodpecker → `CiJob`) so
> repo-declared CI is unioned into the plan via `derive_plan`, and
> then `zoder loop --check` becomes the gate by default. See
> [Roadmap](#roadmap). This document is the design of record.

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
| Rust | `cargo fmt --all -- --check` | `cargo clippy --all-targets -- -D warnings` | `cargo build --all-targets` | `cargo test --all-targets` | `cargo deny check` (req) + `cargo audit` (advisory) |
| Node / TS (npm) | `npx prettier --check .` | `npm run lint` | `npm run build` | `npm test` | `npm audit --audit-level=high` |
| Node / TS (yarn) | `npx prettier --check .` | `yarn lint` | `yarn build` | `yarn test` | `yarn npm audit --audit-level=high` |
| Node / TS (pnpm) | `npx prettier --check .` | `pnpm run lint` | `pnpm run build` | `pnpm test` | `pnpm audit --audit-level=high` |
| Node / TS (bun) | `bunx prettier --check .` | `bun run lint` | `bun run build` | `bun test` | `bun audit --audit-level=high` |
| Python (pip / setuptools) | `ruff format --check .` | `ruff check .` | `python -m build` | `pytest -q` | `pip-audit` |
| Python (poetry) | `ruff format --check .` | `ruff check .` | `poetry build` | `poetry run pytest -q` | `pip-audit -r requirements.txt` |
| Python (uv) | `ruff format --check .` | `ruff check .` | `uv build` | `uv run pytest -q` | `pip-audit -r requirements.txt` |
| Go | `gofmt -l .` | `go vet ./...` | `go build ./...` | `go test ./...` | `govulncheck ./...` |

The Rust / Node / Python / Go / package-manager defaults above are the
canonical baseline commands the gate picks for a repo when its declared
CI does not cover the relevant category. The package-manager column
(node-default `npm` is omitted because it is the baseline default) lets
the gate match the exact toolchain a project actually uses — a silent
fallback from `pnpm test` to `npm test` for a pnpm project would produce
a CI that diverges from upstream.

Framework hints (Next.js, Nuxt, Vite, Vitest, Jest, Django, Pipenv,
pyproject, uv, go-modules, …) are detected in `detect_frameworks` from
marker filenames alone and surfaced for review; they never replace the
canonical command — frameworks can have broken tooling, and the canonical
command remains the safety floor.

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
2. ✅ **CI-file derivation** — `CiJob` / `CompatibilityReport` classifier that
   produces runnable-vs-skipped breakdown from a repo's declared CI
   (`derive_plan` in `gate.rs`).
3. ✅ **The runner** — `GateEnv`/`run_plan` execute steps, route missing tools
   to Skipped (advisory) or Failed (fail-closed) by mode (`gate.rs`).
4. ✅ **Language/framework detectors beyond Rust + honest-degradation
   reporting** — per-language predicates (`detect_rust`/`detect_node`/
   `detect_python`/`detect_go`), `PackageManager` enum with pnpm/yarn/bun/
   poetry/uv refinement, `detect_frameworks` (Next, Nuxt, Vite, Vitest,
   Jest, Django, Pipenv, pyproject, uv, etc.), `RepoSignals`, and the
   `GateReport` pretty/compact renderer that always attaches the
   compatibility breakdown (even on Green) so the gate's "CI parity
   within local compute/network scope" claim stays honest.
5. ✅ **Managed tool bundle + `zoder gate` CLI** — pinned install catalog
   for the gate tools (cargo-deny v0.16.2, cargo-audit v0.21.4,
   osv-scanner v2.2.1, gitleaks v8.28.0, cyclonedx v1.9.1,
   govulncheck v1.1.4, pip-audit v2.9.0) in
   `crates/zoder-core/src/gate_bundle.rs`, `PathEnv` real `GateEnv`
   impl, `ToolLookup`/`InstallHint`/`ToolProbe` for the "what's
   installed / what's missing" view, and the `zoder gate [--root DIR]
   [--strict|--local-iterate] [--tools-only|--plan-only] [--json]`
   subcommand that detects ecosystems, derives the plan, runs it
   against the managed bundle, and prints the `GateReport`. Exit codes
   follow the fail-closed posture: Green/Yellow → 0, Red → 1. The
   report always attaches the runnable / skipped / added-baseline
   breakdown; the renderer never silently passes a missing required
   tool. CI YAML adapters (Actions / GitLab / Woodpecker → `CiJob`)
   remain the next slice, which will union repo-declared CI into the
   plan via `derive_plan` so the local simulation tracks the upstream
   CI verbatim.
6. **Loop wiring** — make the gate the default `zoder loop --check`,
   and ground the adversarial reviewer on the `GateReport` (no approve
   over Red).
