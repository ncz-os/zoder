//! zoder-core: cost-aware, free-first model routing with live health,
//! a default-deny-paid policy gate, and a local spend ledger - over any
//! OpenAI-compatible / LiteLLM fleet. Vendor-neutral; free-tier is the first target.

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
pub mod ledger;
pub mod policy;
pub mod pricing;
pub mod pricing_sync;
pub mod provider;
pub mod quota;
pub mod reconcile;
pub mod report;
pub mod router;
pub mod scenarios;
pub mod session;
pub mod subscription_tiers;
pub mod update;
pub mod utilization;

pub use acp_client::{
    new_session, run_agent, run_agent_dispatch, run_goose_agent, wait_for_socket, AgentEvent,
    AgentOptions, AgentRun, ApprovalPolicy, EngineKind, GooseProviderEnv, DEFAULT_AUTO_APPROVE,
};
pub use budget::{estimate_tokens, Budget, BudgetVerdict};
pub use config::{
    AliasedAgentConfig, Auth, BillingMode, Config, Provider, QuotaUnit, QuotaWindow,
    SubscriptionPlan, Theme,
};
pub use corpus::{Corpus, ModelEntry, RefreshReport};
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
pub use ledger::{Entry, FinOpsTags, Ledger, Period, Rollup};
pub use model_health::{Classification, HealthStore, State};
pub use policy::{Decision, PolicyGate, PAID_WARNING};
pub use pricing::{CostVerdict, ModelPrice, PricingCatalog};
pub use pricing_sync::{sync_catalog, Source as PricingSource, SyncStats};
pub use provider::{
    backoff_delay, CallTelemetry, ChatRequest, ChatResult, ErrKind, Message, OpenAiProvider,
    ProviderError,
};
pub use quota::{amortized_per_call, plan_usage, window_usage, WindowUsage};
pub use reconcile::{anthropic_costs, openai_costs, ReconResult};
pub use report::{build_report, build_report_from_entries, Bucket, Gran, Report, RowByModel};
pub use router::{Route, Router, Tier};
pub use scenarios::{
    candidate_eligible, chain_for_role, chain_for_role_with_account, classify as classify_provider,
    default_scenarios, pick_candidate_for_role, pick_candidate_for_role_with_account,
    resolve_active, ProviderClass, Role as ScenarioRole, RoutableCandidate, RouteScenario,
};
pub use session::Session;
pub use subscription_tiers::{
    load_tier_catalog, resolve_plan_windows, Confidence, ProviderTiers, ResolveSource,
    ResolvedPlan, TierCatalog, TierEntry, TierWindow, WindowProvenance, TIERS_JSON_DEFAULT,
};

/// Crate version (from Cargo).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
