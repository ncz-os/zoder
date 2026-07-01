//! CI-parity gate engine: pure planning core.
//!
//! This module is the SLICE 1 of the gate engine. It is intentionally
//! pure — no subprocess execution and no CI-file parsing here, those
//! belong to later slices. What lives here is the data model
//! (ecosystems, steps, outcomes), marker-based ecosystem detection,
//! baseline OSS-hygiene plans per ecosystem, and the Green/Yellow/Red
//! aggregation of step results.
//!
//! Everything is deterministic and unit-tested so the planning layer
//! can be reasoned about without spinning a process.

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

/// Aggregate step results into the honest Green/Yellow/Red status:
///  - Red if ANY required step outcome is Failed (failures = names of the
///    required steps that Failed, in input order).
///  - else Yellow if ANY step (required or optional) outcome is Skipped
///    (skipped = names of ALL skipped steps, in input order).
///  - else Green.
///
/// Note: a FAILED *optional* step does NOT turn the gate Red or Yellow —
/// only required-Failed => Red, any-Skipped => Yellow.
pub fn aggregate(results: &[StepResult]) -> GateStatus {
    let mut required_failures: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for r in results {
        match &r.outcome {
            StepOutcome::Failed => {
                if r.required {
                    required_failures.push(r.step_name.clone());
                }
                // optional Failed -> intentionally ignored (advisory).
            }
            StepOutcome::Skipped { .. } => {
                skipped.push(r.step_name.clone());
            }
            StepOutcome::Passed => {}
        }
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
}
