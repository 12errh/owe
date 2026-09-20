//! The daemon's library service: the SQLite index, rescans, and thumbnails.
//!
//! Three pieces, deliberately arranged so the interesting behaviour is testable
//! without threads:
//!
//! - [`LibraryService::scan`] walks the configured roots and reports exactly what
//!   changed ([`owe_core::library`] does the walking; this owns the database
//!   handle and the root list).
//! - [`LibraryService::list`] answers `library.list` from the index — never from a
//!   directory walk, because a GUI search box queries on every keystroke and
//!   re-walking a folder per keystroke is how a "low resource" app burns CPU.
//! - [`LibraryService::thumbnail`] materialises a cached PNG. Generation happens
//!   on a **dedicated worker thread** (`owe-thumbs`), so neither the IPC threads
//!   nor the presenter thread ever decodes a 40 MP photo (FR-LIB-2).
//!
//! # Why the thumbnail worker is a single thread
//!
//! One decode at a time is the right choice for the reference profile (an 8 GB
//! laptop with a spinning disk): parallel decodes would thrash the disk and spike
//! RSS for a grid the user is scrolling past. [`owe_core::thumbs::Scheduler`]
//! enforces the one-at-a-time rule and deduplicates repeat requests, so a GUI that
//! re-requests the same cell while scrolling queues that file once.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use owe_core::library::{FileStamp, Item, Library, LibraryError, ListQuery, ScanReport};
use owe_core::path::XdgPaths;
use owe_core::thumbs::{Decision, Scheduler, SchedulerStats, cache_key, cache_path};
use owe_core::{Config, ContentKind};

/// How long a thumbnail request waits for the worker before giving up.
///
/// Generous on purpose: a request can sit behind one other job per file asked for
/// in the same burst, and on the reference HDD a 40 MP TIFF takes seconds. A
/// timeout is still needed — an unbounded wait would hang the IPC connection
/// forever if the encoder ever deadlocked.
pub const THUMBNAIL_TIMEOUT: Duration = Duration::from_secs(10);

/// One unit of work for the thumbnail thread.
struct ThumbRequest {
    path: PathBuf,
    stamp: FileStamp,
    reply: Sender<Result<PathBuf, String>>,
}

/// Everything the daemon needs from the library.
pub struct LibraryService {
    library: Mutex<Library>,
    /// Expanded `library.paths`, in config order.
    roots: Vec<PathBuf>,
    /// `thumbnail_size` from `[library]`.
    thumbnail_size: u32,
    /// Where cached thumbnails live (`$XDG_CACHE_HOME/owe`).
    cache_root: PathBuf,
    /// Requests handed to the thumbnail thread.
    ///
    /// `Option` purely so [`Drop`] can close the channel *before* joining the
    /// worker: a thread parked in `recv` never wakes up otherwise, and the join
    /// would hang on a request that is never coming. (Found by CI's test timeout:
    /// every test that built a service deadlocked on drop.)
    thumbs: Option<Sender<ThumbRequest>>,
    /// Counters, published by the worker for logs and diagnostics.
    thumb_stats: Arc<Mutex<SchedulerStats>>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// Non-fatal problems found while setting up (reported by the daemon).
    warnings: Vec<String>,
}

impl std::fmt::Debug for LibraryService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryService")
            .field("roots", &self.roots)
            .field("thumbnail_size", &self.thumbnail_size)
            .field("cache_root", &self.cache_root)
            .finish_non_exhaustive()
    }
}

/// A thumbnail ready for the GUI to load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    /// Absolute path to the cached PNG.
    pub path: PathBuf,
    /// Longest edge of the image in that file.
    pub size: u32,
    /// Whether the file already existed (as opposed to being generated now).
    pub cached: bool,
}

