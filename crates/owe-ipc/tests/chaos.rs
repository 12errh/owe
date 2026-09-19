//! Those tests that prove the IPC layer survives hostile input.
//!
//! They implement TRD FR-CORE-6 ("a malformed message gets an error frame, never a
//! disconnect") and TRD NFR-REL-2 ("any IPC client may crash or disconnect at any
//! time without corrupting daemon state").

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use owe_ipc::protocol::{self, method};
use owe_ipc::{
    Capabilities, Client, ClientError, ErrorBody, ErrorCode, Handler, HelloParams, HelloReply,
    MAX_FRAME_BYTES, RequestFrame, Server, ServerError,
};
use serde_json::{Value, json};

#[derive(Debug)]
struct TestHandler;

impl Handler for TestHandler {
    fn handle(&self, request: &RequestFrame) -> Result<Value, ErrorBody> {
        match request.method.as_str() {
            method::HELLO => {
                let params: HelloParams = request.params_as()?;
                let schema = protocol::negotiate(&params.schema).ok_or_else(|| {
                    ErrorBody::unsupported("no shared schema major for this client")
                })?;
                let reply = HelloReply {
                    server_version: "test-0.1.0".to_string(),
                    schema,
                    capabilities: Capabilities {
                        methods: vec![method::HELLO.to_string(), method::DAEMON_KILL.to_string()],
                        shell_backends: vec!["hyprland".to_string()],
                        content_kinds: vec!["static-image".to_string()],
                        media_backends: vec!["auto".to_string()],
                        unavailable: Vec::new(),
                    },
                };
                serde_json::to_value(reply).map_err(|error| ErrorBody::internal(error.to_string()))
            }
            method::DAEMON_KILL => Ok(json!({ "shutting_down": true })),
            other => Err(ErrorBody::unsupported(format!(
                "`{other}` is not implemented in this build yet"
            ))),
        }
    }
}

