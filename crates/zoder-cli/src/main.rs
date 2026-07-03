//! zoder CLI - codex-compatible surface + cost-aware routing extensions.

use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

mod agentic;
mod goose;

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use zoder_core::gate::{
    baseline_plan_for, detect_repo_signals, run_plan, CompatibilityReport, GateMode, GateReport,
    GateStatus, GateStep, RepoSignals, StepOutcome,
};
use zoder_core::gate_bundle::{discover_markers, probe_tools, render_probe, PathEnv, ToolLookup};
use zoder_core::{
    amortized_per_call, anthropic_costs, backoff_delay, build_report, build_report_from_entries,
    cap_targets, classify_err, estimate_tokens, fetch_engine_cost, finops_cli, load_tier_catalog,
    openai_costs, plan_usage, probe_request, run_agent_dispatch, sync_catalog, AgentEvent,
    AgentOptions, ApprovalPolicy, BillingMode, BudgetVerdict, ChatRequest, ChatResult, Config,
    Corpus, CostSnapshot, Decision, EngineKind, Entry, GooseProviderEnv, Gran, HealthStore, Ledger,
    Message, ModelEntry, OpenAiProvider, Period, PolicyGate, PricingCatalog, PricingSource,
    ProbeOutcome, Provider, ProviderError, Router, ScopeStat, Session, State, Theme, Tier,
    PROBE_MAX_MODELS_PER_PROVIDER, PROBE_PING_TIMEOUT_SECS,
};

#[derive(Parser, Clone)]
#[command(
    name = "zoder",
    version,
    about = "Free-first, cost-aware, codex-compatible coding CLI",
    arg_required_else_help = false
)]
struct Cli {
    /// Prompt for a quick run (codex-style: `zoder "fix this"`).
    prompt: Option<String>,

    // ---- routing / cost (zoder additions) ----
    /// Force a specific model id (skips routing).
    #[arg(short = 'm', long, global = true)]
    model: Option<String>,
    /// Routing tier: fast | strong | auto (default auto).
    #[arg(long, global = true, default_value = "auto")]
    tier: String,
    /// Allow paid models without interactive confirmation (scripts/CI).
    #[arg(long, global = true)]
    allow_paid: bool,
    /// Only ever use free models; never fall back to paid.
    #[arg(long, global = true)]
    require_free: bool,
    /// Print the routing decision + cost estimate without calling.
    #[arg(long, global = true)]
    dry_run: bool,
    /// Explain why a model was chosen.
    #[arg(long, global = true)]
    explain: bool,
    /// Max output tokens.
    #[arg(long, global = true, default_value_t = 1024)]
    max_tokens: u32,
    /// Disable streaming output.
    #[arg(long, global = true)]
    no_stream: bool,
    /// JSON output (codex/claude-compatible).
    #[arg(long, global = true)]
    json: bool,
    /// Include model reasoning/thinking in the answer (default: off).
    #[arg(long, global = true)]
    show_reasoning: bool,
    /// Reasoning effort to request: minimal | low | medium | high | none
    /// (default: the model's own default). `none` asks it to skip thinking.
    #[arg(long, global = true)]
    reasoning: Option<String>,
    /// Relax the fail-closed free guard when a backend reports no telemetry.
    #[arg(long, global = true)]
    lenient_telemetry: bool,

    // ---- agentic loop (codex-style; drives the zeroclaw engine) ----
    /// Working directory for the agentic run (codex `-C`). Default: current dir.
    #[arg(short = 'C', long = "cd", global = true, value_name = "DIR")]
    cd: Option<String>,
    /// Run as a specific zeroclaw agent alias (see `zoder agents`/zerocode
    /// picker). Default: alias derived from the routed/`-m` model.
    #[arg(long, global = true, value_name = "ALIAS")]
    agent: Option<String>,
    /// Pure single-shot completion (no tools/file edits). The default is the
    /// agentic loop (codex `exec` drop-in) when the engine is reachable.
    #[arg(long, global = true)]
    oneshot: bool,
    /// Tool-approval policy for the agentic loop: all | allowlist | none
    /// (default allowlist).
    #[arg(long, global = true, value_name = "POLICY")]
    approve: Option<String>,
    /// Hard wall-clock budget for an agentic turn, in seconds (default 900).
    #[arg(long, global = true, value_name = "SECS")]
    agent_timeout: Option<u64>,
    /// Hard wall-clock budget for a single `loop` phase (author, `--check`,
    /// review), in seconds (default 900). The watchdog kills the spawned
    /// child AND its process group when the budget elapses, so a wedged
    /// child can never hang the loop indefinitely. `agent_timeout` controls
    /// the engine's internal turn budget and is independent.
    #[arg(long, global = true, value_name = "SECS")]
    loop_timeout: Option<u64>,
    /// Which agentic engine to drive the loop with: zeroclaw | goose
    /// (default zeroclaw — current behavior). `goose` spawns a `goose acp`
    /// child process and drives standard ACP over its stdio; model
    /// selection is via the spawned process's `GOOSE_PROVIDER` / `GOOSE_MODEL`
    /// env (see `GOOSE_PROVIDER` to override the default `openai` provider).
    #[arg(long, global = true, value_name = "ENGINE", default_value = "zeroclaw")]
    engine: String,

    // ---- reliability ----
    /// Per-model transient-failure retries (timeouts/429/5xx) before fallback.
    #[arg(long, global = true, default_value_t = 2)]
    retries: u32,
    /// Disable the cross-model fallback chain (use only the routed primary).
    #[arg(long, global = true)]
    no_fallback: bool,

    // ---- sessions ----
    /// Attach to a named multi-turn session (creates it if missing).
    #[arg(long, global = true)]
    session: Option<String>,
    /// Continue the most-recently-updated session.
    #[arg(long = "continue", global = true)]
    continue_: bool,

    // ---- output ----
    /// Suppress the trailing summary/diagnostic line.
    #[arg(short = 'q', long, global = true)]
    quiet: bool,
    /// Increase log verbosity (-v debug, -vv trace). Also honors RUST_LOG.
    #[arg(short = 'v', long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

/// Named reporting windows for `zoder report` (all period-to-date).
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ReportPeriod {
    Day,
    Week,
    Month,
    Quarter,
    Ytd,
}

#[derive(Subcommand, Clone)]
enum Cmd {
    /// Non-interactive run for automation/CI (codex-compatible). `-` reads stdin.
    Exec { prompt: Option<String> },
    /// Launch the zerocode terminal UI, wired to the local zeroclaw engine.
    /// The matching zerocode + zeroclaw binaries ship alongside zoder; zerocode
    /// auto-starts an ephemeral daemon if none is running. Extra args are
    /// forwarded to zerocode (e.g. `--config-dir DIR`, `--theme NAME`).
    Tui {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// List the model corpus (free routing pool by default).
    Models {
        #[arg(long)]
        free: bool,
        #[arg(long)]
        paid: bool,
        #[arg(long)]
        all: bool,
    },
    /// Check for a newer zoder release and update to it. Without `--check`,
    /// re-runs the official installer to self-replace this binary.
    Update {
        /// Only report whether a newer release exists; do not install.
        #[arg(long)]
        check: bool,
    },
    /// Show the routing decision for a task without executing it.
    Route { prompt: Option<String> },
    /// Rank models for a human to pick from: health (circuit-breaker) + coding
    /// capability + SWE Elo — the same signals the router uses. Use `--json` for
    /// machine output.
    Consult {
        /// Restrict to free/routable models (routable already implies free; this
        /// is an explicit, visible filter).
        #[arg(long)]
        free_only: bool,
        /// Show at most N rows.
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Spend report (local ledger) by period.
    Spend {
        /// day | week | month | year
        #[arg(default_value = "month")]
        period: String,
        /// Only include spend on/after this date (YYYY-MM-DD).
        #[arg(long)]
        since: Option<String>,
        /// Only include spend on/before this date (YYYY-MM-DD).
        #[arg(long)]
        until: Option<String>,
        /// Group by model instead of by period bucket.
        #[arg(long)]
        by_model: bool,
        /// Restrict to one publisher host (the segment before `/` in the model
        /// id, e.g. `--host meta`), summed across every provider that served
        /// that publisher's models.
        #[arg(long, value_name = "HOST")]
        host: Option<String>,
    },
    /// Usage + chargeback report from the local ledger for a period
    /// (day/week/month/quarter/ytd), bucketed by time + by model, with the
    /// free-tier counterfactual + savings. Colorized output.
    Report {
        /// Reporting period: day | week | month | quarter | ytd (period-to-date).
        #[arg(value_enum, default_value_t = ReportPeriod::Month)]
        period: ReportPeriod,
        /// Custom trailing window in days (overrides the period argument).
        #[arg(long)]
        days: Option<i64>,
        /// Max rows in the by-model table (e.g. `--top 100`). 0 = show all.
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// Restrict the report to one vendor profile (e.g. `--vendor enterprise`).
        /// Reads `~/.zoder/config.<vendor>.toml` and only counts ledger
        /// entries whose `provider` id was contributed by that TOML. The
        /// totals, counterfactual, and avoided-spend headline are all
        /// recomputed over the filtered set so the headline is meaningful.
        /// See README §"Config overlays" for how to add a vendor.
        #[arg(long, value_name = "VENDOR", conflicts_with = "host")]
        vendor: Option<String>,
        /// Restrict the report to one publisher host (e.g. `--host meta`),
        /// counting every provider that served that publisher's models. The
        /// host is the segment before `/` in the model id, so it sums enterprise
        /// direct + OpenRouter + any other route for the same publisher.
        /// Mutually exclusive with `--vendor` (which scopes by who served the
        /// call, not who published the model).
        #[arg(long, value_name = "HOST", conflicts_with = "vendor")]
        host: Option<String>,
    },
    /// Model health / circuit-breaker status.
    Health {
        /// Actively ping every free model and record the result.
        #[arg(long)]
        probe: bool,
        /// Probe every configured provider's live model catalog (not just
        /// the default provider / free chat candidates). Discovery calls
        /// `GET /v1/models` per provider; if that fails, falls back to
        /// the provider's declared model. Output is a per-provider report
        /// with model/status/latency rows.
        #[arg(long)]
        all: bool,
        /// Install the daily `--probe --all` sweep into launchd (macOS) or
        /// a systemd user timer (Linux). Idempotent: re-running overwrites
        /// the existing job.
        #[arg(long)]
        install_daily: bool,
        /// Remove the daily sweep installed by `install-daily`. Idempotent.
        #[arg(long)]
        uninstall_daily: bool,
    },
    /// FinOps observability rollup over the local ledger (no enforcement).
    /// Subcommands: `report | advisor | forecast`.
    /// Reads from the same `~/.zoder/ledger.jsonl` and `~/.zoder/pricing.json`
    /// as `zoder spend` / `zoder report`. Reports only.
    Finops {
        /// report | advisor | forecast
        #[arg(default_value = "report")]
        sub: String,
        /// Only include spend on/after this date (YYYY-MM-DD).
        #[arg(long)]
        since: Option<String>,
        /// Only include spend on/before this date (YYYY-MM-DD).
        #[arg(long)]
        until: Option<String>,
        /// Forecast window in days.
        #[arg(long, default_value_t = 30)]
        window_days: u32,
        /// Emit machine-readable JSON to stdout.
        #[arg(long)]
        json: bool,
    },
    /// List configured providers.
    Providers,
    /// Show or validate configuration + corpus.
    Config {
        /// Exit non-zero if the configuration is invalid.
        #[arg(long)]
        validate: bool,
    },
    /// Reconcile the model corpus against the live served-model list.
    Refresh,
    /// Pricing catalog: sync per-token rates from public price lists, or look
    /// up a model's rate.
    Pricing {
        #[command(subcommand)]
        action: PricingCmd,
    },
    /// Reconcile local ledger spend against a provider's billed-dollar cost API
    /// (admin key; OpenAI + Anthropic). Org-level, lagged: a nightly true-up,
    /// not real-time metering.
    Reconcile {
        /// Provider to reconcile: openai | anthropic.
        provider: String,
        /// Trailing window in days.
        #[arg(long, default_value_t = 14)]
        days: i64,
    },
    /// List saved multi-turn sessions.
    Sessions,
    /// Run a code review against local git state (codex `/review` equivalent).
    /// Emits a structured JSON verdict (verdict/summary/findings/next_steps).
    Review {
        /// Base ref for branch review (e.g. `main`); implies branch scope.
        #[arg(long)]
        base: Option<String>,
        /// Review scope: auto | working-tree | branch.
        #[arg(long, value_enum, default_value_t = ReviewScope::Auto)]
        scope: ReviewScope,
        /// Run additional reviewer models in parallel (comma-separated ids).
        #[arg(long, value_name = "M1,M2,...")]
        panel: Option<String>,
        /// Run detached as a tracked background job (see `status`/`result`).
        #[arg(long)]
        background: bool,
    },
    /// Adversarial review: challenge the approach, design, and assumptions.
    /// Accepts optional focus text after the flags.
    #[command(name = "adversarial-review")]
    AdversarialReview {
        #[arg(long)]
        base: Option<String>,
        #[arg(long, value_enum, default_value_t = ReviewScope::Auto)]
        scope: ReviewScope,
        #[arg(long, value_name = "M1,M2,...")]
        panel: Option<String>,
        #[arg(long)]
        background: bool,
        /// Extra focus for the reviewer (free text).
        #[arg(trailing_var_arg = true)]
        focus: Vec<String>,
    },
    /// Delegate an investigation or fix to an agentic, write-capable run
    /// (codex `/rescue` equivalent). Honors `-C`, `--agent`, `-m`, `--approve`.
    Rescue {
        /// What to investigate, solve, or continue (free text; bare words are
        /// joined). Flags may appear in any position; use stdin/`-` for text
        /// starting with `-`.
        task: Vec<String>,
        /// Run detached as a tracked background job.
        #[arg(long)]
        background: bool,
    },
    /// Show active and recent background jobs (or one job's full detail).
    Status {
        /// Specific job id (default: list all jobs in this repo/session).
        job: Option<String>,
        /// Include jobs from all working directories.
        #[arg(long)]
        all: bool,
    },
    /// Show the stored final output for a finished background job.
    Result {
        /// Job id (default: most recent).
        job: Option<String>,
    },
    /// Cancel an active background job.
    Cancel {
        /// Job id (default: most recent running).
        job: Option<String>,
    },
    /// Autonomous fix loop: author -> validate (build/test) -> adversarial
    /// review -> fix, repeating until the check passes and the reviewer raises
    /// no blocking findings (or `--max-iters`). The author keeps one engine
    /// session for continuity; every turn is cost-tracked. "Grind until green",
    /// the codex/claude-code workflow on open-source models.
    #[command(name = "loop")]
    Loop {
        /// What to fix (free text; bare words are joined). Falls back to
        /// `-i FILE`, then stdin. Flags (`--check`, `--reviewer`, `-m`, …) may
        /// appear in any position; use `-i FILE` for task text starting with `-`.
        task: Vec<String>,
        /// Read the task/instructions from a file instead of args.
        #[arg(short = 'i', long = "instructions", value_name = "FILE")]
        instructions: Option<String>,
        /// Maximum author/review iterations before giving up.
        #[arg(long, default_value_t = 6)]
        max_iters: usize,
        /// Validation command that must exit 0 (e.g. "cargo test -p foo"). Run
        /// after each author turn; failing output is fed to the next iteration.
        #[arg(long, value_name = "CMD")]
        check: Option<String>,
        /// Reviewer model id (default: the routed/`-m` model). Use a different
        /// strong model for a genuine adversarial second opinion.
        #[arg(long, value_name = "MODEL")]
        reviewer: Option<String>,
        /// Base ref for branch-scope diffing.
        #[arg(long)]
        base: Option<String>,
        /// Diff scope shown to the reviewer: auto | working-tree | branch.
        #[arg(long, value_enum, default_value_t = ReviewScope::Auto)]
        scope: ReviewScope,
        /// Resolve as soon as `--check` exits 0, treating reviewer findings as
        /// advisory (escape hatch for over-strict reviewers). Requires `--check`.
        #[arg(long)]
        accept_on_green: bool,
        /// Run detached as a tracked background job.
        #[arg(long)]
        background: bool,
    },
    /// Print a resumable engine session id for the current working dir
    /// (codex `/transfer` equivalent) so a follow-up can `--session <id>`.
    Transfer,
    /// Start an interactive agentic session (goose `session` equivalent):
    /// launches the zerocode terminal UI wired to the engine.
    Session {
        /// Forwarded to zerocode (e.g. `--theme NAME`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Headless agentic run (goose `run` equivalent): execute a task to
    /// completion without the UI. Honors `-C`, `--agent`, `-m`, `--approve`.
    Run {
        /// Task text (goose `-t/--text`).
        #[arg(short = 't', long = "text")]
        text: Option<String>,
        /// Read the task/instructions from a file (goose `-i/--instructions`).
        #[arg(short = 'i', long = "instructions", value_name = "FILE")]
        instructions: Option<String>,
        /// Run detached as a tracked background job.
        #[arg(long)]
        background: bool,
    },
    /// Saved prompt/agent templates (goose `recipe` equivalent).
    Recipe {
        #[command(subcommand)]
        action: RecipeCmd,
    },
    /// List MCP extensions/servers configured in the engine (goose extensions).
    Mcp {
        #[command(subcommand)]
        action: McpCmd,
    },
    /// Show/validate/edit configuration (goose `configure` equivalent).
    Configure {
        /// Open the config file in $EDITOR.
        #[arg(long)]
        edit: bool,
        /// Exit non-zero if the configuration is invalid.
        #[arg(long)]
        validate: bool,
    },
    /// Generate shell completions (bash|zsh|fish|powershell|elvish).
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Run the CI-parity gate against the current repo (or `--root DIR`).
    /// Detects ecosystems (Rust/Node/Python/Go), derives a plan from the
    /// repo's marker files + baseline OSS-hygiene defaults, runs it under
    /// the managed-tool bundle (cargo-deny, cargo-audit, osv-scanner,
    /// gitleaks, cyclonedx, govulncheck, pip-audit), and prints the
    /// honest-degradation GateReport. The default mode is `strict`
    /// (fail-closed); `--local-iterate` records skips instead of failing
    /// (use only for inner-loop speed; never to bypass the gate). Exits
    /// 0 only when the gate is Green. See `docs/CI-PARITY-GATE.md`.
    Gate {
        /// Repo root to gate (default: current working directory).
        #[arg(long, value_name = "DIR")]
        root: Option<String>,
        /// Local-iterate mode: missing tools become Skipped (Yellow) so
        /// you can iterate fast on a dev box. The gate reports which
        /// checks were skipped and what install commands restore the
        /// strict posture. Use this only for inner-loop speed; the
        /// `--strict` default is the posture that earns the "it passed
        /// the gate" claim.
        #[arg(long, conflicts_with = "strict")]
        local_iterate: bool,
        /// Strict mode (fail-closed). This is the DEFAULT; the flag
        /// exists so `zoder gate --strict` is a legible escape hatch
        /// when `--local-iterate` is wired into a wrapper.
        #[arg(long)]
        strict: bool,
        /// Print what gate tools are installed vs missing + the install
        /// hint for each missing tool, then exit. Does not run any
        /// steps. Useful for `zoder gate --tools-only` in CI caches.
        #[arg(long)]
        tools_only: bool,
        /// Print the derived plan (ecosystems + framework hints +
        /// step list) without running anything. Useful for inspecting
        /// what the gate would actually do.
        #[arg(long)]
        plan_only: bool,
        /// Emit machine-readable JSON to stdout (the GateReport +
        /// tool-probe, serialized). Suppresses the pretty renderer.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Clone)]
enum RecipeCmd {
    /// List saved recipes in ~/.zoder/recipes.
    List,
    /// Show a recipe's contents.
    Show { file: String },
    /// Run a recipe (JSON: {prompt, model?, agent?, cwd?, oneshot?}).
    Run {
        /// Recipe file path, or a bare name resolved in ~/.zoder/recipes.
        file: String,
    },
}

#[derive(Subcommand, Clone)]
enum McpCmd {
    /// List MCP servers/extensions configured in the engine config.
    List,
}

/// Review target selection for `review` / `adversarial-review`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ReviewScope {
    /// Working tree if dirty, else the branch vs its base.
    Auto,
    /// Uncommitted + staged changes only.
    WorkingTree,
    /// Committed branch changes vs base.
    Branch,
}

#[derive(Subcommand, Clone)]
enum PricingCmd {
    /// Sync the pricing catalog from public price lists (LiteLLM + OpenRouter).
    ///
    /// By default this only refreshes the local report catalog (`~/.zoder/
    /// pricing.json`) used for the avoided-spend headline and reconcile. zoder
    /// stays your provider's billing-authoritative for `cost_usd` and never feeds the cost
    /// engine. Pass `--external` to *also* publish per-token rates for your
    /// configured external (paid) providers into the zeroclaw cost engine, so
    /// those non-vendor models report a real `cost_usd` in the dashboard and
    /// reports. free-tier models are never priced this way.
    Refresh {
        /// Source(s): litellm | openrouter | both.
        #[arg(long, default_value = "both")]
        source: String,
        /// Model used as the avoided-spend baseline in the report.
        #[arg(long, default_value = "gpt-4o")]
        baseline: String,
        /// Also publish rates for configured external (paid) providers into the
        /// zeroclaw cost engine (`<data_dir>/pricing.json`). No-op unless the
        /// config has a non-free provider. free-tier stay $0.
        #[arg(long)]
        external: bool,
    },
    /// Show the catalog rate for a model (per-token components, USD per Mtok).
    Show {
        /// Model id (tolerant: exact, case-insensitive, or leaf match).
        model: String,
    },
}

fn read_prompt(arg: Option<String>) -> anyhow::Result<String> {
    match arg.as_deref() {
        Some("-") | None => {
            if std::io::stdin().is_terminal() {
                if let Some(p) = arg {
                    if p != "-" {
                        return Ok(p);
                    }
                }
                anyhow::bail!("no prompt given (pass a prompt or pipe via stdin)");
            }
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            Ok(s)
        }
        Some(p) => Ok(p.to_string()),
    }
}

struct Engine {
    cfg: Config,
    corpus: Corpus,
}

impl Engine {
    fn load() -> anyhow::Result<Self> {
        let cfg = Config::load()?;
        let corpus = Corpus::load(&cfg.corpus_path)?;
        Ok(Self { cfg, corpus })
    }
}

/// Quota-aware routing context: a snapshot of (ledger entries, tier catalog)
/// taken once per command execution so the smart router can consult window
/// usage on the calling thread without re-reading `ledger.jsonl` and the
/// catalog file for every model in the fallback chain. The tier catalog is
/// loaded the same way the report does (`load_tier_catalog`), so the router
/// and the utilization report agree on what's "exhausted". The struct is
/// cheap to construct (two file reads) and cheap to pass around (just refs).
struct RoutingContext {
    entries: Vec<Entry>,
    catalog: zoder_core::subscription_tiers::TierCatalog,
}

impl RoutingContext {
    fn load(cfg: &Config) -> Self {
        let entries = Ledger::new(&cfg.ledger_path).entries().unwrap_or_default();
        let catalog = load_tier_catalog(Some(
            &zoder_core::subscription_tiers::default_catalog_path(&Config::home()),
        ));
        Self { entries, catalog }
    }

    /// Quota-aware variant of [`Config::real_provider_for_model`]. The CLI
    /// router calls this in preference to the no-ledger form so a
    /// subscription provider whose rolling window is at/over cap
    /// transparently falls through to its metered sibling (vendor
    /// dual-billing). Returns `None` for unbacked models so the caller can
    /// hard-error with a clear message instead of dialing the placeholder.
    fn real_provider_for_model<'a>(&self, cfg: &'a Config, model_id: &str) -> Option<&'a Provider> {
        cfg.real_best_provider_for_model(model_id, &self.entries, &self.catalog)
    }
}

fn init_tracing(verbose: u8) {
    use tracing_subscriber::{fmt, EnvFilter};
    // Precedence: RUST_LOG wins; otherwise -v/-vv set the level.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(match verbose {
            0 => "warn",
            1 => "info,zoder=debug,zoder_core=debug",
            _ => "debug,zoder=trace,zoder_core=trace",
        })
    });
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .try_init();
}

