//! Curated subscription-tier preset catalog + plan resolver.
//!
//! Providers (Anthropic, OpenAI, MiniMax, ...) publish very little about
//! flat-fee subscription rate-limit windows in a machine feed: the caps drift,
//! are often unannounced, and the subscription APIs are admin-key gated. So
//! utilization must be **estimated** locally: measure consumption from the
//! local ledger against *known* plan caps. The caps live in this catalog,
//! hand-maintained, with each row tagged by `confidence` (`published` for
//! numbers on an official pricing page, `observed` for community-observed, and
//! `estimated` for a best-effort guess).
//!
//! Two integration points:
//!
//! 1. **Catalog** ([`TierCatalog`]) — the on-disk JSON, hand-curated. The
//!    shipped default is bundled via [`TIERS_JSON_DEFAULT`] so a fresh install
//!    is offline-safe; a refresh from disk is layered on top (see
//!    [`load_tier_catalog`]) and never silently overwrites a known-good
//!    curated value.
//! 2. **Resolver** ([`resolve_plan_windows`]) — given a `SubscriptionPlan`
//!    and the catalog, return the effective `Vec<QuotaWindow>`. Three shapes:
//!
//!    - **Explicit**: `windows: [...]` declared → use them as-is (today's
//!      behavior; unchanged).
//!    - **Preset**: only `tier: "..."` is set → catalog lookup fills the
//!      windows.
//!    - **Preset + overrides**: `tier: "..."` plus some `windows: [...]` →
//!      start from the catalog preset, then any explicit window with the same
//!      `name` overrides the preset's window (operator tunes one cap without
//!      re-declaring the rest).
//!
//! The resolver is **pure + tolerant**: an unknown `tier` (or a missing
//! catalog) never throws — it returns the explicit windows if any, else an
//! empty vec, and emits a `tracing::warn!` so the operator can see what
//! happened. Downstream `window_usage()` then just sees a plan with whatever
//! windows survived the resolution, with the existing engine path completely
//! unchanged.

use crate::config::{Observability, Provider, QuotaUnit, QuotaWindow, ResetKind, SubscriptionPlan};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Bundled default tier catalog (committed alongside the source tree so a
/// fresh install is offline-safe and the report never claims "no presets
/// known"). Operators refresh on disk via [`load_tier_catalog`].
///
/// This is the exact contents of `subscriptions/tiers.json` at the root of
/// the workspace, embedded at compile time. Keep them in sync — the schema
/// is the same; the on-disk copy wins when it exists and parses.
pub const TIERS_JSON_DEFAULT: &str = include_str!("../../../subscriptions/tiers.json");

/// How confident we are in a tier's caps. Drives display, never auth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// Stated on an official pricing / product page.
    Published,
    /// Community-observed; exact figures drift by model and time of day.
    Observed,
    /// Best-effort guess; treat as approximate and verify against dashboard.
    /// `Default` so a hand-edit that forgets `confidence` never silently
    /// labels a guess as observed/published.
    #[default]
    Estimated,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Published => "published",
            Confidence::Observed => "observed",
            Confidence::Estimated => "estimated",
        }
    }
}

/// One rolling rate-limit window on a subscription, as declared in the
/// catalog. Mirrors [`QuotaWindow`] but carries the confidence tag the
/// operator needs to read numbers honestly.
///
/// `cap = None` means the catalog row is "percent-only" — the cap value
/// is unknown but the window still exists (matches the on-disk shape the
/// engine now accepts from operator config).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierWindow {
    pub name: String,
    pub hours: u32,
    #[serde(default)]
    pub unit: QuotaUnit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<f64>,
    /// Model-id glob patterns this window limits; `None` = all models on
    /// the provider (mirrors [`QuotaWindow::models`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<Vec<String>>,
    /// How this window is fed (mirrors [`QuotaWindow::observability`]).
    #[serde(default)]
    pub observability: Observability,
    /// How this window resets (mirrors [`QuotaWindow::reset`]).
    #[serde(default)]
    pub reset: ResetKind,
}

impl TierWindow {
    /// Project into the runtime [`QuotaWindow`] the quota engine consumes.
    pub fn into_quota_window(self) -> QuotaWindow {
        QuotaWindow {
            name: self.name,
            hours: self.hours,
            unit: self.unit,
            cap: self.cap,
            models: self.models,
            observability: self.observability,
            reset: self.reset,
        }
    }
}

/// A single known subscription tier (e.g. Anthropic's `claude-max-20x`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierEntry {
    /// Flat monthly fee in USD (optional — some tiers bundle multiple seats).
    #[serde(default)]
    pub monthly_fee_usd: f64,
    #[serde(default = "Confidence::default")]
    pub confidence: Confidence,
    /// Free-form provenance string for display (e.g. "minimax_token_plan",
    /// "observed"). Not parsed; just shown next to the caps.
    #[serde(default)]
    pub source: String,
    pub windows: Vec<TierWindow>,
}

