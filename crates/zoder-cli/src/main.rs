//! zoder CLI - codex-compatible surface + cost-aware routing extensions.

use std::io::{IsTerminal, Read, Write};

mod codex;
mod goose;

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use zoder_core::{
    amortized_per_call, anthropic_costs, backoff_delay, build_report, build_report_from_entries,
    estimate_tokens, fetch_engine_cost, finops_cli, openai_costs, plan_usage, run_agent,
    sync_catalog, AgentEvent, AgentOptions, ApprovalPolicy, BillingMode, BudgetVerdict,
    ChatRequest, ChatResult, Config, Corpus, CostSnapshot, Decision, Entry, Gran, HealthStore,
    Ledger, Message, ModelEntry, OpenAiProvider, Period, PolicyGate, PricingCatalog, PricingSource,
    ProviderError, Router, ScopeStat, Session, State, Theme, Tier,
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
    if let Some(dir) = codex::active_job_dir() {
        codex::finalize_job(&dir, res.is_ok());
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
        Some(Cmd::Health { probe }) => cmd_health(&cli, *probe).await,
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
            codex::cmd_review(
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
            codex::cmd_review(
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
        Some(Cmd::Rescue { task, background }) => codex::cmd_rescue(&cli, task, *background).await,
        Some(Cmd::Status { job, all }) => codex::cmd_status(&cli, job.clone(), *all),
        Some(Cmd::Result { job }) => codex::cmd_result(&cli, job.clone()),
        Some(Cmd::Cancel { job }) => codex::cmd_cancel(&cli, job.clone()),
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
            codex::cmd_loop(
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
            )
            .await
        }
        Some(Cmd::Transfer) => codex::cmd_transfer(&cli).await,
        Some(Cmd::Session { args }) => cmd_tui(args),
        Some(Cmd::Run {
            text,
            instructions,
            background,
        }) => goose::cmd_run(&cli, text.clone(), instructions.clone(), *background).await,
        Some(Cmd::Recipe { action }) => goose::cmd_recipe(&cli, action).await,
        Some(Cmd::Mcp { action }) => goose::cmd_mcp(&cli, action),
        Some(Cmd::Configure { edit, validate }) => goose::cmd_configure(*edit, *validate),
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
fn resolve_chain(
    cli: &Cli,
    eng: &Engine,
    health: &HealthStore,
) -> anyhow::Result<(Vec<String>, String)> {
    if let Some(m) = &cli.model {
        return Ok((vec![m.clone()], format!("explicit model {m}")));
    }
    let router = Router::new(&eng.corpus, health);
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
    let router = Router::new(&eng.corpus, &health);
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

    let provider_cfg = eng
        .cfg
        .provider(&eng.cfg.default_provider)
        .ok_or_else(|| anyhow::anyhow!("default provider not configured"))?;

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
    // requires confirmation even when the model id is classified free.
    let provider_paid = eng
        .cfg
        .provider(&eng.cfg.default_provider)
        .map(|p| p.paid || p.billing != BillingMode::Free)
        .unwrap_or(false);
    if let Decision::NeedConfirm(msg) = gate.check(&primary_entry, provider_paid) {
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

    let provider = OpenAiProvider::new(provider_cfg)?;

    // Walk the chain: each model gets `--retries` transient retries; on a clean
    // (no output emitted) failure we fall back to the next model.
    let started = std::time::Instant::now();
    let mut used_model = String::new();
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
        match try_model(&provider, &req, cli.json, cli.retries, cli.quiet).await {
            Ok(res) => {
                // Defer the winning model's health recording until after the
                // policy verify below, so a policy-violating "success" is
                // recorded as a single failure (not success + failure).
                used_model = model_id.clone();
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
        provider: provider_cfg.id.clone(),
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
    // Default coding agent.
    "deepseek-v4-pro".to_string()
}

/// Ensure a zeroclaw agent daemon is reachable; spawn an ephemeral one (using
/// the co-shipped `zeroclaw` binary) if the socket is absent. Returns the socket.
async fn ensure_engine_daemon() -> anyhow::Result<std::path::PathBuf> {
    let socket = engine_socket_path();
    // Readiness is an `initialize` handshake, not a bare connect: a socket that
    // accepts but never answers is NOT a usable engine.
    if zoder_core::probe_ready(&socket, std::time::Duration::from_secs(5))
        .await
        .is_ok()
    {
        return Ok(socket);
    }
    // NOTE: we deliberately do NOT unlink a stale socket here. A failed connect
    // only proves "no listener at this instant"; another zoder/daemon could bind
    // between the check and the unlink (TOCTOU) and we'd delete a live socket.
    // The daemon owns its own stale-socket cleanup on bind.
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
    // Capture daemon stderr to a log instead of discarding it, so a startup
    // failure (bad config, port/socket clash, missing provider key) is
    // diagnosable rather than a silent "not ready" timeout.
    let log_path = zeroclaw_data_dir().join("daemon.log");
    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("daemon")
        .arg("--ephemeral")
        .arg("--config-dir")
        .arg(&config_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => {
            cmd.stderr(std::process::Stdio::from(f));
        }
        Err(_) => {
            cmd.stderr(std::process::Stdio::null());
        }
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn zeroclaw daemon ({}): {e}", bin.display()))?;

    // Poll readiness with the initialize handshake, but bail early if the child
    // process exits during startup (otherwise we'd wait the full budget for a
    // socket that will never appear). On failure, surface the daemon log tail.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!(
                "zeroclaw daemon exited during startup ({status}); log tail:\n{}",
                read_log_tail(&log_path)
            );
        }
        // Short per-probe budget so a socket that accepts but never answers
        // can't block this poll for the full setup timeout — the deadline and
        // the child-exit check stay responsive.
        if zoder_core::probe_ready(&socket, std::time::Duration::from_secs(2))
            .await
            .is_ok()
        {
            return Ok(socket);
        }
        if std::time::Instant::now() >= deadline {
            // Kill AND reap so we don't leave a zombie.
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "zeroclaw daemon not ready within 20s; log tail:\n{}",
                read_log_tail(&log_path)
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Best-effort: return the last ~1.5KB of the daemon log for error context.
/// Tails on a byte boundary safely (logs can contain multibyte UTF-8).
fn read_log_tail(path: &std::path::Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let start = bytes.len().saturating_sub(1500);
            String::from_utf8_lossy(&bytes[start..]).trim().to_string()
        }
        Err(_) => "(no daemon log available)".to_string(),
    }
}

/// Authoritative per-run cost/tokens from the engine's cost tracker, scoped to
/// `[from, now)` and the agent alias.
///
/// Returns `None` when the cost query itself FAILS (the engine is unreachable or
/// errors) — distinct from a successful query that returns an empty window
/// ($0, e.g. a genuinely free local model). The caller MUST treat `None` as
/// "cost/model attribution unavailable" and fail closed under the free policy,
/// rather than silently booking the run as $0 on the primary (which let a paid
/// alias/fallback escape the paid gate — the original fail-open bug).
///
/// The query is retried a few times with a short backoff to ride out the
/// engine's cost-tracker flush lag right after a turn completes.
async fn agentic_cost(
    socket: &std::path::Path,
    from: chrono::DateTime<chrono::Utc>,
    alias: &str,
    fallback_model: &str,
) -> Option<(f64, u64, u64, String)> {
    let mut last_err = None;
    for attempt in 0..3u32 {
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
                return Some((cost, tin, tout, model));
            }
            Err(e) => {
                last_err = Some(e);
                if attempt < 2 {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        300 * (attempt as u64 + 1),
                    ))
                    .await;
                }
            }
        }
    }
    tracing::warn!(
        ?last_err,
        "engine cost/query failed; cost attribution unavailable"
    );
    None
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
    prompt: String,
    session_override: Option<String>,
    stream_output: bool,
) -> anyhow::Result<TurnResult> {
    let eng = Engine::load()?;
    let mut health = HealthStore::load(&eng.cfg.health_path);

    // Resolve the model (routing or -m) for alias selection + paid gate.
    let (chain, reason) = resolve_chain(cli, &eng, &health)?;
    // Dedupe (order-preserving): the fallback loop skips candidates by value, so
    // a duplicate model id in the chain could otherwise be re-selected after
    // being skipped. A unique chain makes the skip/advance logic unambiguous.
    let chain: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        chain
            .into_iter()
            .filter(|m| seen.insert(m.clone()))
            .collect()
    };
    if chain.is_empty() {
        anyhow::bail!("no model resolved");
    }
    if cli.explain {
        eprintln!("[route] {reason}");
    }

    let strict_free = (eng.cfg.strict_free && !cli.lenient_telemetry) || cli.require_free;
    let gate = PolicyGate::new(&eng.cfg, cli.allow_paid, strict_free);
    // A paid/metered serving provider (e.g. an org overlay's default route)
    // requires confirmation even when the model id is classified free.
    let provider_paid = eng
        .cfg
        .provider(&eng.cfg.default_provider)
        .map(|p| p.paid || p.billing != BillingMode::Free)
        .unwrap_or(false);

    let cwd = agentic_cwd(cli)?;
    let socket = ensure_engine_daemon().await?;

    let started = std::time::Instant::now();
    let start_ts = chrono::Utc::now();

    // Candidates skipped because they failed the gate or a pre-side-effect run;
    // the loop always advances to the first chain model not in this set.
    let mut skipped_models: Vec<String> = Vec::new();

    // Walk the routed chain (primary + fallbacks). The agentic engine applies
    // file edits and runs tools as the turn proceeds, so a turn that has STARTED
    // can never be safely re-run on another model. Only a PRE-side-effect
    // failure (connect / initialize / session setup / prompt-send / an immediate
    // prompt error) is eligible for fallback: run_agent returns Err exactly in
    // those pre-streaming cases, while once streaming begins it returns
    // Ok(AgentRun) with a non-success outcome (which we keep, no fallback). This
    // lifts agentic reliability from the primary's success rate toward the
    // chain's, without ever duplicating side effects. Each candidate is
    // gated independently; only the last candidate failing the gate is fatal.
    let (run, used_primary, used_alias) = loop {
        let idx = chain
            .iter()
            .position(|m| !skipped_models.contains(m))
            .unwrap_or(chain.len() - 1);
        let model = chain[idx].clone();
        let last = chain[idx + 1..].iter().all(|m| skipped_models.contains(m));
        let entry = eng
            .corpus
            .get(&model)
            .cloned()
            .unwrap_or_else(|| ModelEntry {
                id: model.clone(),
                gated_reason: Some("unknown model: not in corpus, cannot verify free".into()),
                ..Default::default()
            });
        if cli.require_free && !entry.free {
            if last {
                anyhow::bail!(
                    "--require-free set but no chain model is a known free model (last tried {model})"
                );
            }
            skipped_models.push(model);
            continue;
        }
        if let Decision::NeedConfirm(msg) = gate.check(&entry, provider_paid) {
            if !confirm_paid(&msg)? {
                if last {
                    anyhow::bail!("paid model use declined");
                }
                skipped_models.push(model);
                continue;
            }
        }
        let alias = resolve_agent_alias(cli, &model);
        let opts = AgentOptions {
            socket: socket.clone(),
            agent_alias: alias.clone(),
            cwd: cwd.clone(),
            prompt: prompt.clone(),
            model_override: None,
            session_id: session_override.clone().or_else(|| cli.session.clone()),
            show_reasoning: cli.show_reasoning,
            approval: parse_approval(cli),
            timeout: std::time::Duration::from_secs(cli.agent_timeout.unwrap_or(900)),
        };
        match run_agent(&opts, |ev| {
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
        .await
        {
            Ok(r) => break (r, model, alias),
            Err(e) => {
                // Pre-side-effect failure: feed the breaker so a broken
                // alias/model is not selected again, then fall back to the next
                // chain model if one remains.
                health.record_failure(&model, &format!("agentic setup/turn failed: {e}"));
                if last {
                    save_health(&health);
                    return Err(e);
                }
                if stream_output && !cli.quiet {
                    eprintln!("[zoder] {model} failed before any output; falling back");
                }
                skipped_models.push(model);
                continue;
            }
        }
    };

    let elapsed_ms = started.elapsed().as_millis() as f64;

    let socket2 = engine_socket_path();
    // `None` => the engine cost query failed: we CANNOT prove what (or how
    // much) ran. Fail closed under the free policy instead of booking $0.
    let cost_obs = agentic_cost(&socket2, start_ts, &used_alias, &used_primary).await;
    let attribution_failed = cost_obs.is_none();
    let (cost, tokens_in, tokens_out, model_used) =
        cost_obs.unwrap_or_else(|| (0.0, 0, 0, used_primary.clone()));

    // Post-verify: the engine (via the agent alias) may have run a different —
    // possibly paid — model than the one pre-gated above (the daemon resolves
    // the alias). If it billed real money, the engine-reported model is a known
    // paid model, OR we could not verify cost/model at all, record a policy
    // violation rather than marking the ledger clean (default-deny under the
    // free guard).
    let model_used_paid = eng
        .corpus
        .get(&model_used)
        .map(|m| !m.free)
        .unwrap_or(false);
    let violation = if !cli.allow_paid && (cost > 0.0 || model_used_paid || attribution_failed) {
        let v = if attribution_failed {
            format!(
                "agentic run (alias {used_alias}) cost/model attribution unavailable \
                 (engine cost/query failed); cannot prove the run was free without --allow-paid"
            )
        } else {
            format!(
                "agentic run (alias {used_alias}) billed ${cost:.4} on engine model '{model_used}' \
                 (corpus_paid={model_used_paid}) without --allow-paid"
            )
        };
        eprintln!("zoder: POLICY VIOLATION: {v}");
        Some(v)
    } else {
        None
    };
    // Capture before `violation` is moved into the ledger Entry below; the
    // health block further down needs to know whether the run violated policy.
    let violated = violation.is_some();

    let ledger = Ledger::new(&eng.cfg.ledger_path);
    if let Err(e) = ledger.record(&Entry {
        ts_utc: chrono::Utc::now(),
        provider: eng.cfg.default_provider.clone(),
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
    // finish in budget (the BUG2 routing follow-up consumes this signal). A
    // policy violation (paid model billed without --allow-paid) is ALSO a
    // failure for routing purposes even when the turn "completed": recording it
    // as success would teach the router to keep picking a model the policy gate
    // just rejected (and would mask the violation in the health view). Mirror
    // the oneshot path, which records the winning model's health only after the
    // policy verify passes.
    if run.succeeded() && !violated {
        health.record_success(&used_primary, elapsed_ms);
    } else if violated {
        health.record_failure(
            &used_primary,
            "policy violation: paid model without --allow-paid",
        );
    } else {
        health.record_failure(
            &used_primary,
            &format!("turn did not complete: {}", run.outcome),
        );
    }
    save_health(&health);

    Ok(TurnResult {
        run,
        model: model_used,
        alias: used_alias,
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

    let prompt = read_prompt(prompt)?;
    let t = agentic_turn(cli, prompt, None, !cli.json).await?;

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

async fn cmd_health(cli: &Cli, probe: bool) -> anyhow::Result<()> {
    let eng = Engine::load()?;
    let mut health = HealthStore::load(&eng.cfg.health_path);

    if probe {
        let provider_cfg = eng
            .cfg
            .provider(&eng.cfg.default_provider)
            .ok_or_else(|| anyhow::anyhow!("default provider not configured"))?;
        // Money-safety: even a "free" model id costs real money when the default
        // provider is metered. Refuse to probe through a paid/metered provider
        // without an explicit opt-in (previously the probe spent untracked money
        // and reported the model "healthy").
        let provider_paid = provider_cfg.paid || provider_cfg.billing != BillingMode::Free;
        if provider_paid && !cli.allow_paid {
            anyhow::bail!(
                "health --probe would send requests through metered provider '{}' (could incur \
                 cost); pass --allow-paid to probe anyway",
                provider_cfg.id
            );
        }
        let strict_free = (eng.cfg.strict_free && !cli.lenient_telemetry) || cli.require_free;
        let gate = PolicyGate::new(&eng.cfg, cli.allow_paid, strict_free);
        let ledger = Ledger::new(&eng.cfg.ledger_path);
        let pricing = PricingCatalog::load(&Config::home().join("pricing.json"));
        let provider = OpenAiProvider::new(provider_cfg)?;
        let targets: Vec<String> = eng.corpus.free_chat().map(|m| m.id.clone()).collect();
        if !cli.quiet {
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
                Ok(res) => {
                    let ms = t.elapsed().as_millis() as f64;
                    // Post-verify the probe was actually served free; a metered
                    // fallback makes a "free" id cost money.
                    let entry = eng.corpus.get(id).cloned().unwrap_or_default();
                    let violation = gate.verify_free(&entry, &res.telemetry).err();
                    let tin = res.prompt_tokens.unwrap_or(0);
                    let tout = res.completion_tokens.unwrap_or(res.tokens_out);
                    let cost = res
                        .telemetry
                        .cost_usd
                        .unwrap_or_else(|| pricing.cost(id, tin, tout));
                    // Ledger any nonzero-cost or policy-violating probe so spend
                    // is never invisible. Under a metered provider, record EVERY
                    // probe (even a $0-priced one): a missing cost header on a
                    // paid provider must not silently drop the row.
                    if cost > 0.0 || violation.is_some() || provider_paid {
                        if let Err(e) = ledger.record(&Entry {
                            ts_utc: chrono::Utc::now(),
                            provider: eng.cfg.default_provider.clone(),
                            model: id.clone(),
                            host: zoder_core::ledger::host_of_model(id),
                            tokens_in: tin,
                            tokens_out: tout,
                            cost_usd: cost,
                            calls: 1,
                            violation: violation.clone(),
                        }) {
                            eprintln!(
                                "zoder: warning: failed to record probe ledger entry for {id}: {e}"
                            );
                        }
                    }
                    if let Some(v) = &violation {
                        health.record_failure(id, &format!("probe policy violation: {v}"));
                        if !cli.quiet {
                            eprintln!("  PAID {id}  {v}");
                        }
                    } else {
                        health.record_success(id, ms);
                        if !cli.quiet {
                            eprintln!("  ok   {id}  {ms:.0}ms");
                        }
                    }
                }
                Err(e) => {
                    health.record_failure(id, &e.message);
                    if !cli.quiet {
                        eprintln!("  FAIL {id}  {}", e.message);
                    }
                }
            }
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

async fn cmd_refresh(cli: &Cli) -> anyhow::Result<()> {
    let eng = Engine::load()?;
    let provider_cfg = eng
        .cfg
        .provider(&eng.cfg.default_provider)
        .ok_or_else(|| anyhow::anyhow!("default provider not configured"))?;
    let provider = OpenAiProvider::new(provider_cfg)?;
    let served = provider.list_models().await.map_err(|e| {
        anyhow::anyhow!(
            "could not list models from {}: {}",
            provider_cfg.id,
            e.message
        )
    })?;

    let mut corpus = eng.corpus;
    let report = corpus.reconcile(&served);
    corpus.save(&eng.cfg.corpus_path)?;

    if cli.json {
        println!(
            "{}",
            serde_json::json!({
                "served": served.len(),
                "added": report.added,
                "retired": report.retired,
                "kept": report.kept,
                "total": corpus.models.len(),
            })
        );
    } else {
        println!(
            "refreshed: {} served, {} new, {} retired, {} kept ({} total)",
            served.len(),
            report.added.len(),
            report.retired.len(),
            report.kept,
            corpus.models.len()
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
    if json {
        // Redacted view: never serialize inline bearer tokens (the raw config
        // carries `Auth::Bearer { token }`). Env-var names are safe to show.
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
    let entries = Ledger::new(&eng.cfg.ledger_path)
        .entries()
        .unwrap_or_default();
    for p in &eng.cfg.providers {
        let auth = p.auth.resolve().map(|_| "ok").unwrap_or("MISSING");
        println!(
            "{:10} {:42} kind={:12} billing={:12} auth={}",
            p.id,
            p.base_url,
            p.kind,
            billing_label(p.billing),
            auth
        );
        if let (BillingMode::Subscription, Some(plan)) = (p.billing, &p.subscription) {
            for w in plan_usage(&entries, &p.id, plan) {
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
                println!(
                    "             {:>7} window: {:.0}/{:.0} {} ({:.0}% of cap){}{}",
                    w.name,
                    w.used,
                    w.cap,
                    w.unit,
                    w.pct * 100.0,
                    reset,
                    warn
                );
            }
            let amort = amortized_per_call(&entries, &p.id, plan);
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
