//! CI-parity gate engine: pure planning core.
//!
//! This module is the **gate engine** (see `docs/CI-PARITY-GATE.md` for the
//! design of record). It is intentionally pure — no subprocess execution
//! and no real CI-file YAML parsing here, those belong to later wiring
//! slices. What lives here is the data model, the planning layer, the
//! runner core, and the honest-degradation reporting.
//!
//! Slice status:
//!  - Slice 1 — gate-planning core: `Ecosystem`, `GateStep`, detection,
//!    `baseline_plan`, `GateStatus`, `aggregate`.             ✅
//!  - Slice 2 — gate runner core: `GateMode`, `GateEnv`, `StepExec`,
//!    `run_plan`.                                              ✅
//!  - Slice 3 — CI-derivation classifier: `CiJob`,
//!    `CompatibilityReport`, `derive_plan`.                    ✅
//!  - Slice 4 — language/framework detectors beyond Rust + the
//!    `GateReport` honest-degradation renderer.                ✅
//!  - Slice 5 — managed tool bundle (`gate_bundle::default_bundle` +
//!    `PathEnv`/`ToolLookup`/`InstallHint`) + `zoder gate` CLI
//!    wiring (`crates/zoder-cli/src/main.rs::cmd_gate`).        ✅
//!
//! Everything is deterministic and unit-tested so the planning + reporting
//! layer can be reasoned about without spinning a process.
//!
//! The fail-closed posture is **load-bearing**: aggregate / run / report
//! must never silently pass a missing required tool, and the report
//! renderer must always surface the runnable / skipped / added-baseline
//! breakdown so the claim "it passed the gate" stays honest.

/// Detected project ecosystem (from marker files at the repo root).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ecosystem {
    Rust,
    Node,
    Python,
    Go,
}

/// What kind of check a step is (used for reporting + which are safety-critical).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepCategory {
    Format,
    Lint,
    Build,
    Test,
    Security,
    License,
    Secret,
    Commit,
}

/// One gate step: a command to run, the tool it needs, and whether it is
/// REQUIRED (required steps gate the verdict; optional steps are advisory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateStep {
    pub name: String,
    pub category: StepCategory,
    pub command: Vec<String>, // argv, e.g. ["cargo","fmt","--all","--","--check"]
    pub tool: String,         // the binary the step needs, e.g. "cargo" or "cargo-deny"
    pub required: bool,
}

/// Outcome of running a step (execution happens in a later slice; this models it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    Passed,
    Failed,
    /// e.g. tool missing, or not runnable locally
    Skipped {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepResult {
    pub step_name: String,
    pub required: bool,
    pub outcome: StepOutcome,
}

/// Honest degradation status for the whole gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateStatus {
    /// all required passed, nothing skipped
    Green,
    /// all required passed, but some steps skipped
    Yellow { skipped: Vec<String> },
    /// at least one REQUIRED step failed
    Red { failures: Vec<String> },
    /// Zero required steps ran (empty plan, all-optional plan, or a plan
    /// whose required steps were all absent for some other reason).
    /// The gate CANNOT certify "passed" — there was no required check to
    /// actually run. Adversarial-review pin: the previous behavior
    /// aggregated this to `Green`, letting an autonomous agent pass
    /// the gate without doing any work. `is_passed()` returns false.
    Inconclusive,
}

impl Ecosystem {
    /// The marker filename (at the repo root) that identifies this ecosystem.
    /// Returning `Option<&'static str>` (rather than panicking) keeps this
    /// pure and `unwrap`-free.
    fn marker(self) -> &'static str {
        match self {
            Ecosystem::Rust => "Cargo.toml",
            Ecosystem::Node => "package.json",
            Ecosystem::Python => "pyproject.toml",
            Ecosystem::Go => "go.mod",
        }
    }
}

/// Detect ecosystems from a list of repo-root marker filenames present.
/// `Cargo.toml` -> Rust; `package.json` -> Node; `pyproject.toml` OR
/// `requirements.txt` OR `setup.py` -> Python; `go.mod` -> Go. Returns
/// each matched ecosystem ONCE, in the fixed order Rust, Node, Python,
/// Go (deterministic). Empty if none match.
pub fn detect_ecosystems(marker_files: &[&str]) -> Vec<Ecosystem> {
    // Fixed iteration order -> deterministic output regardless of input order.
    const ORDER: [Ecosystem; 4] = [
        Ecosystem::Rust,
        Ecosystem::Node,
        Ecosystem::Python,
        Ecosystem::Go,
    ];

    ORDER
        .into_iter()
        .filter(|eco| ecosystem_matches(*eco, marker_files))
        .collect()
}

fn ecosystem_matches(eco: Ecosystem, marker_files: &[&str]) -> bool {
    match eco {
        // Three Python markers collapse to a single ecosystem match.
        Ecosystem::Python => ["pyproject.toml", "requirements.txt", "setup.py"]
            .iter()
            .any(|m| marker_files.contains(m)),
        _ => marker_files.iter().any(|f| *f == eco.marker()),
    }
}

/// The baseline OSS-hygiene plan for one ecosystem (applies even when a repo
/// has no CI config). Includes per ecosystem: Format, Lint, Build, Test, and
/// at least one Security step. See the module docs / spec for the exact
/// commands.
pub fn baseline_plan(eco: Ecosystem) -> Vec<GateStep> {
    match eco {
        Ecosystem::Rust => vec![
            step(
                "fmt",
                StepCategory::Format,
                vec_strs(&["cargo", "fmt", "--all", "--", "--check"]),
                "cargo",
                true,
            ),
            step(
                "clippy",
                StepCategory::Lint,
                vec_strs(&["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]),
                "cargo",
                true,
            ),
            step(
                "build",
                StepCategory::Build,
                vec_strs(&["cargo", "build", "--all-targets"]),
                "cargo",
                true,
            ),
            step(
                "test",
                StepCategory::Test,
                vec_strs(&["cargo", "test", "--all-targets"]),
                "cargo",
                true,
            ),
            step(
                "deny",
                StepCategory::Security,
                vec_strs(&["cargo", "deny", "check"]),
                "cargo-deny",
                true,
            ),
            step(
                "audit",
                StepCategory::Security,
                vec_strs(&["cargo", "audit"]),
                "cargo-audit",
                false,
            ),
        ],
        Ecosystem::Node => vec![
            step(
                "fmt",
                StepCategory::Format,
                vec_strs(&["npx", "prettier", "--check", "."]),
                "npx",
                false,
            ),
            step(
                "lint",
                StepCategory::Lint,
                vec_strs(&["npm", "run", "lint"]),
                "npm",
                true,
            ),
            step(
                "build",
                StepCategory::Build,
                vec_strs(&["npm", "run", "build"]),
                "npm",
                true,
            ),
            step(
                "test",
                StepCategory::Test,
                vec_strs(&["npm", "test"]),
                "npm",
                true,
            ),
            step(
                "audit",
                StepCategory::Security,
                vec_strs(&["npm", "audit", "--audit-level=high"]),
                "npm",
                true,
            ),
        ],
        Ecosystem::Python => vec![
            step(
                "fmt",
                StepCategory::Format,
                vec_strs(&["ruff", "format", "--check", "."]),
                "ruff",
                false,
            ),
            step(
                "lint",
                StepCategory::Lint,
                vec_strs(&["ruff", "check", "."]),
                "ruff",
                true,
            ),
            step(
                "build",
                StepCategory::Build,
                vec_strs(&["python", "-m", "build"]),
                "python",
                false,
            ),
            step(
                "test",
                StepCategory::Test,
                vec_strs(&["pytest", "-q"]),
                "pytest",
                true,
            ),
            step(
                "audit",
                StepCategory::Security,
                vec_strs(&["pip-audit"]),
                "pip-audit",
                true,
            ),
        ],
        Ecosystem::Go => vec![
            step(
                "fmt",
                StepCategory::Format,
                vec_strs(&["gofmt", "-l", "."]),
                "gofmt",
                true,
            ),
            step(
                "lint",
                StepCategory::Lint,
                vec_strs(&["go", "vet", "./..."]),
                "go",
                true,
            ),
            step(
                "build",
                StepCategory::Build,
                vec_strs(&["go", "build", "./..."]),
                "go",
                true,
            ),
            step(
                "test",
                StepCategory::Test,
                vec_strs(&["go", "test", "./..."]),
                "go",
                true,
            ),
            step(
                "audit",
                StepCategory::Security,
                vec_strs(&["govulncheck", "./..."]),
                "govulncheck",
                true,
            ),
        ],
    }
}

/// One job extracted from a repo's own CI config (GitHub Actions / GitLab CI /
/// Woodpecker). A later slice parses YAML into these; this slice only classifies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiJob {
    pub name: String,
    pub command: Vec<String>, // argv
    pub tool: String,
    pub category: StepCategory,
    /// References CI secrets (e.g. GitHub `${{ secrets.* }}`) — not runnable locally.
    pub needs_secrets: bool,
    /// Needs service containers / external services (Postgres, etc.).
    pub needs_services: bool,
    /// Targets a self-hosted or custom runner we can't reproduce locally.
    pub self_hosted: bool,
}

/// Honest compatibility breakdown attached to every derived plan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompatibilityReport {
    /// CI job names that WILL run locally.
    pub runnable: Vec<String>,
    /// CI jobs that can't run locally, each with the reason + risk left unverified.
    pub skipped: Vec<(String, String)>,
    /// Baseline OSS-hygiene step names ADDED because the repo CI didn't cover them.
    pub added_baseline: Vec<String>,
}

/// Derive the gate plan from a repo's CI jobs unioned with the baseline hygiene plan.
/// Returns the runnable plan plus an honest compatibility report.
///
/// Rules:
///  - A CiJob that `needs_secrets` OR `needs_services` OR `self_hosted` is NOT
///    runnable locally: add it to `report.skipped` as `(name, reason)` and DO NOT
///    include it as a GateStep. Reason strings, checked in this precedence
///    (secrets > services > self_hosted):
///   - needs_secrets  => "requires CI secrets (upstream CI verifies)"
///   - needs_services => "requires service containers (upstream CI verifies)"
///   - self_hosted    => "requires a self-hosted runner (upstream CI verifies)"
///  - Otherwise the CiJob becomes a runnable required GateStep
///    (name/category/command/tool from the job, `required = true`) and its name is
///    added to `report.runnable`.
///  - Then union with `baseline`: baseline REQUIRED steps ALWAYS run, even if
///    CI already covers the same category — CI-derived steps are additive, never
///    a replacement for the baseline safety set. (Adversarial-review pin Z-7:
///    the previous behavior suppressed baseline REQUIRED steps when CI claimed
///    to cover the same category, letting a repo author commit a CI YAML whose
///    `test` job is `["true"]` and silently displace `cargo test`.) OPTIONAL
///    baseline steps keep their existing dedup rule: a category already covered
///    by the runnable CI steps is not duplicated.
///  - Plan order: runnable CI-derived steps first (input order), then added
///    baseline steps (baseline order). Report vectors follow the same input
///    order.
pub fn derive_plan(
    ci_jobs: &[CiJob],
    baseline: &[GateStep],
) -> (Vec<GateStep>, CompatibilityReport) {
    let mut plan: Vec<GateStep> = Vec::with_capacity(ci_jobs.len() + baseline.len());
    let mut report = CompatibilityReport::default();

    // Step 1: classify each CiJob in input order.
    for job in ci_jobs {
        if let Some(reason) = skip_reason(job) {
            report.skipped.push((job.name.clone(), reason));
            continue;
        }
        let step = GateStep {
            name: job.name.clone(),
            category: job.category,
            command: job.command.clone(),
            tool: job.tool.clone(),
            required: true,
        };
        report.runnable.push(job.name.clone());
        plan.push(step);
    }

    // Step 2: union baseline.
    //
    // Z-7 fix: REQUIRED baseline steps ALWAYS run, regardless of CI category
    // coverage. CI YAML is repo-controlled, and a malicious / low-effort CI
    // file (`test: ["true"]`) must NOT be allowed to silently displace the
    // real baseline safety check (e.g. `cargo test --all-targets`). The
    // CI-derived step is additive — it lives alongside the baseline.
    //
    // OPTIONAL baseline steps keep their original dedup-by-category
    // behavior: an optional check is advisory, and doubling it up when CI
    // already covers the category is noise without safety value.
    //
    // A skipped CI job leaves its category uncovered locally — that part of
    // the original behavior is unchanged (skipped CI does NOT count toward
    // "covered categories" for optional baseline suppression).
    let mut covered_optional: Vec<StepCategory> = plan.iter().map(|s| s.category).collect();

    for base in baseline {
        if base.required {
            // Required baseline: ALWAYS append. Category may already be
            // "covered" by a CI-derived step — that's the Z-7 case, and we
            // keep the baseline here so the real safety check runs.
            report.added_baseline.push(base.name.clone());
            plan.push(base.clone());
        } else if covered_optional.contains(&base.category) {
            // Optional baseline: skip if a runnable CI step already
            // covers this category. (A SKIPPED CI job is not in the plan,
            // so its category does not count as covered — honest
            // degradation preserved for the optional case.)
            continue;
        } else {
            covered_optional.push(base.category);
            report.added_baseline.push(base.name.clone());
            plan.push(base.clone());
        }
    }

    (plan, report)
}

/// Return the skip reason for a non-runnable job, honoring the precedence
/// `secrets > services > self_hosted`. Returns `None` when the job is runnable.
fn skip_reason(job: &CiJob) -> Option<String> {
    if job.needs_secrets {
        return Some("requires CI secrets (upstream CI verifies)".to_string());
    }
    if job.needs_services {
        return Some("requires service containers (upstream CI verifies)".to_string());
    }
    if job.self_hosted {
        return Some("requires a self-hosted runner (upstream CI verifies)".to_string());
    }
    None
}

/// Aggregate step results into the honest Green/Yellow/Red status:
///  - `Inconclusive` if NO required steps ran (empty plan, all-optional
///    plan, or a plan whose required steps were all absent). This is
///    load-bearing: "no required work to check" is NOT the same as
///    "all required work passed". Adversarial-review pin (Z-6): the
///    previous behavior aggregated this to `Green`, letting an
///    autonomous agent pass the gate without doing any work.
///  - Red if ANY required step outcome is Failed (failures = names of
///    the required steps that Failed, in input order).
///  - else Yellow if ANY step (required or optional) outcome is Skipped
///    (skipped = names of ALL skipped steps, in input order).
///  - else Green (a non-empty required set, all required Passed, nothing
///    Skipped anywhere).
///
/// Note: a FAILED *optional* step does NOT turn the gate Red or Yellow —
/// only required-Failed => Red, any-Skipped => Yellow.
pub fn aggregate(results: &[StepResult]) -> GateStatus {
    let mut required_count: usize = 0;
    let mut required_failures: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for r in results {
        match &r.outcome {
            StepOutcome::Failed => {
                if r.required {
                    required_count += 1;
                    required_failures.push(r.step_name.clone());
                }
                // optional Failed -> intentionally ignored (advisory).
            }
            StepOutcome::Skipped { .. } => {
                if r.required {
                    required_count += 1;
                }
                skipped.push(r.step_name.clone());
            }
            StepOutcome::Passed => {
                if r.required {
                    required_count += 1;
                }
            }
        }
    }

    // Z-6 fix: zero required steps means the gate cannot certify a
    // pass. "No work to check" is not "all work passed". Return
    // Inconclusive BEFORE the Red/Yellow/Green cascade so the existing
    // logic for honest plans (with at least one required step) is
    // unchanged: required_count > 0 below means the Inconclusive
    // branch never fires for honest plans.
    if required_count == 0 {
        return GateStatus::Inconclusive;
    }

    if !required_failures.is_empty() {
        // Red dominates Yellow, so we drop any skips we collected.
        return GateStatus::Red {
            failures: required_failures,
        };
    }

    if !skipped.is_empty() {
        return GateStatus::Yellow { skipped };
    }

    GateStatus::Green
}

