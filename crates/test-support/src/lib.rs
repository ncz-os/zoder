//! Test-only helpers shared by the zoder workspace's test binaries.
//!
//! This crate is a dev-dependency only; nothing in a shipped binary links it.
//!
//! * [`ENV_LOCK`] is the one process-wide mutex every test that reads or
//!   writes the process environment serializes on. `std::env::set_var` is
//!   process-global, so two tests that each guard "their" variable with a
//!   private mutex still race: one test's value leaks into the other, and at
//!   the libc level `setenv` can reallocate `environ` while another thread
//!   runs `getenv`.
//! * [`EnvGuard`] holds [`ENV_LOCK`] for its lifetime and restores every
//!   variable it touched when it drops, BEFORE it releases the lock. Restoring
//!   after the unlock (the bug this crate replaces) lets the next test take
//!   the lock, set its own value, and then have it clobbered by our restore.
//! * [`IsolatedHome`] points `HOME`, `ZODER_HOME`, the XDG base dirs and the
//!   engine's config dir and socket into one tempdir, so a test cannot read or
//!   write the operator's real `~/.zoder`, `~/.zeroclaw` or live daemon.
//! * [`git`] hardens fixture repositories against the host's git config.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// The single process-wide lock for environment mutation in a test binary.
///
/// Each test binary links this crate once, so every module in that binary
/// (and every crate-local re-export of it) shares the same mutex.
pub static ENV_LOCK: Mutex<()> = Mutex::new(());

std::thread_local! {
    /// Whether this thread holds [`ENV_LOCK`] through an [`EnvLock`].
    static HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// [`ENV_LOCK`] held through this crate. Dropping it releases the lock.
///
/// Taking the lock a second time on the same thread (two live guards) would
/// deadlock silently, because the mutex is not re-entrant; [`lock_env`]
/// panics with a clear message instead. Code that locks [`ENV_LOCK`] directly
/// bypasses this check.
pub struct EnvLock {
    _guard: MutexGuard<'static, ()>,
}

impl Drop for EnvLock {
    fn drop(&mut self) {
        HELD.with(|h| h.set(false));
    }
}

/// Take [`ENV_LOCK`], ignoring poison: a test that panicked while holding it
/// has already restored the environment through its guard's `Drop`.
///
/// # Panics
///
/// If this thread already holds the lock through another [`EnvLock`],
/// [`EnvGuard`] or [`IsolatedHome`].
pub fn lock_env() -> EnvLock {
    assert!(
        !HELD.with(|h| h.get()),
        "zoder-test-support: ENV_LOCK is already held on this thread (nested \
         EnvGuard / IsolatedHome?); the lock is not re-entrant and this would deadlock"
    );
    let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    HELD.with(|h| h.set(true));
    EnvLock { _guard: guard }
}

/// RAII environment override.
///
/// Holds [`ENV_LOCK`] from construction until drop. Every variable passed to
/// [`EnvGuard::set`] or [`EnvGuard::unset`] is saved the first time it is
/// touched and restored, in reverse order, when the guard drops; the lock is
/// released only after that restore (struct fields drop after `Drop::drop`).
///
/// Constructing a second guard while one is alive on the same thread panics
/// (see [`lock_env`]).
#[must_use = "dropping the guard immediately restores the environment"]
pub struct EnvGuard {
    saved: Vec<(OsString, Option<OsString>)>,
    _lock: EnvLock,
}

impl EnvGuard {
    /// Take [`ENV_LOCK`] and start with nothing overridden.
    pub fn new() -> Self {
        Self {
            saved: Vec::new(),
            _lock: lock_env(),
        }
    }

    fn remember(&mut self, key: &str) {
        if !self.saved.iter().any(|(k, _)| k == key) {
            self.saved
                .push((OsString::from(key), std::env::var_os(key)));
        }
    }

    /// Set `key` to `value` until the guard drops.
    pub fn set(&mut self, key: &str, value: impl AsRef<OsStr>) -> &mut Self {
        self.remember(key);
        std::env::set_var(key, value);
        self
    }

    /// Remove `key` until the guard drops.
    pub fn unset(&mut self, key: &str) -> &mut Self {
        self.remember(key);
        std::env::remove_var(key);
        self
    }
}

impl Default for EnvGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..).rev() {
            match value {
                Some(v) => std::env::set_var(&key, v),
                None => std::env::remove_var(&key),
            }
        }
        // `_lock` drops after this body returns: restore, then unlock.
    }
}

