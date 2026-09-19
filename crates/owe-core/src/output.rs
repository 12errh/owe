//! Outputs (monitors): what we know about them and how a user's target string
//! resolves against them.
//!
//! Pure logic on purpose: resolving `owectl set img.png -o HDMI-A-1` must be
//! testable without a compositor, and every shell backend (Hyprland now,
//! Caelestia and others later) feeds the same [`OutputInfo`].

use std::fmt;

use thiserror::Error;

/// One connected output, as far as the active shell backend can describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputInfo {
    /// Connector name (`eDP-1`, `HDMI-A-1`) — the stable identity users type.
    pub name: String,
    /// Human-readable make/model description, used as a secondary match.
    pub description: String,
    /// Logical width in physical pixels (already scaled).
    pub width: u32,
    /// Logical height in physical pixels (already scaled).
    pub height: u32,
    /// Position on the global layout.
    pub x: i32,
    /// Position on the global layout.
    pub y: i32,
    /// Whether this output currently has keyboard focus.
    pub focused: bool,
    /// Whether the output is enabled (not `disabled`/suspended).
    pub active: bool,
}

impl OutputInfo {
    /// Pixel size a surface on this output must render at.
    pub fn pixel_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

/// A request for one or more outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputTarget {
    /// Every active output.
    All,
    /// Whichever output currently has focus.
    Focused,
    /// One output by name or description.
    Named(String),
}

impl OutputTarget {
    /// Parse the user-facing form used by `owectl -o` and the IPC API.
    ///
    /// `all`, `focused` and an empty string are keywords; anything else is a
    /// name/description to match.
    pub fn parse(spec: &str) -> Self {
        match spec.trim() {
            "" | "all" | "*" => Self::All,
            "focused" | "current" => Self::Focused,
            other => Self::Named(other.to_string()),
        }
    }
}

/// Why a target could not be resolved to outputs.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum OutputSelectError {
    /// No outputs are connected at all.
    #[error("no outputs are connected")]
    NoOutputs,

    /// The target was `focused` but no output reports focus.
    #[error("no output is focused right now (pass an explicit output name or `all`)")]
    NoFocusedOutput,

    /// A named target matched nothing.
    #[error("no output matches `{target}`; connected: {available}")]
    UnknownTarget {
        /// What the user asked for.
        target: String,
        /// Comma-separated names that do exist.
        available: String,
    },
}

/// Resolve a target against the connected outputs.
///
/// Matching order for a named target (FR-LIB-4): exact name, then case-insensitive
/// name, then the description containing the string. Precedence is deliberately
/// written down and tested, because silently matching a *different* monitor is
/// the kind of bug users blame on their wallpaper daemon.
pub fn resolve<'a>(
    outputs: &'a [OutputInfo],
    target: &OutputTarget,
) -> Result<Vec<&'a OutputInfo>, OutputSelectError> {
    let active: Vec<&OutputInfo> = outputs.iter().filter(|output| output.active).collect();

    match target {
        OutputTarget::All => {
            if active.is_empty() {
                return Err(OutputSelectError::NoOutputs);
            }
            Ok(active)
        }
        OutputTarget::Focused => {
            let focused = outputs
                .iter()
                .find(|output| output.focused && output.active);
            focused
                .map(|output| vec![output])
                .ok_or(OutputSelectError::NoFocusedOutput)
        }
        OutputTarget::Named(name) => {
            let wanted = name.trim();
            if let Some(found) = outputs
                .iter()
                .find(|output| output.active && output.name == wanted)
            {
                return Ok(vec![found]);
            }
            if let Some(found) = outputs
                .iter()
                .find(|output| output.active && output.name.eq_ignore_ascii_case(wanted))
            {
                return Ok(vec![found]);
            }
            let needle = wanted.to_ascii_lowercase();
            if let Some(found) = outputs.iter().find(|output| {
                output.active && output.description.to_ascii_lowercase().contains(&needle)
            }) {
                return Ok(vec![found]);
            }
            Err(OutputSelectError::UnknownTarget {
                target: wanted.to_string(),
                available: names(outputs),
            })
        }
    }
}

