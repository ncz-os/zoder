//! Persistent store for engine-side session ids that survive across
//! `zoder` invocations.
//!
//! ACP (and the zeroclaw daemon sitting behind it) accepts a known
//! `session_id` on `session/new` to "resume" an existing conversation
//! rather than spinning up a fresh session. The transport plumbing for
//! resume already exists on [`crate::AgentOptions::session_id`]; this
//! module is the persistence layer that feeds it back on the next run.
//!
//! ## Scope
//!
//! Records are keyed by **scope** = `<engine_kind>:<canonical-cwd>`.
//! Two scopes do not interfere:
//!
//!   * `goose:/home/me/repo-a` and `goose:/home/me/repo-b` are distinct
//!     (different repos don't share sessions).
//!   * `goose:/home/me/repo` and `zeroclaw:/home/me/repo` are distinct
//!     (the two engines have independent session state — even the
//!     `session/new` id format / namespace differs).
//!
//! Picking the smallest scope that avoids accidental cross-talk keeps
//! the on-disk format trivial and makes the records safe to inspect
//! with a JSON viewer. We deliberately do NOT key by routed `model`,
//! alias, or provider: switching the routed model mid-session is
//! normal (the operator may reroute the free tier at any time), and
//! the engine is the layer that knows whether a (cwd, engine, model)
//! triple still maps to a live session. The store only needs to bring
//! back the last id the engine issued for THIS scope; the engine
//! decides whether it can still use it.
//!
//! ## Staleness
//!
//! A record is considered "stale" — and therefore ignored at load time —
//! when ANY of the following holds:
//!
//!   1. `updated_unix` is older than [`DEFAULT_MAX_AGE_SECS`] (engines
//!      tend to evict old sessions server-side on a much shorter
//!      horizon; this is a defensive ceiling so a forgotten id from
//!      last year can't shadow a real session).
//!   2. The record's `scope` (engine+cwd) differs from the load-time
//!      scope (handled by HashMap keying — a different scope simply
//!      does not look up).
//!
//! When the engine ACTUALLY rejects a resume (a `session/new`
//! JSON-RPC error reply after the client sent a known `session_id`),
//! [`clear_record`] is invoked at the wire layer to drop that scope's
//! record immediately rather than wait for the freshness window to
//! age it out.
//!
//! ## Non-breaking
//!
//! Persistence is OFF unless the caller opts in via
//! [`crate::AgentOptions::persist_session_id`]. When OFF, `load` is
//! never called and `save` is a no-op, so existing callers see
//! byte-for-byte identical behavior — including first-run behavior
//! (no record yet) and on the current-runs path where every
//! invocation creates a fresh session.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default freshness window. Engines typically evict sessions server-side
/// on much shorter horizons (goose sessions are per-process; zeroclaw
/// sessions inherit from the daemon's own retention policy). 7 days is a
/// deliberately wide ceiling so a forgotten id from last week still works
/// if the engine still has the session, while a record from years ago is
/// ignored. Callers that need a stricter or looser window override via
/// [`StoreConfig::max_age_secs`].
pub const DEFAULT_MAX_AGE_SECS: i64 = 7 * 24 * 60 * 60;

/// One persisted engine-session record.
///
/// Wire shape (JSON):
/// ```json
/// {
///   "scope": "goose:/home/me/repo",
///   "session_id": "goose-abc-123",
///   "updated_unix": 1749340800
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineSessionRecord {
    /// `"<engine_kind>:<canonical-cwd>"`. Equal to the lookup key
    /// the store uses, so a record's scope is always self-consistent
    /// with the index entry that points to it.
    pub scope: String,
    /// The id the engine returned from its last `session/new` call in
    /// this scope.
    pub session_id: String,
    /// Unix seconds when this record was last written. Drives the
    /// freshness decision in [`StoreConfig::with_now`].
    pub updated_unix: i64,
}

/// Build the lookup key for a record. Canonicalizes the cwd (so a
/// relative-vs-absolute path doesn't accidentally create two scopes)
/// and prefixes the engine kind so two engines in the same cwd are
/// still distinct scopes.
pub fn make_scope(engine_kind: &str, cwd: &Path) -> String {
    let canonical = canonicalize_path(cwd);
    format!("{engine_kind}:{canonical}")
}

/// Best-effort canonicalization. Falls back to the lossy string
/// representation on canonicalize errors — a misplaced symlink
/// shouldn't break persistence, and the engine itself will reject a
/// stale id if it doesn't recognize the cwd.
fn canonicalize_path(p: &Path) -> String {
    match std::fs::canonicalize(p) {
        Ok(c) => c.to_string_lossy().into_owned(),
        Err(_) => p.to_string_lossy().into_owned(),
    }
}

