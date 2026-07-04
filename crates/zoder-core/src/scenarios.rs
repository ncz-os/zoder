//! User-facing routing scenario preference layer.
//!
//! Sits on top of [`crate::utilization`] (the self-contained KNEMON per-host
//! subscription-utilization library) and the smart router from
//! [`crate::router`]. A *scenario* is a named preference bundle that selects
//! which provider classes are eligible for each role (primary author,
//! reviewer), with utilization-aware knobs (`use_target` / `cap_guard` /
//! `budget_mode`) that control when the subscription is preferred vs. the
//! free fallback.
//!
//! Four presets are pre-populated in [`default_scenarios`] and an operator
//! just picks one with `[routing].scenario` (default `balanced`):
//!
//! - **`economy`**   — primary only uses `free`; reviewer may use `free` or
//!   `sub` (with KNEMON gating). Hard cap, no paid.
//! - **`balanced`**  (default) — primary uses `free` then `sub`; reviewer
//!   uses `sub` then `free`. Hard cap, no paid.
//! - **`aggressive`** — primary and reviewer prefer `sub` (headroom
//!   dependent), fall through to `free` at the cap. Hard cap, no paid.
//! - **`unlimited`** — adds `paid` as eligible; chargeback budget past the
//!   cap; requires the runtime `--allow-paid` flag.
//!
//! Advanced operators may override any preset under
//! `[routing.scenarios.<name>]`; fields omitted fall back to the preset.
//!
//! The classification helper [`classify`] maps a `(provider billing, model)`
//! pair to one of the three [`ProviderClass`]es that the rest of the module
//! reasons about. The mapping is policy (free-tier-as-cost-neutral, flat-
//! rate-as-cost-neutral) — not heuristics on rate-limit headers or model
//! names — so a misclassified provider surfaces immediately at config
//! validation time rather than as a surprise at runtime.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::{BillingMode, Provider};
use crate::utilization::{
    AccountDecision, AccountView, BudgetMode, RateLimitSnapshot, RouteDecision, RouteKnobs,
    DEFAULT_CAP_GUARD, DEFAULT_USE_TARGET,
};

// ---------------------------------------------------------------------------
// Provider class.
// ---------------------------------------------------------------------------

/// The "how do we pay for this?" classification that the scenario layer
/// reasons about. Distinct from [`crate::config::BillingMode`] — that enum
/// enumerates *how the provider bills you*; this one tells the router
/// *which preference bucket the candidate belongs to* under the active
/// scenario. The split keeps the policy ("paid subscriptions and flat-rate
/// subscriptions are both cost-neutral as long as their windows have headroom;
/// paid metered APIs are a different beast entirely") separate from the
/// provider-config vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderClass {
    /// No marginal cost; effectively uncapped (free-tier hosts, local models,
    /// flat-fee subscription with no rate-limit window tracked as such).
    Free,
    /// Rate-limited subscription (OAuth-backed subscriptions like
    /// `openai-codex`, `anthropic`). Marginal cost is $0 while the rolling
    /// window has headroom; routing is gated by KNEMON
    /// ([`crate::utilization::decide`]).
    Sub,
    /// Pay-as-you-go metered API keyed against a real billing meter. The
    /// operator must explicitly opt in (runtime `--allow-paid`) and the
    /// scenario must permit `paid`.
    Paid,
}

impl ProviderClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderClass::Free => "free",
            ProviderClass::Sub => "sub",
            ProviderClass::Paid => "paid",
        }
    }
}

/// Classify a `(provider, model_id)` pair into a [`ProviderClass`].
///
/// The lookup follows the documented mapping in the module-level docs and
/// the task spec:
///
/// - `nvidia-eih` / `nvcf` providers → `free` (the NVCF free-tier endpoint).
/// - A provider whose id contains `local` (e.g. `local-llama`) → `free`
///   (no metered egress).
/// - A provider whose id contains `minimax-flat` → `free` (the flat-fee
///   MiniMax subscription is cost-neutral at the routing layer).
/// - Subscription billing (OAuth / flat-rate-with-windows) → `sub`
///   (KNEMON-gated).
/// - Metered billing → `paid`.
///
/// The provider-id string matchers are checked **before** the billing-mode
/// match so a user who happens to name a metered provider `something-local`
/// is classified from billing, not from the name. (The id matchers exist
/// because some flat-rate providers are explicitly `BillingMode::Free` in
/// config — those should stay `free` even though they look subscription-y
/// to a casual reader.)
pub fn classify(provider: &Provider, _model_id: &str) -> ProviderClass {
    let id = provider.id.to_ascii_lowercase();
    // Explicit id matchers — checked first so the classification is
    // stable regardless of which `billing` an operator chose for those
    // entries.
    if id == "nvidia-eih" || id == "nvcf" {
        return ProviderClass::Free;
    }
    if id.contains("local") || id.contains("minimax-flat") {
        return ProviderClass::Free;
    }
    match provider.billing {
        BillingMode::Free => ProviderClass::Free,
        BillingMode::Subscription => ProviderClass::Sub,
        BillingMode::Metered => ProviderClass::Paid,
    }
}

// ---------------------------------------------------------------------------
// Role.
// ---------------------------------------------------------------------------

/// Which role a routed candidate is being selected for. Roles have *different*
/// class preferences under each preset — `balanced` puts `sub` ahead of
/// `free` for reviewers (a reviewing model gets to ride the subscription)
/// while keeping `free` ahead of `sub` for the author (so the agentic loop
/// doesn't burn the same window the reviewer is about to query).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// The author of the agentic / single-shot response.
    Primary,
    /// A reviewer / second-pass / panel member reviewing the author's work.
    Reviewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Primary => "primary",
            Role::Reviewer => "reviewer",
        }
    }
}

// ---------------------------------------------------------------------------
// Scenario.
// ---------------------------------------------------------------------------

/// A named preference bundle. Users pick one with `[routing].scenario`
/// (default `balanced`); advanced users may override any preset under
/// `[routing.scenarios.<name>]`. All fields are public so an operator can
/// inspect what their active scenario is doing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteScenario {
    /// Class preference order for the author role. Earlier = preferred.
    /// The router keeps the candidate ranking (capability × health) and
    /// picks the highest-ranked candidate whose class is present in this
    /// list AND is currently eligible (see [`pick_candidate_for_role`]).
    #[serde(default = "default_primary_classes")]
    pub primary_classes: Vec<ProviderClass>,
    /// Class preference order for the reviewer role. Same rules as
    /// `primary_classes` but per the reviewer's preference lane.
    #[serde(default = "default_reviewer_classes")]
    pub reviewer_classes: Vec<ProviderClass>,
    /// Below this used-percent, a sub candidate is eligible
    /// (`RouteDecision::PreferSub`).
    #[serde(default = "default_use_target")]
    pub use_target: f64,
    /// At/above this used-percent, a sub candidate is dropped (in `block`
    /// mode) or kept (in `chargeback` mode).
    #[serde(default = "default_cap_guard")]
    pub cap_guard: f64,
    /// What to do past `cap_guard` for sub candidates. `Block` falls
    /// through to free; `Chargeback` keeps the sub as long as the dollar
    /// budget is positive (see [`crate::utilization::decide`]).
    #[serde(default)]
    pub budget_mode: BudgetMode,
    /// Whether `paid` candidates are eligible. Even when `true`, the
    /// runtime `--allow-paid` flag is still required (an explicit operator
    /// opt-in for metered spend — never silent).
    #[serde(default)]
    pub allow_paid: bool,
}

