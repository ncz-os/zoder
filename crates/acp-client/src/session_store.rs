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
//! Stale records are ALSO physically evicted: every [`EngineSessionStore::save`]
//! and explicit [`EngineSessionStore::prune`] drops records past the
//! freshness window from disk, so the store doesn't accumulate dead
//! ids across many invocations. The wire layer's
//! [`EngineSessionStore::clear`] still wins on a per-scope basis when
//! the engine ACTUALLY rejects a resume (a `session/new` JSON-RPC
//! error reply after the client sent a known `session_id`) — that
//! path drops the scope's record immediately rather than waiting for
//! the next save to evict it.
//!
//! ## Cap
//!
//! The store also enforces a maximum number of records (default
//! [`DEFAULT_MAX_ENTRIES`] = 128) so a proliferation of distinct scopes
//! can't grow the on-disk file without bound. When the cap is
//! exceeded, the OLDEST records (by `updated_unix`) are evicted first
//! — newest survives. The cap is deliberately high enough that
//! normal use (a handful of repos per engine) is unaffected; only
//! pathological scope churn trips it.
//!
//! ## Non-breaking
//!
//! Persistence is OFF unless the caller opts in via
//! [`crate::AgentOptions::persist_session_id`]. When OFF, `load` is
//! never called and `save` is a no-op, so existing callers see
//! byte-for-byte identical behavior — including first-run behavior
//! (no record yet) and on the current-runs path where every
//! invocation creates a fresh session. For fresh, in-cap records the
//! on-disk JSON is also byte-for-byte identical to the pre-cap
//! store: the `BTreeMap` ordering and `serde_json::to_vec_pretty`
//! formatting are deterministic, so re-saving the same data produces
//! the same bytes.

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

/// Default cap on the number of records persisted in a single store
/// file. The cap is the upper bound on the number of distinct
/// `<engine_kind, canonical-cwd>` scopes tracked simultaneously — it
/// has no effect on a single scope's round-trip behavior, and is only
/// triggered when the on-disk record set grows past it. 128 is well
/// above what normal use (a handful of repos) ever produces, so the
/// cap is a safety belt against pathological scopes (`/tmp/foo1`,
/// `/tmp/foo2`, …) rather than a knob operators normally need to
/// touch. Callers that need a different bound override via
/// [`StoreConfig::with_max_entries`].
pub const DEFAULT_MAX_ENTRIES: usize = 128;

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
    /// Records older than this are ignored at load time AND physically
    /// evicted from disk on the next save / prune — see the
    /// "Staleness" section of the module-level docs for the
    /// rationale.
    pub max_age_secs: i64,
    /// Maximum number of records the store is allowed to keep on
    /// disk. Once exceeded, the OLDEST records (by `updated_unix`)
    /// are evicted first. Set to `usize::MAX` to effectively disable
    /// the cap; see [`StoreConfig::with_max_entries`] for details.
    pub max_entries: usize,
    /// Override the "now" reading (test seam). Production callers
    /// leave this `None` and the store reads the system clock.
    pub now: Option<i64>,
}

impl StoreConfig {
    /// New config with default freshness window and default entry
    /// cap. `path` is required.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            max_age_secs: DEFAULT_MAX_AGE_SECS,
            max_entries: DEFAULT_MAX_ENTRIES,
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