/// Configuration knobs for the store. Defaults match the documented
/// constants so a caller that only sets `path` gets the expected
/// behavior. Kept as a builder (rather than positional args) so
/// adding a new knob later doesn't break call sites.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Where the JSON file lives. The parent directory is created on
    /// write if missing.
    pub path: PathBuf,
    /// Maximum age, in seconds, before a record is treated as stale.
    /// Records older than this are ignored at load time. The record
    /// itself is NOT deleted — a future write will overwrite it.
    pub max_age_secs: i64,
    /// Override the "now" reading (test seam). Production callers
    /// leave this `None` and the store reads the system clock.
    pub now: Option<i64>,
}

impl StoreConfig {
    /// New config with default freshness window. `path` is required.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            max_age_secs: DEFAULT_MAX_AGE_SECS,
            now: None,
        }
    }

    /// Override the freshness window (seconds). `0` disables the
    /// window — every record is considered fresh — which is useful
    /// for tests asserting round-trip behavior across simulated
    /// "old" timestamps.
    pub fn with_max_age_secs(mut self, secs: i64) -> Self {
        self.max_age_secs = secs;
        self
    }

    /// Override the clock reading (test seam). Production code never
    /// calls this; it exists so staleness tests can pin "now" to
    /// a deterministic value without `unsafe`/`setenv` tricks.
    pub fn with_now(mut self, now_unix: i64) -> Self {
        self.now = Some(now_unix);
        self
    }

    fn current_unix(&self) -> i64 {
        self.now.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        })
    }
}

/// Load + persist + clear operations on the engine-session store.
///
/// The store is intentionally tiny: a single JSON object whose values
/// are [`EngineSessionRecord`]s, keyed by scope. All IO is synchronous
/// — persistence happens once per turn and the file is small.
///
/// # Examples
///
/// ```
/// use std::path::PathBuf;
/// use acp_client::session_store::{EngineSessionStore, make_scope};
///
/// # fn run() -> anyhow::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let path = dir.path().join("engine_sessions.json");
/// let cfg = EngineSessionStore::config(&path);
///
/// // First run: nothing on disk yet.
/// assert!(EngineSessionStore::load(&cfg, &make_scope("goose", &PathBuf::from("/repo")))?.is_none());
///
/// // Save a session id and read it back.
/// EngineSessionStore::save(&cfg, "goose", &PathBuf::from("/repo"), "goose-abc-123")?;
/// let rec = EngineSessionStore::load(&cfg, &make_scope("goose", &PathBuf::from("/repo")))?
///     .expect("record should be present after save");
/// assert_eq!(rec.session_id, "goose-abc-123");
/// # Ok(())
/// # }
/// ```
pub struct EngineSessionStore;

impl EngineSessionStore {
    /// Build a [`StoreConfig`] with the default freshness window for
    /// the given file path. The path is the JSON file the store
    /// reads + writes; the parent directory is created on the first
    /// save.
    pub fn config(path: impl Into<PathBuf>) -> StoreConfig {
        StoreConfig::new(path)
    }

