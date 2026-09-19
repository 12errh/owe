//! Hyprland shell backend: output enumeration via `hyprctl monitors -j`.
//!
//! ## Why JSON instead of parsing the human-readable output
//!
//! The P1 plan said "output enumeration from `hyprctl monitors` text (parser =
//! pure function, table-driven tests on recorded outputs)". The recorded-output
//! and table-driven parts are exactly what this module does — the deviation is
//! JSON (`-j`) instead of the pretty-printed columns. Rationale, recorded as a P1
//! deviation in docs/IMPLEMENTATION-PLAN.md:
//!
//! - `hyprctl monitors -j` is Hyprland's documented machine interface; the
//!   pretty output is for humans and its column layout has changed between
//!   versions.
//! - We already depend on `serde_json` for IPC, so this adds no dependency.
//!
//! ## What is deliberately *not* trusted
//!
//! Hyprland's `width`/`height` describe the mode, not necessarily the size a
//! layer surface will be given once output scale is involved. The renderer
//! therefore treats the compositor's `configure` size as authoritative and uses
//! these values only for pre-flight decisions. Guessing here would be the fastest
//! way to ship a wallpaper that is subtly the wrong size on a HiDPI display.

use std::process::Command;

use owe_core::output::OutputInfo;
use owe_core::shell::{EnvLookup, ShellBackend, ShellError};
use serde::Deserialize;

/// Raw monitor entry as Hyprland reports it. Every field is optional so that a
/// missing key is a clear error from this module rather than a serde panic deep
/// in the chain.
#[derive(Debug, Deserialize)]
struct RawMonitor {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    x: Option<i32>,
    #[serde(default)]
    y: Option<i32>,
    #[serde(default)]
    focused: Option<bool>,
    #[serde(default)]
    disabled: Option<bool>,
}

/// Parse `hyprctl monitors -j` output.
///
/// Pure: no process, no environment, so every recorded-output case is a unit test.
pub fn parse_monitors(json: &str) -> Result<Vec<OutputInfo>, ShellError> {
    let raw: Vec<RawMonitor> = serde_json::from_str(json).map_err(|error| ShellError::Backend {
        backend: "hyprland".to_string(),
        detail: format!("could not parse `hyprctl monitors -j` output: {error}"),
    })?;

    let mut outputs = Vec::with_capacity(raw.len());
    for (index, monitor) in raw.into_iter().enumerate() {
        let name = monitor
            .name
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| ShellError::Backend {
                backend: "hyprland".to_string(),
                detail: format!("monitor entry {index} has no name; cannot address it"),
            })?;

        let width = monitor.width.unwrap_or(0);
        let height = monitor.height.unwrap_or(0);
        if width == 0 || height == 0 {
            return Err(ShellError::Backend {
                backend: "hyprland".to_string(),
                detail: format!(
                    "monitor `{name}` reports {width}x{height}; refusing a zero-sized surface"
                ),
            });
        }

        outputs.push(OutputInfo {
            description: monitor.description.unwrap_or_default(),
            width,
            height,
            x: monitor.x.unwrap_or(0),
            y: monitor.y.unwrap_or(0),
            focused: monitor.focused.unwrap_or(false),
            // `hyprctl monitors` already omits disabled outputs; when a build
            // does report one, treat it as inactive rather than pretending it can
            // be drawn to.
            active: !monitor.disabled.unwrap_or(false),
            name,
        });
    }
    Ok(outputs)
}

