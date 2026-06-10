use canopy_ecs::event::update_events;
use canopy_ecs::schedule::{Executor, SystemBuilder};
use canopy_ecs::{
    Changed, Children, Commands, Component, EcsError, Entity, EventReader, EventWriter, Events,
    GlobalTransform, Local, Parent, ResMut, Schedule, Transform, World,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Mark(u32);
impl Component for Mark {}

struct Dropper(Arc<AtomicUsize>);
impl Component for Dropper {}
impl Drop for Dropper {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

const THREADS: usize = 4;
const PER_THREAD: usize = if cfg!(miri) { 8 } else { 500 };

#[test]
fn concurrent_reservations_are_unique_and_become_alive_on_flush() {
    let mut w = World::new();
    // Populate the free list so reservations first reuse slots, then run past it.
    let old: Vec<Entity> = (0..10).map(|i| w.spawn(Mark(i))).collect();
    for &e in &old[..6] {
        assert!(w.despawn(e));
    }
    let world = &w;
    let mut all: Vec<Entity> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..THREADS)
            .map(|_| {
                s.spawn(|| {
                    (0..PER_THREAD)
                        .map(|_| world.reserve_entity())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    let total = THREADS * PER_THREAD;
    assert_eq!(all.len(), total);
    // Not alive until flushed.
    assert!(all.iter().all(|&e| !w.contains(e)));
    assert_eq!(w.entity_count(), 4);
    all.sort();
    all.dedup();
    assert_eq!(all.len(), total, "every reservation is a distinct id");
    // Reused slots carry the bumped generation: no reservation aliases a dead id.
    for &e in &old[..6] {
        assert!(!all.contains(&e));
    }
    w.flush_entities();
    assert!(all.iter().all(|&e| w.contains(e)));
    assert_eq!(w.entity_count(), 4 + total);
    // The allocator is consistent afterwards.
    let fresh = w.spawn(Mark(99));
    assert!(!all.contains(&fresh));
    assert!(w.despawn(all[0]));
    assert!(!w.contains(all[0]));
}

#[test]
fn spawn_after_reserve_does_not_collide() {
    let mut w = World::new();
    let a = w.reserve_entity();
    let b = w.reserve_entity();
    let c = w.spawn(Mark(1));
    assert!(a != c && b != c && a != b);
    assert!(w.contains(a) && w.contains(b) && w.contains(c));
    assert_eq!(w.entity_count(), 3);
}

#[derive(Default)]
struct Made(Vec<Entity>);

fn spawn_family(mut c: Commands, mut made: ResMut<Made>) {
    let parent = c.spawn(Mark(1));
    let kid = c.spawn(Mark(2));
    let grandkid = c.spawn(Mark(3));
    c.set_parent(kid, parent);
    c.set_parent(grandkid, kid);
    made.0 = vec![parent, kid, grandkid];
}

#[test]
fn commands_spawn_returns_a_usable_id() {
    let mut w = World::new();
    w.insert_resource(Made::default());
    let mut s = Schedule::new(Executor::MultiThreaded);
    s.add_fn(SystemBuilder::new("family"), spawn_family)
        .unwrap();
    s.run(&mut w).unwrap();
    let ids = w.resource::<Made>().unwrap().0.clone();
    let (p, k, g) = (ids[0], ids[1], ids[2]);
    assert_eq!(w.get::<Mark>(p).map(|m| m.0), Some(1));
    assert_eq!(w.get::<Mark>(k).map(|m| m.0), Some(2));
    assert_eq!(w.get::<Mark>(g).map(|m| m.0), Some(3));
    assert_eq!(w.children(p), &[k]);
    assert_eq!(w.parent(g), Some(k));
    assert_eq!(w.entity_count(), 3);
}

fn kill_tree(mut c: Commands, made: canopy_ecs::Res<Made>) {
    c.despawn_recursive(made.0[0]);
}

#[test]
fn commands_despawn_recursive_removes_the_subtree() {
    let mut w = World::new();
    w.insert_resource(Made::default());
    let mut s = Schedule::new(Executor::SingleThreaded);
    s.add_fn(SystemBuilder::new("family"), spawn_family)
        .unwrap();
    s.run(&mut w).unwrap();
    let ids = w.resource::<Made>().unwrap().0.clone();
    let mut s2 = Schedule::new(Executor::SingleThreaded);
    s2.add_fn(SystemBuilder::new("kill"), kill_tree).unwrap();
    s2.run(&mut w).unwrap();
    assert!(ids.iter().all(|&e| !w.contains(e)));
    assert_eq!(w.entity_count(), 0);
}

#[test]
fn set_parent_keeps_both_sides_consistent() {
    let mut w = World::new();
    let (a, b, c) = (w.spawn(Mark(0)), w.spawn(Mark(1)), w.spawn(Mark(2)));
    w.set_parent(b, a).unwrap();
    w.set_parent(c, a).unwrap();
    assert_eq!(w.children(a), &[b, c]);
    assert_eq!(w.get::<Parent>(b), Some(&Parent(a)));
    // Reparent c under b: removed from the old parent's list.
    w.set_parent(c, b).unwrap();
    assert_eq!(w.children(a), &[b]);
    assert_eq!(w.children(b), &[c]);
    // Reparenting to the same parent is a no-op.
    w.set_parent(c, b).unwrap();
    assert_eq!(w.children(b), &[c]);
    // Detaching the last child removes the empty list.
    w.remove_parent(c).unwrap();
    assert!(w.get::<Children>(b).is_none());
    assert!(w.get::<Parent>(c).is_none());
}

#[test]
fn set_parent_rejects_cycles_and_dead_entities() {
    let mut w = World::new();
    let (a, b, c) = (w.spawn(Mark(0)), w.spawn(Mark(1)), w.spawn(Mark(2)));
    w.set_parent(b, a).unwrap();
    w.set_parent(c, b).unwrap();
    assert!(matches!(
        w.set_parent(a, c),
        Err(EcsError::HierarchyCycle { .. })
    ));
    assert!(matches!(
        w.set_parent(a, a),
        Err(EcsError::HierarchyCycle { .. })
    ));
    // A rejected change leaves the tree untouched.
    assert!(w.get::<Parent>(a).is_none());
    assert_eq!(w.children(a), &[b]);
    let dead = w.spawn(Mark(9));
    w.despawn(dead);
    assert_eq!(w.set_parent(a, dead), Err(EcsError::NoSuchEntity(dead)));
    assert_eq!(w.set_parent(dead, a), Err(EcsError::NoSuchEntity(dead)));
}

#[test]
fn despawn_recursive_drops_every_descendant_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut w = World::new();
    let root = w.spawn(Dropper(drops.clone()));
    let keep = w.spawn(Dropper(drops.clone()));
    let mid = w.spawn(Dropper(drops.clone()));
    let leaf1 = w.spawn(Dropper(drops.clone()));
    let leaf2 = w.spawn(Dropper(drops.clone()));
    w.set_parent(mid, root).unwrap();
    w.set_parent(leaf1, mid).unwrap();
    w.set_parent(leaf2, mid).unwrap();
    w.set_parent(root, keep).unwrap();
    assert!(w.despawn_recursive(root));
    assert_eq!(drops.load(Ordering::SeqCst), 4);
    for e in [root, mid, leaf1, leaf2] {
        assert!(!w.contains(e));
    }
    assert!(w.contains(keep));
    assert!(
        w.get::<Children>(keep).is_none(),
        "detached from its parent"
    );
    assert!(!w.despawn_recursive(root));
    drop(w);
    assert_eq!(drops.load(Ordering::SeqCst), 5);
}

fn approx(a: [f32; 3], b: [f32; 3]) {
    for i in 0..3 {
        assert!((a[i] - b[i]).abs() < 1e-4, "{a:?} != {b:?}");
    }
}

#[test]
fn transforms_propagate_through_the_hierarchy() {
    let mut w = World::new();
    let root = w.spawn(Transform {
        translation: [10.0, 0.0, 0.0],
        rotation: std::f32::consts::FRAC_PI_2,
        scale: 2.0,
    });
    let child = w.spawn(Transform::from_translation([1.0, 0.0, 0.0]));
    let grand = w.spawn(Transform::from_translation([0.0, 1.0, 3.0]));
    w.set_parent(child, root).unwrap();
    w.set_parent(grand, child).unwrap();
    canopy_ecs::hierarchy::propagate_transforms(&mut w);
    let g = |w: &World, e| w.get::<GlobalTransform>(e).unwrap().0;
    approx(g(&w, root).translation, [10.0, 0.0, 0.0]);
    // Child local (1,0) is scaled by 2 and rotated 90 degrees: (0,2) from the root.
    approx(g(&w, child).translation, [10.0, 2.0, 0.0]);
    assert!((g(&w, child).scale - 2.0).abs() < 1e-6);
    // Grandchild local (0,1,3) under global rotation 90 and scale 2: (-2,0), z 6.
    approx(g(&w, grand).translation, [8.0, 2.0, 6.0]);

    // Moving the root moves the whole subtree.
    w.get_mut::<Transform>(root).unwrap().translation = [0.0, 0.0, 0.0];
    canopy_ecs::hierarchy::propagate_transforms(&mut w);
    approx(g(&w, child).translation, [0.0, 2.0, 0.0]);
    approx(g(&w, grand).translation, [-2.0, 2.0, 6.0]);

    // Detaching makes the former child a root with just its local transform.
    w.remove_parent(child).unwrap();
    canopy_ecs::hierarchy::propagate_transforms(&mut w);
    approx(g(&w, child).translation, [1.0, 0.0, 0.0]);
    approx(g(&w, grand).translation, [1.0, 1.0, 3.0]);
}

#[test]
fn propagation_only_marks_changed_globals() {
    let mut w = World::new();
    let root = w.spawn(Transform::from_translation([1.0, 0.0, 0.0]));
    let child = w.spawn(Transform::from_translation([0.0, 1.0, 0.0]));
    w.set_parent(child, root).unwrap();
    canopy_ecs::hierarchy::propagate_transforms(&mut w);
    w.clear_trackers();
    canopy_ecs::hierarchy::propagate_transforms(&mut w);
    assert_eq!(
        w.query::<Entity, Changed<GlobalTransform>>()
            .unwrap()
            .count(),
        0
    );
    w.get_mut::<Transform>(child).unwrap().translation[0] = 5.0;
    canopy_ecs::hierarchy::propagate_transforms(&mut w);
    let changed: Vec<Entity> = w
        .query::<Entity, Changed<GlobalTransform>>()
        .unwrap()
        .collect();
    assert_eq!(changed, vec![child]);
}

#[test]
fn propagation_skips_despawned_children() {
    let mut w = World::new();
    let root = w.spawn(Transform::IDENTITY);
    let kid = w.spawn(Transform::IDENTITY);
    w.set_parent(kid, root).unwrap();
    // A stale id in `Children` (edited by hand; despawn itself detaches) is skipped.
    let stale = stale_id(&mut w);
    w.get_mut::<Children>(root).unwrap().0.push(stale);
    w.despawn(kid);
    canopy_ecs::hierarchy::propagate_transforms(&mut w);
    assert!(w.get::<GlobalTransform>(root).is_some());
}

fn stale_id(w: &mut World) -> Entity {
    let e = w.spawn_empty();
    w.despawn(e);
    e
}

#[test]
fn plain_despawn_detaches_from_parent_and_orphans_children() {
    let mut w = World::new();
    let root = w.spawn(Transform::IDENTITY);
    let mid = w.spawn(Transform::from_translation([1.0, 0.0, 0.0]));
    let other = w.spawn(Transform::IDENTITY);
    let leaf_a = w.spawn(Transform::from_translation([0.0, 2.0, 0.0]));
    let leaf_b = w.spawn(Transform::IDENTITY);
    w.set_parent(mid, root).unwrap();
    w.set_parent(other, root).unwrap();
    w.set_parent(leaf_a, mid).unwrap();
    w.set_parent(leaf_b, mid).unwrap();
    assert!(w.despawn(mid));
    // Removed from the parent's list; the sibling stays.
    assert_eq!(w.children(root), &[other]);
    // Its children survive as roots.
    assert!(w.contains(leaf_a) && w.contains(leaf_b));
    assert_eq!(w.parent(leaf_a), None);
    assert_eq!(w.parent(leaf_b), None);
    // Roots are propagated from their own transform now.
    canopy_ecs::hierarchy::propagate_transforms(&mut w);
    assert_eq!(
        w.get::<GlobalTransform>(leaf_a).unwrap().0.translation,
        [0.0, 2.0, 0.0]
    );
    // The last child going removes `Children` entirely.
    assert!(w.despawn(other));
    assert!(w.get::<Children>(root).is_none());
    assert_eq!(w.query::<&Children, ()>().unwrap().count(), 0);
    assert_eq!(w.query::<&Parent, ()>().unwrap().count(), 0);
    // Despawning a root (no `Parent`, only `Children`) orphans its children too.
    let root2 = w.spawn_empty();
    let kid2 = w.spawn_empty();
    w.set_parent(kid2, root2).unwrap();
    assert!(w.despawn(root2));
    assert_eq!(w.parent(kid2), None);
    // A plain entity still despawns normally.
    let plain = w.spawn(Transform::IDENTITY);
    assert!(w.despawn(plain));
    assert!(!w.despawn(plain));
}

struct Victim(Entity);

fn despawn_victim(mut c: Commands, v: canopy_ecs::Res<Victim>) {
    c.despawn(v.0);
}

#[test]
fn commands_despawn_detaches_too() {
    let mut w = World::new();
    let root = w.spawn_empty();
    let kid = w.spawn_empty();
    let kid2 = w.spawn_empty();
    w.set_parent(kid, root).unwrap();
    w.set_parent(kid2, root).unwrap();
    w.insert_resource(Victim(kid));
    let mut s = Schedule::new(Executor::SingleThreaded);
    s.add_fn(SystemBuilder::new("despawn"), despawn_victim)
        .unwrap();
    s.run(&mut w).unwrap();
    assert!(!w.contains(kid));
    assert_eq!(w.children(root), &[kid2]);
}

// ---- event lifetime -------------------------------------------------------------

#[derive(Default)]
struct Seen(Vec<usize>);

fn writer(mut ev: EventWriter<u32>, mut frame: Local<u32>) {
    if *frame == 0 {
        ev.send(7);
    }
    *frame += 1;
}

fn reader(mut ev: EventReader<u32>, mut seen: ResMut<Seen>) {
    let n = ev.read().count();
    seen.0.push(n);
}

#[test]
fn events_are_readable_for_exactly_two_updates() {
    let mut w = World::new();
    w.insert_resource(Events::<u32>::default());
    w.insert_resource(Seen::default());
    // Frame schedule: write, read, then advance the event buffers (insertion order
    // orders the conflicting systems).
    let mut frame = Schedule::new(Executor::SingleThreaded);
    frame.add_fn(SystemBuilder::new("write"), writer).unwrap();
    frame.add_fn(SystemBuilder::new("read"), reader).unwrap();
    frame
        .add_fn(SystemBuilder::new("update"), update_events::<u32>)
        .unwrap();
    // A late reader: a fresh system (fresh cursor) in its own schedule, run between
    // frames so it observes what the buffers still hold.
    let observe = |w: &mut World| {
        let mut s = Schedule::new(Executor::SingleThreaded);
        s.add_fn(SystemBuilder::new("late"), reader).unwrap();
        s.run(w).unwrap();
    };
    frame.run(&mut w).unwrap(); // frame 1: sent and read, then update #1
    observe(&mut w); // one update old: still alive
    frame.run(&mut w).unwrap(); // frame 2: update #2 drops it
    observe(&mut w); // two updates old: gone
    frame.run(&mut w).unwrap();
    observe(&mut w);
    // `read` saw the event in frame 1 only; the late readers saw it after update #1
    // but not after update #2.
    let seen = &w.resource::<Seen>().unwrap().0;
    assert_eq!(seen, &vec![1, 1, 0, 0, 0, 0]);
}
