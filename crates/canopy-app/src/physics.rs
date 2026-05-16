//! [`PhysicsPlugin`]: fixed-step velocity integration and axis-aligned box collisions.
//!
//! Deliberately minimal: bodies are boxes ([`Aabb`]) centred on [`Transform::translation`]
//! (rotation and scale are ignored, and the translation is treated as world space, so
//! bodies should not be parented); dynamic bodies have equal mass; there is no gravity,
//! friction, restitution setting, continuous collision or sleeping. Every step is a pure
//! function of the world state ([`physics_step`]), so runs are deterministic.
use crate::{App, Plugin, Time};
use canopy_ecs::schedule::SystemBuilder;
use canopy_ecs::{Component, EcsError, Entity, Events, Transform, Without, World};

/// Linear velocity in units per second, added to [`Transform::translation`] each step.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LinearVelocity(pub [f32; 3]);
impl Component for LinearVelocity {}

/// An axis-aligned box collider centred on the entity's [`Transform::translation`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb {
    /// Half the box size along x, y and z (all positive).
    pub half_extents: [f32; 3],
}
impl Component for Aabb {}

/// Marks a body that never moves: it is not integrated and never pushed. Two static
/// bodies are never tested against each other.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Static;
impl Component for Static {}

/// Sent once per overlapping pair per step (after integration, before resolution).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Collision {
    /// The pair member with the lower [`Entity::to_bits`].
    pub a: Entity,
    /// The other member.
    pub b: Entity,
    /// Unit axis of least penetration, pointing from `a` towards `b`.
    pub normal: [f32; 3],
    /// Overlap along `normal` (always positive: touching boxes do not collide).
    pub depth: f32,
}

/// Adds [`physics_step`] to `FixedUpdate` and registers the [`Collision`] event.
///
/// The step runs as a command, so it sees the effects of every `FixedUpdate` system of
/// the same fixed step. It uses [`Time::fixed_step`] when a [`Time`] resource exists
/// (`TimePlugin`), otherwise the app's `fixed_step` at build time.
#[derive(Default)]
pub struct PhysicsPlugin;

