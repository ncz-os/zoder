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

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Open a config file with `O_NOFOLLOW | O_NONBLOCK` on Unix so a
/// symlink dropped at the path cannot redirect the read to a FIFO /
/// device (defense-in-depth against the symlink variant of the
/// TOCTOU attack), AND so opening a writer-less FIFO fails fast with
/// `ENXIO` instead of blocking inside the kernel waiting for a
/// writer that will never arrive. `O_NONBLOCK` is semantically a
/// no-op on regular files — read returns data immediately on Linux —
/// so this flag combination does not change the happy-path behavior;
/// it only changes the failure mode for non-regular targets from
/// "block forever" to "return ENXIO". The `cfg(unix)` guard keeps
/// the build green on non-Unix targets where the helper then falls
/// through to a plain `read(true)` open via `File::open`.
#[cfg(unix)]
const CONFIG_OPEN_FLAGS: libc::c_int = libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;

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
#[derive(Clone, Serialize, Deserialize)]
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

/// Hand-written so a `{:?}` on an `Auth` (or anything that transitively
/// contains one — a `Provider`, the whole `Config` — via a future
/// tracing/log/anyhow/panic path) NEVER leaks a live secret. The
/// `Bearer` inline token and the resolved value behind env-based variants
/// are the only sensitive fields; every variant renders its shape and
/// non-secret fields but redacts the token itself. `#[derive(Debug)]`
/// would print `token: "sk-..."` verbatim regardless of call site.
impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::None => f.write_str("None"),
            Auth::Env { var } => f.debug_struct("Env").field("var", var).finish(),
            Auth::Bearer { .. } => f
                .debug_struct("Bearer")
                .field("token", &"[redacted]")
                .finish(),
            Auth::ApiKeyHeader { header, var } => f
                .debug_struct("ApiKeyHeader")
                .field("header", header)
                .field("var", var)
                .finish(),
        }
    }
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
    /// Exact Zeroclaw `model_provider` profile reference represented by this
    /// zoder billing provider (for example `custom.minimax`). This is distinct
    /// from [`Self::id`], which remains an arbitrary zoder-local routing name
    /// and may be the short form used by existing configurations.
    ///
    /// When omitted, the exec configuration loader derives the reference from
    /// the engine registry only when the mapping is unambiguous. Operators
    /// must set it explicitly when two engine provider profiles share the same
    /// short alias and compatible endpoint/kind metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_provider_ref: Option<String>,
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
    /// `MiniMax-M3` -> the `minimax` provider, `enterprise/*` -> an
    /// Enterprise LLM Gateway provider — in a single `zoder exec`. Empty
    /// (the default) means this provider claims no models by prefix and is only reached as the
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

impl Provider {
    /// Whether Zeroclaw's resolved implementation kind is compatible with
    /// zoder's provider transport. Callers must resolve implicit family
    /// defaults before invoking this method; absence is not evidence of
    /// equivalence.
    pub fn engine_kind_is_compatible(&self, engine_kind: &str) -> bool {
        let engine_kind = engine_kind.trim();
        let zoder_kind = self.kind.trim();
        engine_kind == zoder_kind
            || matches!(
                (engine_kind, zoder_kind),
                ("openai-compatible", "openai-chat") | ("openai", "openai-chat")
            )
    }

    /// Whether Zeroclaw's resolved effective endpoint agrees with zoder's
    /// gated provider endpoint. Callers must resolve family defaults and typed
    /// endpoint selectors first.
    pub fn engine_endpoint_is_compatible(
        &self,
        engine_uri: &str,
        engine_implementation: &str,
    ) -> bool {
        let engine_uri = engine_uri.trim_end_matches('/');
        let base_url = self.base_url.trim().trim_end_matches('/');
        if engine_implementation == "openai-codex" {
            // Zoder stores provider base URLs, while Zeroclaw reports the
            // Codex implementation's complete Responses endpoint. Match the
            // daemon's builder: preserve a complete `/responses` endpoint or
            // append that suffix to the configured base.
            let gated_uri = if base_url.ends_with("/responses") {
                base_url.to_owned()
            } else {
                format!("{base_url}/responses")
            };
            return engine_uri == gated_uri;
        }
        engine_uri == base_url
    }
}

/// A model-entry contributed by a vendor overlay's `[providers.models.*]`
/// block. These map a provider alias (e.g. `custom.reviewer`) to a model id
/// (e.g. `"reviewer"`) and serve as the bridge for `--agent` → model
/// resolution in the oneshot path when the agent specifies `model_provider`
/// but has no explicit `.model` pin.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelEntry {
    /// The model id that this provider alias resolves to (e.g. `"reviewer"`).
    pub model: Option<String>,
    /// Provider base URL for this model entry (optional, for informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// Sampling temperature to send for this model. `None` leaves the field
    /// off the wire so the model uses its own default.
    ///
    /// Models publish sampling recommendations that differ from ours -- NVIDIA
    /// documents temperature 1.0 / top_p 0.95 for Nemotron 3.5, for instance --
    /// and before this existed there was no way to honour them: the wire field
    /// was plumbed but nothing could populate it from config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Nucleus-sampling cutoff for this model. `None` omits the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    /// Top-k cutoff. `None` omits the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// Presence penalty. `None` omits the field.
    ///
    /// Not a nicety for every model: Qwen3.8 publishes materially different
    /// presets for its two modes -- presence_penalty 0.0 when thinking, 1.5
    /// when not -- so a single hardcoded value cannot serve both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    /// Free-form `chat_template_kwargs` forwarded verbatim on every request to
    /// this model.
    ///
    /// Some chat templates gate real behaviour on a kwarg rather than a
    /// sampling parameter. Nemotron 3.5 Lightning returns EMPTY content for
    /// coding-agent workloads unless `force_nonempty_content` is set: measured
    /// here at 0 content characters across temperature 0 and 1.0, with and
    /// without `enable_thinking`, and 4936 characters the moment the flag is
    /// passed. A model that cannot be configured this way is unusable as an
    /// agent regardless of its weights, so this is deliberately free-form
    /// rather than an allow-list of the kwargs we happen to know about today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<serde_json::Value>,
    /// Default `reasoning_effort` for this model (`none` | `minimal` | `low` |
    /// `medium` | `high`). An explicit `--reasoning` flag wins. `None` omits
    /// the field.
    ///
    /// Reasoning "flash" models spend the whole output budget thinking on a
    /// large review chunk and return an empty message: measured 2026-10-03 on
    /// deepseek-flash and nvidia/nemotron-3-ultra-550b-a55b, both of which
    /// return content with `none`. MiniMax-M3.1-Flash-Preview rejects `none`
    /// ("requires adaptive thinking") and accepts `minimal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Read-only projection of the zeroclaw engine's `config.toml` model routing.
///
/// zoder's provider/cost configuration and zeroclaw's runtime configuration
/// are intentionally different schemas, but `exec` must still understand the
/// engine's two-hop agent route:
///
/// `agents.<alias>.model_provider` -> `providers.models.<provider>.model`.
///
/// Parsing the complete engine file as [`Config`] is incorrect (and used to
/// fail on unrelated keys such as `schema_version`, identity, memory, and
/// workspace settings). This type extracts only agent/model routing plus the
/// provider reference, implementation kind, endpoint, and fallback closure
/// needed to verify model and billing identity; it ignores the rest of the
/// engine schema.
#[derive(Debug, Clone, Default)]
pub struct EngineModelRegistry {
    agents: BTreeMap<String, EngineAgentModel>,
    models: BTreeMap<String, EngineProviderModel>,
    default_agent: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct EngineAgentModel {
    model: Option<String>,
    model_provider: Option<String>,
}

/// Routing-relevant projection of one
/// `[providers.models.<family>.<alias>]` profile. A provider profile is not a
/// single model: after its primary fails, Zeroclaw can try same-provider model
/// fallbacks and then recursively visit other provider profiles. Keeping the
/// complete closure here is therefore part of model-identity verification,
/// not merely a reliability detail.
#[derive(Debug, Clone, Default)]
struct EngineProviderModel {
    model: String,
    chat_template_kwargs: Option<serde_json::Value>,
    reasoning_effort: Option<String>,
    response_format: Option<serde_json::Value>,
    kind: Option<String>,
    uri: Option<String>,
    api_key: Option<String>,
    wire_api: Option<String>,
    endpoint: Option<String>,
    resource: Option<String>,
    deployment: Option<String>,
    requires_openai_auth: bool,
    fallback_models: Vec<String>,
    fallback: Vec<String>,
}

/// Provider identity attached to an engine agent's `model_provider` route.
///
/// Model equality alone cannot establish billing identity: two provider
/// profiles can serve the same model through different endpoints or accounts.
/// The live `config/get` projection therefore retains the complete
/// non-secret identity Zeroclaw exposes so callers can map it explicitly to
/// the provider whose billing policy they approved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineProviderRoute {
    /// Exact Zeroclaw provider reference, such as `custom.minimax` or a
    /// supported non-dotted built-in reference such as `openai`.
    pub provider_ref: String,
    /// Provider family (the part before the first dot). For a non-dotted
    /// built-in reference this is the complete reference.
    pub provider_type: String,
    /// Provider profile alias (the part after the first dot). For a non-dotted
    /// built-in reference this is also the complete reference.
    pub provider_alias: String,
    /// Canonical provider implementation selected by Zeroclaw after applying
    /// the profile's `kind` override and auth-backed specializations. This is
    /// more specific than the wire transport: native and generic
    /// implementations can speak the same protocol while applying different
    /// endpoint rules.
    pub implementation: String,
    /// Effective transport after resolving the provider family, `kind`,
    /// `wire_api`, and auth-backed OpenAI variants.
    pub effective_kind: String,
    /// Effective endpoint after applying explicit URI precedence, typed
    /// endpoint selectors, or computed family fields.
    pub effective_uri: String,
}

impl EngineModelRegistry {
    /// Parse the routing projection from a zeroclaw engine TOML document.
    pub fn from_toml(raw: &str) -> anyhow::Result<Self> {
        let doc: toml::Value = toml::from_str(raw).context("parsing engine config TOML")?;
        let root = doc
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("engine config root must be a TOML table"))?;

