# Confirm-on-reject

A reviewer family that raises mostly-false blocking findings makes its
`request_changes` noise — and its `approve` no evidence either. Confirm-on-reject
re-checks every `high` / `critical` finding on a blocking verdict with a model
from a **different family** before it may block.

This document is the user-facing contract. The implementation lives in
`crates/zoder-cli/src/review_confirm.rs`; the config in
`crates/zoder-core/src/config.rs` (`ReviewConfirmConfig`).

## Trigger

Confirmation runs only when a reviewer returns a blocking verdict
(`request_changes` / `reject` / `block`) that survives the deterministic
(Z3-style) filter and still carries at least one `high` or `critical` finding.

- `medium` / `low` / `info` findings never reach confirmation.
- An `approve` is advisory and is never confirmed.

## Pre-check (no model call)

Each blocking finding must quote the text the reviewer was shown. If the quote
is absent, or is not a **verbatim substring** of that text, the finding is
demoted to `filtered_z3` — non-blocking, still reported — and **no model is
called** for it (`quote_found = false`, zero confirmation calls).

## Confirmer choice

The confirmer chain comes from config, not from hard-coded ids in code:

```toml
[review.confirm]
# Dispatch order. The default chain is documented below.
models = ["deepseek-flash", "MiniMax-M3.1-Flash", "nemotron-3-ultra", "mimo-flash"]
max_per_review = 8          # confirmation calls per review
timeout_s = 60              # per-call wall-clock timeout
reasoning_effort = "low"    # confirmers run at low effort
max_context_lines = 200     # cited-file / definition-site window
critical_rejects = 2        # independent rejects to demote a critical (floor 2)
high_rejects = 1            # independent rejects to demote a high (floor 1)
```

A candidate is skipped when its family equals the **reviewer's** family or any
**author** family. This is what stops a family from confirming its own finding.
Every configured gate still applies to the confirmer call: the provider
peak-window gate, the provider quota gate, the health breaker, and the remote
client rules for a model served off-box.

## Confirmer input

The finding, the diff hunk, the cited file at the reviewed head windowed around
the cited lines, and the definition site of every identifier the finding names
(reusing the same name extraction as the Z3 filter). All bounded in lines.

## Confirmer output

Strict JSON:

```json
{ "finding_id": "<id>", "disposition": "confirm | reject | insufficient",
  "quote": "<verbatim text from the context>", "reason": "<one sentence>" }
```

A `confirm` or `reject` MUST include a quote copied verbatim from the context.
No quote — or a quote that is not in the context — is treated as `insufficient`.

## Aggregation

| Confirmer result            | Effect                                   |
|-----------------------------|------------------------------------------|
| `confirm`                   | Finding keeps blocking.                  |
| `reject` (high)             | One **independent** reject demotes it.   |
| `reject` (critical)         | **Two independent** rejects demote it.   |
| `insufficient`              | Finding keeps blocking, unconfirmed.     |
| timeout / `429` / `503`     | Finding keeps blocking, unconfirmed.     |
| empty / unparsed output     | Finding keeps blocking, unconfirmed.     |

**`insufficient` and `confirmer_failed` NEVER demote a finding.** Only an
explicit, quoted `reject` from an independent family demotes. A failed
confirmer is never a rejection. Two rejects count as independent only when they
come from **different families** — two rejects from the same family count once.

If no finding remains blocking, the verdict is no longer `request_changes`: it
becomes `comment`.

### Limits

- At most **8** confirmation calls per review (`max_per_review`).
- **One call per finding, per confirmer.**
- Each confirmer runs at **low** reasoning effort with a **60 s** timeout.

## Record shape v1

The review payload carries a `confirm` array (one entry per confirmed
reviewer). It is **additive**: a record without it still parses, and
`reviewers[0]` stays the first reviewer so the dashboard is unchanged.

```json
{
  "version": 1,
  "effective_blocking": true,
  "verdict_basis": "reviewer_plus_confirmer",
  "verdict": "request_changes",
  "reviewer_family_recall_ref": 0.5,
  "findings": [
    { "id": "<hash>", "severity": "high", "location": "src/a.rs:12",
      "quote": "let x = 1;", "quote_found": true,
      "disposition": "confirmed", "blocking": true, "reason": "" }
  ],
  "demoted": [
    { "id": "<hash>", "severity": "critical", "reason": "false positive",
      "confirmer_ids": ["confirmer-a", "confirmer-b"] }
  ],
  "reviewers": [
    { "model": "<reviewer>", "family": "<fam>", "role": "reviewer",
      "outcome": "ok", "latency_ms": 0, "tokens_in": 0, "tokens_out": 0 },
    { "model": "<confirmer>", "family": "<fam2>", "role": "confirmer",
      "outcome": "ok", "latency_ms": 1200, "tokens_in": 900, "tokens_out": 40 }
  ]
}
```

- **Per finding `id`**: a stable hash of reviewer family + file + the
  normalised claim.
- **Per finding `disposition`** is a closed list:
  `confirmed | rejected | insufficient | confirmer_failed | filtered_z3 | not_run`.
- **Per reviewer entry `outcome`** is a closed list:
  `ok | timeout | 429 | 503 | empty | unparsed`.
- **`demoted`** carries both confirmer ids for a `critical` (the two
  independent rejects that were required).
- **`verdict_basis`** is `reviewer_only` or `reviewer_plus_confirmer`.

`insufficient` and `confirmer_failed` findings are recorded as **blocking**
(`blocking = true`); they are never demotions. Read the `blocking` flag, not
the absence of a `demoted` entry, to decide whether a finding still blocks.
