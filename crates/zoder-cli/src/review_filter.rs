//! Deterministic post-filter for false "undefined symbol" review findings.
//!
//! A reviewer sees one diff chunk at a time. Even with the cross-chunk context
//! header, a small model can still report a symbol as undefined, missing or
//! unimported when it is defined earlier in the same file, imported at the top
//! of the file, or re-exported from a module the file imports. Those findings
//! are false, and a `critical` one blocks a loop.
//!
//! This module does not call a model. For a finding that claims a symbol is
//! undefined, it resolves the finding's file at the reviewed head and looks for
//! the symbol in that file's definitions/imports (and in the files that file
//! imports). When the symbol is found, the caller drops the finding and records
//! it, with the defining line as evidence, in the review JSON under
//! `filtered_findings`.

use serde_json::{json, Value};

use crate::review_context::{bindings_in, const_name, imported_names, is_def_line, params_of};

/// Phrases that make a finding a claim about a missing definition/import.
const CLAIM_PHRASES: &[&str] = &[
    "undefined",
    "not defined",
    "isn't defined",
    "not declared",
    "undeclared",
    "unbound",
    "not bound",
    "not imported",
    "isn't imported",
    "missing import",
    "missing definition",
    "no definition",
    "cannot find",
    "can't find",
    "does not exist",
    "not in scope",
    "unknown symbol",
    "undefined name",
];

const STOPWORDS: &[&str] = &[
    "the",
    "a",
    "an",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "this",
    "that",
    "these",
    "those",
    "it",
    "its",
    "in",
    "at",
    "on",
    "for",
    "from",
    "to",
    "of",
    "and",
    "or",
    "not",
    "no",
    "never",
    "does",
    "do",
    "did",
    "doesn",
    "don",
    "cannot",
    "can",
    "could",
    "would",
    "should",
    "undefined",
    "unbound",
    "missing",
    "unknown",
    "undeclared",
    "declared",
    "defined",
    "define",
    "imported",
    "import",
    "exports",
    "export",
    "symbol",
    "constant",
    "variable",
    "function",
    "method",
    "class",
    "module",
    "file",
    "line",
    "code",
    "name",
    "value",
    "type",
    "refers",
    "reference",
    "references",
    "referenced",
    "used",
    "use",
    "uses",
    "called",
    "call",
    "calls",
    "exists",
    "exist",
    "defect",
    "bug",
    "error",
    "critical",
    "high",
    "medium",
    "low",
    "info",
];

fn claims_undefined(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    CLAIM_PHRASES.iter().any(|phrase| lower.contains(phrase))
}

fn identifiers(text: &str) -> Vec<&str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .collect()
}

fn looks_like_symbol(word: &str) -> bool {
    if word.len() < 2 || STOPWORDS.contains(&word.to_ascii_lowercase().as_str()) {
        return false;
    }
    if word.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return false;
    }
    let has_upper = word.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = word.chars().any(|c| c.is_ascii_lowercase());
    let snake = word.contains('_');
    // `MAX_RETRIES`, `max_retries`, `MaxRetries`, `run`, `x1`.
    has_upper || snake || has_lower
}

