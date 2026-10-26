//! Tests for the cache-stable review baton (lever e).

use super::*;
use crate::context_opt::prefix::{check_message_stability, Volatile};

const SYSTEM: &str = "You are a meticulous senior software engineer performing a code review.";
const MAP: &str = "Diff map (deterministic; covers ALL chunks of this diff; the chunk under \
review is {n}/{t}). Before reporting a symbol as undefined, redefined, or recursively calling a \
missing function, check it against this map:\nf src/lib.rs +3 -1";

fn chunks(n: usize) -> Vec<String> {
    (1..=n)
        .map(|i| format!("diff --git a/f{i}.rs b/f{i}.rs\n@@ -1 +1 @@\n-a\n+b\n"))
        .collect()
}

fn prefix_for(contexts: &[String]) -> ReviewPrefix {
    review_prefix(
        SYSTEM,
        " Focus especially on: error handling.",
        MAP,
        "branch diff",
        contexts,
    )
}

#[test]
fn every_chunk_prompt_starts_with_one_byte_identical_prefix() {
    let plan = prefix_for(&[]);
    let prompts = review_prompts(&plan, &chunks(16), &[], "");
    let want = plan.render();
    for (i, p) in prompts.iter().enumerate() {
        assert!(
            p.starts_with(&want),
            "chunk {} does not start with the prefix",
            i + 1
        );
        let (head, tail) = split_at(p, want.len());
        assert_eq!(head, want);
        assert!(!tail.contains(MAP), "chunk {} repeats the diff map", i + 1);
        assert!(
            !tail.contains("meticulous"),
            "chunk {} repeats the system prompt",
            i + 1
        );
    }
}

#[test]
fn the_prefix_fingerprint_is_identical_across_chunks_and_sessions() {
    let a = prefix_for(&[]);
    let b = prefix_for(&[]);
    assert_eq!(review_fingerprint(&a), review_fingerprint(&b));
    assert_ne!(review_fingerprint(&a), 0);
}

#[test]
fn a_single_chunk_review_still_places_its_body_in_the_prefix() {
    let plan = review_prefix(SYSTEM, "", "", "branch diff", &[]);
    let prompts = review_prompts(&plan, &chunks(1), &[], "");
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].starts_with(&plan.render()));
    assert!(prompts[0].contains("```diff"));
}

#[test]
fn only_the_variable_tail_differs_between_chunks() {
    let plan = prefix_for(&[]);
    let prompts = review_prompts(&plan, &chunks(3), &[], "");
    let bound = plan.render().len();
    let tails: Vec<&str> = prompts.iter().map(|p| split_at(p, bound).1).collect();
    assert!(tails[0].contains("This is review part 1 of 3."));
    assert!(tails[1].contains("This is review part 2 of 3."));
    assert!(tails[2].contains("This is review part 3 of 3."));
    // The part banner is the first line of the tail and nothing else about the
    // chunk's position leaks into it; a byte count or timestamp there would
    // make the tail (not the prefix) the only thing that differs, which is the
    // property this test exists to pin.
    for (i, tail) in tails.iter().enumerate() {
        // `chunk_prompt` separates the prefix and the banner with one newline,
        // so the tail (everything after the prefix) opens on that separator and
        // the banner is its first non-empty line. No byte count or timestamp
        // joins it there.
        assert_eq!(
            tail.trim_start_matches('\n').lines().next(),
            Some(format!("This is review part {} of 3.", i + 1).as_str())
        );
    }
    // Every tail carries exactly one chunk.
    for t in &tails {
        assert_eq!(t.matches("```diff").count(), 1, "{t}");
    }
}

#[test]
fn an_identical_per_chunk_context_is_hoisted_into_the_prefix_once() {
    let ctx = "Context from the full post-change files (NOT part of the diff):\nuse std::io;\n"
        .to_string();
    let contexts = vec![ctx.clone(), ctx.clone(), ctx.clone()];
    let plan = prefix_for(&contexts);
    assert_eq!(plan.context, ctx);
    let prompts = review_prompts(&plan, &chunks(3), &contexts, "");
    for p in &prompts {
        assert_eq!(
            p.matches("Context from the full post-change").count(),
            1,
            "{p}"
        );
    }
}

