# Review — hv/zf-perf-review-load-20261031 (job 01a190eb)

- verdict: **request-changes**
- head_sha: `27e5ef82fc71599b1c86f34047bdcdb7e1be7dc7`
- base: `origin/master` @ `612e44e12deb27e509aa465d22a384f7770b162c` (merge-base = base, clean)

## Scope
5 commits: D5.6 corpus-free engine load for spend/report/quota/providers
(main.rs), D5.2 engine-load-once for the review loop (agentic.rs + main.rs
counter), new `crates/zoder-cli/src/config_cache.rs` (pricing/health
memoization), and two "WIP: preserve uncommitted work" commits that add
root-level scratch files and further config_cache content.

## Verified
- `cargo build -p zoder-cli` clean (1 dead-code warning: `engine_load_count`
  unused in the non-test build).
- `cargo test -p zoder-cli`: 1022 passed, 1 failed (see F1).
- `engine_loader_tests::spend_loads_config_without_a_corpus` passes;
  `reviewer_chain_dispatch_tests::review_loads_engine_once` passes.
- Working tree clean; local HEAD == remote branch tip == 27e5ef8.

## Findings
- **F1 (blocker, test regression).** `agentic::reviewer_chain_dispatch_tests::reviewer_alias_candidate_resolves_to_real_model_on_the_wire`
  fails: "no real provider is configured for reviewer model 'reviewer'".
  Introduced in 450d9db: the test wrote its alias registry to
  `<home>/engine/config.toml` and passed `Some(&engine_config)` to
  `complete_once`; the signature change dropped that path, and
  `test_engine()` -> `Engine::load()` -> `engine_config_path()` resolves the
  registry from `ZEROCLAW_CONFIG_DIR`/`~/.zoder` instead, so the alias never
  resolves. Passes on base (verified in a worktree), fails on HEAD. The
  test's own comment says the explicit path exists to avoid mutating the
  process-global `ZEROCLAW_CONFIG_DIR` — the fix is to keep an
  engine-config seam in the borrow path (e.g. load the engine once with the
  explicit config) rather than pointing it at the process environment.
- **F2 (hygiene).** HEAD contains `WIP: preserve uncommitted work` commits
  adding root-level `.scratch2.rs`, `.scratch3.rs`, `.scratch_block.rs`,
  `.scratch_tail.diff` (~1150 lines, none referenced from code; the
  PostImageCache D10.7 work they stage was never wired in). These must be
  dropped before merge.
- **F3 (minor).** `config_cache` is never referenced by any source file
  (no `mod config_cache`); its tests still run and pass. Either wire the
  module into the crate or move it out of this branch.
- **F4 (minor).** `engine_load_count`/`ENGINE_LOADS` are test-only
  (dead-code warning in non-test builds); gate with `#[cfg(test)]` or leave
  as-is with an explicit allow.

## JSON
```json
{
  "verdict": "request-changes",
  "reason": "D5.2 broke a pre-existing regression test: reviewer_alias_candidate_resolves_to_real_model_on_the_wire fails on HEAD (passes on base) because the engine-config path seam was dropped and the fixture alias registry is no longer read. HEAD also carries WIP scratch commits (root .scratch* files) and an unwired config_cache module that must be removed or wired before merge.",
  "head_sha": "27e5ef82fc71599b1c86f34047bdcdb7e1be7dc7",
  "job_id": "01a190eb-438c-788c-9427-f9fd603d44b1",
  "branch": "hv/zf-perf-review-load-20261031",
  "base": "612e44e12deb27e509aa465d22a384f7770b162c",
  "findings": [
    {
      "id": "F1",
      "severity": "blocker",
      "file": "crates/zoder-cli/src/agentic.rs",
      "test": "agentic::reviewer_chain_dispatch_tests::reviewer_alias_candidate_resolves_to_real_model_on_the_wire",
      "introduced_in": "450d9db940a59b4432d412f97a54359696345e39",
      "error": "no real provider is configured for reviewer model 'reviewer'",
      "detail": "complete_once's explicit engine-config parameter was replaced with &Engine; the test's fixture engine config (<home>/engine/config.toml, written for the alias registry) is no longer read — test_engine() resolves it from ZEROCLAW_CONFIG_DIR/~/.zoder. Passes on base."
    },
    {
      "id": "F2",
      "severity": "major",
      "files": [".scratch2.rs", ".scratch3.rs", ".scratch_block.rs", ".scratch_tail.diff"],
      "introduced_in": "27e5ef82fc71599b1c86f34047bdcdb7e1be7dc7, e2b0e9ef69bb461a793d9ed815af6d69538f26fd",
      "detail": "WIP commits committed root-level scratch files from an unfinished D10.7 PostImageCache run; nothing in the code references them. Remove before merge."
    },
    {
      "id": "F3",
      "severity": "minor",
      "file": "crates/zoder-cli/src/config_cache.rs",
      "detail": "Module is dead code: no `mod config_cache` declaration anywhere in the crate (only its #[cfg(test)] unit tests run). Wire it in or drop it from this branch."
    },
    {
      "id": "F4",
      "severity": "minor",
      "file": "crates/zoder-cli/src/main.rs",
      "detail": "engine_load_count/ENGINE_LOADS are only used by tests; dead-code warning in non-test builds. Gate #[cfg(test)] or add an explicit allow."
    }
  ],
  "tests": {
    "build": "ok (1 dead-code warning)",
    "cargo_test_zoder_cli": "1022 passed, 1 failed (F1)",
    "new_tests": [
      "engine_loader_tests::spend_loads_config_without_a_corpus: pass",
      "reviewer_chain_dispatch_tests::review_loads_engine_once: pass",
      "config_cache::tests (4): pass (but see F3)"
    ]
  }
}
```
