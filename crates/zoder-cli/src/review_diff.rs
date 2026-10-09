//! Deterministic helpers for reviewing large diffs.
//!
//! Three problems this module solves, all without a model in the loop:
//!
//! 1. **Generated-file exclusion.** `--exclude <glob>` (repeatable), the
//!    `[review].exclude` config list, and a repo-root `.zoderignore` file all
//!    drop matching paths from the diff *and* from the size check, while
//!    reporting how many files/bytes were dropped and which pattern matched.
//! 2. **Chunk-aware context.** When a diff is split across several reviewer
//!    requests, [`build_diff_map`] emits a compact, deterministic map of every
//!    file's +/- counts and its added/removed definitions (plus rename pairs).
//!    The map is prepended to every chunk so a reviewer never reports a symbol
//!    as missing when it was merely defined in another chunk.
//! 3. **Configurable size caps.** [`DEFAULT_MAX_DIFF_BYTES`] (120000) and
//!    [`DEFAULT_MAX_HUNK_BYTES`] (9000) are only defaults; a CLI flag beats the
//!    `[review]` config key, which beats the default. An oversized single hunk
//!    is split deterministically into ordered sub-hunks at line boundaries
//!    (each labelled `part k/n of <file>`), unless splitting is disabled.
//!
//! Everything in here is pure except [`load_zoderignore`] and
//! [`repo_root`], which read the repository.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::bail;

/// Historical total-diff cap, kept as the default so behavior is unchanged
/// when neither the flag nor the config key is set.
pub const DEFAULT_MAX_DIFF_BYTES: usize = 120_000;
/// Historical per-hunk cap, kept as the default.
pub const DEFAULT_MAX_HUNK_BYTES: usize = 9_000;
/// Hard ceiling for the diff map. Larger maps are truncated with a count so
/// the per-chunk prompt stays bounded.
pub const DIFF_MAP_MAX_BYTES: usize = 4_096;
/// Historical maximum number of reviewer chunks. Derived caps can raise it.
pub const DEFAULT_MAX_REVIEW_CHUNKS: usize = 24;
/// Number of trailing context lines repeated at the start of the next part
/// when an oversized hunk is split. Only context (` `) lines are repeated, so
/// the union of the parts' added/removed lines is still exactly the original
/// hunk.
const HUNK_OVERLAP_LINES: usize = 3;
/// Cap on definitions listed per file in the diff map.
const MAX_DEFS_PER_FILE: usize = 40;

/// Resolve a size cap with flag-over-config-over-default precedence.
pub fn resolve_limit(flag: Option<usize>, config: Option<usize>, default: usize) -> usize {
    flag.or(config).unwrap_or(default)
}

/// Derive how many reviewer chunks a total cap implies (with slack), never
/// below the historical floor so the default path is unchanged.
pub fn derived_max_chunks(max_diff_bytes: usize, max_hunk_bytes: usize) -> usize {
    let per_chunk = max_hunk_bytes.max(1);
    let implied = max_diff_bytes / per_chunk + 4;
    implied.max(DEFAULT_MAX_REVIEW_CHUNKS)
}

// ---------------------------------------------------------------------------
// Glob matching
// ---------------------------------------------------------------------------

/// Match `path` (repo-relative, `/`-separated) against a gitignore-style glob.
///
/// * `*` matches any run of characters within one path segment.
/// * `?` matches one character within a segment.
/// * `**` matches any number of whole segments.
/// * A pattern with no `/` matches the path's basename at any depth
///   (`*.min.js` excludes `dist/app.min.js`).
/// * A trailing `/` is treated as "this directory and everything beneath".
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let mut pat = pattern.trim().to_string();
    if pat.is_empty() {
        return false;
    }
    let anchored = pat.starts_with('/');
    pat = pat
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_string();
    if pat.is_empty() {
        return false;
    }
    if pattern.trim_end().ends_with('/') {
        pat.push_str("/**");
    }
    let path = path.trim_start_matches("./");
    let vseg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if vseg.is_empty() {
        return false;
    }
    let pseg: Vec<&str> = pat.split('/').filter(|s| !s.is_empty()).collect();
    if pseg.is_empty() {
        return false;
    }
    if pseg.len() == 1 && !anchored && !pat.contains('/') {
        // No slash in the pattern: match the basename at any depth.
        return vseg.last().is_some_and(|base| segment_match(pseg[0], base));
    }
    match_segments(&pseg, &vseg)
}

fn match_segments(pat: &[&str], path: &[&str]) -> bool {
    match pat.first() {
        None => path.is_empty(),
        Some(&"**") => {
            // `**` consumes zero or more whole segments.
            match_segments(&pat[1..], path) || (!path.is_empty() && match_segments(pat, &path[1..]))
        }
        Some(seg) => match path.first() {
            Some(first) => segment_match(seg, first) && match_segments(&pat[1..], &path[1..]),
            None => false,
        },
    }
}

