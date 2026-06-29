---
description: Verify the Zoder CLI is installed and show its configuration
disable-model-invocation: true
allowed-tools: Bash(node:*)
---

!`node "${CLAUDE_PLUGIN_ROOT}/scripts/zoder-companion.mjs" setup "$ARGUMENTS"`

- If the binary is missing, relay the install instructions verbatim.
- Otherwise relay the configuration summary (providers, config paths, validation).