        let mut registry = Self {
            default_agent: root
                .get("acp")
                .and_then(toml::Value::as_table)
                .and_then(|acp| acp.get("default_agent"))
                .and_then(toml::Value::as_str)
                .map(str::trim)
                .filter(|alias| !alias.is_empty())
                .map(str::to_owned),
            ..Self::default()
        };
        if let Some(agents) = root.get("agents").and_then(toml::Value::as_table) {
            for (alias, value) in agents {
                let Some(agent) = value.as_table() else {
                    continue;
                };
                let route = EngineAgentModel {
                    model: agent
                        .get("model")
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned),
                    model_provider: agent
                        .get("model_provider")
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned),
                };
                if route.model.is_some() || route.model_provider.is_some() {
                    registry.agents.insert(alias.clone(), route);
                }
            }
        }

        if let Some(models) = root
            .get("providers")
            .and_then(toml::Value::as_table)
            .and_then(|providers| providers.get("models"))
            .and_then(toml::Value::as_table)
        {
            collect_engine_models(models, "", &mut registry.models)?;
        }

        Ok(registry)
    }

    /// Parse the same routing projection from the running daemon's masked
    /// `config/get` JSON response. This is deliberately separate from
    /// [`Self::from_toml`]: an on-disk file can be newer than a long-running
    /// daemon, while an explicit model pin must be checked against the config
    /// the daemon is actually serving.
    pub fn from_json(value: &serde_json::Value) -> anyhow::Result<Self> {
        let root = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("engine config JSON root must be an object"))?;
        let mut registry = Self {
            default_agent: root
                .get("acp")
                .and_then(serde_json::Value::as_object)
                .and_then(|acp| acp.get("default_agent"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|alias| !alias.is_empty())
                .map(str::to_owned),
            ..Self::default()
        };

        if let Some(agents) = root.get("agents").and_then(serde_json::Value::as_object) {
            for (alias, value) in agents {
                let Some(agent) = value.as_object() else {
                    continue;
                };
                let route = EngineAgentModel {
                    model: agent
                        .get("model")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    model_provider: agent
                        .get("model_provider")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                };
                if route.model.is_some() || route.model_provider.is_some() {
                    registry.agents.insert(alias.clone(), route);
                }
            }
        }

        if let Some(models) = root
            .get("providers")
            .and_then(serde_json::Value::as_object)
            .and_then(|providers| providers.get("models"))
            .and_then(serde_json::Value::as_object)
        {
            collect_engine_models_json(models, "", &mut registry.models)?;
        }

        Ok(registry)
    }

    /// Load a zeroclaw engine config. A missing file means there are no known
    /// engine routes; malformed or unsafe files fail closed because guessing a
    /// route can run a different model than the operator selected.
    pub fn load_from(path: &Path) -> anyhow::Result<Self> {
        if !path
            .try_exists()
            .with_context(|| format!("checking engine config at {}", path.display()))?
        {
            return Ok(Self::default());
        }
        let raw = read_bounded_regular_file(path, Config::MAX_CONFIG_BYTES)
            .with_context(|| format!("reading engine model routes from {}", path.display()))?;
        Self::from_toml(&raw)
            .with_context(|| format!("loading engine model routes from {}", path.display()))
    }

    /// Whether the engine config contributed any agent model routes.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Resolve an agent alias to the model the engine is configured to run.
    /// A direct `model` wins; otherwise follow `model_provider` into the
    /// flattened `[providers.models.*]` registry.
    pub fn model_for_agent(&self, alias: &str) -> Option<&str> {
        let route = self.agents.get(alias).or_else(|| {
            self.agents
                .iter()
                .find(|(configured, _)| configured.eq_ignore_ascii_case(alias))
                .map(|(_, route)| route)
        })?;
        route.model.as_deref().or_else(|| {
            route
                .model_provider
                .as_deref()
                .and_then(|provider| self.models.get(provider))
                .map(|provider| provider.model.as_str())
        })
    }

    /// Return the chat-template options attached to the engine profile that
    /// serves this model. The reviewer alias wins when several profiles serve
    /// the same checkpoint; an explicit selected agent wins over that alias.
    /// These options are read-only and contain no auth material.
    pub fn chat_template_kwargs_for_model(
        &self,
        model: &str,
        agent_alias: Option<&str>,
    ) -> Option<&serde_json::Value> {
        for alias in agent_alias.into_iter().chain(std::iter::once("reviewer")) {
            if let Some(profile) = self
                .model_provider_ref_for_agent(alias)
                .and_then(|reference| self.models.get(reference))
            {
                if profile.model == model && profile.chat_template_kwargs.is_some() {
                    return profile.chat_template_kwargs.as_ref();
                }
            }
        }
        self.models
            .values()
            .find(|profile| profile.model == model && profile.chat_template_kwargs.is_some())
            .and_then(|profile| profile.chat_template_kwargs.as_ref())
    }

    /// Return the `reasoning_effort` attached to the engine profile that serves
    /// this model, with the same precedence as
    /// [`Self::chat_template_kwargs_for_model`].
    pub fn reasoning_effort_for_model(
        &self,
        model: &str,
        agent_alias: Option<&str>,
    ) -> Option<&str> {
        for alias in agent_alias.into_iter().chain(std::iter::once("reviewer")) {
            if let Some(profile) = self
                .model_provider_ref_for_agent(alias)
                .and_then(|reference| self.models.get(reference))
            {
                if profile.model == model && profile.reasoning_effort.is_some() {
                    return profile.reasoning_effort.as_deref();
                }
            }
        }
        self.models
            .values()
            .find(|profile| profile.model == model && profile.reasoning_effort.is_some())
            .and_then(|profile| profile.reasoning_effort.as_deref())
    }

    /// Return the OpenAI-style `response_format` object attached to the engine
    /// profile that serves this model, with the same precedence as
    /// [`Self::reasoning_effort_for_model`]. A direct reviewer call (which does
    /// not forward the engine's `provider_extra` blob) uses this to ask a
    /// structured-output reviewer for `json_object`. `None` omits the field.
    pub fn response_format_for_model(
        &self,
        model: &str,
        agent_alias: Option<&str>,
    ) -> Option<&serde_json::Value> {
        for alias in agent_alias.into_iter().chain(std::iter::once("reviewer")) {
            if let Some(profile) = self
                .model_provider_ref_for_agent(alias)
                .and_then(|reference| self.models.get(reference))
            {
                if profile.model == model && profile.response_format.is_some() {
                    return profile.response_format.as_ref();
                }
            }
        }
        self.models
            .values()
            .find(|profile| profile.model == model && profile.response_format.is_some())
            .and_then(|profile| profile.response_format.as_ref())
    }

    /// Return the exact provider-profile reference configured on an agent,
    /// without projecting or validating the provider's transport metadata.
    /// Callers that only need to preserve routing identity use this before the
    /// full live-route attestation validates kind and endpoint.
    pub fn model_provider_ref_for_agent(&self, alias: &str) -> Option<&str> {
        self.agents
            .get(alias)
            .or_else(|| {
                self.agents
                    .iter()
                    .find(|(configured, _)| configured.eq_ignore_ascii_case(alias))
                    .map(|(_, route)| route)
            })?
            .model_provider
            .as_deref()
    }

    /// Return the complete ordered model-attempt closure for an agent.
    ///
    /// The first entry is the primary model. Remaining entries are every
    /// `fallback_models` item on that provider followed by the primary and
    /// same-provider fallbacks of each recursively referenced `fallback`
    /// provider. A direct `[agents.<alias>].model` remains the primary when
    /// present, but the selected `model_provider` profile's `fallback_models`
    /// and downstream provider fallbacks still belong to the attempt closure;
    /// only that profile's superseded primary is omitted.
    ///
    /// Dangling provider references and cycles fail closed. Zeroclaw may prune
    /// those edges at runtime, but route verification must never silently
    /// discard a configured attempt that could otherwise escape policy gates.
    pub fn model_candidates_for_agent(&self, alias: &str) -> anyhow::Result<Option<Vec<String>>> {
        let Some(route) = self.agents.get(alias).or_else(|| {
            self.agents
                .iter()
                .find(|(configured, _)| configured.eq_ignore_ascii_case(alias))
                .map(|(_, route)| route)
        }) else {
            return Ok(None);
        };

        let mut candidates = Vec::new();
        if let Some(model) = route.model.as_deref() {
            candidates.push(model.to_owned());
            if let Some(provider) = route.model_provider.as_deref() {
                let mut path = Vec::new();
                self.collect_provider_model_candidates(
                    provider,
                    false,
                    &mut path,
                    &mut candidates,
                )?;
            }
            return Ok(Some(candidates));
        }
        let Some(provider) = route.model_provider.as_deref() else {
            return Ok(None);
        };

        let mut path = Vec::new();
        self.collect_provider_model_candidates(provider, true, &mut path, &mut candidates)?;
        Ok(Some(candidates))
    }

    /// Return the non-secret provider identity for an agent's engine route.
    ///
    /// A direct agent `model` without `model_provider` has no provider identity
    /// and returns `None`. A dangling or malformed provider reference is a hard
    /// error so billing verification cannot silently fall back to model-only
    /// matching.
    pub fn provider_route_for_agent(
        &self,
        alias: &str,
    ) -> anyhow::Result<Option<EngineProviderRoute>> {
        let Some(route) = self.agents.get(alias).or_else(|| {
            self.agents
                .iter()
                .find(|(configured, _)| configured.eq_ignore_ascii_case(alias))
                .map(|(_, route)| route)
        }) else {
            return Ok(None);
        };
        let Some(provider_ref) = route.model_provider.as_deref() else {
            return Ok(None);
        };
        if provider_ref.trim().is_empty() {
            anyhow::bail!("engine model provider reference for agent {alias:?} is empty");
        }
        let Some((configured_ref, provider)) =
            self.models.get_key_value(provider_ref).or_else(|| {
                self.models
                    .iter()
                    .find(|(configured, _)| configured.eq_ignore_ascii_case(provider_ref))
            })
        else {
            anyhow::bail!(
                "engine model provider {provider_ref:?} for agent {alias:?} is not configured"
            );
        };

        Ok(Some(Self::project_provider_route(
            configured_ref,
            provider,
        )?))
    }

    fn provider_route_for_ref(
        &self,
        provider_ref: &str,
    ) -> anyhow::Result<Option<EngineProviderRoute>> {
        let Some((configured_ref, provider)) = self.models.get_key_value(provider_ref) else {
            return Ok(None);
        };
        Ok(Some(Self::project_provider_route(
            configured_ref,
            provider,
        )?))
    }

    /// Resolve only profiles whose short alias could map to the zoder
    /// provider being loaded. Unrelated engine profiles may use native
    /// transports zoder never dispatches and must not break an otherwise
    /// valid oneshot/Goose configuration merely by existing.
    fn provider_routes_for_alias(&self, alias: &str) -> anyhow::Result<Vec<EngineProviderRoute>> {
        self.models
            .iter()
            .filter(|(provider_ref, _)| {
                provider_ref
                    .split_once('.')
                    .map_or(provider_ref.as_str(), |(_, profile)| profile)
                    == alias
            })
            .map(|(provider_ref, provider)| Self::project_provider_route(provider_ref, provider))
            .collect()
    }

    fn project_provider_route(
        provider_ref: &str,
        provider: &EngineProviderModel,
    ) -> anyhow::Result<EngineProviderRoute> {
        let (provider_type, provider_alias) = provider_ref
            .split_once('.')
            .unwrap_or((provider_ref, provider_ref));
        let implementation = resolve_engine_provider_implementation(provider_type, provider);
        let effective_kind = resolve_engine_provider_kind(provider_ref, provider, &implementation)?;
        let effective_uri = resolve_engine_provider_uri(
            provider_ref,
            provider_type,
            provider,
            &implementation,
            &effective_kind,
        )?;
        Ok(EngineProviderRoute {
            provider_ref: provider_ref.to_owned(),
            provider_type: provider_type.to_owned(),
            provider_alias: provider_alias.to_owned(),
            implementation,
            effective_kind,
            effective_uri,
        })
    }

    fn collect_provider_model_candidates(
        &self,
        provider_alias: &str,
        include_primary: bool,
        path: &mut Vec<String>,
        out: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        if let Some(cycle_start) = path
            .iter()
            .position(|seen| seen.eq_ignore_ascii_case(provider_alias))
        {
            let mut cycle = path[cycle_start..].to_vec();
            cycle.push(provider_alias.to_owned());
            anyhow::bail!(
                "provider fallback cycle in engine model routes: {}",
                cycle.join(" -> ")
            );
        }
        let Some((configured_alias, provider)) =
            self.models.get_key_value(provider_alias).or_else(|| {
                self.models
                    .iter()
                    .find(|(configured, _)| configured.eq_ignore_ascii_case(provider_alias))
            })
        else {
            anyhow::bail!(
                "engine model provider {provider_alias:?} is referenced by a route or fallback, \
                 but is not configured"
            );
        };

        path.push(configured_alias.clone());
        if include_primary {
            out.push(provider.model.clone());
        }
        out.extend(provider.fallback_models.iter().cloned());
        for fallback_provider in &provider.fallback {
            self.collect_provider_model_candidates(fallback_provider, true, path, out)?;
        }
        path.pop();
        Ok(())
    }

    /// Resolve a model id to a configured engine agent without silently
    /// selecting the lexicographically-first alias when routes are duplicated.
    /// `[acp].default_agent` is the engine's semantic tie-breaker when it is one
    /// of the matching agents; otherwise the caller must select an agent.
    pub fn agent_for_model(&self, model: &str) -> anyhow::Result<Option<&str>> {
        self.agent_for_model_with_preference(model, None)
    }

    /// Resolve an exact model id while allowing an explicit `--agent` to break
    /// an otherwise ambiguous mapping. Model identifiers are not aliases and
    /// are never matched case-insensitively.
    pub fn agent_for_model_with_preference(
        &self,
        model: &str,
        preferred_agent: Option<&str>,
    ) -> anyhow::Result<Option<&str>> {
        let matches: Vec<&str> = self
            .agents
            .keys()
            .filter(|alias| self.model_for_agent(alias) == Some(model))
            .map(String::as_str)
            .collect();
        self.select_matching_agent(model, preferred_agent, &matches)
    }

    fn select_matching_agent<'a>(
        &'a self,
        model: &str,
        preferred_agent: Option<&str>,
        matches: &[&'a str],
    ) -> anyhow::Result<Option<&'a str>> {
        match matches {
            [] => Ok(None),
            [only] => Ok(Some(*only)),
            _ => {
                if let Some(alias) = preferred_agent.and_then(|preferred| {
                    matches
                        .iter()
                        .copied()
                        .find(|alias| alias.eq_ignore_ascii_case(preferred))
                }) {
                    return Ok(Some(alias));
                }
                if preferred_agent.is_none() {
                    if let Some(alias) = self.default_agent.as_deref().and_then(|default| {
                        matches
                            .iter()
                            .copied()
                            .find(|alias| alias.eq_ignore_ascii_case(default))
                    }) {
                        return Ok(Some(alias));
                    }
                }
                anyhow::bail!(
                    "model {model:?} is configured on multiple engine agents ({}); pass \
                     --agent <alias> to select one{}",
                    matches.join(", "),
                    if self.default_agent.is_some() {
                        " (the configured [acp].default_agent is not one of them)"
                    } else {
                        " or configure [acp].default_agent"
                    }
                )
            }
        }
    }
}

/// Resolve the canonical factory key Zeroclaw selects for a provider profile.
/// `kind` overrides the profile family before the daemon decides whether the
/// supplied URI is meaningful, so endpoint attestation must retain this
/// identity instead of reducing it immediately to a wire protocol.
fn resolve_engine_provider_implementation(
    provider_type: &str,
    provider: &EngineProviderModel,
) -> String {
    let selected = provider
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(provider_type);
    let implementation = canonicalize_engine_provider_implementation(selected);
    if implementation == "openai" && provider.requires_openai_auth {
        // OpenAI's typed factory replaces itself with the subscription-backed
        // Codex Responses implementation before it considers the ordinary
        // OpenAI chat/responses paths.
        "openai-codex".to_owned()
    } else {
        implementation.to_owned()
    }
}

/// Mirror the aliases Zeroclaw canonicalizes before factory dispatch. Keep the
/// canonical implementation in the route even when several implementations
/// share one effective transport.
fn canonicalize_engine_provider_implementation(implementation: &str) -> &str {
    match implementation {
        "azure_openai" | "azure-openai" => "azure",
        "openai_compatible" => "openai-compatible",
        "openai_responses" => "openai-responses",
        "openai_codex" | "codex" => "openai-codex",
        "anthropic-custom" | "claude-code" => "anthropic",
        "grok" => "xai",
        "google" | "google-gemini" => "gemini",
        "together-ai" => "together",
        "fireworks-ai" => "fireworks",
        "vercel-ai" => "vercel",
        "cloudflare-ai" => "cloudflare",
        "nvidia-nim" | "build.nvidia.com" => "nvidia",
        "aws-bedrock" => "bedrock",
        "lm-studio" => "lmstudio",
        "lite-llm" => "litellm",
        "hf" => "huggingface",
        "01ai" | "lingyiwanwu" => "yi",
        "tencent" => "hunyuan",
        "baidu" => "qianfan",
        "github-copilot" => "copilot",
        "ovhcloud" => "ovh",
        "opencode-zen" | "opencode-go" => "opencode",
        "llama.cpp" => "llamacpp",
        "deep-myst" => "deepmyst",
        "silicon-flow" => "siliconflow",
        "deep-infra" => "deepinfra",
        "ai21-labs" => "ai21",
        "friendliai" => "friendli",
        "lepton-ai" => "lepton",
        "lambda-ai" => "lambda_ai",
        "github-models" => "github_models",
        "step" | "stepfun-intl" | "step-intl" => "stepfun",
        "gemini-cli" => "gemini_cli",
        "volcengine" | "ark" | "doubao-cn" => "doubao",
        "kimi" | "kimi-cn" | "kimi-intl" | "kimi-global" | "kimi-code" | "kimi_coding"
        | "kimi_for_coding" | "moonshot-cn" | "moonshot-intl" | "moonshot-global" => "moonshot",
        "qwen-cn"
        | "qwen-intl"
        | "qwen-us"
        | "qwen-international"
        | "qwen-code"
        | "qwen-oauth"
        | "qwen_oauth"
        | "dashscope"
        | "dashscope-cn"
        | "dashscope-intl"
        | "dashscope-us"
        | "dashscope-international"
        | "bailian"
        | "aliyun-bailian"
        | "aliyun" => "qwen",
        "zhipu" | "glm-global" | "zhipu-global" | "glm-cn" | "zhipu-cn" | "bigmodel" => "glm",
        "z.ai" | "zai-global" | "z.ai-global" | "zai-cn" | "z.ai-cn" => "zai",
        "minimax-intl"
        | "minimax-io"
        | "minimax-global"
        | "minimax-portal"
        | "minimax-portal-global"
        | "minimax-cn"
        | "minimaxi"
        | "minimax-portal-cn"
        | "minimax-oauth"
        | "minimax-oauth-global"
        | "minimax-oauth-cn" => "minimax",
        _ => implementation,
    }
}

/// Resolve the transport Zeroclaw actually constructs for a provider profile.
fn resolve_engine_provider_kind(
    provider_ref: &str,
    provider: &EngineProviderModel,
    implementation: &str,
) -> anyhow::Result<String> {
    let wire_api = provider
        .wire_api
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(wire_api) = wire_api {
        anyhow::ensure!(
            matches!(wire_api, "responses" | "chat_completions"),
            "engine provider {provider_ref:?} has unsupported wire_api {wire_api:?}; refusing to \
             infer its effective transport"
        );
    }

    let kind = match implementation {
        "anthropic" => "anthropic",
        "azure" => "azure-openai",
        "openai-responses" | "openai-codex" => "openai-responses",
        "openai" => {
            if provider.requires_openai_auth || wire_api == Some("responses") {
                "openai-responses"
            } else {
                "openai-chat"
            }
        }
        "custom" => {
            if wire_api == Some("responses") {
                "openai-responses"
            } else {
                "openai-chat"
            }
        }
        "openai-compatible" | "openai_compatible" | "openai-chat" | "openai_chat" => "openai-chat",
        family if openai_compatible_engine_family(family) => {
            // Of Zeroclaw's typed compatibility families, only OpenCode
            // currently consumes its profile's `wire_api` override. The rest
            // instantiate the chat-completions provider even if the shared
            // base happens to contain that otherwise-unused field.
            if family == "opencode" && wire_api == Some("responses") {
                "openai-responses"
            } else {
                "openai-chat"
            }
        }
        _ => anyhow::bail!(
            "engine provider {provider_ref:?} omits an authoritative transport mapping for \
             Zeroclaw implementation {implementation:?}; set an explicit supported kind or do \
             not route this provider through zoder"
        ),
    };
    Ok(kind.to_owned())
}

/// Resolve the endpoint Zeroclaw actually binds after applying the selected
/// implementation's URI-consumption rule and typed family defaults. Explicit
/// `uri` wins only for factories that consume it; Azure is deliberately
/// computed from `resource`/`deployment`, matching the current daemon factory.
/// No missing or unknown default is treated as equal.
fn resolve_engine_provider_uri(
    provider_ref: &str,
    provider_type: &str,
    provider: &EngineProviderModel,
    implementation: &str,
    effective_kind: &str,
) -> anyhow::Result<String> {
    if effective_kind == "azure-openai" {
        anyhow::ensure!(
            provider_type == "azure",
            "engine provider {provider_ref:?} selects Azure through a different typed family; \
             its effective resource/deployment endpoint cannot be resolved authoritatively"
        );
        let resource =
            required_engine_provider_field(provider_ref, "resource", &provider.resource)?;
        let deployment =
            required_engine_provider_field(provider_ref, "deployment", &provider.deployment)?;
        return Ok(format!(
            "https://{resource}.openai.azure.com/openai/deployments/{deployment}"
        ));
    }

    // These native factories ignore `api_url` even when the selected profile
    // contains `uri`. Key this rule off the canonical implementation selected
    // by `kind`, not the profile family: `custom.proxy kind = "openrouter"`
    // still runs OpenRouter, while `openrouter.proxy kind =
    // "openai-compatible"` really does consume the explicit URI.
    if let Some(uri) = ignored_uri_engine_provider_endpoint(implementation) {
        return Ok(uri.to_owned());
    }

    if let Some(uri) = provider
        .uri
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(uri.to_owned());
    }

    if implementation == "openai-codex" {
        // OpenAiCodexModelProvider uses this ChatGPT subscription endpoint
        // when provider_api_url is absent. It is not the OpenAI API-family
        // default and must remain a distinct billing/trust boundary.
        return Ok("https://chatgpt.com/backend-api/codex/responses".to_owned());
    }

    if provider_type == "custom" {
        anyhow::bail!(
            "engine provider {provider_ref:?} has no explicit endpoint; custom profiles require \
             uri so their effective billing endpoint can be verified"
        );
    }

    if let Some(uri) = resolved_typed_engine_endpoint(provider_ref, provider_type, provider)? {
        return Ok(uri.to_owned());
    }
    if let Some(uri) = fixed_engine_provider_endpoint(provider_type) {
        return Ok(uri.to_owned());
    }

    anyhow::bail!(
        "engine provider {provider_ref:?} has no explicit uri and zoder cannot resolve an \
         authoritative effective endpoint for Zeroclaw family {provider_type:?}; refusing to \
         equate an omitted endpoint with the gated provider"
    )
}

fn ignored_uri_engine_provider_endpoint(implementation: &str) -> Option<&'static str> {
    match implementation {
        "groq" => Some("https://api.groq.com/openai/v1"),
        "openrouter" => Some("https://openrouter.ai/api/v1"),
        _ => None,
    }
}

fn required_engine_provider_field<'a>(
    provider_ref: &str,
    field: &str,
    value: &'a Option<String>,
) -> anyhow::Result<&'a str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "engine provider {provider_ref:?} requires non-empty typed field {field:?} to \
                 resolve its effective endpoint"
            )
        })
}

