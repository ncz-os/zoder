---
description: Show the stored final output (verdict) for a finished Zoder job
argument-hint: '[job-id]'
disable-model-invocation: true
allowed-tools: Bash(node:*)
---

!`node "${CLAUDE_PLUGIN_ROOT}/scripts/zoder-companion.mjs" result "$ARGUMENTS"`

- Present the command output verbatim to the user.
- For a review job this is the structured verdict (verdict / summary / findings / next_steps).
- Do not paraphrase or fix any issues mentioned.
