//! Unit tests for the context levers. Every lever is pure, so the whole
//! contract is testable without a provider: what moves, what stays put, and
//! how much the move saves.

use super::*;

fn tool_turn(name: &str, bytes: usize) -> Turn {
    Turn::tool_result(name, "x".repeat(bytes))
}

// --- cache telemetry -------------------------------------------------------

#[test]
fn cache_telemetry_splits_hits_from_misses() {
    let t = CacheTelemetry {
        prompt_tokens: 128_000,
        cache_hit_tokens: 127_000,
        completion_tokens: 400,
        reported_cache_miss: Some(1_000),
    };
    assert_eq!(t.cache_miss_tokens(), 1_000);
    assert!((t.hit_share_raw().unwrap() - 0.992_187_5).abs() < 1e-6);
}

#[test]
fn cache_telemetry_derives_the_miss_when_none_is_reported() {
    // OpenAI-shaped `cached_tokens`: only the hit count is published.
    let t = CacheTelemetry {
        prompt_tokens: 1_000,
        cache_hit_tokens: 900,
        completion_tokens: 10,
        reported_cache_miss: None,
    };
    assert_eq!(t.cache_miss_tokens(), 100);
    assert_eq!(t.cache_read_tokens(), 900);
}

#[test]
fn cache_telemetry_empty_prompt_is_a_full_hit() {
    let t = CacheTelemetry::default();
    assert_eq!(t.hit_share_raw(), Some(1.0));
    // A call with no prompt is not evidence either way.
    assert_eq!(
        classify(ProviderCacheShape::Split, &t, 0.98),
        CacheVerdict::Unknown
    );
}

#[test]
fn a_silent_provider_never_claims_a_hit_share() {
    // MiniMax publishes no cache counter. Reporting `Some(0.0)` here would read
    // as a catastrophic cache break on a provider that is actually reusing the
    // prefix; the honest answer is "unknown".
    let t = CacheTelemetry {
        prompt_tokens: 40_000,
        cache_hit_tokens: 0,
        completion_tokens: 300,
        reported_cache_miss: None,
    };
    assert_eq!(t.hit_share(ProviderCacheShape::None), None);
    assert_eq!(t.hit_share_raw(), None);
    assert_eq!(
        classify(ProviderCacheShape::None, &t, DEFAULT_CACHE_HIT_FLOOR),
        CacheVerdict::Unknown
    );
    // The same numbers from a provider that DOES report a split: a miss that is
    // explicitly reported as the whole prompt is a real prefix break.
    let reported = CacheTelemetry {
        reported_cache_miss: Some(40_000),
        ..t
    };
    assert_eq!(
        classify(ProviderCacheShape::Split, &reported, DEFAULT_CACHE_HIT_FLOOR),
        CacheVerdict::PrefixBreak
    );
}

#[test]
fn classify_reports_prefix_break_below_the_floor() {
    let healthy = CacheTelemetry {
        prompt_tokens: 10_000,
        cache_hit_tokens: 9_900,
        completion_tokens: 5,
        reported_cache_miss: None,
    };
    assert_eq!(
        classify(ProviderCacheShape::Split, &healthy, DEFAULT_CACHE_HIT_FLOOR),
        CacheVerdict::Cached
    );
    let broken = CacheTelemetry {
        prompt_tokens: 10_000,
        cache_hit_tokens: 6_000,
        completion_tokens: 5,
        reported_cache_miss: None,
    };
    assert_eq!(
        classify(ProviderCacheShape::Split, &broken, DEFAULT_CACHE_HIT_FLOOR),
        CacheVerdict::PrefixBreak
    );
}