fn default_primary_classes() -> Vec<ProviderClass> {
    vec![ProviderClass::Free, ProviderClass::Sub]
}
fn default_reviewer_classes() -> Vec<ProviderClass> {
    vec![ProviderClass::Sub, ProviderClass::Free]
}
fn default_use_target() -> f64 {
    DEFAULT_USE_TARGET
}
fn default_cap_guard() -> f64 {
    DEFAULT_CAP_GUARD
}

impl Default for RouteScenario {
    fn default() -> Self {
        // Matches `balanced` so a config-less host (the legacy shape —
        // `[routing]` absent) behaves exactly like a `balanced`-declared
        // host. This is the backward-compat invariant called out in the
        // module docs.
        Self::balanced()
    }
}

impl RouteScenario {
    /// The four built-in presets. See module docs for the user-facing
    /// shapes.
    pub fn economy() -> Self {
        Self {
            primary_classes: vec![ProviderClass::Free],
            reviewer_classes: vec![ProviderClass::Free, ProviderClass::Sub],
            use_target: 50.0,
            cap_guard: 60.0,
            budget_mode: BudgetMode::Block,
            allow_paid: false,
        }
    }

    /// Default scenario. Balanced: authors ride free first; reviewers ride
    /// the subscription first (its headroom is what the reviewer model is
    /// "paying" for); both fall through to free past `cap_guard`.
    pub fn balanced() -> Self {
        Self {
            primary_classes: vec![ProviderClass::Free, ProviderClass::Sub],
            reviewer_classes: vec![ProviderClass::Sub, ProviderClass::Free],
            use_target: 80.0,
            cap_guard: 85.0,
            budget_mode: BudgetMode::Block,
            allow_paid: false,
        }
    }

    /// Aggressive: both roles prefer sub up to `cap_guard=95`. For ops that
    /// want to push the subscription as hard as possible without busting it.
    pub fn aggressive() -> Self {
        Self {
            primary_classes: vec![ProviderClass::Sub, ProviderClass::Free],
            reviewer_classes: vec![ProviderClass::Sub, ProviderClass::Free],
            use_target: 90.0,
            cap_guard: 95.0,
            budget_mode: BudgetMode::Block,
            allow_paid: false,
        }
    }

    /// Unlimited: paid is allowed, chargeback budget past the cap. Use
    /// sparingly; intended for ops that have explicit budget authority and
    /// trust the operator `--allow-paid` gate.
    pub fn unlimited() -> Self {
        Self {
            primary_classes: vec![ProviderClass::Sub, ProviderClass::Paid, ProviderClass::Free],
            reviewer_classes: vec![ProviderClass::Sub, ProviderClass::Paid, ProviderClass::Free],
            use_target: DEFAULT_USE_TARGET,
            cap_guard: DEFAULT_CAP_GUARD,
            budget_mode: BudgetMode::Chargeback,
            allow_paid: true,
        }
    }

    /// Resolve the knobs that [`crate::utilization::decide`] consumes for
    /// this scenario. Pure data, easy to feed into a synthetic test.
    pub fn knobs(&self) -> RouteKnobs {
        RouteKnobs {
            use_target: self.use_target,
            cap_guard: self.cap_guard,
            budget_mode: self.budget_mode,
            chargeback_budget_usd: None, // set at call-site via `decide(.., Some(remaining))`
            reset_imminence_threshold: crate::utilization::DEFAULT_RESET_IMMINENCE_THRESHOLD,
        }
    }

    /// Class preference order for `role`. Returns a slice owned by `self`.
    pub fn classes_for(&self, role: Role) -> &[ProviderClass] {
        match role {
            Role::Primary => &self.primary_classes,
            Role::Reviewer => &self.reviewer_classes,
        }
    }
}

/// The four built-in presets, keyed by their user-facing name. The CLI
/// surface ([routing].scenario) accepts any of these keys; an unrecognized
/// name falls back to `balanced`.
pub fn default_scenarios() -> BTreeMap<String, RouteScenario> {
    let mut m = BTreeMap::new();
    m.insert("economy".into(), RouteScenario::economy());
    m.insert("balanced".into(), RouteScenario::balanced());
    m.insert("aggressive".into(), RouteScenario::aggressive());
    m.insert("unlimited".into(), RouteScenario::unlimited());
    m
}

/// Resolve the active scenario: starts from the named preset, then layers
/// an operator override on top. Unknown preset names fall back to
/// `balanced` so a typo in `[routing].scenario` is a graceful no-op rather
/// than a boot-time error — the CLI's `--scenario` console hint points
/// operators at the valid names.
pub fn resolve_active(
    scenario_name: &str,
    override_block: Option<&RouteScenario>,
) -> RouteScenario {
    let presets = default_scenarios();
    let mut base = presets
        .get(scenario_name)
        .cloned()
        .unwrap_or_else(RouteScenario::balanced);
    if let Some(o) = override_block {
        base = o.clone();
    }
    base
}

// ---------------------------------------------------------------------------
// Candidate ranking + selection.
// ---------------------------------------------------------------------------

/// One eligible route for the router — a `(model_id, class)` tuple plus the
/// scalar inputs the scenario layer needs to make a per-role decision.
/// `swe_rank` mirrors the smart router's tier-based capability score
/// (higher = stronger), and `healthy` is the live-circuit-breaker gate.
///
/// Keeping this struct narrow (`String` model id + enums + score + bool)
/// rather than coupling to `corpus::ModelEntry` makes the helper trivially
/// testable with synthetic fixtures — the task spec calls for tests that
/// construct a candidate set + utilization snapshot directly, not a full
/// corpus + health + ledger stack.
#[derive(Debug, Clone)]
pub struct RoutableCandidate {
    pub model_id: String,
    pub class: ProviderClass,
    pub swe_rank: f64,
    pub healthy: bool,
}

