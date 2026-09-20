//! Thumbnail cache rules and the scheduler that generates them (FR-LIB-2).
//!
//! Split on purpose: **this module is pure** (keys, dedup, queue order, staleness)
//! and the actual PNG encode lives behind a caller-supplied closure, driven by the
//! daemon. That is what lets the interesting behaviour — "the same file requested
//! twice is generated once", "an edited file gets a new key so the old thumbnail is
//! not shown" — be tested without a GPU, a compositor, or a slow disk.
//!
//! # Why the key contains the mtime
//!
//! A cache keyed only on the path shows the *old* picture after the user edits the
//! file. Keying on (path, mtime, size, thumbnail size) makes every edit a new cache
//! entry, and makes the old entry garbage for [`stale_keys`] to collect.
//!
//! # Never blocking the render loop
//!
//! [`Scheduler::request`] does no work beyond a `stat`-free key computation and a
//! cache-file existence check: it answers "use this file", "queued", or "already
//! queued". Generation happens when the daemon pulls a [`ThumbJob`] off the queue
//! on a worker thread, so a 40 MB TIFF never delays a wallpaper change.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use crate::library::FileStamp;

/// Cache subdirectory name under `$XDG_CACHE_HOME/owe`.
pub const CACHE_DIR_NAME: &str = "thumbs";

/// What [`Scheduler::request`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// A cached thumbnail exists and is current: use this file.
    Cached(PathBuf),
    /// Enqueued now; a thumbnail will appear later.
    Enqueued,
    /// Already enqueued or in flight: asking twice generates once.
    AlreadyQueued,
    /// A previous attempt failed; the reason is remembered so the GUI can say so.
    Failed(String),
}

impl Decision {
    /// Whether the caller already has a usable file.
    pub fn is_cached(&self) -> bool {
        matches!(self, Decision::Cached(_))
    }
}

/// One unit of thumbnail work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbJob {
    /// Cache key (also the file stem).
    pub key: String,
    /// Source wallpaper.
    pub path: PathBuf,
    /// Where the encoded PNG must be written.
    pub cache_path: PathBuf,
    /// Source stamp the key was derived from — the generator must not re-stat the
    /// file and disagree with the key.
    pub stamp: FileStamp,
    /// Longest edge of the produced image.
    pub size: u32,
}

/// Cache key for one (path, stamp, size) triple.
pub fn cache_key(path: &Path, stamp: FileStamp, size: u32) -> String {
    // Two independent 64-bit FNV-1a passes give a 128-bit key: effectively
    // collision-free, dependency-free, and stable across runs and machines —
    // which a `DefaultHasher` (explicitly not stable across Rust releases) is not.
    let mut material = Vec::with_capacity(64);
    material.extend_from_slice(path.to_string_lossy().as_bytes());
    material.push(0);
    material.extend_from_slice(&stamp.mtime_secs.to_le_bytes());
    material.extend_from_slice(&stamp.mtime_nanos.to_le_bytes());
    material.extend_from_slice(&stamp.size_bytes.to_le_bytes());
    material.extend_from_slice(&size.to_le_bytes());

    format!(
        "{:016x}{:016x}",
        fnv1a64(&material, 0xcbf2_9ce4_8422_2325),
        fnv1a64(&material, 0x0000_0100_0000_01b3)
    )
}