/// `*`/`?` matcher over a single path segment.
fn segment_match(pat: &str, value: &str) -> bool {
    let p = pat.as_bytes();
    let v = value.as_bytes();
    let (mut pi, mut vi) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_value = 0usize;
    while vi < v.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == v[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            pi += 1;
            star_value = vi;
        } else if let Some(sp) = star {
            pi = sp + 1;
            star_value += 1;
            vi = star_value;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

// ---------------------------------------------------------------------------
// Diff parsing
// ---------------------------------------------------------------------------

/// File-section starts: index 0 plus the byte after each `\ndiff --git `.
fn section_starts(diff: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    starts.extend(diff.match_indices("\ndiff --git ").map(|(i, _)| i + 1));
    starts
}

fn sections(diff: &str) -> Vec<&str> {
    let starts = section_starts(diff);
    let mut out = Vec::with_capacity(starts.len());
    for (i, start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(diff.len());
        out.push(&diff[*start..end]);
    }
    out
}

/// Repo-relative path for one diff section, taken from the `+++`/`---` header
/// (or `/dev/null` for pure adds/deletes). `None` when the section carries no
/// path (e.g. a leading preamble).
fn section_path(section: &str) -> Option<String> {
    let mut minus: Option<&str> = None;
    let mut plus: Option<&str> = None;
    for line in section.lines() {
        if line.starts_with("@@") {
            break;
        }
        if minus.is_none() {
            if let Some(rest) = line.strip_prefix("--- ") {
                minus = Some(rest);
            }
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            plus = Some(rest);
            break;
        }
    }
    clean_git_path(plus.or(minus)?)
}

fn clean_git_path(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw == "/dev/null" {
        return None;
    }
    let unquoted = if raw.starts_with('"') {
        unquote_git_path(raw)
    } else {
        raw.to_string()
    };
    let stripped = unquoted
        .strip_prefix("a/")
        .or_else(|| unquoted.strip_prefix("b/"))
        .unwrap_or(&unquoted);
    Some(stripped.to_string())
}

/// Minimal git C-style path unquoting: surrounding quotes plus `\"`, `\\`,
/// `\t`, `\n`, and octal escapes. Non-quoted paths are returned verbatim.
fn unquote_git_path(raw: &str) -> String {
    let inner = raw
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(raw);
    let bytes = inner.as_bytes();
    let mut out = String::with_capacity(inner.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'"' => {
                    out.push('"');
                    i += 2;
                }
                b'\\' => {
                    out.push('\\');
                    i += 2;
                }
                b't' => {
                    out.push('\t');
                    i += 2;
                }
                b'n' => {
                    out.push('\n');
                    i += 2;
                }
                b'r' => {
                    out.push('\r');
                    i += 2;
                }
                b'0'..=b'7' => {
                    // Up to three octal digits.
                    let mut val = 0u32;
                    let mut n = 0;
                    while n < 3
                        && i + 1 + n < bytes.len()
                        && (b'0'..=b'7').contains(&bytes[i + 1 + n])
                    {
                        val = val * 8 + u32::from(bytes[i + 1 + n] - b'0');
                        n += 1;
                    }
                    if let Some(c) = char::from_u32(val) {
                        out.push(c);
                    }
                    i += 1 + n;
                }
                other => {
                    out.push(other as char);
                    i += 2;
                }
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Exclusion
// ---------------------------------------------------------------------------

/// A file dropped from the diff by [`filter_diff`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcludedFile {
    pub path: String,
    pub bytes: usize,
    pub pattern: String,
}

/// Summary of everything [`filter_diff`] removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExclusionReport {
    pub files: Vec<ExcludedFile>,
    pub bytes: usize,
}

impl ExclusionReport {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Human-readable one-block summary for the review output / dry-run.
    pub fn render(&self) -> String {
        if self.files.is_empty() {
            return String::new();
        }
        let mut out = format!(
            "excluded {} file(s), {} bytes, from review",
            self.files.len(),
            self.bytes
        );
        for f in &self.files {
            out.push_str(&format!(
                "\n  - {} ({} bytes) matched `{}`",
                f.path, f.bytes, f.pattern
            ));
        }
        out
    }
}

/// Remove every diff section whose path matches one of `patterns`. Returns the
/// filtered diff (byte-for-byte identical to the input minus the dropped
/// sections) and a report naming the matched pattern for each dropped file.
pub fn filter_diff(diff: &str, patterns: &[String]) -> (String, ExclusionReport) {
    let mut report = ExclusionReport::default();
    if patterns.is_empty() {
        return (diff.to_string(), report);
    }
    let mut out = String::with_capacity(diff.len());
    for section in sections(diff) {
        let path = section_path(section);
        let matched = path
            .as_ref()
            .and_then(|p| patterns.iter().find(|pat| glob_match(pat, p)));
        match (path, matched) {
            (Some(path), Some(pattern)) => {
                report.bytes += section.len();
                report.files.push(ExcludedFile {
                    path,
                    bytes: section.len(),
                    pattern: pattern.clone(),
                });
            }
            _ => out.push_str(section),
        }
    }
    (out, report)
}

/// Read repository-relative exclude globs from `<root>/.zoderignore`.
/// Blank lines and `#` comments are ignored. A missing file yields no patterns.
pub fn load_zoderignore(root: &Path) -> Vec<String> {
    let path = root.join(".zoderignore");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Resolve the repository root for `cwd`, falling back to `cwd` itself.
pub fn repo_root(cwd: &Path) -> std::path::PathBuf {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            let s = s.trim();
            if s.is_empty() {
                cwd.to_path_buf()
            } else {
                std::path::PathBuf::from(s)
            }
        }
        _ => cwd.to_path_buf(),
    }
}

// ---------------------------------------------------------------------------
// Diff map
// ---------------------------------------------------------------------------

/// Build the compact, deterministic map prepended to every reviewer chunk.
/// Covers all files in `diff` (not just one chunk): per-file +/- counts,
/// added/removed definitions, and removed->added rename/replacement pairs.
pub fn build_diff_map(diff: &str) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for section in sections(diff) {
        let Some(path) = section_path(section) else {
            continue;
        };
        let (added, removed, add_defs, del_defs) = file_facts(section);
        let mut renames: BTreeSet<(String, String)> = BTreeSet::new();
        for d in &del_defs {
            for a in &add_defs {
                if a != d && a.contains(d.as_str()) && d.len() >= 3 {
                    renames.insert((d.clone(), a.clone()));
                }
            }
        }
        let mut block = format!("f {path} +{added} -{removed}\n");
        if !add_defs.is_empty() {
            block.push_str(&format!(" +defs: {}\n", join_defs(&add_defs)));
        }
        if !del_defs.is_empty() {
            block.push_str(&format!(" -defs: {}\n", join_defs(&del_defs)));
        }
        for (d, a) in &renames {
            block.push_str(&format!(" rename {d} -> {a}\n"));
        }
        blocks.push(block);
    }

    let mut map = String::from("diff-map (deterministic; covers ALL chunks):\n");
    let mut omitted = 0usize;
    let mut omitted_defs = 0usize;
    for block in &blocks {
        // Always reserve room for the trailing "omitted" note.
        const NOTE_RESERVE: usize = 96;
        if map.len() + block.len() + NOTE_RESERVE > DIFF_MAP_MAX_BYTES {
            omitted += 1;
            omitted_defs += block.matches("\n ").count();
            continue;
        }
        map.push_str(block);
    }
    if omitted > 0 {
        map.push_str(&format!(
            "... {omitted} more file(s) (~{omitted_defs} definition lines) omitted to keep the map under {DIFF_MAP_MAX_BYTES} bytes\n"
        ));
    }
    if map.len() > DIFF_MAP_MAX_BYTES {
        map.truncate(DIFF_MAP_MAX_BYTES);
        if !map.ends_with('\n') {
            map.push('\n');
        }
    }
    map
}

