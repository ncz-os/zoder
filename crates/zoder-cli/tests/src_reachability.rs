//! Hygiene guard: every `.rs` file under a crate's `src/` tree must be
//! reachable from a `mod` declaration.
//!
//! Rust silently ignores a source file that no `mod` declaration names, so a
//! deleted-from-the-module-tree file rots in place: it stops compiling, drifts
//! out of sync with the APIs it used to call, and still reads like live code to
//! anyone grepping the tree. This repo has shipped two of those already —
//! `zoder-core/src/engine_rpc.rs` (715 lines) and `zoder-cli/src/codex.rs`
//! (1,335 lines) — both dead because the name they shadowed was a `pub use`
//! re-export and a module that had been folded elsewhere.
//!
//! This test walks each workspace crate and fails on any orphan. It checks the
//! *declaration* graph only (no compilation), so it stays fast and does not
//! need the crate to build.
//!
//! Two kinds of file are legitimate roots and are skipped:
//!
//! * crate roots — `src/lib.rs`, `src/main.rs`, `src/bin/*.rs`, `build.rs` —
//!   each is compiled on its own, with no declaring `mod`;
//! * anything named by a `mod <name>;` / `mod <name> {` in a sibling, parent,
//!   or child module file, resolved the way rustc resolves it (`foo.rs` or
//!   `foo/mod.rs` next to the declaration, plus the `#[path]` form).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Repo root: `crates/zoder-cli/tests/<this file>`.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("repo root resolves")
}

/// Every `.rs` file under `dir`, recursively.
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Strip line and block comments so a commented-out `mod foo;` never counts as
/// reachability. Deliberately simple: this is source hygiene, not a parser.
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    let mut depth = 0usize;
    while let Some(c) = chars.next() {
        if depth > 0 {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                depth -= 1;
            }
        } else if c == '/' && chars.peek() == Some(&'/') {
            for n in chars.by_ref() {
                if n == '\n' {
                    out.push('\n');
                    break;
                }
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            depth = 1;
        } else {
            out.push(c);
        }
    }
    out
}

fn strip_strings(src: &str) -> String {
    // Bare minimum: drop string/char literal contents so `"mod foo;"` inside a
    // doc example or an error message is not read as a declaration.
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            out.push('"');
            while let Some(d) = chars.next() {
                if d == '\\' {
                    chars.next();
                } else if d == '"' {
                    break;
                }
            }
            out.push('"');
        } else {
            out.push(c);
        }
    }
    out
}

/// Pull the module names a source file declares: `mod foo;`, `pub mod foo;`,
/// `pub(crate) mod foo;`, plus inline `mod foo { .. }` (which may still own a
/// `foo/` subdirectory via `foo/bar.rs`).
fn declared_mods(src: &str) -> Vec<String> {
    let cleaned = strip_strings(&strip_comments(src));
    let mut names = Vec::new();
    // Token stream: look for the `mod` keyword followed by an identifier.
    let mut prev: Option<&str> = None;
    for tok in cleaned
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
    {
        if prev == Some("mod") {
            names.push(tok.to_string());
        }
        prev = Some(tok);
    }
    names
}

/// Resolve a declared module name relative to the file that declares it.
fn resolve_decl(declaring_file: &Path, name: &str) -> Vec<PathBuf> {
    let dir = declaring_file
        .parent()
        .expect("declaring file has a parent");
    // `src/foo.rs` may own `src/foo/bar.rs`; a crate root owns `src/bar.rs`.
    let stem = declaring_file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let base = if stem == "lib" || stem == "main" || stem == "mod" {
        dir.to_path_buf()
    } else {
        dir.join(stem)
    };
    vec![
        dir.join(format!("{name}.rs")),
        base.join(format!("{name}.rs")),
        base.join(name).join("mod.rs"),
    ]
}

fn is_root(file: &Path, src_dir: &Path) -> bool {
    let rel = file.strip_prefix(src_dir).unwrap_or(file);
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if stem == "lib" || stem == "main" || stem == "build" {
        return true;
    }
    // `src/bin/**.rs` are auto-discovered binary targets: each is crate root.
    rel.components().next().and_then(|c| c.as_os_str().to_str()) == Some("bin")
}

/// Orphan `.rs` files (not roots, not named by any reached `mod` decl).
fn orphans_in_crate(crate_dir: &Path) -> Vec<PathBuf> {
    let src = crate_dir.join("src");
    if !src.is_dir() {
        return Vec::new();
    }
    let mut all = Vec::new();
    collect_rs(&src, &mut all);

    let canon = |p: &Path| fs::canonicalize(p).ok();
    let mut reached: BTreeSet<PathBuf> = BTreeSet::new();
    let mut queue: Vec<PathBuf> = Vec::new();
    for f in &all {
        if is_root(f, &src) {
            queue.push(f.clone());
        }
    }
    // A crate root may `mod` a file anywhere under src/.
    for f in &all {
        if let (Some(a), Some(b)) = (canon(f), canon(&src.join("lib.rs"))) {
            if a == b {
                queue.push(f.clone());
            }
        }
        if let (Some(a), Some(b)) = (canon(f), canon(&src.join("main.rs"))) {
            if a == b {
                queue.push(f.clone());
            }
        }
    }

    while let Some(file) = queue.pop() {
        let key = canon(&file).unwrap_or(file.clone());
        if !reached.insert(key) {
            continue;
        }
        let Ok(src_text) = fs::read_to_string(&file) else {
            continue;
        };
        for name in declared_mods(&src_text) {
            for candidate in resolve_decl(&file, &name) {
                if let Ok(c) = fs::canonicalize(&candidate) {
                    if all.iter().any(|f| canon(f).as_ref() == Some(&c)) {
                        queue.push(candidate);
                    }
                }
            }
        }
    }

    all.into_iter()
        .filter(|f| canon(f).map(|c| !reached.contains(&c)).unwrap_or(true))
        .collect()
}

#[test]
fn every_src_rs_file_is_reachable_from_a_mod_declaration() {
    let root = repo_root();
    let crates = root.join("crates");
    let mut entries: Vec<_> = fs::read_dir(&crates)
        .expect("crates/ exists")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.join("Cargo.toml").is_file())
        .collect();
    entries.sort();
    assert!(
        !entries.is_empty(),
        "no crates found under {}",
        crates.display()
    );

    let mut orphans = Vec::new();
    for crate_dir in &entries {
        for orphan in orphans_in_crate(crate_dir) {
            let rel = orphan.strip_prefix(&root).unwrap_or(&orphan);
            orphans.push(rel.display().to_string());
        }
    }

    assert!(
        orphans.is_empty(),
        "these .rs files are not reachable from any `mod` declaration, so they are \
         never compiled and silently rot — delete them or declare them in their \
         parent module:\n  {}",
        orphans.join("\n  ")
    );
}
