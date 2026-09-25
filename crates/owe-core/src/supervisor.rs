//! Output hotplug supervision and per-output fault isolation (FR-LIB-4,
//! NFR-REL-1).
//!
//! The supervisor is pure: it is fed a description of what the compositor's
//! output list looks like now, and it answers with the *actions* the daemon should
//! take. No Wayland, no threads, no clock — `now` is a parameter — which is
//! exactly why the hotplug gate can plug and unplug twenty times in CI and assert
//! on the actions instead of hoping a screenshot looks right.
//!
//! Two rules from BACKEND-DESIGN §5 are implemented here, and both are about not
//! losing the user's wallpaper:
//!
//! - **Unplug does not forget.** A monitor that goes away is torn down (its
//!   buffers are released) but its entry is kept `absent`, so replugging re-applies
//!   the same wallpaper without a daemon restart (PRD-F-08).
//! - **One bad output never stops the others.** Failures are counted per output;
//!   a worker that panics is restarted and its wallpaper re-applied, and after
//!   three restarts inside a minute the output is marked failed while the daemon —
//!   and every other output — carries on.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use crate::output::OutputInfo;

/// Restarts allowed per output inside [`DEFAULT_RESTART_WINDOW`] before the
/// output is marked failed (BACKEND-DESIGN §5).
pub const DEFAULT_MAX_RESTARTS: u32 = 3;

/// Sliding window for restart accounting.
pub const DEFAULT_RESTART_WINDOW: Duration = Duration::from_secs(60);

/// A change in the compositor's output list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotplugEvent {
    /// An output appeared (or reappeared).
    Added {
        /// Connector name.
        name: String,
    },
    #[allow(missing_docs)]
    Activated {
        #[allow(missing_docs)]
        name: String,
    },
    #[allow(missing_docs)]
    Deactivated {
        #[allow(missing_docs)]
        name: String,
    },
    /// An output disappeared.
    Removed {
        /// Connector name.
        name: String,
    },
    /// An output kept existing but changed pixel size (mode or scale change).
    Resized {
        /// Connector name.
        name: String,
        /// New pixel size.
        size: (u32, u32),
    },
}

impl HotplugEvent {
    /// The output this event is about.
    pub fn output(&self) -> &str {
        match self {
            HotplugEvent::Added { name }
            | HotplugEvent::Activated { name }
            | HotplugEvent::Deactivated { name }
            | HotplugEvent::Removed { name }
            | HotplugEvent::Resized { name, .. } => name,
        }
    }
}

/// Diff two output lists into hotplug events.
///
/// `previous` is empty on the first call, which makes the initial enumeration
/// look like "everything was just plugged in" — the right shape, because that is
/// what the daemon has to handle: every output needs a worker and a wallpaper.
pub fn diff(previous: &[OutputInfo], current: &[OutputInfo]) -> Vec<HotplugEvent> {
    let before: BTreeMap<&str, ((u32, u32), bool)> = previous
        .iter()
        .map(|output| (output.name.as_str(), (output.pixel_size(), output.active)))
        .collect();
    let after: BTreeMap<&str, ((u32, u32), bool)> = current
        .iter()
        .map(|output| (output.name.as_str(), (output.pixel_size(), output.active)))
        .collect();

    let mut events = Vec::new();
    for (name, (size, active)) in &after {
        match before.get(name) {
            None if *active => events.push(HotplugEvent::Added {
                name: (*name).to_string(),
            }),
            None => events.push(HotplugEvent::Deactivated {
                name: (*name).to_string(),
            }),
            Some((_, false)) if *active => events.push(HotplugEvent::Activated {
                name: (*name).to_string(),
            }),
            Some((_, true)) if !*active => events.push(HotplugEvent::Deactivated {
                name: (*name).to_string(),
            }),
            Some((previous_size, true)) if *active && previous_size != size => {
                events.push(HotplugEvent::Resized {
                    name: (*name).to_string(),
                    size: *size,
                });
            }
            Some(_) => {}
        }
    }
    for name in before.keys() {
        if !after.contains_key(name) {
            events.push(HotplugEvent::Removed {
                name: (*name).to_string(),
            });
        }
    }

    // Deterministic order: the tests (and the log) should not depend on BTreeMap
    // internals or on the compositor's enumeration order.
    events.sort_by(|left, right| {
        left.output()
            .cmp(right.output())
            .then_with(|| rank(left).cmp(&rank(right)))
    });
    events
}

