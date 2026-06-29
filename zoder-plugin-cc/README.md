# zoder-plugin-cc

A Claude Code plugin that brings the Zoder agentic coding + review surface into a
Claude Code terminal session, mirroring `codex-plugin-cc` but driving
**open-source models** through the **zeroclaw** engine, with cost accounting via
`zoder` (internal) or `zoder` (public).

## What it does

Claude plans/implements; Zoder runs an independent reviewer or write-capable
pair-programmer on open-weight models, and every run is metered into the shared
cost ledger.

## Slash commands

| Command | Native subcommand | Purpose |
| --- | --- | --- |
| `/zoder:review` | `review` | Structured code review of local git state (JSON verdict). |
| `/zoder:adversarial-review` | `adversarial-review` | Demanding senior/security review; accepts focus text. |
| `/zoder:rescue` | `rescue` | Agentic, write-capable investigation/fix (subagent). |
| `/zoder:exec` | `exec` | One-shot agentic task in the repo (codex `exec` drop-in). |
| `/zoder:transfer` | `transfer` | Print a resumable engine session id. |
| `/zoder:status` | `status` | Active/recent background jobs. |
| `/zoder:result` | `result` | Stored verdict/output for a finished job. |
| `/zoder:cancel` | `cancel` | Cancel a running background job. |
| `/zoder:setup` | `configure` | Verify the binary + show configuration. |

Multi-reviewer fan-out: `/zoder:review --panel model1,model2` runs several
reviewers concurrently off the same engine and aggregates the verdict.

## How it works

`scripts/zoder-companion.mjs` auto-detects the native binary (`$ZODER_BIN`, then
`zoder`, then `zoder` on `PATH`), maps the plugin subcommand to the native
subcommand, forwards arguments, and streams stdout back verbatim. There is no
hard dependency on either build — install whichever you have.

## Install

```
/plugin marketplace add <path-or-repo-to>/zoder-plugin-cc
/plugin install zoder
```

Then run `/zoder:setup` to confirm the CLI is installed (the Zoder build ships
`zerocode` + `zeroclaw` alongside the CLI).
