# zoder failure classes and exit codes

The machine-readable failure contract for `zoder review` and `zoder loop`.

Before 2026-10-31 every failure of `zoder review` / `zoder loop` exited `1`
with free text on stderr. A fleet worker that drives those commands could not
tell a TRANSIENT failure (the provider was down, the engine was unreachable,
the budget reset is pending) from a PERMANENT one (the diff is too large to
ever review, the reviewer pin names a model that does not exist, the command
was misused). The only available signal was a substring match against a
human-readable message — brittle, and impossible to keep stable while the
prose improved.

This document is the stable contract. Every statement cites the file and
function it comes from.

## Sources

- `crates/zoder-cli/src/failure.rs`: the whole vocabulary —
  `FailureClass` (`code`, `exit_code`, `retryable`), `ClassifiedFailure`
  (`to_json`), `ClassifiedFailureError`, `classify_error`,
  `classify_message`, `classify_engine_budget_exceeded`, `error_envelope`.
- `crates/zoder-cli/src/main.rs`: `main` (the exit code), `run` (the `--json`
  failure emission), `emit_json_failure`, `turn_failure_error`.
- `crates/zoder-cli/src/agentic.rs`: `cmd_review`, `cmd_loop`, `cmd_loop_run`,
  `emit_reviews`, `build_review_payload`, `aggregate_verdict`,
  `attach_failure_to_result`, `ChainFailure::into_anyhow`.
- `crates/zoder-cli/src/review_diff.rs`: `prepare_review_diff_with_chunks`,
  `review_diff_chunks`.

## The vocabulary

Nine classes. Each has a STABLE string code, a process exit code, and a
`retryable` flag.

| Class code           | Exit | Retryable | Meaning                                                                                   |
| -------------------- | ---- | --------- | ----------------------------------------------------------------------------------------- |
| `config`             | 65   | no        | The environment is misconfigured: an unreadable/invalid config, a `--reviewer` pin naming an unbacked model, exclusions that emptied the diff. |
| `usage`              | 64   | no        | The command was used incorrectly: a scope that cannot be satisfied, a free-policy refusal, mutually exclusive flags. |
| `budget_exceeded`    | 67   | no        | A budget was exhausted: the per-call cap, the month-to-date spend cap, the reviewer's output budget (twice), or the external engine's own `budget_exceeded:` refusal. |
| `reviewer_unavailable` | 68 | yes       | No reviewer produced a verdict: the chain was exhausted, the reviewer returned no verdict twice, every panel member failed, or the review was incomplete. |
| `review_blocked`     | 69   | no        | The review COMPLETED and blocked the change (`request_changes` / `reject` / `block`). |
| `diff_too_large`     | 66   | no        | The diff is larger than the configured review budget (per-diff bytes, per-hunk bytes, chunk count) or was truncated during acquisition. |
| `cancelled`          | 70   | yes       | The run was cancelled, or a required cancellation was never confirmed by the engine. |
| `engine_unreachable` | 71   | yes       | The agent engine (the `zeroclaw` daemon) could not be reached: not running, stale socket, or it died during startup. |
| `internal`           | 72   | no        | An unclassified internal failure: a bug, or a condition the classification does not cover yet. |

`retryable` answers exactly one question: "is this the same request failing,
or is something in the environment broken?" It is a hint, not a guarantee.

The codes are the CONTRACT. They never change meaning, and a code is never
reused for a different class.

## Exit codes

- `0` — success.
- `1` — ANY failure that is not classified. This is the pre-existing
  behaviour and it stays the default, so every caller that predates this
  contract keeps working unchanged.
- `64..=72` — one code per classified failure class (see the table above).

`64`, `65`, `66` and `69` follow the `sysexits.h` convention (`EX_USAGE`,
`EX_DATAERR`, `EX_NOINPUT`, `EX_UNAVAILABLE`). `67`, `68`, `70`, `71` and `72`
are zoder-specific extensions of the same band.

The exit code and the JSON `class` always agree: the dispatcher in `main`
derives the exit code from the same classified failure that the JSON
carries (`main`: `failure::classify_error(&e)` → `classified.exit_code()`).

## The `--json` shape

