//! `canopy-app`: an [`App`] builder with [`Plugin`]s and [`PluginGroup`]s, a [`Time`]
//! resource ([`TimePlugin`]), transform propagation ([`TransformPlugin`]), minimal
//! fixed-step box physics ([`PhysicsPlugin`]) and a headless, deterministic particle demo
//! ([`ParticlePlugin`]).
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]
#![deny(unsafe_code)]
#![warn(missing_docs)]

mod app;
mod particles;
mod physics;
mod plugin;
mod time;
mod transform;

pub use app::{App, DT};
pub use particles::{state_hash, ParticlePlugin, Position, Velocity};
pub use physics::{physics_step, Aabb, Collision, LinearVelocity, PhysicsPlugin, Static};
pub use plugin::{CorePlugins, Plugin, PluginGroup, PluginGroupBuilder};
pub use time::{Time, TimePlugin};
pub use transform::TransformPlugin;

