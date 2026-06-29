---
description: Run a demanding adversarial Zoder review (senior engineer / security auditor)
argument-hint: '[--wait|--background] [--base <ref>] [--scope auto|working-tree|branch] [--panel m1,m2] [-m <model>] [focus text]'
disable-model-invocation: true
allowed-tools: Read, Glob, Grep, Bash(node:*), Bash(git:*), AskUserQuestion
---

Run an adversarial Zoder review: the reviewer acts as a skeptical staff engineer and security auditor, aggressively pressure-testing the change.

Raw slash-command arguments:
`$ARGUMENTS`

Core constraint:
- Review-only. Do not fix issues or apply patches. Return Zoder's output verbatim.

Execution mode rules:
- `--wait` runs foreground; `--background` runs as a tracked job; otherwise ask once via `AskUserQuestion` (Wait vs Background, recommended first).

Argument handling:
- Any non-flag trailing text is treated as extra focus for the reviewer (e.g. "concurrency and auth").
- Use `--panel model1,model2` for multiple adversarial reviewers off the same engine.

Foreground flow:
```bash
node "${CLAUDE_PLUGIN_ROOT}/scripts/zoder-companion.mjs" adversarial-review "$ARGUMENTS"
```
- Return stdout verbatim.

Background flow:
```typescript
Bash({
  command: `node "${CLAUDE_PLUGIN_ROOT}/scripts/zoder-companion.mjs" adversarial-review "$ARGUMENTS"`,
  description: "Zoder adversarial review",
  run_in_background: true
})
```
- Tell the user to check `/zoder:status` and `/zoder:result <id>`.
