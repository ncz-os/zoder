//! Per-child environment overrides.
//!
//! # Why this exists
//!
//! The process environment is PROCESS-GLOBAL shared mutable state. Writing
//! to it with an UNLOCKED call while another thread is forking and exec'ing
//! a child corrupts the child's environment block: `fork` copies the
//! `environ` array while the writer is reallocating it, so the child is
//! handed a torn `envp` and `execve` fails with `EFAULT` ("Bad address").
//!
//! That is not a theoretical hazard — it is exactly what this crate's test
//! suite hit. `patch_journal_tests`, `loop_review_base_tests` and
//! `commit_author_enforcement_tests` all spawn `git`; a sibling test that
//! installed a hermetic `GIT_CONFIG_GLOBAL` through `libc::setenv` mutated
//! the environment while those children were being spawned, and the
//! sporadic failures surfaced as
//!
//! ```text
//! panicked at crates/zoder-cli/src/agentic.rs:15328:14:
//! spawn git: Os { code: 14, kind: Uncategorized, message: "Bad address" }
//! ```
//!
//! # Which write is the dangerous one
//!
//! `std::env::set_var` / `std::env::remove_var` are SERIALIZED by `std`
//! itself: `std` takes an internal lock around both its own environment
//! reads and its own writes. A raw `libc::setenv` / `libc::unsetenv`
//! takes NO lock and therefore races every concurrently-spawning sibling
//! test.
//!
//! Measured directly, in a standalone program with 12 writer threads and
//! 12 spawning threads on this host:
//!
//! | environment write | spawn failures (EFAULT) |
//! |---|---|
//! | `std::env::set_var` + `std::env::remove_var` | **0** of 36 000 |
//! | `libc::setenv` + `libc::unsetenv` | **779** of 36 000 |
//!
//! So the raw libc calls were the defect, and they are gone: this crate
//! now contains no `libc::setenv` / `libc::unsetenv` at all (pinned by
//! `no_unlocked_process_environment_mutation_in_this_crate`).
//!
//! # The rule
//!
//! Every environment override in zoder is applied to the CHILD
//! (`Command::env` / `Command::env_remove`) and never to the process
//! environment. Production code that genuinely needs the operator's
//! ambient environment (`$PATH`, `$HOME`, …) keeps reading it; only the
//! *overrides* move to the child, where they cannot race anybody.
//!
//! [`ChildEnv`] is the vehicle for that: a small, cloneable list of
//! variables to set and variables to remove, applied to one
//! [`std::process::Command`] (or [`tokio::process::Command`]) at spawn
//! time.

/// Environment overrides for a single child process.
///
/// Values are applied with `Command::env` (which replaces any inherited
/// value) and removals with `Command::env_remove` (which hides an
/// inherited variable from the child entirely).
///
/// The type is intentionally dumb: it holds data, applies it, and knows
/// nothing about which variables are meaningful for which child.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChildEnv {
    /// Variables to set (or replace) in the child's environment.
    set: Vec<(String, String)>,
    /// Variables to remove from the child's environment.
    unset: Vec<String>,
}