/// A throwaway home directory with every host-identity variable pointed into
/// it, held under [`ENV_LOCK`] for the value's lifetime.
///
/// Sets (and creates) `HOME`, `ZODER_HOME` (`<dir>/.zoder`), `XDG_CONFIG_HOME`,
/// `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`, `XDG_RUNTIME_DIR`,
/// `ZEROCLAW_CONFIG_DIR` (`<dir>/.zeroclaw`) and `ZEROCLAW_SOCKET`
/// (`<dir>/.zeroclaw/data/daemon.sock`). The socket is never bound, so a
/// client that tries to reach "the" daemon gets a clean "no engine" error
/// instead of the host's live daemon. Further overrides go through
/// [`IsolatedHome::env`].
pub struct IsolatedHome {
    /// The guard holding the lock; extra variables can be set through it.
    /// Declared before `dir` so the environment is restored before the
    /// directory is deleted.
    pub env: EnvGuard,
    dir: tempfile::TempDir,
}

impl IsolatedHome {
    /// Create the tempdir and point the environment into it.
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("create isolated home tempdir");
        let root = dir.path();
        let zoder_home = root.join(".zoder");
        let zeroclaw = root.join(".zeroclaw");
        let runtime = root.join("run");
        let xdg = [
            root.join(".config"),
            root.join(".local").join("share"),
            root.join(".local").join("state"),
            root.join(".cache"),
        ];
        for d in [&zoder_home, &zeroclaw.join("data"), &runtime]
            .into_iter()
            .chain(xdg.iter())
        {
            std::fs::create_dir_all(d).expect("create isolated home subdir");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700));
        }
        let mut env = EnvGuard::new();
        env.set("HOME", root)
            .set("ZODER_HOME", &zoder_home)
            .set("XDG_CONFIG_HOME", &xdg[0])
            .set("XDG_DATA_HOME", &xdg[1])
            .set("XDG_STATE_HOME", &xdg[2])
            .set("XDG_CACHE_HOME", &xdg[3])
            .set("XDG_RUNTIME_DIR", &runtime)
            .set("ZEROCLAW_CONFIG_DIR", &zeroclaw)
            .set("ZEROCLAW_SOCKET", zeroclaw.join("data").join("daemon.sock"));
        Self { env, dir }
    }

    /// The isolated `HOME`.
    pub fn home(&self) -> &Path {
        self.dir.path()
    }

    /// The isolated `ZODER_HOME` (created).
    pub fn zoder_home(&self) -> PathBuf {
        self.dir.path().join(".zoder")
    }

    /// The isolated `ZEROCLAW_CONFIG_DIR` (created).
    pub fn zeroclaw_config_dir(&self) -> PathBuf {
        self.dir.path().join(".zeroclaw")
    }

    /// The isolated `ZEROCLAW_SOCKET` path (never bound).
    pub fn socket(&self) -> PathBuf {
        self.zeroclaw_config_dir().join("data").join("daemon.sock")
    }
}

impl Default for IsolatedHome {
    fn default() -> Self {
        Self::new()
    }
}

/// Git fixture hardening.
pub mod git {
    use std::path::Path;
    use std::process::Command;

    /// Repo-local settings every fixture repository gets, so a developer's or
    /// worker's global git config cannot change what a test observes. Signing
    /// is the one that breaks fixtures outright: with `commit.gpgsign=true`
    /// globally, every fixture commit (and every commit production code makes
    /// in the fixture) fails with "gpg failed to sign the data".
    pub const FIXTURE_CONFIG: &[(&str, &str)] =
        &[("commit.gpgsign", "false"), ("tag.gpgsign", "false")];

    /// Child environment that points `git` at an empty global and system
    /// config: the host's `~/.gitconfig` and `/etc/gitconfig` are not read.
    pub const ISOLATED_ENV: &[(&str, &str)] = &[
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ];

    /// Apply [`ISOLATED_ENV`] to a `git` command. Applied to the child only;
    /// the process environment is never touched.
    pub fn isolate(cmd: &mut Command) -> &mut Command {
        cmd.envs(ISOLATED_ENV.iter().copied())
    }