#[tokio::main]
async fn main() {
    // Restore default SIGPIPE handling so piping into `head`/`less` (which close
    // the reader early) ends the process quietly instead of panicking on a
    // broken pipe mid-table.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let res = run().await;
    // When this process is the detached worker of a background job, stamp the
    // job's terminal status from the outcome.
    if let Some(dir) = agentic::active_job_dir() {
        agentic::finalize_job(&dir, res.is_ok());
    }
    if let Err(e) = res {
        eprintln!("zoder: error: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let result = match &cli.cmd {
        Some(Cmd::Models { free, paid, all }) => cmd_models(*free, *paid, *all, cli.json),
        Some(Cmd::Update { check }) => cmd_update(*check).await,
        Some(Cmd::Route { prompt }) => cmd_route(&cli, prompt.clone()),
        Some(Cmd::Consult { free_only, limit }) => cmd_consult(&cli, *free_only, *limit),
        Some(Cmd::Spend {
            period,
            since,
            until,
            by_model,
            host,
        }) => cmd_spend(
            period,
            since.as_deref(),
            until.as_deref(),
            *by_model,
            host.as_deref(),
            cli.json,
        ),
        Some(Cmd::Report {
            period,
            days,
            top,
            vendor,
            host,
        }) => {
            cmd_report(
                *period,
                *days,
                *top,
                vendor.as_deref(),
                host.as_deref(),
                cli.json,
            )
            .await
        }
        Some(Cmd::Health {
            probe,
            all,
            install_daily,
            uninstall_daily,
        }) => {
            cmd_health(
                &cli,
                HealthCmd {
                    probe: *probe,
                    all: *all,
                    install_daily: *install_daily,
                    uninstall_daily: *uninstall_daily,
                },
            )
            .await
        }
        Some(Cmd::Finops {
            sub,
            since,
            until,
            window_days,
            json,
        }) => cmd_finops(sub, since.as_deref(), until.as_deref(), *window_days, *json).await,
        Some(Cmd::Providers) => cmd_providers(cli.json),
        Some(Cmd::Config { validate }) => cmd_config(*validate),
        Some(Cmd::Refresh) => cmd_refresh(&cli).await,
        Some(Cmd::Pricing { action }) => cmd_pricing(action, cli.json).await,
        Some(Cmd::Reconcile { provider, days }) => cmd_reconcile(provider, *days, cli.json).await,
        Some(Cmd::Sessions) => cmd_sessions(cli.json),
        Some(Cmd::Review {
            base,
            scope,
            panel,
            background,
        }) => {
            agentic::cmd_review(
                &cli,
                base.clone(),
                *scope,
                panel.clone(),
                *background,
                false,
                &[],
            )
            .await
        }
        Some(Cmd::AdversarialReview {
            base,
            scope,
            panel,
            background,
            focus,
        }) => {
            agentic::cmd_review(
                &cli,
                base.clone(),
                *scope,
                panel.clone(),
                *background,
                true,
                focus,
            )
            .await
        }
        Some(Cmd::Rescue { task, background }) => {
            agentic::cmd_rescue(&cli, task, *background).await
        }
        Some(Cmd::Status { job, all }) => agentic::cmd_status(&cli, job.clone(), *all),
        Some(Cmd::Result { job }) => agentic::cmd_result(&cli, job.clone()),
        Some(Cmd::Cancel { job }) => agentic::cmd_cancel(&cli, job.clone()),
        Some(Cmd::Loop {
            task,
            instructions,
            max_iters,
            check,
            reviewer,
            base,
            scope,
            accept_on_green,
            background,
        }) => {
            agentic::cmd_loop(
                &cli,
                task,
                instructions.clone(),
                *max_iters,
                check.clone(),
                reviewer.clone(),
                base.clone(),
                *scope,
                *accept_on_green,
                *background,
                cli.loop_timeout.unwrap_or(900),
            )
            .await
        }
        Some(Cmd::Transfer) => agentic::cmd_transfer(&cli).await,
        Some(Cmd::Session { args }) => cmd_tui(args),
        Some(Cmd::Run {
            text,
            instructions,
            background,
        }) => goose::cmd_run(&cli, text.clone(), instructions.clone(), *background).await,
        Some(Cmd::Recipe { action }) => goose::cmd_recipe(&cli, action).await,
        Some(Cmd::Mcp { action }) => goose::cmd_mcp(&cli, action),
        Some(Cmd::Configure { edit, validate }) => goose::cmd_configure(*edit, *validate),
        Some(Cmd::Gate {
            root,
            local_iterate,
            strict,
            tools_only,
            plan_only,
            json,
        }) => cmd_gate(
            root.as_deref(),
            *local_iterate,
            *strict,
            *tools_only,
            *plan_only,
            *json,
        ),
        Some(Cmd::Completions { shell }) => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            // Generate into an in-memory buffer first (clap_complete writes
            // directly and panics on a write error), then flush it to stdout
            // ourselves so a closed pipe (`... | head`) is a clean no-op.
            let mut buf: Vec<u8> = Vec::new();
            clap_complete::generate(*shell, &mut cmd, name, &mut buf);
            match std::io::stdout().write_all(&buf) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
                Err(e) => Err(e.into()),
            }
        }
        Some(Cmd::Exec { prompt }) => cmd_exec(&cli, prompt.clone()).await,
        Some(Cmd::Tui { args }) => cmd_tui(args),
        None => {
            let p = cli.prompt.clone();
            cmd_exec(&cli, p).await
        }
    };
    // Best-effort, throttled "a new release is available" hint (stderr). Never fatal.
    maybe_notify_update(&cli).await;
    result
}

/// Print a one-line "new release available" hint at most once a day, unless the
/// invocation wants clean machine output (`--json`) or it is the `update`
/// command itself. Cached + opt-out via `ZODER_NO_UPDATE_CHECK=1`; any failure
/// (offline, etc.) is silently ignored.
async fn maybe_notify_update(cli: &Cli) {
    if cli.json || matches!(cli.cmd, Some(Cmd::Update { .. })) {
        return;
    }
    let home = Config::home();
    if let Some(st) =
        zoder_core::update::check_cached(&home, std::time::Duration::from_secs(86_400)).await
    {
        if st.newer {
            let p = Pal::new();
            eprintln!(
                "{}",
                p.dim(&format!(
                    "zoder {} → a new release v{} is available. Run `zoder update` ({}).",
                    st.current,
                    st.latest,
                    zoder_core::update::release_url()
                ))
            );
        }
    }
}

/// `zoder update [--check]`: report whether a newer release exists; without
/// `--check`, re-run the official installer (platform detect + SHA256 verify +
/// atomic install) to self-replace this binary.
async fn cmd_update(check_only: bool) -> anyhow::Result<()> {
    let p = Pal::new();
    let st = zoder_core::update::check().await?;
    if !st.newer {
        println!(
            "{}",
            p.green_b(&format!("zoder {} is the latest release.", st.current))
        );
        return Ok(());
    }
    println!(
        "{}",
        p.amber(&format!(
            "A new zoder release is available: v{} (you have {}).",
            st.latest, st.current
        ))
    );
    println!("  release notes: {}", zoder_core::update::release_url());
    if check_only {
        println!("  update:        {}", zoder_core::update::install_command());
        return Ok(());
    }
    let cmd = zoder_core::update::install_command();
    println!("{}", p.dim(&format!("updating via: {cmd}")));
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to launch installer: {e}"))?;
    if status.success() {
        println!("{}", p.green_b("zoder updated to the latest release."));
        Ok(())
    } else {
        anyhow::bail!("installer exited unsuccessfully ({status}); run manually: {cmd}")
    }
}