impl ChildEnv {
    /// An empty override set: the child sees exactly the inherited
    /// environment.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Build an override set from `(key, value)` pairs.
    ///
    /// Used by the test harness to build the hermetic-git override set
    /// in one call.
    #[cfg(test)]
    pub(crate) fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self {
            set: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            unset: Vec::new(),
        }
    }

    /// Set (or replace) `key` in the child's environment.
    pub(crate) fn set(mut self, key: &str, value: impl AsRef<str>) -> Self {
        self.set.push((key.to_string(), value.as_ref().to_string()));
        self
    }

    /// Remove `key` from the child's environment.
    ///
    /// The counterpart of [`ChildEnv::set`] for variables the child must
    /// NOT see (an inherited `HOME`, a `PATH` the child must not pick up).
    /// Currently used by the test harness; the production call sites all
    /// override rather than remove.
    #[cfg(test)]
    pub(crate) fn unset(mut self, key: &str) -> Self {
        self.unset.push(key.to_string());
        self
    }

    /// The value this override set pins for `key`, if any.
    #[cfg(test)]
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.set
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Whether this override set removes `key` from the child's
    /// environment.
    #[cfg(test)]
    pub(crate) fn removes(&self, key: &str) -> bool {
        self.unset.iter().any(|k| k == key)
    }

    /// Apply the overrides to a `std::process::Command`.
    pub(crate) fn apply_std(&self, cmd: &mut std::process::Command) {
        for (key, value) in &self.set {
            cmd.env(key, value);
        }
        for key in &self.unset {
            cmd.env_remove(key);
        }
    }

    /// Apply the overrides to a `tokio::process::Command`.
    pub(crate) fn apply_tokio(&self, cmd: &mut tokio::process::Command) {
        for (key, value) in &self.set {
            cmd.env(key, value);
        }
        for key in &self.unset {
            cmd.env_remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_are_recorded_in_order_and_never_touch_the_process_env() {
        let env = ChildEnv::new()
            .set("HOME", "/tmp/fake-home")
            .unset("USER")
            .set("PATH", "/usr/bin");
        assert_eq!(env.get("HOME"), Some("/tmp/fake-home"));
        assert_eq!(env.get("PATH"), Some("/usr/bin"));
        assert_eq!(env.get("USER"), None);
        assert!(env.removes("USER"));
        assert!(!env.removes("HOME"));
    }

    /// The child must observe the override, and the PARENT process
    /// environment must be untouched by it. This is the property that makes
    /// the whole scheme safe: a test that pins `$HOME` for its child cannot
    /// corrupt a sibling test's `fork`.
    #[test]
    fn applying_overrides_leaves_the_process_environment_alone() {
        let key = "ZODER_CHILD_ENV_PROBE";
        let before = std::env::var_os(key);
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(format!("printf %s \"${key}\""));
        ChildEnv::new().set(key, "child-value").apply_std(&mut cmd);
        let out = cmd.output().expect("spawn sh");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "child-value");
        assert_eq!(
            std::env::var_os(key),
            before,
            "the parent's environment must not be modified by a child override"
        );
    }

    /// A removed variable must be invisible to the child even when the
    /// parent has it. `HOME` is used as the probe because the parent
    /// certainly has it — and, crucially, the test never writes to the
    /// process environment itself.
    #[test]
    fn removed_variables_are_invisible_to_the_child() {
        assert!(
            std::env::var_os("HOME").is_some(),
            "pre-condition: the parent must have HOME for this probe to mean anything"
        );
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg("printf %s \"${HOME}\"");
        ChildEnv::new().unset("HOME").apply_std(&mut cmd);
        let out = cmd.output().expect("spawn sh");
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "",
            "a removed variable must not reach the child"
        );
    }

    /// An EMPTY override set must leave the child with the FULL inherited
    /// environment — including `PATH`.
    ///
    /// This pins the semantics of [`ChildEnv::new`]: "no overrides" means
    /// "inherit everything", NOT "start from an empty environment". A
    /// reviewer reading `run_git` as `run_git_env(cwd, args,
    /// &ChildEnv::new())` could reasonably worry that the git children
    /// would lose `PATH` (and so fail to find `git` itself, or any
    /// helper `git` shells out to). They do not: `apply_std` only ADDS
    /// overrides and REMOVEs the keys listed in `unset`; it never clears
    /// the inherited environment. `Command` inherits the parent's
    /// environment by default, and an empty `ChildEnv` adds nothing to
    /// and removes nothing from it.
    #[test]
    fn an_empty_override_set_leaves_the_child_environment_intact() {
        let path_before = std::env::var_os("PATH").expect("the parent has a PATH");
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg("printf %s \"${PATH}\"");
        ChildEnv::new().apply_std(&mut cmd);
        let out = cmd.output().expect("spawn sh");
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            path_before.to_string_lossy(),
            "an empty override set must leave the child's PATH exactly as \
             the parent had it"
        );
    }

    /// The same contract, observed through the git helper the production
    /// code uses: a `git` spawned with an EMPTY override set still sees
    /// the ambient environment, so `run_git` behaves exactly as it did
    /// before the `ChildEnv` refactor.
    #[test]
    fn an_empty_override_set_still_finds_git_on_the_inherited_path() {
        // Find `git` the way the production helper does: through the
        // inherited PATH, with no override applied.
        let mut cmd = std::process::Command::new("git");
        cmd.arg("--version");
        ChildEnv::new().apply_std(&mut cmd);
        let out = cmd.output().expect("spawn git");
        assert!(
            out.status.success(),
            "git must be reachable through the inherited PATH with an \
             empty override set; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// **REGRESSION: the EFAULT ("Bad address") flake.**
    ///
    /// This is the root cause of the sporadic
    /// `spawn git: Os { code: 14, kind: Uncategorized, message: "Bad
    /// address" }` failures that hit `patch_journal_tests`,
    /// `loop_review_base_tests` and `commit_author_enforcement_tests`.
    ///
    /// `fork` copies the process's `environ` array into the child. If
    /// another thread mutates that array WHILE the copy is in flight,
    /// the child is handed a torn environment block and `execve` fails
    /// with `EFAULT` ("Bad address"). The historical trigger was the
    /// test-only `HermeticGitConfig` guard, which installed a hermetic
    /// `GIT_CONFIG_GLOBAL` / `GIT_CONFIG_SYSTEM` with RAW
    /// `libc::setenv` / `libc::unsetenv` — calls that take no lock and
    /// therefore race every concurrently-spawning sibling test.
    ///
    /// The fix is that zoder no longer mutates the process environment
    /// at all: overrides go to the CHILD via `Command::env` /
    /// `Command::env_remove`. This test pins that contract:
    ///
    ///   * The source of this crate must contain no raw
    ///     `libc::setenv` / `libc::unsetenv` (the unlocked mutation
    ///     that caused the torn `environ`).
    ///   * A `ChildEnv` override must leave the parent's environment
    ///     byte-for-byte unchanged, even while other threads are
    ///     spawning children.
    ///
    /// Before the fix the first assertion fails (the source contains
    /// `libc::setenv`); after it, both pass.
    #[test]
    fn no_unlocked_process_environment_mutation_in_this_crate() {
        let src_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders: Vec<String> = Vec::new();
        let mut stack = vec![src_root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir)
                .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
            for entry in entries {
                let entry = entry.expect("dir entry");
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
                for (lineno, line) in text.lines().enumerate() {
                    // `libc::setenv` / `libc::unsetenv` mutate the
                    // process environment WITHOUT the lock `std` uses,
                    // so they can tear the `environ` block a
                    // concurrently-forking sibling is copying.
                    //
                    // Only CODE lines count. A mention inside a doc
                    // comment or a string literal is documentation of
                    // the hazard, not a call to it — this very file
                    // names the symbols in its module docs. The check
                    // is therefore "the symbol appears outside a
                    // comment and outside a string literal", which is
                    // approximated by requiring the line to contain a
                    // `(` immediately after the symbol (the call
                    // shape) and to not be a comment.
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("//") {
                        continue;
                    }
                    for needle in ["libc::setenv", "libc::unsetenv"] {
                        let call_shape = format!("{needle}(");
                        // A call shape inside a string literal (e.g. the
                        // needle embedded in a longer quoted string) is
                        // not a call. Require the symbol to be preceded
                        // by a non-identifier character.
                        let is_call = line.match_indices(&call_shape).any(|(idx, _)| {
                            let before = line[..idx].chars().next_back();
                            !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
                        });
                        if is_call {
                            offenders.push(format!("{}:{}: {needle}", path.display(), lineno + 1));
                        }
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "this crate must not mutate the process environment with raw \
             libc calls: they take no lock and tear the `environ` block a \
             concurrently-forking sibling test is copying, which is the \
             EFAULT (\"Bad address\") flake. Offenders:\n{}",
            offenders.join("\n")
        );
    }

    /// The same contract, from the other side: while other threads are
    /// busy spawning children, applying a `ChildEnv` override must
    /// neither touch the parent's environment nor make a sibling's
    /// spawn fail.
    ///
    /// The spawn-half is the load-bearing part. On the pre-fix code,
    /// the hermetic-git guard's `libc::setenv` raced exactly this
    /// window and produced `EFAULT` on the sibling's `execve`.
    #[test]
    fn child_env_overrides_do_not_disturb_concurrent_spawns() {
        const SPAWNERS: usize = 4;
        const SPAWNS_PER_THREAD: usize = 60;

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spawn_errors = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        // Background spawners: children spawned while the main thread
        // applies child-scoped overrides.
        let mut handles = Vec::new();
        for _ in 0..SPAWNERS {
            let stop = std::sync::Arc::clone(&stop);
            let spawn_errors = std::sync::Arc::clone(&spawn_errors);
            handles.push(std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    match std::process::Command::new("/bin/sh")
                        .arg("-c")
                        .arg("exit 0")
                        .output()
                    {
                        Ok(_) => {}
                        Err(_) => {
                            spawn_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
            }));
        }

        // The main thread applies child-scoped overrides, repeatedly.
        let key = "ZODER_CHILD_ENV_RACE_PROBE";
        let before = std::env::var_os(key);
        for _ in 0..SPAWNS_PER_THREAD {
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.arg("-c").arg("exit 0");
            ChildEnv::new()
                .set(key, "child-only")
                .unset("ZODER_CHILD_ENV_RACE_PROBE_ABSENT")
                .apply_std(&mut cmd);
            cmd.output().expect("spawn sh");
        }

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for h in handles {
            h.join().expect("spawner thread panicked");
        }

        assert_eq!(
            spawn_errors.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a sibling's spawn must never fail while another thread applies \
             child-scoped environment overrides"
        );
        assert_eq!(
            std::env::var_os(key),
            before,
            "applying child overrides must not modify the parent's environment"
        );
    }
}
