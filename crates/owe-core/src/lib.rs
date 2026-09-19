//! OWE core — the pure-logic half of the daemon.
//!
//! Contents: the configuration model + validation ([`config`]), the content
//! model ([`model`]), and path expansion ([`path`]).
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
pub mod model;
pub mod path;

pub use config::Config;
pub use error::{ConfigError, ModelError, PathError};
pub use model::{ContentKind, WallpaperRef, WallpaperSource};
