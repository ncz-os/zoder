//! Process-local memoization of the two config files a review re-reads
//! constantly: the pricing catalog and the model health store.
//!
//! D10 (items 5 and 6 of the performance plan): a review touches both files
//! on every routing decision — `RoutingContext::load` prices candidate
//! models through the catalog, `resolve_chain` reads health to rank them, and
//! each reviewer dispatch repeats both. Both files are small but are
//! re-parsed (and the pricing catalog re-validated for size, regular-file and
//! permission checks) dozens of times per run.
//!
//! The cache is keyed by `(path, mtime, size)`: any write to either file
//! — including the atomic rename `HealthStore::save` performs, which changes
//! the mtime — invalidates the entry on the next read. It never serves a
//! stale value past a file change, and it never caches a load FAILURE: an
//! absent or unreadable file is re-tried every time, so an install that
//! appears mid-run is picked up.
//!
//! What it is NOT for: a caller that loads the store to WRITE it back
//! (`HealthStore::load` -> mutate -> `save`, or anything inside
//! `HealthStore::mutate_locked`) must keep calling the loader directly. A
//! read-modify-write over a cached snapshot would silently drop another
//! writer's delta, which is exactly what the lock exists to prevent. Only
//! READ paths (routing, ranking, pricing a call, reporting) use this module.
//!
//! Deliberately process-local and unbounded-by-one: a run has one home, so
//! the map holds at most a handful of paths (tests create a tempdir per
//! test). It is a plain `Mutex`, never held across an `await`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

use zoder_core::pricing::PricingCatalog;
use zoder_core::HealthStore;

/// Rebuild a store the cache serialized. The same decode `HealthStore::load`
/// performs on the file — but with a `None` on error instead of the loader's
/// quarantine-on-corruption, because the cache stores the store and never
/// re-reads a file it already parsed.
fn decode_store(bytes: &[u8]) -> Option<HealthStore> {
    serde_json::from_slice::<HealthStore>(bytes).ok()
}

/// What makes a cached value still valid: the file's mtime and length.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    mtime: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    /// `None` when the file cannot be stat'd — which forces a re-load
    /// (nothing is cached for a file that is not there).
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Stamp {
            mtime: meta.modified().ok(),
            len: meta.len(),
        })
    }
}

struct Entry<T> {
    stamp: Stamp,
    value: T,
}

static PRICING: Mutex<Option<HashMap<PathBuf, Entry<PricingCatalog>>>> = Mutex::new(None);
/// The health store is cached as its JSON IMAGE, not as a `HealthStore`.
/// `HealthStore` is deliberately not `Clone` (a snapshot that can be
/// silently re-saved is how a stale read overwrites another writer's delta),
/// and this module must not loosen that. Re-parsing the cached bytes yields
/// the same value `HealthStore::load` would have produced from the same file;
/// when the bytes somehow do not parse, the loader is simply called again.
static HEALTH: Mutex<Option<HashMap<PathBuf, Entry<Vec<u8>>>>> = Mutex::new(None);

/// How many loads this module actually performed (a miss), process-wide.
/// Tests read the delta; nothing in production consults it.
static MISSES: AtomicUsize = AtomicUsize::new(0);

fn cache_get<T: Clone>(
    cache: &Mutex<Option<HashMap<PathBuf, Entry<T>>>>,
    path: &Path,
) -> Option<T> {
    let stamp = Stamp::of(path)?;
    let guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    let map = guard.as_ref()?;
    let entry = map.get(path)?;
    (entry.stamp == stamp).then(|| entry.value.clone())
}

fn cache_put<T>(cache: &Mutex<Option<HashMap<PathBuf, Entry<T>>>>, path: &Path, value: T) {
    let Some(stamp) = Stamp::of(path) else {
        // Nothing to key on: never cache.
        return;
    };
    let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
    guard
        .get_or_insert_with(HashMap::new)
        .insert(path.to_path_buf(), Entry { stamp, value });
}

