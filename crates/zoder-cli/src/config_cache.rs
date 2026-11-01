//! Path + mtime memoization for the two JSON files a review re-reads on
//! every step (D5 items 5 and 7).
//!
//! `PricingCatalog::load` re-parses `pricing.json` (over a megabyte on a
//! fleet host) and `HealthStore::load` re-parses `health.json` on every
//! reviewer call, every repair and every recorded failure — several times per
//! review, all on the async task. The files change only when something else
//! writes them (`zoder health`, the daily refresh, a parallel worker), so a
//! read can be served from the last parse as long as the file is the SAME
//! file (`dev`, `ino`) with the same `mtime` and size.
//!
//! Keying on the path alone is not enough: a test (or a second `ZODER_HOME`
//! in one process) can replace the file behind the same name. Keying on
//! path + mtime + size is still an approximation — two writes inside one
//! filesystem timestamp tick are indistinguishable — so this cache is
//! deliberately process-local and short-lived: an entry is only consulted
//! while its `(dev, ino, mtime_ns, len)` stamp and its content hash match the
//! file on disk. The catalog's trusted maximum size (2 MiB) is read to
//! compare hashes, which is cheaper than its JSON parse and is the price of
//! never serving stale pricing to the cost math.
//!
//! Caches live in thread-locals: a review is one task, and tests run in
//! parallel threads sharing one process, so a per-thread cache can never
//! hand another test's fixture data to this one.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use zoder_core::pricing::PricingCatalog;

use crate::HealthStore;

/// The identity of one file version. Two loads with the same stamp AND the
/// same content hash may share a parse.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        }
    }
}

/// A parsed value plus the stamp and content hash it was parsed from.
/// `value` is the serde JSON of the parsed struct, so a hit can hand back a
/// value the struct's own crate does not implement `Clone` for (`HealthStore`
/// is `Serialize + Deserialize` but not `Clone`).
struct Entry {
    stamp: Stamp,
    hash: u64,
    value: String,
}

/// Stable 64-bit content hash (FNV-1a, then splitmix64's finalizer). Not
/// cryptographic: it only has to make it overwhelmingly unlikely that two
/// different catalog contents share a stamp and a hash.
fn content_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

/// Cache stats, so a test can prove a read was served from memory rather
/// than re-parsed. Counts are per thread, like the caches themselves.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CacheCounters {
    /// Calls that had to parse the file (a miss, or an unreadable file).
    pub(crate) loads: usize,
    /// Calls answered from the cached parse (no parse).
    pub(crate) hits: usize,
}

thread_local! {
    static PRICING: RefCell<HashMap<PathBuf, Entry>> = RefCell::new(HashMap::new());
    static HEALTH: RefCell<HashMap<PathBuf, Entry>> = RefCell::new(HashMap::new());
    static PRICING_COUNTERS: std::cell::Cell<CacheCounters> = const { std::cell::Cell::new(CacheCounters { loads: 0, hits: 0 }) };
    static HEALTH_COUNTERS: std::cell::Cell<CacheCounters> = const { std::cell::Cell::new(CacheCounters { loads: 0, hits: 0 }) };
}

/// Serve `path` from `slot` when it is unchanged, otherwise parse it with
/// `parse` and remember the result.
fn cached<T: serde::Serialize + serde::de::DeserializeOwned>(
    slot: &'static std::thread::LocalKey<RefCell<HashMap<PathBuf, Entry>>>,
    counters: &'static std::thread::LocalKey<std::cell::Cell<CacheCounters>>,
    path: &Path,
    parse: impl FnOnce(&Path) -> T,
) -> T {
    let stamp = std::fs::metadata(path).ok().map(|m| Stamp::of(&m));
    let hash = stamp.and_then(|_| read_hash(path));
    if let (Some(stamp), Some(hash)) = (stamp, hash) {
        let hit = slot.with(|c| {
            let c = c.borrow();
            c.get(path)
                .filter(|e| e.stamp == stamp && e.hash == hash)
                .map(|e| e.value.clone())
        });
        if let Some(json) = hit {
            // The cached bytes are this program's own serialization of the
            // struct it parsed, so this cannot fail; a failure would only
            // mean the struct changed shape under a running binary.
            if let Ok(value) = serde_json::from_str(&json) {
                counters.with(|c| {
                    let mut n = c.get();
                    n.hits += 1;
                    c.set(n);
                });
                return value;
            }
        }
    }
    counters.with(|c| {
        let mut n = c.get();
        n.loads += 1;
        c.set(n);
    });
    let value = parse(path);
    if let (Some(stamp), Some(hash), Ok(json)) = (stamp, hash, serde_json::to_string(&value)) {
        slot.with(|c| {
            c.borrow_mut().insert(
                path.to_path_buf(),
                Entry {
                    stamp,
                    hash,
                    value: json,
                },
            );
        });
    }
    value
}