    /// Override the per-store record cap. Once the on-disk map holds
    /// more than `n` records, the OLDEST (by `updated_unix`) are
    /// evicted until the count is back at or below `n`. The default
    /// ([`DEFAULT_MAX_ENTRIES`] = 128) is deliberately high enough
    /// that normal use (a handful of repos per engine) never trips
    /// it. Pass [`usize::MAX`] to effectively disable the cap.
    pub fn with_max_entries(mut self, n: usize) -> Self {
        self.max_entries = n;
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
        // Prune-on-save: drop stale records (older than the
        // freshness window) and trim to the cap. Cheap when the map
        // is small (the common case); saves a separate `prune()`
        // round-trip from the caller. The insert above is the
        // freshest entry, so it always survives the cap eviction.
        evict_in_place(&mut map, cfg);
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

    /// Apply the eviction policies in-place to the on-disk store.
    ///
    /// Two policies are applied, in order:
    ///
    ///   1. **Drop stale.** Records whose `updated_unix` is older
    ///      than [`StoreConfig::max_age_secs`] (with `max_age_secs > 0`)
    ///      are removed. This mirrors the staleness check
    ///      [`Self::load`] already does at read time, but physically
    ///      removes the bytes from disk instead of leaving dead
    ///      records to age out across many invocations.
    ///   2. **Enforce cap.** If the resulting record count is still
    ///      above [`StoreConfig::max_entries`], the OLDEST records
    ///      (by `updated_unix`) are evicted until the count is back
    ///      at or below the cap.
    ///
    /// Returns the number of records physically evicted (stale +
    /// over-cap). Returns `0` when the file is absent, corrupt
    /// (treated as empty), or already satisfies both policies —
    /// `prune` is idempotent and side-effect-free in those cases.
    ///
    /// Callers that want explicit cleanup — e.g. a future CLI flag
    /// to compact the on-disk store — invoke this; the save path
    /// already calls it implicitly so a separate invocation is not
    /// needed for normal operation.
    pub fn prune(cfg: &StoreConfig) -> Result<usize> {
        // Absent file: nothing to prune. Same for a corrupt file
        // (treated as "empty", matching how `save`/`clear` handle
        // a missing or unparseable file).
        let Ok(raw) = std::fs::read_to_string(&cfg.path) else {
            return Ok(0);
        };
        let mut map: BTreeMap<String, EngineSessionRecord> =
            serde_json::from_str(&raw).unwrap_or_default();
        let evicted = evict_in_place(&mut map, cfg);
        if evicted > 0 {
            // Rewrite so the on-disk file reflects the in-memory
            // map. `evicted == 0` means no work happened; skip the
            // atomic-write dance to keep the no-op path truly
            // side-effect-free (mtime preserved, no tmp file
            // flicker).
            write_atomically(&cfg.path, &map)?;
        }
        Ok(evicted)
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

/// Apply the store's two eviction policies (drop-stale + enforce-
/// cap) to `map` in place. Returns the number of records removed.
///
/// This helper is the single source of truth for what "evict" means
/// across the public surface: both [`EngineSessionStore::save`] (via
/// the prune-on-save path) and [`EngineSessionStore::prune`] (via the
/// explicit cleanup path) call it. Keeping the policy in one place
/// means a future addition — e.g. "drop records whose cwd no longer
/// exists" — only has to land in one spot.
fn evict_in_place(map: &mut BTreeMap<String, EngineSessionRecord>, cfg: &StoreConfig) -> usize {
    let before = map.len();
    if before == 0 {
        return 0;
    }
    // 1. Drop stale. `max_age_secs == 0` disables the window (used
    // by tests that want full round-trip fidelity); in that case the
    // age check is skipped wholesale so the helper's behavior
    // matches `EngineSessionStore::load`.
    if cfg.max_age_secs > 0 {
        let now = cfg.current_unix();
        map.retain(|_, rec| {
            let age = now.saturating_sub(rec.updated_unix);
            age <= cfg.max_age_secs
        });
    }
    // 2. Enforce cap. `max_entries == usize::MAX` (or otherwise
    // astronomically large) is the documented "off" setting; the
    // arithmetic below is still safe (no overflow in practice for
    // reasonable maps, and `len()` is at most `usize::MAX` already).
    let over_by = map.len().saturating_sub(cfg.max_entries);
    if over_by == 0 {
        return before - map.len();
    }
    // Collect (scope, updated_unix) pairs, sort ascending by
    // timestamp (oldest first), then drop the oldest `over_by`.
    // Ties (identical `updated_unix`) are broken by scope string so
    // eviction is fully deterministic across runs — important for
    // the "byte-for-byte unchanged for fresh in-cap records"
    // property: a deterministic eviction order means the surviving
    // set is independent of any incidental reordering. Keys are
    // cloned (cheap — short scope strings) so the immutable borrow
    // ends before we start mutating with `map.remove`.
    let mut by_age: Vec<(String, i64)> = map
        .iter()
        .map(|(k, v)| (k.clone(), v.updated_unix))
        .collect();
    by_age.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    for (key, _) in by_age.into_iter().take(over_by) {
        map.remove(key.as_str());
    }
    before - map.len()
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

    // ---- EVICTION TESTS (stale + cap) ----------------------------
    //
    // These mirror the style of the load/clear tests above: build a
    // known file with a pinned clock, drive the API, then read back
    // via either `EngineSessionStore::load` or a raw JSON parse
    // (when the test needs to assert physical eviction across all
    // scopes, not just the one being looked up). The `with_now`
    // seam lets every test pin "now" deterministically so the
    // freshness-window math is exact, not "close enough".

    /// Helper: serialize the on-disk map back to a `BTreeMap` so a
    /// test can assert that an eviction physically removed a
    /// record (not just hidden it from `load`). The file is the
    /// source of truth — what survives on disk is what survives
    /// across runs.
    fn read_all_records(path: &std::path::Path) -> BTreeMap<String, EngineSessionRecord> {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => BTreeMap::new(),
        }
    }

    #[test]
    fn save_evicts_records_older_than_freshness_window() {
        // Prune-on-save: a stale record (older than the freshness
        // window at save time) must be physically removed from the
        // file on the next save, not just hidden from `load`. The
        // record existed on disk before the save and must be gone
        // after — that's the contract the eviction layer adds on
        // top of the existing load-time staleness check.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine_sessions.json");
        let base_secs: i64 = 1_700_000_000;

        // Seed the file with a record whose `updated_unix` is
        // firmly past the freshness window: 30 days back from a
        // pinned "now".
        let seed_cfg = StoreConfig::new(&path).with_now(base_secs);
        EngineSessionStore::save(
            &seed_cfg,
            "goose",
            &PathBuf::from("/tmp/acp-store-test/repo"),
            "goose-old",
        )
        .unwrap();
        // Confirm the seed is on disk (sanity — anything else
        // making this test pass would be a false positive).
        assert_eq!(read_all_records(&path).len(), 1);

        // Now save with a clock reading pinned 30 days LATER. The
        // 7-day default window must age the old record out.
        let later_cfg = StoreConfig::new(&path).with_now(base_secs + 30 * 24 * 60 * 60);
        EngineSessionStore::save(
            &later_cfg,
            "goose",
            &PathBuf::from("/tmp/acp-store-test/another-repo"),
            "goose-fresh",
        )
        .unwrap();

        // The stale record must be physically gone from disk — not
        // just ignored at load time. The fresh record must
        // survive (it was just written and is the freshest entry).
        let after = read_all_records(&path);
        assert_eq!(
            after.len(),
            1,
            "the stale record must be physically evicted; after-save map = {after:?}"
        );
        assert!(
            after.values().all(|r| r.session_id == "goose-fresh"),
            "the surviving record must be the freshly-written one; after-save map = {after:?}"
        );
    }

    #[test]
    fn prune_evicts_records_older_than_freshness_window() {
        // Explicit `prune()` must also physically remove stale
        // records — same policy as the prune-on-save path, just
        // triggered on demand. Useful for a future CLI maintenance
        // command, and the count returned must reflect what was
        // actually dropped.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine_sessions.json");
        let base_secs: i64 = 1_700_000_000;

        // Seed two stale records (both 30 days past relative to
        // the post-prune "now").
        let seed_cfg = StoreConfig::new(&path).with_now(base_secs);
        EngineSessionStore::save(
            &seed_cfg,
            "goose",
            &PathBuf::from("/tmp/acp-store-test/repo-a"),
            "stale-a",
        )
        .unwrap();
        EngineSessionStore::save(
            &seed_cfg,
            "goose",
            &PathBuf::from("/tmp/acp-store-test/repo-b"),
            "stale-b",
        )
        .unwrap();
        assert_eq!(read_all_records(&path).len(), 2);

        // Run prune with a clock pinned 30 days forward; default
        // 7-day window must drop both records. Both policies are
        // applied here (stale + cap) but the cap is high enough
        // that only staleness drives the result.
        let prune_cfg = StoreConfig::new(&path).with_now(base_secs + 30 * 24 * 60 * 60);
        let evicted = EngineSessionStore::prune(&prune_cfg).unwrap();
        assert_eq!(
            evicted, 2,
            "prune must report both stale records as evicted; got={evicted}"
        );
        assert!(
            read_all_records(&path).is_empty(),
            "stale records must be physically removed from disk after prune()"
        );
    }

    #[test]
    fn save_enforces_cap_evicting_oldest_first() {
        // Cap enforcement: adding a 4th record to a file that
        // already holds 3 with `max_entries = 3` must drop the
        // OLDEST (by `updated_unix`), not the just-written newest.
        // The newest `cap` records must survive — i.e. the three
        // that were written most recently in clock order.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine_sessions.json");
        let base_secs: i64 = 1_700_000_000;

        // Three records spaced 1 second apart; first is oldest.
        for (i, secs) in [base_secs, base_secs + 1, base_secs + 2].iter().enumerate() {
            let cfg = StoreConfig::new(&path).with_now(*secs).with_max_entries(3);
            EngineSessionStore::save(
                &cfg,
                "goose",
                &PathBuf::from(format!("/tmp/acp-store-test/repo-{i}")),
                &format!("sid-{i}"),
            )
            .unwrap();
        }
        let before = read_all_records(&path);
        assert_eq!(before.len(), 3, "all three seeds should be present");
        // Sanity: the oldest record IS repo-0.
        assert!(
            before
                .values()
                .any(|r| r.session_id == "sid-0" && r.updated_unix == base_secs),
            "oldest seed must be repo-0/sid-0"
        );

        // Now write a 4th record at a later timestamp, still with
        // cap = 3. The oldest (sid-0) must be evicted; the three
        // newest survive.
        let cfg4 = StoreConfig::new(&path)
            .with_now(base_secs + 10)
            .with_max_entries(3);
        EngineSessionStore::save(
            &cfg4,
            "goose",
            &PathBuf::from("/tmp/acp-store-test/repo-3"),
            "sid-3",
        )
        .unwrap();

        let after = read_all_records(&path);
        assert_eq!(
            after.len(),
            3,
            "cap must be enforced; after-save map = {after:?}"
        );
        assert!(
            after.values().all(|r| r.session_id != "sid-0"),
            "the oldest record (sid-0) must be evicted; after-save map = {after:?}"
        );
        let survivors: std::collections::BTreeSet<&str> =
            after.values().map(|r| r.session_id.as_str()).collect();
        assert_eq!(
            survivors,
            ["sid-1", "sid-2", "sid-3"]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "the newest `cap` records must survive; got={survivors:?}"
        );
    }

    #[test]
    fn prune_returns_count_of_evicted_records() {
        // Cap-eviction count: when prune is driven over the cap
        // (not staleness), it must report the number it evicted.
        // This is the knob a future CLI will use to surface "n
        // stale records dropped" to the operator.
        //
        // Important: the prune-on-save path ALSO evicts, so we
        // must seed the file with a config that has a HIGH cap
        // (so saving 5 records doesn't trim anything) and only
        // apply the tight cap at prune time. Otherwise the
        // per-save evictions eat records before prune() runs and
        // the count is wrong.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine_sessions.json");
        let base_secs: i64 = 1_700_000_000;

        // Five records, spaced 1s apart. Seed with a very high cap
        // and a long freshness window so neither policy fires on
        // the save path — we want all 5 to land on disk untouched.
        for i in 0..5 {
            let cfg = StoreConfig::new(&path)
                .with_now(base_secs + i)
                .with_max_entries(usize::MAX)
                .with_max_age_secs(365 * 24 * 60 * 60);
            EngineSessionStore::save(
                &cfg,
                "goose",
                &PathBuf::from(format!("/tmp/acp-store-test/repo-{i}")),
                &format!("sid-{i}"),
            )
            .unwrap();
        }
        assert_eq!(read_all_records(&path).len(), 5);

        // Pin "now" far enough forward that nothing ages out
        // (365-day window above). Only the cap kicks in.
        let prune_cfg = StoreConfig::new(&path)
            .with_now(base_secs + 10)
            .with_max_entries(2)
            .with_max_age_secs(365 * 24 * 60 * 60);
        let evicted = EngineSessionStore::prune(&prune_cfg).unwrap();
        assert_eq!(
            evicted, 3,
            "5 records capped at 2 must evict 3; got={evicted}"
        );
        let after = read_all_records(&path);
        assert_eq!(after.len(), 2);
        let survivors: std::collections::BTreeSet<&str> =
            after.values().map(|r| r.session_id.as_str()).collect();
        // The 2 newest must be the survivors (sid-3, sid-4).
        assert_eq!(
            survivors,
            ["sid-3", "sid-4"]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "the newest `cap` records must survive cap-eviction; got={survivors:?}"
        );
    }

    #[test]
    fn fresh_in_cap_records_round_trip_byte_for_byte() {
        // Regression guard for the non-breaking promise at the
        // module level: when all records are fresh AND below the
        // cap, prune must be a no-op (returns 0) and the on-disk
        // contents must be identical before and after. This is the
        // property that lets a caller safely opt into the new
        // prune-on-save path without changing observable behavior
        // for normal use.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine_sessions.json");
        let cfg = StoreConfig::new(&path); // default freshness + default cap

        EngineSessionStore::save(
            &cfg,
            "goose",
            &PathBuf::from("/tmp/acp-store-test/repo"),
            "sid-1",
        )
        .unwrap();
        EngineSessionStore::save(
            &cfg,
            "zeroclaw",
            &PathBuf::from("/tmp/acp-store-test/repo"),
            "sid-2",
        )
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let evicted = EngineSessionStore::prune(&cfg).unwrap();
        assert_eq!(
            evicted, 0,
            "fresh in-cap records must not be evicted; got={evicted}"
        );
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "on-disk bytes must be unchanged when no record is evicted"
        );

        // And the records themselves must still load.
        let rec1 = EngineSessionStore::load(
            &cfg,
            &make_scope("goose", &PathBuf::from("/tmp/acp-store-test/repo")),
        )
        .unwrap()
        .expect("goose record must survive a no-op prune");
        assert_eq!(rec1.session_id, "sid-1");
        let rec2 = EngineSessionStore::load(
            &cfg,
            &make_scope("zeroclaw", &PathBuf::from("/tmp/acp-store-test/repo")),
        )
        .unwrap()
        .expect("zeroclaw record must survive a no-op prune");
        assert_eq!(rec2.session_id, "sid-2");
    }

    #[test]
    fn prune_on_empty_store_is_a_no_op() {
        // No file on disk yet: prune must return 0 and not create
        // the file (so a no-op prune stays truly side-effect-free
        // — no tmp-file flicker, no mtime change).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine_sessions.json");
        let cfg = StoreConfig::new(&path);
        let evicted = EngineSessionStore::prune(&cfg).unwrap();
        assert_eq!(evicted, 0);
        assert!(
            !path.exists(),
            "prune on an empty store must not create the file"
        );
    }

    #[test]
    fn prune_on_at_limit_store_is_a_no_op() {
        // File exists with exactly `cap` records, all fresh:
        // prune must return 0 and the file must be unchanged.
        // This is the boundary case for the "under cap → no-op"
        // regression guard above.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine_sessions.json");
        let cfg = StoreConfig::new(&path).with_max_entries(2);
        EngineSessionStore::save(
            &cfg,
            "goose",
            &PathBuf::from("/tmp/acp-store-test/repo-a"),
            "sid-a",
        )
        .unwrap();
        EngineSessionStore::save(
            &cfg,
            "goose",
            &PathBuf::from("/tmp/acp-store-test/repo-b"),
            "sid-b",
        )
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let evicted = EngineSessionStore::prune(&cfg).unwrap();
        assert_eq!(evicted, 0, "at-limit store must report 0 evictions");
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "at-limit store must not be rewritten by prune"
        );
    }