#[test]
fn cost_bills_the_cached_tokens_once_at_the_cache_rate() {
    // The 30-day DeepSeek numbers: 10.6B input, 98.9% cached.
    let t = CacheTelemetry {
        prompt_tokens: 10_600_000_000,
        cache_hit_tokens: 10_484_400_000,
        completion_tokens: 50_100_000,
        reported_cache_miss: None,
    };
    let cost = t.cost_usd(0.003, 0.15, 0.60);
    let expected = (10_484_400_000f64 * 0.003
        + 115_600_000f64 * 0.15
        + 50_100_000f64 * 0.60)
        / 1_000_000.0;
    assert!((cost - expected).abs() < 1e-6, "{cost} vs {expected}");
    // Never more than charging every token at the input rate.
    assert!(cost < t.cost_usd(0.15, 0.15, 0.60));
}

#[test]
fn cost_never_bills_more_tokens_than_the_prompt_carried() {
    // A reported miss larger than `prompt - hit` (DeepSeek re-billing the
    // schema block on a break) must not be ADDED to the hit count: the cached
    // tokens are part of the prompt, not extra on top of it.
    let t = CacheTelemetry {
        prompt_tokens: 1_000,
        cache_hit_tokens: 900,
        completion_tokens: 0,
        reported_cache_miss: Some(250),
    };
    let cost = t.cost_usd(0.0, 1.0, 0.0);
    // 900 at $0 + 100 at $1/Mtok.
    assert!((cost - 0.000_1).abs() < 1e-12, "{cost}");
}

#[test]
fn cost_with_no_cache_counter_bills_every_token_at_the_miss_rate() {
    let t = CacheTelemetry {
        prompt_tokens: 1_000,
        cache_hit_tokens: 0,
        completion_tokens: 0,
        reported_cache_miss: None,
    };
    assert!((t.cost_usd(0.000_003, 0.15, 0.0) - 0.000_15).abs() < 1e-12);
}

// --- prompt composition ----------------------------------------------------

#[test]
fn composition_shares_sum_to_one() {
    let c = PromptComposition {
        prefix_bytes: 4_000,
        history_bytes: 2_000,
        tool_output_bytes: 13_000,
        task_bytes: 1_000,
    };
    let total: f64 = c.shares().iter().map(|(_, s)| s).sum();
    assert!((total - 1.0).abs() < 1e-9);
    assert_eq!(c.total_bytes(), 20_000);
}

#[test]
fn composition_of_an_empty_prompt_reports_zero_shares() {
    let c = PromptComposition::default();
    for (_, share) in c.shares() {
        assert_eq!(share, 0.0, "no division by zero");
    }
}

#[test]
fn token_estimate_matches_the_budget_helper() {
    assert_eq!(estimate_tokens(0), 0);
    assert_eq!(estimate_tokens(4), 1);
    assert_eq!(estimate_tokens(5), 2);
    assert_eq!(estimate_tokens(128_000), 32_000);
}

// --- tool-output cap -------------------------------------------------------

#[test]
fn a_small_result_is_untouched() {
    let t = tool_turn("src/lib.rs", 100);
    assert_eq!(cap_tool_output(&t, 16_384), t);
}

#[test]
fn a_cap_of_zero_disables_the_lever() {
    let t = tool_turn("src/lib.rs", 100_000);
    assert_eq!(cap_tool_output(&t, 0), t);
}

#[test]
fn a_large_result_keeps_head_and_tail_and_names_the_omitted_count() {
    let mut body = String::new();
    body.push_str("HEAD-MARKER\n");
    body.push_str(&"m".repeat(50_000));
    body.push_str("\nTAIL-MARKER");
    let t = Turn::tool_result("src/lib.rs", body);
    let capped = cap_tool_output(&t, 3_000);
    assert!(capped.content.starts_with("HEAD-MARKER"));
    assert!(capped.content.ends_with("TAIL-MARKER"));
    assert!(capped.content.contains(ELISION_MARKER));
    assert!(capped.content.contains("src/lib.rs"));
    // The stub's count is the TRUE byte delta, not a guess.
    let omitted: u64 = capped
        .content
        .split(ELISION_MARKER)
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    // The claimed omission is the exact gap between what was kept.
    let kept = (capped.content.len() as u64) - stub_len(&capped.content);
    assert_eq!(
        kept + omitted,
        t.content.len() as u64,
        "head + omitted + tail must reconstruct the original byte count"
    );
    assert!(capped.content.len() < t.content.len());
}