#[test]
fn a_context_left_unhoisted_stays_with_its_own_chunk() {
    let contexts = vec!["ctx-a".to_string(), "ctx-b".to_string()];
    let plan = prefix_for(&contexts);
    assert!(plan.context.is_empty());
    let prompts = review_prompts(&plan, &chunks(2), &contexts, "");
    assert!(prompts[0].contains("ctx-a") && !prompts[0].contains("ctx-b"));
    assert!(prompts[1].contains("ctx-b") && !prompts[1].contains("ctx-a"));
}

#[test]
fn a_single_context_is_never_hoisted() {
    assert_eq!(hoistable_context(&["only".to_string()]), None);
    assert_eq!(hoistable_context(&[]), None);
    assert_eq!(
        hoistable_context(&["same".to_string(), "same".to_string()]),
        Some("same".into())
    );
}

#[test]
fn an_oversized_shared_context_is_left_in_the_chunks() {
    let big = "x".repeat(MAX_HOISTABLE_CONTEXT_BYTES + 1);
    assert_eq!(hoistable_context(&[big.clone(), big]), None);
}

#[test]
fn an_empty_context_is_never_hoisted() {
    let empty = "   ".to_string();
    assert_eq!(hoistable_context(&[empty.clone(), empty]), None);
}

#[test]
fn the_review_prefix_passes_the_volatile_leak_check() {
    let plan = review_prefix(SYSTEM, "", MAP, "branch diff, chunk", &[]);
    let volatile = Volatile {
        cwd: Some("/home/jasonperlow/Projects/zoder".into()),
        session_id: Some("sess-9f2c1ab4".into()),
        model: Some("deepseek-flash".into()),
        iteration: Some(7),
        timestamp: Some("1761432000".into()),
    };
    assert!(check_message_stability(&plan.as_prefix(), &plan.render(), &volatile).is_stable());
}

#[test]
fn a_cwd_leaked_into_a_per_chunk_header_breaks_stability() {
    let volatile = Volatile {
        cwd: Some("/home/jasonperlow/Projects/zoder".into()),
        ..Volatile::default()
    };
    let plan = review_prefix(SYSTEM, "", "", "branch diff", &[]);
    let msg = ChunkMessage {
        part: 1,
        parts: 1,
        preamble: String::new(),
        context: String::new(),
        chunk: "diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b /home/jasonperlow/Projects/zoder\n".into(),
    };
    let prompt = chunk_prompt(&plan, &msg);
    let (head, tail) = split_at(&prompt, plan.render().len());
    assert!(check_message_stability(&plan.as_prefix(), head, &volatile).is_stable());
    assert!(!check_message_stability(&plan.as_prefix(), tail, &volatile).is_stable());
}

#[test]
fn the_notice_is_constant_and_shared() {
    let a = prefix_for(&[]);
    let mut b = a.clone();
    b.head = "a different review".into();
    assert_eq!(a.render().matches(PARTIAL_DIFF_NOTICE).count(), 1);
    assert!(b.render().contains(PARTIAL_DIFF_NOTICE));
}

#[test]
fn an_empty_diff_map_leaves_no_blank_section() {
    let plan = review_prefix(SYSTEM, "", "", "working-tree diff", &[]);
    let rendered = plan.render();
    assert!(!rendered.contains("\n\n\n"), "{rendered:?}");
    assert!(rendered.ends_with('\n'));
}

#[test]
fn the_chunk_body_is_fenced_even_when_it_lacks_a_trailing_newline() {
    let msg = ChunkMessage {
        part: 1,
        parts: 1,
        preamble: String::new(),
        context: String::new(),
        chunk: "+x".into(),
    };
    let rendered = msg.render();
    assert!(rendered.ends_with("```\n"), "{rendered:?}");
    assert!(rendered.starts_with("```diff\n+x\n"));
}

#[test]
fn split_at_never_splits_a_multibyte_code_point() {
    let s = "abéclair";
    let (head, tail) = split_at(s, 3);
    assert_eq!(format!("{head}{tail}"), s);
    assert!(head.is_char_boundary(head.len()));
    // A boundary past the end is a whole-string split, not a panic.
    let (all, rest) = split_at(s, 9_999);
    assert_eq!((all, rest), (s, ""));
}

#[test]
fn a_review_of_zero_chunks_yields_no_prompts() {
    let plan = prefix_for(&[]);
    assert!(review_prompts(&plan, &[], &[], "").is_empty());
}
