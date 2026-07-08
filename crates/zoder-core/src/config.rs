//! Runtime configuration: provider endpoints, auth, and on-disk paths.
//!
//! Vendor-neutral: free-tier is just the default provider entry. Any
//! OpenAI-compatible/LiteLLM backend is added here without code changes.
//!
//! ## Layered config (vendor overlays)
//!
//! `Config::load()` reads `$ZODER_HOME/config.json` (or the default free-tier
//! config) and then layers every `config.<vendor>.toml` sibling in the same
//! directory on top. Each TOML is a vendor profile (e.g. `config.enterprise.toml`,
//! `config.ibm.toml`, `config.microsoft.toml`) that contributes additional
//! `[[providers]]` and, optionally, a `[profile]` table that selects a
//! `default_provider`. The TOML files are the source of truth for what counts
//! as "enterprise spend" / "IBM spend" / etc. in `zoder report --vendor <name>`.
//!
//! A duplicate provider `id` contributed by two overlays is a hard load error;
//! fix the TOML, don't let the last-writer silently win.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Host substring of the built-in placeholder `default` provider. A host with
/// no real routing config resolves every model to this sentinel endpoint;
/// [`Config::real_provider_for_model`] treats a match as "no provider
/// configured" so the router never auto-picks an unbacked model and callers
/// hard-error instead of dialing a bogus URL. Kept as a constant so the
/// sentinel and the detector can never drift.
pub const PLACEHOLDER_PROVIDER_HOST: &str = "api.example.com";

/// Effective `account_id` for a `SubscriptionPlan` that did not declare an
/// `account_id` in config. Centralized so the placeholder and every accessor
/// / validator agree on the same string. Two subscription providers with the
/// same `(provider, account_id, tier)` collapse onto the same logical
/// identity; pinning this to a single constant makes the "absent == default"
/// rule auditable (and reusable across the future per-account rewire).
pub const DEFAULT_ACCOUNT_ID: &str = "default";
use std::path::{Path, PathBuf};

/// How a provider authenticates. Secrets are never stored in the repo; only
/// references (env var names) or values supplied at runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Auth {
    None,
    /// Read a bearer token from this environment variable.
    Env {
        var: String,
    },
    /// Inline bearer token (discouraged; for ad-hoc use).
    Bearer {
        token: String,
    },
    /// Enterprise gateways that authenticate with a custom request header
    /// instead of `Authorization: Bearer` — e.g. Azure OpenAI's `api-key`
    /// header, or an OCI/gateway fronting an OpenAI-compatible endpoint. The
    /// secret is read from env `var` and sent verbatim in header `header`.
    ApiKeyHeader {
        header: String,
        var: String,
    },
}

impl Auth {
    /// The raw credential value, used for presence checks and display. For
    /// header-style auth this is the resolved env value. `None` when unset or
    /// empty.
    pub fn resolve(&self) -> Option<String> {
        match self {
            Auth::None => None,
            Auth::Env { var } => std::env::var(var).ok().filter(|s| !s.is_empty()),
            Auth::Bearer { token } => Some(token.clone()),
            Auth::ApiKeyHeader { var, .. } => std::env::var(var).ok().filter(|s| !s.is_empty()),
        }
    }

    /// The `(header-name, header-value)` pair to attach to an outbound request,
    /// or `None` when there is no usable credential. Bearer styles render as
    /// `Authorization: Bearer <token>`; `ApiKeyHeader` sends `<header>: <value>`
    /// (the shape raw Azure OpenAI and several enterprise gateways require).
    pub fn header_pair(&self) -> Option<(String, String)> {
        match self {
            Auth::None => None,
            Auth::Env { .. } | Auth::Bearer { .. } => self
                .resolve()
                .map(|tok| ("authorization".to_string(), format!("Bearer {tok}"))),
            Auth::ApiKeyHeader { header, .. } => self.resolve().map(|val| (header.clone(), val)),
        }
    }
}

/// How a provider is billed. This is independent of a model's catalog rate:
/// it captures *how you actually pay*, which the report needs to tell real
/// dollars apart from quota consumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BillingMode {
    /// Free / open-weight / local: $0 marginal, effectively uncapped.
    Free,
    /// Pay-as-you-go API: marginal cost = tokens x catalog rate (the default).
    #[default]
    Metered,
    /// Flat-fee subscription with rate-limit windows: marginal cost is $0, but
    /// each call consumes a capped rolling window (and the flat fee can be
    /// amortized for an effective per-call figure).
    Subscription,
}

/// How a rolling rate-limit window counts. `Sessions` counts discrete
/// agent/conversation sessions (e.g. Cursor / Windsurf-style caps) rather
/// than tokens, requests, or message round-trips — declaring all three
/// common shapes so any provider's flat-fee plan is expressible in config
/// without code changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuotaUnit {
    #[default]
    Tokens,
    Requests,
    Messages,
    Sessions,
}

/// How a `QuotaWindow` is fed. `Header` means the rate-limit headers on the
/// provider's HTTP response (the KNEMON "best" path — known exact values).
/// `Counter` means a local counter (the legacy `quota.rs` model-plus-ledger
/// path). `PercentOnly` means the cap itself is unknown and the window is
/// observable only as a used-percent — never a headroom calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Observability {
    #[default]
    Header,
    Counter,
    PercentOnly,
}

/// How a `QuotaWindow` resets. `Rolling` is the legacy "trailing N hours"
/// semantics already implemented in `quota.rs`. `CalendarMonthly` and
/// `CalendarDaily` describe provider calendars (e.g. Codex weekly quota
/// resets Mon 00:00 UTC); the window's `hours` is informational in those
/// cases — the engine MUST look at the provider's reset signal, not the
/// `hours`-based aging, when `reset != Rolling`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResetKind {
    #[default]
    Rolling,
    CalendarMonthly,
    CalendarDaily,
}

/// A rolling rate-limit window on a subscription (e.g. a 5-hour cap or a weekly
/// cap). Consumption is measured from the local ledger over `hours`, except
/// when `reset` says the provider resets on a calendar boundary.
///
/// `cap = None` means the cap is **unknown** — the window is observable only as
/// percent (an Anthropic dashboard-style "82% of weekly budget consumed" view
/// without a raw token figure). When `cap = None`, `quota.rs` treats the
/// window as permanently below cap (headroom), NEVER as saturated on the
/// strength of a zero denominator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaWindow {
    /// Display name, e.g. "5h" or "weekly".
    pub name: String,
    /// Rolling window length in hours (5h = 5, weekly = 168).
    pub hours: u32,
    #[serde(default)]
    pub unit: QuotaUnit,
    /// Cap value, in `unit`, over the rolling window. `None` = unknown /
    /// percent-only — the window still exists but cannot drive a headroom or
    /// "exhausted" decision on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<f64>,
    /// Model-id glob patterns this window limits. `None` means "all models on
    /// the provider" (the legacy single-cap shape). Set to a list of globs
    /// (e.g. `["MiniMax-M3", "claude-opus-*"]`) to express a per-model cap —
    /// Anthropic publishes Sonnet / Opus / Haiku as separately-capped models
    /// on the same endpoint, and that's what this field is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    /// How this window's consumption is observed / fed. Defaults to `Header`
    /// (KNEMON's "best" path), so a minimal config keeps the same semantics
    /// it had before this field existed.
    #[serde(default)]
    pub observability: Observability,
    /// How this window resets. Defaults to `Rolling` (legacy "trailing
    /// `hours`" semantics). Set `CalendarMonthly` / `CalendarDaily` for
    /// provider-driven reset signals.
    #[serde(default)]
    pub reset: ResetKind,
}

/// Subscription terms for a flat-fee provider (ChatGPT/Claude/Cursor-style).
///
/// A plan can be declared in three shapes (see
/// [`crate::subscription_tiers::resolve_plan_windows`]):
///   1. **Explicit**: `windows: [...]` is set, no `tier` → used as-is.
///   2. **Preset**:   only `tier: "..."` is set → catalog lookup fills the
///      windows.
///   3. **Preset + overrides**: both are set → preset windows, then explicit
///      windows override by `name` (operator tunes one cap).
///
/// ## Per-account identity (KNEMON adversarial-review finding #3)
///
/// `account_id` is an **optional**, stable, operator-supplied label for the
/// human/team behind this plan (e.g. `"personal"`, `"work"`, `"ci-bot"`).
/// It is NOT a credential and is NOT the auth subject — it is purely a
/// routing/identity key that lets a single host express multiple accounts
/// on the same `(provider, tier)` combination. KNEMON's per-account
/// portfolio intelligence keys its snapshots by `(provider, account_id,
/// plan)`; without this field every config-author collapses to the literal
/// default and two subscriptions on the same provider+tier silently
/// collide.
///
/// Absent (`account_id: null` or omitted) → the effective id is the
/// constant [`DEFAULT_ACCOUNT_ID`] (see
/// [`SubscriptionPlan::effective_account_id`]). This preserves backward
/// compatibility with every existing config; a host that hasn't been
/// touched continues to load and behave exactly as today.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionPlan {
    /// Flat monthly fee in USD (used only to amortize an effective per-call $).
    #[serde(default)]
    pub monthly_fee_usd: f64,
    /// Optional curated tier id (e.g. `claude-max-20x`, `token-plan-2`). When
    /// set, the windows come from [`crate::subscription_tiers::TierCatalog`]
    /// resolved at load time. May be combined with `windows` to override
    /// individual caps by `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// Rolling rate-limit windows (e.g. a 5-hour cap plus a weekly cap).
    /// - With `tier = None`: used as-is.
    /// - With `tier = Some(_)`: every window with a `name` that also exists
    ///   in the catalog preset overrides that cap; windows without a
    ///   matching preset `name` are appended as extra windows.
    #[serde(default)]
    pub windows: Vec<QuotaWindow>,
    /// Optional stable per-account identity for this plan (see type-level
    /// docs above). `None` ⇒ effective account id is
    /// [`DEFAULT_ACCOUNT_ID`]. Backward-compatible: legacy configs that
    /// omit this field load cleanly and behave exactly as today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl SubscriptionPlan {
    /// The effective `account_id` for this plan: the `account_id` field
    /// when set and non-empty (after trimming), otherwise the sentinel
    /// [`DEFAULT_ACCOUNT_ID`]. Whitespace-only `account_id` is treated as
    /// absent so a typos like `" "` don't sneak past validation under a
    /// distinct key. Returns an owned `String` rather than `&str` because
    /// the user-supplied case doesn't have a static lifetime and an
    /// allocation-free `&'static str` view would either require leaking
    /// `account_id` strings or borrowing through self awkwardly.
    /// `account_id` is a low-cardinality operator-supplied label (think
    /// `"personal"` / `"work"` / `"ci-bot"`) and `effective_account_id` is
    /// not on a per-call hot path, so the allocation is acceptable.
    pub fn effective_account_id(&self) -> String {
        match self.account_id.as_deref() {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => DEFAULT_ACCOUNT_ID.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub id: String,
    pub base_url: String,
    #[serde(default = "default_kind")]
    pub kind: String, // openai-chat | openai-responses | azure-openai | anthropic | custom
    pub auth: Auth,
    /// Provider serves only paid models (used by the policy gate as a hint).
    #[serde(default)]
    pub paid: bool,
    /// How this provider is billed (metered API, flat-fee subscription, free).
    #[serde(default)]
    pub billing: BillingMode,
    /// Subscription terms, when `billing = subscription`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription: Option<SubscriptionPlan>,
    /// Model-id prefixes this provider serves, used for per-model routing
    /// (`Config::provider_for_model`). A routed model id is sent to the FIRST
    /// provider whose `serves` prefix it matches, instead of always going to
    /// `default_provider`. This lets one fallback chain span providers — e.g.
    /// `MiniMax-M3` -> the `minimax` provider, `nvidia/*` -> the `nvidia-eih`
    /// provider — in a single `zoder exec`. Empty (the default) means this
    /// provider claims no models by prefix and is only reached as the
    /// `default_provider`. Prefixes are matched with `str::starts_with`.
    #[serde(default)]
    pub serves: Vec<String>,
    /// Override the Azure OpenAI Data Plane API version (`api-version` query
    /// parameter) for `kind == "azure-openai"` providers. The base URL is
    /// expected to already encode the deployment route
    /// (`…/openai/deployments/<deployment>`) per the Azure OpenAI wire
    /// contract — the deployment itself is therefore intentionally NOT a
    /// separate field, matching how every Azure SDK / curl example builds
    /// the URL.
    ///
    /// Resolution precedence for `kind == "azure-openai"`:
    ///   1. `azure_api_version` field on this provider (per-provider
    ///      override; what most multi-tenant operators will use),
    ///   2. `AZURE_OPENAI_API_VERSION` environment variable (host-wide
    ///      override; legacy / CI workflows),
    ///   3. built-in default `"2024-10-21"` (the current GA Data Plane
    ///      version as of this commit).
    ///
    /// `None` (the default) ⇒ fall through to the env var / built-in default.
    /// Other kinds (`openai-chat`, `openai-responses`, `anthropic`,
    /// `custom`) ignore this field — the OpenAI chat-completions path
    /// doesn't accept `api-version` and Anthropic pins its own version
    /// header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub azure_api_version: Option<String>,
}

fn default_kind() -> String {
    "openai-chat".into()
}

/// Named report colour palette. Each field is an ANSI SGR parameter string
/// (e.g. `"38;2;77;163;255"` truecolor, or `"33"` 8-colour). An org overlay's
/// `[theme]` block brands its reports; any omitted field falls back to the
/// built-in blue/white default. The theme only chooses *which* colours to use
/// — colour is still suppressed entirely when stdout is not a TTY or `NO_COLOR`
/// is set, so a themed deployment stays pipe-safe.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    /// Bold accent for section headers and headline figures.
    pub header: String,
    /// Accent / brand colour (totals, emphasis).
    pub accent: String,
    /// "Good" emphasis — free / $0 / success.
    pub ok: String,
    /// Caution — billed/paid cash, warnings.
    pub warn: String,
    /// Policy violations / errors.
    pub violation: String,
    /// Secondary / muted text (table headers, rules, hints).
    pub dim: String,
}