    /// Write [`FIXTURE_CONFIG`] into `repo`'s local config. Call right after
    /// `git init`. Because the settings are repo-local, they also cover git
    /// commands that production code runs in the fixture without the isolated
    /// environment.
    ///
    /// # Panics
    ///
    /// If `repo` is not a git work tree yet (called before `git init`), or if
    /// `git config` fails.
    pub fn harden_fixture_repo(repo: &Path) {
        assert!(
            repo.join(".git").exists(),
            "harden_fixture_repo({}): not a git repository; call it right after `git init`",
            repo.display()
        );
        for (key, value) in FIXTURE_CONFIG {
            let mut cmd = Command::new("git");
            cmd.arg("-C").arg(repo).args(["config", key, value]);
            let out = isolate(&mut cmd).output().expect("spawn git config");
            assert!(
                out.status.success(),
                "git config {key} {value} failed in {}: {}",
                repo.display(),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "ZODER_TEST_SUPPORT_SELF_TEST";

    #[test]
    fn env_guard_restores_previous_value_and_absence() {
        {
            let mut g = EnvGuard::new();
            g.set(KEY, "outer");
        }
        assert!(
            std::env::var_os(KEY).is_none(),
            "absent before -> absent after"
        );
        {
            let mut g = EnvGuard::new();
            g.set(KEY, "before");
            // Touching the same key twice restores the ORIGINAL value.
            g.set(KEY, "one").set(KEY, "two");
            assert_eq!(std::env::var(KEY).unwrap(), "two");
        }
        assert!(std::env::var_os(KEY).is_none());
    }

    #[test]
    fn env_guard_restores_before_releasing_the_lock() {
        // Another thread waits on ENV_LOCK while we hold an override. When it
        // gets the lock, our restore must already have happened.
        let mut g = EnvGuard::new();
        g.set(KEY, "held");
        let waiter = std::thread::spawn(|| {
            let _l = lock_env();
            std::env::var_os(KEY)
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(g);
        assert_eq!(
            waiter.join().unwrap(),
            None,
            "the next lock holder must see the restored environment"
        );
    }

    #[test]
    fn a_nested_guard_panics_instead_of_deadlocking() {
        let _outer = EnvGuard::new();
        let nested = std::panic::catch_unwind(EnvGuard::new);
        assert!(nested.is_err(), "a second guard on one thread must panic");
    }

    #[test]
    fn isolated_home_points_everything_into_one_tempdir() {
        let prev_home = std::env::var_os("HOME");
        let root;
        {
            let iso = IsolatedHome::new();
            root = iso.home().to_path_buf();
            assert_eq!(std::env::var_os("HOME").unwrap(), root.as_os_str());
            for key in [
                "ZODER_HOME",
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "XDG_STATE_HOME",
                "XDG_CACHE_HOME",
                "XDG_RUNTIME_DIR",
                "ZEROCLAW_CONFIG_DIR",
                "ZEROCLAW_SOCKET",
            ] {
                let v = PathBuf::from(std::env::var_os(key).unwrap());
                assert!(
                    v.starts_with(&root),
                    "{key}={} escapes {}",
                    v.display(),
                    root.display()
                );
                if key.starts_with("XDG_") || key == "ZODER_HOME" {
                    assert!(v.is_dir(), "{key} must exist: {}", v.display());
                }
            }
            assert_eq!(
                PathBuf::from(std::env::var_os("ZEROCLAW_SOCKET").unwrap()),
                iso.socket()
            );
            assert!(iso.zoder_home().is_dir());
            assert!(!iso.socket().exists(), "the socket is never bound");
        }
        let _l = lock_env();
        assert_eq!(std::env::var_os("HOME"), prev_home);
        assert!(!root.exists(), "tempdir removed on drop");
    }

    #[test]
    fn harden_fixture_repo_disables_signing_locally() {
        let dir = tempfile::tempdir().unwrap();
        let mut init = std::process::Command::new("git");
        init.arg("-C").arg(dir.path()).args(["init", "-q"]);
        assert!(git::isolate(&mut init).status().unwrap().success());
        git::harden_fixture_repo(dir.path());
        let mut get = std::process::Command::new("git");
        get.arg("-C")
            .arg(dir.path())
            .args(["config", "--local", "commit.gpgsign"]);
        let out = git::isolate(&mut get).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "false");
    }
}
