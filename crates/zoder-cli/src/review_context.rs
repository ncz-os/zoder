//! Cross-chunk context header for review chunks.
//!
//! A reviewer sees one chunk of a diff at a time: added/removed lines plus a
//! few lines of context. It cannot see a binding made 100 lines earlier in an
//! unchanged part of the same function, or an import at the top of a new file
//! that landed in another chunk, and small reviewer models then report those
//! names as "undefined" or "missing" (measured: gemma4-31b flagged `{label}`
//! in `cmd_review`, bound at agentic.rs:3463, and imports/constants of a new
//! test file that sat in another chunk).
//!
//! For each file a chunk touches, this builds a short, bounded header from the
//! post-change file: its imports, its top-level definitions and constants,
//! and, for every hunk, the enclosing definition and the names bound earlier
//! in it. The header is labelled as context, not diff, so it is never
//! reviewed itself.

use std::collections::BTreeMap;

use crate::review_diff::extract_defs;

/// Default byte budget for one chunk's context header.
pub(crate) const CONTEXT_BUDGET_BYTES: usize = 3000;
/// Smallest context budget, for reviewers whose per-chunk cap is tiny.
pub(crate) const CONTEXT_MIN_BYTES: usize = 512;
/// Context may never take more than this fraction of the per-route chunk cap.
pub(crate) const CONTEXT_SHARE_DENOMINATOR: usize = 4;
/// Bytes reserved for the wrapper sentence the CLI puts around the header.
pub(crate) const CONTEXT_WRAPPER_RESERVE_BYTES: usize = 320;
/// Files at or below this size can be supplied whole instead of as a list of
/// defining lines.
const WHOLE_FILE_THRESHOLD_BYTES: usize = 4096;
const MAX_IMPORTS_PER_FILE: usize = 25;
const MAX_DEFS_PER_FILE: usize = 60;
const MAX_BINDINGS_PER_HUNK: usize = 30;
/// Cap on how many referenced symbols are resolved per chunk.
const MAX_REFERENCED_SYMBOLS: usize = 80;
/// Lines of context printed around a defining line.
const DEF_CONTEXT_LINES: usize = 2;
/// How far above a hunk to look for its enclosing definition.
const MAX_SCOPE_LOOKBACK: usize = 400;

/// The context budget for a reviewer whose per-route chunk cap is
/// `chunk_cap`. The header is capped at a quarter of the chunk budget (never
/// above [`CONTEXT_BUDGET_BYTES`], never below [`CONTEXT_MIN_BYTES`]).
pub(crate) fn context_budget_for_chunk_cap(chunk_cap: usize) -> usize {
    (chunk_cap / CONTEXT_SHARE_DENOMINATOR).clamp(CONTEXT_MIN_BYTES, CONTEXT_BUDGET_BYTES)
}

/// The effective diff-chunk cap when definition context is enabled: reserve
/// the context share plus its wrapper inside the per-route chunk budget, so
/// `chunk + context` stays within `route_cap`. A small cap keeps at least half
/// of its bytes for the diff.
pub(crate) fn chunk_cap_for_context(route_cap: usize) -> usize {
    let reserve = context_budget_for_chunk_cap(route_cap) + CONTEXT_WRAPPER_RESERVE_BYTES;
    route_cap.saturating_sub(reserve).max(route_cap / 2).max(1)
}

/// One file section of a chunk: post-image path and new-side hunk starts.
fn chunk_files(chunk: &str) -> Vec<(String, Vec<usize>)> {
    let mut out: Vec<(String, Vec<usize>)> = Vec::new();
    for line in chunk.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.trim();
            if path == "/dev/null" {
                continue;
            }
            let path = path.strip_prefix("b/").unwrap_or(path).to_string();
            out.push((path, Vec::new()));
        } else if line.starts_with("@@ ") {
            if let Some((_, starts)) = out.last_mut() {
                if let Some(start) = new_start(line) {
                    starts.push(start);
                }
            }
        }
    }
    out
}