/// One provider's section of the catalog.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderTiers {
    /// Tier-id (e.g. "claude-max-20x") → entry. BTreeMap for stable JSON output.
    #[serde(default)]
    pub tiers: BTreeMap<String, TierEntry>,
}

/// The full hand-curated catalog. One JSON file, raw-pullable like
/// `corpus/model_corpus.json`; see `subscriptions/tiers.json` at the repo root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierCatalog {
    #[serde(default = "default_version")]
    pub version: u32,
    /// ISO date this catalog was last curated. Warn when older than ~90 days
    /// (the catalog is hand-maintained; quiet drift = stale caps).
    #[serde(default)]
    pub as_of: String,
    /// Human-readable disclaimer emitted alongside utilization reports so the
    /// operator never reads a `pct` and assumes it is authoritative.
    #[serde(default)]
    pub disclaimer: String,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderTiers>,
}

fn default_version() -> u32 {
    1
}

impl Default for TierCatalog {
    fn default() -> Self {
        Self::empty()
    }
}

impl TierCatalog {
    /// Empty catalog — no presets known. Useful for tests and for the fallback
    /// when both the bundled default and the on-disk refresh are unavailable.
    pub fn empty() -> Self {
        Self {
            version: 1,
            as_of: String::new(),
            disclaimer: String::new(),
            providers: BTreeMap::new(),
        }
    }

    /// Load the bundled default. Always succeeds: the JSON is compile-time
    /// embedded. A future parse failure here would be a build-time bug and we
    /// fall back to an empty catalog (with a warning) rather than panic so a
    /// single broken field doesn't break `zoder providers` at runtime.
    pub fn bundled() -> Self {
        match serde_json::from_str::<TierCatalog>(TIERS_JSON_DEFAULT) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("bundled tier catalog failed to parse: {e}; using empty catalog");
                Self::empty()
            }
        }
    }

    /// Look up one provider's tier by id. Returns `None` for unknown
    /// provider/tier; the resolver treats that as a graceful warning rather
    /// than an error.
    pub fn tier(&self, provider_id: &str, tier_id: &str) -> Option<&TierEntry> {
        self.providers.get(provider_id)?.tiers.get(tier_id)
    }

    /// Resolve the catalog namespace from provider classification rather than
    /// requiring an operator-chosen provider id to equal a catalog key.
    pub fn provider_namespace(&self, provider: &Provider, tier_id: &str) -> Option<String> {
        if self.tier(&provider.id, tier_id).is_some() {
            return Some(provider.id.clone());
        }
        let identity = format!(
            "{} {} {} {}",
            provider.id, provider.kind, provider.base_url, tier_id
        )
        .to_ascii_lowercase();
        let classified = if identity.contains("minimax") {
            Some("minimax")
        } else if identity.contains("anthropic") || identity.contains("claude-") {
            Some("anthropic")
        } else if identity.contains("openai")
            || identity.contains("codex")
            || identity.contains("chatgpt-")
        {
            Some("openai")
        } else {
            None
        };
        if let Some(namespace) = classified.filter(|ns| self.tier(ns, tier_id).is_some()) {
            return Some(namespace.to_string());
        }

        let mut matches = self
            .providers
            .iter()
            .filter(|(_, tiers)| tiers.tiers.contains_key(tier_id))
            .map(|(namespace, _)| namespace.clone());
        let only = matches.next()?;
        matches.next().is_none().then_some(only)
    }

    /// All tier ids known for a provider (sorted), or empty when the
    /// provider has no section in the catalog. Used by completions / docs.
    pub fn tier_ids(&self, provider_id: &str) -> Vec<String> {
        let Some(p) = self.providers.get(provider_id) else {
            return Vec::new();
        };
        p.tiers.keys().cloned().collect()
    }

    /// `as_of` parsed as `YYYY-MM-DD`. `None` when unset or malformed — the
    /// staleness check should treat `None` as "can't tell, assume stale".
    pub fn as_of_parsed(&self) -> Option<chrono::NaiveDate> {
        chrono::NaiveDate::parse_from_str(&self.as_of, "%Y-%m-%d").ok()
    }

    /// Number of days between `as_of` and today, in the catalog's local
    /// interpretation. `None` when `as_of` is unset or unparseable so the
    /// caller can warn / silence the staleness check as appropriate.
    pub fn age_days(&self, today: chrono::NaiveDate) -> Option<i64> {
        self.as_of_parsed().map(|d| (today - d).num_days())
    }
}
/// Load a catalog. Precedence:
///   1. `path` is set AND exists AND parses → use it (operator refresh).
///   2. else fall back to the bundled default ([`TIERS_JSON_DEFAULT`]).
///   3. else (bundled default is also broken) → empty catalog.
///
/// The bundled default is the *baseline* — a refresh on disk REPLACES it
/// wholesale (the file is hand-curated; you do not partial-merge catalog
/// data), but the on-disk version must always be a complete, valid catalog.
/// We never auto-overwrite the on-disk file from the bundle: the catalog is
/// hand-maintained.
pub fn load_tier_catalog(path: Option<&Path>) -> TierCatalog {
    if let Some(p) = path {
        if p.exists() {
            match std::fs::read_to_string(p) {
                Ok(raw) => match serde_json::from_str::<TierCatalog>(&raw) {
                    Ok(c) => return c,
                    Err(e) => {
                        tracing::warn!(
                            "tier catalog {} failed to parse: {e}; falling back to bundled default",
                            p.display()
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        "tier catalog {} could not be read: {e}; falling back to bundled default",
                        p.display()
                    );
                }
            }
        }
    }
    TierCatalog::bundled()
}

/// Where to look for the on-disk catalog refresh. Mirrors the corpus layout:
/// `$ZODER_HOME/subscriptions/tiers.json` (where `$ZODER_HOME` is the same
/// `Config::home()` used by the rest of zoder-core), or the repo's bundled
/// `subscriptions/tiers.json` if `$ZODER_HOME` is unset. Returning `None` is
/// valid — the caller then falls back to the bundled default.
pub fn default_catalog_path(home: &Path) -> std::path::PathBuf {
    home.join("subscriptions").join("tiers.json")
}

/// The outcome of resolving a `SubscriptionPlan` against the catalog.
/// Distinct from a bare `Vec<QuotaWindow>` so callers (tests, reports) can
/// see *why* the windows are what they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveSource {
    /// No preset involved — the explicit `windows` were used as-is.
    Explicit,
    /// All windows came from the catalog preset.
    Preset { provider: String, tier: String },
    /// Started from the preset, then explicit windows with the same `name`
    /// overrode by `name` match.
    PresetWithOverrides {
        provider: String,
        tier: String,
        overridden: Vec<String>,
    },
    /// `tier` was set but did not resolve; explicit windows (if any) still
    /// used as the fallback. Always non-fatal — emitted alongside a warn.
    UnknownTier {
        provider: Option<String>,
        tier: String,
    },
}