/// Symbols a finding could be calling undefined: backticked/quoted names,
/// constant-shaped and snake_case identifiers, and the nearest identifier to
/// each claim phrase.
pub(crate) fn claimed_symbols(title: &str, body: &str) -> Vec<String> {
    let text = format!("{title}\n{body}");
    let mut out: Vec<String> = Vec::new();
    let mut add = |raw: &str| {
        let raw = raw
            .trim()
            .trim_matches(|c: char| matches!(c, '`' | '\'' | '"' | ',' | '.' | ':' | ';'));
        let raw = raw.split(['(', '[', '<']).next().unwrap_or(raw);
        let raw = raw.rsplit('.').next().unwrap_or(raw);
        if raw.len() >= 2
            && !STOPWORDS.contains(&raw.to_ascii_lowercase().as_str())
            && !out.iter().any(|w| w == raw)
        {
            out.push(raw.to_string());
        }
    };
    // Anything quoted or backticked is a deliberate symbol reference.
    for (quote, rest) in [
        ('`', text.as_str()),
        ('\'', text.as_str()),
        ('"', text.as_str()),
    ] {
        let mut parts = rest.split(quote);
        let _ = parts.next();
        for inner in parts.step_by(2) {
            if inner.len() <= 80 && looks_like_symbol(inner.trim()) {
                add(inner);
            }
        }
    }
    // Unquoted identifiers: constants, snake_case names, and camel case.
    for word in identifiers(&text) {
        if looks_like_symbol(word) {
            add(word);
        }
    }
    // Names next to a claim phrase ("undefined helper_build", "`x` is not
    // defined") even when they are short lower-case words.
    let words: Vec<&str> = text.split_whitespace().collect();
    for (i, word) in words.iter().enumerate() {
        let lower = word
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_ascii_lowercase();
        if !CLAIM_PHRASES
            .iter()
            .any(|p| lower == *p || lower.contains(p))
        {
            continue;
        }
        for j in [i.saturating_sub(1), i.saturating_sub(2), i + 1, i + 2] {
            if let Some(candidate) = words.get(j) {
                let candidate =
                    candidate.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
                if candidate.len() >= 2
                    && looks_like_symbol(candidate)
                    && !STOPWORDS.contains(&candidate.to_ascii_lowercase().as_str())
                {
                    add(candidate);
                }
            }
        }
    }
    out
}

fn line_evidence(
    path: &str,
    source: &str,
    symbol: &str,
    target_line: Option<usize>,
) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    // Imports and top-level definitions are file-wide bindings. Local
    // assignments are only evidence when the finding supplies a line and the
    // assignment is in that line's enclosing definition; a same-named local
    // in another function must not disprove an undefined-name claim.
    for (idx, line) in lines.iter().enumerate() {
        if is_import_line(line) && imported_names(line).iter().any(|name| name == symbol) {
            return Some(format!("{path}:{}: {}", idx + 1, line.trim()));
        }
        if line.trim_start() == *line && !line.trim().is_empty() {
            let mut defined = crate::review_diff::extract_defs(line)
                .into_iter()
                .chain(const_name(line));
            if defined.any(|name| name == symbol) {
                return Some(format!("{path}:{}: {}", idx + 1, line.trim()));
            }
        }
    }
    let target = target_line?;
    let target_idx = target.saturating_sub(1).min(lines.len().saturating_sub(1));
    let target_indent = lines[target_idx].len() - lines[target_idx].trim_start().len();
    let scope_idx = (0..target_idx).rev().find(|&idx| {
        is_def_line(lines[idx])
            && (lines[idx].len() - lines[idx].trim_start().len()) < target_indent
    })?;
    for (idx, line) in lines
        .iter()
        .enumerate()
        .take(target_idx + 1)
        .skip(scope_idx)
    {
        let names = if idx == scope_idx {
            params_of(line)
        } else {
            bindings_in(line)
        };
        if names.iter().any(|name| name == symbol) {
            return Some(format!("{path}:{}: {}", idx + 1, line.trim()));
        }
    }
    None
}

fn is_import_line(line: &str) -> bool {
    let t = line.trim_start();
    [
        "use ", "pub use ", "import ", "from ", "#include", "require ", "using ",
    ]
    .iter()
    .any(|p| t.starts_with(p))
}

fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map(|(head, _)| head).unwrap_or("")
}

fn join(base: &str, rest: &str) -> String {
    if base.is_empty() {
        rest.to_string()
    } else {
        format!("{base}/{rest}")
    }
}

