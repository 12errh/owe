//! Resolving `[outputs.*]` config sections against real outputs (FR-LIB-4).
//!
//! Three kinds of section can name an output, in a fixed precedence:
//!
//! ```text
//! exact name  >  description fragment  >  "any"
//! ```
//!
//! The precedence itself is one line; what earns a module is the *reporting*.
//! Silently letting a description match beat an exact one, or letting two
//! descriptions fight, produces the bug users describe as "it put the wrong
//! wallpaper on my second monitor" — which is unfixable without a message saying
//! which sections matched and which one won. So resolution returns both.

use std::collections::BTreeMap;

use crate::config::{OutputConfig, RenderConfig, Transition};
use crate::output::OutputInfo;

/// Why a section matched an output, in precedence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Precedence {
    /// The section key is the output's connector name.
    Exact,
    /// The section key appears in the output's make/model description.
    Description,
    /// The section is the catch-all `any`.
    Any,
}

impl Precedence {
    /// Stable id for logs and the GUI.
    pub fn as_str(self) -> &'static str {
        match self {
            Precedence::Exact => "exact",
            Precedence::Description => "description",
            Precedence::Any => "any",
        }
    }
}

/// The catch-all section key.
pub const CATCH_ALL: &str = "any";

/// A section that matched but lost to a more specific one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// The losing section key.
    pub section: String,
    /// Why it matched.
    pub precedence: Precedence,
    /// One-line explanation, ready to log or show.
    pub message: String,
}

/// The winning section for one output, plus everything worth saying about it.
//
// No `Eq`: `OutputConfig` contains floats-free-but-arbitrary user strings and only
// implements `PartialEq`, and claiming `Eq` here would just move the problem.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputResolution<'a> {
    /// The section key that won.
    pub section: String,
    /// Why it won.
    pub precedence: Precedence,
    /// The winning configuration.
    pub config: &'a OutputConfig,
    /// Sections that also matched and lost, most specific first.
    pub conflicts: Vec<Conflict>,
}

impl OutputResolution<'_> {
    /// The wallpaper this output should get, if one is configured.
    pub fn wallpaper(&self) -> Option<&str> {
        self.config.wallpaper.as_deref()
    }

    /// A one-line explanation for the log: which section won and why.
    pub fn explain(&self) -> String {
        let mut text = format!(
            "output config `{}` matched by {}",
            self.section,
            self.precedence.as_str()
        );
        if !self.conflicts.is_empty() {
            let losers: Vec<String> = self
                .conflicts
                .iter()
                .map(|conflict| {
                    format!("`{}` ({})", conflict.section, conflict.precedence.as_str())
                })
                .collect();
            text.push_str(&format!("; also matched: {}", losers.join(", ")));
        }
        text
    }
}

/// Find the `[outputs.*]` section that applies to `output`.
///
/// Resolution is deterministic and total:
///
/// 1. an exact connector-name match wins (case-insensitively, because monitor
///    names are ASCII and a user typing `edp-1` means `eDP-1`);
/// 2. otherwise the **longest** description fragment that appears in the
///    output's description wins (a more specific key beat a vaguer one);
/// 3. otherwise the `any` section applies.
///
/// Every section that matched and lost is returned in [`OutputResolution::conflicts`],
/// so the daemon can log the ambiguity instead of hiding it.
pub fn resolve<'a>(
    sections: &'a BTreeMap<String, OutputConfig>,
    output: &OutputInfo,
) -> Option<OutputResolution<'a>> {
    let mut exact: Option<(&String, &OutputConfig)> = None;
    let mut descriptions: Vec<(&String, &OutputConfig)> = Vec::new();
    let mut catch_all: Option<(&String, &OutputConfig)> = None;

    for (key, config) in sections {
        if key.eq_ignore_ascii_case(&output.name) {
            exact = Some((key, config));
            continue;
        }
        if key.eq_ignore_ascii_case(CATCH_ALL) {
            catch_all = Some((key, config));
            continue;
        }
        let needle = key.trim().to_ascii_lowercase();
        if !needle.is_empty() && output.description.to_ascii_lowercase().contains(&needle) {
            descriptions.push((key, config));
        }
    }

    // Longest first: `DELL U2720Q` beats `DELL` when both are configured.
    descriptions.sort_by(|left, right| {
        right
            .0
            .len()
            .cmp(&left.0.len())
            .then_with(|| left.0.cmp(right.0))
    });

    let (winner, precedence) = match (exact, descriptions.first().copied(), catch_all) {
        (Some(exact), _, _) => (exact, Precedence::Exact),
        (None, Some(description), _) => (description, Precedence::Description),
        (None, None, Some(catch_all)) => (catch_all, Precedence::Any),
        (None, None, None) => return None,
    };

    let mut conflicts = Vec::new();
    for (key, _) in &descriptions {
        if *key == winner.0 {
            continue;
        }
        conflicts.push(Conflict {
            section: (*key).clone(),
            precedence: Precedence::Description,
            message: format!(
                "`{key}` also describes `{}` but `{}` is more specific",
                output.name, winner.0
            ),
        });
    }
    if let Some((key, _)) = catch_all
        && key != winner.0
    {
        conflicts.push(Conflict {
            section: key.clone(),
            precedence: Precedence::Any,
            message: format!(
                "`any` applies to `{}` too, but `{}` ({}) is more specific",
                output.name,
                winner.0,
                precedence.as_str()
            ),
        });
    }

    Some(OutputResolution {
        section: winner.0.clone(),
        precedence,
        config: winner.1,
        conflicts,
    })
}

