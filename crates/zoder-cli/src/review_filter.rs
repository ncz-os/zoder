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
    "does not import or define",
    "does not define",
    "nameerror",
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
    "missing imports",
    "missing definition",
    "no definition",
    "cannot find",
    "can't find",
    "not in scope",
    "unknown symbol",
    "undefined name",
];

const STOPWORDS: &[&str] = &[
    "if",
    "then",
    "argument",
    "option",
    "runtime",
    "raise",
    "raises",
    "cause",
    "will",
    "which",
    "chunk",
    "chunks",
    "diff",
    "hunk",
    "part",
    "here",
    "there",
    "scope",
    "context",
    "anywhere",
    "codebase",
    "statement",
    "path",
    "shown",
    "visible",
    "see",
    "new",
    "but",
    "in",
    "with",
    "by",
    "as",
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

/// Uses of "undefined" that describe runtime behaviour, not a missing name.
/// A use-after-free or an off-by-one finding routinely says "undefined
/// behaviour"; it is not a claim that a symbol is missing.
const NOT_A_CLAIM: &[&str] = &[
    "undefined behaviour",
    "undefined behavior",
    "undefined-behaviour",
    "undefined-behavior",
];

fn without_non_claims(lower: &str) -> String {
    let mut out = lower.to_string();
    for phrase in NOT_A_CLAIM {
        out = out.replace(phrase, " ");
    }
    out
}

fn claims_undefined(text: &str) -> bool {
    let lower = without_non_claims(&text.to_ascii_lowercase());
    CLAIM_PHRASES.iter().any(|phrase| lower.contains(phrase)) || claims_missing_import(&lower)
}

/// The "missing import" family, beyond the literal `missing import` phrase:
///
/// * `missing import of X [module]` / `potential|possible missing import of X`
/// * `<fn> missing return and X import` (an import noun as the object)
/// * `missing imports` (plural, no named symbol)
/// * `X is not imported` / `X module is not imported` / `import of X is missing`
///
/// These are matched on the raw lowercase text (NOT through `CLAIM_PHRASES`,
/// where a bare `import of` would turn any description of an import into a
/// claim). The filter still requires the named symbol to be found in the file
/// at the head before anything is dropped.
fn claims_missing_import(lower: &str) -> bool {
    const PREAMBLE: &[&str] = &["", "potential ", "possible ", "likely ", "an ", "a "];
    for head in PREAMBLE {
        for tail in ["", " missing", " is missing", " omitted"] {
            if lower.contains(&format!("{head}missing import{tail}")) {
                return true;
            }
        }
    }
    // `<something> ... import` as a noun object of "missing": "missing
    // return and urllib.request import", "a missing urllib.request import".
    let has_import_noun = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| word == "import" || word == "imports");
    if has_import_noun && lower.contains("missing") {
        return true;
    }
    lower.contains("not imported") || lower.contains("isn't imported")
}

/// Words that name the KIND of a symbol, not the symbol: "missing import of
/// json module" names `json`, not `module`.
const SYMBOL_KINDS: &[&str] = &[
    "module",
    "modules",
    "package",
    "packages",
    "library",
    "libraries",
    "lib",
    "import",
    "imports",
    "constant",
    "constants",
    "variable",
    "variables",
    "symbol",
    "symbols",
    "helper",
    "helpers",
];

fn strip_symbol_kind(name: &str) -> &str {
    let mut name = name.trim();
    // A bare kind word ("module", "package") is never a symbol.
    if SYMBOL_KINDS.contains(&name.to_ascii_lowercase().as_str()) {
        return "";
    }
    loop {
        let before = name;
        for kind in SYMBOL_KINDS {
            if let Some(rest) = name.strip_suffix(kind) {
                let rest = rest.trim_end_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
                if !rest.is_empty() {
                    name = rest.trim();
                }
            }
        }
        if name == before {
            return name;
        }
    }
}