impl Plugin for PhysicsPlugin {
    fn build(&self, app: &mut App) -> Result<(), EcsError> {
        app.add_event::<Collision>()?;
        let fallback = app.fixed_step;
        app.fixed_update
            .add_system(SystemBuilder::new("physics_step"), move |ctx| {
                ctx.command(move |w| {
                    let dt = w.resource::<Time>().map_or(fallback, |t| t.fixed_step);
                    physics_step(w, dt);
                });
            })?;
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Body {
    e: Entity,
    pos: [f32; 3],
    half: [f32; 3],
    vel: Option<[f32; 3]>,
    fixed: bool,
}

/// Axis of least penetration of `a` and `b` (lowest axis on ties), the unit normal along
/// it from `a` towards `b` (`+` when the centres coincide on it), and the depth; `None`
/// unless the boxes overlap with positive depth on every axis.
fn overlap(a: &Body, b: &Body) -> Option<(usize, [f32; 3], f32)> {
    let mut best: Option<(usize, f32, f32)> = None;
    for k in 0..3 {
        let d = b.pos[k] - a.pos[k];
        let depth = a.half[k] + b.half[k] - d.abs();
        if depth.is_nan() || depth <= 0.0 {
            return None;
        }
        if best.is_none_or(|(_, bd, _)| depth < bd) {
            best = Some((k, depth, if d >= 0.0 { 1.0 } else { -1.0 }));
        }
    }
    let (k, depth, sign) = best?;
    let mut normal = [0.0; 3];
    normal[k] = sign;
    Some((k, normal, depth))
}

/// One physics step of `dt` seconds:
///
/// 1. every non-[`Static`] entity with [`Transform`] and [`LinearVelocity`] moves by
///    `velocity * dt`;
/// 2. bodies (`Transform` + [`Aabb`]) are swept along x (sorted by their lower x edge, then
///    entity id) and every overlapping pair (not both static) gets a [`Collision`] event,
///    sent in `(a, b)` id order if an `Events<Collision>` resource exists;
/// 3. pairs are resolved in that order, re-testing overlap with the positions updated so
///    far: a dynamic body hitting a static one is pushed out along the normal and loses
///    the velocity component that points into it; two dynamic bodies are each pushed half
///    the depth apart and, if approaching, exchange their velocity components along the
///    normal (an elastic collision of equal masses). A body without `LinearVelocity` is
///    pushed but has no velocity to change.
///
/// Only values that change are written, so `Changed<Transform>` stays meaningful.
pub fn physics_step(world: &mut World, dt: f32) {
    if let Ok(q) = world.query::<(&mut Transform, &LinearVelocity), Without<Static>>() {
        for (mut t, v) in q {
            if v.0 != [0.0; 3] {
                for k in 0..3 {
                    t.translation[k] += v.0[k] * dt;
                }
            }
        }
    }
    let Ok(q) = world.query::<(
        Entity,
        &Transform,
        &Aabb,
        Option<&LinearVelocity>,
        Option<&Static>,
    ), ()>() else {
        return;
    };
    let mut bodies: Vec<Body> = q
        .map(|(e, t, b, v, s)| Body {
            e,
            pos: t.translation,
            half: b.half_extents,
            vel: v.map(|v| v.0),
            fixed: s.is_some(),
        })
        .collect();
    bodies.sort_by(|a, b| {
        (a.pos[0] - a.half[0])
            .total_cmp(&(b.pos[0] - b.half[0]))
            .then(a.e.to_bits().cmp(&b.e.to_bits()))
    });
    let initial = bodies.clone();
    // (index of a, index of b, event) with a's id < b's id.
    let mut pairs: Vec<(usize, usize, Collision)> = Vec::new();
    for i in 0..bodies.len() {
        let max_x = bodies[i].pos[0] + bodies[i].half[0];
        for j in i + 1..bodies.len() {
            if bodies[j].pos[0] - bodies[j].half[0] >= max_x {
                break;
            }
            if bodies[i].fixed && bodies[j].fixed {
                continue;
            }
            let (a, b) = if bodies[i].e.to_bits() < bodies[j].e.to_bits() {
                (i, j)
            } else {
                (j, i)
            };
            if let Some((_, normal, depth)) = overlap(&bodies[a], &bodies[b]) {
                let event = Collision {
                    a: bodies[a].e,
                    b: bodies[b].e,
                    normal,
                    depth,
                };
                pairs.push((a, b, event));
            }
        }
    }
    pairs.sort_by_key(|(_, _, c)| (c.a.to_bits(), c.b.to_bits()));
    for &(a, b, _) in &pairs {
        let Some((k, normal, depth)) = overlap(&bodies[a], &bodies[b]) else {
            continue;
        };
        let (fa, fb) = (bodies[a].fixed, bodies[b].fixed);
        let (push_a, push_b) = match (fa, fb) {
            (true, _) => (0.0, depth),
            (_, true) => (depth, 0.0),
            _ => (depth / 2.0, depth / 2.0),
        };
        bodies[a].pos[k] -= normal[k] * push_a;
        bodies[b].pos[k] += normal[k] * push_b;
        let va = bodies[a].vel.map_or(0.0, |v| v[k]);
        let vb = bodies[b].vel.map_or(0.0, |v| v[k]);
        if fa {
            // b moving towards a (against the normal) stops along this axis.
            if let Some(v) = bodies[b].vel.as_mut().filter(|v| v[k] * normal[k] < 0.0) {
                v[k] = 0.0;
            }
        } else if fb {
            if let Some(v) = bodies[a].vel.as_mut().filter(|v| v[k] * normal[k] > 0.0) {
                v[k] = 0.0;
            }
        } else if (vb - va) * normal[k] < 0.0 {
            if let Some(v) = bodies[a].vel.as_mut() {
                v[k] = vb;
            }
            if let Some(v) = bodies[b].vel.as_mut() {
                v[k] = va;
            }
        }
    }
    for (body, before) in bodies.iter().zip(&initial) {
        if body.pos != before.pos {
            if let Some(t) = world.get_mut::<Transform>(body.e) {
                t.translation = body.pos;
            }
        }
        if body.vel != before.vel {
            if let (Some(v), Some(lv)) = (body.vel, world.get_mut::<LinearVelocity>(body.e)) {
                lv.0 = v;
            }
        }
    }
    if let Some(events) = world.resource_mut::<Events<Collision>>() {
        for (_, _, c) in pairs {
            events.send(c);
        }
    }
}
