//! Process-local memoization of the two config files a review re-reads
//! constantly: the pricing catalog and the model health store.
//!
//! D10 (items 5 and 6 of the performance plan): a review touches both files
//! on every routing decision — `RoutingContext::load` prices candidate
//! models, `resolve_chain` reads health to rank them, and each reviewer
//! dispatch repeats both. Both files are small but are re-parsed (and the
//! pricing catalog re-validated for size, regular-file and permission
//! checks) dozens of times per run.
//!
//! The cache is keyed by `(path, mtime, size)`: any write to either file
//! — including the atomic rename `HealthStore::save` performs, which changes
//! the mtime — invalidates the entry on the next read. It never serves a
//! stale value past a file change, and it never caches a load FAILURE: an
//! absent or unreadable file is re-tried every time, so an install that
//! appears mid-run is picked up.
//!
//! Deliberately process-local and unbounded-by-one: a run has one home, so
//! the map holds at most a handful of paths (tests create a tempdir per
//! test). It is a plain `Mutex`, never held across an `await`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use model_health::HealthStore;
use zoder_core::pricing::PricingCatalog;

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
static HEALTH: Mutex<Option<HashMap<PathBuf, Entry<HealthStore>>>> = Mutex::new(None);

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

/// The pricing catalog at `path`, re-parsed only when the file changed.
///
/// Byte-for-byte the same result as [`PricingCatalog::load`] — the loader is
/// the one place that decides absent/corrupt/insecure handling, and this
/// only decides whether to call it.
pub(crate) fn pricing_catalog(path: &Path) -> PricingCatalog {
    if let Some(hit) = cache_get(&PRICING, path) {
        return hit;
    }
    let cat = PricingCatalog::load(path);
    cache_put(&PRICING, path, cat.clone());
    cat
}

/// The health store at `path`, re-parsed only when the file changed.
///
/// [`HealthStore::mutate_locked`] writes through an atomic rename, so its
/// result is observed by the next `health_store` call without any explicit
/// invalidation. Callers that write WITHOUT the cache (a direct
/// `HealthStore::load` -> modify -> `save`) still land on disk, and the
/// mtime moves, so the cache cannot serve the pre-write value.
pub(crate) fn health_store(path: &Path) -> HealthStore {
    if let Some(hit) = cache_get(&HEALTH, path) {
        return hit;
    }
    let store = HealthStore::load(path);
    cache_put(&HEALTH, path, store.clone());
    store
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
        written.record_classified_failure(
            "m",
            "503",
            "p",
            zoder_core::Classification::Capacity,
        );
        written.save().unwrap();
        let a = health_store(&path);
        let b = health_store(&path);
        assert_eq!(
            a.models.keys().collect::<Vec<_>>(),
            b.models.keys().collect::<Vec<_>>()
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
        assert_eq!(guard.as_ref().unwrap().len(), 2, "one entry per path");
    }
}
