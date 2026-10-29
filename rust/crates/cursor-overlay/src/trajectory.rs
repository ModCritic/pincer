//! Planned agent-cursor trajectories.
//!
//! The motion math lives in the `pincer-cursor-motion` crate, the single source of
//! truth shared with the `pincer-cursor-motion` web package. This module adapts the
//! driver's [`MotionConfig`] to it.

use crate::motion::MotionConfig;
pub use pincer_cursor_motion::ease;
pub use pincer_cursor_motion::path::{bow_path, natural_side, pincer_path, ArcShape, Path};
pub use pincer_cursor_motion::plan::*;
pub use pincer_cursor_motion::rng::{hash_string, Rng};
pub use pincer_cursor_motion::{wrap_angle, Pt};

/// Plan one move for `motion.style`.
pub fn plan_move(motion: &MotionConfig, req: &MoveRequest) -> Trajectory {
    pincer_cursor_motion::plan_move(&motion.params(), req)
}
