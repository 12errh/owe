//! Shell backends: how OWE learns about outputs and (from P3) environment events.
//!
//! Two rules from ARCHITECTURE §1 shape this module:
//!
//! 3. **Registries, not if-chains.** Backends register under a string id, config
//!    selects one, and an unknown id is a configuration error — never a silent
//!    fallback to something the user did not ask for.
//! 4. **Nothing is hardcoded to one compositor.** Hyprland, Caelestia and the
//!    generic layer-shell backend all implement [`ShellBackend`], and adding a
//!    fourth means adding a crate and one `register` call.
//!
//! Detection is env-driven and takes a lookup closure, so it is unit-testable
//! without touching the real environment (same pattern as [`crate::path`]).

use thiserror::Error;

use crate::config::ShellConfig;
use crate::output::OutputInfo;

/// Environment lookup, injected so tests are hermetic.
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Failures a backend can report.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ShellError {
    /// The backend is not usable in this session (no socket, no binary, …).
    #[error("{backend} is not available: {detail}")]
    Unavailable {
        /// Backend id.
        backend: String,
        /// What is missing.
        detail: String,
    },
    /// The backend ran but reported an error.
    #[error("{backend} failed: {detail}")]
    Backend {
        /// Backend id.
        backend: String,
        /// Underlying cause.
        detail: String,
    },
}

/// A source of output information (and, later, session events).
///
/// `Send + Sync` is part of the contract, not an accident: the daemon shares one
/// registry between the IPC threads and (from P3) the event-bus thread, so a
/// backend that cannot cross threads cannot be a backend.
pub trait ShellBackend: Send + Sync {
    /// Stable id: `hyprland`, `caelestia`, `generic-layer-shell`.
    fn id(&self) -> &'static str;

    /// Whether this backend can work in the current environment.
    fn detect(&self, env: EnvLookup<'_>) -> bool;

    /// List connected outputs.
    fn list_outputs(&self) -> Result<Vec<OutputInfo>, ShellError>;
}

/// Why backend selection failed.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SelectError {
    /// `shell.backend` names an id this build does not know.
    #[error("unknown shell backend `{requested}`; this build has: {available}")]
    UnknownBackend {
        /// What the config asked for.
        requested: String,
        /// Ids that exist.
        available: String,
    },

    /// `shell.backend = auto` found nothing that works here.
    #[error(
        "no shell backend is available in this session (tried: {tried}); \
         set `shell.backend` explicitly or make sure a supported shell is running"
    )]
    NoBackendDetected {
        /// Ids that were tried, in order.
        tried: String,
    },
}

/// The set of backends this build knows about.
pub struct Registry {
    backends: Vec<Box<dyn ShellBackend>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("backends", &self.ids())
            .finish()
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            backends: Vec::new(),
        }
    }

    /// Add a backend. Ids are unique; registering a duplicate replaces it, which
    /// keeps tests and future overrides simple.
    pub fn register(&mut self, backend: Box<dyn ShellBackend>) {
        let id = backend.id();
        self.backends.retain(|existing| existing.id() != id);
        self.backends.push(backend);
    }

    /// Every registered id, in registration order.
    pub fn ids(&self) -> Vec<&'static str> {
        self.backends.iter().map(|backend| backend.id()).collect()
    }

    /// Whether an id is registered.
    pub fn contains(&self, id: &str) -> bool {
        self.backends.iter().any(|backend| backend.id() == id)
    }

    /// Look up a backend by id.
    pub fn get(&self, id: &str) -> Option<&dyn ShellBackend> {
        self.backends
            .iter()
            .find(|backend| backend.id() == id)
            .map(AsRef::as_ref)
    }

    /// Choose the backend to use for this session.
    ///
    /// `auto` walks `shell.detect_order` and takes the first registered backend
    /// that detects itself; an explicit id must exist, and if it does not detect
    /// itself the daemon still uses it (the user asked for it, so the honest
    /// failure belongs at first use, not at startup).
    pub fn select<'a>(
        &'a self,
        config: &ShellConfig,
        env: EnvLookup<'_>,
    ) -> Result<&'a dyn ShellBackend, SelectError> {
        let requested = config.backend.trim();
        if requested.is_empty() || requested == "auto" {
            let order = if config.detect_order.is_empty() {
                default_detect_order()
            } else {
                config.detect_order.clone()
            };
            for id in &order {
                if let Some(backend) = self.get(id)
                    && backend.detect(env)
                {
                    return Ok(backend);
                }
            }
            return Err(SelectError::NoBackendDetected {
                tried: order.join(", "),
            });
        }

        self.get(requested)
            .ok_or_else(|| SelectError::UnknownBackend {
                requested: requested.to_string(),
                available: self.ids().join(", "),
            })
    }
}