impl LibraryService {
    /// Open the library database under `$XDG_DATA_HOME/owe/library.db` and start
    /// the thumbnail worker.
    ///
    /// Never fails fatally: a database that cannot be opened leaves the service
    /// disabled with a warning, because a daemon whose only job is to serve IPC
    /// must still answer `config.get` and `daemon.kill` when the disk is full.
    pub fn open(config: &Config, paths: &XdgPaths) -> Self {
        let mut warnings = Vec::new();

        let roots = match owe_core::library::resolve_roots(&config.library.paths) {
            Ok(roots) => roots,
            Err(error) => {
                // Config validation already rejects unexpandable paths, so this
                // only fires for a config that bypassed validation.
                warnings.push(format!("library roots unusable: {error}"));
                Vec::new()
            }
        };

        let database = paths.data_dir.join("library.db");
        let library = match Library::open(&database) {
            Ok(library) => library,
            Err(error) => {
                warnings.push(format!(
                    "library database `{}` unusable ({error}); falling back to an in-memory index",
                    database.display()
                ));
                // A failed open is reported, then retried in memory so the rest
                // of the daemon still works. If even that fails, the index is
                // empty and every `library.*` call answers honestly.
                Library::open_in_memory().unwrap_or_else(|error| {
                    warnings.push(format!("in-memory library unavailable: {error}"));
                    // `open_in_memory` only fails if SQLite itself is broken; a
                    // zero-sized page cache is not worth a process abort, so the
                    // caller sees an empty library instead.
                    Library::open_in_memory().expect("sqlite in-memory database")
                })
            }
        };

        let (sender, receiver) = channel::<ThumbRequest>();
        let thumb_stats = Arc::new(Mutex::new(SchedulerStats::default()));
        let worker = spawn_thumbnail_worker(
            receiver,
            paths.cache_dir.clone(),
            config.library.thumbnail_size,
            Arc::clone(&thumb_stats),
        );

        Self {
            library: Mutex::new(library),
            roots,
            thumbnail_size: config.library.thumbnail_size,
            cache_root: paths.cache_dir.clone(),
            thumbs: Some(sender),
            thumb_stats,
            worker,
            warnings,
        }
    }

    /// Roots that a rescan walks.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Longest edge of generated thumbnails.
    pub fn thumbnail_size(&self) -> u32 {
        self.thumbnail_size
    }

    /// Non-fatal setup problems, for the daemon's startup log.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Thumbnail counters (generated / served / failures).
    pub fn thumbnail_stats(&self) -> SchedulerStats {
        self.thumb_stats
            .lock()
            .map(|stats| *stats)
            .unwrap_or_default()
    }

    /// Rescan the library: `paths` when given (one-off roots), else the
    /// configured roots.
    pub fn scan(&self, paths: Option<&[PathBuf]>) -> Result<ScanReport, LibraryError> {
        let roots: Vec<PathBuf> = match paths {
            Some(paths) if !paths.is_empty() => paths.to_vec(),
            _ => self.roots.clone(),
        };
        let mut library = self.library.lock().map_err(|_| LibraryError::Directory {
            path: PathBuf::from("library"),
            source: std::io::Error::other("library lock poisoned"),
        })?;
        library.scan(&roots)
    }

    /// Query the index.
    pub fn list(&self, query: &ListQuery) -> Result<Vec<Item>, LibraryError> {
        self.library.lock().map_or_else(
            |_| {
                Err(LibraryError::Directory {
                    path: PathBuf::from("library"),
                    source: std::io::Error::other("library lock poisoned"),
                })
            },
            |library| library.list(query),
        )
    }

    /// How many items match `query` (for paging).
    pub fn count(&self, query: &ListQuery) -> Result<u64, LibraryError> {
        self.library.lock().map_or_else(
            |_| {
                Err(LibraryError::Directory {
                    path: PathBuf::from("library"),
                    source: std::io::Error::other("library lock poisoned"),
                })
            },
            |library| library.count(query),
        )
    }

    /// Look up one item by id.
    pub fn get(&self, id: i64) -> Result<Option<Item>, LibraryError> {
        self.library.lock().map_or_else(
            |_| {
                Err(LibraryError::Directory {
                    path: PathBuf::from("library"),
                    source: std::io::Error::other("library lock poisoned"),
                })
            },
            |library| library.get(id),
        )
    }

