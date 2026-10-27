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

A reviewer's prose is never a substring of the diff it was shown, so a finding is
not checked by looking for its prose in the reviewed text. It is checked against
the **code it claims to be about**:

- the place it cites must exist at the reviewed head — the path must be there,
  and the line must be in range;
- the code spans it quotes must actually occur in the reviewed text. The spans
  are the **backticked** code in the finding, taken only from the sentences
  that do not propose a fix ("the loop `for x in xs` never breaks" is evidence,
  "it should use `for x in xs`" is a proposal).

A finding is **ungrounded** — dropped to `filtered_z3`, non-blocking, still
reported — when its cited place does not exist, or when **every** code span it
quotes occurs nowhere in the reviewed text. A finding that quotes no code at
all is not ungrounded: there is nothing to disprove, so it reaches a
confirmer. **No model is called** for a dropped finding.

The corpus the spans are searched in is the cited file at the reviewed head
**and every file the diff touches**, at their post-image. The diff text alone
is not the corpus: it shows the changed lines, while the code a finding quotes
may sit anywhere in the touched files.

The rule was chosen from a recorded replay of 137 blocking findings, not from
taste: it drops 15 of the 73 false findings and loses none of the 62 real ones.
The replay is in the tree
(`crates/zoder-cli/tests/data/grounding-replay-20261026.jsonl`) and is driven
through the production path by a test. The full measurement, including the
end-to-end numbers, is in [REVIEW-CONFIRM-ACCEPTANCE.md](REVIEW-CONFIRM-ACCEPTANCE.md).

## Confirmer choice

The confirmer chain comes from config, not from hard-coded ids in code:

```toml
[review.confirm]
# Dispatch order. The default chain is documented below.
models = ["deepseek-flash", "MiniMax-M3.1-Flash", "nemotron-3-ultra", "mimo-flash"]
# Models that authored the code under review. Their families are excluded
# from the confirmer chain.
author_models = []
max_per_review = 8          # confirmation calls per review
timeout_s = 60              # per-call wall-clock timeout
reasoning_effort = "low"    # confirmers run at low effort
max_context_lines = 200     # cited-file / definition-site window
critical_rejects = 2        # independent rejects to demote a critical (floor 2)
high_rejects = 1            # independent rejects to demote a high (floor 1)
recall_eval_ref = "gptoss-review-eval-20261026"   # the evaluation the recall refers to
```

A candidate is skipped when its family equals the **reviewer's** family or any
**author** family. This is what stops a family from confirming its own finding.
The author families come from two places: the `author_models` list above, and
the **pinned** primary author model (the `-m` override, the selected agent's
`model` pin, or `Config::primary_model`). The auto-routed model is **not** a
source: routing says nothing about who wrote the code under review. The
families that were excluded are recorded in the review payload as
`author_families`, so a reader of the record sees them; an empty list says no
author model was known.

Every configured gate still applies to the confirmer call: the provider
peak-window gate, the provider quota gate, the health breaker, and the remote
client rules for a model served off-box.

## Confirmer input

The finding, the diff hunk, the cited file at the reviewed head windowed around
the cited lines, and the **definition site of every identifier the finding
names** (reusing the same name extraction as the Z3 filter) — a confirmer that
is asked about an "undefined symbol" needs to see where the symbol is defined,
or it will agree with the finding. All bounded in lines.

## Confirmer output

Strict JSON — **exactly one object**, no prose around it:

```json
{ "finding_id": "<id>", "disposition": "confirm | reject | insufficient",
  "quote": "<verbatim text from the context>", "reason": "<one sentence>" }
```

The reply must satisfy the contract or it is not a reply at all:

- the trimmed reply must parse as **one** JSON object — prose around it, a JSON
  array, or two objects in a row are all refused;
- `finding_id` must be present and must be **the finding being confirmed** — a
  reply about a different finding can never demote this one;
- `disposition`, `quote` and `reason` must all be present.

A `confirm` or `reject` MUST include a quote copied verbatim from the context.
No quote — or a quote that is not in the context — is treated as `insufficient`.
A reply that satisfies the contract but carries an unknown disposition is also
`insufficient`: the safe default, which keeps the finding blocking.

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

### Rounds

The budget is spent in **rounds**, and the settlement rule lives in one place
(`settlement(attempts, requires)`) that both the driver and the aggregator
read, so the two can never disagree about the same attempts:

- **round 0 is the primary pair**: the first two confirmers of the chain are
  asked **at the same time**, because a confirmation is a race to a decisive
  answer — one confirm settles the finding, and two serial calls cost twice
  the wall clock for one answer;
- **every later round is one tiebreaker**, and a later round is only entered
  while the finding is still **unsettled**;
- a finding a `confirm` settles, or that enough independent rejects demote,
  never reaches another round: the budget is not spent on calls whose answers
  would be discarded.

Every attempt that **completed** is recorded, including the second member of
the primary pair when the first one settled the finding.

### What `not_run` means, and when it blocks

`not_run` is recorded in two different situations, and the `blocking` flag — not
the disposition alone — says what each one means:

| Situation                                             | disposition | blocking |
|-------------------------------------------------------|-------------|----------|
| `medium` / `low` finding, or an `approve` verdict      | `not_run`   | `false`  |
| A blocking finding that never got a confirmation call  | `not_run`   | `true`   |

The second row is deliberate (spec §7): a `high`/`critical` finding that the
confirmation budget could not reach — or for which no confirmer could be
resolved — stays **blocking and unconfirmed**. It is never silently demoted, so
`effective_blocking` stays `true` and the verdict stays `request_changes`.
Read `blocking`, never `disposition == not_run` alone, to decide whether a
finding blocks.

### Limits

- At most **8** confirmation **calls** per review, across all findings
  (`max_per_review`).
- **One call per finding, per confirmer.**
- Each confirmer runs at **low** reasoning effort with a **60 s** timeout. A
  call that exceeds the timeout is recorded as a timeout — a failure, never a
  rejection.

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
  "reviewer_family_recall_ref": "gptoss-review-eval-20261026",
  "reviewer_family_recall": 0.5,
  "author_families": ["<fam3>"],
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
- **`reviewer_family_recall`** is the share of findings that **reached
  confirmation** (i.e. at least one confirmer attempt was made for them) which
  were then demoted by a confirmer reject. A finding that never reached a
  confirmer — an empty chain, or the budget ran out — is excluded from the
  denominator, not counted as a miss. `null` when nothing reached confirmation.
- **`reviewer_family_recall_ref`** is the **id of the versioned evaluation** the
  recall number refers to, not the number itself. A bare fraction cannot be
  checked against anything; a versioned evaluation id can be opened and
  re-measured. It comes from `[review.confirm].recall_eval_ref`.
- **`author_families`** lists the families that were excluded from the
  confirmer chain because they authored the code under review. An empty list
  says no author model was known, so the chain was excluded from the
  reviewer's family only.

`insufficient` and `confirmer_failed` findings are recorded as **blocking**
(`blocking = true`); they are never demotions. Read the `blocking` flag, not
the absence of a `demoted` entry, to decide whether a finding still blocks.