/// Locate a co-shipped binary: next to the current exe, then on `$PATH`, then
/// in `~/.local/bin`. zoder ships `zerocode` + `zeroclaw` beside itself.
fn locate_sibling(name: &str) -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        for d in std::env::split_paths(&path) {
            let p = d.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let p = std::path::PathBuf::from(home).join(".local/bin").join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Per-tool zerocode config dir under `$HOME`, and the default TUI theme seeded
/// into it on first run. zoder uses its own `~/.zoder` dir with a blue
/// (`icy_blue`) theme so it is visually distinct from sibling tools and ships
/// with clean state. zerocode couples the theme to its `--config-dir`, so a
/// distinct theme means a distinct config dir.
const TUI_CONFIG_SUBDIR: &str = ".zoder";
const TUI_DEFAULT_THEME: &str = "icy_blue";

/// Seed a minimal `zerocode-config.toml` with `theme` if the per-tool config dir
/// has none yet, so a fresh dir opens with the intended default. Never overwrites
/// an existing config — the user's in-TUI theme choice always wins. Best-effort:
/// any error is ignored (zerocode falls back to its own default theme).
fn ensure_zerocode_theme(dir: &std::path::Path, theme: &str) {
    let f = dir.join("zerocode-config.toml");
    if f.exists() {
        return;
    }
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let body = format!("locale = \"en\"\n\n[keybindings]\n\n[theme]\nname = \"{theme}\"\n");
    let _ = std::fs::write(&f, body);
}

/// `zoder gate [--root DIR] [--strict|--local-iterate] [--tools-only|
/// --plan-only] [--json]` — run the CI-parity gate against a repo.
///
/// Detection -> plan derivation -> runner -> honest-degradation report.
/// The default mode is `strict` (fail-closed): a missing REQUIRED tool
/// surfaces as a `Failed` outcome with the managed-bundle install hint
/// attached, and the exit code is non-zero unless the gate is Green.
///
/// `--local-iterate` records skips as Skipped (Yellow) instead of
/// failing closed; this is the inner-loop speed mode. The audit trail
/// is preserved (every skipped step + reason is in the report), but
/// the gate WILL NOT block. Use this only when iterating on a dev box;
/// the strict default is the posture that earns the "it passed the
/// gate" claim.
///
/// `--tools-only` prints the tool-availability probe and exits 0.
/// Useful for CI cache prep and for "what do I need to install?".
///
/// `--plan-only` prints the derived plan and exits 0. No steps are
/// executed.
///
/// `--json` emits a structured JSON document (probe + plan + report)
/// to stdout instead of the pretty renderer; suitable for CI
/// summarizers and dashboards.
///
/// Exit codes:
///   0 — Green (every required check ran and passed, nothing skipped)
///       OR Yellow (some optional check was skipped, but no required
///       check failed). The report tells you exactly which tools to
///       install to get back to Green.
///   1 — Red (one or more required checks failed). Under Strict mode
///       this also covers a missing REQUIRED tool (the runner upgrades
///       a missing required tool to Failed so the gate never silently
///       passes it).
///   64 — usage error (root not a directory, conflicting flags, …)
fn cmd_gate(
    root: Option<&str>,
    local_iterate: bool,
    strict: bool,
    tools_only: bool,
    plan_only: bool,
    json: bool,
) -> anyhow::Result<()> {
    // Resolve the repo root. Default to the current directory; expand
    // ~ (the CLI does not pull in shellexpand, so a minimal hand-rolled
    // expansion is enough — `~` and `~/sub/path`).
    let root_path = match root {
        Some(r) => expand_tilde(r),
        None => std::env::current_dir()
            .map_err(|e| anyhow::anyhow!("zoder gate: cannot read current directory: {e}"))?,
    };
    if !root_path.is_dir() {
        anyhow::bail!(
            "zoder gate: root `{}` is not a directory",
            root_path.display()
        );
    }

    // Mode resolution: --strict is the default; --local-iterate is an
    // opt-in demotion for inner-loop speed only. Both flags explicitly
    // passed is a usage error (we declared `conflicts_with` in clap, so
    // clap already rejects it).
    let mode = if local_iterate {
        GateMode::LocalIterate
    } else {
        // `strict` here is just for explicit-over-implicit readability.
        GateMode::Strict
    };
    let _ = strict; // explicit flag exists for legibility; the default already IS strict.

    // Drive the gate through the pure orchestrator so every branch is
    // unit-testable without touching stdout or the process exit code.
    let outcome = run_gate_for_root(&root_path);

    // --tools-only: print the probe and exit (never blocks).
    if tools_only {
        if json {
            let payload = serde_json::json!({
                "root": root_path.display().to_string(),
                "mode": mode_label(mode),
                "signals": signals_payload(&outcome.signals),
                "probe": probe_payload(&outcome.probe),
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
        } else {
            println!("zoder gate — tool availability ({})", root_path.display());
            println!("{}", render_signals_human(&outcome.signals));
            println!();
            print!("{}", render_probe(&outcome.probe));
        }
        return Ok(());
    }

    // --plan-only: print the plan + probe and exit (never blocks).
    if plan_only {
        if json {
            let payload = serde_json::json!({
                "root": root_path.display().to_string(),
                "mode": mode_label(mode),
                "signals": signals_payload(&outcome.signals),
                "compat": compat_payload(&outcome.pre_run_compat),
                "probe": probe_payload(&outcome.probe),
                "plan": plan_payload(&outcome.plan),
            });
            println!("{}", serde_json::to_string_pretty(&payload)?);
        } else {
            println!("zoder gate — plan ({})", root_path.display());
            println!("{}", render_signals_human(&outcome.signals));
            println!();
            println!(
                "breakdown: {} runnable, {} added-baseline, {} skipped",
                outcome.pre_run_compat.runnable.len(),
                outcome.pre_run_compat.added_baseline.len(),
                outcome.pre_run_compat.skipped.len(),
            );
            for s in &outcome.plan {
                let req = if s.required { "*" } else { " " };
                let cmd = s.command.join(" ");
                println!("  {} {:<14} {cmd}", req, s.name);
            }
            println!();
            print!("{}", render_probe(&outcome.probe));
        }
        return Ok(());
    }

    // Run the plan against the real PATH and produce the final report.
    let report = outcome.run(&mode);
    // We treat the renderer as the canonical output and the exit code
    // as the derived signal. Under strict posture, Yellow (some
    // required tool missing) IS a block — that's the fail-closed
    // contract. Under local-iterate, Yellow is just informational.
    if json {
        let payload = serde_json::json!({
            "root": root_path.display().to_string(),
            "mode": mode_label(mode),
            "signals": signals_payload(&outcome.signals),
            "probe": probe_payload(&outcome.probe),
            "report": {
                "status": status_payload(&report.status),
                "is_passed": report.is_passed(),
                "is_failed": report.is_failed(),
                "headline": report.headline(),
                "results": report.results.iter().map(|r| {
                    serde_json::json!({
                        "name": r.step_name,
                        "required": r.required,
                        "outcome": step_outcome_payload(&r.outcome),
                    })
                }).collect::<Vec<_>>(),
                "compatibility": compat_payload(&report.compatibility),
                "passed_required_names": report.passed_required_names(),
            },
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        // Human renderer: pretty multi-line report. Then append the
        // tool-availability probe as a trailing section so the
        // reviewer can scan for install hints without flipping to
        // --tools-only.
        println!(
            "zoder gate — {} ({})",
            mode_label(mode),
            root_path.display()
        );
        println!("{}", render_signals_human(&outcome.signals));
        println!();
        println!("{}", report.to_pretty());
        print!("{}", render_probe(&outcome.probe));
    }

    // Exit code (fail-closed posture, per docs/CI-PARITY-GATE.md):
    //   Green -> 0
    //   Yellow -> 0 (under both modes — Yellow means "something was
    //     skipped, but nothing failed"; the report records what was
    //     skipped and why, so the audit trail is complete and the
    //     operator can act on it without the gate silently passing).
    //     Under strict, a missing REQUIRED tool upgrades to Failed
    //     (Red) inside run_plan — Yellow under strict therefore only
    //     means "an OPTIONAL tool was missing", which is advisory.
    //   Red -> 1
    let code: i32 = match &report.status {
        GateStatus::Green => 0,
        GateStatus::Yellow { .. } => 0,
        GateStatus::Red { .. } => 1,
    };
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// The pure orchestrator for `zoder gate`. Given a repo root, detect
/// ecosystems, derive the plan from the baseline OSS-hygiene defaults,
/// and build a [`GateOutcome`] that holds everything the CLI needs to
/// render `--tools-only`, `--plan-only`, and the final report. The
/// actual step execution is deferred to [`GateOutcome::run`] so the
/// pre-run data is independent of execution.
///
/// Extracted out of `cmd_gate` so every branch (Rust, Node, polyglot,
/// missing markers, etc.) is fully unit-testable without touching
/// stdout or the process exit code.
struct GateOutcome {
    signals: RepoSignals,
    /// Pre-run compatibility breakdown — what `derive_plan` produced
    /// before the runner touched anything. Used by `--plan-only`.
    pre_run_compat: CompatibilityReport,
    /// The derived plan (in execution order).
    plan: Vec<GateStep>,
    /// Tool probe for every unique `tool` referenced by the plan.
    probe: Vec<zoder_core::gate_bundle::ToolProbe>,
}

impl GateOutcome {
    /// Run the plan against the real PATH and produce the final report.
    /// The runner's `run_plan` + `GateReport::new` recomputes the
    /// status from results so the report stays self-consistent.
    fn run(&self, mode: &GateMode) -> GateReport {
        let env = PathEnv::new();
        let (results, _) = run_plan(&self.plan, *mode, &env);
        GateReport::new(results, self.pre_run_compat.clone())
    }
}

fn run_gate_for_root(root: &std::path::Path) -> GateOutcome {
    let marker_names = discover_markers(root);
    let marker_refs: Vec<&str> = marker_names.iter().map(String::as_str).collect();
    let signals = detect_repo_signals(&marker_refs);

    // Plan derivation: per-ecosystem baseline unioned into a single
    // plan. Today the repo-CI YAML adapters (Actions / GitLab /
    // Woodpecker -> CiJob) are future slices; the gate runs the
    // baseline OSS-hygiene plan against whatever ecosystems the repo
    // advertises.
    let mut plan: Vec<GateStep> = Vec::new();
    let mut compat = CompatibilityReport::default();
    for eco in &signals.ecosystems {
        let baseline = baseline_plan_for(*eco, &marker_refs);
        for step in baseline {
            compat.added_baseline.push(step.name.clone());
            plan.push(step);
        }
    }

    // Tool probe: what's installed + what's missing. Reused by every
    // output path (--tools-only, --plan-only, full report).
    let lookup = ToolLookup::from_default_bundle();
    let env = PathEnv::new();
    let probe = probe_tools(&plan, &env, &lookup);

    GateOutcome {
        signals,
        pre_run_compat: compat,
        plan,
        probe,
    }
}

/// Render the [`RepoSignals`] as a one-line-per-bucket human summary.
fn render_signals_human(signals: &RepoSignals) -> String {
    let mut out = String::new();
    let eco_names: Vec<&str> = signals
        .ecosystems
        .iter()
        .map(|e| match e {
            zoder_core::gate::Ecosystem::Rust => "rust",
            zoder_core::gate::Ecosystem::Node => "node",
            zoder_core::gate::Ecosystem::Python => "python",
            zoder_core::gate::Ecosystem::Go => "go",
        })
        .collect();
    out.push_str(&format!("ecosystems:      [{}]\n", eco_names.join(", ")));
    let pms: Vec<String> = signals
        .package_managers
        .iter()
        .map(|(eco, pm)| {
            let eco_name = match eco {
                zoder_core::gate::Ecosystem::Rust => "rust",
                zoder_core::gate::Ecosystem::Node => "node",
                zoder_core::gate::Ecosystem::Python => "python",
                zoder_core::gate::Ecosystem::Go => "go",
            };
            format!("{eco_name}={pm}", pm = pm.cli_name())
        })
        .collect();
    out.push_str(&format!(
        "package-mgrs:    [{}]\n",
        if pms.is_empty() {
            String::new()
        } else {
            pms.join(", ")
        }
    ));
    out.push_str(&format!(
        "framework-hints: [{}]\n",
        signals.framework_hints.join(", ")
    ));
    out
}

/// JSON-serializable view of [`RepoSignals`].
fn signals_payload(signals: &RepoSignals) -> serde_json::Value {
    serde_json::json!({
        "ecosystems": signals.ecosystems.iter().map(|e| match e {
            zoder_core::gate::Ecosystem::Rust => "rust",
            zoder_core::gate::Ecosystem::Node => "node",
            zoder_core::gate::Ecosystem::Python => "python",
            zoder_core::gate::Ecosystem::Go => "go",
        }).collect::<Vec<_>>(),
        "package_managers": signals.package_managers.iter().map(|(eco, pm)| {
            let eco_name = match eco {
                zoder_core::gate::Ecosystem::Rust => "rust",
                zoder_core::gate::Ecosystem::Node => "node",
                zoder_core::gate::Ecosystem::Python => "python",
                zoder_core::gate::Ecosystem::Go => "go",
            };
            serde_json::json!({
                "ecosystem": eco_name,
                "manager": pm.cli_name(),
            })
        }).collect::<Vec<_>>(),
        "framework_hints": signals.framework_hints,
    })
}

/// JSON-serializable view of [`CompatibilityReport`].
fn compat_payload(compat: &CompatibilityReport) -> serde_json::Value {
    serde_json::json!({
        "runnable": compat.runnable,
        "added_baseline": compat.added_baseline,
        "skipped": compat.skipped.iter().map(|(n, r)| serde_json::json!({
            "name": n,
            "reason": r,
        })).collect::<Vec<_>>(),
    })
}

/// JSON-serializable view of a tool probe.
fn probe_payload(probe: &[zoder_core::gate_bundle::ToolProbe]) -> serde_json::Value {
    probe
        .iter()
        .map(|p| {
            serde_json::json!({
                "tool": p.tool,
                "present": p.present,
                "resolved_path": p.resolved_path.as_ref().map(|pb| pb.display().to_string()),
                "managed": p.managed.map(|m| serde_json::json!({
                    "id": m.id,
                    "version": m.version,
                    "install_hint": m.install_hint.replace("${VERSION}", m.version),
                    "homepage": m.homepage,
                })),
            })
        })
        .collect()
}

/// JSON-serializable view of a [`GateStep`] list.
fn plan_payload(plan: &[GateStep]) -> serde_json::Value {
    plan.iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "category": format!("{:?}", s.category).to_lowercase(),
                "command": s.command,
                "tool": s.tool,
                "required": s.required,
            })
        })
        .collect()
}

/// JSON-serializable view of a [`GateStatus`].
fn status_payload(status: &GateStatus) -> serde_json::Value {
    match status {
        GateStatus::Green => serde_json::json!({"verdict": "green"}),
        GateStatus::Yellow { skipped } => {
            serde_json::json!({"verdict": "yellow", "skipped": skipped})
        }
        GateStatus::Red { failures } => {
            serde_json::json!({"verdict": "red", "failures": failures})
        }
    }
}

/// JSON-serializable view of a [`StepOutcome`]. The Skipped arm
/// carries the structured reason so reviewers can audit the gate
/// even when no pretty renderer ran.
fn step_outcome_payload(outcome: &StepOutcome) -> serde_json::Value {
    match outcome {
        StepOutcome::Passed => serde_json::json!("passed"),
        StepOutcome::Failed => serde_json::json!("failed"),
        StepOutcome::Skipped { reason } => serde_json::json!({
            "skipped": true,
            "reason": reason,
        }),
    }
}

/// Human label for a [`GateMode`].
fn mode_label(mode: GateMode) -> &'static str {
    match mode {
        GateMode::Strict => "strict",
        GateMode::LocalIterate => "local-iterate",
    }
}

/// Minimal `~` expansion for the `--root` flag. Supports `~` and
/// `~/sub/path`; everything else is returned as-is. We deliberately
/// avoid pulling in a shellexpand-style crate for this — the CLI is
/// the only consumer.
fn expand_tilde(input: &str) -> std::path::PathBuf {
    if input == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(rest) = input.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    std::path::PathBuf::from(input)
}

/// Launch the bundled zerocode TUI. zerocode discovers the `zeroclaw` daemon
/// binary next to its own exe and spawns an ephemeral daemon if none is running,
/// so co-locating the two (which the build/install does) is the entire wiring.
/// The default theme is switchable any time in the TUI's theme engine.
fn cmd_tui(extra: &[String]) -> anyhow::Result<()> {
    // Refresh the pricing feed in the background if stale, so the ephemeral
    // daemon zerocode is about to start loads current rates (zoder only).
    maybe_spawn_daily_refresh();
    let bin = locate_sibling("zerocode").ok_or_else(|| {
        anyhow::anyhow!(
            "zerocode binary not found (looked next to zoder, on PATH, and in \
             ~/.local/bin).\nInstall the matching build — zoder ships zerocode + \
             zeroclaw together."
        )
    })?;
    let mut cmd = std::process::Command::new(&bin);
    // Point zerocode at this tool's own config dir (so the TUI theme/state is
    // distinct from sibling tools) unless the caller already chose one via
    // --config-dir or ZEROCLAW_CONFIG_DIR. Empty subdir => use zerocode's default.
    let caller_chose_dir = std::env::var_os("ZEROCLAW_CONFIG_DIR").is_some()
        || extra
            .iter()
            .any(|a| a == "--config-dir" || a.starts_with("--config-dir="));
    if !caller_chose_dir && !TUI_CONFIG_SUBDIR.is_empty() {
        if let Some(home) = std::env::var_os("HOME") {
            let dir = std::path::PathBuf::from(home).join(TUI_CONFIG_SUBDIR);
            ensure_zerocode_theme(&dir, TUI_DEFAULT_THEME);
            cmd.arg("--config-dir").arg(&dir);
        }
    }
    cmd.args(extra);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // exec replaces this process on success; only returns on failure.
        let err = cmd.exec();
        Err(anyhow::anyhow!(
            "failed to exec zerocode ({}): {err}",
            bin.display()
        ))
    }
    #[cfg(not(unix))]
    {
        let status = cmd
            .status()
            .map_err(|e| anyhow::anyhow!("failed to run zerocode ({}): {e}", bin.display()))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn cmd_models(free: bool, paid: bool, all: bool, json: bool) -> anyhow::Result<()> {
    let eng = Engine::load()?;

    // JSON keeps the scriptable flag-filtered dump.
    if json {
        let mut models: Vec<_> = eng
            .corpus
            .models
            .iter()
            .filter(|m| {
                if paid {
                    m.paid
                } else if free {
                    m.free
                } else {
                    // Default and --all: the entire catalog.
                    true
                }
            })
            .collect();
        models.sort_by(|a, b| {
            b.agentic_score
                .unwrap_or(0.0)
                .partial_cmp(&a.agentic_score.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        println!("{}", serde_json::to_string_pretty(&models)?);
        return Ok(());
    }

    let p = Pal::new();
    let pricing = PricingCatalog::load(&Config::home().join("pricing.json"));
    // Live health/latency from the on-disk health store (updated as models are
    // called). Drives the "health" and "resp" columns so the operator sees which
    // models are ready and how fast they have been responding.
    let health = HealthStore::load(&eng.cfg.health_path);

    // Section selection: default (and --all) lists every model with Free on top
    // and Paid below; --free / --paid narrow to one section. Real model ids are
    // always shown, never aliases.
    let show_free = free || all || !paid;
    let show_paid = paid || all || !free;

    let by_agentic = |a: &f64, b: &f64| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal);
    let elo = |v: Option<f64>| match v {
        Some(x) if x > 0.0 => Cell::new(format!("{x:.0}")),
        _ => Cell::dim("—"),
    };
    let agentic = |v: Option<f64>| match v {
        Some(x) if x > 0.0 => Cell::new(format!("{x:.3}")),
        _ => Cell::dim("—"),
    };
    // Unified coding capability: mean of present benchmark scores, tagged with
    // the most-authoritative contributing source (e.g. `75.2 (vals.ai)`).
    let code_cap = |m: &ModelEntry| -> Cell {
        match (m.code_capability(), m.code_capability_source()) {
            (Some(c), Some(src)) => Cell::new(format!("{c:.1} ({src})")),
            (Some(c), None) => Cell::new(format!("{c:.1}")),
            _ => Cell::dim("—"),
        }
    };
    // Crowd-preference (arena.ai), shown separately from the solve-rate `code`.
    let arena = |m: &ModelEntry| -> Cell {
        match m.arena_label() {
            Some(s) => Cell::new(s),
            None => Cell::dim("—"),
        }
    };
    // Health label + response time, looked up by real model id. No data yet =>
    // dim "—" (model has not been exercised in this environment).
    let health_cell = |id: &str| -> Cell {
        match health.models.get(id) {
            None => Cell::dim("—"),
            Some(h) => match h.state() {
                State::Healthy => Cell::green("ready"),
                State::Degraded => Cell::amber("degraded"),
                State::Down => Cell::amber("down"),
            },
        }
    };
    let resp_cell = |id: &str| -> Cell {
        match health.models.get(id).and_then(|h| h.ewma_latency_ms) {
            Some(ms) if ms >= 1000.0 => Cell::new(format!("{:.1}s", ms / 1000.0)),
            Some(ms) => Cell::new(format!("{ms:.0}ms")),
            None => Cell::dim("—"),
        }
    };
    // Curated per-workflow suitability `single-pass/grind` (— when the model is
    // not in the known-good SWE list). Drives `--tier single-pass|grind`.
    let wf_cell = |m: &ModelEntry| -> Cell {
        match m.workflows.as_ref() {
            Some(w) => {
                let f = |v: Option<f64>| v.map(|x| format!("{x:.2}")).unwrap_or_else(|| "—".into());
                Cell::new(format!("{}/{}", f(w.single_pass), f(w.grind)))
            }
            None => Cell::dim("—"),
        }
    };

    if show_free {
        let mut free_m: Vec<_> = eng.corpus.models.iter().filter(|m| !m.paid).collect();
        free_m.sort_by(|a, b| {
            by_agentic(
                &a.agentic_score.unwrap_or(0.0),
                &b.agentic_score.unwrap_or(0.0),
            )
        });
        println!(
            "\n{}  {}",
            p.green_b("Free models"),
            p.dim(&format!("{} models — $0 on free-tier", free_m.len()))
        );
        let mut t = Table::new(
            &p,
            vec![
                ("model", Al::L),
                ("family", Al::L),
                ("health", Al::L),
                ("resp", Al::R),
                ("ovr", Al::R),
                ("swe", Al::R),
                ("code", Al::R),
                ("arena", Al::R),
                ("agentic", Al::R),
                ("sp/grind", Al::R),
            ],
        );
        for m in &free_m {
            t.row(vec![
                Cell::green(m.id.clone()),
                Cell::new(m.family.clone()),
                health_cell(&m.id),
                resp_cell(&m.id),
                elo(m.arena_overall_elo),
                elo(m.swe_elo()),
                code_cap(m),
                arena(m),
                agentic(m.agentic_score),
                wf_cell(m),
            ]);
        }
        t.print();
    }

    if show_paid {
        let mut paid_m: Vec<_> = eng.corpus.models.iter().filter(|m| m.paid).collect();
        paid_m.sort_by(|a, b| {
            by_agentic(
                &a.agentic_score.unwrap_or(0.0),
                &b.agentic_score.unwrap_or(0.0),
            )
        });
        println!(
            "\n{}  {}",
            p.amber("Paid models"),
            p.dim(&format!(
                "{} models — billed cloud usage ($/Mtok)",
                paid_m.len()
            ))
        );
        let mut t = Table::new(
            &p,
            vec![
                ("model", Al::L),
                ("family", Al::L),
                ("in $/Mtok", Al::R),
                ("out $/Mtok", Al::R),
                ("health", Al::L),
                ("resp", Al::R),
                ("ovr", Al::R),
                ("swe", Al::R),
                ("code", Al::R),
                ("arena", Al::R),
                ("agentic", Al::R),
                ("sp/grind", Al::R),
            ],
        );
        for m in &paid_m {
            let (in_c, out_c) = match pricing.lookup(&m.id) {
                Some(pr) => (
                    Cell::amber(format!("{:.2}", pr.input_usd_per_mtok)),
                    Cell::amber(format!("{:.2}", pr.output_usd_per_mtok)),
                ),
                None => (Cell::dim("—"), Cell::dim("—")),
            };
            t.row(vec![
                Cell::amber(m.id.clone()),
                Cell::new(m.family.clone()),
                in_c,
                out_c,
                health_cell(&m.id),
                resp_cell(&m.id),
                elo(m.arena_overall_elo),
                elo(m.swe_elo()),
                code_cap(m),
                arena(m),
                agentic(m.agentic_score),
                wf_cell(m),
            ]);
        }
        t.print();
    }

    Ok(())
}

/// Build the ordered model chain to attempt: an explicit model is used alone;
/// otherwise the routed primary followed by its fallback chain (unless disabled).
/// Model ids in the free pool that a REAL (non-placeholder) provider serves on
/// this host. The router uses this to avoid auto-picking a free-pool model that
/// would fall through to the `api.example.com` placeholder default and fail.
fn backed_free_model_ids(eng: &Engine) -> std::collections::HashSet<String> {
    eng.corpus
        .free_chat()
        .filter(|m| eng.cfg.model_has_real_provider(&m.id))
        .map(|m| m.id.clone())
        .collect()
}

fn resolve_chain(
    cli: &Cli,
    eng: &Engine,
    health: &HealthStore,
) -> anyhow::Result<(Vec<String>, String)> {
    if let Some(m) = &cli.model {
        return Ok((vec![m.clone()], format!("explicit model {m}")));
    }
    let router = Router::new(&eng.corpus, health)
        .with_primary(eng.cfg.primary_model.clone())
        .with_backed(Some(backed_free_model_ids(eng)));
    let route = router.select(Tier::parse(&cli.tier))?;
    let mut chain = vec![route.primary.clone()];
    if !cli.no_fallback {
        chain.extend(route.fallbacks.clone());
    }
    Ok((chain, route.reason))
}

fn cmd_route(cli: &Cli, prompt: Option<String>) -> anyhow::Result<()> {
    let eng = Engine::load()?;
    let health = HealthStore::load(&eng.cfg.health_path);
    let router = Router::new(&eng.corpus, &health)
        .with_primary(eng.cfg.primary_model.clone())
        .with_backed(Some(backed_free_model_ids(&eng)));
    let route = router.select(Tier::parse(&cli.tier))?;
    // Echo the task so the decision is traceable; routing is currently
    // capability/health based, not prompt-content based (see roadmap).
    let task = prompt.filter(|p| p != "-" && !p.trim().is_empty());
    if cli.json {
        println!(
            "{}",
            serde_json::json!({
                "task": task,
                "primary": route.primary,
                "fallbacks": route.fallbacks,
                "reason": route.reason,
            })
        );
    } else {
        if let Some(t) = &task {
            println!("task: {t}");
        }
        println!("{}", route.reason);
        println!("fallbacks: {}", route.fallbacks.join(", "));
    }
    Ok(())
}

/// Print the model-consultant advisory: models ranked by availability, then
/// coding capability, then SWE Elo (the router's own signals), so a human can
/// choose deliberately. Text table by default; `--json` emits the `Advisory`
/// list verbatim.
fn cmd_consult(cli: &Cli, free_only: bool, limit: Option<usize>) -> anyhow::Result<()> {
    let eng = Engine::load()?;
    let health = HealthStore::load(&eng.cfg.health_path);
    let rows = zoder_core::consultant::consult(
        &eng.corpus,
        &health,
        &zoder_core::consultant::ConsultOptions { free_only, limit },
    );
    if cli.json {
        println!("{}", serde_json::to_string(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no routable models in the corpus (run `zoder refresh`)");
        return Ok(());
    }
    println!(
        "{:>3}  {:<44} {:<5} {:>7} {:>7}  family",
        "#", "model", "state", "cap", "swe"
    );
    for a in &rows {
        let state = if a.available { "up" } else { "OPEN" };
        let cap = a
            .code_capability
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "-".to_string());
        let swe = a
            .swe_elo
            .map(|v| format!("{v:.0}"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:>3}  {:<44} {:<5} {:>7} {:>7}  {}",
            a.rank, a.model_id, state, cap, swe, a.family
        );
    }
    Ok(())
}

/// One model's worth of work: try it, retrying transient failures with backoff.
/// Returns `Ok(result)` on success, `Err((err, fatal))` otherwise. `fatal`
/// means the caller must NOT fall back (output was already partially emitted).
async fn try_model(
    provider: &OpenAiProvider,
    req: &ChatRequest,
    json: bool,
    retries: u32,
    quiet: bool,
) -> Result<ChatResult, (ProviderError, bool)> {
    let mut attempt = 0u32;
    loop {
        let mut stdout = std::io::stdout();
        let sink: Option<&mut dyn Write> = if json { None } else { Some(&mut stdout) };
        match provider.stream_chat(req, sink).await {
            Ok(res) => return Ok(res),
            Err(e) => {
                // Output already shown for this model: we cannot cleanly retry or
                // fall back without duplicating/garbling the stream.
                if e.emitted {
                    return Err((e, true));
                }
                if e.retryable() && attempt < retries {
                    let delay = backoff_delay(attempt, e.retry_after);
                    if !quiet {
                        eprintln!(
                            "[zoder] {} (retry {}/{} in {:.1}s)",
                            e.message,
                            attempt + 1,
                            retries,
                            delay.as_secs_f64()
                        );
                    }
                    tracing::debug!(model = %req.model, attempt, ?delay, "retrying transient failure");
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue;
                }
                return Err((e, false));
            }
        }
    }
}

/// Dispatch a prompt run: agentic loop (codex `exec` drop-in) by default, or a
/// single-shot completion with `--oneshot`. When the engine daemon can't be
/// reached and the user didn't force agentic, fall back to single-shot.
async fn cmd_exec(cli: &Cli, prompt: Option<String>) -> anyhow::Result<()> {
    if cli.oneshot {
        return cmd_exec_oneshot(cli, prompt).await;
    }
    match cmd_exec_agentic(cli, prompt.clone()).await {
        Ok(()) => Ok(()),
        Err(e) if is_engine_unavailable(&e) => {
            if !cli.quiet {
                eprintln!("[zoder] engine unavailable ({e}); falling back to single-shot. Use --oneshot to skip the engine.");
            }
            cmd_exec_oneshot(cli, prompt).await
        }
        Err(e) => Err(e),
    }
}

/// Heuristic: did the failure come from not being able to reach/start the
/// engine (vs. a real agent error we should surface)?
fn is_engine_unavailable(e: &anyhow::Error) -> bool {
    let s = e.to_string().to_ascii_lowercase();
    s.contains("connecting to engine")
        || s.contains("socket")
        || s.contains("daemon")
        || s.contains("zeroclaw binary not found")
        || s.contains("not ready within")
}

async fn cmd_exec_oneshot(cli: &Cli, prompt: Option<String>) -> anyhow::Result<()> {
    let eng = Engine::load()?;
    let mut health = HealthStore::load(&eng.cfg.health_path);
    let (chain, reason) = resolve_chain(cli, &eng, &health)?;
    let primary = chain
        .first()
        .ok_or_else(|| anyhow::anyhow!("no model resolved"))?
        .clone();

    if cli.explain || cli.dry_run {
        eprintln!("[route] {reason}");
        if chain.len() > 1 {
            eprintln!("[route] chain: {}", chain.join(" -> "));
        }
    }

    // Resolve the provider for the PRIMARY model (per-model routing): the
    // primary may belong to a different provider than `default_provider`
    // (e.g. a pinned `MiniMax-M3` -> the `minimax` provider). Each link in the
    // chain is resolved independently in the loop below, so one chain can span
    // providers (MiniMax -> EIH). `routing.real_provider_for_model` is the
    // quota-aware variant: when two providers (e.g. a subscription and its
    // metered sibling) claim the same prefix, it picks the cost-neutral one
    // while the subscription's rolling window has headroom and transparently
    // falls through to the metered path when the window is exhausted.
    let routing = RoutingContext::load(&eng.cfg);
    let provider_cfg = routing
        .real_provider_for_model(&eng.cfg, &primary)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no real provider is configured for model '{primary}' — routing would fall through to \
                 the {host} placeholder and fail. Configure a provider that serves it (e.g. in \
                 ~/.zoder/config.toml), pin a backed model via [profile].primary_model, or pass \
                 `-m <backed-model>`.",
                host = zoder_core::config::PLACEHOLDER_PROVIDER_HOST
            )
        })?;

    // L2: --dry-run short-circuits before reading stdin and any paid confirm.
    if cli.dry_run {
        let entry = eng.corpus.get(&primary);
        if cli.json {
            println!(
                "{}",
                serde_json::json!({
                    "dry_run": true,
                    "chain": chain,
                    "model": primary,
                    "provider": provider_cfg.id,
                    "free": entry.map(|e| e.free).unwrap_or(false),
                })
            );
        } else {
            println!(
                "[dry-run] would call {primary} via {} (chain: {})",
                provider_cfg.id,
                chain.join(" -> ")
            );
        }
        return Ok(());
    }

    // Strict free guard: config default, relaxable via --lenient-telemetry, but
    // always enforced when the user explicitly asked for --require-free.
    let strict_free = (eng.cfg.strict_free && !cli.lenient_telemetry) || cli.require_free;
    let gate = PolicyGate::new(&eng.cfg, cli.allow_paid, strict_free);

    // Paid/free pre-checks for the primary (the router-built chain is all free;
    // an explicit model may be paid and needs confirmation here). An explicit
    // model absent from the corpus cannot be proven free, so it is treated as
    // unverified (default-deny): refuse under --require-free, otherwise require
    // the paid confirmation. This preserves the fail-closed posture instead of
    // silently calling an unknown (possibly paid) model.
    let primary_entry = eng
        .corpus
        .get(&primary)
        .cloned()
        .unwrap_or_else(|| ModelEntry {
            id: primary.clone(),
            gated_reason: Some("unknown model: not in corpus, cannot verify free".into()),
            ..Default::default()
        });
    if cli.require_free && !primary_entry.free {
        anyhow::bail!("--require-free set but {primary} is not a known free model");
    }
    // A paid/metered serving provider (e.g. an org overlay's default route)
    // requires confirmation even when the model id is classified free. Checked
    // against the provider that actually serves the primary, not the default.
    // A Subscription-or-Free serving provider is $0-marginal — the call is
    // cost-neutral even if the corpus has the model non-free, so we let it
    // through (paid must still confirm).
    let provider_paid = routing
        .real_provider_for_model(&eng.cfg, &primary)
        .map(|p| p.paid || p.billing == BillingMode::Metered)
        .unwrap_or(false);
    let provider_cost_neutral = routing
        .real_provider_for_model(&eng.cfg, &primary)
        .map(|p| !p.paid && p.billing != BillingMode::Metered)
        .unwrap_or(false);
    if let Decision::NeedConfirm(msg) =
        gate.check(&primary_entry, provider_paid, provider_cost_neutral)
    {
        if !confirm_paid(&msg)? {
            anyhow::bail!("paid model use declined");
        }
    }

    let prompt = read_prompt(prompt)?;

    // Pre-call budget guard: project this call's cost from the prompt size and
    // the configured output estimate, then gate against the per-call and
    // month-to-date caps. A $0 (free-model) estimate is never gated;
    // `--allow-paid` bypasses, matching the paid-model confirmation above.
    if !cli.allow_paid {
        let pricing = PricingCatalog::load(&Config::home().join("pricing.json"));
        let est_usd = pricing.cost(
            &primary,
            estimate_tokens(&prompt),
            eng.cfg.budget.est_output_tokens,
        );
        let month_spent = Ledger::new(&eng.cfg.ledger_path).month_to_date_usd();
        if let BudgetVerdict::Confirm(msg) = eng.cfg.budget.evaluate(est_usd, month_spent) {
            if !confirm_paid(&msg)? {
                anyhow::bail!("call declined: over budget");
            }
        }
    }

    // Sessions: prepend prior transcript so follow-ups carry context.
    let sessions_dir = eng.cfg.sessions_dir();
    let mut session: Option<Session> = if let Some(id) = &cli.session {
        Some(Session::load_or_new(&sessions_dir, id)?)
    } else if cli.continue_ {
        Some(Session::latest(&sessions_dir)?.unwrap_or_else(|| Session::new("default")))
    } else {
        None
    };

    let mut messages: Vec<Message> = Vec::new();
    if let Some(s) = &session {
        messages.extend(s.messages.clone());
    }
    messages.push(Message::new("user", &prompt));

    // Per-model provider clients, built lazily and cached by provider id. A
    // single fallback chain can span providers (e.g. a pinned `MiniMax-M3` on
    // the `minimax` provider, then `nvidia/*` EIH NIMs on `nvidia-eih`), so the
    // serving provider is resolved per link via `provider_for_model` rather
    // than using one `default_provider` client for the whole chain.
    let mut provider_clients: std::collections::HashMap<String, OpenAiProvider> =
        std::collections::HashMap::new();

    // Walk the chain: each model gets `--retries` transient retries; on a clean
    // (no output emitted) failure we fall back to the next model.
    let started = std::time::Instant::now();
    let mut used_model = String::new();
    let mut used_provider_id = provider_cfg.id.clone();
    let mut used_latency_ms = 0.0f64;
    let mut outcome: Option<ChatResult> = None;
    let mut last_err: Option<ProviderError> = None;

    for (i, model_id) in chain.iter().enumerate() {
        // Respect the free guard for every link, not just the primary.
        if let Some(entry) = eng.corpus.get(model_id) {
            if cli.require_free && !entry.free {
                continue;
            }
        }
        // Resolve (and cache) the provider that serves THIS model. A model with
        // no provider claiming its prefix falls back to `default_provider`;
        // `None` only happens when no providers exist at all (validate() bars
        // that), so it is a hard configuration error. The quota-aware
        // router (`routing.real_provider_for_model`) transparently falls
        // through a subscription provider whose rolling window is at/over
        // cap to its metered sibling, so a single fallback chain can span
        // providers AND billing modes without per-link special-casing.
        let pid = routing
            .real_provider_for_model(&eng.cfg, model_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no real provider is configured for fallback model '{model_id}' (would hit the \
                     {host} placeholder)",
                    host = zoder_core::config::PLACEHOLDER_PROVIDER_HOST
                )
            })?
            .id
            .clone();
        // Per-link paid gate: with per-model routing a fallback can resolve to a
        // DIFFERENT provider than the (already-confirmed) primary, so re-run the
        // policy gate for every fallback link. The primary (i == 0) was gated +
        // confirmed before the loop. A fallback that would need paid
        // confirmation is skipped fail-closed (never silently spend mid-chain);
        // `--allow-paid` makes the gate return Allow, so it still runs then.
        // Cost-neutral fallback providers (Free or Subscription) are $0-marginal
        // and so do not require confirmation either.
        if i > 0 {
            let link_provider_paid = eng
                .cfg
                .provider(&pid)
                .map(|p| p.paid || p.billing == BillingMode::Metered)
                .unwrap_or(false);
            let link_provider_cost_neutral = eng
                .cfg
                .provider(&pid)
                .map(|p| !p.paid && p.billing != BillingMode::Metered)
                .unwrap_or(false);
            let link_entry = eng
                .corpus
                .get(model_id)
                .cloned()
                .unwrap_or_else(|| ModelEntry {
                    id: model_id.clone(),
                    gated_reason: Some("unknown model: not in corpus, cannot verify free".into()),
                    ..Default::default()
                });
            if let Decision::NeedConfirm(_) =
                gate.check(&link_entry, link_provider_paid, link_provider_cost_neutral)
            {
                if !cli.quiet {
                    eprintln!(
                        "[zoder] skipping paid fallback {model_id} via {pid} (pass --allow-paid to use it)"
                    );
                }
                continue;
            }
        }
        if !provider_clients.contains_key(&pid) {
            let pcfg = eng
                .cfg
                .provider(&pid)
                .ok_or_else(|| anyhow::anyhow!("provider {pid} not configured"))?;
            provider_clients.insert(pid.clone(), OpenAiProvider::new(pcfg)?);
        }
        let provider = &provider_clients[&pid];
        let req = ChatRequest {
            model: model_id.clone(),
            messages: messages.clone(),
            max_tokens: cli.max_tokens,
            temperature: 0.2,
            stream: !cli.no_stream,
            show_reasoning: cli.show_reasoning,
            reasoning_effort: cli.reasoning.clone(),
        };
        // Per-model timer: health latency must reflect THIS model's call, not
        // the chain-wide elapsed time (which would fold in prior models' time
        // plus retry backoff and skew the router's latency EWMA).
        let model_started = std::time::Instant::now();
        match try_model(provider, &req, cli.json, cli.retries, cli.quiet).await {
            Ok(res) => {
                // Defer the winning model's health recording until after the
                // policy verify below, so a policy-violating "success" is
                // recorded as a single failure (not success + failure).
                used_model = model_id.clone();
                used_provider_id = pid.clone();
                used_latency_ms = model_started.elapsed().as_millis() as f64;
                outcome = Some(res);
                break;
            }
            Err((e, fatal)) => {
                health.record_failure(model_id, &e.message);
                last_err = Some(e);
                if fatal {
                    // Output already partially shown; do not fall back.
                    break;
                }
                if i + 1 < chain.len() && !cli.quiet {
                    let next = &chain[i + 1];
                    eprintln!("[zoder] {model_id} failed; falling back to {next}");
                }
            }
        }
    }

    let elapsed_ms = started.elapsed().as_millis() as f64;

    let Some(res) = outcome else {
        save_health(&health);
        let msg = last_err
            .map(|e| e.message)
            .unwrap_or_else(|| "all models in the chain failed".into());
        anyhow::bail!("{msg}");
    };

    let entry = eng.corpus.get(&used_model).cloned().unwrap_or_default();

    // C1: the anti-paid-fallback guard governs accounting + exit. Record the
    // winning model's health exactly once here: a verified call is a success,
    // a policy violation is a failure.
    let verify = gate.verify_free(&entry, &res.telemetry);
    match verify.as_ref() {
        Ok(()) => health.record_success(&used_model, used_latency_ms),
        Err(v) => health.record_failure(&used_model, &format!("policy: {v}")),
    }

    // M4: honest token accounting from real usage when available.
    let tokens_in = res.prompt_tokens.unwrap_or(0);
    let tokens_out = res.completion_tokens.unwrap_or(res.tokens_out);
    // Cost: trust live telemetry first; otherwise price from the provider-derived
    // catalog (free-tier models resolve to $0 chargeback, paid models to their rate).
    let pricing = PricingCatalog::load(&Config::home().join("pricing.json"));
    let cost = res
        .telemetry
        .cost_usd
        .unwrap_or_else(|| pricing.cost(&used_model, tokens_in, tokens_out));
    let ledger = Ledger::new(&eng.cfg.ledger_path);
    ledger.record(&Entry {
        ts_utc: chrono::Utc::now(),
        provider: used_provider_id.clone(),
        model: used_model.clone(),
        host: zoder_core::ledger::host_of_model(&used_model),
        tokens_in,
        tokens_out,
        cost_usd: cost,
        calls: 1,
        violation: verify.as_ref().err().cloned(),
    })?;
    save_health(&health);

    // C1: a violation is fatal -> stderr message + non-zero exit.
    if let Err(v) = verify {
        eprintln!("zoder: POLICY VIOLATION: {v}");
        anyhow::bail!(
            "policy violation: free model {used_model} was not verified free (exiting non-zero)"
        );
    }

    // Persist the turn to the session transcript.
    if let Some(s) = session.as_mut() {
        s.push("user", &prompt);
        s.push("assistant", &res.content);
        if let Err(e) = s.save(&sessions_dir) {
            eprintln!("zoder: warning: failed to save session: {e}");
        }
    }

    // Reasoning-empty hint: some models spend the whole budget on hidden
    // reasoning, leaving content empty. Nudge the user toward --show-reasoning.
    if res.content.is_empty() && !cli.show_reasoning && !cli.quiet {
        eprintln!(
            "[zoder] note: empty content (model may be reasoning-only at this max-tokens; try --show-reasoning or a higher --max-tokens)"
        );
    }

    if cli.json {
        println!(
            "{}",
            serde_json::json!({
                "model": used_model,
                "content": res.content,
                "tokens_in": tokens_in,
                "tokens_out": tokens_out,
                "cost_usd": cost,
                "served_by": res.telemetry.api_base,
                "key_spend": res.telemetry.key_spend,
                "duration_ms": res.telemetry.duration_ms,
                "latency_ms": elapsed_ms,
            })
        );
    } else {
        println!();
        if !cli.quiet {
            eprintln!("[zoder] {used_model}  {tokens_out} tok  ${cost:.4}  {elapsed_ms:.0}ms");
        }
    }
    Ok(())
}