    /// Load the record for `scope`, if it exists AND is fresh.
    ///
    /// Returns `None` when:
    ///   * the file does not exist (first run / no prior record),
    ///   * the scope is not present in the file,
    ///   * the record is older than [`StoreConfig::max_age_secs`],
    ///   * the file is corrupt (silently treated as absent — a future
    ///     `save` overwrites the bad bytes, so refusing to load here
    ///     is the safe choice).
    pub fn load(cfg: &StoreConfig, scope: &str) -> Result<Option<EngineSessionRecord>> {
        let raw = match std::fs::read_to_string(&cfg.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("reading engine-session store {}", cfg.path.display())
                })
            }
        };
        let map: BTreeMap<String, EngineSessionRecord> = match serde_json::from_str(&raw) {
            Ok(m) => m,
            Err(_) => return Ok(None),
        };
        let Some(rec) = map.get(scope) else {
            return Ok(None);
        };
        // Belt-and-braces: a record's `scope` field is always equal to
        // its index key in the JSON we serialize. If they drift (a
        // hand-edited file), trust the index key — the record's own
        // scope string is just metadata for debuggability.
        let now = cfg.current_unix();
        let age = now.saturating_sub(rec.updated_unix);
        if cfg.max_age_secs > 0 && age > cfg.max_age_secs {
            return Ok(None);
        }
        Ok(Some(rec.clone()))
    }

    /// Persist `session_id` for the scope identified by `(engine_kind, cwd)`.
    ///
    /// Overwrites any existing record for the same scope. Atomic write
    /// (temp file + rename) so a crash mid-write cannot leave a half-
    /// written JSON object on disk and brick the next load.
    pub fn save(cfg: &StoreConfig, engine_kind: &str, cwd: &Path, session_id: &str) -> Result<()> {
        let scope = make_scope(engine_kind, cwd);
        Self::save_with_scope(cfg, &scope, session_id)
    }

    /// Same as [`Self::save`] but takes a pre-built scope string. Used
    /// by the wire-layer save-after path so the driver can persist
    /// using the same `<engine>:<canonical-cwd>` key it looked up at
    /// load time, without re-canonicalizing the path (which is a
    /// small but real IO call that the wire layer does not need to
    /// repeat).
    pub fn save_with_scope(cfg: &StoreConfig, scope: &str, session_id: &str) -> Result<()> {
        // Read existing (may be absent / corrupt — both treated as
        // "empty"). This is the small write-around the atomicity
        // story: the read+merge is not atomic against concurrent
        // writers, but zoder runs ONE turn at a time per CLI
        // invocation so contention is not a real concern.
        let mut map: BTreeMap<String, EngineSessionRecord> =
            match std::fs::read_to_string(&cfg.path) {
                Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
                Err(_) => BTreeMap::new(),
            };
        map.insert(
            scope.to_string(),
            EngineSessionRecord {
                scope: scope.to_string(),
                session_id: session_id.to_string(),
                updated_unix: cfg.current_unix(),
            },
        );
        write_atomically(&cfg.path, &map)
    }

    /// Drop the record for `scope` (no-op if absent).
    ///
    /// Called by the wire layer when the engine rejects a resume:
    /// the persisted id is by definition stale at that point, and
    /// we want the next run to come back with a fresh id rather
    /// than race against the freshness window.
    pub fn clear(cfg: &StoreConfig, scope: &str) -> Result<()> {
        let Ok(raw) = std::fs::read_to_string(&cfg.path) else {
            return Ok(());
        };
        let Ok(mut map) = serde_json::from_str::<BTreeMap<String, EngineSessionRecord>>(&raw)
        else {
            return Ok(());
        };
        if map.remove(scope).is_none() {
            return Ok(());
        }
        write_atomically(&cfg.path, &map)
    }
}

