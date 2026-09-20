//! Path expansion for config values and wallpaper references.
//!
//! Supports a leading `~` (the user's home directory) and `$VAR` / `${VAR}`
//! environment references. Existence is deliberately **not** checked here:
//! wallpapers may live on removable or network mounts, so existence is a
//! scan-time concern (P2 scanner), not a config-load failure (see
//! docs/IMPLEMENTATION-PLAN.md P0 notes).

use std::path::PathBuf;

use crate::error::PathError;

/// Expand `~` and `$VAR` using the process environment.
pub fn expand(input: &str) -> Result<PathBuf, PathError> {
    expand_with(input, |name| std::env::var(name).ok())
}

/// Expand `~` and `$VAR` using a caller-supplied variable lookup.
///
/// The lookup makes tests hermetic: no test depends on the ambient environment.
///
/// The single special case is the literal `$CAELESTIA_WALLPAPERS_DIR`: when that
/// variable is unset the value is left unexpanded and returned as-is as a
/// **deferred** path (it means "whatever Caelestia Shell uses"), because
/// requiring the shell to be running just to validate a config file would be
/// hostile. Deferred paths skip the absolute-path check; every other unresolved
/// reference is a hard error, since a typo that silently expands to nothing is
/// worse than a startup failure.
pub fn expand_with<F>(input: &str, lookup: F) -> Result<PathBuf, PathError>
where
    F: Fn(&str) -> Option<String>,
{
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(PathError::Empty);
    }

    let mut out = String::with_capacity(trimmed.len());
    let mut deferred = false;
    let mut chars = trimmed.chars().peekable();

    // Leading `~` only (a `~` elsewhere is a literal character, as in shells).
    if trimmed.starts_with('~') {
        let home = lookup("HOME")
            .or_else(|| lookup("USERPROFILE"))
            .ok_or_else(|| PathError::UnsetEnv {
                name: "HOME".to_string(),
                input: trimmed.to_string(),
            })?;
        out.push_str(&home);
        chars.next();
    }

    while let Some(ch) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }
        // `$VAR` or `${VAR}`
        let name = if chars.peek() == Some(&'{') {
            chars.next();
            let mut name = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                name.push(c);
            }
            name
        } else {
            let mut name = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    name.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            name
        };

        if name.is_empty() {
            // A lone `$` is a literal dollar sign.
            out.push('$');
            continue;
        }

        match lookup(&name) {
            Some(value) => out.push_str(&value),
            None if name == "CAELESTIA_WALLPAPERS_DIR" => {
                // Special case: keep the reference, do not fail validation.
                deferred = true;
                out.push_str("$CAELESTIA_WALLPAPERS_DIR");
            }
            None => {
                return Err(PathError::UnsetEnv {
                    name,
                    input: trimmed.to_string(),
                });
            }
        }
    }

    let path = PathBuf::from(&out);
    if !deferred && !path.is_absolute() {
        return Err(PathError::NotAbsolute {
            input: trimmed.to_string(),
            expanded: out,
        });
    }
    Ok(path)
}

/// XDG base directories and the OWE files inside them.
///
/// Deviation from docs/ARCHITECTURE.md §7, recorded in the P0 notes: the IPC
/// socket is a stable `$XDG_RUNTIME_DIR/owe/socket` rather than a
/// per-instance subdirectory, because clients must be able to find the daemon
/// without a discovery protocol. Per-session sockets return with multi-session
/// support, if it is ever needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XdgPaths {
    /// `$XDG_CONFIG_HOME/owe/config.toml`
    pub config_file: PathBuf,
    /// `$XDG_STATE_HOME/owe` (session state, last wallpaper per output)
    pub state_dir: PathBuf,
    /// `$XDG_CACHE_HOME/owe` (thumbnails, frame caches)
    pub cache_dir: PathBuf,
    /// `$XDG_DATA_HOME/owe` (library database)
    pub data_dir: PathBuf,
    /// `$XDG_RUNTIME_DIR/owe` (IPC socket); `None` outside a session, where
    /// only config validation is possible.
    pub runtime_dir: Option<PathBuf>,
}

impl XdgPaths {
    /// Resolve from the process environment.
    pub fn resolve() -> Result<Self, PathError> {
        Self::resolve_with(|name| std::env::var(name).ok())
    }