/// Working directory for an agentic run: `-C/--cd` or the current directory.
fn agentic_cwd(cli: &Cli) -> anyhow::Result<std::path::PathBuf> {
    match &cli.cd {
        Some(d) => {
            let p = std::path::PathBuf::from(d);
            let p = p.canonicalize().unwrap_or(p);
            if !p.is_dir() {
                anyhow::bail!("-C/--cd {d:?} is not a directory");
            }
            Ok(p)
        }
        None => Ok(std::env::current_dir()?),
    }
}

fn parse_approval(cli: &Cli) -> ApprovalPolicy {
    match cli.approve.as_deref() {
        Some("all") => ApprovalPolicy::All,
        Some("none") => ApprovalPolicy::None,
        _ => ApprovalPolicy::Allowlist,
    }
}

/// Map a model id to a renamed zeroclaw agent alias (the model-named aliases the
/// TUI picker shows). Falls back to the strongest coding alias. `--agent` wins.
fn resolve_agent_alias(cli: &Cli, model: &str) -> String {
    if let Some(a) = &cli.agent {
        return a.clone();
    }
    let m = model.to_ascii_lowercase();
    // (substring in model id) -> alias
    const MAP: &[(&str, &str)] = &[
        // MiniMax subscription (the pinned primary). Without this the id falls
        // through to the deepseek default alias, and a forced model_override
        // then hands the engine a model the deepseek agent can't serve -> hang.
        ("minimax", "minimax"),
        ("deepseek-v4-pro", "deepseek-v4-pro"),
        ("qwen3-coder", "qwen3-coder-480b"),
        ("qwen-235b", "qwen-235b"),
        ("397", "qwen3-397b"),
        ("kimi", "kimi-k2.6"),
        ("nemotron-3-super", "nemotron-super"),
        ("nemotron-super", "nemotron-super"),
        ("nemotron-ultra-253", "nemotron-ultra-253b"),
        ("llama-3.1-nemotron-ultra", "nemotron-ultra-253b"),
        ("nemotron-3-ultra", "nemotron-ultra"),
        ("nemotron-ultra", "nemotron-ultra"),
        ("gpt-oss", "gpt-oss-120b"),
        ("llama-3.3-70b", "llama-3.3-70b"),
    ];
    for (needle, alias) in MAP {
        if m.contains(needle) {
            return (*alias).to_string();
        }
    }
    // Default coding agent. Was `deepseek-v4-pro`, which dangled / flapped and
    // made zoder non-invokable mid-loop (field reports 2026-06-30). `minimax`
    // is the configured default author on the cutover hosts and is stable.
    "minimax".to_string()
}

/// Default ADVERSARIAL REVIEWER when none is given: a strong, empirically-validated
/// (2026-06-30 bake-off) CROSS-FAMILY free model — never the author's own model.
/// Self-review is weak, and a flat-subscription author (e.g. minimax) uses env-auth
/// on the review path and 401s while the agentic engine authed fine; a cross-family
/// EIH reviewer routes to the working-auth provider.
pub(crate) fn default_cross_family_reviewer(author_model: &str) -> &'static str {
    let a = author_model.to_ascii_lowercase();
    if a.contains("glm") || a.contains("z-ai") {
        "moonshotai/kimi-k2.6"
    } else if a.contains("kimi") || a.contains("moonshot") {
        "z-ai/glm-5.1"
    } else {
        // minimax / deepseek / qwen / nemotron / etc. authors -> glm-5.1 (top free reviewer)
        "z-ai/glm-5.1"
    }
}

/// Ensure a zeroclaw agent daemon is reachable; spawn an ephemeral one (using
/// the co-shipped `zeroclaw` binary) if the socket is absent. Returns the socket.
async fn ensure_engine_daemon() -> anyhow::Result<std::path::PathBuf> {
    let socket = engine_socket_path();
    if tokio::net::UnixStream::connect(&socket).await.is_ok() {
        return Ok(socket);
    }
    let bin = locate_sibling("zeroclaw").ok_or_else(|| {
        anyhow::anyhow!(
            "zeroclaw binary not found (looked next to zoder, on PATH, and in ~/.local/bin); \
             cannot start the agentic engine"
        )
    })?;
    let config_dir = zeroclaw_data_dir()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(zeroclaw_data_dir);
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("daemon")
        .arg("--ephemeral")
        .arg("--config-dir")
        .arg(&config_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn zeroclaw daemon ({}): {e}", bin.display()))?;
    zoder_core::wait_for_socket(&socket, std::time::Duration::from_secs(20)).await?;
    Ok(socket)
}

/// Authoritative per-run cost/tokens from the engine's cost tracker, scoped to
/// `[from, now)` and the agent alias. Falls back to `(0, 0, 0, fallback_model)`
/// when the engine has no record yet.
async fn agentic_cost(
    socket: &std::path::Path,
    from: chrono::DateTime<chrono::Utc>,
    alias: &str,
    fallback_model: &str,
) -> (f64, u64, u64, String) {
    match fetch_engine_cost(socket, Some(from), Some(chrono::Utc::now()), Some(alias)).await {
        Ok(sum) => {
            let cost = sum.window_cost_usd();
            // Pick the dominant model in the window for attribution.
            let model = sum
                .by_model
                .values()
                .max_by(|a, b| a.total_tokens.cmp(&b.total_tokens))
                .map(|m| m.model.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| fallback_model.to_string());
            let tin: u64 = sum.by_model.values().map(|m| m.input_tokens).sum();
            let tout: u64 = sum.by_model.values().map(|m| m.output_tokens).sum();
            (cost, tin, tout, model)
        }
        Err(_) => (0.0, 0, 0, fallback_model.to_string()),
    }
}

/// Outcome of one agentic turn, with the cost/token figures harvested from the
/// engine cost tracker. Returned by [`agentic_turn`] so callers (single `exec`
/// and the multi-turn `loop`) can decide what to print and whether to continue.
pub(crate) struct TurnResult {
    pub run: zoder_core::AgentRun,
    pub model: String,
    pub alias: String,
    pub cost_usd: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub elapsed_ms: f64,
}

/// Drive a single agentic turn against the engine: resolve the model (routing or
/// `-m`), enforce the free/paid gate, run the loop in `cwd`, harvest cost/tokens,
/// and write one ledger record. `session_override` (when `Some`) continues an
/// existing engine session for conversational continuity across turns; otherwise
/// `cli.session` is used. Set `stream_output` to mirror text/tool events to the
/// terminal (off for `--json` and for the inner turns of `loop`). This function
/// does NOT print the final summary — the caller owns presentation.
pub(crate) async fn agentic_turn(
    cli: &Cli,
    engine_kind: EngineKind,
    prompt: String,
    session_override: Option<String>,
    stream_output: bool,
) -> anyhow::Result<TurnResult> {
    let eng = Engine::load()?;
    let mut health = HealthStore::load(&eng.cfg.health_path);

    // Resolve the model (routing or -m) for alias selection + paid gate.
    let (chain, reason) = resolve_chain(cli, &eng, &health)?;
    let primary = chain
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no model resolved"))?;
    if cli.explain {
        eprintln!("[route] {reason}");
    }

    // Backed-provider guard (agentic path): a routed/forced/pinned model with no
    // REAL provider would be handed to the engine and dial the api.example.com
    // placeholder default, failing cryptically. Fail legibly here — BEFORE the
    // paid gate and the engine spawn — so `-m <unbacked>` reports the real cause
    // ("no provider") instead of a misleading paid-confirm, mirroring oneshot.
    // Quota-aware variant: a subscription provider with a saturated window is
    // transparently demoted to its metered sibling, so this guard only fires
    // when NEITHER path has a real backing provider.
    let routing = RoutingContext::load(&eng.cfg);
    routing
        .real_provider_for_model(&eng.cfg, &primary)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no real provider is configured for model '{primary}' — it would fall through to the \
                 {host} placeholder and fail. Configure a provider that serves it, pin a backed model \
                 via [profile].primary_model, or pass `-m <backed-model>`.",
                host = zoder_core::config::PLACEHOLDER_PROVIDER_HOST
            )
        })?;

    let strict_free = (eng.cfg.strict_free && !cli.lenient_telemetry) || cli.require_free;
    let gate = PolicyGate::new(&eng.cfg, cli.allow_paid, strict_free);
    let primary_entry = eng
        .corpus
        .get(&primary)
        .cloned()
        .unwrap_or_else(|| ModelEntry {
            id: primary.clone(),
            gated_reason: Some("unknown model: not in corpus, cannot verify free".into()),
            ..Default::default()
        });
    if cli.require_free && !primary_entry.free {
        anyhow::bail!("--require-free set but {primary} is not a known free model");
    }
    // A paid/metered serving provider (e.g. an org overlay's default route)
    // requires confirmation even when the model id is classified free. Checked
    // against the provider that actually serves the primary, not the default.
    // A Subscription-or-Free serving provider is $0-marginal — the call is
    // cost-neutral even if the corpus has the model non-free, so we let it
    // through (paid must still confirm).
    let provider_paid = routing
        .real_provider_for_model(&eng.cfg, &primary)
        .map(|p| p.paid || p.billing == BillingMode::Metered)
        .unwrap_or(false);
    let provider_cost_neutral = routing
        .real_provider_for_model(&eng.cfg, &primary)
        .map(|p| !p.paid && p.billing != BillingMode::Metered)
        .unwrap_or(false);
    if let Decision::NeedConfirm(msg) =
        gate.check(&primary_entry, provider_paid, provider_cost_neutral)
    {
        if !confirm_paid(&msg)? {
            anyhow::bail!("paid model use declined");
        }
    }

    let cwd = agentic_cwd(cli)?;
    let alias = resolve_agent_alias(cli, &primary);

    // The Goose path SPAWNS its own engine process (`goose acp` over stdio)
    // and does not use `opts.socket`. We must NOT touch the zeroclaw daemon
    // in that case — starting it would waste a process, fight for the same
    // port/socket, and pull the runtime into the "engine unavailable" ->
    // oneshot fallback path that would mask the goose driver with a
    // confusing socket/transport error. Zeroclaw keeps the existing flow.
    let socket = match engine_kind {
        EngineKind::Zeroclaw => Some(ensure_engine_daemon().await?),
        EngineKind::Goose => None,
    };

    // Force the daemon to use the operator's explicit choice — an `-m` model or
    // a configured `primary_model` pin — rather than letting the agent alias
    // fall through to its own default model. Pure-auto routing (no `-m`, no pin)
    // keeps `None` so the alias picks its default as before.
    let model_override = if cli.model.is_some() || eng.cfg.primary_model.is_some() {
        Some(primary.clone())
    } else {
        None
    };

    // `socket` is a dummy path for Goose: the goose driver ignores it and
    // builds its own Stdio transport, but `AgentOptions` still requires the
    // field. Use the engine's configured socket path so any logging that
    // happens to reference it (none on the goose path) is still meaningful.
    let socket_path = socket.unwrap_or_else(engine_socket_path);

    // CREDENTIAL/ENDPOINT bridge (task #19, seam 1): when the operator
    // selected the goose engine, resolve the same provider the oneshot
    // + agentic gates already validated (`real_best_provider_for_model`)
    // and forward its key + base_url to the spawned `goose acp` child.
    // Without this, goose can't authenticate against a free/subscription
    // provider and the loop silently fails or dials the wrong endpoint.
    // We only build this on the goose path — zeroclaw handles its own
    // auth and never sees these vars.
    let goose_provider = if matches!(engine_kind, EngineKind::Goose) {
        routing
            .real_provider_for_model(&eng.cfg, &primary)
            .map(|p| GooseProviderEnv {
                provider_id: p.id.clone(),
                kind: p.kind.clone(),
                base_url: p.base_url.clone(),
                // Resolve the credential the SAME way the engine bridge
                // does (auth.resolve() reads env vars or returns the
                // inline bearer — never log this value; it is redacted
                // by `GooseProviderEnv`'s Debug impl above).
                api_key: p.auth.resolve(),
            })
    } else {
        None
    };

    let opts = AgentOptions {
        socket: socket_path,
        agent_alias: alias.clone(),
        cwd: cwd.clone(),
        prompt,
        model_override,
        // The routed model id — the zeroclaw-free model name goose needs in
        // `GOOSE_MODEL`. ALWAYS pass it when known (we always know: it's
        // `chain[0]`); the goose driver prefers this over the zeroclaw
        // agent alias (which goose doesn't understand).
        model_id: Some(primary.clone()),
        session_id: session_override.or_else(|| cli.session.clone()),
        show_reasoning: cli.show_reasoning,
        approval: parse_approval(cli),
        timeout: std::time::Duration::from_secs(cli.agent_timeout.unwrap_or(900)),
        goose_provider,
    };

    let started = std::time::Instant::now();
    let start_ts = chrono::Utc::now();
    // `engine_kind` is parsed and validated by the caller (cmd_exec_agentic),
    // before any daemon setup, so a Goose request never starts zeroclaw and an
    // unknown value surfaces as a parse error up front.
    let run = run_agent_dispatch(engine_kind, &opts, |ev| {
        if !stream_output {
            return;
        }
        match ev {
            AgentEvent::Text(t) => {
                print!("{t}");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::Thought(t) => {
                eprint!("{t}");
            }
            AgentEvent::ToolCall { name } => {
                eprintln!("\n[tool] {name}");
            }
            AgentEvent::ToolResult { .. } => {}
            AgentEvent::Approval { tool, approved } => {
                eprintln!("[approve:{}] {tool}", if approved { "ok" } else { "deny" });
            }
            AgentEvent::Usage { .. } => {}
        }
    })
    .await?;

    let elapsed_ms = started.elapsed().as_millis() as f64;

    // Cost reconciliation is zeroclaw-specific (it talks to the daemon's
    // `cost/query` endpoint over the Unix socket). Goose doesn't expose that,
    // and we deliberately never started the zeroclaw daemon for the goose
    // path, so there is no authoritative cost to harvest. Record zeros +
    // attribute to the routed model so the ledger still gets a row for the
    // run, but skip the post-verify paid-gate check (the corpus_paid test
    // also implicitly assumes the daemon reported the actual billed model,
    // which we don't have here).
    let (cost, tokens_in, tokens_out, model_used) = match engine_kind {
        EngineKind::Zeroclaw => {
            let socket2 = engine_socket_path();
            agentic_cost(&socket2, start_ts, &alias, &primary).await
        }
        EngineKind::Goose => (0.0, run.input_tokens, 0, primary.clone()),
    };

    // Post-verify: the engine (via the agent alias) may have run a different —
    // possibly paid — model than the one pre-gated above (the daemon resolves
    // the alias). If it billed real money, or the engine-reported model is a
    // known paid model, without --allow-paid, record a policy violation rather
    // than marking the ledger clean. Skipped for goose: we have no engine-
    // reported cost/model, so the gate above (`gate.check` on the routed
    // `primary`) is the only signal we can apply.
    let model_used_paid = eng
        .corpus
        .get(&model_used)
        .map(|m| !m.free)
        .unwrap_or(false);
    let violation = if !cli.allow_paid
        && engine_kind == EngineKind::Zeroclaw
        && (cost > 0.0 || model_used_paid)
    {
        let v = format!(
            "agentic run (alias {alias}) billed ${cost:.4} on engine model '{model_used}' \
             (corpus_paid={model_used_paid}) without --allow-paid"
        );
        eprintln!("zoder: POLICY VIOLATION: {v}");
        Some(v)
    } else {
        None
    };

    let ledger = Ledger::new(&eng.cfg.ledger_path);
    if let Err(e) = ledger.record(&Entry {
        ts_utc: chrono::Utc::now(),
        // Attribute to the provider that serves the model the engine actually
        // ran (per-model routing), not the default provider.
        provider: eng
            .cfg
            .provider_for_model(&model_used)
            .map(|p| p.id.clone())
            .unwrap_or_else(|| eng.cfg.default_provider.clone()),
        model: model_used.clone(),
        host: zoder_core::ledger::host_of_model(&model_used),
        tokens_in,
        tokens_out,
        cost_usd: cost,
        calls: 1,
        violation,
    }) {
        eprintln!("zoder: warning: failed to record ledger entry: {e}");
    }
    // A timed-out (or otherwise non-completed) turn still returns a TurnResult so
    // the caller can preserve partial output — but it is NOT a success: record it
    // as a failure so latency/health-aware routing learns this model couldn't
    // finish in budget (the BUG2 routing follow-up consumes this signal).
    if run.succeeded() {
        health.record_success(&primary, elapsed_ms);
    } else {
        health.record_failure(&primary, &format!("turn did not complete: {}", run.outcome));
    }
    save_health(&health);

    Ok(TurnResult {
        run,
        model: model_used,
        alias,
        cost_usd: cost,
        tokens_in,
        tokens_out,
        elapsed_ms,
    })
}

