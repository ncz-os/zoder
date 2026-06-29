---
description: Delegate an investigation or fix to the Zoder rescue subagent (agentic, write-capable)
argument-hint: "[--background|--wait] [-m <model>] [--agent <alias>] [--approve all|allowlist|none] [what to investigate, solve, or continue]"
allowed-tools: Bash(node:*), AskUserQuestion, Agent
---

Invoke the `zoder:zoder-rescue` subagent via the `Agent` tool (`subagent_type: "zoder:zoder-rescue"`), forwarding the raw user request as the prompt. The final user-visible response must be Zoder's output verbatim.

Raw user request:
$ARGUMENTS

Execution mode:
- `--background` runs the rescue as a tracked background job; `--wait` (or no flag) runs foreground.
- `--background`/`--wait` are execution flags. Do not treat them as task text.
- `-m`, `--agent`, and `--approve` are runtime-selection flags. Preserve them for the forwarded call but do not treat them as task text.

Operating rules:
- The subagent is a thin forwarder. It should use one `Bash` call to invoke `node "${CLAUDE_PLUGIN_ROOT}/scripts/zoder-companion.mjs" task ...` and return that command's stdout as-is.
- Return the companion stdout verbatim. Do not paraphrase or add commentary.
- Do not ask the subagent to poll `/zoder:status`, fetch `/zoder:result`, or do follow-up work.
- If the companion reports the binary is missing, stop and tell the user to run `/zoder:setup`.
- If the user did not supply a request, ask what Zoder should investigate or fix.