/// The transition to use for a change on this output: the output's own, else the
/// global default (BACKEND-DESIGN §4).
pub fn transition_for(
    resolution: Option<&OutputResolution<'_>>,
    render: &RenderConfig,
) -> Transition {
    resolution
        .and_then(|resolution| resolution.config.transition.clone())
        .unwrap_or_else(|| render.default_transition.clone())
}

/// The FPS cap for this output, if the section sets one.
pub fn fps_cap_for(resolution: Option<&OutputResolution<'_>>) -> Option<u32> {
    resolution.and_then(|resolution| resolution.config.fps_cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Transition;

    fn output(name: &str, description: &str) -> OutputInfo {
        OutputInfo {
            name: name.to_string(),
            description: description.to_string(),
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
            focused: true,
            active: true,
        }
    }

    fn sections(entries: &[(&str, &str)]) -> BTreeMap<String, OutputConfig> {
        entries
            .iter()
            .map(|(key, wallpaper)| {
                (
                    (*key).to_string(),
                    OutputConfig {
                        wallpaper: Some((*wallpaper).to_string()),
                        ..OutputConfig::default()
                    },
                )
            })
            .collect()
    }

    /// One row of the precedence table: configured sections, the output, and the
    /// section/precedence expected to win (`None` when nothing matches).
    type Case<'a> = (
        Vec<(&'a str, &'a str)>,
        OutputInfo,
        Option<(&'a str, Precedence)>,
    );

    /// Table-driven: (sections, output, expected winning section, expected precedence).
    #[test]
    fn precedence_table() {
        let cases: Vec<Case> = vec![
            // Exact name wins over everything else that matched.
            (
                vec![
                    ("eDP-1", "exact.png"),
                    ("Samsung", "description.png"),
                    ("any", "any.png"),
                ],
                output("eDP-1", "Samsung Display Corp. 0x5441"),
                Some(("eDP-1", Precedence::Exact)),
            ),
            // Case-insensitive name match is still "exact": same target, typed lazily.
            (
                vec![("edp-1", "exact.png")],
                output("eDP-1", "Samsung"),
                Some(("edp-1", Precedence::Exact)),
            ),
            // No name section: the description fragment applies.
            (
                vec![("U2720Q", "dell.png"), ("any", "any.png")],
                output("HDMI-A-1", "Dell Inc. DELL U2720Q"),
                Some(("U2720Q", Precedence::Description)),
            ),
            // Only `any` matched.
            (
                vec![("any", "any.png")],
                output("DP-1", "Some Panel"),
                Some(("any", Precedence::Any)),
            ),
            // Nothing matched at all: the caller falls back to defaults.
            (
                vec![("DP-9", "elsewhere.png")],
                output("eDP-1", "Samsung"),
                None,
            ),
            // An empty description cannot match a fragment.
            (vec![("Dell", "dell.png")], output("HDMI-A-1", ""), None),
            // The longest description fragment wins.
            (
                vec![("Dell", "vague.png"), ("DELL U2720Q", "precise.png")],
                output("HDMI-A-1", "Dell Inc. DELL U2720Q"),
                Some(("DELL U2720Q", Precedence::Description)),
            ),
            // A description match is case-insensitive.
            (
                vec![("u2720q", "dell.png")],
                output("HDMI-A-1", "Dell Inc. DELL U2720Q"),
                Some(("u2720q", Precedence::Description)),
            ),
        ];

        for (entries, monitor, expected) in cases {
            let sections = sections(&entries);
            let resolved = resolve(&sections, &monitor);
            match expected {
                Some((section, precedence)) => {
                    let resolved = resolved.unwrap_or_else(|| {
                        panic!("expected `{section}` to match {}", monitor.name)
                    });
                    assert_eq!(resolved.section, section, "{}", monitor.name);
                    assert_eq!(resolved.precedence, precedence, "{}", monitor.name);
                }
                None => assert!(resolved.is_none(), "{} should match nothing", monitor.name),
            }
        }
    }

    #[test]
    fn losing_sections_are_reported_as_conflicts() {
        let sections = sections(&[
            ("eDP-1", "exact.png"),
            ("Samsung", "description.png"),
            ("any", "any.png"),
        ]);
        let resolved = resolve(&sections, &output("eDP-1", "Samsung Display Corp.")).unwrap();

        assert_eq!(resolved.section, "eDP-1");
        assert_eq!(resolved.conflicts.len(), 2, "{:?}", resolved.conflicts);
        assert_eq!(resolved.conflicts[0].section, "Samsung");
        assert_eq!(resolved.conflicts[1].section, "any");

        let text = resolved.explain();
        assert!(text.contains("exact"), "{text}");
        assert!(text.contains("also matched"), "{text}");
        assert!(text.contains("Samsung"), "{text}");
    }

    #[test]
    fn two_description_matches_report_the_loser() {
        let sections = sections(&[("Dell", "vague.png"), ("DELL U2720Q", "precise.png")]);
        let resolved = resolve(&sections, &output("HDMI-A-1", "Dell Inc. DELL U2720Q")).unwrap();

        assert_eq!(resolved.wallpaper(), Some("precise.png"));
        assert_eq!(resolved.conflicts.len(), 1);
        assert_eq!(resolved.conflicts[0].section, "Dell");
        assert!(
            resolved.conflicts[0].message.contains("more specific"),
            "{:?}",
            resolved.conflicts[0]
        );
    }

    #[test]
    fn a_catch_all_alone_is_not_a_conflict() {
        let sections = sections(&[("any", "any.png")]);
        let resolved = resolve(&sections, &output("DP-1", "Panel")).unwrap();
        assert_eq!(resolved.precedence, Precedence::Any);
        assert!(
            resolved.conflicts.is_empty(),
            "the winner is not a conflict with itself: {:?}",
            resolved.conflicts
        );
        assert_eq!(resolved.explain(), "output config `any` matched by any");
    }

    #[test]
    fn empty_sections_resolve_to_nothing() {
        assert!(resolve(&BTreeMap::new(), &output("eDP-1", "Samsung")).is_none());
    }

    #[test]
    fn an_empty_key_does_not_match_every_description() {
        // `[outputs.""]` — a whitespace key matches a description containing
        // nothing, which would silently make it a catch-all beating `any`.
        let sections = sections(&[("  ", "oops.png"), ("any", "any.png")]);
        let resolved = resolve(&sections, &output("eDP-1", "Samsung")).unwrap();
        assert_eq!(resolved.section, "any", "{resolved:?}");
    }

    #[test]
    fn transitions_and_fps_caps_come_from_the_winning_section() {
        let mut sections = BTreeMap::new();
        sections.insert(
            "eDP-1".to_string(),
            OutputConfig {
                wallpaper: Some("/w/a.png".to_string()),
                fps_cap: Some(30),
                transition: Some(Transition {
                    name: "slide".to_string(),
                    duration_ms: 450,
                    fps: 60,
                }),
            },
        );
        let monitor = output("eDP-1", "Samsung");
        let resolved = resolve(&sections, &monitor);

        let render = RenderConfig::default();
        let transition = transition_for(resolved.as_ref(), &render);
        assert_eq!(transition.name, "slide");
        assert_eq!(transition.duration_ms, 450);
        assert_eq!(fps_cap_for(resolved.as_ref()), Some(30));
    }

    #[test]
    fn without_a_section_the_global_default_transition_applies() {
        let render = RenderConfig::default();
        let transition = transition_for(None, &render);
        assert_eq!(transition.name, render.default_transition.name);
        assert_eq!(
            transition.duration_ms,
            render.default_transition.duration_ms
        );
        assert_eq!(fps_cap_for(None), None);
    }

    #[test]
    fn precedence_orders_exact_above_description_above_any() {
        assert!(Precedence::Exact < Precedence::Description);
        assert!(Precedence::Description < Precedence::Any);
        assert_eq!(Precedence::Exact.as_str(), "exact");
    }
}