/// Atomic write: serialize `map` to JSON, write to a sibling temp
/// file, rename into place. Keeps the file's predecessor intact if
/// the write fails partway through.
fn write_atomically(path: &Path, map: &BTreeMap<String, EngineSessionRecord>) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "creating parent dir for engine-session store {}",
                    path.display()
                )
            })?;
        }
    }
    let body = serde_json::to_vec_pretty(map).context("serializing engine-session store")?;
    // `.tmp` sibling: same directory so the rename is on the same
    // filesystem (cross-filesystem rename is not atomic on every
    // platform and would defeat the safety guarantee above).
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = std::fs::File::create(&tmp).with_context(|| {
            format!(
                "creating temp file for engine-session store {}",
                tmp.display()
            )
        })?;
        f.write_all(&body)?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path).with_context(|| {
        format!(
            "committing engine-session store {} (rename from {})",
            path.display(),
            tmp.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh scope key for `/repo` and the `goose` engine.
    fn scope() -> String {
        make_scope("goose", &PathBuf::from("/tmp/acp-store-test/repo"))
    }

    #[test]
    fn make_scope_includes_engine_and_cwd() {
        // Two scopes for the same cwd under different engines must
        // not collide (this is the regression guard for the
        // "different engines have independent session state"
        // invariant documented at the module level).
        let a = make_scope("goose", &PathBuf::from("/tmp/acp-store-test/repo"));
        let b = make_scope("zeroclaw", &PathBuf::from("/tmp/acp-store-test/repo"));
        assert_ne!(a, b);
        assert!(a.starts_with("goose:"));
        assert!(b.starts_with("zeroclaw:"));
    }

    #[test]
    fn load_returns_none_when_file_absent() {
        // First-run case: no file exists yet → load returns None
        // (NOT an error). This is the basis for the non-breaking
        // guarantee on the "first run after enabling persistence"
        // path.
        let dir = tempfile::tempdir().unwrap();
        let cfg = StoreConfig::new(dir.path().join("engine_sessions.json"));
        let got = EngineSessionStore::load(&cfg, &scope()).unwrap();
        assert!(got.is_none(), "missing file must look like 'no record'");
    }

    #[test]
    fn save_then_load_round_trips_session_id_and_scope() {
        // Round-trip regression guard: the fields that matter
        // (session_id, scope key, freshness-timestamp) all survive
        // the JSON round trip.
        let dir = tempfile::tempdir().unwrap();
        let cfg = StoreConfig::new(dir.path().join("engine_sessions.json"));
        EngineSessionStore::save(
            &cfg,
            "goose",
            &PathBuf::from("/tmp/acp-store-test/repo"),
            "goose-abc-123",
        )
        .unwrap();
        let rec = EngineSessionStore::load(&cfg, &scope())
            .unwrap()
            .expect("record must round-trip");
        assert_eq!(rec.session_id, "goose-abc-123");
        assert_eq!(rec.scope, scope());
        // updated_unix is within a few seconds of "now" — exact
        // value depends on the system clock but must be > 0.
        assert!(rec.updated_unix > 0);
    }

    #[test]
    fn load_treats_old_records_as_stale() {
        // Build the file with `updated_unix` pinned to long ago,
        // then load with a clock reading pinned to "now". The
        // freshness window must report the record as absent so a
        // stale id never shadows a fresh session/create.
        let dir = tempfile::tempdir().unwrap();
        let cfg = StoreConfig::new(dir.path().join("engine_sessions.json"));
        EngineSessionStore::save(
            &cfg,
            "goose",
            &PathBuf::from("/tmp/acp-store-test/repo"),
            "goose-old",
        )
        .unwrap();
        // Pin "now" 30 days ahead. The 7-day default window must
        // age the record out.
        let future_cfg = StoreConfig::new(cfg.path.clone())
            .with_max_age_secs(DEFAULT_MAX_AGE_SECS)
            .with_now(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64
                    + 30 * 24 * 60 * 60,
            );
        let got = EngineSessionStore::load(&future_cfg, &scope()).unwrap();
        assert!(
            got.is_none(),
            "a 30-day-old record must look stale under the 7-day default; got={got:?}"
        );
    }

    #[test]
    fn clear_drops_only_target_scope() {
        // Two scopes in the same file: clearing one must leave the
        // other intact. Without this guarantee, dropping a stale
        // goose session could evict a live zeroclaw session — both
        // unrelated to the engine that rejected the resume.
        let dir = tempfile::tempdir().unwrap();
        let cfg = StoreConfig::new(dir.path().join("engine_sessions.json"));
        EngineSessionStore::save(
            &cfg,
            "goose",
            &PathBuf::from("/tmp/acp-store-test/repo"),
            "goose-abc",
        )
        .unwrap();
        EngineSessionStore::save(
            &cfg,
            "zeroclaw",
            &PathBuf::from("/tmp/acp-store-test/repo"),
            "zeroclaw-xyz",
        )
        .unwrap();
        let goose_scope = make_scope("goose", &PathBuf::from("/tmp/acp-store-test/repo"));
        EngineSessionStore::clear(&cfg, &goose_scope).unwrap();
        assert!(EngineSessionStore::load(&cfg, &goose_scope)
            .unwrap()
            .is_none());
        let z_rec = EngineSessionStore::load(
            &cfg,
            &make_scope("zeroclaw", &PathBuf::from("/tmp/acp-store-test/repo")),
        )
        .unwrap()
        .expect("zeroclaw record must survive clear of the goose scope");
        assert_eq!(z_rec.session_id, "zeroclaw-xyz");
    }

    #[test]
    fn load_treats_corrupt_file_as_absent() {
        // A future corrupt write (or a hand-edit gone wrong) must
        // not block the next load — we treat a parse failure as
        // "no record" so a stale id never shadows a fresh
        // session/create. The next `save` overwrites the bad
        // bytes with valid JSON.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine_sessions.json");
        std::fs::write(&path, b"{not valid json").unwrap();
        let cfg = StoreConfig::new(path);
        let got = EngineSessionStore::load(&cfg, &scope()).unwrap();
        assert!(got.is_none(), "corrupt file must look like 'no record'");
    }

    #[test]
    fn save_creates_missing_parent_dirs() {
        // The store can be called before any other zoder subsystem
        // has touched `~/.zoder/sessions/`. The parent directory
        // must be created on the first save so this layer never
        // races on filesystem setup.
        let dir = tempfile::tempdir().unwrap();
        let nested = dir
            .path()
            .join("a")
            .join("b")
            .join("c")
            .join("engine_sessions.json");
        let cfg = StoreConfig::new(&nested);
        EngineSessionStore::save(
            &cfg,
            "goose",
            &PathBuf::from("/tmp/acp-store-test/repo"),
            "g1",
        )
        .unwrap();
        assert!(nested.exists());
    }
}
