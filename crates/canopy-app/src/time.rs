//! The [`Time`] resource.
use crate::{App, Plugin};
use canopy_ecs::EcsError;

/// Frame timing, advanced by [`App::advance`] before the `First` schedule runs.
///
/// Time is driven by the `dt` passed to `advance` (never the wall clock), so runs
/// are reproducible.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Time {
    /// Seconds in the current frame.
    pub delta: f32,
    /// Total seconds advanced (accumulated in `f64`).
    pub elapsed: f64,
    /// Frames advanced so far; the first frame sees 1.
    pub frame: u64,
    /// The app's fixed step, for `FixedUpdate` systems.
    pub fixed_step: f32,
}

impl Time {
    pub(crate) fn advance(&mut self, dt: f32) {
        self.delta = dt;
        self.elapsed += f64::from(dt);
        self.frame += 1;
    }
}

/// Inserts a [`Time`] resource that the app keeps up to date.
#[derive(Default)]
pub struct TimePlugin;

impl Plugin for TimePlugin {
    fn build(&self, app: &mut App) -> Result<(), EcsError> {
        app.world.insert_resource(Time {
            fixed_step: app.fixed_step,
            ..Time::default()
        });
        Ok(())
    }
}
