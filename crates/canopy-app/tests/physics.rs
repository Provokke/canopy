//! `PhysicsPlugin`: integration, collision detection, resolution and determinism.
use canopy_app::{
    physics_step, Aabb, App, Collision, LinearVelocity, PhysicsPlugin, Static, TimePlugin, DT,
};
use canopy_ecs::event::EventCursor;
use canopy_ecs::schedule::Executor;
use canopy_ecs::{Entity, Events, Transform, World};

fn body(w: &mut World, pos: [f32; 3], half: f32, vel: [f32; 3]) -> Entity {
    w.spawn((
        Transform::from_translation(pos),
        Aabb {
            half_extents: [half; 3],
        },
        LinearVelocity(vel),
    ))
}

fn wall(w: &mut World, pos: [f32; 3], half: [f32; 3]) -> Entity {
    w.spawn((
        Transform::from_translation(pos),
        Aabb { half_extents: half },
        Static,
    ))
}

fn pos(w: &World, e: Entity) -> [f32; 3] {
    w.get::<Transform>(e).unwrap().translation
}

fn vel(w: &World, e: Entity) -> [f32; 3] {
    w.get::<LinearVelocity>(e).unwrap().0
}

fn app() -> App {
    let mut app = App::new(Executor::SingleThreaded);
    app.add_plugin(TimePlugin).unwrap();
    app.add_plugin(PhysicsPlugin).unwrap();
    app
}

/// Every collision event sent so far, through a persistent cursor.
fn drain(app: &App, cursor: &mut EventCursor, out: &mut Vec<Collision>) {
    let events = app.world.resource::<Events<Collision>>().unwrap();
    out.extend(events.read(cursor).copied());
}

#[test]
fn integrates_velocity_once_per_fixed_step() {
    let mut app = app();
    let e = body(&mut app.world, [0.0; 3], 0.5, [1.0, -2.0, 3.0]);
    let fixed = app.world.spawn((
        Transform::from_translation([50.0, 0.0, 0.0]),
        LinearVelocity([5.0, 0.0, 0.0]),
        Static,
    ));
    let mut expect = [0.0f32; 3];
    for _ in 0..10 {
        app.update().unwrap();
        for (k, v) in [1.0f32, -2.0, 3.0].into_iter().enumerate() {
            expect[k] += v * DT;
        }
    }
    assert_eq!(pos(&app.world, e), expect);
    // Two and a half fixed steps of time: two steps run.
    app.advance(2.5 * DT).unwrap();
    for (k, v) in [1.0f32, -2.0, 3.0].into_iter().enumerate() {
        expect[k] += v * DT;
        expect[k] += v * DT;
    }
    assert_eq!(pos(&app.world, e), expect);
    // Static bodies never move, even with a velocity.
    assert_eq!(pos(&app.world, fixed), [50.0, 0.0, 0.0]);
}

#[test]
fn box_falls_onto_a_static_floor_and_rests_on_it() {
    let mut app = app();
    let floor = wall(&mut app.world, [0.0; 3], [10.0, 0.5, 10.0]);
    let b = body(&mut app.world, [0.0, 3.0, 0.0], 0.5, [0.0, -5.0, 0.0]);
    let (mut cursor, mut seen) = (EventCursor::default(), Vec::new());
    for _ in 0..120 {
        app.update().unwrap();
        drain(&app, &mut cursor, &mut seen);
        // Never ends a step inside the floor.
        assert!(
            pos(&app.world, b)[1] >= 1.0 - 1e-5,
            "{:?}",
            pos(&app.world, b)
        );
    }
    assert_eq!(pos(&app.world, b), [0.0, 1.0, 0.0]);
    assert_eq!(vel(&app.world, b), [0.0, 0.0, 0.0]);
    assert_eq!(pos(&app.world, floor), [0.0; 3]);
    // Exactly one contact: the step that reached the floor (afterwards it rests on the
    // surface, which is touching, not overlapping).
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!((seen[0].a, seen[0].b), (floor, b));
    assert_eq!(seen[0].normal, [0.0, 1.0, 0.0]);
    assert!(seen[0].depth > 0.0);
}

#[test]
fn dynamic_bodies_meeting_head_on_exchange_velocities() {
    let mut w = World::new();
    let l = body(&mut w, [-1.0, 0.0, 0.0], 0.5, [2.0, 0.0, 0.0]);
    let r = body(&mut w, [1.0, 0.0, 0.0], 0.5, [-1.0, 0.0, 0.0]);
    w.insert_resource(Events::<Collision>::default());
    for _ in 0..60 {
        physics_step(&mut w, DT);
        let (pl, pr) = (pos(&w, l), pos(&w, r));
        assert!(pr[0] - pl[0] >= 1.0 - 1e-5, "{pl:?} {pr:?}");
    }
    assert_eq!(vel(&w, l), [-1.0, 0.0, 0.0]);
    assert_eq!(vel(&w, r), [2.0, 0.0, 0.0]);
    let events = w.resource::<Events<Collision>>().unwrap();
    let all: Vec<Collision> = events.read(&mut EventCursor::default()).copied().collect();
    assert_eq!(all.len(), 1);
    assert_eq!((all[0].a, all[0].b, all[0].normal), (l, r, [1.0, 0.0, 0.0]));
}

