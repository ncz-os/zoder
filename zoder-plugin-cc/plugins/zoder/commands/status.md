---
description: Show active and recent Zoder jobs for this repository
argument-hint: '[job-id] [--all]'
disable-model-invocation: true
allowed-tools: Bash(node:*)
---

!`node "${CLAUDE_PLUGIN_ROOT}/scripts/zoder-companion.mjs" status "$ARGUMENTS"`

If the user did not pass a job ID:
- Render the output as a single compact Markdown table (job id, status, kind, started).
- Do not add prose outside the table.

If the user did pass a job ID:
- Present the full command output verbatim. Do not summarize.