fn rank(event: &HotplugEvent) -> u8 {
    match event {
        HotplugEvent::Added { .. } => 0,
        HotplugEvent::Activated { .. } => 1,
        HotplugEvent::Resized { .. } => 2,
        HotplugEvent::Deactivated { .. } => 3,
        HotplugEvent::Removed { .. } => 4,
    }
}

/// What the daemon knows about one output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackedState {
    /// Worker running, last apply succeeded.
    Active,
    /// Restarted at least once inside the window; still considered usable.
    Recovering {
        /// Restarts inside the window.
        restarts: u32,
    },
    /// Too many restarts: give up on this output until it is replugged.
    Failed {
        /// The last failure, for the GUI banner.
        reason: String,
    },
}

impl TrackedState {
    /// Whether the daemon should still be drawing on this output.
    pub fn is_usable(&self) -> bool {
        !matches!(self, TrackedState::Failed { .. })
    }

    /// Stable id for the GUI.
    pub fn as_str(&self) -> &'static str {
        match self {
            TrackedState::Active => "active",
            TrackedState::Recovering { .. } => "recovering",
            TrackedState::Failed { .. } => "failed",
        }
    }
}

/// One output's supervision record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracked {
    /// Whether the compositor currently lists it.
    pub present: bool,
    #[allow(missing_docs)]
    pub active: bool,
    /// Last known pixel size.
    pub size: Option<(u32, u32)>,
    /// Health.
    pub state: TrackedState,
    /// Restart timestamps inside the window.
    restarts: VecDeque<Instant>,
}

/// Something the daemon must do, produced by [`Supervisor::observe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorAction {
    /// Create (or reuse) a worker for this output and apply a wallpaper.
    Spawn {
        /// Output name.
        output: String,
        /// Why: `"appeared"`, `"reappeared"`, or `"startup"`.
        reason: String,
    },
    /// The surface size changed: re-render the same content.
    Resize {
        /// Output name.
        output: String,
        /// New pixel size.
        size: (u32, u32),
    },
    /// Release the worker's surface and buffers. The record is kept, so a replug
    /// restores the wallpaper (PRD-F-08).
    Teardown {
        /// Output name.
        output: String,
        /// Human-readable reason for the log.
        reason: String,
    },
    /// Re-apply the wallpaper after a worker failure.
    Reapply {
        /// Output name.
        output: String,
        /// The failure that triggered it.
        reason: String,
    },
    /// The output is out of restarts; report it and leave the daemon running.
    GiveUp {
        /// Output name.
        output: String,
        /// The failure that exhausted the budget.
        reason: String,
    },
}

impl SupervisorAction {
    /// The output this action is about.
    pub fn output(&self) -> &str {
        match self {
            SupervisorAction::Spawn { output, .. }
            | SupervisorAction::Resize { output, .. }
            | SupervisorAction::Teardown { output, .. }
            | SupervisorAction::Reapply { output, .. }
            | SupervisorAction::GiveUp { output, .. } => output,
        }
    }

    /// One-line description for the daemon log.
    pub fn describe(&self) -> String {
        match self {
            SupervisorAction::Spawn { output, reason } => {
                format!("spawn worker for {output} ({reason})")
            }
            SupervisorAction::Resize { output, size } => {
                format!("resize {output} to {}x{}", size.0, size.1)
            }
            SupervisorAction::Teardown { output, reason } => {
                format!("tear down {output} ({reason})")
            }
            SupervisorAction::Reapply { output, reason } => {
                format!("re-apply wallpaper on {output} after: {reason}")
            }
            SupervisorAction::GiveUp { output, reason } => {
                format!("give up on {output} after repeated failures: {reason}")
            }
        }
    }
}

/// Per-output supervision: presence, health, and restart accounting.
#[derive(Debug, Clone)]
pub struct Supervisor {
    max_restarts: u32,
    window: Duration,
    outputs: BTreeMap<String, Tracked>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_RESTARTS, DEFAULT_RESTART_WINDOW)
    }
}

impl Supervisor {
    /// A supervisor that allows `max_restarts` restarts per `window`.
    pub fn new(max_restarts: u32, window: Duration) -> Self {
        Self {
            max_restarts: max_restarts.max(1),
            window,
            outputs: BTreeMap::new(),
        }
    }

