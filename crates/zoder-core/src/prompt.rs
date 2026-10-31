//! The master prompt: one versioned asset, one assembler, one family map.
//!
//! WHAT THIS IS
//!
//! Before this module, every prompt zoder sent to a model was an inline
//! string literal in a `.rs` file (see `docs/PROMPTS_INVENTORY.md`). The
//! author role prompt existed only inside `zoder loop`; `zoder exec`,
//! `zoder run` and `zoder rescue` sent the operator's text with no role
//! instruction at all. Nothing was a versioned asset, so no prompt could be
//! reviewed, diffed, snapshot-tested, or A/B'd.
//!
//! This module makes the author role prompt a versioned asset:
//!
//! ```text
//! prompts/master/v1.md          the master prompt (behaviour specification)
//! prompts/overlays/<family>.md  per-family corrections
//! ```
//!
//! and assembles them through ONE function, [`assemble`], so there is
//! exactly one place where "what text does this model receive" is decided.
//!
//! THE ASSEMBLY ORDER
//!
//! 1. the master prompt's body (everything between the
//!    `<!-- zoder:prompt:start -->` / `<!-- zoder:prompt:end -->` markers
//!    — see [`extract_overlay_text`]);
//! 2. the matching overlay's marked block, when the family has one;
//! 3. nothing else. The caller appends the task text and the operator's
//!    own instructions (AGENTS.md) separately, so the master prompt never
//!    has to know about them.
//!
//! WHY THE OVERLAY IS A FENCED BLOCK
//!
//! Each overlay file carries its evidence (MEASURED / UNVERIFIED per claim)
//! next to the claim it supports. The evidence is documentation; the marked
//! block is the only part that reaches the model. Keeping them in one file
//! means the evidence cannot drift away from the instruction it justifies.
//!
//! SIZE BUDGET
//!
//! A prompt that grows without bound is a prompt that eventually crowds the
//! model's context. [`MAX_ASSEMBLED_BYTES`] is the ceiling for one
//! assembled author prompt (master + overlay). The per-family unit tests
//! assert that each assembled prompt is under it, and that the assembled
//! text is byte-stable (a snapshot): the same inputs always produce the
//! same bytes, so a change to an asset is a visible diff rather than a
//! silent behaviour change.
//!
//! WHY THIS IS OFF BY DEFAULT
//!
//! The config flag ([`crate::config::PromptConfig`]) defaults to
//! [`crate::config::PromptConfig::DEFAULT_MASTER`] = `false`, and the
//! default path in the CLI never calls the assembler. An operator who has
//! not opted in sees byte-for-byte the prompts zoder sent before this
//! module existed. See `docs/PROMPT_EVAL.md` for how to measure a rollout
//! before flipping it.

use std::path::{Path, PathBuf};

/// The default master-prompt version this build ships.
pub const DEFAULT_MASTER_VERSION: &str = "v1";

/// Ceiling for one assembled author prompt (master + overlay), in bytes.
///
/// The budget is generous enough that the master prompt and the largest
/// overlay both fit with room to spare, and small enough that the author
/// prompt can never crowd out the repository content a coding turn
/// actually needs.
///
/// THE BUDGET IS MEASURED IN BYTES.
///
/// `str::len()` is the BYTE length; the character count is
/// `str::chars().count()`. They differ for any non-ASCII text, and these
/// assets do contain non-ASCII characters (em-dashes, arrows), so the
/// distinction is real rather than theoretical. The checks in this module
/// use `len()`, which is the byte length, and the tests
/// `the_size_budget_counts_bytes_not_characters` and
/// `the_asset_ceiling_counts_bytes_not_characters` pin that down: they
/// build a payload that is UNDER the budget in characters and OVER it in
/// bytes, and require it to be rejected. Do not "optimise" those checks
/// into `chars().count()` — that would silently double the effective
/// budget for every non-ASCII prompt.
pub const MAX_ASSEMBLED_BYTES: usize = 16 * 1024;

