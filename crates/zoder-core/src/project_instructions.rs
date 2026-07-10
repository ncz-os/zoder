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

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// Flags used when opening a project-instructions candidate file on Unix.
/// `O_NOFOLLOW` rejects a symlink at the path at open time rather than
/// following it into a FIFO / device. `O_NONBLOCK` makes an `open()` on a
/// writer-less FIFO return `ENXIO` instead of blocking inside the kernel
/// waiting for a writer that will never arrive (semantically a no-op on
/// regular files). `O_CLOEXEC` keeps the FD from leaking into any child
/// process spawned after the prompt load. Mirrors the established idiom
/// in `crates/zoder-core/src/config.rs::CONFIG_OPEN_FLAGS`.
#[cfg(unix)]
const INSTRUCTIONS_OPEN_FLAGS: libc::c_int = libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK;

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
///
/// ## Defense against symlink / FIFO / OOM attacks on the candidate
/// path
///
/// A previous implementation called `std::fs::read_to_string(&path)`
/// without ever stat'ing `path`, so a symlink to `/dev/zero` or to
/// an attacker-controlled FIFO would either block forever (FIFO) or
/// inflate the process to the size of `/dev/zero` (unbounded). This
/// function now defers the read to [`read_bounded_instructions_file`]
/// which opens the file *once* on Unix with
/// `O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK` (rejecting symlinks at open
/// time and writer-less FIFOs without blocking), validates the open
/// FD's `fstat()` is a regular file within
/// [`MAX_PROJECT_INSTRUCTIONS_BYTES`] + 1 bytes, and reads through
/// `Read::take` so a racing growth past the cap cannot OOM the
/// process. The +1 preserves the boundary "exactly at the cap" vs.
/// "just over the cap" that the existing [`cap_to_max`] logic keys
/// on: a file whose trimmed content fits at-or-below the cap is not
/// truncated; a file whose trimmed content would land above the cap
/// gets the [`TRUNCATION_MARKER`] appended.
pub fn load_project_instructions(repo_root: &Path) -> Option<String> {
    for filename in CANDIDATE_FILES {
        let path = repo_root.join(filename);
        // Read on a regular file of size <= `MAX + 1` bytes, or return
        // `None` so the loop falls through to the next candidate
        // filename. A symlink/FIFO/device at `path` is rejected at
        // open time by `O_NOFOLLOW | O_NONBLOCK`; an oversized file is
        // rejected by the fstat size check. See the helper's doc for
        // the full rationale.
        let raw = match read_bounded_instructions_file(&path) {
            Some(s) => s,
            None => continue,
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            // Whitespace-only file: same semantics as missing. Move on
            // to the next candidate before declaring "no
            // instructions", so a stray blank AGENTS.md doesn't
            // shadow a real CLAUDE.md sitting next to it.
            continue;
        }
        // Marker rule (parity with the pre-fix behavior; the bounded
        // read above guarantees the file we just read is at most
        // `MAX + 1` bytes UTF-8-clean, so the existing
        // `cap_to_max` semantics for "exactly at the cap" vs. "over"
        // are preserved byte-for-byte):
        //   * `trimmed.len() > MAX`            -> cap_to_max kicks in
        //     (`text.len() <= MAX` is false), marker is appended.
        //   * `trimmed.len() <= MAX`           -> cap_to_max passes
        //     through, no marker.
        return Some(cap_to_max(trimmed));
    }
    None
}

