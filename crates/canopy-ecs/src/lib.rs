//! `canopy-ecs`: a small archetype-based Entity Component System.
//!
//! Components are stored type-erased in 16-byte-aligned columns ([`blob_vec::BlobVec`])
//! grouped into archetypes. Archetype transitions are cached as graph edges.
//! See `ARCHITECTURE.md` at the repository root for the safety invariants.
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]
#![deny(unsafe_op_in_unsafe_fn, clippy::undocumented_unsafe_blocks)]
#![warn(missing_docs)]

pub mod blob_vec;
pub mod entity;
pub mod error;
pub mod event;
pub mod hierarchy;
pub mod query;
pub mod schedule;
pub mod set;
pub mod system;
pub mod world;

pub use entity::Entity;
pub use error::EcsError;
pub use event::Events;
pub use hierarchy::{Children, GlobalTransform, Parent, Transform};
pub use query::{
    Added, ArchetypeCache, Changed, Mut, Or, QueryData, QueryFilter, QueryState, With, Without,
};
pub use schedule::{Schedule, SetConfig, SystemContext};
pub use set::SystemSet;
pub use system::{Commands, EventReader, EventWriter, Local, Query, Res, ResMut};
pub use world::{Component, ComponentSparseSet, StorageType, Tick, World};

