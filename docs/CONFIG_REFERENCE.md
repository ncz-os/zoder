# zoder configuration reference

Read from the Rust sources (`crates/zoder-core/src/config.rs`,
`crates/zoder-core/src/budget.rs`, `crates/zoder-cli/src/agentic.rs`,
`crates/zoder-cli/src/main.rs`), the standalone template
(`config.standalone.toml`, via `zoder init --profile standalone`) and
the fleet template (`config.fleet.toml`, via `zoder init --profile
fleet`). Every statement cites file and function. Placeholders only;
no fleet addresses, no keys.

## Key table (checked by `scripts/test_config_reference.py`)

| key |
| --- |
| `[deployment].mode` |
| `[profile].name` |
| `[profile].default` |
| `[profile].default_provider` |
| `[profile].primary_model` |
| `[profile].reviewer_model` |
| `[[providers]].id` |
| `[[providers]].base_url` |
| `[[providers]].kind` |
| `[[providers]].auth` |
| `[[providers]].engine_provider_ref` |
| `[[providers]].model_aliases` |
| `[[providers]].paid` |
| `[[providers]].billing` |
| `[[providers]].serves` |
| `[[providers]].max_context_tokens` |
| `[model_families]` |
| `[budget].max_cost_per_call_usd` |
| `[budget].monthly_cap_usd` |
| `[budget].est_output_tokens` |
| `[review].max_diff_bytes` |
| `[review].max_hunk_bytes` |
| `[review].exclude` |
| `[review].route_defaults` |
| `[review].escalate` |
| `[review].confirm` |
| `[context].enabled` |
| `[routing].scenario` |
| `[providers.models.custom.*]` |
| `[agents.*].model` |
| `[agents.*].reviewer_model` |
| `[agents.*].model_provider` |
| `[agents.*].risk_profile` |
| `[agents.*].runtime_profile` |

## Routing overlay

- `[deployment].mode` is `standalone` (default) or `fleet`
  (`config.rs:DeploymentConfig::mode`, `DeploymentMode`). Profile-
  dependent defaults resolve through one accessor
  (`config.rs:ProfileDefaults`): standalone keeps fleet tuning out
  unless opted in.
- `[profile]` carries `name`, `default`, `default_provider`,
  `primary_model` (author model) and `reviewer_model`
  (`config.standalone.toml`, `[profile]` block). `reviewer_model` is an
  ordered chain, head first, comma-separated
  (`config.rs:parse_reviewer_chain`); with one key the unavailable
  entry fails and zoder warns before falling back
  (`config.standalone.toml` reviewer comment).