fn new_start(header: &str) -> Option<usize> {
    let tok = header
        .split_whitespace()
        .find(|t| t.starts_with('+') && t.len() > 1)?;
    let n = tok[1..].split(',').next()?;
    n.parse().ok()
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_import(trimmed: &str) -> bool {
    [
        "use ",
        "pub use ",
        "import ",
        "from ",
        "#include",
        "extern crate ",
        "require ",
        "using ",
    ]
    .iter()
    .any(|p| trimmed.starts_with(p))
}

/// Top-level constant-like names: `const X`, `static X`, `#define X`, and
/// Python/shell `UPPER_CASE = ...` at column 0.
pub(crate) fn const_name(line: &str) -> Option<String> {
    let t = line.trim_start();
    let ident = |s: &str| -> Option<String> {
        let name: String = s
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        (!name.is_empty()).then_some(name)
    };
    for kw in ["#define ", "const ", "static ", "pub const ", "pub static "] {
        if let Some(rest) = t.strip_prefix(kw) {
            let rest = rest.trim_start();
            let rest = rest.strip_prefix("mut ").unwrap_or(rest);
            return ident(rest);
        }
    }
    if indent_of(line) == 0 {
        if let Some((lhs, _)) = t.split_once('=') {
            let lhs = lhs.trim().trim_end_matches(':');
            let lhs = lhs.split(':').next().unwrap_or(lhs).trim();
            if !lhs.is_empty()
                && lhs
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                && lhs.chars().any(|c| c.is_ascii_uppercase())
            {
                return Some(lhs.to_string());
            }
        }
    }
    None
}

/// Identifiers bound by one source line inside a function: Rust `let` and
/// `for` patterns, Python assignments / `for` / `with ... as`.
pub(crate) fn bindings_in(line: &str) -> Vec<String> {
    let t = line.trim_start();
    let idents = |s: &str| -> Vec<String> {
        s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|w| !w.is_empty())
            .filter(|w| {
                w.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
            })
            .filter(|w| !matches!(*w, "mut" | "ref" | "_" | "in" | "as" | "let" | "for"))
            .map(str::to_string)
            .collect()
    };
    if let Some(rest) = t.strip_prefix("let ") {
        // Pattern ends at `=` (or the type annotation `:` before it).
        let pat = rest.split('=').next().unwrap_or(rest);
        let pat = match pat.find(':') {
            // `let (a, b): (T, U)` -- keep only the pattern.
            Some(i) if !pat[..i].contains('(') || pat[..i].contains(')') => &pat[..i],
            _ => pat,
        };
        // Drop enum/struct path heads like `Some(x)` -> keep `x`.
        return idents(pat)
            .into_iter()
            .filter(|w| !matches!(w.as_str(), "some" | "ok" | "err"))
            .collect();
    }
    if let Some(rest) = t.strip_prefix("for ") {
        if let Some((pat, _)) = rest.split_once(" in ") {
            return idents(pat);
        }
    }
    if t.starts_with("with ") {
        if let Some((_, after)) = t.split_once(" as ") {
            return idents(after.trim_end_matches(':'));
        }
    }
    // Python-style `name = ...` / `a, b = ...` (not `==`, not attribute).
    if let Some(i) = t.find('=') {
        let (lhs, rhs) = t.split_at(i);
        if !rhs.starts_with("==")
            && !lhs.ends_with(['!', '<', '>', '=', '+', '-', '*', '/', '%', '|', '&', '^'])
            && !lhs.contains('(')
            && !lhs.contains('.')
            && !lhs.contains('[')
            && !lhs.trim().is_empty()
            && !lhs.trim_start().starts_with(['#', '/', '*'])
        {
            let lhs = lhs.split(':').next().unwrap_or(lhs);
            let words = idents(lhs);
            // `a, b = ...` binds every name; `name = ...` and C-style
            // `type name = ...` bind the last identifier.
            if lhs.contains(',') {
                return words;
            }
            if let Some(last) = words.last() {
                return vec![last.clone()];
            }
        }
    }
    Vec::new()
}

/// Parameter names from a definition line `fn f(a: T, b: U)` / `def f(a, b=1)`
/// / `int f(int a, char *b)`.
pub(crate) fn params_of(def_line: &str) -> Vec<String> {
    let Some(open) = def_line.find('(') else {
        return Vec::new();
    };
    let inner = &def_line[open + 1..];
    let inner = inner.split(')').next().unwrap_or(inner);
    inner
        .split(',')
        .filter_map(|p| {
            let p = p.trim();
            if p.is_empty() {
                return None;
            }
            let p = p.split('=').next().unwrap_or(p);
            // Rust/Python annotations: name before `:`; C: last identifier.
            let name_part = match p.find(':') {
                Some(i) => &p[..i],
                None => p,
            };
            let name: String = name_part
                .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .find(|w| !w.is_empty())?
                .to_string();
            (!matches!(name.as_str(), "self" | "mut" | "void" | "cls")).then_some(name)
        })
        .collect()
}