async fn cmd_exec_agentic(cli: &Cli, prompt: Option<String>) -> anyhow::Result<()> {
    if cli.dry_run {
        let eng = Engine::load()?;
        let health = HealthStore::load(&eng.cfg.health_path);
        let (chain, _) = resolve_chain(cli, &eng, &health)?;
        let primary = chain.first().cloned().unwrap_or_default();
        let alias = resolve_agent_alias(cli, &primary);
        let cwd = agentic_cwd(cli)?;
        println!(
            "[dry-run] agentic: alias={alias} model={primary} cwd={}",
            cwd.display()
        );
        return Ok(());
    }

    // Parse --engine EARLY, before any zeroclaw socket/daemon setup. An unknown
    // value must surface as a parse error here (not get swallowed), and a Goose
    // request must NOT spawn the zeroclaw daemon — the daemon-unavailable ->
    // oneshot fallback would otherwise mask the goose path and produce a
    // confusing socket/transport failure instead of the real diagnostic.
    let engine_kind = resolve_engine_kind(cli)?;

    let prompt = read_prompt(prompt)?;
    let t = agentic_turn(cli, engine_kind, prompt, None, !cli.json).await?;

    if cli.json {
        println!(
            "{}",
            serde_json::json!({
                "model": t.model,
                "agent": t.alias,
                "session_id": t.run.session_id,
                "outcome": t.run.outcome,
                "content": t.run.content,
                "tokens_in": t.tokens_in,
                "tokens_out": t.tokens_out,
                "cost_usd": t.cost_usd,
                "tool_calls": t.run.tool_calls,
                "cwd": agentic_cwd(cli)?.to_string_lossy(),
                "duration_ms": t.elapsed_ms,
            })
        );
    } else {
        println!();
        if !cli.quiet {
            eprintln!(
                "[zoder] {} via {}  {} tools  ${:.4}  {:.0}ms  [{}]",
                t.model, t.alias, t.run.tool_calls, t.cost_usd, t.elapsed_ms, t.run.outcome
            );
        }
    }

    if !t.run.succeeded() {
        // Partial work is preserved: on-disk edits stay, and the streamed text
        // above is whatever the turn produced. Point the user at a clean resume.
        if !cli.quiet && !cli.json {
            let timed_out = t.run.outcome == "timeout";
            eprintln!(
                "[zoder] turn {} ({} chars captured). Resume with: zoder exec --session {} \"<continue>\"{}",
                if timed_out { "timed out — partial work kept" } else { "did not complete" },
                t.run.content.len(),
                t.run.session_id,
                if timed_out { "  (or raise --agent-timeout <secs>, default 900)" } else { "" },
            );
        }
        anyhow::bail!("agentic turn ended: {}", t.run.outcome);
    }
    Ok(())
}

/// Parse `--engine` and bail early on Goose (which would otherwise start the
/// zeroclaw daemon and have the daemon-unavailable -> oneshot fallback mask
/// the goose path). Must be called BEFORE any zeroclaw socket/daemon setup.
/// Unknown values surface as a parse error here so they aren't swallowed.
fn resolve_engine_kind(cli: &Cli) -> anyhow::Result<EngineKind> {
    // Parse-only: no engine-specific bail. The Goose driver lives in
    // `run_goose_agent` (standard ACP, stdio) and the dispatcher routes to
    // it from `agentic_turn`, which also skips the zeroclaw daemon setup
    // for Goose so we never end up touching the wrong engine.
    cli.engine
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid --engine: {e}"))
}

/// L5: surface a warning instead of silently dropping a persistence failure.
fn save_health(health: &HealthStore) {
    if let Err(e) = health.save() {
        eprintln!("zoder: warning: failed to persist health store: {e}");
    }
}

fn confirm_paid(msg: &str) -> anyhow::Result<bool> {
    eprintln!("\n{msg}\n");
    eprint!("Type 'yes' to proceed with a PAID model: ");
    std::io::stderr().flush().ok();
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().eq_ignore_ascii_case("yes"))
}

/// Parse a YYYY-MM-DD date into a UTC instant. `end_of_day` pushes to 23:59:59.
fn parse_date(s: &str, end_of_day: bool) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
    use chrono::{NaiveTime, TimeZone, Utc};
    let d = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|e| anyhow::anyhow!("invalid date {s:?} (expected YYYY-MM-DD): {e}"))?;
    let t = if end_of_day {
        NaiveTime::from_hms_opt(23, 59, 59).unwrap()
    } else {
        NaiveTime::from_hms_opt(0, 0, 0).unwrap()
    };
    Ok(Utc.from_utc_datetime(&d.and_time(t)))
}

fn cmd_spend(
    period: &str,
    since: Option<&str>,
    until: Option<&str>,
    by_model: bool,
    host: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let eng = Engine::load()?;
    let ledger = Ledger::new(&eng.cfg.ledger_path);
    let since = since.map(|s| parse_date(s, false)).transpose()?;
    let until = until.map(|s| parse_date(s, true)).transpose()?;

    if by_model {
        let roll = match host {
            Some(h) => ledger.by_model_filtered(since, until, |e| e.effective_host() == h)?,
            None => ledger.by_model(since, until)?,
        };
        if json {
            println!("{}", serde_json::to_string_pretty(&roll)?);
            return Ok(());
        }
        let mut total = 0.0;
        println!(
            "{:46} {:>12} {:>14} {:>8}",
            "model", "cost_usd", "tokens_out", "calls"
        );
        for (k, r) in &roll {
            total += r.cost_usd;
            println!(
                "{:46.46} {:>12.4} {:>14} {:>8}",
                k, r.cost_usd, r.tokens_out, r.calls
            );
        }
        println!("{:46} {:>12.4}", "TOTAL", total);
        return Ok(());
    }

    let p = Period::parse(period)
        .ok_or_else(|| anyhow::anyhow!("period must be day|week|month|year"))?;
    let roll = match host {
        Some(h) => ledger.rollup_in_filtered(p, since, until, |e| e.effective_host() == h)?,
        None => ledger.rollup_in(p, since, until)?,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&roll)?);
        return Ok(());
    }
    let mut total = 0.0;
    println!(
        "{:14} {:>12} {:>14} {:>8}",
        "period", "cost_usd", "tokens_out", "calls"
    );
    for (k, r) in &roll {
        total += r.cost_usd;
        println!(
            "{:14} {:>12.4} {:>14} {:>8}",
            k, r.cost_usd, r.tokens_out, r.calls
        );
    }
    println!("{:14} {:>12.4}", "TOTAL", total);
    Ok(())
}

/// Unicode share bar (filled/empty) for a 0..=1 fraction.
fn share_bar(frac: f64, width: usize) -> String {
    let f = frac.clamp(0.0, 1.0);
    let filled = if f > 0.0 {
        ((f * width as f64).round() as usize).clamp(1, width)
    } else {
        0
    };
    let mut s = String::with_capacity(width * 3);
    for i in 0..width {
        s.push(if i < filled { '█' } else { '░' });
    }
    s
}

/// Themed terminal palette. ANSI codes are applied AFTER width padding so
/// column alignment is preserved. Disabled when not a TTY or NO_COLOR is set.
/// The active colours come from the org-config [`Theme`]; `Pal::new()` uses the
/// built-in default palette, `Pal::themed()` honours the resolved org theme.
struct Pal {
    on: bool,
    theme: Theme,
}
impl Pal {
    /// Built-in default palette (used by commands that don't load a config).
    fn new() -> Self {
        Self::themed(&Theme::default())
    }
    /// Palette driven by the resolved org theme.
    fn themed(theme: &Theme) -> Self {
        let forced = std::env::var_os("CLICOLOR_FORCE").is_some();
        let no_color = std::env::var_os("NO_COLOR").is_some();
        Self {
            on: !no_color && (forced || std::io::stdout().is_terminal()),
            theme: theme.clone(),
        }
    }
    fn wrap(&self, s: &str, code: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    /// "Good"/free/$0 accent (theme `ok`).
    fn green(&self, s: &str) -> String {
        self.wrap(s, &self.theme.ok)
    }
    /// Bold header accent (theme `header`).
    fn green_b(&self, s: &str) -> String {
        self.wrap(s, &self.theme.header)
    }
    /// Billed/paid — real external cash (theme `warn`).
    fn amber(&self, s: &str) -> String {
        self.wrap(s, &self.theme.warn)
    }
    fn dim(&self, s: &str) -> String {
        self.wrap(s, &self.theme.dim)
    }
    /// ANSI code for a table-cell role, resolved against the active theme.
    fn role_code(&self, role: CellRole) -> &str {
        match role {
            CellRole::Ok => &self.theme.ok,
            CellRole::Warn => &self.theme.warn,
            CellRole::Dim => &self.theme.dim,
        }
    }
}

/// Column alignment for [`Table`].
#[derive(Clone, Copy)]
enum Al {
    L,
    R,
}

/// Semantic colour role for a table cell, resolved to an ANSI code against the
/// active [`Theme`] at print time (so org themes recolour every table).
#[derive(Clone, Copy)]
enum CellRole {
    Ok,
    Warn,
    Dim,
}

/// A single table cell: raw (uncolored) text plus an optional colour role.
/// Width is always measured on `raw`, and colour is applied *after* padding, so
/// ANSI escapes can never corrupt column alignment (the old bug where a colored
/// string was dropped into a `{:<N}` slot and padded by its escape-byte length).
struct Cell {
    raw: String,
    role: Option<CellRole>,
}
impl Cell {
    fn new(s: impl Into<String>) -> Self {
        Self {
            raw: s.into(),
            role: None,
        }
    }
    fn green(s: impl Into<String>) -> Self {
        Self {
            raw: s.into(),
            role: Some(CellRole::Ok),
        }
    }
    fn amber(s: impl Into<String>) -> Self {
        Self {
            raw: s.into(),
            role: Some(CellRole::Warn),
        }
    }
    fn dim(s: impl Into<String>) -> Self {
        Self {
            raw: s.into(),
            role: Some(CellRole::Dim),
        }
    }
}

fn pad_to(s: &str, w: usize, a: Al) -> String {
    let len = s.chars().count();
    let n = w.saturating_sub(len);
    match a {
        Al::L => format!("{s}{}", " ".repeat(n)),
        Al::R => format!("{}{s}", " ".repeat(n)),
    }
}

/// Aligned text table: a dim header row, a rule, then rows. This is the single
/// rendering style for every zoder report/pricing table so columns always
/// line up whether or not color is on. Column widths size to the widest cell.
struct Table<'a> {
    pal: &'a Pal,
    cols: Vec<(String, Al)>,
    rows: Vec<Vec<Cell>>,
    indent: usize,
}
impl<'a> Table<'a> {
    fn new(pal: &'a Pal, cols: Vec<(&str, Al)>) -> Self {
        Self {
            pal,
            cols: cols.into_iter().map(|(t, a)| (t.to_string(), a)).collect(),
            rows: Vec::new(),
            indent: 2,
        }
    }
    fn row(&mut self, cells: Vec<Cell>) {
        self.rows.push(cells);
    }
    fn print(&self) {
        let n = self.cols.len();
        let mut w = vec![0usize; n];
        for (i, (t, _)) in self.cols.iter().enumerate() {
            w[i] = t.chars().count();
        }
        for r in &self.rows {
            for (i, c) in r.iter().enumerate().take(n) {
                w[i] = w[i].max(c.raw.chars().count());
            }
        }
        let pad = " ".repeat(self.indent);
        // Header.
        let mut hline = String::new();
        for (i, (t, a)) in self.cols.iter().enumerate() {
            if i > 0 {
                hline.push_str("  ");
            }
            hline.push_str(&pad_to(t, w[i], *a));
        }
        println!("{pad}{}", self.pal.dim(&hline));
        // Rule (two-space gutter between columns).
        let total: usize = w.iter().sum::<usize>() + 2 * n.saturating_sub(1);
        println!("{pad}{}", self.pal.dim(&"\u{2500}".repeat(total)));
        // Rows.
        for r in &self.rows {
            let mut line = String::new();
            for (i, (_, a)) in self.cols.iter().enumerate() {
                if i > 0 {
                    line.push_str("  ");
                }
                let cell = r.get(i);
                let raw = cell.map(|c| c.raw.as_str()).unwrap_or("");
                let padded = pad_to(raw, w[i], *a);
                match cell.and_then(|c| c.role) {
                    Some(role) => line.push_str(&self.pal.wrap(&padded, self.pal.role_code(role))),
                    None => line.push_str(&padded),
                }
            }
            println!("{pad}{line}");
        }
    }
}

/// Resolve a [`ReportPeriod`] (or a `--days N` override) into an explicit
/// window and bucket granularity. All named periods are period-to-date.
fn report_window(
    period: ReportPeriod,
    days: Option<i64>,
) -> (DateTime<Utc>, DateTime<Utc>, Gran, String) {
    let now = Utc::now();
    if let Some(d) = days {
        let d = d.max(1);
        return (
            now - Duration::days(d),
            now,
            Gran::Day,
            format!("last {d} days"),
        );
    }
    let midnight = |dt: NaiveDate| dt.and_hms_opt(0, 0, 0).unwrap().and_utc();
    let today = midnight(now.date_naive());
    match period {
        ReportPeriod::Day => (today, now, Gran::Hour, "today".to_string()),
        ReportPeriod::Week => {
            let back = now.weekday().num_days_from_monday() as i64;
            (
                today - Duration::days(back),
                now,
                Gran::Day,
                "this week".to_string(),
            )
        }
        ReportPeriod::Month => {
            let first = midnight(now.date_naive().with_day(1).unwrap());
            (first, now, Gran::Day, "this month".to_string())
        }
        ReportPeriod::Quarter => {
            let q = (now.month() - 1) / 3;
            let first = midnight(NaiveDate::from_ymd_opt(now.year(), q * 3 + 1, 1).unwrap());
            (first, now, Gran::Week, format!("Q{} {}", q + 1, now.year()))
        }
        ReportPeriod::Ytd => {
            let first = midnight(NaiveDate::from_ymd_opt(now.year(), 1, 1).unwrap());
            (first, now, Gran::Month, format!("YTD {}", now.year()))
        }
    }
}