fn with_source_extensions(base: String, out: &mut Vec<String>) {
    for ext in ["py", "rs", "ts", "tsx", "js", "jsx", "go", "h", "hpp"] {
        out.push(format!("{base}.{ext}"));
    }
    for index in ["__init__.py", "mod.rs", "index.ts", "index.js"] {
        out.push(format!("{base}/{index}"));
    }
}

/// Candidate sibling files named by one import line.
fn import_paths(path: &str, line: &str) -> Vec<String> {
    let dir = parent_dir(path);
    let t = line.trim().trim_end_matches(';');
    let mut out: Vec<String> = Vec::new();
    let mut push_module = |module: &str, relative_to_file: bool| {
        let module = module.trim().trim_end_matches("::*").trim_end_matches('*');
        let module = module.trim_matches(|c: char| c == '{' || c == '}' || c == ' ');
        if module.is_empty() || module == "*" {
            return;
        }
        let module = module.replace("::", "/").replace('.', "/");
        let base = if relative_to_file {
            join(dir, &module)
        } else {
            module.clone()
        };
        if !base.is_empty() {
            with_source_extensions(base, &mut out);
            if !relative_to_file {
                with_source_extensions(join(dir, &module), &mut out);
            }
        }
    };
    if let Some(rest) = t.strip_prefix("from ") {
        let module = rest.split(" import ").next().unwrap_or(rest);
        let relative = module.starts_with('.');
        push_module(module.trim_start_matches('.'), relative);
    } else if let Some(rest) = t.strip_prefix("import ") {
        let module = rest.split(" as ").next().unwrap_or(rest).trim();
        let (module, _) = module.split_once(" from ").unwrap_or((module, ""));
        let relative = module.starts_with('.');
        push_module(
            module.trim_start_matches('.').trim_matches(['{', '}']),
            relative,
        );
    } else if let Some(rest) = t
        .strip_prefix("pub use ")
        .or_else(|| t.strip_prefix("use "))
    {
        let rest = rest.trim_end_matches(';');
        let before_braces = rest.split("::{").next().unwrap_or(rest);
        let parts: Vec<&str> = before_braces.split("::").collect();
        // For `use crate::a::b::Sym`, the module path is everything before the
        // symbol. Without a symbol at the end, the whole path is the module.
        let module: String = if parts.last().is_some_and(|p| p.contains('}')) {
            parts
                .join("::")
                .split("::{")
                .next()
                .unwrap_or(rest)
                .to_string()
        } else if parts.len() > 1 {
            parts[..parts.len() - 1].join("::")
        } else {
            rest.to_string()
        };
        let module = module
            .trim_start_matches("crate::")
            .trim_start_matches("super::")
            .trim_start_matches("self::");
        let relative = path.starts_with("src/") || !module.starts_with("src");
        push_module(module, relative);
        if !module.starts_with("src") {
            push_module(&format!("src/{module}"), false);
        }
    } else if let Some(rest) = t.strip_prefix("#include") {
        let module = rest.trim().trim_matches(['<', '>', '"']);
        if !module.is_empty() {
            out.push(module.to_string());
            out.push(join(dir, module));
        }
    } else if let Some(rest) = t.strip_prefix("require ") {
        let module = rest.trim().trim_matches(['(', ')', '\'', '"']);
        if module.starts_with('.') {
            with_source_extensions(join(dir, module.trim_start_matches('.')), &mut out);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn import_mentions(line: &str, symbol: &str) -> bool {
    imported_names(line).iter().any(|name| name == symbol)
        || line.contains('*')
        || line.contains('{')
}

/// Look for `symbol` in the file itself, then in modules the file imports.
/// Returns a one-line evidence string naming the defining/importing line.
pub(crate) fn definition_evidence_at(
    path: &str,
    source: &str,
    symbol: &str,
    target_line: Option<usize>,
    read: &dyn Fn(&str) -> Option<String>,
) -> Option<(String, String)> {
    if let Some(evidence) = line_evidence(path, source, symbol, target_line) {
        return Some((
            "symbol is defined or imported in the file at the reviewed head".to_string(),
            evidence,
        ));
    }
    for line in source.lines() {
        if !is_import_line(line) || !import_mentions(line, symbol) {
            continue;
        }
        for candidate in import_paths(path, line) {
            if candidate == path {
                continue;
            }
            let Some(text) = read(&candidate) else {
                continue;
            };
            if let Some(evidence) = line_evidence(&candidate, &text, symbol, None) {
                return Some((
                    "symbol is defined in a module the file imports at the reviewed head"
                        .to_string(),
                    evidence,
                ));
            }
        }
    }
    None
}

/// Decide whether a finding is a false "undefined symbol" claim. `path` is the
/// repo-relative file from the finding's location. `None` keeps the finding.
#[cfg(test)]
fn false_undefined_record(
    title: &str,
    body: &str,
    path: &str,
    read: &dyn Fn(&str) -> Option<String>,
) -> Option<Value> {
    false_undefined_record_at(title, body, path, None, read)
}

pub(crate) fn false_undefined_record_at(
    title: &str,
    body: &str,
    path: &str,
    target_line: Option<usize>,
    read: &dyn Fn(&str) -> Option<String>,
) -> Option<Value> {
    if path.trim().is_empty() || !claims_undefined(&format!("{title}\n{body}")) {
        return None;
    }
    let path = path.trim().trim_start_matches("./");
    let source = read(path)?;
    for symbol in claimed_symbols(title, body) {
        if let Some((reason, evidence)) =
            definition_evidence_at(path, &source, &symbol, target_line, read)
        {
            return Some(json!({
                "severity": "",
                "title": title,
                "location": path,
                "symbol": symbol,
                "reason": reason,
                "evidence": evidence,
            }));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_from(files: Vec<(&'static str, &'static str)>) -> impl Fn(&str) -> Option<String> {
        move |path: &str| {
            files
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, text)| (*text).to_string())
        }
    }

    #[test]
    fn a_constant_defined_earlier_in_the_same_file_is_a_false_claim() {
        let source =
            "import os\n\nMAX_RETRIES = 3\nOTHER = 4\n\n\ndef run():\n    return MAX_RETRIES\n";
        let read = read_from(vec![("pkg/mod.py", source)]);
        let record = false_undefined_record(
            "MAX_RETRIES is not defined",
            "The new code uses `MAX_RETRIES` but the constant is undefined in this chunk.",
            "pkg/mod.py",
            &read,
        )
        .expect("the defining line must disprove the finding");
        assert_eq!(record["symbol"], "MAX_RETRIES");
        assert!(
            record["evidence"]
                .as_str()
                .unwrap()
                .contains("MAX_RETRIES = 3"),
            "{record}"
        );
        assert!(record["reason"]
            .as_str()
            .unwrap()
            .contains("defined or imported"));
    }

    #[test]
    fn a_symbol_that_really_is_undefined_is_kept() {
        let source = "import os\n\nMAX_RETRIES = 3\n\ndef run():\n    return os.getcwd()\n";
        let read = read_from(vec![("pkg/mod.py", source)]);
        assert!(false_undefined_record(
            "MISSING_LIMIT is undefined",
            "`MISSING_LIMIT` is never defined or imported.",
            "pkg/mod.py",
            &read,
        )
        .is_none());
    }

    #[test]
    fn a_symbol_defined_in_an_imported_sibling_module_is_a_false_claim() {
        let source = "from helpers import *\n\n\ndef run():\n    return MAX_RETRIES\n";
        let helpers = "MAX_RETRIES = 3\n";
        let read = read_from(vec![("pkg/mod.py", source), ("pkg/helpers.py", helpers)]);
        let record = false_undefined_record(
            "MAX_RETRIES is undefined",
            "The chunk does not define `MAX_RETRIES`.",
            "pkg/mod.py",
            &read,
        )
        .expect("the imported sibling module defines it");
        assert!(
            record["evidence"]
                .as_str()
                .unwrap()
                .starts_with("pkg/helpers.py:1:"),
            "{record}"
        );
        assert!(record["reason"]
            .as_str()
            .unwrap()
            .contains("module the file imports"));
    }

    #[test]
    fn a_rust_use_import_disproves_the_claim() {
        let source = "use crate::limits::MAX_RETRIES;\n\nfn run() -> usize { MAX_RETRIES }\n";
        let read = read_from(vec![("src/run.rs", source)]);
        let record = false_undefined_record(
            "undefined constant",
            "`MAX_RETRIES` is not imported into this chunk.",
            "src/run.rs",
            &read,
        )
        .expect("the use line imports it");
        assert_eq!(record["symbol"], "MAX_RETRIES");
        assert!(record["evidence"]
            .as_str()
            .unwrap()
            .contains("use crate::limits"));
    }

    #[test]
    fn a_non_undefined_finding_is_untouched() {
        let read = read_from(vec![("a.py", "MAX = 1\n")]);
        assert!(false_undefined_record(
            "Race in the retry loop",
            "Two threads can both decrement the counter.",
            "a.py",
            &read,
        )
        .is_none());
    }

    #[test]
    fn a_claim_without_a_resolvable_path_is_untouched() {
        let read = read_from(vec![("a.py", "MAX = 1\n")]);
        assert!(false_undefined_record("MAX is undefined", "", "", &read).is_none());
    }

    #[test]
    fn quoted_and_unquoted_symbols_are_extracted() {
        let got = claimed_symbols(
            "NameError",
            "name `MAX_RETRIES` is not defined and helper_build is undefined",
        );
        assert!(got.contains(&"MAX_RETRIES".to_string()), "{got:?}");
        assert!(got.contains(&"helper_build".to_string()), "{got:?}");
    }

    #[test]
    fn multi_chunk_python_context_stops_stub_undefined_findings() {
        let source = "import os\nimport json\n\ndef build():\n    local = {'ok': True}\n    return os.path.join(json.dumps(local), 'x')\n";
        let chunk = "diff --git a/pkg/mod.py b/pkg/mod.py\n--- a/pkg/mod.py\n+++ b/pkg/mod.py\n@@ -6,1 +6,1 @@\n-    return old(local)\n+    return os.path.join(json.dumps(local), 'x')\n";
        let read = read_from(vec![("pkg/mod.py", source)]);
        // The old stub reviewer saw the use sites but no file-wide bindings,
        // so it reported all three names as undefined.
        let old_findings = ["os", "json", "local"];
        assert_eq!(old_findings.len(), 3);
        let context = crate::review_context::chunk_context(chunk, &read, 3000);
        let new_findings: Vec<&str> = old_findings
            .into_iter()
            .filter(|name| !context.contains(name))
            .collect();
        assert!(
            new_findings.is_empty(),
            "new context-free stub findings: {new_findings:?}"
        );
        for name in ["os", "json", "local"] {
            assert!(
                context.contains(name),
                "{name} missing from context: {context}"
            );
            assert!(
                false_undefined_record_at(
                    &format!("{name} is undefined"),
                    &format!("The chunk cannot see `{name}`."),
                    "pkg/mod.py",
                    Some(6),
                    &read,
                )
                .is_some(),
                "the full-file verifier should record the disproved stub finding"
            );
        }
    }

    #[test]
    fn a_local_in_another_function_does_not_disprove_a_finding() {
        let source =
            "def first():\n    local = 1\n    return local\n\ndef second():\n    return local\n";
        let read = read_from(vec![("pkg/mod.py", source)]);
        assert!(false_undefined_record_at(
            "local is undefined",
            "the name is used here",
            "pkg/mod.py",
            Some(6),
            &read,
        )
        .is_none());
    }
}