In `--json` mode the failure information is JSON, never prose. Two shapes,
depending on whether the command produced a result object first.

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
    "class": "review_blocked",
    "code": "review_blocked",
    "retryable": false,
    "message": "review blocked: aggregate verdict `request_changes` requests changes (a blocking review must not exit 0)"
  }
}
```

The `zoder loop` payload carries the same `failure` object next to its
`kind`, `resolved`, `iterations` and `log` fields.

### The command produced no result

The failure is printed as its own `error` object — the same keys, under
`error` instead of `failure`:

```json
{
  "error": {
    "class": "budget_exceeded",
    "code": "budget_exceeded",
    "retryable": false,
    "message": "budget_exceeded: quota exhausted",
    "budget_reset_at": "2026-11-01T00:00:00Z"
  }
}
```

### The `failure` / `error` object

| Field             | Type    | Always present | Meaning                                                       |
| ----------------- | ------- | -------------- | ------------------------------------------------------------- |
| `class`           | string  | yes            | The stable class code (see the table above).                   |
| `code`            | string  | yes            | The same value as `class`, under the name a caller usually expects. |
| `retryable`       | boolean | yes            | Whether the same invocation may usefully be retried as-is.     |
| `message`         | string  | yes            | The operator-facing human message.                             |
| `budget_reset_at` | string  | no             | The moment the external engine's budget window resets. Present ONLY when the engine reported one; never fabricated. |

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
(`failure.rs`), which maps them to the `budget_exceeded` class and extracts
the reset timestamp when one is present. The recognition runs:

- on the engine's own turn detail, in `turn_failure_error` (`main.rs`), so a
  failed agentic turn carries the class and the timestamp; and
- on the whole error chain, in `classify_error` (`failure.rs`), so a failure
  that reaches the dispatcher without an explicit classification still lands
  in the right class instead of the unclassified bucket.

The reset timestamp is carried verbatim. It is never invented, and it is
absent when the engine did not report one.

## How the classes are assigned

| Failure site                                                    | Class                  |
| --------------------------------------------------------------- | ---------------------- |
| `cmd_review`: the branch diff is empty but the tree is dirty       | `usage`                |
| `cmd_review`: exclusions emptied a non-empty diff                | `config`               |
| `cmd_review`: 0 of N reviewers completed                         | `reviewer_unavailable` |
| `cmd_review`: a partial panel (some reviewers failed)            | `reviewer_unavailable` |
| `cmd_review`: the aggregate verdict blocks the change            | `review_blocked`       |
| `cmd_review` / `cmd_loop`: the reviewer exhausted its output budget twice | `budget_exceeded` |
| `cmd_loop`: the `--reviewer` pin names no served model            | `config`               |
| `cmd_loop`: the author turn violated the free policy             | `usage`                |
| `cmd_loop`: the engine never confirmed the author cancellation    | `cancelled`            |
| `cmd_loop`: the loop ended unresolved                             | derived from the recorded evidence (see below) |
| `review_diff`: the diff is above the byte / hunk / chunk budget   | `diff_too_large`       |
| `review_diff`: the diff was truncated during acquisition          | `diff_too_large`       |
| `main`: the engine config could not be loaded                     | `config`               |
| `main`: the agent engine could not be reached                     | `engine_unreachable`   |
| `main`: the engine's `budget_exceeded:` refusal                   | `budget_exceeded`      |

An UNCLASSIFIED failure stays in the `internal` class for the JSON envelope
and exits `1`. The classification is best-effort and never guesses: an
unrecognized message is simply unclassified.

### The loop's unresolved exit

The loop's exit class is derived from the evidence the loop already records
— the `review_error` / `author_error` of the LAST iteration — so it is the
same text the operator already sees:

- a recorded budget refusal → `budget_exceeded` (with the reset timestamp);
- a recorded reviewer failure → `reviewer_unavailable`;
- a recorded cancellation problem → `cancelled`;
- no iterations at all (the author never ran) → `engine_unreachable`;
- anything else → `internal`.

## Reading the class in a fleet worker

```sh
# The class is the stable signal; the exit code is the convenience.
zoder --json review > review.json; rc=$?
case "$rc" in
  0)  echo "review approved" ;;
  68) echo "reviewer unavailable — retry" ;;
  69) echo "review blocked — do not retry the same diff" ;;
  67) echo "budget exhausted — wait for the reset" ;;
  *)  echo "failed (rc=$rc)" ;;
esac
```

A worker that wants the class rather than the code reads it from the JSON:

```sh
zoder --json review > review.json || true
jq -r '.failure.class // .error.class // "unclassified"' review.json
```
