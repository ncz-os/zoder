# PLAN-FB88 — zoder: panic-surface inventory (unwrap/expect/panic in non-test code)

Brief: list every `unwrap()`, `expect(`, `panic!`, `unreachable!`, slice index
and `as` narrowing cast in non-test code of the Rust workspace, classified
SAFE (invariant stated nearby), RISKY (input-dependent) or UNKNOWN; write
`docs/verification/panic-inventory-20261101.md` with file:line and the 20
riskiest first. Report only.

## Clone & scope

- `~/Projects/freebuff-fb88-panicinventory`, base master `1cbe935`, branch
  `freebuff/FB88-20261030`.
- Crates: `acp-client`, `agent-descriptor`, `model-health`, `zoder-cli`,
  `zoder-core`, `zoder-mcp-server`. Non-test code only: exclude paths under
  `#[cfg(test)]`/`#[test]` code and `*_tests*`/`test_*` files; the classifier
  run is scripted (regex sweep + per-hit context read).

## Method

1. Sweep each pattern with `rg -n` per crate (rust files only):
   `.unwrap()`, `.expect(`, `panic!(`, `unreachable!(`, `as ` (narrow casts
   only: ident-to-primitive casts), slice indexing `[i]`/`[n]` forms with
   numeric or variable index, `.first()/.last().unwrap()` chains, integer
   division? (no — not panic-prone except `/ 0` constants), `remainder`),
2. Classify each hit:
   - SAFE: invariant visible nearby (literal `unwrap` on `Some(#[tokio])`
     known-constructed from `HashSet/Vec` push just above, e.g. unlock of
     own lock, index just after bounds expression checked by exit).
   - RISKY: value depends on external input (config text, HTTP response,
     model output, argv, DB rows).
   - UNKNOWN: cannot be decided from the local context without a deeper call
     analysis.
3. Rank the 20 riskiest by (input-controlled severity × frequency of path).
4. Emit the report with exact counts per crate and file:line citation table.

## Risks / limits

- Report-only: no production changes, no refactor.
- `as` casts: only classify narrowing semantics (usize→u32/u16, i64→i32, f64→f32, etc.); keep the classification grounded in what could overflow/panic at runtime.
- Tests that themselves construct panics are out of inventory per brief.
