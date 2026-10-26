//! Wiring between `zoder_core::context_opt::review` and the review command.
//!
//! A chunked review is a fan of independent model calls over one diff. Before
//! this module every call resent the system prompt, the cross-chunk diff map,
//! the focus line and the partial-diff notice; a 16-chunk review therefore paid
//! for those bytes 16 times, and on a provider with a prefix cache they only
//! cost cache-read rates if the leading bytes are *identical* across chunks.
//! That is what [`render_review`] guarantees:
//!
//! * the constant head — system prompt, diff map, partial-diff notice, focus —
//!   is hoisted into one prefix that every chunk shares byte for byte;
//! * when every chunk carries a byte-identical cross-file context header, that
//!   header is hoisted too, so it is cached once instead of repeated per call;
//! * only the part banner, the chunk's own (unhoisted) context and the chunk
//!   itself vary, and they come last.
//!
//! Nothing is dropped: the hoisted header is still in every prompt, just once
//! and up front. That matters because the header exists to stop small reviewers
//! reporting names bound in another chunk as undefined — a review that is cheap
//! because it has less evidence is a worse review, not an optimised one.
//!
//! `ZODER_REVIEW_STABLE_PREFIX=0` restores the historical per-chunk layout for
//! a reviewer that turns out to dislike the reordering. The two layouts carry
//! the same evidence; only the byte order differs.

use zoder_core::context_opt::review::{
    chunk_prompt, hoistable_context, ChunkMessage, ReviewPrefix,
};

/// Whether the cache-stable review layout is enabled (default on).
pub fn stable_prefix_enabled() -> bool {
    match std::env::var("ZODER_REVIEW_STABLE_PREFIX") {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !(v.is_empty() || v == "0" || v == "false" || v == "no" || v == "off")
        }
        Err(_) => true,
    }
}

/// The rendered chunk prompts plus the prefix they all share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderedReview {
    pub prompts: Vec<String>,
    /// The constant head, exactly as it leads every prompt, so a caller can
    /// fingerprint it (cache-stability evidence) without re-deriving it.
    pub prefix: String,
    /// Whether a shared per-chunk context header was hoisted into the prefix.
    pub hoisted_context: bool,
}

/// One chunk prompt, exactly as it goes on the wire.
///
/// `context_for` is the CLI's own per-chunk header builder; it is called once
/// per distinct chunk id so a shared header can be detected and hoisted.
pub(crate) fn render_review(
    system: &str,
    diff_map: &str,
    label: &str,
    focus_txt: &str,
    chunks: &[String],
    context_for: &dyn Fn(usize) -> String,
) -> RenderedReview {
    let stable = stable_prefix_enabled();
    // The chunk ids are unique, so a shared header can only emerge from a
    // closure that really returned the same bytes for two chunks — which is
    // the condition under which hoisting is byte-identical.
    // The per-chunk headers are built ONCE and reused for the hoist check and
    // for the chunk bodies, so the two can never disagree about the bytes.
    let headers: Vec<String> = (0..chunks.len()).map(context_for).collect();
    let hoisted = if stable {
        hoistable_context(&headers)
    } else {
        None
    };
    let prefix = ReviewPrefix {
        system: system.to_string(),
        focus: if stable {
            String::new()
        } else {
            focus_sentence(focus_txt)
        },
        diff_map: diff_map.to_string(),
        head: head_sentence(label, focus_txt, chunks.len(), stable),
        context: hoisted.clone().unwrap_or_default(),
    };
    let prompts = chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            // The hoisted header is already in the prefix; repeating it here
            // would put the large, identical block back on every call, which is
            // the duplication this layout exists to remove.
            let ctx = if hoisted.is_some() {
                String::new()
            } else {
                context_for(i)
            };
            chunk_prompt(
                &prefix,
                &ChunkMessage {
                    part: i + 1,
                    parts: chunks.len(),
                    context: ctx,
                    chunk: chunk.clone(),
                },
            )
        })
        .collect();
    RenderedReview {
        prompts,
        prefix: prefix.render(),
        hoisted_context: hoisted.is_some(),
    }
}

fn focus_sentence(focus_txt: &str) -> String {
    if focus_txt.trim().is_empty() {
        String::new()
    } else {
        format!(" Focus especially on: {focus_txt}.")
    }
}

