//! The CLI seam for the master system prompt.
//!
//! ONE function decides what text an author turn carries:
//! [`compose_author_prompt`]. It is called from the agentic chain walk,
//! once per candidate, because the model family's overlay depends on which
//! model is actually serving the turn (a fallback down the chain is a
//! different family, and gets its own overlay).
//!
//! THE DEFAULT IS THE CURRENT BEHAVIOUR.
//!
//! With `[prompt] master` unset (or `false`), [`compose_author_prompt`]
//! returns the task text BYTE-FOR-BYTE. Every prompt zoder sent before the
//! master-prompt work is therefore still exactly what it was — the
//! `exec` / `run` / `rescue` paths send the operator's text with no role
//! instruction, and the loop's author turn keeps its existing literal.
//!
//! WHY THE PROMPT IS PREPENDED RATHER THAN SENT IN A SYSTEM SLOT
//!
//! The ACP wire shape zoder speaks (`session/prompt`) carries one text
//! block. The engine's role configuration is the engine's business, not
//! zoder's; what zoder controls is the text the model receives. The
//! existing project-instructions block (`AGENTS.md`) is composed the same
//! way — a labelled block ahead of the operator's task — and the two
//! compose in a fixed order: the master prompt (the role) first, then the
//! repository's own instructions, then the operator's task. That order
//! matches how a reader would apply them: the role, then the local rules,
//! then the request.

use zoder_core::config::Config;
use zoder_core::prompt::{self, AssembledPrompt};

/// The header that separates the master prompt from the task text.
pub const MASTER_PROMPT_HEADER: &str = "# Master system prompt (zoder)\n\n";

/// The separator between the master-prompt block and the task text.
pub const MASTER_PROMPT_SEPARATOR: &str = "\n\n---\n\n";

/// The assembled master prompt for a model, plus the text to send.
///
/// `master` is `None` when the master prompt is disabled (the default) or
/// when the assets could not be loaded. The two cases are DIFFERENT on
/// purpose:
///
/// * disabled → the task text is returned unchanged, byte-for-byte;
/// * enabled but the assets are missing → an error naming what is missing,
///   because an operator who asked for the master prompt must not silently
///   get a run without it.
pub fn compose_author_prompt(
    cfg: &Config,
    model: &str,
    task: &str,
) -> Result<String, zoder_core::prompt::PromptError> {
    if !cfg.prompt.master_enabled() {
        return Ok(task.to_string());
    }
    let root = prompt::prompts_root(&Config::home());
    let assembled: AssembledPrompt = prompt::assemble(&root, cfg.prompt.master_version(), model)?;
    Ok(render(&assembled, task))
}

/// Render the assembled master prompt ahead of the task text.
fn render(assembled: &AssembledPrompt, task: &str) -> String {
    let mut out = String::with_capacity(
        MASTER_PROMPT_HEADER.len()
            + assembled.text.len()
            + MASTER_PROMPT_SEPARATOR.len()
            + task.len(),
    );
    out.push_str(MASTER_PROMPT_HEADER);
    out.push_str(&assembled.text);
    out.push_str(MASTER_PROMPT_SEPARATOR);
    out.push_str(task);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ZODER_PROMPT_DIR` is process-global, so the tests that set it are
    /// serialized behind one mutex. Without this, the "enabled" test and
    /// the "missing asset" test race: the second one can read the first
    /// one's directory and wrongly succeed.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `f` with `ZODER_PROMPT_DIR` set to `dir`, restoring the
    /// previous value afterwards.
    fn with_prompt_dir<T>(dir: Option<&std::path::Path>, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("ZODER_PROMPT_DIR").ok();
        match dir {
            Some(d) => std::env::set_var("ZODER_PROMPT_DIR", d),
            None => std::env::remove_var("ZODER_PROMPT_DIR"),
        }
        let out = f();
        match previous {
            Some(v) => std::env::set_var("ZODER_PROMPT_DIR", v),
            None => std::env::remove_var("ZODER_PROMPT_DIR"),
        }
        out
    }

    /// The repository's checked-in prompt assets.
    fn repo_prompts() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("prompts")
    }

    /// A config with the master prompt explicitly OFF.
    ///
    /// The `home` path is only stored, never written to, but it is still
    /// a workspace-local path: this host's `/tmp` is a tmpfs, and the
    /// fleet's job rule is to stay off it entirely.
    fn cfg_off() -> Config {
        let mut cfg = Config::default_provider(std::path::Path::new("/nonexistent/zoder-home"));
        cfg.prompt.master = false;
        cfg.prompt.master_version = None;
        cfg
    }

    #[test]
    fn the_disabled_path_returns_the_task_byte_for_byte() {
        let task = "fix the thing\nwith newlines\n";
        let out = compose_author_prompt(&cfg_off(), "deepseek-flash", task)
            .expect("the disabled path never touches the filesystem");
        assert_eq!(
            out, task,
            "with the master prompt off the task text must be byte-for-byte unchanged"
        );
    }

    #[test]
    fn the_disabled_path_is_the_default() {
        let cfg = Config::default_provider(std::path::Path::new("/nonexistent/zoder-home"));
        assert!(!cfg.prompt.master, "the default must be off");
        let task = "task";
        let out = compose_author_prompt(&cfg, "deepseek-flash", task).expect("default path");
        assert_eq!(out, task);
    }

    #[test]
    fn the_enabled_path_prepends_the_master_prompt() {
        let mut cfg = cfg_off();
        cfg.prompt.master = true;
        let root = repo_prompts();
        let out = with_prompt_dir(Some(&root), || {
            compose_author_prompt(&cfg, "deepseek-flash", "the task")
        });
        let out = out.expect("the assets exist in the repository");
        assert!(out.starts_with(MASTER_PROMPT_HEADER), "{out}");
        assert!(
            out.contains(MASTER_PROMPT_SEPARATOR),
            "the master block must be separated from the task"
        );
        assert!(
            out.ends_with("the task"),
            "the task must remain the tail of the prompt"
        );
        assert!(
            out.contains("Stay with the task until it is finished"),
            "the master prompt body must be present"
        );
        assert!(
            out.contains("Model-specific notes (DeepSeek"),
            "the deepseek overlay must be attached for a deepseek model"
        );
    }

    #[test]
    fn the_overlay_follows_the_model_not_the_head() {
        // A chain fallback lands on a different family, and that family's
        // overlay must be the one attached.
        let mut cfg = cfg_off();
        cfg.prompt.master = true;
        let root = repo_prompts();
        let out = with_prompt_dir(Some(&root), || {
            compose_author_prompt(&cfg, "MiniMax-M3", "the task")
        });
        let out = out.expect("the assets exist in the repository");
        assert!(
            out.contains("Model-specific notes (MiniMax"),
            "the minimax overlay must be attached for a minimax model: {out}"
        );
        assert!(
            !out.contains("Model-specific notes (DeepSeek"),
            "the deepseek overlay must NOT be attached for a minimax model"
        );
    }

    #[test]
    fn a_missing_asset_is_an_error_when_the_prompt_is_enabled() {
        let mut cfg = cfg_off();
        cfg.prompt.master = true;
        let out = with_prompt_dir(Some(std::path::Path::new("/nonexistent/prompts")), || {
            compose_author_prompt(&cfg, "deepseek-flash", "the task")
        });
        assert!(
            out.is_err(),
            "an operator who asked for the master prompt must not silently get a run without it"
        );
    }
}