impl Default for Theme {
    fn default() -> Self {
        // Built-in blue/white palette (the historical zoder default).
        Self {
            header: "1;38;2;77;163;255".into(),
            accent: "38;2;77;163;255".into(),
            ok: "38;2;77;163;255".into(),
            warn: "38;2;240;240;240".into(),
            violation: "38;2;220;80;80".into(),
            dim: "2".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub providers: Vec<Provider>,
    /// Default provider id for routed (`auto`) requests.
    pub default_provider: String,
    pub corpus_path: PathBuf,
    pub ledger_path: PathBuf,
    pub health_path: PathBuf,
    /// Hosts considered free/internal for the anti-paid-fallback guard.
    /// Matched by exact host or registrable suffix (never substring).
    #[serde(default = "default_free_hosts")]
    pub free_api_hosts: Vec<String>,
    /// Fail closed: a "free" call with no cost/api_base/fallback telemetry is
    /// treated as a policy violation. `--lenient-telemetry` relaxes this.
    #[serde(default = "default_strict_free")]
    pub strict_free: bool,
    /// Vendor provenance for each provider id, populated by `Config::load()`
    /// from `config.<vendor>.toml` overlays. Providers from `config.json` or
    /// the default free-tier config are absent from this map (they're
    /// "base" providers, not vendor-tied). Used by `zoder report --vendor X`
    /// to filter the ledger to a specific vendor's providers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vendor_provenance: BTreeMap<String, Vec<String>>,
    /// Active report colour theme, resolved from a `[theme]` block in an org
    /// overlay (the default-claiming overlay wins; otherwise the
    /// alphabetically-last overlay that defines one). Falls back to the
    /// built-in blue/white palette.
    #[serde(default)]
    pub theme: Theme,
    /// Pinned routing primary: a model id the router always tries FIRST,
    /// ahead of the capability/health-ranked free pool. Set from a vendor
    /// overlay's `[profile].primary_model` (e.g. the MiniMax subscription
    /// model). When set and the model is a known free candidate, the router's
    /// `select()` makes it the primary and ranks everything else as fallbacks.
    /// `None` keeps the pure capability-first ordering.
    ///
    /// Resolution precedence (highest first) on the CLI side:
    ///   1. explicit `-m <model>` (per-invocation) wins,
    ///   2. the selected agent's own `[agents.<alias>].model`,
    ///   3. this `primary_model` (the fallback DEFAULT),
    ///   4. capability/health-ranked auto routing.
    ///
    /// `primary_model` is intentionally a DEFAULT only — it must NOT silently
    /// override a per-agent or per-invocation pin (regression 2026-07-04:
    /// `primary_model="MiniMax-M3"` overrode `[agents.codex].model="gpt-5.5"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_model: Option<String>,
    /// `adversarial-review` and the `loop` reviewer when neither
    /// `--reviewer <model>` nor a `[agents.<alias>].reviewer_model` is set.
    /// Independent of `primary_model` so an operator can pin a strong
    /// cross-family reviewer without touching the author default. Falls
    /// back to a strong CROSS-FAMILY model derived from the resolved
    /// author model (see `zoder_core::default_cross_family_reviewer` from
    /// the CLI side) when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_model: Option<String>,
    /// Per-agent overrides keyed by zeroclaw agent alias (the value of
    /// `--agent`). Each entry may pin its own `model` (primary author) and
    /// `reviewer_model` (loop / `review` reviewer) so different agentic
    /// roles can use different model ids without polluting the global
    /// `primary_model` / `reviewer_model`. On the CLI, this is honored via
    /// [`Config::agent_model`] / [`Config::agent_reviewer_model`],
    /// consulted AFTER `-m` / `--reviewer` and BEFORE `primary_model`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub agents: BTreeMap<String, AliasedAgentConfig>,
    /// Pre-call spend caps. A paid call whose *estimated* cost would breach a
    /// cap is gated behind the same confirmation as a paid model. Empty by
    /// default (no caps). See [`crate::budget::Budget`].
    #[serde(default)]
    pub budget: crate::budget::Budget,
    /// Routing scenario preference layer. The operator picks one of the four
    /// built-in presets with `[routing].scenario` (default `balanced`); an
    /// advanced user may override any preset under `[routing.scenarios.<name>]`.
    /// Absent altogether => balanced defaults applied at load time
    /// (`RoutingConfig::active()`), preserving the legacy free-only behavior.
    #[serde(default)]
    pub routing: RoutingConfig,
    /// OS-level sandbox backend selection for the loop's `--check` execution.
    /// Default (`backend = None`) is byte-for-byte identical to the prior
    /// behavior — the loop consults the string denylist only. Selecting
    /// `backend = "seatbelt"` on a macOS host wraps `sh -c` in
    /// `/usr/bin/sandbox-exec -p <profile>`; selecting `backend =
    /// "linux_bubblewrap"` on a Linux host wraps `sh -c` in `bwrap <argv>
    /// -- sh -c <cmd>`. Either backend selected on the wrong host errors
    /// at runtime with a clear "unsupported on this platform" message
    /// (see `crates/zoder-cli/src/exec_safety::wrap_spawn_command`).
    #[serde(default)]
    pub exec_safety: ExecSafetyConfig,
}

/// Routing-scenario block from `config.json` / an overlay TOML. Mirrors the
/// `[routing]` table; the actual scenario data lives in `scenarios`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingConfig {
    /// Active scenario name (e.g. `economy`, `balanced`, `aggressive`,
    /// `unlimited`). Defaults to `balanced`; an unknown name is a graceful
    /// no-op (still resolves to `balanced`) so a typo doesn't break routing.
    #[serde(default = "default_scenario_name")]
    pub scenario: String,
    /// Per-scenario overrides (fields omitted fall through to the preset).
    /// The map keys are the preset names (`economy`, `balanced`, ...).
    ///
    /// Every entry is a [`RouteScenarioOverride`] — a sparse shape where
    /// every field is `Option<T>`. A config block that only carries one
    /// field (e.g. `cap_guard = 55`) parses with `Some(_)` on that field
    /// and `None` everywhere else, so the merge keeps the rest of the
    /// preset intact. (Pre-fix this was a `RouteScenario`, which used
    /// `#[serde(default)]` per field and silently replaced the preset
    /// with generic balanced defaults — see Finding #8.)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scenarios: BTreeMap<String, crate::scenarios::RouteScenarioOverride>,
}

fn default_scenario_name() -> String {
    "balanced".into()
}

impl Default for RoutingConfig {
    /// `RoutingConfig::default()` is the *absent* `[routing]` block — it
    /// must behave exactly as if the operator had typed
    /// `[routing]\nscenario = "balanced"` so a config-less host enjoys
    /// the legacy free-only default without any explicit declaration.
    fn default() -> Self {
        Self {
            scenario: default_scenario_name(),
            scenarios: BTreeMap::new(),
        }
    }
}

impl RoutingConfig {
    /// Resolve the currently-active scenario by name, layering any operator
    /// override on top of the matching preset. Falls back to `balanced` for
    /// an unknown name. Backward compatible: when the `[routing]` block is
    /// absent the default-constructed `RoutingConfig` already names
    /// `balanced`, so existing hosts behave exactly like a `balanced`-set
    /// host with no overrides.
    pub fn active(&self) -> crate::scenarios::RouteScenario {
        let ovr = self.scenarios.get(&self.scenario);
        crate::scenarios::resolve_active(&self.scenario, ovr)
    }
}

fn default_free_hosts() -> Vec<String> {
    vec!["example.com".into(), "free.example.com".into()]
}

/// Per-agent overrides under `[agents.<alias>]` in `config.json`. Both fields
/// are optional and are honored independently of the global `primary_model`
/// / `reviewer_model`: when set they win over the globals but lose to an
/// explicit `-m` / `--reviewer` on the CLI (per-invocation overrides always
/// win). This is the fix for the 2026-07-04 regression where `primary_model`
/// was forcing every agent onto the same model regardless of its own
/// config.
///
/// Both fields are `#[serde(default)]`-able: a config.json that omits
/// `agents`, or a per-agent block that omits `reviewer_model`, parses
/// cleanly into the missing-`None` shape so the existing single-pinned
/// `Config::primary_model` deployment keeps working.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AliasedAgentConfig {
    /// Primary (author) model id for this agent. When `Some`, takes
    /// precedence over `Config::primary_model` but is overridden by `-m`
    /// on the CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reviewer / secondary model id for this agent. Independent of
    /// `Config::primary_model`. When `Some`, takes precedence over
    /// `Config::reviewer_model` but is overridden by `--reviewer` on the
    /// CLI. May be `None` even when `model` is set, in which case the
    /// loop / `review` call falls back to `Config::reviewer_model`, then
    /// to the auto cross-family reviewer derived from the resolved author
    /// model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_model: Option<String>,
}

fn default_strict_free() -> bool {
    true
}

/// Parse a `Config::reviewer_model`-style value into an ordered list of
/// reviewer candidates (head first). The legacy shape is a single model id
/// (a one-element `Vec`); the reviewer chain format introduced by the
/// cross-model fallback fix extends that to a comma-separated list (e.g.
/// `"model_a,model_b,model_c"`) so a single broken provider doesn't sink
/// the whole adversarial review. Whitespace around each entry is trimmed
/// and empty entries are dropped so a trailing comma degrades to a
/// one-element list rather than producing `["model_a", ""]`.
///
/// Public-in-crate visibility so the unit tests under this module can
/// exercise the parser directly (the same way the existing
/// `default_provider` helpers are tested).
pub(crate) fn parse_reviewer_chain(raw: Option<&str>) -> Vec<String> {
    raw.map(|s| {
        s.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

/// One provider-as-candidate for the smart router, decorated with its rank
/// criteria (billing tier + prefix specificity). The struct exists so the
/// inner ranking function can return multiple candidates (tests inspect
/// them, future "show me the fallback chain" UI can too) and so the sort
/// key (`Ord` on `(billing_tier, prefix_len)`) is named once.
struct RankedProvider<'a> {
    provider: &'a Provider,
    /// Longest matching `serves` prefix; higher = more specific.
    prefix_len: usize,
    /// Cost/preference tier (`0` = best, cheapest). See `billing_tier`
    /// for the enum and the assignment.
    billing_tier: BillingTier,
}

/// Ranking tier for the smart router: smaller ordinals are preferred. The
/// ordering encodes the whole "subscription with quota beats metered;
/// exhausted-window subscription falls through to metered" rule.
///
/// - `Free` (`0`): $0 marginal, no windows to be exhausted on. Always wins
///   over everything except a longer-prefix competing Free provider.
/// - `SubscriptionLive` (`1`): a subscription provider with remaining
///   window quota. Marginal cost is $0; the constraint is the rolling
///   cap.
/// - `Metered` (`2`): pay-as-you-go. Billed per call.
/// - `SubscriptionExhausted` (`3`): a subscription whose rolling window
///   is at/over cap. The API call would error, so we transparently fall
///   through to a metered alternative. Only surfaces as a last resort
///   when no live provider claims the model — operators should re-route
///   or wait for the window to elapse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BillingTier {
    Free = 0,
    SubscriptionLive = 1,
    Metered = 2,
    SubscriptionExhausted = 3,
}

/// Decide the billing tier for a provider given its config and (for a
/// `Subscription`) the live usage measured from the ledger. A subscription
/// with no resolved windows (no explicit `windows`, no catalog tier, or an
/// unknown tier) is treated as `SubscriptionLive` (uncapped or
/// operator-trusted): it would be wrong to demote an "I don't track this"
/// plan to metered just because we couldn't see its caps.
fn billing_tier(
    p: &Provider,
    entries: &[crate::ledger::Entry],
    catalog: &crate::subscription_tiers::TierCatalog,
) -> BillingTier {
    match p.billing {
        BillingMode::Free => BillingTier::Free,
        BillingMode::Metered => BillingTier::Metered,
        BillingMode::Subscription => {
            if subscription_window_exhausted(p, entries, catalog) {
                BillingTier::SubscriptionExhausted
            } else {
                BillingTier::SubscriptionLive
            }
        }
    }
}

/// `true` when a subscription provider has at least one rolling window that
/// is at/over cap per the ledger+catalog resolution. A provider with NO
/// resolved windows returns `false` — there is no cap to be over. The
/// check reads the local ledger (via [`crate::quota::plan_usage`]); it is
/// inherently best-effort, exactly like the utilization report, and the
/// window rolls forward so the same provider automatically becomes the
/// preferred choice again once usage drops back under cap.
fn subscription_window_exhausted(
    p: &Provider,
    entries: &[crate::ledger::Entry],
    catalog: &crate::subscription_tiers::TierCatalog,
) -> bool {
    let Some(plan) = p.subscription.as_ref() else {
        return false;
    };
    let catalog_provider = plan
        .tier
        .as_deref()
        .and_then(|tier| catalog.provider_namespace(p, tier))
        .unwrap_or_else(|| p.id.clone());
    let usage = crate::quota::plan_usage_for_catalog_provider(
        entries,
        &p.id,
        plan,
        catalog,
        &catalog_provider,
    );
    if usage.is_empty() {
        return false;
    }
    // A subscription is "exhausted" when ANY of its rolling windows is at
    // or over cap. The shortest window in the plan is the binding
    // constraint — if the 5h window is at 100% the API will refuse the
    // call even if the weekly window has headroom — so we treat any
    // saturated window as fatal. Recovery is automatic: each window
    // rolls forward independently on its own `hours` cadence, and
    // `window_usage().next_reset_utc` is the exact recovery moment.
    usage.iter().any(|w| w.pct >= 1.0)
}

// ---------------------------------------------------------------------------
// Execution-safety sandbox backend selection.
//
// This module's *portable* slice (`inspect_shell_command` in
// `crates/zoder-cli/src/exec_safety.rs`) is a pre-spawn STRING denylist — a
// guard rail, not a containment boundary. The types below are the
// opt-in OS-level sandbox BACKEND that actually wraps the spawned child in an
// OS containment primitive (macOS seatbelt, Linux bubblewrap).
//
// Default = `ExecSandbox::None` = exactly the legacy "denylist only" behavior,
// byte-for-byte. Selecting `Seatbelt` on a host that is not running macOS, or
// `LinuxBubblewrap` on a host that is not running Linux, is a HARD ERROR
// (`Err("…unsupported on this platform")`) at the call site, not a silent
// fallback — see `crates/zoder-cli/src/exec_safety::wrap_spawn_command` for
// the dispatch contract.
//
// We deliberately do NOT silently fall back to `None` on the wrong host — a
// half-implemented sandbox backend that pretends to contain is worse than
// none (see exec_safety module doc on the "half-working platform-specific
// code" failure mode). Landlock is a possible follow-up but is intentionally
// out of scope here: it would require a kernel-version-gated Rust crate and
// the bwrap wire-up already covers the deny-network + cwd-bind
// least-privilege surface the seatbelt backend provides on macOS.
// ---------------------------------------------------------------------------

/// Concrete OS-sandbox backend the loop should wrap `--check` execution in.
///
/// `serde` shape is intentionally a plain lowercase tag so the operator's
/// `config.json` / overlay TOML reads naturally (`"backend": "seatbelt"`).
/// Unknown variants deserialize to `Unsupported` via the `#[serde(other)]`
/// fallback below so a typo or a not-yet-implemented backend in a new build
/// does not turn into a hard config load failure (forward-compat — preserves
/// the "config keeps loading" contract the rest of `Config` already follows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecSandbox {
    /// No OS-level sandbox: the legacy pre-spawn string denylist is the only
    /// guard rail. **This is the default** so a config-less or unmodified
    /// host behaves byte-for-byte as it did before this change.
    #[default]
    None,
    /// macOS seatbelt (`/usr/bin/sandbox-exec -p <profile>`). On any other
    /// platform the call site MUST reject this with a clear "unsupported on
    /// this platform" error rather than silently downgrading to `None`.
    Seatbelt,
    /// Linux bubblewrap (`bwrap`) — an external userspace-sandbox wrapper
    /// invoked as `bwrap <args> -- <cmd>`. Mirrors seatbelt's
    /// "external-wrapper approach" 1:1 (the macOS backend invokes
    /// `/usr/bin/sandbox-exec`; the Linux backend invokes `/usr/bin/bwrap`
    /// when present on `$PATH`). We chose **bubblewrap over landlock** for
    /// this initial wiring for two reasons:
    ///
    ///   1. **Symmetry with seatbelt**: both macOS seatbelt and the bubblewrap
    ///      wrapper are external binaries invoked with a generated
    ///      declarative profile/argv. The dispatch site, the platform-guard
    ///      error, the cross-platform `cfg` contract, and the test surface
    ///      all slot in with zero shape changes.
    ///   2. **No new dependency**: bwrap is a single binary the operator
    ///      installs system-wide (`apt install bubblewrap`, `dnf install
    ///      bubblewrap`, etc.). Wiring `landlock` instead would mean adding
    ///      a kernel-version-gated Rust crate, conditional compilation
    ///      across `target_os = "linux"` × kernel `>= 5.13`, and a much
    ///      larger surface to test. Bubblewrap's `--unshare-net` and
    ///      `--bind` flags give us the same deny-network + cwd-bind
    ///      guarantees from userspace without that surface area.
    ///
    /// On any non-Linux platform the call site MUST reject this with a
    /// clear "unsupported on this platform" error rather than silently
    /// downgrading to `None` — the same cross-platform contract seatbelt
    /// establishes (inverted: Linux variant off-Linux is the error path,
    /// not seatbelt off-mac).
    LinuxBubblewrap,
    /// Forward-compat catch-all: a future build (or a typo) that names a
    /// backend this binary doesn't implement. We deserialize unknown tags
    /// into this variant so a config keeps loading — the dispatch site is
    /// the single place that surfaces the unsupported-backend error to the
    /// operator when they actually try to use it. Kept LAST so the
    /// `#[serde(other)]` fallback matches tags not enumerated above; any
    /// new variant must be added BEFORE this arm so it deserializes
    /// correctly.
    #[serde(other)]
    Unsupported,
}

