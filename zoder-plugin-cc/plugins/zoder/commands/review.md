---
description: Run a Zoder code review against local git state (open-source models)
argument-hint: '[--wait|--background] [--base <ref>] [--scope auto|working-tree|branch] [--panel m1,m2] [-m <model>]'
disable-model-invocation: true
allowed-tools: Read, Glob, Grep, Bash(node:*), Bash(git:*), AskUserQuestion
---

Run a Zoder review through the native reviewer (the `zoder`/`zoder` CLI driving the zeroclaw engine).

Raw slash-command arguments:
`$ARGUMENTS`

Core constraint:
- This command is review-only.
- Do not fix issues, apply patches, or suggest that you are about to make changes.
- Your only job is to run the review and return Zoder's output verbatim to the user.

Execution mode rules:
- If the raw arguments include `--wait`, do not ask. Run the review in the foreground.
- If the raw arguments include `--background`, do not ask. Run the review as a tracked background job.
- Otherwise, estimate the review size before asking:
  - For working-tree review, start with `git status --short --untracked-files=all`.
  - Inspect `git diff --shortstat --cached` and `git diff --shortstat`.
  - For base-branch review, use `git diff --shortstat <base>...HEAD`.
  - Recommend waiting only when the change is clearly tiny (1-2 files); otherwise recommend background.
- Then use `AskUserQuestion` exactly once with two options, recommended option first and suffixed `(Recommended)`:
  - `Wait for results`
  - `Run in background`

Argument handling:
- Preserve the user's arguments exactly. Do not strip `--wait`/`--background` yourself; the companion handles them.
- For multi-reviewer panels, pass `--panel model1,model2`.

Foreground flow:
```bash
node "${CLAUDE_PLUGIN_ROOT}/scripts/zoder-companion.mjs" review "$ARGUMENTS"
```
- Return the command stdout verbatim. Do not paraphrase or add commentary. Do not fix anything.

Background flow:
```typescript
Bash({
  command: `node "${CLAUDE_PLUGIN_ROOT}/scripts/zoder-companion.mjs" review "$ARGUMENTS"`,
  description: "Zoder review",
  run_in_background: true
})
```
- The companion forwards `--background` to the native CLI, which prints a job id. Tell the user: "Zoder review started. Check `/zoder:status` for progress and `/zoder:result <id>` for the verdict."
