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

use crate::review_diff::extract_defs;

/// Default byte budget for one chunk's context header.
pub(crate) const CONTEXT_BUDGET_BYTES: usize = 3000;
const MAX_IMPORTS_PER_FILE: usize = 25;
const MAX_DEFS_PER_FILE: usize = 60;
const MAX_BINDINGS_PER_HUNK: usize = 30;
/// How far above a hunk to look for its enclosing definition.
const MAX_SCOPE_LOOKBACK: usize = 400;

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
fn const_name(line: &str) -> Option<String> {
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
fn bindings_in(line: &str) -> Vec<String> {
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
fn params_of(def_line: &str) -> Vec<String> {
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
    let target_indent = lines
        .get(idx)
        .map(|l| {
            if l.trim().is_empty() {
                usize::MAX
            } else {
                indent_of(l)
            }
        })
        .unwrap_or(usize::MAX);
    let floor = idx.saturating_sub(MAX_SCOPE_LOOKBACK);
    (floor..idx).rev().find(|&i| {
        let l = lines[i];
        !l.trim().is_empty() && indent_of(l) < target_indent && !extract_defs(l).is_empty()
    })
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

/// Context lines for one file of a chunk.
fn file_context(path: &str, text: &str, hunk_starts: &[usize]) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();

    let imports: Vec<String> = lines
        .iter()
        .map(|l| l.trim())
        .filter(|t| is_import(t))
        .take(MAX_IMPORTS_PER_FILE)
        .map(|t| clip(t, 80))
        .collect();
    let mut defs: Vec<String> = Vec::new();
    for l in &lines {
        if indent_of(l) > 4 {
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
    if !imports.is_empty() {
        out.push(format!("- {path}: imports: {}", imports.join("; ")));
    }
    if !defs.is_empty() {
        out.push(format!(
            "- {path}: top-level definitions/constants: {}",
            defs.join(", ")
        ));
    }
    for &start in hunk_starts {
        let Some(def_idx) = enclosing_def(&lines, start) else {
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

/// Build the context header for one chunk. `read` returns the post-change
/// text of a path (None when unavailable, e.g. a deleted file). Returns an
/// empty string when there is nothing useful to say.
pub(crate) fn chunk_context(
    chunk: &str,
    read: &dyn Fn(&str) -> Option<String>,
    budget: usize,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (path, starts) in chunk_files(chunk) {
        if let Some(text) = read(&path) {
            lines.extend(file_context(&path, &text, &starts));
        }
    }
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
        assert!(header.contains("imports: use anyhow::Context;"), "{header}");
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
}
