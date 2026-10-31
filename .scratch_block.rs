/// One review's cache of post-image file text (D10 item 7).
///
/// Every question a review asks about the files under review goes through
/// the same reader: [`chunk_context`](crate::review_context::chunk_context)
/// sizes its header from the whole file, `filter_false_undefined_findings`
/// re-reads the file a finding cites to disprove it, and `ground_finding`
/// reads BOTH the cited file and every file the diff touches, once per
/// finding. A panel of N reviewers over one diff therefore read the same
/// handful of files O(N x findings) times -- each a full file read (or a
/// `git show` subprocess on a branch review).
///
/// The cache lives for ONE review: it is built where the review is built and
/// dropped with it, so a file the author edits between loop iterations is
/// re-read by the next review.
///
/// Only successful reads are remembered: a file that was absent (or
/// unreadable) is asked for again the next time a finding cites it, so a
/// transient IO error is never frozen into a review's verdict.
#[derive(Default)]
pub(crate) struct PostImageCache {
    /// Repo-relative path -> post-image text.
    files: std::sync::Mutex<std::collections::HashMap<String, String>>,
    /// How many reads actually reached the filesystem (or `git show`).
    /// Tests read it; nothing in production consults it.
    reads: std::sync::atomic::AtomicUsize,
}

impl PostImageCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The post-image text of `path`, read at most once per review.
    ///
    /// `source` is the uncached reader (the working tree, or `git show
    /// HEAD:<path>`); it is asked only when this cache has not already seen
    /// `path`. A poisoned lock degrades to the uncached read: the cache is
    /// an optimisation and the result must not depend on it.
    pub(crate) fn read_with(
        &self,
        source: impl FnOnce(&str) -> Option<String>,
        path: &str,
    ) -> Option<String> {
        if let Ok(cache) = self.files.lock() {
            if let Some(known) = cache.get(path) {
                return Some(known.clone());
            }
        }
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let text = source(path)?;
        if let Ok(mut cache) = self.files.lock() {
            cache.insert(path.to_string(), text.clone());
        }
        Some(text)
    }

    /// How many reads reached the filesystem, i.e. cache misses.
    pub(crate) fn read_count(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::Relaxed)
    }
}