/// Length of the elision stub including its surrounding blank lines, i.e. the
/// part of the capped content that was not in the original. Located from the
/// marker so the original content cannot be mistaken for the stub.
fn stub_len(content: &str) -> u64 {
    // The stub is "\n\n" + marker + one line ending in "]" + "\n\n". Locate
    // the terminator from the marker onward; the marker itself never contains
    // a ']'.
    let at = content.find(ELISION_MARKER).expect("marker present");
    let close = content[at..].find(']').expect("stub close") + at;
    let end = close + 1 + 2; // "]" + "\n\n"
    (end - (at - 2)) as u64
}

#[test]
fn a_tiny_cap_cannot_produce_a_stub_that_dropped_nothing() {
    // cap 1 used to round both boundaries to 0/len and emit a stub claiming a
    // huge omission while keeping every byte. It is raised to MIN_CAP_BYTES and
    // the result must still be smaller than the input and honest about the
    // cap it actually applied.
    let t = tool_turn("src/lib.rs", 4_000);
    let capped = cap_tool_output(&t, 1);
    assert!(capped.content.len() < t.content.len());
    assert!(capped.content.contains(ELISION_MARKER));
    // The stub names the cap actually in force (MIN_CAP_BYTES, or the input
    // length if that is smaller), never the impossible one requested.
    assert!(capped.content.contains(&format!("cap of {MIN_CAP_BYTES} bytes")),
        "stub names the applied cap");
    let omitted: u64 = capped
        .content
        .split(ELISION_MARKER)
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let kept = (capped.content.len() as u64) - stub_len(&capped.content);
    assert_eq!(kept + omitted, t.content.len() as u64);
}

#[test]
fn multibyte_content_is_cut_on_char_boundaries() {
    // Every char is 3 bytes; a boundary mid-char would panic on slicing.
    let body: String = "日".repeat(2_000);
    let t = Turn::tool_result("docs/日本.md", body);
    let capped = cap_tool_output(&t, 1_000);
    assert!(capped.content.contains(ELISION_MARKER));
    assert!(capped.content.chars().filter(|c| *c == '日').count() > 0);
}

#[test]
fn a_cap_below_the_floor_never_returns_a_shrunk_but_bigger_stub() {
    // cap 599 < MIN_CAP_BYTES: the request cannot be honoured honestly, so the
    // turn is returned untouched rather than elided to a size that claims more
    // than it saved.
    let t = Turn::tool_result("f", "x".repeat(600));
    let capped = cap_tool_output(&t, 599);
    assert_eq!(capped, t, "below the floor: nothing to do, nothing dropped");
    assert!(!capped.content.contains(ELISION_MARKER));

    // A body only slightly over the floor has almost nothing to elide, so the
    // turn is left alone instead of paying for a stub that drops <1 KiB.
    let tight = Turn::tool_result("f", "y".repeat(MIN_CAP_BYTES + 1));
    assert_eq!(cap_tool_output(&tight, MIN_CAP_BYTES), tight);

    // Far enough over the floor that the elision is worth the stub.
    let big = Turn::tool_result("f", "y".repeat(MIN_CAP_BYTES + MIN_ELISION_BYTES));
    let out = cap_tool_output(&big, MIN_CAP_BYTES);
    assert!(out.content.contains(ELISION_MARKER));
    assert!(out.content.len() < big.content.len());
}

// --- stale elision ---------------------------------------------------------

