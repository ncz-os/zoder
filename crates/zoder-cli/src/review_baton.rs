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
//! * each chunk's cross-file context remains in that chunk's variable tail;
//!   it is never placed in the shared head;
//! * only the part banner, the chunk's context and the chunk itself vary, and
//!   they come last.
//!
//! Nothing is dropped: the per-chunk header remains in every applicable prompt.
//! That matters because the header exists to stop small reviewers
//! reporting names bound in another chunk as undefined — a review that is cheap
//! because it has less evidence is a worse review, not an optimised one.
//!
//! `ZODER_REVIEW_STABLE_PREFIX=0` restores the historical per-chunk layout for
//! a reviewer that turns out to dislike the reordering. The two layouts carry
//! the same evidence; only the byte order differs.

use zoder_core::context_opt::review::{chunk_prompt, ChunkMessage, ReviewPrefix};

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
    /// Byte length of the constant head at the front of every prompt. A
    /// caller that wants to attribute bytes (the telemetry layer) needs to
    /// know where the head ends and the variable tail begins.
    pub prefix_bytes: usize,
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
    preamble: &str,
    context_for: &dyn Fn(usize) -> String,
) -> RenderedReview {
    let stable = stable_prefix_enabled();
    if !stable {
        return render_review_legacy(label, focus_txt, chunks, diff_map, preamble, context_for);
    }
    // Build each header once. Context is intentionally not hoisted: it is
    // hunk-specific evidence and belongs in the variable tail.
    let headers: Vec<String> = (0..chunks.len()).map(context_for).collect();
    let prefix = ReviewPrefix {
        system: system.to_string(),
        focus: focus_sentence(focus_txt),
        diff_map: diff_map.to_string(),
        head: head_sentence(label, chunks.len()),
        context: String::new(),
    };
    let prompts = chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let ctx = headers.get(i).cloned().unwrap_or_default();
            chunk_prompt(
                &prefix,
                &ChunkMessage {
                    part: i + 1,
                    parts: chunks.len(),
                    preamble: preamble.to_string(),
                    context: ctx,
                    chunk: chunk.clone(),
                },
            )
        })
        .collect();
    let rendered = prefix.render();
    RenderedReview {
        prefix_bytes: rendered.len(),
        prompts,
        prefix: rendered,
        hoisted_context: false,
    }
}

/// The historical layout, kept behind `ZODER_REVIEW_STABLE_PREFIX=0`.
///
/// This is the byte-for-byte pre-baton shape: the per-chunk portion label
/// first, the cross-chunk diff map and the per-chunk context repeated on
/// every chunk, and the "this is review part N of M" statement in front of
/// all of it. It has no cache-stable head at all — the leading bytes differ
/// on every chunk — which is exactly why it is opt-out rather than the
/// default.
///
/// It exists for a reviewer that turns out to dislike the reordering, not as
/// a cheaper or a better layout.
fn render_review_legacy(
    label: &str,
    focus_txt: &str,
    chunks: &[String],
    diff_map: &str,
    preamble: &str,
    context_for: &dyn Fn(usize) -> String,
) -> RenderedReview {
    let chunk_count = chunks.len();
    let focus = focus_sentence(focus_txt);
    let prompts = chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let portion = if chunk_count == 1 {
                format!("{label} diff")
            } else {
                format!("{label} diff, chunk {}/{}", i + 1, chunk_count)
            };
            let map = if diff_map.is_empty() {
                String::new()
            } else {
                format!(
                    "\n\nDiff map (deterministic; covers ALL chunks of this diff; the chunk under \
review is {}/{}).\n{diff_map}",
                    i + 1,
                    chunk_count
                )
            };
            let context = context_for(i);
            format!(
                "{preamble}Review the following {portion}.{focus} Report only concrete defects \
visible in this portion; do not infer missing code from other chunks.{map}{context}\n\n\
```diff\n{chunk}\n```"
            )
        })
        .collect();
    RenderedReview {
        prompts,
        prefix: String::new(),
        prefix_bytes: 0,
        hoisted_context: false,
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
///
/// The focus line is NOT repeated here: it is already a section of the
/// prefix, and saying it twice would put the same sentence in the prompt
/// twice.
fn head_sentence(label: &str, n: usize) -> String {
    if n <= 1 {
        format!("Review the following {label} diff.")
    } else {
        format!("Review the following {label} diff, one part at a time.")
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
        render_review("SYS", MAP, "branch", "", &chunks(n), "", &ctx)
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
                p.contains(&format!("This is review part {} of 3.", i + 1)),
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
        let r = render_review(
            "SYS",
            MAP,
            "branch",
            "error handling",
            &chunks(2),
            "",
            &|_| String::new(),
        );
        assert!(r.prefix.contains("Focus especially on: error handling."));
        for p in &r.prompts {
            assert_eq!(p.matches("Focus especially on").count(), 1);
        }
    }

    #[test]
    fn a_shared_context_header_stays_in_each_chunk_tail() {
        // One header, three chunks: each tail carries its own copy.
        let shared = render_review("SYS", MAP, "branch", "", &chunks(3), "", &|_| {
            "Context from the full post-change files: use std::io;".to_string()
        });
        assert!(!shared.hoisted_context);
        assert!(!shared
            .prefix
            .contains("Context from the full post-change files"));
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

    /// The regression that matters: the leading bytes of the prompt that
    /// actually goes on the wire must be identical for every chunk.
    ///
    /// The baton built a constant head, but the CLI used to PREPEND a
    /// "this is review part N of M" sentence to every chunk prompt and the
    /// loop review prepended its preamble (the task text and the validation
    /// output) on top of that. Both put a varying value in front of the
    /// head, so no two chunks of a review ever shared a cache prefix and the
    /// cache-stable layout bought nothing.
    #[test]
    fn every_chunk_still_starts_with_the_same_bytes_when_a_preamble_is_present() {
        let r = render_review(
            "SYS",
            MAP,
            "branch",
            "",
            &chunks(3),
            "TASK: fix it\n",
            &|_| String::new(),
        );
        let head = &r.prefix;
        for p in &r.prompts {
            assert!(
                p.starts_with(head),
                "the preamble must come AFTER the cache-stable head, not before it:\n{p}"
            );
        }
        // The preamble still reaches the reviewer.
        assert!(r.prompts[0].contains("TASK: fix it"));
    }

    #[test]
    fn the_part_number_never_leads_the_prompt() {
        let r = render_review("SYS", MAP, "branch", "", &chunks(3), "", &|_| String::new());
        for (i, p) in r.prompts.iter().enumerate() {
            assert!(p.starts_with(&r.prefix), "{p}");
            assert!(p.contains(&format!("This is review part {} of 3.", i + 1)));
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