/// Enclosing definition for 1-based line `at`: the nearest line above with a
/// definition at a smaller indentation than the hunk's first line.
fn enclosing_def(lines: &[&str], at: usize) -> Option<usize> {
    let idx = at.saturating_sub(1).min(lines.len().saturating_sub(1));
    // Indentation of the hunk's code: the first non-blank line at or after
    // the hunk start that is not just a closing bracket.
    let target_indent = lines
        .iter()
        .skip(idx)
        .take(8)
        .find(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with(['}', ')', ']'])
        })
        .map(|l| indent_of(l))
        .unwrap_or(usize::MAX);
    let floor = idx.saturating_sub(MAX_SCOPE_LOOKBACK);
    (floor..idx).rev().find(|&i| {
        let l = lines[i];
        !l.trim().is_empty() && indent_of(l) < target_indent && is_def_line(l)
    })
}

/// A line that opens a definition scope: a keyword definition (`fn`, `def`,
/// `class`, `impl`, ...) after optional qualifiers, or a C-style function
/// signature at column 0. Deliberately stricter than the diff map's
/// extractor, which also matches calls such as `return Ok(())`.
pub(crate) fn is_def_line(line: &str) -> bool {
    let mut t = line.trim_start();
    if t.starts_with(['/', '*', '#', '"', '\'', '}']) {
        return false;
    }
    loop {
        let before = t;
        for q in [
            "pub(crate) ",
            "pub(super) ",
            "pub ",
            "async ",
            "unsafe ",
            "const ",
            "extern \"C\" ",
            "export ",
            "static ",
            "inline ",
            "default ",
        ] {
            if let Some(rest) = t.strip_prefix(q) {
                t = rest;
            }
        }
        if t == before {
            break;
        }
    }
    let keyword = [
        "fn ",
        "def ",
        "class ",
        "impl ",
        "impl<",
        "trait ",
        "mod ",
        "function ",
        "func ",
        "struct ",
        "enum ",
        "interface ",
        "sub ",
    ]
    .iter()
    .any(|k| t.starts_with(k));
    if keyword {
        return true;
    }
    // C/C++: `int name(args)` / `static void *name(` at column 0, not a
    // statement.
    indent_of(line) == 0
        && line.contains('(')
        && !line.trim_end().ends_with(';')
        && crate::review_diff::extract_defs(line)
            .first()
            .is_some_and(|name| !name.is_empty())
        && !line.trim_start().starts_with("return")
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}...")
    }
}

/// Source files the header is built for; prose and data files are skipped.
fn is_code_path(path: &str) -> bool {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    !matches!(
        ext.as_deref(),
        Some(
            "md" | "txt"
                | "rst"
                | "json"
                | "toml"
                | "yaml"
                | "yml"
                | "lock"
                | "csv"
                | "tsv"
                | "svg"
                | "html"
                | "xml"
                | "patch"
                | "diff"
        )
    )
}

/// Identifiers in `s`, in source order.
fn identifiers(s: &str) -> Vec<&str> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .collect()
}

/// Language keywords and common builtin/type names that are never useful as a
/// definition or a cross-chunk reference.
fn is_noise_symbol(name: &str) -> bool {
    matches!(
        name,
        "self"
            | "this"
            | "mut"
            | "ref"
            | "let"
            | "var"
            | "const"
            | "static"
            | "pub"
            | "use"
            | "from"
            | "import"
            | "as"
            | "fn"
            | "def"
            | "class"
            | "struct"
            | "enum"
            | "trait"
            | "impl"
            | "mod"
            | "type"
            | "interface"
            | "function"
            | "func"
            | "return"
            | "if"
            | "else"
            | "for"
            | "while"
            | "match"
            | "case"
            | "in"
            | "not"
            | "and"
            | "or"
            | "is"
            | "none"
            | "true"
            | "false"
            | "int"
            | "char"
            | "void"
            | "long"
            | "short"
            | "float"
            | "double"
            | "unsigned"
            | "signed"
            | "bool"
            | "string"
            | "str"
            | "usize"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "f32"
            | "f64"
            | "crate"
            | "super"
            | "std"
            | "core"
            | "alloc"
    )
}

