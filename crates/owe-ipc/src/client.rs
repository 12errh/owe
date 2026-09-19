//! The blocking IPC client used by `owectl` and the Tauri GUI backend.
//!
//! One request → one reply, correlated by `id`. A read timeout turns a hung
//! daemon into a clear error instead of a frozen caller.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

use crate::frame;
use crate::protocol::{
    self, ErrorBody, HelloParams, HelloReply, ReplyFrame, RequestFrame, SchemaVersion,
};

/// Default time to wait for a reply before giving up.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Canonical daemon socket path from a runtime directory: `<runtime>/owe/socket`.
///
/// This rule lives in `owe-ipc` rather than `owe-core` so that *clients* (the
/// `owectl` CLI and the Tauri app) can discover the daemon while depending on
/// `owe-ipc` alone — the dependency direction fixed in ARCHITECTURE §2.
/// `owe_core::path::XdgPaths::socket_path` remains the daemon-side accessor;
/// the two are pinned to each other by a test in `owed`'s suite.
///
/// `None` means no runtime directory: there is no session, so no socket.
pub fn socket_path_in(runtime_dir: Option<&OsStr>) -> Option<PathBuf> {
    runtime_dir
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join("owe").join("socket"))
}

/// [`socket_path_in`] against the live process environment.
pub fn default_socket_path() -> Result<PathBuf, ClientError> {
    socket_path_in(std::env::var_os("XDG_RUNTIME_DIR").as_deref()).ok_or_else(|| {
        ClientError::Protocol(
            "XDG_RUNTIME_DIR is not set, so the daemon socket cannot be located".to_string(),
        )
    })
}

/// Client-side failures.
#[derive(Debug, Error)]
pub enum ClientError {
    /// The socket could not be reached or the exchange failed.
    #[error("ipc connection error: {0}")]
    Io(#[from] std::io::Error),

    /// The daemon replied with a protocol violation.
    #[error("ipc protocol error: {0}")]
    Protocol(String),

    /// The daemon returned a structured error.
    #[error("{0}")]
    Server(#[from] ErrorBody),
}

/// A blocking client for the OWE IPC socket.
#[derive(Debug)]
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl Client {
    /// Connect to the daemon socket at `path`.
    pub fn connect(path: &Path) -> Result<Self, ClientError> {
        Self::connect_with_timeout(path, DEFAULT_TIMEOUT)
    }

    /// Connect with a custom reply timeout.
    pub fn connect_with_timeout(path: &Path, timeout: Duration) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let writer = stream.try_clone()?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
            next_id: 1,
        })
    }

    /// Send a request and wait for its reply.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = format!("c{}", self.next_id);
        self.next_id += 1;
        let request = RequestFrame::new(id.clone(), method, params);
        let bytes =
            frame::encode(&request).map_err(|error| ClientError::Protocol(error.to_string()))?;
        self.writer.write_all(&bytes)?;
        self.writer.flush()?;

        let mut line = Vec::new();
        let read = self.reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            return Err(ClientError::Protocol(
                "daemon closed the connection before replying".to_string(),
            ));
        }
        let line = line.strip_suffix(b"\n").unwrap_or(&line);
        let reply: ReplyFrame =
            frame::decode(line).map_err(|error| ClientError::Protocol(error.to_string()))?;
        if reply.id != id {
            return Err(ClientError::Protocol(format!(
                "reply id `{}` does not match request id `{id}`",
                reply.id
            )));
        }
        Ok(reply.into_result()?)
    }

    /// Perform the `hello` handshake.
    pub fn hello(&mut self, client: &str, client_version: &str) -> Result<HelloReply, ClientError> {
        let params = HelloParams {
            client: client.to_string(),
            client_version: client_version.to_string(),
            schema: vec![SchemaVersion::CURRENT],
        };
        let value = self.call(
            protocol::method::HELLO,
            serde_json::to_value(params)
                .map_err(|error| ClientError::Protocol(error.to_string()))?,
        )?;
        serde_json::from_value(value).map_err(|error| ClientError::Protocol(error.to_string()))
    }

    /// Send a request id-less: used by tests that speak raw JSON.
    pub fn raw_call(&mut self, raw: &[u8]) -> Result<Value, ClientError> {
        self.writer.write_all(raw)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        let mut line = Vec::new();
        let read = self.reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            return Err(ClientError::Protocol("connection closed".to_string()));
        }
        let line = line.strip_suffix(b"\n").unwrap_or(&line);
        let reply: ReplyFrame =
            frame::decode(line).map_err(|error| ClientError::Protocol(error.to_string()))?;
        Ok(serde_json::to_value(reply).unwrap_or(Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_is_owe_socket_under_the_runtime_dir() {
        assert_eq!(
            socket_path_in(Some(OsStr::new("/run/user/1000"))),
            Some(PathBuf::from("/run/user/1000/owe/socket"))
        );
        assert_eq!(
            socket_path_in(Some(OsStr::new("/tmp/run"))),
            Some(PathBuf::from("/tmp/run/owe/socket"))
        );
    }

    #[test]
    fn an_absent_or_empty_runtime_dir_means_no_socket() {
        assert_eq!(socket_path_in(None), None);
        assert_eq!(socket_path_in(Some(OsStr::new(""))), None);
    }
}