/// Detection order used when the config does not set one.
///
/// The generic layer-shell backend is last because it always "detects" itself —
/// it is the floor, not a preference.
pub fn default_detect_order() -> Vec<String> {
    vec![
        "caelestia".to_string(),
        "hyprland".to_string(),
        "generic-layer-shell".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct FakeBackend {
        id: &'static str,
        detects: bool,
        outputs: Vec<OutputInfo>,
    }

    impl FakeBackend {
        fn new(id: &'static str, detects: bool) -> Self {
            Self {
                id,
                detects,
                outputs: Vec::new(),
            }
        }
    }

    impl ShellBackend for FakeBackend {
        fn id(&self) -> &'static str {
            self.id
        }
        fn detect(&self, _env: EnvLookup<'_>) -> bool {
            self.detects
        }
        fn list_outputs(&self) -> Result<Vec<OutputInfo>, ShellError> {
            Ok(self.outputs.clone())
        }
    }

    fn env_with<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    fn config(backend: &str, order: &[&str]) -> ShellConfig {
        let mut shell = ShellConfig {
            backend: backend.to_string(),
            ..ShellConfig::default()
        };
        if !order.is_empty() {
            shell.detect_order = order.iter().map(|id| (*id).to_string()).collect();
        }
        shell
    }

    fn registry(backends: &[(&'static str, bool)]) -> Registry {
        let mut registry = Registry::new();
        for (id, detects) in backends {
            registry.register(Box::new(FakeBackend::new(id, *detects)));
        }
        registry
    }

    #[test]
    fn ids_are_reported_in_registration_order() {
        let registry = registry(&[("hyprland", true), ("generic-layer-shell", true)]);
        assert_eq!(registry.ids(), vec!["hyprland", "generic-layer-shell"]);
        assert!(registry.contains("hyprland"));
        assert!(!registry.contains("caelestia"));
    }

    #[test]
    fn registering_the_same_id_twice_keeps_one_backend() {
        let mut registry = Registry::new();
        registry.register(Box::new(FakeBackend::new("hyprland", true)));
        registry.register(Box::new(FakeBackend::new("hyprland", false)));
        assert_eq!(registry.ids(), vec!["hyprland"]);
        assert!(!registry.get("hyprland").unwrap().detect(&|_| None));
    }

    #[test]
    fn explicit_backend_is_selected_by_id() {
        let registry = registry(&[("hyprland", false), ("generic-layer-shell", true)]);
        let selected = registry
            .select(&config("generic-layer-shell", &[]), &env_with(&[]))
            .unwrap();
        assert_eq!(selected.id(), "generic-layer-shell");
    }

    #[test]
    fn explicit_unknown_backend_is_an_error_not_a_fallback() {
        let registry = registry(&[("hyprland", true)]);
        let error = registry
            .select(&config("kde", &[]), &env_with(&[]))
            .err()
            .expect("an unknown backend must not be selected");
        match &error {
            SelectError::UnknownBackend {
                requested,
                available,
            } => {
                assert_eq!(requested, "kde");
                assert_eq!(available, "hyprland");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn auto_walks_the_configured_detect_order() {
        // caelestia is not registered (that is P3), so hyprland must win.
        let registry = registry(&[("hyprland", true), ("generic-layer-shell", true)]);
        let shell = config("auto", &["caelestia", "hyprland", "generic-layer-shell"]);
        let selected = registry.select(&shell, &env_with(&[])).unwrap();
        assert_eq!(selected.id(), "hyprland");
    }

    #[test]
    fn auto_prefers_an_earlier_backend_that_detects_itself() {
        let registry = registry(&[("hyprland", false), ("generic-layer-shell", true)]);
        let shell = config("auto", &["hyprland", "generic-layer-shell"]);
        let selected = registry.select(&shell, &env_with(&[])).unwrap();
        assert_eq!(selected.id(), "generic-layer-shell");
    }

    #[test]
    fn auto_with_nothing_detecting_reports_what_was_tried() {
        let registry = registry(&[("hyprland", false)]);
        let shell = config("auto", &["hyprland", "generic-layer-shell"]);
        let error = registry
            .select(&shell, &env_with(&[]))
            .err()
            .expect("nothing detects itself, so selection must fail");
        match &error {
            SelectError::NoBackendDetected { tried } => {
                assert!(tried.contains("hyprland"), "{tried}");
                assert!(tried.contains("generic-layer-shell"), "{tried}");
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(error.to_string().contains("shell.backend"), "{error}");
    }

    #[test]
    fn auto_uses_the_builtin_order_when_the_config_leaves_it_empty() {
        assert_eq!(
            default_detect_order(),
            vec!["caelestia", "hyprland", "generic-layer-shell"]
        );
        let registry = registry(&[("generic-layer-shell", true), ("hyprland", false)]);
        let shell = config("auto", &[]);
        assert_eq!(
            registry.select(&shell, &env_with(&[])).unwrap().id(),
            "generic-layer-shell"
        );
    }

    #[test]
    fn an_explicit_backend_is_used_even_when_it_does_not_detect_itself() {
        // The user asked for it; failing at first use with a real error beats
        // silently switching their compositor.
        let registry = registry(&[("hyprland", false)]);
        let selected = registry
            .select(&config("hyprland", &[]), &env_with(&[]))
            .unwrap();
        assert_eq!(selected.id(), "hyprland");
    }

    #[test]
    fn empty_backend_string_behaves_like_auto() {
        let registry = registry(&[("hyprland", true)]);
        assert_eq!(
            registry
                .select(&config("", &[]), &env_with(&[]))
                .unwrap()
                .id(),
            "hyprland"
        );
    }
}