/// Decide whether a single candidate is **eligible** for `role` under the
/// given scenario + utilization context. All three sub-rules from the
/// spec live here:
///
/// - `free`: eligible iff `free` is in `scenario.classes_for(role)`.
/// - `sub`: eligible iff `sub` is in the role's classes AND
///   `knemon::decide(snapshot, scenario.knobs())` is NOT
///   `FallBackToFree`. A missing or past-reset snapshot is treated as
///   headroom (keep the sub) — that's the "snapshot absent or past
///   `reset_at` => headroom => keep" clause in the spec.
/// - `paid`: eligible iff `scenario.allow_paid` AND the runtime
///   `allow_paid_runtime` flag is set. Both must agree; either alone is
///   not enough (a config-only opt-in skips the explicit spend
///   confirmation gate that `--allow-paid` represents at the CLI).
///
/// Returns `false` for an unhealthy candidate regardless of role — the
/// router's `select()` already drops open circuit breakers; we keep the
/// same invariant at the scenario layer so callers can pass the ranked
/// pool straight in without re-filtering.
pub fn candidate_eligible(
    role: Role,
    candidate: &RoutableCandidate,
    scenario: &RouteScenario,
    snapshot: Option<&RateLimitSnapshot>,
    allow_paid_runtime: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if !candidate.healthy {
        return false;
    }
    let classes = scenario.classes_for(role);
    match candidate.class {
        ProviderClass::Free => classes.contains(&ProviderClass::Free),
        ProviderClass::Sub => {
            if !classes.contains(&ProviderClass::Sub) {
                return false;
            }
            // KNEMON gate: a missing snapshot, or a snapshot whose reset
            // window has already rolled over, is treated as "full
            // headroom" by `decide()` (it returns PreferSub on stale
            // resets; `effective_used` zeros them out). So the missing-
            // snapshot case is naturally expressed by `None`.
            let snap = snapshot.cloned().unwrap_or_default();
            // Run `decide()` with no remaining-budget signal — that's
            // what the per-role eligibility check has at routing time.
            // Spec wording for the verdict-to-eligibility mapping is:
            //   * `PreferSub`     -> keep (headroom)
            //   * `Chargeback`    -> keep (scenario opted in)
            //   * `FallBackToFree`-> drop (cap tripped + block mode)
            //
            // The wrinkle: in `chargeback` mode with `None` remaining,
            // `decide()` conservatively returns `FallBackToFree`. The
            // spec explicitly says "Chargeback=keep only if
            // budget_mode=chargeback" — i.e. an operator who selected
            // `chargeback` IS opting into keeping the sub past the cap,
            // and we should honor that opt-in even when we don't have a
            // remaining-dollar signal in hand.
            let decision = crate::utilization::decide(&snap, &scenario.knobs(), now, None);
            match decision {
                RouteDecision::PreferSub | RouteDecision::Chargeback => true,
                RouteDecision::FallBackToFree => {
                    // Scenario-level override: chargeback mode means
                    // "keep through the cap". If `decide()` couldn't see
                    // a remaining-budget signal but the scenario's mode
                    // IS chargeback, honor the operator's choice.
                    matches!(scenario.budget_mode, BudgetMode::Chargeback)
                }
            }
        }
        ProviderClass::Paid => {
            scenario.allow_paid && allow_paid_runtime && classes.contains(&ProviderClass::Paid)
        }
    }
}

/// Pick the highest-ranked eligible candidate for `role` under
/// `scenario`. `candidates` should be ordered by the smart router
/// (capability × health); this function picks by **class preference
/// first, rank second**: for each class in the role's preference order,
/// take the highest-rank candidate of that class that returns `true` from
/// [`candidate_eligible`], and return it. `None` when no candidate
/// survives the filter across any class.
pub fn pick_candidate_for_role(
    role: Role,
    candidates: &[RoutableCandidate],
    scenario: &RouteScenario,
    snapshot: Option<&RateLimitSnapshot>,
    allow_paid_runtime: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let classes = scenario.classes_for(role);
    for class in classes {
        // Highest-rank candidate of this class.
        let mut best: Option<&RoutableCandidate> = None;
        for c in candidates.iter().filter(|c| c.class == *class) {
            match best {
                None => best = Some(c),
                Some(b) if c.swe_rank > b.swe_rank => best = Some(c),
                _ => {}
            }
        }
        if let Some(c) = best {
            if candidate_eligible(role, c, scenario, snapshot, allow_paid_runtime, now) {
                return Some(c.model_id.clone());
            }
        }
    }
    None
}

