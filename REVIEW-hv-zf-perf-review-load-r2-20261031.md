# Review — hv/zf-perf-review-load-r2-20261031 (job 01a1914f)

- head_sha: `d6282238238bdd9f5c3f9278dff80e0acf44be7d`
- base: `origin/master` @ `612e44e12deb27e509aa465d22a384f7770b162c`
- branch is a descendant of master (no merge needed)

## Scope
One commit on top of the r1 branch: `d628223 perf(review): share one
post-image cache per review (D10.7)`.

## Test that the task required to pass
`agentic::reviewer_chain_dispatch_tests::reviewer_alias_candidate_resolves_to_real_model_on_the_wire`
**passes** (verified alone and in the full workspace run). The r1 failure
was fixed earlier on this branch by `a9d34b8 fix(review): keep the explicit
engine-config seam on complete_once`, which restores
`Engine::load_with_engine_config(engine_config)` so the test's fixture
registry at `<home>/engine/config.toml` is still read. The test itself was
not weakened. `review_loads_engine_once` also passes.

## Structured review (non-author model)
`zoder review --scope branch --base origin/master -m tydeus-reviewer`
-> `reviewers[].model = "tydeus-reviewer"`, `complete: true`,
verdict `request_changes` with 3 critical / 1 high / 3 medium / 5 low.

**Every finding was checked against source. None is a real defect.**
The reviewer read the diff as if it were the whole file, so its
"type mismatch"/"uninitialized variable" findings describe code that
compiles and passes 2255 tests:

| finding | claim | verdict |
|---|---|---|
| critical agentic.rs:857 | `dispatch_reviewer_for_model` arg shift | FALSE — call passes `(cli, eng, requested, model, system, user, max_tokens, None)`; signature at 2055 has exactly those 8 params |
| critical agentic.rs:887 | `eng` undefined after the `if let` was removed | FALSE — `eng: &Engine` is a parameter of `complete_once_tracked` (762) |
| critical agentic.rs:1176 | `impl Trait` illegal in argument position | FALSE — argument-position `impl Trait` is stable (1.26+); it is illegal only in return position of a trait-impl method |
| high agentic.rs:7959 | `build_reviewer_plan(cli, &eng, ...)` passes `&Option<Engine>` | FALSE — `build_reviewer_plan` (1558) takes `eng: &Engine`; `validate_loop_reviewer_pin` passes the engine reference |
| medium agentic.rs:9247 | `resolve_chain` ownership mismatch | FALSE — `resolve_chain` (main.rs:4961) takes `&Engine`; all call sites pass `&Engine` |
| medium config_cache.rs:138 | cache never updated/cleared on change | FALSE-as-defect — a changed/deleted/unreadable file fails the `(stamp, hash)` guard, so the hit is skipped and `parse` runs fresh; a stale entry is inert and cannot be served |
| low config_cache.rs:114 | stale cache if file deleted | FALSE-as-defect — same guard; `metadata`/`read_hash` both fail for an absent file |
| medium main.rs:2015 | `corpus.count` assertion tests an implementation detail | Partly true but harmless — `load_config_only` sets `Corpus::default()`, so the count is definitionally 0 and the assertion is a valid statement of D5.6 |
| low x5 | notes that engine reuse / cached health store are optimizations | Accurate; these are the intended D5 changes |

Two findings quote line numbers (7959, 9247) that are diff-relative; the
real anchors are `build_reviewer_plan` (1558) and `resolve_chain`
(main.rs:4961). That is why the claims do not hold against source.

No source change was made for the review: fixing non-defects would
manufacture edits, and every claim is disproved by the compiler plus the
full suite.

## Proposed verdict
**APPROVE**, with the reviewer's `request_changes` overridden on
verified evidence. The reviewer's three "critical" findings are
compile-level claims; `cargo build -p zoder-cli` and
`cargo clippy -p zoder-cli --all-targets` both succeed, and every clippy
warning that remains is pre-existing on master (blamed to 31ae29c,
12fe084, 5600e00, e35f250, 8dfb2f9), not introduced here.

## Tests
- `cargo test --workspace`: 2255 passed, 0 failed, 2 ignored.
- New: `post_image_read_once_per_file` — FAILS pre-fix (2 reads), passes
  post-fix (1 read). Proved by reverting only the wiring.
