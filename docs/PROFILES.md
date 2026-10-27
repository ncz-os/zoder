# Deployment profiles: standalone and fleet

zoder has one global deployment profile, selected in `config.json` or an
overlay TOML:

```toml
[deployment]
mode = "standalone"   # default when the table is absent
# mode = "fleet"
```

This is separate from two other things with "profile" in the name:

* the vendor-overlay `[profile]` table (`default_provider`, `primary_model`, …),
* `[context] profile`, which selects the context-lever numbers.

`[context] profile` defaults to the global deployment profile. An explicit
`[context] profile` still wins, so an operator can run a fleet deployment with
the conservative standalone levers (or the reverse).

## Choose a profile

```bash
# Single host (the default)
zoder init --profile standalone

# Shared/fleet deployment
zoder init --profile fleet
```

Both commands write into `$ZODER_HOME` and do nothing when a zoder configuration
already exists. `--force` replaces the target file. `install.sh` accepts the
same flag (`--profile standalone|fleet`) and defaults to `standalone`.

`config.standalone.toml` is the single-host starter: two hosted providers with
environment-variable auth, an optional commented local OpenAI-compatible
endpoint, an author/reviewer route that works with one provider key, a
conservative cost backstop, and context limits. `config.fleet.toml` is only a
profile selector; the fleet's providers and tuning come from the fleet's own
configuration repository and are layered on top at deploy time.

A fleet deployment must select the profile once (`[deployment] mode = "fleet"`,
or use the fleet template above). Without it the deployment is standalone by
definition, so fleet-only defaults (context levers, runner job-id emission, the
tighter cancel budget) are not applied.

## What the profile changes

Fleet values below are the current tuned behaviour. A fleet config does not
need to restate them. A standalone install gets the safer default.

| Setting | Standalone | Fleet | Status |
|---|---|---|---|
| Context levers (`[context]`) | OFF; bigger caps, later compaction, more recent turns kept when enabled | ON with the tuned numbers | Wired: `Config::context_levers_for` inherits `[deployment] mode`, explicit `[context] profile` wins |
| Fleet job workspaces / runner job id | Off (no runner-assigned id is published) | On | Wired: `agentic.rs` checks `ProfileDefaults::fleet_job_workspaces` |
| `session/cancel` settle budget | 1500 ms | 750 ms | Wired: `reaper::cancel_budget_for` |
| Cost backstop default | $50/month suggested for a fresh template | Unset; the fleet's config repository owns fleet caps | Template value; `[budget]` is what the pre-call gate reads |
| Provider reviewer concurrency | Unlimited (0) | Unlimited unless `ZODER_PROVIDER_CONCURRENCY` is set | Wired: `agentic.rs` reads `ProfileDefaults::provider_concurrency_default` |
| Secondary reviewer pool | Off | Off | Opt-in via `--reviewer-pool-routes` on both profiles; not profile-controlled |

The wired fleet optimisations (context optimisation, runner job-id emission,
the cancel settle budget, provider reviewer concurrency) read their defaults
from `Config::profile_defaults()` / `ProfileDefaults`, not from hardcoded fleet
numbers. That keeps the fleet's current behaviour and gives a single-host
install the conservative side by default. Accessor hooks for terse I/O, shared
daemon assumption, and fleet update pulling exist in `ProfileDefaults` for
future use but are not yet consulted by any production path.

## Context inheritance

```toml
[deployment]
mode = "fleet"

# Optional: this is what the deployment already supplies.
[context]
profile = "fleet"
```

An explicit `[context] profile = "standalone"` under `[deployment] mode =
"fleet"` is honoured. An explicit `enabled = false` turns the levers off
regardless of profile.

## Paid gating

Hosted routes are metered. Paid calls require an explicit opt-in:
`--allow-paid` on the command line, or interactive confirmation. There is no
persistent "always allow paid" switch. The `[budget]` caps
(`max_cost_per_call_usd`, `monthly_cap_usd`) gate a capped estimate the same
way while `--allow-paid` is absent; `--allow-paid` is the per-invocation
bypass, not a config-wide setting. A local OpenAI-compatible endpoint is `$0`
marginal and does not need the opt-in.

## Fleet configuration

The fleet profile intentionally contains no endpoints, host names, private
addresses or slot limits. Those live in the fleet's own configuration
repository and are synced into `$ZODER_HOME` at deploy time. `zoder config
--validate` checks the merged result on the host.