/// Per-call-site knobs of the macOS seatbelt profile. These are the
/// well-known "least privilege" choices for the loop's validation command;
/// an operator who wants more freedom edits the config, an operator who
/// wants less freedom keeps the defaults. Each field maps to one SBPL
/// clause so the generated profile is auditable end-to-end (see
/// `crates/zoder-cli/src/exec_safety::generate_seatbelt_profile`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SeatbeltProfileOptions {
    /// Allow outbound network from inside the sandbox. Default `false`
    /// (deny-by-default) — most `--check` commands (`cargo test`, `pytest`)
    /// do not need network and a compromised test must not silently phone
    /// home. Operators who run network-dependent checks (e.g. `npm install`
    /// inside a `--check`) flip this to `true`.
    #[serde(default = "default_seatbelt_allow_network")]
    pub allow_network: bool,
    /// Mount the host's `/tmp` read-write inside the sandbox. Default
    /// `true` because `cargo`, `pytest`, `node`, and almost every common
    /// `--check` writes intermediates there. Operators on hardened hosts
    /// can flip this off; the SBPL will then deny writes under `/tmp` and
    /// any tool that needs scratch space will fail loudly.
    #[serde(default = "default_seatbelt_allow_tmp")]
    pub allow_tmp: bool,
    /// Read access to the user's `$HOME` (read-only; no writes). Default
    /// `false` because `~/.cargo`, `~/.npm`, etc. are common and a sandbox
    /// that can't read them will fail to compile most projects — but
    /// writing to `$HOME` from a `--check` is almost never legitimate and
    /// is denied by default. Operators who need to read `.gitconfig`,
    /// `.cargo/config.toml`, etc. without writing to `$HOME` flip this
    /// to `true`.
    #[serde(default = "default_seatbelt_allow_home_read")]
    pub allow_home_read: bool,
}

fn default_seatbelt_allow_network() -> bool {
    false
}
fn default_seatbelt_allow_tmp() -> bool {
    true
}
fn default_seatbelt_allow_home_read() -> bool {
    false
}

impl Default for SeatbeltProfileOptions {
    fn default() -> Self {
        Self {
            allow_network: default_seatbelt_allow_network(),
            allow_tmp: default_seatbelt_allow_tmp(),
            allow_home_read: default_seatbelt_allow_home_read(),
        }
    }
}

/// Per-call-site knobs of the Linux bubblewrap wrapper. Each field maps to
/// one bwrap argv entry (or block thereof) so the generated argv is
/// auditable end-to-end (see
/// `crates/zoder-cli/src/exec_safety::generate_bubblewrap_argv`). Mirrors
/// `SeatbeltProfileOptions` 1:1 so an operator who has one profile block can
/// copy the shape across to the other backend without re-reading docs.
///
/// Default deny-network + bind-workdir semantics match the macOS seatbelt
/// profile's defaults — the two backends are intentionally symmetric so the
/// operator-visible "least-privilege" contract is the same regardless of
/// host OS.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LinuxBubblewrapProfileOptions {
    /// Isolate the network namespace (`--unshare-net`). Default `true` —
    /// most `--check` commands (`cargo test`, `pytest`) do not need network
    /// and a compromised test must not silently phone home. Operators who
    /// run network-dependent checks flip this to `false`; the argv then
    /// omits the `--unshare-net` flag.
    #[serde(default = "default_bwrap_unshare_net")]
    pub unshare_net: bool,
    /// Mount `/tmp` (and `/var/tmp` as a symlink-fallback) read-write
    /// inside the sandbox. Default `true` because `cargo`, `pytest`,
    /// `node`, and almost every common `--check` writes intermediates there.
    /// Operators on hardened hosts can flip this off; the argv then omits
    /// the tmp bind mounts and any tool that needs scratch space will fail
    /// loudly.
    #[serde(default = "default_bwrap_allow_tmp")]
    pub allow_tmp: bool,
    /// Read access to `/home` (read-only bind; no writes). Default `false`
    /// because most builds either inline everything they need under the
    /// working dir or fail loudly — granting read access to `/home`
    /// effectively exposes `~/.cargo`, `~/.npm`, etc. Operators who need
    /// those to be visible flip this to `true`; the argv then includes a
    /// `--ro-bind /home /home` entry. Writes to `/home` remain denied
    /// because the bind is read-only.
    #[serde(default = "default_bwrap_allow_home_read")]
    pub allow_home_read: bool,
}

fn default_bwrap_unshare_net() -> bool {
    true
}
fn default_bwrap_allow_tmp() -> bool {
    true
}
fn default_bwrap_allow_home_read() -> bool {
    false
}

impl Default for LinuxBubblewrapProfileOptions {
    fn default() -> Self {
        Self {
            unshare_net: default_bwrap_unshare_net(),
            allow_tmp: default_bwrap_allow_tmp(),
            allow_home_read: default_bwrap_allow_home_read(),
        }
    }
}

/// `[exec_safety]` block from `config.json` / an overlay TOML. Owns the
/// opt-in OS-sandbox backend selection and the per-backend profile knobs.
/// Absent block or absent `backend` = `ExecSandbox::None` = current behavior.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecSafetyConfig {
    /// Which OS-level sandbox backend to wrap `--check` execution in.
    /// Default (`None`) preserves the pre-existing "denylist only" behavior
    /// byte-for-byte. See [`ExecSandbox`] for the supported variants and
    /// their cross-platform contracts.
    #[serde(default)]
    pub backend: ExecSandbox,
    /// Per-backend profile knobs. Currently consulted when
    /// `backend == ExecSandbox::Seatbelt`; ignored otherwise so an operator
    /// who later flips the backend from `None` to `Seatbelt` doesn't have
    /// to also move their profile options.
    #[serde(default)]
    pub seatbelt: SeatbeltProfileOptions,
    /// Per-backend profile knobs for the Linux bubblewrap wrapper.
    /// Consulted when `backend == ExecSandbox::LinuxBubblewrap`; ignored
    /// otherwise so an operator who flips the backend from `Seatbelt` (on
    /// a Mac) to `LinuxBubblewrap` (on a Linux box) doesn't have to also
    /// move their profile options.
    #[serde(default)]
    pub linux_bubblewrap: LinuxBubblewrapProfileOptions,
}

impl Config {
    /// Config directory: $ZODER_HOME or ~/.zoder.
    pub fn home() -> PathBuf {
        if let Ok(h) = std::env::var("ZODER_HOME") {
            return PathBuf::from(h);
        }
        dirs::home_dir().unwrap_or_default().join(".zoder")
    }

    /// Load from $ZODER_HOME/config.json (if present, else sensible free-tier
    /// default) and then layer every `config.<vendor>.toml` in the same
    /// directory on top. See module docs for the layered-config model.
    pub fn load() -> anyhow::Result<Self> {
        let home = Self::home();
        let mut cfg = if home.join("config.json").exists() {
            let raw = std::fs::read_to_string(home.join("config.json"))?;
            serde_json::from_str(&raw)?
        } else {
            Self::default_provider(&home)
        };
        apply_overlays(&mut cfg, &home)?;
        // Fail loud on a misconfigured merge (duplicate ids, missing
        // default_provider, bad base_urls, …) rather than discovering it at
        // call time.
        let problems = cfg.validate();
        if !problems.is_empty() {
            anyhow::bail!(
                "invalid zoder configuration:\n  - {}",
                problems.join("\n  - ")
            );
        }
        Ok(cfg)
    }

    /// Like `load()`, but never reads `config.json` — starts from the default
    /// free-tier config and applies only the named vendor TOML. Used by
    /// `--vendor <name>` when the user wants a vendor-only view from a clean
    /// slate.
    pub fn load_vendor_only(vendor: &str) -> anyhow::Result<Self> {
        let home = Self::home();
        let mut cfg = Self::default_provider(&home);
        apply_overlays_filtered(&mut cfg, &home, Some(vendor))?;
        let problems = cfg.validate();
        if !problems.is_empty() {
            anyhow::bail!(
                "invalid zoder configuration:\n  - {}",
                problems.join("\n  - ")
            );
        }
        Ok(cfg)
    }