// --- internal helpers (pure, no panic surface) -----------------------------

fn step(
    name: &str,
    category: StepCategory,
    command: Vec<String>,
    tool: &str,
    required: bool,
) -> GateStep {
    GateStep {
        name: name.to_string(),
        category,
        command,
        tool: tool.to_string(),
        required,
    }
}

fn vec_strs(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

/// Gate execution mode. Strict is the default, fail-closed posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateMode {
    /// Fail-closed: a missing REQUIRED tool is a failure (Red), not a silent skip.
    Strict,
    /// Fast inner-loop: a missing tool (required or not) is a recorded Skip.
    LocalIterate,
}

/// Result of executing one step's command (side-effecting execution lives behind
/// the `GateEnv` trait so the runner core stays pure and unit-testable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepExec {
    Passed,
    Failed,
}

/// The environment the runner needs, injected so tests can fake it (the real
/// impl — shelling out, checking `PATH` — lives in a later slice).
pub trait GateEnv {
    /// Is the step's `tool` available on this machine?
    fn tool_available(&self, tool: &str) -> bool;
    /// Run the step's command; report pass/fail.
    fn run_step(&self, step: &GateStep) -> StepExec;
}

/// Run a plan and produce per-step results plus the aggregated status.
///
/// Rules, per step:
///  - tool NOT available:
///      * Strict + step.required  => StepOutcome::Failed  (fail-closed; -> Red)
///      * Strict + !step.required => StepOutcome::Skipped { reason }  (advisory)
///      * LocalIterate (either)   => StepOutcome::Skipped { reason }
///        where `reason` = format!("tool `{}` not available", step.tool).
///  - tool available: run it; StepExec::Passed => Passed, StepExec::Failed => Failed.
///
/// Then call the existing `aggregate(&results)` for the GateStatus. Returns
/// `(results, status)` with results in plan order. Pure, `unwrap`-free.
pub fn run_plan(
    plan: &[GateStep],
    mode: GateMode,
    env: &dyn GateEnv,
) -> (Vec<StepResult>, GateStatus) {
    let mut results: Vec<StepResult> = Vec::with_capacity(plan.len());

    for step in plan {
        let outcome = if env.tool_available(&step.tool) {
            // Tool present: actually run it.
            match env.run_step(step) {
                StepExec::Passed => StepOutcome::Passed,
                StepExec::Failed => StepOutcome::Failed,
            }
        } else {
            // Tool missing: route by mode + required.
            let reason = format!("tool `{}` not available", step.tool);
            match (mode, step.required) {
                (GateMode::Strict, true) => StepOutcome::Failed, // fail-closed
                (GateMode::Strict, false) => StepOutcome::Skipped { reason },
                (GateMode::LocalIterate, _) => StepOutcome::Skipped { reason },
            }
        };

        results.push(StepResult {
            step_name: step.name.clone(),
            required: step.required,
            outcome,
        });
    }

    let status = aggregate(&results);
    (results, status)
}

// ============================================================================
// === SLICE 4 (a) — language / framework detectors BEYOND Rust + canonical ===
// === commands per language.                                              ===
// ============================================================================
//
// The original `detect_ecosystems` + `baseline_plan` cover Rust, Node, Python,
// Go at the ecosystem level. What this slice adds is:
//
//  1. Per-language explicit detectors (Rust / Node / Python / Go) that
//     canonicalize on MARKER files (the same set `detect_ecosystems` uses)
//     but expose them as named predicates so callers don't have to know the
//     marker convention.
//  2. A `PackageManager` enum + `detect_package_manager` so we can pick the
//     correct test/build command (yarn vs pnpm vs npm vs bun for Node;
//     poetry vs uv vs pip for Python). Pnpm/Yarn/Poetry/Uv are first-class
//     in the wild and silently defaulting to `npm`/`pip` ships wrong CI.
//  3. `baseline_plan_for(eco, marker_files)` — the marker-aware version
//     of `baseline_plan` that returns the canonical fmt/lint/test/build
//     commands for the EXACT package manager the project uses. Falls back
//     to `baseline_plan(eco)` when the package manager can't be inferred,
//     so this never weakens the existing plans.
//  4. `detect_frameworks(marker_files)` — lightweight, pure, no-JSON
//     framework-hint detector that returns strings like "typescript",
//     "react", "next.js", "vite", "django", "flask", "fastapi", "poetry",
//     "uv". Marker names drive the hints so the detector stays deterministic,
//     `unwrap`-free, and trivial to unit-test.
//
// Honest-degradation contract for this slice:
//  - Per-language detectors MUST be exact-string marker checks (no
//    substring fuzz). The existing detect contract forbids that, and
//    these detectors honor it.
//  - `baseline_plan_for` MUST return at least the same canonical fmt/lint/
//    test/build steps as `baseline_plan` (degrades gracefully); it only
//    ADDS package-manager refinement on top.
//  - A detected framework MUST never REPLACE the canonical step — only
//    flag an extra hint. Frameworks can have broken tooling; the canonical
//    command remains the safety floor.

/// A detected package manager (Node- or Python-side; Cargo/Go are
/// single-tool, so they don't get an enum variant). Anything not enumerated
/// here falls through to the ecosystem's default in `baseline_plan`.
///
/// This enum is deliberately scoped to package managers whose command line
/// differs meaningfully from the ecosystem default. We do not enumerate
/// `npm` as a separate Node variant — npm is the ecosystem default. Same
/// for `pip`: the default Python baseline assumes `pip` / the user env.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManager {
    // Node / JS / TS — non-default
    Yarn,
    Pnpm,
    Bun,
    // Python — non-default
    Poetry,
    Uv,
}

impl PackageManager {
    /// Canonical cli name ("yarn" / "pnpm" / "bun" / "poetry" / "uv").
    /// Slotted here (rather than inlined at the call site) so the canonical
    /// name list is the single source of truth.
    pub fn cli_name(self) -> &'static str {
        match self {
            PackageManager::Yarn => "yarn",
            PackageManager::Pnpm => "pnpm",
            PackageManager::Bun => "bun",
            PackageManager::Poetry => "poetry",
            PackageManager::Uv => "uv",
        }
    }
}

/// Detect the per-ecosystem package manager from repo-root marker files.
/// Marker → package manager mapping is intentional and exact:
///   - `pnpm-lock.yaml` / `.pnpmfile.cjs`      -> Pnpm (Node)
///   - `yarn.lock`                              -> Yarn  (Node)
///   - `bun.lockb`                              -> Bun   (Node)
///   - `poetry.lock`                            -> Poetry (Python)
///   - `uv.lock`                                -> Uv    (Python)
///   - `Pipfile.lock` / `requirements.txt`     -> (default: pip) — intentionally
///     left as `None` because pip is the Python baseline default.
///
/// For Rust and Go there is no relevant enum variant (single toolchain),
/// so `detect_package_manager` for those ecosystems always returns `None`.
pub fn detect_package_manager(eco: Ecosystem, marker_files: &[&str]) -> Option<PackageManager> {
    let has = |name: &str| marker_files.contains(&name);
    match eco {
        Ecosystem::Node => {
            if has("pnpm-lock.yaml") {
                Some(PackageManager::Pnpm)
            } else if has("yarn.lock") {
                Some(PackageManager::Yarn)
            } else if has("bun.lockb") {
                Some(PackageManager::Bun)
            } else {
                None
            }
        }
        Ecosystem::Python => {
            if has("uv.lock") {
                Some(PackageManager::Uv)
            } else if has("poetry.lock") {
                Some(PackageManager::Poetry)
            } else {
                None
            }
        }
        // Rust (cargo) and Go (go toolchain) are single-tool ecosystems;
        // if a project uses one it uses that one. No refinement needed.
        Ecosystem::Rust | Ecosystem::Go => None,
    }
}

/// Marker-aware baseline plan. Equal to `baseline_plan(eco)` UNLESS a
/// non-default package manager can be inferred from the markers, in which
/// case the canonical fmt/lint/test/build argv is adjusted to use that
/// package manager's CLI (e.g. `pnpm test` for a pnpm project, `uv run
/// pytest` for a uv-managed project).
///
/// IMPORTANT — fail-closed contract:
///  - This function NEVER returns fewer required steps than `baseline_plan`.
///    At worst, it returns the same steps (degraded to default CLI args).
///  - When the package-manager refinement changes a step's `required`
///    classification relative to the default baseline, it can only ever
///    raise it (optional -> required), never lower it. The strict posture
///    must not be weakened by a marker-fuzzy refinement.
pub fn baseline_plan_for(eco: Ecosystem, marker_files: &[&str]) -> Vec<GateStep> {
    let default = baseline_plan(eco);
    let Some(pm) = detect_package_manager(eco, marker_files) else {
        return default;
    };

    let refined = match (eco, pm) {
        // --- Node refinements -----------------------------------------------
        (Ecosystem::Node, PackageManager::Yarn) => Some(vec![
            step(
                "fmt",
                StepCategory::Format,
                vec_strs(&["npx", "prettier", "--check", "."]),
                "npx",
                false,
            ),
            step(
                "lint",
                StepCategory::Lint,
                vec_strs(&["yarn", "lint"]),
                "yarn",
                true,
            ),
            step(
                "build",
                StepCategory::Build,
                vec_strs(&["yarn", "build"]),
                "yarn",
                true,
            ),
            step(
                "test",
                StepCategory::Test,
                vec_strs(&["yarn", "test"]),
                "yarn",
                true,
            ),
            step(
                "audit",
                StepCategory::Security,
                vec_strs(&["yarn", "npm", "audit", "--audit-level=high"]),
                "yarn",
                true,
            ),
        ]),
        (Ecosystem::Node, PackageManager::Pnpm) => Some(vec![
            step(
                "fmt",
                StepCategory::Format,
                vec_strs(&["npx", "prettier", "--check", "."]),
                "npx",
                false,
            ),
            step(
                "lint",
                StepCategory::Lint,
                vec_strs(&["pnpm", "run", "lint"]),
                "pnpm",
                true,
            ),
            step(
                "build",
                StepCategory::Build,
                vec_strs(&["pnpm", "run", "build"]),
                "pnpm",
                true,
            ),
            step(
                "test",
                StepCategory::Test,
                vec_strs(&["pnpm", "test"]),
                "pnpm",
                true,
            ),
            step(
                "audit",
                StepCategory::Security,
                vec_strs(&["pnpm", "audit", "--audit-level=high"]),
                "pnpm",
                true,
            ),
        ]),
        (Ecosystem::Node, PackageManager::Bun) => Some(vec![
            step(
                "fmt",
                StepCategory::Format,
                vec_strs(&["bunx", "prettier", "--check", "."]),
                "bun",
                false,
            ),
            step(
                "lint",
                StepCategory::Lint,
                vec_strs(&["bun", "run", "lint"]),
                "bun",
                true,
            ),
            step(
                "build",
                StepCategory::Build,
                vec_strs(&["bun", "run", "build"]),
                "bun",
                true,
            ),
            step(
                "test",
                StepCategory::Test,
                vec_strs(&["bun", "test"]),
                "bun",
                true,
            ),
            step(
                "audit",
                StepCategory::Security,
                vec_strs(&["bun", "audit", "--audit-level=high"]),
                "bun",
                true,
            ),
        ]),

        // --- Python refinements ---------------------------------------------
        //
        // For Poetry / Uv the canonical test invocation is `poetry run
        // pytest` / `uv run pytest` and the canonical audit must be run
        // *inside* the locked env, otherwise we're auditing the wrong set
        // of packages. The audit command is the package-manager-native
        // form (`poetry run pip-audit` / `uv run pip-audit`), which runs
        // pip-audit against the locked env directly. This avoids the
        // previous bug where the audit step hard-coded
        // `pip-audit -r requirements.txt` even when no `requirements.txt`
        // exists in the repo (the typical Poetry/uv layout is
        // `pyproject.toml` + `<pm>.lock` only). Tool stays `pip-audit` —
        // the binary that actually performs the audit must be on PATH
        // (or installable into the PM env) for the step to succeed.
        (Ecosystem::Python, PackageManager::Poetry) => Some(vec![
            step(
                "fmt",
                StepCategory::Format,
                vec_strs(&["ruff", "format", "--check", "."]),
                "ruff",
                false,
            ),
            step(
                "lint",
                StepCategory::Lint,
                vec_strs(&["ruff", "check", "."]),
                "ruff",
                true,
            ),
            step(
                "build",
                StepCategory::Build,
                vec_strs(&["poetry", "build"]),
                "poetry",
                false,
            ),
            step(
                "test",
                StepCategory::Test,
                vec_strs(&["poetry", "run", "pytest", "-q"]),
                "poetry",
                true,
            ),
            step(
                "audit",
                StepCategory::Security,
                vec_strs(&["poetry", "run", "pip-audit"]),
                "pip-audit",
                true,
            ),
        ]),
        (Ecosystem::Python, PackageManager::Uv) => Some(vec![
            step(
                "fmt",
                StepCategory::Format,
                vec_strs(&["ruff", "format", "--check", "."]),
                "ruff",
                false,
            ),
            step(
                "lint",
                StepCategory::Lint,
                vec_strs(&["ruff", "check", "."]),
                "ruff",
                true,
            ),
            step(
                "build",
                StepCategory::Build,
                vec_strs(&["uv", "build"]),
                "uv",
                false,
            ),
            step(
                "test",
                StepCategory::Test,
                vec_strs(&["uv", "run", "pytest", "-q"]),
                "uv",
                true,
            ),
            step(
                "audit",
                StepCategory::Security,
                vec_strs(&["uv", "run", "pip-audit"]),
                "pip-audit",
                true,
            ),
        ]),

        // The remaining combinations are unreachable:
        //   - `detect_package_manager` returns `Some(pm)` only for Node /
        //     Python.
        //   - Rust and Go ecosystems never produce `Some(pm)`; they reach
        //     here only via `None` -> default (handled above).
        //   - Cross-ecosystem combos (e.g. Node + Poetry) are impossible:
        //     `detect_package_manager` keys `pm` off `eco`.
        // These collapses to the default baseline — exactly what the
        // fail-closed contract requires.
        (Ecosystem::Rust, _)
        | (Ecosystem::Go, _)
        | (Ecosystem::Node, PackageManager::Poetry)
        | (Ecosystem::Node, PackageManager::Uv)
        | (Ecosystem::Python, PackageManager::Yarn)
        | (Ecosystem::Python, PackageManager::Pnpm)
        | (Ecosystem::Python, PackageManager::Bun) => None,
    };

    refined.unwrap_or(default)
}

/// Per-ecosystem explicit detectors (boolean predicates). These are the
/// "I want a clear yes/no for THIS language" shape; `detect_ecosystems`
/// is for "give me every ecosystem this repo looks like". Both must agree
/// on the same markers — and they do: see tests.
pub fn detect_rust(marker_files: &[&str]) -> bool {
    marker_files.contains(&"Cargo.toml")
}
pub fn detect_node(marker_files: &[&str]) -> bool {
    marker_files.contains(&"package.json")
}
pub fn detect_python(marker_files: &[&str]) -> bool {
    marker_files.contains(&"pyproject.toml")
        || marker_files.contains(&"requirements.txt")
        || marker_files.contains(&"setup.py")
}
pub fn detect_go(marker_files: &[&str]) -> bool {
    marker_files.contains(&"go.mod")
}

