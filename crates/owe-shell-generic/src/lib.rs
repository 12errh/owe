//! Generic Wayland shell backend: output enumeration with no compositor-specific
//! code.
//!
//! # What this backend is for
//!
//! Hyprland gives us names, focus and workspace state cheaply (`hyprctl`). A
//! wlroots compositor we have no integration for, or a nested compositor in CI,
//! gives us none of that — but the core Wayland protocols still describe the
//! outputs, so a wallpaper can still be put on the right screens. That is the
//! floor: OWE works on a plain `wl_output`-speaking compositor.
//!
//! # What it honestly cannot do (P1)
//!
//! - **Focus.** `wl_output` has no concept of the focused output, so
//!   `focused` is always `false` here. `owectl set img.png -o focused` on this
//!   backend therefore reports a precise "no output matched" error instead of
//!   guessing which screen the user means. Compositor-specific backends fix this
//!   properly (P3 for Caelestia, P2 for Hyprland workspace data).
//! - **Per-monitor scale.** We report the logical size the compositor advertises.
//!   Fractional-scaling-aware rendering arrives in P2 with physical-size support.
//!
//! # Why not just use this everywhere?
//!
//! Because a backend is also the place where a shell hands us *events* (P3's
//! event bus) and where coexistence rules live (P3's swww/hyprpaper detection).
//! The generic backend is the correct default for "something else", not a
//! replacement for the integrations.

use owe_core::output::OutputInfo;
use owe_core::shell::{EnvLookup, ShellBackend, ShellError};
use smithay_client_toolkit::{
    delegate_output, delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
};
use wayland_client::{
    Connection, Proxy, QueueHandle, globals::registry_queue_init, protocol::wl_output,
};

/// The id this backend registers under (`shell.backend = generic-layer-shell`).
pub const ID: &str = "generic-layer-shell";

/// Output enumeration over plain Wayland.
#[derive(Debug, Default)]
pub struct GenericBackend;

impl GenericBackend {
    /// A new backend. Holds no connection: one is opened per call, which keeps
    /// the type `Send + Sync` and means a compositor restart is not a sticky
    /// failure.
    pub fn new() -> Self {
        Self
    }
}

impl ShellBackend for GenericBackend {
    fn id(&self) -> &'static str {
        ID
    }

    /// Detectable whenever a Wayland session is reachable at all.
    ///
    /// This deliberately does *not* claim availability in a session with no
    /// compositor: `WAYLAND_DISPLAY` unset means there is nothing to talk to, and
    /// reporting a backend that would fail on first use is how a GUI ends up
    /// showing a healthy status for a broken session.
    fn detect(&self, env: EnvLookup<'_>) -> bool {
        env("WAYLAND_DISPLAY")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    }

    fn list_outputs(&self) -> Result<Vec<OutputInfo>, ShellError> {
        let connection = Connection::connect_to_env().map_err(|error| ShellError::Unavailable {
            backend: ID.to_string(),
            detail: format!("cannot connect to the Wayland display: {error}"),
        })?;

        let (globals, mut queue) =
            registry_queue_init::<Enumerator>(&connection).map_err(|error| {
                ShellError::Unavailable {
                    backend: ID.to_string(),
                    detail: format!("cannot read the Wayland registry: {error}"),
                }
            })?;
        let qh = queue.handle();

        let mut state = Enumerator {
            registry_state: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
        };

        // Two roundtrips: the first advertises the outputs, the second delivers
        // their geometry/name events. One roundtrip would report outputs with no
        // usable size, which is exactly the kind of half-truth this project tries
        // not to ship.
        for _ in 0..2 {
            queue
                .roundtrip(&mut state)
                .map_err(|error| ShellError::Unavailable {
                    backend: ID.to_string(),
                    detail: format!("Wayland roundtrip failed: {error}"),
                })?;
        }

        let outputs: Vec<OutputInfo> = state
            .output_state
            .outputs()
            .map(|output| {
                let info = state.output_state.info(&output);
                let (width, height) = info
                    .as_ref()
                    .and_then(|info| info.logical_size)
                    .map(|(w, h)| (w.max(0) as u32, h.max(0) as u32))
                    .unwrap_or((0, 0));
                let (x, y) = info.as_ref().map(|info| info.location).unwrap_or((0, 0));

                OutputInfo {
                    name: info
                        .as_ref()
                        .and_then(|info| info.name.clone())
                        .unwrap_or_else(|| format!("wl-output-{}", output.id().protocol_id())),
                    description: info
                        .as_ref()
                        .and_then(|info| info.description.clone())
                        .unwrap_or_default(),
                    width,
                    height,
                    x,
                    y,
                    // See the module docs: the core protocol cannot tell us this.
                    focused: false,
                    // Everything in the registry is a live output.
                    active: true,
                }
            })
            .collect();

        if outputs.is_empty() {
            return Err(ShellError::Backend {
                backend: ID.to_string(),
                detail: "the compositor reports no outputs".to_string(),
            });
        }
        Ok(outputs)
    }
}

/// Minimal SCTK state: just enough to be told about outputs.
struct Enumerator {
    registry_state: RegistryState,
    output_state: OutputState,
}

impl ProvidesRegistryState for Enumerator {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState];
}

impl OutputHandler for Enumerator {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

delegate_output!(Enumerator);
delegate_registry!(Enumerator);

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn detects_any_reachable_wayland_display() {
        let backend = GenericBackend::new();
        assert_eq!(backend.id(), ID);
        assert!(backend.detect(&env_with(&[("WAYLAND_DISPLAY", "wayland-1")])));
        assert!(
            backend.detect(&env_with(&[("WAYLAND_DISPLAY", "wayland-0")])),
            "a nested compositor socket counts"
        );
    }

    #[test]
    fn does_not_claim_to_work_without_a_compositor() {
        let backend = GenericBackend::new();
        assert!(
            !backend.detect(&env_with(&[])),
            "no WAYLAND_DISPLAY means there is nothing to talk to"
        );
        assert!(
            !backend.detect(&env_with(&[("WAYLAND_DISPLAY", "")])),
            "an empty display name is not a session"
        );
        assert!(
            !backend.detect(&env_with(&[("WAYLAND_DISPLAY", "   ")])),
            "whitespace is not a session either"
        );
    }

    #[test]
    fn listing_outside_a_session_fails_with_the_real_reason() {
        // The environment this test runs in may or may not have a compositor, so
        // assert the *shape* of the failure, not that it happens. A backend that
        // silently returns an empty output list would make \"no monitors\" and
        // \"cannot talk to Wayland\" indistinguishable to the user.
        match GenericBackend::new().list_outputs() {
            Ok(outputs) => {
                assert!(
                    !outputs.is_empty(),
                    "an Ok result must never be an empty list"
                );
                for output in outputs {
                    assert!(!output.name.is_empty(), "every output needs an identity");
                    assert!(output.active);
                    // Documented limitation, asserted so it cannot quietly change.
                    assert!(!output.focused, "the core protocol cannot report focus");
                }
            }
            Err(error) => {
                let text = error.to_string();
                assert!(!text.is_empty(), "errors must say something");
                assert!(
                    text.contains(ID) || text.contains("Wayland"),
                    "the error must name the backend or the reason: {text}"
                );
            }
        }
    }
}