    /// Name of every vendor overlay currently present on disk (filenames of
    /// the form `config.<vendor>.toml` in `$ZODER_HOME`). Returned in the
    /// stable alphabetical order the loader uses. Used to build the
    /// `--vendor` completion list and to validate `--vendor X` arguments.
    pub fn available_vendors() -> Vec<String> {
        let home = Self::home();
        let Ok(rd) = std::fs::read_dir(&home) else {
            return Vec::new();
        };
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                // Filename must be exactly `config.<vendor>.toml`. Strip the
                // prefix and the suffix in two steps so a file like
                // `config.foo.toml.bak` doesn't sneak in.
                let rest = name.strip_prefix("config.")?;
                let stem = rest.strip_suffix(".toml")?;
                if stem.is_empty() || stem.contains('.') {
                    return None;
                }
                Some(stem.to_string())
            })
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Default config: free-tier as the single free provider.
    ///
    /// NOTE: the placeholder base_url ([`PLACEHOLDER_PROVIDER_HOST`]) is a
    /// deliberate sentinel — a host with no real routing config resolves every
    /// model to this, and [`real_provider_for_model`](Config::real_provider_for_model)
    /// treats that as "no provider configured" so callers hard-error instead of
    /// dialing a bogus endpoint.
    pub fn default_provider(home: &std::path::Path) -> Self {
        Config {
            providers: vec![Provider {
                id: "default".into(),
                base_url: format!("https://{PLACEHOLDER_PROVIDER_HOST}/v1"),
                kind: "openai-chat".into(),
                auth: Auth::Env {
                    var: "ZODER_API_KEY".into(),
                },
                paid: false,
                billing: BillingMode::Free,
                subscription: None,
                serves: Vec::new(),
                azure_api_version: None,
            }],
            default_provider: "default".into(),
            corpus_path: home.join("model_corpus.json"),
            ledger_path: home.join("ledger.jsonl"),
            health_path: home.join("health.json"),
            free_api_hosts: default_free_hosts(),
            strict_free: default_strict_free(),
            vendor_provenance: BTreeMap::new(),
            theme: Theme::default(),
            primary_model: None,
            reviewer_model: None,
            agents: BTreeMap::new(),
            budget: crate::budget::Budget::default(),
            routing: RoutingConfig::default(),
            exec_safety: ExecSafetyConfig::default(),
        }
    }

    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id == id)
    }

    /// Resolve which provider should serve a given model id, ranked by cost:
    /// `Free` > `Subscription` (with remaining window quota) > `Metered`.
    /// Within each billing tier, the provider with the LONGEST matching
    /// `serves` prefix wins (most specific claim, e.g. `nvidia/` vs
    /// `nvidia/llama-`); equal-length ties break by config order. A model
    /// no provider claims falls back to `default_provider`.
    ///
    /// Subscription providers are treated as cost-neutral ($0 marginal) only
    /// while they have remaining window quota. An exhausted-window
    /// subscription is demoted to the metered tier — it would error at the
    /// API side anyway, so we skip it in favor of a working alternative.
    /// The window rolls forward over time and the subscription becomes
    /// available again automatically; no operator intervention is required
    /// for recovery.
    ///
    /// This is the route the dual-billing vendor overlay relies on: a single
    /// vendor modeled as SEPARATE provider entries (one subscription, one
    /// metered — different auth and endpoint, both claiming the same model
    /// prefixes). The subscription wins while its window has headroom; the
    /// metered entry picks up the slack when the subscription is exhausted.
    ///
    /// Window-exhaustion detection needs the local ledger and the tier
    /// catalog. Pass them in via [`best_provider_for_model`]; this
    /// convenience wrapper passes empty entries and an empty catalog, which
    /// degenerates to "every subscription looks non-exhausted" — still
    /// preferred over metered, but unable to skip a saturated one. Callers
    /// that have a `Ledger` open (the CLI router loop, pre-call routing in
    /// `zoder exec`) should prefer [`best_provider_for_model`] so the
    /// metered fallback actually triggers.
    pub fn provider_for_model(&self, model_id: &str) -> Option<&Provider> {
        self.best_provider_for_model(
            model_id,
            &[],
            &crate::subscription_tiers::TierCatalog::empty(),
        )
    }

    /// Full quota-aware ranking for routing. Identical to
    /// [`provider_for_model`] but the subscription-vs-metered decision is
    /// driven by the ledger (`entries`) and the tier `catalog` so that a
    /// subscription whose rolling window is at/over cap is treated like a
    /// metered provider (i.e. demoted below live subscriptions). Pass the
    /// same `entries` / `catalog` pair the report uses so routing and the
    /// utilization report never disagree about whether a window is full.
    pub fn best_provider_for_model(
        &self,
        model_id: &str,
        entries: &[crate::ledger::Entry],
        catalog: &crate::subscription_tiers::TierCatalog,
    ) -> Option<&Provider> {
        let candidates = self.ranked_providers_for_model(model_id, entries, catalog);
        if candidates.is_empty() {
            // No provider claims the prefix -> fall through to
            // `default_provider`. This is the historical single-endpoint
            // behavior: the CLI expects a non-None answer for `-m
            // <unprefixed>`, and the report always shows the default as
            // the "unrouted" route. We deliberately do NOT re-rank
            // `default_provider` against its real billing tier here —
            // there is no competition (the list is empty by construction),
            // so any tier assignment would be cosmetic. Just return it.
            return self.provider(&self.default_provider);
        }
        candidates.into_iter().next().map(|c| c.provider)
    }

    /// Internal: return every provider that claims `model_id`, sorted best
    /// (cheapest tier, longest prefix, earliest config order) first. The
    /// router returns just the head; the wider list is exposed here for
    /// testing and for future "show me the fallback chain" affordances.
    fn ranked_providers_for_model(
        &self,
        model_id: &str,
        entries: &[crate::ledger::Entry],
        catalog: &crate::subscription_tiers::TierCatalog,
    ) -> Vec<RankedProvider<'_>> {
        let mut candidates: Vec<RankedProvider<'_>> = self
            .providers
            .iter()
            .filter_map(|p| {
                let best_prefix_len = p
                    .serves
                    .iter()
                    .filter(|prefix| !prefix.is_empty() && model_id.starts_with(prefix.as_str()))
                    .map(|prefix| prefix.len())
                    .max()?;
                Some(RankedProvider {
                    provider: p,
                    prefix_len: best_prefix_len,
                    billing_tier: billing_tier(p, entries, catalog),
                })
            })
            .collect();
        // Sort: smaller `billing_tier` wins (cheaper). Within a tier, longer
        // `serves` prefix wins (more specific). Within those ties, original
        // config order (stable sort on `Index`).
        candidates.sort_by(|a, b| {
            a.billing_tier
                .cmp(&b.billing_tier)
                .then_with(|| b.prefix_len.cmp(&a.prefix_len))
        });
        candidates
    }

    /// Like [`provider_for_model`], but returns `None` when the only match is
    /// the built-in placeholder `default` provider (base_url `api.example.com`,
    /// see [`Config::default_provider`]). A model that resolves ONLY to the
    /// placeholder has no real backing provider on this host — dialing it hits
    /// a bogus endpoint and fails cryptically. Callers use this to (a) keep the
    /// router from auto-picking unbacked free-pool models and (b) hard-error
    /// with a clear message instead of calling `api.example.com`.
    pub fn real_provider_for_model(&self, model_id: &str) -> Option<&Provider> {
        self.provider_for_model(model_id)
            .filter(|p| !p.base_url.contains(PLACEHOLDER_PROVIDER_HOST))
    }

    /// Quota-aware variant of [`real_provider_for_model`]. When multiple
    /// providers claim the model's prefix, the smart router prefers a
    /// subscription with remaining quota over its metered sibling; an
    /// exhausted-window subscription falls through to metered. Pass the
    /// same ledger entries and tier catalog the report uses.
    pub fn real_best_provider_for_model(
        &self,
        model_id: &str,
        entries: &[crate::ledger::Entry],
        catalog: &crate::subscription_tiers::TierCatalog,
    ) -> Option<&Provider> {
        self.best_provider_for_model(model_id, entries, catalog)
            .filter(|p| !p.base_url.contains(PLACEHOLDER_PROVIDER_HOST))
    }

    /// `true` if a real (configured, non-placeholder) provider serves `model_id`.
    pub fn model_has_real_provider(&self, model_id: &str) -> bool {
        self.real_provider_for_model(model_id).is_some()
    }

    /// Resolve the per-agent PRIMARY model id for `--agent <alias>`. Returns
    /// `None` when no per-agent override is configured (no such alias, or
    /// the alias has no `model` pin). The CLI precedence chains this with
    /// `-m` (which wins first) and `primary_model` (which falls through
    /// last); see [`crate::resolve_effective_primary`] in the CLI for the
    /// canonical application of the order.
    ///
    /// Resolution precedence (highest first), as wired by the CLI side:
    ///   1. explicit `-m <model>` (per-invocation) — caller short-circuits
    ///      BEFORE this lookup,
    ///   2. `[agents.<alias>].model` (this fn — when `alias` is `Some` and
    ///      present in the map),
    ///   3. `Config::primary_model` (the global default).
    ///
    /// This fn only returns the per-agent pin (step 2); the caller chains
    /// it against `primary_model`. Returning owned `String` (rather than
    /// `&str`) keeps the call site `let m: Option<String>`-friendly without
    /// a clone on the success path.
    pub fn agent_model(&self, alias: Option<&str>) -> Option<String> {
        let alias = alias?;
        self.agents.get(alias).and_then(|a| a.model.clone())
    }

    /// Resolve the per-agent REVIEWER / secondary model id for
    /// `--agent <alias>`. Returns `None` when no per-agent reviewer override
    /// is configured. Independent of `primary_model`: an agent may pin a
    /// different reviewer from its own author model.
    pub fn agent_reviewer_model(&self, alias: Option<&str>) -> Option<String> {
        let alias = alias?;
        self.agents
            .get(alias)
            .and_then(|a| a.reviewer_model.clone())
    }

    /// Resolve the profile-level `reviewer_model` setting as an ORDERED list of
    /// reviewer candidates (head first). The legacy single-string form is
    /// preserved: a `Config::reviewer_model` containing a single model id is
    /// returned as a one-element vector — the same shape the field has always
    /// produced when consulted as a chain. When the operator writes a
    /// comma-separated string (e.g. `"model_a,model_b,model_c"`) it is split
    /// on `,` so a single stuck model does not sink the whole review (the
    /// reviewer pipeline falls through to the next candidate instead of bailing
    /// out at the head).
    ///
    /// Whitespace around each entry is trimmed and empty entries are dropped
    /// — a trailing comma is treated like a one-element list, not as
    /// `["model_a", ""]`. Callers should treat this as the reviewer chain's
    /// profile-level contribution; the per-agent pin
    /// (`agent_reviewer_model`) and the scenario-routed reviewer chain
    /// (`ResolvedRoutes::reviewer`) compose on top of the head this returns.
    pub fn reviewer_models(&self) -> Vec<String> {
        parse_reviewer_chain(self.reviewer_model.as_deref())
    }

    /// Same as [`Self::reviewer_models`] but takes an explicit alias so the
    /// per-agent `[agents.<alias>].reviewer_model` pin is honored first,
    /// falling through to the profile-level chain. Returning `Vec<String>`
    /// mirrors the reviewer chain shape consumed by the reviewer dispatch
    /// loop in `complete_once` and keeps the call sites symmetric.
    pub fn reviewer_models_for(&self, alias: Option<&str>) -> Vec<String> {
        if let Some(pin) = self.agent_reviewer_model(alias) {
            parse_reviewer_chain(Some(&pin))
        } else {
            self.reviewer_models()
        }
    }

    /// Provider ids contributed by a given vendor overlay. Returns an empty
    /// vec for unknown vendors and for the synthetic "base" (providers from
    /// `config.json` / default config). Used by `--vendor <name>` filtering.
    pub fn vendor_providers(&self, vendor: &str) -> &[String] {
        self.vendor_provenance
            .get(vendor)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// `true` if this provider id was contributed by any vendor overlay
    /// (vs. coming from `config.json` / defaults). Useful for the report
    /// header when a vendor filter is active.
    pub fn vendor_of(&self, provider_id: &str) -> Option<&str> {
        self.vendor_provenance
            .iter()
            .find(|(_, ids)| ids.iter().any(|i| i == provider_id))
            .map(|(v, _)| v.as_str())
    }

    /// All vendor names that currently contribute providers (i.e. have at
    /// least one entry in `vendor_provenance`). Includes "base" if any
    /// providers came from `config.json` / defaults.
    pub fn active_vendors(&self) -> Vec<String> {
        self.vendor_provenance.keys().cloned().collect()
    }

    /// Directory holding multi-turn session transcripts.
    pub fn sessions_dir(&self) -> PathBuf {
        Self::home().join("sessions")
    }

    /// Validate the config for internal consistency. Returns a list of
    /// human-readable problems; empty means valid.
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        let tier_catalog = crate::subscription_tiers::load_tier_catalog(Some(
            &crate::subscription_tiers::default_catalog_path(&Self::home()),
        ));
        if self.providers.is_empty() {
            errs.push("no providers configured".into());
        }
        let mut seen = std::collections::HashSet::new();
        for p in &self.providers {
            if p.id.trim().is_empty() {
                errs.push("a provider has an empty id".into());
            } else if !seen.insert(p.id.clone()) {
                errs.push(format!("duplicate provider id: {}", p.id));
            }
            if let Err(e) = url::Url::parse(&p.base_url) {
                errs.push(format!(
                    "provider {}: invalid base_url {:?}: {e}",
                    p.id, p.base_url
                ));
            } else if !p.base_url.starts_with("http://") && !p.base_url.starts_with("https://") {
                errs.push(format!("provider {}: base_url must be http(s)", p.id));
            }
            // An empty/whitespace `serves` prefix would match EVERY model id and
            // silently capture the whole routing pool onto one provider — refuse
            // it. Prefixes should be delimiter-bounded (e.g. `nvidia/`,
            // `meta/llama-`, `MiniMax-`) to avoid surprises like `meta` also
            // matching `metamath/...`; that is advisory, but emptiness is fatal.
            for prefix in &p.serves {
                if prefix.trim().is_empty() {
                    errs.push(format!(
                        "provider {}: `serves` contains an empty prefix (would match every model)",
                        p.id
                    ));
                }
            }
            if p.kind.trim().is_empty() {
                errs.push(format!("provider {}: kind must not be empty", p.id));
            }
            if p.billing != BillingMode::Subscription && p.subscription.is_some() {
                errs.push(format!(
                    "provider {}: subscription terms require billing=subscription",
                    p.id
                ));
            }
            // A subscription WITHOUT explicit terms is valid, not an error: a
            // flat-fee subscription has $0 marginal cost, and the runtime treats
            // an unspecified tier as uncapped `SubscriptionLive` (see effective
            // tier logic). Rejecting it broke valid providers with a working key
            // (e.g. MiniMax). Terms remain optional and only add rate-limit
            // windows when present.
            if let Some(plan) = &p.subscription {
                if !plan.monthly_fee_usd.is_finite() || plan.monthly_fee_usd < 0.0 {
                    errs.push(format!(
                        "provider {}: monthly_fee_usd must be finite and non-negative",
                        p.id
                    ));
                }
                if plan.tier.as_ref().is_some_and(|t| t.trim().is_empty()) {
                    errs.push(format!(
                        "provider {}: subscription tier must not be empty",
                        p.id
                    ));
                }
                if let Some(tier) = plan.tier.as_deref().filter(|tier| !tier.trim().is_empty()) {
                    if tier_catalog.provider_namespace(p, tier).is_none() {
                        errs.push(format!(
                            "provider {}: subscription tier {:?} does not resolve in the tier catalog for this provider",
                            p.id, tier
                        ));
                    }
                }
                let mut window_names = std::collections::HashSet::new();
                for w in &plan.windows {
                    if w.name.trim().is_empty() {
                        errs.push(format!("provider {}: quota window has an empty name", p.id));
                    } else if !window_names.insert(w.name.as_str()) {
                        errs.push(format!(
                            "provider {}: duplicate quota window name {:?}",
                            p.id, w.name
                        ));
                    }
                    if w.hours == 0 {
                        errs.push(format!(
                            "provider {} window {}: hours must be greater than zero",
                            p.id, w.name
                        ));
                    }
                    if w.cap.is_some_and(|c| !c.is_finite() || c <= 0.0) {
                        errs.push(format!(
                            "provider {} window {}: cap must be finite and positive",
                            p.id, w.name
                        ));
                    }
                    if w.models.as_ref().is_some_and(|models| {
                        models.is_empty() || models.iter().any(|m| m.trim().is_empty())
                    }) {
                        errs.push(format!(
                            "provider {} window {}: models must contain non-empty patterns",
                            p.id, w.name
                        ));
                    }
                }
            }
        }
        // Reject duplicate `(provider, effective_account_id, tier)` triples
        // across subscription providers (KNEMON adversarial-review finding
        // #3). The existing duplicate-`Provider.id` check above already
        // prevents the strong case of two entries with the same routing
        // id; this check is the per-account identity check — an operator
        // who genuinely wants two providers serving the same logical
        // subscription is forced to declare distinct `account_id`s so the
        // capture/routing layers (added in a follow-up) can disambiguate
        // them. Mirrors the `duplicate provider id: …` idiom above.
        //
        // We key on `(Provider.id, effective_account_id, tier)` rather
        // than `(Provider.id, effective_account_id)` so that the same
        // account on TWO different tiers (e.g. personal/chatgpt-pro and
        // personal/chatgpt-pro-team) is permitted; the plan (`tier`) is
        // part of the identity. A `tier = None` plan is keyed under the
        // empty string so two termless subscriptions on the same
        // `(provider, account)` also collide — they would otherwise
        // collapse to the same routing+account identity with no
        // disambiguator at all.
        let mut seen_triples: std::collections::HashSet<(String, String, String)> =
            std::collections::HashSet::new();
        for p in &self.providers {
            let Some(plan) = p.subscription.as_ref() else {
                continue;
            };
            let key = (
                p.id.clone(),
                plan.effective_account_id(),
                plan.tier.clone().unwrap_or_default(),
            );
            if !seen_triples.insert(key.clone()) {
                errs.push(format!(
                    "duplicate subscription identity (provider={}, account_id={}, tier={:?}): two providers share the same (provider, effective_account_id, tier) triple; set a distinct account_id on one of them",
                    key.0, key.1, key.2,
                ));
            }
        }
        if self.provider(&self.default_provider).is_none() {
            errs.push(format!(
                "default_provider {:?} is not among configured providers",
                self.default_provider
            ));
        }
        if self.free_api_hosts.is_empty() && self.strict_free {
            errs.push(
                "strict_free is on but free_api_hosts is empty (every call would violate)".into(),
            );
        }
        for (name, cap) in [
            ("max_cost_per_call_usd", self.budget.max_cost_per_call_usd),
            ("monthly_cap_usd", self.budget.monthly_cap_usd),
        ] {
            if cap.is_some_and(|v| !v.is_finite() || v < 0.0) {
                errs.push(format!("budget.{name} must be finite and non-negative"));
            }
        }

        let presets = crate::scenarios::default_scenarios();
        if !presets.contains_key(&self.routing.scenario) {
            errs.push(format!(
                "unknown routing scenario {:?} (expected economy, balanced, aggressive, or unlimited)",
                self.routing.scenario
            ));
        }
        for name in self.routing.scenarios.keys() {
            if !presets.contains_key(name) {
                errs.push(format!("routing override names unknown scenario {name:?}"));
            }
        }
        for name in presets.keys() {
            let scenario = crate::scenarios::resolve_active(name, self.routing.scenarios.get(name));
            if !scenario.use_target.is_finite()
                || !scenario.cap_guard.is_finite()
                || !(0.0..=100.0).contains(&scenario.use_target)
                || !(0.0..=100.0).contains(&scenario.cap_guard)
                || scenario.use_target > scenario.cap_guard
            {
                errs.push(format!(
                    "routing scenario {name}: use_target/cap_guard must be finite percentages with 0 <= use_target <= cap_guard <= 100"
                ));
            }
            for (role, classes) in [
                ("primary_classes", &scenario.primary_classes),
                ("reviewer_classes", &scenario.reviewer_classes),
            ] {
                let unique: std::collections::HashSet<_> = classes.iter().collect();
                if classes.is_empty() || unique.len() != classes.len() {
                    errs.push(format!(
                        "routing scenario {name}: {role} must be non-empty and contain no duplicates"
                    ));
                }
            }
        }
        errs
    }
}

