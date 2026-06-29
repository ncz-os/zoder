---
description: Run a one-shot agentic Zoder task in the current repo (codex exec drop-in)
argument-hint: '[-C <dir>] [-m <model>] [--agent <alias>] [--approve all|allowlist|none] <task>'
allowed-tools: Bash(node:*)
---

Run an agentic Zoder task to completion in the working directory (file edits, build/test via the zeroclaw engine).

Raw arguments:
$ARGUMENTS

```bash
node "${CLAUDE_PLUGIN_ROOT}/scripts/zoder-companion.mjs" exec "$ARGUMENTS"
```

- Return the command stdout verbatim.
- This is write-capable: it may edit files in the working directory. Use `--approve none` for a read-only run.