/// Per-window provenance carried alongside a `ResolvedPlan`.
///
/// The resolver decides *for each* window whether it came from the catalog or
/// from the operator's config. Carrying this PER WINDOW — not flattened onto
/// the `ResolveSource` (which only describes the merge shape) — is what stops
/// the report from blanket-labeling operator-entered override windows and
/// appended "extra" windows as catalog-backed estimates. A `ResolvedPlan`
/// `windows` and `provenance` are guaranteed to be the same length and in the
/// same order.
///
/// `Clone` + `Eq`/`PartialEq` so tests can assert on it directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowProvenance {
    /// Window came from the catalog preset untouched. `confidence` is the
    /// catalog row's confidence; `source` is the catalog row's free-form
    /// provenance string (e.g. `"minimax_token_plan"`, `"observed"`).
    Catalog {
        confidence: Confidence,
        source: String,
    },
    /// Window came from the operator's explicit `windows` — either as an
    /// override of a same-named preset window, or as an "extra" window
    /// (declared in config but not present in the preset). Operator-entered
    /// windows are unlabeled for `confidence` (the operator is the source of
    /// truth) but carry a distinct `source = "operator"` so JSON consumers
    /// can filter/sort the report.
    Operator,
}

#[derive(Debug, Clone)]
pub struct ResolvedPlan {
    pub windows: Vec<QuotaWindow>,
    pub provenance: Vec<WindowProvenance>,
    pub source: ResolveSource,
}

