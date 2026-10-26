//! The LIVE half of the context benchmark: three real task shapes, run
//! against a real provider, with the context levers off and then on.
//!
//! The deterministic benchmark (`context_bench`) proves what the levers do
//! to a transcript. This one proves what they do to a real bill and to a
//! real answer, on a real provider, with the provider's own `usage` numbers
//! read back from the wire.
//!
//! ## What it measures
//!
//! For each task it runs the SAME conversation twice:
//!
//! * **before** — the levers off. The full transcript is resent on every
//!   call, which is the measured loop shape.
//! * **after** — the levers on, at the profile for the provider family.
//!
//! and reports, from the provider's own `usage`:
//!
//! * prompt tokens, cache hit tokens, cache miss tokens and completion
//!   tokens, per call and in total;
//! * the cost, at the operator's stated rates;
//! * whether the task SUCCEEDED.
//!
//! ## What "succeeded" means here
//!
//! Each task ends with a question whose answer is a decision the agent
//! recorded EARLY in the conversation. The decisive tool output is large
//! enough to be capped and elided, and it sits in the folded span — so
//! with the levers on, the only way to answer is the compaction digest
//! carrying the decision forward. That is exactly the quality claim the
//! levers make, and this is the measurement of it.
//!
//! It is NOT a claim about a full agentic loop: the replay cannot re-read a
//! file the way a real agent would, so this measures "the decision
//! survives the compaction", not "the agent can still do the task".
//!
//! ## Usage
//!
//! ```text
//! cargo run -p zoder-core --bin context_bench_live -- \
//!     --base-url https://api.deepseek.com/v1 \
//!     --model deepseek-flash \
//!     --api-key-file /path/to/key \
//!     --miss-rate 0.14 --cache-read-rate 0.003 --output-rate 0.28
//! ```
//!
//! The API key is read from a FILE, never from the command line, so it
//! never lands in a shell history or a process list.

use std::fmt::Write as _;
use std::path::PathBuf;

use zoder_core::config::{Auth, Provider};
use zoder_core::context_opt::measure::{CallRecord, Part, Segment};
use zoder_core::context_opt::{
    estimate_tokens, CacheTelemetry, ContextProfile, Levers, ProviderCacheShape, Turn,
};
use zoder_core::provider::{ChatRequest, Message, OpenAiProvider};

/// The system prompt every call carries. It is the first message and never
/// changes, which is what makes the provider's prefix cache useful.
const SYSTEM: &str =
    "You are the AUTHOR in an autonomous fix loop. The repository's tests and the \
operator's brief bound your work. Report what you decided and why, in one short paragraph.";