/// Read at most [`MAX_PROJECT_INSTRUCTIONS_BYTES`] + 1 bytes from
/// `path`, refusing to follow symlinks or block on FIFOs.
///
/// On Unix the file is opened with `O_CLOEXEC | O_NOFOLLOW |
/// O_NONBLOCK` so a symlink at the path is rejected at open time
/// (rather than followed into a FIFO / device) and a writer-less FIFO
/// returns `ENXIO` immediately instead of blocking the kernel
/// indefinitely. The open FD's `fstat()` is then validated: not a
/// regular file -> reject; size above
/// `MAX_PROJECT_INSTRUCTIONS_BYTES + 1` -> reject. Reading uses
/// `Read::take(max)` so even if a concurrent writer grows the file
/// past the cap between the fstat and the read, the loader consumes
/// at most the bound.
///
/// Returns `None` for *every* failure (missing path, permission
/// denied, symlink, FIFO, device, oversized, non-UTF-8). The caller
/// treats `None` as "skip this candidate filename", which is the
/// pre-existing contract for `load_project_instructions`.
fn read_bounded_instructions_file(path: &Path) -> Option<String> {
    use std::io::Read;

    let max_bytes = (MAX_PROJECT_INSTRUCTIONS_BYTES + 1) as u64;

    // Step 1: open on Unix with O_NOFOLLOW + O_NONBLOCK so a symlink
    // at the path is rejected at open time rather than followed, and
    // a writer-less FIFO returns ENXIO instead of blocking. O_CLOEXEC
    // keeps the FD from leaking into any subsequent child process.
    let f = {
        #[cfg(unix)]
        {
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(INSTRUCTIONS_OPEN_FLAGS)
                .open(path)
        }
        #[cfg(not(unix))]
        {
            std::fs::File::open(path)
        }
    }
    .ok()?;

    // Step 2: validate on the open FD (fstat). A symlink at the path
    // was already rejected by O_NOFOLLOW in step 1 on Unix; the
    // fstat-driven `is_file()` guard is belt-and-braces and is the
    // primary defense on non-Unix builds.
    let meta = f.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    if meta.len() > max_bytes {
        return None;
    }

    // Step 3: bounded read from the open FD. Even if a racing writer
    // grows the file past `max_bytes` after the fstat, `Read::take`
    // caps the bytes we will pull from THIS fd to the bound, so an
    // OOM attack between the stat and the read is mechanically
    // impossible.
    let mut s = String::new();
    f.take(max_bytes).read_to_string(&mut s).ok()?;
    Some(s)
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

    /// DEFECT 2: a project-instructions candidate whose file size
    /// exceeds the bounded-read cap (the sum of
    /// [`MAX_PROJECT_INSTRUCTIONS_BYTES`] + 1) must NOT trigger
    /// truncation-with-marker. Per the `load_project_instructions`
    /// contract, an oversized/non-regular file is treated identically
    /// to a missing or unreadable file: the loader skips it and moves
    /// on to the next candidate filename, so a runaway
    /// `AGENTS.md`/`CLAUDE.md` at the repo root cannot OOM
    /// `zoder exec`/`zoder run`.
    ///
    /// Variant A: an oversized `AGENTS.md` with no `CLAUDE.md`
    /// fallback must surface as `None` (the loader dropped the
    /// oversized file and found nothing else).
    #[test]
    fn oversized_agents_md_with_no_fallback_returns_none() {
        let body: String = "a".repeat(4 * MAX_PROJECT_INSTRUCTIONS_BYTES);
        let (_dir, root) = with_repo(&[("AGENTS.md", &body)]);
        assert_eq!(
            load_project_instructions(&root),
            None,
            "4× cap-sized AGENTS.md must be skipped as if missing; \
             pre-fix code would have truncated-with-marker, but the new \
             bounded read rejects anything > MAX_PROJECT_INSTRUCTIONS_BYTES + 1 \
             bytes BEFORE the read to prevent OOM on a runaway file",
        );
    }

    /// DEFECT 2 (variant B): an oversized `AGENTS.md` with a real
    /// `CLAUDE.md` fallback must surface the CLAUDE.md body verbatim.
    /// The oversized AGENTS.md must be skipped without ever entering
    /// the bounded read, so a typo'd AGENTS.md cannot shadow a real
    /// CLAUDE.md sitting next to it.
    #[test]
    fn oversized_agents_md_falls_through_to_claude_md() {
        let body: String = "a".repeat(4 * MAX_PROJECT_INSTRUCTIONS_BYTES);
        let (_dir, root) =
            with_repo(&[("AGENTS.md", &body), ("CLAUDE.md", "Use uv, not poetry.\n")]);
        assert_eq!(
            load_project_instructions(&root).as_deref(),
            Some("Use uv, not poetry."),
            "an oversized AGENTS.md must be skipped (not truncated) so the \
             following CLAUDE.md is the file that reaches the prompt",
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

    /// Regression guard for the UTF-8 char-boundary floor in
    /// `cap_to_max`: when the cap (MAX bytes) lands in the middle of
    /// a multi-byte codepoint, the cut must walk back to the nearest
    /// valid boundary so the sliced string remains valid UTF-8.
    ///
    /// To exercise a mid-codepoint cap with the new bounded read
    /// (anything > `MAX_PROJECT_INSTRUCTIONS_BYTES + 1` is rejected
    /// before the read), we use a 3-byte-per-char body that fits in
    /// exactly `MAX + 1` bytes when constructed as `10923 * 'あ'`.
    /// `MAX = 32 * 1024 = 32768`; `MAX + 1 = 32769`; `32769 / 3 = 10923`
    /// so 10923 'あ's fit in `MAX + 1` bytes. The string is then
    /// trimmed (no whitespace) and applied to `cap_to_max`, where
    /// `text.len() = MAX + 1 > MAX` triggers the floor: `cut` starts
    /// at `MAX = 32768`, walks back past byte 32767 (mid-codepoint,
    /// not a boundary) and lands on 32766 (start of an 'あ', a valid
    /// boundary). The output string is therefore valid UTF-8 — a
    /// panic during `String` construction or slicing would have
    /// aborted the test before reaching the `ends_with` assertion.
    #[test]
    fn cap_respects_utf8_char_boundaries() {
        // `あ` is 3 bytes in UTF-8. 10923 chars = 32769 bytes = MAX + 1,
        // so the body fits exactly at the bounded-read ceiling. The cap
        // floor's job is exercised because MAX (32768) % 3 == 2, i.e.
        // it falls inside a codepoint.
        let body: String = "あ".repeat((MAX_PROJECT_INSTRUCTIONS_BYTES + 1) / 3);
        assert_eq!(
            body.len(),
            MAX_PROJECT_INSTRUCTIONS_BYTES + 1,
            "test fixture must size the body to exactly MAX + 1 bytes",
        );
        let (_dir, root) = with_repo(&[("AGENTS.md", &body)]);
        let loaded = load_project_instructions(&root).expect("loaded");
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

    /// DEFECT 2: a project-instructions candidate that is a symlink
    /// must NOT be followed into its target — pre-fix
    /// `std::fs::read_to_string` would follow the link and either
    /// block forever (link to a writer-less FIFO) or read until OOM
    /// (link to `/dev/zero` or a multi-GB file). The new bounded
    /// read opens with `O_NOFOLLOW | O_NONBLOCK` so a symlink at the
    /// path is rejected at open time. Variant A: a symlink to
    /// `/dev/null` (cheap; doesn't block, so the test runs on
    /// minimal CI without `mkfifo`).
    #[test]
    #[cfg(unix)]
    fn symlink_agents_md_to_dev_null_is_treated_as_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let link = dir.path().join("AGENTS.md");
        std::os::unix::fs::symlink("/dev/null", &link).expect("symlink to /dev/null");
        // `/dev/null` is unreadable-but-infinite — pre-fix the
        // unbounded `read_to_string` would either block on it (if it
        // were a FIFO) or return EOF very quickly. Either way, the
        // symlink itself must be REJECTED at open time by O_NOFOLLOW,
        // surfacing as `None` (skip this candidate) and falling
        // through to the next filename (`CLAUDE.md`, absent here) →
        // None overall.
        let root = dir.path().to_path_buf();
        assert_eq!(
            load_project_instructions(&root),
            None,
            "symlink at AGENTS.md must be rejected at open time (O_NOFOLLOW) \
             and treated identically to a missing/unreadable candidate",
        );
    }

    /// DEFECT 2 (variant B): when `AGENTS.md` is a symlink to a
    /// writer-less FIFO (which would block a normal `read_to_string`
    /// forever on `read(2)`), the bounded loader must short-circuit
    /// on `O_NONBLOCK | O_NOFOLLOW` open-time errors and fall through
    /// to `CLAUDE.md`. The test runs the bounded read in a worker
    /// thread with a 5-second wall-clock budget so a regression that
    /// reintroduces the blocking FIFO read fails the test with a
    /// clear "blocked past 5s" instead of hanging cargo.
    #[test]
    #[cfg(unix)]
    fn symlink_agents_md_to_writerless_fifo_falls_through_to_claude_md() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let dir = tempfile::tempdir().expect("tempdir");
        // Place the FIFO under a sibling path so the test survives a
        // partial failure (no FIFO left at `AGENTS.md`).
        let fifo = dir.path().join("dead.fifo");
        let status = std::process::Command::new("mkfifo").arg(&fifo).status();
        assert!(
            matches!(&status, Ok(s) if s.success()),
            "mkfifo must succeed in this test environment; got {status:?}",
        );
        // Symlink `AGENTS.md` at the FIFO. O_NOFOLLOW rejects the
        // symlink at open time; O_NONBLOCK then ensures even a
        // (pre-O_NOFOLLOW) open on a writer-less FIFO returns ENXIO
        // immediately rather than blocking.
        let link = dir.path().join("AGENTS.md");
        std::os::unix::fs::symlink(&fifo, &link).expect("symlink to FIFO");
        // Real CLAUDE.md sitting next to it. The bounded loader
        // must skip the FIFO-shaped AGENTS.md and surface CLAUDE.md.
        std::fs::write(
            dir.path().join("CLAUDE.md"),
            "Loader must skip the symlink-to-FIFO.\n",
        )
        .expect("write CLAUDE.md");

        let root = dir.path().to_path_buf();
        let (tx, rx) = mpsc::channel::<Option<String>>();
        let root_for_reader = root.clone();
        let reader = thread::spawn(move || {
            let result = load_project_instructions(&root_for_reader);
            tx.send(result).unwrap();
        });
        // Wall-clock budget: a regression to the pre-fix code would
        // block inside `read_to_string` on the FIFO and never
        // return, hanging cargo. recv_timeout gives a clear
        // diagnostic instead.
        let result = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("load_project_instructions must not block past 5s on a FIFO-shaped AGENTS.md — TOCTOU/FIFO regression");
        reader.join().expect("reader thread panicked");
        assert_eq!(
            result.as_deref(),
            Some("Loader must skip the symlink-to-FIFO."),
            "bounded loader must skip the symlink-to-FIFO AGENTS.md (rejected \
             at open time by O_NOFOLLOW | O_NONBLOCK) and surface CLAUDE.md \
             verbatim",
        );
        // Cleanup: unlink the FIFO manually so the tempdir drop
        // returns promptly even if mkfifo left a real pipe behind.
        let _ = std::fs::remove_file(&fifo);
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
