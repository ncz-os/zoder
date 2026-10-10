# Fault to fix

zoder is the long-range coding tool. When zoder fails, opencode is the
fallback that resolves the immediate problem, and zoder itself is then
corrected so the same failure cannot recur. This file makes that loop
traceable: every fault seen in real use gets a row here with its root cause,
the fixing commit, and the regression test that pins it, or an explicit,
reasoned "not fixed because".

## Engine hierarchy

**Zeroclaw is zoder's engine.** `zoder exec` and loop author turns run
through the Zeroclaw daemon. **goose is an alternative engine only**, used
when selected explicitly (`--engine goose`), normally because Zeroclaw is
broken on a host. zoder never switches to goose on its own, and no default,
route or fallback in the fleet configuration points at goose.

## Fixed

| Symptom | Root cause | Fix | Test |
|---|---|---|---|
| Review of a split diff panicked on a multibyte character before `(` | `c_style_def` sliced at a byte index mid-codepoint | `0a05a80` | `c_style_def_handles_multibyte_before_paren`, `split_diff_with_multibyte_definition_does_not_panic` |
| Deleted files could not be excluded and split labels were empty | `section_path` fed `+++ /dev/null` to the path cleaner | `0a05a80` | `deleted_file_path_falls_back_to_preimage`, `deleted_file_is_excludable_and_appears_in_diff_map` |
| A branch adding a `*` `.zoderignore` hid itself and was approved | empty-after-exclusion diff reviewed as one empty chunk; ignore file read from the changed tree | `0a05a80` | `self_hiding_zoderignore_star_is_detected_as_emptied`, `zoderignore_is_read_from_a_revision_for_base_scopes` |
| Malformed reviewer JSON counted as a completed review | prose verdict synthesized into request_changes | `03ab41d` | `chunk_json_retry_is_bounded_and_never_accepts_schema_echo` |
| Partial reviewer panel advertised complete/approve | aggregate ignored failed members | `4788c71` | partial-panel aggregate tests |
| Default reviewer failed "returned invalid JSON twice" on real diffs | 2048-token review budget truncated reasoning models before the verdict | `7d45797` | `reviewer_budget_clears_reasoning_models_and_retry_escalates` |
| MiniMax-M3 could never complete a review | `<think>` block in `content` before the fenced verdict | `83683c9` | `strict_review_parses_a_leading_think_block_then_fenced_verdict` |
| `cancel_session` accepted another session's `turn_complete` | session id never checked | `da9cb67` | socket regression tests in acp-client |
| `review --json` had no `substituted` field (saver F2) | payload never derived it | `ef8483f` | `receipts_show_substitution_detects_a_mismatch` |
| Every fix-branch pipeline failed `cli-surface` from `4788c71` on | new `adversarial-review` flags had no help text; clap's indentation-only lines did not match the snapshot | `3bdc7c1` | CI `cli-surface` job |
| `--exclude` / `.zoderignore` never matched non-ASCII paths | git octal escapes decoded as Latin-1 chars | `c15196a` | `quoted_utf8_path_is_decoded_as_utf8` |
| A stream that only emitted reasoning and then hung rode the whole request budget (A11) | idle guard armed only on answer text, in all three wire formats | `cd41970`, `e8b3f47` | `reasoning_only_stream_still_has_idle_stall_guard`, `anthropic_thinking_only_stream_has_idle_stall_guard`, `responses_reasoning_only_stream_has_idle_stall_guard` |
| `zoder loop --scope working-tree` reported a false "empty diff" when the author committed mid-turn (B10) | explicit scopes bypassed the task baseline | `2dc63dc` | `working_tree_scope_survives_mid_turn_commit` |
| `zoder exec --json` failures carried no provider/model/HTTP detail (B12) | engine detail printed only in human mode; oneshot error named no route | `030440a` | `turn_failure_summary_names_the_engine_reason`, `oneshot_failure_summary_names_model_and_provider`, `exec_error_json_is_structured_and_reported_errors_are_marked` |
| `-m google/gemma-4-31b-it` failed with an opaque placeholder error; adding an allowlist said "outside the allowlist" (blackhole2 F1) | only `serves` prefixes route a model; message B was message A mislabelled | `739e62a` | `unbacked_explicit_reviewer_reports_missing_provider_not_allowlist`, `reviewer_model_suggestion_matches_vendor_and_suffix_variants` |
| `review --scope branch` with no commits approved without calling a model while uncommitted work existed (saver F4) | empty diff short-circuited to approve | `e983e7e` | `empty_branch_diff_with_dirty_tree_is_refused` |
| Reviews on a shared host queued silently for many minutes (megacity2) | ledger grew 64 KiB per call (506 MB on ACHILLES) and every reservation re-read it under a host-wide lock | `4055ad8`, `5f7405a` | `legacy_padded_ledger_is_compacted_without_losing_entries`, `armed_reservation_survives_compaction_and_reconciles`, `recent_legacy_reservation_postpones_compaction` |
| No way to see where review wall time went | no queue/first-token telemetry; no client concurrency control; non-streaming reviewer waits were silent | `14ab039`, `9af04b0` | `slow_first_token_with_notices_succeeds_and_is_measured`, `slots_serialize_measure_wait_and_time_out_clearly`, `non_streaming_wait_with_notices_succeeds_and_stays_bounded` |
| Saver F1: review refused a single hunk over 9000 bytes | stale installed binary predating hunk splitting | `129f47e` (splitting), deploy | `review_chunks_split_oversized_hunk_when_enabled` |

## Not fixed in zoder, and why

| Fault | Why not |
|---|---|
| Saver F3: `zoder exec -m qwen38` streamed zero tokens for minutes | The model socket belongs to the Zeroclaw engine, not zoder's provider layer. zoder bounds the turn with `--agent-timeout` (default 900s), which would have ended it. A zoder-side ACP idle watchdog is a candidate follow-up. |
| blackhole2 F2 / zoderqa B6: EIH HTTP 429/503 under load | Provider capacity. Retries with backoff exist, the reviewer chain falls back, and the per-provider slot limit can cap a host's burst. A pre-query EIH health check and breaker are tracked separately. |
| zoderqa B7: gemma4-31b slow on CERBERUS | Server capacity (one GPU). Now visible via `first_token_ms`; cap per host with `ZODER_PROVIDER_CONCURRENCY`. |
| zoderqa B8: deploy deferred while any zoder process runs | By design (installer idle gate). |
| megacity2: qwen38 authored GLSL that did not compile | Model output quality, caught by the review/check gate as intended. |
| Chunked-diff false positives from small reviewer models (names defined outside the chunk) | Model behaviour. Mitigations are measured before they ship; see the review context header work. |
