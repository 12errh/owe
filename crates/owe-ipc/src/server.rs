//! The IPC server: socket lifecycle, connection handling, graceful shutdown.
//!
//! Threading (ADR-004, documented deviation for P0): the listener is a plain
//! `UnixListener`, accepted connections get one short-lived thread each. There
//! are at most a handful of local clients, so this is cheap and — importantly —
//! testable without a compositor. P1 introduces calloop for the Wayland sources
//! and will register the listener as a calloop source via
//! [`Server::accept_pending`]; the connection behaviour stays identical.

use std::io::{BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use thiserror::Error;

use crate::frame::{self, FrameDecoder, FrameError};
use crate::protocol::{self, ErrorBody, ReplyFrame, RequestFrame};

/// How long the accept loop sleeps when there is nothing to accept.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// A cloneable shutdown signal shared between the server and whatever must be
/// able to stop it (the daemon's IPC handler, a signal handler, a supervisor).
///
/// This exists because two independent flags is a bug: `daemon.kill` flipped
/// the handler's flag while the accept loop watched its own, so the daemon kept
/// running. One object, one truth.
#[derive(Debug, Clone, Default)]
pub struct Shutdown {
    flag: Arc<AtomicBool>,
}

impl Shutdown {
    /// Create a fresh, un-requested signal.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request shutdown (idempotent).
    pub fn request(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Whether shutdown has been requested.
    pub fn is_requested(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

/// Errors from binding or serving the socket.
#[derive(Debug, Error)]
pub enum ServerError {
    /// Underlying I/O failure.
    #[error("ipc io error: {0}")]
    Io(#[from] std::io::Error),

    /// Another daemon is already listening on this socket.
    #[error("another owed instance is already listening on {0}")]
    AlreadyRunning(PathBuf),

    /// The socket path has no parent directory (cannot happen for XDG paths).
    #[error("ipc socket path {0} has no parent directory")]
    NoParent(PathBuf),
}

/// What the daemon must implement to serve IPC requests.
///
/// Implementations must be cheap and non-blocking: they run on the connection
/// thread. State shared with the render engine belongs behind an `Arc` with
/// interior mutability, and long work is queued, never awaited inline.
pub trait Handler: Send + Sync + 'static {
    /// Answer one request.
    fn handle(&self, request: &RequestFrame) -> Result<serde_json::Value, ErrorBody>;
}

/// The IPC server.
pub struct Server {
    listener: UnixListener,
    path: PathBuf,
    handler: Arc<dyn Handler>,
    shutdown: Shutdown,
    connections: Arc<AtomicUsize>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("path", &self.path)
            .field("shutdown", &self.shutdown.is_requested())
            .field("connections", &self.connections.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Server {
    /// Bind the socket, creating its parent directory with `0700` permissions.
    ///
    /// Stale sockets (a leftover file nobody is listening on) are removed; a
    /// socket with a live listener is reported as [`ServerError::AlreadyRunning`]
    /// so two daemons cannot fight over one socket.
    pub fn bind(path: &Path, handler: Arc<dyn Handler>) -> Result<Self, ServerError> {
        Self::bind_with_shutdown(path, handler, Shutdown::new())
    }

    /// Bind with a caller-owned [`Shutdown`] signal, so other components (the
    /// IPC handler, signal handlers) can stop the accept loop.
    pub fn bind_with_shutdown(
        path: &Path,
        handler: Arc<dyn Handler>,
        shutdown: Shutdown,
    ) -> Result<Self, ServerError> {
        let parent = path
            .parent()
            .ok_or_else(|| ServerError::NoParent(path.to_path_buf()))?;
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;

        if path.exists() {
            match UnixStream::connect(path) {
                Ok(_) => return Err(ServerError::AlreadyRunning(path.to_path_buf())),
                Err(_) => {
                    // Nobody is listening: a stale socket from a crashed daemon.
                    tracing::warn!(path = %path.display(), "removing stale ipc socket");
                    std::fs::remove_file(path)?;
                }
            }
        }

        let listener = UnixListener::bind(path)?;
        // 0600: same user only (TRD NFR-SEC-1). The enclosing dir is 0700 too.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        Ok(Self {
            listener,
            path: path.to_path_buf(),
            handler,
            shutdown,
            connections: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// The shutdown signal, cloneable to other components.
    pub fn shutdown_signal(&self) -> Shutdown {
        self.shutdown.clone()
    }

    /// The socket path this server is bound to.
    pub fn socket_path(&self) -> &Path {
        &self.path
    }

    /// The underlying listener (for calloop registration in P1).
    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }

    /// Accept and start serving every pending connection. Returns how many.
    pub fn accept_pending(&self) -> Result<usize, ServerError> {
        let mut accepted = 0;
        loop {
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    accepted += 1;
                    let handler = Arc::clone(&self.handler);
                    let connections = Arc::clone(&self.connections);
                    if let Err(error) = stream.set_read_timeout(Some(Duration::from_secs(30))) {
                        tracing::warn!(%error, "cannot set read timeout");
                    }
                    connections.fetch_add(1, Ordering::Relaxed);
                    let spawned = std::thread::Builder::new()
                        .name("owe-ipc-conn".to_string())
                        .spawn(move || {
                            if let Err(error) = serve_connection(stream, handler) {
                                tracing::debug!(%error, "ipc connection ended with error");
                            }
                            connections.fetch_sub(1, Ordering::Relaxed);
                        });
                    if let Err(error) = spawned {
                        tracing::error!(%error, "cannot spawn ipc connection thread");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    return Ok(accepted);
                }
                Err(error) => return Err(ServerError::Io(error)),
            }
        }
    }

    /// Accept connections until shutdown is requested (tests, and the daemon's
    /// P0 bootstrap; P1 replaces the driver with calloop).
    pub fn serve_blocking(&self) -> Result<(), ServerError> {
        while !self.is_shutdown() {
            self.accept_pending()?;
            if self.is_shutdown() {
                break;
            }
            std::thread::sleep(ACCEPT_POLL_INTERVAL);
        }
        Ok(())
    }

    /// Ask the accept loop to stop (idempotent).
    pub fn request_shutdown(&self) {
        self.shutdown.request();
    }

    /// Whether shutdown has been requested.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.is_requested()
    }

    /// Number of connections currently being served.
    pub fn active_connections(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Best-effort cleanup so the next start does not trip over a stale socket.
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(path = %self.path.display(), %error, "cannot remove ipc socket");
        }
    }
}

/// Serve one connection until the client disconnects or violates framing.
fn serve_connection(stream: UnixStream, handler: Arc<dyn Handler>) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut decoder = FrameDecoder::new();
    let mut buffer = [0_u8; 8192];

    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(()), // clean disconnect
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };

        if decoder.push(&buffer[..read]).is_err() {
            return Ok(());
        }

        loop {
            match decoder.next_frame() {
                Ok(Some(payload)) => {
                    let reply = handle_payload(&*handler, &payload);
                    if write_reply(&mut writer, &reply).is_err() {
                        return Ok(());
                    }
                }
                Ok(None) => break,
                Err(FrameError::TooLarge) => {
                    // Cannot resynchronise: reply once, then close (documented
                    // exception to "malformed never disconnects").
                    let reply = ReplyFrame::err(
                        String::new(),
                        ErrorBody::bad_request(FrameError::TooLarge.to_string()),
                    );
                    let _ = write_reply(&mut writer, &reply);
                    return Ok(());
                }
                Err(FrameError::Poisoned) => return Ok(()),
                Err(FrameError::Malformed(message)) => {
                    let reply = ReplyFrame::err(String::new(), ErrorBody::bad_request(message));
                    if write_reply(&mut writer, &reply).is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn handle_payload(handler: &dyn Handler, payload: &[u8]) -> ReplyFrame {
    match protocol::decode_request(payload) {
        Ok(request) => {
            tracing::trace!(id = %request.id, method = %request.method, "ipc request");
            match handler.handle(&request) {
                Ok(value) => ReplyFrame::ok(request.id, value),
                Err(error) => ReplyFrame::err(request.id, error),
            }
        }
        Err(error) => ReplyFrame::err(
            protocol::extract_id(payload),
            ErrorBody::bad_request(error.to_string()),
        ),
    }
}

fn write_reply(writer: &mut UnixStream, reply: &ReplyFrame) -> std::io::Result<()> {
    let bytes = match frame::encode(reply) {
        Ok(bytes) => bytes,
        Err(error) => frame::encode(&ReplyFrame::err(
            reply.id.clone(),
            ErrorBody::internal(format!("cannot encode reply: {error}")),
        ))
        .unwrap_or_else(|_| {
            b"{\"v\":1,\"id\":\"\",\"err\":{\"code\":\"INTERNAL\",\"msg\":\"encoding\"}}\n".to_vec()
        }),
    };
    writer.write_all(&bytes)?;
    writer.flush()
}
