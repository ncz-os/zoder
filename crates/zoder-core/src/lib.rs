//! zoder-core: cost-aware, free-first model routing with live health,
//! a default-deny-paid policy gate, and a local spend ledger - over any
//! OpenAI-compatible / LiteLLM fleet. Vendor-neutral; free-tier is the first target.

use std::time::Duration;

pub mod budget;
pub mod config;
pub mod consultant;
pub mod corpus;
pub mod engine_cost;
pub use acp_client as engine_rpc;
pub mod enterprise_cost;
pub mod finops;
pub mod gate;
pub mod gate_bundle;
pub mod health_probe;
pub use health_probe::{
    cap_targets, classify_err, classify_err_kind, probe_all, probe_request, Probe, ProbeOutcome,
    ProbePlan, ProbeResolver, PROBE_MAX_MODELS_PER_PROVIDER, PROBE_MAX_TOKENS,
    PROBE_PING_TIMEOUT_SECS, PROBE_PROMPT,
};
pub use model_health as health;
pub mod catalog_models;
pub mod ledger;
pub mod mcp;
pub mod policy;
pub mod pricing;
pub mod pricing_sync;
pub mod project_instructions;
pub mod provider;
pub mod quota;
pub mod reconcile;
pub mod report;
pub mod router;
pub mod scenarios;
pub mod session;
pub mod sop_graph;
pub mod subscription_tiers;
pub mod update;
pub mod utilization;

pub use acp_client::{
    agents_status, cancel_session, fetch_engine_config, fetch_quickstart_diagnostics, list_agents,
    new_session, probe_ready, run_agent, run_agent_dispatch, run_goose_agent, sop_rpc_call,
    wait_for_socket, write_tool_matrix, write_tool_matrix_human, AgentEntry, AgentEvent,
    AgentIdleWindows, AgentOptions, AgentRun, AgentStatusEntry, AgentsListResult,
    AgentsStatusResult, ApprovalPolicy, DaemonReadiness, EngineKind, GooseProviderEnv,
    WriteToolMatrixRow, AGENT_IDLE_BETWEEN_EVENTS_ENV, AGENT_IDLE_FIRST_EVENT_ENV,
    AGENT_IDLE_TIMEOUT_ENV, DEFAULT_AGENT_IDLE_BETWEEN_EVENTS_S, DEFAULT_AGENT_IDLE_FIRST_EVENT_S,
    DEFAULT_AUTO_APPROVE, HOSTED_AGENT_IDLE_BETWEEN_EVENTS_S, HOSTED_AGENT_IDLE_FIRST_EVENT_S,
    IDLE_TIMEOUT_OUTCOME,
};
/// Settle budget the loop grants the daemon to ACK a `session/cancel` after
/// the author-phase watchdog fires (see `cancel_session`). Canonical value
/// referenced by `zoder-cli`'s author-phase wrapper and the `acp-client`
/// wire-shape tests so a change here is exercised by the test that guards
/// the loop's `build_diff` ordering.
pub const CANCEL_SETTLE_BUDGET: Duration = Duration::from_secs(5);

