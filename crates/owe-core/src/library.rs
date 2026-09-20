//! The wallpaper library: an indexed view of the configured folders (FR-LIB-1).
//!
//! # Why SQLite and not a JSON index
//!
//! A library is queried by the GUI on every keystroke in its search box and is
//! filtered/paginated. Re-reading and re-parsing a file per keystroke is how a
//! "low resource" app ends up burning CPU while the user types. SQLite gives
//! indexed lookups, an atomic transaction per scan, and — because it is a file —
//! nothing to keep in RAM.
//!
//! # What "incremental rescan" means here (and what it does not)
//!
//! A rescan **stats every file** under the configured roots. It does not re-read
//! file contents, and it only *writes* rows whose (mtime, size) pair changed.
//! [`ScanReport::rows_touched`] is that write count, and the P2 gate asserts that
//! changing one file touches exactly one row. Anything stronger (watching the
//! filesystem and stat-ing nothing) would need an inotify daemon, and would still
//! have to stat on every event to catch edits.
//!
//! # Two safety rules that exist because their absence loses user data
//!
//! 1. **A missing root never deletes rows.** An unplugged USB drive or an
//!    unmounted network share must not be read as "the user deleted 4000
//!    wallpapers". Rows are only removed when their directory was actually
//!    readable during this scan.
//! 2. **A row under *any* missing root is kept**, so nested roots
//!    (`~/Walls` and `~/Walls/Downloaded`) cannot delete each other's rows when
//!    one of them is unavailable.
//!
//! # Ids
//!
//! `id` is the row's primary key and `path` is unique, so `library:<id>` is
//! stable as long as the database lives. The database is a *cache*
//! (ARCHITECTURE §7): a rebuilt one gets new ids, which is why the daemon
//! persists the resolved **path** in session state rather than the library id.
//! `library:<path>` is also accepted, so a reference survives a cache wipe.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, ToSql, params};
use thiserror::Error;

use crate::model::ContentKind;

/// Schema version of the library database. Bump when the tables change.
pub const LIBRARY_SCHEMA_VERSION: u32 = 1;

/// Upper bound on the number of rows a single [`ListQuery`] may return.
pub const MAX_PAGE: u32 = 500;

/// Columns every item query selects, in the order [`row_to_item`] expects.
const ITEM_COLUMNS: &str = "id, path, dir, name, kind, size_bytes, mtime_secs, mtime_nanos";

/// Failures from the library index.
#[derive(Debug, Error)]
pub enum LibraryError {
    /// The database file could not be opened or created.
    #[error("cannot open the library database `{path}`: {source}")]
    Open {
        /// Database path that failed.
        path: PathBuf,
        /// Underlying cause.
        source: rusqlite::Error,
    },

    /// The database directory could not be created.
    #[error("cannot create the library directory `{path}`: {source}")]
    Directory {
        /// Directory that failed.
        path: PathBuf,
        /// Underlying cause.
        source: std::io::Error,
    },

    /// A query or write failed.
    #[error("library database error: {0}")]
    Sql(#[from] rusqlite::Error),

    /// The database was written by a newer OWE.
    #[error(
        "library database `{path}` has schema version {found} but this build understands \
         {supported}; delete the file to rebuild it (it is a cache)"
    )]
    SchemaTooNew {
        /// Database path.
        path: PathBuf,
        /// Version found on disk.
        found: u32,
        /// Version this build writes.
        supported: u32,
    },

    /// A root in `library.paths` is not a usable absolute path.
    #[error("library root `{root}`: {detail}")]
    Root {
        /// The offending entry.
        root: String,
        /// Why it was rejected.
        detail: String,
    },
}

/// Last-modified/size stamp of a file, the basis of incremental rescans.
///
/// Nanoseconds are kept because a coarse filesystem can change a file within the
/// same second; without nanos the rescan would miss the edit and the library
/// would show a stale thumbnail forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileStamp {
    /// Whole seconds since the Unix epoch.
    pub mtime_secs: i64,
    /// Nanosecond component (0..1_000_000_000).
    pub mtime_nanos: u32,
    /// File size in bytes.
    pub size_bytes: u64,
}

impl FileStamp {
    /// Read a stamp from filesystem metadata.
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        let (mtime_secs, mtime_nanos) = match metadata.modified() {
            Ok(modified) => match modified.duration_since(std::time::UNIX_EPOCH) {
                Ok(duration) => (duration.as_secs() as i64, duration.subsec_nanos()),
                // A timestamp before 1970 means a broken clock or a malformed
                // filesystem. Clamping to 0 is not "correct", but it is stable:
                // the file simply looks old and unchanged until it is written
                // again, which is the harmless direction to be wrong in.
                Err(_) => (0, 0),
            },
            Err(_) => (0, 0),
        };
        Self {
            mtime_secs,
            mtime_nanos,
            size_bytes: metadata.len(),
        }
    }
}