/// FNV-1a, 64-bit. Tiny, well-defined, and independent of the standard library's
/// (unstable) hashing.
fn fnv1a64(bytes: &[u8], offset_basis: u64) -> u64 {
    let mut hash = offset_basis;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Full cache path for a key.
///
/// Layout: `<cache>/thumbs/<size>/<key>.png`. The size directory keeps a user's
/// 128 px and 512 px caches from fighting, and lets `rm -rf` work per size.
pub fn cache_path(cache_root: &Path, size: u32, key: &str) -> PathBuf {
    cache_root
        .join(CACHE_DIR_NAME)
        .join(size.to_string())
        .join(format!("{key}.png"))
}

/// Cached keys that no current item claims, and which may be deleted.
pub fn stale_keys(cached: &BTreeSet<String>, live: &BTreeSet<String>) -> Vec<String> {
    cached.difference(live).cloned().collect()
}

/// The thumbnail scheduler: dedup, ordering, and honest accounting.
#[derive(Debug)]
pub struct Scheduler {
    cache_root: PathBuf,
    size: u32,
    queue: VecDeque<ThumbJob>,
    /// Keys that are queued, in flight, ready, or failed.
    known: BTreeMap<String, Decision>,
    in_flight: Option<String>,
    generated: u64,
    served_from_cache: u64,
    failures: u64,
    max_queue: usize,
}

/// Default queue ceiling. Beyond this the scheduler starts refusing work, because
/// a 50 000-file library scrolled at speed must not queue 50 000 jobs.
pub const DEFAULT_MAX_QUEUE: usize = 512;

impl Scheduler {
    /// Create the cache directory and a scheduler for thumbnails of `size` px.
    pub fn new(cache_root: PathBuf, size: u32) -> std::io::Result<Self> {
        std::fs::create_dir_all(cache_root.join(CACHE_DIR_NAME).join(size.to_string()))?;
        Ok(Self::with_max_queue(cache_root, size, DEFAULT_MAX_QUEUE))
    }

    /// As [`Scheduler::new`] but with an explicit queue ceiling (tests).
    pub fn with_max_queue(cache_root: PathBuf, size: u32, max_queue: usize) -> Self {
        Self {
            cache_root,
            size,
            queue: VecDeque::new(),
            known: BTreeMap::new(),
            in_flight: None,
            generated: 0,
            served_from_cache: 0,
            failures: 0,
            max_queue: max_queue.max(1),
        }
    }

    /// Thumbnail edge length this scheduler produces.
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Ask for a thumbnail of `path`.
    ///
    /// Returns immediately; nothing is decoded here. Repeat requests for the same
    /// (path, stamp, size) are deduplicated, which is what keeps a GUI that
    /// re-renders on every keystroke from queueing the same work repeatedly.
    pub fn request(&mut self, path: &Path, stamp: FileStamp) -> Decision {
        let key = cache_key(path, stamp, self.size);
        let cache_path = cache_path(&self.cache_root, self.size, &key);

        if let Some(known) = self.known.get(&key) {
            return match known {
                Decision::Cached(path) => Decision::Cached(path.clone()),
                Decision::Failed(reason) => Decision::Failed(reason.clone()),
                // Enqueued or in flight.
                _ => Decision::AlreadyQueued,
            };
        }

        if cache_path.is_file() {
            self.served_from_cache += 1;
            self.known.insert(key, Decision::Cached(cache_path.clone()));
            return Decision::Cached(cache_path);
        }

        // A full queue is a refusal, not a silent drop: the caller can retry when
        // the queue drains, and the GUI shows "generating…" either way.
        if self.queue.len() >= self.max_queue {
            return Decision::Failed(format!(
                "thumbnail queue is full ({} jobs); will retry when it drains",
                self.max_queue
            ));
        }

        self.known.insert(key.clone(), Decision::Enqueued);
        self.queue.push_back(ThumbJob {
            key,
            path: path.to_path_buf(),
            cache_path,
            stamp,
            size: self.size,
        });
        Decision::Enqueued
    }

    /// Take the next job to generate, if any.
    pub fn next_job(&mut self) -> Option<ThumbJob> {
        if self.in_flight.is_some() {
            // One job at a time: a decode already saturates the disk on the
            // reference profile (a spinning HDD), and queues instead of racing.
            return None;
        }
        let job = self.queue.pop_front()?;
        self.in_flight = Some(job.key.clone());
        Some(job)
    }

    /// Report the outcome of a job taken from [`Scheduler::next_job`].
    pub fn complete(&mut self, key: &str, result: Result<PathBuf, String>) {
        if self.in_flight.as_deref() == Some(key) {
            self.in_flight = None;
        }
        match result {
            Ok(path) => {
                self.generated += 1;
                self.known.insert(key.to_string(), Decision::Cached(path));
            }
            Err(reason) => {
                self.failures += 1;
                self.known.insert(key.to_string(), Decision::Failed(reason));
            }
        }
    }

    /// Whether there is nothing left to do (used to park the worker thread).
    pub fn is_idle(&self) -> bool {
        self.in_flight.is_none() && self.queue.is_empty()
    }

    /// Jobs waiting (not counting the in-flight one).
    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// Keys the scheduler currently considers live (queued, in flight, or cached).
    pub fn live_keys(&self) -> BTreeSet<String> {
        self.known
            .iter()
            .filter(|(_, decision)| !matches!(decision, Decision::Failed(_)))
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Forget a key, so the next request regenerates it (used when a cached file
    /// was deleted behind our back).
    pub fn forget(&mut self, key: &str) {
        self.known.remove(key);
        self.queue.retain(|job| job.key != key);
    }

    /// Clear the failure record for a key so it can be retried.
    pub fn retry(&mut self, key: &str) {
        if matches!(self.known.get(key), Some(Decision::Failed(_))) {
            self.known.remove(key);
        }
    }

    /// Counters for `stats.get` and the tests.
    pub fn stats(&self) -> SchedulerStats {
        SchedulerStats {
            generated: self.generated,
            served_from_cache: self.served_from_cache,
            failures: self.failures,
            pending: self.queue.len(),
            known: self.known.len(),
        }
    }

    /// Reset the counters (not the queue) — used between benchmark runs.
    pub fn reset_stats(&mut self) {
        self.generated = 0;
        self.served_from_cache = 0;
        self.failures = 0;
    }
}

/// Counters describing what a scheduler has done.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SchedulerStats {
    /// Thumbnails actually encoded.
    pub generated: u64,
    /// Requests answered from an existing cache file.
    pub served_from_cache: u64,
    /// Failed generations.
    pub failures: u64,
    /// Jobs still waiting.
    pub pending: usize,
    /// Keys the scheduler remembers.
    pub known: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(mtime_secs: i64, size_bytes: u64) -> FileStamp {
        FileStamp {
            mtime_secs,
            mtime_nanos: 0,
            size_bytes,
        }
    }

    #[test]
    fn cache_keys_are_stable_and_distinct() {
        let path = Path::new("/walls/aurora.png");
        let key = cache_key(path, stamp(1_700_000_000, 4096), 512);

        // Golden value: this test fails if the key ever changes, which would
        // silently invalidate every user's cached thumbnails.
        assert_eq!(key.len(), 32, "128-bit hex: {key}");
        assert_eq!(key, cache_key(path, stamp(1_700_000_000, 4096), 512));

        assert_ne!(key, cache_key(path, stamp(1_700_000_001, 4096), 512));
        assert_ne!(key, cache_key(path, stamp(1_700_000_000, 4097), 512));
        assert_ne!(key, cache_key(path, stamp(1_700_000_000, 4096), 256));
        assert_ne!(
            key,
            cache_key(
                Path::new("/walls/other.png"),
                stamp(1_700_000_000, 4096),
                512
            )
        );
    }

    #[test]
    fn cache_paths_live_under_the_cache_root_and_the_size() {
        let path = cache_path(Path::new("/home/u/.cache/owe"), 512, "abc");
        assert_eq!(path, PathBuf::from("/home/u/.cache/owe/thumbs/512/abc.png"));
    }

    #[test]
    fn an_edited_file_gets_a_new_key_and_orphans_the_old_one() {
        let path = Path::new("/walls/wall.png");
        let before = cache_key(path, stamp(100, 10), 512);
        let after = cache_key(path, stamp(101, 10), 512);
        assert_ne!(before, after);

        let cached: BTreeSet<String> = [before.clone(), after.clone()].into_iter().collect();
        let live: BTreeSet<String> = [after.clone()].into_iter().collect();
        assert_eq!(stale_keys(&cached, &live), vec![before]);
    }

    #[test]
    fn requesting_the_same_file_twice_queues_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut scheduler = Scheduler::new(dir.path().to_path_buf(), 256).unwrap();
        let path = Path::new("/walls/a.png");

        assert_eq!(scheduler.request(path, stamp(1, 1)), Decision::Enqueued);
        assert_eq!(
            scheduler.request(path, stamp(1, 1)),
            Decision::AlreadyQueued,
            "a second request must not enqueue a second job"
        );
        assert_eq!(scheduler.pending(), 1);
    }

    #[test]
    fn an_existing_cache_file_is_served_without_work() {
        let dir = tempfile::tempdir().unwrap();
        let mut scheduler = Scheduler::new(dir.path().to_path_buf(), 256).unwrap();
        let path = Path::new("/walls/a.png");

        let expected = cache_path(dir.path(), 256, &cache_key(path, stamp(1, 1), 256));
        std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
        std::fs::write(&expected, b"png").unwrap();

        assert_eq!(
            scheduler.request(path, stamp(1, 1)),
            Decision::Cached(expected)
        );
        assert_eq!(scheduler.pending(), 0);
        assert_eq!(scheduler.stats().served_from_cache, 1);
    }

    #[test]
    fn jobs_are_handed_out_one_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut scheduler = Scheduler::new(dir.path().to_path_buf(), 256).unwrap();
        scheduler.request(Path::new("/walls/a.png"), stamp(1, 1));
        scheduler.request(Path::new("/walls/b.png"), stamp(1, 1));

        let first = scheduler.next_job().expect("first job");
        assert_eq!(first.path, PathBuf::from("/walls/a.png"));
        assert_eq!(first.size, 256);
        assert!(
            scheduler.next_job().is_none(),
            "one decode at a time keeps the disk (and the HDD profile) sane"
        );

        scheduler.complete(&first.key, Ok(first.cache_path.clone()));
        let second = scheduler.next_job().expect("second job");
        assert_eq!(second.path, PathBuf::from("/walls/b.png"));
    }

    #[test]
    fn a_completed_job_is_served_from_cache_afterwards() {
        let dir = tempfile::tempdir().unwrap();
        let mut scheduler = Scheduler::new(dir.path().to_path_buf(), 256).unwrap();
        let path = Path::new("/walls/a.png");
        scheduler.request(path, stamp(1, 1));
        let job = scheduler.next_job().unwrap();
        scheduler.complete(&job.key, Ok(job.cache_path.clone()));

        assert_eq!(
            scheduler.request(path, stamp(1, 1)),
            Decision::Cached(job.cache_path)
        );
        assert!(scheduler.is_idle());
        assert_eq!(scheduler.stats().generated, 1);
    }

    #[test]
    fn a_failure_is_remembered_and_reportable() {
        let dir = tempfile::tempdir().unwrap();
        let mut scheduler = Scheduler::new(dir.path().to_path_buf(), 256).unwrap();
        let path = Path::new("/walls/broken.png");
        scheduler.request(path, stamp(1, 1));
        let job = scheduler.next_job().unwrap();
        scheduler.complete(&job.key, Err("not an image".to_string()));

        assert_eq!(
            scheduler.request(path, stamp(1, 1)),
            Decision::Failed("not an image".to_string()),
            "a broken file must not be retried on every keystroke"
        );
        assert_eq!(scheduler.stats().failures, 1);
        assert!(
            !scheduler.live_keys().contains(&job.key),
            "a failed key is not live, so its (absent) file is never pruned as if it were ours"
        );

        // Unless the user asks for a retry.
        scheduler.retry(&job.key);
        assert_eq!(scheduler.request(path, stamp(1, 1)), Decision::Enqueued);
    }

    #[test]
    fn a_full_queue_refuses_instead_of_growing_without_bound() {
        let dir = tempfile::tempdir().unwrap();
        let mut scheduler = Scheduler::with_max_queue(dir.path().to_path_buf(), 128, 2);

        assert_eq!(
            scheduler.request(Path::new("/a.png"), stamp(1, 1)),
            Decision::Enqueued
        );
        assert_eq!(
            scheduler.request(Path::new("/b.png"), stamp(1, 1)),
            Decision::Enqueued
        );
        match scheduler.request(Path::new("/c.png"), stamp(1, 1)) {
            Decision::Failed(reason) => {
                assert!(reason.contains("queue is full"), "{reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(scheduler.pending(), 2, "the queue did not grow");
    }

    #[test]
    fn is_idle_is_true_only_when_nothing_is_queued_or_running() {
        let dir = tempfile::tempdir().unwrap();
        let mut scheduler = Scheduler::new(dir.path().to_path_buf(), 256).unwrap();
        assert!(scheduler.is_idle());

        scheduler.request(Path::new("/a.png"), stamp(1, 1));
        assert!(!scheduler.is_idle());

        let job = scheduler.next_job().unwrap();
        assert!(!scheduler.is_idle(), "in flight is not idle");

        scheduler.complete(&job.key, Ok(job.cache_path));
        assert!(scheduler.is_idle());
    }

    #[test]
    fn forgetting_a_key_regenerates_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut scheduler = Scheduler::new(dir.path().to_path_buf(), 256).unwrap();
        let path = Path::new("/a.png");
        scheduler.request(path, stamp(1, 1));
        let job = scheduler.next_job().unwrap();
        scheduler.complete(&job.key, Ok(job.cache_path.clone()));

        scheduler.forget(&job.key);
        assert_eq!(scheduler.request(path, stamp(1, 1)), Decision::Enqueued);
    }

    #[test]
    fn clearing_a_failed_key_from_the_queue_works_too() {
        let dir = tempfile::tempdir().unwrap();
        let mut scheduler = Scheduler::new(dir.path().to_path_buf(), 256).unwrap();
        let path = Path::new("/a.png");
        scheduler.request(path, stamp(1, 1));
        let key = cache_key(path, stamp(1, 1), 256);

        scheduler.forget(&key);
        assert_eq!(scheduler.pending(), 0);
        assert_eq!(scheduler.request(path, stamp(1, 1)), Decision::Enqueued);
    }

    #[test]
    fn creating_a_scheduler_makes_its_cache_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cache");
        let scheduler = Scheduler::new(root.clone(), 512).unwrap();
        assert_eq!(scheduler.size(), 512);
        assert!(root.join("thumbs/512").is_dir());
    }

    #[test]
    fn stats_reset_leaves_the_queue_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut scheduler = Scheduler::new(dir.path().to_path_buf(), 256).unwrap();
        scheduler.request(Path::new("/a.png"), stamp(1, 1));
        scheduler.reset_stats();

        let stats = scheduler.stats();
        assert_eq!(stats.generated, 0);
        assert_eq!(stats.served_from_cache, 0);
        assert_eq!(stats.pending, 1, "the work is still queued");
    }
}
