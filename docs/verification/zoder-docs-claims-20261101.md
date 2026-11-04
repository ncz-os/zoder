# zoder docs claim verification — 2026-11-01 (FB82)

Scope: every concrete claim in zoder's `docs/` (flags, env vars, defaults,
exit codes, config keys, file paths, budget rules) verified against the Rust
sources on the same tree (`master` `1cbe935`). README.md is outside `docs/`
and was not verified (exclusion noted in PLAN-FB82.md).

Base caveat: `1cbe935` was the branch point of `freebuff/fb82-20261101`.
`master` has since moved on (it gained the `infrastructure` class /
`session_limit` code and the FB108–FB133 work), so a handful of the TRUE rows
below — the five-class/five-rung contract in particular — describe the audit
base rather than today's `master`. The four FALSE findings were re-checked
against current `master` (`535bb3ea`) and all four still reproduce there; see
"Re-verification against current master" at the end.

Method: for each concrete claim, `grep -n` the cited token in the cited
source file(s); record the source line. No doc or source file was modified.

Verdicts: **FALSE** (source disagrees — one-line evidence quote),
**TRUE** (claim and value match source), **UNVERIFIABLE** (needs a live
provider, fleet runtime, or historical binary this host cannot stand up).

---

## FALSE claims (4)

| # | Claim (doc file:line) | Verdict | Source evidence (file:line, quote) |
|---|---|---|---|
| F1 | CONCURRENCY.md:66-67 — the ACP idle-watchdog table pins the **hosted** defaults at 60s / 60s | FALSE | `crates/acp-client/src/lib.rs:843/848`: `pub const HOSTED_AGENT_IDLE_FIRST_EVENT_S: u64 = DEFAULT_AGENT_IDLE_FIRST_EVENT_S;` (=600) and `HOSTED_AGENT_IDLE_BETWEEN_EVENTS_S = DEFAULT_AGENT_IDLE_BETWEEN_EVENTS_S` (=180, `acp-client/src/lib.rs:830`). A hosted route gets the **same** 600s/180s pair, not 60s/60s. The doc's own prose at CONCURRENCY.md:120 states this correctly ("…and so does a hosted provider"); the table and its prose contradict each other |
| F2 | PROFILES.md:56 — "`session/cancel` settle budget \| 1500 ms \| 750 ms \| Wired: `reaper::cancel_budget_for`" | FALSE | `crates/zoder-cli/src/reaper.rs:163-180`: fleet takes `REAP_CANCEL_BUDGET` (= `zoder_core::CANCEL_SETTLE_BUDGET`, **5 s**, `reaper.rs:145`; 5 s confirmed at `config.rs:2459`) unchanged; standalone gets 5 s (**5.75 s total**), because the profile numbers are applied as a *surplus* over the legacy 750 ms: `REAP_CANCEL_BUDGET + profile_ms - 750`. The source comment (reaper.rs:162-163: "'750 ms IS the value that starved every session after the first') says exactly that the 1500/750 values are no longer the wired budget |
| F3 | PROFILES.md:65-67 — "Accessors for terse I/O, **shared daemon assumption**, and fleet update pulling exist in `ProfileDefaults` for future use but are not yet consulted by any production path" | FALSE (partially) | `shared_daemon` IS consulted: `crates/zoder-cli/src/agentic.rs:7584-7589` reads `config.profile_defaults().shared_daemon()` and `agentic.rs:7672` feeds it to `unconfirmed_cancel_abort_threshold`. TRUE for `terse_io` and `fleet_pull_updates` (no references in `zoder-cli` or `zoder-core` production code; both are marked "Hook only" in `config.rs`) |
| F4 | EXIT_CODES.md:77 (and the failure-site assignment table) — `worktree_tainted` is a shipped failure code with an emission site | FALSE (vocabulary only) | `worktree_tainted` exists only as vocabulary: the `WorktreeTainted` variant at `failure.rs:218`, its code string at `failure.rs:247`, and its transient class at `failure.rs:268`. A repo-wide grep finds **no** production emission or recognition site for `FailureCode::WorktreeTainted` outside `failure.rs`'s own classification tables. It is a documented-but-unreachable code; the doc presents it the same as reachable codes |

Note: the value `PROBE_PING_TIMEOUT_S` quoted at CONCURRENCY.md:179 is
stale-but-true: the constant is `PROBE_PING_TIMEOUT_SECS` in
`crates/zoder-core/src/health_probe.rs:56` (=20). Recorded as TRUE with a
name correction, not FALSE.

## TRUE claims (by doc)

### docs/EXIT_CODES.md

| Claim (doc line) | Source (file:line) |
|---|---|
| Five classes; transient→20, permanent→50, config→40, budget→30, blocked→10; retryability per class (45-49) | `failure.rs:88-135` (FailureClass), `:141-152` (EXIT_* constants 10/20/30/40/50), `:16-31` (contract table incl. retryability) |
| Named-code table, 18 codes, class+exit per code (63-80) | `failure.rs:235-254` enumerates exactly 18 variants incl. `internal`; "The table in `docs/EXIT_CODES.md` is the same table `each_named_code_carries_its_documented_class_and_exit_code` pins" (`failure.rs` impl `class()`) |
| Exit ladder: 0/10/20/30/40/50, 75 reserved for the oneshot refusal band, never assigned (93-118) | `failure.rs:141-157` (`EXIT_RESERVED_ONESHOT = 75`, test-only invariant) |
| `78` = budget-cap decline on the oneshot path, checked first; `1` otherwise (106-118) | `main.rs:13794-13799` (`EXIT_BUDGET_EXCEEDED = 78`), `main.rs:2748` ("historical codes (`error_exit_code`: 78 for a budget-cap decline"), asserted `main.rs:23992` |
| `UNCLASSIFIED_EXIT_CODE = 50` (128) | `failure.rs:173`, asserted `failure.rs:1069` |
| Budget-refusal recognition ("budget_exceeded:" prefix + optional reset) runs in `turn_failure_error` and `classify_error` (208-232) | `main.rs:13693-13704` (`turn_failure_error` probes the engine's own detail for the `budget_exceeded:` marker via `classify_engine_budget_exceeded`); `failure.rs:532` (`classify_error`), `:546` (`classify_message`) |
| `--json` shapes: one JSON document; `failure` member only on failure; fields `message`/`reviewers`/`resets_at` (131-197) | `failure.rs:4-13` (error_envelope contract), `emit_json_failure` (`main.rs:3100`) |
| Backward compatibility: fields additive, never renamed (197) | `failure.rs:33-35` |
| Failure-site rows for `diff_truncated`, `engine_unreachable`, `usage`, `config` | `agentic.rs:1124/5821/5829` (DiffTruncated), `agentic.rs:9374` (EngineUnreachable), `agentic.rs:5854/5862` (UsageError), `agentic.rs:1026/6236/…` (ConfigError) |

### docs/CONFIG_REFERENCE.md

| Claim (doc line) | Source (file:line) |
|---|---|
| Key table, all 32 keys, checked by `scripts/test_config_reference.py` (27-58) | self-test executed on this tree — passes |
| `[deployment].mode` = `standalone` (default) or `fleet` (78) | `config.rs:2359, 2388` |
| `reviewer_model` ordered chain, head first (82) | `config.rs:parse_reviewer_chain`; template comment in `config.standalone.toml` |
| `[[providers]]` fields incl. `kind` enum (`openai-chat`/`openai-responses`/`azure-openai`/`anthropic`/`custom`), `auth` enum, `engine_provider_ref`, billing modes, `rate_limit_group` default = provider's own id (84-90) | `config.rs:Provider` struct + `Auth` enum; `engine_provider_ref` `config.rs:355, 4017-4026`; Template comment in `config.standalone.toml` |
| `engine_provider_ref` auto-derivation; ambiguous mapping fails at load with an instruction to set it explicitly (43-49, ROUTING) | `config.rs:4017-4026` ("never a per-turn guess"), `:4109` ("set engine_provider_ref explicitly"), load-fail test `config.rs:6252` |
| `[model_families]` validation: empty keys, wildcard-only keys, empty families, case-colliding keys → load-time errors (93-95) | `config.rs:5041` ("model_families key {key:?} is empty"), `:5045` ("is only wildcards"), `:5050` ("maps to an empty family"), `:5061-5066` (case-colliding) |
| `[budget]`: `est_output_tokens` default 1024; caps gate paid calls only; 0/absent disables (99-101) | `budget.rs:Budget` |
| `[review]` fields; confirm-on-reject chain (102-104) | `config.rs:ReviewConfig`, `ReviewConfirmConfig` |
| `[routing].scenario` four presets, default `balanced` (110) | `config.rs:Config::routing` |
| Engine config surface `[providers.models.<family>.<alias>]`, `[agents.<alias>]` (117-121) | `config.rs:EngineProviderRoute::from_toml/from_json`; groq-shape engine-registry tests in `config.rs` |
| `ZODER_HOME` default `~/.zoder`; empty/relative/unresolvable rejected (126-128) | `config.rs:3842-3846` |
| `ZEROCLAW_CONFIG_DIR` / `ZEROCLAW_SOCKET` semantics | `agentic.rs` engine-config docs |
| Timeout precedence `--request-timeout` > model TZ `request_timeout_s` > engine profile > global > `ZODER_TIMEOUT_S` > default (150-153) | `main.rs:effective_request_timeout_s_for_model` |
| `ZODER_QUEUE_TIMEOUT_S` (155) | `provider_slots.rs:158-161` |
| `ZODER_PRICING_SOURCE=file` authoritative (157) | `config.rs:Config::pricing`, `PricingConfig` |
| `ZODER_UPDATE_CHECK_RETRY_S` default 900, failure cached under `$ZODER_HOME`, `ZODER_NO_UPDATE_CHECK=1` opts out (160-163) | `update.rs:failure_retry_interval`, `DEFAULT_FAILURE_RETRY` |
| Route flags: `--allowed-routes`, `--reviewer-allowed-routes` required with `--reviewer-pool-routes`, pool constraints (167-176) | `agentic.rs` route-resolution + reviewer-pool setup |

### docs/PROVIDER-ROUTING.md

| Claim (doc line) | Source (file:line) |
|---|---|
| `serves` = model-id prefixes; longest match wins; config order breaks ties; unclaimed → `default_provider` (12-15) | `config.rs:4420` ("longest prefix, earliest config order"), `:4436` |
| Per-link policy gate; paid/metered link skipped fail-closed; `--allow-paid` permits (70-71) | `agentic.rs:2158-2185` (per-model policy gate), `:2607-2611` (not allow-paid && not cost-neutral → refused) |
| Empty `serves` prefix rejected (75) | `config.rs:4726` ("An empty/whitespace `serves` prefix would match EVERY model id") |
| `primary_model` tried first; honored even outside the free pool (80-84) | `main.rs:4824` ("over the global `primary_model`"), `:5215-5223` (`Route.primary`) |
| Overlay precedence: default-claiming overlay wins, else alphabetically-last; independent of `default_provider` (87-89) | `config.rs:2594`, `:5115`, `:5176-5180` |
| Effective transport/endpoint comparison, not literal `kind`/`uri`; openai-codex projection; `/responses` normalization; unresolvable family rejected, no wildcard fallback (52-66) | `config.rs:1399-1417` (`openai-codex` projection), `:605-613` (`/responses` suffix normalization), `:1608` ("refusing to guess its effective endpoint"), `:6126` load-fail tests |
| `zoder refresh` ingests from every `serves`+free provider, filtered to its own allowlist (110-113) | `corpus.rs:1411` ("already prefix-filtered to the provider's `serves` allowlist"), `catalog_models.rs:21` |
| A model the corpus already classifies as paid is never silently flipped (127) | `corpus.rs:1450` ("Never silently flip a model the corpus already classifies as [paid]"), `catalog_models.rs:717-745` test "daemon_paid_claim_does_not_flip_paid_corpus_row_to_free" |
| Newly-ingested unbenched model gets a neutral capability prior; real benchmark overrides (128) | `corpus.rs:1418` ("unbenched id gets the neutral prior"), `:1467` |
| Reconciliation against the union of served ids; models leaving all catalogs retired (129-130) | `corpus.rs:1374-1395` |
| Reviewer refuses same-family authors; cross-family review (136-139) | `agentic.rs:1825-1828` (same_family skip), `:1586, :1614` ("never silently falls back to a same-family review") |
| Unmatched id falls back to vendor prefix; exact id beats glob; case-insensitive (94, ROUTING) | `config.rs:2682-2699`, `:5032` (case-insensitive match), `:3068` |
| Overlay `[model_families]` merged per key; later overlay wins; base-only keys survive (142-144) | `config.rs:5438` (per-key merge loop), test `:9538` (`overlay_model_families_merge_over_base_and_later_overlay_wins`) |
| `[model_families]` validation errors (145-152) | `config.rs:5041/5045/5050/5061-5066` |

### docs/PROFILES.md

| Claim (doc line) | Source (file:line) |
|---|---|
| `[deployment].mode` default `standalone` when the table is absent (8) | `config.rs:2359` |
| `[context] profile` defaults to the deployment profile; explicit wins; explicit `enabled=false` turns levers off (17-19, 60-64) | `config.rs:2372, 2417-2419, 2919-2922` |
| `zoder init` does nothing when a config exists; `--force` replaces (32-33) | `main.rs:969-976` ("already exists …; leaving it untouched (use --force…)"), `:1204` |
| `install.sh --profile standalone\|fleet`, default standalone (33) | `install.sh:30` ("standalone (default) or fleet starter") |
| `config.fleet.toml` is only a profile selector; fleet tuning comes from the fleet's config repository (38-39) | `config.fleet.toml` (template inspected) |
| Fleet-only defaults not applied without the profile (43-45) | `config.rs:2359-2388` (opt-in mode) |
| Context levers wired via `Config::context_levers_for` / `profile_defaults` (52) | `config.rs:2417-2419`, `:2504`, `:3860` |
| Runner job-id emission wired (53) | `agentic.rs:9823-9824` (`enterprise_cfg.profile_defaults().fleet_job_workspaces()`), `:10997` |
| Cost backstop: standalone $50/month template-suggested, fleet unset (57) | `config.rs:2465-2477` (`cost_backstop_monthly_usd` = arms), hook-only comment |
| Provider reviewer concurrency: unlimited unless `ZODER_PROVIDER_CONCURRENCY` (58) | `config.rs:2484-2492` (0 = unlimited both modes), `provider_slots.rs:15-30` |
| Secondary reviewer pool: off, opt-in via `--reviewer-pool-routes` on both profiles (59) | `config.rs reviewer_pool_default` hook-only; `--reviewer-pool-routes` setup in `agentic.rs` |
| Paid gating: `--allow-paid` or interactive confirmation; no persistent "always allow paid" switch (76-83) | `agentic.rs:2607-2611`, `config.rs:4364` (cost-neutral carve-out), `budget.rs` pre-call gate |
| `zoder config --validate` checks the merged result (86) | validated CLI path (self-test + `config.rs` validation table) |

### docs/CONCURRENCY.md

| Claim (doc line) | Source (file:line) |
|---|---|
| goose is an alternative engine only, explicit `--engine goose`; zeroclaw default (10-13) | `main.rs:791-792` |
| Paths: review direct to the reviewer endpoint (no daemon); `exec` agentic via Zeroclaw daemon over ACP with oneshot fallback; `--oneshot` direct (18-21) | reviewer path is plain provider HTTP (`provider.rs:412` "Non-streaming calls (the reviewer path)"); `main.rs:6718` (engine-unavailable → oneshot fallback of `cmd_exec`) |
| Provenance fields `slot_wait_ms`, `slot_limit`, `slot_held`, `ledger_wait_ms`, `headers_ms`, `first_token_ms`, `latency_ms` (33-38) | `agentic.rs:2709` (`"slot_wait_ms": slot.waited…` etc. in the same receipt block) |
| Reviewer calls non-streaming (`stream: false`), so `first_token_ms` empty; oneshot streams (40-44) | `agentic.rs:25777` ("The reviewer call is non-streaming (`stream: false`)"), `provider.rs:412` |
| Wait notice every 30 s (`ZODER_WAIT_NOTICE_S`), suppressed under `--quiet`; never re-sent (46-50) | `provider.rs:1809-1813` (env, default 30 s), `main.rs:7053` (`.with_wait_notices(!cli.quiet)`) |
| Request default 120 s; `ZODER_IDLE_S` default 25 s (51-53) | `provider.rs:544` (`DEFAULT_REQUEST_TIMEOUT_S = 120`), `:547` (`DEFAULT_IDLE_TIMEOUT_S = 25`), `:1768` |
| Two watchdog windows; self-hosted 600s/180s; measured between substantive events; `--agent-timeout` default 900 s (59-68, 91) | `acp-client/src/lib.rs:828/830`; substantive = token or tool call (`main.rs:389-395`); `main.rs:2970` (`cli.agent_timeout.unwrap_or(900)`) |
| Substantive = token or tool call; metadata/usage/keepalives not substantive (69) | `main.rs:389-395` |
| Pending tool call suspends the watchdog; phase resets to first_event after the tool round (79, 124) | `main.rs:389-405` (phase rules codified in `effective_agent_idle_windows` docs) |
| `idle_timeout` outcome distinct from `timeout`, with structured reason (window, window_secs, last-event age, model) (92-99) | `acp-client/src/lib.rs:851-852` (`IDLE_TIMEOUT_OUTCOME = "idle_timeout"`), structured reason format in main.rs idle path |
| Config knobs: specific flag > legacy flag > specific config key > legacy key > specific env > legacy env > route default; alias sets BOTH; each window independent; 0 disables that window (105-121) | `main.rs:391-407` (the precedence is written out line-for-line), `:436-452` (implementation), `acp-client/src/lib.rs:856-874` (env aliases, per-window 0) |
| Built-in default follows the ROUTE: self-hosted (loopback/private-LAN or `vllm`/`sglang`/`llamacpp`) gets 600/180, **and so does a hosted provider** (119-121) | `main.rs:420-432` — `IdleRoute::Hosted` arm aliases to the same constants (`acp-client/src/lib.rs:843/848`); comment: "a hosted gateway can queue… a short window would end healthy turns" |
| Per-model budget precedence: CLI > zoder `models` entry > engine profile > global > `ZODER_TIMEOUT_S` > 120 (140-150) | `main.rs:effective_request_timeout_s_for_model` |
| `zoder exec --oneshot --json` reports `headers_ms` and `first_token_ms` (44) | oneshot receipt path, `agentic.rs` provenance writer |
| Applies to review panel (incl. loop review phase) and exec author chain (172) | `agentic.rs` provider-call sites for both paths |
| Catalog fetches and health probes keep the global budget; probes bounded 20 s (174-179) | probes: `health_probe.rs:56` (`PROBE_PING_TIMEOUT_SECS = 20`), bound asserted `:785-796` |
| `request_timeout_s = 0` rejected at load (as validation error on config, parse error on engine profile) (176-183) | `config.rs:4891` and `:4895` (zero-budget rejection in `validate`) |
| zoder-side `models` map: two aliases carrying different budgets for the same model id is a load-time error (181-183) | `config.rs:4904` ("hide the budget another alias declared for the same model. Fail at [load]"), test `:5906` |
| Ledger: was 64 KiB/call (506 MB / 7,725 calls); now 4 KiB slots; compaction postponed while a legacy-size reservation row younger than an hour exists (192-201) | `ledger.rs:57-75` (comments quote the same 506 MB/7,725 figures), `:64` (`BILLABLE_RESERVATION_BYTES = 4 * 1024`), `:67` (`LEGACY_RESERVATION_BYTES = 64 * 1024`), `:75` (`LEGACY_INFLIGHT_GRACE_S = 3600`), recovery scan postponement `:1202-1239` |
| Concurrency limit: env-only (`ZODER_PROVIDER_CONCURRENCY`), `flock`-ed files under `<home>/state/slots/<provider>/`, released when a process dies (218-224) | `provider_slots.rs:10-30`, `config.rs:455` |
| `ZODER_QUEUE_TIMEOUT_S` default 900 s; a queue timeout is a capacity failure → the reviewer chain moves to the next configured reviewer (224-226) | `provider_slots.rs:158-161`; chain failover `agentic.rs:588-599, 799, 992` |
| Health gate: opt-in `[health].providers`; before first query and after TTL (default 300 s) or any 429/5xx/timeout, bounded `GET {base}/models` (default 5 s) must list the model; state in `~/.zoder/health.json` (253-270) | `provider_health.rs:15-24` (module docs match line-for-line), `:41-42` (`DEFAULT_TTL_SECS = 300`, `DEFAULT_PROBE_TIMEOUT_MS = 5000`), `:53-65` (env names match) |
| Breaker: opens on 429/5xx/timeouts/failed probes; 30 s backoff doubling to 15 min, never shorter than Retry-After; half-open single probe claimed by one process (270-272) | `provider_health.rs:22-24` (verbatim), `:119-120` (`claim_half_open_at`) |
| 429 not retried in-process; skipped model listed in `skipped_unhealthy`; a skipped reviewer is never a pass (274-280) | `provider_health.rs` module docs; `skipped_unhealthy` in the review JSON written by `agentic.rs` |
| `zoder exec --oneshot` skips an unhealthy gated link; agentic exec stops with the reason (283-285) | `provider_health.rs` precheck contract + `agentic.rs` route gating |

## UNVERIFIABLE claims (4)

| # | Claim (doc file:line) | Why |
|---|---|---|
| U1 | PROVIDER-ROUTING.md:155-157 — "A binary older than this change rejects the whole overlay when it meets an unknown `[model_families]` table" | Needs the historical (pre-change) binary; behavior cannot be reproduced from this tree |
| U2 | CONCURRENCY.md:135-137 / 192-196 — the measured figures ("review cut twice at 120s on a ~650-line diff on 2026-10-21"; "506 MB / 7,725 calls and 328 MB on two fleet hosts") | Operational measurements; the source's own comments repeat the same numbers (`provider.rs:694`, `ledger.rs:60-61`), which corroborates but does not re-measure them |
| U3 | CONCURRENCY.md:262 — "HTTP 429 on 7.5% of reviewer calls, in bursts, even at one request in flight" | Study run against a live hosted provider |
| U4 | Concurrency/health runtime behavior end-to-end (breaker soak, mixed-version ledger compaction with a legacy build mid-dispatch) | Structure verified in code (`ledger.rs:1202-1239`); the race itself needs a live mixed-version rollout |

## Re-verification against current master

The audit above is pinned to `1cbe935` (the branch point). Before landing, the
four FALSE findings were re-checked against current `master` (`535bb3ea`); all
four still reproduce, so none of them has been fixed by later work:

| # | Re-check on current master | Still false? |
|---|---|---|
| F1 | `docs/CONCURRENCY.md` still carries the hosted row as 60s/60s, while `crates/acp-client/src/lib.rs` aliases `HOSTED_AGENT_IDLE_*` to the 600s/180s defaults | yes |
| F2 | `docs/PROFILES.md` still states 1500 ms / 750 ms, while `reaper::cancel_budget_for` returns `REAP_CANCEL_BUDGET` (5 s) for fleet and 5.75 s for standalone | yes |
| F3 | `docs/PROFILES.md` still claims the shared-daemon assumption is unconsulted, while `agentic.rs` reads `profile_defaults().shared_daemon()` | yes |
| F4 | `docs/EXIT_CODES.md` still lists `worktree_tainted` beside reachable codes; there is still no production emission site | yes |

Rows the audit records as TRUE for the five-class/18-code contract were
accurate at `1cbe935` but are superseded on current `master`, which added the
`infrastructure` class (exit 60) and the `session_limit` code (19 codes, 6
classes). Those rows should be read against the audit base, not against today's
`master`; the drift is documentation progress (the doc and the source agree at
both bases), not a defect this audit should report as FALSE.
