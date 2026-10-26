//! Deterministic context benchmark: the levers over transcripts shaped like
//! real `zoder` jobs.
//!
//! The brief for the 2026-10-25 study asks for a benchmark over past job
//! briefs on deepseek-flash and MiniMax-M3 recording tokens per task, turns,
//! cost, wall time and success. This binary is the half of that benchmark that
//! costs nothing and can be re-run by anyone: it replays three real job shapes
//! (a small fix, a test repair, a 16-chunk review) through the real levers and
//! reports what each lever saves. It never calls a provider, so its numbers are
//! deterministic and reviewable in CI.
//!
//! What it deliberately does NOT do is pretend to be the quality measurement.
//! Token savings are a *mechanical* result here; "no drop in task success" and
//! unchanged review verdicts need the paid runs, and
//! `docs/CONTEXT-OPTIMIZATION.md` says so rather than quoting a number it does
//! not have.
//!
//! Usage:
//!
//! ```text
//! cargo run -p zoder-core --bin context_bench            # text report
//! cargo run -p zoder-core --bin context_bench -- --json  # machine-readable
//! ```

use std::fmt::Write as _;

use zoder_core::context_opt::measure::{
    compose, ledger_line, summarise, CallRecord, Part, Segment,
};
use zoder_core::context_opt::{cap_tool_output, elide_stale, estimate_tokens, Levers, Turn};

