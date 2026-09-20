//! The Phase 2 gate measurement for library scale (IMPLEMENTATION-PLAN, Phase 2).
//!
//! > 500-file library scan < 2 s warm; rescan with 1 changed file touches only that
//! > row (measured in CI, not HW-dependent).
//!
//! It lives here, as an ordinary test, for a specific reason: the gate must be
//! *enforced*, and a number pasted into a document is not enforced by anything. If
//! a future change makes the scanner quadratic, or makes it re-read file contents,
//! or makes the rescan rewrite every row, this test fails in the same CI job that
//! runs every other test — not in a benchmark report nobody gates on.
//!
//! The files are deliberately tiny and content-free: the scanner's contract is that
//! it stats files and indexes metadata, never that it decodes them. A 500-file scan
//! that reads pixel data would be a design regression, and this test would catch it
//! as a timeout rather than silently.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use owe_core::library::{Library, ListQuery};

/// Fixture size from the gate wording.
const FILES: usize = 500;

/// Gate budget for one scan of the fixture.
const BUDGET: Duration = Duration::from_secs(2);

/// Write `FILES` small images across a few subdirectories.
///
/// Subdirectories matter: a walk that only reads the top level would otherwise pass.
fn seed(root: &Path) -> Vec<PathBuf> {
    let mut written = Vec::with_capacity(FILES);
    for index in 0..FILES {
        let directory = root.join(format!("set-{}", index % 10));
        let path = directory.join(format!("wall-{index:04}.png"));
        std::fs::create_dir_all(&directory).expect("create directory");
        std::fs::write(&path, b"not really a png, and that is the point").expect("write file");
        written.push(path);
    }
    written
}

fn library() -> Library {
    Library::open_in_memory().expect("in-memory library")
}

#[test]
fn a_five_hundred_file_library_scans_inside_the_gate_budget() {
    let dir = tempfile::tempdir().expect("temp dir");
    seed(dir.path());
    let roots = [dir.path().to_path_buf()];

    let mut library = library();

    // Cold: index construction plus the walk.
    let cold = library.scan(&roots).expect("cold scan");
    assert_eq!(cold.added, FILES, "{cold:?}");
    assert_eq!(cold.skipped_unsupported, 0, "{cold:?}");

    // Warm: the same tree, already indexed — this is the number the gate names.
    let warm_started = Instant::now();
    let warm = library.scan(&roots).expect("warm scan");
    let warm_elapsed = warm_started.elapsed();

    assert_eq!(warm.unchanged, FILES, "{warm:?}");
    assert_eq!(
        warm.rows_touched(),
        0,
        "a warm rescan of an unchanged tree must write nothing: {warm:?}"
    );
    assert!(
        warm_elapsed < BUDGET,
        "warm scan of {FILES} files took {warm_elapsed:?}, over the {BUDGET:?} gate budget"
    );

    // The library is actually usable at that size, not just fast to index.
    let page = library
        .list(&ListQuery {
            limit: Some(25),
            ..ListQuery::default()
        })
        .expect("page");
    assert_eq!(page.len(), 25);
    assert_eq!(library.len().expect("count"), FILES as u64);

    eprintln!(
        "gate: {FILES}-file warm scan {warm_elapsed:?} (budget {BUDGET:?}); \
         cold scan {:?}",
        cold.duration
    );
}

#[test]
fn one_changed_file_in_five_hundred_touches_exactly_one_row() {
    // The other half of the gate: an incremental rescan must be incremental in the
    // *writes* it performs, not merely in the time it takes.
    let dir = tempfile::tempdir().expect("temp dir");
    let written = seed(dir.path());
    let roots = [dir.path().to_path_buf()];

    let mut library = library();
    library.scan(&roots).expect("cold scan");

    // Change exactly one file, in a way the (mtime, size) stamp must notice.
    let changed = &written[FILES / 2];
    let replacement = b"a different length entirely";
    std::fs::write(changed, replacement).expect("rewrite");

    let report = library.scan(&roots).expect("incremental scan");
    assert_eq!(report.rows_touched(), 1, "{report:?}");
    assert_eq!(report.updated, 1, "{report:?}");
    assert_eq!(report.unchanged, FILES - 1, "{report:?}");

    let item = library
        .get_by_path(changed)
        .expect("lookup")
        .expect("indexed");
    assert_eq!(
        item.stamp.size_bytes,
        replacement.len() as u64,
        "the new stamp must be stored"
    );
}