/// Comma-separated names of every output, for error messages and `--help`.
pub fn names(outputs: &[OutputInfo]) -> String {
    if outputs.is_empty() {
        return "none".to_string();
    }
    outputs
        .iter()
        .map(|output| output.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

impl fmt::Display for OutputInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}x{}+{}+{}{}",
            self.name,
            self.width,
            self.height,
            self.x,
            self.y,
            if self.focused { " (focused)" } else { "" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(name: &str, description: &str, focused: bool) -> OutputInfo {
        OutputInfo {
            name: name.to_string(),
            description: description.to_string(),
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
            focused,
            active: true,
        }
    }

    fn two_outputs() -> Vec<OutputInfo> {
        vec![
            output("eDP-1", "Samsung Display Corp. 0x5441", true),
            output("HDMI-A-1", "Dell Inc. DELL U2720Q", false),
        ]
    }

    #[test]
    fn parses_target_keywords() {
        assert_eq!(OutputTarget::parse(""), OutputTarget::All);
        assert_eq!(OutputTarget::parse("all"), OutputTarget::All);
        assert_eq!(OutputTarget::parse("*"), OutputTarget::All);
        assert_eq!(OutputTarget::parse("focused"), OutputTarget::Focused);
        assert_eq!(OutputTarget::parse("  current "), OutputTarget::Focused);
        assert_eq!(
            OutputTarget::parse("eDP-1"),
            OutputTarget::Named("eDP-1".to_string())
        );
    }

    #[test]
    fn all_returns_every_active_output() {
        let outputs = two_outputs();
        let resolved = resolve(&outputs, &OutputTarget::All).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].name, "eDP-1");
    }

    #[test]
    fn all_ignores_inactive_outputs() {
        let mut outputs = two_outputs();
        outputs[1].active = false;
        let resolved = resolve(&outputs, &OutputTarget::All).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "eDP-1");
    }

    #[test]
    fn all_with_nothing_connected_is_an_error() {
        assert_eq!(
            resolve(&[], &OutputTarget::All).unwrap_err(),
            OutputSelectError::NoOutputs
        );
    }

    #[test]
    fn focused_picks_the_focused_output() {
        let outputs = two_outputs();
        let resolved = resolve(&outputs, &OutputTarget::Focused).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "eDP-1");
    }

    #[test]
    fn focused_without_focus_explains_what_to_do() {
        let mut outputs = two_outputs();
        for output in &mut outputs {
            output.focused = false;
        }
        let error = resolve(&outputs, &OutputTarget::Focused).unwrap_err();
        assert_eq!(error, OutputSelectError::NoFocusedOutput);
        assert!(error.to_string().contains("explicit output name"));
    }

    #[test]
    fn named_match_is_exact_first_then_case_insensitive() {
        let outputs = two_outputs();
        assert_eq!(
            resolve(&outputs, &OutputTarget::Named("HDMI-A-1".into())).unwrap()[0].name,
            "HDMI-A-1"
        );
        assert_eq!(
            resolve(&outputs, &OutputTarget::Named("hdmi-a-1".into())).unwrap()[0].name,
            "HDMI-A-1"
        );
    }

    #[test]
    fn named_match_falls_back_to_description() {
        let outputs = two_outputs();
        let resolved = resolve(&outputs, &OutputTarget::Named("u2720q".into())).unwrap();
        assert_eq!(resolved[0].name, "HDMI-A-1");
    }

    #[test]
    fn unknown_target_lists_what_exists() {
        let outputs = two_outputs();
        let error = resolve(&outputs, &OutputTarget::Named("DP-9".into())).unwrap_err();
        match &error {
            OutputSelectError::UnknownTarget { target, available } => {
                assert_eq!(target, "DP-9");
                assert!(available.contains("eDP-1"));
                assert!(available.contains("HDMI-A-1"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn inactive_output_is_not_matched_by_name() {
        let mut outputs = two_outputs();
        outputs[1].active = false;
        assert!(resolve(&outputs, &OutputTarget::Named("HDMI-A-1".into())).is_err());
    }

    #[test]
    fn pixel_size_is_the_logical_size() {
        assert_eq!(two_outputs()[0].pixel_size(), (1920, 1080));
    }

    #[test]
    fn display_shows_geometry_and_focus() {
        let text = two_outputs()[0].to_string();
        assert!(text.starts_with("eDP-1 1920x1080+0+0"), "{text}");
        assert!(text.contains("(focused)"), "{text}");
    }
}
