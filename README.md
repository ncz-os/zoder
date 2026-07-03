# zoder

**The full-stack developer's AI pair-coding and headless coding-dispatch system —
free-first, cost-governed, MNEMOS-first.**

## TL;DR — install in one line

```bash
curl -fsSL https://raw.githubusercontent.com/ncz-os/zoder/main/install.sh | sh
```

Detects your OS/arch, verifies the SHA-256 checksum, and installs the
version-matched trio — `zoder` (CLI), `zerocode` (TUI), `zeroclaw` (engine) — to
`~/.local/bin`. Targets: linux-x86_64, linux-aarch64, macOS-arm64 (Windows → WSL).
Pin/automate: `… | ZODER_VERSION=v0.2.0 ZODER_BIN_DIR=~/.local/bin sh`. Full
options, manual, and source builds are [below](#install--build-targets).

**Quick usage:**

```bash
zoder "refactor this for readability" < src/foo.rs   # run a task on the best FREE model
zerocode                                             # interactive pair-coding TUI
zoder review        # multi-model code review → consensus verdict
zoder fix           # review → fix in place → re-review, until it passes
zoder report        # spend + what the free path saved you, in dollars
```

---

zoder is to [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw) what Ubuntu is
to Debian: a curated, opinionated distribution built on the same engine, aimed at
a specific audience. ZeroClaw is the general-purpose agent framework; zoder is the
developer-facing build of it — tuned for two jobs:

1. **Interactive pair-coding** at the terminal (the `zerocode` TUI), and
2. **Headless, automated coding dispatch** — running zoder as a worker in a
   *hive* of agents that pick up coding tasks, run them on the cheapest capable
   model, review, fix, and report cost — with no human in the loop.

It routes work to **free / open-weight models first**, refuses to silently fall
back to a paid backend, tracks every call in a local spend ledger, and produces
FinOps-style chargeback reports that show — in dollars — what the free path saved.

zoder is **vendor-neutral**: it works against any OpenAI-compatible / LiteLLM
endpoint. Free, open-weight providers are first-class; enterprise gateways are
added via config overlays, never hardcoded.

### Relationship to ZeroClaw (a friendly fork)

zoder **consumes ZeroClaw's `master`** and carries its own enhancements as a
clean, rebasing patch stack on the [`ncz-os/zeroclaw`](https://gitlab.com/ncz-os/zeroclaw)
fork (branch `zoder-integration`; see [`docs/VENDORING.md`](docs/VENDORING.md)).
The fork exists because the *objectives* differ, not out of antagonism: we keep
ingesting upstream, and ZeroClaw is free to adopt any of our patches. We do **not**
open PRs upstream — our roadmap lives here. `scripts/package.sh` builds the
engine + `zerocode` UI from the fork and ships them next to the `zoder` binary.

### Part of the ncz-os family — MNEMOS-first

zoder belongs to the **ncz-os** family and is **MNEMOS-first** for memory.
**[MNEMOS](https://github.com/ncz-os/mnemos)** is ncz-os's open-source agent
memory system: a versioned, queryable store (REST + an MCP server) for
long-lived memories and session history, with semantic + full-text search and
pluggable vector backends. It's the system of record that lets a fleet of
agents share context across runs and hosts instead of each keeping scattered
local files.

When a MNEMOS datastore is configured, zoder logs memories and recall there. For
durable session history zoder also ships **database-backed session persistence**
(PostgreSQL / MySQL / Oracle / Db2, feature-gated) — so a hive of headless
workers shares one system of record. See **[Enterprise memory & persistence](#enterprise-memory--persistence)**
for the two wiring paths (MCP-per-agent recall vs. direct DB logging).

---

## Commands

```bash
# Run a coding task — zoder picks the best FREE model automatically.
zoder "refactor this function for readability" < src/foo.rs

# Non-interactive (CI/automation), codex-compatible. `-` reads stdin.
zoder exec -

# Multi-model code review with a consensus verdict.
zoder review

# Review -> agent applies fixes in place -> re-review, until it passes.
zoder fix

# See which model the router would choose (and the fallback chain).
zoder route "write a unit test"

# See your spend and how much the free models saved you.
zoder report
```

| Command | What it does |
|---|---|
| `zoder "<prompt>"` / `zoder exec` | Run a task through the free-first router (codex-compatible; `-` reads stdin). |
| `zoder review` | Fan a code review to a panel of models and reach a consensus verdict. |
| `zoder fix` | Review → agent applies fixes in place → re-review, looping until it passes. |
| `zoder route` | Show the model the router would pick + the cross-family fallback chain. |
| `zoder report` | Usage + chargeback report: daily/weekly/by-model, with the savings headline. Pass `--vendor <name>` to scope to a TOML-defined vendor (e.g. `enterprise`, `ibm`, `microsoft`). |
| `zoder spend` | Raw spend rollups from the local ledger. |
| `zoder models` | List the classified model corpus (free routing pool by default). |
| `zoder health` | Per-model circuit-breaker state + measured latency. |
| `zoder refresh` | Reconcile the corpus against the live served-model list. |
| `zoder sessions` | List saved multi-turn sessions. |
| `zoder config` | Show / validate configuration + corpus. |
| `zoder completions` | Shell completions (bash/zsh/fish/powershell/elvish). |

Everything is **local-first**: routing, the ledger, health, and reports work on
your workstation with no service to stand up.

---

## Why zoder exists

Teams spend real money on frontier LLM APIs while excellent open-weight models
are available for free or near-free. zoder closes that gap by making the free
path the default path:

- **Conserve spend.** Free and open-weight models cost $0. zoder routes to them
  first and only touches a paid frontier model when you explicitly opt in — then
  it shows you, in dollars, exactly what you saved.
- **Exercise open-weight models.** Every task that runs on an open-weight model
  is real-world use of that stack. zoder turns daily coding into continuous,
  measured evaluation of the free fleet.
- **Make cost safety structural, not advisory.** The policy gate is
  **default-deny-paid**. A post-call guard inspects each call's telemetry so a
  model treated as "free" can't silently bill you through a provider-side
  free→paid fallback. A free workflow stays free, provably.

---

## How it works

A single `zoder` run is a short, auditable pipeline:

1. **Classify.** zoder maintains a *corpus* of the models your endpoint serves,
   each tagged free or paid (from live pricing) and scored for capability
   (LMArena Elo + SWE-bench) and measured latency/throughput.
2. **Route.** For your task it picks the **best free model** for the tier,
   skipping any model whose health circuit-breaker is open, and prepares a
   **cross-family fallback chain** so a single provider outage can't strand you.
3. **Run.** The task executes on the chosen model through the zerocore engine
   (streaming, retries, timeouts, sessions).
4. **Verify free.** A post-call guard checks the call's real cost/host
   telemetry. If a "free" model was actually billed, that's a policy violation —
   it's recorded and the run exits non-zero.
5. **Record.** Every call (free included) is appended to a local spend ledger:
   timestamp, provider, model, tokens in/out, cost.
6. **Report.** `zoder report` rolls the ledger up, prices it from a refreshable
   catalog, and reconciles it against your provider's billing.

Paid models are never reached by accident: they are off by default and require
an explicit opt-in.

To route a subscription model first and fall back to a *different* provider's
free models (e.g. a subscription model first, then another provider's free
open-weight endpoints), see
[docs/PROVIDER-ROUTING.md](docs/PROVIDER-ROUTING.md): per-model `serves` routing,
a pinned `primary_model`, and free open-weight ingestion via `zoder refresh`.

---

## Engines: ZeroClaw + Goose (dual-engine)

zoder is an **engine-agnostic governance layer** — the corpus, free-first router,
policy gate, ledger, and review/fix loop govern *whichever* agent actually runs
the turn. Engine selection is one flag:

```bash
zoder exec --engine zeroclaw "…"   # default — the ncz-os fleet engine (daemon)
zoder exec --engine goose    "…"   # Block / Linux-Foundation Goose (goose acp)
```

Both are **Rust agentic coding engines** (memory-safe, fast, single-binary) and
both speak the **Agent Client Protocol (ACP)** — so zoder drives them through one
transport abstraction (Unix-socket for the resident daemon, spawned stdio for
`goose acp`). But they embody **different design principles**, which is exactly
why they complement rather than duplicate each other:

| | **ZeroClaw** (default) | **Goose** |
|---|---|---|
| Process model | long-running **daemon** (resident sessions, Unix socket) | **per-turn subprocess** (`goose acp`, stdio) |
| Built for | the ncz-os **fleet**: cost-ledger-native, configurable agents/aliases, hive/worker orchestration, networked session backends (PG/MySQL/Oracle/Db2/MNEMOS) | broad **single-machine** use: huge **MCP-extension** ecosystem, recipes, and ACP-server interop (Zed / JetBrains / VS Code) |
| Strength | governance, multi-agent fleet, durable networked sessions | provider/extension **breadth**, maturity, community momentum |
| Backed by | ncz-os (a friendly ZeroClaw fork) | Block → **Agentic AI Foundation (Linux Foundation)** |

**Why dual-engine is a valid strategy (not fragmentation):**

- **The governance is the moat; the engine is pluggable.** zoder's value —
  free-first routing, the default-deny-paid gate, the corpus, health-aware
  selection, the cost ledger, the adversarial review/fix loop — is engine-agnostic.
  Adding Goose *widens the substrate zoder governs*; it does not split it.
- **Resilience.** The two engines fail independently. When the resident ZeroClaw
  daemon is mis/un-configured or non-invokable mid-loop, Goose needs no daemon and
  is an immediate fallback (and vice-versa).
- **Engine diversity as a correctness signal.** Two independently-architected
  engines can author + cross-review the same task — engine diversity catches
  engine-specific failure modes, the same way cross-*family* model reviewers catch
  model-specific ones. For the hardest tasks, run both and reconcile.
- **A standards bet, not a Goose bet.** Because the seam is ACP, zoder is really an
  **ACP orchestrator**: any future ACP agent (Zed's registry, others) plugs in for
  free. Goose is simply the first second engine that proves the abstraction.

They are **selectable alternatives, not run simultaneously by default** —
doubling cost buys little on easy/medium work. Simultaneity is opt-in: a
resilience fallback when one engine is degraded, or an ensemble for the hardest
tasks.

The [**systems-architecture diagram**](#systems-architecture) below shows both
engines sitting under zoder's one ACP transport abstraction: `--engine zeroclaw`
(daemon, ACP over a Unix socket) and `--engine goose` (`goose acp`, ACP over a
stdio subprocess), governed identically and fed by the free-first provider pool.

### Goose is bundled *core*, not the kitchen sink

zoder builds and ships goose in a **lean core** configuration —
`cargo build -p goose-cli --bin goose --no-default-features --features rustls-tls`
— and nothing else. That declines goose's default feature stack: `local-inference`
(a full local-model runtime — candle + llama-cpp), `aws-providers` (Bedrock/
SageMaker), `nostr`, goose's own `tui`, `update` (sigstore self-update),
`otel`/`telemetry`, and `system-keyring`.

**Why core:**

- **zoder never invokes any of it.** It drives goose purely as a *remote-API ACP
  agent over stdio*: model calls go to the free-first provider pool (not a local
  GPU), keys come from zoder's config (not the OS keyring), and zoder governs
  telemetry/updates itself. Those features are dead weight in this role.
- **Footprint + build honesty.** Core takes the arm64 binary from **242 MB → 65 MB**
  (−73%) and the from-source build from ~8m → ~3m. The whole zoder stack
  (zoder ≈ 8 MB + zeroclaw ≈ 19 MB + zerocode ≈ 33 MB ≈ **60 MB**) is smaller than a
  single default goose.
- **Smaller surface.** Fewer dependencies and no bundled ML/crypto self-update path
  — less to audit, faster reproducible builds.
- **Still a faithful upstream build.** Core is *feature selection*, not a fork:
  `GOOSE_REF` stays pinned and we patch nothing, so tracking Block/LF goose releases
  stays cheap. (goose *core*'s own `default` feature set is already empty — `[]`; it
  is goose-**cli** that stacks the heavy defaults, and those are what we decline.)

**Want the full goose (local inference, code-mode, AWS, …)? Copy it in.** zoder
spawns whatever `goose` is on `PATH` (`goose acp`), so a heavier build is a pure
drop-in: build it yourself (`--features local-inference`, …) or install Block's
official goose, put it ahead on `PATH`, and zoder drives it identically — no zoder
change. To widen the *bundled* build instead, set `GOOSE_FEATURES` for
`scripts/package.sh`, and re-run the `acp-client` real-goose integration test after
widening (it is the gate that the feature set still drives a live turn).

> Verified: goose core's own test suite passes on the lean feature set (1423/1423
> library tests, ACP module included); the one feature-conditioned snapshot
> (`code-mode`) passes once that feature is re-enabled.

---

## Systems architecture

zoder is layered on the **ZeroClaw / zerocore** foundation (vendored from the
`ncz-os/zeroclaw` fork). Each layer is independently useful; together they make a
complete coding system.

![zoder systems architecture](docs/architecture.svg)

**zerocore — the (pluggable) engine.** ZeroClaw's agent/turn engine and its
OpenAI-compatible provider abstraction is the **default** engine; zoder rides on
the fork's loop and inherits streaming, retries, timeouts, and sessions instead
of reimplementing them. This is the part that actually talks to models. Durable
session history is pluggable: a JSONL file by default, or a **networked database
backend** (PostgreSQL / MySQL / Oracle / Db2) / **MNEMOS** when configured. As of
the dual-engine work the engine itself is pluggable too — `--engine goose` drives
Block/LF **Goose** over ACP instead (see [Engines: ZeroClaw + Goose](#engines-zeroclaw--goose-dual-engine)); zoder's governance is engine-agnostic.

**zerocode — the terminal UX.** The interactive pair-coding experience. It
surfaces cost at the point of decision: a free-vs-paid model picker, per-turn and
session cost, and a live savings readout (one color for $0 work, another for real
paid spend).

**zerocode → zodercode — where the TUI is going (roadmap).** `zerocode` today is a
**single-engine** (ZeroClaw) TUI with **no model selection** — and that is *by
design*: choosing the model is the CLI's job (`zoder` auto-routes free-first). The
dual-engine work opens a gap zerocode was never meant to fill — a terminal user
who wants to **pick the engine and the model directly**, and to see *why* one over
another. That is **zodercode**, zoder's own dual-engine TUI:

- **It consumes zerocode; it does not fork it.** zodercode reuses zerocode's ACP
  chat core (the same client / chat / render layer) and gates the ZeroClaw-specific
  config panes behind `engine == zeroclaw`. Same posture as `zoder`-the-CLI over
  zeroclaw: *wrap and extend, don't re-implement.*
- **Direct engine + direct model choice.** An engine toggle (`zeroclaw` ⇄ `goose`)
  and an explicit model picker — the deliberate counterpart to the CLI's automatic
  routing. The CLI stays the "just do the cheap right thing" path; zodercode is the
  "I know exactly what I want" path.
- **A model-consultant pane** — the thing neither zerocode nor any upstream agent
  TUI has: a live view of the corpus zoder already maintains — per-model **health**
  (circuit-breaker state + measured latency) and **SWE-bench / LMArena** rankings —
  so a human chooses with the *same signals the router uses*.

Positioning, stated plainly:

| | role | status |
|---|---|---|
| `zoder` (CLI) | automatic, free-first, headless / fleet dispatch | **ships today** |
| `zerocode` (TUI) | single-engine interactive pair-coding UX | **ships today** |
| `zodercode` (TUI) | dual-engine · direct model choice · model-consultant pane | **roadmap** (skeleton not yet built) |

zodercode **supersedes zerocode as zoder's branded TUI by *containing* it** — the
same way `zoder`-the-CLI contains the zeroclaw engine. Until zodercode lands,
`zerocode` is the shipping TUI and nothing here is vaporware-by-omission.

**zoder — the governance + dispatch layer.** The cost- and quality-governance
brain. It provides the classified model **corpus**, the **free-first router**
with cross-family fallback, the **default-deny-paid policy gate** with its
anti-paid-fallback guard, per-model **health** (circuit breaker + measured
latency), **multi-model review / agentic fix**, and the **headless dispatch**
surface (`zoder exec -`) that lets a hive of workers run coding tasks
unattended. It is the part that turns ZeroClaw into a *distribution*.

**Pricing engine + FinOps reporting.** A deterministic, conformance-tested
pricing engine (with an optional live LiteLLM/OpenRouter cache) prices every call
so the savings headline and chargeback numbers are real, not estimated.

**Cost accounting + reporting (ledger-backed).** This is what makes the savings
real and visible:

- Every call lands in a local append-only **spend ledger** (timestamp, provider,
  model, tokens, cost) — free calls included, so adoption is measurable.
- A refreshable **pricing catalog** prices each call; free models resolve to $0.
- `zoder report` rolls the ledger up into daily / weekly / by-model views with an
  **avoided-spend** headline and a free-token share.

**Model rankings — LMArena (Elo) + SWE-bench.** Selection is driven by *synced*
rankings, not a hardcoded table: **LMArena Elo** for general capability and
**SWE-bench Verified** for coding skill. Because zoder is a coding tool, the
router ranks **SWE-bench-primary, Elo-secondary**, filtered free-first and
health-aware. Both the model catalog and the rankings refresh periodically and
are cached, so nothing goes stale in the binary.

---

## The CI-parity gate — compliance-first

zoder runs a **full, fail-closed CI simulation** on **both** authoring and code
review. Before a change converges in the `zoder loop` — and before an adversarial
reviewer can approve it — it must pass the *same* checks the upstream project's CI
will run, **plus** a baseline of universal open-source hygiene. A change that
passes zoder's gate shouldn't surprise GitHub / GitLab / Codeberg CI, and should
already meet the community's norms.

This is deliberate, and it costs authoring speed. The trade is worth it:

- **It's a differentiator.** General-purpose coding agents (Codex, Cursor, …) don't
  simulate the target repo's full CI before proposing a change. zoder does — so
  *"it passed my gate"* is an honest, load-bearing claim.
- **It de-slops the work.** Red CI, a failing license/audit gate, unformatted code,
  a missing sign-off — those are the fingerprints of careless automation that make
  maintainers distrust AI contributions. Arriving already green + compliant removes
  the tells, which is the direct antidote to that friction.
- **It respects the community.** Good citizenship on GitHub / GitLab / Codeberg
  means running *their* declared CI and hygiene, not a tool's own "good enough."

The gate = the repo's **own CI** (GitHub Actions / `.gitlab-ci.yml` / Woodpecker,
so local == upstream) **∪** a **multi-language baseline** (Rust, Node/TS, Python,
Go, … — format, lint, build, test, supply-chain/security audit, license/SPDX,
secret scan, conventional-commits + DCO, SBOM on release).

It degrades **honestly** — 🟢 Green (all required ran + passed), 🟡 Yellow (passed
what could run; skips are reported *with the risk they leave unverified*), 🔴 Red (a
required check failed). Jobs that genuinely can't run locally (cloud secrets, GPU,
self-hosted runners) are never silently passed — the claim is *"CI parity within
local compute/network scope,"* never false total parity. Default mode is `strict`
(fail-closed; a missing required tool is a hard error, not a silent skip); a fast
`local-iterate` mode logs every skip and is disabled before push.

**Design of record + roadmap:** [`docs/CI-PARITY-GATE.md`](docs/CI-PARITY-GATE.md).
Slice 1 (the gate-planning core: ecosystem detection, step model, Green/Yellow/Red
aggregation, baseline plans) has landed; the CI-file derivation, runner, and
loop/review wiring are in progress.

## Example: `zoder report`

An example `zoder report` — a **blended organization view**, enumerated **by
model**, across both the **paid AI coding tools** (Claude Code, Cursor, Codex,
Claude.ai) and the **free open-weight models** served via an enterprise gateway.
Of **$337,420 spent**, routing the open-weight slice free **avoided
$1,851,423** at the frontier baseline (`claude-opus-4.8`) — **34× cheaper** on
the free tier, with Claude Code the top cost driver at 74%:

![zoder report — blended paid + free org spend by model: $337,420 paid, $1,851,423 avoided on the free tier](docs/img/zoder-spend.png)

zoder's cost engine is shared with **[tokenomics](https://gitlab.com/ncz-os/tokenomics)**,
the unified LLM-spend ledger across Hermes, Goose, OpenClaw, and zoder. tokenomics
also provides the **FinOps observability** view (spend allocation, realized
$/Mtok, a cheapest-equivalent advisor, and a burn forecast) over the same ledger:

![FinOps observability — totals, by-caller/host, realized $/Mtok by model, burn forecast](docs/img/zoder-finops.png)

> **Subscription & OAuth billing (in progress).** Flat-rate tools — e.g. Codex on
> a ChatGPT subscription, or any provider used via an OAuth login rather than a
> metered API key — report **$0 per token**, so they currently surface as *free*
> even though a fixed subscription fee sits behind them. We're adding
> **subscription-aware cost modeling** — OAuth- and API-key-based subscription
> support for OpenAI, Anthropic, MiniMax, and others — to amortize those flat fees
> across real usage so the report reflects true effective cost.

> tokenomics: repo <https://gitlab.com/ncz-os/tokenomics> (mirror
> <https://github.com/ncz-os/tokenomics>) · package
> [`ncz-tokenomics`](https://pypi.org/project/ncz-tokenomics/).

What the report is telling you at a glance:

- **The counterfactual** — what this period *would* have cost on the frontier
  baseline vs what you actually paid, and the multiple (e.g. "34× cheaper"). This is
  the headline number for free-model adoption.
- **Free token share** — proof the free models are doing real work (token share, not just call count).
- **Top cost driver** — the one model eating most of your remaining spend.

### Vendor-scoped reports: `zoder report --vendor <name>`

To see what your spend looks like against *one* vendor's providers — useful for
chargeback to a team, an org, or a finance review — pass `--vendor`. The flag
filters the ledger to entries whose `provider` id was contributed by the named
vendor's TOML, then **recomputes totals, the counterfactual, and the
avoided-spend headline over that slice**, so the headline numbers are the
vendor's story, not the whole mixed fleet:

```
$ zoder report --days 200 --vendor frontier
ZODER usage report  last 200 days  vendor=frontier  2025-12-11 -> 2026-06-29  (200 days)
filtered to providers:  ai-tools

By day
  day           cost($)  tokens(M)  calls
  ───────────────────────────────────────
  2026-03-15   50613.12    77281.0   773K
  2026-04-15  118097.29   180322.4   1.8M
  2026-05-15   84355.21   128801.7   1.3M
  2026-06-15   84355.21   128801.7   1.3M

Paid models  billed cloud usage — real $ (input/output per Mtok)
  model                            cost($)  in $/Mtok  out $/Mtok  tokens(M)  calls  tok%
  ───────────────────────────────────────────────────────────────────────────────────────
  anthropic/claude-opus-4.8      253219.28       5.00       25.00   346682.5   3.5M   80%
  anthropic/claude-sonnet-4.6     49965.80       3.00       15.00    48313.8   483K   11%
  openai/gpt-5.5                  27428.43       5.00       30.00    31550.6   316K    7%
  google/gemini-3.1-pro-preview    6807.32       2.00       12.00     8417.2    84K    2%

Free models  $0 chargeback
  model                         tokens(M)  calls  tok%  share           
  ──────────────────────────────────────────────────────────────────────
  openai/gpt-5-codex              78506.9   785K   98%  ████████████████
  enterprise-gateway/megamodel     1735.8    17K    2%  █░░░░░░░░░░░░░░░

By host  model publisher, summed across all providers
  host                  cost($)  tokens(M)  calls  tok%
  ─────────────────────────────────────────────────────
  anthropic           303185.08   394996.2   3.9M   77%
  openai               27428.43   110057.5   1.1M   21%
  google                6807.32     8417.2    84K    2%
  enterprise-gateway       0.00     1735.8    17K    0%

Summary
  On claude-opus-4.8 this would cost $11334549.56; you paid $337420.83.
  free models saved you $10997128.73  (34x cheaper)
  free token share    : 16%  (80242.8M of 515206.8M tokens ran $0)
  top cost driver     : anthropic/claude-opus-4.8  75% of spend ($253219.28)
  external chargeback : $337420.83
  free tokens     : 80242.8M  ($0 chargeback)
  avoided spend       : $1765340.55  (80242.8M free tok @ claude-opus-4.8 = $22.00/M)
```

`--vendor` is invalid unless `~/.zoder/config.<name>.toml` is present and
contributes at least one `[[providers]]` entry — zoder exits non-zero with a
clear message listing the vendors it does see. Add a new vendor by copying
`config.ibm.toml` (a commented template) to `config.<name>.toml`, uncomment
the `[[providers]]` blocks, and you're done — no code change.

JSON output includes `vendor` and `vendor_provider_ids` keys when `--vendor`
is set, so dashboards can pin to a specific vendor slice without re-parsing
the by-model table.

---

## Install / build targets

### Install — one line (prebuilt trio: zoder + zerocode + zeroclaw)

```bash
curl -fsSL https://raw.githubusercontent.com/ncz-os/zoder/main/install.sh | sh
```

Detects your OS/arch, downloads the version-matched trio for your platform,
verifies the SHA-256 checksum, and installs `zoder` (CLI), `zerocode` (TUI), and
`zeroclaw` (engine) to `~/.local/bin`.

**Agent / non-interactive** (no prompts; pin the version + dir, fails with a
machine-readable `zoder-install:` message on error):

```bash
curl -fsSL https://raw.githubusercontent.com/ncz-os/zoder/main/install.sh \
  | ZODER_VERSION=v0.2.0 ZODER_BIN_DIR="$HOME/.local/bin" sh
```

Knobs: `ZODER_VERSION` (default `latest`), `ZODER_BIN_DIR` (default
`~/.local/bin`), `ZODER_REPO`, `ZODER_NO_VERIFY=1` (skip checksum).

**Manual:** grab the tarball for your platform from the releases page —
GitLab <https://gitlab.com/ncz-os/zoder/-/releases> or GitHub
<https://github.com/ncz-os/zoder/releases> — verify against `SHA256SUMS`, then:

```bash
tar -xzf zoder-<ver>-<target>.tar.gz
sudo install zoder-<ver>-<target>/{zoder,zerocode,zeroclaw} /usr/local/bin/
zoder --help          # the trio is now on your PATH
```

Targets: `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`,
`aarch64-apple-darwin` (Apple Silicon). Windows → use WSL.

### Build from source

zoder builds natively for:

| Platform | Target |
|---|---|
| macOS arm (Apple Silicon) | `aarch64-apple-darwin` |
| macOS x86_64 (Intel) | `x86_64-apple-darwin` |
| linux x86 | `x86_64-unknown-linux-gnu` |
| linux arm | `aarch64-unknown-linux-gnu` |

- Local build: `./scripts/build.sh mac` (or `mac-x86`, `linux`).
- Release targets: the CI matrix in `.github/workflows/release.yml` (native runners).
- The quality gate (`.github/workflows/quality-gate.yml` on GitHub, `.gitlab-ci.yml`
  on GitLab) runs fmt / clippy `-D warnings` / build+check / nextest / cargo-deny.

### Windows? Use WSL.

**zoder does not ship a native Windows build.** Its engine relies on Unix domain
sockets and a Unix-oriented runtime, and the developer workflows it targets assume
a POSIX environment. Windows users should run zoder inside **WSL2** (Ubuntu or any
glibc distro) — install the Linux target there and it behaves exactly as it does on
native Linux. This is a deliberate ncz-os policy, not a temporary gap: there is no
Windows target in the build or CI matrix.

---

## Configuration

zoder reads `~/.zoder/` (override with `$ZODER_HOME`): `config.json`, the model
corpus, the spend ledger, and the pricing catalog. Without a config it falls
back to a single OpenAI-compatible provider entry you can point at any endpoint.
A paid frontier provider can be added but is **default-deny** and only used on
explicit opt-in.

### Budget caps (pre-call estimate)

Beyond the per-model paid/free gate, zoder can gate on **projected dollars**.
Before a paid call it estimates the cost (prompt tokens × the pricing catalog,
plus an assumed output size) and checks it against optional caps. A call that
would breach a cap prompts the same confirmation as a paid model; `--allow-paid`
bypasses it, and a $0 (free-model) estimate is never gated.

```json
{
  "budget": {
    "max_cost_per_call_usd": 0.50,
    "monthly_cap_usd": 100.0,
    "est_output_tokens": 1024
  }
}
```

- `max_cost_per_call_usd` — confirm any single call estimated above this.
- `monthly_cap_usd` — confirm a call that would push **month-to-date** ledger
  spend (current calendar month) over this total.
- `est_output_tokens` — assumed completion size for the estimate (default 1024).

Omit the `budget` block (or any field) for no cap. The estimate is a ballpark
(token counts are approximate until the call's real telemetry lands in the
ledger); it is a guard rail, not a hard meter.

### Vendor overlays: `config.<name>.toml`

`Config::load()` reads `config.json` (or the default free-tier config) and
then layers every `config.<vendor>.toml` in the same directory on top. Each
TOML is a vendor profile (e.g. `config.enterprise.toml`, `config.ibm.toml`,
`config.microsoft.toml`) that contributes additional `[[providers]]` and,
optionally, a `[profile]` table that selects a `default_provider`. The TOMLs
are the source of truth for what counts as a given org's spend
in `zoder report --vendor <name>`.

```toml
# config.enterprise.toml — an Enterprise gateway profile.
[[providers]]
id = "enterprise-gateway"
base_url = "https://YOUR_ENTERPRISE_GATEWAY/v1"
kind = "openai-chat"
auth = { type = "env", var = "ENTERPRISE_API_KEY" }
# — or, for gateways that use an api-key request header (e.g. Azure OpenAI):
# auth = { type = "api_key_header", header = "api-key", var = "ENTERPRISE_API_KEY" }
paid = true
billing = "metered"

[profile]
name = "enterprise"
# default = false   # leave config.json's default_provider alone
```

Rules the loader enforces (fail loud, don't silently merge):

- **Duplicate provider `id` across two overlays is a hard load error.** Rename
  one in the offending TOML; last-wins is disabled so misconfigurations don't
  masquerade as working installs.
- **At most one overlay may set `[profile].default = true`.** Multiple
  defaults = ambiguous routed-default = error.
- **`[profile].default_provider` must name a provider the same overlay
  contributes.** Otherwise the routed default would point at a non-existent
  provider.
- **An overlay with no `[[providers]]` and no `[profile].default` is an
  error.** A file that does nothing is almost certainly a half-finished
  template — delete it instead of leaving it on disk.

Adding a new vendor is a copy-paste exercise:

1. `cp config.ibm.toml config.<name>.toml` (the IBM/Microsoft templates are
   fully commented out as starters).
2. Fill in `id`, `base_url`, `kind`, and `auth` for each provider. Use the
   matching `ENTERPRISE_API_KEY` / `AZURE_OPENAI_API_KEY` / etc. env var name.
3. Set `paid = true` / `billing = "metered"` for token-billed providers; the
   policy gate uses `paid` to decide whether a call is a "free fallback" or a
   "paid escalation".
4. Drop the file in `~/.zoder/` (or `$ZODER_HOME`). `zoder config --validate`
   will catch the obvious errors and `zoder report --vendor <name>` will pick
   it up automatically — no restart, no re-install.

`free_api_hosts` lives on the base `Config` (not on overlays), so any new
vendor host that should be treated as free for the policy gate still needs to
be added to `config.json`'s `free_api_hosts` array — TOML overlays cannot
extend that list. This is intentional: free-tier policy is a security knob,
not a vendor knob.

**Keep enterprise specifics out of the public tree.** The public zoder source
refers only to a generic **`enterprise`** profile. A real organization's gateway
URLs, auth header names, and report color scheme belong in *its own* private
config repo (e.g. an internal `zoder-config`), kept out of this repository and
synced into `$ZODER_HOME` at deploy time — `scripts/package.sh` honors a
`CONFIG_REPO` hook for exactly this. That keeps vendor identity and endpoints
private while the public code stays vendor-neutral.

### Report color scheme

An overlay's `[theme]` table sets the named color palette `zoder report` uses for
that vendor's reports, so a chargeback view can match an org's brand without any
code change.

---

## Corpus & pricing sync

zoder routes off two data files in `$ZODER_HOME` (`~/.zoder`):

- **`model_corpus.json`** — the classified routing corpus (which models exist,
  which are free/paid, capability/latency signals).
- **`data/pricing.json`** — per-token rates, the cost source for `zoder report`
  / `spend` / `finops`.

Both are **public, self-serve, and rebuilt daily from public price data** — they
do **not** depend on any private/internal build process:

- **Source of truth:** [`corpus/model_corpus.json`](corpus/model_corpus.json) +
  [`pricing/catalog.json`](pricing/catalog.json) in this repo, regenerated daily
  by [`scripts/build-public-corpus.py`](scripts/build-public-corpus.py) from the
  public **LiteLLM** + **OpenRouter** price lists (plus an optional public coding-
  benchmark overlay for tier ranking). A model is marked free/routable only on an
  **explicit** zero public price — unpriced placeholders and enterprise SKUs are
  filtered out.
- **Pulled from:** `https://raw.githubusercontent.com/ncz-os/zoder/main/{corpus/model_corpus.json,pricing/catalog.json}`.

### Keeping it current

| You want… | Run |
|---|---|
| Seed on first install | the installer does it automatically (`--no-corpus` to skip) |
| Refresh rates from public price lists | `zoder pricing sync` |
| Re-pull the latest public corpus | `curl -fsSL https://raw.githubusercontent.com/ncz-os/zoder/main/corpus/model_corpus.json -o ~/.zoder/model_corpus.json` |
| Reconcile the corpus against **your own** endpoint's served models | `zoder refresh` |

The installer seeds both files and leaves any existing copy untouched, so your
own `zoder refresh` / `zoder pricing sync` edits are never clobbered. The same
public artifacts back the tokenomics plugins' pricing, so they stay current on
the same cadence without any shared private dependency. (A built-in TTL
self-heal — auto re-pull when the file is missing or stale — is on the roadmap;
until then use the commands above or re-run the installer.)

> Note: the public corpus marks models free **only** when their *public* price is
> zero. Your working providers (a self-hosted endpoint, a free-tier key, etc.)
> may be free for *you* though they carry a list price — `zoder refresh` against
> your own endpoint, plus local config, is what marks those routable for you.

---

## Enterprise memory & persistence

zoder has two complementary ways to give agents durable, shared memory backed by
your own infrastructure. Use either or both.

### 1. MCP recall via MNEMOS (per-agent chat memory)

Point an agent at a [MNEMOS](https://github.com/ncz-os/mnemos) server as an MCP
tool source — the agent gains `save`/`search` memory tools and recalls across
runs. MNEMOS itself is backed by your enterprise database (PostgreSQL, Oracle,
Db2, MySQL/MariaDB), so this is the path to those engines. MCP grants are
**bundle-scoped and secure-by-default** (omission is not a grant):

```toml
# 1) Declare the MNEMOS MCP server.
[[mcp.servers]]
name = "mnemos"
transport = "http"                       # or stdio (command/args) for the local bridge
url = "https://mnemos.internal/mcp"
headers = { Authorization = "Bearer ${MNEMOS_TOKEN}" }   # secret; sourced from env

# 2) Put it in a bundle.
[mcp_bundles.memory]
servers = ["mnemos"]

# 3) Grant that bundle to the agent(s) that should recall.
[agents.coder]
mcp_bundles = ["memory"]
```

### 2. Direct memory persistence to an enterprise database

Write the engine's memory subsystem straight to a database via the
`[storage.*]` layer, then point `[memory]` (globally) or a single agent at it.
Engine-native backends: **postgres**, **sqlite**, **qdrant**, **markdown**,
**lucid**.

```toml
# Define a Postgres storage instance.
[storage.postgres.work]
url = "postgres://zoder:${PGPASSWORD}@db.internal:5432/zoder_memory"

# Use it for all memory…
[memory]
backend = "postgres.work"

# …or scope it to one agent (per-agent overrides the global default).
[agents.coder.memory]
backend = "postgres.work"
```

A hive of headless workers pointed at the same instance shares one system of
record instead of scattered local files. For enterprise engines beyond Postgres
(Oracle / Db2 / MySQL / MariaDB), use **path 1** — MNEMOS provides those backends
and zoder reaches them over MCP.

---

## License

Apache-2.0. See [`LICENSE`](LICENSE).
