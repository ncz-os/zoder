//! The global `[deployment]` profile and the single-host starter template.
//!
//! `[deployment] mode` is separate from the vendor-overlay `[profile]` table
//! and from `[context] profile`. The rules pinned here:
//!
//! * absent means `standalone` (a fresh single-host install is never silently
//!   tuned for a fleet),
//! * `[context] profile` defaults to the global deployment profile, while an
//!   explicit `[context] profile` still wins,
//! * the shipped standalone template parses as a vendor overlay and describes
//!   one host with env-var auth only.

use std::fs;
use zoder_core::budget::Budget;
use zoder_core::config::{Config, DeploymentMode, ProfileDefaults, VendorOverlay};
use zoder_core::context_opt::{ContextProfile, Levers};

const STANDALONE_TEMPLATE: &str = include_str!("../../../config.standalone.toml");
const FLEET_TEMPLATE: &str = include_str!("../../../config.fleet.toml");

fn minimal() -> &'static str {
    r#"
default_provider = "p"
corpus_path = "/nonexistent/corpus.json"
ledger_path = "/nonexistent/ledger.jsonl"
health_path = "/nonexistent/health.json"
[[providers]]
id = "p"
base_url = "https://example.invalid/v1"
kind = "openai-chat"
auth = { type = "none" }
"#
}

#[test]
fn an_absent_deployment_table_means_standalone() {
    let cfg: Config = toml::from_str(minimal()).expect("config parses");
    assert_eq!(cfg.deployment_profile(), DeploymentMode::Standalone);
    assert_eq!(cfg.context_profile(), ContextProfile::Standalone);
    assert_eq!(
        cfg.context_levers_for("deepseek-flash"),
        None,
        "standalone leaves the context levers off"
    );
}

#[test]
fn fleet_deployment_supplies_the_context_default() {
    let cfg: Config = toml::from_str(&format!("{}\n[deployment]\nmode = \"fleet\"\n", minimal()))
        .expect("config parses");
    assert_eq!(cfg.deployment_profile(), DeploymentMode::Fleet);
    assert_eq!(cfg.context_profile(), ContextProfile::Fleet);
    assert_eq!(
        cfg.context_levers_for("deepseek-flash"),
        Some(Levers::FLEET_DEEPSEEK),
        "no [context] block: the fleet deployment profile turns the levers on"
    );
}

#[test]
fn an_explicit_context_profile_still_wins_over_the_deployment() {
    let cfg: Config = toml::from_str(&format!(
        "{}\n[deployment]\nmode = \"fleet\"\n[context]\nprofile = \"standalone\"\n",
        minimal()
    ))
    .expect("config parses");
    assert_eq!(cfg.context_profile(), ContextProfile::Standalone);
    assert_eq!(cfg.context_levers_for("deepseek-flash"), None);
}

/// Verify the accessor API contract for `ProfileDefaults`.
///
/// The wired settings (context_profile, fleet_job_workspaces,
/// daemon_cancel_budget_ms, provider_concurrency_default) are exercised by
/// production-path tests elsewhere. The hooks (terse_io, shared_daemon,
/// fleet_pull_updates, cost_backstop_monthly_usd, reviewer_pool_default)
/// are accessor-only today; this test pins their return values so a future
/// wiring commit can rely on the contract without breaking silently.
#[test]
fn profile_defaults_accessor_contract() {
    let fleet = ProfileDefaults::for_mode(DeploymentMode::Fleet);
    let standalone = ProfileDefaults::for_mode(DeploymentMode::Standalone);

    assert_eq!(fleet.context_profile(), ContextProfile::Fleet);
    assert_eq!(standalone.context_profile(), ContextProfile::Standalone);

    assert!(fleet.terse_io());
    assert!(!standalone.terse_io());

    assert!(fleet.shared_daemon());
    assert!(!standalone.shared_daemon());

    assert!(fleet.fleet_job_workspaces());
    assert!(!standalone.fleet_job_workspaces());

    assert!(fleet.fleet_pull_updates());
    assert!(!standalone.fleet_pull_updates());

    assert_eq!(
        fleet.daemon_cancel_budget_ms(),
        750,
        "the fleet value today"
    );
    assert!(
        standalone.daemon_cancel_budget_ms() > fleet.daemon_cancel_budget_ms(),
        "a single host can afford a longer cancel settle window"
    );

    assert_eq!(fleet.cost_backstop_monthly_usd(), None);
    assert_eq!(standalone.cost_backstop_monthly_usd(), Some(50.0));

    assert_eq!(fleet.provider_concurrency_default(), 0);
    assert_eq!(standalone.provider_concurrency_default(), 0);

    assert!(fleet.reviewer_pool_default());
    assert!(!standalone.reviewer_pool_default());
}

