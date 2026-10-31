# zoder failure classes and exit codes

The machine-readable failure contract for `zoder review` and `zoder loop`.

Before 2026-10-31 every failure of `zoder review` / `zoder loop` exited `1`
with free text on stderr. A fleet worker that drives those commands could not
tell a TRANSIENT failure (the provider was down, the engine was unreachable)
from a PERMANENT one (the diff is too large to ever review, the loop ran out of
iterations) or from a CONFIG one (the reviewer pin names a model that does not
exist, the command was misused). The only available signal was a substring
match against a human-readable message — brittle, and impossible to keep
stable while the prose improved.

This document is the stable contract. Every statement cites the file and
function it comes from.

## Sources

- `crates/zoder-cli/src/failure.rs`: the whole vocabulary —
  `FailureClass` (`as_str`, `exit_code`, `retryable`), `FailureCode`
  (`code`, `class`, `exit_code`, `retryable`), `ClassifiedFailure`
  (`to_json`), `ClassifiedFailureError`, `classify_error`,
  `classify_message`, `classify_engine_budget_exceeded`, `error_envelope`.
- `crates/zoder-cli/src/main.rs`: `main` (the exit code), `run` (the `--json`
  failure emission), `emit_json_failure`, `turn_failure_error`.
- `crates/zoder-cli/src/agentic.rs`: `cmd_review`, `cmd_loop`, `cmd_loop_run`,
  `emit_reviews`, `build_review_payload`, `aggregate_verdict`,
  `attach_failure_to_result`, `ChainFailure::into_anyhow`.
- `crates/zoder-cli/src/review_diff.rs`: `prepare_review_diff_with_chunks`,
  `review_diff_chunks`.

## Two levels: the code and the class

- `code` is the SPECIFIC failure (`FailureCode`, the frozen list below). It is
  what a caller dispatches on, and it never changes meaning.
- `class` is the RETRY POLICY the caller applies (`FailureClass`): one of
  transient, permanent, config, budget or blocked — see the class table below.

Several codes share a class, and the class fixes the exit code.

## The five classes

| Class       | Exit | Retryable | Meaning                                                                                     |
| ----------- | ---- | --------- | ------------------------------------------------------------------------------------------- |
| `transient` | 20   | yes       | The same invocation may succeed later: the provider was down, the engine was unreachable.    |
| `permanent` | 50   | no        | The same invocation fails identically and nothing is misconfigured. Nothing to fix, nothing to retry. |
| `config`    | 40   | no        | An operator must change something: a bad config, a bad `--reviewer` pin, the reviewer's output budget. |
| `budget`    | 30   | no        | A budget window is exhausted. Wait for `retry_after_s` / `resets_at` before retrying.       |
| `blocked`   | 10   | no        | Nothing is broken: the review completed and refused the change.                              |

`retryable` answers exactly one question: "is this the same request failing,
or is something in the environment broken?" It is a hint, not a guarantee. A
`budget` failure becomes retryable once its window resets, which the envelope
says with `retry_after_s`.

## The named codes

Every code below is frozen. It is never renamed, removed, or reused for a
different failure.

| Code                        | Class       | Exit | Meaning                                                                                                   |
| --------------------------- | ----------- | ---- | --------------------------------------------------------------------------------------------------------- |
| `review_blocked`            | `blocked`   | 10   | The review COMPLETED and blocked the change (`request_changes` / `reject` / `block`).                      |
| `reviewers_unavailable`     | `transient` | 20   | No reviewer produced a verdict; `detail.reviewers` names what each model hit.                              |
| `diff_too_large`            | `permanent` | 50   | The diff is above the configured review budget (bytes, hunks, chunks).                                     |
| `diff_truncated`            | `permanent` | 50   | The diff was cut off during acquisition, so the tail was never seen.                                       |
| `empty_diff`                | `permanent` | 50   | The scope produced no diff: nothing was reviewed, so this is NOT an approval.                              |
| `no_reviewer_model`         | `config`    | 40   | No reviewer model could be resolved from `--reviewer`, the agent pin or `[review].reviewer_model`.         |
| `loop_exhausted`            | `permanent` | 50   | The loop stopped without resolving the task.                                                               |
| `reviewer_budget_exhausted` | `config`    | 40   | The reviewer spent its whole output budget (twice) on hidden reasoning. Raise `max_tokens` or lower the reasoning effort. |
| `author_failed`             | `transient` | 20   | The author turn failed outright (no in-place timeout).                                                     |
| `author_timeout`            | `transient` | 20   | The author turn hit its wall-clock budget.                                                                 |
| `policy_violation`          | `config`    | 40   | The run was refused by policy (for example the free-only policy).                                          |
| `engine_budget_exceeded`    | `budget`    | 30   | The EXTERNAL engine refused for budget; `detail.resets_at` when known.                                     |
| `engine_unreachable`        | `transient` | 20   | The agent engine (the `zeroclaw` daemon) could not be reached at all.                                      |
| `cancel_unconfirmed`        | `transient` | 20   | A cancel was never confirmed by the engine; the worktree is tainted.                                       |
| `worktree_tainted`          | `transient` | 20   | The worktree was left in an unknown state (see `cancel_unconfirmed`).                                      |
| `config`                    | `config`    | 40   | An environment / config problem: an unreadable or invalid config, a bad `--engine` value.                  |
| `usage`                     | `config`    | 40   | The invocation itself is wrong: mutually exclusive flags, a scope that cannot be satisfied.                |
| `internal`                  | `permanent` | 50   | A bug, or a failure this contract does not classify yet.                                                    |