/// ACP idle watchdog window from the environment (`ZODER_AGENT_IDLE_S`).
/// `None` when unset or unparseable; an explicit `0` is honored by the
/// resolver (`agent_idle_timeout_s`) as "disabled".
pub fn agent_idle_timeout_s_from_env() -> Option<u64> {
    std::env::var(AGENT_IDLE_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
}

/// First-event idle window from the environment
/// (`ZODER_AGENT_IDLE_FIRST_EVENT_S`).
///
/// `None` when unset or unparseable; an explicit `0` is honored by the
/// resolver as "this window is disabled".
pub fn agent_idle_first_event_s_from_env() -> Option<u64> {
    std::env::var(AGENT_IDLE_FIRST_EVENT_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
}

/// Between-events idle window from the environment
/// (`ZODER_AGENT_IDLE_BETWEEN_EVENTS_S`).
///
/// `None` when unset or unparseable; an explicit `0` is honored by the
/// resolver as "this window is disabled".
pub fn agent_idle_between_events_s_from_env() -> Option<u64> {
    std::env::var(AGENT_IDLE_BETWEEN_EVENTS_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
}
pub use budget::{estimate_tokens, Budget, BudgetVerdict};
pub use catalog_models::{
    fetch_catalog_models, merge_into_corpus, merge_or_degrade, CatalogModel, CatalogResponse,
    EnrichmentOutcome,
};
pub use config::{
    AliasedAgentConfig, Auth, BillingMode, Config, EngineModelRegistry, EngineProviderRoute,
    ExecSafetyConfig, ExecSandbox, LinuxBubblewrapProfileOptions, LinuxLandlockProfileOptions,
    Provider, QuotaUnit, QuotaWindow, ReviewConfig, SeatbeltProfileOptions, SubscriptionPlan,
    Theme,
};
pub use corpus::{
    AliasOverlay, Corpus, ModelEntry, OverlayModel, OverlayReport, RefreshReport,
    MODEL_ALIASES_ENV, MODEL_ALIASES_FILE, OVERLAY_SOURCE,
};
pub use engine_cost::{
    fetch_engine_cost, AgentStats as EngineAgentStats, CostSummary as EngineCostSummary,
    ModelStats as EngineModelStats,
};
pub use enterprise_cost::{CostSnapshot, MonthCost, ScopeStat};
pub use finops::{
    build_finops_report, cache_savings_by_model, cheapest_equivalent_advisor,
    cli_run as finops_cli, forecast_burn, realized_rate_by_model, spend_by_dimension, AdvisorRow,
    BurnForecast, CacheSavingsRow, Dimension, FinOpsReport, ModelRealized, SpendGroup,
};
pub use ledger::{BillableReservation, Entry, FinOpsTags, Ledger, Period, Rollup};
pub use mcp::{
    parse_mcp_servers_config, parse_mcp_servers_file, to_acp_mcp_servers, McpServerSpec, McpSource,
    McpTransportKind,
};
pub use model_health::{Classification, HealthStore, State};
pub use policy::{Decision, PolicyGate, PAID_WARNING};
pub use pricing::{CostVerdict, ModelPrice, PricingCatalog};
pub use pricing_sync::{sync_catalog, Source as PricingSource, SyncStats};
pub use project_instructions::{
    compose_prompt, load_project_instructions, MAX_PROJECT_INSTRUCTIONS_BYTES,
    PROJECT_INSTRUCTIONS_HEADER, PROJECT_INSTRUCTIONS_SEPARATOR, TRUNCATION_MARKER,
};
pub use provider::{
    backoff_delay, request_timeout_s_from_env, CallTelemetry, ChatRequest, ChatResult, ErrKind,
    Message, OpenAiProvider, ProviderError, DEFAULT_REQUEST_TIMEOUT_S,
};
pub use quota::{
    amortized_per_call, plan_usage, plan_usage_for_catalog_provider, window_usage, WindowUsage,
};
pub use reconcile::{anthropic_costs, openai_costs, ReconResult};
pub use report::{build_report, build_report_from_entries, Bucket, Gran, Report, RowByModel};
pub use router::{Route, Router, Tier};
pub use scenarios::{
    candidate_eligible, chain_for_role, chain_for_role_with_account, classify as classify_provider,
    default_scenarios, pick_candidate_for_role, pick_candidate_for_role_with_account,
    resolve_active, ProviderClass, Role as ScenarioRole, RoutableCandidate, RouteScenario,
};
pub use session::Session;
pub use sop_graph::{
    fetch_sop_graph, fetch_sop_graph_report, fetch_sop_overlay, render_sop_graph_human, SopGraph,
    SopGraphReport, SopOverlay, SopOverlayStep, SopStep,
};
pub use subscription_tiers::{
    load_tier_catalog, resolve_plan_windows, Confidence, ProviderTiers, ResolveSource,
    ResolvedPlan, TierCatalog, TierEntry, TierWindow, WindowProvenance, TIERS_JSON_DEFAULT,
};

/// Crate version (from Cargo).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod idle_timeout_env_tests {
    //! Tests for the ACP idle-watchdog environment override.
    //!
    //! The env var is process-global, so the tests that flip it are
    //! serialized behind one mutex (the same pattern the provider tests
    //! use for `AZURE_OPENAI_API_VERSION`).

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    use super::*;

    #[test]
    fn env_var_name_is_the_documented_one() {
        assert_eq!(
            AGENT_IDLE_TIMEOUT_ENV, "ZODER_AGENT_IDLE_S",
            "the documented environment variable name must not drift"
        );
    }

    #[test]
    fn env_value_is_parsed_and_zero_is_preserved() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var(AGENT_IDLE_TIMEOUT_ENV).ok();
        std::env::set_var(AGENT_IDLE_TIMEOUT_ENV, "45");
        assert_eq!(agent_idle_timeout_s_from_env(), Some(45));

        // `0` must survive the parse: it is the documented "disable"
        // value, and the resolver (not the env reader) decides what it
        // means.
        std::env::set_var(AGENT_IDLE_TIMEOUT_ENV, "0");
        assert_eq!(agent_idle_timeout_s_from_env(), Some(0));

        // A non-numeric value is ignored (the resolver falls back to the
        // config / default), never a panic.
        std::env::set_var(AGENT_IDLE_TIMEOUT_ENV, "not-a-number");
        assert_eq!(agent_idle_timeout_s_from_env(), None);

        match previous {
            Some(value) => std::env::set_var(AGENT_IDLE_TIMEOUT_ENV, value),
            None => std::env::remove_var(AGENT_IDLE_TIMEOUT_ENV),
        }
    }
}