struct TestServer {
    _dir: tempfile::TempDir,
    server: Arc<Server>,
    socket: PathBuf,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let socket = dir.path().join("socket");
        let handler: Arc<dyn Handler> = Arc::new(TestHandler);
        let server = Arc::new(Server::bind(&socket, handler).expect("bind"));
        let thread = {
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                let _ = server.serve_blocking();
            })
        };
        wait_for_socket(&socket);
        Self {
            _dir: dir,
            server,
            socket,
            thread: Some(thread),
        }
    }

    fn client(&self) -> Client {
        Client::connect(&self.socket).expect("connect")
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.server.request_shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn wait_for_socket(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if UnixStream::connect(path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("server never listened on {}", path.display());
}

#[test]
fn hello_reports_version_schema_and_capabilities() {
    let server = TestServer::start();
    let mut client = server.client();

    let reply = client.hello("test-client", "9.9.9").expect("hello");
    assert_eq!(reply.server_version, "test-0.1.0");
    assert_eq!(reply.schema, protocol::SchemaVersion::CURRENT);
    assert_eq!(
        reply.capabilities.shell_backends,
        vec!["hyprland".to_string()]
    );
}

#[test]
fn unused_methods_answer_unsupported_and_the_connection_survives() {
    let server = TestServer::start();
    let mut client = server.client();
    client.hello("test-client", "0.1.0").expect("hello");

    let error = client
        .call(method::WALLPAPER_SET, json!({}))
        .expect_err("stub method must fail");
    match error {
        ClientError::Server(body) => {
            assert_eq!(body.code, ErrorCode::Unsupported);
            assert!(body.msg.contains("wallpaper.set"), "{}", body.msg);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // Same connection, still usable (FR-CORE-6 spirit).
    let reply = client.hello("test-client", "0.1.0").expect("second hello");
    assert_eq!(reply.server_version, "test-0.1.0");
}

#[test]
fn malformed_line_gets_an_error_frame_not_a_disconnect() {
    let server = TestServer::start();
    let mut client = server.client();

    let reply = client.raw_call(b"{ this is not json").expect("raw call");
    assert_eq!(reply["err"]["code"], json!("BAD_REQUEST"));

    // The socket was not closed: a proper request still works.
    let hello = client
        .hello("test-client", "0.1.0")
        .expect("hello after garbage");
    assert_eq!(hello.server_version, "test-0.1.0");
}

#[test]
fn oversize_frame_is_reported_then_the_connection_closes() {
    let server = TestServer::start();
    let mut client = server.client();

    let mut blob = vec![b'x'; MAX_FRAME_BYTES + 64];
    blob.push(b'\n');
    let reply = client
        .raw_call(&blob)
        .expect("error reply for oversize frame");
    assert_eq!(reply["err"]["code"], json!("BAD_REQUEST"));
    assert!(
        reply["err"]["msg"].as_str().unwrap().contains("exceeds"),
        "{reply}"
    );

    // Documented exception: this stream cannot be resynchronised, so it is closed.
    let after = client.hello("test-client", "0.1.0");
    assert!(
        after.is_err(),
        "expected the connection to be closed: {after:?}"
    );
}

#[test]
fn clients_are_served_independently() {
    let server = TestServer::start();
    let mut first = server.client();
    let mut second = server.client();

    // The real assertion: two clients interleave requests on one socket and both
    // are served correctly. (This failed intermittently in CI run 9 in a weaker
    // form that asserted an instantaneous connection count, which races with
    // connection teardown.)
    first.hello("first", "0.1.0").expect("first hello");
    second.hello("second", "0.1.0").expect("second hello");
    first
        .call(method::DAEMON_KILL, json!({}))
        .expect("kill via first");
    second
        .call(method::DAEMON_KILL, json!({}))
        .expect("kill via second");

    // A connection thread lives until its *client* disconnects (the 30s read
    // timeout is the backstop, not the mechanism), so draining requires the
    // clients to let go first. Only then must the count reach zero — polled with
    // a deadline, never sampled at one arbitrary instant, because teardown is
    // asynchronous by design.
    drop(first);
    drop(second);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let active = server.server.active_connections();
        if active == 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "connections never drained: {active} still active"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(server.server.active_connections(), 0);
}

#[test]
fn a_client_that_dies_mid_frame_is_harmless() {
    let server = TestServer::start();

    {
        let mut socket = UnixStream::connect(&server.socket).expect("connect");
        use std::io::Write;
        socket.write_all(b"{\"v\":1,\"id\":\"c1\",\"met").unwrap();
        // Dropped without finishing the frame: truncated JSON, no newline.
    }

    // Give the server a moment to notice the disconnect, then verify it is alive.
    let mut client = server.client();
    let hello = client.hello("after-crash", "0.1.0").expect("hello");
    assert_eq!(hello.server_version, "test-0.1.0");
}

#[test]
fn a_second_daemon_refuses_to_steal_a_live_socket() {
    let server = TestServer::start();
    let handler: Arc<dyn Handler> = Arc::new(TestHandler);

    let error = Server::bind(&server.socket, handler).expect_err("must refuse");
    match error {
        ServerError::AlreadyRunning(path) => assert_eq!(path, server.socket),
        other => panic!("unexpected: {other}"),
    }
}

#[test]
fn a_stale_socket_file_is_replaced() {
    let dir = tempfile::tempdir().expect("temp dir");
    let socket = dir.path().join("socket");
    // A leftover plain file (crashed daemon, or someone's cat > socket).
    std::fs::write(&socket, b"stale").expect("write stale file");

    let handler: Arc<dyn Handler> = Arc::new(TestHandler);
    let server = Server::bind(&socket, handler).expect("bind over stale file");
    assert_eq!(server.socket_path(), socket);
    assert!(socket.exists());

    // Permissions are 0600 on the socket (TRD NFR-SEC-1).
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "socket mode was {mode:o}");
}

#[test]
fn socket_creation_is_allowed_only_for_the_owner_directory() {
    let dir = tempfile::tempdir().expect("temp dir");
    let nested = dir.path().join("owe");
    let socket = nested.join("socket");

    let handler: Arc<dyn Handler> = Arc::new(TestHandler);
    let _server = Server::bind(&socket, handler).expect("bind creates the directory");

    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "runtime dir mode was {mode:o}");
}
