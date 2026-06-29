//! zoder-core: cost-aware, free-first model routing with live health,
//! a default-deny-paid policy gate, and a local spend ledger - over any
//! OpenAI-compatible / LiteLLM fleet. Vendor-neutral; free-tier is the first target.

pub mod budget;
pub mod config;
pub mod corpus;
pub mod engine_cost;
pub mod engine_rpc;
pub mod enterprise_cost;
pub mod finops;
pub mod health;
pub mod ledger;
pub mod policy;
pub mod pricing;
pub mod pricing_sync;
pub mod provider;
pub mod quota;
pub mod reconcile;
pub mod report;
pub mod router;
pub mod session;

pub use budget::{estimate_tokens, Budget, BudgetVerdict};
pub use config::{
    Auth, BillingMode, Config, Provider, QuotaUnit, QuotaWindow, SubscriptionPlan, Theme,
};
pub use corpus::{Corpus, ModelEntry, RefreshReport};
pub use engine_cost::{
    fetch_engine_cost, AgentStats as EngineAgentStats, CostSummary as EngineCostSummary,
    ModelStats as EngineModelStats,
};
pub use engine_rpc::{
    new_session, run_agent, wait_for_socket, AgentEvent, AgentOptions, AgentRun, ApprovalPolicy,
    DEFAULT_AUTO_APPROVE,
};
pub use enterprise_cost::{CostSnapshot, MonthCost, ScopeStat};
pub use finops::{
    build_finops_report, cache_savings_by_model, cheapest_equivalent_advisor,
    cli_run as finops_cli, forecast_burn, realized_rate_by_model, spend_by_dimension, AdvisorRow,
    BurnForecast, CacheSavingsRow, Dimension, FinOpsReport, FinOpsTags, ModelRealized, SpendGroup,
};
pub use health::{HealthStore, State};
pub use ledger::{Entry, Ledger, Period, Rollup};
pub use policy::{Decision, PolicyGate, PAID_WARNING};
pub use pricing::{ModelPrice, PricingCatalog};
pub use pricing_sync::{sync_catalog, Source as PricingSource, SyncStats};
pub use provider::{
    backoff_delay, CallTelemetry, ChatRequest, ChatResult, ErrKind, Message, OpenAiProvider,
    ProviderError,
};
pub use quota::{amortized_per_call, plan_usage, window_usage, WindowUsage};
pub use reconcile::{anthropic_costs, openai_costs, ReconResult};
pub use report::{build_report, build_report_from_entries, Bucket, Gran, Report, RowByModel};
pub use router::{Route, Router, Tier};
pub use session::Session;

/// Crate version (from Cargo).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