Two codes map to a READER-AWARE class on purpose:

- `reviewer_budget_exhausted` is `config`, not `budget`: the reviewer's OUTPUT
  budget was spent twice and never produced a verdict, so an identical retry
  fails identically. The operator must raise `max_tokens` or lower the
  reasoning effort. Only the external engine's own refusal
  (`engine_budget_exceeded`) is the retryable, window-based `budget` class.
- `empty_diff` and `diff_truncated` are `permanent`, not `config`: a wrong
  `--base` or a truncated acquisition is not an operator setting to repair;
  the same invocation fails identically. The messages explain the cause.

## Exit codes

The ladder is a single frozen numbering. One `FailureClass` per rung below
75; `permanent` shares the `internal` rung by design (a permanent failure IS an
internal-class outcome: nothing to fix, nothing to retry).

- `0` — success (approved / resolved).
- `10` — blocked: the review completed and said no.
- `20` — transient: retry the same invocation.
- `30` — budget: wait for the window to reset.
- `40` — config: an operator has to change something.
- `50` — internal / permanent: a bug, an identical repeat failure, or an
  unclassified error.
- `75` — reserved for the oneshot refusal band. Never assigned to a review /
  loop code.

Every classified failure is NON-ZERO, so a caller that only tests "did it
fail" (the fleet worker) is unaffected by the contract.

An UNCLASSIFIED failure (an error the contract cannot classify) prints the
envelope with code `internal` and exits `50`: it is the `permanent` /
`internal` rung, and it is a gap in the contract worth reporting.

The exit code and the JSON `class` always agree: the dispatcher in `main`
derives the exit code from the same classified failure that the JSON
carries (`main`: `failure::classify_error(&e)` → `classified.exit_code()`,
falling back to `UNCLASSIFIED_EXIT_CODE`, which is `50`).

## The `--json` shape

Everything on stdout is exactly ONE JSON document, on success and on failure.
Two failure shapes, depending on whether the command produced a result object
first.

### The command produced a result

The failure travels INSIDE the result document, as a `failure` object. stdout
stays exactly ONE JSON document, which is what every existing consumer
parses.

```json
{
  "verdict": "request_changes",
  "complete": true,
  "requested": 1,
  "ok_models": 1,
  "failed_models": 0,
  "cost_usd": 0.0,
  "reviewers": [ ... ],
  "failure": {
    "code": "review_blocked",
    "class": "blocked",
    "detail": { "message": "review blocked: aggregate verdict `request_changes` requests changes (a blocking review must not exit 0)" }
  }
}
```

The `zoder loop` payload carries the same `failure` object next to its
`kind`, `resolved`, `iterations` and `log` fields, plus the loop's
`teardown.reason`.

### The command produced no result

The failure is printed as its own `error` object — the same keys, under
`error` instead of `failure`:

```json
{
  "ok": false,
  "error": {
    "code": "engine_budget_exceeded",
    "class": "budget",
    "retry_after_s": 3600,
    "detail": {
      "message": "budget_exceeded: quota exhausted",
      "resets_at": "2026-11-01T00:00:00Z"
    }
  }
}
```

### The `failure` / `error` object

