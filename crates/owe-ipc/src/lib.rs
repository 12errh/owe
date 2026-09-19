//! OWE IPC protocol v1 — one Unix socket, newline-delimited JSON.
//!
//! Contract (docs/BACKEND-DESIGN.md §3):
//!
//! - Transport: Unix stream socket at `$XDG_RUNTIME_DIR/owe/<id>/socket`, mode
//!   `0600`, same user only (TRD NFR-SEC-1).
//! - Framing: UTF-8 JSON, one object per line, capped at [`frame::MAX_FRAME_BYTES`].
//! - Every message carries `v` (schema major) and `id` (client-chosen, echoed back).
//! - A malformed **line** gets an error frame and the connection stays open
//!   (TRD FR-CORE-6). The one exception is a frame too large to resynchronise:
//!   there we reply with the error, then close — documented, not accidental.
//! - Schema negotiation happens in `hello`; the server picks the newest minor it
//!   shares with the client (TRD NFR-COMPAT-2).
//!
//! No async runtime: the server accepts connections on one thread and serves each
//! connection on its own short-lived thread. calloop drives the daemon's *other*
//! sources from P1 (ADR-004).

pub mod client;
pub mod frame;
pub mod protocol;
pub mod server;

pub use client::{Client, ClientError, default_socket_path, socket_path_in};
pub use frame::{FrameDecoder, FrameError, MAX_FRAME_BYTES};
pub use protocol::{
    Capabilities, ErrorBody, ErrorCode, EventFrame, HelloParams, HelloReply, ReplyFrame,
    RequestFrame, SchemaVersion, negotiate,
};
pub use server::{Handler, Server, ServerError, Shutdown};