async fn cmd_report(
    period: ReportPeriod,
    days: Option<i64>,
    top: usize,
    vendor: Option<&str>,
    host: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    maybe_spawn_daily_refresh();
    let cfg = Config::load()?;
    let ledger = Ledger::new(&cfg.ledger_path);
    let pricing = PricingCatalog::load(&Config::home().join("pricing.json"));
    let (since, until, gran, label) = report_window(period, days);

    // `--vendor <name>` scopes the report to entries whose `provider` id was
    // contributed by `~/.zoder/config.<name>.toml`. Validate early with a
    // clear error if the TOML is missing, then filter the ledger before
    // building so totals + counterfactual + avoided-spend headline all
    // reflect the vendor slice.
    let vendor_filter: Option<Vec<String>> = match vendor {
        Some(name) => {
            let available = Config::available_vendors();
            if !available.iter().any(|v| v == name) {
                anyhow::bail!(
                    "--vendor {}: no config.{}.toml in {} (vendors on disk: {})",
                    name,
                    name,
                    Config::home().display(),
                    if available.is_empty() {
                        "(none)".to_string()
                    } else {
                        available.join(", ")
                    }
                );
            }
            let ids = cfg.vendor_providers(name).to_vec();
            if ids.is_empty() {
                anyhow::bail!(
                    "--vendor {}: config.{}.toml is present but contributes no providers",
                    name,
                    name
                );
            }
            Some(ids)
        }
        None => None,
    };
    let rep = if let Some(ids) = &vendor_filter {
        let id_set: std::collections::HashSet<&String> = ids.iter().collect();
        let entries = ledger
            .entries_in_filtered(Some(since), Some(until), |e| id_set.contains(&e.provider))?;
        build_report_from_entries(&entries, &pricing, since, until, gran, &label)?
    } else if let Some(h) = host {
        // `--host <name>` scopes to every entry whose model publisher is `h`,
        // regardless of which provider served it — the complement of `--vendor`.
        let entries =
            ledger.entries_in_filtered(Some(since), Some(until), |e| e.effective_host() == h)?;
        build_report_from_entries(&entries, &pricing, since, until, gran, &label)?
    } else {
        build_report(&ledger, &pricing, since, until, gran, &label)?
    };

    // Live engine usage by period for the user's account (same source as the
    // zerocode dashboard). Best-effort: None when the daemon is unreachable.
    let engine_periods = engine_period_usage().await;

    if json {
        let mut root = serde_json::to_value(&rep)?;
        if let Some(obj) = root.as_object_mut() {
            if let Some(name) = vendor {
                obj.insert("vendor".into(), serde_json::Value::String(name.to_string()));
                if let Some(ids) = &vendor_filter {
                    obj.insert(
                        "vendor_provider_ids".into(),
                        serde_json::Value::Array(
                            ids.iter()
                                .map(|s| serde_json::Value::String(s.clone()))
                                .collect(),
                        ),
                    );
                }
            }
            if let Some(h) = host {
                obj.insert("host".into(), serde_json::Value::String(h.to_string()));
            }
        }
        if let (Some(rows), Some(obj)) = (&engine_periods, root.as_object_mut()) {
            obj.insert(
                "engine_periods".into(),
                serde_json::Value::Array(
                    rows.iter()
                        .map(|r| {
                            serde_json::json!({
                                "period": r.label,
                                "cost_usd": r.cost_usd,
                                "tokens": r.tokens,
                                "calls": r.calls,
                            })
                        })
                        .collect(),
                ),
            );
        }
        println!("{}", serde_json::to_string_pretty(&root)?);
        return Ok(());
    }

    let mtok = |t: u64| t as f64 / 1_000_000.0;
    let p = Pal::themed(&cfg.theme);
    // Cost cell: amber for real spend, green for $0 (color applied after padding
    // by the table renderer, so alignment is preserved).
    let cost_c = |c: f64| {
        let s = format!("{c:.2}");
        if c > 0.0 {
            Cell::amber(s)
        } else {
            Cell::green(s)
        }
    };

    if let Some(name) = vendor {
        println!(
            "{}  {}  vendor={}  {} -> {}  ({} days)",
            p.green_b("ZODER usage report"),
            p.amber(&rep.period),
            p.green_b(name),
            rep.since,
            rep.until,
            rep.days
        );
        if let Some(ids) = &vendor_filter {
            println!(
                "{}  {}",
                p.dim("filtered to providers:"),
                p.dim(&ids.join(", "))
            );
        }
    } else if let Some(h) = host {
        println!(
            "{}  {}  host={}  {} -> {}  ({} days)",
            p.green_b("ZODER usage report"),
            p.amber(&rep.period),
            p.green_b(h),
            rep.since,
            rep.until,
            rep.days
        );
    } else {
        println!(
            "{}  {}  {} -> {}  ({} days)",
            p.green_b("ZODER usage report"),
            p.amber(&rep.period),
            rep.since,
            rep.until,
            rep.days
        );
    }

    // Your account, by period — live from the cost engine (matches the zerocode
    // dashboard). Shown regardless of the selected window so day/month/quarter/
    // YTD are always visible for the user's account.
    if let Some(rows) = &engine_periods {
        println!(
            "\n{}  {}",
            p.green_b("Your account"),
            p.dim("live engine usage")
        );
        let mut t = Table::new(
            &p,
            vec![
                ("period", Al::L),
                ("cost($)", Al::R),
                ("tokens(M)", Al::R),
                ("calls", Al::R),
            ],
        );
        for r in rows {
            t.row(vec![
                Cell::new(r.label.clone()),
                cost_c(r.cost_usd),
                Cell::new(format!("{:.1}", mtok(r.tokens))),
                Cell::new(fmt_count(r.calls as u64)),
            ]);
        }
        t.print();
    } else {
        println!("\n{}", p.dim(&engine_offline_hint()));
    }

    if rep.total_calls == 0 {
        println!("\n{}", p.dim("(no ledger usage recorded in this window)"));
        return Ok(());
    }

    println!("\n{}", p.green_b(&format!("By {}", rep.bucket_gran)));
    {
        let mut t = Table::new(
            &p,
            vec![
                (rep.bucket_gran.as_str(), Al::L),
                ("cost($)", Al::R),
                ("tokens(M)", Al::R),
                ("calls", Al::R),
            ],
        );
        for b in &rep.buckets {
            t.row(vec![
                Cell::new(b.key.clone()),
                cost_c(b.cost_usd),
                Cell::new(format!("{:.1}", mtok(b.tokens_in + b.tokens_out))),
                Cell::new(fmt_count(b.calls)),
            ]);
        }
        t.print();
    }

    // Split the corpus into paid vs free. The split is unambiguous: `billed`
    // is derived from recorded spend, so free free-tier models can never be
    // mislabeled paid. `--top N` caps each section (0 = all). Real model names
    // are shown (truncated only past 44 chars).
    let limit = |n: usize| if top == 0 { n } else { top.min(n) };
    let name_cell = |model: &str| -> String {
        if model.chars().count() > 44 {
            let truncated: String = model.chars().take(43).collect();
            format!("{truncated}…")
        } else {
            model.to_string()
        }
    };
    let tok_pct = |t: u64| {
        if rep.total_tokens > 0 {
            t as f64 / rep.total_tokens as f64 * 100.0
        } else {
            0.0
        }
    };

    let paid: Vec<&_> = rep.by_model.iter().filter(|r| r.billed).collect();
    let free: Vec<&_> = rep.by_model.iter().filter(|r| !r.billed).collect();
    let paid_tok: u64 = paid.iter().map(|r| r.tokens).sum();
    let free_tok: u64 = free.iter().map(|r| r.tokens).sum();
    let pct = |t: u64, total: u64| {
        if total > 0 {
            t as f64 / total as f64 * 100.0
        } else {
            0.0
        }
    };

    println!(
        "\n{}  {}",
        p.amber("Paid models"),
        p.dim("billed cloud usage — real $ (input/output per Mtok)")
    );
    if paid.is_empty() {
        println!("  {}", p.dim("(none — all usage ran on free models)"));
    } else {
        let rate = |v: f64| {
            if v > 0.0 {
                Cell::amber(format!("{v:.2}"))
            } else {
                Cell::dim("—")
            }
        };
        let mut t = Table::new(
            &p,
            vec![
                ("model", Al::L),
                ("cost($)", Al::R),
                ("in $/Mtok", Al::R),
                ("out $/Mtok", Al::R),
                ("tokens(M)", Al::R),
                ("calls", Al::R),
                ("tok%", Al::R),
            ],
        );
        for r in paid.iter().take(limit(paid.len())) {
            t.row(vec![
                Cell::amber(name_cell(&r.model)),
                cost_c(r.cost_usd),
                rate(r.input_usd_per_mtok),
                rate(r.output_usd_per_mtok),
                Cell::new(format!("{:.1}", mtok(r.tokens))),
                Cell::new(fmt_count(r.calls)),
                Cell::new(format!("{:.0}%", pct(r.tokens, paid_tok))),
            ]);
        }
        t.print();
        if top != 0 && paid.len() > top {
            println!(
                "  {}",
                p.dim(&format!("… {} more paid (use --top 0)", paid.len() - top))
            );
        }
    }

    println!("\n{}  {}", p.green_b("Free models"), p.dim("$0 chargeback"));
    if free.is_empty() {
        println!("  {}", p.dim("(none)"));
    } else {
        let max_tok = free.iter().map(|r| r.tokens).max().unwrap_or(0);
        let mut t = Table::new(
            &p,
            vec![
                ("model", Al::L),
                ("tokens(M)", Al::R),
                ("calls", Al::R),
                ("tok%", Al::R),
                ("share", Al::L),
            ],
        );
        for r in free.iter().take(limit(free.len())) {
            let bar_frac = if max_tok > 0 {
                r.tokens as f64 / max_tok as f64
            } else {
                0.0
            };
            t.row(vec![
                Cell::green(name_cell(&r.model)),
                Cell::new(format!("{:.1}", mtok(r.tokens))),
                Cell::new(fmt_count(r.calls)),
                Cell::new(format!("{:.0}%", pct(r.tokens, free_tok))),
                Cell::green(share_bar(bar_frac, 16)),
            ]);
        }
        t.print();
        if top != 0 && free.len() > top {
            println!(
                "  {}",
                p.dim(&format!("… {} more free (use --top 0)", free.len() - top))
            );
        }
    }

    // By publisher host: the same spend re-sliced by who *published* the model
    // (segment before `/`), summed across every provider that served it. This
    // is the complement of the per-provider view above and the lens `--host`
    // filters on.
    if !rep.by_host.is_empty() {
        println!(
            "\n{}  {}",
            p.green_b("By host"),
            p.dim("model publisher, summed across all providers")
        );
        let mut t = Table::new(
            &p,
            vec![
                ("host", Al::L),
                ("cost($)", Al::R),
                ("tokens(M)", Al::R),
                ("calls", Al::R),
                ("tok%", Al::R),
            ],
        );
        for h in rep.by_host.iter().take(limit(rep.by_host.len())) {
            let host_cell = if h.billed {
                Cell::amber(h.host.clone())
            } else {
                Cell::green(h.host.clone())
            };
            t.row(vec![
                host_cell,
                cost_c(h.cost_usd),
                Cell::new(format!("{:.1}", mtok(h.tokens))),
                Cell::new(fmt_count(h.calls)),
                Cell::new(format!("{:.0}%", tok_pct(h.tokens))),
            ]);
        }
        t.print();
        if top != 0 && rep.by_host.len() > top {
            println!(
                "  {}",
                p.dim(&format!(
                    "… {} more hosts (use --top 0)",
                    rep.by_host.len() - top
                ))
            );
        }
    }

    println!("\n{}", p.green_b("Summary"));
    if rep.counterfactual_usd > 0.0 {
        let saved = (rep.counterfactual_usd - rep.total_cost_usd).max(0.0);
        let mult = if rep.total_cost_usd > 0.0 {
            format!(
                "{:.0}x cheaper",
                rep.counterfactual_usd / rep.total_cost_usd
            )
        } else {
            "all free".to_string()
        };
        println!(
            "  On {} this would cost {}; you paid {}.",
            rep.baseline_model,
            p.amber(&format!("${:.2}", rep.counterfactual_usd)),
            p.amber(&format!("${:.2}", rep.total_cost_usd))
        );
        println!(
            "  {} {}  {}",
            p.green_b("free models saved you"),
            p.green_b(&format!("${:.2}", saved)),
            p.green(&format!("({mult})"))
        );
    }
    let free_share = if rep.total_tokens > 0 {
        rep.free_tokens as f64 / rep.total_tokens as f64 * 100.0
    } else {
        0.0
    };
    println!(
        "  free token share    : {}  {}",
        p.green_b(&format!("{free_share:.0}%")),
        p.dim(&format!(
            "({:.1}M of {:.1}M tokens ran $0)",
            mtok(rep.free_tokens),
            mtok(rep.total_tokens)
        ))
    );
    if let Some(top) = rep.by_model.iter().find(|r| r.billed && r.cost_usd > 0.0) {
        let share = if rep.total_cost_usd > 0.0 {
            top.cost_usd / rep.total_cost_usd * 100.0
        } else {
            0.0
        };
        println!(
            "  top cost driver     : {}  {}",
            p.amber(&top.model),
            p.dim(&format!("{:.0}% of spend (${:.2})", share, top.cost_usd))
        );
    }
    println!(
        "  external chargeback : {}",
        p.amber(&format!("${:.2}", rep.total_cost_usd))
    );
    println!(
        "  free tokens     : {}  {}",
        p.green_b(&format!("{:.1}M", mtok(rep.free_tokens))),
        p.green("($0 chargeback)")
    );
    if rep.baseline_usd_per_mtok > 0.0 {
        println!(
            "  avoided spend       : {}  {}",
            p.green_b(&format!("${:.2}", rep.avoided_usd)),
            p.dim(&format!(
                "({:.1}M free tok @ {} = ${:.2}/M)",
                mtok(rep.free_tokens),
                rep.baseline_model,
                rep.baseline_usd_per_mtok
            ))
        );
    }

    // Enterprise billed cost snapshot (authoritative YTD + full-year projection),
    // when a config-driven source has written one to $ZODER_HOME/cost_snapshot.json.
    // Local-first: absent => silently skipped (the ledger view above stands alone).
    if let Some(snap) = CostSnapshot::load(&Config::home().join("cost_snapshot.json")) {
        if snap.org.is_some() || snap.personal.is_some() {
            let frac = CostSnapshot::frac_year_elapsed();
            println!(
                "\n{}  {}",
                p.green_b(&format!("Enterprise billed ({})", snap.year)),
                p.dim("authoritative YTD + full-year projection")
            );
            let show = |label: &str, s: &ScopeStat| {
                let proj = match s.project_runrate() {
                    Some((rr, m)) => format!(
                        "~${:.0}/yr linear · ~${:.0}/yr run-rate ({m})",
                        s.project_linear(frac),
                        rr
                    ),
                    None => format!("~${:.0}/yr linear", s.project_linear(frac)),
                };
                println!(
                    "  {} YTD {}  {}",
                    p.green_b(label),
                    p.amber(&format!("${:.2}", s.ytd_cost_usd)),
                    p.dim(&format!("-> {proj}"))
                );
            };
            if let Some(s) = &snap.org {
                show("org", s);
            }
            if let Some(s) = &snap.personal {
                show("you", s);
            }
        }
    }

    Ok(())
}

fn fmt_count(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1}B", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 10_000 {
        format!("{:.0}K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

async fn cmd_finops(
    sub: &str,
    since: Option<&str>,
    until: Option<&str>,
    window_days: u32,
    json: bool,
) -> anyhow::Result<()> {
    let eng = Engine::load()?;
    let led = Ledger::new(&eng.cfg.ledger_path);
    let pricing = PricingCatalog::load(&Config::home().join("pricing.json"));
    let argv: Vec<String> = std::iter::empty()
        .chain(std::iter::once("zoder".to_string()))
        .chain(std::iter::once(sub.to_string()))
        .chain(
            since
                .into_iter()
                .flat_map(|s| ["--since".to_string(), s.to_string()]),
        )
        .chain(
            until
                .into_iter()
                .flat_map(|s| ["--until".to_string(), s.to_string()]),
        )
        .chain(std::iter::once("--window-days".to_string()))
        .chain(std::iter::once(window_days.to_string()))
        .chain(json.then(|| "--json".to_string()))
        .collect();
    let code = finops_cli(&led, &pricing, &eng.cfg.theme, &argv)?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

async fn cmd_health(cli: &Cli, opts: HealthCmd) -> anyhow::Result<()> {
    let eng = Engine::load()?;
    let mut health = HealthStore::load(&eng.cfg.health_path);

    if opts.uninstall_daily {
        // Idempotent uninstall: missing files are not an error.
        match uninstall_daily_job() {
            Ok(msg) => println!("{msg}"),
            Err(e) => anyhow::bail!("uninstall-daily failed: {e}"),
        }
        return Ok(());
    }
    if opts.install_daily {
        // Idempotent install: overwrites an existing job.
        let bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("zoder"));
        match install_daily_job(&bin) {
            Ok(msg) => println!("{msg}"),
            Err(e) => anyhow::bail!("install-daily failed: {e}"),
        }
        return Ok(());
    }

    if opts.probe {
        if opts.all {
            run_probe_all(&eng, &mut health, cli.quiet, cli.json).await?;
        } else {
            run_probe_default(&eng, &mut health, cli.quiet).await?;
        }
        save_health(&health);
    }

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&health.models)?);
        return Ok(());
    }
    if health.models.is_empty() {
        println!("no health data yet (no calls recorded)");
        return Ok(());
    }
    println!(
        "{:46} {:>8} {:>8} {:>10} {:>9}",
        "model", "calls", "fails", "ewma_ms", "state"
    );
    for (id, h) in &health.models {
        println!(
            "{:46.46} {:>8} {:>8} {:>10.0} {:>9?}",
            id,
            h.calls,
            h.failures,
            h.ewma_latency_ms.unwrap_or(0.0),
            h.state()
        );
    }
    Ok(())
}

/// Backward-compatible probe: default provider only, free chat candidates.
/// Preserved unchanged for `--probe` without `--all` so existing scripts
/// keep their narrow, fast behavior.
async fn run_probe_default(
    eng: &Engine,
    health: &mut HealthStore,
    quiet: bool,
) -> anyhow::Result<()> {
    let provider_cfg = eng
        .cfg
        .provider(&eng.cfg.default_provider)
        .filter(|p| {
            !p.base_url
                .contains(zoder_core::config::PLACEHOLDER_PROVIDER_HOST)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot probe: default provider '{}' is unconfigured (the {} placeholder). \
                 Configure a real provider first.",
                eng.cfg.default_provider,
                zoder_core::config::PLACEHOLDER_PROVIDER_HOST
            )
        })?;
    let provider = OpenAiProvider::new(provider_cfg)?;
    let targets: Vec<String> = eng.corpus.free_chat().map(|m| m.id.clone()).collect();
    if !quiet {
        eprintln!("[zoder] probing {} free models...", targets.len());
    }
    for id in &targets {
        let req = ChatRequest {
            model: id.clone(),
            messages: vec![Message::new("user", "ping")],
            max_tokens: 1,
            temperature: 0.0,
            stream: false,
            show_reasoning: false,
            reasoning_effort: None,
        };
        let t = std::time::Instant::now();
        match provider.stream_chat(&req, None).await {
            Ok(_) => {
                let ms = t.elapsed().as_millis() as f64;
                health.record_success(id, ms);
                if !quiet {
                    eprintln!("  ok   {id}  {ms:.0}ms");
                }
            }
            Err(e) => {
                health.record_failure(id, &e.message);
                if !quiet {
                    eprintln!("  FAIL {id}  {}", e.message);
                }
            }
        }
    }
    Ok(())
}

/// Run the cross-provider probe: iterate every non-placeholder provider,
/// fetch the live model catalog, ping each model, classify, stamp into
/// the store, and print a per-provider report.
async fn run_probe_all(
    eng: &Engine,
    health: &mut HealthStore,
    quiet: bool,
    json: bool,
) -> anyhow::Result<()> {
    use std::io::Write as _;
    use zoder_core::Classification;

    /// One planned provider: the live `OpenAiProvider`, the capped
    /// target list, and how many entries the cap dropped (0 when the
    /// list fit under the cap). Bundled into a tiny struct so the
    /// "build plans" pass and the "ping plans" pass share a single
    /// shape and the dropped count never has to be tracked in a
    /// parallel Vec.
    struct Plan {
        provider_id: String,
        provider: OpenAiProvider,
        targets: Vec<String>,
        dropped: usize,
    }

    // Per-ping wall-clock cap. A single misbehaving endpoint must never
    // be able to wedge the whole daily launchd/systemd job: when the
    // timeout elapses the ping is recorded as a classified Error and
    // the sweep moves on to the next model.
    let ping_budget = std::time::Duration::from_secs(PROBE_PING_TIMEOUT_SECS);
    // Same cap for `list_models()` — a hanging /models endpoint on a
    // captive portal or dead proxy is just as fatal as a hanging chat
    // call. On timeout we fall back to `vec![p.id.clone()]` exactly
    // like the existing `.ok().filter(...)` fallback.
    let list_budget = ping_budget;

    // Build the live per-provider target lists up front, then do the
    // actual pings one provider at a time so the operator sees
    // per-provider progress and a killed run yields partial data.
    // The JSON path still collects everything in `flat_outcomes` so it
    // can emit a single well-formed JSON array at the end (the public
    // `--json` shape is preserved).
    let mut plans: Vec<Plan> = Vec::new();
    for p in &eng.cfg.providers {
        if p.base_url
            .contains(zoder_core::config::PLACEHOLDER_PROVIDER_HOST)
        {
            continue;
        }
        let provider = match OpenAiProvider::new(p) {
            Ok(pr) => pr,
            Err(e) => {
                if !quiet {
                    eprintln!("[zoder] skip provider {}: {e}", p.id);
                }
                continue;
            }
        };
        // Per-provider live catalog fetch — wrap in the same timeout
        // used for individual pings so a slow /models endpoint can't
        // hang the sweep either. On timeout, fall back to the provider
        // id itself (same shape as the existing `.ok().filter(...)`
        // fallback path).
        let live = match tokio::time::timeout(list_budget, provider.list_models()).await {
            Ok(Ok(v)) if !v.is_empty() => Some(v),
            Ok(Ok(_)) => None,
            Ok(Err(_)) => None,
            Err(_) => {
                if !quiet {
                    eprintln!(
                        "[zoder] list_models() for {} timed out after {}s; \
                         falling back to provider id",
                        p.id, PROBE_PING_TIMEOUT_SECS
                    );
                }
                None
            }
        };
        // Apply the per-provider cap BEFORE we start pinging. The cap is
        // logged, never silent: when dropped > 0 the human-readable
        // path emits a "(capped: probing X of Y models)" note alongside
        // the provider header.
        let (targets, dropped) = cap_targets(
            live.unwrap_or_else(|| vec![p.id.clone()]),
            PROBE_MAX_MODELS_PER_PROVIDER,
        );
        plans.push(Plan {
            provider_id: p.id.clone(),
            provider,
            targets,
            dropped,
        });
    }

    let mut flat_outcomes: Vec<ProbeOutcome> = Vec::new();

    // Iterate provider-by-provider. For each provider: ping every
    // (capped) target under the per-ping timeout, classify, stamp into
    // the store, and — in the human path — print the provider's block
    // immediately and flush stdout so the operator sees progress and a
    // SIGTERM/kill yields partial data.
    for plan in plans {
        let Plan {
            provider_id,
            provider,
            targets,
            dropped,
        } = plan;
        let mut provider_outcomes: Vec<ProbeOutcome> = Vec::with_capacity(targets.len());

        for model_id in &targets {
            let req = probe_request(model_id);
            let t = std::time::Instant::now();
            let outcome =
                match tokio::time::timeout(ping_budget, provider.stream_chat(&req, None)).await {
                    Ok(Ok(_)) => {
                        let ms = t.elapsed().as_millis() as f64;
                        health.record_classified_success(
                            model_id,
                            ms,
                            &provider_id,
                            Classification::Reachable,
                        );
                        ProbeOutcome {
                            provider_id: provider_id.clone(),
                            model_id: model_id.clone(),
                            latency_ms: Some(ms),
                            classification: Classification::Reachable,
                            note: None,
                        }
                    }
                    Ok(Err(err)) => {
                        let cls = classify_err(&err);
                        health.record_classified_failure(model_id, &err.message, &provider_id, cls);
                        ProbeOutcome {
                            provider_id: provider_id.clone(),
                            model_id: model_id.clone(),
                            latency_ms: None,
                            classification: cls,
                            note: Some(err.message.clone()),
                        }
                    }
                    Err(_) => {
                        // Per-ping timeout elapsed. Stamp a classified Error
                        // so the breaker sees it, surface a clear note to
                        // the operator, and continue to the next model —
                        // never abort the sweep.
                        let note = format!("probe timed out after {}s", PROBE_PING_TIMEOUT_SECS);
                        health.record_classified_failure(
                            model_id,
                            &note,
                            &provider_id,
                            Classification::Error,
                        );
                        ProbeOutcome {
                            provider_id: provider_id.clone(),
                            model_id: model_id.clone(),
                            latency_ms: None,
                            classification: Classification::Error,
                            note: Some(note),
                        }
                    }
                };
            provider_outcomes.push(outcome);
        }

        // Human path: print this provider's block IMMEDIATELY and
        // flush so the operator sees incremental progress and a killed
        // run still yields the providers that did finish. JSON path:
        // keep accumulating into the flat vec and emit one array at
        // the end (the `--json` shape must not change).
        if json {
            flat_outcomes.extend(provider_outcomes);
        } else {
            println!("provider {provider_id}:");
            if dropped > 0 {
                println!(
                    "  (capped: probing {} of {} models)",
                    targets.len(),
                    targets.len() + dropped,
                );
            }
            for o in &provider_outcomes {
                let lat = match o.latency_ms {
                    Some(ms) => format!("{ms:.0}ms"),
                    None => "   ---".to_string(),
                };
                let note = o.note.as_deref().unwrap_or("");
                println!(
                    "  {:<14}  {:<46.46}  {:>8}  {}",
                    o.classification.as_str(),
                    o.model_id,
                    lat,
                    note,
                );
            }
            // Flush after every provider so a SIGTERM/launchd kill
            // leaves a partial-but-coherent report on disk/stdout.
            let _ = std::io::stdout().flush();
        }
    }

    if json {
        // Emit a single JSON array of every outcome, exactly like the
        // pre-change shape: the `--json` contract is preserved.
        println!("{}", serde_json::to_string_pretty(&flat_outcomes)?);
    }
    Ok(())
}

/// Args handed to `cmd_health` after CLI parsing. Keeps the function
/// signature readable and makes the install/uninstall paths easy to test
/// in isolation (the install function takes a `bin` path so a fixture
/// can pass a temp binary path).
#[derive(Clone, Copy, Debug, Default)]
struct HealthCmd {
    probe: bool,
    all: bool,
    install_daily: bool,
    uninstall_daily: bool,
}

impl From<&Cmd> for Option<HealthCmd> {
    fn from(_cmd: &Cmd) -> Option<HealthCmd> {
        None
    }
}

/// Targets for the platform-specific scheduler. The struct is built by
/// `install_daily_job_paths` so unit tests can verify the chosen paths
/// without touching a real $HOME.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DailyInstallPaths {
    /// Files that must exist after install_daily succeeds.
    written: Vec<PathBuf>,
    /// Commands the test should run to verify (macOS) / load (systemd).
    load: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DailyBackend {
    Launchd,
    SystemdUser,
}

/// Decide where the daily-job files should live on the current OS. macOS
/// gets a single launchd plist at `~/Library/LaunchAgents/dev.ncz.zoder-health.plist`;
/// Linux gets a systemd user service + timer pair under
/// `~/.config/systemd/user/`. Tests pass an explicit `home` so they can
/// inspect the paths without touching the real $HOME.
fn install_daily_job_paths(home: &Path) -> DailyInstallPaths {
    match current_backend() {
        DailyBackend::Launchd => {
            let plist = home.join("Library/LaunchAgents/dev.ncz.zoder-health.plist");
            DailyInstallPaths {
                written: vec![plist.clone()],
                load: vec![format!("launchctl load -w {}", plist.display())],
            }
        }
        DailyBackend::SystemdUser => {
            let dir = home.join(".config/systemd/user");
            DailyInstallPaths {
                written: vec![
                    dir.join("zoder-health.service"),
                    dir.join("zoder-health.timer"),
                ],
                load: vec![format!(
                    "systemctl --user enable --now zoder-health.timer (in {})",
                    dir.display()
                )],
            }
        }
    }
}

/// `true` for the platforms we ship schedulers for. Anything else
/// returns an error from `install_daily_job`.
fn current_backend() -> DailyBackend {
    if cfg!(target_os = "macos") {
        DailyBackend::Launchd
    } else {
        DailyBackend::SystemdUser
    }
}

/// Render the launchd plist body. `bin` is the absolute path of the
/// `zoder` binary the launchd job will exec. Kept pure so tests can
/// assert the XML is well-formed.
fn render_launchd_plist(bin: &Path) -> String {
    // Use 09:00 local time so the daily sweep runs once a day, even on
    // a quiet dev box, with StartCalendarInterval. The plist is a string
    // so we can keep the test's expectations explicit.
    let bin = bin.display();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dev.ncz.zoder-health</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>health</string>
        <string>--probe</string>
        <string>--all</string>
    </array>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key>
        <integer>9</integer>
        <key>Minute</key>
        <integer>0</integer>
    </dict>
    <key>StandardOutPath</key>
    <string>/tmp/zoder-health.out.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/zoder-health.err.log</string>
</dict>
</plist>
"#
    )
}

