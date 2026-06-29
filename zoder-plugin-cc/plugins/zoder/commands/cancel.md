---
description: Cancel an active Zoder background job
argument-hint: '[job-id]'
disable-model-invocation: true
allowed-tools: Bash(node:*)
---

!`node "${CLAUDE_PLUGIN_ROOT}/scripts/zoder-companion.mjs" cancel "$ARGUMENTS"`

- Report the cancellation result verbatim. If no job id is given, the most recent running job is cancelled.