#[test]
fn elision_replaces_old_large_tool_outputs_only() {
    let mut turns = vec![
        Turn::new("system", "prefix"),
        tool_turn("old.rs", 5_000),
        Turn::new("assistant", "working"),
        tool_turn("recent.rs", 5_000),
    ];
    let n = elide_stale(&mut turns, 2, 2_048);
    assert_eq!(n, 1, "only the old tool result is elided");
    assert!(is_elided(&turns[1]));
    assert!(turns[1].content.contains("old.rs"), "re-read reference kept");
    assert!(turns[1].content.contains("5000 bytes"));
    assert!(!is_elided(&turns[3]), "the recent turn stays verbatim");
    assert_eq!(turns[0].content, "prefix");
}

#[test]
fn elision_leaves_small_and_non_tool_turns_alone() {
    let mut turns = vec![
        tool_turn("tiny.rs", 10),
        Turn::new("assistant", "z".repeat(9_000)),
        tool_turn("big.rs", 9_000),
        tool_turn("tail.rs", 9_000),
    ];
    let n = elide_stale(&mut turns, 1, 2_048);
    assert_eq!(n, 1);
    assert!(!is_elided(&turns[0]), "below the byte floor");
    assert!(!is_elided(&turns[1]), "not a tool turn");
    assert!(is_elided(&turns[2]));
}

#[test]
fn elision_is_idempotent() {
    let mut turns = vec![tool_turn("a.rs", 9_000), tool_turn("b.rs", 9_000)];
    assert_eq!(elide_stale(&mut turns, 1, 2_048), 1);
    let once = turns.clone();
    assert_eq!(elide_stale(&mut turns, 1, 2_048), 0, "already elided");
    assert_eq!(turns, once);
}

#[test]
fn elision_of_an_empty_conversation_is_a_no_op() {
    let mut turns: Vec<Turn> = Vec::new();
    assert_eq!(elide_stale(&mut turns, 8, 2_048), 0);
}

#[test]
fn a_zero_keep_window_elides_every_eligible_turn() {
    let mut turns = vec![tool_turn("a.rs", 9_000), tool_turn("b.rs", 9_000)];
    assert_eq!(elide_stale(&mut turns, 0, 2_048), 2);
}

// --- history compaction ----------------------------------------------------

fn conversation(turns: usize, bytes: usize) -> Vec<Turn> {
    let mut v = vec![Turn::new("system", "You are the AUTHOR.")];
    v.push(Turn::new("user", "TASK: fix the RiskyEats total rounding"));
    for i in 0..turns {
        // A real tool result: a diff naming the file, then bulk.
        let mut body = format!("diff --git a/src/f{i}.rs b/src/f{i}.rs\n+++ b/src/f{i}.rs\n");
        body.push_str(&"m".repeat(bytes));
        v.push(Turn::tool_result(&format!("src/f{i}.rs"), body));
        v.push(Turn::new(
            "assistant",
            format!("We decided to clamp at the boundary in step {i}. TODO: rerun the test."),
        ));
    }
    v
}

#[test]
fn compaction_below_the_threshold_moves_nothing() {
    let turns = conversation(3, 1_000);
    let out = compact(&turns, 48_000, 6, 64, 40);
    assert_eq!(out.folded, 0);
    assert_eq!(out.turns, turns);
    assert_eq!(out.bytes_after, out.bytes_before);
}

#[test]
fn compaction_is_disabled_at_a_zero_threshold() {
    let turns = conversation(30, 4_000);
    let out = compact(&turns, 0, 6, 64, 40);
    assert_eq!(out.folded, 0);
}

#[test]
fn compaction_folds_old_turns_and_keeps_the_recent_ones_verbatim() {
    let turns = conversation(60, 4_000);
    let out = compact(&turns, 48_000, 6, 64, 40);
    assert!(out.folded > 0);
    assert!(out.bytes_after < out.bytes_before);
    // Layout: system prompt, then the brief verbatim, then the digest, then the
    // recent turns verbatim. The instructions must never be replaced by a
    // summary of themselves.
    assert_eq!(out.turns[0].role, "system");
    assert_eq!(out.turns[0].content, "You are the AUTHOR.");
    assert!(out.turns[1].content.starts_with("TASK:"), "brief survives");
    assert!(is_digest(&out.turns[2]), "digest follows the brief");
    let tail = &out.turns[out.turns.len() - 6..];
    assert_eq!(tail, &turns[turns.len() - 6..]);
    assert!(
        out.turns.iter().any(|t| t.content.starts_with("TASK:")),
        "the brief survives compaction"
    );
}