/// Render the systemd user service unit. Runs `zoder health --probe --all`
/// once per execution; the timer unit (`render_systemd_timer`) is what
/// schedules it daily.
fn render_systemd_service(bin: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=zoder daily model-health sweep\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={} health --probe --all\n",
        bin.display()
    )
}

/// Render the systemd user timer unit: 09:00 local time, every day,
/// persistent across reboots.
fn render_systemd_timer() -> &'static str {
    "[Unit]\n\
     Description=zoder daily model-health sweep timer\n\
     \n\
     [Timer]\n\
     OnCalendar=*-*-* 09:00:00\n\
     Persistent=true\n\
     Unit=zoder-health.service\n\
     \n\
     [Install]\n\
     WantedBy=timers.target\n"
}

/// Install the daily sweep into the OS scheduler. Writes the platform
/// unit(s) and tries to load them so a fresh install is immediately
/// active. Returns a human-readable summary.
fn install_daily_job(bin: &Path) -> anyhow::Result<String> {
    let home = Config::home();
    let paths = install_daily_job_paths(&home);
    match current_backend() {
        DailyBackend::Launchd => {
            std::fs::create_dir_all(
                paths.written[0]
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("plist path has no parent"))?,
            )?;
            std::fs::write(&paths.written[0], render_launchd_plist(bin))?;
        }
        DailyBackend::SystemdUser => {
            let dir = paths.written[0]
                .parent()
                .ok_or_else(|| anyhow::anyhow!("systemd dir missing"))?;
            std::fs::create_dir_all(dir)?;
            std::fs::write(&paths.written[0], render_systemd_service(bin))?;
            std::fs::write(&paths.written[1], render_systemd_timer())?;
        }
    }
    Ok(format!(
        "installed daily health sweep:\n  wrote: {}\n  load: {}",
        paths
            .written
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        paths.load.join(" ; ")
    ))
}

/// Remove the daily sweep files. Missing files are not an error so this
/// is idempotent. The function returns a summary of what was removed.
fn uninstall_daily_job() -> anyhow::Result<String> {
    let home = Config::home();
    let paths = install_daily_job_paths(&home);
    let mut removed = Vec::new();
    for p in &paths.written {
        match std::fs::remove_file(p) {
            Ok(()) => removed.push(p.display().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(anyhow::anyhow!("remove {}: {e}", p.display())),
        }
    }
    if removed.is_empty() {
        Ok("daily health sweep not installed; nothing to remove".into())
    } else {
        Ok(format!("removed: {}", removed.join(", ")))
    }
}

async fn cmd_refresh(cli: &Cli) -> anyhow::Result<()> {
    let eng = Engine::load()?;
    let default_id = eng.cfg.default_provider.clone();
    let provider_cfg = eng
        .cfg
        .provider(&default_id)
        .filter(|p| {
            !p.base_url
                .contains(zoder_core::config::PLACEHOLDER_PROVIDER_HOST)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot refresh: default provider '{default_id}' is unconfigured (the {} \
                 placeholder). Configure a real provider first.",
                zoder_core::config::PLACEHOLDER_PROVIDER_HOST
            )
        })?;
    let provider = OpenAiProvider::new(provider_cfg)?;
    let served = provider.list_models().await.map_err(|e| {
        anyhow::anyhow!(
            "could not list models from {}: {}",
            provider_cfg.id,
            e.message
        )
    })?;

    // Free-provider catalog ingestion: every provider that declares `serves`
    // prefixes and is billed free contributes its live open-weight catalog to
    // the routing pool. Each provider's returned ids are filtered to its own
    // `serves` allowlist — so e.g. NVIDIA EIH's azure/aws/oci/gcp/google-hosted
    // catalog entries are dropped, leaving only the free NIMs (nvidia/* |
    // deepseek-ai/* | meta/llama-* | mistralai/*). A provider that is down or
    // missing its key is warned and skipped, never fatal to the refresh.
    let mut all_served = served.clone();
    let mut free_ids: Vec<String> = Vec::new();
    for p in &eng.cfg.providers {
        if p.serves.is_empty() || p.paid || p.billing != BillingMode::Free {
            continue;
        }
        // Reuse the default provider's already-fetched catalog; query others.
        let ids = if p.id == default_id {
            served.clone()
        } else {
            match OpenAiProvider::new(p) {
                Ok(client) => match client.list_models().await {
                    Ok(ids) => ids,
                    Err(e) => {
                        if !cli.quiet {
                            eprintln!("[refresh] skip free provider {}: {}", p.id, e.message);
                        }
                        continue;
                    }
                },
                Err(e) => {
                    if !cli.quiet {
                        eprintln!("[refresh] skip free provider {}: {e}", p.id);
                    }
                    continue;
                }
            }
        };
        let kept: Vec<String> = ids
            .into_iter()
            .filter(|id| p.serves.iter().any(|pre| id.starts_with(pre.as_str())))
            .collect();
        all_served.extend(kept.iter().cloned());
        free_ids.extend(kept);
    }

    let mut corpus = eng.corpus;
    // Reconcile against the UNION of every provider's served ids so a free
    // provider's NIMs are never retired by the default provider's narrower
    // list. Dedup first: `reconcile` snapshots the existing-id set once, so a
    // duplicate id (default provider also declares `serves`) would otherwise be
    // inserted as a duplicate corpus entry.
    {
        let mut seen = std::collections::HashSet::new();
        all_served.retain(|id| seen.insert(id.clone()));
    }
    let report = corpus.reconcile(&all_served);
    let promoted = corpus.ingest_free_chat(&free_ids);
    corpus.save(&eng.cfg.corpus_path)?;

    if cli.json {
        println!(
            "{}",
            serde_json::json!({
                "served": served.len(),
                "free_ingested": free_ids.len(),
                "promoted": promoted,
                "added": report.added,
                "retired": report.retired,
                "kept": report.kept,
                "total": corpus.models.len(),
            })
        );
    } else {
        println!(
            "refreshed: {} served, {} new, {} retired, {} kept ({} total); {} free-provider NIM(s) ingested, {} promoted into routing",
            served.len(),
            report.added.len(),
            report.retired.len(),
            report.kept,
            corpus.models.len(),
            free_ids.len(),
            promoted,
        );
        if !report.added.is_empty() {
            println!("  new (unclassified, run corpus builder to score/bench):");
            for id in &report.added {
                println!("    + {id}");
            }
        }
        if !report.retired.is_empty() {
            println!("  retired (no longer served):");
            for id in &report.retired {
                println!("    - {id}");
            }
        }
    }
    Ok(())
}

/// Build brand. The zoder export rewrites the `zoder` token to `zoder`
/// everywhere, so this constant is `"zoder"` in the internal build and
/// `"zoder"` in the public one — a transform-safe way to diverge behavior that
/// must differ between the two flavors.
const BRAND: &str = "zoder";

/// True in the public zoder build. zoder has no your provider's billing: the public pricing
/// feed is its *only* cost source, so `pricing refresh` feeds the engine by
/// default. zoder stays your provider's billing-authoritative and only feeds the engine for
/// explicitly-configured external providers (behind `--external`).
fn is_zoder_build() -> bool {
    BRAND == "zoder"
}

/// Resolve the zeroclaw cost-engine data dir (where the daemon reads
/// `pricing.json`). Mirrors zeroclaw's own resolution: `ZEROCLAW_CONFIG_DIR`
/// (tilde-expanded) else `$HOME/.zeroclaw`, then `/data`.
fn zeroclaw_data_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    if let Ok(dir) = std::env::var("ZEROCLAW_CONFIG_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            let expanded = if let Some(rest) = dir.strip_prefix("~/") {
                PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest)
            } else {
                PathBuf::from(dir)
            };
            return expanded.join("data");
        }
    }
    // Brand-default keeps the two builds' engines isolated on a shared machine:
    // the public build uses ~/.zoder, the internal build ~/.zeroclaw. The
    // zerocode/zeroclaw launch wiring points the daemon at the same dir.
    let engine_home = if is_zoder_build() {
        ".zoder"
    } else {
        ".zeroclaw"
    };
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(engine_home)
        .join("data")
}

/// Human-actionable message when the cost engine is unreachable. Distinguishes a
/// stale socket (file present, nobody listening) from a daemon that was never
/// started, and prints the exact command to bring it up for *this* config dir.
fn engine_offline_hint() -> String {
    let socket = engine_socket_path();
    let data_dir = zeroclaw_data_dir();
    let config_dir = data_dir.parent().unwrap_or(&data_dir).to_path_buf();
    let state = if socket.exists() {
        "a stale socket is present but no daemon is listening"
    } else {
        "no daemon is running"
    };
    format!(
        "(cost engine offline — {state}; start it with:  zeroclaw daemon --config-dir {})",
        config_dir.display()
    )
}

/// Unix-socket path of the local cost engine: `$ZEROCLAW_SOCKET` if set,
/// otherwise `<data_dir>/daemon.sock`.
fn engine_socket_path() -> std::path::PathBuf {
    if let Ok(s) = std::env::var("ZEROCLAW_SOCKET") {
        if !s.trim().is_empty() {
            return std::path::PathBuf::from(s);
        }
    }
    zeroclaw_data_dir().join("daemon.sock")
}

/// One period's live engine usage for the user's account.
struct PeriodUsage {
    label: String,
    cost_usd: f64,
    tokens: u64,
    calls: usize,
}

/// Pull day / month-to-date / quarter-to-date / year-to-date usage for the
/// user's account from the live cost engine. Returns `None` when the daemon is
/// unreachable so the caller can degrade gracefully (the engine is the same one
/// the zerocode dashboard reads, so these match the TUI).
async fn engine_period_usage() -> Option<Vec<PeriodUsage>> {
    let socket = engine_socket_path();
    let periods = [
        ReportPeriod::Day,
        ReportPeriod::Month,
        ReportPeriod::Quarter,
        ReportPeriod::Ytd,
    ];
    let mut rows = Vec::new();
    let mut any_ok = false;
    for p in periods {
        let (since, until, _, label) = report_window(p, None);
        match zoder_core::fetch_engine_cost(&socket, Some(since), Some(until), None).await {
            Ok(sum) => {
                any_ok = true;
                rows.push(PeriodUsage {
                    label,
                    cost_usd: sum.window_cost_usd(),
                    tokens: sum.total_tokens,
                    calls: sum.request_count,
                });
            }
            Err(_) => rows.push(PeriodUsage {
                label,
                cost_usd: 0.0,
                tokens: 0,
                calls: 0,
            }),
        }
    }
    any_ok.then_some(rows)
}

/// Build a cost-engine catalog restricted to the *external paid* models zoder
/// knows about. Keyed by the served id the engine records (plus the leaf as an
/// alias) so exact matching in the engine resolves a real rate. Free free-tier
/// models are excluded by construction (`paid == false`), so they stay $0.
fn build_external_catalog(cat: &PricingCatalog, corpus: &Corpus) -> PricingCatalog {
    let mut out = PricingCatalog {
        generated: cat.generated.clone(),
        window: cat.window.clone(),
        baseline_usd_per_mtok: cat.baseline_usd_per_mtok,
        baseline_model: cat.baseline_model.clone(),
        ..Default::default()
    };
    for m in &corpus.models {
        if !m.paid {
            continue;
        }
        if let Some(price) = cat.lookup(&m.id) {
            if price.is_priced() {
                out.models.insert(m.id.clone(), price.clone());
                if !m.leaf.is_empty() {
                    out.models
                        .entry(m.leaf.clone())
                        .or_insert_with(|| price.clone());
                }
            }
        }
    }
    out
}

/// All priced entries of `cat`, used by the zoder build (the public feed is
/// zoder's only cost source, so it prices every model the feed knows).
fn full_priced_catalog(cat: &PricingCatalog) -> PricingCatalog {
    let mut out = PricingCatalog {
        generated: cat.generated.clone(),
        window: cat.window.clone(),
        baseline_usd_per_mtok: cat.baseline_usd_per_mtok,
        baseline_model: cat.baseline_model.clone(),
        ..Default::default()
    };
    for (id, price) in &cat.models {
        if price.is_priced() {
            out.models.insert(id.clone(), price.clone());
        }
    }
    out
}

/// Publish per-token rates into the zeroclaw cost engine
/// (`<data_dir>/pricing.json`), so models report a real `cost_usd` in the
/// dashboard and reports.
///
/// Behavior diverges by build:
///   * **zoder** publishes the full priced feed by default (its only cost
///     source). `--external` is accepted but redundant.
///   * **zoder** stays your provider's billing-authoritative: it publishes nothing unless
///     `--external` is passed *and* a non-free provider is configured, and even
///     then only that provider's paid models (free-tier stay $0).
fn publish_engine_catalog(cat: &PricingCatalog, external: bool, json: bool) -> anyhow::Result<()> {
    let zoder = is_zoder_build();
    if !zoder && !external {
        // zoder default: leave the engine untouched (your provider's billing-exclusive).
        return Ok(());
    }

    let engine_path = zeroclaw_data_dir().join("pricing.json");

    let engine_cat = if zoder {
        full_priced_catalog(cat)
    } else {
        // zoder --external: require a configured external provider, then scope
        // to the paid models zoder actually knows about.
        let cfg = Config::load()?;
        let has_external = cfg
            .providers
            .iter()
            .any(|p| p.billing != BillingMode::Free || p.paid);
        if !has_external {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "external": false,
                        "reason": "no external (non-free) provider configured; prices via your provider's billing only",
                    })
                );
            } else {
                println!(
                    "  external   : skipped (no non-free provider in config; \
                     prices via your provider's billing only)"
                );
            }
            return Ok(());
        }
        let corpus = Corpus::load(&cfg.corpus_path).unwrap_or_default();
        build_external_catalog(cat, &corpus)
    };

    engine_cat.save(&engine_path)?;
    let priced = engine_cat.models.len();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "external": true,
                "engine_catalog": engine_path.display().to_string(),
                "priced_models": priced,
            })
        );
    } else {
        let label = if zoder { "engine" } else { "external" };
        println!(
            "  {label:<10} : {priced} model rates -> {}",
            engine_path.display()
        );
        println!("               (restart/reload the daemon to apply)");
    }
    Ok(())
}

/// Keep the pricing feed fresh without a daemon-side fetch or an external
/// scheduler: when the engine catalog is missing or older than 24h, spawn a
/// detached `pricing refresh` so the *next* run (and the ephemeral daemon it
/// starts) sees current rates. Non-blocking and network-tolerant — a failed
/// refresh just leaves the prior file in place.
///
/// zoder-only by design: the public feed is zoder's sole cost source. zoder
/// is your provider's billing-authoritative and never auto-fetches the public feed; its
/// external rates are an explicit `pricing refresh --external` (or a user-owned
/// schedule).
fn maybe_spawn_daily_refresh() {
    if !is_zoder_build() {
        return;
    }
    let path = zeroclaw_data_dir().join("pricing.json");
    let stale = match std::fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(modified) => modified
            .elapsed()
            .map(|age| age > std::time::Duration::from_secs(24 * 60 * 60))
            .unwrap_or(true),
        Err(_) => true,
    };
    if !stale {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .args(["pricing", "refresh"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

async fn cmd_pricing(action: &PricingCmd, json: bool) -> anyhow::Result<()> {
    let path = Config::home().join("pricing.json");
    match action {
        PricingCmd::Refresh {
            source,
            baseline,
            external,
        } => {
            let sources = PricingSource::parse_list(source);
            let (cat, stats) = sync_catalog(&sources, Some(baseline)).await?;
            cat.save(&path)?;
            publish_engine_catalog(&cat, *external, json)?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "litellm": stats.litellm,
                        "openrouter": stats.openrouter,
                        "total": stats.total,
                        "baseline_model": cat.baseline_model,
                        "baseline_usd_per_mtok": cat.baseline_usd_per_mtok,
                        "errors": stats.errors,
                        "path": path.display().to_string(),
                    })
                );
            } else {
                println!("pricing catalog refreshed -> {}", path.display());
                println!("  litellm    : {} models", stats.litellm);
                println!("  openrouter : {} models", stats.openrouter);
                println!("  total      : {} models", stats.total);
                if cat.baseline_usd_per_mtok > 0.0 {
                    println!(
                        "  baseline   : {} (${:.2}/Mtok blended)",
                        cat.baseline_model, cat.baseline_usd_per_mtok
                    );
                } else {
                    println!(
                        "  baseline   : {:?} not found (report counterfactual disabled)",
                        baseline
                    );
                }
                for e in &stats.errors {
                    eprintln!("  warning: {e}");
                }
            }
            Ok(())
        }
        PricingCmd::Show { model } => {
            let cat = PricingCatalog::load(&path);
            match cat.lookup(model) {
                Some(p) if json => println!("{}", serde_json::to_string_pretty(p)?),
                Some(p) => {
                    println!("{model}");
                    println!("  input    : ${:.3}/Mtok", p.input_usd_per_mtok);
                    println!("  output   : ${:.3}/Mtok", p.output_usd_per_mtok);
                    if p.cache_read_usd_per_mtok > 0.0 {
                        println!("  cache rd : ${:.3}/Mtok", p.cache_read_usd_per_mtok);
                    }
                    if p.cache_write_usd_per_mtok > 0.0 {
                        println!("  cache wr : ${:.3}/Mtok", p.cache_write_usd_per_mtok);
                    }
                    if p.reasoning_usd_per_mtok > 0.0 {
                        println!("  reasoning: ${:.3}/Mtok", p.reasoning_usd_per_mtok);
                    }
                    if p.usd_per_mtok > 0.0 {
                        println!("  blended  : ${:.3}/Mtok", p.usd_per_mtok);
                    }
                    println!("  source   : {}", p.source);
                    println!(
                        "  tier     : {}",
                        if p.is_priced() { "PAID" } else { "free" }
                    );
                }
                None if json => println!("null"),
                None => println!("{model}: not in catalog (treated as $0 / free)"),
            }
            Ok(())
        }
    }
}

async fn cmd_reconcile(provider: &str, days: i64, json: bool) -> anyhow::Result<()> {
    let res = match provider.to_ascii_lowercase().as_str() {
        "openai" => {
            let key = std::env::var("OPENAI_ADMIN_KEY").map_err(|_| {
                anyhow::anyhow!(
                    "set OPENAI_ADMIN_KEY (an OpenAI Admin API key, not your inference key)"
                )
            })?;
            openai_costs(&key, days).await?
        }
        "anthropic" => {
            let key = std::env::var("ANTHROPIC_ADMIN_KEY").map_err(|_| {
                anyhow::anyhow!(
                    "set ANTHROPIC_ADMIN_KEY (an Anthropic Admin API key, prefix sk-ant-admin...)"
                )
            })?;
            anthropic_costs(&key, days).await?
        }
        other => anyhow::bail!("unsupported provider {other:?} (supported: openai, anthropic)"),
    };

    // Local ledger spend over the same window, priced by the catalog, for the
    // same provider id -- the figure the provider's cost API is trued up against.
    let cfg = Config::load()?;
    let ledger = Ledger::new(&cfg.ledger_path);
    let pricing = PricingCatalog::load(&Config::home().join("pricing.json"));
    let since = chrono::Utc::now() - chrono::Duration::days(days.max(1));
    let local: f64 = ledger
        .entries_in(Some(since), None)?
        .iter()
        .filter(|e| e.provider.eq_ignore_ascii_case(provider))
        .map(|e| pricing.cost(&e.model, e.tokens_in, e.tokens_out))
        .sum();

    if json {
        println!(
            "{}",
            serde_json::json!({
                "provider": res.provider,
                "days": res.days,
                "provider_billed_usd": res.billed_usd,
                "local_ledger_usd": local,
                "delta_usd": res.billed_usd - local,
                "source": res.source,
            })
        );
    } else {
        println!("reconcile {} ({} days)", res.provider, res.days);
        println!(
            "  provider billed : ${:.2}  ({})",
            res.billed_usd, res.source
        );
        println!("  local ledger    : ${:.2}  (priced by catalog)", local);
        println!("  delta           : ${:.2}", res.billed_usd - local);
    }
    Ok(())
}

fn cmd_sessions(json: bool) -> anyhow::Result<()> {
    let cfg = Config::load()?;
    let dir = cfg.sessions_dir();
    let list = Session::list(&dir)?;
    if json {
        let arr: Vec<_> = list
            .iter()
            .map(|(id, updated, msgs)| serde_json::json!({"id": id, "updated": updated, "messages": msgs}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&arr)?);
        return Ok(());
    }
    if list.is_empty() {
        println!("no sessions yet (use --session <id> or --continue)");
        return Ok(());
    }
    println!("{:24} {:>20} {:>8}", "session", "updated_unix", "messages");
    for (id, updated, msgs) in &list {
        println!("{:24.24} {:>20} {:>8}", id, updated, msgs);
    }
    Ok(())
}

fn billing_label(b: BillingMode) -> &'static str {
    match b {
        BillingMode::Free => "free",
        BillingMode::Metered => "metered",
        BillingMode::Subscription => "subscription",
    }
}

/// A non-secret descriptor of a provider's auth for display/JSON. Never emits
/// an inline bearer token — only the env-var NAME (not a secret) or a redacted
/// placeholder for inline tokens.
fn auth_kind_label(a: &zoder_core::Auth) -> String {
    match a {
        zoder_core::Auth::None => "none".to_string(),
        zoder_core::Auth::Env { var } => format!("env:{var}"),
        zoder_core::Auth::Bearer { .. } => "bearer:inline([redacted])".to_string(),
        zoder_core::Auth::ApiKeyHeader { header, var } => {
            format!("api_key_header:{header} via env:{var}")
        }
    }
}