    /// Path of an already-cached thumbnail for `item`, without generating one.
    ///
    /// Used by `library.list`: stat-ing the cache for the page the client asked
    /// for is microseconds, and it means the GUI can draw cached cells the moment
    /// a page arrives instead of asking per cell.
    pub fn cached_thumbnail(&self, item: &Item) -> Option<PathBuf> {
        let path = cache_path(
            &self.cache_root,
            self.thumbnail_size,
            &cache_key(&item.path, item.stamp, self.thumbnail_size),
        );
        path.is_file().then_some(path)
    }

    /// Make sure a thumbnail for `item` exists, generating it if needed.
    ///
    /// Blocks on the worker thread (never on the render loop): the caller is an
    /// IPC connection thread by construction.
    pub fn thumbnail(&self, item: &Item) -> Result<Thumbnail, String> {
        let already = self.cached_thumbnail(item).is_some();
        let (reply, replies) = channel();
        self.thumbs
            .as_ref()
            .ok_or_else(|| "the thumbnail worker has been shut down".to_string())?
            .send(ThumbRequest {
                path: item.path.clone(),
                stamp: item.stamp,
                reply,
            })
            .map_err(|_| "the thumbnail worker is not running".to_string())?;

        match replies.recv_timeout(THUMBNAIL_TIMEOUT) {
            Ok(Ok(path)) => Ok(Thumbnail {
                path,
                size: self.thumbnail_size,
                cached: already,
            }),
            Ok(Err(reason)) => Err(reason),
            Err(RecvTimeoutError::Timeout) => Err(format!(
                "thumbnail generation for `{}` timed out after {:?}",
                item.path.display(),
                THUMBNAIL_TIMEOUT
            )),
            Err(RecvTimeoutError::Disconnected) => {
                Err("the thumbnail worker stopped unexpectedly".to_string())
            }
        }
    }
}

