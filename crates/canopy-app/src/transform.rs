//! [`TransformPlugin`]: keeps `GlobalTransform` in sync with the hierarchy.
use crate::{App, Plugin};
use canopy_ecs::hierarchy::propagate_transforms;
use canopy_ecs::schedule::SystemBuilder;
use canopy_ecs::EcsError;

/// Runs [`propagate_transforms`] at the end of every frame (`PostUpdate`), after all
/// other commands of that schedule have been applied.
#[derive(Default)]
pub struct TransformPlugin;

impl Plugin for TransformPlugin {
    fn build(&self, app: &mut App) -> Result<(), EcsError> {
        app.post_update
            .add_system(SystemBuilder::new("propagate_transforms"), |ctx| {
                ctx.command(propagate_transforms);
            })?;
        Ok(())
    }
}