/// A framework / family hint derived purely from marker filenames. The
/// returned strings are stable, lowercase, and human-readable — meant
/// for "what did we find in this repo?" reporting, not for naming any
/// step. Every entry is independently testable.
pub fn detect_frameworks(marker_files: &[&str]) -> Vec<String> {
    // Deterministic output is load-bearing for the audit trail: this
    // function may be called with marker lists produced by filesystem
    // enumeration (different order on different machines / filesystems),
    // so output order MUST NOT depend on input order. We iterate
    // MARKER_HINTS in table order as the OUTER loop and the input marker
    // list as the INNER check — first hit for each (key -> hint) wins,
    // and each hint is pushed at most once because the table is
    // deduplicated by construction.
    //
    // Every hint string in the table is already canonical lowercase, so
    // the case-sensitive dedup (via the `seen` set below) is identical to
    // a case-insensitive one in practice. No external I/O. No JSON
    // parsing (we can't safely without serde_json being a hard dep on
    // this planning module — and the gate engine wants pure planning).
    // Future slices may add content-based hints (e.g. reading
    // `package.json` to spot a React dep), gated behind a trait.
    const MARKER_HINTS: &[(&str, &str)] = &[
        // Node / JS / TS
        ("tsconfig.json", "typescript"),
        ("tsconfig.base.json", "typescript"),
        ("next.config.js", "next.js"),
        ("next.config.ts", "next.js"),
        ("next.config.mjs", "next.js"),
        ("nuxt.config.ts", "nuxt"),
        ("nuxt.config.js", "nuxt"),
        ("vite.config.ts", "vite"),
        ("vite.config.js", "vite"),
        ("vitest.config.ts", "vitest"),
        ("vitest.config.js", "vitest"),
        ("jest.config.js", "jest"),
        ("jest.config.ts", "jest"),
        // Python
        ("manage.py", "django"),
        ("Pipfile", "pipenv"),
        ("poetry.lock", "poetry"),
        ("pyproject.toml", "pyproject"), // generic Python packaging
        ("uv.lock", "uv"),
        // Go
        ("go.mod", "go-modules"),
    ];
    let mut out: Vec<String> = Vec::new();
    for (key, hint) in MARKER_HINTS {
        if !marker_files.contains(key) {
            continue;
        }
        // Each hint is emitted at most once even when multiple table
        // entries collapse to the same hint (e.g. tsconfig.json and
        // tsconfig.base.json both -> "typescript").
        if out.iter().any(|h| h == hint) {
            continue;
        }
        out.push((*hint).to_string());
    }
    out
}

/// Bring all per-language detectors + framework detectors under a single
/// "what does the repo look like?" call. Pure, deterministic, no I/O.
/// Structured for: "given a list of repo-root filenames, give me every
/// signal we can extract without reading file contents".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepoSignals {
    pub ecosystems: Vec<Ecosystem>,
    pub package_managers: Vec<(Ecosystem, PackageManager)>,
    pub framework_hints: Vec<String>,
}

pub fn detect_repo_signals(marker_files: &[&str]) -> RepoSignals {
    let ecosystems = detect_ecosystems(marker_files);
    let mut package_managers: Vec<(Ecosystem, PackageManager)> = Vec::new();
    for &eco in &ecosystems {
        if let Some(pm) = detect_package_manager(eco, marker_files) {
            package_managers.push((eco, pm));
        }
    }
    let framework_hints = detect_frameworks(marker_files);
    RepoSignals {
        ecosystems,
        package_managers,
        framework_hints,
    }
}

// ============================================================================
// === SLICE 4 (b) — fail-closed Green / Yellow / Red honest-degradation    ===
// === reporting. The aggregator logic already exists (`aggregate`); this    ===
// === slice adds the human/serializable report that the runner hands to     ===
// === reviewers / logs / CI dashboards.                                     ===
// ============================================================================
//
// The reporting layer is load-bearing for the gate's claim:
// "CI parity within local compute/network scope". To honor that claim it
// MUST:
//  (1) attach the compatibility breakdown (runnable / added_baseline /
//      skipped) to every verdict,
//  (2) render the 🟢 / 🟡 / 🔴 badge corresponding to the verdict — never
//      round-trip Red -> Yellow to "look nicer",
//  (3) when Red, list every required-failed step name (for the reviewer)
//      and every Skipped-with-reason (so the audit trail is complete),
//  (4) when Yellow, list every Skipped step + its reason,
/// The full structured gate outcome: verdict + per-step results +
/// compatibility breakdown. This is what the runner returns, what the
/// reviewer is grounded on, and what `to_pretty` / `to_compact` render.
///
/// Construct via [`GateReport::new`] so the compatibility breakdown can't
/// be lost (the renderer relies on it for honest accounting).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateReport {
    /// The aggregated verdict (Green / Yellow / Red / Inconclusive).
    /// Sourced from `aggregate(&results)`. Held by value so the report
    /// is fully self-contained and cannot drift from the results.
    pub status: GateStatus,
    /// Per-step results, in plan order.
    pub results: Vec<StepResult>,
    /// The compatibility breakdown (runnable / added_baseline /
    /// skipped-with-reason). MUST be attached even when the verdict is
    /// Green — an all-Green report without a compatibility breakdown is
    /// not honestly auditable.
    pub compatibility: CompatibilityReport,
    /// The [`GateMode`] the runner executed under. Stamped into the
    /// report so a consumer can tell a `LocalIterate` verdict (dev
    /// inner-loop, deliberately degraded) from a `Strict` verdict
    /// (fail-closed, approval-grade). Without this stamp, a
    /// `LocalIterate` Green/Yellow can be mistaken for an authoritative
    /// pass at an approval / merge gate. See `was_strict()` /
    /// `is_authoritative_pass()`.
    mode: GateMode,
}

impl GateReport {
    /// Build the canonical `GateReport` from runner outputs. Always
    /// recomputes the status from `results` via `aggregate` so the report
    /// can never get out of sync with what actually happened.
    ///
    /// The `mode` is STAMPED into the report so consumers can tell a
    /// `LocalIterate` verdict from a `Strict` verdict via `was_strict()`
    /// / `is_authoritative_pass()`. See the field doc on `mode` for the
    /// adversarial-review rationale (Z-5).
    ///
    /// Fail-closed contract for the compatibility breakdown: a non-empty
    /// `compatibility.skipped` set means CI jobs exist that this local
    /// gate did NOT verify. "Green = nothing skipped" is load-bearing —
    /// the gate's claim of CI parity is only honest when nothing was
    /// pushed off to upstream CI. Therefore, if the results-only status
    /// would be Green but the compatibility report has at least one
    /// skipped job, we downgrade to Yellow (with the skipped job names
    /// surfaced for the reviewer). Red still dominates: a results-only
    /// Red is preserved as Red regardless of how clean the compatibility
    /// breakdown is. `Inconclusive` (zero required steps ran) is also
    /// preserved end-to-end — neither the compatibility-driven
    /// downgrade nor any other path may turn it back into Green.
    pub fn new(
        results: Vec<StepResult>,
        compatibility: CompatibilityReport,
        mode: GateMode,
    ) -> Self {
        // Defensive recompute: a caller could in principle construct a
        // status manually and disagree with the results. We refuse to be
        // the source of that disagreement.
        let mut status = aggregate(&results);
        // Compatibility-driven downgrade: only Green can be downgraded to
        // Yellow. Red (required failures) and Yellow (skipped steps in
        // results) already block convergence, so we leave them alone.
        // Yellow via results is widened to include the compatibility
        // skipped names so the reviewer sees the full unverified set.
        // Inconclusive (Z-6) is preserved — there is nothing honest to
        // downgrade it TO.
        if !compatibility.skipped.is_empty() {
            match &status {
                GateStatus::Green => {
                    let mut skipped: Vec<String> = compatibility
                        .skipped
                        .iter()
                        .map(|(name, _reason)| name.clone())
                        .collect();
                    // Stable, deterministic ordering for the surfaced set.
                    skipped.sort();
                    skipped.dedup();
                    status = GateStatus::Yellow { skipped };
                }
                GateStatus::Yellow { skipped: existing } => {
                    // Union the compatibility-skipped names into the
                    // already-skipped list, sorted+deduped for stability.
                    let mut union: Vec<String> = existing.clone();
                    for (name, _reason) in &compatibility.skipped {
                        union.push(name.clone());
                    }
                    union.sort();
                    union.dedup();
                    status = GateStatus::Yellow { skipped: union };
                }
                GateStatus::Red { .. } | GateStatus::Inconclusive => {
                    // Red dominates: do not mutate. Inconclusive is also
                    // preserved (we do not pretend a "skipped CI job"
                    // narrows the gap when the gate ran zero required
                    // steps in the first place).
                }
            }
        }
        Self {
            status,
            results,
            compatibility,
            mode,
        }
    }

    /// The [`GateMode`] this report was produced under.
    pub fn mode(&self) -> GateMode {
        self.mode
    }

    /// True iff the gate ran in [`GateMode::Strict`] — the
    /// approval-grade, fail-closed posture. `LocalIterate` returns
    /// false here even if its verdict happens to be Green.
    ///
    /// Adversarial-review pin (Z-5): callers in approval / merge-gate
    /// contexts MUST key on `is_authoritative_pass()` (or pair
    /// `is_passed()` with `was_strict()`). A `LocalIterate` verdict is
    /// deliberately degraded for the dev inner-loop; reading its
    /// Green as "safe to merge" was the defect.
    pub fn was_strict(&self) -> bool {
        matches!(self.mode, GateMode::Strict)
    }

    /// True iff the gate is Green (safe to merge, within known local
    /// scope). Yellow, Red, and Inconclusive all block convergence /
    /// approval.
    ///
    /// NOTE: `is_passed()` does NOT consider the gate mode. A
    /// `LocalIterate` run with all optional checks passing returns
    /// `is_passed() == true` (so dev ergonomics in the inner-loop
    /// path are preserved: "everything I had passed"). Approval-grade
    /// callers must combine with `was_strict()` — use
    /// `is_authoritative_pass()` for that.
    pub fn is_passed(&self) -> bool {
        matches!(self.status, GateStatus::Green)
    }

    /// True iff the gate is Red. Most callers want `is_passed()` — this is
    /// for callers that explicitly handle the blocking branches.
    pub fn is_failed(&self) -> bool {
        matches!(self.status, GateStatus::Red { .. })
    }

    /// True iff the gate ran zero required steps (empty plan,
    /// all-optional plan, or a plan whose required steps were all
    /// absent). The gate CANNOT certify a pass in this state — there
    /// was no required check to actually run. Adversarial-review pin
    /// (Z-6): the previous behavior surfaced this as Green.
    pub fn is_inconclusive(&self) -> bool {
        matches!(self.status, GateStatus::Inconclusive)
    }

    /// True iff this is an AUTHORITATIVE pass: the gate ran in
    /// [`GateMode::Strict`] AND its status is Green. Approval / merge
    /// gates should key on this rather than on `is_passed()` alone —
    /// a `LocalIterate` Green is intentionally not authoritative (the
    /// dev inner-loop mode skips required tools rather than failing).
    pub fn is_authoritative_pass(&self) -> bool {
        self.was_strict() && self.is_passed()
    }

    /// Names of every required step that ran AND passed. Useful for
    /// reviewers and for differential testing across runs.
    pub fn passed_required_names(&self) -> Vec<&str> {
        self.results
            .iter()
            .filter(|r| r.required && matches!(r.outcome, StepOutcome::Passed))
            .map(|r| r.step_name.as_str())
            .collect()
    }

    /// Single-line human verdict ("🟢 GREEN", "🟡 YELLOW (skipped: n)",
    /// "🔴 RED (failed: a, b)", "⚪ INCONCLUSIVE"). Useful for log
    /// lines / structured logs / dashboards where a `GateStatus` enum
    /// is awkward.
    pub fn headline(&self) -> String {
        match &self.status {
            GateStatus::Green => "🟢 GREEN \u{2014} every required check ran and passed, nothing was skipped; safe to merge within known local scope".to_string(),
            GateStatus::Yellow { skipped } => format!(
                "🟡 YELLOW \u{2014} all required checks that could run passed, but {} step(s) skipped: [{}]",
                skipped.len(),
                skipped.join(", "),
            ),
            GateStatus::Red { failures } => format!(
                "🔴 RED \u{2014} {} required check(s) failed: [{}]; cannot converge, cannot approve",
                failures.len(),
                failures.join(", "),
            ),
            GateStatus::Inconclusive => "\u{26aa} INCONCLUSIVE \u{2014} no required checks ran (empty plan or all-optional plan); the gate cannot certify a pass \u{2014} cannot merge, cannot approve".to_string(),
        }
    }

