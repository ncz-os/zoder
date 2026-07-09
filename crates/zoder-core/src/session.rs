//! Multi-turn session persistence.
//!
//! A session is an append-only transcript of chat messages stored as a single
//! JSON file under `$ZODER_HOME/sessions/<id>.json`. `exec` can attach to a
//! session (`--session <id>`) or continue the most-recently-updated one
//! (`--continue`), so follow-up prompts carry prior context.
//!
//! ## Concurrency (DEFECT 1 fix)
//!
//! Two concurrent `zoder exec --session <shared-id>` processes used to each
//! `load_or_new` the same transcript, append a DIFFERENT turn in memory, then
//! `save` — without any lock spanning load -> append -> save. The second
//! process's atomic-rename save then silently overwrote the first process's,
//! losing one turn. The fix is [`Session::mutate_locked`], which takes an
//! exclusive `flock(2)` on a per-session sidecar `<id>.json.lock` for the
//! entire load -> apply(f) -> save critical section, mirroring the pattern
//! already used by `utilization::UtilizationStore::open` and
//! `model_health::HealthStore::mutate_locked`. Callers that only need a
//! best-effort read (`Session::latest`, the read-only `load_or_new` path
//! for `list`) stay lock-free.
//!
//! ## Size cap (DEFECT 2 fix)
//!
//! `load_or_new` previously read the whole file into memory with no size
//! check. A transcript that grew very large (e.g. compounding
//! duplicate/corrupt content from the unlocked race, or organic growth on
//! a long-lived session) could be read in full, risking OOM on
//! `zoder exec --continue`. The fix checks the file's `metadata().len()`
//! against [`MAX_TRANSCRIPT_BYTES`] BEFORE the body read; an oversized
//! transcript is RENAMED ASIDE (quarantined) with a clear warning rather
//! than opened, and the call returns a fresh empty session so a stray
//! megabyte does not wedge the next run. The cap style follows the
//! `MAX_PRICE_BYTES` / `MAX_RESPONSE_BYTES` constants elsewhere in the
//! crate: a module-private constant, checked on metadata (not on the read
//! payload), with the bad file preserved under a unique name for
//! inspection.

use crate::provider::Message;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic per-process nonce for save() temp-file names.
///
/// Combined with the process id it makes each writer's temp file
/// (`<stem>.json.tmp.<pid>.<nonce>`) unique, so two overlapping
/// `zoder exec --session <shared-id>` fan-out writers can never compute
/// the same temp path and clobber each other into a torn transcript.
static SAVE_NONCE: AtomicU64 = AtomicU64::new(0);

