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

/// What a rolling rate-limit window counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QuotaUnit {
    #[default]
    Tokens,
    Requests,
    Messages,
}

/// A rolling rate-limit window on a subscription (e.g. a 5-hour cap or a weekly
/// cap). Consumption is measured from the local ledger over `hours`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// Display name, e.g. "5h" or "weekly".
    pub name: String,
    /// Rolling window length in hours (5h = 5, weekly = 168).
    pub hours: u32,
    #[serde(default)]
    pub unit: QuotaUnit,
    /// Cap value, in `unit`, over the rolling window.
    pub cap: f64,
}

/// Subscription terms for a flat-fee provider (ChatGPT/Claude/Cursor-style).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubscriptionPlan {
    /// Flat monthly fee in USD (used only to amortize an effective per-call $).
    #[serde(default)]
    pub monthly_fee_usd: f64,
    /// Rolling rate-limit windows (e.g. a 5-hour cap plus a weekly cap).
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
}

fn default_free_hosts() -> Vec<String> {
    vec!["example.com".into(), "free.example.com".into()]
}

fn default_strict_free() -> bool {
    true
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
    pub fn default_provider(home: &std::path::Path) -> Self {
        Config {
            providers: vec![Provider {
                id: "default".into(),
                base_url: "https://api.example.com/v1".into(),
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
        }
    }

    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id == id)
    }

    /// Resolve which provider should serve a given model id. Returns the FIRST
    /// configured provider whose `serves` list contains a prefix of `model_id`
    /// (config order is preserved through the overlay merge, so a vendor
    /// overlay's providers are matched after the base providers). Falls back to
    /// `default_provider` when no provider claims the prefix — preserving the
    /// historical single-endpoint behavior for unclaimed models. This is what
    /// lets a single routed fallback chain span providers (MiniMax -> EIH).
    pub fn provider_for_model(&self, model_id: &str) -> Option<&Provider> {
        self.providers
            .iter()
            .find(|p| p.serves.iter().any(|prefix| model_id.starts_with(prefix.as_str())))
            .or_else(|| self.provider(&self.default_provider))
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
            cfg.provider_for_model("deepseek-ai/deepseek-r1").unwrap().id,
            "nvidia-eih"
        );
        // No prefix claims it -> falls back to default_provider.
        assert_eq!(
            cfg.provider_for_model("azure/gpt-4o").unwrap().id,
            cfg.default_provider
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
}