#[test]
fn the_digest_names_files_decisions_and_todos() {
    let turns = conversation(60, 4_000);
    let out = compact(&turns, 48_000, 6, 64, 40);
    let digest = &out.turns.iter().find(|t| is_digest(t)).expect("a digest").content;
    assert!(digest.contains("Files touched:"), "{digest}");
    assert!(digest.contains("src/f0.rs"));
    assert!(digest.contains("Decisions:"));
    assert!(digest.contains("Decided") || digest.contains("decided"));
    assert!(digest.contains("Open todos:"));
    assert!(digest.contains("TODO"));
}

#[test]
fn compaction_twice_keeps_exactly_one_digest() {
    // The pre-fix code folded a digest into a fresh digest, so every pass
    // nested one level deeper and the conversation grew instead of shrinking.
    let turns = conversation(60, 4_000);
    let once = compact(&turns, 48_000, 6, 64, 40);
    let digest_count = once.turns.iter().filter(|t| is_digest(t)).count();
    assert_eq!(digest_count, 1);
    let twice = compact(&once.turns, 1, 6, 64, 40);
    assert_eq!(
        twice.turns.iter().filter(|t| is_digest(t)).count(),
        1,
        "a second pass must reuse the digest, not nest one inside another"
    );
    let once_digest = once.turns.iter().find(|t| is_digest(t)).unwrap();
    let twice_digest = twice.turns.iter().find(|t| is_digest(t)).unwrap();
    assert_eq!(twice_digest.content, once_digest.content, "unchanged digest");
    // The pinned head and the verbatim tail are untouched by the second pass.
    assert_eq!(twice.turns[0].content, once.turns[0].content);
    assert_eq!(twice.turns[1].content, once.turns[1].content);
}

#[test]
fn a_carried_digest_keeps_its_files_and_todos() {
    let mut turns = conversation(20, 4_000);
    turns.push(Turn::new("system", digest_for(&["src/deep.rs"], &["TODO: audit"])));
    for i in 0..30 {
        let name = format!("src/new{i}.rs");
        let body = format!("diff --git a/{name} b/{name}\n+++ b/{name}\n{}", "m".repeat(4_000));
        turns.push(Turn::tool_result(&name, body));
        turns.push(Turn::new("assistant", "We must keep going."));
    }
    // A wide cap so the carried-vs-new distinction is not masked by truncation.
    let out = compact(&turns, 48_000, 6, 256, 256);
    let digest = &out.turns.iter().find(|t| is_digest(t)).expect("a digest").content;
    assert_eq!(out.turns.iter().filter(|t| is_digest(t)).count(), 1);
    assert!(digest.contains("src/deep.rs"), "carried file: {digest}");
    assert!(digest.contains("TODO: audit"), "carried todo: {digest}");
    // Newly folded work is represented (the list is capped, so assert on the
    // fact that new files are present rather than on a specific index).
    assert!(digest.contains("src/new"), "new work folded in: {digest}");
}

fn digest_for(files: &[&str], todos: &[&str]) -> String {
    Digest {
        files: files.iter().map(|s| s.to_string()).collect(),
        decisions: vec![],
        open_todos: todos.iter().map(|s| s.to_string()).collect(),
        turns: 12,
        bytes_before: 48_000,
    }
    .render()
}

#[test]
fn a_short_conversation_with_nothing_foldable_is_unchanged() {
    let turns = vec![
        Turn::new("system", "s"),
        Turn::new("user", "task"),
        Turn::new("assistant", "ok"),
    ];
    let out = compact(&turns, 1, 3, 64, 40);
    assert_eq!(out.folded, 0);
    assert_eq!(out.turns, turns);
}