// ---------------------------------------------------------------------------
// Layered vendor overlays (config.<vendor>.toml)
// ---------------------------------------------------------------------------

/// A vendor overlay TOML contributes providers and (optionally) a default
/// provider. The TOML never sets the on-disk paths or the free-tier policy —
/// those come from the base `config.json` / default config — so a vendor
/// profile is purely additive: it adds routes, it doesn't change semantics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VendorOverlay {
    /// Optional profile metadata. `name` is informational (the loader already
    /// knows it from the filename). `default = true` selects this overlay's
    /// `default_provider` as the new active default. Multiple overlays with
    /// `default = true` is a hard load error.
    #[serde(default)]
    pub profile: VendorProfile,
    /// Providers contributed by this overlay. Each becomes a routable
    /// `Provider` in the merged `Config.providers`.
    #[serde(default)]
    pub providers: Vec<Provider>,
    /// Optional report colour palette for this org. When this overlay is the
    /// active/default one, its theme colours every report. Omitted fields fall
    /// back to the built-in default palette.
    #[serde(default)]
    pub theme: Option<Theme>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VendorProfile {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub default: bool,
    /// Provider id to use as `default_provider` when `default = true`. If
    /// omitted, the first `[[providers]]` id is used.
    #[serde(default)]
    pub default_provider: Option<String>,
    /// Pinned routing primary: a model id the router tries first, ahead of the
    /// capability/health-ranked pool. Independent of `default` — an overlay can
    /// pin the primary model without owning the default provider (e.g. the
    /// MiniMax overlay pins `MiniMax-M3` while the NVIDIA overlay stays the
    /// default profile). If several overlays set it, the default-claiming one
    /// wins, otherwise the alphabetically-last overlay that defines one.
    #[serde(default)]
    pub primary_model: Option<String>,
    /// Pinned reviewer / secondary model id. Same precedence as
    /// `primary_model` (default-claimer wins, else alphabetical-last).
    /// Applied to `Config::reviewer_model` after overlay merge. Independent
    /// of `primary_model` so an overlay can pin a strong cross-family
    /// reviewer without owning the author default.
    #[serde(default)]
    pub reviewer_model: Option<String>,
}

/// Apply every `config.<vendor>.toml` in alphabetical order. Tracks the set of
/// provider ids contributed by each vendor so `--vendor <name>` can filter
/// the report. On any duplicate-id collision or ambiguous `default = true`,
/// returns an error.
fn apply_overlays(cfg: &mut Config, home: &Path) -> anyhow::Result<()> {
    apply_overlays_filtered(cfg, home, None)
}

fn apply_overlays_filtered(
    cfg: &mut Config,
    home: &Path,
    only_vendor: Option<&str>,
) -> anyhow::Result<()> {
    let overlays = collect_overlays(home, only_vendor)?;
    if overlays.is_empty() {
        return Ok(());
    }

    // Track which provider ids came from which vendor so `Config::vendors()`
    // (and `--vendor <name>` filtering) can answer "is this provider from
    // enterprise's TOML?". Providers from `config.json` / defaults are tagged
    // `vendor = "base"` so they're never matched by `--vendor enterprise`.
    let mut vendors: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut seen_ids: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    // Seed with the base providers (from config.json / the default config) so an
    // overlay can't silently reuse/clobber a base provider id (e.g. `default`) —
    // that would misattribute base traffic and make `Config::provider()` return
    // the wrong provider for that id.
    for p in &cfg.providers {
        seen_ids.insert(p.id.clone(), "base".to_string());
    }

    let mut defaults_count = 0usize;
    // Theme resolution: the default-claiming overlay's theme wins; otherwise
    // the last overlay (alphabetical) that defines one. `None` keeps the
    // built-in default already on `cfg.theme`.
    let mut default_theme: Option<Theme> = None;
    let mut fallback_theme: Option<Theme> = None;
    // Pinned primary resolution mirrors theme: the default-claiming overlay's
    // primary_model wins, else the last (alphabetical) overlay that sets one.
    let mut default_primary: Option<String> = None;
    let mut fallback_primary: Option<String> = None;
    // Pinned reviewer resolution mirrors the primary shape (default-claimer
    // wins, else alphabetical-last). reviewer_model is INDEPENDENT of
    // primary_model — an overlay can pin a strong cross-family reviewer
    // without touching the author default.
    let mut default_reviewer: Option<String> = None;
    let mut fallback_reviewer: Option<String> = None;

    for (vendor, overlay) in overlays {
        for p in &overlay.providers {
            if let Some(prev) = seen_ids.get(&p.id) {
                anyhow::bail!(
                    "duplicate provider id {:?}: contributed by {} and {}; rename one of them in the TOML",
                    p.id,
                    prev,
                    vendor
                );
            }
            seen_ids.insert(p.id.clone(), vendor.clone());
            vendors
                .entry(vendor.clone())
                .or_default()
                .push(p.id.clone());
            cfg.providers.push(p.clone());
        }
        if overlay.theme.is_some() {
            fallback_theme = overlay.theme.clone();
        }
        if overlay.profile.primary_model.is_some() {
            fallback_primary = overlay.profile.primary_model.clone();
        }
        if overlay.profile.reviewer_model.is_some() {
            fallback_reviewer = overlay.profile.reviewer_model.clone();
        }
        if overlay.profile.default {
            defaults_count += 1;
            if overlay.theme.is_some() {
                default_theme = overlay.theme.clone();
            }
            if overlay.profile.primary_model.is_some() {
                default_primary = overlay.profile.primary_model.clone();
            }
            if overlay.profile.reviewer_model.is_some() {
                default_reviewer = overlay.profile.reviewer_model.clone();
            }
            let new_default = overlay
                .profile
                .default_provider
                .clone()
                .or_else(|| overlay.providers.first().map(|p| p.id.clone()));
            if let Some(d) = new_default {
                if cfg.provider(&d).is_none() {
                    anyhow::bail!(
                        "overlay {} sets default_provider {:?} but no provider with that id is contributed (either add a [[providers]] entry with id {:?} or omit [profile].default_provider)",
                        vendor,
                        d,
                        d
                    );
                }
                cfg.default_provider = d;
            }
        }
    }

    if defaults_count > 1 {
        anyhow::bail!(
            "{} vendor overlays set [profile].default = true; only one overlay may do so",
            defaults_count
        );
    }

    // Record vendor provenance on the merged config for `--vendor` filtering.
    cfg.vendor_provenance = vendors;
    // Apply the resolved org theme (default-claimer wins, else last defined).
    if let Some(theme) = default_theme.or(fallback_theme) {
        cfg.theme = theme;
    }
    // Apply the resolved pinned primary (default-claimer wins, else last set).
    if let Some(primary) = default_primary.or(fallback_primary) {
        cfg.primary_model = Some(primary);
    }
    // Apply the resolved pinned reviewer (default-claimer wins, else last
    // set). Same precedence shape as primary, but the field is independent
    // so a config can pin a cross-family reviewer without touching the
    // author default.
    if let Some(reviewer) = default_reviewer.or(fallback_reviewer) {
        cfg.reviewer_model = Some(reviewer);
    }
    Ok(())
}

