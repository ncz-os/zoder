# Prompt assets

Versioned, reviewable prompt text for zoder. Nothing in this directory is
active until an operator opts in.

## Layout

```
prompts/
  README.md            <- this file
  master/
    v1.md              <- the master system prompt (author role)
  overlays/
    deepseek.md        <- per-family corrections
    gemma-local.md
    gpt-oss.md
    minimax.md
    mimo.md
    muse-spark.md
    qwen-local.md
```

## What is loaded, and by whom

`zoder_core::prompt` is the single loader. It reads the files above at
runtime and assembles the final prompt text for a role.

| Role | Assembled from | Function |
|------|----------------|----------|
| author | `master/v1.md` + the matching `overlays/<family>.md` | `zoder_core::prompt::assemble` |
| reviewer | `master/v1.md` is NOT used; the reviewer contract stays in Rust | — |

The reviewer prompts (`REVIEW_SYSTEM` / `ADVERSARIAL_SYSTEM`) are **not**
part of this directory yet: they are strict JSON contracts whose wording is
tightly coupled to the verdict parser, and moving them into an asset without
a parser-compatible snapshot would be a larger change than this task
allows. `docs/PROMPTS_INVENTORY.md` §2 lists them with their sizes so the
remaining duplication is explicit and scheduled.

## The overlay contract

Every overlay file has this shape:

1. A YAML-ish front block: family key, the model ids it matches, and the
   status of each claim below (MEASURED or UNVERIFIED).
2. A single fenced code block whose contents are the **exact text** appended
   to the master prompt.

The loader reads the fenced block; everything outside it is documentation.
This is deliberate: the evidence for each claim stays next to the claim, and
the prompt text stays reviewable as plain text.

### Rules an overlay must follow

- It must not contradict the master prompt. It *sharpens* the master for one
  family; it never reverses it.
- It must be short. An overlay is a correction list, not a second prompt.
- Every behavioural claim must be marked MEASURED or UNVERIFIED in the
  header. A claim with no evidence must be marked UNVERIFIED.

### Adding a family

1. Add `prompts/overlays/<family>.md` with the shape above.
2. Add the family to `zoder_core::prompt::family_for_model`.
3. Add the unit test for that family (snapshot + size budget).

## Enabling

Off by default. An operator turns it on with either:

```toml
[prompt]
master = true
```

or, for a single run:

```sh
ZODER_PROMPT_MASTER=1 zoder "…"
```

With the flag off, zoder sends exactly the prompts it sent before this
directory existed — the byte-for-byte behaviour described in
`docs/PROMPTS_INVENTORY.md` §1–§4. See `docs/PROMPT_EVAL.md` for how to
measure a rollout before flipping it fleet-wide.

## Editing an existing prompt

The files are versioned by directory (`master/v1.md`). A behavioural change
that should not apply retroactively gets a **new** directory
(`master/v2.md`) and a new config key value (`master_version = "v2"`); the
old file stays in the tree so a fleet that has not upgraded still resolves.
The assembler refuses to fall back silently: an unknown version is an error,
not the newest file.
