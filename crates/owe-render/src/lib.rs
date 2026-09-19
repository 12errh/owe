//! OWE rendering: headless wgpu device management and golden-image tooling.
//!
//! P0 ships the *verification harness* only: a way to render off-screen and
//! compare pixels against a committed reference. The layer-shell surfaces,
//! transition engine, and shader runtime land in P1/P2/P5 on top of it.
//!
//! # Golden images: the freeze process (IMPLEMENTATION-PLAN §0)
//!
//! Graphics code cannot be meaningfully written test-first, so the honest
//! pattern is:
//!
//! 1. implement the renderer (`[I]` task),
//! 2. render a frame and **record** it with [`golden::GoldenMode::RecordIfMissing`],
//! 3. a human reviews the produced PNG once — this is the only manual step, and
//!    it exists precisely so tests never get frozen against their own bugs,
//! 4. commit it as the reference,
//! 5. from then on the same test runs in [`golden::GoldenMode::Strict`] and is a
//!    normal failing-first test ([`GoldenImage::diff`]).
//!
//! CI runs strictly: a missing reference is a failure, never a silent record.

pub mod golden;
pub mod gpu;

pub use golden::{GoldenImage, GoldenMode, GoldenOutcome, ImageDiff, verify_or_record};
pub use gpu::{HeadlessGpu, RenderError};