fn resolved_typed_engine_endpoint(
    provider_ref: &str,
    provider_type: &str,
    provider: &EngineProviderModel,
) -> anyhow::Result<Option<&'static str>> {
    let endpoint = provider
        .endpoint
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let resolved = match provider_type {
        "minimax" => match endpoint.unwrap_or("intl") {
            "cn" => "https://api.minimaxi.com/v1",
            "intl" => "https://api.minimax.io/v1",
            value => return unknown_engine_endpoint(provider_ref, value),
        },
        "moonshot" => match endpoint.unwrap_or("intl") {
            "cn" => "https://api.moonshot.cn/v1",
            "intl" => "https://api.moonshot.ai/v1",
            "code" => "https://api.kimi.com/coding/v1",
            value => return unknown_engine_endpoint(provider_ref, value),
        },
        "qwen" => match endpoint.unwrap_or("intl") {
            "cn" => "https://dashscope.aliyuncs.com/compatible-mode/v1",
            "intl" => "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
            "us" => "https://dashscope-us.aliyuncs.com/compatible-mode/v1",
            "code" => {
                "https://dashscope.aliyuncs.com/api/v1/services/aigc/text-generation/generation"
            }
            value => return unknown_engine_endpoint(provider_ref, value),
        },
        "glm" => match endpoint.unwrap_or("global") {
            "cn" => "https://open.bigmodel.cn/api/paas/v4",
            "global" => "https://api.z.ai/api/paas/v4",
            value => return unknown_engine_endpoint(provider_ref, value),
        },
        "zai" => match endpoint.unwrap_or("global") {
            "cn" => "https://open.bigmodel.cn/api/coding/paas/v4",
            "global" => "https://api.z.ai/api/coding/paas/v4",
            value => return unknown_engine_endpoint(provider_ref, value),
        },
        "stepfun" => match endpoint.unwrap_or("intl") {
            "cn" => "https://api.stepfun.com/v1",
            "intl" => "https://api.stepfun.ai/v1",
            value => return unknown_engine_endpoint(provider_ref, value),
        },
        "kilo" => match endpoint.unwrap_or("gateway") {
            "gateway" => "https://api.kilo.ai/api/gateway",
            value => return unknown_engine_endpoint(provider_ref, value),
        },
        _ => return Ok(None),
    };
    Ok(Some(resolved))
}

fn unknown_engine_endpoint<T>(provider_ref: &str, endpoint: &str) -> anyhow::Result<T> {
    anyhow::bail!(
        "engine provider {provider_ref:?} has unknown typed endpoint selector {endpoint:?}; \
         refusing to guess its effective endpoint"
    )
}

/// Families whose current Zeroclaw implementation uses the OpenAI-compatible
/// chat transport when `kind` is omitted. This mirrors the downstream daemon
/// factory; unlisted native/subprocess families fail closed above.
fn openai_compatible_engine_family(family: &str) -> bool {
    matches!(
        family,
        "ai21"
            | "aihubmix"
            | "anyscale"
            | "arcee"
            | "astrai"
            | "atomic_chat"
            | "avian"
            | "baichuan"
            | "baseten"
            | "cerebras"
            | "cloudflare"
            | "cohere"
            | "deepinfra"
            | "deepmyst"
            | "deepseek"
            | "doubao"
            | "featherless"
            | "fireworks"
            | "friendli"
            | "github_models"
            | "glm"
            | "groq"
            | "huggingface"
            | "hunyuan"
            | "hyperbolic"
            | "inception"
            | "kilo"
            | "lambda_ai"
            | "lepton"
            | "litellm"
            | "llamacpp"
            | "lmstudio"
            | "manifest"
            | "minimax"
            | "mistral"
            | "moonshot"
            | "morph"
            | "nearai"
            | "nebius"
            | "novita"
            | "nscale"
            | "nvidia"
            | "opencode"
            | "openrouter"
            | "osaurus"
            | "perplexity"
            | "qianfan"
            | "qwen"
            | "reka"
            | "sambanova"
            | "sglang"
            | "siliconflow"
            | "stepfun"
            | "synthetic"
            | "together"
            | "upstage"
            | "venice"
            | "vercel"
            | "vllm"
            | "xai"
            | "yi"
            | "zai"
    )
}

/// Fixed defaults consumed by Zeroclaw's current OpenAI-compatible factories.
/// Multi-region and computed families are resolved separately above.
fn fixed_engine_provider_endpoint(family: &str) -> Option<&'static str> {
    Some(match family {
        "openai" => "https://api.openai.com/v1",
        "anthropic" => "https://api.anthropic.com",
        "openrouter" => "https://openrouter.ai/api/v1",
        "groq" => "https://api.groq.com/openai/v1",
        "vercel" => "https://ai-gateway.vercel.sh/v1",
        "cloudflare" => "https://gateway.ai.cloudflare.com/v1",
        "synthetic" => "https://api.synthetic.new/openai/v1",
        "opencode" => "https://opencode.ai/zen/v1",
        "doubao" => "https://ark.cn-beijing.volces.com/api/v3",
        "mistral" => "https://api.mistral.ai/v1",
        "deepseek" => "https://api.deepseek.com",
        "together" => "https://api.together.xyz",
        "fireworks" => "https://api.fireworks.ai/inference/v1",
        "novita" => "https://api.novita.ai/openai",
        "perplexity" => "https://api.perplexity.ai",
        "cohere" => "https://api.cohere.com/compatibility",
        "sglang" => "http://localhost:30000/v1",
        "vllm" => "http://localhost:8000/v1",
        "astrai" => "https://as-trai.com/v1",
        "siliconflow" => "https://api.siliconflow.com/v1",
        "aihubmix" => "https://aihubmix.com/v1",
        "litellm" => "http://localhost:4000/v1",
        "cerebras" => "https://api.cerebras.ai/v1",
        "sambanova" => "https://api.sambanova.ai/v1",
        "hyperbolic" => "https://api.hyperbolic.xyz/v1",
        "deepinfra" => "https://api.deepinfra.com/v1/openai",
        "huggingface" => "https://router.huggingface.co/v1",
        "ai21" => "https://api.ai21.com/studio/v1",
        "reka" => "https://api.reka.ai/v1",
        "baseten" => "https://inference.baseten.co/v1",
        "nscale" => "https://inference.api.nscale.com/v1",
        "anyscale" => "https://api.endpoints.anyscale.com/v1",
        "nebius" => "https://api.tokenfactory.nebius.com/v1",
        "friendli" => "https://api.friendli.ai/serverless/v1",
        "lepton" => "https://llama3-1-405b.lepton.run/api/v1",
        "manifest" => "https://app.manifest.build/v1",
        "morph" => "https://api.morphllm.com/v1",
        "github_models" => "https://models.github.ai/inference",
        "upstage" => "https://api.upstage.ai/v1",
        "featherless" => "https://api.featherless.ai/v1",
        "arcee" => "https://api.arcee.ai/api/v1",
        "lambda_ai" => "https://api.lambda.ai/v1",
        "inception" => "https://api.inceptionlabs.ai/v1",
        "baichuan" => "https://api.baichuan-ai.com/v1",
        "yi" => "https://api.lingyiwanwu.com/v1",
        "hunyuan" => "https://api.hunyuan.cloud.tencent.com/v1",
        "avian" => "https://api.avian.io/v1",
        "deepmyst" => "https://api.deepmyst.com/v1",
        "venice" => "https://api.venice.ai",
        "nearai" => "https://cloud-api.near.ai/v1",
        "atomic_chat" => "http://127.0.0.1:1337/v1",
        "xai" => "https://api.x.ai/v1",
        "lmstudio" => "http://localhost:1234/v1",
        "llamacpp" => "http://localhost:8080/v1",
        "osaurus" => "http://localhost:1337/v1",
        "qianfan" => "https://qianfan.baidubce.com/v2",
        _ => return None,
    })
}

fn collect_engine_models(
    table: &toml::Table,
    prefix: &str,
    out: &mut BTreeMap<String, EngineProviderModel>,
) -> anyhow::Result<()> {
    for (name, value) in table {
        let Some(child) = value.as_table() else {
            continue;
        };
        let alias = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if let Some(model) = child.get("model").and_then(toml::Value::as_str) {
            out.insert(
                alias.clone(),
                EngineProviderModel {
                    model: model.to_owned(),
                    chat_template_kwargs: child
                        .get("chat_template_kwargs")
                        .and_then(|value| serde_json::to_value(value).ok())
                        .filter(serde_json::Value::is_object),
                    // zeroclaw forwards provider_extra verbatim into the request
                    // body, so provider_extra.reasoning_effort is the one place an
                    // operator sets it for both the engine loop and zoder's direct
                    // reviewer path (which does not forward provider_extra). When
                    // both are set, provider_extra wins: it is the value the
                    // engine actually sends on the wire.
                    reasoning_effort: child
                        .get("provider_extra")
                        .and_then(|extra| extra.get("reasoning_effort"))
                        .or_else(|| child.get("reasoning_effort"))
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned),
                    // zeroclaw forwards `provider_extra` verbatim, so
                    // `provider_extra.response_format` is the one place an
                    // operator pins an OpenAI-style structured-output object
                    // for both the engine loop and zoder's direct reviewer
                    // path. Only a table/object is accepted (a stray string or
                    // array is ignored rather than forwarded as a malformed
                    // body field).
                    response_format: child
                        .get("provider_extra")
                        .and_then(|extra| extra.get("response_format"))
                        .or_else(|| child.get("response_format"))
                        .and_then(|value| serde_json::to_value(value).ok())
                        .filter(serde_json::Value::is_object),
                    kind: child
                        .get("kind")
                        .or_else(|| child.get("type"))
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned),
                    uri: child
                        .get("uri")
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned),
                    api_key: child
                        .get("api_key")
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned),
                    wire_api: child
                        .get("wire_api")
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned),
                    endpoint: child
                        .get("endpoint")
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned),
                    resource: child
                        .get("resource")
                        .or_else(|| child.get("azure_openai_resource"))
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned),
                    deployment: child
                        .get("deployment")
                        .or_else(|| child.get("azure_openai_deployment"))
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned),
                    requires_openai_auth: child
                        .get("requires_openai_auth")
                        .and_then(toml::Value::as_bool)
                        .unwrap_or(false),
                    fallback_models: toml_string_array(child, "fallback_models", &alias)?,
                    fallback: toml_string_array(child, "fallback", &alias)?,
                },
            );
        }
        collect_engine_models(child, &alias, out)?;
    }
    Ok(())
}

