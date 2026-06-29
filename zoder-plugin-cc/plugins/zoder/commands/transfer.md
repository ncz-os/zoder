---
description: Hand off a resumable Zoder engine session for the current working dir
argument-hint: '[-C <dir>] [-m <model>] [--agent <alias>]'
disable-model-invocation: true
allowed-tools: Bash(node:*)
---

!`node "${CLAUDE_PLUGIN_ROOT}/scripts/zoder-companion.mjs" transfer "$ARGUMENTS"`

- The command prints a resumable engine session id and a resume command.
- Present it verbatim so the user can continue the thread with `zoder --session <id> -C <dir> "<next step>"`.
