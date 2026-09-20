//! Hotplug: the presenter's output events, turned into wallpaper work (FR-LIB-4).
//!
//! # Why the events come from the presenter and not from polling
//!
//! The cheap way to notice a monitor being plugged in is to re-run `hyprctl
//! monitors` every second. That costs a process spawn per second forever, on a
//! laptop, to detect something that happens a few times a week — the opposite of
//! this project's whole thesis. Instead the presenter already receives
//! `wl_output` add/remove events from the compositor (it has to, to draw on the
//! right surfaces), so its [`PresenterEvent`] stream *is* the hotplug source, and
//! listening on it costs zero wakeups until the compositor actually says
//! something.
//!
//! # Coalescing
//!
//! Switching monitors or resuming from suspend produces bursts: one real change
//! plus a handful of configure/close events for surfaces being rebuilt. The
//! driver drains the burst and reconciles **once** per burst, so a monitor switch
//! does not re-list outputs five times.

use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;

use owe_render::surface::PresenterEvent;

use crate::engine::Engine;

/// Whether an event can change the output list.
///
/// `Configured` and `Closed` cannot: they are about *our surface* on an output
/// that is still there. Filtering them keeps a surface rebuild from looking like a
/// hotplug.
pub fn is_output_change(event: &PresenterEvent) -> bool {
    matches!(
        event,
        PresenterEvent::OutputAdded { .. } | PresenterEvent::OutputRemoved { .. }
    )
}

/// The daemon's hotplug listener: one thread, blocked on the presenter's channel.
pub struct HotplugDriver {
    engine: Arc<Engine>,
    join: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for HotplugDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HotplugDriver").finish_non_exhaustive()
    }
}

impl HotplugDriver {
    /// Listen for output changes on the presenter's event stream.
    ///
    /// The thread blocks in `recv`, so a session where nothing is plugged or
    /// unplugged costs nothing at all (NFR-PERF-1).
    pub fn spawn(events: Receiver<PresenterEvent>, engine: Arc<Engine>) -> Self {
        let worker = Arc::clone(&engine);
        let join = std::thread::Builder::new()
            .name("owe-hotplug".to_string())
            .spawn(move || {
                // `while let` rather than `loop { match … break }`: a closed
                // channel is the only exit, and it reads as exactly that.
                while let Ok(first) = events.recv() {
                    let mut batch = vec![first];
                    // Drain whatever else arrived in the same burst. Bounded, so a
                    // pathological flood cannot starve reconciliation forever.
                    while batch.len() < MAX_BATCH {
                        match events.try_recv() {
                            Ok(event) => batch.push(event),
                            Err(_) => break,
                        }
                    }

                    for event in &batch {
                        tracing::debug!(event = ?event, "presenter event");
                    }
                    if !batch.iter().any(is_output_change) {
                        continue;
                    }

                    match worker.refresh_outputs() {
                        Ok((outputs, report)) => {
                            if !report.is_empty() {
                                tracing::info!(
                                    summary = %report.summary(),
                                    outputs = %outputs
                                        .iter()
                                        .map(|output| output.name.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                    "outputs changed"
                                );
                                for (output, reason) in &report.failures {
                                    tracing::warn!(%output, %reason, "hotplug re-apply failed");
                                }
                                for conflict in &report.conflicts {
                                    tracing::warn!("{conflict}");
                                }
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                "outputs changed but the output list could not be refreshed"
                            );
                        }
                    }
                }
                tracing::debug!("hotplug listener stopped");
            })
            .ok();

        Self { engine, join }
    }

    /// Reconcile once, synchronously.
    ///
    /// The listener thread is the normal path; this exists so a caller that must
    /// not wait for the next event (tests, and the `outputs.list` handler after a
    /// monitor appears) can drive the same code directly instead of duplicating it.
    #[allow(
        dead_code,
        reason = "the listener thread is the production path; this is its synchronous twin, used \
                  by tests and by the P3 event bus after a resume"
    )]
    pub fn reconcile_now(&self) -> Result<(), String> {
        let (_, report) = self.engine.refresh_outputs().map_err(|e| e.to_string())?;
        tracing::info!(summary = %report.summary(), "outputs reconciled");
        Ok(())
    }
}

impl Drop for HotplugDriver {
    fn drop(&mut self) {
        // Order matters: the listener only wakes when the presenter's channel
        // closes, so the presenter has to go first. Doing it here (rather than
        // relying on the engine being dropped) is what keeps shutdown from
        // deadlocking on a thread waiting for an event that will never come.
        self.engine.stop_presenter();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Most events processed in one burst before reconciling.
const MAX_BATCH: usize = 256;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn only_output_events_count_as_hotplug() {
        assert!(is_output_change(&PresenterEvent::OutputAdded {
            output: "HDMI-A-1".to_string()
        }));
        assert!(is_output_change(&PresenterEvent::OutputRemoved {
            output: "HDMI-A-1".to_string()
        }));
        assert!(
            !is_output_change(&PresenterEvent::Configured {
                output: "eDP-1".to_string(),
                size: (1920, 1080),
            }),
            "a surface configure is not a hotplug: the output is still there"
        );
        assert!(!is_output_change(&PresenterEvent::Closed {
            output: "eDP-1".to_string()
        }));
    }

    #[test]
    fn a_closed_channel_ends_a_blocking_receiver() {
        // The listener thread relies on exactly this: when the presenter goes
        // away, `recv` returns `Err` and the thread exits instead of hanging.
        let (sender, receiver) = channel::<PresenterEvent>();
        drop(sender);
        assert!(receiver.recv().is_err());
    }
}