/// A job whose transcript is generated deterministically.
struct Job {
    name: &'static str,
    brief: &'static str,
    /// Turns of `(role, kind, bytes)`; `kind` picks what the body looks like.
    plan: Vec<(&'static str, Kind, usize)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A file read: a plausible Rust source file.
    Read,
    /// A test log with a failure at the tail.
    TestLog,
    /// A unified diff.
    Diff,
    /// Assistant reasoning/answer text.
    Prose,
}

fn file_body(path: &str, bytes: usize) -> String {
    let mut out = format!("// {path}\nuse std::collections::BTreeMap;\n\n");
    let mut i = 0usize;
    while out.len() < bytes {
        let _ = writeln!(
            out,
            "pub fn helper_{i}(input: &str) -> Option<usize> {{\n    let map: BTreeMap<&str, usize> = BTreeMap::new();\n    map.get(input).copied().or_else(|| Some(input.len()))\n}}\n"
        );
        i += 1;
    }
    out
}

fn test_log(bytes: usize) -> String {
    let mut out = String::from("running 412 tests\n");
    let mut i = 0usize;
    while out.len() < bytes {
        let _ = writeln!(out, "test module::case_{i} ... ok");
        i += 1;
    }
    out.push_str(
        "\nfailures:\n\n---- module::case_17 stdout ----\nthread 'module::case_17' panicked at \
         crates/zoder-core/src/context.rs:412:9:\nassertion `left == right` failed\n  left: 3\n \
         right: 4\n\ntest result: FAILED. 411 passed; 1 failed\n",
    );
    out
}

fn diff_body(bytes: usize) -> String {
    let mut out = String::from(
        "diff --git a/crates/zoder-core/src/router.rs b/crates/zoder-core/src/router.rs\n",
    );
    out.push_str("--- a/crates/zoder-core/src/router.rs\n+++ b/crates/zoder-core/src/router.rs\n");
    let mut i = 0usize;
    while out.len() < bytes {
        let _ = writeln!(
            out,
            "@@ -{i},4 +{i},4 @@\n-    let pick = rank(model);\n+    let pick = rank_checked(model)?;\n     if pick.is_none() {{\n-        return None;\n+        return Err(anyhow!(\"no route\"));\n     }}\n"
        );
        i += 40;
    }
    out
}

fn prose(bytes: usize) -> String {
    "I re-read the router and the gate. The failing assertion is in the ledger path, so I will \
     fix the ordering there and re-run the tests. Decision: keep the public signature stable. \
     Open TODO: add a regression test for the ordering case. "
        .repeat(bytes / 200 + 1)
}

fn body(kind: Kind, path: &str, bytes: usize) -> String {
    match kind {
        Kind::Read => file_body(path, bytes),
        Kind::TestLog => test_log(bytes),
        Kind::Diff => diff_body(bytes),
        Kind::Prose => prose(bytes),
    }
}

/// The three job shapes from the brief: a small fix, a test repair, a review.
fn jobs() -> Vec<Job> {
    vec![
        Job {
            name: "riskyeats-small-fix",
            brief: "Fix the RiskyEats order-total rounding: the split between diners truncates \
                    instead of rounding half-up, so a $10.00 bill split three ways charges \
                    $9.99. Add a regression test.",
            plan: vec![
                ("tool", Kind::Read, 9_000),
                ("assistant", Kind::Prose, 400),
                ("tool", Kind::Diff, 6_000),
                ("assistant", Kind::Prose, 600),
                ("tool", Kind::TestLog, 14_000),
                ("assistant", Kind::Prose, 500),
                ("tool", Kind::Read, 9_000),
                ("tool", Kind::Diff, 6_000),
                ("assistant", Kind::Prose, 700),
                ("tool", Kind::TestLog, 14_000),
            ],
        },
        Job {
            name: "zoder-test-fix",
            brief: "The context_config integration test fails on the corpus_path field: the \
                    fixture omits it. Fix the fixture and make the test assert the parsed \
                    value rather than just that parsing succeeded.",
            plan: vec![
                ("tool", Kind::Read, 12_000),
                ("assistant", Kind::Prose, 300),
                ("tool", Kind::Read, 12_000),
                ("tool", Kind::TestLog, 18_000),
                ("assistant", Kind::Prose, 500),
                ("tool", Kind::Diff, 8_000),
                ("tool", Kind::TestLog, 18_000),
                ("assistant", Kind::Prose, 400),
                ("tool", Kind::Read, 12_000),
                ("tool", Kind::Diff, 8_000),
                ("assistant", Kind::Prose, 600),
                ("tool", Kind::TestLog, 18_000),
            ],
        },
        Job {
            name: "review-16-chunks",
            brief: "Review the context-optimization branch against origin/master and report \
                    concrete defects only.",
            plan: (0..16)
                .flat_map(|_| vec![("tool", Kind::Diff, 7_500), ("assistant", Kind::Prose, 800)])
                .collect(),
        },
    ]
}

/// One job's transcript as the levers see it: a prefix, a brief, then turns.
fn transcript(job: &Job) -> Vec<Turn> {
    let mut turns = vec![
        Turn::new(
            "system",
            "SYSTEM-PROMPT: You are the AUTHOR in an autonomous fix loop. Bounded by the \
             repository's tests and the operator's brief.",
        ),
        Turn::new("user", job.brief),
    ];
    for (i, (role, kind, bytes)) in job.plan.iter().enumerate() {
        let path = format!("crates/module_{}.rs", i % 5);
        let content = body(*kind, &path, *bytes);
        if *role == "tool" {
            turns.push(Turn::tool_result(&format!("read_file {path}"), content));
        } else {
            turns.push(Turn::new(role, content));
        }
    }
    turns
}

struct Savings {
    before_bytes: u64,
    after_caps_bytes: u64,
    after_elide_bytes: u64,
    after_all_bytes: u64,
    compacted: bool,
    resent_before: u64,
    resent_after: u64,
    turns_before: usize,
    turns_after: usize,
}

fn measure(job: &Job, levers: Levers) -> Savings {
    let turns = transcript(job);
    let before_bytes: u64 = turns.iter().map(Turn::bytes).sum();
    let capped: Vec<Turn> = turns
        .iter()
        .map(|t| {
            if t.role == "tool" {
                cap_tool_output(t, levers.tool_output_cap_bytes)
            } else {
                t.clone()
            }
        })
        .collect();
    let after_caps_bytes: u64 = capped.iter().map(Turn::bytes).sum();
    let mut elided = capped;
    elide_stale(
        &mut elided,
        levers.elide_after_turns,
        levers.elide_min_bytes,
    );
    let after_elide_bytes: u64 = elided.iter().map(Turn::bytes).sum();
    let applied = levers.apply(&turns);
    let after_all_bytes: u64 = applied.turns.iter().map(Turn::bytes).sum();
    Savings {
        before_bytes,
        after_caps_bytes,
        after_elide_bytes,
        after_all_bytes,
        compacted: applied.folded > 0,
        resent_before: resend_total(&turns),
        resent_after: resend_total(&applied.turns),
        turns_before: turns.len(),
        turns_after: applied.turns.len(),
    }
}

/// Total prompt tokens a run of `turns.len()` model calls would send: call `i`
/// resends every turn up to and including `i` — the measured loop shape, where
/// 250 calls each resent a full window. A single call's prompt is the metric
/// that hid the problem; the sum is the one that matches the bill.
fn resend_total(turns: &[Turn]) -> u64 {
    let mut running = 0u64;
    for t in turns {
        running += estimate_tokens(t.bytes());
    }
    running * turns.len() as u64
}

fn pct(after: u64, before: u64) -> f64 {
    if before == 0 {
        0.0
    } else {
        100.0 * (1.0 - after as f64 / before as f64)
    }
}

fn main() {
    let json = std::env::args().any(|a| a == "--json");
    let profiles: [(&str, Levers); 3] = [
        ("deepseek", Levers::DEEPSEEK),
        ("minimax", Levers::MINIMAX),
        ("other", Levers::OTHER),
    ];
    let mut rows: Vec<(String, &str, Savings)> = Vec::new();
    for job in jobs() {
        for (name, levers) in profiles {
            rows.push((job.name.to_string(), name, measure(&job, levers)));
        }
    }
    if json {
        println!("{{\"jobs\": [");
        for (i, (job, profile, s)) in rows.iter().enumerate() {
            let comma = if i + 1 == rows.len() { "" } else { "," };
            println!(
                "  {{\"job\": \"{job}\", \"profile\": \"{profile}\", \"bytes_before\": {}, \
                 \"bytes_after_caps\": {}, \"bytes_after_elide\": {}, \"bytes_after_all\": {}, \
                 \"compacted\": {}, \"turns_before\": {}, \"turns_after\": {}, \
                 \"resent_tokens_before\": {}, \"resent_tokens_after\": {}}}{comma}",
                s.before_bytes,
                s.after_caps_bytes,
                s.after_elide_bytes,
                s.after_all_bytes,
                s.compacted,
                s.turns_before,
                s.turns_after,
                s.resent_before,
                s.resent_after
            );
        }
        println!("]}}");
        return;
    }
    println!("# Deterministic context benchmark (no provider calls, no spend)\n");
    println!(
        "Resent prompt tokens = what a run sends when every turn resends the whole \
         conversation (the measured loop shape), in estimated tokens (~4 bytes/token).\n"
    );
    for (job, profile, s) in &rows {
        println!("## {job}  [{profile}]");
        println!(
            "  transcript bytes  {:>9} -> caps {:>9} ({:>3.0}%) -> +elide {:>9} ({:>3.0}%) -> all {:>9} ({:>3.0}%)  compacted={}",
            s.before_bytes,
            s.after_caps_bytes,
            pct(s.after_caps_bytes, s.before_bytes),
            s.after_elide_bytes,
            pct(s.after_elide_bytes, s.before_bytes),
            s.after_all_bytes,
            pct(s.after_all_bytes, s.before_bytes),
            s.compacted
        );
        println!(
            "  resent prompt tok {:>9} -> {:>9}  ({:.0}% saved)   turns {} -> {}",
            s.resent_before,
            s.resent_after,
            pct(s.resent_after, s.resent_before),
            s.turns_before,
            s.turns_after
        );
        println!();
    }
    println!("## Per-call ledger sample: zoder-test-fix");
    let turns = transcript(&jobs()[1]);
    let mut records: Vec<CallRecord> = Vec::new();
    // Call `i` carries every turn up to `i`, which is what makes the prompt
    // grow; the composition of EACH call is what says whether the growth is
    // tool output or history.
    let mut segments: Vec<Segment> = Vec::new();
    for (i, t) in turns.iter().enumerate() {
        let part = if t.role == "system" {
            Part::Prefix
        } else if i == 1 {
            Part::Task
        } else if t.role == "tool" {
            Part::ToolOutput
        } else {
            Part::History
        };
        segments.push(Segment::new(part, t.content.clone()));
        let prompt = segments.iter().map(|s| s.text.len() as u64).sum::<u64>();
        records.push(CallRecord::new(
            i as u32,
            "author",
            &segments,
            Some(estimate_tokens(prompt)),
            None,
            None,
            estimate_tokens(t.bytes() / 8),
        ));
    }
    for r in &records {
        println!("  {}", ledger_line(r));
    }
    let summary = summarise(&records);
    let composition = compose(&[Segment::new(
        Part::Prefix,
        "x".repeat(prefix_bytes as usize),
    )]);
    println!(
        "  calls={} prompt_tokens={} completion_tokens={} peak_prompt={} resend_share={:.0}% prefix_bytes={}",
        summary.calls,
        summary.prompt_tokens,
        summary.completion_tokens,
        summary.peak_prompt_tokens,
        summary.resend_share() * 100.0,
        composition.prefix_bytes
    );
}