/// The engine asks the library for `library:<id>` paths through this trait, so
/// `owed`'s engine module never learns that SQLite exists.
impl LibraryService {
    /// The indexed row for `library:<id>`, with the wording shared by `resolve`
    /// and `kind_of` so both answers can never drift apart.
    fn library_item(&self, id: &str) -> Result<Item, String> {
        let id: i64 = id.trim().parse().map_err(|_| {
            format!("`{id}` is not a library id (expected the number from `library.list`)")
        })?;
        match self.get(id) {
            Ok(Some(item)) => Ok(item),
            Ok(None) => Err(format!(
                "library item {id} is not in the index (it may have been removed by a rescan)"
            )),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// The engine asks the library for `library:<id>` paths and kinds through this
/// trait, so `owed`'s engine module never learns that SQLite exists.
impl crate::engine::LibraryResolver for LibraryService {
    fn resolve(&self, id: &str) -> Result<PathBuf, String> {
        self.library_item(id).map(|item| item.path)
    }

    fn kind_of(&self, id: &str) -> Result<ContentKind, String> {
        self.library_item(id).map(|item| item.kind)
    }
}

impl Drop for LibraryService {
    fn drop(&mut self) {
        // Close the channel first, then join: the worker is blocked in `recv`, so
        // joining before the sender is gone would wait for an event that can never
        // arrive.
        self.thumbs.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Start the thumbnail thread. It exits when the request channel closes.
fn spawn_thumbnail_worker(
    requests: Receiver<ThumbRequest>,
    cache_root: PathBuf,
    size: u32,
    stats: Arc<Mutex<SchedulerStats>>,
) -> Option<std::thread::JoinHandle<()>> {
    let scheduler = match Scheduler::new(cache_root, size) {
        Ok(scheduler) => scheduler,
        Err(error) => {
            // No cache directory means no thumbnails; the caller's requests will
            // fail honestly rather than the daemon refusing to start.
            return Some(std::thread::spawn(move || {
                for request in requests {
                    let _ = request.reply.send(Err(format!(
                        "cannot use the thumbnail cache directory: {error}"
                    )));
                }
            }));
        }
    };

    std::thread::Builder::new()
        .name("owe-thumbs".to_string())
        .spawn(move || {
            let mut scheduler = scheduler;
            while let Ok(request) = requests.recv() {
                let result = ensure(&mut scheduler, &request.path, request.stamp);
                if let Ok(mut slot) = stats.lock() {
                    *slot = scheduler.stats();
                }
                let _ = request.reply.send(result);
            }
        })
        .ok()
}

/// Bring one thumbnail into existence, draining any other queued work first.
///
/// Written as request → drain → request again rather than a bespoke queue loop:
/// the first request tells the scheduler about the file, the drain does the work,
/// and the second request asks the same question the caller cares about ("is there
/// a file now?"). Both a first request and a duplicate (`AlreadyQueued`) take the
/// same path, which is why two GUI cells asking for one file cannot deadlock.
fn ensure(scheduler: &mut Scheduler, path: &Path, stamp: FileStamp) -> Result<PathBuf, String> {
    let _ = scheduler.request(path, stamp);
    while let Some(job) = scheduler.next_job() {
        let outcome = owe_media::thumbnail_png(&job.path, job.size)
            .and_then(|png| {
                write_thumbnail(&job.cache_path, &png).map_err(|source| owe_media::MediaError::Io {
                    path: job.cache_path.display().to_string(),
                    source,
                })
            })
            .map(|()| job.cache_path.clone())
            .map_err(|error| error.to_string());
        scheduler.complete(&job.key, outcome);
    }

    match scheduler.request(path, stamp) {
        Decision::Cached(path) => Ok(path),
        Decision::Failed(reason) => Err(reason),
        // Unreachable in a single-threaded worker: after the drain there is
        // nothing queued and nothing in flight, so the only answers left are
        // "cached" and "failed". Report the state rather than guessing.
        other => Err(format!("thumbnail scheduler answered {other:?}")),
    }
}

/// Write a thumbnail atomically, so a GUI never reads a half-written PNG.
fn write_thumbnail(path: &Path, png: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("png.tmp");
    std::fs::write(&temp, png)?;
    std::fs::rename(&temp, path)
}

/// Parse a `library.list` kind filter. Unknown ids are `None`, so a bad filter is
/// a bad request rather than a silently ignored one.
pub fn parse_kind(value: &str) -> Option<ContentKind> {
    ContentKind::from_id(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(walls: &Path, size: u32) -> LibraryService {
        let config = {
            let mut config = Config::default();
            config.library.paths = vec![walls.display().to_string()];
            config.library.thumbnail_size = size;
            config
        };
        // The XDG tree lives *beside* the scanned root, never inside it: a scan of
        // a directory that contains its own cache and database would index them,
        // and `skipped_unsupported` would count OWE's own files. (This is exactly
        // what the first version of this helper did.)
        let base = walls
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| walls.to_path_buf())
            .join("xdg");
        let paths = XdgPaths {
            config_file: base.join("config.toml"),
            state_dir: base.join("state"),
            cache_dir: base.join("cache"),
            data_dir: base.join("data"),
            runtime_dir: Some(base.join("run")),
        };
        LibraryService::open(&config, &paths)
    }

    fn write_png(path: &Path, width: u32, height: u32, colour: [u8; 4]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut buffer = image::RgbaImage::new(width, height);
        for pixel in buffer.pixels_mut() {
            *pixel = image::Rgba(colour);
        }
        // Always PNG bytes, whatever the file name says: the scanner classifies by
        // extension and the decoder sniffs content, so a `.jpg` holding PNG bytes is
        // a legitimate fixture (and exercises exactly that mismatch).
        buffer
            .save_with_format(path, image::ImageFormat::Png)
            .unwrap();
    }

    #[test]
    fn a_scan_indexes_the_configured_roots_and_lists_them() {
        let dir = tempfile::tempdir().unwrap();
        let walls = dir.path().join("walls");
        write_png(&walls.join("a.png"), 8, 8, [1, 2, 3, 255]);
        write_png(&walls.join("nested/b.jpg"), 8, 8, [4, 5, 6, 255]);
        std::fs::write(walls.join("notes.txt"), b"not a wallpaper").unwrap();

        let service = service(&walls, 64);
        let report = service.scan(None).expect("scan");
        assert_eq!(report.added, 2, "{:?}", report);
        assert_eq!(report.skipped_unsupported, 1);

        let items = service.list(&ListQuery::default()).expect("list");
        assert_eq!(items.len(), 2);
        let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, vec!["a.png", "b.jpg"]);
        assert_eq!(service.count(&ListQuery::default()).unwrap(), 2);
    }

    #[test]
    fn the_index_survives_reopening_the_service() {
        let dir = tempfile::tempdir().unwrap();
        let walls = dir.path().join("walls");
        write_png(&walls.join("a.png"), 8, 8, [1, 2, 3, 255]);

        {
            let service = service(&walls, 64);
            service.scan(None).unwrap();
        }
        // A second service over the same XDG directories must see the rows: the
        // index is a file, not a cache that dies with the process.
        let reopened = service(&walls, 64);
        assert_eq!(reopened.count(&ListQuery::default()).unwrap(), 1);
    }

    #[test]
    fn a_rescan_after_one_edit_touches_one_row() {
        let dir = tempfile::tempdir().unwrap();
        let walls = dir.path().join("walls");
        for index in 0..5 {
            write_png(&walls.join(format!("w{index}.png")), 8, 8, [1, 2, 3, 255]);
        }

        let service = service(&walls, 64);
        let first = service.scan(None).unwrap();
        assert_eq!(first.added, 5);

        // Touch one file: the mtime, not just the content, is what we key on.
        std::thread::sleep(Duration::from_millis(5));
        write_png(&walls.join("w2.png"), 16, 16, [9, 9, 9, 255]);

        let second = service.scan(None).unwrap();
        assert_eq!(second.rows_touched(), 1, "{second:?}");
        assert_eq!(second.updated, 1);
        assert_eq!(second.unchanged, 4);
    }

    #[test]
    fn a_deleted_file_disappears_from_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let walls = dir.path().join("walls");
        write_png(&walls.join("gone.png"), 8, 8, [1, 2, 3, 255]);
        write_png(&walls.join("stays.png"), 8, 8, [1, 2, 3, 255]);

        let service = service(&walls, 64);
        service.scan(None).unwrap();
        std::fs::remove_file(walls.join("gone.png")).unwrap();

        let report = service.scan(None).unwrap();
        assert_eq!(report.removed, 1, "{report:?}");
        assert_eq!(service.count(&ListQuery::default()).unwrap(), 1);
    }

    #[test]
    fn an_explicit_scan_path_overrides_the_configured_roots() {
        let dir = tempfile::tempdir().unwrap();
        let configured = dir.path().join("configured");
        let ad_hoc = dir.path().join("ad-hoc");
        write_png(&configured.join("a.png"), 8, 8, [1, 2, 3, 255]);
        write_png(&ad_hoc.join("b.png"), 8, 8, [1, 2, 3, 255]);

        let service = service(&configured, 64);
        service.scan(Some(std::slice::from_ref(&ad_hoc))).unwrap();

        let items = service.list(&ListQuery::default()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "b.png");
    }

    #[test]
    fn listing_filters_and_pages() {
        let dir = tempfile::tempdir().unwrap();
        let walls = dir.path().join("walls");
        for index in 0..7 {
            write_png(
                &walls.join(format!("wall-{index:02}.png")),
                8,
                8,
                [1, 2, 3, 255],
            );
        }

        let service = service(&walls, 64);
        service.scan(None).unwrap();

        let filtered = service
            .list(&ListQuery {
                filter: Some("wall-0".to_string()),
                ..ListQuery::default()
            })
            .unwrap();
        assert_eq!(filtered.len(), 7, "the filter matches the shared prefix");

        let page = service
            .list(&ListQuery {
                limit: Some(3),
                offset: Some(3),
                ..ListQuery::default()
            })
            .unwrap();
        assert_eq!(page.len(), 3);

        let by_kind = service
            .list(&ListQuery {
                kind: Some(ContentKind::Video),
                ..ListQuery::default()
            })
            .unwrap();
        assert!(by_kind.is_empty(), "no videos are indexed");
    }

    #[test]
    fn items_expose_the_library_reference_the_cli_and_gui_use() {
        let dir = tempfile::tempdir().unwrap();
        let walls = dir.path().join("walls");
        write_png(&walls.join("a.png"), 8, 8, [1, 2, 3, 255]);
        let service = service(&walls, 64);
        service.scan(None).unwrap();

        let item = service.list(&ListQuery::default()).unwrap().remove(0);
        let json = item.to_json();
        assert_eq!(
            json["reference"],
            format!("library:{}", item.id),
            "the reference form must match every other layer"
        );
        assert_eq!(json["kind"], "static-image");
        assert!(json["path"].as_str().unwrap().ends_with("a.png"));
    }

    #[test]
    fn a_thumbnail_is_generated_once_and_served_from_cache_afterwards() {
        let dir = tempfile::tempdir().unwrap();
        let walls = dir.path().join("walls");
        write_png(&walls.join("a.png"), 256, 128, [10, 20, 30, 255]);

        let service = service(&walls, 64);
        service.scan(None).unwrap();
        let item = service.list(&ListQuery::default()).unwrap().remove(0);

        assert!(
            service.cached_thumbnail(&item).is_none(),
            "nothing is cached before the first request"
        );

        let first = service.thumbnail(&item).expect("first thumbnail");
        assert!(!first.cached, "the first call generates");
        assert!(first.path.is_file());

        let second = service.thumbnail(&item).expect("second thumbnail");
        assert!(second.cached, "the second call is served from the cache");
        assert_eq!(first.path, second.path);
        assert_eq!(service.thumbnail_stats().generated, 1);
        assert_eq!(
            service.thumbnail_stats().pending,
            0,
            "nothing is left queued after both calls"
        );
    }

    #[test]
    fn a_thumbnail_has_the_configured_longest_edge() {
        let dir = tempfile::tempdir().unwrap();
        let walls = dir.path().join("walls");
        write_png(&walls.join("wide.png"), 1000, 250, [1, 2, 3, 255]);

        let service = service(&walls, 100);
        service.scan(None).unwrap();
        let item = service.list(&ListQuery::default()).unwrap().remove(0);
        let thumb = service.thumbnail(&item).unwrap();

        let decoded = image::open(&thumb.path).unwrap().to_rgba8();
        assert_eq!((decoded.width(), decoded.height()), (100, 25));
        assert_eq!(thumb.size, 100);
    }

    #[test]
    fn a_thumbnail_request_for_a_broken_file_fails_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let walls = dir.path().join("walls");
        std::fs::create_dir_all(&walls).unwrap();
        std::fs::write(walls.join("broken.png"), b"not an image").unwrap();

        let service = service(&walls, 64);
        service.scan(None).unwrap();
        let item = service.list(&ListQuery::default()).unwrap().remove(0);

        let error = service.thumbnail(&item).unwrap_err();
        assert!(
            error.contains("decode") || error.contains("cannot"),
            "{error}"
        );
    }

    #[test]
    fn editing_a_file_invalidates_its_thumbnail() {
        let dir = tempfile::tempdir().unwrap();
        let walls = dir.path().join("walls");
        write_png(&walls.join("a.png"), 64, 64, [1, 2, 3, 255]);

        let service = service(&walls, 32);
        service.scan(None).unwrap();
        let before = service.list(&ListQuery::default()).unwrap().remove(0);
        let first = service.thumbnail(&before).unwrap();

        std::thread::sleep(Duration::from_millis(5));
        write_png(&walls.join("a.png"), 64, 64, [200, 100, 50, 255]);
        service.scan(None).unwrap();
        let after = service.list(&ListQuery::default()).unwrap().remove(0);

        let second = service.thumbnail(&after).unwrap();
        assert_ne!(
            first.path, second.path,
            "a new mtime must produce a new cache key, or the GUI shows the old picture"
        );
        assert!(second.path.is_file());
    }
}