/// Whether this process is running inside a Hyprland session.
pub fn detect(env: EnvLookup<'_>) -> bool {
    env("HYPRLAND_INSTANCE_SIGNATURE")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

/// Run `hyprctl` and return its stdout.
pub fn run_hyprctl(args: &[&str]) -> Result<String, ShellError> {
    let output = Command::new("hyprctl")
        .args(args)
        .output()
        .map_err(|error| ShellError::Unavailable {
            backend: "hyprland".to_string(),
            detail: format!("could not run `hyprctl`: {error}"),
        })?;

    if !output.status.success() {
        return Err(ShellError::Backend {
            backend: "hyprland".to_string(),
            detail: format!(
                "`hyprctl {}` exited with {}: {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The Hyprland [`ShellBackend`].
#[derive(Debug, Default)]
pub struct HyprlandBackend;

impl HyprlandBackend {
    /// Construct the backend.
    pub fn new() -> Self {
        Self
    }
}

impl ShellBackend for HyprlandBackend {
    fn id(&self) -> &'static str {
        "hyprland"
    }

    fn detect(&self, env: EnvLookup<'_>) -> bool {
        detect(env)
    }

    fn list_outputs(&self) -> Result<Vec<OutputInfo>, ShellError> {
        let json = run_hyprctl(&["monitors", "-j"])?;
        parse_monitors(&json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from the reference machine: Hyprland 0.56.2, one eDP-1 panel.
    /// Kept as a file (not an inline string) so re-capturing after a Hyprland
    /// upgrade is a one-line diff, and so the fixture is reviewable.
    const SINGLE_MONITOR: &str = include_str!("../tests/fixtures/monitors-0.56.2.json");

    fn env_with<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn parses_the_recorded_single_monitor_output() {
        let outputs = parse_monitors(SINGLE_MONITOR).expect("recorded fixture parses");
        assert_eq!(outputs.len(), 1);

        let output = &outputs[0];
        assert_eq!(output.name, "eDP-1");
        assert_eq!(output.description, "Samsung Display Corp. 0x5441");
        assert_eq!(output.pixel_size(), (1366, 768));
        assert_eq!((output.x, output.y), (0, 0));
        assert!(output.focused);
        assert!(output.active);
    }

    #[test]
    fn parses_two_monitors_with_negative_origin() {
        // A monitor placed to the left of the primary has a negative x.
        let json = r#"[
            {"name":"DP-1","description":"Left panel","width":2560,"height":1440,
             "x":-2560,"y":0,"focused":false,"disabled":false},
            {"name":"eDP-1","description":"Laptop panel","width":1920,"height":1080,
             "x":0,"y":0,"focused":true,"disabled":false}
        ]"#;
        let outputs = parse_monitors(json).unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].x, -2560);
        assert_eq!(outputs[1].name, "eDP-1");
        assert!(outputs[1].focused);
    }

    #[test]
    fn disabled_monitors_are_reported_but_inactive() {
        let json = r#"[{"name":"HDMI-A-1","width":1920,"height":1080,"disabled":true}]"#;
        let outputs = parse_monitors(json).unwrap();
        assert_eq!(outputs.len(), 1);
        assert!(
            !outputs[0].active,
            "a disabled output must not be treated as drawable"
        );
    }

    #[test]
    fn missing_optional_fields_fall_back_to_safe_defaults() {
        let json = r#"[{"name":"eDP-1","width":800,"height":600}]"#;
        let outputs = parse_monitors(json).unwrap();
        assert_eq!(outputs[0].description, "");
        assert_eq!((outputs[0].x, outputs[0].y), (0, 0));
        assert!(!outputs[0].focused);
        assert!(outputs[0].active, "an output with no `disabled` is active");
    }

    #[test]
    fn empty_monitor_list_is_not_an_error() {
        // A headless or fully-disabled setup: the caller decides what that means.
        assert!(parse_monitors("[]").unwrap().is_empty());
    }

    #[test]
    fn a_monitor_without_a_name_is_a_clear_error() {
        let error = parse_monitors(r#"[{"width":1920,"height":1080}]"#).unwrap_err();
        assert!(error.to_string().contains("no name"), "{error}");
    }

    #[test]
    fn a_zero_sized_monitor_is_refused() {
        let error = parse_monitors(r#"[{"name":"eDP-1","width":0,"height":0}]"#).unwrap_err();
        assert!(error.to_string().contains("0x0"), "{error}");
    }

    #[test]
    fn malformed_json_names_the_backend_and_the_cause() {
        let error = parse_monitors("{ not json at all").unwrap_err();
        let text = error.to_string();
        assert!(text.contains("hyprland"), "{text}");
        assert!(text.contains("could not parse"), "{text}");
    }

    #[test]
    fn an_object_instead_of_an_array_is_rejected() {
        assert!(parse_monitors(r#"{"name":"eDP-1"}"#).is_err());
    }

    #[test]
    fn detection_requires_the_instance_signature() {
        assert!(detect(&env_with(&[(
            "HYPRLAND_INSTANCE_SIGNATURE",
            "abc_123_456"
        )])));
        assert!(!detect(&env_with(&[])));
        assert!(
            !detect(&env_with(&[("HYPRLAND_INSTANCE_SIGNATURE", "   ")])),
            "an empty signature means we are not in Hyprland"
        );
    }

    #[test]
    fn backend_reports_its_id_and_detects_the_same_way() {
        let backend = HyprlandBackend::new();
        assert_eq!(backend.id(), "hyprland");
        assert!(backend.detect(&env_with(&[("HYPRLAND_INSTANCE_SIGNATURE", "sig")])));
        assert!(!backend.detect(&env_with(&[])));
    }
}