/// One task: a brief, a decision to remember, a question, and the shape of
/// the conversation in between.
struct Task {
    name: &'static str,
    brief: &'static str,
    /// The assistant turn that records the decision. It is placed EARLY on
    /// purpose: with the levers on, this turn is folded into the digest, so
    /// the task can only be answered if the digest carried it forward.
    decision: &'static str,
    /// The question at the end of the conversation.
    question: &'static str,
    /// Tokens any of which the answer may contain for the task to count as
    /// done. A single exact token would fail on a correct answer that
    /// phrases it differently, which would be a failure of the CHECK, not
    /// of the task. Every token names something only the DECISION says: none
    /// may appear in the system prompt, the brief, the question or any
    /// generated tool output (a test enforces it), or an answer that merely
    /// echoed its input would score as a success.
    expect: &'static [&'static str],
    /// `(role, kind, bytes)` for the turns in between.
    plan: Vec<(&'static str, Kind, usize)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    TestLog,
    Diff,
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
         crates/zoder-core/src/billing.rs:412:9:\nassertion `left == right` failed\n  left: 3\n \
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

/// The three task shapes: a small fix, a test repair, and a review.
fn tasks() -> Vec<Task> {
    vec![
        Task {
            name: "order-total-fix",
            brief: "The checkout total for a split bill is wrong: a $10.00 bill split three ways \
                    charges $9.99 instead of $10.00. Find the cause and state the fix you decided \
                    on.",
            decision: "We decided to round each share half-up instead of truncating, so the \
                        three shares of $10.00 come to $3.34, $3.33 and $3.33.",
            question: "State the fix you decided on for the split-bill total, in one sentence.",
            expect: &["half-up", "half up"],
            plan: vec![
                ("tool", Kind::Read, 48_000),
                ("assistant", Kind::Prose, 400),
                ("tool", Kind::Diff, 48_000),
                ("assistant", Kind::Prose, 600),
                ("tool", Kind::TestLog, 48_000),
                ("assistant", Kind::Prose, 500),
            ],
        },
        Task {
            name: "config-fixture-fix",
            brief: "The context-config integration test fails on the corpus_path field: the \
                    fixture omits it. Find the cause and state the fix you decided on.",
            decision: "We decided to add the missing corpus_path key to the fixture instead of \
                        making the parser default it, so a missing path still fails loudly.",
            question: "State the fix you decided on for the failing config fixture, in one \
                       sentence.",
            expect: &["loud"],
            plan: vec![
                ("tool", Kind::Read, 48_000),
                ("assistant", Kind::Prose, 300),
                ("tool", Kind::TestLog, 48_000),
                ("assistant", Kind::Prose, 500),
                ("tool", Kind::Diff, 48_000),
                ("assistant", Kind::Prose, 400),
            ],
        },
        Task {
            name: "review-diff-defect",
            brief: "Review the routing change and report the one concrete defect it introduces. \
                    State the defect you decided on.",
            decision: "We decided the defect is the swallowed error: the new rank_checked path \
                        turns a routing miss into a plain Err, so the caller no longer sees the \
                        model that failed.",
            question: "State the defect you decided on in the routing change, in one sentence.",
            expect: &["swallow", "model that failed", "which model failed"],
            plan: vec![
                ("tool", Kind::Diff, 48_000),
                ("assistant", Kind::Prose, 800),
                ("tool", Kind::Read, 48_000),
                ("assistant", Kind::Prose, 500),
                ("tool", Kind::Diff, 48_000),
                ("assistant", Kind::Prose, 600),
            ],
        },
    ]
}

/// One assistant turn from the long tail of a working loop.
fn history_turn(i: usize) -> String {
    format!(
        "Turn {i}: re-read the file, confirmed the behaviour, and carried on with the rest of \
the change. The module compiles, the failing case still reproduces, and the next edit is in \
the caller that passes the truncated value down. Nothing new to report; the working set is \
unchanged and the remaining work is the same list as before."
    )
}

/// One task's conversation, as the levers see it.
///
/// The shape is the measured one: a brief, an early decision, a few large
/// tool results, and then the LONG tail of assistant turns a working loop
/// accumulates. The tail is what the compaction threshold is about — the
/// tool outputs are capped and elided, but the history is not, so it is the
/// history that decides whether the conversation has to be folded.
fn transcript(task: &Task, history_turns: usize) -> Vec<Turn> {
    let mut turns = vec![
        Turn::new("system", SYSTEM),
        Turn::new("user", task.brief),
        // The decision is recorded EARLY, so the levers fold it into the
        // digest and the task can only be answered if the digest carries it.
        Turn::new("assistant", task.decision),
    ];
    for (i, (role, kind, bytes)) in task.plan.iter().enumerate() {
        let path = format!("crates/module_{}.rs", i % 5);
        let content = body(*kind, &path, *bytes);
        if *role == "tool" {
            turns.push(Turn::tool_result(&format!("read_file {path}"), content));
        } else {
            turns.push(Turn::new(role, content));
        }
    }
    for i in 0..history_turns {
        turns.push(Turn::new("assistant", history_turn(i)));
    }
    turns.push(Turn::new("user", task.question));
    turns
}

/// Where in the conversation the provider is called, as fractions of the
/// transcript.
///
/// The conversation is resent from the start on every call — the measured
/// loop shape — so the call points sample the growth: early, middle and the
/// end (where the question is).
fn call_points(len: usize) -> [usize; 3] {
    let last = len.saturating_sub(1);
    [last / 3, (2 * last) / 3, last]
}

/// One run of one task with one lever setting.
struct Run {
    task: &'static str,
    levers_on: bool,
    calls: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_hit_tokens: u64,
    cache_miss_tokens: u64,
    cache_reported_calls: u64,
    cost_usd: f64,
    succeeded: bool,
    answer: String,
    wall_ms: u64,
    /// The per-call cache verdict: `cached`, `prefix_break` or `unknown`.
    cache_verdicts: Vec<&'static str>,
    /// The per-call records, one per provider call.
    records: Vec<CallRecord>,
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut base_url = String::new();
    let mut model = String::new();
    let mut api_key_file: Option<PathBuf> = None;
    let mut miss_rate = 0.0f64;
    let mut cache_read_rate = 0.0f64;
    let mut output_rate = 0.0f64;
    let mut profile = ContextProfile::Fleet;
    let mut json = false;
    let mut dry_run = false;
    let mut history_turns = 640usize;
    let mut i = 0usize;
    // A value-taking flag with no value is an error, not a panic.
    let take = |i: &mut usize, flag: &str| -> anyhow::Result<String> {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
    };
    while i < args.len() {
        match args[i].as_str() {
            "--base-url" => {
                base_url = take(&mut i, "--base-url")?;
                i += 1;
            }
            "--model" => {
                model = take(&mut i, "--model")?;
                i += 1;
            }
            "--api-key-file" => {
                api_key_file = Some(PathBuf::from(take(&mut i, "--api-key-file")?));
                i += 1;
            }
            "--miss-rate" => {
                miss_rate = take(&mut i, "--miss-rate")?.parse()?;
                i += 1;
            }
            "--cache-read-rate" => {
                cache_read_rate = take(&mut i, "--cache-read-rate")?.parse()?;
                i += 1;
            }
            "--output-rate" => {
                output_rate = take(&mut i, "--output-rate")?.parse()?;
                i += 1;
            }
            "--profile" => {
                let raw = take(&mut i, "--profile")?;
                profile = ContextProfile::named(&raw)
                    .ok_or_else(|| anyhow::anyhow!("unknown deployment profile {raw:?}"))?;
                i += 1;
            }
            "--json" => {
                json = true;
                i += 1;
            }
            "--dry-run" => {
                dry_run = true;
                i += 1;
            }
            "--history-turns" => {
                history_turns = take(&mut i, "--history-turns")?.parse()?;
                i += 1;
            }
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    if dry_run {
        // No provider: show what the levers do to each task's conversation, so
        // a run can be checked before it spends anything.
        let levers = Levers::for_model(profile, &model);
        println!("# dry run — model {model}, profile {profile:?}\n");
        for task in tasks() {
            let turns = transcript(&task, history_turns);
            let applied = levers.apply(&turns);
            let before: u64 = turns.iter().map(Turn::bytes).sum();
            let after: u64 = applied.turns.iter().map(Turn::bytes).sum();
            println!(
                "## {}  turns {} -> {}  bytes {} -> {}  tokens {} -> {}  folded {}",
                task.name,
                turns.len(),
                applied.turns.len(),
                before,
                after,
                estimate_tokens(before),
                estimate_tokens(after),
                applied.folded
            );
            for point in call_points(turns.len()) {
                if turns.is_empty() {
                    break;
                }
                let upto = point.min(turns.len() - 1);
                let before = estimate_tokens(turns[..=upto].iter().map(Turn::bytes).sum::<u64>());
                let after = estimate_tokens(
                    levers
                        .apply(&turns[..=upto])
                        .turns
                        .iter()
                        .map(Turn::bytes)
                        .sum::<u64>(),
                );
                println!("   call at turn {upto}: prompt tokens {before} -> {after}");
            }
        }
        return Ok(());
    }
    if base_url.is_empty() || model.is_empty() {
        anyhow::bail!(
            "--base-url and --model are required. Example:\n  \
             context_bench_live --base-url https://api.deepseek.com/v1 --model deepseek-flash \
             --api-key-file /path/to/key"
        );
    }
    let key = match api_key_file {
        Some(path) => std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("reading the api key file: {e}"))?,
        None => String::new(),
    };
    let key = key.trim().to_string();
    if key.is_empty() {
        anyhow::bail!(
            "--api-key-file is required for a live run (the key is read from a file, \
                       never from the command line)"
        );
    }
    let provider = Provider {
        model_aliases: Vec::new(),
        id: "benchmark".to_string(),
        engine_provider_ref: None,
        base_url: base_url.clone(),
        kind: "openai-chat".to_string(),
        auth: Auth::Bearer { token: key },
        paid: true,
        billing: zoder_core::config::BillingMode::Metered,
        subscription: None,
        serves: Vec::new(),
        azure_api_version: None,
        max_context_tokens: None,
        overload_backoff_min_s: None,
        first_token_timeout_s: None,
        stream: None,
        rate_limit_group: None,
        max_concurrent: None,
        max_requests_per_minute: None,
    };
    let client = OpenAiProvider::new_with_request_timeout_s(&provider, Some(600))?;
    let levers = Levers::for_model(profile, &model);
    // The provider-side cache shape, from what the wire actually carries.
    //
    // MEASURED, not assumed: the 2026-10-25 study recorded that the second
    // provider published no cache counter at all, and that turned out to be
    // wrong - it publishes the OpenAI-shaped `cached_tokens` subset. The
    // live benchmark reads it (the hit counts are non-zero), so the shape
    // here is `CachedSubset` and the hit share is asserted rather than
    // guessed.
    let shape = if model.to_ascii_lowercase().contains("deepseek") {
        ProviderCacheShape::Split
    } else {
        ProviderCacheShape::CachedSubset
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let mut runs: Vec<Run> = Vec::new();
    for task in tasks() {
        for levers_on in [false, true] {
            let run = runtime.block_on(run_one(
                &client,
                &task,
                &model,
                &levers,
                levers_on,
                shape,
                miss_rate,
                cache_read_rate,
                output_rate,
                history_turns,
            ))?;
            runs.push(run);
        }
    }

    if json {
        // serde_json, not a hand-rolled format: a task name or an answer
        // with a quote or a newline in it must not produce invalid JSON.
        let rows: Vec<serde_json::Value> = runs
            .iter()
            .map(|r| {
                serde_json::json!({
                    "task": r.task,
                    "levers": if r.levers_on { "on" } else { "off" },
                    "calls": r.calls,
                    "prompt_tokens": r.prompt_tokens,
                    "completion_tokens": r.completion_tokens,
                    "cache_hit_tokens": r.cache_hit_tokens,
                    "cache_miss_tokens": r.cache_miss_tokens,
                    "cache_reported_calls": r.cache_reported_calls,
                    "cost_usd": r.cost_usd,
                    "succeeded": r.succeeded,
                    "wall_ms": r.wall_ms,
                    "cache_verdicts": r.cache_verdicts,
                    "answer": r.answer,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "runs": rows }))
                .unwrap_or_else(|_| "{\"runs\":[]}".to_string())
        );
        return Ok(());
    }

    println!("# Live context benchmark — model {model}, profile {profile:?}\n");
    for task in tasks() {
        let before = runs.iter().find(|r| r.task == task.name && !r.levers_on);
        let after = runs.iter().find(|r| r.task == task.name && r.levers_on);
        let (Some(before), Some(after)) = (before, after) else {
            continue;
        };
        println!("## {} ", task.name);
        println!(
            "  prompt tok   {:>10} -> {:>10}  ({:+.0}%)",
            before.prompt_tokens,
            after.prompt_tokens,
            pct(after.prompt_tokens, before.prompt_tokens)
        );
        println!(
            "  cache hit    {:>10} -> {:>10}   cache miss {:>10} -> {:>10}",
            before.cache_hit_tokens,
            after.cache_hit_tokens,
            before.cache_miss_tokens,
            after.cache_miss_tokens
        );
        println!(
            "  completion   {:>10} -> {:>10}",
            before.completion_tokens, after.completion_tokens
        );
        println!(
            "  cost         {:>10.6} -> {:>10.6}   ({} -> {})",
            before.cost_usd,
            after.cost_usd,
            if before.succeeded { "OK" } else { "FAIL" },
            if after.succeeded { "OK" } else { "FAIL" }
        );
        println!(
            "  wall ms      {:>10} -> {:>10}",
            before.wall_ms, after.wall_ms
        );
        println!(
            "  answer (after): {}",
            after
                .answer
                .replace('\n', " ")
                .chars()
                .take(160)
                .collect::<String>()
        );
        println!(
            "  answer (before): {}",
            before
                .answer
                .replace('\n', " ")
                .chars()
                .take(160)
                .collect::<String>()
        );
        println!();
    }
    let all_before_succeeded = runs.iter().filter(|r| !r.levers_on).all(|r| r.succeeded);
    let all_after_succeeded = runs.iter().filter(|r| r.levers_on).all(|r| r.succeeded);
    println!(
        "task success: before {} / after {} — {}",
        if all_before_succeeded {
            "all OK"
        } else {
            "SOME FAILED"
        },
        if all_after_succeeded {
            "all OK"
        } else {
            "SOME FAILED"
        },
        if all_after_succeeded && all_before_succeeded {
            "no drop"
        } else {
            "REGRESSION"
        }
    );
    let total_before: f64 = runs
        .iter()
        .filter(|r| !r.levers_on)
        .map(|r| r.cost_usd)
        .sum();
    let total_after: f64 = runs
        .iter()
        .filter(|r| r.levers_on)
        .map(|r| r.cost_usd)
        .sum();
    println!(
        "total cost: ${total_before:.6} -> ${total_after:.6} ({:+.0}%)",
        pct_cost(total_after, total_before)
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_one(
    client: &OpenAiProvider,
    task: &Task,
    model: &str,
    levers: &Levers,
    levers_on: bool,
    shape: ProviderCacheShape,
    miss_rate: f64,
    cache_read_rate: f64,
    output_rate: f64,
    history_turns: usize,
) -> anyhow::Result<Run> {
    let turns = transcript(task, history_turns);
    let started = std::time::Instant::now();
    let mut run = Run {
        task: task.name,
        levers_on,
        calls: 0,
        prompt_tokens: 0,
        completion_tokens: 0,
        cache_hit_tokens: 0,
        cache_miss_tokens: 0,
        cache_reported_calls: 0,
        cost_usd: 0.0,
        succeeded: false,
        answer: String::new(),
        wall_ms: 0,
        cache_verdicts: Vec::new(),
        records: Vec::new(),
    };
    if turns.is_empty() {
        // An empty conversation has nothing to send and no answer to check.
        return Ok(run);
    }
    for point in call_points(turns.len()) {
        let upto = point.min(turns.len().saturating_sub(1));
        // The levers run on the conversation AS IT STANDS at this point, which
        // is what the CLI does: the loop's prompt at step `upto`, bounded.
        // Applying them to the whole conversation and then slicing would
        // compare a 650-turn prompt against a 9-turn one.
        let compacted = if levers_on {
            Some(levers.apply(&turns[..=upto]).turns)
        } else {
            None
        };
        let slice: &[Turn] = compacted.as_deref().unwrap_or(&turns[..=upto]);
        // A tool result goes out as a labelled USER message.
        //
        // A provider that validates the chat-completions schema rejects a
        // `role = "tool"` message outright unless it follows an assistant
        // turn carrying `tool_calls` — a pair zoder's own provider client
        // cannot express (its `Message` has no tool-call shape at all). The
        // LEVERS still see the turn as a tool output, which is what matters
        // for them: the cap and the elision bound the BYTES of a tool
        // result, wherever it travels.
        let messages: Vec<Message> = slice
            .iter()
            .map(|t| {
                if t.role == "tool" {
                    let source = t.source.clone().unwrap_or_else(|| "tool output".into());
                    Message::new("user", format!("[tool result: {source}]\n{}", t.content))
                } else {
                    Message::new(&t.role, t.content.clone())
                }
            })
            .collect();
        let segments: Vec<Segment> = slice
            .iter()
            .map(|t| {
                let part = match t.role.as_str() {
                    "system" => Part::Prefix,
                    "tool" => Part::ToolOutput,
                    _ => Part::History,
                };
                Segment::new(part, t.content.clone())
            })
            .collect();
        let req = ChatRequest {
            model: model.to_string(),
            messages,
            // A reasoning model spends part of the output budget thinking
            // before it answers; a small budget leaves the answer EMPTY
            // (the provider returns a 2xx with reasoning only, which zoder
            // correctly refuses to treat as an answer).
            max_tokens: 8192,
            temperature: Some(0.0),
            stream: false,
            show_reasoning: false,
            reasoning_effort: None,
            top_p: None,
            top_k: None,
            presence_penalty: None,
            chat_template_kwargs: None,
            response_format: None,
        };
        let res = client
            .stream_chat(&req, None)
            .await
            .map_err(|e| anyhow::anyhow!("provider call failed: {}", e.message))?;
        run.calls += 1;
        run.prompt_tokens += res
            .prompt_tokens
            .unwrap_or_else(|| estimate_tokens(segments.iter().map(|s| s.text.len() as u64).sum()));
        run.completion_tokens += res.completion_tokens.unwrap_or(res.tokens_out);
        // A call carried cache telemetry when the provider reported EITHER
        // counter: some providers publish only the hit side, some both.
        if res.cached_prompt_tokens.is_some() || res.cache_miss_tokens.is_some() {
            run.cache_reported_calls += 1;
        }
        if let Some(hit) = res.cached_prompt_tokens {
            run.cache_hit_tokens += hit;
        }
        if let Some(miss) = res.cache_miss_tokens {
            run.cache_miss_tokens += miss;
        }
        // The cost is billed the way a provider bills a cache: the cached
        // tokens at the cache-read rate, the rest at the miss rate, and the
        // completion at the output rate.
        let telemetry = CacheTelemetry {
            // A provider that reports no prompt count must not be billed as
            // if it had sent nothing: fall back to the byte estimate, which
            // is what the per-call record reports too.
            prompt_tokens: res.prompt_tokens.unwrap_or_else(|| {
                estimate_tokens(segments.iter().map(|s| s.text.len() as u64).sum())
            }),
            cache_hit_tokens: res.cached_prompt_tokens.unwrap_or(0),
            completion_tokens: res.completion_tokens.unwrap_or(res.tokens_out),
            reported_cache_miss: res.cache_miss_tokens,
        };
        run.cost_usd += telemetry.cost_usd(cache_read_rate, miss_rate, output_rate);
        // The cache verdict for this call, from the provider's own numbers.
        let verdict = zoder_core::context_opt::classify(
            shape,
            &telemetry,
            zoder_core::context_opt::DEFAULT_CACHE_HIT_FLOOR,
        );
        run.cache_verdicts.push(match verdict {
            zoder_core::context_opt::CacheVerdict::Cached => "cached",
            zoder_core::context_opt::CacheVerdict::PrefixBreak => "prefix_break",
            zoder_core::context_opt::CacheVerdict::Unknown => "unknown",
        });
        if upto + 1 == turns.len() {
            run.answer = res.content.clone();
        }
        run.records.push(CallRecord::new(
            run.calls as u32,
            task.name,
            &segments,
            res.prompt_tokens,
            res.cached_prompt_tokens,
            res.cache_miss_tokens,
            res.completion_tokens.unwrap_or(res.tokens_out),
        ));
    }
    run.succeeded = scores(task, &run.answer);
    run.wall_ms = started.elapsed().as_millis() as u64;
    Ok(run)
}

/// The part of a reply that is the answer: a reasoning model may put its
/// thinking before a closing `</think>`, and that thinking routinely quotes
/// the transcript (including the decision), so it must not be scored.
fn final_answer(reply: &str) -> &str {
    reply
        .rfind("</think>")
        .map_or(reply, |at| &reply[at + "</think>".len()..])
}

/// Did the answer state the recorded decision?
fn scores(task: &Task, reply: &str) -> bool {
    let answer = final_answer(reply).to_ascii_lowercase();
    task.expect
        .iter()
        .any(|needle| answer.contains(&needle.to_ascii_lowercase()))
}

fn pct(after: u64, before: u64) -> f64 {
    if before == 0 {
        0.0
    } else {
        100.0 * (1.0 - after as f64 / before as f64)
    }
}

fn pct_cost(after: f64, before: f64) -> f64 {
    if before == 0.0 {
        0.0
    } else {
        100.0 * (1.0 - after / before)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The success check must not be satisfiable by echoing the input: every
    /// expected token appears in the decision and nowhere else in the
    /// transcript the model is shown.
    #[test]
    fn expected_tokens_name_only_the_decision() {
        for task in tasks() {
            let decision = task.decision.to_ascii_lowercase();
            assert!(
                task.expect
                    .iter()
                    .any(|t| decision.contains(&t.to_ascii_lowercase())),
                "{}: no expected token is in the decision",
                task.name
            );
            for turn in transcript(&task, 4) {
                if turn.content == task.decision {
                    continue;
                }
                let text = turn.content.to_ascii_lowercase();
                for token in task.expect {
                    assert!(
                        !text.contains(&token.to_ascii_lowercase()),
                        "{}: expected token {token:?} also appears in a {} turn",
                        task.name,
                        turn.role
                    );
                }
            }
        }
    }

    /// Reasoning before `</think>` is not the answer: a reply whose thinking
    /// quotes the decision but whose answer states a different fix fails.
    #[test]
    fn only_the_final_answer_is_scored() {
        let task = tasks().remove(0);
        assert!(!scores(
            &task,
            "<think>they decided to round each share half-up</think>\n\nAssign the leftover cent to one share."
        ));
        assert!(scores(
            &task,
            "<think>...</think>\n\nRound each share half-up so the shares sum to $10.00."
        ));
        assert!(scores(&task, "Round each share half up."));
    }
}