fn collect_overlays(
    home: &Path,
    only_vendor: Option<&str>,
) -> anyhow::Result<Vec<(String, VendorOverlay)>> {
    let Ok(rd) = std::fs::read_dir(home) else {
        return Ok(Vec::new());
    };
    let mut entries: Vec<(String, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            // Filename must be exactly `config.<vendor>.toml`. Reject
            // `config.toml` (no vendor stem), `config.foo.toml.bak`
            // (wrong suffix), and `config.foo.bar.toml` (vendor stem
            // contains a dot — that's a sub-overlay, not a top-level
            // vendor).
            let rest = name.strip_prefix("config.")?;
            let stem = rest.strip_suffix(".toml")?;
            if stem.is_empty() || stem.contains('.') {
                return None;
            }
            if let Some(want) = only_vendor {
                if stem != want {
                    return None;
                }
            }
            Some((stem.to_string(), e.path()))
        })
        .collect();
    // Deterministic: alphabetical by vendor stem. `config.ibm.toml` overrides
    // nothing in `config.enterprise.toml` (we forbid duplicates instead), but the
    // load order is at least stable for any cross-overlay `[profile].default`
    // tiebreak.
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = Vec::with_capacity(entries.len());
    for (vendor, path) in entries {
        let raw = std::fs::read_to_string(&path)?;
        let overlay: VendorOverlay =
            toml::from_str(&raw).map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
        if overlay.providers.is_empty() && !overlay.profile.default {
            anyhow::bail!(
                "{} contributes no [[providers]] and no [profile].default; either add providers or remove the file",
                path.display()
            );
        }
        out.push((vendor, overlay));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Entry;
    use crate::subscription_tiers::TierCatalog;
    use chrono::{Duration, Utc};

    #[test]
    fn provider_for_model_routes_by_serves_prefix_else_default() {
        let mut cfg = Config::default_provider(std::path::Path::new("/tmp/zoder-test"));
        cfg.providers.push(Provider {
            id: "minimax".into(),
            base_url: "https://api.minimax.io/v1".into(),
            kind: "openai-chat".into(),
            auth: Auth::None,
            paid: false,
            billing: BillingMode::Free,
            subscription: None,
            serves: vec!["MiniMax-".into()],
            azure_api_version: None,
        });
        cfg.providers.push(Provider {
            id: "nvidia-eih".into(),
            base_url: "https://integrate.api.nvidia.com/v1".into(),
            kind: "openai-chat".into(),
            auth: Auth::None,
            paid: false,
            billing: BillingMode::Free,
            subscription: None,
            serves: vec![
                "nvidia/".into(),
                "deepseek-ai/".into(),
                "meta/llama-".into(),
                "mistralai/".into(),
            ],
            azure_api_version: None,
        });
        // Prefix match wins, in config order.
        assert_eq!(cfg.provider_for_model("MiniMax-M3").unwrap().id, "minimax");
        assert_eq!(
            cfg.provider_for_model("nvidia/llama-3.3-nemotron-super-49b-v1.5")
                .unwrap()
                .id,
            "nvidia-eih"
        );
        assert_eq!(
            cfg.provider_for_model("deepseek-ai/deepseek-r1")
                .unwrap()
                .id,
            "nvidia-eih"
        );
        // No prefix claims it -> falls back to default_provider.
        assert_eq!(
            cfg.provider_for_model("azure/gpt-4o").unwrap().id,
            cfg.default_provider
        );
    }

    /// Build a minimal config with two providers that both serve the same
    /// `MiniMax-` prefix — one as a flat-fee subscription, one as
    /// pay-as-you-go metered. This is the vendor-dual-billing shape:
    /// `serves` is identical, `auth` and `base_url` differ (subscription
    /// rides the vendor's admin-key path, metered goes through the public
    /// API), and the smart router must pick the subscription while its
    /// window has headroom. Tests below vary the ledger to exercise the
    /// three phase-2 invariants.
    fn dual_billing_fixture() -> (Config, TierCatalog) {
        let mut cfg = Config::default_provider(std::path::Path::new("/tmp/zoder-test"));
        cfg.providers.push(Provider {
            id: "minimax-sub".into(),
            base_url: "https://api.minimax.io/admin/v1".into(),
            kind: "openai-chat".into(),
            auth: Auth::None,
            paid: false,
            billing: BillingMode::Subscription,
            subscription: Some(SubscriptionPlan {
                monthly_fee_usd: 20.0,
                // Explicit window: 5-hour rolling cap of 900 messages.
                // Tests saturate the ledger with >= 900 messages in the
                // last 5 hours to flip it to "exhausted".
                windows: vec![QuotaWindow {
                    name: "5h".into(),
                    hours: 5,
                    unit: QuotaUnit::Messages,
                    cap: Some(900.0),
                    models: None,
                    observability: Observability::default(),
                    reset: ResetKind::default(),
                }],
                tier: None,
                ..Default::default()
            }),
            serves: vec!["MiniMax-".into()],
            azure_api_version: None,
        });
        cfg.providers.push(Provider {
            id: "minimax-met".into(),
            base_url: "https://api.minimax.io/v1".into(),
            kind: "openai-chat".into(),
            auth: Auth::None,
            // paid=false keeps `--require-free` strict-mode honest for the
            // metered path only insofar as the *model* (not the billing
            // mode) decides it; the smart router does NOT short-circuit on
            // `paid` here — billing-tier ranking owns the decision, as
            // documented.
            paid: false,
            billing: BillingMode::Metered,
            subscription: None,
            serves: vec!["MiniMax-".into()],
            azure_api_version: None,
        });
        // Empty catalog: explicit `windows` on the subscription resolve
        // directly, no preset lookup needed. (Passing an empty catalog is
        // equivalent here; checked explicitly in case `plan_usage` is
        // called.)
        (cfg, TierCatalog::empty())
    }

    /// Synthesize `n` ledger entries on `provider_id` whose `ts_utc` is
    /// `back_min + (i as i64 % spread)` minutes behind `now`. Spreading
    /// entries across `[back_min, back_min + spread)` lets tests pin
    /// whether they fall INSIDE or OUTSIDE a given rolling window —
    ///   - "in-window" use a back_min inside the lookback and a tight
    ///     spread, so every entry counts toward `used`.
    ///   - "out-of-window" use a back_min past the lookback so every
    ///     entry ages out and `used` is 0.
    ///
    /// Each entry counts as one message (the `QuotaUnit::Messages` unit
    /// used in the fixture).
    fn entries_n(provider_id: &str, n: usize, back_min: i64, spread: i64) -> Vec<Entry> {
        let now = Utc::now();
        (0..n)
            .map(|i| Entry {
                ts_utc: now - Duration::minutes(back_min + i as i64 % spread),
                provider: provider_id.into(),
                model: "MiniMax-M3".into(),
                host: String::new(),
                tokens_in: 0,
                tokens_out: 0,
                cost_usd: 0.0,
                cost_unknown: false,
                calls: 1,
                violation: None,
                tags: crate::ledger::FinOpsTags::default(),
            })
            .collect()
    }

    #[test]
    fn best_provider_prefers_subscription_with_remaining_quota_over_metered() {
        // Zero usage on the subscription: the 5h window is at 0/900. The
        // smart router must pick the subscription (tier 1) over the
        // metered sibling (tier 2). Without this, dual-billing would
        // always burn the metered path and the subscription would be
        // dead weight on disk.
        let (cfg, cat) = dual_billing_fixture();
        let entries = entries_n("minimax-sub", 0, 0, 1);
        let picked = cfg
            .best_provider_for_model("MiniMax-M3", &entries, &cat)
            .expect("dual-billing fixture must resolve");
        assert_eq!(
            picked.id, "minimax-sub",
            "subscription with remaining quota must beat metered"
        );

        // Sanity: the convenience `provider_for_model` (no ledger context)
        // also ranks subscription above metered — its degenerated
        // "every subscription looks non-exhausted" assumption is exactly
        // the success-path behavior.
        assert_eq!(
            cfg.provider_for_model("MiniMax-M3").unwrap().id,
            "minimax-sub",
            "the no-ledger routing path must also prefer the subscription"
        );
    }

    #[test]
    fn best_provider_falls_through_to_metered_when_subscription_window_exhausted() {
        let (cfg, cat) = dual_billing_fixture();
        // 900 messages spread across the last 5h (5h = 300 min) ==
        // window at cap (cap = 900.0). The subscription is "exhausted";
        // the API would error anyway, so the router must transparently
        // fall through to the metered sibling. Both providers claim the
        // same prefix, so this is a pure billing-tier decision.
        let entries = entries_n("minimax-sub", 900, 1, 290);
        let picked = cfg
            .best_provider_for_model("MiniMax-M3", &entries, &cat)
            .expect("even with both providers claimed, one must resolve");
        assert_eq!(
            picked.id, "minimax-met",
            "exhausted-window subscription must fall through to metered sibling"
        );

        // Regression guard for the no-ledger path: with no entries it
        // CAN'T know the window is gone, so it picks the subscription
        // (correctly: that IS its degenerate assumption). Document the
        // asymmetry in the test so a future reader understands why the
        // two views disagree — and why callers that care MUST pass
        // entries.
        let picked_no_ledger = cfg.provider_for_model("MiniMax-M3").unwrap();
        assert_eq!(
            picked_no_ledger.id, "minimax-sub",
            "without ledger context the smart router has no signal that the \
             subscription is saturated and conservatively picks it; this is \
             why `best_provider_for_model` exists"
        );
    }

    #[test]
    fn best_provider_recovers_subscription_once_window_resets() {
        // Same fixture, but the 900-saturating entries are OUTSIDE the
        // rolling 5h window — they're 5h20m to 6h20m old, so the 5h
        // window's measured `used` is 0/900. The router must treat the
        // subscription as live again. This is the "automatic recovery"
        // half of the spec: no operator intervention, the window rolls
        // forward on its own.
        let (cfg, cat) = dual_billing_fixture();
        // 900 entries spread across `[320, 380)` minutes back — every
        // one of them is older than the 5h (300 min) lookback, so the
        // 5h window measures `used == 0`.
        let entries = entries_n("minimax-sub", 900, 320, 60);
        let picked = cfg
            .best_provider_for_model("MiniMax-M3", &entries, &cat)
            .expect("both providers claim the prefix; one must resolve");
        assert_eq!(
            picked.id, "minimax-sub",
            "the rolling 5h window must have aged the saturating entries \
             out; the subscription is live again and must be preferred"
        );

        // And the saturating entries, when placed INSIDE the window,
        // still trigger the metered fall-through (sanity — recovery is
        // the contrast, not a substitute for the saturation case).
        let in_window = entries_n("minimax-sub", 900, 1, 290);
        let picked_saturated = cfg
            .best_provider_for_model("MiniMax-M3", &in_window, &cat)
            .unwrap();
        assert_eq!(
            picked_saturated.id, "minimax-met",
            "control: in-window saturation still falls through to metered"
        );
    }

    #[test]
    fn bearer_auth_renders_authorization_header() {
        let (name, value) = Auth::Bearer {
            token: "sk-test".into(),
        }
        .header_pair()
        .expect("bearer yields a header");
        assert_eq!(name, "authorization");
        assert_eq!(value, "Bearer sk-test");
    }

    #[test]
    fn api_key_header_uses_custom_header_name_and_env_value() {
        // Enterprise gateway shape (Azure OpenAI / OCI gateway): a custom
        // header carries the raw secret, not `Authorization: Bearer`.
        let var = "ZODER_TEST_APIKEY_HEADER_VALUE";
        std::env::set_var(var, "secret-azure-value");
        let (name, value) = Auth::ApiKeyHeader {
            header: "api-key".into(),
            var: var.into(),
        }
        .header_pair()
        .expect("api_key_header yields a header when the env var is set");
        assert_eq!(name, "api-key");
        assert_eq!(value, "secret-azure-value");
        std::env::remove_var(var);
    }

    #[test]
    fn missing_or_none_credential_yields_no_header() {
        assert!(Auth::None.header_pair().is_none());
        assert!(
            Auth::ApiKeyHeader {
                header: "api-key".into(),
                var: "ZODER_TEST_DEFINITELY_UNSET_VAR".into(),
            }
            .header_pair()
            .is_none(),
            "an unset env var must yield no header (fail closed, not a blank credential)"
        );
    }

    #[test]
    fn org_overlay_theme_becomes_active_theme() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.acme.toml"),
            r#"
[profile]
name = "acme"
default = true
default_provider = "acme-gw"

[[providers]]
id = "acme-gw"
base_url = "https://gw.acme.example/v1"
kind = "openai-chat"
auth = { type = "api_key_header", header = "api-key", var = "ACME_KEY" }
paid = true
billing = "metered"

[theme]
accent = "38;2;10;20;30"
header = "1;38;2;10;20;30"
"#,
        )
        .unwrap();
        let mut cfg = Config::default_provider(dir.path());
        apply_overlays(&mut cfg, dir.path()).unwrap();
        // The org overlay's theme colours win.
        assert_eq!(cfg.theme.accent, "38;2;10;20;30");
        assert_eq!(cfg.theme.header, "1;38;2;10;20;30");
        // Fields the overlay omitted fall back to the built-in default.
        assert_eq!(cfg.theme.dim, Theme::default().dim);
        assert_eq!(cfg.theme.warn, Theme::default().warn);
        // And the default-claiming overlay also set the active default provider.
        assert_eq!(cfg.default_provider, "acme-gw");
    }

    #[test]
    fn overlay_reusing_a_base_provider_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        // The base config (Config::default_provider) contributes id "default";
        // an overlay must not be able to silently reuse/clobber it.
        std::fs::write(
            dir.path().join("config.acme.toml"),
            r#"
[[providers]]
id = "default"
base_url = "https://gw.acme.example/v1"
kind = "openai-chat"
auth = { type = "env", var = "ACME_KEY" }
"#,
        )
        .unwrap();
        let mut cfg = Config::default_provider(dir.path());
        let err = apply_overlays(&mut cfg, dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("duplicate provider id"),
            "overlay reusing base id 'default' must be rejected: {err}"
        );
    }

    // ---------- KNEMON declarative QuotaWindow round-trips ----------
    //
    // The whole point of the `models` / `observability` / `reset` /
    // `Option<cap>` extension is that ANY subscription plan an operator
    // dreams up is expressible in config and reachable from a JSON parse
    // with no code change. These three tests pin the contract.
    //
    // 1. The maximal shape (every new field present, cap null) round-trips
    //    exactly: any field a real provider needs is reachable.
    // 2. The minimal shape (`{"name": ..., "hours": ...}` only) deserializes
    //    with the documented defaults: `unit = Tokens`, `observability =
    //    Header`, `reset = Rolling`, `cap = None`, `models = None`.
    // 3. `QuotaUnit::Sessions` survives a round trip — the third common
    //    "flat-fee plan" unit, alongside tokens/requests/messages, exists
    //    so Cursor/Windsurf-style caps can be declared in config.

    #[test]
    fn quota_window_maximal_round_trip_with_null_cap() {
        // Anthropic per-model cap: 5h of 200M tokens on opus-* models,
        // observed via response headers (`observability: "header"`),
        // rolling reset. The `cap` is intentionally `null` — the operator
        // only knows it's not the headline 900-message cap; the real
        // token cap is published as a percent later. This shape MUST
        // deserialize cleanly and round-trip back to the same JSON so a
        // TOML/JSON config the operator types by hand keeps every field.
        let json = r#"{
            "name": "5h-opus",
            "hours": 5,
            "unit": "tokens",
            "cap": null,
            "models": ["claude-opus-*", "claude-3-opus-*"],
            "observability": "header",
            "reset": "rolling"
        }"#;
        let w: QuotaWindow = serde_json::from_str(json).unwrap();
        assert_eq!(w.name, "5h-opus");
        assert_eq!(w.hours, 5);
        assert_eq!(w.unit, QuotaUnit::Tokens);
        assert_eq!(w.cap, None, "cap = null in JSON must deserialize to None");
        assert_eq!(
            w.models.as_deref(),
            Some(&["claude-opus-*".to_string(), "claude-3-opus-*".to_string()][..])
        );
        assert_eq!(w.observability, Observability::Header);
        assert_eq!(w.reset, ResetKind::Rolling);

        // Round-trip back to JSON and confirm both `cap` and `models`
        // skip cleanly (per the `skip_serializing_if = "Option::is_none"`
        // policy on those fields — `None` = "all models" / "unknown
        // cap", not "explicit zero").
        let out = serde_json::to_string(&w).unwrap();
        let re: QuotaWindow = serde_json::from_str(&out).unwrap();
        assert_eq!(w.name, re.name);
        assert_eq!(w.cap, re.cap);
        assert_eq!(w.models, re.models);
        assert_eq!(w.observability, re.observability);
        assert_eq!(w.reset, re.reset);
    }

    #[test]
    fn quota_window_minimal_uses_documented_defaults() {
        // Bare minimum: name + hours. Everything else must collapse to
        // the documented defaults so an operator who only knows the
        // window's duration can still express the plan.
        let json = r#"{"name": "5h", "hours": 5}"#;
        let w: QuotaWindow = serde_json::from_str(json).unwrap();
        assert_eq!(w.name, "5h");
        assert_eq!(w.hours, 5);
        assert_eq!(
            w.unit,
            QuotaUnit::default(),
            "missing unit must default to QuotaUnit::default() (Tokens)"
        );
        assert_eq!(
            w.cap, None,
            "missing cap must default to None (percent-only / unknown)"
        );
        assert_eq!(
            w.models, None,
            "missing models must default to None (all models on provider)"
        );
        assert_eq!(
            w.observability,
            Observability::default(),
            "missing observability must default to Header"
        );
        assert_eq!(
            w.reset,
            ResetKind::default(),
            "missing reset must default to Rolling"
        );

        // Re-serialize: the minimal form must collapse back to the same
        // shape so a defaulted config and a hand-typed config are
        // indistinguishable on the wire.
        let out = serde_json::to_string(&w).unwrap();
        let re: QuotaWindow = serde_json::from_str(&out).unwrap();
        assert_eq!(w.unit, re.unit);
        assert_eq!(w.cap, re.cap);
        assert_eq!(w.models, re.models);
        assert_eq!(w.observability, re.observability);
        assert_eq!(w.reset, re.reset);
    }

    #[test]
    fn quota_unit_sessions_round_trips() {
        // `Sessions` is the third flat-fee-plan unit shape (Cursor's
        // "N active sessions at once", Windsurf / Codex session caps).
        // It must survive a JSON round trip with the same
        // `snake_case` rename the other variants already use.
        let u: QuotaUnit = serde_json::from_str(r#""sessions""#).unwrap();
        assert_eq!(u, QuotaUnit::Sessions);

        let s = serde_json::to_string(&QuotaUnit::Sessions).unwrap();
        assert_eq!(s, r#""sessions""#);

        // And it MUST be distinct from the existing units so the rename
        // collision doesn't quietly downgrade a Cursor session cap to a
        // messages cap.
        assert_ne!(QuotaUnit::Sessions, QuotaUnit::Tokens);
        assert_ne!(QuotaUnit::Sessions, QuotaUnit::Requests);
        assert_ne!(QuotaUnit::Sessions, QuotaUnit::Messages);
    }

    #[test]
    fn quota_window_known_cap_with_models_calendar_monthly_observed_via_counter() {
        // The realistic "second" window most operators will actually
        // type: a CodeX / Anthropic-style monthly cap observed via a
        // local counter (`observability = "counter"`) with a calendar
        // monthly reset (`reset = "calendar_monthly"`) — both new
        // fields, both `#[serde(default)]`, and a known cap value.
        // Round-trips intacts.
        let json = r#"{
            "name": "monthly",
            "hours": 720,
            "unit": "messages",
            "cap": 4000.0,
            "observability": "counter",
            "reset": "calendar_monthly"
        }"#;
        let w: QuotaWindow = serde_json::from_str(json).unwrap();
        assert_eq!(w.name, "monthly");
        assert_eq!(w.hours, 720);
        assert_eq!(w.unit, QuotaUnit::Messages);
        assert_eq!(w.cap, Some(4000.0));
        assert_eq!(w.models, None);
        assert_eq!(w.observability, Observability::Counter);
        assert_eq!(w.reset, ResetKind::CalendarMonthly);

        // Round-trip preserves every field by value.
        let out = serde_json::to_string(&w).unwrap();
        let re: QuotaWindow = serde_json::from_str(&out).unwrap();
        assert_eq!(re.name, "monthly");
        assert_eq!(re.cap, Some(4000.0));
        assert_eq!(re.observability, Observability::Counter);
        assert_eq!(re.reset, ResetKind::CalendarMonthly);
    }

    #[test]
    fn validate_rejects_non_finite_budget_and_routing_percentages() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default_provider(dir.path());
        cfg.budget.monthly_cap_usd = Some(f64::NAN);
        cfg.routing
            .scenarios
            .entry("balanced".into())
            .or_default()
            .cap_guard = Some(f64::INFINITY);
        let errs = cfg.validate().join("\n");
        assert!(errs.contains("budget.monthly_cap_usd"), "{errs}");
        assert!(errs.contains("use_target/cap_guard"), "{errs}");
    }

    #[test]
    fn validate_rejects_invalid_subscription_windows_and_scenario_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default_provider(dir.path());
        cfg.routing.scenario = "balnced".into();
        cfg.providers[0].billing = BillingMode::Subscription;
        cfg.providers[0].subscription = Some(SubscriptionPlan {
            monthly_fee_usd: -1.0,
            tier: None,
            windows: vec![QuotaWindow {
                name: "".into(),
                hours: 0,
                unit: QuotaUnit::Tokens,
                cap: Some(f64::NAN),
                models: None,
                observability: Observability::Counter,
                reset: ResetKind::Rolling,
            }],
            ..Default::default()
        });
        let errs = cfg.validate().join("\n");
        assert!(errs.contains("unknown routing scenario"), "{errs}");
        assert!(errs.contains("monthly_fee_usd"), "{errs}");
        assert!(errs.contains("empty name"), "{errs}");
        assert!(errs.contains("hours must"), "{errs}");
        assert!(errs.contains("cap must"), "{errs}");
    }

    #[test]
    fn config_deserialization_rejects_misspelled_scenario_field() {
        let dir = tempfile::tempdir().unwrap();
        let mut value = serde_json::to_value(Config::default_provider(dir.path())).unwrap();
        value["routing"]["scenarios"] = serde_json::json!({
            "balanced": {"cap_gaurd": 50.0}
        });
        let err = serde_json::from_value::<Config>(value).unwrap_err();
        assert!(err.to_string().contains("cap_gaurd"), "{err}");
    }

    #[test]
    fn config_deserialization_rejects_misspelled_budget_field() {
        let dir = tempfile::tempdir().unwrap();
        let mut value = serde_json::to_value(Config::default_provider(dir.path())).unwrap();
        value["budget"] = serde_json::json!({"monthly_cap_usd_typo": 25.0});
        let err = serde_json::from_value::<Config>(value).unwrap_err();
        assert!(err.to_string().contains("monthly_cap_usd_typo"), "{err}");
    }

    #[test]
    fn nested_operational_config_rejects_unknown_fields() {
        let raw = serde_json::json!({
            "id": "openai",
            "base_url": "https://api.openai.com/v1",
            "kind": "openai-responses",
            "auth": {"type": "none"},
            "billing": "subscription",
            "subscription": {
                "tier": "chatgpt-pro",
                "windows": [{
                    "name": "5h",
                    "hours": 5,
                    "observabilty": "header"
                }]
            }
        });
        let err = serde_json::from_value::<Provider>(raw).unwrap_err();
        assert!(err.to_string().contains("observabilty"), "{err}");

        let overlay_err =
            toml::from_str::<VendorOverlay>("[profile]\nname = 'acme'\ndefualt = true\n")
                .unwrap_err();
        assert!(overlay_err.to_string().contains("defualt"), "{overlay_err}");
    }

    #[test]
    fn validate_rejects_unresolved_provider_tier_pair() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default_provider(dir.path());
        cfg.providers[0].id = "openai".into();
        cfg.default_provider = "openai".into();
        cfg.providers[0].billing = BillingMode::Subscription;
        cfg.providers[0].subscription = Some(SubscriptionPlan {
            monthly_fee_usd: 200.0,
            tier: Some("chatgpt-pr0".into()),
            windows: Vec::new(),
            ..Default::default()
        });
        let errs = cfg.validate().join("\n");
        assert!(errs.contains("does not resolve"), "{errs}");
        assert!(errs.contains("chatgpt-pr0"), "{errs}");
    }

    #[test]
    fn validate_accepts_subscription_billing_without_plan() {
        // A flat-fee subscription with a valid key but no explicit terms is
        // valid: marginal cost is \$0 and the runtime treats an unspecified tier
        // as uncapped SubscriptionLive. It must NOT be rejected (this broke
        // real providers like MiniMax that ship a working key without terms).
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default_provider(dir.path());
        cfg.providers[0].billing = BillingMode::Subscription;
        cfg.providers[0].subscription = None;
        let errors = cfg.validate().join("\n");
        assert!(
            !errors.contains("billing=subscription requires subscription terms"),
            "termless subscription must be accepted, got: {errors}"
        );
    }

    #[test]
    fn validate_resolves_tiers_by_provider_classification_not_arbitrary_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default_provider(dir.path());
        cfg.providers.clear();
        cfg.providers.push(Provider {
            id: "openai-codex".into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            kind: "openai-responses".into(),
            auth: Auth::None,
            paid: false,
            billing: BillingMode::Subscription,
            subscription: Some(SubscriptionPlan {
                monthly_fee_usd: 200.0,
                tier: Some("chatgpt-pro".into()),
                windows: Vec::new(),
                ..Default::default()
            }),
            serves: vec!["gpt-".into()],
            azure_api_version: None,
        });
        cfg.providers.push(Provider {
            id: "minimax-sub".into(),
            base_url: "https://api.minimax.io/v1".into(),
            kind: "openai-chat".into(),
            auth: Auth::None,
            paid: false,
            billing: BillingMode::Subscription,
            subscription: Some(SubscriptionPlan {
                monthly_fee_usd: 200.0,
                tier: Some("minimax-max".into()),
                windows: Vec::new(),
                ..Default::default()
            }),
            serves: vec!["MiniMax-".into()],
            azure_api_version: None,
        });
        cfg.default_provider = "openai-codex".into();
        let errs = cfg.validate();
        assert!(errs.is_empty(), "{}", errs.join("\n"));
    }

    // ---------- KNEMON per-account identity (adversarial-review finding #3) ----------
    //
    // KNEMON claims per-account portfolio intelligence, but the config-facing
    // `SubscriptionPlan` did not expose an `account_id`, so every config
    // collapsed to the literal default and two subscriptions on the same
    // `(provider, tier)` tuple silently collided. These three regression
    // tests pin the fix on `crates/zoder-core/src/config.rs`:
    //
    //   (a) differentiated: two providers share the same `Provider.id` and
    //       same `tier` but carry different `account_id`s — both must load
    //       and be retained distinctly.
    //   (b) collision: two providers share the same `(Provider.id,
    //       effective_account_id, tier)` triple — validate() must reject
    //       this as a HARD error.
    //   (c) legacy: a config with NO `account_id` anywhere loads cleanly and
    //       the effective id resolves to `DEFAULT_ACCOUNT_ID`, preserving
    //       backward compatibility bit-for-bit.
    //
    // The dup-triple check fires AFTER the existing dup-`Provider.id`
    // check, which already rejects same-id providers. Test (b) therefore
    // bypasses the existing dup-`Provider.id` check by mutating
    // `cfg.providers` directly so the `validate()` call sees the
    // duplicate and the assertion can pin the new, semantically-distinct
    // error string. Any future relaxation of the dup-`Provider.id` rule
    // (a planned follow-up rewire) will start producing exactly the error
    // message these tests assert on.

    #[test]
    fn subscription_plan_two_providers_same_tier_different_account_ids_both_load() {
        // (a) differentiated: same routing provider, same tier, TWO distinct
        // accounts. Without the fix both effective_account_ids collapse to
        // "default" and the duplicate-triple check has nothing to
        // disambiguate them; with `account_id` plumbed through the
        // validation, both providers load cleanly and their `account_id`s
        // round-trip via JSON intact.
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default_provider(dir.path());
        cfg.providers.clear();
        cfg.providers.push(Provider {
            id: "minimax-personal".into(),
            base_url: "https://api.minimax.io/personal/v1".into(),
            kind: "openai-chat".into(),
            auth: Auth::None,
            paid: false,
            billing: BillingMode::Subscription,
            subscription: Some(SubscriptionPlan {
                monthly_fee_usd: 20.0,
                tier: Some("minimax-max".into()),
                windows: Vec::new(),
                account_id: Some("personal".into()),
            }),
            serves: vec!["MiniMax-".into()],
            azure_api_version: None,
        });
        cfg.providers.push(Provider {
            id: "minimax-team".into(),
            base_url: "https://api.minimax.io/team/v1".into(),
            kind: "openai-chat".into(),
            auth: Auth::None,
            paid: false,
            billing: BillingMode::Subscription,
            subscription: Some(SubscriptionPlan {
                monthly_fee_usd: 200.0,
                tier: Some("minimax-max".into()),
                windows: Vec::new(),
                account_id: Some("team".into()),
            }),
            serves: vec!["MiniMax-".into()],
            azure_api_version: None,
        });
        cfg.default_provider = "minimax-personal".into();

        // Pre-fix this would either silently accept (no triple check) or
        // reject with "duplicate subscription identity" because both
        // effective accounts were "default". Post-fix both load.
        let errs = cfg.validate();
        assert!(
            errs.is_empty(),
            "differentiated accounts must both load; got: {}",
            errs.join("\n")
        );

        // And both account_ids MUST survive a JSON round trip — that's
        // the whole point of exposing the field on the wire.
        let raw = serde_json::to_string(&cfg).unwrap();
        let re: Config = serde_json::from_str(&raw).unwrap();
        let p_acct: Vec<Option<String>> = re
            .providers
            .iter()
            .filter_map(|p| p.subscription.as_ref().map(|s| s.account_id.clone()))
            .collect();
        assert_eq!(
            p_acct,
            vec![Some("personal".into()), Some("team".into())],
            "account_ids must round-trip through JSON for both providers"
        );
        // Both effective ids are what the field says (none collapsed to
        // "default" because both are non-empty).
        assert_eq!(
            re.providers[0]
                .subscription
                .as_ref()
                .unwrap()
                .effective_account_id(),
            "personal"
        );
        assert_eq!(
            re.providers[1]
                .subscription
                .as_ref()
                .unwrap()
                .effective_account_id(),
            "team"
        );
    }

    #[test]
    fn subscription_plan_duplicate_triple_rejected_with_clear_message() {
        // (b) collision: two providers share the same `(Provider.id,
        // effective_account_id, tier)` triple. The `Provider.id`
        // dedup already rejects same-id providers, but this test sets up
        // the scenario on the same id directly to exercise the
        // SUBSCRIPTION-IDENTITY validator (the new check) — which
        // produces its own, more semantically meaningful error message.
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default_provider(dir.path());
        cfg.providers.clear();
        let shared_plan = SubscriptionPlan {
            monthly_fee_usd: 20.0,
            tier: Some("minimax-max".into()),
            windows: Vec::new(),
            account_id: Some("personal".into()),
        };
        // Same id, same account, same tier — both the dup-id check AND
        // the new dup-triple check fire. The assertion targets the new
        // one (the dup-id error is allowed to coexist; we don't pretend
        // the dup-id rule is gone).
        cfg.providers.push(Provider {
            id: "minimax-x".into(),
            base_url: "https://api.minimax.io/x/v1".into(),
            kind: "openai-chat".into(),
            auth: Auth::None,
            paid: false,
            billing: BillingMode::Subscription,
            subscription: Some(shared_plan.clone()),
            serves: vec!["MiniMax-".into()],
            azure_api_version: None,
        });
        cfg.providers.push(Provider {
            id: "minimax-x".into(), // intentional duplicate to also trip dup-id
            base_url: "https://api.minimax.io/x2/v1".into(),
            kind: "openai-chat".into(),
            auth: Auth::None,
            paid: false,
            billing: BillingMode::Subscription,
            subscription: Some(shared_plan),
            serves: vec!["MiniMax-".into()],
            azure_api_version: None,
        });
        cfg.default_provider = "minimax-x".into();

        let errs = cfg.validate();
        let joined = errs.join("\n");
        assert!(
            joined.contains("duplicate subscription identity"),
            "validate() must emit the new per-account triple error; got: {joined}"
        );
        assert!(
            joined.contains("minimax-x"),
            "error must name the colliding provider id; got: {joined}"
        );
        assert!(
            joined.contains("personal"),
            "error must name the colliding account_id; got: {joined}"
        );
        assert!(
            joined.contains("\"minimax-max\""),
            "error must name the colliding tier; got: {joined}"
        );
    }

    #[test]
    fn subscription_plan_legacy_config_without_account_id_loads_with_default() {
        // (c) legacy / back-compat: a config that OMITS `account_id`
        // everywhere must still load, validate, and the effective id
        // must be the constant `DEFAULT_ACCOUNT_ID`. This is the
        // contract every existing config in the wild relies on; breaking
        // it would silently fail every host that hasn't been touched.
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default_provider(dir.path());
        cfg.providers.clear();
        cfg.providers.push(Provider {
            id: "minimax-legacy".into(),
            base_url: "https://api.minimax.io/v1".into(),
            kind: "openai-chat".into(),
            auth: Auth::None,
            paid: false,
            billing: BillingMode::Subscription,
            subscription: Some(SubscriptionPlan {
                monthly_fee_usd: 20.0,
                tier: Some("minimax-max".into()),
                windows: Vec::new(),
                // No account_id set — the field is `None`. The accessor
                // must report `DEFAULT_ACCOUNT_ID` so backward
                // compatibility is preserved.
                account_id: None,
            }),
            serves: vec!["MiniMax-".into()],
            azure_api_version: None,
        });
        cfg.default_provider = "minimax-legacy".into();

        // Pre-fix and post-fix: validate accepts the legacy shape.
        let errs = cfg.validate();
        assert!(
            errs.is_empty(),
            "legacy subscription without account_id must still validate; got: {}",
            errs.join("\n")
        );

        // The accessor returns the sentinel constant for the absent case.
        let plan = cfg.providers[0]
            .subscription
            .as_ref()
            .expect("legacy subscription must be retained");
        assert_eq!(
            plan.effective_account_id(),
            DEFAULT_ACCOUNT_ID,
            "absent account_id must resolve to DEFAULT_ACCOUNT_ID for back-compat"
        );

        // And the wire-level invariant: a config authored without
        // `account_id` (i.e. the literal JSON an operator would have
        // typed before this feature existed) deserializes with
        // `account_id == None` and the effective id is the sentinel.
        // This is the strict "behave exactly as today" test the spec
        // demands.
        let legacy_json = r#"{
            "providers": [{
                "id": "minimax-legacy",
                "base_url": "https://api.minimax.io/v1",
                "kind": "openai-chat",
                "auth": {"type": "none"},
                "billing": "subscription",
                "subscription": {
                    "monthly_fee_usd": 20.0,
                    "tier": "minimax-max",
                    "windows": []
                },
                "serves": ["MiniMax-"]
            }],
            "default_provider": "minimax-legacy",
            "corpus_path": "/tmp/zoder-test/corpus.json",
            "ledger_path": "/tmp/zoder-test/ledger.json",
            "health_path": "/tmp/zoder-test/health.json"
        }"#;
        let parsed: Config = serde_json::from_str(legacy_json).unwrap();
        let legacy_plan = parsed.providers[0]
            .subscription
            .as_ref()
            .expect("legacy config subscription must survive parsing");
        assert_eq!(
            legacy_plan.account_id, None,
            "legacy config without account_id must deserialize to None"
        );
        assert_eq!(
            legacy_plan.effective_account_id(),
            DEFAULT_ACCOUNT_ID,
            "legacy config must behave exactly as today (effective id = default)"
        );
        let legacy_errs = parsed.validate();
        assert!(
            legacy_errs.is_empty(),
            "legacy config must still validate cleanly; got: {}",
            legacy_errs.join("\n")
        );
    }

    // ---------------------------------------------------------------------
    // Cross-model reviewer fallback: chain parsing.
    //
    // Regression fix for the 2026-07-07 reviewer-pipeline defect. A single
    // dead/misbehaving reviewer model used to kill the whole adversarial
    // review even though the author path already had a
    // cross-model fallback chain. The fix lifts the reviewer's
    // `Config::reviewer_model` (a single string today) into an ordered
    // chain so a comma-separated list expresses "if the head fails, try
    // the tail in order, until one answers". These tests pin the parser
    // contract end-to-end so the dispatcher can rely on it.
    // ---------------------------------------------------------------------

    /// Legacy single-model config: `reviewer_model = "x"` produces a
    /// one-element chain. The chain dispatch must treat a single candidate
    /// identically to today's behavior — run it once and report its error
    /// verbatim if it fails (no fallback to be had).
    #[test]
    fn parse_reviewer_chain_single_is_one_element_vec() {
        assert_eq!(
            parse_reviewer_chain(Some("deepseek-coder")),
            vec!["deepseek-coder"]
        );
        assert_eq!(parse_reviewer_chain(None), Vec::<String>::new());
        // Whitespace around the head is trimmed (defensive — JSON deserialization
        // generally doesn't introduce whitespace, but a TOML "" wrapped
        // entry should not produce a " x " key with spaces).
        assert_eq!(
            parse_reviewer_chain(Some("  kimi-k2.6  ")),
            vec!["kimi-k2.6"]
        );
        // Empty string is treated as "unset" — operator wrote the field
        // but left it blank. A blank field MUST NOT silently degrade to
        // a one-element chain containing "" (that would fail loudly at
        // provider resolution and look like a hard config error).
        assert_eq!(parse_reviewer_chain(Some("")), Vec::<String>::new());
    }

    /// Comma-separated chain: a config with `reviewer_model = "a,b,c"`
    /// produces an ordered list of three candidates, head first. Whitespace
    /// around each entry is trimmed and empty entries (the trailing comma
    /// case) are dropped so a typo doesn't slip a sentinel through.
    #[test]
    fn parse_reviewer_chain_csv_splits_and_dedups_whitespace() {
        assert_eq!(
            parse_reviewer_chain(Some("kimi-k2.6,glm-5.1,qwen-coder")),
            vec!["kimi-k2.6", "glm-5.1", "qwen-coder"]
        );
        // Whitespace around entries is trimmed.
        assert_eq!(
            parse_reviewer_chain(Some(" kimi-k2.6 , glm-5.1 ")),
            vec!["kimi-k2.6", "glm-5.1"]
        );
        // Trailing comma + empty entries are dropped (don't sneak "" into
        // the chain — that would route to the placeholder host and 404).
        assert_eq!(parse_reviewer_chain(Some("kimi-k2.6,,")), vec!["kimi-k2.6"]);
        // Leading comma is also tolerated.
        assert_eq!(parse_reviewer_chain(Some(",glm-5.1")), vec!["glm-5.1"]);
    }

    /// `Config::reviewer_models()` is the public accessor the reviewer
    /// dispatcher calls. Wire-format round-trip: a config.json that the
    /// operator writes with `reviewer_model` as a single string still
    /// produces a one-element chain; a comma-separated value produces
    /// the full list.
    #[test]
    fn config_reviewer_models_parses_string_field_into_chain() {
        let mut cfg = Config::default_provider(std::path::Path::new("/tmp/zoder-chain-test"));
        // Legacy single-pin shape — must survive byte-for-byte.
        cfg.reviewer_model = Some("kimi-k2.6".into());
        assert_eq!(
            cfg.reviewer_models(),
            vec!["kimi-k2.6".to_string()],
            "single-string reviewer_model must produce a 1-element chain (back-compat)"
        );
        // Multi-pin shape — the reviewer-pipeline-defect fix.
        cfg.reviewer_model = Some("kimi-k2.6, glm-5.1, qwen-coder".into());
        assert_eq!(
            cfg.reviewer_models(),
            vec![
                "kimi-k2.6".to_string(),
                "glm-5.1".to_string(),
                "qwen-coder".to_string()
            ],
            "comma-separated reviewer_model must produce an ordered candidate list"
        );
        // Unset field — chain is empty (the dispatcher falls through to
        // the cross-family default).
        cfg.reviewer_model = None;
        assert!(
            cfg.reviewer_models().is_empty(),
            "absent reviewer_model must yield an empty chain"
        );
    }

    /// `reviewer_models_for(alias)` honors the per-agent pin first
    /// (the `[agents.<alias>].reviewer_model` channel), falling through
    /// to the profile-level chain. The per-agent pin is independent of
    /// `primary_model` — an operator can pin a different reviewer per
    /// alias without touching the author default. The chain form
    /// extends naturally: a per-agent `reviewer_model = "x,y"` produces
    /// a two-candidate chain just like the profile-level field does.
    #[test]
    fn config_reviewer_models_for_alias_applies_per_agent_pin_first() {
        let mut cfg = Config::default_provider(std::path::Path::new("/tmp/zoder-chain-test"));
        cfg.reviewer_model = Some("fallback-head, fallback-tail".into());
        // Per-agent override wins: only the alias-pinned chain is
        // returned; the profile-level "fallback-*" entries are NOT
        // merged into it (the operator wrote a per-agent pin with
        // explicit intent — layering profile fallbacks would corrupt
        // that intent).
        let mut agents = BTreeMap::new();
        agents.insert(
            "codex".into(),
            AliasedAgentConfig {
                model: None,
                reviewer_model: Some("z-ai/glm-5.1,nvidia/llama-3.3-nemotron".into()),
            },
        );
        cfg.agents = agents;

        let per_agent = cfg.reviewer_models_for(Some("codex"));
        assert_eq!(
            per_agent,
            vec![
                "z-ai/glm-5.1".to_string(),
                "nvidia/llama-3.3-nemotron".to_string()
            ],
            "per-agent reviewer chain must take precedence over profile-level"
        );

        // Unknown alias falls through to the profile-level chain (no
        // per-agent pin to consult), preserving the legacy "per-agent
        // wins, profile-level otherwise" precedence.
        let profile = cfg.reviewer_models_for(Some("unknown-agent"));
        assert_eq!(
            profile,
            vec!["fallback-head".to_string(), "fallback-tail".to_string()],
            "unknown alias must fall through to the profile-level chain"
        );

        // No alias and no per-agent pin — profile-level chain as-is.
        let none = cfg.reviewer_models_for(None);
        assert_eq!(
            none,
            vec!["fallback-head".to_string(), "fallback-tail".to_string()],
            "alias=None must fall through to the profile-level chain"
        );
    }

    // ---------------------------------------------------------------------
    // Azure OpenAI native wire adapter — config tests.
    //
    // The Azure adapter takes a per-provider `azure_api_version` field
    // on `Provider` (see `Provider::azure_api_version` for the full
    // resolution precedence docs). These tests pin the parse contract:
    //
    //   * `azure_api_version = "2024-10-21"` round-trips through both
    //     TOML and JSON without mutation,
    //   * a config that omits the field entirely still parses cleanly
    //     and the field is `None` (back-compat: every existing config
    //     in the wild works unchanged),
    //   * `Provider` carries `azure_api_version` as an explicit field
    //     rather than tacking it onto `base_url` or a separate
    //     `meta` block — the operator inspects one struct, not a
    //     parallel one.
    //
    // The runtime resolution (config field -> env var -> default) is
    // covered by the unit + integration tests in
    // `crates/zoder-core/src/provider.rs` and `crates/zoder-core/tests/provider.rs`.
    // ---------------------------------------------------------------------

    /// `azure_api_version = "..."` parses cleanly through TOML (the
    /// vendor-overlay format) and the parsed field round-trips
    /// byte-for-byte. The serialization shape uses `serde(default,
    /// skip_serializing_if = "Option::is_none")` so absent fields do
    /// NOT pollute the serialized output (back-compat for every
    /// pre-Azure config in the wild).
    #[test]
    fn provider_azure_api_version_round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.azure.toml");
        std::fs::write(
            &path,
            r#"
[[providers]]
id = "azure-gpt4o"
base_url = "https://res.openai.azure.com/openai/deployments/gpt4o"
kind = "azure-openai"
auth = { type = "api_key_header", header = "api-key", var = "AZURE_OPENAI_API_KEY" }
paid = true
billing = "metered"
azure_api_version = "2024-10-21"
"#,
        )
        .unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let overlay: VendorOverlay = toml::from_str(&raw).expect("azure overlay must parse");
        let azure_provider = overlay
            .providers
            .iter()
            .find(|p| p.id == "azure-gpt4o")
            .expect("azure provider must be present");
        assert_eq!(
            azure_provider.azure_api_version.as_deref(),
            Some("2024-10-21"),
            "azure_api_version must round-trip through TOML"
        );
        assert_eq!(azure_provider.kind, "azure-openai");

        // And serialization: serialize the provider back to TOML and
        // confirm `azure_api_version` appears verbatim (no
        // re-shuffling). The `skip_serializing_if = "Option::is_none"`
        // annotation keeps the field absent for legacy providers.
        let re_serialized = toml::to_string(&Provider {
            id: "azure-gpt4o".into(),
            base_url: "https://res.openai.azure.com/openai/deployments/gpt4o".into(),
            kind: "azure-openai".into(),
            auth: Auth::ApiKeyHeader {
                header: "api-key".into(),
                var: "AZURE_OPENAI_API_KEY".into(),
            },
            paid: true,
            billing: BillingMode::Metered,
            subscription: None,
            serves: Vec::new(),
            azure_api_version: Some("2024-10-21".into()),
        })
        .expect("serialize");
        assert!(
            re_serialized.contains("azure_api_version"),
            "serialized output must carry the field: {re_serialized}"
        );
        assert!(
            re_serialized.contains("2024-10-21"),
            "serialized output must carry the version: {re_serialized}"
        );
    }

    /// Back-compat: a config that OMITS `azure_api_version` everywhere
    /// (the pre-Azure shape every existing config uses) must still
    /// load, validate, and the field must be `None`. This is the
    /// "behave exactly as today" guarantee — the new field is purely
    /// additive and never required.
    #[test]
    fn provider_azure_api_version_omitted_means_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default_provider(dir.path());
        cfg.providers.clear();
        cfg.providers.push(Provider {
            id: "azure-legacy".into(),
            base_url: "https://res.openai.azure.com/openai/deployments/gpt4o".into(),
            kind: "azure-openai".into(),
            auth: Auth::ApiKeyHeader {
                header: "api-key".into(),
                var: "AZURE_OPENAI_API_KEY".into(),
            },
            paid: false,
            billing: BillingMode::Metered,
            subscription: None,
            serves: Vec::new(),
            azure_api_version: None,
        });
        cfg.default_provider = "azure-legacy".into();

        // Validate accepts the legacy shape.
        let errs = cfg.validate();
        assert!(
            errs.is_empty(),
            "azure config without azure_api_version must still validate; got: {}",
            errs.join("\n")
        );

        // And the wire-level invariant: a TOML/JSON config that
        // never declared `azure_api_version` parses with the field
        // == None. The runtime then resolves to env var or default
        // at `OpenAiProvider::new` time (covered by the
        // `azure_api_version_resolution_precedence` unit test).
        let legacy_toml = r#"