/// The sentence that introduces the diff.
///
/// Single-chunk reviews keep the historical wording verbatim. The multi-chunk
/// wording is constant per review (the *part* number moved into the variable
/// banner, where it cannot break the prefix).
fn head_sentence(label: &str, focus_txt: &str, n: usize, stable: bool) -> String {
    if !stable {
        // Historical layout: the CLI's own wording, kept for the escape hatch.
        return format!(
            "Review the following {label} diff{}.",
            if focus_txt.trim().is_empty() {
                String::new()
            } else {
                format!(" Focus especially on: {focus_txt}.")
            }
        );
    }
    let focus = focus_sentence(focus_txt);
    if n <= 1 {
        format!("Review the following {label} diff.{focus}")
    } else {
        format!("Review the following {label} diff, one part at a time.{focus}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(n: usize) -> Vec<String> {
        (1..=n)
            .map(|i| format!("diff --git a/a{i}.rs b/a{i}.rs\n@@ -1 +1 @@\n-x\n+y{i}\n"))
            .collect()
    }

    const MAP: &str = "Diff map (deterministic; covers ALL chunks of this diff):\nf a.rs +1 -1";

    fn render(n: usize, ctx: impl Fn(usize) -> String) -> RenderedReview {
        render_review("SYS", MAP, "branch", "", &chunks(n), &ctx)
    }

    #[test]
    fn every_chunk_shares_one_byte_identical_prefix() {
        let r = render(4, |_| String::new());
        assert_eq!(r.prompts.len(), 4);
        for p in &r.prompts {
            assert!(p.starts_with(&r.prefix), "{p}");
            // The diff map and the system prompt are in each prompt exactly
            // once — in the prefix, never repeated in the chunk body.
            assert_eq!(p.matches("Diff map").count(), 1);
            assert_eq!(p.matches("SYS").count(), 1);
        }
    }

    #[test]
    fn the_part_number_lives_only_in_the_variable_tail() {
        let r = render(3, |_| String::new());
        assert!(!r.prefix.contains("Part 1/3"));
        for (i, p) in r.prompts.iter().enumerate() {
            assert!(
                p.contains(&format!("Part {}/3 of this review.", i + 1)),
                "{p}"
            );
        }
    }

    #[test]
    fn a_single_chunk_keeps_the_head_in_the_prefix() {
        let r = render(1, |_| String::new());
        assert!(r.prefix.contains("Review the following branch diff."));
        assert!(r.prompts[0].starts_with(&r.prefix));
    }

    #[test]
    fn the_focus_line_is_constant_and_in_the_prefix() {
        let r = render_review("SYS", MAP, "branch", "error handling", &chunks(2), &|_| {
            String::new()
        });
        assert!(r.prefix.contains("Focus especially on: error handling."));
        for p in &r.prompts {
            assert_eq!(p.matches("Focus especially on").count(), 1);
        }
    }

    #[test]
    fn a_shared_context_header_is_hoisted_and_still_reaches_every_chunk() {
        // One header, three chunks: hoisted into the prefix and therefore sent
        // once per prompt instead of once per chunk body.
        let shared = render_review("SYS", MAP, "branch", "", &chunks(3), &|_| {
            "Context from the full post-change files: use std::io;".to_string()
        });
        assert!(shared.hoisted_context);
        for p in &shared.prompts {
            assert_eq!(
                p.matches("Context from the full post-change files").count(),
                1,
                "{p}"
            );
        }
    }

    #[test]
    fn a_per_chunk_context_header_stays_with_its_chunk() {
        let r = render(3, |i| format!("ctx-{i}"));
        assert!(!r.hoisted_context);
        for (i, p) in r.prompts.iter().enumerate() {
            assert!(p.contains(&format!("ctx-{i}")), "{p}");
            assert_eq!(p.matches("ctx-").count(), 1, "{p}");
        }
    }

    #[test]
    fn no_chunks_renders_nothing() {
        let r = render(0, |_| String::new());
        assert!(r.prompts.is_empty());
        assert!(!r.hoisted_context);
    }

    #[test]
    fn each_prompt_carries_its_own_chunk_exactly_once() {
        let r = render(3, |_| String::new());
        for (i, p) in r.prompts.iter().enumerate() {
            assert_eq!(p.matches(&format!("+y{}", i + 1)).count(), 1, "{p}");
        }
    }

    #[test]
    fn the_stable_layout_never_puts_a_volatile_value_in_the_prefix() {
        let r = render(2, |_| String::new());
        for needle in ["/home/", "session", "2026-10-25T", "iteration"] {
            assert!(
                !r.prefix.contains(needle),
                "{needle} leaked into {:?}",
                r.prefix
            );
        }
    }
}