/// `(added_lines, removed_lines, added_defs, removed_defs)` for one section.
fn file_facts(section: &str) -> (usize, usize, Vec<String>, Vec<String>) {
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut add_defs: Vec<String> = Vec::new();
    let mut del_defs: Vec<String> = Vec::new();
    let mut in_hunks = false;
    for line in section.lines() {
        if !in_hunks {
            if line.starts_with("@@") {
                in_hunks = true;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            if rest.starts_with("++") {
                continue;
            }
            added += 1;
            for d in extract_defs(rest) {
                if !add_defs.contains(&d) {
                    add_defs.push(d);
                }
            }
        } else if let Some(rest) = line.strip_prefix('-') {
            if rest.starts_with("--") {
                continue;
            }
            removed += 1;
            for d in extract_defs(rest) {
                if !del_defs.contains(&d) {
                    del_defs.push(d);
                }
            }
        }
    }
    (added, removed, add_defs, del_defs)
}

fn join_defs(defs: &[String]) -> String {
    let mut sorted: Vec<&String> = defs.iter().collect();
    sorted.sort();
    sorted.dedup();
    if sorted.len() > MAX_DEFS_PER_FILE {
        let shown: Vec<&str> = sorted[..MAX_DEFS_PER_FILE]
            .iter()
            .map(|s| s.as_str())
            .collect();
        format!(
            "{} (+{} more)",
            shown.join(", "),
            sorted.len() - MAX_DEFS_PER_FILE
        )
    } else {
        sorted
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Keywords that introduce a named definition in the supported languages.
const DEF_KEYWORDS: &[&str] = &[
    "fn",
    "def",
    "func",
    "function",
    "class",
    "struct",
    "enum",
    "union",
    "interface",
    "trait",
    "type",
    "impl",
    "sub",
    "proc",
    "namespace",
    "errordomain",
    "record",
];

/// Control-flow / expression keywords that a `name(` scan must not mistake for
/// a C-style function definition.
const CONTROL_KEYWORDS: &[&str] = &[
    "if", "else", "for", "while", "switch", "case", "return", "sizeof", "do", "catch", "try",
    "throw", "new", "delete", "assert", "defined", "typeof", "await", "yield", "match", "when",
];

/// Extract candidate definition names from one (prefix-stripped) source line.
/// Language-agnostic: keyword definitions (`fn`/`def`/`class`/`struct`/…),
/// C-style `RET name(`, and shell-style `name() {`.
pub fn extract_defs(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let line = raw.trim();
    if line.is_empty() {
        return out;
    }
    let first = line.as_bytes()[0];
    if matches!(first, b'/' | b'*' | b'#') {
        return out;
    }
    if let Some(name) = keyword_def(line) {
        out.push(name);
        return out;
    }
    if let Some(name) = shell_def(line) {
        out.push(name);
        return out;
    }
    if let Some(name) = c_style_def(line) {
        out.push(name);
    }
    out
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn keyword_def(line: &str) -> Option<String> {
    // Only scan the declaration part: a `struct` inside a parameter list is
    // not the definition keyword (`static int wl_handle_init(struct s *st)`).
    let decl = match line.find('(') {
        Some(open) => &line[..open],
        None => line,
    };
    let bytes = decl.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if is_ident_start(bytes[i]) {
            let start = i;
            while i < bytes.len() && is_ident_char(bytes[i]) {
                i += 1;
            }
            let word = &decl[start..i];
            if DEF_KEYWORDS.contains(&word) {
                let mut j = i;
                while j < bytes.len() && !is_ident_start(bytes[j]) {
                    j += 1;
                }
                if j < bytes.len() {
                    let s2 = j;
                    while j < bytes.len() && is_ident_char(bytes[j]) {
                        j += 1;
                    }
                    return Some(decl[s2..j].to_string());
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

fn shell_def(line: &str) -> Option<String> {
    let open = line.find('(')?;
    let name = line[..open].trim_end();
    if name.is_empty() || !name.bytes().all(is_ident_char) || !is_ident_start(name.as_bytes()[0]) {
        return None;
    }
    let after = line[open + 1..].trim_start();
    if after.starts_with(')') {
        Some(name.to_string())
    } else {
        None
    }
}

fn c_style_def(line: &str) -> Option<String> {
    let open = line.find('(')?;
    let prefix = line[..open].trim_end();
    if prefix.contains('=') {
        return None;
    }
    let name_start = prefix.rfind(|c: char| !is_ident_char(c as u8))? + 1;
    let name = &prefix[name_start..];
    if name.is_empty() || !is_ident_start(name.as_bytes()[0]) {
        return None;
    }
    if CONTROL_KEYWORDS.contains(&name) {
        return None;
    }
    let before = prefix[..name_start].trim_end();
    if before.is_empty() {
        return None;
    }
    // A method/member call (`obj.foo(`, `ptr->foo(`) is not a definition.
    if before.ends_with('.') || before.ends_with("->") || before.ends_with("::") {
        return None;
    }
    // The return type must contain something type-like, not just punctuation.
    if !before
        .chars()
        .any(|c| c.is_alphanumeric() || c == '_' || c == '*' || c == '&')
    {
        return None;
    }
    Some(name.to_string())
}

// ---------------------------------------------------------------------------
// Hunk splitting and chunk packing
// ---------------------------------------------------------------------------

/// Split `diff` into reviewer chunks, each at most `max_hunk_bytes`.
///
/// Hunks larger than the cap are split into ordered sub-hunks at line
/// boundaries when `split_hunks` is set; otherwise the call fails naming
/// `--max-hunk-bytes`. `max_chunks` bounds the total number of chunks.
pub fn review_diff_chunks(
    diff: &str,
    max_hunk_bytes: usize,
    split_hunks: bool,
    max_chunks: usize,
) -> anyhow::Result<Vec<String>> {
    let cap = max_hunk_bytes.max(1);
    let mut units: Vec<String> = Vec::new();
    for section in sections(diff) {
        let path = section_path(section).unwrap_or_default();
        let Some(first_hunk) = section.find("\n@@").map(|i| i + 1) else {
            if section.len() > cap {
                bail!(
                    "one diff section is {} bytes with no split point, above the {}-byte review limit; pass --max-hunk-bytes N to raise it",
                    section.len(),
                    cap
                );
            }
            units.push(section.to_owned());
            continue;
        };
        let header = &section[..first_hunk];
        let body = &section[first_hunk..];
        let mut hunk_starts = vec![0usize];
        hunk_starts.extend(body.match_indices("\n@@").map(|(i, _)| i + 1));
        for (hidx, hstart) in hunk_starts.iter().enumerate() {
            let hend = hunk_starts.get(hidx + 1).copied().unwrap_or(body.len());
            let unit = format!("{header}{}", &body[*hstart..hend]);
            if unit.len() <= cap {
                units.push(unit);
            } else if split_hunks {
                let parts = split_oversized_hunk(header, &body[*hstart..hend], &path, cap)?;
                units.extend(parts);
            } else {
                bail!(
                    "one diff hunk is {} bytes, above the {}-byte review limit; pass --max-hunk-bytes N to raise it, or omit --no-split-hunks to split it automatically",
                    unit.len(),
                    cap
                );
            }
        }
    }

    let mut chunks: Vec<String> = Vec::new();
    for unit in units {
        if chunks
            .last()
            .is_none_or(|chunk| chunk.len() + unit.len() > cap)
        {
            chunks.push(String::new());
        }
        chunks
            .last_mut()
            .expect("chunk was just created")
            .push_str(&unit);
        if chunks.len() > max_chunks {
            bail!(
                "diff needs more than {max_chunks} review chunks at {cap} bytes each; raise --max-hunk-bytes (or --max-diff-bytes) or --exclude generated files"
            );
        }
    }
    Ok(chunks)
}

/// Split one oversized hunk (file `header` + a single `@@` hunk) into ordered
/// sub-hunks no larger than `cap`. Each part repeats the file header and a
/// recomputed hunk header labelled `part k/n of <file>`, with a few trailing
/// context lines carried over from the previous part for readability.
fn split_oversized_hunk(
    header: &str,
    hunk: &str,
    path: &str,
    cap: usize,
) -> anyhow::Result<Vec<String>> {
    let (hunk_header, body) = match hunk.find('\n') {
        Some(nl) => (&hunk[..nl], &hunk[nl + 1..]),
        None => (hunk, ""),
    };
    let (old_start, _, new_start, _) = parse_hunk_header(hunk_header);
    let lines: Vec<&str> = body.split_inclusive('\n').collect();
    if lines.is_empty() {
        bail!("cannot split an empty diff hunk for {path}");
    }

    // Reserve room for the repeated file header plus a generated hunk header.
    let overhead = header.len() + path.len() + 160;
    if overhead >= cap {
        bail!(
            "cannot split the diff hunk for {path}: the {}-byte cap is smaller than its file header; pass --max-hunk-bytes N to raise it",
            cap
        );
    }
    let budget = cap - overhead;

    let mut groups: Vec<(Vec<usize>, usize)> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    let mut cur_bytes = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if line.len() > budget {
            bail!(
                "cannot split the diff hunk for {path}: a single line is {} bytes, above the {}-byte cap; pass --max-hunk-bytes N to raise it",
                line.len(),
                cap
            );
        }
        if !cur.is_empty() && cur_bytes + line.len() > budget {
            groups.push((std::mem::take(&mut cur), 0));
            cur_bytes = 0;
        }
        cur.push(i);
        cur_bytes += line.len();
    }
    if !cur.is_empty() {
        groups.push((cur, 0));
    }

    // Prepend up to HUNK_OVERLAP_LINES trailing context lines from the previous
    // group to each subsequent group (tracked as `overlap` prefix length).
    let plain: Vec<Vec<usize>> = groups.into_iter().map(|(v, _)| v).collect();
    let mut with_overlap: Vec<(Vec<usize>, usize)> = Vec::with_capacity(plain.len());
    for (gi, group) in plain.iter().enumerate() {
        if gi == 0 {
            with_overlap.push((group.clone(), 0));
            continue;
        }
        let prev = &plain[gi - 1];
        // Only a contiguous suffix can be carried forward without moving
        // context across intervening additions/deletions and corrupting the
        // generated line numbers.
        let mut overlap: Vec<usize> = prev
            .iter()
            .rev()
            .copied()
            .take_while(|&j| lines[j].starts_with(' '))
            .take(HUNK_OVERLAP_LINES)
            .collect();
        overlap.reverse();
        // Never let the carried context defeat the cap.
        let mut overlap_bytes: usize = overlap.iter().map(|&j| lines[j].len()).sum();
        while !overlap.is_empty()
            && overlap_bytes + group.iter().map(|&j| lines[j].len()).sum::<usize>() > budget
        {
            let removed = overlap.remove(0);
            overlap_bytes -= lines[removed].len();
        }
        let ov = overlap.len();
        overlap.extend(group.iter().copied());
        with_overlap.push((overlap, ov));
    }

    let total = with_overlap.len();
    let mut parts = Vec::with_capacity(total);
    let mut old_pos = old_start;
    let mut new_pos = new_start;
    for (gi, (indices, overlap)) in with_overlap.iter().enumerate() {
        let mut ov_old = 0usize;
        let mut ov_new = 0usize;
        for &j in &indices[..*overlap] {
            let (o, n) = line_counts(lines[j]);
            ov_old += o;
            ov_new += n;
        }
        let part_old_start = old_pos.saturating_sub(ov_old);
        let part_new_start = new_pos.saturating_sub(ov_new);
        let mut count_old = 0usize;
        let mut count_new = 0usize;
        let mut core_old = 0usize;
        let mut core_new = 0usize;
        for (li, &j) in indices.iter().enumerate() {
            let (o, n) = line_counts(lines[j]);
            count_old += o;
            count_new += n;
            if li >= *overlap {
                core_old += o;
                core_new += n;
            }
        }
        let label = format!("part {}/{} of {}", gi + 1, total, path);
        let mut part = String::with_capacity(header.len() + budget + 160);
        part.push_str(header);
        part.push_str(&format!(
            "@@ -{part_old_start},{count_old} +{part_new_start},{count_new} @@ {label}\n"
        ));
        for &j in indices.iter() {
            part.push_str(lines[j]);
        }
        if !part.ends_with('\n') {
            part.push('\n');
        }
        anyhow::ensure!(part.len() <= cap, "split hunk exceeded {cap}-byte cap");
        parts.push(part);
        old_pos += core_old;
        new_pos += core_new;
    }
    Ok(parts)
}

/// `(old_lines, new_lines)` contributed by one hunk body line.
fn line_counts(line: &str) -> (usize, usize) {
    match line.as_bytes().first() {
        Some(b'-') => (1, 0),
        Some(b'+') => (0, 1),
        Some(b'\\') => (0, 0), // "No newline at end of file" is metadata.
        _ => (1, 1),
    }
}

/// Parse `@@ -a,b +c,d @@ tail` into `(old_start, old_count, new_start,
/// new_count)`, tolerating omitted counts.
fn parse_hunk_header(header: &str) -> (usize, usize, usize, usize) {
    let parse_pair = |s: &str| -> (usize, usize) {
        let (start, count) = s.split_once(',').unwrap_or((s, "1"));
        (
            start.trim().parse().unwrap_or(0),
            count.trim().parse().unwrap_or(1),
        )
    };
    let rest = header.strip_prefix("@@ ").unwrap_or(header);
    let mut old = (0usize, 1usize);
    let mut new = (0usize, 1usize);
    for tok in rest.split_whitespace().take(2) {
        if let Some(s) = tok.strip_prefix('-') {
            old = parse_pair(s);
        } else if let Some(s) = tok.strip_prefix('+') {
            new = parse_pair(s);
        }
    }
    (old.0, old.1, new.0, new.1)
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

/// The result of preparing a diff for review: what survived exclusion, the
/// exclusion report, the reviewer chunks, and the cross-chunk diff map.
#[derive(Debug, Clone)]
pub struct PreparedDiff {
    pub filtered: String,
    pub excluded: ExclusionReport,
    /// `None` when the diff fits in a single chunk (no cross-chunk blindness).
    pub diff_map: Option<String>,
    pub chunks: Vec<String>,
}

/// Full prep pipeline shared by `zoder review` and the dry-run mode.
///
/// Pattern precedence for exclusion is union (repo `.zoderignore`, then
/// configured `[review].exclude`, then `--exclude`). The total cap is checked
/// *after* exclusion; a diff over it fails naming `--max-diff-bytes`.
pub fn prepare_review_diff(
    repo_root: &Path,
    raw_diff: &str,
    cli_excludes: &[String],
    config_excludes: &[String],
    max_diff_bytes: usize,
    max_hunk_bytes: usize,
    split_hunks: bool,
) -> anyhow::Result<PreparedDiff> {
    let mut patterns = load_zoderignore(repo_root);
    patterns.extend(config_excludes.iter().cloned());
    patterns.extend(cli_excludes.iter().cloned());

    let (filtered, excluded) = filter_diff(raw_diff, &patterns);
    if filtered.len() > max_diff_bytes {
        bail!(
            "review diff is {} bytes, above the {}-byte limit; pass --max-diff-bytes N (or set [review].max_diff_bytes) to raise it, or --exclude <glob> to drop generated files",
            filtered.len(),
            max_diff_bytes
        );
    }
    let max_chunks = derived_max_chunks(max_diff_bytes, max_hunk_bytes);
    let chunks = review_diff_chunks(&filtered, max_hunk_bytes, split_hunks, max_chunks)?;
    let diff_map = if chunks.len() > 1 {
        Some(build_diff_map(&filtered))
    } else {
        None
    };
    Ok(PreparedDiff {
        filtered,
        excluded,
        diff_map,
        chunks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff_file(path: &str, hunk: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\nindex 1111111..2222222 100644\n--- a/{path}\n+++ b/{path}\n{hunk}"
        )
    }

    fn new_file(path: &str, body: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\nindex 0000000..2222222\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{} @@\n{body}",
            body.lines().count()
        )
    }

    #[test]
    fn resolve_limit_prefers_flag_then_config_then_default() {
        assert_eq!(resolve_limit(Some(7), Some(9), 3), 7);
        assert_eq!(resolve_limit(None, Some(9), 3), 9);
        assert_eq!(resolve_limit(None, None, 3), 3);
    }

    #[test]
    fn glob_matches_basename_at_any_depth() {
        assert!(glob_match("*.min.js", "dist/app.min.js"));
        assert!(glob_match("*.min.js", "app.min.js"));
        assert!(!glob_match("*.min.js", "dist/app.js"));
        assert!(glob_match("Cargo.lock", "deep/nested/Cargo.lock"));
        assert!(glob_match("*.tsv", "assets/x/y.tsv"));
    }

    #[test]
    fn glob_matches_path_patterns_and_doublestar() {
        assert!(glob_match("assets/**", "assets/x.tsv"));
        assert!(glob_match("assets/**", "assets/a/b/c.tsv"));
        assert!(!glob_match("assets/**", "src/a.tsv"));
        assert!(glob_match("**/generated/*.json", "a/b/generated/x.json"));
        assert!(glob_match("src/*.rs", "src/lib.rs"));
        assert!(!glob_match("src/*.rs", "src/nested/lib.rs"));
        assert!(glob_match("generated/", "generated/a/b.json"));
    }

    #[test]
    fn filter_diff_drops_matches_and_reports_them() {
        let d = format!(
            "{}{}",
            new_file("assets/big.tsv", "+a\n+b\n"),
            diff_file("src/real.c", "@@ -1 +1 @@\n-old\n+new\n")
        );
        let (out, rep) = filter_diff(&d, &["*.tsv".to_string()]);
        assert!(!out.contains("assets/big.tsv"));
        assert!(out.contains("src/real.c"));
        assert_eq!(rep.files.len(), 1);
        assert_eq!(rep.files[0].path, "assets/big.tsv");
        assert_eq!(rep.files[0].pattern, "*.tsv");
        assert!(rep.bytes > 0);
        assert!(rep.render().contains("excluded 1 file(s)"));
    }

    #[test]
    fn filter_diff_with_no_patterns_is_byte_identical() {
        let d = diff_file("src/a.c", "@@ -1 +1 @@\n-x\n+y\n");
        let (out, rep) = filter_diff(&d, &[]);
        assert_eq!(out, d);
        assert!(rep.is_empty());
    }

    #[test]
    fn diff_map_extracts_definitions_and_rename_pairs() {
        let d = format!(
            "{}{}{}",
            diff_file(
                "launcher/idled.c",
                "@@ -1 +1 @@\n-static int wl_handle_init(struct s *st, GError **err)\n+static int wl_handle_init_once(struct s *st, GError **err)\n"
            ),
            diff_file(
                "launcher/idled.c",
                "@@ -10 +10 @@\n-static int wl_handle_init(struct s *st, GError **err)\n+static int wl_handle_init(struct s *st, GError **err)\n"
            ),
            diff_file(
                "src/app.py",
                "@@ -1 +1 @@\n-def old_name():\n+def new_name():\n+class Helper:\n"
            ),
        );
        let map = build_diff_map(&d);
        assert!(map.contains("wl_handle_init_once"), "{map}");
        assert!(
            map.contains("rename wl_handle_init -> wl_handle_init_once"),
            "{map}"
        );
        assert!(map.contains("new_name"), "{map}");
    }

    #[test]
    fn diff_map_is_truncated_under_the_cap() {
        let mut body = String::new();
        for i in 0..500 {
            body.push_str(&format!("+static int generated_fn_{i}(void)\n"));
        }
        let mut d = String::new();
        for f in 0..30 {
            d.push_str(&new_file(&format!("gen/file_{f}.c"), &body));
        }
        let map = build_diff_map(&d);
        assert!(
            map.len() <= DIFF_MAP_MAX_BYTES,
            "map was {} bytes",
            map.len()
        );
        assert!(map.contains("omitted"), "{map}");
    }

    #[test]
    fn diff_map_unchanged_by_hunk_splitting() {
        let mut body = String::new();
        for i in 0..2000 {
            body.push_str(&format!("+int line_{i} = {i};\n"));
        }
        let d = new_file("src/big.c", &body);
        let before = build_diff_map(&d);
        let _ = review_diff_chunks(&d, 9000, true, 64).unwrap();
        let after = build_diff_map(&d);
        assert_eq!(before, after);
    }

    #[test]
    fn size_check_names_the_knob_without_excludes() {
        let d = new_file("assets/generated.tsv", &"+x\n".repeat(2000));
        let dir = tempfile::tempdir().unwrap();
        let err = prepare_review_diff(dir.path(), &d, &[], &[], 5000, 9000, true).unwrap_err();
        assert!(err.to_string().contains("--max-diff-bytes"), "{err}");
    }

    #[test]
    fn integration_large_generated_diff_excluded_then_proceeds() {
        let generated = new_file("assets/catalog.tsv", &"+col\n".repeat(4000));
        let real = diff_file(
            "src/real.c",
            "@@ -1,2 +1,2 @@\n-int old(void) {\n+int old_once(void) {\n return 0;\n",
        );
        let d = format!("{generated}{real}");
        let dir = tempfile::tempdir().unwrap();

        // Without --exclude the generated file pushes it over the cap: the
        // error must name the knob.
        let err = prepare_review_diff(dir.path(), &d, &[], &[], 5000, 9000, true).unwrap_err();
        assert!(err.to_string().contains("--max-diff-bytes"), "{err}");

        // With --exclude it proceeds and reports exactly what was dropped.
        let prepared = prepare_review_diff(
            dir.path(),
            &d,
            &["*.tsv".to_string()],
            &[],
            5000,
            9000,
            true,
        )
        .unwrap();
        assert!(prepared.excluded.files.len() == 1);
        assert!(prepared.filtered.contains("src/real.c"));
        assert!(!prepared.filtered.contains("assets/catalog.tsv"));
        assert!(prepared.excluded.render().contains("matched `*.tsv`"));
        assert!(!prepared.chunks.is_empty());
    }

    #[test]
    fn config_exclude_patterns_are_honored() {
        let d = format!(
            "{}{}",
            new_file("assets/big.tsv", "+a\n"),
            diff_file("src/real.c", "@@ -1 +1 @@\n-x\n+y\n")
        );
        let dir = tempfile::tempdir().unwrap();
        let prepared = prepare_review_diff(
            dir.path(),
            &d,
            &[],
            &["*.tsv".to_string()],
            9000,
            9000,
            true,
        )
        .unwrap();
        assert!(!prepared.filtered.contains("big.tsv"));
    }

    #[test]
    fn zoderignore_defaults_are_loaded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".zoderignore"),
            "# generated files\n*.tsv\n\n*.min.js\n",
        )
        .unwrap();
        let pats = load_zoderignore(dir.path());
        assert_eq!(pats, vec!["*.tsv".to_string(), "*.min.js".to_string()]);
    }

    #[test]
    fn hunk_split_40kb_new_file_parts_are_ordered_and_exact() {
        let file_content: String = (0..2000)
            .map(|i| format!("int generated_line_{i}(void) {{\n"))
            .collect();
        let body: String = file_content.lines().map(|l| format!("+{l}\n")).collect();
        let d = new_file("src/huge.c", &body);
        assert!(d.len() > 40_000, "fixture only {} bytes", d.len());

        let chunks = review_diff_chunks(&d, 9000, true, 64).unwrap();
        // Each chunk respects the cap.
        for chunk in &chunks {
            assert!(chunk.len() <= 9000, "chunk was {} bytes", chunk.len());
        }
        // Every part is labelled and carries the file header.
        let joined = chunks.join("");
        assert!(joined.contains("part 1/"), "no part label");
        assert!(joined.contains("@@ -0,0 +1,"));
        // Concatenating the added lines reproduces the file exactly.
        let added: String = joined
            .lines()
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .map(|l| format!("{}\n", &l[1..]))
            .collect();
        assert_eq!(added, file_content, "added lines do not reproduce the file");
    }

    #[test]
    fn hunk_split_disabled_errors_naming_the_knob() {
        let body: String = (0..2000)
            .map(|i| format!("+int generated_line_{i}(void) {{\n"))
            .collect();
        let d = new_file("src/huge.c", &body);
        let err = review_diff_chunks(&d, 9000, false, 64).unwrap_err();
        assert!(err.to_string().contains("--max-hunk-bytes"), "{err}");
        assert!(err.to_string().contains("--no-split-hunks"), "{err}");
    }

    #[test]
    fn single_small_hunk_is_not_split() {
        let d = diff_file("src/a.c", "@@ -1 +1 @@\n-old\n+new\n");
        let chunks = review_diff_chunks(&d, 9000, true, 24).unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(!chunks[0].contains("part 1/"));
    }

    #[test]
    fn derived_max_chunks_scales_with_caps() {
        assert_eq!(
            derived_max_chunks(DEFAULT_MAX_DIFF_BYTES, DEFAULT_MAX_HUNK_BYTES),
            24
        );
        assert!(derived_max_chunks(1_000_000, 9000) > 100);
    }

    #[test]
    fn split_long_utf8_path_preserves_bytes_and_bound() {
        let path = format!("{}/résumé.rs", "nested/".repeat(50));
        let body = "+let message = \"こんにちは\";\n".repeat(700);
        let chunks = review_diff_chunks(&new_file(&path, &body), 9000, true, 64).unwrap();
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.len() <= 9000));
        let recovered: String = chunks
            .iter()
            .flat_map(|c| c.split_inclusive('\n'))
            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
            .collect();
        assert_eq!(recovered, body);
    }

    #[test]
    fn hunk_positions_ignore_metadata_and_function_tail() {
        assert_eq!(line_counts("\\ No newline at end of file\n"), (0, 0));
        assert_eq!(
            parse_hunk_header("@@ -10,3 +20,4 @@ fn f() +999 -888"),
            (10, 3, 20, 4)
        );
        let header = "diff --git a/a b/a\n--- a/a\n+++ b/a\n";
        // Force groups with a context line followed by an addition at the
        // boundary. Non-contiguous context must not be moved into part 2.
        let hunk = format!(
            "@@ -1,1 +1,4 @@\n {}\n+{}\n+{}\n+{}\n",
            "c".repeat(48),
            "a".repeat(48),
            "b".repeat(48),
            "d".repeat(48)
        );
        let parts = split_oversized_hunk(header, &hunk, "a", header.len() + 161 + 102).unwrap();
        assert_eq!(parts.len(), 2);
        assert!(parts[1].contains("@@ -2,0 +3,2 @@"), "{}", parts[1]);
        assert!(!parts[1].contains(&"c".repeat(48)));
    }
}