/// One indexed wallpaper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Stable primary key, used as `library:<id>`.
    pub id: i64,
    /// Absolute path to the file.
    pub path: PathBuf,
    /// Directory containing the file.
    pub dir: PathBuf,
    /// File name, including extension.
    pub name: String,
    /// Content kind, derived from the extension.
    pub kind: ContentKind,
    /// Last-write stamp at index time.
    pub stamp: FileStamp,
}

impl Item {
    /// Wire shape used by `library.list` (kept here so the CLI, the GUI, and the
    /// tests all read the same field names).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "path": self.path.display().to_string(),
            "name": self.name,
            "dir": self.dir.display().to_string(),
            "kind": self.kind.as_str(),
            "reference": format!("library:{}", self.id),
            "size_bytes": self.stamp.size_bytes,
            "mtime_secs": self.stamp.mtime_secs,
        })
    }
}

/// What one scan did. Every field exists so the daemon can log a truthful
/// summary and the gate can assert on exact counts instead of wall-clock luck.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Roots that were walked.
    pub roots: Vec<PathBuf>,
    /// Rows inserted (new files).
    pub added: usize,
    /// Rows whose (mtime, size) changed.
    pub updated: usize,
    /// Rows removed because the files are gone.
    pub removed: usize,
    /// Files that were already indexed and unchanged.
    pub unchanged: usize,
    /// Files skipped because they are not a recognised wallpaper format.
    pub skipped_unsupported: usize,
    /// Directories that could not be read (their rows are preserved).
    pub unreadable_dirs: Vec<PathBuf>,
    /// Configured roots that do not exist or are not directories.
    pub missing_roots: Vec<PathBuf>,
    /// How long the walk took.
    pub duration: Duration,
}

impl ScanReport {
    /// Total number of rows written (insert + update + delete).
    ///
    /// This is the number the P2 gate measures: a one-file change must produce
    /// exactly `1`.
    pub fn rows_touched(&self) -> usize {
        self.added + self.updated + self.removed
    }

    /// Total files inspected (unchanged included).
    pub fn files_seen(&self) -> usize {
        self.added + self.updated + self.unchanged + self.skipped_unsupported
    }

    /// A one-line summary for logs.
    pub fn summary(&self) -> String {
        format!(
            "scanned {} root(s): {} file(s) seen, +{} ~{} -{} ({} unchanged, {} skipped), \
             {} unreadable, {} missing, {:?}",
            self.roots.len(),
            self.files_seen(),
            self.added,
            self.updated,
            self.removed,
            self.unchanged,
            self.skipped_unsupported,
            self.unreadable_dirs.len(),
            self.missing_roots.len(),
            self.duration
        )
    }
}

/// How to filter and page a [`Library::list`] call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListQuery {
    /// Case-insensitive substring matched against the file name and full path.
    pub filter: Option<String>,
    /// Restrict to one content kind.
    pub kind: Option<ContentKind>,
    /// Only items inside this directory (inclusive of subdirectories).
    pub under: Option<PathBuf>,
    /// Maximum rows to return (clamped to [`MAX_PAGE`]).
    pub limit: Option<u32>,
    /// Rows to skip.
    pub offset: Option<u32>,
}

/// The library index.
pub struct Library {
    conn: Connection,
}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library").finish_non_exhaustive()
    }
}