fn collect_engine_models_json(
    object: &serde_json::Map<String, serde_json::Value>,
    prefix: &str,
    out: &mut BTreeMap<String, EngineProviderModel>,
) -> anyhow::Result<()> {
    for (name, value) in object {
        let Some(child) = value.as_object() else {
            continue;
        };
        let alias = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if let Some(model) = child.get("model").and_then(serde_json::Value::as_str) {
            out.insert(
                alias.clone(),
                EngineProviderModel {
                    model: model.to_owned(),
                    chat_template_kwargs: child
                        .get("chat_template_kwargs")
                        .filter(|value| value.is_object())
                        .cloned(),
                    reasoning_effort: child
                        .get("provider_extra")
                        .and_then(|extra| extra.get("reasoning_effort"))
                        .or_else(|| child.get("reasoning_effort"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    response_format: child
                        .get("provider_extra")
                        .and_then(|extra| extra.get("response_format"))
                        .or_else(|| child.get("response_format"))
                        .filter(|value| value.is_object())
                        .cloned(),
                    kind: child
                        .get("kind")
                        .or_else(|| child.get("type"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    uri: child
                        .get("uri")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    api_key: child
                        .get("api_key")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    wire_api: child
                        .get("wire_api")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    endpoint: child
                        .get("endpoint")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    resource: child
                        .get("resource")
                        .or_else(|| child.get("azure_openai_resource"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    deployment: child
                        .get("deployment")
                        .or_else(|| child.get("azure_openai_deployment"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    requires_openai_auth: child
                        .get("requires_openai_auth")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    fallback_models: json_string_array(child, "fallback_models", &alias)?,
                    fallback: json_string_array(child, "fallback", &alias)?,
                },
            );
        }
        collect_engine_models_json(child, &alias, out)?;
    }
    Ok(())
}

fn toml_string_array(
    table: &toml::Table,
    field: &str,
    provider_alias: &str,
) -> anyhow::Result<Vec<String>> {
    let Some(value) = table.get(field) else {
        return Ok(Vec::new());
    };
    let values = value.as_array().ok_or_else(|| {
        anyhow::anyhow!(
            "engine provider {provider_alias:?} field {field:?} must be an array of strings"
        )
    })?;
    values
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                anyhow::anyhow!(
                    "engine provider {provider_alias:?} field {field:?} must contain only strings"
                )
            })
        })
        .collect()
}

fn json_string_array(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    provider_alias: &str,
) -> anyhow::Result<Vec<String>> {
    let Some(value) = object.get(field) else {
        return Ok(Vec::new());
    };
    let values = value.as_array().ok_or_else(|| {
        anyhow::anyhow!(
            "engine provider {provider_alias:?} field {field:?} must be an array of strings"
        )
    })?;
    values
        .iter()
        .map(|value| {
            value.as_str().map(str::to_owned).ok_or_else(|| {
                anyhow::anyhow!(
                    "engine provider {provider_alias:?} field {field:?} must contain only strings"
                )
            })
        })
        .collect()
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
    /// Overall provider request timeout in seconds. The timer includes time
    /// spent queued inside an upstream gateway before the model starts
    /// generating. CLI precedence: `--request-timeout` > this key >
    /// `ZODER_TIMEOUT_S` > provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_timeout_s: Option<u64>,
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
    /// Model entries contributed by `[providers.models.*]` blocks in overlay
    /// TOMLs. Maps a provider alias (e.g. `custom.reviewer`) to a model id
    /// (e.g. `"reviewer"`). Used by the oneshot router to resolve an agent's
    /// `model_provider` to an actual model id when no explicit `.model` pin
    /// is set on the agent.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, ModelEntry>,
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
    /// -- sh -c <cmd>`; selecting `backend = "linux_landlock"` on a Linux
    /// host applies an in-kernel Landlock ruleset via `pre_exec` (no
    /// external binary, requires Linux 5.13+). Either backend selected on
    /// the wrong host errors at runtime with a clear "unsupported on this
    /// platform" message (see
    /// `crates/zoder-cli/src/exec_safety::wrap_spawn_command`).
    #[serde(default)]
    pub exec_safety: ExecSafetyConfig,
    /// `[review]` block: size caps and generated-file exclusions for
    /// `zoder review` / `adversarial-review`. An absent block preserves the
    /// historical 120000-byte total cap and 9000-byte per-hunk cap and
    /// excludes nothing, so default behavior is unchanged.
    #[serde(default, skip_serializing_if = "ReviewConfig::is_empty")]
    pub review: ReviewConfig,
}

/// `[review]` block from `config.json` / an overlay TOML. Every field is
/// optional: an absent block preserves the historical review size caps and
/// excludes nothing. CLI flags (`--max-diff-bytes`, `--max-hunk-bytes`,
/// `--exclude`) take precedence over these values.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReviewConfig {
    /// Total diff byte cap for a single review pass. CLI: `--max-diff-bytes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_diff_bytes: Option<usize>,
    /// Per-hunk byte cap; an oversized hunk is split into ordered sub-hunks
    /// when splitting is enabled. CLI: `--max-hunk-bytes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hunk_bytes: Option<usize>,
    /// Globs excluded from the diff sent to reviewers (and from the size
    /// check), in addition to any `.zoderignore` at the repo root and any
    /// `--exclude` on the command line. CLI: `--exclude`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Per-reviewer-route size caps. The map key is a reviewer model id (the
    /// exact id as configured/served, e.g. `qwen38`, or its basename after the
    /// last `/` for namespaced ids such as
    /// `nvidia/nemotron-3-ultra-550b-a55b`). A route entry beats the general
    /// `max_diff_bytes` / `max_hunk_bytes` above; an explicit CLI flag beats
    /// both; a reviewer whose id has no entry falls back to the general value
    /// and then the historical 9000-byte default. See
    /// [`crate::config::RouteReviewDefaults`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub route_defaults: BTreeMap<String, RouteReviewDefaults>,
}

/// Per-reviewer size caps inside `[review.route_defaults.<model>]`. Both fields
/// are optional; an absent entry leaves the general `[review]` value (and then
/// the built-in default) in force for that reviewer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RouteReviewDefaults {
    /// Per-hunk / per-chunk byte cap for this reviewer. CLI `--max-hunk-bytes`
    /// still wins. Falls back to `[review].max_hunk_bytes` then 9000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_hunk_bytes: Option<usize>,
    /// Total-diff byte cap for this reviewer. CLI `--max-diff-bytes` still
    /// wins. Falls back to `[review].max_diff_bytes` then 120000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_diff_bytes: Option<usize>,
}

impl ReviewConfig {
    fn is_empty(&self) -> bool {
        self.max_diff_bytes.is_none()
            && self.max_hunk_bytes.is_none()
            && self.exclude.is_empty()
            && self.route_defaults.is_empty()
    }
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
    /// Provider id used by the zeroclaw engine for this agent. When set
    /// and `model` is `None`, the oneshot router follows this chain to
    /// resolve the model id — looking up `[providers.models.<provider_id>].model`
    /// from the agent-descriptor config surface. Without `model` set, the
    /// agent has no pin for the zoder oneshot router and `resolve_chain`
    /// will error out when `--agent` is specified under `--oneshot`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
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
// OS containment primitive (macOS seatbelt, Linux bubblewrap, Linux
// Landlock).
//
// Default = `ExecSandbox::None` = exactly the legacy "denylist only" behavior,
// byte-for-byte. Selecting `Seatbelt` on a host that is not running macOS, or
// `LinuxBubblewrap` / `LinuxLandlock` on a host that is not running Linux,
// is a HARD ERROR (`Err("…unsupported on this platform")`) at the call site,
// not a silent fallback — see
// `crates/zoder-cli/src/exec_safety::wrap_spawn_command` for the dispatch
// contract.
//
// We deliberately do NOT silently fall back to `None` on the wrong host — a
// half-implemented sandbox backend that pretends to contain is worse than
// none (see exec_safety module doc on the "half-working platform-specific
// code" failure mode).
// ---------------------------------------------------------------------------

/// Concrete OS-sandbox backend the loop should wrap `--check` execution in.
///
/// `serde` shape is intentionally a plain lowercase tag so the operator's
/// `config.json` / overlay TOML reads naturally (`"backend": "seatbelt"`).
/// Unknown variants deserialize to `Unsupported` via the `#[serde(other)]`
/// fallback below so a typo or a not-yet-implemented backend in a new build
/// does not silently select [`ExecSandbox::None`]. The config can still load,
/// but the dispatch site fails closed with a clear error before spawning the
/// command.
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
    /// Linux **Landlock** (kernel LSM, in-process `landlock_restrict_self`).
    /// Unlike [`ExecSandbox::LinuxBubblewrap`], Landlock is a *kernel*
    /// feature: there is no external wrapper binary to invoke. The crate
    /// applies the ruleset to the spawned child via a `pre_exec` hook on
    /// the [`std::process::Command`] (i.e. between `fork` and `exec`),
    /// so the ruleset attaches to the *child* before it ever runs the
    /// user's `sh -c <cmd>`. The two Linux backends are complementary:
    /// bubblewrap gives the operator a userspace wrapper with
    /// mount-namespace isolation, while Landlock is in-kernel and adds
    /// no new binary dependency, but requires a Linux >= 5.13 host.
    ///
    /// On any non-Linux platform the call site MUST reject this with a
    /// clear "unsupported on this platform" error rather than silently
    /// downgrading to `None` — the same cross-platform contract the
    /// bubblewrap variant establishes.
    LinuxLandlock,
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

/// Per-call-site knobs of the Linux Landlock ruleset. Mirrors
/// [`LinuxBubblewrapProfileOptions`] 1:1 so an operator who has a bubblewrap
/// profile block can copy the shape across to the in-kernel Landlock
/// backend without re-reading docs. The semantics are intentionally
/// equivalent (read-only system paths, read-write workdir, optional
/// read-only home, optional tmp); the only difference is the
/// *implementation* — Landlock is a kernel LSM (no external binary, no
/// mount-namespace manipulation) while bubblewrap is a userspace wrapper
/// binary that builds a new mount namespace.
///
/// `unshare_net` is intentionally absent from this profile: Landlock's
/// network-port scoping is ABI-gated (it was added in Landlock ABI v4,
/// Linux 6.7) and tying it to the same default-on/off knob as bubblewrap
/// would create either a silent network-policy gap or a hard runtime
/// incompatibility on older kernels. The in-kernel ruleset currently handles
/// filesystem rights only and uses hard compatibility: if the selected
/// filesystem policy cannot be fully enforced, the backend fails closed
/// before running the command. Operators who need network isolation should
/// use `linux_bubblewrap` or add a separate network-scoping knob in a future
/// iteration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LinuxLandlockProfileOptions {
    /// Mount `/tmp` (and `/var/tmp` as a symlink-fallback) read-write
    /// inside the ruleset. Default `true` because `cargo`, `pytest`,
    /// `node`, and almost every common `--check` writes intermediates
    /// there. Operators on hardened hosts can flip this off; the ruleset
    /// then omits the `/tmp` rules and any tool that needs scratch space
    /// will fail loudly.
    #[serde(default = "default_landlock_allow_tmp")]
    pub allow_tmp: bool,
    /// Read access to `/home` (read-only; no writes). Default `false`
    /// because most builds either inline everything they need under the
    /// working dir or fail loudly — granting read access to `/home`
    /// effectively exposes `~/.cargo`, `~/.npm`, etc. Operators who need
    /// those to be visible flip this to `true`; the ruleset then includes
    /// a read-only `/home` rule. Writes to `/home` remain denied because
    /// the rule is read-only.
    #[serde(default = "default_landlock_allow_home_read")]
    pub allow_home_read: bool,
}

fn default_landlock_allow_tmp() -> bool {
    true
}
fn default_landlock_allow_home_read() -> bool {
    false
}

impl Default for LinuxLandlockProfileOptions {
    fn default() -> Self {
        Self {
            allow_tmp: default_landlock_allow_tmp(),
            allow_home_read: default_landlock_allow_home_read(),
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
    /// Per-backend profile knobs for the in-kernel Linux Landlock
    /// ruleset. Consulted when `backend == ExecSandbox::LinuxLandlock`;
    /// ignored otherwise so an operator who flips the backend from
    /// `LinuxBubblewrap` to `LinuxLandlock` (both Linux) doesn't have to
    /// also move their profile options.
    #[serde(default)]
    pub linux_landlock: LinuxLandlockProfileOptions,
}

impl Config {
    /// Config directory: `$ZODER_HOME` or `~/.zoder`.
    ///
    /// Infallible for the many display / path-join call sites. When neither
    /// `ZODER_HOME` nor a real home directory can be resolved this falls back
    /// to the relative `.zoder` — which is exactly the silent-wrong-dir
    /// hazard. The loud check lives at the resolution entry point
    /// [`Config::resolve_home`], which [`Config::load`] uses; callers that
    /// need a trustworthy, absolute home should go through `resolve_home`.
    pub fn home() -> PathBuf {
        Self::resolve_home().unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".zoder"))
    }

    /// Resolve the config home from the real environment, failing LOUD when
    /// it cannot be trusted. This is the guard behind [`Config::load`]: an
    /// empty `ZODER_HOME`, or an absent home directory with `ZODER_HOME`
    /// unset, or any resolution that yields a non-absolute path (which would
    /// be silently resolved against the current working directory and split
    /// state across CWDs) is rejected with a clear error instead of loading
    /// config/ledger/health from the wrong place.
    pub fn resolve_home() -> anyhow::Result<PathBuf> {
        Self::resolve_home_from(std::env::var("ZODER_HOME").ok(), dirs::home_dir())
    }

    /// Pure, env-free core of [`Config::resolve_home`] so the resolution rules
    /// can be tested hermetically without mutating the process environment.
    fn resolve_home_from(
        zoder_home: Option<String>,
        home_dir: Option<PathBuf>,
    ) -> anyhow::Result<PathBuf> {
        let resolved = match zoder_home {
            Some(h) => {
                if h.is_empty() {
                    anyhow::bail!(
                        "ZODER_HOME is set but empty; unset it or set it to an \
                         absolute directory path"
                    );
                }
                PathBuf::from(h)
            }
            None => home_dir
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "cannot resolve home directory; set ZODER_HOME to an \
                         absolute directory path"
                    )
                })?
                .join(".zoder"),
        };
        if !resolved.is_absolute() {
            anyhow::bail!(
                "resolved zoder home {:?} is not an absolute path; set \
                 ZODER_HOME to an absolute directory path",
                resolved
            );
        }
        Ok(resolved)
    }

    /// Maximum trusted size of a single on-disk config file — the primary
    /// `config.json` OR any `config.<vendor>.toml` overlay. Larger files are
    /// rejected before the body is read: a tampered/oversized config can't be
    /// trusted to drive routing, and a FIFO or device at the configured path
    /// would otherwise block forever in `read_to_string` (or OOM the
    /// process), before any validation logic runs.
    pub const MAX_CONFIG_BYTES: u64 = 2_097_152; // 2 MiB — mirrors pricing.rs

    /// Load from $ZODER_HOME/config.json (if present, else sensible free-tier
    /// default) and then layer every `config.<vendor>.toml` in the same
    /// directory on top. See module docs for the layered-config model.
    pub fn load() -> anyhow::Result<Self> {
        let cfg = Self::load_unvalidated()?;
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

    /// Read and merge the on-disk configuration WITHOUT running
    /// [`Config::validate`]. Unlike [`Config::load`], a config with
    /// validation problems (duplicate provider ids, empty providers, an
    /// unresolved default_provider, …) is returned successfully so the caller
    /// can decide how to surface those problems (e.g. `configure` prints them
    /// as a problem list and picks the exit code itself).
    ///
    /// A genuinely *unreadable* config — a missing `$ZODER_HOME`, an I/O error,
    /// or a `config.json` that is not valid JSON (a trailing comma, etc.) —
    /// still returns `Err`, with a clean message the caller can render as a
    /// single config problem rather than a raw backtrace.
    ///
    /// Other callers should keep using [`Config::load`], which validates.
    pub fn load_unvalidated() -> anyhow::Result<Self> {
        let home = Self::resolve_home()?;
        Self::load_unvalidated_from(&home)
    }

    /// [`Config::load_unvalidated`] against an explicit home directory, so
    /// tests can exercise the read/parse/overlay behavior against a temp dir
    /// without touching the process-global `ZODER_HOME` env var (which would
    /// race other tests). Still does NOT run `validate`.
    pub fn load_unvalidated_from(home: &std::path::Path) -> anyhow::Result<Self> {
        let mut cfg = if home.join("config.json").exists() {
            let path = home.join("config.json");
            // Bounded, regular-file-only read: see [`read_bounded_regular_file`].
            // A FIFO at the configured path would otherwise block forever in
            // `read_to_string` (before any validation runs), and an oversized
            // file could OOM the process on every config load.
            let raw = read_bounded_regular_file(&path, Config::MAX_CONFIG_BYTES)?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing zoder config at {}", path.display()))?
        } else {
            Self::default_provider(home)
        };
        apply_overlays(&mut cfg, home)?;
        Ok(cfg)
    }

    /// Resolve and validate zoder-to-Zeroclaw provider-profile mappings once
    /// while the two configuration files are loaded.
    ///
    /// Existing zoder configurations commonly use a short provider id such as
    /// `minimax`, while Zeroclaw names the same profile `custom.minimax`. When
    /// [`Provider::engine_provider_ref`] is absent, an exact reference match
    /// wins; otherwise a single matching profile alias is adopted. If multiple
    /// profiles share that alias, an explicit endpoint and compatible kind may
    /// disambiguate them. A remaining ambiguity is a load-time error requiring
    /// `engine_provider_ref`, never a per-turn guess.
    ///
    /// Providers with no name-related engine profile are left unmapped because
    /// they may be valid oneshot/Goose-only routes. A Zeroclaw dispatch through
    /// such a provider still fails closed and asks for an explicit mapping.
    pub fn bind_engine_provider_refs(
        &mut self,
        registry: &EngineModelRegistry,
    ) -> anyhow::Result<()> {
        if registry.models.is_empty() {
            return Ok(());
        }

        let mut claimed = BTreeMap::<String, String>::new();
        for provider in &mut self.providers {
            if provider.base_url.contains(PLACEHOLDER_PROVIDER_HOST) {
                continue;
            }
            let explicit_ref = provider.engine_provider_ref.as_deref();
            let selected = if let Some(provider_ref) = explicit_ref {
                Some(
                    registry
                        .provider_route_for_ref(provider_ref)?
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                        "zoder provider {:?} maps engine_provider_ref {:?}, but that profile is \
                         absent from the loaded Zeroclaw configuration",
                        provider.id,
                        provider_ref
                    )
                        })?,
                )
            } else if let Some(exact) = registry.provider_route_for_ref(&provider.id)? {
                Some(exact)
            } else {
                let named = registry.provider_routes_for_alias(&provider.id)?;
                match named.as_slice() {
                    [] => None,
                    [only] => Some(only.clone()),
                    _ => {
                        let compatible = named
                            .iter()
                            .filter(|route| {
                                provider.engine_kind_is_compatible(&route.effective_kind)
                                    && provider.engine_endpoint_is_compatible(
                                        &route.effective_uri,
                                        &route.implementation,
                                    )
                            })
                            .collect::<Vec<_>>();
                        match compatible.as_slice() {
                            [only] => Some((*only).clone()),
                            [] => {
                                anyhow::bail!(
                                    "zoder provider {:?} matches multiple Zeroclaw profiles by \
                                     alias ({}) but none matches its endpoint/kind; set \
                                     engine_provider_ref explicitly",
                                    provider.id,
                                    named
                                        .iter()
                                        .map(|route| route.provider_ref.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                )
                            }
                            _ => {
                                anyhow::bail!(
                                    "zoder provider {:?} ambiguously matches multiple Zeroclaw \
                                     profiles ({}); set engine_provider_ref explicitly",
                                    provider.id,
                                    compatible
                                        .iter()
                                        .map(|route| route.provider_ref.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                )
                            }
                        }
                    }
                }
            };

            let Some(route) = selected else {
                continue;
            };
            if !provider.engine_kind_is_compatible(&route.effective_kind) {
                anyhow::bail!(
                    "zoder provider {:?} (kind {:?}) maps Zeroclaw profile {:?}, but its \
                     effective transport {:?} is incompatible",
                    provider.id,
                    provider.kind,
                    route.provider_ref,
                    route.effective_kind
                );
            }
            if !provider.engine_endpoint_is_compatible(&route.effective_uri, &route.implementation)
            {
                anyhow::bail!(
                    "zoder provider {:?} (endpoint {:?}) maps Zeroclaw profile {:?}, but its \
                     effective endpoint {:?} differs",
                    provider.id,
                    provider.base_url,
                    route.provider_ref,
                    route.effective_uri
                );
            }
            if let Some(previous) = claimed.insert(route.provider_ref.clone(), provider.id.clone())
            {
                anyhow::bail!(
                    "zoder providers {previous:?} and {:?} both map Zeroclaw profile {:?}; each \
                     engine billing boundary must map to exactly one zoder provider",
                    provider.id,
                    route.provider_ref
                );
            }
            provider.engine_provider_ref = Some(route.provider_ref);
        }

        // An engine provider profile is a complete dispatch target even when
        // the operator intentionally omitted it from zoder's router-level
        // fallback array. Materialize each profile selected by an agent so an
        // explicit `--agent` pin can retain its provider identity and still
        // pass through zoder's normal provider gate. Existing zoder providers
        // always win: synthesis is only for otherwise-unrepresented routes.
        let agent_provider_refs = registry
            .agents
            .values()
            .filter_map(|agent| agent.model_provider.as_deref())
            .collect::<std::collections::BTreeSet<_>>();
        for provider_ref in agent_provider_refs {
            if self.providers.iter().any(|provider| {
                provider.id == provider_ref
                    || provider.engine_provider_ref.as_deref() == Some(provider_ref)
            }) {
                continue;
            }
            let Some(engine_provider) = registry.models.get(provider_ref) else {
                // Preserve the existing fail-closed execution diagnostic for
                // a genuinely dangling agent model_provider reference.
                continue;
            };
            let Some(route) = registry.provider_route_for_ref(provider_ref)? else {
                continue;
            };
            let auth = engine_provider
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map_or(Auth::None, |token| Auth::Bearer {
                    token: token.to_owned(),
                });
            self.providers.push(Provider {
                id: provider_ref.to_owned(),
                engine_provider_ref: Some(provider_ref.to_owned()),
                base_url: route.effective_uri,
                kind: route.effective_kind,
                auth,
                paid: false,
                billing: BillingMode::default(),
                subscription: None,
                serves: vec![engine_provider.model.clone()],
                azure_api_version: None,
            });
        }
        Ok(())
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
                engine_provider_ref: None,
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
            request_timeout_s: None,
            vendor_provenance: BTreeMap::new(),
            theme: Theme::default(),
            primary_model: None,
            reviewer_model: None,
            agents: BTreeMap::new(),
            models: BTreeMap::new(),
            budget: crate::budget::Budget::default(),
            routing: RoutingConfig::default(),
            exec_safety: ExecSafetyConfig::default(),
            review: ReviewConfig::default(),
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
    /// Simple prefix-matching lookup: returns the first provider whose
    /// `serves` list explicitly claims `model_id`. Returns `None` when no
    /// provider serves the model — the caller must decide what to do
    /// (error, route via the default, etc.). This is the "real provider"
    /// lookup; it does NOT fall through to `default_provider` for unmatched
    /// models.
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

    /// Full quota-aware ranking for routing. Returns a provider that explicitly
    /// claims the model via one of its `serves` prefix entries. Returns `None`
    /// when no provider serves the model — callers must handle this instead of
    /// silently falling through to the default provider.
    ///
    /// This is a "real provider" lookup: the default provider is NOT returned as
    /// a fallback for unmatched models. A model must be explicitly claimed by at
    /// least one provider's `serves` list to be routed.
    pub fn best_provider_for_model(
        &self,
        model_id: &str,
        entries: &[crate::ledger::Entry],
        catalog: &crate::subscription_tiers::TierCatalog,
    ) -> Option<&Provider> {
        let candidates = self.ranked_providers_for_model(model_id, entries, catalog);
        if candidates.is_empty() {
            // No provider claims this model's prefix via its `serves` list.
            // Do NOT fall through to `default_provider` — that would silently
            // route the model to a provider that never declared it, causing
            // cryptic 404s or silent misrouting when a different provider
            // happens to share the same model id.
            return None;
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
    /// Does ANY provider's `serves` claim this model id?
    ///
    /// Distinct from [`model_has_real_provider`], which additionally requires
    /// the provider not to be the placeholder. Config validation wants this
    /// looser question; routing wants the stricter one.
    pub fn model_is_claimed_by_a_provider(&self, model_id: &str) -> bool {
        self.provider_for_model(model_id).is_some()
    }

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

    /// Resolve the PRIMARY model id for `--agent <alias>` through the
    /// `model_provider` chain. When the agent has a `model_provider` set
    /// (e.g. `custom.reviewer`) but no explicit `.model` pin, this method
    /// looks up the model id from `Config::models` keyed by the provider
    /// alias. Returns `None` when the agent has no `model_provider` set or
    /// when the provider alias is not found in the models registry.
    ///
    /// This enables `--agent <alias> --oneshot` to resolve correctly even
    /// when the agent's model is defined via `model_provider` rather than
    /// an explicit `.model` pin — mirroring the zeroclaw engine's path
    /// where `model_provider` is the authoritative model selector.
    pub fn resolve_model_for_agent(&self, alias: Option<&str>) -> Option<String> {
        let alias = alias?;
        let agent = self.agents.get(alias)?;
        let mp = agent.model_provider.as_deref()?;
        self.models.get(mp).and_then(|m| m.model.clone())
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
        let mut seen_engine_provider_refs = BTreeMap::<&str, &str>::new();
        for p in &self.providers {
            if p.id.trim().is_empty() {
                errs.push("a provider has an empty id".into());
            } else if !seen.insert(p.id.clone()) {
                errs.push(format!("duplicate provider id: {}", p.id));
            }
            if let Some(provider_ref) = p.engine_provider_ref.as_deref() {
                if provider_ref.trim().is_empty() {
                    errs.push(format!(
                        "provider {}: engine_provider_ref must not be empty",
                        p.id
                    ));
                } else if let Some(previous) =
                    seen_engine_provider_refs.insert(provider_ref, p.id.as_str())
                {
                    errs.push(format!(
                        "providers {previous} and {} both declare engine_provider_ref \
                         {provider_ref:?}",
                        p.id
                    ));
                }
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
        if self.request_timeout_s.is_some_and(|secs| secs == 0) {
            errs.push("request_timeout_s must be greater than zero".into());
        }
        for (name, cap) in [
            ("max_cost_per_call_usd", self.budget.max_cost_per_call_usd),
            ("monthly_cap_usd", self.budget.monthly_cap_usd),
        ] {
            if cap.is_some_and(|v| !v.is_finite() || v < 0.0) {
                errs.push(format!("budget.{name} must be finite and non-negative"));
            }
        }

        // Validate that model references are CLAIMED by some provider's `serves`.
        //
        // Deliberately `provider_for_model`, not `real_provider_for_model`. The
        // "real" variant also rejects providers whose base_url is the
        // api.example.com placeholder, which is a ROUTING concern, not a config
        // -validity one: a freshly-installed config has exactly one placeholder
        // provider, so validating against it made `zoder config --validate` fail
        // on a default install and on every legacy default-provider config.
        //
        // Routing already refuses a placeholder at call time with a precise,
        // actionable message ("no real provider is configured for model X ...
        // configure a provider that serves it"). That is the right place for it:
        // it fires when the model is actually needed, not when an unrelated
        // command loads the file.
        //
        // What #17 asked for is that an unservable id ERROR instead of silently
        // resolving to an arbitrary provider. Claim-checking here gives that,
        // without making a fresh install invalid.
        if let Some(ref model) = self.primary_model {
            if !self.model_is_claimed_by_a_provider(model) {
                errs.push(format!(
                    "[profile].primary_model {:?} is not served by any provider; add a \
                     [[providers]] entry with `serves` matching it",
                    model
                ));
            }
        }
        if let Some(ref model) = self.reviewer_model {
            if !self.model_is_claimed_by_a_provider(model) {
                errs.push(format!(
                    "[profile].reviewer_model {:?} is not served by any provider; add a \
                     [[providers]] entry with `serves` matching it",
                    model
                ));
            }
        }
        for (alias, agent) in &self.agents {
            if let Some(ref model) = agent.model {
                if !self.model_is_claimed_by_a_provider(model) {
                    errs.push(format!(
                        "[agents.{}].model {:?} is not served by any provider; add a \
                         [[providers]] entry with `serves` matching it",
                        alias, model
                    ));
                }
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
    /// Overall provider request timeout in seconds for this profile. Same
    /// default-claimer/last-defined merge semantics as reviewer_model.
    #[serde(default)]
    pub request_timeout_s: Option<u64>,
}

/// Read a regular file with a bounded byte cap, mirroring the guard used by
/// `PricingCatalog::load` in `crates/zoder-core/src/pricing.rs`. Used for the
/// primary `config.json` AND every `config.<vendor>.toml` overlay so a FIFO,
/// device, symlink-to-non-regular-file, or unexpectedly huge file at the
/// configured path is rejected BEFORE the body is read.
///
/// Without this guard, `read_to_string` would block forever on a FIFO (or
/// OOM the process on a huge file), before any validation logic runs — the
/// original defect reported at the config read sites (the primary read at
/// `Config::load_unvalidated_from` and the overlay read in
/// `collect_overlays`).
///
/// ## TOCTOU-safety
///
/// Validation is performed against the **open file descriptor**, not a
/// re-stat of the path. The previous implementation called
/// `std::fs::metadata(path)` and then `std::fs::read_to_string(path)` —
/// those are two separate `path` lookups, and an attacker able to replace
/// the file between them (rename a regular file away and `mkfifo` in its
/// place, or `truncate --size=10G` an existing path) could defeat the
/// guard.
///
/// This implementation instead:
///   1. Opens the file *once* — on Unix with `O_CLOEXEC | O_NOFOLLOW |
///      O_NONBLOCK`. `O_NOFOLLOW` rejects a symlink at the path at open
///      time rather than following it into a FIFO; `O_NONBLOCK` makes a
///      `open()` on a writer-less FIFO return `ENXIO` immediately
///      instead of blocking forever inside the kernel waiting for a
///      writer that will never arrive. (`O_NONBLOCK` is semantically a
///      no-op on regular files — read returns data immediately on
///      POSIX — so the flag combination does not change happy-path
///      behavior; it only changes the failure mode for non-regular
///      targets from "block forever" to "return ENXIO".)
///   2. Calls `File::metadata()` on the open descriptor (i.e. `fstat(fd)`)
///      to confirm the inode is a regular file and within `max_bytes`,
///   3. Reads from the descriptor via `Read::take(max_bytes)` so even if
///      the file is grown after step 2 we never consume more than the cap.
///
/// Once we hold an FD referencing a specific inode, `unlink(path)` and a
/// subsequent `mkfifo path` from another process cannot affect us: our
/// FD still points at the original inode, and the new FIFO is a different
/// inode that no FD we hold references.
fn read_bounded_regular_file(path: &Path, max_bytes: u64) -> anyhow::Result<String> {
    use std::io::Read;

    // Step 1: open on Unix with O_NOFOLLOW (symlink rejection) plus
    // O_NONBLOCK (fail-fast on writer-less FIFOs). O_CLOEXEC keeps
    // the FD from leaking into any child process spawned
    // post-config-load. See the function-level doc for the full
    // rationale.
    let f = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(CONFIG_OPEN_FLAGS)
                .open(path)
        }
        #[cfg(not(unix))]
        {
            std::fs::File::open(path)
        }
    }
    .with_context(|| format!("opening zoder config at {}", path.display()))?;

    // Step 2: validate on the open descriptor (fstat). If the path was
    // replaced between this call and the open, the FD we hold still
    // references the inode we opened — and that inode is what fstat
    // inspects. The path itself is irrelevant from here on.
    let meta = f
        .metadata()
        .with_context(|| format!("fstat zoder config at {}", path.display()))?;
    if !meta.is_file() {
        anyhow::bail!(
            "zoder config {} is not a regular file (FIFOs, devices, and \
             symlinks to non-regular files are rejected before the read to \
             avoid blocking or OOMing the process on every config load)",
            path.display()
        );
    }
    if meta.len() > max_bytes {
        anyhow::bail!(
            "zoder config {} rejected — {} bytes exceeds {} byte limit",
            path.display(),
            meta.len(),
            max_bytes
        );
    }

    // Step 3: bounded read from the open FD. Even if a concurrent writer
    // grows the file past the cap after our fstat, Read::take caps the
    // bytes we will pull from THIS fd to `max_bytes`, and `read_to_string`
    // fails (not silently truncates) if the cap is actually hit. So a
    // racing growth cannot OOM us, and a racing shrink-then-give-different-
    // content cannot bypass the cap.
    let mut s = String::new();
    f.take(max_bytes)
        .read_to_string(&mut s)
        .with_context(|| format!("reading zoder config at {}", path.display()))?;
    Ok(s)
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
    // Request timeout follows the same profile merge shape as the pinned
    // model defaults: the default-claiming overlay wins, else the
    // alphabetically-last overlay that defines it.
    let mut default_request_timeout_s: Option<u64> = None;
    let mut fallback_request_timeout_s: Option<u64> = None;

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
        if overlay.profile.request_timeout_s.is_some() {
            fallback_request_timeout_s = overlay.profile.request_timeout_s;
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
            if overlay.profile.request_timeout_s.is_some() {
                default_request_timeout_s = overlay.profile.request_timeout_s;
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
    if let Some(request_timeout_s) = default_request_timeout_s.or(fallback_request_timeout_s) {
        cfg.request_timeout_s = Some(request_timeout_s);
    }
    Ok(())
}

fn collect_overlays(
    home: &Path,
    only_vendor: Option<&str>,
) -> anyhow::Result<Vec<(String, VendorOverlay)>> {
    // Distinguish the legitimate "no overlay directory yet" case (`NotFound` —
    // normal first run before `ZODER_HOME` has been created, or a fresh
    // install) from any OTHER `read_dir` error (permission denied, the path
    // is unexpectedly a file/symlink-to-file, I/O error, etc.). Silently
    // collapsing the latter into "no overlays" is dangerous: a
    // `config.<vendor>.toml` may exist on disk, but if `read_dir` fails for
    // a real reason we would skip it and proceed with the wrong
    // (non-overlaid) provider set — quietly wrong routing with no warning.
    let rd = match std::fs::read_dir(home) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(anyhow::Error::new(e).context(format!(
                "enumerating vendor overlays under {}",
                home.display()
            )));
        }
    };
    let mut entries: Vec<(String, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            // Top-level vendor overlay: `config.<vendor>.toml`.
            // Also accept exactly `config.toml` (the root-level
            // overlay — engine-specific config, distinct from the
            // JSON `config.json`).  Reject `config.foo.toml.bak`
            // (wrong suffix), and `config.foo.bar.toml` (vendor
            // stem contains a dot — that's a sub-overlay, not a
            // top-level vendor).
            let rest = name.strip_prefix("config.")?;
            // `config.toml` is deliberately NOT a zoder vendor overlay. That
            // file belongs to the zeroclaw/openclaw ENGINE and uses a different
            // schema (`schema_version`, `[agents.*]` with `model_provider`,
            // `identity.format`). Loading it here made zoder parse a config that
            // was never its own and reject it with
            //   unknown field `schema_version`, expected one of
            //   `profile`, `providers`, `theme`
            // which stops the binary from starting at all on a real host.
            //
            // Two config surfaces with overlapping names is already the root of
            // several routing bugs (ncz-os/zoder#15/#16/#17); the fix is to keep
            // them separate, not to teach one to read the other.
            let stem = rest.strip_suffix(".toml")?;
            if stem.is_empty() || stem.contains('.') {
                return None;
            }
            if stem.contains('.') || stem.is_empty() {
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
        // Same bounded, regular-file-only guard as the primary config read:
        // a FIFO or an oversized overlay would otherwise block or OOM the
        // process on every config load, before any validation runs.
        let raw = read_bounded_regular_file(&path, Config::MAX_CONFIG_BYTES)?;
        // Unknown keys WARN, they do not abort. The structs carry
        // `deny_unknown_fields` so a typo is caught rather than silently
        // ignored -- that is the point of ncz-os/zoder#15 -- but a strict
        // parse that refuses to start is the wrong end of the trade: an
        // operator with one stale key in one overlay loses the whole tool,
        // including the `zoder config` command they would use to find it.
        //
        // So: report the key, drop that overlay, and keep going. Errors are
        // reserved for `--strict` / `zoder config --validate`, where the
        // caller has asked to be blocked.
        let overlay: VendorOverlay = match toml::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[zoder] warning: ignoring overlay {}: {e}", path.display());
                continue;
            }
        };
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

    #[test]
    fn reviewer_template_kwargs_come_from_selected_engine_profile() {
        let registry = EngineModelRegistry::from_toml(
            r#"
[providers.models.custom.other]
model = "model-a"
chat_template_kwargs = { enable_thinking = true }
[providers.models.custom.reviewer]
model = "model-a"
chat_template_kwargs = { enable_thinking = false }
[agents.reviewer]
model_provider = "custom.reviewer"
"#,
        )
        .unwrap();
        assert_eq!(
            registry.chat_template_kwargs_for_model("model-a", None),
            Some(&serde_json::json!({"enable_thinking": false}))
        );
        assert_eq!(
            registry.chat_template_kwargs_for_model("different", None),
            None
        );
    }

    #[test]
    fn reviewer_reasoning_effort_comes_from_selected_engine_profile() {
        let registry = EngineModelRegistry::from_toml(
            r#"
[providers.models.custom.other]
model = "model-a"
reasoning_effort = "high"
[providers.models.custom.reviewer]
model = "model-a"
provider_extra = { reasoning_effort = "none" }
[providers.models.custom.flash]
model = "flash-b"
reasoning_effort = "minimal"
[agents.reviewer]
model_provider = "custom.reviewer"
"#,
        )
        .unwrap();
        assert_eq!(
            registry.reasoning_effort_for_model("model-a", None),
            Some("none")
        );
        assert_eq!(
            registry.reasoning_effort_for_model("flash-b", None),
            Some("minimal")
        );
        assert_eq!(registry.reasoning_effort_for_model("different", None), None);
    }

    #[test]
    fn provider_extra_reasoning_effort_wins_over_top_level() {
        let registry = EngineModelRegistry::from_toml(
            r#"
[providers.models.custom.both]
model = "model-c"
reasoning_effort = "high"
provider_extra = { reasoning_effort = "none" }
"#,
        )
        .unwrap();
        assert_eq!(
            registry.reasoning_effort_for_model("model-c", None),
            Some("none")
        );
    }

    #[test]
    fn reviewer_response_format_comes_from_selected_engine_profile() {
        let registry = EngineModelRegistry::from_toml(
            r#"
[providers.models.custom.other]
model = "model-a"
response_format = { type = "text" }
[providers.models.custom.reviewer]
model = "model-a"
provider_extra = { response_format = { type = "json_object" } }
[agents.reviewer]
model_provider = "custom.reviewer"
"#,
        )
        .unwrap();
        // The reviewer alias wins over the other profile serving the same model.
        assert_eq!(
            registry.response_format_for_model("model-a", None),
            Some(&serde_json::json!({"type": "json_object"}))
        );
        assert_eq!(registry.response_format_for_model("different", None), None);
    }

    #[test]
    fn provider_extra_response_format_wins_over_top_level_and_rejects_non_objects() {
        let registry = EngineModelRegistry::from_toml(
            r#"
[providers.models.custom.both]
model = "model-c"
response_format = { type = "text" }
provider_extra = { response_format = { type = "json_object" } }
[providers.models.custom.bogus]
model = "model-d"
response_format = "not-an-object"
"#,
        )
        .unwrap();
        assert_eq!(
            registry.response_format_for_model("model-c", None),
            Some(&serde_json::json!({"type": "json_object"}))
        );
        assert_eq!(registry.response_format_for_model("model-d", None), None);
    }
    use crate::ledger::Entry;
    use crate::subscription_tiers::TierCatalog;
    use chrono::{Duration, Utc};

    #[test]
    fn engine_model_registry_resolves_real_agent_provider_shape() {
        let registry = EngineModelRegistry::from_toml(
            r#"
schema_version = 1

[providers.models.custom.reviewer]
type = "openai-compatible"
model = "nvidia/nvidia/nemotron-3-super-v3"
uri = "https://example.invalid/v1"

[agents.reviewer]
model_provider = "custom.reviewer"

[agents.reviewer.identity]
name = "ignored by the routing projection"
"#,
        )
        .unwrap();

        assert_eq!(
            registry.model_for_agent("reviewer"),
            Some("nvidia/nvidia/nemotron-3-super-v3")
        );
        assert_eq!(
            registry
                .agent_for_model("nvidia/nvidia/nemotron-3-super-v3")
                .unwrap(),
            Some("reviewer")
        );
        assert_eq!(
            registry
                .agent_for_model("NVIDIA/NVIDIA/NEMOTRON-3-SUPER-V3")
                .unwrap(),
            None,
            "model ids must match exactly rather than case-insensitively"
        );
        assert_eq!(
            registry.provider_route_for_agent("reviewer").unwrap(),
            Some(EngineProviderRoute {
                provider_ref: "custom.reviewer".into(),
                provider_type: "custom".into(),
                provider_alias: "reviewer".into(),
                implementation: "openai-compatible".into(),
                effective_kind: "openai-chat".into(),
                effective_uri: "https://example.invalid/v1".into(),
            })
        );
    }

    #[test]
    fn engine_only_agent_provider_is_synthesized_from_real_config_shape() {
        let registry = EngineModelRegistry::from_toml(
            r#"
[providers.models.custom.groq]
api_key = "gsk_test_only"
uri = "https://api.groq.com/openai/v1"
model = "openai/gpt-oss-120b"
native_tools = true

[agents.groq]
enabled = true
risk_profile = "default"
runtime_profile = "zoder_coder"
model_provider = "custom.groq"
[agents.groq.identity]
format = "openclaw"
[agents.groq.memory]
backend = "sqlite"
[agents.groq.workspace]
unrestricted_filesystem = true
"#,
        )
        .unwrap();
        let mut cfg = Config::default_provider(Path::new("/tmp/zoder-test"));
        cfg.providers.clear();

        cfg.bind_engine_provider_refs(&registry).unwrap();

        let provider = cfg
            .provider("custom.groq")
            .expect("the engine-only provider must be materialized");
        assert_eq!(provider.engine_provider_ref.as_deref(), Some("custom.groq"));
        assert_eq!(provider.base_url, "https://api.groq.com/openai/v1");
        assert_eq!(provider.kind, "openai-chat");
        assert_eq!(provider.serves, ["openai/gpt-oss-120b"]);
        assert!(matches!(
            &provider.auth,
            Auth::Bearer { token } if token == "gsk_test_only"
        ));
    }

    #[test]
    fn dangling_engine_agent_provider_is_not_invented() {
        let registry = EngineModelRegistry::from_toml(
            r#"
[agents.broken]
model_provider = "custom.nonexistent"
"#,
        )
        .unwrap();
        let mut cfg = Config::default_provider(Path::new("/tmp/zoder-test"));
        cfg.providers.clear();

        cfg.bind_engine_provider_refs(&registry).unwrap();

        assert!(cfg.provider("custom.nonexistent").is_none());
        let error = registry
            .provider_route_for_agent("broken")
            .expect_err("the dangling provider must still fail loudly")
            .to_string();
        assert!(error.contains("is not configured"), "{error}");
    }

    #[test]
    fn engine_model_registry_prefers_direct_agent_model() {
        let registry = EngineModelRegistry::from_toml(
            r#"
[providers.models.custom.old]
model = "old-model"
fallback_models = ["provider-backup"]

[agents.codex]
model_provider = "custom.old"
model = "gpt-5.5"
"#,
        )
        .unwrap();

        assert_eq!(registry.model_for_agent("codex"), Some("gpt-5.5"));
        assert_eq!(registry.agent_for_model("gpt-5.5").unwrap(), Some("codex"));
        assert_eq!(registry.agent_for_model("old-model").unwrap(), None);
        assert_eq!(
            registry
                .model_candidates_for_agent("codex")
                .unwrap()
                .unwrap(),
            vec!["gpt-5.5", "provider-backup"],
            "the direct model replaces only the provider primary, not its fallback closure"
        );
    }

    #[test]
    fn engine_model_registry_rejects_ambiguous_model_agents() {
        let registry = EngineModelRegistry::from_toml(
            r#"
[providers.models.custom.shared]
model = "shared-model"

[agents.author]
model_provider = "custom.shared"

[agents.reviewer]
model_provider = "custom.shared"
"#,
        )
        .unwrap();

        let err = registry
            .agent_for_model("shared-model")
            .expect_err("duplicate routes must not select the first BTreeMap key");
        let message = err.to_string();
        assert!(message.contains("author"), "{message}");
        assert!(message.contains("reviewer"), "{message}");
        assert!(message.contains("--agent"), "{message}");
        assert_eq!(
            registry
                .agent_for_model_with_preference("shared-model", Some("reviewer"))
                .unwrap(),
            Some("reviewer")
        );
    }

    #[test]
    fn engine_model_registry_default_agent_resolves_ambiguous_model() {
        let registry = EngineModelRegistry::from_toml(
            r#"
[acp]
default_agent = "author"

[agents.author]
model = "shared-model"

[agents.reviewer]
model = "shared-model"
"#,
        )
        .unwrap();

        assert_eq!(
            registry.agent_for_model("shared-model").unwrap(),
            Some("author")
        );
    }

    #[test]
    fn engine_model_registry_parses_live_config_json() {
        let registry = EngineModelRegistry::from_json(&serde_json::json!({
            "acp": { "default_agent": "author" },
            "providers": {
                "models": {
                    "custom": {
                        "author": {
                            "kind": "openai-compatible",
                            "uri": "https://live.example/v1",
                            "model": "live-model"
                        }
                    }
                }
            },
            "agents": {
                "author": { "model_provider": "custom.author" }
            }
        }))
        .unwrap();

        assert_eq!(registry.model_for_agent("author"), Some("live-model"));
        assert_eq!(
            registry.agent_for_model("live-model").unwrap(),
            Some("author")
        );
        assert_eq!(
            registry.provider_route_for_agent("author").unwrap(),
            Some(EngineProviderRoute {
                provider_ref: "custom.author".into(),
                provider_type: "custom".into(),
                provider_alias: "author".into(),
                implementation: "openai-compatible".into(),
                effective_kind: "openai-chat".into(),
                effective_uri: "https://live.example/v1".into(),
            })
        );
    }

    #[test]
    fn engine_provider_binding_supports_non_dotted_builtin_reference() {
        let registry = EngineModelRegistry::from_toml(
            r#"
[providers.models.minimax]
model = "MiniMax-M3"

[agents.author]
model_provider = "minimax"
"#,
        )
        .unwrap();
        assert_eq!(
            registry.provider_route_for_agent("author").unwrap(),
            Some(EngineProviderRoute {
                provider_ref: "minimax".into(),
                provider_type: "minimax".into(),
                provider_alias: "minimax".into(),
                implementation: "minimax".into(),
                effective_kind: "openai-chat".into(),
                effective_uri: "https://api.minimax.io/v1".into(),
            })
        );

        let mut cfg = Config::default_provider(Path::new("/tmp/zoder-test"));
        cfg.providers[0].id = "minimax".into();
        cfg.providers[0].base_url = "https://api.minimax.io/v1".into();
        cfg.bind_engine_provider_refs(&registry).unwrap();
        assert_eq!(
            cfg.providers[0].engine_provider_ref.as_deref(),
            Some("minimax")
        );
    }

    #[test]
    fn ambiguous_short_engine_provider_ref_requires_explicit_mapping_at_load() {
        let registry = EngineModelRegistry::from_json(&serde_json::json!({
            "providers": {
                "models": {
                    "custom": {
                        "subscription": {
                            "kind": "openai-chat",
                            "uri": "https://shared.example/v1",
                            "model": "shared-model"
                        }
                    },
                    "other": {
                        "subscription": {
                            "kind": "openai-chat",
                            "uri": "https://shared.example/v1",
                            "model": "shared-model"
                        }
                    }
                }
            }
        }))
        .unwrap();
        let mut cfg = Config::default_provider(Path::new("/tmp/zoder-test"));
        cfg.providers[0].id = "subscription".into();
        cfg.providers[0].base_url = "https://shared.example/v1".into();

        let error = cfg
            .bind_engine_provider_refs(&registry)
            .expect_err("same-suffix profiles must not be selected arbitrarily")
            .to_string();
        assert!(error.contains("custom.subscription"), "{error}");
        assert!(error.contains("other.subscription"), "{error}");
        assert!(error.contains("engine_provider_ref explicitly"), "{error}");

        cfg.providers[0].engine_provider_ref = Some("custom.subscription".into());
        cfg.bind_engine_provider_refs(&registry)
            .expect("the dedicated mapping must resolve the ambiguity");
        assert_eq!(
            cfg.providers[0].engine_provider_ref.as_deref(),
            Some("custom.subscription")
        );
    }

    #[test]
    fn custom_engine_provider_requires_endpoint_at_mapping_load_time() {
        let registry = EngineModelRegistry::from_json(&serde_json::json!({
            "providers": {
                "models": {
                    "custom": {
                        "minimax": { "model": "MiniMax-M3" }
                    }
                }
            }
        }))
        .unwrap();
        let mut cfg = Config::default_provider(std::path::Path::new("/tmp/zoder-test"));
        cfg.providers[0].id = "minimax".into();
        cfg.providers[0].base_url = "https://api.minimax.io/v1".into();

        let error = cfg
            .bind_engine_provider_refs(&registry)
            .expect_err("a custom profile without uri cannot be cross-validated")
            .to_string();
        assert!(error.contains("custom.minimax"), "{error}");
        assert!(error.contains("no explicit endpoint"), "{error}");
    }

    #[test]
    fn engine_model_registry_projects_full_provider_fallback_closure() {
        let registry = EngineModelRegistry::from_toml(
            r#"
[providers.models.custom.primary]
model = "safe-primary"
fallback_models = ["same-provider-backup"]
fallback = ["custom.secondary"]

[providers.models.custom.secondary]
model = "paid-secondary"
fallback_models = ["secondary-backup"]
fallback = ["custom.tertiary"]

[providers.models.custom.tertiary]
model = "last-resort"

[agents.author]
model_provider = "custom.primary"
"#,
        )
        .unwrap();

        assert_eq!(
            registry
                .model_candidates_for_agent("author")
                .unwrap()
                .unwrap(),
            vec![
                "safe-primary",
                "same-provider-backup",
                "paid-secondary",
                "secondary-backup",
                "last-resort",
            ]
        );
        assert_eq!(registry.model_for_agent("author"), Some("safe-primary"));
        assert_eq!(
            registry.agent_for_model("paid-secondary").unwrap(),
            None,
            "a fallback model must not become an independently selectable primary route"
        );
    }

    #[test]
    fn engine_model_registry_projects_live_json_fallbacks_and_rejects_cycles() {
        let registry = EngineModelRegistry::from_json(&serde_json::json!({
            "providers": {
                "models": {
                    "custom": {
                        "one": {
                            "model": "model-one",
                            "fallback_models": ["model-one-b"],
                            "fallback": ["custom.two"]
                        },
                        "two": {
                            "model": "model-two",
                            "fallback": ["custom.one"]
                        }
                    }
                }
            },
            "agents": {
                "author": { "model_provider": "custom.one" }
            }
        }))
        .unwrap();

        let err = registry
            .model_candidates_for_agent("author")
            .expect_err("a fallback cycle must not be silently removed from policy projection");
        assert!(err
            .to_string()
            .contains("custom.one -> custom.two -> custom.one"));
    }

    #[test]
    fn provider_for_model_routes_by_serves_prefix_else_none() {
        let mut cfg = Config::default_provider(std::path::Path::new("/tmp/zoder-test"));
        cfg.providers.push(Provider {
            id: "minimax".into(),
            engine_provider_ref: None,
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
            id: "enterprise-gateway".into(),
            engine_provider_ref: None,
            base_url: "https://gateway.example.invalid/v1".into(),
            kind: "openai-chat".into(),
            auth: Auth::None,
            paid: false,
            billing: BillingMode::Free,
            subscription: None,
            serves: vec!["enterprise/".into()],
            azure_api_version: None,
        });
        // Prefix match wins, in config order.
        assert_eq!(cfg.provider_for_model("MiniMax-M3").unwrap().id, "minimax");
        assert_eq!(
            cfg.provider_for_model("enterprise/review-model")
                .unwrap()
                .id,
            "enterprise-gateway"
        );
        assert_eq!(
            cfg.provider_for_model("enterprise/assistant-model")
                .unwrap()
                .id,
            "enterprise-gateway"
        );
        // No prefix claims it -> returns None (not default_provider).
        assert!(
            cfg.provider_for_model("azure/gpt-4o").is_none(),
            "unmatched model must return None, not fall through to default_provider"
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
            engine_provider_ref: None,
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
            engine_provider_ref: None,
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

    // ---- C2-5: Auth::Bearer must never leak its token under `{:?}` ----

    #[test]
    fn bearer_debug_redacts_the_inline_token() {
        // A future tracing/log/anyhow/panic that formats an `Auth` (or a
        // `Provider`/`Config` containing one) with `{:?}` must NOT print the
        // secret. The hand-written Debug impl redacts it regardless of call
        // site.
        let rendered = format!(
            "{:?}",
            Auth::Bearer {
                token: "sk-secret".into(),
            }
        );
        assert!(
            !rendered.contains("sk-secret"),
            "Auth::Bearer Debug leaked the token: {rendered}"
        );
        assert!(
            rendered.contains("[redacted]"),
            "Auth::Bearer Debug should mark the token redacted: {rendered}"
        );
    }

    #[test]
    fn non_secret_auth_variants_keep_useful_debug() {
        // Env/ApiKeyHeader only carry the env var NAME (not the value) plus a
        // header name — safe and useful to print. Keep them legible.
        let env_dbg = format!(
            "{:?}",
            Auth::Env {
                var: "MY_API_KEY".into()
            }
        );
        assert!(env_dbg.contains("MY_API_KEY"), "{env_dbg}");
        let hdr_dbg = format!(
            "{:?}",
            Auth::ApiKeyHeader {
                header: "api-key".into(),
                var: "MY_API_KEY".into(),
            }
        );
        assert!(hdr_dbg.contains("api-key"), "{hdr_dbg}");
        assert!(hdr_dbg.contains("MY_API_KEY"), "{hdr_dbg}");
        assert_eq!(format!("{:?}", Auth::None), "None");
    }

    // ---- C2-4: home resolution fails LOUD instead of silently relative ----

    #[test]
    fn resolve_home_rejects_empty_zoder_home() {
        // ZODER_HOME="" would become PathBuf::from("") -> relative -> config
        // silently resolved against CWD. Must error.
        let err = Config::resolve_home_from(Some(String::new()), Some(PathBuf::from("/home/op")))
            .expect_err("empty ZODER_HOME must be rejected");
        assert!(
            err.to_string().contains("ZODER_HOME"),
            "error should name ZODER_HOME: {err}"
        );
    }

    #[test]
    fn resolve_home_errors_when_home_dir_is_none_and_zoder_home_unset() {
        // systemd/no-HOME/minimal-container/cron: home_dir() is None and
        // ZODER_HOME unset. unwrap_or_default() used to yield "" -> `.zoder`
        // relative. Must error instead.
        let err = Config::resolve_home_from(None, None)
            .expect_err("no home dir + no ZODER_HOME must be rejected");
        assert!(
            err.to_string().contains("home directory"),
            "error should explain the missing home directory: {err}"
        );
    }

    #[test]
    fn resolve_home_rejects_relative_zoder_home() {
        // A non-absolute ZODER_HOME is resolved against CWD -> split state.
        let err = Config::resolve_home_from(Some("relative/dir".into()), None)
            .expect_err("relative ZODER_HOME must be rejected");
        assert!(
            err.to_string().contains("absolute"),
            "error should demand an absolute path: {err}"
        );
    }

    #[test]
    fn resolve_home_accepts_absolute_zoder_home_and_default_dotdir() {
        assert_eq!(
            Config::resolve_home_from(Some("/opt/zoder".into()), None).unwrap(),
            PathBuf::from("/opt/zoder"),
        );
        assert_eq!(
            Config::resolve_home_from(None, Some(PathBuf::from("/home/op"))).unwrap(),
            PathBuf::from("/home/op/.zoder"),
        );
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

    // ---------- load_unvalidated: read/parse without validate-and-bail ----------
    //
    // C3-1/C3-2: `configure` needs a load that surfaces validation problems to
    // its OWN reporting/exit logic (not a bail) and that renders a malformed
    // config.json as a clean error, not a raw serde backtrace.

    #[test]
    fn load_unvalidated_accepts_a_valid_config_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{
                "providers": [{
                    "id": "acme",
                    "base_url": "https://gw.acme.example/v1",
                    "kind": "openai-chat",
                    "auth": {"type": "none"}
                }],
                "corpus_path": "/tmp/zoder-c3b/corpus.json",
                "ledger_path": "/tmp/zoder-c3b/ledger.json",
                "health_path": "/tmp/zoder-c3b/health.json",
                "default_provider": "acme"
            }"#,
        )
        .unwrap();
        let cfg =
            Config::load_unvalidated_from(dir.path()).expect("a well-formed config.json loads");
        assert!(
            cfg.validate().is_empty(),
            "a well-formed config has no validate() problems: {:?}",
            cfg.validate()
        );
        assert!(cfg.providers.iter().any(|p| p.id == "acme"));
    }

    #[test]
    fn load_unvalidated_returns_invalid_config_instead_of_bailing() {
        // Duplicate provider ids are a validate() problem. `load()` would
        // bail; `load_unvalidated` must RETURN the config so `configure` can
        // report the problem itself (C3-1). It must NOT be an Err.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{
                "providers": [
                    {"id": "dup", "base_url": "https://a.example/v1", "kind": "openai-chat", "auth": {"type": "none"}},
                    {"id": "dup", "base_url": "https://b.example/v1", "kind": "openai-chat", "auth": {"type": "none"}}
                ],
                "corpus_path": "/tmp/zoder-c3b/corpus.json",
                "ledger_path": "/tmp/zoder-c3b/ledger.json",
                "health_path": "/tmp/zoder-c3b/health.json",
                "default_provider": "dup"
            }"#,
        )
        .unwrap();
        let cfg = Config::load_unvalidated_from(dir.path())
            .expect("an INVALID-but-parseable config must load, not bail");
        let problems = cfg.validate();
        assert!(
            problems.iter().any(|e| e.contains("duplicate provider id")),
            "validate() should surface the duplicate id: {problems:?}"
        );
    }

    #[test]
    fn load_unvalidated_reports_empty_providers_as_a_problem_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{ "providers": [], "default_provider": "", "corpus_path": "/tmp/zoder-c3b/corpus.json", "ledger_path": "/tmp/zoder-c3b/ledger.json", "health_path": "/tmp/zoder-c3b/health.json" }"#,
        )
        .unwrap();
        let cfg = Config::load_unvalidated_from(dir.path())
            .expect("empty-providers config parses; problems are for validate()");
        assert!(
            cfg.validate()
                .iter()
                .any(|e| e.contains("no providers configured")),
            "empty providers must be a validate() problem: {:?}",
            cfg.validate()
        );
    }

    #[test]
    fn load_unvalidated_reports_malformed_json_as_a_clean_error() {
        // A trailing comma is not valid JSON. `load_unvalidated` must return a
        // clean, contextual Err (rendered by `configure` as one config
        // problem) rather than panicking or emitting a raw backtrace (C3-2).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.json"),
            r#"{
                "providers": [
                    {"id": "x", "base_url": "https://a.example/v1", "kind": "openai-chat", "auth": {"type": "none"}},
                ],
                "default_provider": "x"
            }"#,
        )
        .unwrap();
        let err = Config::load_unvalidated_from(dir.path())
            .expect_err("malformed config.json must be an Err");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parsing zoder config at"),
            "error should name the parse step + path: {msg}"
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
            engine_provider_ref: None,
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
            engine_provider_ref: None,
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

    #[test]
    fn validate_rejects_unservable_primary_model() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default_provider(dir.path());
        cfg.providers[0].serves = vec!["gpt-".into()];
        // Set primary_model to a model that no provider serves.
        cfg.primary_model = Some("unknown-model".into());
        let errs = cfg.validate();
        assert!(
            !errs.is_empty(),
            "unservable primary_model must be rejected, got: {errs:?}"
        );
        assert!(
            errs.join("\n").contains("primary_model"),
            "error must reference primary_model, got: {errs:?}"
        );
        assert!(
            errs.join("\n").contains("not served by any provider"),
            "error must say 'not served by any provider', got: {errs:?}"
        );
    }

    #[test]
    fn validate_rejects_unservable_per_agent_model() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default_provider(dir.path());
        cfg.providers[0].serves = vec!["gpt-".into()];
        cfg.agents.insert(
            "codex".into(),
            AliasedAgentConfig {
                model: Some("unknown-model".into()),
                reviewer_model: None,
                // Added by the --agent/--oneshot fix (zoder#16). This test came
                // from the provider-validation fix (zoder#17); the two merged
                // cleanly on text and then failed to compile, because one added
                // a field the other constructs the struct without.
                model_provider: None,
            },
        );
        let errs = cfg.validate();
        assert!(
            !errs.is_empty(),
            "unservable per-agent model must be rejected, got: {errs:?}"
        );
        assert!(
            errs.join("\n").contains("[agents.codex]"),
            "error must reference the agent, got: {errs:?}"
        );
    }

    #[test]
    fn validate_accepts_served_primary_model() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default_provider(dir.path());
        cfg.providers[0].serves = vec!["gpt-".into()];
        cfg.primary_model = Some("gpt-4".into());
        let errs = cfg.validate();
        assert!(
            errs.is_empty(),
            "served primary_model must not be rejected, got: {errs:?}"
        );
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
            engine_provider_ref: None,
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
            engine_provider_ref: None,
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
            engine_provider_ref: None,
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
            engine_provider_ref: None,
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
            engine_provider_ref: None,
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
                model_provider: None,
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
            engine_provider_ref: None,
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
            engine_provider_ref: None,
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
                engine_provider_ref: None,
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

    /// DEFECT 1 (config read at line ~1105): a FIFO, symlink-to-FIFO, or any
    /// non-regular file at `$ZODER_HOME/config.json` must be REJECTED by
    /// `read_bounded_regular_file` BEFORE the body is read. The pre-fix code
    /// called `fs::read_to_string` unconditionally, which blocks forever on a
    /// FIFO (the open call never returns) on every config load. The fix must
    /// short-circuit with a clear error.
    #[test]
    #[cfg(unix)]
    fn load_unvalidated_rejects_non_regular_config_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        // Try mkfifo first — the canonical way to fabricate a blocking
        // path. If `mkfifo` isn't on PATH (uncommon), fall back to a
        // symlink to /dev/null, which is also non-regular (`is_file()`
        // returns false) but does not block on open.
        let mkfifo_status = std::process::Command::new("mkfifo").arg(&path).status();
        let fifo_ok = matches!(&mkfifo_status, Ok(s) if s.success());
        if !fifo_ok {
            std::fs::write(&path, b"").unwrap();
            std::os::unix::fs::symlink("/dev/null", &path).unwrap();
        }

        let err = Config::load_unvalidated_from(dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        // Either of two rejection paths is acceptable (and both are
        // safe under the new TOCTOU-safe helper):
        //   - "not a regular file" — the open succeeded (e.g. on a
        //     FIFO without O_NONBLOCK, or on the symlink-to-/dev/null
        //     fallback when O_NOFOLLOW didn't fire), and `f.metadata()`
        //     returned a non-regular file descriptor.
        //   - "No such device" / "No such file" / "symbolic link" /
        //     "ELOOP" / "Too many levels" — the open itself failed
        //     before fstat could run, either because O_NONBLOCK got
        //     ENXIO on a writer-less FIFO or because O_NOFOLLOW got
        //     ELOOP on the symlink-to-/dev/null fallback.
        // The pre-fix code (no O_NOFOLLOW, no O_NONBLOCK, separate
        // metadata + read_to_string calls) would have blocked forever
        // on the FIFO; reaching this assertion at all is the proof
        // that the new helper refuses the non-regular target.
        assert!(
            msg.contains("not a regular file")
                || msg.contains("No such device")
                || msg.contains("No such file")
                || msg.contains("symbolic link")
                || msg.contains("ELOOP")
                || msg.contains("Too many levels"),
            "non-regular config.json must be rejected before the read; got: {msg}"
        );
        // Reachable: if the FIFO guard had not fired, the test would block
        // forever here instead of unwrapping the error.
    }

    /// DEFECT 1 (overlay read at line ~1869): the same regular-file + size
    /// guard must apply to `config.<vendor>.toml` overlays via
    /// `read_bounded_regular_file`. An oversized overlay must be rejected
    /// with a bounded-size error before the body is read (otherwise a huge
    /// overlay OOMs the process on every config load).
    #[test]
    fn collect_overlays_rejects_oversized_overlay() {
        use crate::config::Config;
        let dir = tempfile::tempdir().unwrap();
        // Write a TOML overlay whose declared size exceeds `MAX_CONFIG_BYTES`.
        // The header is valid TOML so a sanity check that the helper, not the
        // parser, is what fails.
        let mut contents = String::from(
            r#"
[[providers]]
id = "oversized-vendor"
base_url = "https://example.com/v1"
kind = "openai-chat"
auth = { type = "env", var = "OVRD_KEY" }

# padding to push the file past the cap
"#,
        );
        let cap = Config::MAX_CONFIG_BYTES as usize;
        contents.push_str(&"x".repeat(cap + 1024));
        std::fs::write(dir.path().join("config.oversized.toml"), contents).unwrap();

        let mut cfg = Config::default_provider(dir.path());
        let err = apply_overlays(&mut cfg, dir.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exceeds") && msg.contains("byte limit"),
            "oversized overlay must be rejected by the size guard, not the parser; got: {msg}"
        );
    }

    /// DEFECT 2 (read_dir enumeration at line ~1836): a `read_dir` failure
    /// that is NOT `io::ErrorKind::NotFound` (e.g. `PermissionDenied` from a
    /// `chmod 000` directory, or `NotADirectory` from a file at the home
    /// path) must surface as an error to the caller — NOT be silently
    /// swallowed into `Ok(Vec::new())`. Otherwise a real `config.<vendor>.toml`
    /// overlay that exists on disk is quietly skipped and routing proceeds
    /// with the wrong (non-overlaid) provider set.
    #[test]
    #[cfg(unix)]
    fn collect_overlays_surfaces_non_notfound_read_dir_errors() {
        use std::os::unix::fs::PermissionsExt;
        // `chmod 000` cannot produce a `PermissionDenied` `read_dir` when the
        // test runs as root: DAC permission checks are bypassed for uid 0 on
        // every Unix this crate targets, so `read_dir` would succeed and the
        // `unwrap_err()` below would panic on an `Ok` value -- a false
        // failure of the TEST HARNESS, not of the code under test (CI runners
        // commonly execute as root inside a container). Skip rather than
        // assert something this environment cannot exercise.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!(
                "skipping collect_overlays_surfaces_non_notfound_read_dir_errors: \
                 running as root, chmod 000 cannot produce PermissionDenied"
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // Place a real overlay in the directory so the test would otherwise
        // be observable as "overlay was silently skipped" if the bug
        // regresses.
        std::fs::write(
            dir.path().join("config.guard.toml"),
            r#"
[[providers]]
id = "guard-vendor"
base_url = "https://example.com/v1"
kind = "openai-chat"
auth = { type = "env", var = "GUARD_KEY" }
"#,
        )
        .unwrap();
        // Revoke all permissions so `read_dir` fails with `PermissionDenied`.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o000)).unwrap();

        let mut cfg = Config::default_provider(dir.path());
        let err = apply_overlays(&mut cfg, dir.path()).unwrap_err();

        // Restore permissions before any assertion (so tempdir cleanup
        // works and so the test doesn't leave a 000'd dir behind if
        // assertions fail).
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

        let msg = err.to_string();
        assert!(
            msg.contains("enumerating vendor overlays"),
            "non-NotFound read_dir error must surface wrapped with context, not be silently swallowed; got: {msg}"
        );
        // The whole point: a guard-vendor overlay that exists on disk must
        // NOT have been silently merged into cfg as if there were no
        // overlays. If the bug regresses, `cfg.providers` would contain
        // guard-vendor.
        assert!(
            cfg.providers.iter().all(|p| p.id != "guard-vendor"),
            "overlay that exists on disk must not be silently skipped when read_dir fails; got providers: {:?}",
            cfg.providers.iter().map(|p| &p.id).collect::<Vec<_>>()
        );
    }

    /// DEFECT 2 sanity check: a `read_dir` error of kind `NotFound` (the
    /// "no overlay directory yet" first-run case) must STILL keep silent
    /// (return `Ok(Vec::new())`) — only non-`NotFound` errors must surface.
    /// This test guards against the regression "fix always errors when home
    /// doesn't exist yet", which would break every fresh install.
    #[test]
    fn collect_overlays_keeps_silent_on_missing_home() {
        let dir = tempfile::tempdir().unwrap();
        let nonexistent = dir.path().join("does-not-exist");
        let mut cfg = Config::default_provider(&nonexistent);
        // Must not error.
        apply_overlays(&mut cfg, &nonexistent)
            .expect("missing home is the legitimate first-run case");
    }

    /// TOCTOU-safety regression: the previous implementation of
    /// `read_bounded_regular_file` did `fs::metadata(path)` THEN
    /// `fs::read_to_string(path)` as two separate `path` lookups. An
    /// attacker who could replace the path between those two syscalls
    /// (unlink the regular file, `mkfifo` in its place) bypassed the
    /// size + `is_file()` guards — `read_to_string` would `open()` the
    /// FIFO and block forever on `read()`. The current implementation
    /// opens the file *once* (on Unix with `O_CLOEXEC | O_NOFOLLOW` so
    /// a symlink at the path is rejected at open time), validates the
    /// inode of the resulting descriptor via `f.metadata()` (i.e.
    /// `fstat(fd)`), and reads through `Read::take(max_bytes)`. Once
    /// we hold an FD on a specific inode, `unlink` + `mkfifo` at the
    /// path by another process cannot affect us.
    ///
    /// Three regression tests below cover the attack surface:
    ///
    /// 1. **Symlink-to-FIFO at the path.** A symlink that resolves to
    ///    a FIFO must be rejected at open time by `O_NOFOLLOW` —
    ///    never blocking, never reading from the FIFO.
    /// 2. **Path swap during a read.** A racing thread swaps a
    ///    regular file for a FIFO at the path while the helper is
    ///    mid-read. The helper's FD is bound to the inode we
    ///    opened, so the swap is invisible to it; the read must
    ///    return the original content OR a clear open-time error,
    ///    and must not block past the wall-clock budget.
    /// 3. **Continuous swapper with bounded read budget.** A
    ///    dedicated thread continuously renames the target aside,
    ///    mkfifos the path, then immediately moves the original
    ///    back. The reader runs many iterations; every iteration
    ///    must either (a) succeed reading the original content
    ///    byte-for-byte, or (b) fail with a clear open-time error.
    ///    Critically, no iteration may (i) block past the budget,
    ///    (ii) return success with non-original content (a FIFO
    ///    read), or (iii) panic. The OLD design (metadata then
    ///    read_to_string as separate path lookups) would either
    ///    block on a FIFO read or, on kernels that unlink-then-
    ///    re-create the FIFO, potentially read FIFO content —
    ///    neither outcome is acceptable here.
    #[test]
    #[cfg(unix)]
    fn read_bounded_regular_file_rejects_symlink_to_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo.target");
        let _ = std::fs::remove_file(&fifo);
        let mkfifo = std::process::Command::new("mkfifo").arg(&fifo).status();
        assert!(
            matches!(&mkfifo, Ok(s) if s.success()),
            "mkfifo must succeed in this test environment"
        );
        // Place a symlink at the config path pointing AT the FIFO.
        // `O_NOFOLLOW` on open rejects the symlink itself, before
        // the kernel ever considers the FIFO inode. The
        // open-then-fstat pattern then validates the descriptor is
        // a regular file. Either layer is sufficient; both are
        // belt-and-braces against the TOCTOU window the OLD
        // implementation left between metadata and read_to_string.
        let config_path = dir.path().join("config.json");
        std::os::unix::fs::symlink(&fifo, &config_path).unwrap();

        let err = crate::config::read_bounded_regular_file(
            &config_path,
            crate::config::Config::MAX_CONFIG_BYTES,
        )
        .expect_err("symlink-to-FIFO at the config path must be rejected");

        let msg = format!("{err:#}");
        assert!(
            msg.contains("not a regular file")
                || msg.contains("symbolic link")
                || msg.contains("No such device")
                || msg.contains("ELOOP")
                || msg.contains("Too many levels of symbolic links"),
            "symlink-to-FIFO must be rejected with a clear error (O_NOFOLLOW \
             surfaces ELOOP, fstat surfaces 'not a regular file'); got: {msg}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn read_bounded_regular_file_survives_path_swap_during_read() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        // A few KiB of content so the helper's read has a
        // non-trivial duration, giving the swapper a window.
        let mut content = String::with_capacity(4 * 1024 + 64);
        content.push_str("{\"_pad\":[");
        for i in 0..256 {
            if i > 0 {
                content.push(',');
            }
            content.push_str(&format!("{i}"));
        }
        content.push_str("]}");
        let expected = content.clone();
        std::fs::write(&target, content.as_bytes()).unwrap();

        // Spawn the helper in a worker thread so we can bound the
        // wall-clock time. A regression that hangs on the FIFO read
        // fails the test with a clear "blocked past 5s" instead of
        // hanging cargo.
        let (tx, rx) = mpsc::channel::<anyhow::Result<String>>();
        let target_for_reader = target.clone();
        let reader = thread::spawn(move || {
            let result = crate::config::read_bounded_regular_file(
                &target_for_reader,
                crate::config::Config::MAX_CONFIG_BYTES,
            );
            tx.send(result).unwrap();
        });

        // Swap the path while the helper is mid-read. The exact
        // interleaving is not deterministic, but BOTH outcomes are
        // acceptable under the new design:
        //   - swap-before-open: open sees the FIFO (or ENOENT
        //     between the rename-out and the mkfifo-in), helper
        //     errors immediately. SAFE.
        //   - swap-after-open: helper's FD is bound to the
        //     original inode, swap is irrelevant, read succeeds
        //     with the original content. SAFE.
        // A regression to the OLD design would either block on a
        // FIFO read or, on kernels that unlink-then-re-create,
        // potentially read FIFO content — neither outcome is
        // acceptable here.
        thread::sleep(Duration::from_millis(2));
        let backup = dir.path().join("config.json.bak");
        let _ = std::fs::rename(&target, &backup);
        let mkfifo = std::process::Command::new("mkfifo").arg(&target).status();
        let fifo_ok = matches!(&mkfifo, Ok(s) if s.success());
        if fifo_ok {
            // Unlink the FIFO so any subsequent open sees ENOENT
            // (rather than blocking on a write-less FIFO forever).
            let _ = std::fs::remove_file(&target);
        }
        // Restore the original at the path so subsequent tests
        // and the tempdir cleanup find the expected file layout.
        let _ = std::fs::rename(&backup, &target);

        // Wait for the reader to finish (or the budget to elapse).
        let result = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("read_bounded_regular_file must not block past 5s — TOCTOU regression");
        reader.join().expect("reader thread panicked");

        // The load-bearing assertions:
        //   - The helper must NOT have panicked (reader.join()
        //     above),
        //   - The helper must NOT have blocked past the budget
        //     (recv_timeout above),
        //   - On the success path, the helper must have read the
        //     ORIGINAL content byte-for-byte — proving the FD was
        //     bound to the inode we opened, not whatever the
        //     swapper installed at the path.
        //   - On the failure path, the error must be a clear
        //     open-time error, NOT a silent truncation or
        //     empty-string return.
        match result {
            Ok(s) => {
                assert_eq!(
                    s.as_bytes(),
                    expected.as_bytes(),
                    "helper must have read the original inode's content, \
                     not whatever was at the path after the swap"
                );
            }
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("not a regular file")
                        || msg.contains("No such file")
                        || msg.contains("No such device")
                        || msg.contains("symbolic link"),
                    "swap-induced error must be a clear open-time error, \
                     not a silent failure; got: {msg}"
                );
            }
        }
    }

    /// Self-defense bounds for [`run_swap_loop`].
    ///
    /// The coordinator (the test thread) normally sets the `stop` flag as
    /// soon as the reader finishes its 64 iterations — well inside the
    /// 10s receive budget below. These bounds exist purely so a
    /// *panicked or starved* coordinator cannot leave the swapper thread
    /// spinning a core forever. That unbounded-under-contention failure
    /// mode is ncz-os/zoder#20: `cargo test` ran 3.96 days instead of
    /// failing. With these bounds the worst case is a bounded wait and a
    /// failed assertion.
    #[cfg(unix)]
    const SWAP_MAX_ITERS: u64 = 100_000;
    #[cfg(unix)]
    const SWAP_MAX_WALL: std::time::Duration = std::time::Duration::from_secs(60);

    /// Why a [`run_swap_loop`] invocation returned. Lets the TOCTOU
    /// race test distinguish a clean coordinator stop from one of the
    /// self-defense bounds, and lets the bound tests below assert on it.
    #[cfg(unix)]
    #[derive(Debug, PartialEq, Eq)]
    enum SwapExit {
        /// The coordinator set the `stop` flag (the normal path).
        Stopped,
        /// The swapper's handshake wait elapsed before the reader
        /// signalled — the race was never exercised.
        HandshakeTimeout,
        /// The loop hit its explicit iteration cap.
        IterationCap,
        /// The loop hit its explicit wall-clock deadline.
        Deadline,
    }

    /// Run the rename/mkfifo/restore swap cycle until `stop` is set,
    /// `max_iters` cycles have run, or `deadline` elapses.
    ///
    /// The explicit bounds are the fix for ncz-os/zoder#20. The
    /// coordinating test thread is the only thing that used to end this
    /// loop; if it panicked (or was starved past the test harness's
    /// patience) the swapper would spin a core forever and `cargo test`
    /// would never return. Bounding the loop here turns that failure
    /// mode into a bounded return — and, at the call site, a failed
    /// assertion — instead of an immortal process.
    #[cfg(unix)]
    fn run_swap_loop(
        dir: &std::path::Path,
        target: &std::path::Path,
        stop: &std::sync::atomic::AtomicBool,
        max_iters: u64,
        deadline: std::time::Instant,
    ) -> SwapExit {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};

        let mut i: u64 = 0;
        while !stop.load(Ordering::Relaxed) {
            // Check both self-defense bounds before doing any work so a
            // starved coordinator can never extend the loop past them.
            if i >= max_iters {
                return SwapExit::IterationCap;
            }
            if Instant::now() >= deadline {
                return SwapExit::Deadline;
            }

            // Brief "file present" pause so at least *some* reader
            // iterations after the handshake can race past the swap
            // into the happy path too — keeps the swapper's duty cycle
            // honest without making the test's outcomes depend on this
            // interval.
            std::thread::sleep(Duration::from_micros(2000));

            // Now perform the swap: rename target aside, mkfifo,
            // unlink, restore.
            let backup = dir.join(format!("config.bak.{i}"));
            if std::fs::rename(target, &backup).is_ok() {
                let mkfifo = std::process::Command::new("mkfifo").arg(target).status();
                if matches!(&mkfifo, Ok(s) if s.success()) {
                    let _ = std::fs::remove_file(target);
                }
                let _ = std::fs::rename(&backup, target);
            }
            i += 1;
        }
        SwapExit::Stopped
    }

    /// RAII flag-flipper: sets the wrapped `AtomicBool` to `true` on
    /// drop, including during unwind. Used so a panicking coordinator
    /// releases the swapper thread promptly instead of making it wait
    /// out its full self-defense deadline.
    #[cfg(unix)]
    struct StopOnDrop(std::sync::Arc<std::sync::atomic::AtomicBool>);

    #[cfg(unix)]
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Continuous swapper, synchronized to the reader so the wall-clock
    /// timing assumptions don't hold the test hostage to host load.
    ///
    /// **Why this test exists.** The OLD `read_bounded_regular_file` did
    /// `fs::metadata(path)` THEN `fs::read_to_string(path)` as two
    /// separate `path` lookups, leaving a TOCTOU window that an attacker
    /// could widen by replacing the path with a FIFO between the two
    /// calls. The NEW helper opens once (with `O_NOFOLLOW | O_NONBLOCK`
    /// on Unix so symlinks-to-FIFO and writer-less FIFOs fail fast at
    /// open time), fstat's the open FD, and reads through
    /// `Read::take(max_bytes)`. Once we hold an FD on a specific inode,
    /// `rename` + `mkfifo` at the path by another thread cannot affect
    /// us.
    ///
    /// **Why the previous timing-based fix was flaky.** The previous
    /// design had the swapper loop continuously with a fixed 2 ms
    /// "file present" sleep between rename cycles, and relied on the
    /// reader thread racing into that window at least once across 64
    /// iterations. Under real CPU contention (e.g. the default-parallel
    /// `cargo test --workspace` run with 530+ other tests competing),
    /// the swapper's 2 ms sleep stretched out of proportion to the
    /// reader's iteration cost (filesystem `rename`/`mkfifo` syscalls
    /// under load can balloon into the millisecond range), so the
    /// reader's 64 short opens could all fall into the swapper's
    /// "file missing" window and produce `ok-original=0, err-enoent=64`
    /// — a CI flake unrelated to whatever change was actually being
    /// verified.
    ///
    /// **The fix.** Replace the fixed sleep with an explicit
    /// handshake: the swapper blocks on `recv()` until the reader has
    /// completed at least one successful read of the original inode.
    /// That guarantees `ok-original > 0` deterministically (no timing
    /// dependency), then the swapper races the remaining iterations so
    /// the safety assertions (`!ok-other`, no panics, no
    /// `recv_timeout` blowups) still exercise every code path the
    /// race could trigger — ENOENT, ENXIO, "not a regular file" —
    /// against the fully-loaded, racing swapper.
    #[test]
    #[cfg(unix)]
    fn read_bounded_regular_file_is_toctou_safe_against_continuous_swap() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::mpsc;
        use std::sync::Arc;
        use std::thread;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        let original = b"{}";
        std::fs::write(&target, original).unwrap();

        // Synchronization channel: the reader signals the swapper via
        // `go_tx` only after it has completed at least one successful
        // read of the original inode. This is what makes
        // `ok-original > 0` deterministic regardless of host load —
        // the reader's first iteration runs against a swapper that is
        // provably blocked on `go_rx.recv()`, so the file is stable
        // and the read cannot fail.
        let (go_tx, go_rx) = mpsc::channel::<()>();

        // The swapper: keep the file PRESENT most of the time, but
        // periodically swap to a FIFO and back. With the NEW design
        // (open-then-fstat) every read after the swap starts either:
        //   - sees the file before the swap fires and reads it
        //     successfully,
        //   - sees the file after the swap has restored it and reads
        //     it successfully, OR
        //   - catches the FIFO (rejected at fstat), the missing path
        //     (ENOENT at open), or the brief mkfifo window (the
        //     unlinked FIFO surfaces ENXIO).
        // The OLD design would have either blocked forever on a FIFO
        // read (recv_timeout catches) or returned non-original bytes
        // (the helper's `Ok(_)` arm catches). The `recv_timeout` on
        // `go_rx` is a hard safety net: if the helper ever regresses
        // to the point that even the first iteration cannot succeed
        // against a stable file, the swapper exits cleanly instead of
        // hanging `cargo test` — the test then fails on the
        // `ok-original > 0` assertion below with a clear tally, which
        // is the right behavior.
        let stop = Arc::new(AtomicBool::new(false));
        // Belt-and-braces (ncz-os/zoder#20): if the reader or an
        // assertion panics, this guard still flips `stop` during unwind
        // so the swapper leaves at the top of its next iteration rather
        // than waiting out its full self-defense deadline. The loop's
        // own bounds are the load-bearing fix; this just makes the
        // common panic path prompt.
        let _stop_on_drop = StopOnDrop(stop.clone());
        let stop_clone = stop.clone();
        let swap_dir = dir.path().to_path_buf();
        let swap_target = target.clone();
        // Explicit self-defense bounds for the swapper loop (see
        // `run_swap_loop`). The coordinator normally sets `stop` well
        // before these fire; they exist so a panicked or starved
        // coordinator can never leave the swapper spinning forever.
        let swap_deadline = Instant::now() + SWAP_MAX_WALL;
        let swapper = thread::spawn(move || {
            // Block until the reader has at least one successful
            // read of the original inode. The 2s budget is generous:
            // a stable 2-byte file reads in microseconds, so anything
            // past 2s is a regression, not a flake. On timeout we
            // return `HandshakeTimeout` (never entering the racing
            // loop) so the assertion below fails with a clear reason
            // instead of silently passing without exercising the race.
            if go_rx.recv_timeout(Duration::from_secs(2)).is_err() {
                return SwapExit::HandshakeTimeout;
            }

            // Bounded loop — exits on `stop`, the iteration cap, or the
            // wall-clock deadline, never unboundedly.
            run_swap_loop(
                &swap_dir,
                &swap_target,
                &stop_clone,
                SWAP_MAX_ITERS,
                swap_deadline,
            )
        });

        // Reader: run N iterations and collect observations. Each
        // iteration must complete promptly and either return the
        // original content or a clear open-time error. On the very
        // first successful read of the original content, signal the
        // swapper that the happy path is proven and it's safe to
        // start racing; subsequent iterations then exercise the racing
        // safety checks (ENOENT, ENXIO, "not a regular file") without
        // the timing pressure that used to cause `ok-original=0`
        // false-negatives under parallel `cargo test --workspace`
        // load.
        let (tx, rx) = mpsc::channel::<Vec<&'static str>>();
        let target_for_reader = target.clone();
        let reader = thread::spawn(move || {
            let mut observations: Vec<&'static str> = Vec::with_capacity(64);
            let mut signaled = false;
            for i in 0..64 {
                if i == 0 {
                    eprintln!("[reader] first iteration starting (swapper blocked on go_rx)");
                }
                match crate::config::read_bounded_regular_file(
                    &target_for_reader,
                    crate::config::Config::MAX_CONFIG_BYTES,
                ) {
                    Ok(s) if s.as_bytes() == original => {
                        // First success: hand control to the swapper.
                        // After this point the test exercises the
                        // racing safety paths; the happy-path assertion
                        // is already locked in regardless of how ugly
                        // the race gets.
                        if !signaled {
                            go_tx.send(()).expect(
                                "swapper must accept the handshake — it is alive until `stop`",
                            );
                            signaled = true;
                        }
                        observations.push("ok-original");
                    }
                    Ok(s) => {
                        observations.push("ok-other");
                        eprintln!(
                            "UNEXPECTED CONTENT ({} bytes): {:?}",
                            s.len(),
                            &s[..s.len().min(64)]
                        );
                    }
                    Err(e) => {
                        if observations.is_empty() && i == 0 {
                            // The very first iteration — swapper is
                            // blocked, file is stable — should never
                            // fail. Surface this loudly because it
                            // means the helper regressed against a
                            // trivial 2-byte regular file, not just
                            // that the swapper is fast.
                            eprintln!(
                                "[reader] UNEXPECTED first-iter err against stable file: {e:#}"
                            );
                        }
                        let msg = format!("{e:#}");
                        if msg.contains("not a regular file") {
                            observations.push("err-not-regular");
                        } else if msg.contains("No such file") {
                            observations.push("err-enoent");
                        } else if msg.contains("No such device") {
                            // ENXIO from O_NONBLOCK on a writer-less
                            // FIFO. SAFE — the helper refused the
                            // FIFO instead of blocking on it.
                            observations.push("err-enxio");
                        } else {
                            observations.push("err-other");
                        }
                    }
                }
            }
            tx.send(observations).unwrap();
        });

        let observations = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("read_bounded_regular_file must not block past 10s — TOCTOU regression");
        reader.join().expect("reader thread panicked");
        stop.store(true, Ordering::Relaxed);
        let swap_exit = swapper.join().expect("swapper thread panicked");

        // Tally the observations for the failure message.
        let ok_original = observations.iter().filter(|o| **o == "ok-original").count();
        let err_not_regular = observations
            .iter()
            .filter(|o| **o == "err-not-regular")
            .count();
        let err_enoent = observations.iter().filter(|o| **o == "err-enoent").count();
        let err_enxio = observations.iter().filter(|o| **o == "err-enxio").count();
        let err_other = observations.iter().filter(|o| **o == "err-other").count();

        // The load-bearing assertion: NO iteration returned a
        // successful read of non-original content. An "ok-other"
        // would mean the helper read from a swapped FIFO or device,
        // which is the regression we're guarding against — and it's
        // provably independent of timing because the FD-based open +
        // fstat pattern is race-safe by construction.
        assert!(
            !observations.contains(&"ok-other"),
            "no iteration may return successful read of non-original \
             content — that means the helper read from a swapped target. \
             observations: ok-original={ok_original}, err-not-regular=\
             {err_not_regular}, err-enoent={err_enoent}, err-enxio=\
             {err_enxio}, err-other={err_other}"
        );

        // At least one iteration must have read the original content
        // successfully — this proves the happy path works. With the
        // synchronous handshake between reader and swapper, the
        // FIRST iteration runs against a swapper that is provably
        // blocked on `go_rx.recv()`, so the file is stable and the
        // read cannot fail; the assertion is now deterministic
        // regardless of host load.
        assert!(
            ok_original > 0,
            "at least one iteration must succeed reading the \
             original inode's content. observations: ok-original=\
             {ok_original}, err-not-regular={err_not_regular}, \
             err-enoent={err_enoent}, err-enxio={err_enxio}, \
             err-other={err_other}. If this fires while the swapper \
             handshake reported a clean run, the helper has regressed \
             against a trivial 2-byte regular file — look at the \
             'UNEXPECTED first-iter err against stable file' \
             eprintln above the assertion."
        );

        // The swapper must have exited because the coordinator set
        // `stop`, not because it hit one of its self-defense bounds. A
        // bound hit means the test was starved long enough that the old,
        // unbounded design could have spun forever (ncz-os/zoder#20);
        // surface that as a clear failure instead of a silent pass.
        assert_eq!(
            swap_exit,
            SwapExit::Stopped,
            "swapper must stop via the coordinator's `stop` signal; got \
             {swap_exit:?}. A bound hit means the test was starved past \
             {SWAP_MAX_WALL:?} or the handshake never arrived — under the \
             old unbounded loop this would have been an immortal process \
             rather than a failed test."
        );

        // The error mix is informational. ENOENT, ENXIO, and "not a
        // regular file" are all expected outcomes under a swap —
        // they prove the helper is rejecting dangerous targets fast
        // rather than blocking on them. `err-other` is suspicious
        // but not necessarily wrong (e.g. EACCES on a
        // permission-flipped file); we don't fail on `err-other`
        // alone; we just want to make sure it's not masking a deeper
        // issue.
    }

    /// Regression test for ncz-os/zoder#20: the swapper loop must
    /// terminate on its own wall-clock bound even when the coordinator
    /// never sets `stop` (e.g. it panicked). Without the bound in
    /// [`run_swap_loop`] this test would hang forever — which is exactly
    /// the four-day `cargo test` hang the issue reports.
    #[test]
    #[cfg(unix)]
    fn swap_loop_terminates_on_deadline_without_stop_signal() {
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        std::fs::write(&target, b"{}").unwrap();
        // Deliberately never set: simulates a coordinator that panicked
        // before it could signal the swapper.
        let stop = AtomicBool::new(false);
        let started = Instant::now();
        let exit = run_swap_loop(
            dir.path(),
            &target,
            &stop,
            u64::MAX,
            started + Duration::from_millis(250),
        );
        assert_eq!(exit, SwapExit::Deadline);
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "deadline-bound swapper must return promptly instead of \
             hanging; elapsed {:?}",
            started.elapsed()
        );
    }

    /// Companion to the deadline test: the iteration cap is an
    /// independent bound, so a coordinator that never sets `stop` still
    /// cannot run the swap loop more than `max_iters` times
    /// (ncz-os/zoder#20).
    #[test]
    #[cfg(unix)]
    fn swap_loop_terminates_on_iteration_cap_without_stop_signal() {
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.json");
        std::fs::write(&target, b"{}").unwrap();
        // Never set — the cap, not the coordinator, must end the loop.
        let stop = AtomicBool::new(false);
        let exit = run_swap_loop(
            dir.path(),
            &target,
            &stop,
            3,
            Instant::now() + Duration::from_secs(300),
        );
        assert_eq!(exit, SwapExit::IterationCap);
    }

    #[test]
    fn review_route_defaults_parse_and_are_optional() {
        let raw = r#"
            max_hunk_bytes = 16000
            [route_defaults.qwen38]
            max_hunk_bytes = 64000
            [route_defaults."nvidia/nemotron-3-ultra-550b-a55b"]
            max_hunk_bytes = 24000
            max_diff_bytes = 400000
        "#;
        let rc: ReviewConfig = toml::from_str(raw).expect("route_defaults must parse");
        assert_eq!(rc.max_hunk_bytes, Some(16_000));
        assert_eq!(rc.route_defaults["qwen38"].max_hunk_bytes, Some(64_000));
        assert_eq!(
            rc.route_defaults["nvidia/nemotron-3-ultra-550b-a55b"].max_diff_bytes,
            Some(400_000)
        );
        // A route block with only one cap leaves the other None.
        assert!(
            rc.route_defaults["qwen38"].max_diff_bytes.is_none(),
            "unset field stays None"
        );

        let empty: ReviewConfig = toml::from_str("").expect("empty review block is valid");
        assert!(empty.route_defaults.is_empty());
        assert!(empty.is_empty(), "empty block must round-trip as empty");
    }
}