/// True when `name` is a dotted module path (`a.b`, `urllib.request`) rather
/// than a `Class.member` reference: every segment is lowercase-ish and the
/// first is a known-import-shaped module name.
fn looks_like_module_path(name: &str) -> bool {
    name.contains('.')
        && name.split('.').all(|seg| {
            !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

/// Source-file extensions: a token ending in one is a path, and its stem
/// (`main` in `src/main.rs`) is not a symbol the finding is talking about.
const SOURCE_EXTENSIONS: &[&str] = &[
    ".rs", ".py", ".c", ".h", ".cc", ".cpp", ".hpp", ".go", ".js", ".ts", ".tsx", ".jsx", ".sh",
    ".java", ".kt", ".rb", ".toml", ".json", ".yaml", ".yml", ".md",
];

fn is_path_like(raw: &str) -> bool {
    let w = raw
        .trim_matches(|c: char| matches!(c, '`' | '\'' | '"' | ',' | ';' | '(' | ')' | '.'))
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    w.contains('/')
        || w.contains('\\')
        || SOURCE_EXTENSIONS
            .iter()
            .any(|ext| w.len() > ext.len() && w.ends_with(ext))
}

/// A token that reads as code rather than prose: snake_case, an upper-case
/// letter, a digit, `::`, a call or a backtick.
fn code_shaped(raw: &str) -> bool {
    let word = raw.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    raw.contains('_')
        || raw.contains("::")
        || raw.contains('(')
        || raw.contains('`')
        || word
            .chars()
            .skip(1)
            .any(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
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

/// Symbols a finding claims are undefined, unbound or not imported.
///
/// Only names tied to the claim count: a quoted or backticked name in a
/// sentence that makes the claim, or a name right next to the claim phrase
/// ("`x` is not defined", "undefined function verify_sig"). Every other
/// identifier in the finding (the function a defect sits in, a file path, a
/// name in another sentence) is NOT a claimed symbol: treating it as one let a
/// defined neighbour disprove a finding about a name that really is missing.
pub(crate) fn claimed_symbols(title: &str, body: &str) -> Vec<String> {
    let text = format!("{title}\n{body}");
    let mut out: Vec<String> = Vec::new();
    let mut add = |raw: &str| {
        let raw = raw.trim();
        // `X as Y` (an aliased import claim): either name satisfies it, so
        // both are candidates. The file must bind at least one of them.
        if let Some(alias) = alias_of(raw) {
            add_one(alias, &mut out);
            let module = raw.split_whitespace().next().unwrap_or("");
            add_one(module, &mut out);
            return;
        }
        add_one(raw, &mut out);
    };
    let sentences = text
        .replace(". ", ".\n")
        .replace("; ", ";\n")
        .replace("? ", "?\n")
        .replace("! ", "!\n");
    for sentence in sentences.lines() {
        let lower = without_non_claims(&sentence.to_ascii_lowercase());
        let is_claim =
            CLAIM_PHRASES.iter().any(|p| lower.contains(p)) || claims_missing_import(&lower);
        if !is_claim {
            continue;
        }
        // Quoted or backticked names in a claiming sentence.
        for quote in ['`', '\'', '"'] {
            let mut parts = sentence.split(quote);
            let _ = parts.next();
            for inner in parts.step_by(2) {
                if inner.len() <= 80 && looks_like_symbol(inner.trim()) {
                    add(inner);
                }
            }
        }
        // Names right next to the claim phrase.
        let words: Vec<&str> = sentence.split_whitespace().collect();
        let norm: Vec<String> = words
            .iter()
            .map(|w| {
                w.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '\''))
                    .to_ascii_lowercase()
            })
            .collect();
        for phrase in CLAIM_PHRASES {
            let pw: Vec<&str> = phrase.split(' ').collect();
            if pw.len() > norm.len() {
                continue;
            }
            for start in 0..=norm.len() - pw.len() {
                if !pw.iter().zip(&norm[start..]).all(|(p, w)| p == w) {
                    continue;
                }
                let end = start + pw.len();
                // A predicate's subject can be a plain lowercase name (rglob,
                // gzip). Do not collect arbitrary neighbours or the subject of
                // "does not define", which is the defining code, not the name.
                if !phrase.starts_with("does not") && *phrase != "nameerror" {
                    collect_subject(&words, &norm, start, &mut add);
                }
                collect_object(&words, end, &mut add);
            }
        }
        // "missing import of X [as Y] [module]" and "missing imports": the
        // name follows the phrase. A plural claim without a name is handled by
        // the no-symbol path in `false_undefined_record_at`.
        collect_after(&words, &norm, &["missing", "import"], &mut add);
        collect_after(&words, &norm, &["missing", "imports"], &mut add);
        // "import of X is missing" / "the import of X is missing".
        collect_after(&words, &norm, &["import", "of"], &mut add);
        // "<fn> missing return and X import": the import noun ends the
        // sentence and its module is the name right before it.
        collect_before_import_noun(&words, &norm, &mut add);
        // "<fn> missing return": the function name is the claim's subject.
        if let Some(at) = norm.iter().position(|w| w == "return") {
            if at > 0 && ["missing", "no"].contains(&norm[at - 1].as_str()) && at >= 2 {
                add(words[at - 2]);
            }
        }
    }
    out
}

/// `X as Y` -> `Some("Y")` for a single-token alias claim.
fn alias_of(raw: &str) -> Option<&str> {
    let mut parts = raw.split_whitespace();
    let module = parts.next()?;
    if parts.next()? != "as" {
        return None;
    }
    let alias = parts.next()?;
    if module.is_empty() || alias.is_empty() {
        return None;
    }
    Some(alias)
}

/// Normalize one candidate token and push it when it is a plausible symbol.
fn add_one(raw: &str, out: &mut Vec<String>) {
    let raw = raw
        .trim()
        .trim_matches(|c: char| matches!(c, '`' | '\'' | '"' | ',' | '.' | ':' | ';'));
    let raw = raw.split(['(', '[', '<']).next().unwrap_or(raw);
    let raw = raw.rsplit("::").next().unwrap_or(raw);
    let raw = strip_symbol_kind(raw);
    if raw.is_empty() || is_path_like(raw) || STOPWORDS.contains(&raw.to_ascii_lowercase().as_str())
    {
        return;
    }
    // The leading NAME of an attribute/call chain: `json.loads(...)` ->
    // `json`, `urllib.request.urlopen` -> `urllib.request` (a module path
    // that `import urllib.request` binds). `Class.member` is not a module.
    let raw = if looks_like_module_path(raw) && raw.starts_with(|c: char| c.is_ascii_lowercase()) {
        module_path_of(raw)
    } else if let Some((head, _)) = raw.split_once('.') {
        head
    } else {
        raw
    };
    if raw.len() >= 2
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        && looks_like_symbol(raw.split('.').next().unwrap_or(raw))
        && !STOPWORDS.contains(&raw.to_ascii_lowercase().as_str())
        && !out.iter().any(|w| w == raw)
    {
        out.push(raw.to_string());
    }
}

/// The longest all-lowercase dotted prefix of a module path:
/// `urllib.request.urlopen` -> `urllib.request`.
fn module_path_of(raw: &str) -> &str {
    if !raw.contains('.') {
        return raw;
    }
    let mut len = 0;
    let mut best = 0;
    for seg in raw.split('.') {
        if !seg.starts_with(|c: char| c.is_ascii_lowercase()) {
            break;
        }
        len += seg.len() + 1;
        best = len - 1;
    }
    if best == 0 {
        return raw.split('.').next().unwrap_or(raw);
    }
    &raw[..best]
}

/// Names before a claim phrase: the subject ("helper_name is not defined").
fn collect_subject(words: &[&str], norm: &[String], start: usize, add: &mut impl FnMut(&str)) {
    let mut subject = start;
    while subject > 0 && start - subject < 3 && STOPWORDS.contains(&norm[subject - 1].as_str()) {
        subject -= 1;
    }
    while subject > 0 {
        let word = words[subject - 1];
        if code_shaped(word) || word.chars().all(|c| c.is_ascii_lowercase()) {
            add(word);
        }
        subject -= 1;
        if subject > 0 && ["and", "or"].contains(&norm[subject - 1].as_str()) {
            subject -= 1;
        } else if subject == 0 || !words[subject - 1].ends_with(',') {
            break;
        }
    }
}

/// Names after a claim phrase, up to the first joining word ("missing import
/// of json module in build_report" -> `json`).
fn collect_object(words: &[&str], end: usize, add: &mut impl FnMut(&str)) {
    let mut named = false;
    for word in words.iter().skip(end) {
        let plain = word
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_ascii_lowercase();
        if [
            "in", "from", "at", "when", "by", "into", "on", "of", "to", "for", "within",
        ]
        .contains(&plain.as_str())
        {
            break;
        }
        if STOPWORDS.contains(&plain.as_str()) {
            if named && !["and", "or", "as"].contains(&plain.as_str()) {
                break;
            }
            continue;
        }
        let dotted = word
            .trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
            .contains('.');
        if code_shaped(word) || dotted || plain.chars().all(|c| c.is_ascii_lowercase()) {
            add(word);
            named = true;
        } else {
            break;
        }
    }
}

/// `<fn> missing return and urllib.request import`: the import noun is last
/// and the module it is about sits immediately before it.
fn collect_before_import_noun(words: &[&str], norm: &[String], add: &mut impl FnMut(&str)) {
    for (idx, word) in norm.iter().enumerate() {
        if !(word == "import" || word == "imports") || idx == 0 {
            continue;
        }
        let mut at = idx;
        if at > 0 && ["the", "a", "an", "and", "missing"].contains(&norm[at - 1].as_str()) {
            at -= 1;
            if at > 0 && norm[at] == "missing" {
                at -= 1;
            }
            if at > 0 && norm[at] == "and" {
                at -= 1;
            }
        }
        if at > 0 && at == idx {
            // `... X import`: the token right before the noun is the module.
            add(words[idx - 1]);
        }
    }
}

/// Collect the name(s) that follow one specific phrase shape. Used for the
/// "missing import"-family phrasings that `CLAIM_PHRASES` does not spell out
/// word for word ("missing import of X", "import of X is missing").
fn collect_after(words: &[&str], norm: &[String], phrase: &[&str], add: &mut impl FnMut(&str)) {
    if phrase.len() > norm.len() {
        return;
    }
    for start in 0..=norm.len() - phrase.len() {
        if phrase.iter().zip(&norm[start..]).all(|(p, w)| p == w) {
            collect_object(words, start + phrase.len(), add);
        }
    }
}

/// Option tokens only count in sentences that explicitly claim rejection or
/// absence. All options claimed in such a sentence must have definitions.
fn claimed_options(title: &str, body: &str) -> Vec<String> {
    let mut options = Vec::new();
    for sentence in format!("{title}\n{body}").replace(". ", "\n").lines() {
        let lower = sentence.to_ascii_lowercase();
        let missing_option = lower
            .split_whitespace()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|pair| {
                pair[0] == "missing"
                    && pair[1]
                        .trim_start_matches(['`', '\'', '"'])
                        .starts_with('-')
            });
        if !missing_option
            && ![
                "not defined",
                "does not define",
                "does not accept",
                "does not recognize",
                "does not recognise",
                "not accepted",
                "not recognized",
                "not recognised",
                "unrecognized",
                "unrecognised",
                "unknown option",
                "unknown argument",
            ]
            .iter()
            .any(|phrase| lower.contains(phrase))
        {
            continue;
        }
        for token in sentence.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        {
            let valid = token.strip_prefix("--").is_some_and(|s| {
                !s.is_empty() && s.starts_with(|c: char| c.is_ascii_alphanumeric())
            }) || (token.len() == 2
                && token.starts_with('-')
                && token.as_bytes()[1].is_ascii_alphanumeric());
            if valid && !options.iter().any(|o| o == token) {
                options.push(token.to_string());
            }
        }
    }
    options
}

/// A small lexical scanner keeps comments and string contents out of call
/// recognition. Only consecutive positional string literals immediately after
/// an option-definition call are evidence; keyword values such as help are not.
fn option_evidence(path: &str, source: &str, option: &str) -> Option<String> {
    #[derive(Debug)]
    enum Token<'a> {
        Word(&'a str),
        String(&'a str),
        Punct(u8),
    }
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut line = 1;
    while i < bytes.len() {
        let start = i;
        let start_line = line;
        match bytes[i] {
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'\n' => {
                line += 1;
                i += 1;
            }
            b' ' | b'\t' | b'\r' => {
                i += 1;
            }
            quote @ (b'\'' | b'"') => {
                let triple = bytes.get(i..i + 3).is_some_and(|s| s == [quote; 3]);
                let width = if triple { 3 } else { 1 };
                i += width;
                let content = i;
                let mut escaped = false;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        escaped = true;
                        i += 1;
                        if bytes.get(i) == Some(&b'\n') {
                            line += 1;
                        }
                        i = (i + 1).min(bytes.len());
                        continue;
                    }
                    if bytes[i] == b'\n' {
                        line += 1;
                    }
                    if bytes
                        .get(i..i + width)
                        .is_some_and(|s| s.iter().all(|b| *b == quote))
                    {
                        break;
                    }
                    i += 1;
                }
                // Triple strings and escapes are deliberately not evidence.
                let value = if !triple && !escaped && i < bytes.len() {
                    &source[content..i]
                } else {
                    ""
                };
                tokens.push((Token::String(value), start_line));
                i = (i + width).min(bytes.len());
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                tokens.push((Token::Word(&source[start..i]), start_line));
            }
            b => {
                tokens.push((Token::Punct(b), line));
                i += 1;
            }
        }
    }
    for (idx, (token, defining_line)) in tokens.iter().enumerate() {
        let Token::Word(name) = token else {
            continue;
        };
        let method = matches!(*name, "add_argument" | "add_option")
            && idx > 0
            && matches!(tokens[idx - 1].0, Token::Punct(b'.'));
        let click = *name == "option"
            && idx >= 2
            && matches!(tokens[idx - 1].0, Token::Punct(b'.'))
            && matches!(tokens[idx - 2].0, Token::Word("click"));
        if !(method || click) || !matches!(tokens.get(idx + 1), Some((Token::Punct(b'('), _))) {
            continue;
        }
        let mut arg = idx + 2;
        while let Some((Token::String(value), _)) = tokens.get(arg) {
            if !matches!(tokens.get(arg + 1), Some((Token::Punct(b',' | b')'), _))) {
                break;
            }
            if *value == option {
                return Some(format!(
                    "{path}:{defining_line}: {}",
                    source.lines().nth(defining_line - 1)?.trim()
                ));
            }
            if !matches!(tokens.get(arg + 1), Some((Token::Punct(b','), _))) {
                break;
            }
            arg += 2;
        }
    }
    None
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
    if lines.is_empty() {
        return None;
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

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn false_undefined_record_at(
    title: &str,
    body: &str,
    path: &str,
    target_line: Option<usize>,
    read: &dyn Fn(&str) -> Option<String>,
) -> Option<Value> {
    false_undefined_record_with_location(title, body, path, None, target_line, read)
}

/// Like [`false_undefined_record_at`], with the finding's raw location so a
/// `path:A-B` / `path (lines A-B)` / `path#LA-LB` range can be read. The
/// caller passes the whole location string; the path is still the argument
/// the file is resolved by.
pub(crate) fn false_undefined_record_with_location(
    title: &str,
    body: &str,
    path: &str,
    location: Option<&str>,
    target_line: Option<usize>,
    read: &dyn Fn(&str) -> Option<String>,
) -> Option<Value> {
    let options = claimed_options(title, body);
    if path.trim().is_empty()
        || (!claims_undefined(&format!("{title}\n{body}")) && options.is_empty())
    {
        return None;
    }
    let path = path.trim().trim_start_matches("./");
    let source = read(path)?;
    let lines: Vec<&str> = source.lines().collect();
    let range = cited_range(title, body, location, target_line, lines.len());
    let mut symbols = claimed_symbols(title, body);
    symbols.extend(options);
    let mut disproved: Vec<(String, String, String)> = Vec::new();
    // Every claimed name must be disproved. One name the file really lacks
    // keeps the finding: dropping it would hide a real defect.
    for symbol in &symbols {
        let (reason, evidence) = if symbol.starts_with('-') {
            (
                "option is defined in the file at the reviewed head".to_string(),
                option_evidence(path, &source, symbol)?,
            )
        } else {
            // An alias claim ("datetime as dt") is satisfied by EITHER name.
            let found = name_evidence(path, &source, symbol, target_line, read);
            found?
        };
        disproved.push((symbol.clone(), reason, evidence));
    }
    // Mixed claim (`_fetch_json missing return and urllib.request import`):
    // every part must be disproved before the finding is dropped. The import
    // part is above; the return part is checked against the named function.
    let mut extra: Vec<String> = Vec::new();
    for function in claimed_missing_return(title, body) {
        let evidence = function_return_evidence(path, &source, &function)?;
        extra.push(evidence);
    }
    // A no-symbol claim ("Undefined constants and missing imports") is only
    // disproved by checking every identifier the cited range uses.
    if symbols.is_empty() && extra.is_empty() {
        let mut bound = range_binding_evidence(path, &lines, range?)?;
        if bound.is_empty() {
            return None;
        }
        let (symbol, reason, evidence) = bound.remove(0);
        return Some(json!({
            "severity": "",
            "title": title,
            "location": path,
            "symbol": symbol,
            "symbols": bound
                .iter()
                .map(|(name, _, _)| json!(name))
                .chain(std::iter::once(json!(symbol)))
                .collect::<Vec<_>>(),
            "reason": reason,
            "evidence": evidence,
            "evidence_lines": bound
                .iter()
                .map(|(_, _, line)| json!(line))
                .chain(std::iter::once(json!(evidence)))
                .collect::<Vec<_>>(),
        }));
    }
    let (symbol, reason, evidence) = disproved.first().cloned()?;
    let mut record = json!({
        "severity": "",
        "title": title,
        "location": path,
        "symbol": symbol,
        "symbols": symbols,
        "reason": reason,
        "evidence": evidence,
    });
    if !extra.is_empty() {
        record["evidence_lines"] = json!(extra);
    }
    Some(record)
}

/// Evidence that `symbol` is bound: the definition/import line for a plain
/// name, or, for a dotted module path, any import that binds it.
fn name_evidence(
    path: &str,
    source: &str,
    symbol: &str,
    target_line: Option<usize>,
    read: &dyn Fn(&str) -> Option<String>,
) -> Option<(String, String)> {
    if let Some(found) = definition_evidence_at(path, source, symbol, target_line, read) {
        return Some(found);
    }
    // `urllib.request` is bound by `import urllib.request` (and by
    // `import urllib` for the `urllib` prefix); a `Class.member` chain
    // (`dt.datetime.now`) is bound by the import of its leading name. The
    // exact import line is the evidence.
    let mut prefix = symbol.to_string();
    loop {
        for (idx, line) in source.lines().enumerate() {
            if !is_import_line(line) {
                continue;
            }
            if imported_names(line).iter().any(|name| name == &prefix) {
                return Some((
                    "symbol is imported in the file at the reviewed head".to_string(),
                    format!("{path}:{}: {}", idx + 1, line.trim()),
                ));
            }
        }
        match prefix.rfind('.') {
            Some(cut) => prefix.truncate(cut),
            None => return None,
        }
    }
}

/// `(start, end)` 1-based, inclusive, of the lines a finding cites: from the
/// location field (`path:A`, `path:A-B`, `path:line A`, `path:lines A-B`,
/// `path (lines A-B)`, `path#LA-LB`) or from "lines A-B" in the body when the
/// location carries only a path.
pub(crate) fn cited_range(
    title: &str,
    body: &str,
    location: Option<&str>,
    target_line: Option<usize>,
    line_count: usize,
) -> Option<(usize, usize)> {
    let mut found: Option<(usize, usize)> = None;
    // The location field first (`path:A-B`, `path (lines A-B)`, `path#LA-LB`),
    // then the title and body ("lines A-B in ...").
    for text in location.into_iter().chain([title, body]) {
        if found.is_some() {
            break;
        }
        for (start, end) in range_specs(text) {
            if start >= 1 && start <= line_count {
                let end = end.min(line_count).max(start);
                found = Some((start, end));
                break;
            }
        }
    }
    if found.is_none() {
        // The location's single line (`path:122`) still names the range a
        // no-symbol claim ranges over: that line alone.
        found = target_line
            .filter(|line| *line >= 1 && *line <= line_count)
            .map(|line| (line, line));
    }
    found
}

/// Every `A-B` (or `A-B` with an en dash / comma / `L` prefix) digit range in
/// `text`, in order.
fn range_specs(text: &str) -> Vec<(usize, usize)> {
    let lower: String = text
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if matches!(c, '\u{2013}' | '\u{2014}') {
                '-'
            } else {
                c
            }
        })
        .collect();
    let chars: Vec<char> = lower.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        // An `L` prefix (`#L398-L452`) belongs to the number, not to the text.
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let first_at = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        let first: usize = chars[first_at..i]
            .iter()
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        let mut j = i;
        while j < chars.len() && matches!(chars[j], '-' | ',' | ' ') {
            j += 1;
        }
        if j < chars.len() && chars[j] == 'l' && j > i && matches!(chars[j - 1], '-' | ',' | ' ') {
            j += 1;
        }
        if j == i {
            continue;
        }
        let second_at = j;
        while j < chars.len() && chars[j].is_ascii_digit() {
            j += 1;
        }
        if second_at == j {
            continue;
        }
        let second: usize = chars[second_at..j]
            .iter()
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        if first >= 1 && second >= first {
            out.push((first, second));
        }
        i = j.max(first_at + 1);
    }
    out
}