    #[test]
    fn save_does_not_evict_when_under_cap() {
        // Pair to `save_enforces_cap_evicting_oldest_first`:
        // a save that keeps the store under the cap must NOT
        // evict anything. This is the regression guard that keeps
        // the "default cap of 128 is invisible to normal use"
        // promise in the module docstring honest.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine_sessions.json");
        let cfg = StoreConfig::new(&path).with_max_entries(4);

        for i in 0..3 {
            EngineSessionStore::save(
                &cfg,
                "goose",
                &PathBuf::from(format!("/tmp/acp-store-test/repo-{i}")),
                &format!("sid-{i}"),
            )
            .unwrap();
        }
        let map = read_all_records(&path);
        assert_eq!(map.len(), 3, "all three records must be present");
        for i in 0..3 {
            assert!(
                map.values().any(|r| r.session_id == format!("sid-{i}")),
                "sid-{i} must be present after under-cap saves"
            );
        }
    }

    #[test]
    fn max_entries_default_matches_documented_constant() {
        // The documented `DEFAULT_MAX_ENTRIES` is the default
        // `max_entries` for `StoreConfig::new`. A drift between
        // the constant and the default would silently change the
        // "normal use is unaffected" property, so this is the
        // guard.
        let cfg = StoreConfig::new(std::path::PathBuf::from("/tmp/whatever"));
        assert_eq!(cfg.max_entries, DEFAULT_MAX_ENTRIES);
        assert_eq!(cfg.max_entries, 128);
    }
}