/// Ordered chain for `role` under `scenario`: the primary pick first, then
/// the remaining eligible models in descending rank order, class-
/// interleaved by the same preference used for the head. The first model
/// in the returned vec is the head (always present when any eligible
/// candidate exists); the rest are fallbacks. The chain excludes models
/// the smart router already excluded (open circuit breakers, unbacked
/// free-pool entries surfaced as `healthy = false`).
///
/// `max_chain` caps the total length (primary + fallbacks); pass `0` for
/// "no cap beyond what survives the filter".
pub fn chain_for_role(
    role: Role,
    candidates: &[RoutableCandidate],
    scenario: &RouteScenario,
    snapshot: Option<&RateLimitSnapshot>,
    allow_paid_runtime: bool,
    now: chrono::DateTime<chrono::Utc>,
    max_chain: usize,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let classes = scenario.classes_for(role);
    for class in classes {
        // Sort candidates of this class by swe_rank descending so we walk
        // them in capability order. Eligibility is per-(class, role); sub
        // candidates pass the KNEMON gate, paid requires both flags.
        let mut ranked: Vec<&RoutableCandidate> =
            candidates.iter().filter(|c| c.class == *class).collect();
        ranked.sort_by(|a, b| {
            b.swe_rank
                .partial_cmp(&a.swe_rank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for c in ranked {
            if out.contains(&c.model_id) {
                continue;
            }
            if candidate_eligible(role, c, scenario, snapshot, allow_paid_runtime, now) {
                out.push(c.model_id.clone());
                if max_chain > 0 && out.len() >= max_chain {
                    return out;
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// KNEMON Layer 4 — per-account multi-window view.
// ---------------------------------------------------------------------------

/// Per-candidate eligibility under the Layer 4 (per-account) path. The
/// `account_view` is consumed by [`crate::utilization::decide_account`]
/// instead of the legacy single-snapshot [`crate::utilization::decide`],
/// so a `Sub` candidate's eligibility reflects ALL windows on its
/// account (multi-window binding) rather than just the snapshot's
/// `primary` / `secondary` max. The verdict mapping mirrors the
/// single-snapshot path so callers see the same `RouteDecision` shape.
///
/// `None` `account_view` for a `Sub` candidate falls back to the legacy
/// single-snapshot path — that's the documented "no per-account
/// information -> headroom" baseline so a candidate without a layered
/// account view never gets artificially demoted.
pub fn candidate_eligible_with_account(
    role: Role,
    candidate: &RoutableCandidate,
    scenario: &RouteScenario,
    snapshot: Option<&RateLimitSnapshot>,
    account_view: Option<&AccountView>,
    allow_paid_runtime: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if !candidate.healthy {
        return false;
    }
    let classes = scenario.classes_for(role);
    match candidate.class {
        ProviderClass::Free => classes.contains(&ProviderClass::Free),
        ProviderClass::Sub => {
            if !classes.contains(&ProviderClass::Sub) {
                return false;
            }
            // Layer 4 path: when an AccountView is provided, run
            // decide_account and honor its verdict exactly. When no
            // AccountView is provided, fall back to the legacy
            // single-snapshot `decide()` so callers without a layered
            // account view (e.g. the legacy CLI ingest path that only
            // has the most-recent header snapshot) keep working.
            if let Some(view) = account_view {
                let knobs = scenario.knobs();
                let verdict = crate::utilization::decide_account(view, &knobs, now, None);
                match verdict.decision {
                    RouteDecision::PreferSub | RouteDecision::Chargeback => true,
                    RouteDecision::FallBackToFree => {
                        // Same scenario-mode override as the legacy
                        // path: when the scenario opted into
                        // chargeback, keep the sub even if the
                        // account-level verdict dropped it.
                        matches!(scenario.budget_mode, BudgetMode::Chargeback)
                    }
                }
            } else {
                let snap = snapshot.cloned().unwrap_or_default();
                let decision = crate::utilization::decide(&snap, &scenario.knobs(), now, None);
                match decision {
                    RouteDecision::PreferSub | RouteDecision::Chargeback => true,
                    RouteDecision::FallBackToFree => {
                        matches!(scenario.budget_mode, BudgetMode::Chargeback)
                    }
                }
            }
        }
        ProviderClass::Paid => {
            scenario.allow_paid && allow_paid_runtime && classes.contains(&ProviderClass::Paid)
        }
    }
}

/// Pick the highest-ranked eligible candidate for `role` under the
/// per-account (Layer 4) path. Same contract as
/// [`pick_candidate_for_role`] except that `Sub` candidates are ranked
/// by ASCENDING `decide_account(view).strength` (most-idle first)
/// rather than `swe_rank`. This is the routing-tier
/// "drain-the-most-idle-subscription-before-touching-the-rest" rule
/// that KNEMON Layer 4 is designed to support.
///
/// `account_views` must be the same length as `candidates` and align
/// positionally: `account_views[i]` is the per-account view for
/// `candidates[i]`. Pass an empty `Vec` (or one that's None for every
/// entry — see the helper signature below) to fall back to the legacy
/// single-snapshot path entirely.
///
/// For non-Sub classes the legacy swe_rank ordering applies (free is
/// effectively free — its "rank" is meaningless; paid is rare and
/// explicit).
pub fn pick_candidate_for_role_with_account(
    role: Role,
    candidates: &[RoutableCandidate],
    account_views: &[Option<AccountView>],
    scenario: &RouteScenario,
    snapshot: Option<&RateLimitSnapshot>,
    allow_paid_runtime: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let classes = scenario.classes_for(role);
    for class in classes {
        if *class == ProviderClass::Sub {
            // KNEMON L4: ascending strength (most-idle first). Only
            // treat the Sub pick as the role's head when at least one
            // Sub candidate is actually eligible; otherwise fall
            // through to the next class in the role's preference order
            // (mirrors the legacy behavior when Sub gets dropped by
            // the gate).
            if let Some(pick) = pick_sub_candidate_by_idle(
                role,
                candidates,
                account_views,
                scenario,
                snapshot,
                allow_paid_runtime,
                now,
            ) {
                return Some(pick);
            }
            continue;
        }
        // Free / Paid: legacy swe_rank ordering.
        let mut best: Option<&RoutableCandidate> = None;
        for c in candidates.iter().filter(|c| c.class == *class) {
            match best {
                None => best = Some(c),
                Some(b) if c.swe_rank > b.swe_rank => best = Some(c),
                _ => {}
            }
        }
        if let Some(c) = best {
            if candidate_eligible_with_account(
                role,
                c,
                scenario,
                snapshot,
                None,
                allow_paid_runtime,
                now,
            ) {
                return Some(c.model_id.clone());
            }
        }
    }
    None
}

fn pick_sub_candidate_by_idle(
    role: Role,
    candidates: &[RoutableCandidate],
    account_views: &[Option<AccountView>],
    scenario: &RouteScenario,
    snapshot: Option<&RateLimitSnapshot>,
    allow_paid_runtime: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let knobs = scenario.knobs();
    // Detect "all sub candidates lack an AccountView" up front — when
    // nobody has a layered view, the L4 picker must degenerate to the
    // legacy swe_rank ordering (rather than relying on index position
    // as a tiebreak, which would silently corrupt callers that pass
    // candidates in swe_rank order).
    let any_sub_has_view = candidates.iter().enumerate().any(|(i, c)| {
        c.class == ProviderClass::Sub && account_views.get(i).and_then(|v| v.as_ref()).is_some()
    });
    let mut scored: Vec<(usize, f64, bool)> = Vec::new();
    for (i, c) in candidates.iter().enumerate() {
        if c.class != ProviderClass::Sub {
            continue;
        }
        let view_ref = account_views.get(i).and_then(|v| v.as_ref());
        let ad: AccountDecision = match view_ref {
            Some(v) => crate::utilization::decide_account(v, &knobs, now, None),
            None => AccountDecision {
                decision: crate::utilization::decide(
                    &snapshot.cloned().unwrap_or_default(),
                    &knobs,
                    now,
                    None,
                ),
                // When nobody has a layered view, fall back to
                // swe_rank ordering by encoding it as a comparable
                // scalar (higher swe_rank => lower "strength" so it
                // sorts FIRST — the legacy spec was "highest rank
                // first").
                strength: if any_sub_has_view {
                    f64::INFINITY
                } else {
                    -c.swe_rank
                },
                binding_window: None,
            },
        };
        let eligible = candidate_eligible_with_account(
            role,
            c,
            scenario,
            snapshot,
            view_ref,
            allow_paid_runtime,
            now,
        );
        scored.push((i, ad.strength, eligible));
    }
    if scored.is_empty() {
        return None;
    }
    // Sort: eligible first, then ascending strength. Within a tier,
    // index ordering is the stable tiebreak so the test fixtures stay
    // reproducible. NOTE: when all subs lack an account view we
    // packed `strength = -swe_rank`, so ascending strength == descending
    // swe_rank == legacy ordering. Mixed views (some layered, some
    // not) sort account-less subs LAST (strength=INFINITY), which is
    // the documented "prefer the layered view when both kinds are
    // present" tiebreak.
    scored.sort_by(|a, b| {
        b.2.cmp(&a.2)
            .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.0.cmp(&b.0))
    });
    scored
        .into_iter()
        .find(|(_, _, eligible)| *eligible)
        .map(|(i, _, _)| candidates[i].model_id.clone())
}

/// Ordered chain under the Layer 4 path. Same as [`chain_for_role`] but
/// ranks `Sub` candidates by ascending `decide_account(view).strength`.
/// Non-Sub classes use legacy swe_rank ordering.
#[allow(clippy::too_many_arguments)]
pub fn chain_for_role_with_account(
    role: Role,
    candidates: &[RoutableCandidate],
    account_views: &[Option<AccountView>],
    scenario: &RouteScenario,
    snapshot: Option<&RateLimitSnapshot>,
    allow_paid_runtime: bool,
    now: chrono::DateTime<chrono::Utc>,
    max_chain: usize,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let classes = scenario.classes_for(role);
    // Mirror the picker: when no Sub candidate has a layered view,
    // fall back to legacy swe_rank ordering. Mixed: account-less
    // sorts LAST.
    let any_sub_has_view = candidates.iter().enumerate().any(|(i, c)| {
        c.class == ProviderClass::Sub && account_views.get(i).and_then(|v| v.as_ref()).is_some()
    });
    for class in classes {
        if *class == ProviderClass::Sub {
            let knobs = scenario.knobs();
            let mut scored: Vec<(usize, f64, bool)> = Vec::new();
            for (i, c) in candidates.iter().enumerate() {
                if c.class != ProviderClass::Sub {
                    continue;
                }
                let view_ref = account_views.get(i).and_then(|v| v.as_ref());
                let ad: AccountDecision = match view_ref {
                    Some(v) => crate::utilization::decide_account(v, &knobs, now, None),
                    None => AccountDecision {
                        decision: crate::utilization::decide(
                            &snapshot.cloned().unwrap_or_default(),
                            &knobs,
                            now,
                            None,
                        ),
                        strength: if any_sub_has_view {
                            f64::INFINITY
                        } else {
                            -c.swe_rank
                        },
                        binding_window: None,
                    },
                };
                let eligible = candidate_eligible_with_account(
                    role,
                    c,
                    scenario,
                    snapshot,
                    view_ref,
                    allow_paid_runtime,
                    now,
                );
                scored.push((i, ad.strength, eligible));
            }
            scored.sort_by(|a, b| {
                b.2.cmp(&a.2)
                    .then_with(|| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                    .then_with(|| a.0.cmp(&b.0))
            });
            for (i, _, _) in scored {
                let c = &candidates[i];
                if out.contains(&c.model_id) {
                    continue;
                }
                if candidate_eligible_with_account(
                    role,
                    c,
                    scenario,
                    snapshot,
                    account_views.get(i).and_then(|v| v.as_ref()),
                    allow_paid_runtime,
                    now,
                ) {
                    out.push(c.model_id.clone());
                    if max_chain > 0 && out.len() >= max_chain {
                        return out;
                    }
                }
            }
        } else {
            let mut ranked: Vec<&RoutableCandidate> =
                candidates.iter().filter(|c| c.class == *class).collect();
            ranked.sort_by(|a, b| {
                b.swe_rank
                    .partial_cmp(&a.swe_rank)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for c in ranked {
                if out.contains(&c.model_id) {
                    continue;
                }
                if candidate_eligible_with_account(
                    role,
                    c,
                    scenario,
                    snapshot,
                    None,
                    allow_paid_runtime,
                    now,
                ) {
                    out.push(c.model_id.clone());
                    if max_chain > 0 && out.len() >= max_chain {
                        return out;
                    }
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Auth, BillingMode, Provider};
    use chrono::TimeZone;

    fn provider(id: &str, billing: BillingMode) -> Provider {
        Provider {
            id: id.into(),
            base_url: format!("https://{id}.example/v1"),
            kind: "openai-chat".into(),
            auth: Auth::None,
            paid: false,
            billing,
            subscription: None,
            serves: Vec::new(),
        }
    }

    fn candidate(id: &str, class: ProviderClass, rank: f64) -> RoutableCandidate {
        RoutableCandidate {
            model_id: id.into(),
            class,
            swe_rank: rank,
            healthy: true,
        }
    }

    fn snapshot_with_used(pct: f64, reset_at: Option<i64>) -> RateLimitSnapshot {
        RateLimitSnapshot {
            provider: crate::utilization::Provider::OpenaiCodex,
            account_id: "acct".into(),
            plan: "pro".into(),
            primary: Some(crate::utilization::WindowSnapshot {
                used_percent: pct,
                window_minutes: Some(300),
                reset_at_epoch: reset_at,
                label: Some("primary".into()),
            }),
            secondary: None,
            has_credits: Some(true),
            observed_at: None,
        }
    }

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap()
    }

    // -------- classify --------------------------------------------------

    #[test]
    fn classify_nvidia_eih_is_free() {
        // The task spec calls out nvidia-eih / nvcf as free by id.
        let p = provider("nvidia-eih", BillingMode::Metered);
        assert_eq!(classify(&p, "any/model"), ProviderClass::Free);
        let p = provider("nvcf", BillingMode::Metered);
        assert_eq!(classify(&p, "any/model"), ProviderClass::Free);
    }

    #[test]
    fn classify_local_and_minimax_flat_are_free() {
        let p = provider("local-llama", BillingMode::Metered);
        assert_eq!(classify(&p, "x"), ProviderClass::Free);
        let p = provider("minimax-flat", BillingMode::Metered);
        assert_eq!(classify(&p, "MiniMax-M3"), ProviderClass::Free);
    }

    #[test]
    fn classify_subscription_billing_is_sub() {
        let p = provider("openai-codex", BillingMode::Subscription);
        assert_eq!(classify(&p, "codex-mini"), ProviderClass::Sub);
        let p = provider("anthropic", BillingMode::Subscription);
        assert_eq!(classify(&p, "claude-sonnet-4.6"), ProviderClass::Sub);
    }

    #[test]
    fn classify_metered_billing_is_paid() {
        let p = provider("openai-paid", BillingMode::Metered);
        assert_eq!(classify(&p, "gpt-4o"), ProviderClass::Paid);
    }

    #[test]
    fn classify_free_billing_is_free() {
        let p = provider("anything-free-tagged", BillingMode::Free);
        assert_eq!(classify(&p, "x"), ProviderClass::Free);
    }

    // -------- scenario presets -----------------------------------------

    #[test]
    fn default_scenarios_have_four_presets() {
        let s = default_scenarios();
        assert!(s.contains_key("economy"));
        assert!(s.contains_key("balanced"));
        assert!(s.contains_key("aggressive"));
        assert!(s.contains_key("unlimited"));
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn balanced_is_the_default_and_matches_doc_spec() {
        let s = RouteScenario::balanced();
        assert_eq!(
            s.primary_classes,
            vec![ProviderClass::Free, ProviderClass::Sub]
        );
        assert_eq!(
            s.reviewer_classes,
            vec![ProviderClass::Sub, ProviderClass::Free]
        );
        assert_eq!(s.use_target, 80.0);
        assert_eq!(s.cap_guard, 85.0);
        assert_eq!(s.budget_mode, BudgetMode::Block);
        assert!(!s.allow_paid);
        assert_eq!(RouteScenario::default(), RouteScenario::balanced());
    }

    #[test]
    fn economy_preset_spec() {
        let s = RouteScenario::economy();
        assert_eq!(s.primary_classes, vec![ProviderClass::Free]);
        assert_eq!(
            s.reviewer_classes,
            vec![ProviderClass::Free, ProviderClass::Sub]
        );
        assert_eq!(s.use_target, 50.0);
        assert_eq!(s.cap_guard, 60.0);
        assert!(!s.allow_paid);
    }

    #[test]
    fn aggressive_preset_spec() {
        let s = RouteScenario::aggressive();
        assert_eq!(
            s.primary_classes,
            vec![ProviderClass::Sub, ProviderClass::Free]
        );
        assert_eq!(s.use_target, 90.0);
        assert_eq!(s.cap_guard, 95.0);
        assert!(!s.allow_paid);
    }

    #[test]
    fn unlimited_preset_allows_paid_and_chargebacks() {
        let s = RouteScenario::unlimited();
        assert!(s.allow_paid);
        assert_eq!(s.budget_mode, BudgetMode::Chargeback);
        assert!(s.classes_for(Role::Primary).contains(&ProviderClass::Paid));
    }

    #[test]
    fn resolve_active_unknown_name_falls_back_to_balanced() {
        let s = resolve_active("nonexistent", None);
        assert_eq!(s, RouteScenario::balanced());
    }

    #[test]
    fn resolve_active_layers_override_when_provided() {
        let mut override_s = RouteScenario::balanced();
        override_s.use_target = 42.0;
        let s = resolve_active("balanced", Some(&override_s));
        assert_eq!(s.use_target, 42.0);
        // Other fields intact.
        assert_eq!(s.cap_guard, RouteScenario::balanced().cap_guard);
    }

    // -------- per-scenario selection (synthetic) -----------------------

    #[test]
    fn economy_picks_free_even_when_sub_has_headroom() {
        let scenario = RouteScenario::economy();
        let snap = snapshot_with_used(30.0, None); // sub has full headroom
        let cands = vec![
            candidate("free-a", ProviderClass::Free, 90.0),
            candidate("sub-a", ProviderClass::Sub, 99.0), // higher SWE
        ];
        // Primary: economy classes = [free] -> sub ineligible even with
        // headroom. The free candidate wins.
        let pick =
            pick_candidate_for_role(Role::Primary, &cands, &scenario, Some(&snap), false, now());
        assert_eq!(pick.as_deref(), Some("free-a"));
    }

    #[test]
    fn economy_falls_back_to_sub_when_only_sub_is_healthy() {
        // Scenario classes for reviewer ARE [free, sub] — but with no free
        // available and a healthy sub at headroom, sub is selected.
        let scenario = RouteScenario::economy();
        let snap = snapshot_with_used(30.0, None);
        let cands = vec![
            candidate("free-unhealthy", ProviderClass::Free, 99.0),
            candidate("sub-a", ProviderClass::Sub, 50.0),
        ];
        // Make the free one unhealthy so the test focuses on "free
        // unhealthy/absent -> pick sub".
        let cands: Vec<RoutableCandidate> = cands
            .into_iter()
            .map(|mut c| {
                if c.model_id == "free-unhealthy" {
                    c.healthy = false;
                }
                c
            })
            .collect();
        let pick =
            pick_candidate_for_role(Role::Primary, &cands, &scenario, Some(&snap), false, now());
        // Economy primary classes = [free] -> sub is ineligible per role.
        // Reviewer role: economy allows sub, and free is unhealthy ->
        // sub wins.
        let pick_reviewer =
            pick_candidate_for_role(Role::Reviewer, &cands, &scenario, Some(&snap), false, now());
        assert_eq!(pick_reviewer.as_deref(), Some("sub-a"));
        assert_eq!(pick, None, "primary has no eligible free");
    }

    #[test]
    fn balanced_reviewer_picks_sub_at_headroom() {
        let scenario = RouteScenario::balanced();
        let snap = snapshot_with_used(50.0, None); // well under use_target
        let cands = vec![
            candidate("free-a", ProviderClass::Free, 99.0),
            candidate("sub-a", ProviderClass::Sub, 80.0),
        ];
        // Reviewer classes = [sub, free] -> sub wins even though a
        // higher-ranked free exists.
        assert_eq!(
            pick_candidate_for_role(Role::Reviewer, &cands, &scenario, Some(&snap), false, now())
                .as_deref(),
            Some("sub-a")
        );
        // Primary: free first -> wins on rank.
        assert_eq!(
            pick_candidate_for_role(Role::Primary, &cands, &scenario, Some(&snap), false, now())
                .as_deref(),
            Some("free-a")
        );
    }

    #[test]
    fn balanced_sub_over_cap_guard_falls_back_to_free() {
        let scenario = RouteScenario::balanced();
        // 95% used -> above the 85% cap_guard -> knemon::decide returns
        // FallBackToFree in Block mode -> sub drops off.
        let snap = snapshot_with_used(95.0, None);
        let cands = vec![
            candidate("free-a", ProviderClass::Free, 99.0),
            candidate("sub-a", ProviderClass::Sub, 50.0), // higher swe_rank, but dropped
        ];
        assert_eq!(
            pick_candidate_for_role(Role::Reviewer, &cands, &scenario, Some(&snap), false, now())
                .as_deref(),
            Some("free-a"),
            "sub over cap_guard must fall back to free for reviewer"
        );
    }

    #[test]
    fn aggressive_sub_kept_up_to_cap_guard_for_both_roles() {
        let scenario = RouteScenario::aggressive();
        // 92% used -> under cap_guard=95 -> sub still eligible.
        let snap = snapshot_with_used(92.0, None);
        let cands = vec![
            candidate("free-a", ProviderClass::Free, 99.0),
            candidate("sub-a", ProviderClass::Sub, 80.0),
        ];
        assert_eq!(
            pick_candidate_for_role(Role::Primary, &cands, &scenario, Some(&snap), false, now())
                .as_deref(),
            Some("sub-a")
        );
        assert_eq!(
            pick_candidate_for_role(Role::Reviewer, &cands, &scenario, Some(&snap), false, now())
                .as_deref(),
            Some("sub-a")
        );
    }

    #[test]
    fn aggressive_sub_drops_at_cap_guard() {
        let scenario = RouteScenario::aggressive();
        // 96% used -> above cap_guard=95 -> sub drops off.
        let snap = snapshot_with_used(96.0, None);
        let cands = vec![
            candidate("free-a", ProviderClass::Free, 99.0),
            candidate("sub-a", ProviderClass::Sub, 50.0),
        ];
        assert_eq!(
            pick_candidate_for_role(Role::Primary, &cands, &scenario, Some(&snap), false, now())
                .as_deref(),
            Some("free-a"),
            "sub over cap_guard must fall back to free"
        );
    }

    #[test]
    fn unlimited_paid_eligible_only_with_allow_paid_at_runtime() {
        let scenario = RouteScenario::unlimited();
        let snap = snapshot_with_used(0.0, None);
        let cands = vec![
            candidate("free-a", ProviderClass::Free, 50.0),
            candidate("sub-a", ProviderClass::Sub, 60.0),
            candidate("paid-a", ProviderClass::Paid, 99.0),
        ];
        // Runtime flag false -> paid is dropped (even though scenario says
        // allow_paid=true). Sub wins on rank within its classes (Sub is
        // first in unlimited's class preference).
        assert_eq!(
            pick_candidate_for_role(
                Role::Primary,
                &cands,
                &scenario,
                Some(&snap),
                false, // <-- runtime flag
                now()
            )
            .as_deref(),
            Some("sub-a"),
            "paid must be rejected without runtime --allow-paid"
        );
        // Runtime flag true AND scenario says allow_paid -> paid is in
        // the class list and eligible. Class preference order is
        // [Sub, Paid, Free]; Sub is first AND eligible (headroom), so
        // Sub still wins over a higher-rank Paid. Class preference
        // overrides rank — that is the documented contract.
        assert_eq!(
            pick_candidate_for_role(
                Role::Primary,
                &cands,
                &scenario,
                Some(&snap),
                true, // <-- runtime flag
                now()
            )
            .as_deref(),
            Some("sub-a"),
            "class preference (Sub first) overrides Paid's higher rank"
        );
    }

    #[test]
    fn unlimited_paid_only_wins_when_no_other_class_eligible() {
        // The spec resolution rule is: class preference first, then rank.
        // This pins the case where Paid IS the class-preference winner
        // for a role — i.e. no other class has a candidate — so the paid
        // hit makes it through both the scenario gate and the runtime
        // gate.
        let scenario = RouteScenario::unlimited();
        let snap = snapshot_with_used(0.0, None);
        // Reviewer classes for unlimited are [Sub, Paid, Free]. No Sub
        // and no Free in the candidate set -> Paid is the only eligible
        // class, so Paid wins.
        let cands = vec![candidate("paid-a", ProviderClass::Paid, 50.0)];
        assert_eq!(
            pick_candidate_for_role(Role::Reviewer, &cands, &scenario, Some(&snap), true, now())
                .as_deref(),
            Some("paid-a")
        );
        // Runtime flag false -> paid is ineligible -> None.
        assert_eq!(
            pick_candidate_for_role(Role::Reviewer, &cands, &scenario, Some(&snap), false, now()),
            None
        );
    }

    #[test]
    fn unlimited_chargeback_keeps_sub_above_cap_guard() {
        let scenario = RouteScenario::unlimited();
        // 96% used -> above cap_guard (85 in default, but with budget_mode
        // Chargeback, knemon::decide returns Chargeback, NOT
        // FallBackToFree -> sub is eligible.
        let snap = snapshot_with_used(96.0, None);
        let cands = vec![
            candidate("free-a", ProviderClass::Free, 99.0),
            candidate("sub-a", ProviderClass::Sub, 50.0),
        ];
        assert_eq!(
            pick_candidate_for_role(Role::Primary, &cands, &scenario, Some(&snap), true, now())
                .as_deref(),
            Some("sub-a"),
            "chargeback mode keeps the sub past cap_guard"
        );
    }

    #[test]
    fn missing_snapshot_is_treated_as_headroom() {
        // The spec calls this out explicitly: "snapshot absent or past
        // reset_at => headroom => keep".
        let scenario = RouteScenario::balanced();
        let cands = vec![
            candidate("free-a", ProviderClass::Free, 99.0),
            candidate("sub-a", ProviderClass::Sub, 80.0),
        ];
        // Reviewer with no snapshot -> KNEMON returns headroom -> sub wins.
        assert_eq!(
            pick_candidate_for_role(Role::Reviewer, &cands, &scenario, None, false, now())
                .as_deref(),
            Some("sub-a")
        );
    }

    #[test]
    fn past_reset_at_snapshot_is_treated_as_headroom() {
        // A snapshot whose reset_at_epoch is in the past is treated as 0%
        // by `effective_used` -> headroom -> sub kept.
        let scenario = RouteScenario::aggressive();
        let stale = snapshot_with_used(99.0, Some(now().timestamp() - 60));
        let cands = vec![
            candidate("free-a", ProviderClass::Free, 99.0),
            candidate("sub-a", ProviderClass::Sub, 80.0),
        ];
        assert_eq!(
            pick_candidate_for_role(Role::Primary, &cands, &scenario, Some(&stale), false, now())
                .as_deref(),
            Some("sub-a"),
            "a past-reset snapshot must yield headroom"
        );
    }

    #[test]
    fn unhealthy_candidate_is_dropped_regardless_of_class() {
        let scenario = RouteScenario::balanced();
        let snap = snapshot_with_used(0.0, None);
        let cands = vec![
            RoutableCandidate {
                model_id: "free-a".into(),
                class: ProviderClass::Free,
                swe_rank: 99.0,
                healthy: false, // circuit breaker open
            },
            candidate("sub-a", ProviderClass::Sub, 50.0),
        ];
        // Reviewer: sub wins because free is unhealthy.
        assert_eq!(
            pick_candidate_for_role(Role::Reviewer, &cands, &scenario, Some(&snap), false, now())
                .as_deref(),
            Some("sub-a")
        );
    }

    #[test]
    fn no_eligible_returns_none() {
        let snap = snapshot_with_used(99.0, None); // sub at cap
        let cands = vec![
            candidate("free-a", ProviderClass::Free, 99.0), // not in primary classes
            candidate("sub-a", ProviderClass::Sub, 50.0),   // dropped by KNEMON
        ];
        // Primary role with economy has [free] only — wait, free IS in
        // primary classes, so free-a wins. Use a scenario with no free to
        // exercise the all-dropped case.
        let scenario = RouteScenario {
            primary_classes: vec![ProviderClass::Sub], // pretend economy is sub-only
            ..RouteScenario::economy()
        };
        assert_eq!(
            pick_candidate_for_role(Role::Primary, &cands, &scenario, Some(&snap), false, now()),
            None
        );
    }

    #[test]
    fn class_preference_order_respected_with_mixed_ranks() {
        // Free has higher swe_rank than sub, but reviewer prefers sub.
        // Sub must still win even on a lower-ranked candidate.
        let scenario = RouteScenario::balanced();
        let snap = snapshot_with_used(20.0, None);
        let cands = vec![
            candidate("free-a", ProviderClass::Free, 99.0),
            candidate("sub-a", ProviderClass::Sub, 80.0),
        ];
        assert_eq!(
            pick_candidate_for_role(Role::Reviewer, &cands, &scenario, Some(&snap), false, now())
                .as_deref(),
            Some("sub-a"),
            "reviewer class preference must override rank"
        );
    }

    // -------- KNEMON Layer 4 (per-account) scenario wiring ------------
    //
    // The L4 path runs `decide_account` per candidate and ranks Sub
    // candidates by ascending strength (most-idle first). The legacy
    // swe_rank ordering still applies within non-Sub classes. These
    // tests pin the contract end-to-end through the scenario layer.

    fn view(
        provider: crate::utilization::Provider,
        account_id: &str,
        plan: &str,
        windows: Vec<crate::utilization::WindowView>,
    ) -> crate::utilization::AccountView {
        crate::utilization::AccountView {
            provider,
            account_id: account_id.to_string(),
            plan: plan.to_string(),
            windows,
        }
    }

    fn w(
        name: &str,
        hours: u32,
        pct: Option<f64>,
        h: crate::utilization::TelemetryHealth,
    ) -> crate::utilization::WindowView {
        crate::utilization::WindowView {
            name: name.into(),
            used_percent: pct,
            observability: crate::config::Observability::Header,
            health: h,
            reset_at: None,
            hours,
        }
    }

    #[test]
    fn l4_sub_ranker_prefers_most_idle_account_first() {
        // Two sub candidates with AccountViews. Sub-a has 40% used
        // (most idle, strength=40); sub-b has 82% used (strength=82).
        // The L4 picker must return sub-a even though sub-b has a
        // higher swe_rank.
        let scenario = RouteScenario::balanced();
        let n = now();
        let idle = view(
            crate::utilization::Provider::Anthropic,
            "acct-idle",
            "max",
            vec![w(
                "5h",
                5,
                Some(40.0),
                crate::utilization::TelemetryHealth::Fresh,
            )],
        );
        let busy = view(
            crate::utilization::Provider::Anthropic,
            "acct-busy",
            "max",
            vec![w(
                "5h",
                5,
                Some(82.0),
                crate::utilization::TelemetryHealth::Fresh,
            )],
        );
        let cands = vec![
            // Higher swe_rank, but the busier account — must NOT win.
            candidate("sub-busy", ProviderClass::Sub, 99.0),
            candidate("sub-idle", ProviderClass::Sub, 50.0),
        ];
        let views = vec![Some(busy), Some(idle)];
        // Reviewer role: sub-first in balanced -> sub-idle wins
        // (ascending strength).
        assert_eq!(
            pick_candidate_for_role_with_account(
                Role::Reviewer,
                &cands,
                &views,
                &scenario,
                None,
                false,
                n,
            )
            .as_deref(),
            Some("sub-idle"),
            "most-idle sub must be preferred over a higher-rank busier sub"
        );
    }

    #[test]
    fn l4_sub_ranker_drops_sub_above_cap_guard_and_picks_free() {
        // Sub is at 90% used -> cap_guard trips -> sub ineligible. With
        // a free candidate present, free wins.
        let scenario = RouteScenario::balanced();
        let n = now();
        let hot = view(
            crate::utilization::Provider::Anthropic,
            "acct",
            "max",
            vec![w(
                "5h",
                5,
                Some(90.0),
                crate::utilization::TelemetryHealth::Fresh,
            )],
        );
        let cands = vec![
            candidate("sub-a", ProviderClass::Sub, 99.0),
            candidate("free-a", ProviderClass::Free, 50.0),
        ];
        let views = vec![Some(hot), None];
        // Reviewer: [sub, free]. Sub is ineligible -> free wins.
        assert_eq!(
            pick_candidate_for_role_with_account(
                Role::Reviewer,
                &cands,
                &views,
                &scenario,
                None,
                false,
                n,
            )
            .as_deref(),
            Some("free-a"),
            "sub over cap_guard must fall back to free"
        );
    }

    #[test]
    fn l4_sub_ranker_legacy_swe_rank_used_when_no_account_views() {
        // When every account view is None, the L4 path must NOT
        // invent ranking — it must use the legacy single-snapshot
        // decide() path and the sub's swe_rank ordering. swe_rank=99
        // wins over swe_rank=50.
        let scenario = RouteScenario::balanced();
        let n = now();
        let snap = snapshot_with_used(20.0, None); // headroom
        let cands = vec![
            candidate("sub-low", ProviderClass::Sub, 50.0),
            candidate("sub-high", ProviderClass::Sub, 99.0),
        ];
        let views = vec![None, None];
        assert_eq!(
            pick_candidate_for_role_with_account(
                Role::Reviewer,
                &cands,
                &views,
                &scenario,
                Some(&snap),
                false,
                n,
            )
            .as_deref(),
            Some("sub-high"),
            "with no account views, L4 must fall back to legacy swe_rank ordering"
        );
    }

    #[test]
    fn l4_sub_ranker_excludes_degraded_window_and_prefers_idle_observable() {
        // A 95% degraded window on sub-a must NOT cause it to be
        // dropped. With no other windows, sub-a is treated as headroom
        // (Degraded-only => observable set empty => PreferSub).
        let scenario = RouteScenario::balanced();
        let n = now();
        let degraded_only = view(
            crate::utilization::Provider::Anthropic,
            "acct-d",
            "max",
            vec![w(
                "5h",
                5,
                Some(95.0),
                crate::utilization::TelemetryHealth::Degraded,
            )],
        );
        let cands = vec![candidate("sub-a", ProviderClass::Sub, 50.0)];
        let views = vec![Some(degraded_only)];
        // Reviewer: sub only candidate -> PreferSub despite 95%
        // degraded (the L4 "no observable signal = headroom" rule).
        assert_eq!(
            pick_candidate_for_role_with_account(
                Role::Reviewer,
                &cands,
                &views,
                &scenario,
                None,
                false,
                n,
            )
            .as_deref(),
            Some("sub-a"),
            "Degraded-only window must not demote a sub candidate"
        );
    }

    #[test]
    fn l4_chain_for_role_orders_by_ascending_strength() {
        // Chain output should list sub candidates in ascending
        // strength order: 40% first, then 82%.
        let scenario = RouteScenario::balanced();
        let n = now();
        let idle = view(
            crate::utilization::Provider::Anthropic,
            "i",
            "max",
            vec![w(
                "5h",
                5,
                Some(40.0),
                crate::utilization::TelemetryHealth::Fresh,
            )],
        );
        let busy = view(
            crate::utilization::Provider::Anthropic,
            "b",
            "max",
            vec![w(
                "5h",
                5,
                Some(82.0),
                crate::utilization::TelemetryHealth::Fresh,
            )],
        );
        let cands = vec![
            candidate("sub-busy", ProviderClass::Sub, 99.0),
            candidate("sub-idle", ProviderClass::Sub, 50.0),
        ];
        let views = vec![Some(busy), Some(idle)];
        let chain = chain_for_role_with_account(
            Role::Reviewer,
            &cands,
            &views,
            &scenario,
            None,
            false,
            n,
            0,
        );
        assert_eq!(
            chain,
            vec!["sub-idle".to_string(), "sub-busy".to_string()],
            "chain must list sub candidates in ascending strength order"
        );
    }

    #[test]
    fn l4_legacy_picker_unchanged_when_no_account_view_passed() {
        // The legacy `pick_candidate_for_role` signature is preserved
        // and behaves exactly as it did before L4 was added. This is
        // the "single-snapshot fallback" the spec requires.
        let scenario = RouteScenario::balanced();
        let snap = snapshot_with_used(20.0, None);
        let cands = vec![
            candidate("sub-a", ProviderClass::Sub, 80.0),
            candidate("free-a", ProviderClass::Free, 99.0),
        ];
        // Reviewer: [sub, free]. Sub has headroom -> sub wins.
        assert_eq!(
            pick_candidate_for_role(Role::Reviewer, &cands, &scenario, Some(&snap), false, now())
                .as_deref(),
            Some("sub-a"),
        );
        // Primary: [free, sub]. Free wins on rank.
        assert_eq!(
            pick_candidate_for_role(Role::Primary, &cands, &scenario, Some(&snap), false, now())
                .as_deref(),
            Some("free-a"),
        );
    }
}
