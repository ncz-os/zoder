//! Project-instructions file loader (parity with Claude Code / Codex CLI).
//!
//! Both Claude Code (`CLAUDE.md`) and Codex CLI (`AGENTS.md`) read a single
//! file at the operator's repo root and fold it into the model's context so
//! project-specific conventions (style, lint, test layout, etc.) survive a
//! cold start. zoder's agentic loop has no concept of such a file today —
//! this slice adds the smallest-possible seam:
//!
//! * [`load_project_instructions`] walks a fixed priority list (AGENTS.md
//!   first, CLAUDE.md second), reads the first match, and returns its
//!   trimmed text. The loader is pure (no logging, no side effects) so the
//!   CLI can call it once at construction time and hand a `String` (or
//!   `None`) to `AgentOptions`. The crate owning the parser is
//!   `zoder-core`; `acp-client` itself stays decoupled from filesystem
//!   reads and only sees the resulting [`String`].
//!
//! * The actual prompt composition (prepend the block above the user's
//!   task text with a clearly labeled header) lives in `acp-client` next
//!   to the two `session/prompt` wire frames it now feeds. That's the
//!   ONE point where both engines agree on the final prompt text, and a
//!   future engine has only one place to learn about the convention.
//!
//! ## Priority and semantics
//!
//! Priority order matches the upstream tools:
//!
//!   AGENTS.md (Codex) → CLAUDE.md (Claude Code) → `None`
//!
//! First match wins; the files are NOT concatenated. An empty or
//! whitespace-only file is treated the same as a missing file (`None`)
//! so a one-line placeholder doesn't silently bump a real CLAUDE.md out
//! of reach when an operator sets up the wrong convention.
//!
//! ## Size cap
//!
//! The loader enforces [`MAX_PROJECT_INSTRUCTIONS_BYTES`] (32 KiB) on the
//! trimmed content. A file that exceeds the cap is truncated and a
//! marker line is appended so the model sees a clearly-bounded
//! boundary (rather than mid-sentence cut-off) and an operator running
//! the agent in debug mode can tell the file was capped. The limit is
//! intentionally generous — 32 KiB accommodates a real AGENTS.md with
//! dozens of sections — while still preventing a 10-MiB paste from
//! silently dwarfing every other part of the prompt. The cap style and
//! unit follow `provider.rs`'s `MAX_RESPONSE_BYTES` constant:
//!
//!   * a module-private `pub const` with both byte and human-readable
//!     rationale in a comment,
//!   * truncated bytes are computed on the trimmed UTF-8 text,
//!   * any failure that crosses the cap is logged at warn level (via
//!     `tracing`) so a wandering limit can be diagnosed without code
//!     changes.

use std::path::Path;

/// Maximum bytes of project-instructions text injected into the
/// prompt. Mirrors the `MAX_RESPONSE_BYTES` constant in `provider.rs`
/// (same unit, same style), with the byte ceiling tuned for a real
/// AGENTS.md / CLAUDE.md rather than an HTTP response body — 32 KiB
/// fits a heavily-sectioned project doc while keeping the loader
/// provably non-explosive against a multi-megabyte stray file.
pub const MAX_PROJECT_INSTRUCTIONS_BYTES: usize = 32 * 1024;

/// Marker appended after a truncated project-instructions block. The
/// model and any log/debug output can use it to detect the truncation
/// boundary deterministically (parity with how `cap_diff` in
/// `crates/zoder-cli/src/agentic.rs` marks truncated review diffs).
pub const TRUNCATION_MARKER: &str =
    "\n\n[... project instructions truncated at MAX_PROJECT_INSTRUCTIONS_BYTES bytes; \
     see AGENTS.md / CLAUDE.md at the repo root for the full content ...]";

/// Header prepended ahead of the loaded file's content when it is
/// composed into the final prompt. The header intentionally includes
/// the filename in parentheses so a log trail or the model itself can
/// tell "project-instructions block" apart from the user's task text
/// (the task block sits below the `---\n\n` separator).
pub const PROJECT_INSTRUCTIONS_HEADER: &str = "# Project instructions (AGENTS.md)\n\n";