/// Ceiling for a single asset file (master or overlay), in bytes.
///
/// An asset larger than this is rejected rather than truncated: a silently
/// truncated prompt is a prompt with a missing rule, and the model cannot
/// tell that from a prompt that never had the rule.
pub const MAX_ASSET_BYTES: usize = 64 * 1024;

/// The model families this fleet runs, each with its own overlay.
///
/// The key is the family name; the value is the overlay file's stem (the
/// file is `prompts/overlays/<stem>.md`).
pub const OVERLAY_FILES: &[(&str, &str)] = &[
    ("deepseek", "deepseek"),
    ("gemma", "gemma-local"),
    ("gpt-oss", "gpt-oss"),
    ("minimax", "minimax"),
    ("mimo", "mimo"),
    ("muse", "muse-spark"),
    ("qwen", "qwen-local"),
];

/// Every family this module can produce an overlay for, in a stable order.
///
/// Used by the unit tests so a new family cannot be added without a test.
pub const FAMILIES: &[&str] = &[
    "deepseek", "gemma", "gpt-oss", "minimax", "mimo", "muse", "qwen",
];

/// Where a family key's overlay lives, relative to the prompts root.
fn overlay_rel_path(family: &str) -> Option<PathBuf> {
    let stem = OVERLAY_FILES
        .iter()
        .find(|(key, _)| *key == family)
        .map(|(_, stem)| *stem)?;
    Some(PathBuf::from("overlays").join(format!("{stem}.md")))
}

/// The family key for a model id, or `None` when the id matches no known
/// family.
///
/// Matching is case-insensitive and substring-based on the model id, with
/// one special case: `gpt-oss` is matched before the generic `gpt-`
/// prefix, because a `gpt-oss` model is not a chat model from the same
/// vendor.
///
/// The order matters: the first matching key in [`FAMILIES`] wins, so the
/// more specific keys are listed first.
pub fn family_for_model(model_id: &str) -> Option<&'static str> {
    let lower = model_id.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    FAMILIES
        .iter()
        .find(|family| family_matches(family, &lower))
        .copied()
}

/// Whether a lower-cased model id belongs to `family`.
fn family_matches(family: &str, lower_model: &str) -> bool {
    match family {
        // `gpt-oss` first: `gpt-oss-20b` is an open-weights reasoning model,
        // not a hosted chat model, and it has its own reasoning-channel
        // failure mode.
        "gpt-oss" => lower_model.contains("gpt-oss") || lower_model.contains("gpt_oss"),
        "deepseek" => lower_model.contains("deepseek"),
        "gemma" => lower_model.contains("gemma"),
        "minimax" => lower_model.contains("minimax"),
        "mimo" => lower_model.contains("mimo"),
        "muse" => lower_model.contains("muse-spark") || lower_model.contains("muse_spark"),
        "qwen" => lower_model.contains("qwen"),
        _ => false,
    }
}

/// A prompt asset that could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptError {
    /// The prompts root does not exist or is not a directory.
    MissingRoot(PathBuf),
    /// The master prompt file for the requested version does not exist.
    MissingMaster { version: String, path: PathBuf },
    /// The overlay file for the family does not exist.
    MissingOverlay { family: String, path: PathBuf },
    /// An asset file could not be read.
    Unreadable { path: PathBuf, reason: String },
    /// An asset file is larger than [`MAX_ASSET_BYTES`].
    TooLarge { path: PathBuf, bytes: usize },
    /// An asset file has no marker-delimited prompt block.
    NoOverlayBlock { path: PathBuf },
    /// The assembled prompt is larger than [`MAX_ASSEMBLED_BYTES`].
    AssembledTooLarge { bytes: usize },
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromptError::MissingRoot(p) => {
                write!(f, "prompt assets root not found: {}", p.display())
            }
            PromptError::MissingMaster { version, path } => write!(
                f,
                "master prompt version {version:?} not found at {}",
                path.display()
            ),
            PromptError::MissingOverlay { family, path } => write!(
                f,
                "no overlay asset for family {family:?} at {}",
                path.display()
            ),
            PromptError::Unreadable { path, reason } => {
                write!(f, "cannot read prompt asset {}: {reason}", path.display())
            }
            PromptError::TooLarge { path, bytes } => write!(
                f,
                "prompt asset {} is {bytes} bytes, over the {MAX_ASSET_BYTES}-byte limit",
                path.display()
            ),
            PromptError::NoOverlayBlock { path } => write!(
                f,
                "prompt asset {} has no marker-delimited prompt block",
                path.display()
            ),
            PromptError::AssembledTooLarge { bytes } => write!(
                f,
                "assembled prompt is {bytes} bytes, over the {MAX_ASSEMBLED_BYTES}-byte budget"
            ),
        }
    }
}