fn cmd_providers(json: bool) -> anyhow::Result<()> {
    let eng = Engine::load()?;
    // The tier catalog is bundled + on-disk refreshable; same pattern as
    // Corpus. Loading it is best-effort: an unreadable / unparseable catalog
    // never breaks `zoder providers` — explicit windows still work, the
    // `tier` field on a plan just degrades to "unknown tier" gracefully.
    let catalog = load_tier_catalog(Some(&zoder_core::subscription_tiers::default_catalog_path(
        &Config::home(),
    )));
    // Track whether any provider actually USED the catalog (preset or
    // preset-with-overrides). The disclaimer is only relevant when a
    // preset-backed number appears in the output — an operator who hand-
    // entered every window doesn't need the "estimates — verify against
    // your dashboard" reminder. We track per-provider so the disclaimer
    // shows up exactly when it earns its keep, and not as noise.
    let has_any_preset = eng
        .cfg
        .providers
        .iter()
        .any(|p| match (p.billing, &p.subscription) {
            (BillingMode::Subscription, Some(plan)) => plan.tier.is_some(),
            _ => false,
        });
    if json {
        // Top-level array shape (NOT wrapped in `{ "providers": [...] }`).
        // The JSON contract for `zoder providers --json` is a bare array of
        // provider descriptors — preserving this shape is required so
        // existing scrapers / scripts that consume the output by indexing
        // the array directly continue to work. The tier-catalog disclaimer
        // is emitted on the human-readable path (below); it is intentionally
        // NOT threaded into the machine JSON here because doing so would
        // break the array contract.
        //
        // Redacted view: never serialize inline bearer tokens (the raw
        // config carries `Auth::Bearer { token }`). Env-var names are safe
        // to show.
        let redacted: Vec<serde_json::Value> = eng
            .cfg
            .providers
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "base_url": p.base_url,
                    "kind": p.kind,
                    "paid": p.paid,
                    "billing": billing_label(p.billing),
                    "auth": auth_kind_label(&p.auth),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&redacted)?);
        return Ok(());
    }
    // The disclaimer is the "verify-dashboard" reminder that makes a
    // preset-driven report honest. We print it ONCE, above the table, and
    // only when at least one provider in the output used a preset — never
    // as boilerplate on a fully-explicit config.
    if has_any_preset && !catalog.disclaimer.is_empty() {
        println!(
            "tier catalog: as_of={} — {}",
            catalog.as_of, catalog.disclaimer
        );
    }
    let entries = Ledger::new(&eng.cfg.ledger_path)
        .entries()
        .unwrap_or_default();
    for p in &eng.cfg.providers {
        let auth = p.auth.resolve().map(|_| "ok").unwrap_or("MISSING");
        // Subscription plan header: declare the tier name + `as_of` +
        // `confidence` so the operator reads the cap honestly and knows when
        // the catalog was last curated. Explicit (no `tier`) plans print
        // nothing extra; that path is unchanged.
        let plan_header = match (p.billing, &p.subscription) {
            (BillingMode::Subscription, Some(plan)) => {
                if let Some(t) = &plan.tier {
                    let conf = catalog
                        .tier(&p.id, t)
                        .map(|e| e.confidence.as_str())
                        .unwrap_or("unknown");
                    format!(" tier={} (as_of={}, confidence={})", t, catalog.as_of, conf)
                } else {
                    String::new()
                }
            }
            _ => String::new(),
        };
        println!(
            "{:10} {:42} kind={:12} billing={:12} auth={}{}",
            p.id,
            p.base_url,
            p.kind,
            billing_label(p.billing),
            auth,
            plan_header
        );
        if let (BillingMode::Subscription, Some(plan)) = (p.billing, &p.subscription) {
            for w in plan_usage(&entries, &p.id, plan, &catalog) {
                let reset = w
                    .next_reset_utc
                    .as_deref()
                    .map(|r| format!("; next reset {r}"))
                    .unwrap_or_default();
                let warn = if w.approaching {
                    "  ⚠ approaching cap"
                } else {
                    ""
                };
                let conf_tag = w
                    .confidence
                    .as_deref()
                    .map(|c| format!(" [est:{}]", c))
                    .unwrap_or_default();
                println!(
                    "             {:>7} window: {:.0}/{:.0} {} ({:.0}% of cap){}{}{}",
                    w.name,
                    w.used,
                    w.cap,
                    w.unit,
                    w.pct * 100.0,
                    reset,
                    conf_tag,
                    warn
                );
            }
            let amort = amortized_per_call(&entries, &p.id, plan, &catalog);
            if amort > 0.0 {
                println!("             amortized: ${amort:.4}/call (flat fee / 30d calls)");
            }
        }
    }
    Ok(())
}

fn cmd_config(validate: bool) -> anyhow::Result<()> {
    let cfg = Config::load()?;
    println!("home:        {}", Config::home().display());
    println!("corpus:      {}", cfg.corpus_path.display());
    println!("ledger:      {}", cfg.ledger_path.display());
    println!("health:      {}", cfg.health_path.display());
    println!("sessions:    {}", cfg.sessions_dir().display());
    println!("default:     {}", cfg.default_provider);

    let problems = cfg.validate();
    match Corpus::load(&cfg.corpus_path) {
        Ok(c) => {
            let free = c.free_chat().count();
            println!(
                "corpus OK:   {} models, {} free chat route-candidates",
                c.models.len(),
                free
            );
        }
        Err(e) => println!("corpus ERROR: {e}"),
    }
    for p in &cfg.providers {
        let auth = p.auth.resolve().map(|_| "ok").unwrap_or("MISSING");
        println!("provider {}: auth={}", p.id, auth);
    }

    if problems.is_empty() {
        println!("config:      VALID");
    } else {
        println!("config:      {} PROBLEM(S)", problems.len());
        for p in &problems {
            println!("  - {p}");
        }
        if validate {
            anyhow::bail!("configuration is invalid");
        }
    }
    Ok(())
}

#[cfg(test)]
mod gate_tests {
    //! Unit tests for the `zoder gate` slice-5 wiring. These exercise
    //! the pure orchestrator (`run_gate_for_root` + `GateOutcome`)
    //! without touching stdout or `process::exit`, so every branch
    //! (Rust, Node, polyglot, missing markers, missing required tool)
    //! is verifiable deterministically.
    //!
    //! `tempfile::tempdir()` is the only new dep introduced by these
    //! tests; `zoder-cli` doesn't depend on `tempfile` directly, so
    //! we declare a tiny dev-dependency on it in `Cargo.toml`.

    use super::run_gate_for_root;
    use std::path::Path;
    use zoder_core::gate::{Ecosystem, GateMode, GateStatus, GateStep, StepCategory, StepOutcome};
    use zoder_core::gate_bundle::default_bundle;

    #[test]
    fn empty_repo_yields_no_plan_and_green_status() {
        // A repo with NO marker files must produce an empty plan, no
        // signals, and a Green (empty) report — the gate operates on
        // whatever it can see and reports honestly. Never panics.
        let tmp = tempfile::tempdir().expect("tempdir");
        let outcome = run_gate_for_root(tmp.path());
        assert!(outcome.plan.is_empty(), "no markers -> empty plan");
        assert!(outcome.signals.ecosystems.is_empty());
        assert!(outcome.probe.is_empty(), "no plan -> no probe");
        let report = outcome.run(&GateMode::Strict);
        assert_eq!(report.status, GateStatus::Green);
        assert!(report.results.is_empty());
    }

    #[test]
    fn rust_repo_detected_and_baseline_plan_has_required_core() {
        // A repo with only Cargo.toml at the root must be detected as
        // Rust and produce the Rust baseline plan (fmt, clippy,
        // build, test, deny, audit).
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("write");
        let outcome = run_gate_for_root(tmp.path());
        assert_eq!(outcome.signals.ecosystems, vec![Ecosystem::Rust]);
        let names: Vec<&str> = outcome.plan.iter().map(|s| s.name.as_str()).collect();
        for required in ["fmt", "clippy", "build", "test", "deny"] {
            assert!(
                names.contains(&required),
                "Rust baseline missing `{required}`; got {names:?}"
            );
        }
        // Added-baseline breakdown should name every baseline step.
        for required in ["fmt", "clippy", "build", "test", "deny", "audit"] {
            assert!(
                outcome
                    .pre_run_compat
                    .added_baseline
                    .contains(&required.to_string()),
                "added_baseline missing `{required}`",
            );
        }
    }

    #[test]
    fn node_repo_with_pnpm_uses_pnpm_commands() {
        // A repo with package.json + pnpm-lock.yaml must be detected
        // as Node + pnpm and produce the pnpm-refined plan.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("package.json"), "{}\n").expect("write");
        std::fs::write(tmp.path().join("pnpm-lock.yaml"), "").expect("write");
        let outcome = run_gate_for_root(tmp.path());
        assert_eq!(outcome.signals.ecosystems, vec![Ecosystem::Node]);
        assert_eq!(
            outcome.signals.package_managers,
            vec![(Ecosystem::Node, zoder_core::gate::PackageManager::Pnpm)]
        );
        // The lint step must use `pnpm run lint`, NOT the npm default.
        let lint = outcome
            .plan
            .iter()
            .find(|s| s.name == "lint")
            .expect("lint step");
        assert_eq!(lint.command, vec!["pnpm", "run", "lint"]);
        assert_eq!(lint.tool, "pnpm");
    }

    #[test]
    fn polyglot_repo_unions_per_ecosystem_baselines() {
        // Rust + Node in the same repo must produce a unioned plan
        // with both ecosystems' baseline steps, in deterministic order
        // (Rust first, then Node).
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("write");
        std::fs::write(tmp.path().join("package.json"), "{}\n").expect("write");
        let outcome = run_gate_for_root(tmp.path());
        assert_eq!(
            outcome.signals.ecosystems,
            vec![Ecosystem::Rust, Ecosystem::Node],
        );
        // Rust steps come first.
        let first_rust = outcome
            .plan
            .iter()
            .position(|s| {
                matches!(
                    s.category,
                    StepCategory::Lint
                        | StepCategory::Security
                        | StepCategory::Build
                        | StepCategory::Test
                        | StepCategory::Format
                ) && s.command.first().map(String::as_str) == Some("cargo")
                    || s.command.first().map(String::as_str) == Some("cargo")
            })
            .expect("at least one rust step");
        // First non-rust step (npx prettier --check) comes after the
        // Rust baselines.
        let first_node = outcome
            .plan
            .iter()
            .position(|s| s.command.first().map(String::as_str) == Some("npx"))
            .expect("at least one node step");
        assert!(
            first_rust < first_node,
            "rust steps must precede node steps"
        );
    }

    #[test]
    fn python_repo_with_uv_uses_uv_run_pytest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("pyproject.toml"),
            "[project]\nname = \"x\"\n",
        )
        .expect("write");
        std::fs::write(tmp.path().join("uv.lock"), "").expect("write");
        let outcome = run_gate_for_root(tmp.path());
        assert_eq!(outcome.signals.ecosystems, vec![Ecosystem::Python]);
        let test = outcome
            .plan
            .iter()
            .find(|s| s.name == "test")
            .expect("test step");
        assert_eq!(test.command, vec!["uv", "run", "pytest", "-q"]);
    }

    #[test]
    fn framework_hints_surfaced_in_signals() {
        // Next.js + Vite + Vitest config files must produce matching
        // framework hints so the report tells the reviewer what's in
        // the repo beyond the ecosystem defaults.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("package.json"), "{}\n").expect("write");
        std::fs::write(tmp.path().join("next.config.js"), "module.exports = {};\n").expect("write");
        std::fs::write(tmp.path().join("vite.config.ts"), "export default {};\n").expect("write");
        std::fs::write(tmp.path().join("vitest.config.ts"), "export default {};\n").expect("write");
        let outcome = run_gate_for_root(tmp.path());
        for hint in ["next.js", "vite", "vitest"] {
            assert!(
                outcome.signals.framework_hints.contains(&hint.to_string()),
                "framework_hints missing `{hint}`: {:?}",
                outcome.signals.framework_hints,
            );
        }
    }

    #[test]
    fn probe_lists_every_unique_tool_in_plan_order() {
        // The probe must dedupe by tool name AND preserve plan order,
        // so the rendered output is deterministic.
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("write");
        let outcome = run_gate_for_root(tmp.path());
        let tools: Vec<&str> = outcome.probe.iter().map(|p| p.tool.as_str()).collect();
        // Rust baseline tools (in plan order, deduped): cargo,
        // cargo-deny, cargo-audit. Every entry must be unique.
        let mut deduped = tools.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(
            tools.len(),
            deduped.len(),
            "probe must not duplicate: {tools:?}"
        );
        // Plan-order assertion: cargo first (used by fmt/clippy/
        // build/test), then cargo-deny, then cargo-audit.
        assert_eq!(tools.first().copied(), Some("cargo"));
    }

    #[test]
    fn outcome_run_under_strict_with_missing_required_managed_tool_is_red() {
        // End-to-end Slice-5 promise: a required managed tool that
        // isn't on PATH must produce Red under Strict, and the
        // GateReport must surface it. We test by constructing a plan
        // that references gitleaks (in the bundle, commonly absent)
        // and running it. If gitleaks happens to be installed on
        // this dev box, we assert the run produces a sensible status
        // and skip the Red assertion.
        let bundle_ids: Vec<&str> = default_bundle().iter().map(|t| t.id).collect();
        assert!(bundle_ids.contains(&"gitleaks"));

        let plan = vec![GateStep {
            name: "secrets".to_string(),
            category: StepCategory::Secret,
            command: vec!["gitleaks".to_string(), "detect".to_string()],
            tool: "gitleaks".to_string(),
            required: true,
        }];
        let env = zoder_core::gate_bundle::PathEnv::new();
        let (results, status) = zoder_core::gate::run_plan(&plan, GateMode::Strict, &env);
        if env.find_binary("gitleaks").is_some() {
            // gitleaks installed on this box; run is Green.
            assert!(matches!(status, GateStatus::Green));
            assert!(matches!(results[0].outcome, StepOutcome::Passed));
            return;
        }
        assert_eq!(
            status,
            GateStatus::Red {
                failures: vec!["secrets".to_string()],
            },
            "strict + missing required managed tool must be Red",
        );
        assert!(matches!(results[0].outcome, StepOutcome::Failed));
    }

    #[test]
    fn outcome_run_under_local_iterate_with_missing_required_managed_tool_is_yellow() {
        // Mirror of the above: under LocalIterate, a missing required
        // managed tool is recorded as Skipped (Yellow) so the
        // inner-loop mode does not block.
        let plan = vec![GateStep {
            name: "secrets".to_string(),
            category: StepCategory::Secret,
            command: vec!["gitleaks".to_string(), "detect".to_string()],
            tool: "gitleaks".to_string(),
            required: true,
        }];
        let env = zoder_core::gate_bundle::PathEnv::new();
        if env.find_binary("gitleaks").is_some() {
            // Same skip-if-installed posture as the strict test: we
            // cannot assert on a path-dependent outcome if the tool
            // is on this box.
            return;
        }
        let (results, status) = zoder_core::gate::run_plan(&plan, GateMode::LocalIterate, &env);
        assert_eq!(
            status,
            GateStatus::Yellow {
                skipped: vec!["secrets".to_string()],
            },
        );
        assert!(matches!(results[0].outcome, StepOutcome::Skipped { .. }));
    }

    #[test]
    fn outcome_with_no_ecosystems_still_returns_a_usable_handle() {
        // Defensive: an empty-repo run must still produce a
        // GateOutcome that can be queried without panicking. The
        // probe is empty, the plan is empty, signals are empty, but
        // the report still renders an honest Green.
        let tmp = tempfile::tempdir().expect("tempdir");
        let outcome = run_gate_for_root(tmp.path());
        assert_eq!(outcome.plan.len(), 0);
        assert_eq!(outcome.pre_run_compat.added_baseline.len(), 0);
        let report = outcome.run(&GateMode::Strict);
        assert!(report.is_passed());
        assert!(!report.is_failed());
    }

    #[test]
    fn discover_markers_under_orchestrator_is_deterministic() {
        // Two parallel orchestrator runs over the same dir must
        // produce identical plans + probes (the gate is reproducible).
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("write");
        std::fs::write(tmp.path().join("package.json"), "{}\n").expect("write");
        let a = run_gate_for_root(tmp.path());
        let b = run_gate_for_root(tmp.path());
        let names_a: Vec<&str> = a.plan.iter().map(|s| s.name.as_str()).collect();
        let names_b: Vec<&str> = b.plan.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names_a, names_b);
    }

    #[test]
    fn expand_tilde_handles_bare_and_subpath() {
        // The minimal hand-rolled `~` expansion used by `--root`.
        // We can't easily test the actual filesystem result without
        // knowing the user's $HOME, so we exercise the negative
        // cases here and let the positive path rely on integration
        // testing.
        let r = super::expand_tilde("/nonexistent/path");
        assert_eq!(r, Path::new("/nonexistent/path"));
        // "~user" (user-named tilde) is intentionally NOT expanded;
        // only the bare `~` and `~/sub` forms are supported. Make
        // sure that boundary is honored.
        let r = super::expand_tilde("~user/foo");
        assert_eq!(r, Path::new("~user/foo"));
    }
}

#[cfg(test)]
mod health_install_tests {
    //! Unit tests for `zoder health install-daily` / `uninstall-daily`.
    //!
    //! The install path is OS-dependent (macOS launchd plist vs Linux
    //! systemd user timer) so each test detects the current target via
    //! [`super::current_backend`] and asserts on the right artefact.
    //! Everything runs against a `tempfile::tempdir()` masquerading as
    //! `$ZODER_HOME` so nothing touches the real LaunchAgents / systemd
    //! dirs. A process-wide mutex serializes the env-mutating tests so
    //! parallel runs of `cargo test` can't trample each other's
    //! `ZODER_HOME`.

    use super::*;
    use std::path::Path;
    use std::sync::Mutex;

    /// Process-wide mutex around all tests that mutate `ZODER_HOME`. The
    /// env var is process-global so the tests MUST be serialized.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn fake_home() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// Run `f` with `ZODER_HOME` pointed at `home`, restoring the prior
    /// value (or unsetting it) on the way out. The `ENV_LOCK` mutex makes
    /// the read-modify-write atomic w.r.t. other tests in the same
    /// process.
    fn with_fake_home<F: FnOnce(&Path)>(home: &Path, f: F) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("ZODER_HOME").ok();
        std::env::set_var("ZODER_HOME", home);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(home)));
        match prev {
            Some(v) => std::env::set_var("ZODER_HOME", v),
            None => std::env::remove_var("ZODER_HOME"),
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn install_paths_match_expected_layout_for_current_os() {
        // The path layout is part of the platform contract: macOS expects
        // a single plist under ~/Library/LaunchAgents; Linux expects a
        // service+timer pair under ~/.config/systemd/user. Anything else
        // would silently fail to load with launchctl / systemctl.
        let home = fake_home();
        let paths = install_daily_job_paths(home.path());
        match current_backend() {
            DailyBackend::Launchd => {
                assert_eq!(paths.written.len(), 1, "macOS: one plist");
                let p = &paths.written[0];
                assert!(p.ends_with("Library/LaunchAgents/dev.ncz.zoder-health.plist"));
                assert_eq!(paths.load.len(), 1);
                assert!(paths.load[0].contains("launchctl load"));
            }
            DailyBackend::SystemdUser => {
                assert_eq!(paths.written.len(), 2, "linux: service + timer");
                assert!(paths.written[0].ends_with(".config/systemd/user/zoder-health.service"));
                assert!(paths.written[1].ends_with(".config/systemd/user/zoder-health.timer"));
                assert!(paths.load[0].contains("systemctl"));
            }
        }
    }

    #[test]
    fn launchd_plist_is_well_formed_xml_and_runs_health_probe_all() {
        if current_backend() != DailyBackend::Launchd {
            // Plist renderer is macOS-only by definition; skip on Linux so
            // the suite doesn't depend on the host OS.
            return;
        }
        let bin = Path::new("/usr/local/bin/zoder");
        let body = render_launchd_plist(bin);
        // Well-formed: opens with the XML declaration + plist doctype, has
        // a single root <plist><dict>…</dict></plist>.
        assert!(body.starts_with("<?xml"));
        assert!(body.contains("<!DOCTYPE plist"));
        assert_eq!(body.matches("<plist").count(), 1);
        assert_eq!(body.matches("</plist>").count(), 1);
        // Runs the daily probe: Label, ProgramArguments must reference
        // the binary and the health --probe --all subcommand.
        assert!(body.contains("<string>dev.ncz.zoder-health</string>"));
        assert!(body.contains(bin.to_str().unwrap()));
        assert!(body.contains("<string>health</string>"));
        assert!(body.contains("<string>--probe</string>"));
        assert!(body.contains("<string>--all</string>"));
        // Daily cadence: StartCalendarInterval pinned to 09:00.
        assert!(body.contains("<key>Hour</key>"));
        assert!(body.contains("<integer>9</integer>"));
        assert!(body.contains("<key>Minute</key>"));
        assert!(body.contains("<integer>0</integer>"));
    }

    #[test]
    fn systemd_units_are_well_formed_and_run_health_probe_all() {
        if current_backend() != DailyBackend::SystemdUser {
            return;
        }
        let bin = Path::new("/usr/local/bin/zoder");
        let svc = render_systemd_service(bin);
        let timer = render_systemd_timer();
        // Service is Type=oneshot and execs the probe.
        assert!(svc.contains("Type=oneshot"));
        assert!(svc.contains("ExecStart=/usr/local/bin/zoder health --probe --all"));
        // Timer has a daily OnCalendar and references the service.
        assert!(timer.contains("OnCalendar=*-*-* 09:00:00"));
        assert!(timer.contains("Unit=zoder-health.service"));
        assert!(timer.contains("[Install]"));
    }

    #[test]
    fn install_then_uninstall_round_trip_writes_then_removes_files() {
        let tmp = fake_home();
        with_fake_home(tmp.path(), |home| {
            let bin = Path::new("/usr/local/bin/zoder");

            // Sanity: nothing exists before install.
            let paths = install_daily_job_paths(home);
            for p in &paths.written {
                assert!(!p.exists(), "fixture must start clean: {}", p.display());
            }

            let install_msg = install_daily_job(bin).expect("install");
            assert!(
                install_msg.contains("installed daily health sweep"),
                "install summary must say so: {install_msg}"
            );
            for p in &paths.written {
                assert!(p.exists(), "install must write {}", p.display());
            }
            // The written plist / service must contain the binary path.
            let first = std::fs::read_to_string(&paths.written[0]).unwrap();
            assert!(first.contains("/usr/local/bin/zoder"));

            // Idempotent uninstall: removes every file, returns a summary.
            let uninstall_msg = uninstall_daily_job().expect("uninstall");
            assert!(
                uninstall_msg.contains("removed"),
                "summary: {uninstall_msg}"
            );
            for p in &paths.written {
                assert!(!p.exists(), "uninstall must remove {}", p.display());
            }
            // Re-running uninstall must be a no-op (no error, says so).
            let again = uninstall_daily_job().expect("uninstall again");
            assert!(
                again.contains("not installed"),
                "idempotent summary: {again}"
            );
        });
    }

    #[test]
    fn install_is_idempotent_overwriting_existing_job() {
        let tmp = fake_home();
        with_fake_home(tmp.path(), |_home| {
            let bin_a = Path::new("/usr/local/bin/zoder-v1");
            let bin_b = Path::new("/usr/local/bin/zoder-v2");
            install_daily_job(bin_a).expect("install v1");
            install_daily_job(bin_b).expect("install v2 (overwrite)");
            let paths = install_daily_job_paths(_home);
            let body = std::fs::read_to_string(&paths.written[0]).unwrap();
            // Re-installing with a new binary path replaces the previous
            // content — no half-state, no leftovers.
            assert!(
                body.contains("zoder-v2"),
                "second install overwrites first: {body}"
            );
            assert!(!body.contains("zoder-v1"));
            // Cleanup so we don't leave artefacts behind in case the env var
            // happened to point at a real dir on a dev box.
            uninstall_daily_job().ok();
        });
    }
}