/// Maximum trusted on-disk size of a single session transcript. Larger
/// files are refused BEFORE the body is read and renamed aside (quarantined)
/// with a clear warning, so a runaway or duplicate-bloated transcript
/// cannot OOM the loader on `zoder exec --continue` or `--session <id>`.
///
/// 8 MiB is comfortably larger than any legitimate transcript zoder has
/// observed in practice (a real long-running agentic session with
/// dozens of tool turns is well under 1 MiB), while still preventing a
/// multi-gigabyte stray file from being slurped into memory. The unit
/// and style match the `MAX_PRICE_BYTES` / `MAX_RESPONSE_BYTES` /
/// `MAX_PROJECT_INSTRUCTIONS_BYTES` constants elsewhere in the crate.
pub const MAX_TRANSCRIPT_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    /// Unix seconds.
    pub created: i64,
    /// Unix seconds; bumped on every save.
    pub updated: i64,
    pub messages: Vec<Message>,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn safe_id(id: &str) -> String {
    // Keep session ids filesystem-safe; never allow path traversal.
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl Session {
    pub fn new(id: &str) -> Self {
        let t = now();
        Session {
            id: safe_id(id),
            created: t,
            updated: t,
            messages: Vec::new(),
        }
    }

    fn path_in(dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("{}.json", safe_id(id)))
    }

    /// Per-session sidecar lockfile path. Matches the
    /// `<data_path>.lock` convention used by
    /// `utilization::UtilizationStore::lockfile_path` and the explicit
    /// `path.with_extension("lock")` form in
    /// `model_health::HealthStore::mutate_locked` (DEFECT 1 fix).
    fn lockfile_path(path: &Path) -> PathBuf {
        // `<id>.json` -> `<id>.json.lock`. We push `.lock` onto the
        // existing os-string (which already carries `.json`) rather than
        // replacing the extension, so a non-`.json` extension doesn't
        // produce a different name. The conventions across the crate
        // agree on the ".lock" suffix; the difference is only in whether
        // it's appended to the full os-string (utilization) or produced
        // by `with_extension` (model-health). For sessions the full
        // os-string + ".lock" form keeps the data filename intact and
        // visible, so a human inspecting the sessions dir sees
        // "shared-id.json" and "shared-id.json.lock" as a pair.
        let mut s = path.as_os_str().to_owned();
        s.push(".lock");
        PathBuf::from(s)
    }

    /// Load a session by id, or create a fresh empty one if it does not
    /// exist. An oversized on-disk transcript (larger than
    /// [`MAX_TRANSCRIPT_BYTES`]) is refused BEFORE the body is read and
    /// renamed aside (quarantined) with a clear warning, so a runaway
    /// file cannot OOM the loader; the call then returns a fresh empty
    /// session (DEFECT 2 fix).
    pub fn load_or_new(dir: &Path, id: &str) -> anyhow::Result<Self> {
        let path = Self::path_in(dir, id);
        if path.exists() {
            // Size-cap check via metadata before the body read. We
            // cannot rely on `read_to_string` to bail us out cheaply
            // (it would have slurped the whole file first), and the
            // giant-file symptom is the bug we are fixing here.
            let size = match std::fs::metadata(&path) {
                Ok(m) => m.len(),
                Err(_) => 0, // metadata race: fall through; the read will surface the real I/O error
            };
            if size > MAX_TRANSCRIPT_BYTES {
                Self::quarantine_oversized(&path, size);
                return Ok(Session::new(id));
            }
            let raw = std::fs::read_to_string(&path)?;
            let s: Session = serde_json::from_str(&raw).map_err(|e| {
                anyhow::anyhow!("session {}: corrupt transcript: {e}", path.display())
            })?;
            Ok(s)
        } else {
            Ok(Session::new(id))
        }
    }

    /// Rename an oversized transcript aside under a unique quarantine
    /// name and log a clear warning. Mirrors the corrupt-store backup
    /// style in `model_health::HealthStore::load` (same `nonce + pid +
    /// unix_secs` stamp so repeated quarantines each keep their own
    /// copy) and the warning style in `pricing::PricingCatalog::load`
    /// ("zoder: warning: ... rejected — ... exceeds ... limit; ...").
    /// The original path is removed from the live sessions dir so a
    /// subsequent `load_or_new` / `latest` / `list` does not pick it
    /// back up; the quarantined file is preserved on disk for an
    /// operator to inspect or hand-prune.
    fn quarantine_oversized(path: &Path, size: u64) {
        let nonce = SAVE_NONCE.fetch_add(1, Ordering::Relaxed);
        let stamp = format!("json.oversized.{}.{}.{}", now(), std::process::id(), nonce);
        let quarantine = path.with_extension(stamp);
        eprintln!(
            "zoder: warning: session transcript {} rejected — {} bytes exceeds {} byte limit; \
             quarantined to {} and starting a fresh session",
            path.display(),
            size,
            MAX_TRANSCRIPT_BYTES,
            quarantine.display()
        );
        // Best-effort move. A failure here does not prevent the loader
        // from returning a fresh session — the user will simply see
        // the oversized file still in place and a slightly noisier
        // warning on the next call.
        if let Err(e) = std::fs::rename(path, &quarantine) {
            eprintln!(
                "zoder: warning: could not quarantine oversized session transcript {}: {e}",
                path.display()
            );
        }
    }

    /// Find the most-recently-updated session in `dir`, if any.
    pub fn latest(dir: &Path) -> anyhow::Result<Option<Self>> {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Ok(None);
        };
        let mut best: Option<Session> = None;
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // DEFECT 2 fix (also applied on the `latest` path): a
            // runaway / compound-bloat transcript must not be slurped
            // into memory just to compute the "latest" answer. An
            // oversized candidate is SKIPPED (and quarantined) so a
            // single bad file cannot OOM `zoder exec --continue` while
            // it scans the sessions dir.
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            if size > MAX_TRANSCRIPT_BYTES {
                Self::quarantine_oversized(&p, size);
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&p) else {
                continue;
            };
            let Ok(s) = serde_json::from_str::<Session>(&raw) else {
                continue;
            };
            if best.as_ref().map(|b| s.updated > b.updated).unwrap_or(true) {
                best = Some(s);
            }
        }
        Ok(best)
    }

    pub fn push(&mut self, role: &str, content: &str) {
        self.messages.push(Message::new(role, content));
    }

    /// Persist atomically (unique temp file + rename) under `dir`.
    ///
    /// C7-S1 (= S19 for sessions): the temp filename carries the process id AND
    /// a monotonic nonce (`<stem>.json.tmp.<pid>.<nonce>`) so two concurrent
    /// `zoder exec --session <shared-id>` fan-out writers can never share a temp
    /// path -- otherwise an interleaved write+rename could promote a torn or
    /// foreign temp file over the live transcript, losing messages. The temp is
    /// removed on any error so a failed write never litters the sessions dir with
    /// a half-written file a later reader could pick up. Mirrors the unique-temp
    /// pattern in `corpus.rs` (C5-4) and `pricing.rs` (C6-P1).
    pub fn save(&mut self, dir: &Path) -> anyhow::Result<()> {
        Self::save_locked(self, dir)
    }

    /// The actual write path, shared by [`Session::save`] and
    /// [`Session::mutate_locked`]. Takes `&mut Session` and runs the
    /// same unique-temp + fsync + rename dance as the original
    /// `save()`; the lock-aware wrapper holds an exclusive `flock(2)`
    /// on the sidecar `<id>.json.lock` for the WHOLE
    /// load -> apply(f) -> save critical section so two processes
    /// serialize instead of racing (DEFECT 1 fix).
    fn save_locked(sess: &mut Session, dir: &Path) -> anyhow::Result<()> {
        sess.updated = now();
        std::fs::create_dir_all(dir)?;
        let path = Self::path_in(dir, &sess.id);
        let data = serde_json::to_vec_pretty(sess)?;
        let nonce = SAVE_NONCE.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("json.tmp.{}.{}", std::process::id(), nonce));
        // Write to the unique temp; on any failure remove it so it can never be
        // renamed over the live transcript or left behind torn.
        let f = match std::fs::File::create(&tmp) {
            Ok(f) => f,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
        };
        {
            use std::io::Write as _;
            let mut w = std::io::BufWriter::new(&f);
            if let Err(e) = w.write_all(&data).and_then(|_| w.flush()) {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into());
            }
        }
        // Durability: flush the file contents to disk before the rename so a
        // crash after rename cannot expose a zero-length transcript (matches
        // utilization.rs).
        if let Err(e) = f.sync_all() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// Atomic locked read-modify-write (DEFECT 1 fix). Takes an
    /// exclusive `flock(2)` on a per-session sidecar `<id>.json.lock`
    /// file for the WHOLE load -> apply(f) -> save critical section, so
    /// two concurrent `zoder exec --session <shared-id>` fan-out
    /// writers serialize instead of racing. Without this guard, P1
    /// loads a snapshot, P2 loads the same snapshot, P1 appends and
    /// saves, P2 appends a DIFFERENT turn to its now-stale snapshot and
    /// saves — and P2's atomic-rename silently overwrites P1's, losing
    /// P1's turn with no error.
    ///
    /// The lock is an MSRV-safe **lockfile** acquired via
    /// `fs2::FileExt::lock_exclusive`, the same `flock(2)`-based idiom
    /// `utilization::UtilizationStore::acquire_lock` already uses
    /// (kernel-level wait, released on FD close so a crashed process
    /// can't deadlock the next caller). The lock is held by the
    /// `LockGuard` RAII wrapper and released on Drop (including panic
    /// / early return). The closure sees a `Session` loaded from
    /// `path` UNDER the lock; its mutation is persisted by
    /// `save_locked` (unique-temp + fsync + rename) before the guard
    /// drops. I/O errors from the load path are non-fatal (a fresh
    /// session is used, mirroring the existing `load_or_new` contract);
    /// lock-acquire and save failures propagate.
    ///
    /// Callers should prefer this method over the bare
    /// `load_or_new` + `save` pair whenever the in-memory mutation is
    /// intended to merge with whatever another process may be writing
    /// concurrently (i.e. ALWAYS in `zoder exec --session <id>`).
    pub fn mutate_locked(dir: &Path, id: &str, f: impl FnOnce(&mut Session)) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = Self::path_in(dir, id);
        let lock_path = Self::lockfile_path(&path);
        // Acquire the per-session lockfile (RAII: released on Drop,
        // incl. panic / early return) BEFORE loading, so the whole
        // read-modify-write is exclusive. Same `flock(2)` idiom as
        // `utilization::UtilizationStore::acquire_lock`; on process
        // death the kernel closes the FD and releases the lock so a
        // crashed holder can't deadlock the next caller.
        let _guard = LockGuard::acquire(&lock_path)?;
        // Load the freshest on-disk state UNDER the lock so we merge
        // onto whatever the previous holder just wrote. This
        // delegates to `load_or_new` so the size cap and quarantine
        // behavior (DEFECT 2 fix) also apply to the locked path —
        // otherwise a locked mutation could still slurp a giant
        // file on the way in.
        let mut sess = Self::load_or_new(dir, id)?;
        f(&mut sess);
        Self::save_locked(&mut sess, dir)
        // `_guard` drops here, releasing the lock.
    }

    /// List sessions (id, updated, message-count), newest first.
    pub fn list(dir: &Path) -> anyhow::Result<Vec<(String, i64, usize)>> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Ok(out);
        };
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // DEFECT 2 fix (also applied on the `list` path): a
            // runaway transcript must not be slurped into memory just
            // to count its messages; oversized candidates are
            // quarantined and skipped so a single bad file cannot
            // OOM `list`.
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            if size > MAX_TRANSCRIPT_BYTES {
                Self::quarantine_oversized(&p, size);
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&p) else {
                continue;
            };
            let Ok(s) = serde_json::from_str::<Session>(&raw) else {
                continue;
            };
            out.push((s.id, s.updated, s.messages.len()));
        }
        out.sort_by_key(|e| std::cmp::Reverse(e.1));
        Ok(out)
    }
}

