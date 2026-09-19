//! Per-output worker state machine — the pure logic that decides when a worker
//! renders, presents, or sleeps.
//!
//! Why this is a separate, testable thing: the expensive properties of a
//! wallpaper engine (idle cost, "no frames when nothing changes", surviving a
//! resize) are *timing* properties, and timing logic buried in a Wayland callback
//! cannot be tested. So all of it lives here, and the Wayland layer only obeys
//! the [`WorkerAction`]s this returns.
//!
//! ```text
//!            Apply(g,s)                 Prepared(g)
//!   Idle ───────────────▶ Preparing(g,s) ─────────▶ (present) ──▶ Presented(g,s)
//!     ▲                                                                 │
//!     │ Clear                                                  Configure(s')
//!     │                                                                 ▼
//!   Failed(reason) ◀── Failed(g,reason)                      Reconfigured(g,s')
//! ```
//!
//! Two rules keep this honest:
//!
//! 1. **Stale generations are ignored.** If two `wallpaper.set` calls race, the
//!    newer generation wins and the older one's late `Prepared`/`Presented`
//!    events do nothing. Without this, a slow decode can overwrite a newer
//!    wallpaper — a real bug class in wallpaper daemons.
//! 2. **`Presented` means sleep.** Nothing is scheduled until an event arrives,
//!    which is what makes the P1 "zero frames while idle" gate reachable.

/// What a worker is currently doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerState {
    /// Nothing to draw; the worker is asleep holding no pixel buffers.
    Idle,
    /// Decoding/rendering generation `generation` for `size`.
    Preparing {
        /// Monotonic request id.
        generation: u64,
        /// Target pixel size.
        size: (u32, u32),
    },
    /// Presented and asleep: the compositor holds the buffer, we do nothing.
    Presented {
        /// Generation currently on screen.
        generation: u64,
        /// Size currently on screen.
        size: (u32, u32),
    },
    /// The output changed size; the same generation must be re-rendered.
    Reconfigured {
        /// Generation being re-rendered (unchanged by a resize).
        generation: u64,
        /// New target size.
        size: (u32, u32),
    },
    /// The last attempt failed; the worker is idle but remembers why.
    Failed {
        /// Human-readable cause.
        reason: String,
    },
}

impl WorkerState {
    /// Whether the worker is holding pixels (and therefore memory).
    pub fn holds_pixels(&self) -> bool {
        matches!(
            self,
            WorkerState::Preparing { .. }
                | WorkerState::Presented { .. }
                | WorkerState::Reconfigured { .. }
        )
    }

    /// Whether the worker schedules no further frames.
    ///
    /// This is the NFR-PERF-1 property, and it is true in three states for three
    /// different reasons: `Idle` (nothing asked for), `Failed` (gave up), and
    /// `Presented` — the "Presented(Sleep)" state, where the compositor holds the
    /// pixels and there is genuinely nothing left to do until an event arrives.
    pub fn is_sleeping(&self) -> bool {
        !matches!(
            self,
            WorkerState::Preparing { .. } | WorkerState::Reconfigured { .. }
        )
    }
}

/// Something that happened to an output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerEvent {
    /// A new wallpaper was requested.
    Apply {
        /// Request generation (strictly increasing).
        generation: u64,
        /// Size to render at.
        size: (u32, u32),
    },
    /// Pixels for `generation` are ready to upload.
    Prepared {
        /// Generation that finished rendering.
        generation: u64,
    },
    /// The compositor acknowledged the presented buffer.
    Presented {
        /// Generation presented.
        generation: u64,
        /// Size presented.
        size: (u32, u32),
    },
    /// The compositor reconfigured the surface to a new size.
    Configure {
        /// New size.
        size: (u32, u32),
    },
    /// The surface was closed/destroyed by the compositor.
    SurfaceClosed,
    /// An error occurred while handling `generation`.
    Failed {
        /// Generation that failed.
        generation: u64,
        /// Human-readable cause.
        reason: String,
    },
}