/// Read a file for hashing. Anything larger than the catalog's own trusted
/// maximum is not hashed: `PricingCatalog::load` rejects it anyway, and the
/// health store's limit is larger than that, so such a file simply misses.
fn read_hash(path: &Path) -> Option<u64> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::with_capacity(64 * 1024);
    std::io::Read::read_to_end(&mut f, &mut buf).ok()?;
    Some(content_hash(&buf))
}

/// `PricingCatalog::load(path)`, memoized by path + mtime + size + hash.
///
/// The parsed catalog is a pure function of the file's bytes, so a hit is
/// indistinguishable from a re-read. An absent or unreadable file is not
/// cached: `PricingCatalog::load` returns an empty catalog for it, and the
/// next call must see a catalog that has since appeared.
pub(crate) fn pricing_catalog(path: &Path) -> PricingCatalog {
    cached(&PRICING, &PRICING_COUNTERS, path, PricingCatalog::load)
}

/// `HealthStore::load(path)`, memoized by path + mtime + size + hash.
///
/// ONLY for read-only consumers (consult, routing, the review's cap sizing).
/// A read-modify-write must go through [`HealthStore::mutate_locked`], which
/// loads under the lock and is never served from here — a cached copy handed
/// to a writer would silently drop another process's update.
pub(crate) fn health_store(path: &Path) -> HealthStore {
    cached(&HEALTH, &HEALTH_COUNTERS, path, HealthStore::load)
}

/// Per-thread stats for the two caches. Test-only: nothing in a production
/// build reads them (the counters are still incremented, one relaxed
/// `Cell` update per call, so a test tally can never defer to a different
/// code path).
#[cfg(test)]
pub(crate) fn counters() -> (CacheCounters, CacheCounters) {
    (
        PRICING_COUNTERS.with(std::cell::Cell::get),
        HEALTH_COUNTERS.with(std::cell::Cell::get),
    )
}

/// Drop every cached parse so a test starts from a known state. Test-only:
/// production never needs to flush (a changed file's stamp and hash miss on
/// their own).
#[cfg(test)]
pub(crate) fn clear() {
    PRICING.with(|c| c.borrow_mut().clear());
    HEALTH.with(|c| c.borrow_mut().clear());
}

/// One review's post-change file images, read at most once per path.
///
/// The review reads the same files up to three times per chunk today: once
/// to build the cross-chunk context header (`review_context::chunk_context`
/// per chunk and per reviewer slot), once to disprove a reviewer's
/// false-"undefined" finding (`filter_false_undefined_findings`), and once
/// to ground a blocking finding before confirmation
/// (`review_confirm::ground_finding`). For a branch review each read is a
/// `git show HEAD:<path>` subprocess, so a five-file, four-chunk review forked
/// the same files dozens of times. This cache makes it one read per path per
/// review.
///
/// It is deliberately NOT a global: a review's post-image is fixed for the
/// review (branch reviews read an immutable HEAD; working-tree reviews are
/// read once and compared against the same working tree the diff came from),
/// and a shared cache would serve one review's files to a later one in the
/// same process — the false-"defined" failure this whole family of filters
/// exists to avoid. `Ok`/`None` (a missing or unreadable path, e.g. a deleted
/// file) is cached too: a path that does not exist does not start existing
/// mid-review, and re-probing it per finding is the same wasted read.
pub(crate) struct PostImages<F> {
    read: F,
    cache: RefCell<HashMap<String, Option<String>>>,
}

