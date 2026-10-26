//! Tests for the per-call measurement layer.

use super::*;

fn seg(part: Part, len: usize) -> Segment {
    Segment::new(part, "x".repeat(len))
}

#[test]
fn compose_attributes_every_byte_to_exactly_one_part() {
    let segments = vec![
        seg(Part::Prefix, 400),
        seg(Part::Task, 100),
        seg(Part::History, 200),
        seg(Part::ToolOutput, 1_300),
    ];
    let c = compose(&segments);
    assert_eq!(c.prefix_bytes, 400);
    assert_eq!(c.task_bytes, 100);
    assert_eq!(c.history_bytes, 200);
    assert_eq!(c.tool_output_bytes, 1_300);
    assert_eq!(c.total_bytes(), 2_000);
}

#[test]
fn growth_is_tool_output_plus_history_and_never_includes_the_prefix() {
    let c = compose(&[
        seg(Part::Prefix, 1_000),
        seg(Part::Task, 1_000),
        seg(Part::ToolOutput, 1_500),
        seg(Part::History, 500),
    ]);
    let g = growth(&c);
    assert_eq!(g.growing_bytes, 2_000);
    assert!((g.growing_share - 0.5).abs() < 1e-9);
    assert!((g.tool_output_share - 0.375).abs() < 1e-9);
    assert!((g.history_share - 0.125).abs() < 1e-9);
}

#[test]
fn an_empty_prompt_reports_zero_growth_rather_than_dividing_by_zero() {
    let g = growth(&PromptComposition::default());
    assert_eq!(g.growing_share, 0.0);
    assert_eq!(g.tool_output_share, 0.0);
    assert_eq!(g.history_share, 0.0);
}

#[test]
fn a_provider_prompt_count_wins_over_the_byte_estimate() {
    let segments = vec![seg(Part::Task, 4_000)];
    let r = CallRecord::new(1, "author", &segments, Some(1_234), Some(1_000), None, 50);
    assert_eq!(r.prompt_tokens, 1_234);
    assert!(r.reported_usage);
    // The byte estimate is 1 000 tokens; the provider said 1 234 and that wins.
    assert_ne!(r.prompt_tokens, estimate_tokens(4_000));
}

#[test]
fn an_unreported_cache_is_none_and_never_a_fabricated_zero() {
    let r = CallRecord::new(1, "author", &[], None, None, None, 10);
    assert_eq!(r.hit_share(), None);
    assert_eq!(r.cache_hit_tokens, None);
    assert_eq!(r.cache_miss_tokens, None);
    assert!(ledger_line(&r).contains("cache -"));
}

#[test]
fn a_reported_cache_miss_is_used_verbatim_when_it_exceeds_prompt_minus_hit() {
    // DeepSeek re-bills a broken prefix: miss can exceed prompt - hit.
    let r = CallRecord::new(1, "author", &[], Some(1_000), Some(900), Some(250), 10);
    assert_eq!(r.cache_hit_tokens, Some(900));
    assert_eq!(r.cache_miss_tokens, Some(250));
    assert!((r.hit_share().unwrap() - 0.9).abs() < 1e-9);
}

#[test]
fn a_missing_reported_miss_is_derived_from_prompt_minus_hit() {
    let r = CallRecord::new(1, "author", &[], Some(1_000), Some(900), None, 10);
    assert_eq!(r.cache_miss_tokens, Some(100));
}

#[test]
fn a_hit_larger_than_the_prompt_never_underflows() {
    let r = CallRecord::new(1, "author", &[], Some(10), Some(99), None, 1);
    assert_eq!(r.cache_miss_tokens, Some(0));
    assert_eq!(r.hit_share(), Some(1.0));
}

#[test]
fn resend_tokens_never_exceed_the_prompt_that_was_sent() {
    let mut r = CallRecord::new(1, "author", &[], Some(1_000), None, None, 10);
    r.next_prompt_tokens = Some(1_500);
    assert_eq!(r.resend_tokens(), Some(1_000));
    r.next_prompt_tokens = None;
    assert_eq!(r.resend_tokens(), None);
}

#[test]
fn summarise_sums_prompts_and_only_counts_reported_cache_calls() {
    let mut a = CallRecord::new(
        1,
        "author",
        &[seg(Part::Task, 400)],
        Some(100),
        Some(90),
        None,
        5,
    );
    a.next_prompt_tokens = Some(200);
    let b = CallRecord::new(
        2,
        "author",
        &[seg(Part::Task, 800)],
        Some(200),
        None,
        None,
        6,
    );
    let s = summarise(&[a, b]);
    assert_eq!(s.calls, 2);
    assert_eq!(s.prompt_tokens, 300);
    assert_eq!(s.completion_tokens, 11);
    assert_eq!(s.cache_reported_calls, 1);
    assert_eq!(s.cache_hit_tokens, 90);
    assert_eq!(s.cache_miss_tokens, 10);
    assert_eq!(s.final_prompt_tokens, 200);
    assert_eq!(s.peak_prompt_tokens, 200);
    assert_eq!(s.resent_prompt_tokens, 100);
    assert!((s.hit_share().unwrap() - 0.9).abs() < 1e-9);
    assert!((s.resend_share() - 100.0 / 300.0).abs() < 1e-9);
}

#[test]
fn a_run_with_no_cache_counters_reports_no_hit_share() {
    let s = summarise(&[CallRecord::new(1, "author", &[], None, None, None, 1)]);
    assert_eq!(s.hit_share(), None);
    assert_eq!(s.resend_share(), 0.0);
}

#[test]
fn the_ledger_line_names_growth_and_its_source() {
    let r = CallRecord::new(
        7,
        "chunk:3/16",
        &[seg(Part::Prefix, 1_000), seg(Part::ToolOutput, 3_000)],
        Some(1_000),
        Some(998),
        None,
        42,
    );
    let line = ledger_line(&r);
    assert!(line.starts_with("t7 chunk:3/16 "), "{line}");
    assert!(line.contains("cache 998/1000"), "{line}");
    assert!(line.contains("growth=75%"), "{line}");
    assert!(line.contains("tool 75%"), "{line}");
}

#[test]
fn a_call_record_round_trips_through_json() {
    let r = CallRecord::new(
        2,
        "author",
        &[seg(Part::Task, 40)],
        Some(10),
        Some(9),
        None,
        3,
    );
    let text = serde_json::to_string(&r).unwrap();
    let back: CallRecord = serde_json::from_str(&text).unwrap();
    assert_eq!(back, r);
    let s = summarise(&[r]);
    let back: RunSummary = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert_eq!(back, s);
}

#[test]
fn part_maps_onto_the_shared_prompt_slice() {
    assert_eq!(Part::Prefix.as_slice(), PromptSlice::Prefix);
    assert_eq!(Part::Task.as_slice(), PromptSlice::Task);
    assert_eq!(Part::ToolOutput.as_slice(), PromptSlice::ToolOutput);
    assert_eq!(Part::History.as_slice(), PromptSlice::History);
}