    /// Multi-line, human-readable gate report suitable for printing to a
    /// reviewer's terminal or a CI summary. Always includes the
    /// compatibility breakdown — even on Green — so the audit trail is
    /// complete.
    ///
    /// Format (line-by-line, with section markers); `*` marks required
    /// steps so reviewers can scan the table for blockers vs advisories:
    ///   <headline badge + verdict line>
    ///   breakdown:   <N> runnable, <M> added-baseline, <K> skipped
    ///   steps:
    ///     PASSED  * fmt
    ///     FAILED  * test (tool or command failed)
    ///     SKIPPED  audit (<reason>)
    ///   compatibility:
    ///     runnable       [fmt, clippy, build, test]
    ///     added-baseline [deny]
    ///     skipped-with-reason [(integration, requires service containers (upstream CI verifies))]
    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.headline());
        out.push('\n');

        out.push_str(&format!(
            "breakdown: {} runnable, {} added-baseline, {} skipped\n",
            self.compatibility.runnable.len(),
            self.compatibility.added_baseline.len(),
            self.compatibility.skipped.len(),
        ));

        out.push_str("steps:\n");
        for r in &self.results {
            let (tag, note) = match &r.outcome {
                StepOutcome::Passed => ("PASSED ", String::new()),
                StepOutcome::Failed => ("FAILED ", "(tool or command failed)".to_string()),
                StepOutcome::Skipped { reason } => ("SKIPPED", format!("({})", reason)),
            };
            // Required-ness is encoded in the tag so reviewers can scan
            // the list and immediately see required-blockers vs optional.
            let req_marker = if r.required { "*" } else { " " };
            if note.is_empty() {
                out.push_str(&format!("  {} {} {}\n", tag, req_marker, r.step_name));
            } else {
                out.push_str(&format!(
                    "  {} {} {} {}\n",
                    tag, req_marker, r.step_name, note
                ));
            }
        }

        out.push_str("compatibility:\n");
        out.push_str(&format!(
            "  runnable        [{}]\n",
            self.compatibility.runnable.join(", "),
        ));
        out.push_str(&format!(
            "  added-baseline  [{}]\n",
            self.compatibility.added_baseline.join(", "),
        ));
        if self.compatibility.skipped.is_empty() {
            out.push_str("  skipped         []\n");
        } else {
            out.push_str("  skipped-with-reason [\n");
            for (name, reason) in &self.compatibility.skipped {
                out.push_str(&format!("    ({}, {})\n", name, reason));
            }
            out.push_str("  ]\n");
        }

        out
    }

    /// Compact one-line summary, suitable for structured log lines and
    /// for CI summarizers that want a single record per gate run. Format:
    ///   <Badge> GREEN|YELLOW|RED|INCONCLUSIVE [required=N passed=M optional=K]
    pub fn to_compact(&self) -> String {
        let mut required = 0usize;
        let mut required_passed = 0usize;
        let mut optional = 0usize;
        let mut optional_passed = 0usize;
        for r in &self.results {
            if r.required {
                required += 1;
                if matches!(r.outcome, StepOutcome::Passed) {
                    required_passed += 1;
                }
            } else {
                optional += 1;
                if matches!(r.outcome, StepOutcome::Passed) {
                    optional_passed += 1;
                }
            }
        }

        let (badge, label) = match &self.status {
            GateStatus::Green => ("🟢", "GREEN"),
            GateStatus::Yellow { .. } => ("🟡", "YELLOW"),
            GateStatus::Red { .. } => ("🔴", "RED"),
            GateStatus::Inconclusive => ("⚪", "INCONCLUSIVE"),
        };
        format!(
            "{badge} {label} [required={required} passed={required_passed} optional={optional} passed={optional_passed}]"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----- detect_ecosystems ----------------------------------------------

    #[test]
    fn detect_empty_when_no_markers() {
        assert!(detect_ecosystems(&[]).is_empty());
    }

    #[test]
    fn detect_single_marker_per_ecosystem() {
        assert_eq!(detect_ecosystems(&["Cargo.toml"]), vec![Ecosystem::Rust],);
        assert_eq!(detect_ecosystems(&["package.json"]), vec![Ecosystem::Node],);
        assert_eq!(detect_ecosystems(&["go.mod"]), vec![Ecosystem::Go],);
        // Each Python marker alone maps to Python.
        assert_eq!(
            detect_ecosystems(&["pyproject.toml"]),
            vec![Ecosystem::Python],
        );
        assert_eq!(
            detect_ecosystems(&["requirements.txt"]),
            vec![Ecosystem::Python],
        );
        assert_eq!(detect_ecosystems(&["setup.py"]), vec![Ecosystem::Python],);
    }

    #[test]
    fn detect_python_dedupes_across_markers() {
        // Both Python markers present must collapse to a single Python entry.
        let got = detect_ecosystems(&["pyproject.toml", "requirements.txt"]);
        assert_eq!(got, vec![Ecosystem::Python]);

        let got = detect_ecosystems(&["requirements.txt", "pyproject.toml", "setup.py"]);
        assert_eq!(got, vec![Ecosystem::Python]);
    }

    #[test]
    fn detect_returns_in_fixed_order_regardless_of_input_order() {
        // Scrambled input; output must still be Rust, Node, Python, Go.
        let got = detect_ecosystems(&["go.mod", "Cargo.toml", "package.json", "pyproject.toml"]);
        assert_eq!(
            got,
            vec![
                Ecosystem::Rust,
                Ecosystem::Node,
                Ecosystem::Python,
                Ecosystem::Go,
            ],
        );

        // Reverse input order — same fixed output order.
        let got = detect_ecosystems(&["go.mod", "requirements.txt", "package.json", "Cargo.toml"]);
        assert_eq!(
            got,
            vec![
                Ecosystem::Rust,
                Ecosystem::Node,
                Ecosystem::Python,
                Ecosystem::Go,
            ],
        );
    }

    #[test]
    fn detect_unrelated_markers_are_ignored() {
        // Non-marker filenames must not trigger an ecosystem match.
        assert!(detect_ecosystems(&["README.md", "LICENSE", "Makefile"]).is_empty());
        // Substring confusion: a file NAMED "fake_Cargo.toml" or a folder
        // "crates/cargo.toml" must not match; we compare exact strings.
        assert!(detect_ecosystems(&["cargo.toml", "Cargo.lock", "Cargo.toml.bak"]).is_empty());
    }

    // ----- baseline_plan ---------------------------------------------------

    fn step_names(plan: &[GateStep]) -> Vec<&str> {
        plan.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn rust_baseline_has_required_core_and_optional_audit() {
        let plan = baseline_plan(Ecosystem::Rust);
        let names = step_names(&plan);

        // fmt, clippy, build, test, deny must all be present.
        for required in ["fmt", "clippy", "build", "test", "deny"] {
            assert!(
                names.contains(&required),
                "Rust baseline missing required step `{required}`, got {names:?}",
            );
        }

        let deny = plan.iter().find(|s| s.name == "deny").expect("deny step");
        assert_eq!(deny.tool, "cargo-deny");
        assert!(deny.required, "deny must be required");

        let audit = plan.iter().find(|s| s.name == "audit").expect("audit step");
        assert!(
            !audit.required,
            "audit must be advisory (required == false)"
        );
    }

    #[test]
    fn every_baseline_plan_has_a_security_step() {
        for eco in [
            Ecosystem::Rust,
            Ecosystem::Node,
            Ecosystem::Python,
            Ecosystem::Go,
        ] {
            let plan = baseline_plan(eco);
            let sec_count = plan
                .iter()
                .filter(|s| s.category == StepCategory::Security)
                .count();
            assert!(
                sec_count >= 1,
                "{eco:?} baseline must include at least one Security step, got {plan:?}",
            );
        }
    }

    #[test]
    fn rust_baseline_commands_match_spec() {
        // Lock the exact argv tuples so future edits don't silently change
        // what `cargo fmt --check` actually means.
        let plan = baseline_plan(Ecosystem::Rust);

        let fmt = plan.iter().find(|s| s.name == "fmt").expect("fmt");
        assert_eq!(
            fmt.command,
            vec!["cargo", "fmt", "--all", "--", "--check"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        );

        let clippy = plan.iter().find(|s| s.name == "clippy").expect("clippy");
        assert_eq!(
            clippy.command,
            vec!["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        );

        let build = plan.iter().find(|s| s.name == "build").expect("build");
        assert_eq!(
            build.command,
            vec!["cargo", "build", "--all-targets"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        );

        let test = plan.iter().find(|s| s.name == "test").expect("test");
        assert_eq!(
            test.command,
            vec!["cargo", "test", "--all-targets"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        );
    }

    // ----- aggregate -------------------------------------------------------

    fn result(name: &str, required: bool, outcome: StepOutcome) -> StepResult {
        StepResult {
            step_name: name.to_string(),
            required,
            outcome,
        }
    }

    #[test]
    fn aggregate_all_passed_is_green() {
        let results = vec![
            result("fmt", false, StepOutcome::Passed),
            result("lint", true, StepOutcome::Passed),
            result("test", true, StepOutcome::Passed),
        ];
        assert_eq!(aggregate(&results), GateStatus::Green);
    }

    #[test]
    fn aggregate_required_failed_is_red_with_name() {
        let results = vec![
            result("lint", true, StepOutcome::Passed),
            result("test", true, StepOutcome::Failed),
            result("fmt", false, StepOutcome::Passed),
        ];
        assert_eq!(
            aggregate(&results),
            GateStatus::Red {
                failures: vec!["test".to_string()],
            },
        );
    }

    #[test]
    fn aggregate_optional_skipped_is_yellow_with_name() {
        let results = vec![
            result("lint", true, StepOutcome::Passed),
            result(
                "audit",
                false,
                StepOutcome::Skipped {
                    reason: "tool missing".to_string(),
                },
            ),
            result("test", true, StepOutcome::Passed),
        ];
        assert_eq!(
            aggregate(&results),
            GateStatus::Yellow {
                skipped: vec!["audit".to_string()],
            },
        );
    }

    #[test]
    fn aggregate_optional_failed_stays_green() {
        // A failed optional step is advisory and must NOT turn the gate Red.
        let results = vec![
            result("lint", true, StepOutcome::Passed),
            result("fmt", false, StepOutcome::Failed),
            result("test", true, StepOutcome::Passed),
        ];
        assert_eq!(aggregate(&results), GateStatus::Green);
    }

    #[test]
    fn aggregate_required_failed_dominates_skipped() {
        // Red wins over Yellow: required Failed AND something Skipped -> Red.
        let results = vec![
            result("lint", true, StepOutcome::Passed),
            result(
                "audit",
                false,
                StepOutcome::Skipped {
                    reason: "tool missing".to_string(),
                },
            ),
            result("test", true, StepOutcome::Failed),
            result("fmt", false, StepOutcome::Passed),
        ];
        match aggregate(&results) {
            GateStatus::Red { failures } => {
                assert_eq!(failures, vec!["test".to_string()]);
            }
            other => panic!("expected Red, got {other:?}"),
        }
    }

    #[test]
    fn aggregate_yellow_collects_all_skipped_in_input_order() {
        let results = vec![
            result("lint", true, StepOutcome::Passed),
            result(
                "z-skipped-first",
                false,
                StepOutcome::Skipped {
                    reason: "r".to_string(),
                },
            ),
            result("test", true, StepOutcome::Passed),
            result(
                "a-skipped-second",
                true,
                StepOutcome::Skipped {
                    reason: "r".to_string(),
                },
            ),
        ];
        assert_eq!(
            aggregate(&results),
            GateStatus::Yellow {
                skipped: vec![
                    "z-skipped-first".to_string(),
                    "a-skipped-second".to_string(),
                ],
            },
        );
    }

    #[test]
    fn aggregate_red_collects_all_required_failures_in_input_order() {
        let results = vec![
            result("a", true, StepOutcome::Failed),
            result("b", true, StepOutcome::Passed),
            result("c", true, StepOutcome::Failed),
        ];
        assert_eq!(
            aggregate(&results),
            GateStatus::Red {
                failures: vec!["a".to_string(), "c".to_string()],
            },
        );
    }

    // ----- run_plan (runner core) -----------------------------------------

    use std::collections::{BTreeSet, HashMap};

    /// Test double for `GateEnv`:
    ///  - `tools`: the set of binaries that are considered available on PATH.
    ///  - `failures`: step names whose `run_step` should return `Failed`.
    ///    Everything else passes; tools not in `tools` are "missing".
    struct FakeEnv {
        tools: BTreeSet<String>,
        failures: HashMap<String, StepExec>,
    }

    impl FakeEnv {
        fn new(tools: &[&str]) -> Self {
            Self {
                tools: tools.iter().map(|t| (*t).to_string()).collect(),
                failures: HashMap::new(),
            }
        }

        fn failing(mut self, name: &str) -> Self {
            self.failures.insert(name.to_string(), StepExec::Failed);
            self
        }
    }

    impl GateEnv for FakeEnv {
        fn tool_available(&self, tool: &str) -> bool {
            self.tools.contains(tool)
        }

        fn run_step(&self, step: &GateStep) -> StepExec {
            // Default to Passed; only names explicitly listed fail.
            self.failures
                .get(&step.name)
                .copied()
                .unwrap_or(StepExec::Passed)
        }
    }

    /// Small helper: build a single step with the given name + required flag.
    fn s(name: &str, required: bool) -> GateStep {
        GateStep {
            name: name.to_string(),
            category: StepCategory::Lint,
            command: vec!["true".to_string()],
            tool: name.to_string(), // tool name == step name keeps FakeEnv tidy
            required,
        }
    }

    #[test]
    fn run_plan_all_tools_present_all_pass_is_green() {
        let plan = vec![s("fmt", true), s("lint", false), s("test", true)];
        let env = FakeEnv::new(&["fmt", "lint", "test"]);

        let (results, status) = run_plan(&plan, GateMode::Strict, &env);

        assert_eq!(status, GateStatus::Green);
        // Every step Passed.
        assert!(results
            .iter()
            .all(|r| matches!(r.outcome, StepOutcome::Passed)));
    }

    #[test]
    fn run_plan_strict_required_missing_tool_fails_closed_red() {
        // `deny` is required and its tool is NOT available; under Strict this
        // must turn into a Failed (not Skipped) -> Red naming the step.
        let plan = vec![s("fmt", true), s("deny", true)];
        let env = FakeEnv::new(&["fmt"]); // "deny" missing

        let (results, status) = run_plan(&plan, GateMode::Strict, &env);

        assert_eq!(
            status,
            GateStatus::Red {
                failures: vec!["deny".to_string()],
            },
        );
        let deny = results
            .iter()
            .find(|r| r.step_name == "deny")
            .expect("deny result");
        assert_eq!(
            deny.outcome,
            StepOutcome::Failed,
            "Strict + required + missing tool must be Failed (fail-closed)",
        );
    }

    #[test]
    fn run_plan_local_iterate_required_missing_tool_is_skipped_yellow() {
        // Same setup as above but in LocalIterate mode: missing REQUIRED tool
        // becomes Skipped (not Failed) -> Yellow naming the step.
        let plan = vec![s("fmt", true), s("deny", true)];
        let env = FakeEnv::new(&["fmt"]); // "deny" missing

        let (results, status) = run_plan(&plan, GateMode::LocalIterate, &env);

        assert_eq!(
            status,
            GateStatus::Yellow {
                skipped: vec!["deny".to_string()],
            },
        );
        let deny = results
            .iter()
            .find(|r| r.step_name == "deny")
            .expect("deny result");
        match &deny.outcome {
            StepOutcome::Skipped { reason } => {
                assert!(
                    reason.contains("deny"),
                    "reason should mention tool: {reason}"
                );
            }
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn run_plan_strict_optional_missing_tool_is_skipped_yellow_not_red() {
        // `audit` is optional; missing tool under Strict => Skipped (advisory),
        // not Failed. Gate must be Yellow (not Red).
        let plan = vec![s("fmt", true), s("audit", false)];
        let env = FakeEnv::new(&["fmt"]); // "audit" missing

        let (results, status) = run_plan(&plan, GateMode::Strict, &env);

        assert_eq!(
            status,
            GateStatus::Yellow {
                skipped: vec!["audit".to_string()],
            },
        );
        let audit = results
            .iter()
            .find(|r| r.step_name == "audit")
            .expect("audit result");
        assert!(
            matches!(audit.outcome, StepOutcome::Skipped { .. }),
            "Strict + optional + missing tool must be Skipped, got {:?}",
            audit.outcome,
        );
    }

    #[test]
    fn run_plan_tool_present_required_command_failure_is_red() {
        // Tool IS available, but the command itself fails on a REQUIRED step.
        // This must produce a Red verdict (independent of mode).
        let plan = vec![s("fmt", true), s("test", true)];
        let env = FakeEnv::new(&["fmt", "test"]).failing("test");

        let (results, status) = run_plan(&plan, GateMode::Strict, &env);

        assert_eq!(
            status,
            GateStatus::Red {
                failures: vec!["test".to_string()],
            },
        );
        let test = results
            .iter()
            .find(|r| r.step_name == "test")
            .expect("test result");
        assert_eq!(test.outcome, StepOutcome::Failed);
    }

    #[test]
    fn run_plan_skipped_reason_contains_tool_name() {
        // The reason text on a Skipped step must mention the tool that was
        // missing — that's what shows up in user-facing diagnostics.
        // Use an OPTIONAL step under Strict, so the missing tool legitimately
        // routes to Skipped (rather than the fail-closed Failed branch).
        let plan = vec![s("cargo-audit", false)];
        let env = FakeEnv::new(&[]); // nothing available

        let (results, _status) = run_plan(&plan, GateMode::Strict, &env);
        let only = &results[0];
        match &only.outcome {
            StepOutcome::Skipped { reason } => {
                assert!(
                    reason.contains("cargo-audit"),
                    "reason must mention the missing tool name: {reason}",
                );
                assert!(
                    reason.contains("not available"),
                    "reason should describe the condition: {reason}",
                );
            }
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn run_plan_results_are_returned_in_plan_order() {
        // The order of `results` must mirror the order of `plan`, regardless
        // of which steps pass, fail, or skip.
        let plan = vec![
            s("alpha", true),
            s("bravo", false),
            s("charlie", true),
            s("delta", false),
        ];
        // "bravo" and "delta" missing -> both skip; "charlie" runs and fails.
        let env = FakeEnv::new(&["alpha", "charlie"]).failing("charlie");

        let (results, _status) = run_plan(&plan, GateMode::Strict, &env);

        let names: Vec<&str> = results.iter().map(|r| r.step_name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "bravo", "charlie", "delta"]);
    }

    // ----- derive_plan (CI-derivation classifier) --------------------------

    /// Build a CiJob with the runnable defaults (no secrets/services/self-hosted).
    fn ci(name: &str, cat: StepCategory) -> CiJob {
        CiJob {
            name: name.to_string(),
            command: vec!["tool".to_string(), "run".to_string()],
            tool: "tool".to_string(),
            category: cat,
            needs_secrets: false,
            needs_services: false,
            self_hosted: false,
        }
    }

    #[test]
    fn derive_plan_plain_ci_job_becomes_runnable_step() {
        let jobs = vec![ci("lint", StepCategory::Lint)];
        let (plan, report) = derive_plan(&jobs, &[]);

        // Step ends up in the plan with `required = true` and original fields.
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].name, "lint");
        assert_eq!(plan[0].category, StepCategory::Lint);
        assert_eq!(plan[0].command, vec!["tool", "run"]);
        assert_eq!(plan[0].tool, "tool");
        assert!(plan[0].required, "runnable CI jobs must be required");

        // And the report names it as runnable.
        assert_eq!(report.runnable, vec!["lint".to_string()]);
        assert!(report.skipped.is_empty());
        assert!(report.added_baseline.is_empty());
    }

    #[test]
    fn derive_plan_needs_secrets_is_skipped_with_secrets_reason() {
        let mut job = ci("deploy", StepCategory::Build);
        job.needs_secrets = true;

        let (plan, report) = derive_plan(&[job], &[]);

        assert!(
            plan.is_empty(),
            "needs_secrets job must NOT be in the plan, got {plan:?}",
        );
        assert_eq!(
            report.skipped,
            vec![(
                "deploy".to_string(),
                "requires CI secrets (upstream CI verifies)".to_string(),
            )],
        );
        assert!(report.runnable.is_empty());
    }

    #[test]
    fn derive_plan_needs_services_and_self_hosted_each_get_their_own_reason() {
        let mut services_job = ci("integration", StepCategory::Test);
        services_job.needs_services = true;

        let mut hosted_job = ci("android-arm64", StepCategory::Build);
        hosted_job.self_hosted = true;

        let (plan, report) = derive_plan(
            &[services_job, hosted_job],
            &[], // no baseline; keeps the test focused on skip classification
        );

        assert!(plan.is_empty(), "both jobs must be skipped, got {plan:?}");
        assert_eq!(
            report.skipped,
            vec![
                (
                    "integration".to_string(),
                    "requires service containers (upstream CI verifies)".to_string(),
                ),
                (
                    "android-arm64".to_string(),
                    "requires a self-hosted runner (upstream CI verifies)".to_string(),
                ),
            ],
        );
    }

    #[test]
    fn derive_plan_skip_reason_precedence_is_secrets_over_self_hosted() {
        // Both flags set: secrets must win, per the documented precedence
        // (secrets > services > self_hosted).
        let mut job = ci("publish", StepCategory::Build);
        job.needs_secrets = true;
        job.self_hosted = true;

        let (plan, report) = derive_plan(&[job], &[]);

        assert!(plan.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].0, "publish");
        assert_eq!(
            report.skipped[0].1, "requires CI secrets (upstream CI verifies)",
            "secrets reason must beat self_hosted reason",
        );
    }

    #[test]
    fn derive_plan_skip_reason_precedence_is_secrets_over_services() {
        // Belt-and-suspenders: also check secrets > services explicitly.
        let mut job = ci("release", StepCategory::Build);
        job.needs_secrets = true;
        job.needs_services = true;

        let (_plan, report) = derive_plan(&[job], &[]);
        assert_eq!(
            report.skipped[0].1,
            "requires CI secrets (upstream CI verifies)",
        );
    }

    #[test]
    fn derive_plan_skip_reason_precedence_is_services_over_self_hosted() {
        // services > self_hosted (middle of the precedence chain).
        let mut job = ci("e2e", StepCategory::Test);
        job.needs_services = true;
        job.self_hosted = true;

        let (_plan, report) = derive_plan(&[job], &[]);
        assert_eq!(
            report.skipped[0].1,
            "requires service containers (upstream CI verifies)",
        );
    }

    #[test]
    fn derive_plan_baseline_fills_security_gap_when_ci_does_not_cover_it() {
        // CI covers Lint + Test, but no Security category. Baseline has a
        // Format step and a Security step, neither of which is covered by
        // the runnable CI steps — so both baseline steps get added.
        let jobs = vec![
            ci("lint", StepCategory::Lint),
            ci("test", StepCategory::Test),
        ];
        let baseline = vec![
            GateStep {
                name: "fmt".to_string(),
                category: StepCategory::Format,
                command: vec!["fmt".to_string()],
                tool: "fmt".to_string(),
                required: true,
            },
            GateStep {
                name: "deny".to_string(),
                category: StepCategory::Security,
                command: vec!["cargo".to_string(), "deny".to_string()],
                tool: "cargo-deny".to_string(),
                required: true,
            },
        ];

        let (plan, report) = derive_plan(&jobs, &baseline);

        // `deny` (Security) must be appended because CI did not cover Security.
        assert!(
            plan.iter().any(|s| s.name == "deny"),
            "Security baseline must be added to fill the gap, got {plan:?}",
        );
        // `fmt` (Format) is also not covered by CI, so it is added too.
        // The point of this test is the Security gap; the Format step being
        // added as well is the natural consequence of the union rule.
        assert_eq!(
            report.added_baseline,
            vec!["fmt".to_string(), "deny".to_string()],
        );
    }

    #[test]
    fn derive_plan_baseline_required_step_always_runs_even_when_ci_covers_category() {
        // Adversarial-review pin (Z-7): a repo-authored CI job covering a
        // category that is ALSO covered by a baseline REQUIRED step must
        // NOT displace the baseline. Baseline safety required steps
        // ALWAYS run, even if CI claims to cover the same category. The
        // CI-derived step is additive, never a replacement.
        //
        // (The previous behavior suppressed the baseline required step
        // when CI covered the category, letting a no-op CI YAML
        // silently displace e.g. `cargo test`.)
        let jobs = vec![ci("trivy", StepCategory::Security)];
        let baseline = vec![
            GateStep {
                name: "deny".to_string(),
                category: StepCategory::Security,
                command: vec!["cargo".to_string(), "deny".to_string()],
                tool: "cargo-deny".to_string(),
                required: true,
            },
            GateStep {
                name: "fmt".to_string(),
                category: StepCategory::Format,
                command: vec!["fmt".to_string()],
                tool: "fmt".to_string(),
                required: true,
            },
        ];

        let (plan, report) = derive_plan(&jobs, &baseline);

        // CI's Security step is in the plan (still runnable).
        assert!(plan.iter().any(|s| s.name == "trivy"));
        // AND the baseline Security step ("deny") is ALSO in the plan —
        // baseline REQUIRED steps are additive, not displaced (Z-7).
        assert!(
            plan.iter().any(|s| s.name == "deny"),
            "baseline REQUIRED Security step must run even when CI covers Security (Z-7), got {plan:?}",
        );
        // Both baseline steps are recorded as added (they were missing
        // from the CI input).
        assert_eq!(
            report.added_baseline,
            vec!["deny".to_string(), "fmt".to_string()],
            "both required baseline steps must be added (Z-7), got {:?}",
            report.added_baseline,
        );
    }

    #[test]
    fn derive_plan_baseline_optional_step_is_still_deduped_when_ci_covers_category() {
        // Counter-test for Z-7: the fix is specifically about REQUIRED
        // baseline steps. Optional baseline steps keep their existing
        // dedup-by-category rule — an optional check is advisory, and
        // doubling it up when CI already covers the category is noise
        // without safety value.
        let jobs = vec![ci("trivy", StepCategory::Security)];
        let baseline = vec![
            GateStep {
                name: "deny".to_string(),
                category: StepCategory::Security,
                command: vec!["cargo".to_string(), "deny".to_string()],
                tool: "cargo-deny".to_string(),
                required: true, // required — must always run
            },
            GateStep {
                name: "optional-audit".to_string(),
                category: StepCategory::Security,
                command: vec!["audit".to_string()],
                tool: "audit".to_string(),
                required: false, // optional — dedup if CI covers
            },
        ];

        let (plan, report) = derive_plan(&jobs, &baseline);

        // The required baseline deny step is in the plan (Z-7).
        assert!(
            plan.iter().any(|s| s.name == "deny"),
            "baseline REQUIRED Security step must run (Z-7), got {plan:?}",
        );
        // The OPTIONAL baseline audit step is deduped out (CI's trivy
        // already covers Security).
        assert!(
            !plan.iter().any(|s| s.name == "optional-audit"),
            "baseline OPTIONAL Security step must be deduped when CI covers Security (Z-7 counter-test), got {plan:?}",
        );
        // Only the required baseline is in added_baseline (optional
        // audit was suppressed by the dedup rule).
        assert_eq!(
            report.added_baseline,
            vec!["deny".to_string()],
            "only the required baseline must appear in added_baseline (Z-7 counter-test)",
        );
    }

    #[test]
    fn derive_plan_baseline_can_fill_a_gap_left_by_a_skipped_ci_job() {
        // Honest degradation: a SKIPPED CI job leaves its category uncovered
        // locally, so the baseline MUST fill that gap.
        let mut skipped = ci("trivy", StepCategory::Security);
        skipped.needs_services = true; // can't run locally
        let jobs = vec![skipped];

        let baseline = vec![GateStep {
            name: "deny".to_string(),
            category: StepCategory::Security,
            command: vec!["cargo".to_string(), "deny".to_string()],
            tool: "cargo-deny".to_string(),
            required: true,
        }];

        let (plan, report) = derive_plan(&jobs, &baseline);

        assert_eq!(
            report.skipped,
            vec![(
                "trivy".to_string(),
                "requires service containers (upstream CI verifies)".to_string(),
            )],
        );
        // The baseline Security step fills the gap left by the skipped job.
        assert_eq!(
            report.added_baseline,
            vec!["deny".to_string()],
            "baseline must cover a category the runnable CI didn't run",
        );
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].name, "deny");
    }

    #[test]
    fn derive_plan_order_is_ci_first_then_baseline() {
        // Runnable CI steps come first in input order; added baseline steps
        // follow in baseline order. Report vectors mirror the plan.
        let jobs = vec![
            ci("ci-lint", StepCategory::Lint),
            ci("ci-build", StepCategory::Build),
        ];
        // Baseline introduces new categories (Format, Security, Test) — none
        // of which are covered by the CI steps above.
        let baseline = vec![
            GateStep {
                name: "fmt".to_string(),
                category: StepCategory::Format,
                command: vec!["fmt".to_string()],
                tool: "fmt".to_string(),
                required: true,
            },
            GateStep {
                name: "deny".to_string(),
                category: StepCategory::Security,
                command: vec!["deny".to_string()],
                tool: "deny".to_string(),
                required: true,
            },
            GateStep {
                name: "test".to_string(),
                category: StepCategory::Test,
                command: vec!["test".to_string()],
                tool: "test".to_string(),
                required: true,
            },
        ];

        let (plan, report) = derive_plan(&jobs, &baseline);

        assert_eq!(
            plan.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["ci-lint", "ci-build", "fmt", "deny", "test"],
            "plan order: runnable CI (input order) then added baseline (baseline order)",
        );
        assert_eq!(
            report.runnable,
            vec!["ci-lint".to_string(), "ci-build".to_string()],
        );
        assert_eq!(
            report.added_baseline,
            vec!["fmt".to_string(), "deny".to_string(), "test".to_string()],
        );
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn derive_plan_empty_inputs_yield_empty_outputs() {
        // No CI jobs, no baseline => nothing in the plan, all report vecs empty.
        let (plan, report) = derive_plan(&[], &[]);
        assert!(plan.is_empty());
        assert!(report.runnable.is_empty());
        assert!(report.skipped.is_empty());
        assert!(report.added_baseline.is_empty());
    }

    #[test]
    fn derive_plan_all_ci_jobs_skipped_baseline_still_fills_gaps() {
        // Every CI job is skipped (e.g. repo's CI is fully service-dependent).
        // The baseline plan must still produce a complete local plan and
        // record every job in `skipped` for honest accounting.
        let mut a = ci("integration", StepCategory::Test);
        a.needs_services = true;
        let mut b = ci("deploy", StepCategory::Build);
        b.needs_secrets = true;
        let jobs = vec![a, b];

        let baseline = vec![
            GateStep {
                name: "fmt".to_string(),
                category: StepCategory::Format,
                command: vec!["fmt".to_string()],
                tool: "fmt".to_string(),
                required: true,
            },
            GateStep {
                name: "lint".to_string(),
                category: StepCategory::Lint,
                command: vec!["lint".to_string()],
                tool: "lint".to_string(),
                required: true,
            },
            GateStep {
                name: "build".to_string(),
                category: StepCategory::Build,
                command: vec!["build".to_string()],
                tool: "build".to_string(),
                required: true,
            },
            GateStep {
                name: "test".to_string(),
                category: StepCategory::Test,
                command: vec!["test".to_string()],
                tool: "test".to_string(),
                required: true,
            },
            GateStep {
                name: "deny".to_string(),
                category: StepCategory::Security,
                command: vec!["deny".to_string()],
                tool: "deny".to_string(),
                required: true,
            },
        ];

        let (plan, report) = derive_plan(&jobs, &baseline);

        // Nothing from CI made it into the plan.
        assert!(plan
            .iter()
            .all(|s| baseline.iter().any(|b| b.name == s.name)));
        // Every CI job is accounted for as skipped, in input order.
        assert_eq!(
            report.skipped,
            vec![
                (
                    "integration".to_string(),
                    "requires service containers (upstream CI verifies)".to_string(),
                ),
                (
                    "deploy".to_string(),
                    "requires CI secrets (upstream CI verifies)".to_string(),
                ),
            ],
        );
        // Baseline covers every category the CI didn't, so all baseline
        // steps are added.
        assert_eq!(
            report.added_baseline,
            vec![
                "fmt".to_string(),
                "lint".to_string(),
                "build".to_string(),
                "test".to_string(),
                "deny".to_string(),
            ],
        );
        assert!(report.runnable.is_empty());
    }

    // ----- SLICE 4 (a) — per-language detectors + canonical commands -----

    #[test]
    fn per_language_detectors_match_detect_ecosystems_truth_table() {
        // The per-language detector predicates must agree with the marker
        // set `detect_ecosystems` already canonicalizes — otherwise the
        // "explicit Rust detector" and the "what ecosystems" call would
        // disagree, which is a bug, not a feature.
        let cases: &[(&[&str], Ecosystem, bool)] = &[
            (&["Cargo.toml"], Ecosystem::Rust, true),
            (&["Cargo.toml", "package.json"], Ecosystem::Rust, true),
            (&["README.md"], Ecosystem::Rust, false),
            (&["Cargo.toml"], Ecosystem::Go, false),
            (&["package.json"], Ecosystem::Node, true),
            (&["package.json", "yarn.lock"], Ecosystem::Node, true),
            (&["Cargo.toml"], Ecosystem::Node, false),
            (&["pyproject.toml"], Ecosystem::Python, true),
            (&["requirements.txt"], Ecosystem::Python, true),
            (&["setup.py"], Ecosystem::Python, true),
            (&["requirements.txt", "setup.py"], Ecosystem::Python, true),
            (
                &["pyproject.toml", "requirements.txt"],
                Ecosystem::Python,
                true,
            ),
            (&["Cargo.toml"], Ecosystem::Python, false),
            (&["go.mod"], Ecosystem::Go, true),
            (&["go.sum"], Ecosystem::Go, false), // NOT a marker (lockfile only)
            (&["package.json"], Ecosystem::Go, false),
        ];

        for (markers, eco, want) in cases {
            let got = match eco {
                Ecosystem::Rust => detect_rust(markers),
                Ecosystem::Node => detect_node(markers),
                Ecosystem::Python => detect_python(markers),
                Ecosystem::Go => detect_go(markers),
            };
            assert_eq!(
                got, *want,
                "per-language detector disagreed for {markers:?}/{eco:?}: got {got}, want {want}",
            );

            // Cross-check: `detect_ecosystems` should agree in the broad
            // sense (yes iff the per-language detector is yes).
            let in_ecosystems = detect_ecosystems(markers).contains(eco);
            assert_eq!(
                in_ecosystems, *want,
                "detect_ecosystems disagreed with per-language detector for {markers:?}/{eco:?}",
            );
        }
    }

    #[test]
    fn detect_package_manager_node_prioritizes_pnpm_over_yarn_over_bun() {
        // pnpm-lock.yaml wins over yarn.lock (a project should only have
        // one primary lockfile in practice, but the precedence is
        // well-defined if both happen to be present).
        let got = detect_package_manager(Ecosystem::Node, &["package.json", "pnpm-lock.yaml"]);
        assert_eq!(got, Some(PackageManager::Pnpm));
        let got = detect_package_manager(Ecosystem::Node, &["package.json", "yarn.lock"]);
        assert_eq!(got, Some(PackageManager::Yarn));
        let got = detect_package_manager(Ecosystem::Node, &["package.json", "bun.lockb"]);
        assert_eq!(got, Some(PackageManager::Bun));

        // Pnpm wins over Yarn.
        let got = detect_package_manager(
            Ecosystem::Node,
            &["package.json", "yarn.lock", "pnpm-lock.yaml"],
        );
        assert_eq!(got, Some(PackageManager::Pnpm));
    }

    #[test]
    fn detect_package_manager_node_defaults_to_none_when_only_package_json() {
        // npm is the Node default; we don't enumerate it. A plain
        // package.json (no lockfile) means "use the baseline default".
        assert_eq!(
            detect_package_manager(Ecosystem::Node, &["package.json"]),
            None,
        );
    }

    #[test]
    fn detect_package_manager_node_does_not_match_unrelated_lockfiles() {
        // Lockfiles for other ecosystems must not trigger a Node
        // package-manager detection.
        assert_eq!(
            detect_package_manager(Ecosystem::Node, &["Cargo.lock"]),
            None,
        );
        assert_eq!(
            detect_package_manager(Ecosystem::Node, &["poetry.lock"]),
            None,
        );
    }

    #[test]
    fn detect_package_manager_python_uv_beats_poetry() {
        assert_eq!(
            detect_package_manager(Ecosystem::Python, &["pyproject.toml", "uv.lock"]),
            Some(PackageManager::Uv),
        );
        assert_eq!(
            detect_package_manager(Ecosystem::Python, &["pyproject.toml", "poetry.lock"]),
            Some(PackageManager::Poetry),
        );
        assert_eq!(
            detect_package_manager(
                Ecosystem::Python,
                &["pyproject.toml", "poetry.lock", "uv.lock"],
            ),
            Some(PackageManager::Uv),
            "uv.lock wins over poetry.lock when both are present",
        );
    }

    #[test]
    fn detect_package_manager_python_defaults_to_none_when_only_pyproject() {
        assert_eq!(
            detect_package_manager(Ecosystem::Python, &["pyproject.toml"]),
            None,
            "pyproject.toml alone means 'use pip / setuptools' — the default baseline",
        );
    }

    #[test]
    fn detect_package_manager_rust_and_go_always_return_none() {
        // These ecosystems have a single toolchain (cargo / go); no
        // refinement possible.
        assert_eq!(
            detect_package_manager(Ecosystem::Rust, &["Cargo.toml", "Cargo.lock"]),
            None,
        );
        assert_eq!(
            detect_package_manager(Ecosystem::Go, &["go.mod", "go.sum"]),
            None,
        );
        // Even wild markers shouldn't fool it.
        assert_eq!(
            detect_package_manager(Ecosystem::Rust, &["Cargo.toml", "yarn.lock"]),
            None,
        );
    }

    #[test]
    fn package_manager_cli_name_is_canonical_lowercase() {
        assert_eq!(PackageManager::Yarn.cli_name(), "yarn");
        assert_eq!(PackageManager::Pnpm.cli_name(), "pnpm");
        assert_eq!(PackageManager::Bun.cli_name(), "bun");
        assert_eq!(PackageManager::Poetry.cli_name(), "poetry");
        assert_eq!(PackageManager::Uv.cli_name(), "uv");
    }

    #[test]
    fn baseline_plan_for_node_pnpm_uses_pnpm_argv() {
        let plan = baseline_plan_for(Ecosystem::Node, &["package.json", "pnpm-lock.yaml"]);
        // The test step must use `pnpm test`, NOT `npm test`.
        let test = plan.iter().find(|s| s.name == "test").expect("test");
        assert_eq!(
            test.command,
            vec!["pnpm", "test"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        );
        assert_eq!(test.tool, "pnpm");

        // The required-classification must be preserved (and we never lose
        // a required step vs. the npm baseline).
        let default_plan = baseline_plan(Ecosystem::Node);
        let refined_required: usize = plan.iter().filter(|s| s.required).count();
        let default_required: usize = default_plan.iter().filter(|s| s.required).count();
        assert!(
            refined_required >= default_required,
            "refined Node plan must not have fewer required steps ({refined_required}) than default ({default_required})",
        );
    }

    #[test]
    fn baseline_plan_for_node_yarn_uses_yarn_argv_and_required_count_dominates() {
        let plan = baseline_plan_for(Ecosystem::Node, &["package.json", "yarn.lock"]);
        let test = plan.iter().find(|s| s.name == "test").expect("test");
        assert_eq!(
            test.command,
            vec!["yarn", "test"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        );
        assert_eq!(test.tool, "yarn");

        // Every step name from the default baseline must still exist in
        // the refined plan (fail-closed: we don't lose checks).
        for s in baseline_plan(Ecosystem::Node) {
            assert!(
                plan.iter().any(|p| p.name == s.name),
                "refined Yarn plan missing step `{}` from default baseline",
                s.name,
            );
        }
    }

    #[test]
    fn baseline_plan_for_node_bun_uses_bun_argv() {
        let plan = baseline_plan_for(Ecosystem::Node, &["package.json", "bun.lockb"]);
        let test = plan.iter().find(|s| s.name == "test").expect("test");
        assert_eq!(test.command, vec!["bun", "test"]);
        assert_eq!(test.tool, "bun");
    }

    #[test]
    fn baseline_plan_for_python_poetry_uses_poetry_run_pytest() {
        let plan = baseline_plan_for(Ecosystem::Python, &["pyproject.toml", "poetry.lock"]);
        let test = plan.iter().find(|s| s.name == "test").expect("test");
        assert_eq!(
            test.command,
            vec!["poetry", "run", "pytest", "-q"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
        );
        assert_eq!(test.tool, "poetry");
        assert!(test.required, "test remains required under Poetry");
    }

    #[test]
    fn baseline_plan_for_python_uv_uses_uv_run_pytest() {
        let plan = baseline_plan_for(Ecosystem::Python, &["pyproject.toml", "uv.lock"]);
        let test = plan.iter().find(|s| s.name == "test").expect("test");
        assert_eq!(test.command, vec!["uv", "run", "pytest", "-q"]);
        assert_eq!(test.tool, "uv");
        assert!(test.required);
    }

    #[test]
    fn baseline_plan_for_python_poetry_audit_uses_pm_native_pip_audit_not_requirements_txt() {
        // Adversarial-review pin: Poetry projects typically do NOT ship
        // a `requirements.txt` file (the canonical lockfile is
        // `poetry.lock`). The audit step must therefore NOT hard-code
        // `pip-audit -r requirements.txt`; it must use the Poetry-native
        // audit path so pip-audit runs against the locked env.
        let plan = baseline_plan_for(Ecosystem::Python, &["pyproject.toml", "poetry.lock"]);
        let audit = plan.iter().find(|s| s.name == "audit").expect("audit");
        assert_eq!(
            audit.command,
            vec!["poetry", "run", "pip-audit"],
            "Poetry audit must use PM-native `poetry run pip-audit`, got {:?}",
            audit.command,
        );
        // No -r requirements.txt anywhere in the argv.
        assert!(
            !audit.command.iter().any(|a| a == "-r"),
            "Poetry audit must not use `-r`; got {:?}",
            audit.command,
        );
        assert!(
            !audit.command.iter().any(|a| a == "requirements.txt"),
            "Poetry audit must not reference requirements.txt; got {:?}",
            audit.command,
        );
        // Tool stays pip-audit (the binary doing the actual audit).
        assert_eq!(audit.tool, "pip-audit");
        // And it remains required: an unverified audit is a blocker.
        assert!(audit.required, "audit remains required under Poetry");
    }

    #[test]
    fn baseline_plan_for_python_uv_audit_uses_pm_native_pip_audit_not_requirements_txt() {
        // Same contract for uv: `uv.lock` is the canonical lockfile, no
        // `requirements.txt` typically present. The audit must use
        // `uv run pip-audit` so pip-audit reads the locked env directly.
        let plan = baseline_plan_for(Ecosystem::Python, &["pyproject.toml", "uv.lock"]);
        let audit = plan.iter().find(|s| s.name == "audit").expect("audit");
        assert_eq!(
            audit.command,
            vec!["uv", "run", "pip-audit"],
            "uv audit must use PM-native `uv run pip-audit`, got {:?}",
            audit.command,
        );
        assert!(
            !audit.command.iter().any(|a| a == "-r"),
            "uv audit must not use `-r`; got {:?}",
            audit.command,
        );
        assert!(
            !audit.command.iter().any(|a| a == "requirements.txt"),
            "uv audit must not reference requirements.txt; got {:?}",
            audit.command,
        );
        assert_eq!(audit.tool, "pip-audit");
        assert!(audit.required);
    }

    #[test]
    fn baseline_plan_for_python_default_audit_does_not_reference_requirements_txt() {
        // When no Poetry/uv markers are present, the default Python
        // baseline is used. The audit step in that baseline is just
        // `pip-audit` (no `-r requirements.txt`) — pip-audit walks the
        // active interpreter's installed packages. This is what the
        // Poetry/uv refinements diverge FROM (and why they now use the
        // PM-native form to ensure the audit reads the LOCKED env, not
        // whatever the local interpreter happens to have installed).
        let plan = baseline_plan_for(Ecosystem::Python, &["pyproject.toml", "requirements.txt"]);
        let audit = plan.iter().find(|s| s.name == "audit").expect("audit");
        assert_eq!(
            audit.command,
            vec!["pip-audit"],
            "default-Python audit (no Poetry/uv markers) is `pip-audit` (no -r), got {:?}",
            audit.command,
        );
        assert!(
            !audit.command.iter().any(|a| a == "-r"),
            "default Python audit must not use `-r`; got {:?}",
            audit.command,
        );
        assert!(
            !audit.command.iter().any(|a| a == "requirements.txt"),
            "default Python audit must not reference requirements.txt; got {:?}",
            audit.command,
        );
        assert_eq!(audit.tool, "pip-audit");
        assert!(audit.required);
    }

    #[test]
    fn baseline_plan_for_python_with_no_pm_marker_falls_back_to_default() {
        // pyproject.toml only (no poetry.lock, no uv.lock) -> default
        // Python baseline (pip / pytest / pip-audit).
        let refined = baseline_plan_for(Ecosystem::Python, &["pyproject.toml", "requirements.txt"]);
        let default = baseline_plan(Ecosystem::Python);
        assert_eq!(
            refined, default,
            "no PM marker present must yield the baseline unchanged",
        );
    }

    #[test]
    fn baseline_plan_for_node_with_no_pm_marker_falls_back_to_default() {
        // npm is the default; a project with package.json but no lockfile
        // gets the npm baseline unchanged.
        let refined = baseline_plan_for(Ecosystem::Node, &["package.json"]);
        let default = baseline_plan(Ecosystem::Node);
        assert_eq!(refined, default);
    }

    #[test]
    fn baseline_plan_for_rust_ignores_unrelated_lockfiles() {
        // Rust's `baseline_plan` does no PM refinement, so the output is
        // identical regardless of markers. Test this stays true.
        let refined_a = baseline_plan_for(Ecosystem::Rust, &["Cargo.toml"]);
        let refined_b = baseline_plan_for(Ecosystem::Rust, &["Cargo.toml", "yarn.lock"]);
        let default = baseline_plan(Ecosystem::Rust);
        assert_eq!(refined_a, default);
        assert_eq!(refined_b, default);
    }

    #[test]
    fn baseline_plan_for_never_drops_a_required_step() {
        // The blanket contract: for every ecosystem + PM combination we
        // can possibly refine, the refined plan's required-set must
        // SUPERSET the default plan's required-set. (failure here would
        // mean the marker-fuzzy refinement silently weakens the gate.)
        let combos: &[(&[&str], Ecosystem)] = &[
            (&["package.json"], Ecosystem::Node),
            (&["package.json", "pnpm-lock.yaml"], Ecosystem::Node),
            (&["package.json", "yarn.lock"], Ecosystem::Node),
            (&["package.json", "bun.lockb"], Ecosystem::Node),
            (&["pyproject.toml", "requirements.txt"], Ecosystem::Python),
            (&["pyproject.toml", "poetry.lock"], Ecosystem::Python),
            (&["pyproject.toml", "uv.lock"], Ecosystem::Python),
            (&["Cargo.toml"], Ecosystem::Rust),
            (&["go.mod"], Ecosystem::Go),
        ];
        for (markers, eco) in combos {
            let default = baseline_plan(*eco);
            let refined = baseline_plan_for(*eco, markers);
            let default_required: std::collections::BTreeSet<&str> = default
                .iter()
                .filter(|s| s.required)
                .map(|s| s.name.as_str())
                .collect();
            let refined_required: std::collections::BTreeSet<&str> = refined
                .iter()
                .filter(|s| s.required)
                .map(|s| s.name.as_str())
                .collect();
            for name in &default_required {
                assert!(
                    refined_required.contains(name),
                    "{markers:?} for {eco:?}: refined plan dropped required step `{name}`",
                );
            }
        }
    }

    #[test]
    fn detect_frameworks_returns_known_markers() {
        // Each marker maps to a known hint. The hints are stable, lowercase.
        assert_eq!(
            detect_frameworks(&["tsconfig.json"]),
            vec!["typescript".to_string()],
        );
        assert_eq!(
            detect_frameworks(&["next.config.ts"]),
            vec!["next.js".to_string()],
        );
        assert_eq!(
            detect_frameworks(&["vite.config.js"]),
            vec!["vite".to_string()],
        );
        assert_eq!(
            detect_frameworks(&["manage.py"]),
            vec!["django".to_string()],
        );
        assert_eq!(
            detect_frameworks(&["pyproject.toml"]),
            vec!["pyproject".to_string()],
        );
        assert_eq!(detect_frameworks(&["uv.lock"]), vec!["uv".to_string()],);
    }

    #[test]
    fn detect_frameworks_dedupes_across_alternate_marker_names() {
        // next.config.js and next.config.ts both map to "next.js" — must dedupe.
        let got = detect_frameworks(&[
            "package.json",
            "next.config.js",
            "next.config.ts",
            "next.config.mjs",
        ]);
        // Containment (rather than exact eq) because "package.json" may
        // also fire framework hints via overlap; in this dataset it does
        // not.
        assert_eq!(got, vec!["next.js".to_string()]);
    }

    #[test]
    fn detect_frameworks_is_deterministic_and_excludes_unrelated_files() {
        // Determinism contract: same input set in different orders MUST
        // yield the same output VECTOR (not just the same set) — order
        // matters because this is consumed by reporting layers that
        // surface the hints to reviewers. We iterate MARKER_HINTS in
        // table order as the outer loop, so input order cannot perturb
        // the output.
        let a = detect_frameworks(&["Cargo.toml", "tsconfig.json", "vite.config.ts", "README.md"]);
        let b = detect_frameworks(&["README.md", "vite.config.ts", "tsconfig.json", "Cargo.toml"]);

        // Exact vector equality: same input set in different orders must
        // produce IDENTICAL output (no input-order dependence).
        assert_eq!(
            a, b,
            "detect_frameworks must be input-order independent, got left={a:?} right={b:?}",
        );

        // Expected output: ORDER IS FIXED by the MARKER_HINTS table, not
        // the input. tsconfig.json ("typescript") comes before
        // vite.config.ts ("vite") in the table, so "typescript" precedes
        // "vite" in the output regardless of input order.
        assert_eq!(
            a,
            vec!["typescript".to_string(), "vite".to_string()],
            "output order must follow MARKER_HINTS table order (input-order independent)",
        );

        // No false positives on locked-down unrelated filenames.
        assert!(!a.iter().any(|h| h == "django"));
        assert!(!a.iter().any(|h| h == "flask"));
    }

    #[test]
    fn detect_frameworks_is_input_order_independent_under_filesystem_enumeration() {
        // Adversarial-review pin: marker_files in the wild can come from
        // filesystem enumeration, where ordering is platform- and
        // filesystem-dependent (ext4 vs APFS vs NTFS, dir readdir order,
        // case-folding filesystems). Output MUST be the same vector
        // regardless of permutation.
        let canonical = vec![
            "Cargo.toml",
            "package.json",
            "tsconfig.json",
            "next.config.ts",
            "vite.config.ts",
            "jest.config.js",
            "pyproject.toml",
            "poetry.lock",
            "uv.lock",
            "manage.py",
            "go.mod",
        ];

        // Reference: forward order.
        let reference = detect_frameworks(&canonical);

        // Reversed: exact reverse of the canonical list.
        let mut reversed = canonical.clone();
        reversed.reverse();
        assert_eq!(detect_frameworks(&reversed), reference);

        // Rotated-by-3: shift every element 3 positions left.
        let rotated: Vec<&str> = canonical
            .iter()
            .cycle()
            .skip(3)
            .take(canonical.len())
            .copied()
            .collect();
        assert_eq!(detect_frameworks(&rotated), reference);

        // Shuffled-by-swaps: a deterministic non-identity permutation
        // that swaps pairs to ensure the output isn't accidentally equal
        // only because of structural similarity.
        let swapped: Vec<&str> = vec![
            "go.mod",
            "pyproject.toml",
            "poetry.lock",
            "uv.lock",
            "manage.py",
            "package.json",
            "jest.config.js",
            "vite.config.ts",
            "next.config.ts",
            "tsconfig.json",
            "Cargo.toml",
        ];
        assert_eq!(detect_frameworks(&swapped), reference);

        // Single-element list (degenerate case): each marker individually
        // produces a one-element vector in table order.
        assert_eq!(
            detect_frameworks(&["pyproject.toml"]),
            vec!["pyproject".to_string()],
        );
        assert_eq!(
            detect_frameworks(&["tsconfig.json"]),
            vec!["typescript".to_string()],
        );
        // "Cargo.toml" is NOT a framework hint (Rust ecosystem marker
        // only). The empty vector must be exactly empty, not skipped.
        assert_eq!(detect_frameworks(&["Cargo.toml"]), Vec::<String>::new());

        // The reference itself, asserted for the audit trail: it must
        // equal the MARKER_HINTS table order restricted to entries whose
        // key is in the input. The table order is Node/TS first, then
        // Python, then Go, with internal dedup (typescript once, etc.).
        let mut want = vec![
            "typescript".to_string(),
            "next.js".to_string(),
            "vite".to_string(),
            "jest".to_string(),
            "django".to_string(),
            "poetry".to_string(),
            "pyproject".to_string(),
            "uv".to_string(),
            "go-modules".to_string(),
        ];
        // Sanity: the reference equals want.
        assert_eq!(reference, want);
        // Spot-check: every hint comes from the table, no surprise extras.
        want.sort();
        let mut got_sorted = reference.clone();
        got_sorted.sort();
        assert_eq!(got_sorted, want);
    }

    #[test]
    fn detect_frameworks_returns_empty_when_no_known_markers() {
        assert!(detect_frameworks(&[]).is_empty());
        assert!(detect_frameworks(&["README.md", "LICENSE"]).is_empty());
        // Substring confusion: a file named exactly "fake_vite.config.ts"
        // (no, we match exact) — verify the contract.
        assert!(detect_frameworks(&["fake_vite.config.ts"]).is_empty());
        assert!(detect_frameworks(&["vite.config.ts.bak"]).is_empty());
    }

    #[test]
    fn detect_repo_signals_groups_ecosystems_pms_and_frameworks() {
        let markers = [
            "Cargo.toml",
            "package.json",
            "yarn.lock",
            "tsconfig.json",
            "next.config.ts",
            "pyproject.toml",
            "uv.lock",
            "go.mod",
        ];
        let signals = detect_repo_signals(&markers);

        // Each ecosystem detected once, in canonical order.
        assert_eq!(
            signals.ecosystems,
            vec![
                Ecosystem::Rust,
                Ecosystem::Node,
                Ecosystem::Python,
                Ecosystem::Go,
            ],
        );

        // Two PMs detected (Yarn for Node, Uv for Python). Rust/Go: none.
        assert_eq!(
            signals.package_managers,
            vec![
                (Ecosystem::Node, PackageManager::Yarn),
                (Ecosystem::Python, PackageManager::Uv),
            ],
        );

        // Frameworks: typescript + next.js (deduped) + pyproject + uv (the
        // Python PM marker) + go-modules. Order is input-marker order; we
        // assert via set-containment on a sorted view for stability.
        let mut sorted_hints = signals.framework_hints.clone();
        sorted_hints.sort();
        assert_eq!(
            sorted_hints,
            vec![
                "go-modules".to_string(),
                "next.js".to_string(),
                "pyproject".to_string(),
                "typescript".to_string(),
                "uv".to_string(),
            ],
            "framework hints (sorted) must match the expected set",
        );

        // Dedup contract: the raw (unsorted) hint vector must equal its
        // deduped form, regardless of order. This is the cheap invariant
        // that pins down "we didn't push duplicates".
        let mut deduped = signals.framework_hints.clone();
        deduped.sort();
        deduped.dedup();
        let mut raw_sorted = signals.framework_hints.clone();
        raw_sorted.sort();
        assert_eq!(
            raw_sorted, deduped,
            "framework hints must already be deduped (sorted equality after dedup)",
        );
        assert!(signals.framework_hints.contains(&"typescript".to_string()));
        assert!(signals.framework_hints.contains(&"next.js".to_string()));
        assert!(signals.framework_hints.contains(&"uv".to_string()));
    }

    #[test]
    fn detect_repo_signals_default_for_npm_only_node_project() {
        let markers = ["package.json", "tsconfig.json", "vite.config.js"];
        let signals = detect_repo_signals(&markers);
        assert_eq!(signals.ecosystems, vec![Ecosystem::Node]);
        assert!(
            signals.package_managers.is_empty(),
            "npm-only project has no refined PM",
        );
        // typescript + vite are both detected.
        assert!(signals.framework_hints.contains(&"typescript".to_string()));
        assert!(signals.framework_hints.contains(&"vite".to_string()));
    }

    // ----- SLICE 4 (b) — GateReport honest-degradation reporting -----

    /// Helper to build a CompatibilityReport with explicit vectors.
    fn compat(
        runnable: &[&str],
        skipped: &[(&str, &str)],
        added_baseline: &[&str],
    ) -> CompatibilityReport {
        CompatibilityReport {
            runnable: runnable.iter().map(|s| (*s).to_string()).collect(),
            skipped: skipped
                .iter()
                .map(|(n, r)| ((*n).to_string(), (*r).to_string()))
                .collect(),
            added_baseline: added_baseline.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn gate_report_recomputes_status_from_results() {
        // Construct the report with results that imply Green, but pretend
        // a caller forced a stale Red. `GateReport::new` must recompute
        // from results and emit Green (no drift).
        let results = vec![
            result("fmt", true, StepOutcome::Passed),
            result("test", true, StepOutcome::Passed),
        ];
        let report = GateReport::new(
            results,
            compat(&["fmt", "test"], &[], &[]),
            GateMode::Strict,
        );
        assert_eq!(report.status, GateStatus::Green);
        assert!(report.is_passed());
        assert!(!report.is_failed());
    }

    #[test]
    fn gate_report_green_is_passed_and_required_count_matches_results() {
        let results = vec![
            result("fmt", true, StepOutcome::Passed),
            result("lint", true, StepOutcome::Passed),
            result("test", true, StepOutcome::Passed),
        ];
        let report = GateReport::new(
            results,
            compat(&["fmt", "lint", "test"], &[], &[]),
            GateMode::Strict,
        );
        assert_eq!(report.status, GateStatus::Green);
        assert!(report.is_passed());
        assert_eq!(report.passed_required_names(), vec!["fmt", "lint", "test"],);
    }

    #[test]
    fn gate_report_yellow_when_only_optional_steps_skipped() {
        let results = vec![
            result("fmt", true, StepOutcome::Passed),
            result(
                "audit",
                false,
                StepOutcome::Skipped {
                    reason: "tool `cargo-audit` not available".to_string(),
                },
            ),
            result("test", true, StepOutcome::Passed),
        ];
        let report = GateReport::new(
            results,
            compat(&["fmt", "test"], &[], &["audit"]),
            GateMode::Strict,
        );
        assert_eq!(
            report.status,
            GateStatus::Yellow {
                skipped: vec!["audit".to_string()],
            },
        );
        assert!(!report.is_passed());
        assert!(!report.is_failed());
    }

    #[test]
    fn gate_report_red_when_any_required_fails() {
        // A single required-failed step -> Red, even if everything else is
        // passed and even if some optional steps were skipped.
        let results = vec![
            result("fmt", true, StepOutcome::Passed),
            result(
                "audit",
                false,
                StepOutcome::Skipped {
                    reason: "tool `cargo-audit` not available".to_string(),
                },
            ),
            result("test", true, StepOutcome::Failed),
        ];
        let report = GateReport::new(
            results,
            compat(&["fmt", "test"], &[], &["audit"]),
            GateMode::Strict,
        );
        assert_eq!(
            report.status,
            GateStatus::Red {
                failures: vec!["test".to_string()],
            },
        );
        assert!(report.is_failed());
        assert!(!report.is_passed());
        // Required-Failed still surfaces as a failure; required-passed
        // names exclude it.
        assert_eq!(report.passed_required_names(), vec!["fmt"]);
    }

    #[test]
    fn gate_report_red_preserves_full_required_failure_set() {
        let results = vec![
            result("a", true, StepOutcome::Failed),
            result("b", true, StepOutcome::Failed),
            result("c", true, StepOutcome::Passed),
        ];
        let report = GateReport::new(
            results,
            compat(&["a", "b", "c"], &[], &[]),
            GateMode::Strict,
        );
        assert_eq!(
            report.status,
            GateStatus::Red {
                failures: vec!["a".to_string(), "b".to_string()],
            },
        );
    }

    #[test]
    fn gate_report_headline_badge_matches_status() {
        // Contract: headline must embed the right badge + verdict string.
        let green = GateReport::new(
            vec![result("fmt", true, StepOutcome::Passed)],
            compat(&["fmt"], &[], &[]),
            GateMode::Strict,
        );
        assert!(green.headline().contains("🟢"));
        assert!(green.headline().contains("GREEN"));

        let yellow = GateReport::new(
            vec![
                result("lint", true, StepOutcome::Passed),
                result(
                    "audit",
                    false,
                    StepOutcome::Skipped {
                        reason: "tool `x` not available".to_string(),
                    },
                ),
            ],
            compat(&["lint"], &[], &["audit"]),
            GateMode::Strict,
        );
        assert!(yellow.headline().contains("🟡"));
        assert!(yellow.headline().contains("YELLOW"));

        let red = GateReport::new(
            vec![result("test", true, StepOutcome::Failed)],
            compat(&["test"], &[], &[]),
            GateMode::Strict,
        );
        assert!(red.headline().contains("🔴"));
        assert!(red.headline().contains("RED"));
    }

    #[test]
    fn gate_report_to_pretty_includes_compatibility_breakdown_even_when_green() {
        // A Green report MUST attach the compatibility breakdown so the
        // audit trail is complete. The contract is set in the docstring:
        // an all-Green report without a breakdown is not honestly
        // auditable. With the fail-closed skip contract in effect, a
        // genuine Green report is only possible when the compatibility
        // `skipped` set is empty — so we exercise that path here.
        let report = GateReport::new(
            vec![
                result("fmt", true, StepOutcome::Passed),
                result("test", true, StepOutcome::Passed),
            ],
            compat(&["fmt", "test"], &[], &["deny"]),
            GateMode::Strict,
        );
        let pretty = report.to_pretty();
        // Verdict line.
        assert!(pretty.contains("🟢 GREEN"));
        // Breakdown header summary.
        assert!(pretty.contains("breakdown:"));
        assert!(pretty.contains("2 runnable"));
        assert!(pretty.contains("1 added-baseline"));
        assert!(pretty.contains("0 skipped"));
        // Per-step table.
        assert!(pretty.contains("PASSED"));
        assert!(pretty.contains("fmt"));
        assert!(pretty.contains("test"));
        // Compatibility section, with the added-baseline item surfaced.
        assert!(pretty.contains("compatibility:"));
        assert!(pretty.contains("runnable"));
        assert!(pretty.contains("added-baseline"));
        assert!(pretty.contains("deny"));
    }

    #[test]
    fn gate_report_skipped_compat_downgrades_green_to_yellow() {
        // Adversarial-review pin: the original bug let a report with
        // skipped CI jobs + all-passing baseline steps render Green. That
        // is dishonest — "Green = nothing skipped". A non-empty
        // compatibility.skipped set MUST downgrade Green to Yellow so
        // reviewers see that some CI jobs were not verified locally.
        let results = vec![
            result("fmt", true, StepOutcome::Passed),
            result("test", true, StepOutcome::Passed),
        ];
        let report = GateReport::new(
            results,
            compat(
                &["fmt", "test"],
                &[(
                    "integration",
                    "requires service containers (upstream CI verifies)",
                )],
                &["deny"],
            ),
            GateMode::Strict,
        );
        // The status must be Yellow (not Green) because CI skipped a job.
        match &report.status {
            GateStatus::Yellow { skipped } => {
                assert!(
                    skipped.iter().any(|n| n == "integration"),
                    "the skipped CI job name must surface in the Yellow skipped set, got {skipped:?}",
                );
            }
            other => panic!(
                "expected Yellow (skip-compat downgrade), got {other:?} — fail-closed posture broken"
            ),
        }
        assert!(!report.is_passed(), "skip-compat must block is_passed()");
        assert!(
            !report.is_failed(),
            "skip-compat downgrade is Yellow, not Red"
        );
        // The headline + pretty + compact renderings must reflect Yellow.
        assert!(report.headline().contains("🟡 YELLOW"));
        assert!(report.to_pretty().contains("🟡 YELLOW"));
        assert!(report.to_compact().contains("🟡 YELLOW"));
    }

    #[test]
    fn gate_report_skipped_compat_unions_into_existing_yellow() {
        // If results alone would already yield Yellow (e.g. an optional
        // step skipped because its tool is missing), the compatibility
        // skipped names are UNIONED into the Yellow.skipped list so the
        // reviewer sees the full unverified set, not just the in-process
        // one. Order is sorted for determinism.
        let results = vec![
            result("fmt", true, StepOutcome::Passed),
            result(
                "audit",
                false,
                StepOutcome::Skipped {
                    reason: "tool `cargo-audit` not available".to_string(),
                },
            ),
        ];
        let report = GateReport::new(
            results,
            compat(
                &["fmt"],
                &[("zeta", "requires CI secrets (upstream CI verifies)")],
                &["audit"],
            ),
            GateMode::Strict,
        );
        match &report.status {
            GateStatus::Yellow { skipped } => {
                // Both names surfaced (sorted, deduped).
                assert_eq!(
                    skipped,
                    &vec!["audit".to_string(), "zeta".to_string()],
                    "compat.skipped + results-skipped must be unioned (sorted+deduped) into Yellow.skipped, got {skipped:?}",
                );
            }
            other => panic!("expected Yellow with unioned skips, got {other:?}"),
        }
    }

    #[test]
    fn gate_report_skipped_compat_does_not_downgrade_red() {
        // Red dominates: a results-only Red must NOT be masked by an
        // empty compatibility.skipped (and conversely, a results-only
        // Red stays Red even when compat has skipped items).
        let results = vec![
            result("fmt", true, StepOutcome::Passed),
            result("test", true, StepOutcome::Failed),
        ];
        let report = GateReport::new(
            results,
            compat(
                &["fmt"],
                &[(
                    "integration",
                    "requires service containers (upstream CI verifies)",
                )],
                &["test"],
            ),
            GateMode::Strict,
        );
        // Red must be preserved end-to-end.
        assert_eq!(
            report.status,
            GateStatus::Red {
                failures: vec!["test".to_string()],
            },
        );
        assert!(report.is_failed());
        assert!(!report.is_passed());
    }

    #[test]
    fn gate_report_to_pretty_red_lists_required_failures() {
        let report = GateReport::new(
            vec![
                result("fmt", true, StepOutcome::Passed),
                result("test", true, StepOutcome::Failed),
            ],
            compat(&["fmt"], &[], &["test"]),
            GateMode::Strict,
        );
        let pretty = report.to_pretty();
        assert!(pretty.contains("🔴 RED"));
        assert!(pretty.contains("test"));
        assert!(pretty.contains("FAILED"));
    }

    #[test]
    fn gate_report_to_pretty_yellow_lists_skipped_with_reason() {
        let report = GateReport::new(
            vec![
                result("lint", true, StepOutcome::Passed),
                result(
                    "audit",
                    false,
                    StepOutcome::Skipped {
                        reason: "tool `cargo-audit` not available".to_string(),
                    },
                ),
            ],
            compat(&["lint"], &[], &["audit"]),
            GateMode::Strict,
        );
        let pretty = report.to_pretty();
        assert!(pretty.contains("🟡 YELLOW"));
        assert!(pretty.contains("SKIPPED"));
        assert!(pretty.contains("audit"));
        assert!(pretty.contains("tool `cargo-audit` not available"));
    }

    #[test]
    fn gate_report_to_pretty_marks_required_vs_optional_for_skip() {
        // The renderer marks required (`*`) vs optional (` `) on every
        // step line so reviewers can scan for blockers vs advisories.
        let report = GateReport::new(
            vec![
                result("required-step", true, StepOutcome::Passed),
                result(
                    "optional-step",
                    false,
                    StepOutcome::Skipped {
                        reason: "no tool".to_string(),
                    },
                ),
            ],
            compat(&["required-step"], &[], &["optional-step"]),
            GateMode::Strict,
        );
        let pretty = report.to_pretty();
        // The required-step line must have the `*` marker.
        let required_line = pretty
            .lines()
            .find(|l| l.contains("required-step"))
            .expect("required-step line");
        assert!(required_line.contains('*'));

        // The optional step line must NOT carry the `*` marker.
        let optional_line = pretty
            .lines()
            .find(|l| l.contains("optional-step"))
            .expect("optional-step line");
        assert!(!optional_line.contains('*'));
    }

    #[test]
    fn gate_report_to_compact_is_one_line_and_labels_color() {
        let report = GateReport::new(
            vec![
                result("fmt", true, StepOutcome::Passed),
                result(
                    "audit",
                    false,
                    StepOutcome::Skipped {
                        reason: "no tool".to_string(),
                    },
                ),
            ],
            compat(&["fmt"], &[], &["audit"]),
            GateMode::Strict,
        );
        let compact = report.to_compact();
        assert!(compact.lines().count() <= 2, "compact must fit on one line");
        assert!(compact.contains("🟡 YELLOW"));
        assert!(compact.contains("required=1"));
        assert!(compact.contains("passed=1"));
        assert!(compact.contains("optional=1"));
        assert!(compact.contains("passed=0"));
    }

    #[test]
    fn gate_report_fail_closed_required_skipped_under_strict_runs_to_red() {
        // End-to-end: the runner with a missing REQUIRED tool under Strict
        // mode produces a Skipped-with-reason that the runner core
        // upgrades to a Failed outcome — which then aggregates to Red.
        // This test pins down the full integration (run_plan + aggregate +
        // GateReport) so the fail-closed contract is verified
        // end-to-end, not just at the aggregator level.
        let plan = vec![
            s("fmt", true),
            GateStep {
                name: "deny".to_string(),
                category: StepCategory::Security,
                command: vec!["cargo-deny", "check"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
                tool: "cargo-deny".to_string(),
                required: true,
            },
        ];
        // Only `fmt` is available; `deny` is missing.
        let env = FakeEnv::new(&["fmt"]);
        let (results, status) = run_plan(&plan, GateMode::Strict, &env);

        assert_eq!(
            status,
            GateStatus::Red {
                failures: vec!["deny".to_string()],
            },
        );

        let report = GateReport::new(
            results.clone(),
            compat(&["fmt"], &[], &["deny"]),
            GateMode::Strict,
        );
        assert_eq!(
            report.status,
            GateStatus::Red {
                failures: vec!["deny".to_string()],
            },
        );
        assert!(report.is_failed());
        // Headline + pretty must reflect Red.
        assert!(report.headline().contains("🔴 RED"));
        let pretty = report.to_pretty();
        assert!(pretty.contains("🔴 RED"));
        assert!(pretty.contains("deny"));
    }

    #[test]
    fn gate_report_local_iterate_required_missing_tool_runs_to_yellow() {
        // Mirror image of the above: in LocalIterate mode, a missing
        // REQUIRED tool is recorded as Skipped -> Yellow. Fail-closed
        // posture is preserved: this is the inner-loop mode, deliberately
        // degraded, and the report says so.
        let plan = vec![s("fmt", true), s("deny", true)];
        let env = FakeEnv::new(&["fmt"]);
        let (results, status) = run_plan(&plan, GateMode::LocalIterate, &env);

        assert_eq!(
            status,
            GateStatus::Yellow {
                skipped: vec!["deny".to_string()],
            },
        );

        let report = GateReport::new(
            results,
            compat(&["fmt"], &[], &["deny"]),
            GateMode::LocalIterate,
        );
        assert!(!report.is_passed());
        assert!(!report.is_failed());
        assert!(report.headline().contains("🟡 YELLOW"));
        // Z-5: LocalIterate verdict is NOT authoritative.
        assert!(!report.was_strict());
        assert!(!report.is_authoritative_pass());
    }

    #[test]
    fn gate_report_does_not_round_trip_red_to_green_or_yellow() {
        // Adversarial: no rendering path should ever mutate Red to a
        // friendlier verdict. We construct a Red report and assert all
        // three renderings (headline, pretty, compact) carry Red, and
        // `is_passed()` stays false.
        let results = vec![result("test", true, StepOutcome::Failed)];
        let report = GateReport::new(results, compat(&["test"], &[], &[]), GateMode::Strict);
        assert_eq!(
            report.status,
            GateStatus::Red {
                failures: vec!["test".to_string()],
            },
        );
        assert!(report.headline().contains("🔴 RED"));
        assert!(report.to_pretty().contains("🔴 RED"));
        assert!(report.to_compact().contains("🔴 RED"));
        assert!(!report.is_passed());
    }

    // -------------------------------------------------------------------
    //  Z-5 / Z-6 / Z-7 — adversarial-review pins for gate-gaming holes.
    //
    //  Each test below was authored against the BUGGY behavior and is
    //  expected to FAIL on the pre-fix code. After the fix, every test
    //  in this block must pass.
    // -------------------------------------------------------------------

    // ----- Z-6: empty / all-optional plan aggregates to Green -------------

    #[test]
    fn z6_aggregate_empty_results_is_not_green() {
        // Adversarial-review pin (Z-6): a plan with ZERO required steps
        // (empty plan) must NOT aggregate to Green. "No work to check"
        // is silently "passed" today — that is dishonest.
        let results: Vec<StepResult> = vec![];
        let status = aggregate(&results);
        assert!(
            !matches!(status, GateStatus::Green),
            "empty results must not be Green (Z-6), got {status:?}",
        );
        assert!(
            matches!(status, GateStatus::Inconclusive),
            "empty results must aggregate to Inconclusive, got {status:?}",
        );
    }

    #[test]
    fn z6_aggregate_only_optional_passed_is_not_green() {
        // Adversarial-review pin (Z-6): a plan with only OPTIONAL steps
        // (all passing) must NOT aggregate to Green — there were zero
        // required checks, so the gate cannot certify a pass.
        let results = vec![
            result("fmt", false, StepOutcome::Passed),
            result("lint", false, StepOutcome::Passed),
            result("audit", false, StepOutcome::Passed),
        ];
        let status = aggregate(&results);
        assert!(
            !matches!(status, GateStatus::Green),
            "all-optional + all-passed must not be Green (Z-6), got {status:?}",
        );
        assert!(
            matches!(status, GateStatus::Inconclusive),
            "all-optional + all-passed must aggregate to Inconclusive, got {status:?}",
        );
    }

    #[test]
    fn z6_aggregate_only_optional_skipped_is_not_green_either() {
        // Adversarial-review pin (Z-6): a plan with only OPTIONAL steps
        // (any combination of pass/fail/skip) must NOT be Green — there
        // were zero required checks.
        let results = vec![
            result("fmt", false, StepOutcome::Passed),
            result(
                "audit",
                false,
                StepOutcome::Skipped {
                    reason: "tool `cargo-audit` not available".to_string(),
                },
            ),
        ];
        let status = aggregate(&results);
        assert!(
            !matches!(status, GateStatus::Green),
            "all-optional + mixed outcomes must not be Green (Z-6), got {status:?}",
        );
    }

    #[test]
    fn z6_aggregate_real_required_passed_still_yields_green() {
        // Counter-test for the Z-6 fix: a real plan with at least one
        // required step that PASSED must still aggregate to Green. The
        // fix must not break honest plans.
        let results = vec![
            result("fmt", false, StepOutcome::Passed),
            result("lint", true, StepOutcome::Passed),
            result("test", true, StepOutcome::Passed),
        ];
        assert_eq!(aggregate(&results), GateStatus::Green);
    }

    #[test]
    fn z6_gate_report_empty_plan_is_not_passed() {
        // Adversarial-review pin (Z-6): GateReport::new over an empty
        // results list must NOT report `is_passed() == true`. The
        // headline must reflect Inconclusive, not Green.
        let report = GateReport::new(vec![], CompatibilityReport::default(), GateMode::Strict);
        assert!(
            !report.is_passed(),
            "empty plan must not be is_passed() (Z-6), got status {:?}",
            report.status,
        );
        assert!(
            report.is_inconclusive(),
            "empty plan must be is_inconclusive(), got status {:?}",
            report.status,
        );
        assert!(!report.is_failed());
        assert!(report.headline().contains("INCONCLUSIVE"));
        assert!(!report.headline().contains("GREEN"));
    }

    #[test]
    fn z6_gate_report_all_optional_passed_is_not_passed() {
        // Adversarial-review pin (Z-6): GateReport::new over an
        // all-optional, all-passed results list must NOT report
        // `is_passed() == true`.
        let results = vec![
            result("fmt", false, StepOutcome::Passed),
            result("audit", false, StepOutcome::Passed),
        ];
        let report = GateReport::new(results, CompatibilityReport::default(), GateMode::Strict);
        assert!(
            !report.is_passed(),
            "all-optional + all-passed must not be is_passed() (Z-6), got status {:?}",
            report.status,
        );
        assert!(report.is_inconclusive());
    }

    // ----- Z-5: LocalIterate downgrades missing REQUIRED tools to skips ---

    #[test]
    fn z5_gate_report_stamps_mode_and_was_strict_accessor() {
        // Adversarial-review pin (Z-5): the gate runner must record the
        // mode it ran in. A consumer must be able to tell a
        // LocalIterate verdict from a Strict one via `was_strict()`.
        let plan = vec![s("fmt", true)];
        let env = FakeEnv::new(&["fmt"]);
        let (results, _status) = run_plan(&plan, GateMode::LocalIterate, &env);
        let report = GateReport::new(
            results,
            CompatibilityReport::default(),
            GateMode::LocalIterate,
        );
        // mode is stamped and accessible.
        assert_eq!(report.mode(), GateMode::LocalIterate);
        assert!(
            !report.was_strict(),
            "LocalIterate report must not report was_strict() (Z-5)",
        );
        // The "this is an authoritative pass" combo accessor must agree.
        assert!(
            !report.is_authoritative_pass(),
            "LocalIterate + Green is never an authoritative pass (Z-5)",
        );
    }

    #[test]
    fn z5_gate_report_local_iterate_zero_required_tools_is_not_authoritative() {
        // Adversarial-review pin (Z-5): under LocalIterate, a REQUIRED
        // step whose tool is unavailable routes to Skipped (Yellow)
        // instead of Failed (Red). That verdict must be marked
        // non-authoritative — callers keying on Strict-Green must
        // reject it.
        let plan = vec![s("fmt", true), s("deny", true)];
        let env = FakeEnv::new(&[]); // zero required tools
        let (results, status) = run_plan(&plan, GateMode::LocalIterate, &env);
        let report = GateReport::new(
            results,
            CompatibilityReport::default(),
            GateMode::LocalIterate,
        );

        // Sanity: LocalIterate + missing REQUIRED tool -> Skipped -> Yellow.
        assert!(
            matches!(status, GateStatus::Yellow { .. }),
            "LocalIterate + missing required tool must be Yellow, got {status:?}",
        );

        // The mode stamp makes the verdict non-authoritative.
        assert!(
            !report.was_strict(),
            "LocalIterate report must not be was_strict()"
        );
        assert!(
            !report.is_authoritative_pass(),
            "LocalIterate zero-tools report must NOT be an authoritative pass (Z-5)",
        );

        // is_passed() may remain true under LocalIterate only when the
        // status is Green (it is not, here — it is Yellow).
        assert!(!report.is_passed(), "Yellow is not is_passed() (Z-5)");
    }

    #[test]
    fn z5_gate_report_strict_full_pass_is_authoritative() {
        // Counter-test for Z-5: under Strict with all required steps
        // passing and all required tools present, the verdict IS an
        // authoritative pass.
        let plan = vec![s("fmt", true), s("lint", true)];
        let env = FakeEnv::new(&["fmt", "lint"]);
        let (results, status) = run_plan(&plan, GateMode::Strict, &env);
        let report = GateReport::new(results, CompatibilityReport::default(), GateMode::Strict);

        assert_eq!(status, GateStatus::Green);
        assert!(
            report.was_strict(),
            "Strict report must report was_strict()"
        );
        assert!(
            report.is_authoritative_pass(),
            "Strict + all-required-passed IS an authoritative pass",
        );
        assert!(report.is_passed());
    }

    // ----- Z-7: repo-authored CI job displaces baseline safety steps ------

    #[test]
    fn z7_derive_plan_baseline_test_step_runs_even_when_ci_test_job_claims_to_cover_it() {
        // Adversarial-review pin (Z-7): a repo-authored CI `test` job
        // with a no-op command (`["true"]`) must NOT displace the
        // baseline test step. Baseline safety required steps ALWAYS
        // run, even if CI claims to cover the same category. The
        // CI-derived step is additive, never a replacement.
        let ci_jobs = vec![CiJob {
            name: "fake-test".to_string(),
            command: vec!["true".to_string()],
            tool: "true".to_string(),
            category: StepCategory::Test,
            needs_secrets: false,
            needs_services: false,
            self_hosted: false,
        }];
        let baseline = vec![GateStep {
            name: "test".to_string(),
            category: StepCategory::Test,
            command: vec!["cargo", "test", "--all-targets"]
                .into_iter()
                .map(String::from)
                .collect(),
            tool: "cargo".to_string(),
            required: true,
        }];
        let (plan, _report) = derive_plan(&ci_jobs, &baseline);

        // The baseline `test` step must be present + required in the plan.
        let baseline_test = plan
            .iter()
            .find(|s| s.name == "test")
            .expect("baseline test step must be present");
        assert!(
            baseline_test.required,
            "baseline test step must be required (Z-7), got {plan:?}",
        );
        // And it must be the REAL cargo test, not the CI's `true` no-op.
        assert_eq!(
            baseline_test.command,
            vec!["cargo", "test", "--all-targets"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>(),
            "baseline cargo test must NOT be displaced by a no-op CI job (Z-7)",
        );
        assert_eq!(baseline_test.tool, "cargo");
        // The CI's fake job is also still in the plan (additive).
        assert!(
            plan.iter().any(|s| s.name == "fake-test"),
            "the CI-derived job must still appear in the plan additively, got {plan:?}",
        );
    }
}