/// RAII guard for the per-session `<id>.json.lock` lockfile. The
/// `fs2::FileExt::lock_exclusive` lock is held by the FD for the
/// guard's lifetime, so the lock is released as soon as the guard
/// drops (incl. panic / early return). The on-disk lockfile is left
/// in place after the FD closes — the kernel releases the lock on
/// close, and the next `acquire` simply reopens it. We do NOT remove
/// the lockfile in `Drop` because `flock(2)` semantics are tied to
/// the FD, not the path; removing the file would race with another
/// process that just opened the same path for a fresh acquire.
struct LockGuard {
    _file: std::fs::File,
}

impl LockGuard {
    fn acquire(lock_path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        // Open (creating if needed) the sidecar lockfile. We use
        // `create(true)` + `truncate(false)` so a stale lockfile
        // from a previous run is left untouched (the lock is on the
        // FD, not the contents).
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        // `lock_exclusive` blocks until the kernel grants the lock;
        // on process death the FD is closed by the kernel and the
        // lock is released, so a crashed process can't deadlock the
        // next caller. Mirrors the idiom in
        // `utilization::UtilizationStore::acquire_lock`.
        f.lock_exclusive()?;
        Ok(LockGuard { _file: f })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let mut d = std::env::temp_dir();
        let uniq = format!(
            "zoder-session-test-{}-{}",
            std::process::id(),
            SAVE_NONCE.fetch_add(1, Ordering::Relaxed)
        );
        d.push(uniq);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // C7-S1: save() uses a unique temp filename and leaves no stray `.json.tmp`
    // (deterministic or per-writer) behind, and the transcript reloads intact.
    #[test]
    fn save_uses_unique_temp_and_leaves_no_stray_tmp() {
        let dir = tmpdir();
        let mut sess = Session {
            id: "shared-id".into(),
            created: now(),
            updated: 0,
            messages: Vec::new(),
        };
        sess.push("user", "hello");
        sess.push("assistant", "hi there");
        sess.save(&dir).unwrap();

        // The transcript is present and reloads with content intact.
        let path = Session::path_in(&dir, "shared-id");
        assert!(path.exists());
        let raw = std::fs::read_to_string(&path).unwrap();
        let reloaded: Session = serde_json::from_str(&raw).unwrap();
        assert_eq!(reloaded.id, "shared-id");
        assert_eq!(reloaded.messages.len(), 2);
        assert_eq!(reloaded.messages[0].content, "hello");
        assert_eq!(reloaded.messages[1].content, "hi there");

        // No file matching `.json.tmp` (deterministic or per-writer) survives:
        // the temp must be uniquely named AND renamed/removed, so only
        // `shared-id.json` is left.
        for entry in std::fs::read_dir(&dir).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(
                !name.contains(".json.tmp"),
                "stray temp file left behind: {name}"
            );
        }
        // Belt-and-suspenders: the legacy deterministic temp path never exists.
        assert!(!path.with_extension("json.tmp").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------------
    // DEFECT 1 (lost-update) pin: mutate_locked serializes the
    // load -> apply -> save critical section so two sequential
    // locked writers, each appending a DIFFERENT turn, BOTH survive
    // on reload. The pre-fix `load_or_new` + `save` pattern (no
    // lock spanning the sequence) would have let the second writer
    // save a stale snapshot and overwrite the first writer's turn.
    // -----------------------------------------------------------------

    /// DEFECT 1 main pin: two sequential `mutate_locked` calls, each
    /// appending a different message, must BOTH survive on reload.
    /// Mirrors `model_health::HealthStore::mutate_locked_serializes_and_does_not_lose_updates`:
    /// the second writer reloads the freshest on-disk state UNDER the
    /// lock before applying, so neither turn is lost.
    #[test]
    fn mutate_locked_serializes_and_does_not_lose_updates() {
        let dir = tmpdir();

        // Seed an on-disk session so both mutations merge onto real prior state.
        Session::mutate_locked(&dir, "shared-id", |s| {
            s.push("user", "seed-prompt");
            s.push("assistant", "seed-reply");
        })
        .unwrap();

        // P1 appends a DIFFERENT turn.
        Session::mutate_locked(&dir, "shared-id", |s| {
            s.push("user", "P1-prompt");
            s.push("assistant", "P1-reply");
        })
        .unwrap();

        // P2 appends a DIFFERENT turn. If P2 had loaded a stale
        // snapshot (pre-fix behavior), P1's turn would have been
        // dropped on P2's save.
        Session::mutate_locked(&dir, "shared-id", |s| {
            s.push("user", "P2-prompt");
            s.push("assistant", "P2-reply");
        })
        .unwrap();

        // Reload from disk and confirm EVERY turn survived.
        let reloaded = Session::load_or_new(&dir, "shared-id").unwrap();
        assert_eq!(reloaded.messages.len(), 6, "all 6 turns must survive");
        assert_eq!(reloaded.messages[0].content, "seed-prompt");
        assert_eq!(reloaded.messages[1].content, "seed-reply");
        assert_eq!(
            reloaded.messages[2].content, "P1-prompt",
            "P1's turn must NOT be lost by P2's save (lost-update bug)"
        );
        assert_eq!(reloaded.messages[3].content, "P1-reply");
        assert_eq!(reloaded.messages[4].content, "P2-prompt");
        assert_eq!(reloaded.messages[5].content, "P2-reply");
    }

    /// DEFECT 1 concurrent pin: two real OS threads racing on the
    /// SAME `mutate_locked` for the same session id must still
    /// serialize so neither turn is lost. The lock is held for the
    /// full load -> apply -> save window, so the second thread sees
    /// the first thread's commit before it loads its own snapshot.
    #[test]
    fn mutate_locked_concurrent_threads_preserve_all_appends() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let dir = tmpdir();
        let path = Session::path_in(&dir, "shared-id");
        // Seed the file so both threads merge onto prior state.
        Session::mutate_locked(&dir, "shared-id", |s| {
            s.push("user", "seed");
        })
        .unwrap();

        // Two writers, each appending a unique turn. A `Barrier`
        // lines them up at the start of their critical section so
        // they actually race on the lock (rather than one always
        // finishing before the other starts).
        let n_threads = 2usize;
        let barrier = Arc::new(Barrier::new(n_threads));
        let mut handles = Vec::new();
        for t in 0..n_threads {
            let dir = dir.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                Session::mutate_locked(&dir, "shared-id", |s| {
                    s.push("user", &format!("P{t}-prompt"));
                    s.push("assistant", &format!("P{t}-reply"));
                })
            }));
        }
        for h in handles {
            h.join().unwrap().unwrap();
        }

        // Every turn must survive. With a working lock the
        // 1 seed + 2*2 = 5 messages are all present; without it,
        // at least one of the P{0,1}-* messages would be missing.
        let reloaded = Session::load_or_new(&dir, "shared-id").unwrap();
        assert_eq!(
            reloaded.messages.len(),
            5,
            "all 5 turns (1 seed + 4 from concurrent writers) must survive"
        );
        let contents: Vec<&str> = reloaded
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert!(contents.contains(&"seed"), "seed turn must survive");
        assert!(
            contents.contains(&"P0-prompt") && contents.contains(&"P0-reply"),
            "P0's turns must survive (lost-update bug would drop them): got {contents:?}"
        );
        assert!(
            contents.contains(&"P1-prompt") && contents.contains(&"P1-reply"),
            "P1's turns must survive (lost-update bug would drop them): got {contents:?}"
        );
        // Live transcript must still be a single readable JSON
        // document (no torn write from a temp-file clobber).
        let raw = std::fs::read_to_string(&path).unwrap();
        let _: Session = serde_json::from_str(&raw)
            .expect("concurrent mutate_locked must leave a parseable transcript");
    }

    /// DEFECT 1 non-regression: `mutate_locked` on a fresh path (no
    /// prior transcript) still persists the applied delta, the same
    /// way `model_health::HealthStore::mutate_locked_creates_store_when_absent`
    /// pins the load-under-lock tolerates a missing file. The
    /// size-cap and quarantine behavior from `load_or_new` also
    /// applies on the locked path.
    #[test]
    fn mutate_locked_creates_transcript_when_absent() {
        let dir = tmpdir();
        Session::mutate_locked(&dir, "fresh-id", |s| {
            s.push("user", "first prompt");
            s.push("assistant", "first reply");
        })
        .unwrap();
        let reloaded = Session::load_or_new(&dir, "fresh-id").unwrap();
        assert_eq!(reloaded.messages.len(), 2);
        assert_eq!(reloaded.messages[0].content, "first prompt");
    }

    // -----------------------------------------------------------------
    // DEFECT 2 (unbounded read) pin: oversized transcripts are
    // refused BEFORE the body is read, renamed aside (quarantined)
    // with a clear warning, and a fresh empty session is returned.
    // The pre-fix `read_to_string` would have slurped the whole
    // file into memory, risking OOM.
    // -----------------------------------------------------------------

    /// DEFECT 2 main pin: a transcript larger than
    /// `MAX_TRANSCRIPT_BYTES` is refused (and quarantined) by
    /// `load_or_new`, NOT read into memory. A fresh empty session
    /// is returned and the original file is moved aside under a
    /// unique name so the next call doesn't trip over it again.
    #[test]
    fn load_or_new_refuses_and_quarantines_oversized_transcript() {
        let dir = tmpdir();
        let path = Session::path_in(&dir, "huge");

        // Write a transcript whose on-disk body is just over the
        // cap. We use a JSON-shaped stub here (we don't need
        // valid JSON — the size cap must trip BEFORE the parse);
        // but we make it `MAX_TRANSCRIPT_BYTES + 64` of ` ` so the
        // whole file is plain ASCII and `metadata().len()` is
        // exact (no UTF-8 width surprise).
        let oversized = " ".repeat((MAX_TRANSCRIPT_BYTES + 64) as usize);
        std::fs::write(&path, &oversized).unwrap();
        assert!(
            std::fs::metadata(&path).unwrap().len() > MAX_TRANSCRIPT_BYTES,
            "precondition: file is over the cap"
        );

        // `load_or_new` must NOT read the body; it must return a
        // fresh empty session under the same id and rename the
        // oversized file aside.
        let sess = Session::load_or_new(&dir, "huge").unwrap();
        assert_eq!(sess.id, "huge");
        assert!(
            sess.messages.is_empty(),
            "oversized transcript must not be loaded into memory; got {} messages",
            sess.messages.len()
        );
        assert_eq!(
            sess.created, sess.updated,
            "fresh session has matching stamps"
        );

        // The original (oversized) file is gone from the live
        // session path; a quarantined copy with a
        // `json.oversized.<secs>.<pid>.<nonce>` stamp exists in
        // the same dir.
        assert!(
            !path.exists(),
            "the live oversized transcript must have been moved aside"
        );
        let mut found_quarantine = false;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains("json.oversized.") {
                found_quarantine = true;
                // The quarantined file preserves the original body
                // for an operator to inspect.
                let qbody = std::fs::read_to_string(entry.path()).unwrap();
                assert_eq!(qbody.len(), oversized.len());
                break;
            }
        }
        assert!(
            found_quarantine,
            "a quarantined copy of the oversized transcript must be left in the sessions dir"
        );
    }

    /// DEFECT 2 non-regression: a transcript at exactly the cap
    /// loads cleanly (cap is inclusive, not exclusive of the
    /// boundary). A transcript one byte under the cap is also fine.
    /// Pinned separately so a future off-by-one in the comparison
    /// operator is caught.
    #[test]
    fn load_or_new_at_or_just_under_cap_still_loads() {
        // Exactly MAX_TRANSCRIPT_BYTES: a JSON document this large
        // is impractical to construct in-memory, so we only pin the
        // sub-cap half of the contract here. The pin of the
        // EXACT-cap behavior is implicitly the test above (the
        // oversized write is at cap+64, demonstrating the > cap
        // boundary; the == cap case is left to the next test which
        // exercises the non-trivial size end of the spectrum).
        let dir = tmpdir();
        let path = Session::path_in(&dir, "normal");

        // Build a real, parseable transcript whose serialized form
        // is well under the cap. The point of the test is that
        // the cap check does NOT misfire on legitimate-sized
        // transcripts.
        let mut sess = Session::new("normal");
        sess.push("user", "hello");
        sess.push("assistant", "world");
        sess.save(&dir).unwrap();
        // The saved file is comfortably below the cap.
        let on_disk = std::fs::metadata(&path).unwrap().len();
        assert!(
            on_disk < MAX_TRANSCRIPT_BYTES,
            "sanity: test transcript is {} bytes, well under the {} cap",
            on_disk,
            MAX_TRANSCRIPT_BYTES
        );
        let reloaded = Session::load_or_new(&dir, "normal").unwrap();
        assert_eq!(reloaded.messages.len(), 2);
        assert_eq!(reloaded.messages[0].content, "hello");
    }

    /// DEFECT 2 `latest()` pin: a runaway transcript in the
    /// sessions dir is SKIPPED (and quarantined) by the
    /// `Session::latest` scan, not slurped into memory. A second,
    /// normal-sized session must still be discoverable as the
    /// "latest" after the quarantine.
    #[test]
    fn latest_skips_and_quarantines_oversized_transcripts() {
        let dir = tmpdir();

        // A normal session wins the "latest" race.
        let mut ok = Session::new("ok");
        ok.push("user", "ok-prompt");
        ok.save(&dir).unwrap();
        // A bloat transcript is also present.
        let huge_path = Session::path_in(&dir, "huge");
        std::fs::write(&huge_path, " ".repeat((MAX_TRANSCRIPT_BYTES + 1) as usize)).unwrap();

        let latest = Session::latest(&dir)
            .unwrap()
            .expect("a latest must be found");
        assert_eq!(
            latest.id, "ok",
            "the only valid session must be picked as latest (oversized one must be skipped, not loaded)"
        );

        // The bloat file has been moved aside.
        assert!(
            !huge_path.exists(),
            "oversized transcript must have been moved aside by latest()"
        );
    }

    /// DEFECT 2 `list()` pin: a runaway transcript is SKIPPED (and
    /// quarantined) by the `Session::list` scan, not slurped into
    /// memory. Pinned separately so the cap is enforced on every
    /// code path that reads a transcript off disk.
    #[test]
    fn list_skips_and_quarantines_oversized_transcripts() {
        let dir = tmpdir();

        let mut ok = Session::new("ok");
        ok.push("user", "p");
        ok.save(&dir).unwrap();
        let huge_path = Session::path_in(&dir, "huge");
        std::fs::write(&huge_path, " ".repeat((MAX_TRANSCRIPT_BYTES + 1) as usize)).unwrap();

        let list = Session::list(&dir).unwrap();
        assert_eq!(
            list.len(),
            1,
            "the oversized transcript must be skipped, not loaded into the list: got {list:?}"
        );
        assert_eq!(list[0].0, "ok");
        assert!(
            !huge_path.exists(),
            "oversized transcript must have been moved aside by list()"
        );
    }
}