/// Resolve a `SubscriptionPlan` against the catalog into the effective
/// `Vec<QuotaWindow>` the quota engine will measure against. See module docs
/// for the three input shapes and the resolution order. Pure: never mutates
/// `plan` or `catalog`. Tolerant: unknown tiers + missing catalog never throw.
///
/// The optional `provider_id` is required when `plan.tier` is set so the
/// resolver knows which provider's section of the catalog to look in. When
/// the caller already knows the provider (the normal `zoder providers` path
/// iterates `Config::providers`), pass it; tests that exercise the resolver
/// standalone can pass it explicitly.
pub fn resolve_plan_windows(
    plan: &SubscriptionPlan,
    catalog: &TierCatalog,
    provider_id: Option<&str>,
) -> ResolvedPlan {
    let tier_name = plan.tier.as_deref();
    // No `tier` set → explicit-only (today's behavior; unchanged). Every
    // window is operator-entered; provenance is all `Operator`.
    let Some(tier) = tier_name else {
        return ResolvedPlan {
            windows: plan.windows.clone(),
            provenance: plan
                .windows
                .iter()
                .map(|_| WindowProvenance::Operator)
                .collect(),
            source: ResolveSource::Explicit,
        };
    };

    // `tier` set; we need a provider to look the tier up. When the caller
    // didn't pass one, fall back to explicit windows (warn) — never invent a
    // provider id.
    let Some(provider) = provider_id else {
        tracing::warn!(
            tier = tier,
            "subscription plan has `tier` but no provider_id was supplied to the resolver; \
             falling back to explicit windows (set `provider` in the config or pass \
             the provider id explicitly)"
        );
        return ResolvedPlan {
            windows: plan.windows.clone(),
            provenance: plan
                .windows
                .iter()
                .map(|_| WindowProvenance::Operator)
                .collect(),
            source: ResolveSource::UnknownTier {
                provider: None,
                tier: tier.to_string(),
            },
        };
    };

    let Some(preset) = catalog.tier(provider, tier) else {
        tracing::warn!(
            provider = provider,
            tier = tier,
            "unknown subscription tier; falling back to explicit windows (if any) — \
             check the tier id against `subscriptions/tiers.json` or omit `tier`"
        );
        return ResolvedPlan {
            windows: plan.windows.clone(),
            provenance: plan
                .windows
                .iter()
                .map(|_| WindowProvenance::Operator)
                .collect(),
            source: ResolveSource::UnknownTier {
                provider: Some(provider.to_string()),
                tier: tier.to_string(),
            },
        };
    };

    // Preset is known. If the operator also declared explicit `windows`,
    // those override the preset's same-named windows. Windows in the
    // preset that have no explicit override survive untouched. Windows
    // declared explicitly that are NOT in the preset are appended as new
    // windows (handy for adding a "daily" window on top of the preset's
    // "5h" + "weekly").
    //
    // Provenance rule per window:
    //   - Preset window that was NOT overridden         → Catalog { … preset … }
    //   - Preset window that WAS overridden              → Operator
    //   - Explicit window that has NO preset counterpart → Operator (appended)
    if plan.windows.is_empty() {
        let windows: Vec<QuotaWindow> = preset
            .windows
            .iter()
            .cloned()
            .map(TierWindow::into_quota_window)
            .collect();
        let provenance: Vec<WindowProvenance> = preset
            .windows
            .iter()
            .map(|_| WindowProvenance::Catalog {
                confidence: preset.confidence,
                source: preset.source.clone(),
            })
            .collect();
        return ResolvedPlan {
            windows,
            provenance,
            source: ResolveSource::Preset {
                provider: provider.to_string(),
                tier: tier.to_string(),
            },
        };
    }

    // Merge: preset windows first, then explicit windows appended only when
    // their `name` doesn't collide. Override by name. The same merge governs
    // the per-window provenance: catalog-tagged until overridden, operator
    // for the override AND for any appended "extra" window.
    let mut merged: Vec<QuotaWindow> = Vec::with_capacity(preset.windows.len());
    let mut provenance: Vec<WindowProvenance> = Vec::with_capacity(preset.windows.len());
    let mut overridden: Vec<String> = Vec::new();
    for tw in &preset.windows {
        if let Some(ow) = plan.windows.iter().find(|w| w.name == tw.name) {
            merged.push(ow.clone());
            provenance.push(WindowProvenance::Operator);
            overridden.push(tw.name.clone());
        } else {
            merged.push(tw.clone().into_quota_window());
            provenance.push(WindowProvenance::Catalog {
                confidence: preset.confidence,
                source: preset.source.clone(),
            });
        }
    }
    for ow in &plan.windows {
        if !preset.windows.iter().any(|w| w.name == ow.name) {
            merged.push(ow.clone());
            provenance.push(WindowProvenance::Operator);
        }
    }

    ResolvedPlan {
        windows: merged,
        provenance,
        source: ResolveSource::PresetWithOverrides {
            provider: provider.to_string(),
            tier: tier.to_string(),
            overridden,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{QuotaUnit, QuotaWindow, SubscriptionPlan};

    fn w(name: &str, hours: u32, cap: f64, unit: QuotaUnit) -> QuotaWindow {
        QuotaWindow {
            name: name.into(),
            hours,
            unit,
            cap: Some(cap),
            models: None,
            observability: crate::config::Observability::default(),
            reset: crate::config::ResetKind::default(),
        }
    }

    fn tw(name: &str, hours: u32, cap: f64, unit: QuotaUnit) -> TierWindow {
        TierWindow {
            name: name.into(),
            hours,
            unit,
            cap: Some(cap),
            models: None,
            observability: crate::config::Observability::default(),
            reset: crate::config::ResetKind::default(),
        }
    }

    fn catalog_with_anthropic_max20x() -> TierCatalog {
        let mut providers = BTreeMap::new();
        let mut tiers = BTreeMap::new();
        tiers.insert(
            "claude-max-20x".into(),
            TierEntry {
                monthly_fee_usd: 200.0,
                confidence: Confidence::Observed,
                source: "observed".into(),
                windows: vec![
                    tw("5h", 5, 900.0, QuotaUnit::Messages),
                    tw("weekly", 168, 8000.0, QuotaUnit::Messages),
                ],
            },
        );
        providers.insert(
            "anthropic".into(),
            ProviderTiers {
                tiers: tiers.clone(),
            },
        );
        TierCatalog {
            version: 1,
            as_of: "2026-06-30".into(),
            disclaimer: "ESTIMATES".into(),
            providers,
        }
    }

    #[test]
    fn explicit_windows_unchanged_when_no_tier_set() {
        // Today behavior: `windows: [...]` is used as-is.
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![w("5h", 5, 100.0, QuotaUnit::Tokens)],
            tier: None,
        };
        let cat = catalog_with_anthropic_max20x();
        let r = resolve_plan_windows(&plan, &cat, Some("anthropic"));
        assert_eq!(r.source, ResolveSource::Explicit);
        assert_eq!(r.windows.len(), 1);
        assert_eq!(r.windows[0].cap, Some(100.0));
    }

    #[test]
    fn preset_alone_yields_catalog_windows() {
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![],
            tier: Some("claude-max-20x".into()),
        };
        let cat = catalog_with_anthropic_max20x();
        let r = resolve_plan_windows(&plan, &cat, Some("anthropic"));
        match r.source {
            ResolveSource::Preset { provider, tier } => {
                assert_eq!(provider, "anthropic");
                assert_eq!(tier, "claude-max-20x");
            }
            other => panic!("expected Preset, got {other:?}"),
        }
        assert_eq!(r.windows.len(), 2);
        let by_name: std::collections::HashMap<_, _> =
            r.windows.iter().map(|x| (x.name.as_str(), x)).collect();
        assert_eq!(by_name["5h"].cap, Some(900.0));
        assert_eq!(by_name["weekly"].cap, Some(8000.0));
        assert_eq!(by_name["5h"].hours, 5);
    }

    #[test]
    fn explicit_windows_override_preset_by_name_and_extras_are_appended() {
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![
                w("5h", 5, 1500.0, QuotaUnit::Messages), // override the preset's 900
                w("daily", 24, 500.0, QuotaUnit::Messages), // new window not in the preset
            ],
            tier: Some("claude-max-20x".into()),
        };
        let cat = catalog_with_anthropic_max20x();
        let r = resolve_plan_windows(&plan, &cat, Some("anthropic"));
        match r.source {
            ResolveSource::PresetWithOverrides {
                provider,
                tier,
                overridden,
            } => {
                assert_eq!(provider, "anthropic");
                assert_eq!(tier, "claude-max-20x");
                assert_eq!(overridden, vec!["5h".to_string()]);
            }
            other => panic!("expected PresetWithOverrides, got {other:?}"),
        }
        let by_name: std::collections::HashMap<_, _> =
            r.windows.iter().map(|x| (x.name.as_str(), x)).collect();
        // 5h: overridden.
        assert_eq!(by_name["5h"].cap, Some(1500.0));
        // weekly: untouched.
        assert_eq!(by_name["weekly"].cap, Some(8000.0));
        // daily: appended (not in the preset).
        assert_eq!(by_name["daily"].cap, Some(500.0));
        assert_eq!(by_name["daily"].hours, 24);
        assert_eq!(r.windows.len(), 3);
    }

    #[test]
    fn unknown_tier_falls_back_to_explicit_windows_without_throwing() {
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![w("5h", 5, 250.0, QuotaUnit::Messages)],
            tier: Some("claude-max-9999x-not-real".into()),
        };
        let cat = catalog_with_anthropic_max20x();
        let r = resolve_plan_windows(&plan, &cat, Some("anthropic"));
        match r.source {
            ResolveSource::UnknownTier { provider, tier } => {
                assert_eq!(provider.as_deref(), Some("anthropic"));
                assert_eq!(tier, "claude-max-9999x-not-real");
            }
            other => panic!("expected UnknownTier, got {other:?}"),
        }
        // Fallback: explicit windows survive untouched.
        assert_eq!(r.windows.len(), 1);
        assert_eq!(r.windows[0].cap, Some(250.0));
    }

    #[test]
    fn unknown_tier_with_no_explicit_windows_yields_empty() {
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![],
            tier: Some("does-not-exist".into()),
        };
        let cat = catalog_with_anthropic_max20x();
        let r = resolve_plan_windows(&plan, &cat, Some("anthropic"));
        match r.source {
            ResolveSource::UnknownTier { provider, tier } => {
                assert_eq!(provider.as_deref(), Some("anthropic"));
                assert_eq!(tier, "does-not-exist");
            }
            other => panic!("expected UnknownTier, got {other:?}"),
        }
        assert!(r.windows.is_empty());
    }

    #[test]
    fn unknown_provider_falls_back_to_explicit_windows() {
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![w("5h", 5, 1.0, QuotaUnit::Tokens)],
            tier: Some("claude-max-20x".into()),
        };
        let cat = catalog_with_anthropic_max20x();
        // provider_id is right (e.g. operator typo'd the provider id in their config).
        let r = resolve_plan_windows(&plan, &cat, Some("mysterycorp"));
        match r.source {
            ResolveSource::UnknownTier { provider, tier } => {
                assert_eq!(provider.as_deref(), Some("mysterycorp"));
                assert_eq!(tier, "claude-max-20x");
            }
            other => panic!("expected UnknownTier, got {other:?}"),
        }
        assert_eq!(r.windows.len(), 1);
        assert_eq!(r.windows[0].cap, Some(1.0));
    }

    #[test]
    fn missing_provider_id_with_tier_set_falls_back_gracefully() {
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![w("5h", 5, 7.0, QuotaUnit::Tokens)],
            tier: Some("claude-max-20x".into()),
        };
        let cat = catalog_with_anthropic_max20x();
        let r = resolve_plan_windows(&plan, &cat, None);
        match r.source {
            ResolveSource::UnknownTier { provider, tier } => {
                assert_eq!(provider, None);
                assert_eq!(tier, "claude-max-20x");
            }
            other => panic!("expected UnknownTier(None), got {other:?}"),
        }
        assert_eq!(r.windows.len(), 1);
    }

    #[test]
    fn empty_catalog_falls_back_to_explicit_windows() {
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![w("5h", 5, 42.0, QuotaUnit::Tokens)],
            tier: Some("claude-max-20x".into()),
        };
        let cat = TierCatalog::empty();
        let r = resolve_plan_windows(&plan, &cat, Some("anthropic"));
        match r.source {
            ResolveSource::UnknownTier { provider, tier } => {
                assert_eq!(provider.as_deref(), Some("anthropic"));
                assert_eq!(tier, "claude-max-20x");
            }
            other => panic!("expected UnknownTier, got {other:?}"),
        }
        assert_eq!(r.windows.len(), 1);
        assert_eq!(r.windows[0].cap, Some(42.0));
    }

    #[test]
    fn load_tier_catalog_prefers_on_disk_then_bundled() {
        // On-disk file exists and parses -> it wins.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiers.json");
        std::fs::write(
            &path,
            r#"{
                "version": 1,
                "as_of": "2030-01-01",
                "providers": {
                    "myprov": { "tiers": { "x": { "confidence": "published",
                        "source": "test", "windows": [] } } }
                }
            }"#,
        )
        .unwrap();
        let cat = load_tier_catalog(Some(&path));
        assert_eq!(cat.as_of, "2030-01-01");
        assert!(cat.providers.contains_key("myprov"));

        // On-disk file is unparseable -> bundled default (which has 3 providers).
        std::fs::write(&path, "not json {").unwrap();
        let cat = load_tier_catalog(Some(&path));
        assert!(
            !cat.providers.is_empty(),
            "bundled default should be non-empty"
        );

        // Path doesn't exist -> bundled default.
        let cat = load_tier_catalog(Some(&dir.path().join("does-not-exist.json")));
        assert!(!cat.providers.is_empty());

        // No path -> bundled default.
        let cat = load_tier_catalog(None);
        assert!(!cat.providers.is_empty());
    }

    #[test]
    fn bundled_default_is_well_formed_and_complete() {
        // The bundled default must parse cleanly AND contain every (provider,
        // tier) shape we ship.
        let cat = TierCatalog::bundled();
        assert_eq!(cat.version, 1);
        assert!(!cat.as_of.is_empty(), "as_of must be set");
        assert!(
            !cat.disclaimer.is_empty(),
            "disclaimer must be set on the bundled default"
        );
        for provider in ["anthropic", "openai", "minimax"] {
            let p = cat
                .providers
                .get(provider)
                .unwrap_or_else(|| panic!("bundled default must include provider {provider}"));
            assert!(
                !p.tiers.is_empty(),
                "bundled default must have at least one tier for {provider}"
            );
            for (tier_id, entry) in &p.tiers {
                assert!(
                    !entry.windows.is_empty(),
                    "{provider}/{tier_id} must declare at least one window"
                );
                for w in &entry.windows {
                    assert!(
                        w.cap.map(|c| c > 0.0).unwrap_or(true),
                        "{provider}/{tier_id}/{} cap must be > 0 when set",
                        w.name
                    );
                    assert!(
                        w.hours > 0,
                        "{provider}/{tier_id}/{} hours must be > 0",
                        w.name
                    );
                }
            }
        }
    }

    #[test]
    fn as_of_parsed_and_age_days_round_trip() {
        let cat = catalog_with_anthropic_max20x(); // as_of = "2026-06-30"
        cat.as_of_parsed().expect("as_of must parse");
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        assert_eq!(cat.age_days(today), Some(1));
        // Unset / unparseable -> None.
        let mut bad = cat.clone();
        bad.as_of = "not-a-date".into();
        assert_eq!(bad.as_of_parsed(), None);
        assert_eq!(bad.age_days(today), None);
    }

    #[test]
    fn tier_ids_returns_sorted_list() {
        let cat = catalog_with_anthropic_max20x();
        let ids = cat.tier_ids("anthropic");
        assert_eq!(ids, vec!["claude-max-20x".to_string()]);
        // Unknown provider -> empty.
        assert!(cat.tier_ids("nope").is_empty());
    }

    #[test]
    fn confidence_default_is_estimated_when_field_missing() {
        // A row without `confidence` should default to Estimated so a hand
        // edit that forgets the field never silently labels a guess as
        // observed/published.
        let json = r#"{
            "tiers": { "x": { "source": "s", "windows": [] } }
        }"#;
        let p: ProviderTiers = serde_json::from_str(json).unwrap();
        assert_eq!(p.tiers["x"].confidence, Confidence::Estimated);
    }

    // ----- KNEMON Layer 3B preset: minimax-max -----
    //
    // The MiniMax counter-fed path. MiniMax publishes no rate-limit
    // headers, so its utilization must be measured locally by counting
    // tokens. The `minimax-max` preset declares a monthly Counter
    // window (known cap, Counter observability, CalendarMonthly reset)
    // plus two PercentOnly windows (`5h`, `weekly`) whose caps are
    // unknown — the report will show the running token count but no
    // percent for those.

    #[test]
    fn minimax_max_preset_declares_counter_monthly_and_percent_only_others() {
        use crate::config::{Observability, QuotaUnit, ResetKind};
        let cat = TierCatalog::bundled();
        let entry = cat
            .tier("minimax", "minimax-max")
            .expect("minimax-max preset must exist in the bundled catalog");
        assert_eq!(entry.monthly_fee_usd, 200.0);

        // Index by name for direct assertions.
        let by_name: std::collections::HashMap<_, _> =
            entry.windows.iter().map(|w| (w.name.as_str(), w)).collect();
        assert_eq!(
            by_name.len(),
            3,
            "minimax-max must declare exactly 3 windows"
        );

        // Monthly: Counter, CalendarMonthly reset, known cap = 5.1e9.
        let m = by_name["monthly"];
        assert_eq!(m.hours, 720);
        assert_eq!(m.unit, QuotaUnit::Tokens);
        assert_eq!(m.cap, Some(5_100_000_000.0));
        assert_eq!(m.observability, Observability::Counter);
        assert_eq!(m.reset, ResetKind::CalendarMonthly);

        // 5h: PercentOnly, cap = None.
        let w5 = by_name["5h"];
        assert_eq!(w5.hours, 5);
        assert_eq!(w5.unit, QuotaUnit::Tokens);
        assert_eq!(w5.cap, None);
        assert_eq!(w5.observability, Observability::PercentOnly);

        // weekly: PercentOnly, cap = None.
        let ww = by_name["weekly"];
        assert_eq!(ww.hours, 168);
        assert_eq!(ww.unit, QuotaUnit::Tokens);
        assert_eq!(ww.cap, None);
        assert_eq!(ww.observability, Observability::PercentOnly);

        // Resolver path: a plan that names this tier must produce the
        // same three windows.
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![],
            tier: Some("minimax-max".into()),
        };
        let r = resolve_plan_windows(&plan, &cat, Some("minimax"));
        assert_eq!(r.windows.len(), 3);
        let resolved_by_name: std::collections::HashMap<_, _> = r
            .windows
            .iter()
            .map(|w| (w.name.as_str(), w.clone()))
            .collect();
        assert_eq!(
            resolved_by_name["monthly"].observability,
            Observability::Counter
        );
        assert_eq!(
            resolved_by_name["5h"].observability,
            Observability::PercentOnly
        );
        assert_eq!(
            resolved_by_name["weekly"].observability,
            Observability::PercentOnly
        );
    }

    // ----- per-window provenance tests (the BLOCKER fix) -----
    //
    // The resolver now attaches a `WindowProvenance` to every emitted window
    // so the report can distinguish catalog rows from operator-entered
    // overrides / extras. These tests assert each resolution shape carries
    // the correct per-window tag. The invariant `windows.len() ==
    // provenance.len()` must hold for every path through `resolve_plan_windows`.

    #[test]
    fn resolved_plan_explicit_windows_all_operator() {
        // No `tier` → all windows are hand-entered → all `Operator`.
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![
                w("5h", 5, 100.0, QuotaUnit::Messages),
                w("daily", 24, 50.0, QuotaUnit::Tokens),
            ],
            tier: None,
        };
        let cat = catalog_with_anthropic_max20x();
        let r = resolve_plan_windows(&plan, &cat, Some("anthropic"));
        assert_eq!(r.windows.len(), 2);
        assert_eq!(r.provenance.len(), 2);
        assert!(r
            .provenance
            .iter()
            .all(|p| matches!(p, WindowProvenance::Operator)));
    }

    #[test]
    fn resolved_plan_preset_alone_marks_every_window_catalog() {
        // Preset with no operator overrides → every window inherits the
        // catalog row's (confidence, source).
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![],
            tier: Some("claude-max-20x".into()),
        };
        let cat = catalog_with_anthropic_max20x();
        let r = resolve_plan_windows(&plan, &cat, Some("anthropic"));
        assert_eq!(r.windows.len(), 2);
        assert_eq!(r.provenance.len(), 2);
        // Catalog row is `(Observed, "observed")` per the fixture.
        for p in &r.provenance {
            match p {
                WindowProvenance::Catalog { confidence, source } => {
                    assert_eq!(*confidence, Confidence::Observed);
                    assert_eq!(source, "observed");
                }
                WindowProvenance::Operator => {
                    panic!("preset-only plan should not produce Operator rows")
                }
            }
        }
    }

    #[test]
    fn resolved_plan_preset_with_overrides_replaces_only_overridden_rows() {
        // Override on "5h" by name → that row switches to Operator; the
        // untouched "weekly" stays Catalog. "daily" is appended as Operator
        // (not in the preset). Critical: a single `PresetWithOverrides` plan
        // produces a MIXED provenance list — NOT a uniform one.
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![
                w("5h", 5, 1500.0, QuotaUnit::Messages),    // override
                w("daily", 24, 500.0, QuotaUnit::Messages), // append (extra)
            ],
            tier: Some("claude-max-20x".into()),
        };
        let cat = catalog_with_anthropic_max20x();
        let r = resolve_plan_windows(&plan, &cat, Some("anthropic"));
        assert_eq!(r.windows.len(), 3);
        assert_eq!(r.provenance.len(), 3);

        let by_name: std::collections::HashMap<_, _> = r
            .windows
            .iter()
            .zip(r.provenance.iter())
            .map(|(w, p)| (w.name.clone(), p.clone()))
            .collect();

        // 5h: overridden → Operator.
        assert_eq!(by_name["5h"], WindowProvenance::Operator);
        // weekly: untouched preset row → Catalog(observed, "observed").
        assert_eq!(
            by_name["weekly"],
            WindowProvenance::Catalog {
                confidence: Confidence::Observed,
                source: "observed".into(),
            }
        );
        // daily: appended → Operator (not a catalog row, even though the
        // preset's confidence is also Observed — that's the whole bug we're
        // fixing).
        assert_eq!(by_name["daily"], WindowProvenance::Operator);
    }

    #[test]
    fn resolved_plan_unknown_tier_marks_every_window_operator() {
        // No catalog row → every explicit window is operator-entered (we
        // can't inherit anything from a missing tier).
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![w("5h", 5, 250.0, QuotaUnit::Messages)],
            tier: Some("claude-max-9999x-not-real".into()),
        };
        let cat = catalog_with_anthropic_max20x();
        let r = resolve_plan_windows(&plan, &cat, Some("anthropic"));
        assert_eq!(r.windows.len(), 1);
        assert_eq!(r.provenance.len(), 1);
        assert_eq!(r.provenance[0], WindowProvenance::Operator);
    }

    #[test]
    fn resolved_plan_empty_windows_with_unknown_tier_yields_empty_provenance() {
        // No windows, unknown tier → both vecs empty (parallel length holds).
        let plan = SubscriptionPlan {
            monthly_fee_usd: 0.0,
            windows: vec![],
            tier: Some("does-not-exist".into()),
        };
        let cat = catalog_with_anthropic_max20x();
        let r = resolve_plan_windows(&plan, &cat, Some("anthropic"));
        assert!(r.windows.is_empty());
        assert!(r.provenance.is_empty());
    }

    #[test]
    fn resolved_plan_windows_and_provenance_are_parallel_for_every_shape() {
        // Smoke test for the invariant `windows.len() == provenance.len()`
        // across every resolution shape we care about. The runtime
        // `plan_usage` has a debug_assert on this; if it ever trips, the
        // catalog resolver regressed.
        let cases: Vec<(&str, SubscriptionPlan)> = vec![
            (
                "explicit",
                SubscriptionPlan {
                    monthly_fee_usd: 0.0,
                    windows: vec![w("5h", 5, 100.0, QuotaUnit::Tokens)],
                    tier: None,
                },
            ),
            (
                "preset",
                SubscriptionPlan {
                    monthly_fee_usd: 0.0,
                    windows: vec![],
                    tier: Some("claude-max-20x".into()),
                },
            ),
            (
                "preset+override",
                SubscriptionPlan {
                    monthly_fee_usd: 0.0,
                    windows: vec![
                        w("5h", 5, 1.0, QuotaUnit::Messages),     // override
                        w("daily", 24, 2.0, QuotaUnit::Messages), // append
                    ],
                    tier: Some("claude-max-20x".into()),
                },
            ),
        ];
        let cat = catalog_with_anthropic_max20x();
        for (label, plan) in cases {
            let r = resolve_plan_windows(&plan, &cat, Some("anthropic"));
            assert_eq!(
                r.windows.len(),
                r.provenance.len(),
                "length mismatch in {label} case: {} windows vs {} provenance",
                r.windows.len(),
                r.provenance.len(),
            );
        }
    }
}
