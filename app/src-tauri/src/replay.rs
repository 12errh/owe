//! Replay harness: the GUI's command layer driven by **recorded** daemon traffic.
//!
//! # Why both a stub and a recording
//!
//! The unit tests in `main.rs` host a hand-written stub daemon. That proves the GUI
//! sends and reads the shapes *we believe* the daemon speaks — and nothing more: if
//! the daemon's real reply disagrees with our belief, the stub happily confirms it.
//!
//! This module closes that gap. `scripts/record-gui-fixtures.sh` captures the real
//! `owed` binary's replies (headless, no compositor needed) into
//! `tests/fixtures/daemon-session.json`; the tests here serve those recorded bytes
//! back through the same `Handler` interface the daemon implements and assert that
//! the GUI's parsing produces usable views. A daemon-side wire change therefore
//! fails a GUI test until the fixture is deliberately re-recorded — a reviewable
//! diff of real traffic.
//!
//! # What it still cannot prove
//!
//! Nothing here drives the webview: the DOM wiring between a click and `invoke` is
//! still verified by hand (the same honest limitation the P1 gate records for the
//! apply button). What is automated is everything below the click.
//!
//! # Requests are asserted, replies are recorded
//!
//! The fixture stores replies only. The harness records what the GUI *asked* for,
//! so the tests can assert the parameters the GUI constructs — which is the half of
//! the contract the recording cannot supply, because the recorder drove the daemon
//! through `owectl`, not through this code.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use owe_ipc::Handler;
use owe_ipc::protocol::{ErrorBody, ErrorCode, RequestFrame};
use serde_json::Value;

/// The literal a recorded reply uses for the fixture directory.
const FIXTURE_DIR: &str = "$FIXTURE_DIR";

/// A recorded daemon session, served as an [`owe_ipc::Handler`].
#[derive(Debug)]
pub struct Fixture {
    replies: BTreeMap<String, Value>,
    dir: PathBuf,
    asked: Mutex<Vec<(String, Value)>>,
}

impl Fixture {
    /// Load a recording written by `scripts/record-gui-fixtures.sh`.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read `{}`: {error}", path.display()))?;
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|error| format!("`{}` is not valid JSON: {error}", path.display()))?;

        let replies = parsed
            .get("replies")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("`{}` has no `replies` object", path.display()))?;

        Ok(Self {
            replies: replies
                .iter()
                .map(|(method, reply)| (method.clone(), reply.clone()))
                .collect(),
            dir: path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
            asked: Mutex::new(Vec::new()),
        })
    }

    /// The fixture directory, where `thumb.png` lives.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Everything the GUI asked for, in order.
    pub fn asked(&self) -> Vec<(String, Value)> {
        self.asked
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// The methods the GUI called, in order.
    pub fn asked_methods(&self) -> Vec<String> {
        self.asked().into_iter().map(|(method, _)| method).collect()
    }

    /// The recorded reply for one method, with `$FIXTURE_DIR` resolved.
    pub fn reply_for(&self, method: &str) -> Option<Value> {
        self.replies
            .get(method)
            .map(|reply| substitute(reply, &self.dir))
    }

    /// Whether the recording covers a method at all.
    pub fn covers(&self, method: &str) -> bool {
        self.replies.contains_key(method)
    }
}

impl Handler for Fixture {
    fn handle(&self, request: &RequestFrame) -> Result<Value, ErrorBody> {
        if let Ok(mut asked) = self.asked.lock() {
            asked.push((request.method.clone(), request.params.clone()));
        }
        self.reply_for(&request.method).ok_or_else(|| {
            // Naming the recorder is the point: a missing reply is not a protocol
            // error to debug, it is a fixture that needs re-recording.
            ErrorBody::new(
                ErrorCode::Unsupported,
                format!(
                    "the recording has no reply for `{}`; re-run \
                     scripts/record-gui-fixtures.sh and commit the result",
                    request.method
                ),
            )
        })
    }
}

/// Replace `$FIXTURE_DIR` throughout a recorded reply.
fn substitute(value: &Value, dir: &Path) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace(FIXTURE_DIR, &dir.display().to_string())),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| substitute(item, dir)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), substitute(item, dir)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Path of the committed recording.
fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/daemon-session.json")
}

// ---------------------------------------------------------------------- tests

use std::sync::Arc;

use crate::{
    LibraryQuery, library_index_from, library_scan_from, library_thumb_from, list_outputs_from,
    probe_socket,
};

