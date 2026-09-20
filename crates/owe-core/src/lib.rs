//! OWE core — the pure-logic half of the daemon.
//!
//! Contents: the configuration model + validation ([`config`]), the content
//! model ([`model`]), path expansion ([`path`]), the output model ([`output`]),
//! the shell-backend registry ([`shell`]), session state ([`state`]), and the
//! per-output worker state machine ([`worker`]).
//!
//! # Design rules (ARCHITECTURE §1)
//!
//! - No Wayland, no wgpu, no sockets, no async runtime in this crate.
//! - Everything here is unit-testable without a compositor.
//! - Unknown configuration keys are hard errors, never silently ignored.
//!
//! The library database and the governor rule engine land in P2/P6; they belong
//! in this crate for the same reason (pure logic, testable headless).

pub mod config;
pub mod error;
pub mod library;
pub mod model;
pub mod output;
pub mod outputs;
pub mod path;
pub mod shell;
pub mod state;
pub mod supervisor;
pub mod thumbs;
pub mod worker;

pub use config::Config;
pub use error::{ConfigError, ModelError, PathError};
pub use library::{FileStamp, Item, Library, LibraryError, ListQuery, ScanReport};
pub use model::{ContentKind, WallpaperRef, WallpaperSource};
pub use output::{OutputInfo, OutputSelectError, OutputTarget, resolve as resolve_outputs};
pub use outputs::{OutputResolution, Precedence};
pub use shell::{Registry as ShellRegistry, SelectError, ShellBackend, ShellError};
pub use state::{STATE_VERSION, SessionState};
pub use supervisor::{HotplugEvent, Supervisor, SupervisorAction};
pub use thumbs::{Decision as ThumbDecision, Scheduler as ThumbnailScheduler, ThumbJob};
pub use worker::{OutputWorker, WorkerAction, WorkerEvent, WorkerState};