/// What the Wayland layer should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerAction {
    /// Decode/scale, then report `Prepared`.
    Render {
        /// Generation to render.
        generation: u64,
        /// Target size.
        size: (u32, u32),
    },
    /// Upload the prepared pixels and commit, then report `Presented`.
    Present {
        /// Generation to present.
        generation: u64,
        /// Size to present.
        size: (u32, u32),
    },
    /// Drop cached pixels (clearing the wallpaper frees memory immediately).
    DropPixels,
    /// Nothing to do — this is the idle case that must stay free.
    Idle,
}

/// The state machine for one output.
#[derive(Debug, Clone)]
pub struct OutputWorker {
    output: String,
    state: WorkerState,
    generation: u64,
}

impl OutputWorker {
    /// A worker for `output`, asleep.
    pub fn new(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            state: WorkerState::Idle,
            generation: 0,
        }
    }

    /// The output this worker serves.
    pub fn output(&self) -> &str {
        &self.output
    }

    /// Current state.
    pub fn state(&self) -> &WorkerState {
        &self.state
    }

    /// Highest generation applied so far.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the worker schedules no further frames (see [`WorkerState::is_sleeping`]).
    pub fn is_sleeping(&self) -> bool {
        self.state.is_sleeping()
    }

    /// Apply an event and return what should happen next.
    pub fn on(&mut self, event: WorkerEvent) -> WorkerAction {
        match event {
            WorkerEvent::Apply { generation, size } => self.apply(generation, size),

            WorkerEvent::Prepared { generation } => {
                // Only the generation we are waiting for may present.
                match &self.state {
                    WorkerState::Preparing {
                        generation: current,
                        size,
                    }
                    | WorkerState::Reconfigured {
                        generation: current,
                        size,
                    } if *current == generation => WorkerAction::Present {
                        generation,
                        size: *size,
                    },
                    _ => WorkerAction::Idle,
                }
            }

            WorkerEvent::Presented { generation, size } => {
                // Only a generation we are actively working on may claim the
                // screen: an acknowledgement for cancelled work must not leave
                // the state machine thinking a stale frame is current.
                let relevant = match &self.state {
                    WorkerState::Preparing {
                        generation: current,
                        ..
                    }
                    | WorkerState::Reconfigured {
                        generation: current,
                        ..
                    } => *current == generation,
                    _ => false,
                };
                if relevant {
                    self.state = WorkerState::Presented { generation, size };
                }
                WorkerAction::Idle
            }

            WorkerEvent::Configure { size } => self.configure(size),

            WorkerEvent::SurfaceClosed => {
                self.state = WorkerState::Idle;
                WorkerAction::DropPixels
            }

            WorkerEvent::Failed { generation, reason } => {
                // A failure in an already-superseded generation is noise.
                if generation < self.generation {
                    return WorkerAction::Idle;
                }
                self.state = WorkerState::Failed { reason };
                WorkerAction::DropPixels
            }
        }
    }

    fn apply(&mut self, generation: u64, size: (u32, u32)) -> WorkerAction {
        // Duplicates and out-of-order requests are ignored rather than restarted:
        // re-rendering a wallpaper the user already has would burn CPU for nothing.
        if generation <= self.generation {
            return WorkerAction::Idle;
        }
        self.generation = generation;
        self.state = WorkerState::Preparing { generation, size };
        WorkerAction::Render { generation, size }
    }

    fn configure(&mut self, size: (u32, u32)) -> WorkerAction {
        match &self.state {
            WorkerState::Presented {
                generation,
                size: current,
            } if *current != size => {
                let generation = *generation;
                self.state = WorkerState::Reconfigured { generation, size };
                WorkerAction::Render { generation, size }
            }
            WorkerState::Preparing { generation, .. } => {
                let generation = *generation;
                self.state = WorkerState::Preparing { generation, size };
                WorkerAction::Render { generation, size }
            }
            WorkerState::Reconfigured { generation, .. } => {
                let generation = *generation;
                self.state = WorkerState::Reconfigured { generation, size };
                WorkerAction::Render { generation, size }
            }
            // Idle/Failed with nothing on screen: remember nothing, do nothing.
            // Rendering a size change with no wallpaper would paint a blank
            // surface over the user's desktop.
            WorkerState::Presented { .. } => WorkerAction::Idle,
            WorkerState::Idle | WorkerState::Failed { .. } => {
                self.state = WorkerState::Idle;
                WorkerAction::Idle
            }
        }
    }

    /// Clear the wallpaper: drop pixels and sleep.
    pub fn clear(&mut self) -> WorkerAction {
        self.state = WorkerState::Idle;
        WorkerAction::DropPixels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: (u32, u32) = (1920, 1080);

    fn presented() -> OutputWorker {
        let mut worker = OutputWorker::new("eDP-1");
        assert_eq!(
            worker.on(WorkerEvent::Apply {
                generation: 1,
                size: SIZE
            }),
            WorkerAction::Render {
                generation: 1,
                size: SIZE
            }
        );
        worker.on(WorkerEvent::Prepared { generation: 1 });
        worker.on(WorkerEvent::Presented {
            generation: 1,
            size: SIZE,
        });
        assert_eq!(
            worker.state(),
            &WorkerState::Presented {
                generation: 1,
                size: SIZE
            }
        );
        worker
    }

    #[test]
    fn new_worker_is_asleep() {
        let worker = OutputWorker::new("eDP-1");
        assert_eq!(worker.state(), &WorkerState::Idle);
        assert!(worker.is_sleeping());
        assert!(!worker.state().holds_pixels());
    }

    #[test]
    fn apply_then_prepared_then_presented_is_the_happy_path() {
        let mut worker = OutputWorker::new("eDP-1");

        assert_eq!(
            worker.on(WorkerEvent::Apply {
                generation: 1,
                size: SIZE
            }),
            WorkerAction::Render {
                generation: 1,
                size: SIZE
            }
        );
        assert!(matches!(
            worker.state(),
            WorkerState::Preparing { generation: 1, .. }
        ));

        assert_eq!(
            worker.on(WorkerEvent::Prepared { generation: 1 }),
            WorkerAction::Present {
                generation: 1,
                size: SIZE
            }
        );

        assert_eq!(
            worker.on(WorkerEvent::Presented {
                generation: 1,
                size: SIZE
            }),
            WorkerAction::Idle
        );
        assert!(
            worker.is_sleeping(),
            "a presented worker must schedule no further frames (NFR-PERF-1)"
        );
    }

    #[test]
    fn a_late_prepared_from_an_old_generation_never_presents() {
        let mut worker = OutputWorker::new("eDP-1");
        worker.on(WorkerEvent::Apply {
            generation: 1,
            size: SIZE,
        });
        worker.on(WorkerEvent::Apply {
            generation: 2,
            size: SIZE,
        });

        // Generation 1 finished decoding late: it must not reach the screen.
        assert_eq!(
            worker.on(WorkerEvent::Prepared { generation: 1 }),
            WorkerAction::Idle
        );
        assert_eq!(
            worker.on(WorkerEvent::Prepared { generation: 2 }),
            WorkerAction::Present {
                generation: 2,
                size: SIZE
            }
        );
    }

    #[test]
    fn a_stale_apply_does_not_restart_work() {
        let mut worker = presented();
        // A duplicate/old request (same generation) is ignored, not re-rendered.
        assert_eq!(
            worker.on(WorkerEvent::Apply {
                generation: 1,
                size: SIZE
            }),
            WorkerAction::Idle
        );
        assert!(matches!(
            worker.state(),
            WorkerState::Presented { generation: 1, .. }
        ));
    }

    #[test]
    fn a_newer_apply_while_presented_re_renders() {
        let mut worker = presented();
        assert_eq!(
            worker.on(WorkerEvent::Apply {
                generation: 2,
                size: SIZE
            }),
            WorkerAction::Render {
                generation: 2,
                size: SIZE
            }
        );
    }

    #[test]
    fn configure_at_a_new_size_re_renders_the_same_generation() {
        let mut worker = presented();
        let new_size = (2560, 1440);

        assert_eq!(
            worker.on(WorkerEvent::Configure { size: new_size }),
            WorkerAction::Render {
                generation: 1,
                size: new_size
            }
        );
        assert_eq!(
            worker.state(),
            &WorkerState::Reconfigured {
                generation: 1,
                size: new_size
            }
        );

        assert_eq!(
            worker.on(WorkerEvent::Prepared { generation: 1 }),
            WorkerAction::Present {
                generation: 1,
                size: new_size
            }
        );
        assert_eq!(
            worker.on(WorkerEvent::Presented {
                generation: 1,
                size: new_size
            }),
            WorkerAction::Idle
        );
    }

    #[test]
    fn configure_at_the_same_size_is_a_no_op() {
        let mut worker = presented();
        assert_eq!(
            worker.on(WorkerEvent::Configure { size: SIZE }),
            WorkerAction::Idle
        );
        assert_eq!(
            worker.state(),
            &WorkerState::Presented {
                generation: 1,
                size: SIZE
            }
        );
    }

    #[test]
    fn configure_while_idle_paints_nothing() {
        let mut worker = OutputWorker::new("eDP-1");
        assert_eq!(
            worker.on(WorkerEvent::Configure { size: (800, 600) }),
            WorkerAction::Idle,
            "a resize with no wallpaper must not paint a blank surface"
        );
        assert_eq!(worker.state(), &WorkerState::Idle);
    }

    #[test]
    fn configure_during_preparing_updates_the_target_size() {
        let mut worker = OutputWorker::new("eDP-1");
        worker.on(WorkerEvent::Apply {
            generation: 1,
            size: SIZE,
        });

        let new_size = (1280, 720);
        assert_eq!(
            worker.on(WorkerEvent::Configure { size: new_size }),
            WorkerAction::Render {
                generation: 1,
                size: new_size
            }
        );
        assert_eq!(
            worker.on(WorkerEvent::Prepared { generation: 1 }),
            WorkerAction::Present {
                generation: 1,
                size: new_size
            }
        );
    }

    #[test]
    fn failure_drops_pixels_and_sleeps_but_keeps_the_reason() {
        let mut worker = presented();
        assert_eq!(
            worker.on(WorkerEvent::Failed {
                generation: 2,
                reason: "decode error".into()
            }),
            WorkerAction::DropPixels
        );
        match worker.state() {
            WorkerState::Failed { reason } => assert_eq!(reason, "decode error"),
            other => panic!("unexpected: {other:?}"),
        }
        assert!(worker.is_sleeping());
        assert!(!worker.state().holds_pixels());
    }

    #[test]
    fn failure_of_a_superseded_generation_is_ignored() {
        let mut worker = OutputWorker::new("eDP-1");
        worker.on(WorkerEvent::Apply {
            generation: 2,
            size: SIZE,
        });
        assert_eq!(
            worker.on(WorkerEvent::Failed {
                generation: 1,
                reason: "old".into()
            }),
            WorkerAction::Idle
        );
        assert!(matches!(
            worker.state(),
            WorkerState::Preparing { generation: 2, .. }
        ));
    }

    #[test]
    fn a_failed_worker_recovers_on_the_next_apply() {
        let mut worker = presented();
        worker.on(WorkerEvent::Failed {
            generation: 2,
            reason: "boom".into(),
        });
        assert_eq!(
            worker.on(WorkerEvent::Apply {
                generation: 3,
                size: SIZE
            }),
            WorkerAction::Render {
                generation: 3,
                size: SIZE
            }
        );
    }

    #[test]
    fn surface_closed_returns_to_idle_and_frees_pixels() {
        let mut worker = presented();
        assert_eq!(
            worker.on(WorkerEvent::SurfaceClosed),
            WorkerAction::DropPixels
        );
        assert_eq!(worker.state(), &WorkerState::Idle);
    }

    #[test]
    fn clear_frees_pixels_and_sleeps() {
        let mut worker = presented();
        assert_eq!(worker.clear(), WorkerAction::DropPixels);
        assert_eq!(worker.state(), &WorkerState::Idle);
        assert!(worker.is_sleeping());
    }

    #[test]
    fn generations_only_ever_move_forward() {
        let mut worker = presented();
        assert_eq!(worker.generation(), 1);
        worker.on(WorkerEvent::Apply {
            generation: 7,
            size: SIZE,
        });
        assert_eq!(worker.generation(), 7);
        // An out-of-order older generation must not roll the counter back.
        worker.on(WorkerEvent::Apply {
            generation: 3,
            size: SIZE,
        });
        assert_eq!(worker.generation(), 7);
    }
}