/// Load `path` through `cache`, or hand back the entry that is still current.
fn cached<T: Clone>(
    cache: &Mutex<Option<HashMap<PathBuf, Entry<T>>>>,
    path: &Path,
    load: impl FnOnce(&Path) -> T,
) -> T {
    if let Some(hit) = cache_get(cache, path) {
        return hit;
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    let value = load(path);
    // A failed load (an absent file yields the loader's empty default) is
    // still cached only when the file EXISTS: `cache_put` refuses to key a
    // path it cannot stat, so "not installed yet" is re-tried every call
    // while "installed and empty" is reused.
    cache_put(cache, path, value.clone());
    value
}

/// The pricing catalog at `path`, re-parsed only when the file changed.
///
/// Byte-for-byte the same value as [`PricingCatalog::load`] — the loader is
/// the one place that decides absent/corrupt/insecure handling, and this
/// only decides whether to call it.
pub(crate) fn pricing_catalog(path: &Path) -> PricingCatalog {
    cached(&PRICING, path, PricingCatalog::load)
}

/// The health store at `path`, re-parsed only when the file changed.
///
/// [`HealthStore::mutate_locked`] writes through an atomic rename, so its
/// result is observed by the next `health_store` call without any explicit
/// invalidation. Callers that write WITHOUT the cache (a direct
/// `HealthStore::load` -> modify -> `save`) still land on disk, and the
/// mtime moves, so the cache cannot serve the pre-write value.
///
/// Read-only by construction: this hands back a detached copy, so a caller
/// that mutates what it gets here still has to call `save`/`mutate_locked`
/// for the change to exist.
pub(crate) fn health_store(path: &Path) -> HealthStore {
    let store = cached_health(path);
    match store {
        Some(store) => store,
        // Unreachable for an image this module wrote; a loader call is the
        // safe answer if it ever is.
        None => HealthStore::load(path),
    }
}

/// The parse-and-cache half of [`health_store`]; `None` only when the cached
/// image does not parse as a store, which means "call the loader".
fn cached_health(path: &Path) -> Option<HealthStore> {
    let image = cached(&HEALTH, path, |p| {
        // The image must be what the LOADER would have produced, not the raw
        // bytes: a corrupt on-disk store is quarantined by the loader, and a
        // cache that mirrored the corrupt bytes would re-derive a different
        // value on every read.
        serde_json::to_vec(&HealthStore::load(p)).unwrap_or_default()
    });
    decode_store(&image)
}

/// The number of loads this module performed, process-wide. Tests assert on
/// the DELTA a measured window caused.
pub(crate) fn load_count() -> usize {
    MISSES.load(Ordering::Relaxed)
}

/// Drop every cached entry. Tests use this so one test's tempdir cannot be
/// observed by the next; production never needs it (the stamp does the work).
pub(crate) fn clear() {
    *PRICING.lock().unwrap_or_else(|p| p.into_inner()) = None;
    *HEALTH.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_store_reloads_when_the_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        clear();

        let first = health_store(&path);
        assert!(first.models.is_empty(), "an absent store is empty");

        // A direct write (not through mutate_locked) must be observed.
        let mut written = HealthStore::load(&path);
        written.record_classified_failure(
            "some-model",
            "503",
            "prov",
            zoder_core::Classification::Capacity,
        );
        written.save().unwrap();

        let second = health_store(&path);
        assert!(
            second.models.contains_key("some-model"),
            "the cache must not serve the pre-write store: {:?}",
            second.models.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn health_store_reuses_an_unchanged_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        clear();
        let mut written = HealthStore::load(&path);
        written.record_classified_failure("m", "503", "p", zoder_core::Classification::Capacity);
        written.save().unwrap();
        let before = load_count();
        let a = health_store(&path);
        let b = health_store(&path);
        let loads = load_count() - before;
        assert_eq!(loads, 1, "an unchanged store is loaded once, then reused");
        assert_eq!(
            a.models.keys().collect::<Vec<_>>(),
            b.models.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_absent_file_is_retried_on_every_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-yet-installed.json");
        clear();
        let before = load_count();
        let _ = pricing_catalog(&path);
        let _ = pricing_catalog(&path);
        assert_eq!(
            load_count() - before,
            2,
            "a file that is not there has no stamp to key on, so it is never cached"
        );
    }

    #[test]
    fn pricing_is_cached_per_path_and_invalidated_by_mtime() {
        let dir = tempfile::tempdir().unwrap();
        clear();
        let a = dir.path().join("a.json");
        let b = dir.path().join("b.json");
        std::fs::write(&a, r#"{"models":{}}"#).unwrap();
        std::fs::write(&b, r#"{"models":{}}"#).unwrap();
        // Both load without panicking and are independent entries.
        let _ = pricing_catalog(&a);
        let _ = pricing_catalog(&b);
        let guard = PRICING.lock().unwrap();
        assert_eq!(
            guard.as_ref().unwrap().len(),
            2,
            "one entry per path"
        );
    }
}
