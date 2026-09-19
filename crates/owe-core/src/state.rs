//! Session state: which wallpaper OWE put on each output (FR-LIB-5).
//!
//! Written on every successful apply so that a restarted daemon can restore the
//! desktop instead of leaving it blank. The format is plain JSON because it is a
//! debugging surface as much as a file format — a user should be able to read it
//! and see what OWE thinks is on their monitors.
//!
//! Two deliberate properties:
//!
//! - **A corrupt state file never stops the daemon.** It is a cache of the
//!   current session, not the user's configuration; losing it costs one
//!   re-apply, so it degrades to "nothing known" with a warning.
//! - **Writes are atomic** (temp file + rename). A crash or power loss mid-write
//!   must not leave a half-written JSON file behind, which is exactly how a
//!   wallpaper engine ends up unable to restore anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// State-file schema version. Bump when the format changes incompatibly.
pub const STATE_VERSION: u32 = 1;

/// Failures while persisting session state.
#[derive(Debug, Error)]
pub enum StateError {
    /// The state file could not be read.
    #[error("cannot read session state `{path}`: {source}")]
    Read {
        /// File that failed.
        path: PathBuf,
        /// Underlying cause.
        source: std::io::Error,
    },
    /// The state file could not be written.
    #[error("cannot write session state `{path}`: {source}")]
    Write {
        /// File that failed.
        path: PathBuf,
        /// Underlying cause.
        source: std::io::Error,
    },
}

/// What OWE last applied to one output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputEntry {
    /// The reference exactly as the user gave it (path, `library:<id>`, …).
    pub reference: String,
    /// Resolved content kind id, for logging and GUI display.
    pub kind: String,
}

/// Every output OWE has applied a wallpaper to in this session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionState {
    /// Format version; a file from the future is ignored rather than guessed at.
    pub version: u32,
    /// Output name → last applied wallpaper.
    pub outputs: BTreeMap<String, OutputEntry>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            outputs: BTreeMap::new(),
        }
    }
}

/// Outcome of loading the state file: the state plus anything worth telling the
/// user about. Warnings are returned rather than logged so this stays pure and
/// the caller decides how loud to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The state to use.
    pub state: SessionState,
    /// Non-fatal problems (missing file is not one of them).
    pub warnings: Vec<String>,
}

