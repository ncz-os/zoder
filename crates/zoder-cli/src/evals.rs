//! Behavioural, model-backed evaluation of the loop itself.
//!
//! WHAT THIS IS FOR
//!
//! `zoder perf` reports what already happened. This RUNS a defined suite so two
//! configurations can be compared deliberately: swap the author, swap the
//! reviewer, change the iteration budget, and hold everything else fixed.
//!
//! WHY IT IS SHAPED THIS WAY
//!
//! The shell harness this replaces re-implemented what a test runner gives you,
//! and got each piece wrong at least once in production:
//!
//!   * it counted ATTEMPTS as results, so a host too busy to run produced rows
//!     that read as total model failure;
//!   * it was edited while running, and bash reads a script lazily by byte
//!     offset, so a deploy corrupted a round silently;
//!   * it scored an unreadable artifact as a failure rather than as unknown.
//!
//! Every one of those is the same bug: an apparatus problem recorded as a model
//! result. So the rules here are explicit rather than emergent:
//!
//!   1. A run that did not happen is NOT a data point. It is recorded with its
//!      reason and excluded from rates.
//!   2. Each run gets its own clone. Runs never share a working tree.
//!   3. Each run is a separate process, so a panic or a hang is isolated to it.
//!   4. The index records the CONFIGURATION as well as the outcome -- which
//!      models, which cap, which base -- because a rate without its conditions
//!      is not interpretable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub(crate) struct Suite {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default, rename = "case")]
    pub cases: Vec<Case>,
    /// Configurations to compare. With none, a single run uses the defaults.
    #[serde(default, rename = "arm")]
    pub arms: Vec<Arm>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Defaults {
    pub max_iters: Option<usize>,
    pub check: Option<String>,
    pub agent_timeout_secs: Option<u64>,
    pub loop_timeout_secs: Option<u64>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Case {
    pub name: String,
    /// Repository to clone for each run. Cloned, never worked in directly.
    pub repo: String,
    /// Commit/branch each run starts from. Pinning this is what makes two arms
    /// comparable; leaving it at a moving branch means the task changes under
    /// you between arms.
    #[serde(default = "default_base")]
    pub base: String,
    /// File containing the task text.
    pub task: String,
    pub check: Option<String>,
}

fn default_base() -> String {
    "master".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Arm {
    /// Label used in the index and the summary. Required: an arm without a name
    /// cannot be referred to in a result.
    pub name: String,
    pub author: String,
    pub reviewer: String,
    pub max_iters: Option<usize>,
}

#[derive(Debug, Serialize)]
struct RunRow {
    case: String,
    arm: String,
    author: String,
    reviewer: String,
    base: String,
    max_iters: usize,
    /// `true` only when the loop actually ran to a verdict.
    ran: bool,
    /// Why a run did not happen. Present only when `ran` is false, and the
    /// reason is preserved rather than collapsed into a failure.
    skipped_reason: Option<String>,
    resolved: Option<bool>,
    iterations: Option<u64>,
    final_verdict: Option<String>,
    diff_lines: Option<u64>,
    tool_calls: Option<u64>,
    duration_ms: Option<u64>,
    workdir: String,
    artifact: Option<String>,
}

pub(crate) struct EvalArgs {
    pub suite: PathBuf,
    pub filter: Option<String>,
    pub out: Option<PathBuf>,
    pub dry_run: bool,
}

/// Extract the loop's JSON payload from mixed stdout.
///
/// `zoder loop` interleaves human progress lines with the final object, so a
/// whole-buffer parse fails on output that is perfectly fine. Returning `None`
/// here means UNKNOWN, never failure -- the caller records it as a run that did
/// not produce a readable result rather than as a model that did nothing.
fn extract_payload(stdout: &str) -> Option<serde_json::Value> {
    let start = stdout.find('{')?;
    let end = stdout.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&stdout[start..=end]).ok()
}

fn clone_case(repo: &str, base: &str, dest: &Path) -> anyhow::Result<()> {
    let st = Command::new("git")
        .args(["clone", "--quiet", repo])
        .arg(dest)
        .status()
        .context("git clone failed to start")?;
    if !st.success() {
        return Err(anyhow!("git clone {repo} failed"));
    }
    let st = Command::new("git")
        .arg("-C")
        .arg(dest)
        .args(["checkout", "--quiet", base])
        .status()
        .context("git checkout failed to start")?;
    if !st.success() {
        return Err(anyhow!("git checkout {base} failed"));
    }
    Ok(())
}

pub(crate) fn cmd_eval(cli: &crate::Cli, args: EvalArgs) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(&args.suite)
        .with_context(|| format!("reading suite {}", args.suite.display()))?;
    let suite: Suite =
        toml::from_str(&text).with_context(|| format!("parsing suite {}", args.suite.display()))?;

    if suite.cases.is_empty() {
        return Err(anyhow!("suite defines no [[case]] entries"));
    }
    // A suite with no arms still runs once, but the arm has to be nameable so
    // the result says what produced it.
    let arms: Vec<Arm> = if suite.arms.is_empty() {
        return Err(anyhow!(
            "suite defines no [[arm]] entries; an arm names the author/reviewer pair \
             a result belongs to, and a result without one cannot be attributed"
        ));
    } else {
        suite.arms.clone()
    };

    let cases: Vec<&Case> = suite
        .cases
        .iter()
        .filter(|c| args.filter.as_ref().is_none_or(|f| c.name.contains(f)))
        .collect();
    if cases.is_empty() {
        println!("no cases match filter");
        return Ok(());
    }

    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let out = args.out.unwrap_or_else(|| {
        crate::agentic::jobs_dir()
            .parent()
            .unwrap_or(Path::new("."))
            .join("evals")
            .join(&stamp)
    });
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let index = out.join("runs.jsonl");
    // APPEND AS EACH RUN COMPLETES, never once at the end. A suite is long and
    // unattended by design; buffering the index until the last run means a
    // crash, a kill, or a host reboot discards every result that had already
    // been earned. Each line is a complete record on its own.
    let mut index_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&index)
        .with_context(|| format!("opening {}", index.display()))?;

    let exe = std::env::current_exe().context("locating the zoder binary to re-invoke")?;
    let total = cases.len() * arms.len();
    eprintln!(
        "[eval] {} case(s) x {} arm(s) = {total} run(s); artifacts -> {}",
        cases.len(),
        arms.len(),
        out.display()
    );

    let mut rows: Vec<RunRow> = Vec::new();
    // Flush after every write: an unflushed buffer is the same data loss the
    // append is meant to prevent.
    let record = |f: &mut std::fs::File, r: &RunRow| {
        use std::io::Write;
        if let Ok(line) = serde_json::to_string(r) {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    };

    for case in &cases {
        for arm in &arms {
            let cap = arm.max_iters.or(suite.defaults.max_iters).unwrap_or(3);
            let check = case.check.clone().or_else(|| suite.defaults.check.clone());
            let wd = out.join(format!("{}__{}", case.name, arm.name));

            let mut row = RunRow {
                case: case.name.clone(),
                arm: arm.name.clone(),
                author: arm.author.clone(),
                reviewer: arm.reviewer.clone(),
                base: case.base.clone(),
                max_iters: cap,
                ran: false,
                skipped_reason: None,
                resolved: None,
                iterations: None,
                final_verdict: None,
                diff_lines: None,
                tool_calls: None,
                duration_ms: None,
                workdir: wd.display().to_string(),
                artifact: None,
            };

            if args.dry_run {
                row.skipped_reason = Some("dry-run".into());
                eprintln!("[eval] would run {} / {}", case.name, arm.name);
                record(&mut index_file, &row);
                rows.push(row);
                continue;
            }

            if let Err(e) = clone_case(&case.repo, &case.base, &wd) {
                // Setup failure is an APPARATUS problem. It is recorded with its
                // reason and excluded from rates -- calling it a model failure is
                // the exact mistake this runner exists to stop making.
                row.skipped_reason = Some(format!("setup: {e}"));
                eprintln!("[eval] SKIP {} / {}: {e}", case.name, arm.name);
                record(&mut index_file, &row);
                rows.push(row);
                continue;
            }

            let mut cmd = Command::new(&exe);
            cmd.arg("loop")
                .arg("--json")
                .arg("-C")
                .arg(&wd)
                .args(["--agent", &arm.author, "-m", &arm.author])
                .args(["--reviewer", &arm.reviewer])
                .arg("--allow-paid")
                .args(["--max-iters", &cap.to_string()])
                .args(["-i", &case.task]);
            if let Some(c) = &check {
                cmd.args(["--check", c]);
            }
            if let Some(t) = suite.defaults.agent_timeout_secs {
                cmd.args(["--agent-timeout", &t.to_string()]);
            }
            if let Some(t) = suite.defaults.loop_timeout_secs {
                cmd.args(["--loop-timeout", &t.to_string()]);
            }
            if let Some(t) = suite.defaults.max_tokens {
                cmd.args(["--max-tokens", &t.to_string()]);
            }

            eprintln!("[eval] run {} / {} (cap {cap})", case.name, arm.name);
            let output = match cmd.output() {
                Ok(o) => o,
                Err(e) => {
                    row.skipped_reason = Some(format!("spawn: {e}"));
                    record(&mut index_file, &row);
                    rows.push(row);
                    continue;
                }
            };
            let stdout = String::from_utf8_lossy(&output.stdout);
            let artifact = wd.join("result.json");
            let _ = std::fs::write(&artifact, stdout.as_bytes());
            row.artifact = Some(artifact.display().to_string());

            match extract_payload(&stdout) {
                None => {
                    // Unreadable output is UNKNOWN, not zero.
                    row.skipped_reason = Some("no readable loop payload in stdout".into());
                }
                Some(d) => {
                    row.ran = true;
                    row.resolved = d.get("resolved").and_then(|v| v.as_bool());
                    row.iterations = d.get("iterations").and_then(|v| v.as_u64());
                    row.final_verdict = d
                        .get("final_verdict")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    row.duration_ms = d.get("duration_ms").and_then(|v| v.as_u64());
                    if let Some(log) = d.get("log").and_then(|v| v.as_array()) {
                        row.diff_lines = Some(
                            log.iter()
                                .filter_map(|x| x.get("diff_lines").and_then(|v| v.as_u64()))
                                .max()
                                .unwrap_or(0),
                        );
                        row.tool_calls = Some(
                            log.iter()
                                .filter_map(|x| x.get("tool_calls").and_then(|v| v.as_u64()))
                                .sum(),
                        );
                    }
                }
            }
            record(&mut index_file, &row);
            rows.push(row);
        }
    }

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    // Per-arm summary. The denominator is runs that ACTUALLY RAN, and the
    // skipped count is printed beside it -- a rate over attempted runs would
    // quietly fold setup failures into the model's score.
    let mut by_arm: BTreeMap<&str, (u64, u64, u64)> = BTreeMap::new();
    for r in &rows {
        let e = by_arm.entry(r.arm.as_str()).or_insert((0, 0, 0));
        if r.ran {
            e.1 += 1;
            if r.resolved == Some(true) {
                e.0 += 1;
            }
        } else {
            e.2 += 1;
        }
    }
    println!(
        "\n{:<20} {:>10} {:>10} {:>10}",
        "arm", "resolved", "ran", "skipped"
    );
    for (arm, (ok, ran, skipped)) in &by_arm {
        let rate = if *ran > 0 {
            format!("{:.0}%", 100.0 * *ok as f64 / *ran as f64)
        } else {
            "n/a".to_string()
        };
        println!("{arm:<20} {rate:>10} {ran:>10} {skipped:>10}");
    }
    println!("\nindex: {}", index.display());
    if rows.iter().any(|r| !r.ran && r.skipped_reason.is_some()) {
        println!(
            "NOTE: skipped runs are excluded from the rates above, with reasons in the index."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loop interleaves progress lines with its JSON payload. Parsing the
    /// whole buffer fails on output that is completely fine.
    #[test]
    fn payload_is_extracted_from_mixed_stdout() {
        let s = "[loop] iter 1/2: author…\n[loop] iter 1: review…\n{\"resolved\":true,\"iterations\":2}\n";
        let d = extract_payload(s).expect("payload must be recoverable from mixed output");
        assert_eq!(d["resolved"], serde_json::json!(true));
        assert_eq!(d["iterations"], serde_json::json!(2));
    }

    /// Unreadable output must be UNKNOWN, never a zero score. Returning a
    /// default here is how an apparatus failure becomes a model result.
    #[test]
    fn unreadable_output_is_unknown_not_failure() {
        assert!(extract_payload("").is_none());
        assert!(extract_payload("[loop] crashed before emitting anything").is_none());
        assert!(extract_payload("{not json at all}").is_none());
    }

    /// A suite whose arms are unnamed cannot attribute a result, so it is
    /// rejected up front rather than producing rows nobody can interpret.
    #[test]
    fn suite_parses_cases_and_arms() {
        let t = r#"
[defaults]
max_iters = 6
check = "cargo check"

[[case]]
name = "dry-run"
repo = "/tmp/x"
base = "master"
task = "t.txt"

[[arm]]
name = "qwen-vs-gemma"
author = "coder"
reviewer = "reviewer"
"#;
        let s: Suite = toml::from_str(t).expect("suite must parse");
        assert_eq!(s.cases.len(), 1);
        assert_eq!(s.arms.len(), 1);
        assert_eq!(s.defaults.max_iters, Some(6));
        assert_eq!(s.cases[0].base, "master");
    }
}