#[test]
fn the_brief_is_never_folded() {
    let mut turns = vec![
        Turn::new("system", "s"),
        Turn::new("user", "THE BRIEF"),
    ];
    for _ in 0..60 {
        turns.push(Turn::new("assistant", "y".repeat(4_000)));
    }
    let out = compact(&turns, 1_000, 4, 64, 40);
    assert!(out.turns.iter().any(|t| t.content == "THE BRIEF"));
}

#[test]
fn is_brief_is_bounds_safe() {
    let turns = vec![Turn::new("user", "a")];
    assert!(is_brief(&turns, 0));
    assert!(!is_brief(&turns, 9), "out of range is not the brief");
    assert!(!is_brief(&[], 0));
    let digest = Turn::new("system", Digest::default().render());
    assert!(!is_brief(&[digest], 0));
}

// --- lever profiles --------------------------------------------------------

#[test]
fn deepseek_profile_compacts_inside_the_measured_window() {
    let l = Levers::DEEPSEEK;
    assert!((40_000..=60_000).contains(&l.compact_after_tokens));
    assert!(l.tool_output_cap_bytes > 0 && l.tool_output_cap_bytes <= 32_768);
    assert!(l.elide_after_turns > 0);
}

#[test]
fn minimax_profile_is_tighter_than_deepseek_and_still_enabled() {
    let d = Levers::DEEPSEEK;
    let m = Levers::MINIMAX;
    assert!(m.compact_after_tokens > 0);
    assert!(m.compact_after_tokens < d.compact_after_tokens);
    assert!(m.tool_output_cap_bytes < d.tool_output_cap_bytes);
    assert!(m.elide_after_turns <= d.elide_after_turns);
}

#[test]
fn the_other_profile_caps_and_elides_but_never_compacts() {
    // A local model has no per-token bill; a summarised history is pure
    // quality risk there.
    let o = Levers::OTHER;
    assert_eq!(o.compact_after_tokens, 0);
    assert!(o.tool_output_cap_bytes > 0);
    assert!(o.elide_after_turns > 0);
    assert_eq!(Levers::default(), Levers::OTHER);
}

#[test]
fn applying_every_lever_shrinks_a_long_conversation() {
    let turns = conversation(120, 8_000);
    let out = Levers::DEEPSEEK.apply(&turns);
    assert!(
        out.bytes_after * 4 < out.bytes_before,
        "levers should cut a 120-turn conversation to a quarter: {} -> {}",
        out.bytes_before,
        out.bytes_after
    );
}

#[test]
fn a_conversation_shrunk_below_the_threshold_is_not_compacted() {
    // Caps and elision alone can bring a conversation under the compaction
    // threshold; folding it then would summarise a prompt that is already
    // small, which only risks quality.
    let turns = conversation(60, 8_000);
    let out = Levers::DEEPSEEK.apply(&turns);
    let capped_and_elided = {
        let mut v: Vec<Turn> = turns
            .iter()
            .map(|t| {
                if t.role == "tool" {
                    cap_tool_output(t, Levers::DEEPSEEK.tool_output_cap_bytes)
                } else {
                    t.clone()
                }
            })
            .collect();
        elide_stale(&mut v, Levers::DEEPSEEK.elide_after_turns, Levers::DEEPSEEK.elide_min_bytes);
        v
    };
    let tokens: u64 = capped_and_elided.iter().map(|t| estimate_tokens(t.bytes())).sum();
    if tokens <= Levers::DEEPSEEK.compact_after_tokens {
        assert_eq!(out.folded, 0, "under the threshold after caps/elision");
    }
    assert!(out.bytes_after < out.bytes_before);
}

#[test]
fn applying_the_levers_to_a_short_conversation_is_a_no_op() {
    let turns = conversation(2, 500);
    let out = Levers::DEEPSEEK.apply(&turns);
    assert_eq!(out.folded, 0);
    assert_eq!(out.bytes_after, out.bytes_before);
}