impl SessionState {
    /// Load state, tolerating every kind of damage.
    pub fn load(path: &Path) -> Loaded {
        if !path.exists() {
            return Loaded {
                state: SessionState::default(),
                warnings: Vec::new(),
            };
        }

        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                return Loaded {
                    state: SessionState::default(),
                    warnings: vec![format!(
                        "session state `{}` could not be read ({error}); starting fresh",
                        path.display()
                    )],
                };
            }
        };

        match serde_json::from_str::<SessionState>(&text) {
            Ok(state) if state.version == STATE_VERSION => Loaded {
                state,
                warnings: Vec::new(),
            },
            Ok(state) => Loaded {
                state: SessionState::default(),
                warnings: vec![format!(
                    "session state `{}` has version {} but this build writes version {STATE_VERSION}; ignoring it",
                    path.display(),
                    state.version
                )],
            },
            Err(error) => Loaded {
                state: SessionState::default(),
                warnings: vec![format!(
                    "session state `{}` is not valid JSON ({error}); starting fresh",
                    path.display()
                )],
            },
        }
    }

    /// Write state atomically (temp file + rename in the same directory).
    pub fn save(&self, path: &Path) -> Result<(), StateError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StateError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        }

        let text = serde_json::to_string_pretty(self).map_err(|source| StateError::Write {
            path: path.to_path_buf(),
            source: std::io::Error::other(source),
        })?;

        // Same directory as the target: rename is only atomic within a filesystem.
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, text.as_bytes()).map_err(|source| StateError::Write {
            path: temp.clone(),
            source,
        })?;
        std::fs::rename(&temp, path).map_err(|source| StateError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Record what was applied to `output`.
    pub fn set(&mut self, output: impl Into<String>, reference: impl Into<String>, kind: &str) {
        self.outputs.insert(
            output.into(),
            OutputEntry {
                reference: reference.into(),
                kind: kind.to_string(),
            },
        );
    }

    /// Forget an output (it disappeared, or was cleared).
    pub fn remove(&mut self, output: &str) -> Option<OutputEntry> {
        self.outputs.remove(output)
    }

    /// What is known about an output.
    pub fn get(&self, output: &str) -> Option<&OutputEntry> {
        self.outputs.get(output)
    }

    /// Drop entries for outputs that no longer exist.
    ///
    /// Returns the names that were removed, so the caller can log them. Stale
    /// entries are how a restore ends up applying a wallpaper to a monitor that
    /// is not there.
    pub fn retain_outputs(&mut self, connected: &[String]) -> Vec<String> {
        let before: Vec<String> = self.outputs.keys().cloned().collect();
        self.outputs.retain(|name, _| connected.contains(name));
        before
            .into_iter()
            .filter(|name| !self.outputs.contains_key(name))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty_and_current_version() {
        let state = SessionState::default();
        assert_eq!(state.version, STATE_VERSION);
        assert!(state.outputs.is_empty());
    }

    #[test]
    fn set_get_and_remove_round_trip() {
        let mut state = SessionState::default();
        state.set("eDP-1", "/walls/a.png", "static-image");
        assert_eq!(state.get("eDP-1").unwrap().reference, "/walls/a.png");
        assert_eq!(state.get("eDP-1").unwrap().kind, "static-image");

        let removed = state.remove("eDP-1");
        assert_eq!(removed.unwrap().reference, "/walls/a.png");
        assert!(state.get("eDP-1").is_none());
    }

    #[test]
    fn missing_file_loads_as_empty_without_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = SessionState::load(&dir.path().join("absent.json"));
        assert_eq!(loaded.state, SessionState::default());
        assert!(
            loaded.warnings.is_empty(),
            "a first run is not a problem worth warning about"
        );
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/session.json");

        let mut state = SessionState::default();
        state.set("eDP-1", "/walls/a.png", "static-image");
        state.set("HDMI-A-1", "library:abc", "static-image");
        state.save(&path).expect("save");

        let loaded = SessionState::load(&path);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.state, state);
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let mut state = SessionState::default();
        state.set("eDP-1", "/w/a.png", "static-image");
        state.save(&path).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn corrupt_file_degrades_to_empty_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(&path, "{ this is not json").unwrap();

        let loaded = SessionState::load(&path);
        assert_eq!(loaded.state, SessionState::default());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains("not valid JSON"),
            "{:?}",
            loaded.warnings
        );
    }

    #[test]
    fn future_version_is_ignored_rather_than_misread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(
            &path,
            r#"{"version":99,"outputs":{"eDP-1":{"reference":"/w/a.png","kind":"static-image"}}}"#,
        )
        .unwrap();

        let loaded = SessionState::load(&path);
        assert_eq!(loaded.state, SessionState::default());
        assert!(
            loaded.warnings[0].contains("version 99"),
            "{:?}",
            loaded.warnings
        );
    }

    #[test]
    fn unknown_extra_fields_are_tolerated_on_load() {
        // Forward compatibility: version 1 readers must not choke on fields a
        // later 1.x writer added.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(
            &path,
            r#"{"version":1,"outputs":{"eDP-1":{"reference":"/w/a.png","kind":"static-image","scaling":"cover"}}}"#,
        )
        .unwrap();

        let loaded = SessionState::load(&path);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.state.get("eDP-1").unwrap().reference, "/w/a.png");
    }

    #[test]
    fn retain_outputs_drops_disconnected_monitors_and_reports_them() {
        let mut state = SessionState::default();
        state.set("eDP-1", "/w/a.png", "static-image");
        state.set("HDMI-A-1", "/w/b.png", "static-image");
        state.set("DP-1", "/w/c.png", "static-image");

        let removed = state.retain_outputs(&["eDP-1".to_string(), "DP-1".to_string()]);
        assert_eq!(removed, vec!["HDMI-A-1".to_string()]);
        assert!(state.get("HDMI-A-1").is_none());
        assert!(state.get("eDP-1").is_some());
    }

    #[test]
    fn written_file_is_human_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        let mut state = SessionState::default();
        state.set("eDP-1", "/w/a.png", "static-image");
        state.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\": 1"), "{text}");
        assert!(text.contains("\"eDP-1\""), "{text}");
        assert!(text.contains("static-image"), "{text}");
    }
}