[[providers]]
id = "azure-legacy"
base_url = "https://res.openai.azure.com/openai/deployments/gpt4o"
kind = "azure-openai"
auth = { type = "api_key_header", header = "api-key", var = "AZURE_OPENAI_API_KEY" }
paid = false
billing = "metered"
"#;
        let overlay: VendorOverlay =
            toml::from_str(legacy_toml).expect("legacy azure overlay must parse");
        assert_eq!(
            overlay.providers[0].azure_api_version, None,
            "absent azure_api_version must deserialize to None (back-compat)"
        );

        // And the serialized output must NOT include the field when
        // it's None — the `skip_serializing_if = "Option::is_none"`
        // annotation keeps legacy config serializations bit-identical
        // (so a config reload doesn't diff-pollute the on-disk file).
        let serialized = toml::to_string(&overlay.providers[0]).expect("serialize");
        assert!(
            !serialized.contains("azure_api_version"),
            "absent azure_api_version must NOT pollute serialized output: {serialized}"
        );
    }

    /// The config field accepts any non-empty string the operator
    /// pins — including a custom preview version that's not the
    /// built-in default. The runtime never validates the format
    /// (Azure's Data Plane treats unknown versions as the latest
    /// supported GA version), so the parse contract is "preserve
    /// verbatim, send verbatim".
    #[test]
    fn provider_azure_api_version_preserves_operator_pinned_versions_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default_provider(dir.path());
        cfg.providers.clear();
        // Three distinct shapes an operator might pin: a preview
        // version with a suffix, a custom data-plane version, and
        // the built-in GA version. Each round-trips verbatim.
        let pinned_versions = ["2024-10-21", "2024-12-01-preview", "2025-01-01"];
        for (i, v) in pinned_versions.iter().enumerate() {
            cfg.providers.push(Provider {
                id: format!("azure-{i}"),
                base_url: format!("https://res.openai.azure.com/openai/deployments/gpt4o-{i}"),
                kind: "azure-openai".into(),
                auth: Auth::ApiKeyHeader {
                    header: "api-key".into(),
                    var: format!("AZURE_KEY_{i}"),
                },
                paid: false,
                billing: BillingMode::Metered,
                subscription: None,
                serves: Vec::new(),
                azure_api_version: Some((*v).to_string()),
            });
        }
        // Set the default_provider so the validate() step doesn't
        // reject the config for "default_provider not among
        // configured providers" — `cfg.providers.clear()` left the
        // default pointing at the now-removed `default` placeholder.
        cfg.default_provider = "azure-0".into();
        // All three validate (the version string is opaque to the
        // engine — Azure's Data Plane handles the version negotiation
        // at request time).
        let errs = cfg.validate();
        assert!(
            errs.is_empty(),
            "all three pinned versions must validate; got: {}",
            errs.join("\n")
        );
        // And each pinned version survives a JSON round-trip.
        let json = serde_json::to_string(&cfg.providers).expect("serialize");
        let re_parsed: Vec<Provider> = serde_json::from_str(&json).expect("re-parse");
        for (i, v) in pinned_versions.iter().enumerate() {
            assert_eq!(
                re_parsed[i].azure_api_version.as_deref(),
                Some(*v),
                "version {v:?} must survive JSON round-trip (found {:?})",
                re_parsed[i].azure_api_version
            );
        }
    }
}