/// A fixture hosted on a temporary socket, stopped when this is dropped.
struct Running {
    shutdown: owe_ipc::Shutdown,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.shutdown.request();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Host a fixture on a temporary socket, returning the socket and its guard.
///
/// The temp dir is returned too: dropping it early would unlink the socket while a
/// test is still talking to it.
fn start(fixture: Arc<Fixture>) -> (PathBuf, tempfile::TempDir, Running) {
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("socket");
    let server = owe_ipc::Server::bind(&socket, fixture).expect("bind fixture server");
    let shutdown = server.shutdown_signal();
    let handle = std::thread::spawn(move || {
        let _ = server.serve_blocking();
    });
    (
        socket,
        dir,
        Running {
            shutdown,
            handle: Some(handle),
        },
    )
}

fn load() -> Arc<Fixture> {
    Arc::new(Fixture::load(&fixture_path()).expect("recorded fixture"))
}

#[test]
fn the_recording_covers_every_method_the_gui_v1_screens_call() {
    let fixture = load();
    for method in [
        "hello",
        "library.scan",
        "library.list",
        "library.thumb",
        "outputs.list",
    ] {
        assert!(
            fixture.covers(method),
            "the recording has no reply for `{method}`; re-run \
             scripts/record-gui-fixtures.sh"
        );
    }
}

#[test]
fn a_recorded_session_replays_through_the_command_layer() {
    let fixture = load();
    let (socket, _dir, _running) = start(Arc::clone(&fixture));

    // `hello` first: this is the status bar's exact code path.
    let status = probe_socket(&socket);
    assert!(
        status.connected,
        "the recorded hello must satisfy the probe"
    );
    assert_eq!(status.server_version.as_deref(), Some("0.1.0"));
    let recorded_hello = fixture.reply_for("hello").expect("hello reply");
    assert_eq!(
        status.transitions,
        recorded_hello["capabilities"]["transitions"]
            .as_array()
            .expect("recorded transitions")
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>(),
        "the picker must offer exactly what the real daemon advertised"
    );

    // The library page, parsed out of the daemon's real bytes. The query mirrors
    // what the grid sends: a page number and a page size, no filter.
    let query = LibraryQuery {
        page: Some(1),
        per_page: Some(60),
        ..LibraryQuery::default()
    };
    let page = library_index_from(&socket, &query).expect("library page");
    let recorded_list = fixture.reply_for("library.list").expect("list reply");
    assert_eq!(
        page.total,
        recorded_list["total"].as_u64().unwrap_or(0),
        "the page total must come from the daemon, not from the page contents"
    );
    assert_eq!(
        page.items.len(),
        recorded_list["items"].as_array().unwrap().len()
    );
    assert_eq!(page.thumbnail_size, 256);
    assert_eq!(
        page.roots,
        recorded_list["roots"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>(),
        "the UI tells the user which folders are indexed; it must be the daemon's list"
    );
    for item in &page.items {
        assert!(item.name.ends_with(".png"), "{item:?}");
        assert_eq!(
            item.reference,
            format!("library:{}", item.id),
            "every grid cell applies by the reference the daemon printed"
        );
        assert_eq!(item.kind, "static-image");
        assert!(item.bytes > 0, "a real file has real bytes: {item:?}");
    }

    // A scan summary, from the recorded scan.
    let summary = library_scan_from(&socket, None).expect("scan summary");
    let recorded_scan = fixture.reply_for("library.scan").expect("scan reply");
    assert_eq!(summary.summary, recorded_scan["summary"].as_str().unwrap());
    assert_eq!(
        summary.files_seen,
        recorded_scan["files_seen"].as_u64().unwrap()
    );

    // Outputs. The recording was made headless, so this asserts the honest empty
    // shape a compositor-less daemon reports rather than inventing outputs.
    let outputs = list_outputs_from(&socket).expect("outputs");
    let recorded_outputs = fixture.reply_for("outputs.list").expect("outputs reply");
    assert_eq!(
        outputs.outputs.len(),
        recorded_outputs["outputs"].as_array().unwrap().len()
    );
    assert_eq!(
        outputs.paused,
        recorded_outputs["paused"].as_bool().unwrap()
    );

    // The thumbnail: real PNG bytes the daemon really encoded, handed to the
    // webview as a data URL without it ever touching the filesystem.
    let id = page.items[0].id;
    let thumbnail = library_thumb_from(&socket, id).expect("thumbnail");
    assert_eq!(
        thumbnail.path,
        fixture.dir().join("thumb.png").display().to_string(),
        "the recorded path must resolve into the fixture directory"
    );
    assert_eq!(thumbnail.size, 256);
    assert!(
        thumbnail.data_url.starts_with("data:image/png;base64,"),
        "{}",
        &thumbnail.data_url[..thumbnail.data_url.len().min(40)]
    );
    assert!(
        thumbnail.data_url.len() > 200,
        "a real 256px PNG is not a stub; got {} bytes of data URL",
        thumbnail.data_url.len()
    );

    // Finally, the request side: the GUI must have called these methods, and it
    // must have sent the parameters the daemon documents.
    let methods = fixture.asked_methods();
    let commands: Vec<&str> = methods
        .iter()
        .map(String::as_str)
        .filter(|method| *method != "hello")
        .collect();
    assert_eq!(
        commands,
        vec![
            "library.list",
            "library.scan",
            "outputs.list",
            "library.thumb"
        ],
        "the command layer's call sequence changed"
    );
    assert_eq!(
        methods.iter().filter(|method| *method == "hello").count(),
        commands.len() + 1,
        "every command must open its own connection and handshake first \
         (the probe accounts for the extra one)"
    );
    let asked = fixture.asked();
    let library_list = asked
        .iter()
        .find(|(method, _)| method == "library.list")
        .expect("library.list was asked");
    assert_eq!(library_list.1["page"], serde_json::json!(1));
    assert_eq!(library_list.1["per_page"], serde_json::json!(60));
    assert!(
        library_list.1.get("filter").is_none(),
        "an empty search must not become a filter that matches nothing"
    );
    let thumb = asked
        .iter()
        .find(|(method, _)| method == "library.thumb")
        .expect("library.thumb was asked");
    assert_eq!(thumb.1["id"], serde_json::json!(id));
}

#[test]
fn an_unrecorded_method_is_reported_as_a_missing_recording() {
    // The harness must not silently answer "ok" for something it never recorded:
    // that would turn a wire change into a passing test.
    let fixture = load();
    let (socket, _dir, _running) = start(Arc::clone(&fixture));

    // `wallpaper.set` is deliberately absent from the recording: the GUI's applies
    // are covered by the stub tests, which can assert the request *and* reply.
    let error = crate::assign_wallpaper_to(&socket, "/walls/a.png", &[], None)
        .expect_err("the recording has no wallpaper.set reply");
    assert_eq!(error.code, "UNSUPPORTED");
    assert!(
        error.message.contains("record-gui-fixtures.sh"),
        "the failure must say how to fix it: {error:?}"
    );
}
