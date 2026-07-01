# Vendoring zeroclaw — the zoder delta

zoder is a **downstream consumer** of zeroclaw: `scripts/package.sh` builds the
zeroclaw engine + `zerocode` UI from the **ncz-os fork** and ships them next to
the `zoder` binary. We carry our own enhancements on top of upstream as a
**single squashed delta commit** on the `zoder-integration` branch.

> Historically this was a rebasing one-commit-per-feature stack. It was
> **collapsed to a single delta on 2026-06-28** (operator decision): zoder is a
> pure downstream consumer with no upstream PRs, so per-feature granularity for
> PR-ability no longer earns its keep. The squash was **tree byte-identical** to
> the prior 17-patch head — content unchanged, history simplified.

## The model

- **Fork**: `gitlab.com/ncz-os/zeroclaw` (mirrored to `github.com/ncz-os/zeroclaw`).
- **Integration branch**: `zoder-integration` = `upstream/master` + **one
  squashed commit** (the ncz-os delta). No merge commits, no stack.
- **What's "ours"** is exactly that one commit:

  ```sh
  git log upstream/master..zoder-integration      # one commit: the ncz-os delta
  git diff upstream/master..zoder-integration      # the full net diff
  ```

- zoder vendors this branch: `package.sh` defaults `ZEROCLAW_REF=zoder-integration`.

## What the delta contains

Current base: **`upstream/master` @ `c170d39a8`**. Delta: the single
**`feat(zoder-integration): ncz-os zoder delta over upstream/master`** commit
(squashed 2026-06-28; the head SHA changes whenever the delta is amended — see
`git log upstream/master..zoder-integration`). Bundled features:

