# PLAN-FB120 — fallback spend billed to the route that answered (F5.4) + fresh-create retry error (F1r)

Base: master (662ee2d, includes FB110 + FB111 merges). Branch: freebuff/FB120-20261102.

## Finding verification (re-checked against master head)

### F1r — PREMISE FALSE, already fixed
- `crates/acp-client/src/lib.rs` `drive` (~3076) and `drive_goose_io` (~5017) BOTH already
  keep the retry error: `"engine error on session/new: {err} (and the fresh-create retry also failed: {retry_err})"`.
- Introduced by ad376c6 ("retry session/new across daemon session-capacity refusals", FB110),
  merged into master. Existing tests pin it (resume+fresh-create both fail -> Err mentions
  "fresh-create retry also failed", ~L9158-9294).
- Action: no code change. Run the acp-client session tests to confirm green, record here.

### F5.4 — REAL, but narrower than the audit text
Already correct on master (ledger `Entry` uses `model_used` + `real_provider_for_model(model_used)`;
`agentic_cost` queries the engine with `used_alias`; `TurnResult`/`--json` use the winner).
Still recorded against the HEAD route:
1. `main.rs:13257` — `persist_agentic_utilization(&routed_provider, headers)`: Utilization
   headers emitted by the WINNING turn are stored under the HEAD provider. Cross-provider
   fallback -> quota/telemetry misattributed.
2. `main.rs:13349` — `persist_agentic_counter(&routed_provider, agentic_usage.get())`: MiniMax
   counter windows incremented against the HEAD provider for a turn the fallback served.
3. `AgenticCostScope::start(head_alias)` — scope dir keyed by head alias. Only affects local
   overlap detection (false overlap -> cost_unknown, fail-safe). NOT changing: restructuring
   per-candidate scopes risks the overlap semantics; worst case is unknown, never wrong spend.

## Fix (main.rs only, 2 small edits + comment updates)
- In the dispatch event closure, use the candidate being dispatched:
  `link_provider.as_ref().unwrap_or(&routed_provider)` (link_provider is in scope, per-iteration).
- Lock in the winner's provider in the `Ok(r)` arm (`used_provider`, alongside
  used_primary/used_alias) and use it for `persist_agentic_counter`.
- Route both through a tiny helper `serving_provider(link, head) -> &Provider` so the rule
  ("telemetry belongs to the route that answered") is named and unit-testable.
- Update the two stale "known minor accounting miss" comments.

## Tests (red-first where the defect lives)
- NEW `serving_provider_selects_answering_route_over_chain_head`: head=minimax-like provider,
  fallback=other provider -> helper returns fallback; None -> head. Red = compile fail before
  helper exists (standard TDD red), green after.
- NEW store-separation test via existing `_at` helpers: fallback usage recorded under fallback
  provider id only; head store untouched (pins no cross-contamination end state).
- PINNING (passes before/after, documents the ledger half): `real_provider_for_model` returns
  the fallback's provider for the fallback model with a two-provider config (the ledger Entry
  is built from exactly this resolution).
- Run: `cargo test -p zoder-cli` (touched crate) + `cargo test -p acp-client` session tests for
  F1r confirmation + `cargo fmt --all`. Full workspace `cargo test --workspace` once at end if
  time permits (brief: tests for the crates you touch).

## Risks
- `link_provider` for idx==0 carries `agent_provider_ref`; using it uniformly per-iteration is
  equal-or-better vs `routed_provider` for the head too (same resolution, fresher).
- No daemon/engine changes; no e2e agentic_turn test possible hermetically (needs live daemon
  binary) — stated plainly in summary.