    /// Turn hotplug events into actions.
    ///
    /// The first call for each output yields `Spawn`, which is how startup is
    /// expressed: an empty previous list makes every connected output an
    /// `Added` event.
    pub fn observe(&mut self, events: &[HotplugEvent], now: Instant) -> Vec<SupervisorAction> {
        let mut actions = Vec::new();

        for event in events {
            match event {
                HotplugEvent::Added { name } => {
                    let record = self.outputs.entry(name.clone()).or_insert_with(|| Tracked {
                        present: false,
                        active: false,
                        size: None,
                        state: TrackedState::Active,
                        restarts: VecDeque::new(),
                    });
                    let reason = if record.present && record.active {
                        "reconfigured"
                    } else if record.present {
                        "reactivated"
                    } else {
                        "appeared"
                    };
                    record.present = true;
                    record.active = true;
                    actions.push(SupervisorAction::Spawn {
                        output: name.clone(),
                        reason: reason.to_string(),
                    });
                }
                HotplugEvent::Activated { name } => {
                    let record = self.outputs.entry(name.clone()).or_insert_with(|| Tracked {
                        present: false,
                        active: false,
                        size: None,
                        state: TrackedState::Active,
                        restarts: VecDeque::new(),
                    });
                    let reason = if record.present {
                        "reactivated"
                    } else {
                        "appeared"
                    };
                    record.present = true;
                    record.active = true;
                    actions.push(SupervisorAction::Spawn {
                        output: name.clone(),
                        reason: reason.to_string(),
                    });
                }
                HotplugEvent::Deactivated { name } => {
                    let record = self.outputs.entry(name.clone()).or_insert_with(|| Tracked {
                        present: true,
                        active: false,
                        size: None,
                        state: TrackedState::Active,
                        restarts: VecDeque::new(),
                    });
                    let was_active = record.active;
                    record.present = true;
                    record.active = false;
                    if was_active {
                        actions.push(SupervisorAction::Teardown {
                            output: name.clone(),
                            reason: "output became inactive".to_string(),
                        });
                    }
                }
                HotplugEvent::Removed { name } => {
                    let Some(record) = self.outputs.get_mut(name) else {
                        continue;
                    };
                    let was_active = record.active;
                    record.present = false;
                    record.active = false;
                    record.size = None;
                    if was_active {
                        actions.push(SupervisorAction::Teardown {
                            output: name.clone(),
                            reason: "output disappeared".to_string(),
                        });
                    }
                }
                HotplugEvent::Resized { name, size } => {
                    let record = self.outputs.entry(name.clone()).or_insert_with(|| Tracked {
                        present: true,
                        active: true,
                        size: None,
                        state: TrackedState::Active,
                        restarts: VecDeque::new(),
                    });
                    let changed = record.active && record.size != Some(*size);
                    record.size = Some(*size);
                    record.present = true;
                    if changed {
                        actions.push(SupervisorAction::Resize {
                            output: name.clone(),
                            size: *size,
                        });
                    }
                }
            }
        }

        // Window bookkeeping happens even without events, so a long-idle output
        // stops being "recovering" once its restarts age out.
        self.expire(now);
        actions
    }

    /// Convenience: diff two output lists and observe the result.
    pub fn observe_snapshot(
        &mut self,
        previous: &[OutputInfo],
        current: &[OutputInfo],
        now: Instant,
    ) -> Vec<SupervisorAction> {
        let events = diff(previous, current);
        let actions = self.observe(&events, now);
        for output in current {
            if let Some(record) = self.outputs.get_mut(&output.name) {
                record.present = true;
                record.active = output.active;
                record.size = Some(output.pixel_size());
            }
        }
        actions
    }

    /// Record that a worker failed, and decide what to do about it.
    pub fn record_failure(
        &mut self,
        output: &str,
        reason: &str,
        now: Instant,
    ) -> Option<SupervisorAction> {
        let record = self.outputs.get_mut(output)?;
        if !record.present || !record.active {
            return None;
        }
        record.restarts.push_back(now);
        // Age out first: a failure that lands more than a window after the
        // previous ones is not a crash loop, and must not count towards the
        // budget it would otherwise exhaust.
        self.expire(now);

        let record = self.outputs.get_mut(output)?;
        let restarts = record.restarts.len() as u32;
        let exhausted = restarts >= self.max_restarts;

        if exhausted {
            record.state = TrackedState::Failed {
                reason: reason.to_string(),
            };
            return Some(SupervisorAction::GiveUp {
                output: output.to_string(),
                reason: reason.to_string(),
            });
        }

        record.state = TrackedState::Recovering { restarts };
        Some(SupervisorAction::Reapply {
            output: output.to_string(),
            reason: reason.to_string(),
        })
    }