#[test]
fn the_standalone_template_is_a_valid_single_host_overlay() {
    let overlay: VendorOverlay =
        toml::from_str(STANDALONE_TEMPLATE).expect("standalone template parses");
    assert_eq!(
        overlay.deployment.expect("deployment table").mode,
        DeploymentMode::Standalone
    );
    assert!(overlay.profile.default);
    assert_eq!(
        overlay.profile.default_provider.as_deref(),
        Some("deepseek")
    );
    assert_eq!(
        overlay.profile.primary_model.as_deref(),
        Some("deepseek-chat")
    );

    let ids: Vec<&str> = overlay.providers.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["deepseek", "minimax"]);
    for provider in &overlay.providers {
        assert!(provider.paid);
        assert_eq!(provider.billing, zoder_core::BillingMode::Metered);
        assert!(provider.max_context_tokens.is_some());
        assert!(matches!(
            provider.auth,
            zoder_core::config::Auth::Env { .. }
        ));
        assert!(
            provider.base_url.starts_with("https://"),
            "hosted routes use https"
        );
        assert!(
            !provider.base_url.contains("localhost"),
            "the local endpoint stays commented out"
        );
    }

    let context = overlay.context.expect("context block");
    assert_eq!(context.enabled, Some(false));
    assert_eq!(context.profiles.len(), 2);

    let budget: Budget = overlay.budget.expect("budget backstop");
    assert_eq!(budget.max_cost_per_call_usd, Some(1.0));
    // The template's cap must stay in lockstep with the accessor that
    // documents the standalone default (cost_backstop_monthly_usd).
    let standalone_default =
        ProfileDefaults::for_mode(DeploymentMode::Standalone).cost_backstop_monthly_usd();
    assert_eq!(budget.monthly_cap_usd, standalone_default);
}

#[test]
fn the_fleet_template_is_a_profile_only_overlay() {
    let overlay: VendorOverlay = toml::from_str(FLEET_TEMPLATE).expect("fleet template parses");
    assert_eq!(
        overlay.deployment.expect("deployment table").mode,
        DeploymentMode::Fleet
    );
    assert_eq!(
        overlay.context.expect("context block").profile,
        Some(ContextProfile::Fleet)
    );
    assert!(
        overlay.providers.is_empty(),
        "fleet providers come from the fleet's own configuration repository"
    );
}

#[test]
fn the_standalone_template_loads_and_selects_its_own_profile() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("config.standalone.toml"),
        STANDALONE_TEMPLATE,
    )
    .expect("write template");

    let cfg = Config::load_unvalidated_from(dir.path()).expect("template loads");
    assert_eq!(cfg.default_provider, "deepseek");
    assert_eq!(cfg.deployment_profile(), DeploymentMode::Standalone);
    assert_eq!(cfg.context_profile(), ContextProfile::Standalone);
    assert!(!cfg.context.levers_enabled());
    assert_eq!(
        cfg.reviewer_models_for(None),
        ["MiniMax-M3", "deepseek-chat"]
    );
    assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
}

#[test]
fn standalone_template_has_no_model_families_and_keeps_default_family_answers() {
    let overlay: VendorOverlay =
        toml::from_str(STANDALONE_TEMPLATE).expect("standalone template parses");
    assert!(
        overlay.model_families.is_empty(),
        "the shipped standalone template must not ask for fleet families"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("config.standalone.toml"),
        STANDALONE_TEMPLATE,
    )
    .expect("write template");
    let cfg = Config::load_unvalidated_from(dir.path()).expect("template loads");

    assert!(cfg.model_families.is_empty(), "{:?}", cfg.model_families);
    assert_eq!(cfg.model_family("openai/gpt-oss-120b"), "openai");
    assert_eq!(cfg.model_family("Qwen/Qwen3-Coder"), "qwen");
    assert_eq!(cfg.model_family("MiniMax-M3"), "minimax");
    assert_eq!(cfg.model_family("provider-a/model-x"), "provider-a");
    assert_eq!(cfg.model_family("mystery-model"), "mystery-model");

    let serialized = serde_json::to_value(&cfg).expect("config serializes");
    assert!(
        serialized.get("model_families").is_none(),
        "an empty families map must stay out of the serialized config: {serialized}"
    );
}

#[test]
fn a_fleet_only_overlay_makes_context_inherit_fleet() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("config.example.toml"),
        "[deployment]\nmode = \"fleet\"\n",
    )
    .expect("write overlay");

    let cfg = Config::load_unvalidated_from(dir.path()).expect("overlay loads");
    assert_eq!(cfg.deployment_profile(), DeploymentMode::Fleet);
    assert_eq!(cfg.context.profile, Some(ContextProfile::Fleet));
    assert_eq!(cfg.context_profile(), ContextProfile::Fleet);
    assert_eq!(
        cfg.context_levers_for("deepseek-flash"),
        Some(Levers::FLEET_DEEPSEEK)
    );
}

#[test]
fn an_empty_overlay_is_still_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("config.empty.toml"), "# nothing\n").expect("write overlay");
    assert!(
        Config::load_unvalidated_from(dir.path()).is_err(),
        "a profile-only overlay is allowed, a wholly empty one is not"
    );
}