- `[[providers]]` entries become routable `Provider`s
  (`config.rs:Provider`, `VendorOverlay::providers` merge at
  `config.rs:4994`). Fields:
  - `id`, `base_url` (the standalone template uses the host-free
    `unset` sentinel and refuses to send while it stands;
    `config.standalone.toml` providers comment),
  - `kind`: `openai-chat` | `openai-responses` | `azure-openai` |
    `anthropic` | `custom` (`config.rs:Provider::kind`),
  - `auth`: `none` | `env { var }` | `Bearer { token }` (discouraged,
    ad-hoc) | `ApiKeyHeader { header, var }` for gateways with a custom
    header (`config.rs:Auth`). Debug output never prints a live secret
    (`config.rs:impl Debug for Auth`).
  - `engine_provider_ref`: the exact zeroclaw provider reference such
    as `custom.minimax` (`config.rs:EngineProviderRoute`,
    `config.rs:Provider::engine_provider_ref`); compatibility is
    checked by `engine_kind_is_compatible` /
    `engine_endpoint_is_compatible` (`config.rs`).
  - `model_aliases`, `paid`, `billing` (`BillingMode`: metered and
    flat-plan variants; `config.rs:Provider`, `is_flat_plan`),
    `serves` (model-id prefix list), `max_context_tokens` (conservative
    working window even when the provider advertises more;
    `config.standalone.toml`),
  - `azure_api_version`, `overload_backoff_min_s`,
    `first_token_timeout_s`, `stream`, `rate_limit_group`
    (default: the provider's own `id`), `max_concurrent`,
    `max_requests_per_minute` (`config.rs:Provider`).
  - optional `subscription` with quota `windows` (`name`, `hours`,
    `unit`, `cap`, `models`, `observability`, `reset`;
    `config.rs:QuotaWindow`, `SubscriptionPlan`).
- `[model_families]` maps a served model id or glob to a family name
  (`config.rs:Config::model_families`). Matching is case-insensitive;
  empty keys, wildcard-only keys, empty families and case-colliding
  keys are load-time errors (`config.rs` model-family validation).
- `[budget]` fields: `max_cost_per_call_usd` (per-call estimate
  ceiling; gated by confirmation or `--allow-paid`),
  `monthly_cap_usd` (rolling month-to-date cap against the local
  ledger), `est_output_tokens` (default 1024)
  (`budget.rs:Budget`). Caps gate paid calls only; `0` or absent
  disables a cap (`config.standalone.toml` budget comment).
- `[review]`: `max_diff_bytes` (CLI `--max-diff-bytes`),
  `max_hunk_bytes` (CLI `--max-hunk-bytes`), `exclude` globs (plus
  `.zoderignore` and `--exclude`), per-route `route_defaults` keyed by
  reviewer model id, optional `escalate` (high-risk paths needing a
  non-local reviewer), `[review.confirm]` (confirm-on-reject chain and
  per-severity limits; `insufficient`/`confirmer_failed` never demote)
  (`config.rs:ReviewConfig`, `ReviewConfirmConfig`,
  `RouteReviewDefaults`).
- `[context]`: standalone leaves levers off (`enabled = false` with
  conservative per-provider windows);
  fleet selects `profile = "fleet"`
  (`config.standalone.toml`, `config.fleet.toml`).
- `[routing].scenario` picks one of four built-in presets (default
  `balanced`); `[routing.scenarios.<name>]` overrides
  (`config.rs:Config::routing`, `RoutingConfig`).

## Engine config surface

- `[providers.models.<family>.<alias>]`: provider profile with `model`,
  `uri`, `api_key`, `native_tools`, per-profile `request_timeout_s`
  (`config.rs:EngineProviderRoute::from_toml`, `from_json`;
  `[providers.models.custom.groq]` shape exercised in
  `config.rs` engine-registry tests).
- `[agents.<alias>]`: `model` (author pin; beats `primary_model`,
  loses to `-m`), `reviewer_model` (beats `reviewer_model`, loses to
  `--reviewer`), `model_provider` (zeroclaw provider chain when `model`
  is unset), `risk_profile` (for example `default`),
  `runtime_profile` (for example `zoder_coder`)
  (`config.rs:AliasedAgentConfig`, engine-registry test shape).

## Environment variables

- `ZODER_HOME`: config directory (`$ZODER_HOME/config.json`,
  `$ZODER_HOME/config.<vendor>.toml` overlays); empty, relative or
  unresolvable values are rejected
  (`config.rs:resolve_home_from`, `Config::load`).
- `ZEROCLAW_CONFIG_DIR`: process-global engine config source; engine
  overrides layer on top of it. Must not be mutated by the runner
  (`agentic.rs` engine-config docs).
- `ZEROCLAW_SOCKET`: daemon socket; the runner points it at the job
  home's `daemon.sock` (`agentic.rs`, socket setup).
- `MNEMOS_URL` / `MNEMOS_TOKEN`: loop-event memory backend (read by
  the loop-events path; unset disables it).
- `HIVE_JOB_ID`: present when running under Hive; drives the
  `ZODER_LOOP_RESULT` line (`agentic.rs` loop-result docs).
- `ZODER_LOOP_RESULT`: `1` forces the result line, `0` suppresses it
  even under Hive (`agentic.rs`).
- `ZODER_TIMEOUT_S`: per-model request budget floor in the timeout
  precedence: `--request-timeout` > model `request_timeout_s` >
  engine provider profile > global `request_timeout_s` >
  `ZODER_TIMEOUT_S` > built-in default
  (`main.rs:effective_request_timeout_s_for_model`).
- `ZODER_QUEUE_TIMEOUT_S`: bound on waiting for a provider turn
  (`config.rs:Provider` queue docs).
- `ZODER_PRICING_SOURCE=file`: installed pricing files are
  authoritative, never re-pulled (`config.rs:Config::pricing`,
  `PricingConfig`).

## CLI route flags

- `--allowed-routes provider=model,...`: pin the author chain
  (`agentic.rs` route resolution).
- `--reviewer-allowed-routes provider=model,...`: pin the reviewer
  chain; required whenever `--reviewer-pool-routes` is used
  (`agentic.rs` reviewer-pool setup).
- `--reviewer-pool-routes provider=model,...`: secondary-only pool.
  The primary reviewer (chain head) is preserved and every other
  candidate is replaced by verified, healthy, latency-ranked pool
  members. A reviewer pool primary must not be an author route, and
  the secondary pool must contain neither the primary nor any author
  route (`agentic.rs` reviewer-pool setup).
