//! Error types for [`crate::config`], [`crate::model`], and [`crate::path`].
//!
//! Every variant is written to be shown to a user verbatim: it names the
//! offending key or value and, where possible, the accepted alternatives
//! (TRD FR-CORE-6: precise validation errors, exit code 1).

use std::path::PathBuf;

use thiserror::Error;

/// Errors produced while loading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The configuration file could not be read.
    #[error("cannot read config file `{path}`: {source}")]
    Io {
        /// Path that could not be read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The file is not valid TOML (the message includes line/column).
    #[error("invalid TOML: {0}")]
    Toml(String),

    /// The config declares a schema version this build does not support.
    #[error(
        "unsupported config schema {found}: this build supports schema {supported} \
         (see docs/BACKEND-DESIGN.md §4)"
    )]
    UnsupportedSchema {
        /// Version found in the file.
        found: u64,
        /// Version supported by this build.
        supported: u32,
    },

    /// One or more validation rules failed. All problems are listed at once.
    #[error("config validation failed ({} problem(s)):\n{}", .0.len(), format_problems(.0))]
    Invalid(Vec<String>),
}

impl ConfigError {
    /// Exit code for this error, per BACKEND-DESIGN §8 (1 = config invalid).
    pub fn exit_code(&self) -> i32 {
        1
    }
}

fn format_problems(problems: &[String]) -> String {
    problems
        .iter()
        .map(|p| format!("  - {p}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Errors produced while parsing a wallpaper reference ([`crate::model`]).
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ModelError {
    /// The reference was empty or whitespace-only.
    #[error("empty wallpaper reference")]
    Empty,

    /// A prefix such as `library:` or `shader:` was used without a name.
    #[error("{prefix}: reference needs a non-empty name")]
    EmptyName {
        /// The prefix that was used (`library` or `shader`).
        prefix: &'static str,
    },

    /// A plain path had no file extension, so the content kind is unknown.
    #[error(
        "cannot tell the content kind of `{path}`: no file extension \
         (shader packs are addressed as `shader:<name>`)"
    )]
    NoExtension {
        /// The offending path.
        path: String,
    },

    /// The file extension is not a recognised content type.
    #[error(
        "unsupported content type `{extension}` in `{path}` \
         (supported: images, gif/apng, mp4/webm/mkv, wgsl)"
    )]
    UnknownExtension {
        /// The unrecognised extension.
        extension: String,
        /// The offending path.
        path: String,
    },

    /// The content kind cannot be known without the library database.
    #[error(
        "`{reference}` is a library item: its content kind is resolved from the library database"
    )]
    KindUnresolved {
        /// The reference that needs resolution.
        reference: String,
    },
}

/// Errors produced while expanding `~` and `$VARS` in paths.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PathError {
    /// A `$VAR` / `${VAR}` reference points at an unset variable.
    #[error("environment variable `{name}` is not set (referenced in `{input}`)")]
    UnsetEnv {
        /// Variable name that is not set.
        name: String,
        /// The original, unexpanded input.
        input: String,
    },

    /// The input was empty or whitespace-only.
    #[error("empty path")]
    Empty,

    /// After expansion the path is still not absolute.
    #[error(
        "path `{input}` must be absolute or start with `~` \
         (got `{expanded}`; relative paths are not allowed)"
    )]
    NotAbsolute {
        /// The original input.
        input: String,
        /// What it expanded to.
        expanded: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_lists_every_problem() {
        let err = ConfigError::Invalid(vec![
            "shell.backend: unknown backend `gnome`".to_string(),
            "render.buffering.max_in_flight: must be 1..=8".to_string(),
        ]);
        let text = err.to_string();
        assert!(text.contains("2 problem(s)"), "{text}");
        assert!(text.contains("shell.backend"), "{text}");
        assert!(text.contains("max_in_flight"), "{text}");
    }

    #[test]
    fn config_errors_exit_with_code_1() {
        let err = ConfigError::UnsupportedSchema {
            found: 9,
            supported: 1,
        };
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("schema 9"));
    }
}