/// Names imported by one line: Rust `use` (including braces and `as`),
/// Python/JS `import`/`from ... import`, and C `#include` stems.
///
/// The alias form and the dotted module form both count:
/// `import datetime as dt` binds `datetime` AND `dt`, `import urllib.request`
/// binds `urllib` AND `urllib.request`, and `from x import (y, z)` lists the
/// parenthesised names. A trailing `,` or a dangling `(` (a Python import that
/// continues on the next line) is tolerated: the `from x import (` line binds
/// `x`, and `from x import (y, z)` binds `y` and `z`.
pub(crate) fn imported_names(line: &str) -> Vec<String> {
    let t = line.trim().trim_end_matches([';', ',', '(', ')']);
    let mut out: Vec<String> = Vec::new();
    fn push_unique(out: &mut Vec<String>, word: &str) {
        let word = word.trim();
        if word.len() >= 2 && !is_noise_symbol(word) && !out.iter().any(|w| w == word) {
            out.push(word.to_string());
        }
    }
    // A Python module path binds each dotted prefix (`a.b` -> `a`, `a.b`) and
    // its alias (`import a.b as c` -> `c`).
    fn add_python_target(target: &str, out: &mut Vec<String>) {
        let target = target.trim();
        let (module, alias) = match target.split_once(" as ") {
            Some((module, alias)) => (module.trim(), Some(alias.trim())),
            None => (target, None),
        };
        let mut prefix = String::new();
        for segment in module.split('.') {
            let segment = segment.trim();
            if segment.is_empty() || segment == "*" {
                continue;
            }
            if !prefix.is_empty() {
                prefix.push('.');
            }
            prefix.push_str(segment);
            if !out.iter().any(|w| w == &prefix) {
                out.push(prefix.clone());
            }
        }
        if let Some(alias) = alias {
            push_unique(out, alias);
        }
    }
    if let Some(rest) = t
        .strip_prefix("pub use ")
        .or_else(|| t.strip_prefix("use "))
        .or_else(|| t.strip_prefix("extern crate "))
    {
        for word in identifiers(rest) {
            push_unique(&mut out, word);
        }
    } else if let Some(rest) = t.strip_prefix("from ") {
        let (module, names) = rest.split_once(" import ").unwrap_or((rest, ""));
        for word in identifiers(module) {
            push_unique(&mut out, word);
        }
        for name in names.split(',') {
            add_python_target(name.trim_matches(['(', ')', '[', ']']), &mut out);
        }
    } else if let Some(rest) = t.strip_prefix("import ") {
        for target in rest.split(',') {
            add_python_target(target, &mut out);
        }
        for word in identifiers(rest) {
            push_unique(&mut out, word);
        }
    } else if let Some(rest) = t.strip_prefix("#include") {
        let rest = rest.trim().trim_matches(['<', '>', '"']);
        let stem = rest.rsplit('/').next().unwrap_or(rest);
        let stem = stem.split('.').next().unwrap_or(stem);
        push_unique(&mut out, stem);
    } else if let Some(rest) = t.strip_prefix("using ") {
        for word in identifiers(rest) {
            push_unique(&mut out, word);
        }
    }
    out
}

/// Map every symbol defined or imported in `lines` to the 0-based line that
/// defines it. Used both for the chunk header and for the false-undefined
/// post-filter.
pub(crate) fn definition_index(lines: &[&str]) -> BTreeMap<String, usize> {
    let mut map = BTreeMap::new();
    let mut note = |name: String, line: usize| {
        if name.len() >= 2 && !is_noise_symbol(&name) {
            map.entry(name).or_insert(line);
        }
    };
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.is_empty()
            || t.starts_with(['/', '*'])
            || (t.starts_with('#') && !t.starts_with("#define") && !t.starts_with("#include"))
        {
            continue;
        }
        for name in extract_defs(line).into_iter().chain(const_name(line)) {
            note(name, i);
        }
        for name in bindings_in(line) {
            note(name, i);
        }
        if is_import(t) {
            for name in imported_names(line) {
                note(name, i);
            }
        }
    }
    map
}

