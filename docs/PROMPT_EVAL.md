# Prompt eval — running the suite

The master system prompt (`prompts/master/v1.md`) and the per-family
overlays (`prompts/overlays/*.md`) are versioned assets, and zoder can
load them behind a config flag that is **off by default**. Before an
operator flips that flag on a fleet, they need evidence that the assembled
prompt does the job it is supposed to do.

This document is how to get that evidence. It describes:

1. the offline suite (no network, no provider) — the default;
2. the live suite (a real route) — opt-in, and deliberately not run as
   part of the change that introduced the assets;
3. how to read the result and decide whether to enable the flag.

## TL;DR

```sh
# Offline: replays the recorded fixture transcripts. No provider is called.
python3 scripts/prompt-eval.py

# The harness' own tests.
python3 scripts/tests/prompt-eval-tests.py

# Machine-readable output.
python3 scripts/prompt-eval.py --json
```

Exit codes:

| Code | Meaning |
|------|---------|
| 0 | every task passed |
| 1 | at least one task failed — a RESULT, not a harness bug |
| 2 | a harness error (missing fixture, malformed criteria) — a BUG |

The distinction matters. A failing task means the prompt is not doing its
job. A harness error means the measurement is broken and tells you
nothing about the prompt.

## 1. Offline mode (the default)

Offline mode never calls a provider. Each task ships a recorded fixture
transcript (`transcript.jsonl`) and a set of machine-checkable criteria
(`criteria.json`). The harness replays the transcript through a **fake
transport** against a copy of the task's starting repository, then scores
the resulting tree, the tool-call list and the final message.

The fake transport is faithful about the two things the tasks depend on:

- `shell` steps actually run in the working tree;
- `apply_patch` steps are applied as a real **unified diff** (`git apply`),
  not as a whole-file overwrite.

That second point is the reason task 09 (`do-not-revert-others`) is
scoreable at all. An "append a function" edit must not delete the rest of
the file; a transport that could only overwrite would make the task fail
for a reason that has nothing to do with the model under test.

### Scratch space

The harness copies each task's `repo/` into a scratch tree and replays the
transcript there. The scratch lives **inside the repository**
(`.prompt-eval-scratch/` by default; `--scratch PATH` moves it), never on
the platform temp dir. This host's `/tmp` is a tmpfs, and the fleet's job
rule is to stay inside the job workspace. The scratch is removed when the
run ends, so the working tree stays clean for the delivery check.

### The 12 tasks

See `prompts/eval/README.md` for the full table and the criterion kinds.

Every task is scored against the **result** tree — the starting `repo/`
with the transcript's tool calls applied — not against the starting tree.
A task must never be satisfiable by doing nothing: for every task that
ships a `run_tests.sh`, the acceptance script fails on the starting tree
and exits 0 on the result tree. `scripts/tests/prompt-eval-tests.py`
enforces that, so a fixture cannot drift into passing for the wrong
reason.

## 2. Live mode (opt-in)

Live mode sends each task to a real route and scores the result with the
**same criteria**, so an offline number and a live number are comparable.

```sh
export ZODER_PROMPT_EVAL_LIVE=1
export ZODER_PROMPT_EVAL_MODEL=deepseek-flash
python3 scripts/prompt-eval.py
```

`ZODER_PROMPT_EVAL_LIVE=1` is the switch. It is deliberately not a
command-line flag: a live run costs money and talks to a provider, and it
should be impossible to trigger it by accident.

Live mode shells out to the `zoder` binary, so it uses the same dispatch
the operator uses. It needs the binary on `PATH`.

One limitation, stated plainly: the `tool_called` / `tool_not_called`
criteria cannot be scored from a live run, because a live run's tool
calls are not reconstructed from a transcript. Those criteria are reported
as **not satisfied** in live mode rather than being counted as a pass.
A live task therefore has a maximum score lower than its offline score,
and the two numbers are not directly comparable on those tasks.

**This suite was not run against any provider while the assets were
being written.** The task that introduced these assets asked for
documentation only, so every claim about how a model behaves under the
master prompt is UNVERIFIED until somebody runs live mode against a route
and records the result.

## 3. Deciding whether to enable the flag

The flag is off by default:

```toml
[prompt]
master = true
```

or, for a single run:

```sh
ZODER_PROMPT_MASTER=1 zoder "…"
```

Before enabling it fleet-wide, the operator should:

1. Run the offline suite — it must be 12/12. A failing offline task is a
   broken fixture or a broken harness; fix that first.
2. Run the live suite against each route the fleet actually uses, and
   compare against the same run with the flag OFF. The comparison is the
   measurement; the live number alone is not.
3. Enable the flag for one route or one agent group, not the whole fleet.
4. Keep the old prompts reachable: `master/v1.md` stays in the tree, and
   turning the flag off restores the previous behaviour byte-for-byte.

### The overlay evidence is not the eval result

Each overlay carries a MEASURED or UNVERIFIED note per claim. Those notes
record what was known when the overlay was written. They are NOT a
substitute for running the eval: a claim marked MEASURED describes a
past observation, and a claim marked UNVERIFIED is a hypothesis the
overlay encodes to compensate for a known weakness.

See `prompts/README.md` for the overlay contract and
`docs/PROMPTS_INVENTORY.md` for the inventory of the prompts that existed
before this directory.