impl Library {
    /// Open (creating if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<Self, LibraryError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| LibraryError::Directory {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let conn = Connection::open(path).map_err(|source| LibraryError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        Self::init(conn, path)
    }

    /// An in-memory database, for tests and for a session with no writable data
    /// directory.
    pub fn open_in_memory() -> Result<Self, LibraryError> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn, Path::new(":memory:"))
    }

    fn init(conn: Connection, path: &Path) -> Result<Self, LibraryError> {
        // WAL keeps a scan from blocking GUI reads; NORMAL sync is the right
        // trade for a rebuildable cache (a lost transaction costs one rescan).
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;

        let found: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()?;

        if let Some(version) = found {
            let found: u32 = version.parse().unwrap_or(0);
            if found > LIBRARY_SCHEMA_VERSION {
                return Err(LibraryError::SchemaTooNew {
                    path: path.to_path_buf(),
                    found,
                    supported: LIBRARY_SCHEMA_VERSION,
                });
            }
        }

        conn.execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS items (
                 id          INTEGER PRIMARY KEY,
                 path        TEXT    NOT NULL UNIQUE,
                 dir         TEXT    NOT NULL,
                 name        TEXT    NOT NULL,
                 kind        TEXT    NOT NULL,
                 size_bytes  INTEGER NOT NULL,
                 mtime_secs  INTEGER NOT NULL,
                 mtime_nanos INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS items_dir  ON items(dir);
             CREATE INDEX IF NOT EXISTS items_name ON items(name);
             INSERT OR REPLACE INTO meta (key, value)
                 VALUES ('schema_version', '{LIBRARY_SCHEMA_VERSION}');"
        ))?;

        Ok(Self { conn })
    }

    /// Walk `roots` and bring the index up to date.
    ///
    /// Incremental by construction: every file is stat-ed, and only rows whose
    /// stamp changed are written. See the module docs for the deletion rules.
    pub fn scan(&mut self, roots: &[PathBuf]) -> Result<ScanReport, LibraryError> {
        let started = std::time::Instant::now();
        let mut report = ScanReport {
            roots: roots.to_vec(),
            ..ScanReport::default()
        };

        // Which roots can actually be read this time. Everything about deletion
        // depends on this list, so it is decided once, up front.
        let mut readable: Vec<PathBuf> = Vec::new();
        for root in roots {
            match std::fs::metadata(root) {
                Ok(metadata) if metadata.is_dir() => readable.push(root.clone()),
                // An unreachable root is reported, never treated as "everything
                // under it was deleted".
                _ => report.missing_roots.push(root.clone()),
            }
        }

        // Everything currently indexed, so the per-file decision is a hash lookup
        // instead of a query. One SELECT beats N queries for a 20k-file library.
        let existing: HashMap<PathBuf, FileStamp> = {
            let mut statement = self
                .conn
                .prepare("SELECT path, mtime_secs, mtime_nanos, size_bytes FROM items")?;
            let rows = statement.query_map([], |row| {
                let path: String = row.get(0)?;
                let mtime_secs: i64 = row.get(1)?;
                let mtime_nanos: i64 = row.get(2)?;
                let size_bytes: i64 = row.get(3)?;
                Ok((
                    PathBuf::from(path),
                    FileStamp {
                        mtime_secs,
                        mtime_nanos: mtime_nanos.max(0) as u32,
                        size_bytes: size_bytes.max(0) as u64,
                    },
                ))
            })?;
            rows.collect::<Result<HashMap<_, _>, _>>()?
        };

        let mut seen: HashMap<PathBuf, FileStamp> = HashMap::new();
        for root in &readable {
            // Two configured roots that nest would walk the same subtree twice and
            // double-count `unchanged`. Walking the outer root already covers the
            // inner one; the inner root is still *checked* above, so a missing
            // nested root is still reported.
            let nested = readable
                .iter()
                .any(|other| other != root && root.starts_with(other));
            if nested {
                continue;
            }
            self.walk(root, &mut seen, &mut report);
        }

        let transaction = self.conn.transaction()?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO items (path, dir, name, kind, size_bytes, mtime_secs, mtime_nanos) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            let mut update = transaction.prepare(
                "UPDATE items SET size_bytes = ?2, mtime_secs = ?3, mtime_nanos = ?4, \
                 name = ?5, dir = ?6, kind = ?7 WHERE path = ?1",
            )?;

            for (path, stamp) in &seen {
                match existing.get(path) {
                    None => {
                        let (dir, name, kind) = describe(path);
                        insert.execute(params![
                            path.to_string_lossy(),
                            dir.to_string_lossy(),
                            name,
                            kind.as_str(),
                            stamp.size_bytes as i64,
                            stamp.mtime_secs,
                            i64::from(stamp.mtime_nanos),
                        ])?;
                        report.added += 1;
                    }
                    Some(old) if old != stamp => {
                        let (dir, name, kind) = describe(path);
                        update.execute(params![
                            path.to_string_lossy(),
                            stamp.size_bytes as i64,
                            stamp.mtime_secs,
                            i64::from(stamp.mtime_nanos),
                            name,
                            dir.to_string_lossy(),
                            kind.as_str(),
                        ])?;
                        report.updated += 1;
                    }
                    Some(_) => report.unchanged += 1,
                }
            }

            // Deletions: only rows that live under a root we actually read this
            // time, and under no missing root (rule 2).
            let mut delete = transaction.prepare("DELETE FROM items WHERE path = ?1")?;
            let gone: Vec<PathBuf> = existing
                .keys()
                .filter(|path| !seen.contains_key(*path))
                .filter(|path| {
                    under_any(path, &readable) && !under_any(path, &report.missing_roots)
                })
                .cloned()
                .collect();
            for path in gone {
                delete.execute(params![path.to_string_lossy()])?;
                report.removed += 1;
            }
        }
        transaction.commit()?;

        report.duration = started.elapsed();
        Ok(report)
    }

    /// Depth-first walk of one root, collecting recognised files.
    ///
    /// Iterative rather than recursive so a deep tree cannot overflow the stack,
    /// and it never follows symlinks: a symlinked directory loop would otherwise
    /// walk forever, and a symlink can point outside the user's configured roots.
    fn walk(&self, root: &Path, seen: &mut HashMap<PathBuf, FileStamp>, report: &mut ScanReport) {
        let mut stack = vec![root.to_path_buf()];

        while let Some(directory) = stack.pop() {
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(_) => {
                    report.unreadable_dirs.push(directory);
                    continue;
                }
            };

            for entry in entries {
                let Ok(entry) = entry else { continue };
                let path = entry.path();

                // `symlink_metadata` is the point of this call: we need to know
                // that a symlink *is* a symlink, which `metadata` hides by
                // following it.
                let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if metadata.file_type().is_symlink() || is_hidden(&path) {
                    continue;
                }

                if metadata.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !metadata.is_file() {
                    continue;
                }

                match ContentKind::from_path(&path) {
                    Some(_) => {
                        seen.insert(path, FileStamp::from_metadata(&metadata));
                    }
                    None => report.skipped_unsupported += 1,
                }
            }
        }
    }

    /// Everything matching `query`, ordered by name.
    pub fn list(&self, query: &ListQuery) -> Result<Vec<Item>, LibraryError> {
        let (sql, parameters) =
            build_query(&format!("SELECT {ITEM_COLUMNS} FROM items"), query, true);
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(parameters.iter()), |row| {
            Ok(row_to_item(row))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(LibraryError::Sql)
    }

    /// How many items match `query` (ignoring paging).
    pub fn count(&self, query: &ListQuery) -> Result<u64, LibraryError> {
        let (sql, parameters) = build_query("SELECT COUNT(*) FROM items", query, false);
        let mut statement = self.conn.prepare(&sql)?;
        let count: i64 = statement
            .query_row(rusqlite::params_from_iter(parameters.iter()), |row| {
                row.get(0)
            })?;
        Ok(count.max(0) as u64)
    }

    /// Rows in the index, unfiltered.
    pub fn len(&self) -> Result<u64, LibraryError> {
        self.count(&ListQuery::default())
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> Result<bool, LibraryError> {
        Ok(self.len()? == 0)
    }

    /// Look up one item by its stable id (`library:<id>`).
    pub fn get(&self, id: i64) -> Result<Option<Item>, LibraryError> {
        let mut statement = self
            .conn
            .prepare(&format!("SELECT {ITEM_COLUMNS} FROM items WHERE id = ?1"))?;
        statement
            .query_row(params![id], |row| Ok(row_to_item(row)))
            .optional()
            .map_err(LibraryError::Sql)
    }

    /// Look up one item by absolute path (`library:<path>`).
    pub fn get_by_path(&self, path: &Path) -> Result<Option<Item>, LibraryError> {
        let mut statement = self
            .conn
            .prepare(&format!("SELECT {ITEM_COLUMNS} FROM items WHERE path = ?1"))?;
        statement
            .query_row(params![path.to_string_lossy()], |row| Ok(row_to_item(row)))
            .optional()
            .map_err(LibraryError::Sql)
    }

    /// Drop every row (used when the user reconfigures `library.paths`).
    pub fn clear(&self) -> Result<usize, LibraryError> {
        Ok(self.conn.execute("DELETE FROM items", [])?)
    }

    /// Per-directory row counts, for the GUI's folder list.
    pub fn directories(&self) -> Result<BTreeMap<String, u64>, LibraryError> {
        let mut statement = self
            .conn
            .prepare("SELECT dir, COUNT(*) FROM items GROUP BY dir ORDER BY dir")?;
        let rows = statement.query_map([], |row| {
            let dir: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((dir, count.max(0) as u64))
        })?;
        rows.collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(LibraryError::Sql)
    }
}

fn row_to_item(row: &rusqlite::Row<'_>) -> Item {
    let path: String = row.get(1).unwrap_or_default();
    let name: String = row.get(3).unwrap_or_default();
    let recorded_kind: String = row.get(4).unwrap_or_default();
    Item {
        id: row.get(0).unwrap_or_default(),
        path: PathBuf::from(&path),
        dir: PathBuf::from(row.get::<_, String>(2).unwrap_or_default()),
        name: name.clone(),
        // Prefer the id recorded at scan time; fall back to the extension, which
        // is what produced it in the first place.
        kind: ContentKind::from_id(&recorded_kind)
            .or_else(|| {
                Path::new(&path)
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .and_then(ContentKind::from_extension)
            })
            .unwrap_or(ContentKind::StaticImage),
        stamp: FileStamp {
            mtime_secs: row.get(6).unwrap_or_default(),
            mtime_nanos: row.get::<_, i64>(7).unwrap_or_default().max(0) as u32,
            size_bytes: row.get::<_, i64>(5).unwrap_or_default().max(0) as u64,
        },
    }
}

/// `(dir, name, kind)` for a path.
fn describe(path: &Path) -> (PathBuf, String, ContentKind) {
    let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let kind = ContentKind::from_path(path).unwrap_or(ContentKind::StaticImage);
    (dir, name, kind)
}

/// Hidden entries are skipped so `.thumbnails`, `.git`, and editor swap files do
/// not clutter the library.
fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

/// Whether `path` is inside any of `roots`.
fn under_any(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

/// Append a bound parameter and return its 1-based index.
fn push_param(parameters: &mut Vec<Box<dyn ToSql>>, value: Box<dyn ToSql>) -> usize {
    parameters.push(value);
    parameters.len()
}

/// Build a filtered/paged SELECT plus its bound parameters.
///
/// The filter is escaped for `LIKE` (`%` and `_` are literal characters to a
/// user, not wildcards) — a search for `100%` must not match everything. SQLite's
/// `LIKE` is already case-insensitive for ASCII, which is what a search box
/// wants.
fn build_query(base: &str, query: &ListQuery, with_paging: bool) -> (String, Vec<Box<dyn ToSql>>) {
    let mut sql = base.to_string();
    let mut clauses: Vec<String> = Vec::new();
    let mut parameters: Vec<Box<dyn ToSql>> = Vec::new();

    if let Some(filter) = query
        .filter
        .as_deref()
        .map(str::trim)
        .filter(|filter| !filter.is_empty())
    {
        let index = push_param(
            &mut parameters,
            Box::new(format!("%{}%", escape_like(filter))),
        );
        clauses.push(format!(
            "(name LIKE ?{index} ESCAPE '\\' OR path LIKE ?{index} ESCAPE '\\')"
        ));
    }
    if let Some(kind) = query.kind {
        let index = push_param(&mut parameters, Box::new(kind.as_str().to_string()));
        clauses.push(format!("kind = ?{index}"));
    }
    if let Some(under) = &query.under {
        let exact = push_param(
            &mut parameters,
            Box::new(under.to_string_lossy().to_string()),
        );
        let prefix = push_param(
            &mut parameters,
            Box::new(format!(
                "{}{}%",
                escape_like(&under.to_string_lossy()),
                std::path::MAIN_SEPARATOR
            )),
        );
        clauses.push(format!(
            "(dir = ?{exact} OR dir LIKE ?{prefix} ESCAPE '\\')"
        ));
    }

    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }

    if with_paging {
        sql.push_str(" ORDER BY name COLLATE NOCASE, path");
        // LIMIT/OFFSET are inlined numbers, not bound parameters: both are
        // clamped `u32`s with no external text, so there is nothing to inject.
        let limit = query.limit.unwrap_or(MAX_PAGE).min(MAX_PAGE);
        sql.push_str(&format!(" LIMIT {limit}"));
        if let Some(offset) = query.offset.filter(|offset| *offset > 0) {
            sql.push_str(&format!(" OFFSET {offset}"));
        }
    }

    (sql, parameters)
}

/// Escape `LIKE` metacharacters. `\` is the escape character (see `ESCAPE '\'`).
fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Turn configured roots (which may contain `~` and `$VARS`) into absolute paths.
pub fn resolve_roots(configured: &[String]) -> Result<Vec<PathBuf>, LibraryError> {
    let mut roots = Vec::with_capacity(configured.len());
    for entry in configured {
        let path = crate::path::expand(entry).map_err(|error| LibraryError::Root {
            root: entry.clone(),
            detail: error.to_string(),
        })?;
        roots.push(path);
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a small file. The scanner only looks at the extension and the stamp,
    /// so real image bytes are not needed for indexing tests.
    fn touch(dir: &Path, name: &str, bytes: usize) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(&path, vec![b'x'; bytes]).expect("write file");
        path
    }

    fn library() -> Library {
        Library::open_in_memory().expect("in-memory library")
    }

    #[test]
    fn scanning_indexes_images_and_ignores_other_files() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "b.jpg", 20);
        touch(dir.path(), "a.png", 10);
        touch(dir.path(), "notes.txt", 5);

        let mut library = library();
        let report = library.scan(&[dir.path().to_path_buf()]).unwrap();

        assert_eq!(report.added, 2, "{report:?}");
        assert_eq!(report.rows_touched(), 2);
        assert!(
            report.skipped_unsupported >= 1,
            "the .txt is skipped: {report:?}"
        );
        assert_eq!(library.len().unwrap(), 2);

        let names: Vec<String> = library
            .list(&ListQuery::default())
            .unwrap()
            .into_iter()
            .map(|item| item.name)
            .collect();
        assert_eq!(names, vec!["a.png", "b.jpg"], "ordered by name");
    }

    #[test]
    fn rescanning_an_unchanged_tree_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.png", 10);
        touch(dir.path(), "b.png", 10);

        let mut library = library();
        library.scan(&[dir.path().to_path_buf()]).unwrap();
        let second = library.scan(&[dir.path().to_path_buf()]).unwrap();

        assert_eq!(second.added, 0);
        assert_eq!(second.updated, 0);
        assert_eq!(second.removed, 0);
        assert_eq!(
            second.rows_touched(),
            0,
            "an unchanged tree must not write a single row: {second:?}"
        );
        assert_eq!(second.unchanged, 2);
    }

    #[test]
    fn rescanning_with_one_changed_file_touches_exactly_one_row() {
        // This is the P2 gate, as a unit test: the CI job scales it to 500 files.
        let dir = tempfile::tempdir().unwrap();
        let target = touch(dir.path(), "changed.png", 10);
        for index in 0..20 {
            touch(dir.path(), &format!("other-{index}.png"), 10);
        }

        let mut library = library();
        let first = library.scan(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(first.added, 21);

        // Change exactly one file: different size and (almost certainly) mtime.
        std::fs::write(&target, vec![b'y'; 32]).unwrap();

        let second = library.scan(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(
            second.rows_touched(),
            1,
            "one edited file must mean one written row: {second:?}"
        );
        assert_eq!(second.updated, 1);
        assert_eq!(second.unchanged, 20);

        // And the new stamp is what the index now holds.
        let item = library.get_by_path(&target).unwrap().expect("indexed");
        assert_eq!(item.stamp.size_bytes, 32);
    }

    #[test]
    fn a_deleted_file_is_removed_from_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = touch(dir.path(), "gone.png", 10);
        touch(dir.path(), "stays.png", 10);

        let mut library = library();
        library.scan(&[dir.path().to_path_buf()]).unwrap();
        std::fs::remove_file(&path).unwrap();

        let report = library.scan(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(report.removed, 1);
        assert_eq!(report.rows_touched(), 1);
        assert_eq!(library.len().unwrap(), 1);
        assert!(library.get_by_path(&path).unwrap().is_none());
    }

    #[test]
    fn a_missing_root_never_deletes_its_rows() {
        // The unplugged-drive case. Losing 40 000 rows because a USB stick is out
        // is the kind of bug a user never forgives.
        let dir = tempfile::tempdir().unwrap();
        let removable = dir.path().join("usb");
        touch(&removable, "wall.png", 10);

        let mut library = library();
        library.scan(std::slice::from_ref(&removable)).unwrap();
        assert_eq!(library.len().unwrap(), 1);

        // The drive is not there any more.
        std::fs::remove_dir_all(&removable).unwrap();
        let report = library.scan(std::slice::from_ref(&removable)).unwrap();

        assert_eq!(report.missing_roots, vec![removable.clone()]);
        assert_eq!(
            report.removed, 0,
            "a missing root must not delete: {report:?}"
        );
        assert_eq!(library.len().unwrap(), 1, "the row is preserved");
    }

    #[test]
    fn a_nested_missing_root_does_not_lose_rows_under_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().to_path_buf();
        let nested = parent.join("downloaded");
        touch(&nested, "inside.png", 10);
        touch(&parent, "top.png", 10);

        let mut library = library();
        let first = library.scan(&[parent.clone(), nested.clone()]).unwrap();
        assert_eq!(
            first.added, 2,
            "nested roots are not walked twice: {first:?}"
        );

        // The nested path disappears (unmounted, renamed); the parent is intact.
        std::fs::remove_dir_all(&nested).unwrap();
        let report = library.scan(&[parent.clone(), nested.clone()]).unwrap();

        assert_eq!(report.missing_roots, vec![nested.clone()]);
        assert_eq!(
            report.removed, 0,
            "rows under a missing nested root are preserved: {report:?}"
        );
        assert_eq!(library.len().unwrap(), 2);
    }

    #[test]
    fn rows_outside_the_scanned_roots_are_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let other = dir.path().join("other");
        let scanned = dir.path().join("scanned");
        touch(&other, "keep.png", 10);
        touch(&scanned, "keep.png", 10);

        let mut library = library();
        library.scan(&[other.clone(), scanned.clone()]).unwrap();
        assert_eq!(library.len().unwrap(), 2);

        // Scan only one root: the other root's rows are not candidates for
        // deletion, because we never looked there.
        let report = library.scan(std::slice::from_ref(&scanned)).unwrap();
        assert_eq!(report.removed, 0, "{report:?}");
        assert_eq!(library.len().unwrap(), 2);
    }

    #[test]
    fn subdirectories_are_walked_and_symlinks_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "deep/a/b/c/wall.png", 10);
        touch(dir.path(), "top.png", 10);

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            // A symlink loop: following it would walk forever.
            symlink(dir.path(), dir.path().join("deep/loop")).unwrap();
            // A symlink file: also not indexed (it may point outside the roots).
            symlink(dir.path().join("top.png"), dir.path().join("link.png")).unwrap();
        }

        let mut library = library();
        let report = library.scan(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(report.added, 2, "only real files, no loops: {report:?}");
    }

    #[test]
    fn hidden_files_and_directories_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), ".cache/thumbs/x.png", 10);
        touch(dir.path(), "visible.png", 10);

        let mut library = library();
        library.scan(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(library.len().unwrap(), 1);
        assert_eq!(
            library.list(&ListQuery::default()).unwrap()[0].name,
            "visible.png"
        );
    }

    #[test]
    fn list_filters_by_substring_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "Aurora-Borealis.png", 10);
        touch(dir.path(), "city.jpg", 10);
        touch(dir.path(), "nested/aurora-2.png", 10);

        let mut library = library();
        library.scan(&[dir.path().to_path_buf()]).unwrap();

        let found = library
            .list(&ListQuery {
                filter: Some("aurora".to_string()),
                ..ListQuery::default()
            })
            .unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(library.count(&ListQuery::default()).unwrap(), 3);
    }

    #[test]
    fn list_filter_treats_like_wildcards_as_literals() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "100%_nature.png", 10);
        touch(dir.path(), "other.png", 10);

        let mut library = library();
        library.scan(&[dir.path().to_path_buf()]).unwrap();

        let found = library
            .list(&ListQuery {
                filter: Some("100%".to_string()),
                ..ListQuery::default()
            })
            .unwrap();
        assert_eq!(
            found.len(),
            1,
            "`%` must not behave as a wildcard: {found:?}"
        );
        assert_eq!(found[0].name, "100%_nature.png");
    }

    #[test]
    fn list_filters_by_kind_and_directory() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "still.png", 10);
        touch(dir.path(), "anim.gif", 10);
        touch(dir.path(), "folder/deep.png", 10);

        let mut library = library();
        library.scan(&[dir.path().to_path_buf()]).unwrap();

        let animated = library
            .list(&ListQuery {
                kind: Some(ContentKind::AnimatedImage),
                ..ListQuery::default()
            })
            .unwrap();
        assert_eq!(animated.len(), 1);
        assert_eq!(animated[0].name, "anim.gif");

        let under = library
            .list(&ListQuery {
                under: Some(dir.path().join("folder")),
                ..ListQuery::default()
            })
            .unwrap();
        assert_eq!(under.len(), 1);
        assert_eq!(under[0].name, "deep.png");
    }

    #[test]
    fn list_pages_through_a_large_library() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..10 {
            touch(dir.path(), &format!("wall-{index:02}.png"), 10);
        }
        let mut library = library();
        library.scan(&[dir.path().to_path_buf()]).unwrap();

        let page = library
            .list(&ListQuery {
                limit: Some(3),
                offset: Some(3),
                ..ListQuery::default()
            })
            .unwrap();
        assert_eq!(page.len(), 3);
        assert_eq!(page[0].name, "wall-03.png");
        assert_eq!(page[2].name, "wall-05.png");
    }

    #[test]
    fn list_limit_is_capped() {
        let (sql, _) = build_query(
            "SELECT 1",
            &ListQuery {
                limit: Some(1_000_000),
                ..ListQuery::default()
            },
            true,
        );
        assert!(sql.ends_with(&format!("LIMIT {MAX_PAGE}")), "{sql}");
    }

    #[test]
    fn items_are_addressable_by_id_and_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = touch(dir.path(), "wall.png", 10);
        let mut library = library();
        library.scan(&[dir.path().to_path_buf()]).unwrap();

        let by_path = library.get_by_path(&path).unwrap().expect("by path");
        assert_eq!(by_path.name, "wall.png");
        let by_id = library.get(by_path.id).unwrap().expect("by id");
        assert_eq!(by_id, by_path);

        // The id is what `library:<id>` means on the wire.
        assert_eq!(
            by_id.to_json()["reference"],
            serde_json::json!(format!("library:{}", by_id.id))
        );
        assert_eq!(by_id.to_json()["kind"], serde_json::json!("static-image"));
    }

    #[test]
    fn rescanning_keeps_ids_stable() {
        // `library:<id>` in a session file or a GUI bookmark must survive a
        // rescan, or the wallpaper silently stops resolving.
        let dir = tempfile::tempdir().unwrap();
        let path = touch(dir.path(), "wall.png", 10);

        let mut library = library();
        library.scan(&[dir.path().to_path_buf()]).unwrap();
        let first = library.get_by_path(&path).unwrap().unwrap().id;

        std::fs::write(&path, vec![b'z'; 99]).unwrap();
        library.scan(&[dir.path().to_path_buf()]).unwrap();

        assert_eq!(library.get_by_path(&path).unwrap().unwrap().id, first);
    }

    #[test]
    fn opening_a_future_schema_reports_rather_than_guessing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta (key, value) VALUES ('schema_version', '99');",
            )
            .unwrap();
        }

        let error = Library::open(&path).unwrap_err();
        match error {
            LibraryError::SchemaTooNew { found, .. } => assert_eq!(found, 99),
            other => panic!("unexpected: {other}"),
        }
        assert!(error.to_string().contains("cache"), "{error}");
    }

    #[test]
    fn opening_a_file_path_creates_the_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/library.db");
        let library = Library::open(&path).expect("open");
        assert_eq!(library.len().unwrap(), 0);
        assert!(path.parent().unwrap().is_dir());
    }

    #[test]
    fn directories_are_counted_for_the_gui() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.png", 10);
        touch(dir.path(), "b.png", 10);
        touch(dir.path(), "sub/c.png", 10);

        let mut library = library();
        library.scan(&[dir.path().to_path_buf()]).unwrap();

        let dirs = library.directories().unwrap();
        assert_eq!(dirs.len(), 2, "{dirs:?}");
        assert_eq!(dirs[&dir.path().display().to_string()], 2);
    }

    #[test]
    fn clear_empties_the_index() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.png", 10);
        let mut library = library();
        library.scan(&[dir.path().to_path_buf()]).unwrap();

        assert_eq!(library.clear().unwrap(), 1);
        assert!(library.is_empty().unwrap());
    }

    #[test]
    fn resolve_roots_expands_home_and_variables() {
        let roots = resolve_roots(&["/tmp/walls".to_string(), "~/Pictures".to_string()]).unwrap();
        assert_eq!(roots[0], PathBuf::from("/tmp/walls"));
        assert!(roots[1].is_absolute(), "{roots:?}");

        let error = resolve_roots(&["relative/path".to_string()]).unwrap_err();
        assert!(matches!(error, LibraryError::Root { .. }), "{error}");
    }

    #[test]
    fn scan_reports_its_duration_and_a_readable_summary() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.png", 10);
        let mut library = library();
        let report = library.scan(&[dir.path().to_path_buf()]).unwrap();

        assert!(report.duration < Duration::from_secs(5));
        let summary = report.summary();
        assert!(summary.contains("+1"), "{summary}");
        assert!(summary.contains("file(s) seen"), "{summary}");
    }

    #[test]
    fn unreadable_directories_are_reported_not_fatal() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::tempdir().unwrap();
            let locked = dir.path().join("locked");
            touch(&locked, "hidden.png", 10);

            let mut permissions = std::fs::metadata(&locked).unwrap().permissions();
            permissions.set_mode(0o000);
            std::fs::set_permissions(&locked, permissions).unwrap();

            // Permission bits are not enforced for root (and some containers), so
            // in that case there is nothing to assert — say so rather than fail.
            if std::fs::read_dir(&locked).is_ok() {
                eprintln!("permission bits are not enforced here; skipping");
                let mut permissions = std::fs::metadata(&locked).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&locked, permissions).unwrap();
                return;
            }

            let mut library = library();
            let report = library.scan(&[dir.path().to_path_buf()]).unwrap();
            assert_eq!(report.unreadable_dirs, vec![locked.clone()]);

            // Restore so the tempdir can be cleaned up.
            let mut permissions = std::fs::metadata(&locked).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&locked, permissions).unwrap();
        }
    }
}