/// Symbols a chunk mentions: identifiers from its added, removed and context
/// lines, excluding diff headers. Sorted and de-duplicated so the header does
/// not vary with hunk order.
fn referenced_symbols(chunk: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in chunk.lines() {
        if line.starts_with("diff --git ")
            || line.starts_with("index ")
            || line.starts_with("new file mode ")
            || line.starts_with("deleted file mode ")
            || line.starts_with("old mode ")
            || line.starts_with("new mode ")
            || line.starts_with("similarity index ")
            || line.starts_with("rename from ")
            || line.starts_with("rename to ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("@@")
            || line.starts_with("\\ No newline")
        {
            continue;
        }
        let code = match line.as_bytes().first() {
            Some(b'+' | b'-' | b' ') => &line[1..],
            _ => line,
        };
        let code = code.trim_start();
        if code.is_empty() || code.starts_with(['#', '/', '*', ';']) {
            continue;
        }
        for word in identifiers(code) {
            if word.len() < 2
                || is_noise_symbol(word)
                || word.starts_with(|c: char| c.is_ascii_digit())
            {
                continue;
            }
            if !out.iter().any(|w| w == word) {
                out.push(word.to_string());
                if out.len() >= MAX_REFERENCED_SYMBOLS {
                    return out;
                }
            }
        }
    }
    out.sort();
    out
}

/// The defining line for `symbol`, plus a few lines of context, as header
/// lines. `None` when the symbol is not defined or imported in `lines`.
fn definition_lines(
    path: &str,
    lines: &[&str],
    index: &BTreeMap<String, usize>,
    symbol: &str,
) -> Vec<String> {
    let Some(&idx) = index.get(symbol) else {
        return Vec::new();
    };
    let mut out = vec![format!(
        "- {path}:{}: {symbol} is defined or imported at the reviewed head: {}",
        idx + 1,
        clip(lines[idx], 120)
    )];
    let start = idx.saturating_sub(1);
    let end = (idx + 1 + DEF_CONTEXT_LINES).min(lines.len());
    for (j, line) in lines.iter().enumerate().take(end).skip(start) {
        if j == idx {
            continue;
        }
        let text = line.trim_end();
        if text.trim().is_empty() {
            continue;
        }
        out.push(format!("    {path}:{}: {}", j + 1, clip(text, 120)));
    }
    out
}

/// Whole-file context, when the file is small enough to send verbatim. Line
/// numbers are included so the reviewer can relate the file to the diff.
fn whole_file_lines(path: &str, text: &str, budget: usize) -> Option<Vec<String>> {
    if text.len() > WHOLE_FILE_THRESHOLD_BYTES || text.len() > budget {
        return None;
    }
    let mut out = vec![format!(
        "- {path}: whole file at the reviewed head ({} bytes; not part of the diff):",
        text.len()
    )];
    for (i, line) in text.lines().enumerate() {
        out.push(format!("    {:>4}: {}", i + 1, clip(line, 160)));
    }
    Some(out)
}

/// Per-hunk scope lines for one file: enclosing definition plus the names
/// bound earlier in it.
fn hunk_scopes(path: &str, lines: &[&str], hunk_starts: &[usize]) -> Vec<String> {
    let mut out = Vec::new();
    for &start in hunk_starts {
        let Some(def_idx) = enclosing_def(lines, start) else {
            continue;
        };
        let mut names: Vec<String> = Vec::new();
        for (i, l) in lines
            .iter()
            .enumerate()
            .take(start.saturating_sub(1))
            .skip(def_idx)
        {
            let found = if i == def_idx {
                params_of(l)
            } else {
                bindings_in(l)
            };
            for name in found {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
        if names.len() > MAX_BINDINGS_PER_HUNK {
            names.drain(..names.len() - MAX_BINDINGS_PER_HUNK);
        }
        let bound = if names.is_empty() {
            String::new()
        } else {
            format!("; names bound earlier in it: {}", names.join(", "))
        };
        out.push(format!(
            "- {path} hunk at line {start}: inside `{}` (line {}){bound}",
            clip(lines[def_idx], 90),
            def_idx + 1
        ));
    }
    out
}

/// File-wide lines for one file: imports, then column-0 definitions and
/// constants.
fn file_wide(path: &str, lines: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let imports: Vec<String> = lines
        .iter()
        .map(|l| l.trim())
        .filter(|t| is_import(t))
        .take(MAX_IMPORTS_PER_FILE)
        .map(|t| clip(t.trim_end_matches(';'), 80))
        .collect();
    if !imports.is_empty() {
        out.push(format!("- {path}: imports: {}", imports.join("; ")));
    }
    let mut defs: Vec<String> = Vec::new();
    for l in lines {
        if indent_of(l) > 0 {
            continue;
        }
        for name in extract_defs(l).into_iter().chain(const_name(l)) {
            if !defs.contains(&name) {
                defs.push(name);
            }
        }
        if defs.len() >= MAX_DEFS_PER_FILE {
            break;
        }
    }
    if !defs.is_empty() {
        out.push(format!(
            "- {path}: top-level definitions/constants: {}",
            defs.join(", ")
        ));
    }
    out
}

/// Build the context header for one chunk. `read` returns the post-change
/// text of a path (None when unavailable, e.g. a deleted file). Returns an
/// empty string when there is nothing useful to say.
pub(crate) fn chunk_context(
    chunk: &str,
    read: &dyn Fn(&str) -> Option<String>,
    budget: usize,
) -> String {
    // Merge repeated sections of the same file (split hunk parts).
    let mut files: Vec<(String, Vec<usize>)> = Vec::new();
    for (path, starts) in chunk_files(chunk) {
        if !is_code_path(&path) {
            continue;
        }
        match files.iter_mut().find(|(p, _)| *p == path) {
            Some((_, s)) => s.extend(starts),
            None => files.push((path, starts)),
        }
    }
    // Most specific first: every file's per-hunk scope lines, then the
    // defining lines of the symbols this chunk mentions, then the file-wide
    // lists. Truncation drops the least specific information.
    let refs = referenced_symbols(chunk);
    let per_file_budget = budget / files.len().max(1);
    let mut scope: Vec<String> = Vec::new();
    let mut defined: Vec<String> = Vec::new();
    let mut wide: Vec<String> = Vec::new();
    for (path, starts) in &files {
        if let Some(text) = read(path) {
            let lines: Vec<&str> = text.lines().collect();
            scope.extend(hunk_scopes(path, &lines, starts));
            // A small file is cheaper and clearer as a whole than as a list
            // of individual definitions.
            if let Some(whole) = whole_file_lines(path, &text, per_file_budget) {
                wide.extend(whole);
                // Keep the compact import/definition summary as well: it is
                // the part the existing reviewers were tuned against, and it
                // costs a few hundred bytes on a small file.
                wide.extend(file_wide(path, &lines));
                continue;
            }
            let index = definition_index(&lines);
            let before = defined.len();
            for symbol in &refs {
                defined.extend(definition_lines(path, &lines, &index, symbol));
            }
            if defined.len() == before {
                wide.extend(file_wide(path, &lines));
            }
        }
    }
    let lines: Vec<String> = scope.into_iter().chain(defined).chain(wide).collect();
    if lines.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for line in lines {
        if out.len() + line.len() + 1 > budget {
            out.push_str("- (context truncated)\n");
            break;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Whether the context header is enabled (`ZODER_REVIEW_CONTEXT=0` disables).
pub(crate) fn enabled() -> bool {
    !matches!(
        std::env::var("ZODER_REVIEW_CONTEXT")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("0" | "false" | "off" | "no")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The measured false positive: `label` is bound ~100 lines above the hunk
    /// in an unchanged part of the same function. The header must name it.
    #[test]
    fn binding_far_above_the_hunk_is_listed() {
        let mut src = String::from("use anyhow::Context;\nconst LIMIT: usize = 9;\n\nasync fn cmd_review(cli: &Cli, scope: Scope) -> anyhow::Result<()> {\n    let (label, diff) = build_diff(&cwd, scope)?;\n");
        for i in 0..100 {
            src.push_str(&format!("    work({i});\n"));
        }
        src.push_str(
            "    if diff.is_empty() {\n        bail!(\"nothing in {label}\");\n    }\n}\n",
        );
        let hunk_line = 4 + 1 + 100 + 2; // the bail! line
        let chunk = format!(
            "diff --git a/src/agentic.rs b/src/agentic.rs\n--- a/src/agentic.rs\n+++ b/src/agentic.rs\n@@ -{hunk_line},1 +{hunk_line},1 @@\n-        bail!(\"x\");\n+        bail!(\"nothing in {{label}}\");\n"
        );
        let read = |p: &str| (p == "src/agentic.rs").then(|| src.clone());
        let header = chunk_context(&chunk, &read, CONTEXT_BUDGET_BYTES);
        assert!(header.contains("inside `async fn cmd_review("), "{header}");
        assert!(
            header.contains("names bound earlier in it: cli, scope, label, diff"),
            "{header}"
        );
        assert!(header.contains("imports: use anyhow::Context"), "{header}");
        assert!(header.contains("cmd_review"), "{header}");
        assert!(header.contains("LIMIT"), "{header}");
    }

    /// New-file case (t2 9014): imports and constants at the top of a new
    /// file are listed for a chunk that only shows the bottom of it.
    #[test]
    fn new_file_imports_and_constants_reach_later_chunks() {
        let src = "import os\nimport re\nfrom pathlib import Path\n\nPATCH = Path('x')\nQUIRK_ID = 0x1234\n\ndef test_quirk():\n    text = PATCH.read_text()\n    assert re.search('anfe', text)\n";
        let chunk = "diff --git a/tests/t.py b/tests/t.py\nnew file mode 100644\n--- /dev/null\n+++ b/tests/t.py\n@@ -0,0 +8,3 @@\n+def test_quirk():\n+    text = PATCH.read_text()\n+    assert re.search('anfe', text)\n";
        let read = |_: &str| Some(src.to_string());
        let header = chunk_context(chunk, &read, CONTEXT_BUDGET_BYTES);
        assert!(header.contains("import re"), "{header}");
        assert!(header.contains("PATCH"), "{header}");
        assert!(header.contains("QUIRK_ID"), "{header}");
        assert!(header.contains("test_quirk"), "{header}");
    }

    #[test]
    fn header_is_bounded_and_skips_unreadable_files() {
        let mut src = String::new();
        for i in 0..500 {
            src.push_str(&format!("fn function_number_{i}() {{}}\n"));
        }
        let chunk = "+++ b/a.rs\n@@ -1 +1 @@\n+x\n+++ b/gone.rs\n@@ -1 +1 @@\n+y\n";
        let read = |p: &str| (p == "a.rs").then(|| src.clone());
        let header = chunk_context(chunk, &read, 400);
        assert!(header.len() <= 430, "{}", header.len());
        assert!(header.contains("(context truncated)"));
        assert!(!header.contains("gone.rs"));
        assert_eq!(chunk_context(chunk, &|_| None, 400), "");
    }

    /// Measured regression: in a large file the file-wide lists filled the
    /// budget and the hunk scope line (the one naming `label`) was truncated.
    #[test]
    fn hunk_scope_survives_a_tight_budget_and_docs_are_skipped() {
        let mut src = String::new();
        for i in 0..300 {
            src.push_str(&format!(
                "use crate::module_number_{i};\nfn function_number_{i}() {{}}\n"
            ));
        }
        src.push_str("fn cmd_review(cli: &Cli) {\n    let (label, diff) = build();\n    work();\n    bail!(\"{label}\");\n}\n");
        let n = src.lines().count();
        let chunk = format!(
            "+++ b/a.rs\n@@ -{0},1 +{0},1 @@\n+    bail!(\"{{label}}\");\n+++ b/README.md\n@@ -1 +1 @@\n+x\n",
            n - 1
        );
        let read = |p: &str| (p == "a.rs" || p == "README.md").then(|| src.clone());
        let header = chunk_context(&chunk, &read, 600);
        assert!(header.starts_with("- a.rs hunk at line"), "{header}");
        assert!(header.contains("label, diff"), "{header}");
        assert!(!header.contains("README.md"), "{header}");
    }

    #[test]
    fn binding_and_param_parsers() {
        assert_eq!(
            bindings_in("    let (label, diff) = build();"),
            vec!["label", "diff"]
        );
        assert_eq!(
            bindings_in("    let mut eng: Engine = load();"),
            vec!["eng"]
        );
        assert_eq!(
            bindings_in("    let Some(x) = y else { return };"),
            vec!["x"]
        );
        assert_eq!(
            bindings_in("    for (i, line) in lines.iter() {"),
            vec!["i", "line"]
        );
        assert_eq!(bindings_in("    text = PATCH.read_text()"), vec!["text"]);
        assert_eq!(bindings_in("    int count = 0;"), vec!["count"]);
        assert!(bindings_in("    if a == b {").is_empty());
        assert!(bindings_in("    self.x = 1").is_empty());
        assert_eq!(
            params_of("fn f(cli: &Cli, mut n: usize) {"),
            vec!["cli", "n"]
        );
        assert_eq!(params_of("def g(self, a, b=1):"), vec!["a", "b"]);
        assert_eq!(
            params_of("static int h(struct s *st, char *buf)"),
            vec!["st", "buf"]
        );
    }
    /// The reported regression: two constants defined near the top of a
    /// ~1000-line module were flagged as undefined in a late chunk. The chunk
    /// must carry their defining lines, not just a list of names.
    #[test]
    fn referenced_constants_defined_early_reach_a_late_chunk() {
        let mut src = String::from("import os\n\nRETRY_LIMIT = 3\nQUIRK_ID = 0x1234\n");
        for i in 0..900 {
            src.push_str(&format!("def filler_{i}():\n    return {i}\n"));
        }
        src.push_str("def run():\n    return RETRY_LIMIT + QUIRK_ID\n");
        assert!(src.len() > WHOLE_FILE_THRESHOLD_BYTES);
        let n = src.lines().count();
        let chunk = format!(
            "+++ b/pkg/mod.py\n@@ -{0},1 +{0},1 @@\n+    return RETRY_LIMIT + QUIRK_ID\n",
            n
        );
        let read = |p: &str| (p == "pkg/mod.py").then(|| src.clone());
        let header = chunk_context(&chunk, &read, 2000);
        assert!(
            header.contains("RETRY_LIMIT is defined or imported"),
            "{header}"
        );
        assert!(header.contains("RETRY_LIMIT = 3"), "{header}");
        assert!(
            header.contains("QUIRK_ID is defined or imported"),
            "{header}"
        );
        assert!(header.contains("QUIRK_ID = 0x1234"), "{header}");
    }

    /// A file below the threshold is supplied whole, so the reviewer can see
    /// the definition in place instead of a summary line.
    #[test]
    fn a_small_file_is_supplied_whole() {
        let src = "import os\n\nPATCH_PATH = 'x'\n\ndef load():\n    return open(PATCH_PATH)\n";
        let chunk = "+++ b/tool.py\n@@ -6,1 +6,1 @@\n+    return open(PATCH_PATH)\n";
        let read = |p: &str| (p == "tool.py").then(|| src.to_string());
        let header = chunk_context(chunk, &read, CONTEXT_BUDGET_BYTES);
        assert!(
            header.contains("whole file at the reviewed head"),
            "{header}"
        );
        assert!(header.contains("PATCH_PATH = 'x'"), "{header}");
    }

    /// Context is a capped share of the per-route chunk budget, and the chunk
    /// cap is reduced so `chunk + context` stays inside that budget.
    #[test]
    fn context_share_is_capped_by_the_route_chunk_budget() {
        assert_eq!(context_budget_for_chunk_cap(9_000), 2_250);
        assert_eq!(context_budget_for_chunk_cap(64_000), CONTEXT_BUDGET_BYTES);
        assert_eq!(context_budget_for_chunk_cap(1_000), CONTEXT_MIN_BYTES);
        for cap in [2_000usize, 9_000, 16_000, 32_000, 64_000] {
            let context = context_budget_for_chunk_cap(cap);
            let chunk = chunk_cap_for_context(cap);
            assert!(chunk + context + CONTEXT_WRAPPER_RESERVE_BYTES <= cap);
            assert!(chunk >= cap / 2);
        }
    }

    #[test]
    fn imported_names_cover_the_supported_import_styles() {
        assert!(imported_names("use crate::limits::MAX_RETRIES;").contains(&"MAX_RETRIES".into()));
        assert!(imported_names("from helpers import LIMIT, OTHER").contains(&"LIMIT".into()));
        assert!(imported_names("import { build, run as go } from './x'").contains(&"go".into()));
        assert!(imported_names("#include \"limits.h\"").contains(&"limits".into()));
    }
}