    /// Resolve from a caller-supplied lookup (hermetic tests).
    ///
    /// A missing `XDG_RUNTIME_DIR` is *not* fatal here: `owed --check-config`
    /// must work in containers and CI. Starting the daemon does require it, and
    /// [`XdgPaths::socket_path`] reports the failure.
    pub fn resolve_with<F>(lookup: F) -> Result<Self, PathError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let home = lookup("HOME")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| PathError::UnsetEnv {
                name: "HOME".to_string(),
                input: "$HOME".to_string(),
            })?;
        let home = PathBuf::from(home);

        let base = |var: &str, fallback: &str| -> PathBuf {
            lookup(var)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(fallback))
        };

        let runtime = lookup("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .map(|value| PathBuf::from(value).join("owe"));

        // `OWE_STATE_DIR` overrides only OWE's own state dir. Redirecting
        // `XDG_STATE_HOME` instead looks like the obvious sandbox trick, but the
        // shell backends spawn children (`caelestia wallpaper …`) that inherit
        // the environment — and the Caelestia CLI derives its own state dir
        // from the very same variable, so a redirected daemon quietly writes a
        // shadow state tree the running shell never reads. A whole-sale env
        // redirect must never be needed to isolate OWE.
        let state = lookup("OWE_STATE_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| base("XDG_STATE_HOME", ".local/state").join("owe"));

        Ok(Self {
            config_file: base("XDG_CONFIG_HOME", ".config")
                .join("owe")
                .join("config.toml"),
            state_dir: state,
            cache_dir: base("XDG_CACHE_HOME", ".cache").join("owe"),
            data_dir: base("XDG_DATA_HOME", ".local/share").join("owe"),
            runtime_dir: runtime,
        })
    }

    /// The IPC socket path: `$XDG_RUNTIME_DIR/owe/socket`.
    pub fn socket_path(&self) -> Result<PathBuf, PathError> {
        self.runtime_dir
            .as_ref()
            .map(|dir| dir.join("socket"))
            .ok_or_else(|| PathError::UnsetEnv {
                name: "XDG_RUNTIME_DIR".to_string(),
                input: "$XDG_RUNTIME_DIR".to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_paths_follow_the_environment() {
        let paths = XdgPaths::resolve_with(env(&[
            ("HOME", "/home/owe"),
            ("XDG_CONFIG_HOME", "/home/owe/.config"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
        ]))
        .unwrap();
        assert_eq!(
            paths.config_file,
            PathBuf::from("/home/owe/.config/owe/config.toml")
        );
        assert_eq!(paths.state_dir, PathBuf::from("/home/owe/.local/state/owe"));
        assert_eq!(paths.cache_dir, PathBuf::from("/home/owe/.cache/owe"));
        assert_eq!(paths.data_dir, PathBuf::from("/home/owe/.local/share/owe"));
        assert_eq!(paths.runtime_dir, Some(PathBuf::from("/run/user/1000/owe")));
        assert_eq!(
            paths.socket_path().unwrap(),
            PathBuf::from("/run/user/1000/owe/socket")
        );
    }

    #[test]
    fn xdg_overrides_are_honoured() {
        let paths = XdgPaths::resolve_with(env(&[
            ("HOME", "/home/owe"),
            ("XDG_CONFIG_HOME", "/tmp/cfg"),
            ("XDG_STATE_HOME", "/tmp/state"),
            ("XDG_CACHE_HOME", "/tmp/cache"),
            ("XDG_DATA_HOME", "/tmp/data"),
            ("XDG_RUNTIME_DIR", "/tmp/run"),
        ]))
        .unwrap();
        assert_eq!(paths.config_file, PathBuf::from("/tmp/cfg/owe/config.toml"));
        assert_eq!(paths.state_dir, PathBuf::from("/tmp/state/owe"));
        assert_eq!(paths.cache_dir, PathBuf::from("/tmp/cache/owe"));
        assert_eq!(paths.data_dir, PathBuf::from("/tmp/data/owe"));
        assert_eq!(
            paths.socket_path().unwrap(),
            PathBuf::from("/tmp/run/owe/socket")
        );
    }

    /// The Caelestia backend spawns `caelestia` children that derive *their*
    /// state dir from `XDG_STATE_HOME`; a daemon that isolated itself by
    /// redirecting that variable would leave the shell writing a shadow state
    /// tree. Isolating OWE therefore gets its own variable, so the session
    /// environment is never something OWE has to bend.
    #[test]
    fn owe_state_dir_overrides_only_owes_own_state() {
        let paths = XdgPaths::resolve_with(env(&[
            ("HOME", "/home/owe"),
            ("OWE_STATE_DIR", "/tmp/owe-only-state"),
            ("XDG_STATE_HOME", "/tmp/real-session-state"),
        ]))
        .unwrap();
        assert_eq!(paths.state_dir, PathBuf::from("/tmp/owe-only-state"));

        // Without the override the XDG default applies, unchanged.
        let paths = XdgPaths::resolve_with(env(&[
            ("HOME", "/home/owe"),
            ("XDG_STATE_HOME", "/tmp/real-session-state"),
        ]))
        .unwrap();
        assert_eq!(
            paths.state_dir,
            PathBuf::from("/tmp/real-session-state/owe")
        );
    }

    #[test]
    fn config_paths_resolve_without_a_runtime_dir() {
        // `owed --check-config` must work in CI containers: config resolution
        // never requires a session, only the socket does.
        let paths = XdgPaths::resolve_with(env(&[("HOME", "/home/owe")])).unwrap();
        assert_eq!(paths.runtime_dir, None);
        assert_eq!(
            paths.config_file,
            PathBuf::from("/home/owe/.config/owe/config.toml")
        );

        let err = paths.socket_path().unwrap_err();
        match err {
            PathError::UnsetEnv { name, .. } => assert_eq!(name, "XDG_RUNTIME_DIR"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn missing_home_is_an_error() {
        let err =
            XdgPaths::resolve_with(env(&[("XDG_RUNTIME_DIR", "/run/user/1000")])).unwrap_err();
        match err {
            PathError::UnsetEnv { name, .. } => assert_eq!(name, "HOME"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn expands_tilde_to_home() {
        let got = expand_with("~/Pictures/Wallpapers", env(&[("HOME", "/home/owe")])).unwrap();
        assert_eq!(got, PathBuf::from("/home/owe/Pictures/Wallpapers"));
    }

    #[test]
    fn expands_dollar_var() {
        let got = expand_with(
            "$XDG_DATA_HOME/owe",
            env(&[("XDG_DATA_HOME", "/home/owe/.local/share")]),
        )
        .unwrap();
        assert_eq!(got, PathBuf::from("/home/owe/.local/share/owe"));
    }

    #[test]
    fn expands_braced_var() {
        let got = expand_with("${HOME}/wall/wall.png", env(&[("HOME", "/home/owe")])).unwrap();
        assert_eq!(got, PathBuf::from("/home/owe/wall/wall.png"));
    }

    #[test]
    fn absolute_paths_pass_through() {
        let got = expand_with("/usr/share/backgrounds/x.png", env(&[])).unwrap();
        assert_eq!(got, PathBuf::from("/usr/share/backgrounds/x.png"));
    }

    #[test]
    fn relative_path_is_rejected() {
        let err = expand_with("Pictures/wall.png", env(&[])).unwrap_err();
        assert!(matches!(err, PathError::NotAbsolute { .. }), "{err:?}");
        assert!(err.to_string().contains("must be absolute"));
    }

    #[test]
    fn empty_input_is_rejected() {
        assert_eq!(expand_with("   ", env(&[])).unwrap_err(), PathError::Empty);
    }

    #[test]
    fn unset_variable_is_rejected_by_name() {
        let err = expand_with("$NOPE/x.png", env(&[])).unwrap_err();
        match err {
            PathError::UnsetEnv { name, .. } => assert_eq!(name, "NOPE"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn caelestia_wallpapers_dir_survives_unset() {
        // Special case documented in the module docs: the shell owns this dir.
        let got = expand_with("$CAELESTIA_WALLPAPERS_DIR", env(&[])).unwrap();
        assert_eq!(got, PathBuf::from("$CAELESTIA_WALLPAPERS_DIR"));
    }

    #[test]
    fn lone_dollar_is_literal() {
        // A `$` not followed by an identifier character is a literal dollar sign.
        let got = expand_with("/tmp/a$/b", env(&[])).unwrap();
        assert_eq!(got, PathBuf::from("/tmp/a$/b"));
    }

    #[test]
    fn dollar_name_that_is_not_a_variable_errors_instead_of_vanishing() {
        // `$weird` must NOT silently expand to nothing: that would hide typos.
        let err = expand_with("/tmp/$weird", env(&[])).unwrap_err();
        match err {
            PathError::UnsetEnv { name, .. } => assert_eq!(name, "weird"),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