impl<F: Fn(&str) -> Option<String>> PostImages<F> {
    pub(crate) fn new(read: F) -> Self {
        Self {
            read,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// The post-change text of `path`, read at most once per review.
    pub(crate) fn get(&self, path: &str) -> Option<String> {
        if let Some(hit) = self.cache.borrow().get(path) {
            return hit.clone();
        }
        let value = (self.read)(path);
        self.cache
            .borrow_mut()
            .insert(path.to_string(), value.clone());
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// Write a pricing file the loader will accept: it refuses a catalog that
    /// is group- or world-writable, and this host's umask makes a fresh file
    /// 0664 without this.
    fn write_pricing(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
    }

    /// The observable the tests assert on: `m`'s blended rate.
    fn blended(cat: &PricingCatalog) -> f64 {
        cat.lookup("m").map_or(0.0, |p| p.usd_per_mtok)
    }

    const TWO: &str = r#"{"models":{"m":{"usd_per_mtok":2.0}}}"#;
    const NINE: &str = r#"{"models":{"m":{"usd_per_mtok":9.0}}}"#;

    #[test]
    fn pricing_loads_once_and_serves_unchanged_file_from_memory() {
        let dir = tempdir();
        let path = dir.path().join("pricing.json");
        write_pricing(&path, TWO);
        clear();
        let (before, _) = counters();
        let first = pricing_catalog(&path);
        let second = pricing_catalog(&path);
        let (after, _) = counters();
        assert_eq!(after.loads - before.loads, 1, "exactly one parse");
        assert_eq!(after.hits - before.hits, 1, "the second read is a hit");
        assert_eq!(blended(&first), 2.0);
        assert_eq!(
            blended(&first),
            blended(&second),
            "a hit must hand back the same prices the parse produced"
        );
    }

    #[test]
    fn a_same_length_rewrite_inside_one_mtime_tick_is_not_served_stale() {
        let dir = tempdir();
        let path = dir.path().join("pricing.json");
        clear();
        // TWO and NINE are the same length: on a coarse filesystem clock the
        // stamp may be identical, and only the content hash can tell them
        // apart.
        assert_eq!(TWO.len(), NINE.len());
        write_pricing(&path, TWO);
        let first = pricing_catalog(&path);
        write_pricing(&path, NINE);
        let second = pricing_catalog(&path);
        assert_eq!(blended(&first), 2.0, "the first parse sees the first file");
        assert_eq!(
            blended(&second),
            9.0,
            "a rewritten catalog must be re-parsed even when size and mtime are unchanged"
        );
    }

    #[test]
    fn a_file_that_appears_after_an_empty_load_is_read() {
        let dir = tempdir();
        let path = dir.path().join("pricing.json");
        clear();
        let empty = pricing_catalog(&path);
        assert!(empty.models.is_empty(), "an absent catalog is empty");
        // An absent file is not cached, so the catalog that appears later is
        // the one the next call sees.
        write_pricing(&path, TWO);
        let later = pricing_catalog(&path);
        assert_eq!(
            blended(&later),
            2.0,
            "a catalog written after the empty read must be parsed"
        );
    }

    #[test]
    fn health_is_loaded_once_per_version_and_re_read_after_a_write() {
        let dir = tempdir();
        let path = dir.path().join("health.json");
        clear();
        HealthStore::mutate_locked(&path, |h| h.record_success("m", 10.0)).unwrap();

        let (_, before) = counters();
        let a = health_store(&path);
        let b = health_store(&path);
        let (_, after) = counters();
        assert_eq!(after.loads - before.loads, 1, "exactly one parse");
        assert_eq!(after.hits - before.hits, 1, "the second read is a hit");
        assert_eq!(a.models.len(), 1);
        assert_eq!(b.models.len(), 1);

        // A writer (which never goes through this cache) must be visible: the
        // review path reads the store to size chunks and to consult the
        // breaker, and must not act on a copy older than the file.
        HealthStore::mutate_locked(&path, |h| h.record_failure("m", "boom")).unwrap();
        let c = health_store(&path);
        assert_eq!(
            c.models["m"].failures, 1,
            "the cached store must not hide another writer's update"
        );
    }

    #[test]
    fn content_hash_separates_similar_bytes() {
        assert_ne!(content_hash(b"aaaa"), content_hash(b"aaab"));
        assert_eq!(content_hash(b"same"), content_hash(b"same"));
    }
}