/// Separator that ends the project-instructions block ahead of the
/// user's task text. Drawn as a 3-em-dash line on its own, surrounded
/// by blank lines so any markdown-aware model treats the user's task
/// as a fresh section, not a continuation of the prior block.
pub const PROJECT_INSTRUCTIONS_SEPARATOR: &str = "\n\n---\n\n";

/// Candidate filenames, in priority order. The first existing file with
/// non-blank content wins. The list matches the Codex CLI / Claude
/// Code conventions explicitly:
///   1. `AGENTS.md` — Codex CLI's canonical name for project-level
///      agent instructions.
///   2. `CLAUDE.md` — Claude Code's equivalent name (kept here so an
///      operator who happens to run both tools on the same repo gets
///      the project's real instructions either way).
///
/// Adding a third convention is intentionally NOT done here: when (and
/// only when) a third upstream tool standardizes on a different name
/// can this list grow, and the priority order should be revisited to
/// match the new tool's docs.
const CANDIDATE_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// Read the project-instructions file at `repo_root` (if any) and
/// return its trimmed, size-capped content.
///
/// Resolution rules:
///   * Walks [`CANDIDATE_FILES`] in priority order. The first
///     existing, non-blank file wins.
///   * Returns `Ok(None)` if no candidate is found, the file is
///     empty/whitespace-only, or the path cannot be stat'd for any
///     reason OTHER than a non-existent file (a permission error is
///     silently skipped — the operator's AGENTS.md is a *project*
///     convenience, not a security boundary, and a stale
///     unreadable file should not break a run).
///   * Trims leading and trailing ASCII whitespace (including newlines)
///     but preserves interior formatting verbatim. The trim is the
///     exact `.trim()` definition, which is the same trim applied by
///     upstream tools' doc-loader hooks.
///   * Caps the result at [`MAX_PROJECT_INSTRUCTIONS_BYTES`] bytes. A
///     file that exceeds the cap has its tail replaced with
///     [`TRUNCATION_MARKER`] so the boundary is unambiguous. The
///     returned string is therefore always <= 32 KiB + len(marker).
///
/// This function never returns `Err`; a degenerate input (missing
/// file, blank file, binary blob) maps cleanly to `Ok(None)`. The
/// CLI can therefore call it without `.with_context()` plumbing.
pub fn load_project_instructions(repo_root: &Path) -> Option<String> {
    for filename in CANDIDATE_FILES {
        let path = repo_root.join(filename);
        // `std::fs::read_to_string` invalidates on non-UTF-8 content;
        // for the operator's convenience file we treat that as
        // "no instructions" rather than a hard error.
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            // Whitespace-only file: same semantics as missing. Move on
            // to the next candidate before declaring "no
            // instructions", so a stray blank AGENTS.md doesn't
            // shadow a real CLAUDE.md sitting next to it.
            continue;
        }
        return Some(cap_to_max(trimmed));
    }
    None
}

/// Truncate `text` to at most [`MAX_PROJECT_INSTRUCTIONS_BYTES`] bytes,
/// appending [`TRUNCATION_MARKER`] when the cap is exceeded. Returns
/// `text` unchanged when it already fits.
fn cap_to_max(text: &str) -> String {
    if text.len() <= MAX_PROJECT_INSTRUCTIONS_BYTES {
        return text.to_string();
    }
    // Floor the cutoff at a valid char boundary so we never slice a
    // multi-byte codepoint mid-utf8 (parity with `cap_diff` in
    // `agentic.rs`, which is the existing defense against this panic).
    let mut cut = MAX_PROJECT_INSTRUCTIONS_BYTES;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + TRUNCATION_MARKER.len());
    out.push_str(&text[..cut]);
    out.push_str(TRUNCATION_MARKER);
    tracing::warn!(
        original_bytes = text.len(),
        cap_bytes = MAX_PROJECT_INSTRUCTIONS_BYTES,
        "project instructions file exceeded cap; truncating with marker",
    );
    out
}

