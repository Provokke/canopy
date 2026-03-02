//! Error type for the ECS.
use thiserror::Error;

/// Errors produced by world, query and schedule operations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EcsError {
    /// The entity does not exist (despawned or never spawned).
    #[error("entity {0:?} does not exist")]
    NoSuchEntity(crate::Entity),
    /// A query requested mutable access to the same component twice, or read+write.
    #[error("query has conflicting access to component `{0}`")]
    ConflictingQueryAccess(&'static str),
    /// A system tried to access data it did not declare.
    #[error("system `{system}` accessed `{component}` without declaring it")]
    UndeclaredAccess {
        /// System name.
        system: String,
        /// Component type name.
        component: &'static str,
    },
    /// A system panicked during execution.
    #[error("system `{0}` panicked")]
    SystemPanicked(String),
    /// A `Res`/`ResMut` parameter's resource is not in the world.
    #[error("resource `{0}` does not exist")]
    MissingResource(&'static str),
    /// Parenting would make an entity its own ancestor.
    #[error("making {child:?} a child of {parent:?} would create a cycle")]
    HierarchyCycle {
        /// The entity being reparented.
        child: crate::Entity,
        /// The requested parent.
        parent: crate::Entity,
    },
    /// Explicit ordering constraints form a cycle.
    #[error("schedule ordering contains a cycle")]
    OrderingCycle,
}
