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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub base_url: String,
    #[serde(default = "default_kind")]
    pub kind: String, // openai-chat | openai-responses | anthropic | custom
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
#[serde(default)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_model: Option<String>,
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
}

/// Routing-scenario block from `config.json` / an overlay TOML. Mirrors the
/// `[routing]` table; the actual scenario data lives in `scenarios`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    /// Active scenario name (e.g. `economy`, `balanced`, `aggressive`,
    /// `unlimited`). Defaults to `balanced`; an unknown name is a graceful
    /// no-op (still resolves to `balanced`) so a typo doesn't break routing.
    #[serde(default = "default_scenario_name")]
    pub scenario: String,
    /// Per-scenario overrides (fields omitted fall through to the preset).
    /// The map keys are the preset names (`economy`, `balanced`, ...).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scenarios: BTreeMap<String, crate::scenarios::RouteScenario>,
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

fn default_strict_free() -> bool {
    true
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
    let usage = crate::quota::plan_usage(entries, &p.id, plan, catalog);
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
            budget: crate::budget::Budget::default(),
            routing: RoutingConfig::default(),
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
        if overlay.profile.default {
            defaults_count += 1;
            if overlay.theme.is_some() {
                default_theme = overlay.theme.clone();
            }
            if overlay.profile.primary_model.is_some() {
                default_primary = overlay.profile.primary_model.clone();
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
            }),
            serves: vec!["MiniMax-".into()],
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
                calls: 1,
                violation: None,
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
}