#[test]
fn touching_or_partly_overlapping_boxes_do_not_collide() {
    let mut w = World::new();
    w.insert_resource(Events::<Collision>::default());
    // Exactly touching along x.
    body(&mut w, [0.0; 3], 0.5, [0.0; 3]);
    body(&mut w, [1.0, 0.0, 0.0], 0.5, [0.0; 3]);
    // Overlapping in x and z, but separated in y.
    body(&mut w, [10.0, 0.0, 0.0], 0.5, [0.0; 3]);
    body(&mut w, [10.2, 2.0, 0.0], 0.5, [0.0; 3]);
    // Two static walls overlapping each other are never tested.
    wall(&mut w, [20.0, 0.0, 0.0], [1.0; 3]);
    wall(&mut w, [20.5, 0.0, 0.0], [1.0; 3]);
    physics_step(&mut w, DT);
    assert!(w.resource::<Events<Collision>>().unwrap().is_empty());
}

#[test]
fn least_penetration_axis_and_static_push_out() {
    let mut w = World::new();
    w.insert_resource(Events::<Collision>::default());
    let floor = wall(&mut w, [0.0; 3], [5.0, 0.5, 5.0]);
    // Sunk 0.2 into the floor from above and moving sideways: pushed up along y only,
    // keeping its x velocity; the y velocity points away from the floor, so it is kept.
    let b = body(&mut w, [1.0, 0.8, 0.0], 0.5, [3.0, 0.5, 0.0]);
    physics_step(&mut w, 0.0);
    let p = pos(&w, b);
    assert!((p[1] - 1.0).abs() < 1e-6, "{p:?}");
    assert_eq!(p[0], 1.0);
    assert_eq!(vel(&w, b), [3.0, 0.5, 0.0]);
    let all: Vec<Collision> = w
        .resource::<Events<Collision>>()
        .unwrap()
        .read(&mut EventCursor::default())
        .copied()
        .collect();
    assert_eq!(all.len(), 1);
    assert_eq!(
        (all[0].a, all[0].b, all[0].normal),
        (floor, b, [0.0, 1.0, 0.0])
    );
    assert!((all[0].depth - 0.2).abs() < 1e-6);
}

/// A box of walls with bodies bouncing inside: identical results for both executors and
/// for repeated runs, and no body ever escapes or tunnels out.
#[test]
fn many_bodies_are_deterministic_and_stay_inside() {
    fn run(executor: Executor) -> Vec<(u64, [u32; 6])> {
        let mut app = App::new(executor);
        app.add_plugin(TimePlugin).unwrap();
        app.add_plugin(PhysicsPlugin).unwrap();
        let w = &mut app.world;
        wall(w, [-11.0, 0.0, 0.0], [1.0, 12.0, 1.0]);
        wall(w, [11.0, 0.0, 0.0], [1.0, 12.0, 1.0]);
        wall(w, [0.0, -11.0, 0.0], [12.0, 1.0, 1.0]);
        wall(w, [0.0, 11.0, 0.0], [12.0, 1.0, 1.0]);
        let mut s: u32 = 12345;
        let mut next = || {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 8) as f32 / (1u32 << 24) as f32
        };
        for i in 0..40 {
            let p = [(i % 8) as f32 * 2.2 - 7.7, (i / 8) as f32 * 2.2 - 4.4, 0.0];
            let v = [next() * 6.0 - 3.0, next() * 6.0 - 3.0, 0.0];
            body(w, p, 0.4, v);
        }
        for _ in 0..300 {
            app.update().unwrap();
        }
        let mut out: Vec<(u64, [u32; 6])> = app
            .world
            .query::<(Entity, &Transform, &LinearVelocity), ()>()
            .unwrap()
            .map(|(e, t, v)| {
                let p = t.translation;
                assert!(p[0].abs() < 10.0 && p[1].abs() < 10.0, "escaped: {p:?}");
                let b = |x: f32| x.to_bits();
                (
                    e.to_bits(),
                    [b(p[0]), b(p[1]), b(p[2]), b(v.0[0]), b(v.0[1]), b(v.0[2])],
                )
            })
            .collect();
        out.sort_unstable();
        out
    }
    let single = run(Executor::SingleThreaded);
    assert_eq!(single.len(), 40);
    assert_eq!(single, run(Executor::MultiThreaded));
    assert_eq!(single, run(Executor::SingleThreaded));
}

#[test]
fn events_name_the_lower_id_first_whatever_the_sweep_order() {
    let mut w = World::new();
    w.insert_resource(Events::<Collision>::default());
    // The lower id is on the right, so the x sweep meets it second.
    let right = body(&mut w, [1.0, 0.0, 0.0], 0.5, [0.0; 3]);
    let left = body(&mut w, [0.2, 0.0, 0.0], 0.5, [0.0; 3]);
    physics_step(&mut w, DT);
    let all: Vec<Collision> = w
        .resource::<Events<Collision>>()
        .unwrap()
        .read(&mut EventCursor::default())
        .copied()
        .collect();
    assert_eq!(all.len(), 1);
    assert_eq!(
        (all[0].a, all[0].b, all[0].normal),
        (right, left, [-1.0, 0.0, 0.0])
    );
    // Each pushed half the 0.2 overlap, away from the other.
    assert!((pos(&w, right)[0] - 1.1).abs() < 1e-6);
    assert!((pos(&w, left)[0] - 0.1).abs() < 1e-6);
}