| Field                         | Type    | Always present | Meaning                                                        |
| ----------------------------- | ------- | -------------- | -------------------------------------------------------------- |
| `code`                        | string  | yes            | The stable code (see the table above).                          |
| `class`                       | string  | yes            | The retry policy the code carries.                              |
| `retry_after_s`               | integer | no             | Seconds until `resets_at`. Present ONLY when the engine gave a reset timestamp in the future; never fabricated. |
| `detail`                      | object  | yes            | Free-form; `message` is always present.                         |
| `detail.message`              | string  | yes            | The operator-facing human message.                              |
| `detail.reviewers`            | array   | no             | Per-model `{model, err_kind}` for `reviewers_unavailable`.      |
| `detail.resets_at`            | string  | no             | The engine's reset timestamp, verbatim, when it reported one.   |

### Backward compatibility

Existing JSON fields are NOT renamed, removed, or reshaped. The failure
information is purely ADDITIVE:

- The `failure` field is present ONLY on a failure. A successful run has no
  `failure` key at all.
- Every pre-existing field keeps its exact previous meaning and value.
- A non-`--json` run is unchanged: the message goes to stderr, stdout keeps
  its previous content.

## The external engine's budget refusal

The engine (`zeroclaw`) reports a budget refusal on stderr as text beginning
with `budget_exceeded:`, optionally followed by the moment the budget window
resets:

```text
budget_exceeded: monthly spend cap reached
budget_exceeded: quota exhausted, resets at 2026-11-01T00:00:00Z
```

Both shapes are recognized by `classify_engine_budget_exceeded`
(`failure.rs`), which maps them to `engine_budget_exceeded` (class `budget`)
and extracts the reset timestamp when one is present. The recognition runs:

- on the engine's own turn detail, in `turn_failure_error` (`main.rs`), so a
  failed agentic turn carries the code and the timestamp; and
- on the whole error chain, in `classify_error` (`failure.rs`), so a failure
  that reaches the dispatcher without an explicit classification still lands
  on the right code instead of the unclassified bucket.

The reset timestamp is carried verbatim. It is never invented, and it is
absent when the engine did not report one.

## How the codes are assigned

| Failure site                                                                 | Code                        |
| ---------------------------------------------------------------------------- | --------------------------- |
| `cmd_review`: the scope produced no diff (for example `--base` equal to HEAD) | `empty_diff`                |
| `cmd_review`: exclusions emptied a non-empty diff                            | `config`                    |
| `cmd_review`: 0 of N reviewers completed                                     | `reviewers_unavailable`     |
| `cmd_review`: a partial panel (some reviewers failed)                        | `reviewers_unavailable`     |
| `cmd_review`: the aggregate verdict blocks the change                        | `review_blocked`            |
| `cmd_review` / `cmd_loop`: the reviewer exhausted its output budget twice    | `reviewer_budget_exhausted` |
| `cmd_loop`: the `--reviewer` pin names no served model                       | `no_reviewer_model`         |
| `cmd_loop`: the author turn violated the free policy                         | `policy_violation`          |
| `cmd_loop`: the engine never confirmed the author cancellation               | `cancel_unconfirmed`        |
| `cmd_loop`: the worktree was left in an unknown state                        | `worktree_tainted`          |
| `cmd_loop`: the loop ended unresolved                                        | `loop_exhausted`            |
| `review_diff`: the diff is above the byte / hunk / chunk budget              | `diff_too_large`            |
| `review_diff`: the diff was truncated during acquisition                     | `diff_truncated`            |
| `main`: the engine config could not be loaded                                | `config`                    |
| `main`: the agent engine could not be reached                                | `engine_unreachable`        |
| `main`: the engine's `budget_exceeded:` refusal                              | `engine_budget_exceeded`    |
| `main`: the invocation was used incorrectly                                  | `usage`                     |

The classification is best-effort and never guesses: an unrecognized message
stays unclassified, which prints as `internal` and exits `50`.

## Reading the code in a fleet worker

```sh
# The class is the retry policy; the exit code is the convenience.
zoder --json review > review.json; rc=$?
case "$rc" in
  0)  echo "review approved" ;;
  10) echo "review blocked — do not retry the same diff" ;;
  20) echo "transient — retry the same invocation" ;;
  30) echo "budget exhausted — wait for resets_at" ;;
  40) echo "config — an operator must fix something" ;;
  50) echo "permanent / internal — do not retry, report it" ;;
  *)  echo "failed (rc=$rc)" ;;
esac
```

A worker that wants the specific code rather than the class reads it from the
JSON:

```sh
zoder --json review > review.json || true
jq -r '.failure.code // .error.code // "unclassified"' review.json
jq -r '.failure.class // .error.class // "unclassified"' review.json
```