| Feature | Origin | Touches |
|---|---|---|
| `feat(cost): offline pricing catalog + cost engine` | ex-#8380 | `crates/zeroclaw-config/src/cost/`, `crates/zeroclaw-runtime/src/agent/cost.rs`, `pricing.json` |
| `fix(cost): atomic-append ledger + concatenated-record recovery` | ex-#8412 | `crates/zeroclaw-config/src/cost/tracker.rs` |
| `feat(ui): panel-plugin system + ReportPanel + theme picker` | ex-#8408 | `apps/zerocode/` (`Mode::Plugin`, `PanelAction`, `ReportPanel`, `theme_picker.rs`, 41 themes) |
| `fix(runtime): treat ENOBUFS as recoverable accept() error` | ex-#8122 | `crates/zeroclaw-runtime/` accept loop |
| `feat(scripts): agent-preflight pre-PR validation gate` | ex-#8016 | `scripts/` preflight |
| `ci(quality-gate): align strict clippy gate with ci-all` | ex-#8020 | CI clippy gate |
| `test(providers): per-agent runtime-option resolution coverage` | ex-#7990 | `crates/zeroclaw-providers/` tests |
| `fix(cost): windowed per-agent cost query` | ncz-os Codex review #6 | `crates/zeroclaw-config/src/cost/tracker.rs`, `crates/zeroclaw-runtime/src/rpc/dispatch.rs` |
| `feat(infra): scaffold network session backends` | ncz-os (handoff) | `crates/zeroclaw-infra/src/session_backend/`, `ChannelsConfig` (+7 fields), orchestrator persist-lock (#7753 async) |
| `feat(infra): postgres/mysql/oracle/db2 session backends` | ncz-os (handoff) | `crates/zeroclaw-infra/` `backend-{postgres,mysql,oracle,db2}` features |
| `feat(pricing): zeroclaw-pricing engine + conformance vectors` | ncz-os (handoff) | `crates/zeroclaw-pricing/` (new crate) |
| `feat(pricing): live LiteLLM+OpenRouter cache with TTL refresh` | ncz-os (handoff) | `crates/zeroclaw-pricing/` (feature-gated live cache) |
| `feat(cost): runtime catalog fallback (resolve_rates → pricing_catalog)` | ncz-os (handoff) | `crates/zeroclaw-runtime/src/agent/{cost.rs,pricing_catalog.rs}` |
| `fix(integration): rustfmt + complete ChannelsConfig test literals` | ncz-os (integration repair) | `apps/zerocode/`, `crates/zeroclaw-runtime/`, `crates/zeroclaw-config/src/schema.rs` |
| `ci(gitlab): GitLab quality gate mirroring upstream ci.yml (linux)` | ncz-os (CI standardization) | `.gitlab-ci.yml`, `.gitlab/merge_request_templates/Default.md` |

(`#8411` theme-pane is intentionally **omitted** — superseded by the picker in #8408.)

**Session-backend features need native client libs** (`backend-postgres` builds
locally; `backend-oracle`/`backend-db2`/`backend-mysql` need Oracle Instant
Client / DB2 CLI / MySQL client at build+runtime). They are all **feature-gated
off by default** so the default build and `--features ci-all` never require them.

### Verification (ULTRA, `cargo +1.93.0`, base `c170d39a8`)

`check --workspace` ✅ · `clippy --workspace --all-targets --features ci-all -D warnings` ✅ ·
`fmt --all --check` ✅ · `test -p zeroclaw-pricing` (4 conformance vectors) ✅ ·
both architecture guards ✅.

## Re-integrate when upstream master shifts

```sh
cd <zeroclaw fork checkout>
git fetch upstream master            # zeroclaw-labs (read-only; never push there)
git checkout zoder-integration
git rebase upstream/master           # replays the single delta onto new master
# resolve any conflicts ONCE; git rerere (enabled in this repo) records the
# resolution so the same conflict auto-resolves next time.
git push --force-with-lease origin zoder-integration   # fork only, never upstream
```

Rebasing one commit is simpler than a stack, but a large net diff can still
conflict on hot files (e.g. the session scaffold touches master's #7753
persist-lock in `orchestrator/mod.rs`). Resolve once; rerere remembers.
`git config rerere.enabled true` is set in the fork checkout — keep it on.

## Add new work to the delta

The branch is **one commit**; fold new work in and keep it one commit:

```sh
git checkout zoder-integration
# build the change directly, or on a feature branch then `git merge --squash <branch>`
git commit --amend --no-edit          # fold into the delta
#   — or, to rebuild the delta cleanly from the base:
#     git reset --soft upstream/master && git commit
git push --force-with-lease origin zoder-integration
```

Then add a row to the feature inventory above. Keep the delta a **single commit**.

## Build / vendor flow

- `scripts/package.sh` clones `ZEROCLAW_REPO@ZEROCLAW_REF` into `.zeroclaw-src`
  (gitignored) and builds the engine + `zerocode`.
- `ZEROCLAW_SRC_DIR=<path>` reuses an existing checkout (skips clone, reuses the
  build cache) — handy for CI and local iteration.
- Heavy zeroclaw builds run **off the dev Mac** (e.g. ULTRA) per fleet policy.

## Vendoring goose (the second engine)

goose is vendored **differently** from zeroclaw: there is **no source delta at
all**. We consume Block/LF goose as an upstream binary built from a pinned ref
with a chosen feature set — pure *feature selection*, no `zoder-integration`-style
commit.

- `scripts/package.sh` clones `GOOSE_REPO@GOOSE_REF` (pinned tag, default
  `v1.39.0`) into `.goose-src` (gitignored) and builds **core only**:
  `cargo build -p goose-cli --bin goose --no-default-features --features "$GOOSE_FEATURES"`
  (`GOOSE_FEATURES` default `rustls-tls`) with `strip`/`thin-lto`/`opt-level=z` via
  `--config` — **242 MB → 65 MB**, ~8m → ~3m. `GOOSE_SRC_DIR=<path>` reuses a
  checkout; `ZODER_SKIP_GOOSE=1` omits goose entirely (lean zeroclaw-only bundle).
- **Why core, and how to get the full build:** see
  [Goose is bundled *core*](../README.md#engines-zeroclaw--goose-dual-engine) in
  the README. Short version: zoder drives goose only as a remote-API `goose acp`
  agent, so `local-inference`/`aws-providers`/`nostr`/`tui`/`update`/`otel`/
  `system-keyring` are dead weight. A heavier goose is a **PATH drop-in** (zoder
  spawns whatever `goose` is on `PATH`); no zoder change needed.
- **The gate:** `crates/acp-client`'s real-goose integration test does a live
  handshake + turn against the built binary. Bump `GOOSE_REF` or widen
  `GOOSE_FEATURES` **only** when that test stays green — it's the contract check
  that the chosen build still drives a real turn. Verified for `v1.39.0` core:
  goose's own suite is 1423/1423 lib tests green on the lean features.

## Rules

- **No upstream PRs.** The delta lives on the fork's `zoder-integration` only.
- **Fork-only pushes** — never push to `zeroclaw-labs`.
- The integration branch is a **single squashed delta** on `upstream/master` —
  keep it one commit. Force-push with `--force-with-lease`.