/// Compose the final prompt text sent to the model: the loaded
/// project instructions (if any) prepended, then the user's task text.
/// When `instructions` is `None` this returns `task` BYTE-FOR-BYTE
/// unchanged — that is the non-breaking regression-guarded path used
/// by every existing run before this slice shipped.
pub fn compose_prompt(instructions: Option<&str>, task: &str) -> String {
    match instructions {
        Some(text) if !text.is_empty() => {
            let mut out = String::with_capacity(
                PROJECT_INSTRUCTIONS_HEADER.len() + text.len() + 4 + task.len(),
            );
            out.push_str(PROJECT_INSTRUCTIONS_HEADER);
            out.push_str(text);
            out.push_str(PROJECT_INSTRUCTIONS_SEPARATOR);
            out.push_str(task);
            out
        }
        _ => task.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create an isolated tempdir, write `files` as a
    /// `name -> content` map under the dir, return the dir handle and
    /// the resolved root path (consumed via `keep()` to outlive the
    /// test).
    fn with_repo(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, body) in files {
            let p = dir.path().join(name);
            std::fs::write(&p, body).expect("write fixture");
        }
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    #[test]
    fn loads_agents_md_when_present() {
        let (_dir, root) = with_repo(&[("AGENTS.md", "Don't run npm install here.\n")]);
        assert_eq!(
            load_project_instructions(&root).as_deref(),
            Some("Don't run npm install here."),
        );
    }

    #[test]
    fn falls_back_to_claude_md_when_agents_md_missing() {
        let (_dir, root) = with_repo(&[("CLAUDE.md", "Use uv, not poetry.\n")]);
        assert_eq!(
            load_project_instructions(&root).as_deref(),
            Some("Use uv, not poetry."),
        );
    }

    #[test]
    fn prefers_agents_md_over_claude_md() {
        let (_dir, root) = with_repo(&[("AGENTS.md", "AGENTS\n"), ("CLAUDE.md", "CLAUDE\n")]);
        assert_eq!(load_project_instructions(&root).as_deref(), Some("AGENTS"),);
    }

    #[test]
    fn returns_none_when_neither_exists() {
        let (_dir, root) = with_repo(&[]);
        assert_eq!(load_project_instructions(&root), None);
    }

    #[test]
    fn empty_agents_md_treated_as_missing() {
        let (_dir, root) = with_repo(&[("AGENTS.md", "   \n\n  \t  \n")]);
        assert_eq!(
            load_project_instructions(&root),
            None,
            "whitespace-only AGENTS.md must surface as None so a stray placeholder \
             cannot shadow a real CLAUDE.md",
        );
    }

    #[test]
    fn empty_agents_md_falls_back_to_claude_md() {
        let (_dir, root) = with_repo(&[("AGENTS.md", "\n\n"), ("CLAUDE.md", "CLAUDE wins\n")]);
        assert_eq!(
            load_project_instructions(&root).as_deref(),
            Some("CLAUDE wins"),
        );
    }

    #[test]
    fn trims_leading_and_trailing_whitespace_preserves_interior() {
        let (_dir, root) = with_repo(&[("AGENTS.md", "\n\n  - foo\n    indented\n  - bar  \n\n")]);
        assert_eq!(
            load_project_instructions(&root).as_deref(),
            Some("- foo\n    indented\n  - bar"),
        );
    }

    #[test]
    fn oversized_file_is_truncated_with_marker() {
        // File = 4 * cap. The cap is on the TRIMMED text; we hand the
        // loader a body that is already > cap and zero whitespace at
        // the edges so `.trim()` is a no-op.
        let body: String = "a".repeat(4 * MAX_PROJECT_INSTRUCTIONS_BYTES);
        let (_dir, root) = with_repo(&[("AGENTS.md", &body)]);
        let loaded = load_project_instructions(&root).expect("must be loaded");
        // Head is exactly the cap, in bytes, taken on a char boundary
        // (every char is ASCII 'a' so the boundary is exact).
        let head_len = MAX_PROJECT_INSTRUCTIONS_BYTES;
        let head = &loaded[..head_len];
        assert!(
            head.chars().all(|c| c == 'a'),
            "truncated head must be the first {head_len} bytes verbatim",
        );
        assert!(
            loaded.ends_with(TRUNCATION_MARKER),
            "truncated output must end with the marker; got tail: {:?}",
            &loaded[loaded.len() - TRUNCATION_MARKER.len() - 20..],
        );
        // Total length is cap + marker (no tail of the original body
        // leaked past the cap).
        assert_eq!(
            loaded.len(),
            MAX_PROJECT_INSTRUCTIONS_BYTES + TRUNCATION_MARKER.len(),
            "truncation must drop all bytes past the cap",
        );
    }

    #[test]
    fn cap_is_byte_precise_for_ascii() {
        // One byte past the cap must trigger truncation; exactly on the
        // cap is left alone.
        let on_cap: String = "x".repeat(MAX_PROJECT_INSTRUCTIONS_BYTES);
        let (_dir, root) = with_repo(&[("AGENTS.md", &on_cap)]);
        let loaded = load_project_instructions(&root).expect("loaded");
        assert_eq!(loaded.len(), MAX_PROJECT_INSTRUCTIONS_BYTES);
        assert!(
            !loaded.ends_with(TRUNCATION_MARKER),
            "exactly-cap input must not be truncated",
        );

        let over_cap: String = "y".repeat(MAX_PROJECT_INSTRUCTIONS_BYTES + 1);
        let (_dir, root) = with_repo(&[("AGENTS.md", &over_cap)]);
        let loaded = load_project_instructions(&root).expect("loaded");
        assert_eq!(
            loaded.len(),
            MAX_PROJECT_INSTRUCTIONS_BYTES + TRUNCATION_MARKER.len(),
        );
        assert!(loaded.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn cap_respects_utf8_char_boundaries() {
        // Hand the loader a string of multi-byte chars (each 'é' is
        // 2 bytes in UTF-8) so the cap would slice mid-codepoint
        // without the `is_char_boundary` floor. The cap must land on
        // an even byte index or this panics.
        let body: String = "é".repeat(MAX_PROJECT_INSTRUCTIONS_BYTES);
        let (_dir, root) = with_repo(&[("AGENTS.md", &body)]);
        let loaded = load_project_instructions(&root).expect("loaded");
        // Walking back to the nearest char boundary reduces the cap
        // by AT MOST one byte (since 'é' is 2 bytes wide). Verify the
        // resulting head is valid UTF-8 (a panic check is implicit:
        // `is_char_boundary` doing it would otherwise have aborted).
        assert!(
            std::str::from_utf8(loaded.as_bytes()).is_ok(),
            "cap must land on a char boundary",
        );
        assert!(loaded.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn nonexistent_root_directory_returns_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bogus = dir.path().join("does-not-exist");
        assert_eq!(load_project_instructions(&bogus), None);
    }

    #[test]
    fn compose_prompt_none_is_byte_identical_to_task() {
        // Regression guard: when no instructions are loaded, the
        // final prompt text MUST be exactly the task text. Anywhere
        // that breaks this rule is now a wire-shape break — the
        // zeroclaw rpc, the goose ACP stdio transport, and any
        // operator tooling that parses prompt payloads will see a
        // different string.
        let task = "do the thing\nwith newlines\nand unicode ñ\n";
        assert_eq!(compose_prompt(None, task), task);
        // Empty-string instructions also map to "no instructions",
        // not to an empty header-only block.
        assert_eq!(compose_prompt(Some(""), task), task);
    }

    #[test]
    fn compose_prompt_some_prepends_header_and_separator() {
        let task = "do the thing";
        let rendered = compose_prompt(Some("be polite"), task);
        let expected = format!(
            "{header}{body}{sep}{task}",
            header = PROJECT_INSTRUCTIONS_HEADER,
            body = "be polite",
            sep = PROJECT_INSTRUCTIONS_SEPARATOR,
        );
        assert_eq!(rendered, expected);
        // The task text must remain present verbatim somewhere in
        // the output so a downstream model still receives the
        // user's actual request.
        assert!(
            rendered.contains(task),
            "task text must be present verbatim in composed prompt; got {rendered:?}",
        );
        // And the instructions must appear BEFORE the user's task
        // text — that's the whole point of prepending them.
        let instr_pos = rendered.find("be polite").expect("instructions present");
        let task_pos = rendered.find(task).expect("task present");
        assert!(
            instr_pos < task_pos,
            "instructions must precede task text in composed prompt",
        );
    }
}