impl std::error::Error for PromptError {}

/// The text of a master prompt plus its family overlay.
///
/// `overlay` is `None` when the family has no overlay asset, and `Some`
/// (possibly empty) when it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledPrompt {
    /// The master prompt body.
    pub master: String,
    /// The overlay text appended to the master body, if any.
    pub overlay: Option<String>,
    /// The full text handed to the model.
    pub text: String,
}

impl AssembledPrompt {
    /// Byte length of the assembled text.
    pub fn bytes(&self) -> usize {
        self.text.len()
    }
}

/// The opening marker of the "text that reaches the model" block.
pub const PROMPT_BLOCK_START: &str = "<!-- zoder:prompt:start -->";

/// The closing marker of the "text that reaches the model" block.
pub const PROMPT_BLOCK_END: &str = "<!-- zoder:prompt:end -->";

/// Extract the text that reaches the model from an asset file.
///
/// Every asset in `prompts/` has the same shape: documentation first, then
/// a marker-delimited block that is the prompt itself. This function
/// returns that block, trimmed at the edges, or `None` when the markers are
/// absent or out of order.
///
/// The marker form (rather than a fenced code block) is deliberate: the
/// master prompt's body contains no code fences today, but it is prose with
/// inline backticks, and a fenced block would be ambiguous with a prompt
/// that legitimately wanted to show a fence. An HTML comment is invisible
/// in rendered markdown and unambiguous.
/// Extract the prompt block by MATCHING WHOLE LINES.
///
/// This is the robust form of the extraction: a marker counts only when it
/// is the entire (trimmed) line. An asset's documentation header may name
/// the marker constants — and a reader needs to know what delimits the
/// block — so the literal marker text can appear inside a sentence. A
/// whole-line match is immune to that, because a quoted marker inside a
/// sentence is never a line of its own.
///
/// The block is the text between the first whole-line start marker and the
/// first whole-line end marker after it. A file that has the start marker
/// but no end marker after it has no block.
pub fn extract_prompt_block(asset: &str) -> Option<String> {
    let mut collecting = false;
    let mut out: Vec<&str> = Vec::new();
    for line in asset.lines() {
        let trimmed = line.trim();
        if !collecting {
            if trimmed == PROMPT_BLOCK_START {
                collecting = true;
            }
            continue;
        }
        if trimmed == PROMPT_BLOCK_END {
            return Some(out.join("\n").trim().to_string());
        }
        out.push(line);
    }
    None
}

/// Extract the prompt block from an asset.
///
/// Alias for [`extract_prompt_block`], kept under the name the overlay
/// contract uses.
pub fn extract_overlay_text(asset: &str) -> Option<String> {
    extract_prompt_block(asset)
}