/// Names a no-symbol claim could be about, taken from the cited line range:
/// every UPPER_CASE constant and the leading module of each dotted
/// attribute/call (`json.loads` -> `json`, `urllib.request.urlopen` ->
/// `urllib.request`). Locals bound in the range are skipped (they are the
/// enclosing function's own names). Returns every name with its binding line,
/// or `None` when any name is unbound (the finding must then be kept).
fn range_binding_evidence(
    path: &str,
    lines: &[&str],
    (start, end): (usize, usize),
) -> Option<Vec<(String, String, String)>> {
    let start = start.saturating_sub(1);
    let end = end.min(lines.len());
    if start >= end {
        return None;
    }
    let full = lines.join("\n");
    let range = &lines[start..end];
    let mut names: Vec<String> = Vec::new();
    for line in range {
        let code = line.split(" #").next().unwrap_or(line);
        for word in code.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.')) {
            let word = word.trim_matches('.');
            if word.is_empty() {
                continue;
            }
            let head = word.split('.').next().unwrap_or(word);
            let is_constant = head
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                && head.chars().any(|c| c.is_ascii_uppercase())
                && head.len() >= 2;
            let is_module = word.contains('.')
                && !is_constant
                && word
                    .split('.')
                    .next()
                    .is_some_and(|h| h.chars().all(|c| c.is_ascii_lowercase()));
            if !(is_constant || is_module) || PY_BUILTINS.contains(&head) {
                continue;
            }
            // The LEADING name of the chain is what a binding is looked up
            // by: `json.loads` -> `json`, `urllib.request.urlopen` ->
            // `urllib` (whose import of `urllib.request` also binds the
            // dotted path), `dt.datetime.now` -> `dt`.
            let _ = is_module;
            let name = head.to_string();
            if !names.iter().any(|n| n == &name) {
                names.push(name);
            }
        }
    }
    if names.is_empty() {
        return None;
    }
    let mut out: Vec<(String, String, String)> = Vec::new();
    for name in names {
        // A name the range itself binds (its enclosing function's parameter
        // or local) needs no module-level binding.
        if range.iter().any(|line| {
            params_of(line).iter().any(|p| p == &name)
                || bindings_in(line).iter().any(|b| b == &name)
        }) {
            continue;
        }
        // One unbound name is enough to keep the finding: return `None` (the
        // caller keeps it unchanged) rather than collecting partial evidence.
        // Written as an explicit match so the no-panic contract is visible:
        // `name_evidence` answers `None` for every name the file does not
        // bind, and this never unwraps or panics.
        #[allow(clippy::question_mark)] // explicit `None` documents the no-panic contract
        let evidence = match name_evidence(path, &full, &name, Some(start + 1), &|_| None) {
            Some(evidence) => evidence,
            None => return None,
        };
        out.push((name, evidence.0, evidence.1));
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

/// Python builtins a module may use without importing them.
const PY_BUILTINS: &[&str] = &[
    "self",
    "cls",
    "str",
    "int",
    "float",
    "bool",
    "list",
    "dict",
    "set",
    "tuple",
    "bytes",
    "len",
    "range",
    "print",
    "isinstance",
    "getattr",
    "setattr",
    "hasattr",
    "super",
    "enumerate",
    "zip",
    "sorted",
    "min",
    "max",
    "sum",
    "abs",
    "round",
    "open",
    "repr",
    "format",
    "type",
    "object",
    "Exception",
    "ValueError",
    "TypeError",
    "KeyError",
    "RuntimeError",
    "None",
    "True",
    "False",
    "os",
    "sys",
];

/// The function a "<fn> missing return" / "<fn> has no return" claim names.
fn claimed_missing_return(title: &str, body: &str) -> Vec<String> {
    let text = format!("{title}\n{body}");
    let mut out: Vec<String> = Vec::new();
    for sentence in text.split(['.', ';', '\n']) {
        let words: Vec<&str> = sentence.split_whitespace().collect();
        let norm: Vec<String> = words
            .iter()
            .map(|w| {
                w.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .to_ascii_lowercase()
            })
            .collect();
        for at in 0..norm.len() {
            let is_return_claim = norm[at] == "return"
                && at >= 1
                && ["missing", "no"].contains(&norm[at - 1].as_str())
                || (norm[at] == "return" && at >= 2 && norm[at - 1] == "and");
            if !is_return_claim || at < 2 {
                continue;
            }
            // The subject is the token before the claim ("_fetch_json missing
            // return"); the verb "has" ("_fetch_json has no return") is skipped.
            let mut subject = at - 1;
            while subject > 0 && STOPWORDS.contains(&norm[subject - 1].as_str()) {
                subject -= 1;
            }
            if subject == 0 {
                continue;
            }
            let name =
                words[subject - 1].trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_'));
            if name.len() >= 2
                && name
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !out.iter().any(|n| n == name)
            {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// Evidence that the body of function `name` contains a `return <expr>`.
fn function_return_evidence(path: &str, source: &str, name: &str) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    let start = lines.iter().position(|line| {
        is_def_line(line)
            && crate::review_diff::extract_defs(line)
                .iter()
                .any(|d| d == name)
    })?;
    let indent = lines[start].len() - lines[start].trim_start().len();
    for (idx, line) in lines.iter().enumerate().skip(start + 1) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let this_indent = line.len() - line.trim_start().len();
        if this_indent <= indent && !trimmed.starts_with(')') && is_def_line(line) {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("return") {
            let rest = rest.trim();
            if !rest.is_empty() && rest != ";" {
                return Some(format!("{path}:{}: {}", idx + 1, trimmed));
            }
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn gap_fixture(
        case: usize,
        defined: bool,
    ) -> (&'static str, &'static str, &'static str, String, usize) {
        let (title, body, location, count, line, definition) = match case {
            0 => ("Missing import for looks_like_tar", "The function payload_kind calls looks_like_tar(head) to detect plain tar archives, but the code does not import or define looks_like_tar in this file. If looks_like_tar is not imported from another module, this will raise a NameError at runtime when scanning payloads. The test suite references this function, so it must be available.", "tools/scan.py:line 70", 2406, 1682, "def looks_like_tar(data: bytes) -> bool:"),
            1 => ("Missing --receipt argument", "The test script passes a --receipt option to the scanner, but the main() function does not define this argument. As a result, argparse will raise an error and the command will fail before any scanning logic runs. The receipt file is actually used by receipt_variant() which reads <img>.receipt.json, so the flag is unnecessary for functionality but required for compatibility with the test harness.", "tools/scan.py:2175-2185", 2406, 2239, "    ap.add_argument(\"--receipt\", help=\"write the JSON receipt here\")"),
            _ => ("rglob used but not defined in scan_server_payload", "The scan_server_payload function calls rglob() multiple times (lines 1830, 1840, 1845) but rglob is not defined in the diff. The context shows rglob used elsewhere (line 2058) but its definition is not in the provided diff. If rglob is not imported or defined in the module, this will cause NameError.", "tools/provenance.py:1830", 2100, 82, "def rglob(root, pattern):"),
        };
        let mut lines = vec!["# filler"; count];
        if defined {
            lines[line - 1] = definition;
        }
        if case == 0 {
            lines[2072] = "def payload_kind(path: str) -> str:";
        }
        if case == 1 {
            lines[2226] = "def main(argv=None) -> int:";
            lines[2227] = "    ap = argparse.ArgumentParser()";
        }
        if case == 2 {
            lines[1828] = "def scan_server_payload():";
        }
        (title, body, location, lines.join("\n"), line)
    }

    fn assert_gap(case: usize) {
        for defined in [false, true] {
            let (title, body, location, source, line) = gap_fixture(case, defined);
            let path = location.split(':').next().unwrap();
            let read = |p: &str| (p == path).then(|| source.clone());
            let record = false_undefined_record_at(title, body, path, Some(70), &read);
            assert_eq!(record.is_some(), defined, "{title}: {record:?}");
            if let Some(record) = record {
                assert!(
                    record["evidence"]
                        .as_str()
                        .unwrap()
                        .starts_with(&format!("{path}:{line}:")),
                    "{record}"
                );
            }
        }
    }

    #[test]
    fn gap_long_module_definition() {
        assert_gap(0);
    }
    #[test]
    fn gap_receipt_option() {
        assert_gap(1);
    }
    #[test]
    fn gap_rglob_definition() {
        assert_gap(2);
    }

    #[test]
    fn gap_prose_is_not_a_symbol() {
        assert_eq!(
            claimed_symbols(
                "",
                "If helper_name is not imported or defined in the module."
            ),
            vec!["helper_name"]
        );
    }

    #[test]
    fn option_definitions_require_exact_positional_literals() {
        for definition in [
            "ap.add_argument(\"--receipt\", help=\"write receipt\")",
            "ap.add_argument(\"-r\", \"--receipt\", help=\"write receipt\")",
            "ap.add_option('--receipt')",
            "@click.option('--receipt')",
            "click.option('--receipt')",
            "ap.add_argument(\n    '-r',\n    '--receipt',\n    help='write receipt'\n)",
        ] {
            let source = format!("# heading\n{definition}\n");
            let read = |_: &str| Some(source.clone());
            let record =
                false_undefined_record("Missing --receipt argument", "", "tools/scan.py", &read)
                    .unwrap();
            assert!(
                record["evidence"]
                    .as_str()
                    .unwrap()
                    .starts_with("tools/scan.py:2:"),
                "{record}"
            );
        }
        for source in [
            "ap.add_argument('--receipt-dir')",
            "ap.add_argument('--other', help=\"--receipt\")",
            "# ap.add_argument('--receipt')",
            "log('--receipt')",
            "text = \"ap.add_argument('--receipt')\"",
            "text = '''\nap.add_argument('--receipt')\n'''",
            "ap.add_argument(help='--receipt')",
            "ap.add_argument('--receipt' + suffix)",
            "ap.add_argument('--receipt' '-dir')",
            "import helpers",
            "",
        ] {
            let read = |path: &str| {
                Some(
                    if path == "tools/scan.py" {
                        source
                    } else {
                        "ap.add_argument('--receipt')"
                    }
                    .to_string(),
                )
            };
            assert!(
                false_undefined_record("Missing --receipt argument", "", "tools/scan.py", &read)
                    .is_none(),
                "{source}"
            );
        }
    }

    #[test]
    fn option_claims_keep_missing_names_and_unrelated_findings() {
        let read = |_: &str| Some("ap.add_argument('-r', '--receipt')\n".to_string());
        for title in [
            "-r is not accepted",
            "--receipt is not recognised",
            "Unrecognized option --receipt",
        ] {
            assert!(
                false_undefined_record(title, "", "tools/scan.py", &read).is_some(),
                "{title}"
            );
        }
        for (title, body) in [
            (
                "Missing output file for --receipt",
                "The output file was never written.",
            ),
            ("Missing --receipt and --other options", ""),
            (
                "Missing --receipt argument",
                "`missing_name` is not defined.",
            ),
            (
                "Race writing --receipt",
                "The receipt file does not exist when checked.",
            ),
            (
                "Undefined behavior with --receipt",
                "A buffer is freed twice.",
            ),
        ] {
            assert!(
                false_undefined_record(title, body, "tools/scan.py", &read).is_none(),
                "{title}"
            );
        }
    }

    #[test]
    fn every_unquoted_claimed_name_must_be_disproved() {
        let read = |_: &str| Some("def defined_name(): pass\n".to_string());
        for title in [
            "missing_name and defined_name are not defined",
            "defined_name and missing_name are not defined",
            "Undefined functions defined_name and missing_name",
            "Undefined functions missing_name and defined_name",
        ] {
            assert!(
                false_undefined_record(title, "", "tools/scan.py", &read).is_none(),
                "{title}"
            );
        }
    }

    #[test]
    fn additional_name_claim_phrases_and_prose_guards() {
        for text in [
            "The code does not import or define helper_name.",
            "The code does not define helper_name.",
            "helper_name is not imported or defined.",
            "NameError: `helper_name`",
            "If helper_name is not defined in the module.",
            "However helper_name is not imported in the module.",
        ] {
            assert_eq!(claimed_symbols(text, ""), vec!["helper_name"], "{text}");
        }
        assert!(claimed_symbols("Undefined behavior in helper_name()", "").is_empty());
        assert!(claimed_symbols("The file does not exist", "").is_empty());
    }

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

    /// The seeded benchmark: a reviewer correctly reported `verify_sig` as
    /// undefined in `src/main.rs`, and the filter dropped it because `main`
    /// (from the file path and from "the main function") is defined.
    #[test]
    fn a_real_undefined_call_is_kept_even_when_its_neighbours_are_defined() {
        let source = "use crate::crypto;\n\nfn main() {\n    let (blob, sig) = load();\n    if verify_sig(blob, sig) {\n        run();\n    }\n}\n";
        let read = read_from(vec![("src/main.rs", source)]);
        for (title, body) in [
            (
                "Missing verify_sig function",
                "src/main.rs calls verify_sig which is not defined or imported. The intended function is crypto::verify_signature, but the call was changed incorrectly.",
            ),
            (
                "Missing import for verify_sig in main.rs",
                "The main function calls verify_sig directly, but the function is defined in another module. Without a use statement or fully-qualified path, this will not compile.",
            ),
            (
                "Undefined function verify_sig in main.rs",
                "src/main.rs calls verify_sig, which is not defined anywhere in the codebase, leading to a compile error.",
            ),
        ] {
            assert!(
                false_undefined_record_at(title, body, "src/main.rs", Some(5), &read).is_none(),
                "a real undefined call was filtered: {title}"
            );
        }
    }

    /// "Undefined behaviour" describes a memory-safety defect, not a missing
    /// name: a use-after-free or an off-by-one finding must never be filtered.
    #[test]
    fn undefined_behaviour_is_not_an_undefined_symbol_claim() {
        let source = "size_t parse_len_and_release(char *buf) {\n    xfree(buf);\n    return strlen(buf);\n}\n";
        let read = read_from(vec![("src/parser.c", source)]);
        for (title, body) in [
            (
                "Undefined behavior in parse_len_and_release()",
                "src/parser.c calls strlen on a pointer after freeing it, which is undefined behavior.",
            ),
            (
                "parse_len_and_release uses freed pointer",
                "`strlen(buf)` is called after `xfree(buf)`. The pointer is no longer valid, leading to undefined behaviour.",
            ),
        ] {
            assert!(false_undefined_record_at(title, body, "src/parser.c", Some(3), &read).is_none());
        }
    }

    /// A race finding that says a FILE "does not exist" is not a claim about
    /// a symbol.
    #[test]
    fn a_missing_file_is_not_a_missing_symbol() {
        let source = "import os\n\ndef write_unique(path, data):\n    if not os.path.exists(path):\n        open(path, 'w').write(data)\n";
        let read = read_from(vec![("lib/paths.py", source)]);
        assert!(false_undefined_record_at(
            "write_unique race condition",
            "write_unique checks os.path.exists before opening the file, which is not atomic. It drops O_EXCL, removing the guarantee that the file is created only if it does not exist.",
            "lib/paths.py",
            Some(4),
            &read,
        )
        .is_none());
    }

    /// Two claimed names, one defined and one not: the finding stays.
    #[test]
    fn one_real_missing_name_keeps_the_finding() {
        let read = read_from(vec![("pkg/mod.py", "MAX_RETRIES = 3\n")]);
        assert!(false_undefined_record(
            "Undefined names",
            "`MAX_RETRIES` and `MISSING_LIMIT` are not defined.",
            "pkg/mod.py",
            &read,
        )
        .is_none());
    }

    #[test]
    fn a_path_stem_is_never_a_claimed_symbol() {
        let got = claimed_symbols(
            "Undefined function verify_sig in main.rs",
            "src/main.rs calls verify_sig, which is not defined.",
        );
        assert!(got.contains(&"verify_sig".to_string()), "{got:?}");
        assert!(!got.iter().any(|w| w == "main" || w == "rs"), "{got:?}");
    }
    // ---- "missing import" claim shapes and range locations (F1-F4) ----

    /// A generic ~600-line Python report module: every import and constant a
    /// reviewer might call "missing" exists at the reviewed head.
    pub(crate) fn report_module(with_imports: bool) -> String {
        let mut lines: Vec<String> = vec![
            "#!/usr/bin/env python3".into(),
            "\"\"\"Report generator (fixture).\"\"\"".into(),
            String::new(),
        ];
        if with_imports {
            lines.push("import json".into()); // 4
            lines.push("import datetime as dt".into()); // 5
            lines.push("import urllib.request".into()); // 6
            lines.push("import os".into()); // 7
        } else {
            lines.push("import os".into());
        }
        while lines.len() < 44 {
            lines.push(String::new());
        }
        lines.push("REPORT_MODE = \"full\"".into()); // 45
        lines.push("MAX_ROWS = 100".into()); // 46
        lines.push("TABLE_NAME = \"reports\"".into()); // 47
        lines.push(String::new());
        lines.push("def _fetch_json(url, timeout=10):".into()); // 49
        lines.push("    response = urllib.request.urlopen(url, timeout=timeout)".into());
        lines.push("    return json.loads(response.read())".into());
        lines.push(String::new());
        while lines.len() < 396 {
            lines.push(format!("# filler line {}", lines.len() + 1));
        }
        lines.push("def build_report(path):".into()); // 397
        lines.push("    stamp = dt.datetime.now().isoformat()".into()); // 398
        lines.push("    rows = json.loads(open(path).read())".into()); // 399
        lines.push("    if len(rows) > MAX_ROWS:".into()); // 400
        lines.push("        rows = rows[:MAX_ROWS]".into()); // 401
        lines.push("    return {\"mode\": REPORT_MODE, \"stamp\": stamp, \"rows\": rows}".into()); // 402
        lines.push(String::new());
        while lines.len() < 566 {
            lines.push(format!("# filler line {}", lines.len() + 1));
        }
        lines.push("def run(argv):".into()); // 567
        lines.push("    payload = _fetch_json(argv[1])".into()); // 568
        lines.push("    return build_report(payload), TABLE_NAME".into()); // 569
        while lines.len() < 600 {
            lines.push(String::new());
        }
        lines.join("\n")
    }

    /// The four false findings a small reviewer raised on the module. Each
    /// names a claim shape or a location form the filter used to skip.
    pub(crate) fn import_shapes() -> Vec<(&'static str, &'static str, &'static str)> {
        vec![
            (
                "Missing import of json module",
                "build_report calls json.loads but the chunk does not show the import.",
                "tools/report.py:122-150",
            ),
            (
                "Potential missing import of datetime as dt",
                "dt is used at line 398 without an import in this chunk.",
                "tools/report.py:122-150",
            ),
            (
                "_fetch_json missing return and urllib.request import",
                "The helper binds response and never returns a value; urllib.request is never imported.",
                "tools/report.py:566-571",
            ),
            (
                "Undefined constants and missing imports",
                "lines 398-402 use REPORT_MODE, MAX_ROWS and the json module but nothing in the diff defines them.",
                "tools/report.py:398-452",
            ),
        ]
    }

    pub(crate) fn import_shape_read(with_imports: bool) -> impl Fn(&str) -> Option<String> {
        let source = report_module(with_imports);
        move |path: &str| (path == "tools/report.py").then(|| source.clone())
    }

    /// F1: "missing import of X module" over a `path:A-B` range.
    #[test]
    fn missing_import_of_a_module_is_disproved_by_its_import() {
        let read = import_shape_read(true);
        let record = false_undefined_record(
            "Missing import of json module",
            "build_report calls json.loads but the chunk does not show the import.",
            "tools/report.py",
            &read,
        )
        .expect("the module imports json at the head");
        assert_eq!(record["symbol"], "json");
        assert!(
            record["evidence"]
                .as_str()
                .unwrap()
                .starts_with("tools/report.py:4:"),
            "{record}"
        );
    }

    /// F2: "potential missing import of X as Y" — the alias or the module name
    /// satisfies it.
    #[test]
    fn potential_missing_import_of_an_alias_is_disproved() {
        let read = import_shape_read(true);
        let record = false_undefined_record(
            "Potential missing import of datetime as dt",
            "dt is used at line 398 without an import in this chunk.",
            "tools/report.py",
            &read,
        )
        .expect("the module imports datetime as dt at the head");
        assert!(
            record["evidence"]
                .as_str()
                .unwrap()
                .starts_with("tools/report.py:5:"),
            "{record}"
        );
    }

    /// F3: a mixed claim — missing return AND a dotted module import. Both are
    /// disproved (`_fetch_json` returns, `import urllib.request` exists).
    #[test]
    fn a_mixed_return_and_import_claim_is_disproved_only_when_both_parts_are() {
        let read = import_shape_read(true);
        let record = false_undefined_record_with_location(
            "_fetch_json missing return and urllib.request import",
            "The helper binds response and never returns a value; urllib.request is never imported.",
            "tools/report.py",
            Some("tools/report.py:566-571"),
            Some(566),
            &read,
        )
        .expect("the helper returns and the module is imported");
        let evidence = record["evidence"].as_str().unwrap();
        assert!(
            evidence.starts_with("tools/report.py:6:") || evidence.contains("urllib.request"),
            "{record}"
        );
        assert!(
            record["evidence_lines"]
                .as_array()
                .is_some_and(|lines| lines
                    .iter()
                    .any(|l| l.as_str().unwrap_or("").contains("return json.loads"))),
            "the return line is evidence too: {record}"
        );
    }

    /// F3 negative: the import removed -> the finding is kept.
    #[test]
    fn a_mixed_claim_with_an_unimported_module_is_kept() {
        let read = import_shape_read(false);
        assert!(false_undefined_record(
            "_fetch_json missing return and urllib.request import",
            "The helper binds response and never returns a value; urllib.request is never imported.",
            "tools/report.py",
            &read,
        )
        .is_none());
    }

    /// F3 negative: the function really has no return -> kept.
    #[test]
    fn a_missing_return_that_is_real_keeps_the_finding() {
        let source = "import urllib.request\n\n\ndef _fetch_json(url):\n    response = urllib.request.urlopen(url)\n    json.loads(response.read())\n";
        let read = read_from(vec![("tools/report.py", source)]);
        assert!(false_undefined_record(
            "_fetch_json missing return and urllib.request import",
            "The helper never returns a value.",
            "tools/report.py",
            &read,
        )
        .is_none());
    }

    /// F4: a no-symbol claim over a range — every constant and module the range
    /// uses is bound at module level.
    #[test]
    fn a_no_symbol_range_claim_is_disproved_when_every_used_name_is_bound() {
        let read = import_shape_read(true);
        let record = false_undefined_record_with_location(
            "Undefined constants and missing imports",
            "lines 398-402 use REPORT_MODE, MAX_ROWS and the json module but nothing in the diff defines them.",
            "tools/report.py",
            Some("tools/report.py:398-452"),
            Some(398),
            &read,
        )
        .expect("every name in the range is bound at the head");
        assert!(
            record["evidence_lines"]
                .as_array()
                .is_some_and(|l| !l.is_empty()),
            "{record}"
        );
        let symbols = record["symbols"].as_array().unwrap();
        assert!(symbols.contains(&json!("REPORT_MODE")), "{record}");
        assert!(symbols.contains(&json!("MAX_ROWS")), "{record}");
    }

    /// F4 negative: one constant the range USES is not defined -> kept.
    #[test]
    fn a_no_symbol_range_claim_with_one_unbound_constant_is_kept() {
        let mut source = report_module(true);
        // The range uses UNDEFINED_LIMIT, which the module never defines.
        source = source.replacen(
            "    rows = json.loads(open(path).read())",
            "    rows = json.loads(open(path).read())[:UNDEFINED_LIMIT]",
            1,
        );
        let read = move |path: &str| (path == "tools/report.py").then(|| source.clone());
        assert!(false_undefined_record_with_location(
            "Undefined constants and missing imports",
            "lines 398-402 use UNDEFINED_LIMIT that nothing defines.",
            "tools/report.py",
            Some("tools/report.py:398-452"),
            Some(398),
            &read,
        )
        .is_none());
    }

    /// F1-F4 negative: the same claim text with the imports removed keeps every
    /// finding.
    #[test]
    fn every_import_claim_shape_is_kept_when_the_import_is_absent() {
        let read = import_shape_read(false);
        for (title, body, location) in import_shapes() {
            let path = location.split(':').next().unwrap();
            assert!(
                false_undefined_record(title, body, path, &read).is_none(),
                "a claim whose import is really missing must be kept: {title}"
            );
        }
    }

    /// The location forms the reviewer uses for the same range all parse.
    #[test]
    fn cited_range_accepts_every_location_form() {
        for location in [
            "tools/report.py:398-452",
            "tools/report.py:line 398-452",
            "tools/report.py:lines 398-452",
            "tools/report.py (lines 398-452)",
            "tools/report.py#L398-L452",
        ] {
            assert_eq!(
                cited_range("Undefined constants", "", Some(location), Some(398), 600),
                Some((398, 452)),
                "{location}"
            );
        }
        // A path-only location falls back to "lines A-B" in the body.
        assert_eq!(
            cited_range(
                "Undefined constants",
                "lines 398-452 are undefined.",
                Some("tools/report.py"),
                None,
                600
            ),
            Some((398, 452))
        );
        // A single line stays a single-line range; no range at all is None.
        assert_eq!(
            cited_range(
                "Undefined name",
                "x is undefined",
                Some("tools/report.py:12"),
                Some(12),
                600
            ),
            Some((12, 12))
        );
        assert_eq!(
            cited_range(
                "Undefined name",
                "x is undefined",
                Some("tools/report.py"),
                None,
                600
            ),
            None
        );
    }

    /// Import forms: aliases, dotted modules, multi-line and relative imports.
    #[test]
    fn python_import_forms_bind_every_name_they_introduce() {
        for (line, names) in [
            ("import json", vec!["json"]),
            ("import os, sys", vec!["os", "sys"]),
            (
                "import json, datetime as dt",
                vec!["json", "datetime", "dt"],
            ),
            ("import urllib.request", vec!["urllib", "urllib.request"]),
            (
                "import urllib.request as urlreq",
                vec!["urllib", "urllib.request", "urlreq"],
            ),
            ("from utils import helper", vec!["utils", "helper"]),
            (
                "from utils import (helper, other)",
                vec!["utils", "helper", "other"],
            ),
            ("from utils import (", vec!["utils"]),
            ("from . import helper", vec!["helper"]),
            (
                "from pkg.mod import thing as other",
                vec!["pkg", "thing", "other"],
            ),
        ] {
            let got = crate::review_context::imported_names(line);
            for name in names {
                assert!(
                    got.iter().any(|g| g == name),
                    "{line}: {name} not in {got:?}"
                );
            }
        }
        // Existing forms are unchanged.
        assert!(
            crate::review_context::imported_names("use crate::limits::MAX_RETRIES;")
                .contains(&"MAX_RETRIES".to_string())
        );
        assert!(
            crate::review_context::imported_names("const LIMIT = require('limits')").is_empty()
        );
        // A JS `require` is not a Python import line: nothing is bound.
        assert!(crate::review_context::imported_names("const n = require('limits')").is_empty());
    }

    /// `X is not imported` / `import of X is missing` are claims too.
    #[test]
    fn other_import_claim_phrasings_are_recognized() {
        for text in [
            "json is not imported",
            "the json module is not imported",
            "the import of json is missing",
            "Possible missing import of json",
        ] {
            assert!(claims_undefined(text), "{text}");
            assert!(
                claimed_symbols(text, "").iter().any(|s| s == "json"),
                "{text}: {:?}",
                claimed_symbols(text, "")
            );
        }
    }

    /// The module/package/library kind word is not part of the symbol.
    #[test]
    fn a_trailing_symbol_kind_word_is_stripped() {
        assert_eq!(
            claimed_symbols("Missing import of json module", ""),
            vec!["json"]
        );
        assert_eq!(
            claimed_symbols("Missing import of the re package", ""),
            vec!["re"]
        );
    }
    /// An unbound identifier in the cited range must make the function return
    /// `None` (never panic), whatever combination of bound and unbound names
    /// the range uses.
    #[test]
    fn an_unbound_name_in_the_range_returns_none_without_panicking() {
        let lines = [
            "import json",
            "MAX_ROWS = 10",
            "def run(path):",
            "    rows = json.loads(path)",
            "    return rows[:UNDEFINED_LIMIT]",
        ];
        // Every name `range_binding_evidence` collects, checked one at a time:
        // the bound ones prove, the unbound one only ever answers `None`.
        assert!(range_binding_evidence("tools/report.py", &lines, (4, 5)).is_none());
        assert!(range_binding_evidence("tools/report.py", &lines, (3, 5)).is_none());
        // A range whose names are all bound still proves.
        assert!(range_binding_evidence("tools/report.py", &lines, (2, 2)).is_some());
        // An empty or out-of-file range is not a range at all.
        assert!(range_binding_evidence("tools/report.py", &lines, (9, 12)).is_none());
        assert!(range_binding_evidence("tools/report.py", &lines, (3, 3)).is_none());
    }

    /// The same through the public entry point: the filter keeps the finding
    /// and does not panic when the range mixes bound and unbound names.
    #[test]
    fn a_range_with_a_mixed_boundness_keeps_the_finding() {
        let source = format!(
            "{}\n",
            [
                "import json",
                "MAX_ROWS = 10",
                "def run(path):",
                "    rows = json.loads(path)",
                "    return rows[:UNDEFINED_LIMIT]",
            ]
            .join("\n")
        );
        let read = move |path: &str| (path == "tools/report.py").then(|| source.clone());
        assert!(false_undefined_record_with_location(
            "Undefined constants and missing imports",
            "lines 4-5 use UNDEFINED_LIMIT and the json module.",
            "tools/report.py",
            Some("tools/report.py:4-5"),
            Some(4),
            &read,
        )
        .is_none());
    }
}