    /// Record that a worker is healthy again.
    pub fn record_success(&mut self, output: &str, now: Instant) {
        let _ = now;
        if let Some(record) = self.outputs.get_mut(output) {
            record.restarts.clear();
            record.state = TrackedState::Active;
        }
    }

    /// Drop restart timestamps older than the window, and let aged-out outputs
    /// stop being `Recovering`.
    fn expire(&mut self, now: Instant) {
        let window = self.window;
        for record in self.outputs.values_mut() {
            while let Some(front) = record.restarts.front() {
                // `checked_duration_since` guards against a `now` earlier than a
                // recorded restart (only possible if a caller passes an older
                // Instant, which is a test's prerogative).
                let age = now
                    .checked_duration_since(*front)
                    .unwrap_or_else(|| window + Duration::from_secs(1));
                if age >= window {
                    record.restarts.pop_front();
                } else {
                    break;
                }
            }
            if let TrackedState::Recovering { .. } = record.state
                && record.restarts.is_empty()
            {
                record.state = TrackedState::Active;
            }
        }
    }

    /// Everything currently supervised, present or not.
    pub fn tracked(&self) -> Vec<String> {
        self.outputs.keys().cloned().collect()
    }

    /// Outputs the compositor currently lists.
    pub fn present(&self) -> Vec<String> {
        self.outputs
            .iter()
            .filter(|(_, record)| record.present)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Outputs that exhausted their restart budget (the GUI banner list).
    pub fn failed(&self) -> Vec<(String, String)> {
        self.outputs
            .iter()
            .filter_map(|(name, record)| match &record.state {
                TrackedState::Failed { reason } => Some((name.clone(), reason.clone())),
                _ => None,
            })
            .collect()
    }

    /// One output's record.
    pub fn get(&self, output: &str) -> Option<&Tracked> {
        self.outputs.get(output)
    }

    /// Whether the supervisor believes it can draw on this output.
    pub fn is_usable(&self, output: &str) -> bool {
        self.outputs
            .get(output)
            .is_some_and(|record| record.present && record.active && record.state.is_usable())
    }

    /// Forget an output completely (used by `clear` on a removed output).
    pub fn forget(&mut self, output: &str) {
        self.outputs.remove(output);
    }

    /// Present output names as a set, for callers that need membership tests.
    pub fn present_set(&self) -> BTreeSet<String> {
        self.present().into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(name: &str, size: (u32, u32)) -> OutputInfo {
        OutputInfo {
            name: name.to_string(),
            description: format!("{name} panel"),
            width: size.0,
            height: size.1,
            x: 0,
            y: 0,
            focused: false,
            active: true,
        }
    }

    #[test]
    fn diff_of_nothing_to_something_is_all_added() {
        let events = diff(&[], &[output("eDP-1", (1920, 1080))]);
        assert_eq!(
            events,
            vec![HotplugEvent::Added {
                name: "eDP-1".to_string()
            }]
        );
    }

    #[test]
    fn diff_orders_events_deterministically() {
        let previous = vec![
            output("DP-1", (1920, 1080)),
            output("HDMI-A-1", (1280, 720)),
        ];
        let current = vec![output("DP-1", (2560, 1440)), output("eDP-1", (1920, 1080))];

        let events = diff(&previous, &current);
        assert_eq!(
            events,
            vec![
                HotplugEvent::Resized {
                    name: "DP-1".to_string(),
                    size: (2560, 1440)
                },
                HotplugEvent::Removed {
                    name: "HDMI-A-1".to_string()
                },
                HotplugEvent::Added {
                    name: "eDP-1".to_string()
                },
            ],
            "sorted by name (byte order: `DP-1` < `HDMI-A-1` < `eDP-1`), then added/resized/removed"
        );
    }

    #[test]
    fn the_first_snapshot_spawns_a_worker_per_output() {
        let mut supervisor = Supervisor::default();
        let now = Instant::now();
        let current = vec![
            output("eDP-1", (1920, 1080)),
            output("HDMI-A-1", (1280, 720)),
        ];

        let actions = supervisor.observe_snapshot(&[], &current, now);
        assert_eq!(actions.len(), 2, "{actions:?}");
        assert_eq!(
            actions,
            vec![
                SupervisorAction::Spawn {
                    output: "HDMI-A-1".to_string(),
                    reason: "appeared".to_string()
                },
                SupervisorAction::Spawn {
                    output: "eDP-1".to_string(),
                    reason: "appeared".to_string()
                },
            ]
        );
        assert_eq!(supervisor.present(), vec!["HDMI-A-1", "eDP-1"]);
    }

    #[test]
    fn an_idle_snapshot_produces_no_actions() {
        let mut supervisor = Supervisor::default();
        let now = Instant::now();
        let current = vec![output("eDP-1", (1920, 1080))];
        supervisor.observe_snapshot(&[], &current, now);

        let again = supervisor.observe_snapshot(&current, &current, now);
        assert!(
            again.is_empty(),
            "a hotplug poll with no change must be free: {again:?}"
        );
    }

    #[test]
    fn unplugging_tears_down_but_keeps_the_record_for_replug() {
        // PRD-F-08: unplug/replug restores without a restart. That only works if
        // the record (and the session entry) survives the unplug.
        let mut supervisor = Supervisor::default();
        let now = Instant::now();
        let plugged = vec![output("HDMI-A-1", (1920, 1080))];
        supervisor.observe_snapshot(&[], &plugged, now);

        let actions = supervisor.observe_snapshot(&plugged, &[], now);
        assert_eq!(
            actions,
            vec![SupervisorAction::Teardown {
                output: "HDMI-A-1".to_string(),
                reason: "output disappeared".to_string()
            }]
        );
        assert!(
            supervisor.tracked().contains(&"HDMI-A-1".to_string()),
            "the record must survive the unplug"
        );
        assert!(!supervisor.is_usable("HDMI-A-1"), "nothing to draw on");
        assert!(supervisor.present().is_empty());

        let actions = supervisor.observe_snapshot(&[], &plugged, now);
        assert_eq!(
            actions,
            vec![SupervisorAction::Spawn {
                output: "HDMI-A-1".to_string(),
                reason: "appeared".to_string()
            }],
            "a replug must re-apply the wallpaper"
        );
        assert!(supervisor.is_usable("HDMI-A-1"));
    }

    #[test]
    fn a_mode_change_resizes_instead_of_respawning() {
        let mut supervisor = Supervisor::default();
        let now = Instant::now();
        let before = vec![output("eDP-1", (1920, 1080))];
        let after = vec![output("eDP-1", (2560, 1440))];
        supervisor.observe_snapshot(&[], &before, now);

        let actions = supervisor.observe_snapshot(&before, &after, now);
        assert_eq!(
            actions,
            vec![SupervisorAction::Resize {
                output: "eDP-1".to_string(),
                size: (2560, 1440)
            }],
            "a resize must not destroy and recreate the surface"
        );
        assert_eq!(supervisor.get("eDP-1").unwrap().size, Some((2560, 1440)));
    }

    #[test]
    fn an_inactive_output_is_torn_down_and_reactivated_without_becoming_a_ghost() {
        let mut supervisor = Supervisor::default();
        let now = Instant::now();
        let active = output("eDP-1", (1920, 1080));
        let mut inactive = active.clone();
        inactive.active = false;
        supervisor.observe_snapshot(&[], std::slice::from_ref(&active), now);

        assert_eq!(
            diff(
                std::slice::from_ref(&active),
                std::slice::from_ref(&inactive)
            ),
            vec![HotplugEvent::Deactivated {
                name: "eDP-1".to_string()
            }]
        );
        let actions = supervisor.observe_snapshot(
            std::slice::from_ref(&active),
            std::slice::from_ref(&inactive),
            now,
        );
        assert_eq!(
            actions,
            vec![SupervisorAction::Teardown {
                output: "eDP-1".to_string(),
                reason: "output became inactive".to_string()
            }]
        );
        assert!(supervisor.present().contains(&"eDP-1".to_string()));
        assert!(!supervisor.is_usable("eDP-1"));

        let actions =
            supervisor.observe_snapshot(&[inactive], &[output("eDP-1", (1920, 1080))], now);
        assert_eq!(
            actions,
            vec![SupervisorAction::Spawn {
                output: "eDP-1".to_string(),
                reason: "reactivated".to_string()
            }]
        );
        assert!(supervisor.is_usable("eDP-1"));
    }

    #[test]
    fn an_initially_inactive_output_is_tracked_without_spawning() {
        let mut supervisor = Supervisor::default();
        let mut inactive = output("DP-1", (1920, 1080));
        inactive.active = false;
        let actions = supervisor.observe_snapshot(&[], &[inactive], Instant::now());
        assert!(actions.is_empty(), "{actions:?}");
        assert!(supervisor.present().contains(&"DP-1".to_string()));
        assert!(!supervisor.is_usable("DP-1"));
    }

    #[test]
    fn a_worker_failure_is_reapplied_then_given_up_on_after_three_in_a_minute() {
        let mut supervisor = Supervisor::default();
        let now = Instant::now();
        let current = vec![output("eDP-1", (1920, 1080))];
        supervisor.observe_snapshot(&[], &current, now);

        let first = supervisor.record_failure("eDP-1", "panicked", now).unwrap();
        assert!(
            matches!(first, SupervisorAction::Reapply { .. }),
            "{first:?}"
        );
        assert_eq!(
            supervisor.get("eDP-1").unwrap().state.as_str(),
            "recovering"
        );

        let second = supervisor.record_failure("eDP-1", "panicked", now).unwrap();
        assert!(
            matches!(second, SupervisorAction::Reapply { .. }),
            "{second:?}"
        );

        let third = supervisor.record_failure("eDP-1", "panicked", now).unwrap();
        assert!(
            matches!(third, SupervisorAction::GiveUp { .. }),
            "{third:?}"
        );
        assert_eq!(supervisor.get("eDP-1").unwrap().state.as_str(), "failed");
        assert_eq!(
            supervisor.failed(),
            vec![("eDP-1".to_string(), "panicked".to_string())]
        );
        assert!(!supervisor.is_usable("eDP-1"));

        // The fourth failure does not resurrect anything: once failed, the output
        // stays failed until a replug (or a patch).
        let fourth = supervisor.record_failure("eDP-1", "panicked", now).unwrap();
        assert!(
            matches!(fourth, SupervisorAction::GiveUp { .. }),
            "{fourth:?}"
        );
    }

    #[test]
    fn restarts_age_out_of_the_window() {
        let mut supervisor = Supervisor::new(3, Duration::from_secs(60));
        let start = Instant::now();
        let current = vec![output("eDP-1", (1920, 1080))];
        supervisor.observe_snapshot(&[], &current, start);

        supervisor.record_failure("eDP-1", "boom", start);
        supervisor.record_failure("eDP-1", "boom", start + Duration::from_secs(30));

        // The 40-second mark: the first failure is still inside the window.
        let mid = supervisor
            .record_failure("eDP-1", "boom", start + Duration::from_secs(40))
            .unwrap();
        assert!(matches!(mid, SupervisorAction::GiveUp { .. }), "{mid:?}");

        // A fresh supervisor with the same schedule but a gap between failures
        // must never give up: the older failures have aged out.
        let mut patient = Supervisor::new(3, Duration::from_secs(60));
        patient.observe_snapshot(&[], &current, start);
        patient.record_failure("eDP-1", "boom", start);
        patient.record_failure("eDP-1", "boom", start + Duration::from_secs(61));
        let late = patient.record_failure("eDP-1", "boom", start + Duration::from_secs(122));
        assert!(
            matches!(late, Some(SupervisorAction::Reapply { .. })),
            "failures more than a window apart are not a crash loop: {late:?}"
        );
    }

    #[test]
    fn a_successful_apply_clears_the_restart_history() {
        let mut supervisor = Supervisor::default();
        let now = Instant::now();
        let current = vec![output("eDP-1", (1920, 1080))];
        supervisor.observe_snapshot(&[], &current, now);

        supervisor.record_failure("eDP-1", "boom", now);
        supervisor.record_failure("eDP-1", "boom", now);
        supervisor.record_success("eDP-1", now);

        assert_eq!(supervisor.get("eDP-1").unwrap().state, TrackedState::Active);
        // Two more failures must not immediately give up.
        let first = supervisor.record_failure("eDP-1", "boom", now).unwrap();
        assert!(
            matches!(first, SupervisorAction::Reapply { .. }),
            "{first:?}"
        );
    }

    #[test]
    fn a_recovering_output_stops_being_recovering_once_the_window_passes() {
        let mut supervisor = Supervisor::new(3, Duration::from_secs(5));
        let start = Instant::now();
        let current = vec![output("eDP-1", (1920, 1080))];
        supervisor.observe_snapshot(&[], &current, start);
        supervisor.record_failure("eDP-1", "boom", start);

        // Any later poll is enough: the window bookkeeping rides along with it.
        supervisor.observe(&[], start + Duration::from_secs(6));
        assert_eq!(supervisor.get("eDP-1").unwrap().state, TrackedState::Active);
    }

    #[test]
    fn failures_on_one_output_never_touch_another() {
        // NFR-REL-1: fault isolation is the whole point of per-output workers.
        let mut supervisor = Supervisor::default();
        let now = Instant::now();
        let current = vec![output("eDP-1", (1920, 1080)), output("DP-1", (2560, 1440))];
        supervisor.observe_snapshot(&[], &current, now);

        supervisor.record_failure("eDP-1", "boom", now);
        supervisor.record_failure("eDP-1", "boom", now);
        supervisor.record_failure("eDP-1", "boom", now);

        assert_eq!(supervisor.failed().len(), 1);
        assert!(
            supervisor.is_usable("DP-1"),
            "the other output is untouched"
        );
        assert_eq!(supervisor.get("DP-1").unwrap().state, TrackedState::Active);
    }

    #[test]
    fn a_replugged_failed_output_gets_another_chance_but_keeps_its_history() {
        let mut supervisor = Supervisor::default();
        let now = Instant::now();
        let plugged = vec![output("HDMI-A-1", (1920, 1080))];
        supervisor.observe_snapshot(&[], &plugged, now);

        for _ in 0..3 {
            supervisor.record_failure("HDMI-A-1", "boom", now);
        }
        assert!(!supervisor.is_usable("HDMI-A-1"));

        supervisor.observe_snapshot(&plugged, &[], now);
        let actions = supervisor.observe_snapshot(&[], &plugged, now);
        assert_eq!(actions.len(), 1, "a replug is a new attempt: {actions:?}");
        assert!(
            !supervisor.is_usable("HDMI-A-1"),
            "and its history is not wiped: a monitor in a crash loop stays flagged"
        );
    }

    #[test]
    fn twenty_plug_unplug_cycles_leave_no_ghost_outputs() {
        // The P2 gate, as a pure-logic test: the CI version repeats this against a
        // real compositor and checks RSS.
        let mut supervisor = Supervisor::new(3, Duration::from_secs(60));
        let now = Instant::now();

        for cycle in 0..20 {
            let current = vec![output("HDMI-A-1", (1920, 1080))];
            let spawned = supervisor.observe_snapshot(&[], &current, now);
            assert_eq!(spawned.len(), 1, "cycle {cycle}: {spawned:?}");

            let torn = supervisor.observe_snapshot(&current, &[], now);
            assert_eq!(torn.len(), 1, "cycle {cycle}: {torn:?}");
            assert_eq!(
                supervisor.tracked().len(),
                1,
                "cycle {cycle}: records must not accumulate"
            );
            assert!(
                supervisor.present().is_empty(),
                "cycle {cycle}: nothing should be present"
            );
        }
    }

    #[test]
    fn forget_removes_the_record_entirely() {
        let mut supervisor = Supervisor::default();
        let now = Instant::now();
        supervisor.observe_snapshot(&[], &[output("DP-1", (1920, 1080))], now);
        assert_eq!(supervisor.tracked().len(), 1);

        supervisor.forget("DP-1");
        assert!(supervisor.tracked().is_empty());
        assert!(supervisor.get("DP-1").is_none());
    }

    #[test]
    fn present_set_matches_present() {
        let mut supervisor = Supervisor::default();
        let now = Instant::now();
        supervisor.observe_snapshot(
            &[],
            &[output("eDP-1", (1920, 1080)), output("DP-1", (1920, 1080))],
            now,
        );
        assert_eq!(supervisor.present_set().len(), 2);
        assert!(supervisor.present_set().contains("eDP-1"));
    }
}