/// Read an asset file, enforcing the per-asset size ceiling.
fn read_asset(path: &Path) -> Result<String, PromptError> {
    let meta = std::fs::metadata(path).map_err(|e| PromptError::Unreadable {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })?;
    if meta.len() > MAX_ASSET_BYTES as u64 {
        return Err(PromptError::TooLarge {
            path: path.to_path_buf(),
            bytes: meta.len() as usize,
        });
    }
    std::fs::read_to_string(path).map_err(|e| PromptError::Unreadable {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

/// The prompts root for a given zoder home.
///
/// Resolution order:
///   1. `$ZODER_PROMPT_DIR` — an explicit override, used by the test suite
///      and by an operator who keeps the assets outside the install tree;
///   2. `<zoder home>/prompts` — the installed location;
///   3. `prompts` next to the running executable — the source-tree /
///      in-place layout, which is what a `cargo run` from a checkout uses.
///
/// The first of these that exists as a directory wins. When none exists,
/// the `<zoder home>/prompts` path is returned so the error names the place
/// the operator should put the assets.
pub fn prompts_root(zoder_home: &Path) -> PathBuf {
    if let Ok(dir) = std::env::var("ZODER_PROMPT_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return p;
        }
    }
    let home_prompts = zoder_home.join("prompts");
    if home_prompts.is_dir() {
        return home_prompts;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("prompts");
            if sibling.is_dir() {
                return sibling;
            }
        }
    }
    home_prompts
}

/// The master prompt asset path for a version.
pub fn master_path(root: &Path, version: &str) -> PathBuf {
    root.join("master").join(format!("{version}.md"))
}

/// The overlay asset path for a family.
pub fn overlay_path(root: &Path, family: &str) -> Option<PathBuf> {
    overlay_rel_path(family).map(|rel| root.join(rel))
}

/// Assemble the author prompt for a model id, from the assets under
/// `root`.
///
/// `model_id` selects the family overlay. A model id that matches no known
/// family gets the master prompt with no overlay — the master prompt is
/// complete on its own, and a wrong overlay is worse than no overlay.
///
/// The result is byte-stable: the same `root`, `version` and `model_id`
/// always produce the same `text`.
pub fn assemble(
    root: &Path,
    version: &str,
    model_id: &str,
) -> Result<AssembledPrompt, PromptError> {
    if !root.is_dir() {
        return Err(PromptError::MissingRoot(root.to_path_buf()));
    }
    let master_file = master_path(root, version);
    if !master_file.is_file() {
        return Err(PromptError::MissingMaster {
            version: version.to_string(),
            path: master_file,
        });
    }
    let master_raw = read_asset(&master_file)?;
    // The master's marked block is its prompt body. A master prompt with no
    // marked block is a broken asset, not an empty prompt: fail loudly
    // rather than send an empty system prompt.
    let master = extract_overlay_text(&master_raw).ok_or_else(|| PromptError::NoOverlayBlock {
        path: master_file.clone(),
    })?;

    let family = family_for_model(model_id);
    let overlay = match family {
        Some(fam) => {
            let path = overlay_path(root, fam).ok_or_else(|| PromptError::MissingOverlay {
                family: fam.to_string(),
                path: PathBuf::new(),
            })?;
            if !path.is_file() {
                return Err(PromptError::MissingOverlay {
                    family: fam.to_string(),
                    path,
                });
            }
            let raw = read_asset(&path)?;
            Some(
                extract_overlay_text(&raw)
                    .ok_or_else(|| PromptError::NoOverlayBlock { path: path.clone() })?,
            )
        }
        None => None,
    };

    let mut text = master.clone();
    if let Some(ov) = overlay.as_deref() {
        if !ov.is_empty() {
            text.push_str("\n\n");
            text.push_str(ov);
        }
    }
    // SIZE BUDGET, IN BYTES.
    //
    // `text.len()` is the byte length, which is what the budget is
    // measured in. (`chars().count()` would be the CHARACTER count, and
    // these assets contain non-ASCII text, so the two are not
    // interchangeable — see `MAX_ASSEMBLED_BYTES`.) The check is on the
    // assembled text, so the master body and the overlay are counted
    // together, including the separator added above.
    if text.len() > MAX_ASSEMBLED_BYTES {
        return Err(PromptError::AssembledTooLarge { bytes: text.len() });
    }
    Ok(AssembledPrompt {
        master,
        overlay,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locate the repository's `prompts/` directory from this crate's
    /// manifest directory (`crates/zoder-core`), so the tests read the
    /// checked-in assets rather than a copy.
    fn repo_prompts_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("prompts")
    }

    /// A representative served model id per family, as this fleet serves
    /// them.
    const FAMILY_MODELS: &[(&str, &str)] = &[
        ("deepseek", "deepseek-flash"),
        ("gemma", "aiand/google/gemma-4-31b-it"),
        ("gpt-oss", "aihubmix/gpt-oss-20b"),
        ("minimax", "MiniMax-M3"),
        ("mimo", "mimo-v2.6-flash"),
        ("muse", "aihubmix/muse-spark-1.2"),
        ("qwen", "qwen38"),
    ];

    /// A model id that matches no known family, so the master prompt is
    /// used with NO overlay.  The synthetic prompt roots below contain no
    /// overlay files, so this is what lets them exercise the master-prompt
    /// path on its own.
    const NO_OVERLAY_MODEL: &str = "grok-4.7";

    /// Write a synthetic prompt root holding a master prompt whose body is
    /// `body`, and return the root path.
    ///
    /// The root lives under a private temp directory rather than the
    /// platform temp dir root: `/tmp` is a tmpfs on the fleet's hosts and a
    /// job must stay inside its own workspace. `tempfile` creates under
    /// `$TMPDIR` when it is set (the fleet runner points it at the job's
    /// workspace) and falls back to `/tmp` only when it is not.
    ///
    /// This used `env!("CARGO_TARGET_DIR")` and did not compile: that is a
    /// RUSTC-time env var, defined only when the build itself sets it, so
    /// `cargo test -p zoder-core` failed with "environment variable
    /// `CARGO_TARGET_DIR` not defined at compile time" before a single test
    /// ran. The whole `prompt` test module — not just the four byte-budget
    /// tests — was therefore unbuildable.
    fn synthetic_root(name: &str, body: &str) -> PathBuf {
        let root = tempfile::Builder::new()
            .prefix(&format!("zoder-prompt-{name}-"))
            .tempdir()
            .expect("create synthetic prompt root")
            .keep();
        std::fs::create_dir_all(root.join("master")).expect("create master dir");
        let asset = format!(
            "documentation outside the block\n\n{PROMPT_BLOCK_START}\n{body}\n{PROMPT_BLOCK_END}\n"
        );
        std::fs::write(master_path(&root, "v1"), asset).expect("write master asset");
        root
    }

    /// The size budget is a limit on BYTES, not on characters.
    ///
    /// `String::len()` is the byte length (the character count is
    /// `chars().count()`), so the budget check in `assemble` is already
    /// byte-based. This test pins that down, because the distinction is
    /// invisible in the current assets and a future edit that swapped
    /// `len()` for `chars().count()` would let a prompt of non-ASCII
    /// text through at twice its intended size.
    ///
    /// The payload is built so that its CHARACTER count is under the
    /// budget while its BYTE count is over it — exactly the case a
    /// character-count check would wrongly accept.
    #[test]
    fn the_size_budget_counts_bytes_not_characters() {
        // "é" is 2 UTF-8 bytes, so this is over the byte budget but
        // under it in characters.
        let body: String = "é".repeat(MAX_ASSEMBLED_BYTES);
        assert!(
            body.chars().count() <= MAX_ASSEMBLED_BYTES,
            "the payload must be under the budget in CHARACTERS"
        );
        assert!(
            body.len() > MAX_ASSEMBLED_BYTES,
            "the payload must be over the budget in BYTES"
        );

        let root = synthetic_root("budget-bytes", &body);
        let err = assemble(&root, "v1", NO_OVERLAY_MODEL)
            .expect_err("an over-budget prompt must be rejected");
        match err {
            PromptError::AssembledTooLarge { bytes } => {
                assert!(
                    bytes > MAX_ASSEMBLED_BYTES,
                    "the error must report the BYTE count, got {bytes}"
                );
            }
            other => panic!("expected AssembledTooLarge, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A prompt that is under the budget in bytes is accepted, even when
    /// it contains multi-byte characters.
    #[test]
    fn a_multi_byte_prompt_under_the_budget_is_accepted() {
        let body: String = "é".repeat(MAX_ASSEMBLED_BYTES / 2);
        assert!(body.len() <= MAX_ASSEMBLED_BYTES);
        let root = synthetic_root("budget-ok", &body);
        let assembled = assemble(&root, "v1", NO_OVERLAY_MODEL)
            .expect("a prompt under the byte budget must be accepted");
        assert_eq!(assembled.bytes(), body.len());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The per-asset ceiling is enforced on the file's byte length.
    ///
    /// A multi-byte asset is over the limit by bytes well before it is
    /// over it by characters, so the check must use the byte length.
    #[test]
    fn the_asset_ceiling_counts_bytes_not_characters() {
        let body: String = "é".repeat(MAX_ASSET_BYTES);
        assert!(body.chars().count() <= MAX_ASSET_BYTES);
        assert!(body.len() > MAX_ASSET_BYTES);

        let root = synthetic_root("asset-budget-bytes", &body);
        let err = assemble(&root, "v1", NO_OVERLAY_MODEL)
            .expect_err("an over-budget asset must be rejected");
        match err {
            PromptError::TooLarge { bytes, .. } => {
                assert!(
                    bytes > MAX_ASSET_BYTES,
                    "the error must report the BYTE count, got {bytes}"
                );
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The reported size of an assembled prompt is its byte length.
    #[test]
    fn the_reported_size_is_a_byte_count() {
        let body = "aé".repeat(10);
        let root = synthetic_root("reported-size", &body);
        let assembled = assemble(&root, "v1", NO_OVERLAY_MODEL).expect("assemble");
        assert_eq!(
            assembled.bytes(),
            body.len(),
            "the reported size must be the byte length"
        );
        assert_ne!(
            assembled.bytes(),
            body.chars().count(),
            "the byte and character counts differ here, so the reported size must be bytes"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn family_for_model_matches_the_fleet_routes() {
        for (family, model) in FAMILY_MODELS {
            assert_eq!(
                family_for_model(model),
                Some(*family),
                "model {model:?} must resolve to family {family:?}"
            );
        }
    }

    #[test]
    fn family_for_model_is_case_insensitive() {
        assert_eq!(family_for_model("MINIMAX-M3"), Some("minimax"));
        assert_eq!(family_for_model("DeepSeek-Flash"), Some("deepseek"));
    }

    #[test]
    fn family_for_model_returns_none_for_an_unknown_model() {
        assert_eq!(family_for_model("grok-4.7"), None);
        assert_eq!(family_for_model(""), None);
    }

    #[test]
    fn gpt_oss_wins_over_the_generic_gpt_prefix() {
        // `gpt-oss` is an open-weights reasoning model with its own
        // failure mode; it must not be filed under a hosted chat model.
        assert_eq!(family_for_model("openai/gpt-oss-120b"), Some("gpt-oss"));
    }

    #[test]
    fn every_family_has_an_overlay_file_on_disk() {
        let root = repo_prompts_root();
        for family in FAMILIES {
            let path = overlay_path(&root, family)
                .unwrap_or_else(|| panic!("family {family:?} has no overlay mapping"));
            assert!(
                path.is_file(),
                "overlay asset missing for family {family:?}: {}",
                path.display()
            );
        }
    }

    #[test]
    fn every_overlay_carries_an_evidence_header() {
        // Each overlay must say which claims are MEASURED and which are
        // UNVERIFIED. A claim with no evidence label is a claim nobody
        // checked.
        let root = repo_prompts_root();
        for family in FAMILIES {
            let path = overlay_path(&root, family).unwrap();
            let text = std::fs::read_to_string(&path).expect("read overlay");
            assert!(
                text.contains("MEASURED") && text.contains("UNVERIFIED"),
                "overlay {family:?} must mark claims MEASURED or UNVERIFIED"
            );
            assert!(
                extract_overlay_text(&text).is_some(),
                "overlay {family:?} must carry a marker-delimited prompt block"
            );
        }
    }

    #[test]
    fn the_master_prompt_is_versioned_and_marked() {
        let root = repo_prompts_root();
        let path = master_path(&root, DEFAULT_MASTER_VERSION);
        assert!(path.is_file(), "master prompt missing: {}", path.display());
        let text = std::fs::read_to_string(&path).expect("read master");
        let body =
            extract_overlay_text(&text).expect("master prompt must have a marker-delimited block");
        assert!(
            body.contains("Stay with the task until it is finished"),
            "master prompt must state the persistence rule"
        );
        assert!(
            body.contains("apply_patch"),
            "master prompt must state the patch-editing convention"
        );
        assert!(
            body.contains("rg"),
            "master prompt must state the search-tool preference"
        );
    }

    /// PER-FAMILY SNAPSHOT + SIZE BUDGET.
    ///
    /// One test per family. Each asserts that the assembled prompt for that
    /// family's representative model is byte-stable and within the size
    /// budget. The snapshot is the assembled text's length plus a digest of
    /// its shape — enough to catch a silent asset edit (which changes both)
    /// without pinning the whole prompt in the test file (which would make
    /// every legitimate edit a two-file change).
    fn assert_family_snapshot(family: &str, model: &str) {
        let root = repo_prompts_root();
        let first = assemble(&root, DEFAULT_MASTER_VERSION, model)
            .unwrap_or_else(|e| panic!("assembling {family:?} failed: {e}"));
        let second = assemble(&root, DEFAULT_MASTER_VERSION, model)
            .expect("assembling must be deterministic");

        // 1. Byte-stable: the same inputs produce the same bytes.
        assert_eq!(
            first.text, second.text,
            "assembled prompt for {family:?} must be byte-stable"
        );

        // 2. Within the size budget.
        assert!(
            first.bytes() <= MAX_ASSEMBLED_BYTES,
            "assembled prompt for {family:?} is {} bytes, over the {MAX_ASSEMBLED_BYTES}-byte budget",
            first.bytes()
        );

        // 3. The overlay is actually attached for this family.
        let overlay = first
            .overlay
            .as_deref()
            .unwrap_or_else(|| panic!("family {family:?} must have an overlay"));
        assert!(
            !overlay.is_empty(),
            "overlay for {family:?} must not be empty"
        );
        assert!(
            first.text.ends_with(overlay),
            "the overlay must be the tail of the assembled prompt for {family:?}"
        );

        // 4. Snapshot: a cheap shape digest of the assembled text. Any
        // edit to the master prompt or to this family's overlay changes
        // the digest, so the snapshot fails and the change is visible in
        // the diff instead of being absorbed silently.
        let snapshot = snapshot_of(&first.text);
        assert_eq!(
            snapshot,
            snapshot_of(&second.text),
            "snapshot must be stable across two assemblies of {family:?}"
        );
        assert!(
            !snapshot.is_empty(),
            "snapshot for {family:?} must not be empty"
        );
        // The snapshot value itself is asserted below, per family, in
        // `family_snapshots_are_pinned`.
    }

    /// A stable, cheap digest of a prompt's shape.
    ///
    /// It mixes the byte length with a simple, dependency-free checksum so
    /// that both a length change and a content change are detected. It is
    /// NOT a security digest: it exists to make an accidental edit visible.
    fn snapshot_of(text: &str) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in text.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        format!("{}:{h:016x}", text.len())
    }

    #[test]
    fn snapshot_deepseek() {
        assert_family_snapshot("deepseek", "deepseek-flash");
    }

    #[test]
    fn snapshot_gemma() {
        assert_family_snapshot("gemma", "aiand/google/gemma-4-31b-it");
    }

    #[test]
    fn snapshot_gpt_oss() {
        assert_family_snapshot("gpt-oss", "aihubmix/gpt-oss-20b");
    }

    #[test]
    fn snapshot_minimax() {
        assert_family_snapshot("minimax", "MiniMax-M3");
    }

    #[test]
    fn snapshot_mimo() {
        assert_family_snapshot("mimo", "mimo-v2.6-flash");
    }

    #[test]
    fn snapshot_muse() {
        assert_family_snapshot("muse", "aihubmix/muse-spark-1.2");
    }

    #[test]
    fn snapshot_qwen() {
        assert_family_snapshot("qwen", "qwen38");
    }

    /// The pinned snapshot values, one per family.
    ///
    /// These are the values the per-family snapshot tests compute today.
    /// When a prompt asset is edited on purpose, this table is updated in
    /// the SAME commit as the asset edit — so the diff shows both the
    /// behavioural change and the acknowledgement that it was intentional.
    const PINNED_SNAPSHOTS: &[(&str, &str, &str)] = &[
        ("deepseek", "deepseek-flash", "7516:60e85721eddcb79d"),
        (
            "gemma",
            "aiand/google/gemma-4-31b-it",
            "7322:a8dac5546470ea1c",
        ),
        ("gpt-oss", "aihubmix/gpt-oss-20b", "7300:a451a6b9bb3e76c6"),
        ("minimax", "MiniMax-M3", "7460:db563a65bf55cd91"),
        ("mimo", "mimo-v2.6-flash", "7359:4b9ee9eaebb775fa"),
        ("muse", "aihubmix/muse-spark-1.2", "7318:80b0eb6416297607"),
        ("qwen", "qwen38", "7184:5c8b0477e0d8c1ee"),
    ];

    #[test]
    fn family_snapshots_are_pinned() {
        let root = repo_prompts_root();
        let mut unpinned = Vec::new();
        for (family, model, pinned) in PINNED_SNAPSHOTS {
            let assembled = assemble(&root, DEFAULT_MASTER_VERSION, model)
                .unwrap_or_else(|e| panic!("assembling {family:?} failed: {e}"));
            let actual = snapshot_of(&assembled.text);
            if pinned.is_empty() {
                unpinned.push(format!("    (\"{family}\", \"{model}\", \"{actual}\"),"));
                continue;
            }
            assert_eq!(
                &actual, pinned,
                "assembled prompt for {family:?} changed; update the pinned snapshot \
                 in the same commit as the prompt asset edit"
            );
        }
        assert!(
            unpinned.is_empty(),
            "the pinned snapshot table is out of date; add these rows to \
             PINNED_SNAPSHOTS:\n{}",
            unpinned.join("\n")
        );
    }

    #[test]
    fn an_unknown_family_gets_the_master_prompt_alone() {
        let root = repo_prompts_root();
        let assembled = assemble(&root, DEFAULT_MASTER_VERSION, "grok-4.7").expect("assemble");
        assert!(assembled.overlay.is_none());
        assert_eq!(assembled.text, assembled.master);
    }

    #[test]
    fn a_missing_version_is_an_error_not_a_fallback() {
        let root = repo_prompts_root();
        let err = assemble(&root, "v99", "deepseek-flash").expect_err("must not fall back");
        assert!(
            matches!(err, PromptError::MissingMaster { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn a_missing_root_is_an_error() {
        let err = assemble(Path::new("/nonexistent/prompts"), "v1", "deepseek-flash")
            .expect_err("must fail");
        assert!(matches!(err, PromptError::MissingRoot(_)), "got {err:?}");
    }

    #[test]
    fn the_overlay_never_contradicts_the_master() {
        // The overlays correct, they never reverse. Every overlay must
        // keep the master's persistence rule and its reporting rule
        // visible in intent: the overlay may sharpen them, but it must not
        // contain a phrase that tells the model to stop early or to end a
        // turn without a result.
        let root = repo_prompts_root();
        let forbidden = [
            "stop after the first",
            "you may stop early",
            "no need to run the test",
            "skip the tests",
        ];
        for family in FAMILIES {
            let path = overlay_path(&root, family).unwrap();
            let text = std::fs::read_to_string(&path).expect("read overlay");
            let body = extract_overlay_text(&text).expect("marker-delimited block");
            let lower = body.to_ascii_lowercase();
            for needle in forbidden {
                assert!(
                    !lower.contains(needle),
                    "overlay {family:?} contradicts the master prompt: {needle:?}"
                );
            }
        }
    }
}
