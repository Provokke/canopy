//! Parent/child relationships, recursive despawn and transform propagation.
//!
//! The hierarchy is stored as components: a child has [`Parent`], and its parent has
//! a [`Children`] list (in the order the children were attached). [`World::set_parent`]
//! keeps both sides consistent and rejects cycles. [`World::despawn_recursive`]
//! removes a whole subtree; plain [`World::despawn`] removes the entity from its
//! parent's `Children` and turns its children into roots (removes their `Parent`).
//! Ids can still go stale if `Parent`/`Children` are edited by hand; propagation skips
//! dead ids.
use crate::entity::Entity;
use crate::error::EcsError;
use crate::query::Without;
use crate::world::{Component, World};

/// The entity this entity is attached to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parent(pub Entity);
impl Component for Parent {}

/// The entities attached to this entity, in attachment order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Children(pub Vec<Entity>);
impl Component for Children {}

/// A similarity transform: translation, rotation about Z (radians) and uniform scale.
/// Closed under composition, which keeps propagation exact for any hierarchy.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    /// Translation (x, y, z).
    pub translation: [f32; 3],
    /// Rotation about the Z axis, in radians.
    pub rotation: f32,
    /// Uniform scale.
    pub scale: f32,
}
impl Component for Transform {}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Transform {
    /// No translation, no rotation, unit scale.
    pub const IDENTITY: Transform = Transform {
        translation: [0.0; 3],
        rotation: 0.0,
        scale: 1.0,
    };

    /// A pure translation.
    pub const fn from_translation(translation: [f32; 3]) -> Self {
        Transform {
            translation,
            ..Self::IDENTITY
        }
    }

    /// `self` applied after `local`: the world transform of a child with local
    /// transform `local` whose parent's world transform is `self`.
    pub fn compose(&self, local: &Transform) -> Transform {
        let (s, c) = self.rotation.sin_cos();
        let (x, y) = (
            local.translation[0] * self.scale,
            local.translation[1] * self.scale,
        );
        Transform {
            translation: [
                self.translation[0] + c * x - s * y,
                self.translation[1] + s * x + c * y,
                self.translation[2] + self.scale * local.translation[2],
            ],
            rotation: self.rotation + local.rotation,
            scale: self.scale * local.scale,
        }
    }
}

/// The world-space transform computed by [`propagate_transforms`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GlobalTransform(pub Transform);
impl Component for GlobalTransform {}

impl World {
    /// The parent of `e`, if it has one.
    pub fn parent(&self, e: Entity) -> Option<Entity> {
        self.get::<Parent>(e).map(|p| p.0)
    }

    /// The children of `e` (empty if none).
    pub fn children(&self, e: Entity) -> &[Entity] {
        self.get::<Children>(e).map_or(&[], |c| c.0.as_slice())
    }

    /// Makes `child` a child of `parent`, detaching it from any previous parent.
    ///
    /// Fails with [`EcsError::NoSuchEntity`] if either is dead, and with
    /// [`EcsError::HierarchyCycle`] if `parent` is `child` or one of its descendants.
    pub fn set_parent(&mut self, child: Entity, parent: Entity) -> Result<(), EcsError> {
        self.flush_entities();
        if !self.contains(child) {
            return Err(EcsError::NoSuchEntity(child));
        }
        if !self.contains(parent) {
            return Err(EcsError::NoSuchEntity(parent));
        }
        let mut up = Some(parent);
        while let Some(a) = up {
            if a == child {
                return Err(EcsError::HierarchyCycle { child, parent });
            }
            up = self.parent(a);
        }
        if self.parent(child) == Some(parent) {
            return Ok(());
        }
        self.remove_parent(child)?;
        self.insert(child, Parent(parent))?;
        match self.get_mut::<Children>(parent) {
            Some(c) => c.0.push(child),
            None => self.insert(parent, Children(vec![child]))?,
        }
        Ok(())
    }

    /// Detaches `child` from its parent (no-op if it has none).
    pub fn remove_parent(&mut self, child: Entity) -> Result<(), EcsError> {
        if !self.contains(child) {
            return Err(EcsError::NoSuchEntity(child));
        }
        let Some(old) = self.parent(child) else {
            return Ok(());
        };
        self.remove::<Parent>(child)?;
        let now_empty = self.get_mut::<Children>(old).map(|c| {
            c.0.retain(|&x| x != child);
            c.0.is_empty()
        });
        if now_empty == Some(true) {
            self.remove::<Children>(old)?;
        }
        Ok(())
    }

    /// Despawns `e` and every descendant, and removes `e` from its parent's
    /// [`Children`]. Returns whether `e` was alive.
    pub fn despawn_recursive(&mut self, e: Entity) -> bool {
        self.flush_entities();
        if !self.contains(e) {
            return false;
        }
        let _ = self.remove_parent(e);
        let mut stack = vec![e];
        let mut order = Vec::new();
        while let Some(x) = stack.pop() {
            order.push(x);
            stack.extend_from_slice(self.children(x));
        }
        // The whole subtree goes, so no per-entity hierarchy fix-up is needed (it would
        // cost O(siblings) per child). Reverse discovery order despawns children before
        // their parents.
        for x in order.into_iter().rev() {
            self.despawn_raw(x);
        }
        true
    }

    /// Hierarchy fix-up for [`World::despawn`]: detaches `e` from its parent and removes
    /// `Parent` from each of its children that still points at it.
    pub(crate) fn detach_for_despawn(&mut self, e: Entity) {
        let _ = self.remove_parent(e);
        let children = self.get::<Children>(e).map(|c| c.0.clone());
        for child in children.into_iter().flatten() {
            if self.parent(child) == Some(e) {
                let _ = self.remove::<Parent>(child);
            }
        }
    }
}

/// Recomputes [`GlobalTransform`] for every entity reachable from a root (an entity
/// with a [`Transform`] and no [`Parent`]) through [`Children`]. A descendant without a
/// `Transform` is treated as identity and passes its parent's global transform on.
/// Values are only written when they change, so `Changed<GlobalTransform>` stays
/// meaningful. Traversal order is deterministic (roots in archetype order, children in
/// attachment order).
pub fn propagate_transforms(world: &mut World) {
    // The query's data never aliases, so it cannot fail.
    let Ok(q) = world.query::<(Entity, &Transform), Without<Parent>>() else {
        return;
    };
    let roots: Vec<(Entity, Transform)> = q.map(|(e, t)| (e, *t)).collect();
    let mut stack: Vec<(Entity, Transform)> = Vec::new();
    for (root, local) in roots {
        set_global(world, root, local);
        stack.extend(world.children(root).iter().rev().map(|&c| (c, local)));
        while let Some((e, parent_global)) = stack.pop() {
            if !world.contains(e) {
                continue;
            }
            let global = match world.get::<Transform>(e) {
                Some(local) => {
                    let g = parent_global.compose(local);
                    set_global(world, e, g);
                    g
                }
                None => parent_global,
            };
            stack.extend(world.children(e).iter().rev().map(|&c| (c, global)));
        }
    }
}

fn set_global(world: &mut World, e: Entity, t: Transform) {
    match world.get::<GlobalTransform>(e) {
        Some(g) if g.0 == t => {}
        Some(_) => {
            if let Some(g) = world.get_mut::<GlobalTransform>(e) {
                g.0 = t;
            }
        }
        None => {
            let _ = world.insert(e, GlobalTransform(t));
        }
    }
}
