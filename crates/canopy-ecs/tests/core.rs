use canopy_ecs::schedule::{Executor, SystemBuilder};
use canopy_ecs::{Changed, EcsError, Entity, Schedule, Tick, With, Without, World};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug, PartialEq, Clone, Copy)]
struct Pos(f32);
impl canopy_ecs::Component for Pos {}
#[derive(Debug, PartialEq, Clone, Copy)]
struct Vel(f32);
impl canopy_ecs::Component for Vel {}
#[derive(Debug, PartialEq)]
struct Tag;
impl canopy_ecs::Component for Tag {}

struct Dropper(Arc<AtomicUsize>);
impl canopy_ecs::Component for Dropper {}
impl Drop for Dropper {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

const N: usize = if cfg!(miri) { 20 } else { 1000 };

#[test]
fn entity_reuse_bumps_generation() {
    let mut w = World::new();
    let a = w.spawn(Pos(1.0));
    assert!(w.despawn(a));
    let b = w.spawn(Pos(2.0));
    assert_eq!(a.index(), b.index());
    assert_ne!(a.generation(), b.generation());
    assert!(!w.contains(a));
    assert!(w.get::<Pos>(a).is_none());
    assert!(!w.despawn(a));
}

#[test]
fn archetype_moves_preserve_data() {
    let mut w = World::new();
    let es: Vec<Entity> = (0..N).map(|i| w.spawn((Pos(i as f32), Vel(1.0)))).collect();
    for (i, &e) in es.iter().enumerate() {
        if i % 2 == 0 {
            w.insert(e, Tag).unwrap();
        }
        if i % 3 == 0 {
            assert!(w.remove::<Vel>(e).unwrap());
        }
    }
    for (i, &e) in es.iter().enumerate() {
        assert_eq!(w.get::<Pos>(e), Some(&Pos(i as f32)));
        assert_eq!(w.get::<Tag>(e).is_some(), i % 2 == 0);
        assert_eq!(w.get::<Vel>(e).is_some(), i % 3 != 0);
    }
    let n = w
        .query::<&Pos, (With<Tag>, Without<Vel>)>()
        .unwrap()
        .count();
    assert_eq!(n, (0..N).filter(|i| i % 6 == 0).count());
    for (mut p, v) in w.query::<(&mut Pos, &Vel), ()>().unwrap() {
        p.0 += v.0;
    }
    assert_eq!(w.get::<Pos>(es[1]), Some(&Pos(2.0)));
}

#[test]
fn aliasing_queries_rejected() {
    let mut w = World::new();
    w.spawn(Pos(0.0));
    assert!(matches!(
        w.query::<(&mut Pos, &mut Pos), ()>(),
        Err(EcsError::ConflictingQueryAccess(_))
    ));
    assert!(matches!(
        w.query::<(&Pos, &mut Pos), ()>(),
        Err(EcsError::ConflictingQueryAccess(_))
    ));
    assert!(w.query::<(&Pos, &Pos), ()>().is_ok());
}

#[test]
fn change_detection() {
    let mut w = World::new();
    let a = w.spawn(Pos(0.0));
    let b = w.spawn(Pos(0.0));
    w.clear_trackers();
    assert_eq!(w.query::<Entity, Changed<Pos>>().unwrap().count(), 0);
    w.get_mut::<Pos>(b).unwrap().0 = 1.0;
    let changed: Vec<Entity> = w.query::<Entity, Changed<Pos>>().unwrap().collect();
    assert_eq!(changed, vec![b]);
    let _ = a;
}

#[test]
fn change_ticks_wraparound() {
    assert!(Tick(5).is_newer_than(Tick(u32::MAX - 5), Tick(10)));
    assert!(!Tick(u32::MAX - 10).is_newer_than(Tick(u32::MAX - 5), Tick(10)));
    let mut w = World::new();
    w.set_change_tick(u32::MAX - 1);
    let e = w.spawn(Pos(0.0));
    w.clear_trackers(); // tick wraps past u32::MAX
    w.clear_trackers();
    assert_eq!(w.query::<Entity, Changed<Pos>>().unwrap().count(), 0);
    w.get_mut::<Pos>(e).unwrap();
    assert_eq!(w.query::<Entity, Changed<Pos>>().unwrap().count(), 1);
}

#[test]
fn every_component_dropped_exactly_once() {
    let c = Arc::new(AtomicUsize::new(0));
    {
        let mut w = World::new();
        let es: Vec<Entity> = (0..N)
            .map(|_| w.spawn((Dropper(c.clone()), Pos(0.0))))
            .collect();
        for &e in es.iter().step_by(4) {
            w.despawn(e);
        }
        for &e in es.iter().skip(1).step_by(4) {
            w.remove::<Dropper>(e).unwrap();
        }
        for &e in es.iter().skip(2).step_by(4) {
            w.insert(e, Dropper(c.clone())).unwrap(); // replace drops old
            w.insert(e, Tag).unwrap(); // archetype move must not drop
        }
        w.insert_resource(Dropper(c.clone()));
    }
    assert_eq!(c.load(Ordering::SeqCst), N + N / 4 + 1);
}

#[test]
fn system_panic_does_not_double_drop() {
    let c = Arc::new(AtomicUsize::new(0));
    {
        let mut w = World::new();
        for _ in 0..10 {
            w.spawn(Dropper(c.clone()));
        }
        let mut s = Schedule::new(Executor::MultiThreaded);
        s.add_system(SystemBuilder::new("boom").write::<Pos>(), |_| {
            panic!("boom")
        })
        .unwrap();
        assert_eq!(s.run(&mut w), Err(EcsError::SystemPanicked("boom".into())));
    }
    assert_eq!(c.load(Ordering::SeqCst), 10);
}

#[test]
fn ordering_and_commands() {
    let mut w = World::new();
    w.insert_resource(Vec::<&'static str>::new());
    let mut s = Schedule::new(Executor::MultiThreaded);
    s.add_system(
        SystemBuilder::new("b")
            .res_write::<Vec<&'static str>>()
            .after("a"),
        |ctx| {
            ctx.resource_mut::<Vec<&'static str>>()
                .unwrap()
                .unwrap()
                .push("b");
        },
    )
    .unwrap();
    s.add_system(
        SystemBuilder::new("a").res_write::<Vec<&'static str>>(),
        |ctx| {
            ctx.resource_mut::<Vec<&'static str>>()
                .unwrap()
                .unwrap()
                .push("a");
            ctx.command(|w| {
                w.spawn(Tag);
            });
        },
    )
    .unwrap();
    s.run(&mut w).unwrap();
    assert_eq!(w.resource::<Vec<&'static str>>().unwrap(), &vec!["a", "b"]);
    assert_eq!(w.entity_count(), 1);
    assert!(s.ambiguities().unwrap().is_empty());
}

#[test]
fn undeclared_access_rejected_and_ambiguity_reported() {
    let mut w = World::new();
    w.spawn(Pos(0.0));
    let mut s = Schedule::new(Executor::SingleThreaded);
    let seen = Arc::new(std::sync::Mutex::new(None));
    let s2 = seen.clone();
    s.add_system(SystemBuilder::new("x").read::<Vel>(), move |ctx| {
        *s2.lock().unwrap() = Some(ctx.query::<&mut Pos, ()>().err());
    })
    .unwrap();
    s.add_system(SystemBuilder::new("y").write::<Vel>(), |_| {})
        .unwrap();
    s.run(&mut w).unwrap();
    assert!(matches!(
        seen.lock().unwrap().clone().flatten(),
        Some(EcsError::UndeclaredAccess { .. })
    ));
    assert_eq!(
        s.ambiguities().unwrap(),
        &[("x".to_string(), "y".to_string())]
    );
    assert!(SystemBuilder::new("bad")
        .read::<Pos>()
        .write::<Pos>()
        .pipe(|b| s.add_system(b, |_| {}).is_err()));
}

trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}

#[test]
fn conflicting_systems_never_overlap_nonconflicting_do() {
    let runs = if cfg!(miri) { 3 } else { 2000 };
    let mut w = World::new();
    for i in 0..10 {
        w.spawn((Pos(i as f32), Vel(1.0)));
    }
    let in_pos = Arc::new(AtomicUsize::new(0));
    let violations = Arc::new(AtomicUsize::new(0));
    let mut s = Schedule::new(Executor::MultiThreaded);
    for k in 0..3 {
        let (ip, v) = (in_pos.clone(), violations.clone());
        s.add_system(
            SystemBuilder::new(format!("w{k}"))
                .write::<Pos>()
                .read::<Vel>(),
            move |ctx| {
                if ip.fetch_add(1, Ordering::SeqCst) != 0 {
                    v.fetch_add(1, Ordering::SeqCst);
                }
                for (mut p, vel) in ctx.query::<(&mut Pos, &Vel), ()>().unwrap() {
                    p.0 += vel.0;
                }
                std::hint::spin_loop();
                ip.fetch_sub(1, Ordering::SeqCst);
            },
        )
        .unwrap();
    }
    for _ in 0..runs {
        s.run(&mut w).unwrap();
    }
    assert_eq!(violations.load(Ordering::SeqCst), 0);
    assert_eq!(
        w.query::<&Pos, ()>().unwrap().map(|p| p.0).sum::<f32>(),
        45.0 + 30.0 * runs as f32
    );

    // Two readers rendezvous: only possible if they run concurrently.
    let met = Arc::new(AtomicUsize::new(0));
    let ok = Arc::new(AtomicUsize::new(0));
    let mut s = Schedule::new(Executor::MultiThreaded);
    for k in 0..2 {
        let (m, o) = (met.clone(), ok.clone());
        s.add_system(
            SystemBuilder::new(format!("r{k}")).read::<Pos>(),
            move |_| {
                m.fetch_add(1, Ordering::SeqCst);
                let start = std::time::Instant::now();
                while m.load(Ordering::SeqCst) < 2 && start.elapsed().as_secs() < 5 {
                    std::thread::yield_now();
                }
                if m.load(Ordering::SeqCst) >= 2 {
                    o.fetch_add(1, Ordering::SeqCst);
                }
            },
        )
        .unwrap();
    }
    s.run(&mut w).unwrap();
    assert_eq!(ok.load(Ordering::SeqCst), 2);
}

#[test]
fn added_and_or_filters() {
    use canopy_ecs::{Added, Or};
    let mut w = World::new();
    let a = w.spawn(Pos(0.0));
    let b = w.spawn(Vel(0.0));
    let _c = w.spawn(Tag);
    w.clear_trackers();
    assert_eq!(w.query::<Entity, Added<Pos>>().unwrap().count(), 0);
    w.get_mut::<Pos>(a).unwrap();
    assert_eq!(w.query::<Entity, Added<Pos>>().unwrap().count(), 0);
    let d = w.spawn(Pos(1.0));
    assert_eq!(
        w.query::<Entity, Added<Pos>>().unwrap().collect::<Vec<_>>(),
        vec![d]
    );
    let mut either: Vec<Entity> = w
        .query::<Entity, Or<(With<Pos>, With<Vel>)>>()
        .unwrap()
        .collect();
    either.sort();
    let mut expect = vec![a, b, d];
    expect.sort();
    assert_eq!(either, expect);
    w.clear_trackers();
    w.get_mut::<Vel>(b).unwrap();
    let changed: Vec<Entity> = w
        .query::<Entity, Or<(Changed<Pos>, Changed<Vel>)>>()
        .unwrap()
        .collect();
    assert_eq!(changed, vec![b]);
}

#[test]
fn par_for_each_visits_each_entity_once_and_matches_sequential() {
    let n = if cfg!(miri) { 30 } else { 50_000 };
    let mut w = World::new();
    for i in 0..n {
        if i % 3 == 0 {
            w.spawn((Pos(i as f32), Vel(2.0), Tag));
        } else {
            w.spawn((Pos(i as f32), Vel(2.0)));
        }
    }
    let visits = AtomicUsize::new(0);
    w.par_for_each::<(&mut Pos, &Vel), ()>(|(mut p, v)| {
        p.0 += v.0;
        visits.fetch_add(1, Ordering::Relaxed);
    })
    .unwrap();
    assert_eq!(visits.load(Ordering::Relaxed), n);
    let mut got: Vec<f32> = w.query::<&Pos, ()>().unwrap().map(|p| p.0).collect();
    got.sort_by(f32::total_cmp);
    let expect: Vec<f32> = (0..n).map(|i| i as f32 + 2.0).collect();
    assert_eq!(got, expect);
    assert!(w.par_for_each::<(&mut Pos, &mut Pos), ()>(|_| {}).is_err());
}

#[derive(Default)]
struct Counter(u32);

fn move_sys(mut q: canopy_ecs::Query<(&mut Pos, &Vel)>, mut c: canopy_ecs::ResMut<Counter>) {
    for (mut p, v) in q.iter_mut() {
        p.0 += v.0;
        c.0 += 1;
    }
}

fn read_sys(mut q: canopy_ecs::Query<&Pos, With<Vel>>, c: canopy_ecs::Res<Counter>) {
    assert!(q.iter_mut().count() == 10 && c.0 % 10 == 0);
}

fn aliasing_sys(_a: canopy_ecs::Query<&mut Pos>, _b: canopy_ecs::Query<&Pos>) {}

#[test]
fn function_systems_derive_access() {
    let mut w = World::new();
    w.insert_resource(Counter::default());
    for i in 0..10 {
        w.spawn((Pos(i as f32), Vel(1.0)));
    }
    let mut s = Schedule::new(Executor::MultiThreaded);
    s.add_fn(SystemBuilder::new("read"), read_sys).unwrap();
    s.add_fn(SystemBuilder::new("move"), move_sys).unwrap();
    // Conflict derived from params: unordered -> reported as ambiguity.
    assert_eq!(
        s.ambiguities().unwrap(),
        &[("read".to_string(), "move".to_string())]
    );
    for _ in 0..5 {
        s.run(&mut w).unwrap();
    }
    assert_eq!(w.resource::<Counter>().unwrap().0, 50);
    assert_eq!(
        w.query::<&Pos, ()>().unwrap().map(|p| p.0).sum::<f32>(),
        45.0 + 50.0
    );
    // Two params with conflicting access are rejected when the system is added.
    let mut s2 = Schedule::new(Executor::SingleThreaded);
    assert!(matches!(
        s2.add_fn(SystemBuilder::new("alias"), aliasing_sys).err(),
        Some(EcsError::ConflictingQueryAccess(_))
    ));
    // Missing resource surfaces as an error, not a panic.
    let mut w2 = World::new();
    let mut s3 = Schedule::new(Executor::SingleThreaded);
    s3.add_fn(SystemBuilder::new("move"), move_sys).unwrap();
    assert!(matches!(s3.run(&mut w2), Err(EcsError::MissingResource(_))));
}

fn spawner(mut c: canopy_ecs::Commands, mut ev: canopy_ecs::EventWriter<u32>) {
    c.spawn(Tag);
    ev.send(7);
}

#[test]
fn commands_event_writer_and_before() {
    let mut w = World::new();
    w.insert_resource(canopy_ecs::Events::<u32>::default());
    w.insert_resource(Vec::<&'static str>::new());
    let mut s = Schedule::new(Executor::MultiThreaded);
    s.add_system(
        SystemBuilder::new("late").res_write::<Vec<&'static str>>(),
        |ctx| {
            ctx.resource_mut::<Vec<&'static str>>()
                .unwrap()
                .unwrap()
                .push("late");
        },
    )
    .unwrap();
    s.add_system(
        SystemBuilder::new("early")
            .res_write::<Vec<&'static str>>()
            .before("late"),
        |ctx| {
            ctx.resource_mut::<Vec<&'static str>>()
                .unwrap()
                .unwrap()
                .push("early");
        },
    )
    .unwrap();
    s.add_fn(SystemBuilder::new("spawner"), spawner).unwrap();
    s.run(&mut w).unwrap();
    assert_eq!(
        w.resource::<Vec<&'static str>>().unwrap(),
        &vec!["early", "late"]
    );
    assert_eq!(w.query::<&Tag, ()>().unwrap().count(), 1);
    let mut cur = canopy_ecs::event::EventCursor::default();
    let got: Vec<u32> = w
        .resource::<canopy_ecs::Events<u32>>()
        .unwrap()
        .read(&mut cur)
        .copied()
        .collect();
    assert_eq!(got, vec![7]);
}

#[derive(Default)]
struct Seen(Vec<(u32, u32)>);

fn reader(
    mut runs: canopy_ecs::Local<u32>,
    mut ev: canopy_ecs::EventReader<u32>,
    mut seen: canopy_ecs::ResMut<Seen>,
) {
    *runs += 1;
    let r = *runs;
    for e in ev.read() {
        seen.0.push((r, *e));
    }
}

#[test]
fn local_and_event_reader_keep_per_system_state() {
    let mut w = World::new();
    w.insert_resource(canopy_ecs::Events::<u32>::default());
    w.insert_resource(Seen::default());
    let mut s = Schedule::new(Executor::MultiThreaded);
    s.add_fn(SystemBuilder::new("reader"), reader).unwrap();
    w.resource_mut::<canopy_ecs::Events<u32>>().unwrap().send(1);
    s.run(&mut w).unwrap();
    s.run(&mut w).unwrap(); // event already seen: not read again
    w.resource_mut::<canopy_ecs::Events<u32>>().unwrap().send(2);
    s.run(&mut w).unwrap();
    assert_eq!(w.resource::<Seen>().unwrap().0, vec![(1, 1), (3, 2)]);
}

// ---- Filtered (archetype-disjoint) access -------------------------------------

use canopy_ecs::query::{Access, FilteredAccess, SystemAccess};
use canopy_ecs::{Or, Query, Res};

type TagOrVel = Or<(With<Tag>, With<Vel>)>;
type TagOrNotTag = Or<(With<Tag>, Without<Tag>)>;

fn split_writers(
    mut tagged: Query<&mut Pos, With<Tag>>,
    mut untagged: Query<&mut Pos, Without<Tag>>,
) {
    // Both iterators are live at once: sound only because they are archetype-disjoint.
    let mut a = tagged.iter_mut();
    let mut b = untagged.iter_mut();
    loop {
        let (x, y) = (a.next(), b.next());
        if x.is_none() && y.is_none() {
            break;
        }
        if let Some(mut p) = x {
            p.0 += 100.0;
        }
        if let Some(mut p) = y {
            p.0 += 1.0;
        }
    }
}
fn data_implied_with(_a: Query<(&mut Pos, &Tag)>, _b: Query<&mut Pos, Without<Tag>>) {}
fn changed_implied_with(
    _a: Query<&mut Pos, canopy_ecs::Changed<Tag>>,
    _b: Query<&Pos, Without<Tag>>,
) {
}
fn overlap_two_withs(_a: Query<&mut Pos, With<Tag>>, _b: Query<&mut Pos, With<Vel>>) {}
fn overlap_or(_a: Query<&mut Pos, TagOrVel>, _b: Query<&Pos, Without<Tag>>) {}
fn overlap_or_complement(_a: Query<&mut Pos, TagOrNotTag>, _b: Query<&Pos, Without<Tag>>) {}
fn overlap_option(_a: Query<(&mut Pos, Option<&Tag>)>, _b: Query<&Pos, Without<Tag>>) {}
fn overlap_unfiltered(_a: Query<&mut Pos, With<Tag>>, _b: Query<&Pos>) {}
fn overlap_filter_on_other_type(_a: Query<&mut Pos, With<Tag>>, _b: Query<&mut Pos, Without<Vel>>) {
}

#[test]
fn disjoint_filters_let_params_share_a_component() {
    let mut w = World::new();
    let tagged: Vec<Entity> = (0..4).map(|i| w.spawn((Pos(i as f32), Tag))).collect();
    let plain: Vec<Entity> = (0..4).map(|i| w.spawn(Pos(i as f32))).collect();
    let mut s = Schedule::new(Executor::SingleThreaded);
    s.add_fn(SystemBuilder::new("split"), split_writers)
        .unwrap();
    s.add_fn(SystemBuilder::new("data"), data_implied_with)
        .unwrap();
    s.add_fn(SystemBuilder::new("changed"), changed_implied_with)
        .unwrap();
    s.run(&mut w).unwrap();
    for (i, e) in tagged.iter().enumerate() {
        assert_eq!(w.get::<Pos>(*e), Some(&Pos(i as f32 + 100.0)));
    }
    for (i, e) in plain.iter().enumerate() {
        assert_eq!(w.get::<Pos>(*e), Some(&Pos(i as f32 + 1.0)));
    }
}

#[test]
fn possibly_overlapping_params_still_conflict() {
    macro_rules! rejected {
        ($f:expr) => {{
            let mut s = Schedule::new(Executor::SingleThreaded);
            assert!(
                matches!(
                    s.add_fn(SystemBuilder::new(stringify!($f)), $f).err(),
                    Some(EcsError::ConflictingQueryAccess(_))
                ),
                "{} must be rejected",
                stringify!($f)
            );
        }};
    }
    rejected!(overlap_two_withs);
    rejected!(overlap_or);
    rejected!(overlap_or_complement);
    rejected!(overlap_option);
    rejected!(overlap_unfiltered);
    rejected!(overlap_filter_on_other_type);
    // Builder-declared access is unfiltered, so it overlaps both filtered params.
    let mut s = Schedule::new(Executor::SingleThreaded);
    assert!(matches!(
        s.add_fn(SystemBuilder::new("b").read::<Pos>(), split_writers)
            .err(),
        Some(EcsError::ConflictingQueryAccess(_))
    ));
}

#[test]
fn filtered_access_is_disjoint_only_on_with_vs_without() {
    let fa = |r: Result<FilteredAccess, EcsError>| r.unwrap();
    let with_tag = fa(FilteredAccess::of::<&mut Pos, With<Tag>>());
    let without_tag = fa(FilteredAccess::of::<&mut Pos, Without<Tag>>());
    let with_vel = fa(FilteredAccess::of::<&mut Pos, With<Vel>>());
    let or = fa(FilteredAccess::of::<&mut Pos, Or<(With<Tag>, With<Vel>)>>());
    let opt = fa(FilteredAccess::of::<(&mut Pos, Option<&Tag>), ()>());
    assert!(with_tag.is_disjoint(&without_tag) && without_tag.is_disjoint(&with_tag));
    assert!(!with_tag.conflicts(&without_tag));
    assert!(!with_tag.is_disjoint(&with_vel) && with_tag.conflicts(&with_vel));
    assert!(!or.is_disjoint(&without_tag) && or.conflicts(&without_tag));
    assert!(!opt.is_disjoint(&without_tag) && opt.conflicts(&without_tag));
    // Filtered entries never licence ad-hoc (unfiltered) context queries.
    let mut set = SystemAccess::default();
    set.add(with_tag).unwrap();
    set.add(without_tag).unwrap();
    let mut need = Access::default();
    need.add_write::<Pos>().unwrap();
    assert!(set.covers(&need).is_err());
    assert!(SystemAccess::from_unfiltered(need.clone())
        .covers(&need)
        .is_ok());
}

fn sys_tagged(mut q: Query<&mut Pos, With<Tag>>) {
    q.iter_mut().for_each(|mut p| p.0 += 1.0);
}
fn sys_untagged(mut q: Query<&mut Pos, Without<Tag>>) {
    q.iter_mut().for_each(|mut p| p.0 += 1.0);
}
fn sys_all(mut q: Query<&Pos>) {
    q.iter_mut().for_each(|p| assert!(p.0.is_finite()));
}
fn sys_or(mut q: Query<&mut Pos, TagOrVel>) {
    q.iter_mut().for_each(|mut p| p.0 += 0.5);
}

#[test]
fn scheduler_orders_overlapping_but_not_disjoint_systems() {
    let mut s = Schedule::new(Executor::MultiThreaded);
    s.add_fn(SystemBuilder::new("tagged"), sys_tagged).unwrap();
    s.add_fn(SystemBuilder::new("untagged"), sys_untagged)
        .unwrap();
    s.add_fn(SystemBuilder::new("all"), sys_all).unwrap();
    s.add_fn(SystemBuilder::new("or"), sys_or).unwrap();
    let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
    assert_eq!(
        s.ambiguities().unwrap(),
        &[
            pair("tagged", "all"),
            pair("untagged", "all"),
            pair("tagged", "or"),
            pair("untagged", "or"),
            pair("all", "or"),
        ]
    );
    let mut w = World::new();
    w.spawn((Pos(0.0), Tag));
    w.spawn(Pos(0.0));
    s.run(&mut w).unwrap();
    let mut got: Vec<f32> = w.query::<&Pos, ()>().unwrap().map(|p| p.0).collect();
    got.sort_by(f32::total_cmp);
    assert_eq!(got, vec![1.0, 1.5]); // `or` (With<Tag>) ran after `tagged`
}

/// Two systems writing the same component type behind `With<Tag>`/`Without<Tag>`
/// must run concurrently (rendezvous) and write on both sides of the rendezvous.
/// Under Miri the data-race detector checks those writes never touch the same memory.
struct Meet {
    arrived: AtomicUsize,
    met: AtomicUsize,
}
fn rendezvous(m: &Meet) {
    m.arrived.fetch_add(1, Ordering::SeqCst);
    let start = std::time::Instant::now();
    while m.arrived.load(Ordering::SeqCst) < 2 && start.elapsed().as_secs() < 5 {
        std::thread::yield_now();
    }
    if m.arrived.load(Ordering::SeqCst) >= 2 {
        m.met.fetch_add(1, Ordering::SeqCst);
    }
}
fn race_tagged(mut q: Query<&mut Pos, With<Tag>>, m: Res<Meet>) {
    q.iter_mut().for_each(|mut p| p.0 += 1.0);
    rendezvous(&m);
    q.iter_mut().for_each(|mut p| p.0 += 1.0);
}
fn race_untagged(mut q: Query<&mut Pos, Without<Tag>>, m: Res<Meet>) {
    q.iter_mut().for_each(|mut p| p.0 += 10.0);
    rendezvous(&m);
    q.iter_mut().for_each(|mut p| p.0 += 10.0);
}

#[test]
fn disjoint_writers_run_concurrently_without_racing() {
    let n = if cfg!(miri) { 6 } else { 200 };
    let mut w = World::new();
    w.insert_resource(Meet {
        arrived: AtomicUsize::new(0),
        met: AtomicUsize::new(0),
    });
    for i in 0..n {
        if i % 2 == 0 {
            w.spawn((Pos(0.0), Tag));
        } else {
            w.spawn((Pos(0.0), Vel(0.0)));
        }
    }
    let mut s = Schedule::new(Executor::MultiThreaded);
    s.add_fn(SystemBuilder::new("t"), race_tagged).unwrap();
    s.add_fn(SystemBuilder::new("u"), race_untagged).unwrap();
    assert!(s.ambiguities().unwrap().is_empty());
    s.run(&mut w).unwrap();
    assert_eq!(w.resource::<Meet>().unwrap().met.load(Ordering::SeqCst), 2);
    let sum: f32 = w.query::<&Pos, ()>().unwrap().map(|p| p.0).sum();
    assert_eq!(sum, (n / 2) as f32 * 2.0 + (n / 2) as f32 * 20.0);
}

// ---- System sets, set ordering and run conditions ------------------------------

use canopy_ecs::schedule::{resource_exists, run_once};
use canopy_ecs::set::intern;
use canopy_ecs::{SetConfig, SystemSet};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Phase {
    Input,
    Physics,
    Render,
}
impl SystemSet for Phase {}
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Outer;
impl SystemSet for Outer {}
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Inner;
impl SystemSet for Inner {}

type Order = std::sync::Mutex<Vec<&'static str>>;

/// A system that records its name. Every one reads (not writes) the log resource,
/// so none of them conflict: any ordering seen comes from set/system edges alone.
fn push(s: &mut Schedule, b: SystemBuilder, name: &'static str) {
    s.add_system(b.res_read::<Order>(), move |ctx| {
        ctx.resource::<Order>()
            .unwrap()
            .unwrap()
            .lock()
            .unwrap()
            .push(name);
    })
    .unwrap();
}

fn run_log(s: &mut Schedule, w: &mut World) -> Vec<&'static str> {
    w.insert_resource(Order::default());
    s.run(w).unwrap();
    w.remove_resource::<Order>().unwrap().into_inner().unwrap()
}

fn pos_of(log: &[&str], n: &str) -> usize {
    log.iter().position(|x| *x == n).unwrap()
}

#[test]
fn set_labels_intern_by_type_and_value() {
    assert_eq!(intern(Phase::Input), intern(Phase::Input));
    assert_ne!(intern(Phase::Input), intern(Phase::Physics));
    // Distinct zero-sized label types must stay distinct (a leaked ZST has no unique
    // address, so identity cannot be the pointer).
    assert_ne!(intern(Outer), intern(Inner));
    assert_eq!(format!("{:?}", intern(Phase::Render)), "Render");
}

#[test]
fn sets_order_their_members() {
    for executor in [Executor::SingleThreaded, Executor::MultiThreaded] {
        let mut s = Schedule::new(executor);
        // Inserted in reverse; only set edges make the order.
        push(
            &mut s,
            SystemBuilder::new("render").in_set(Phase::Render),
            "render",
        );
        push(
            &mut s,
            SystemBuilder::new("collide").in_set(Phase::Physics),
            "collide",
        );
        push(
            &mut s,
            SystemBuilder::new("integrate").in_set(Phase::Physics),
            "integrate",
        );
        push(
            &mut s,
            SystemBuilder::new("input").in_set(Phase::Input),
            "input",
        );
        // A system ordered against a whole set, and a set ordered against a system.
        push(
            &mut s,
            SystemBuilder::new("hud").after(Phase::Physics),
            "hud",
        );
        push(&mut s, SystemBuilder::new("clear"), "clear");
        s.configure_set(SetConfig::new(Phase::Physics).after(Phase::Input))
            .configure_set(SetConfig::new(Phase::Render).after(Phase::Physics))
            .configure_set(SetConfig::new(Phase::Input).after("clear"));
        assert!(s.ambiguities().unwrap().is_empty());
        let mut w = World::new();
        for _ in 0..3 {
            let log = run_log(&mut s, &mut w);
            assert_eq!(log.len(), 6);
            let p = |n| pos_of(&log, n);
            assert!(p("clear") < p("input"));
            assert!(p("input") < p("collide") && p("input") < p("integrate"));
            assert!(p("collide") < p("render") && p("integrate") < p("render"));
            assert!(p("collide") < p("hud") && p("integrate") < p("hud"));
        }
    }
}

#[test]
fn nested_sets_inherit_parent_ordering_and_before_works() {
    let mut s = Schedule::new(Executor::MultiThreaded);
    push(&mut s, SystemBuilder::new("inner").in_set(Inner), "inner");
    push(&mut s, SystemBuilder::new("first"), "first");
    push(&mut s, SystemBuilder::new("early").before(Outer), "early");
    s.configure_set(SetConfig::new(Inner).in_set(Outer));
    s.configure_set(SetConfig::new(Outer).after("first"));
    let mut w = World::new();
    let log = run_log(&mut s, &mut w);
    assert!(pos_of(&log, "first") < pos_of(&log, "inner"));
    assert!(pos_of(&log, "early") < pos_of(&log, "inner"));
}

#[test]
fn set_ordering_cycle_is_an_error() {
    let mut s = Schedule::new(Executor::SingleThreaded);
    push(&mut s, SystemBuilder::new("a").in_set(Phase::Input), "a");
    push(&mut s, SystemBuilder::new("b").in_set(Phase::Physics), "b");
    s.configure_set(SetConfig::new(Phase::Physics).after(Phase::Input));
    s.configure_set(SetConfig::new(Phase::Input).after(Phase::Physics));
    let mut w = World::new();
    assert_eq!(s.run(&mut w), Err(EcsError::OrderingCycle));
}

struct Flag;

#[test]
fn run_conditions_gate_systems_and_sets() {
    let mut s = Schedule::new(Executor::MultiThreaded);
    let set_evals = Arc::new(AtomicUsize::new(0));
    let se = set_evals.clone();
    push(
        &mut s,
        SystemBuilder::new("needs_flag").run_if(resource_exists::<Flag>()),
        "needs_flag",
    );
    push(
        &mut s,
        SystemBuilder::new("once").run_if(run_once()),
        "once",
    );
    push(&mut s, SystemBuilder::new("r1").in_set(Phase::Render), "r1");
    push(&mut s, SystemBuilder::new("r2").in_set(Phase::Render), "r2");
    push(
        &mut s,
        SystemBuilder::new("after_r").after(Phase::Render),
        "after_r",
    );
    s.configure_set(SetConfig::new(Phase::Render).run_if(move |w: &World| {
        se.fetch_add(1, Ordering::SeqCst);
        w.resource::<Flag>().is_some()
    }));
    let mut w = World::new();
    let sorted = |mut v: Vec<&'static str>| {
        v.sort();
        v
    };
    // No flag: gated systems skipped; a skipped set still releases `after_r`.
    assert_eq!(sorted(run_log(&mut s, &mut w)), vec!["after_r", "once"]);
    w.insert_resource(Flag);
    assert_eq!(
        sorted(run_log(&mut s, &mut w)),
        vec!["after_r", "needs_flag", "r1", "r2"]
    );
    // The set condition ran once per schedule run, not once per member.
    assert_eq!(set_evals.load(Ordering::SeqCst), 2);
}

#[test]
fn run_condition_sees_writes_of_systems_ordered_before_it() {
    let mut s = Schedule::new(Executor::MultiThreaded);
    s.add_system(
        SystemBuilder::new("set_flag").res_write::<Counter>(),
        |ctx| {
            ctx.resource_mut::<Counter>().unwrap().unwrap().0 = 1;
        },
    )
    .unwrap();
    let ran = Arc::new(AtomicUsize::new(0));
    let r = ran.clone();
    s.add_system(
        SystemBuilder::new("gated")
            .after("set_flag")
            .run_if(|w: &World| w.resource::<Counter>().is_some_and(|c| c.0 == 1)),
        move |_| {
            r.fetch_add(1, Ordering::SeqCst);
        },
    )
    .unwrap();
    let mut w = World::new();
    w.insert_resource(Counter(0));
    s.run(&mut w).unwrap();
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}

// ---- Ready-queue executor -------------------------------------------------------

/// `c` depends only on `a`, so it must start as soon as `a` finishes, while the
/// unrelated `b` is still running. `b` waits (up to 5 s) for `c`'s flag; a per-wave
/// barrier would run `c` only after `b` gave up.
#[test]
fn ready_system_starts_while_an_unrelated_system_still_runs() {
    use std::sync::atomic::AtomicBool;
    let flag = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(AtomicBool::new(false));
    let mut s = Schedule::new(Executor::MultiThreaded);
    s.add_system(SystemBuilder::new("a"), |_| {}).unwrap();
    let (f, sn) = (flag.clone(), seen.clone());
    s.add_system(SystemBuilder::new("b"), move |_| {
        let start = std::time::Instant::now();
        while !f.load(Ordering::SeqCst) && start.elapsed().as_secs() < 5 {
            std::thread::yield_now();
        }
        sn.store(f.load(Ordering::SeqCst), Ordering::SeqCst);
    })
    .unwrap();
    let f = flag.clone();
    s.add_system(SystemBuilder::new("c").after("a"), move |_| {
        f.store(true, Ordering::SeqCst)
    })
    .unwrap();
    s.run(&mut World::new()).unwrap();
    assert!(seen.load(Ordering::SeqCst));
}

/// Run conditions (a system's own and a set's) are evaluated only while no system
/// of the schedule is running: `a` returns only once `slow` is running, so when
/// `gated`/`in_set` become ready `slow` is in flight and their conditions must wait.
#[test]
fn run_conditions_are_never_evaluated_while_a_system_runs() {
    let running = Arc::new(AtomicUsize::new(0));
    let seen_running = Arc::new(AtomicUsize::new(0));
    let ran = Arc::new(AtomicUsize::new(0));
    let mut s = Schedule::new(Executor::MultiThreaded);
    let r = running.clone();
    s.add_system(SystemBuilder::new("a"), move |_| {
        let start = std::time::Instant::now();
        while r.load(Ordering::SeqCst) == 0 && start.elapsed().as_secs() < 5 {
            std::thread::yield_now();
        }
    })
    .unwrap();
    let r = running.clone();
    s.add_system(SystemBuilder::new("slow"), move |_| {
        r.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(50));
        r.fetch_sub(1, Ordering::SeqCst);
    })
    .unwrap();
    let probe = |running: &Arc<AtomicUsize>, seen: &Arc<AtomicUsize>| {
        let (r, sn) = (running.clone(), seen.clone());
        move |_: &World| {
            sn.fetch_add(r.load(Ordering::SeqCst), Ordering::SeqCst);
            true
        }
    };
    let rn = ran.clone();
    s.add_system(
        SystemBuilder::new("gated")
            .after("a")
            .run_if(probe(&running, &seen_running)),
        move |_| {
            rn.fetch_add(1, Ordering::SeqCst);
        },
    )
    .unwrap();
    let rn = ran.clone();
    s.add_system(
        SystemBuilder::new("in_set").after("a").in_set(Phase::Input),
        move |_| {
            rn.fetch_add(1, Ordering::SeqCst);
        },
    )
    .unwrap();
    s.configure_set(SetConfig::new(Phase::Input).run_if(probe(&running, &seen_running)));
    s.run(&mut World::new()).unwrap();
    assert_eq!(ran.load(Ordering::SeqCst), 2);
    assert_eq!(seen_running.load(Ordering::SeqCst), 0);
}

/// Several failing systems: the error of the lowest system index is returned,
/// whichever finished first.
#[test]
fn first_error_is_by_system_index_not_completion_order() {
    let mut s = Schedule::new(Executor::MultiThreaded);
    s.add_system(SystemBuilder::new("late"), |_| {
        std::thread::sleep(std::time::Duration::from_millis(30));
        panic!("late");
    })
    .unwrap();
    s.add_system(SystemBuilder::new("early"), |_| panic!("early"))
        .unwrap();
    assert_eq!(
        s.run(&mut World::new()),
        Err(EcsError::SystemPanicked("late".into()))
    );
}

#[test]
fn panicking_run_condition_skips_the_system_and_reports() {
    let mut s = Schedule::new(Executor::SingleThreaded);
    let ran = Arc::new(AtomicUsize::new(0));
    let r = ran.clone();
    s.add_system(
        SystemBuilder::new("x").run_if(|_: &World| panic!("cond")),
        move |_| {
            r.fetch_add(1, Ordering::SeqCst);
        },
    )
    .unwrap();
    let mut w = World::new();
    assert!(matches!(s.run(&mut w), Err(EcsError::SystemPanicked(_))));
    assert_eq!(ran.load(Ordering::SeqCst), 0);
}

// ---- Change-tick clamping (check_change_ticks) ---------------------------------

fn count_changed(
    mut q: Query<Entity, canopy_ecs::Changed<Pos>>,
    mut hits: canopy_ecs::ResMut<Hits>,
) {
    hits.0 += q.iter_mut().count() as u32;
}

/// Simulates more than 2^32 ticks (steps of 2^28 via `set_change_tick`, one schedule
/// run per step). Without clamping, the component's tick from the start wraps around
/// and reads as "changed" again once the world tick laps it.
#[test]
fn change_ticks_are_clamped_across_a_full_u32_wrap() {
    let mut w = World::new();
    w.insert_resource(Hits::default());
    let e = w.spawn(Pos(0.0));
    let mut s = Schedule::new(Executor::SingleThreaded);
    s.add_fn(SystemBuilder::new("count"), count_changed)
        .unwrap();
    s.run(&mut w).unwrap();
    assert_eq!(w.resource::<Hits>().unwrap().0, 1, "spawn is a change");
    let step = 1u32 << 28;
    let steps = (1u64 << 32) / u64::from(step) + 4; // 20 steps: over a full lap
    for _ in 0..steps {
        let t = w.change_tick().0.wrapping_add(step);
        w.set_change_tick(t);
        s.run(&mut w).unwrap();
        assert_eq!(w.resource::<Hits>().unwrap().0, 1, "spurious change");
        // World-level change filters stay correct too.
        assert_eq!(
            w.query::<Entity, canopy_ecs::Changed<Pos>>()
                .unwrap()
                .count(),
            0
        );
    }
    // A real change after all that is still detected, exactly once.
    w.get_mut::<Pos>(e).unwrap().0 = 1.0;
    s.run(&mut w).unwrap();
    s.run(&mut w).unwrap();
    assert_eq!(w.resource::<Hits>().unwrap().0, 2);
}

/// A schedule that sits idle while the world advances `3 * 2^30` ticks: the world
/// clamps the component's tick, so the idle system's own `last_run` must be clamped
/// too, or the component reads as changed when the schedule finally runs.
#[test]
fn idle_schedule_last_run_is_clamped_too() {
    let mut w = World::new();
    w.insert_resource(Hits::default());
    let e = w.spawn(Pos(0.0));
    let mut rare = Schedule::new(Executor::SingleThreaded);
    rare.add_fn(SystemBuilder::new("count"), count_changed)
        .unwrap();
    rare.run(&mut w).unwrap();
    assert_eq!(w.resource::<Hits>().unwrap().0, 1, "spawn is a change");
    let step = 1u32 << 28;
    for _ in 0..12 {
        let t = w.change_tick().0.wrapping_add(step);
        w.set_change_tick(t);
        w.check_change_ticks();
    }
    rare.run(&mut w).unwrap();
    assert_eq!(
        w.resource::<Hits>().unwrap().0,
        1,
        "spurious change after idling"
    );
    w.get_mut::<Pos>(e).unwrap().0 = 1.0;
    rare.run(&mut w).unwrap();
    assert_eq!(w.resource::<Hits>().unwrap().0, 2, "real change still seen");
}

#[test]
fn check_tick_clamps_only_old_ticks() {
    use canopy_ecs::world::MAX_CHANGE_AGE;
    let now = Tick(5);
    let mut young = Tick(5u32.wrapping_sub(MAX_CHANGE_AGE));
    assert!(!young.check_tick(now));
    let mut old = Tick(5u32.wrapping_sub(MAX_CHANGE_AGE + 1));
    assert!(old.check_tick(now));
    assert_eq!(old, young);
}

// ---- Mut<T>: change detection on actual write ----------------------------------

#[test]
fn mut_marks_changed_only_on_actual_write() {
    let mut w = World::new();
    let a = w.spawn(Pos(0.0));
    let b = w.spawn(Pos(0.0));
    w.clear_trackers();
    // Reading through a `&mut` query does not mark anything changed.
    let sum: f32 = w.query::<&mut Pos, ()>().unwrap().map(|p| p.0).sum();
    assert_eq!(sum, 0.0);
    assert_eq!(w.query::<Entity, Changed<Pos>>().unwrap().count(), 0);
    // Writing one entity marks exactly that one.
    for (e, mut p) in w.query::<(Entity, &mut Pos), ()>().unwrap() {
        if e == b {
            p.0 = 1.0;
        }
    }
    let changed: Vec<Entity> = w.query::<Entity, Changed<Pos>>().unwrap().collect();
    assert_eq!(changed, vec![b]);
    // Bypass writes without marking; set_changed marks without writing.
    w.clear_trackers();
    for (e, mut p) in w.query::<(Entity, &mut Pos), ()>().unwrap() {
        if e == a {
            p.bypass_change_detection().0 = 5.0;
        } else {
            p.set_changed();
        }
    }
    let changed: Vec<Entity> = w.query::<Entity, Changed<Pos>>().unwrap().collect();
    assert_eq!(changed, vec![b]);
    assert_eq!(w.get::<Pos>(a), Some(&Pos(5.0)));
    // `into_inner` hands out a plain `&mut` and marks changed.
    w.clear_trackers();
    if let Ok(Some(p)) = w.query_one::<&mut Pos>(a) {
        p.into_inner().0 = 6.0;
    }
    let changed: Vec<Entity> = w.query::<Entity, Changed<Pos>>().unwrap().collect();
    assert_eq!(changed, vec![a]);
}

fn read_through_mut(mut q: Query<&mut Pos>) {
    for p in q.iter_mut() {
        assert!(p.0.is_finite());
    }
}
fn observe_changed(
    mut q: Query<Entity, canopy_ecs::Changed<Pos>>,
    mut hits: canopy_ecs::ResMut<Hits>,
) {
    hits.0 += q.iter_mut().count() as u32;
}

#[test]
fn reading_through_a_mut_query_param_is_not_a_change() {
    let mut w = World::new();
    w.insert_resource(Hits::default());
    for _ in 0..3 {
        w.spawn(Pos(0.0));
    }
    let mut s = Schedule::new(Executor::SingleThreaded);
    // `read` runs first each run, so a mark it made would be seen by `observe`.
    s.add_fn(SystemBuilder::new("read"), read_through_mut)
        .unwrap();
    s.add_fn(SystemBuilder::new("observe"), observe_changed)
        .unwrap();
    s.run(&mut w).unwrap(); // spawns are changes: 3
    s.run(&mut w).unwrap();
    s.run(&mut w).unwrap();
    assert_eq!(w.resource::<Hits>().unwrap().0, 3);
}

// ---- More than four parameters -------------------------------------------------

#[derive(Default)]
struct Hits(u32);
fn five(
    _a: Query<&Pos>,
    _b: Query<&Vel>,
    _c: canopy_ecs::Local<u8>,
    _d: canopy_ecs::Commands,
    mut e: canopy_ecs::ResMut<Hits>,
) {
    e.0 += 1;
}
#[allow(clippy::too_many_arguments)]
fn twelve(
    mut tagged: Query<&mut Pos, With<Tag>>,
    mut untagged: Query<&mut Pos, Without<Tag>>,
    vel: Query<&Vel>,
    ent: Query<Entity>,
    counter: Res<Counter>,
    mut hits: canopy_ecs::ResMut<Hits>,
    mut l1: canopy_ecs::Local<u32>,
    mut l2: canopy_ecs::Local<u64>,
    mut cmd: canopy_ecs::Commands,
    mut ev: canopy_ecs::EventWriter<u32>,
    mut rd: canopy_ecs::EventReader<u8>,
    _seen: Res<Seen>,
) {
    let _ = (vel, ent, counter.0);
    *l1 += 1;
    *l2 += 2;
    hits.0 += tagged.iter_mut().count() as u32 * 10 + untagged.iter_mut().count() as u32;
    ev.send(*l1 + rd.read().count() as u32);
    if *l1 == 1 {
        cmd.spawn(Vel(0.0));
    }
}

#[test]
fn function_systems_take_up_to_twelve_params() {
    let mut w = World::new();
    w.insert_resource(Counter::default());
    w.insert_resource(Hits::default());
    w.insert_resource(Seen::default());
    w.insert_resource(canopy_ecs::Events::<u32>::default());
    w.insert_resource(canopy_ecs::Events::<u8>::default());
    w.spawn((Pos(0.0), Tag));
    w.spawn(Pos(0.0));
    w.spawn(Pos(0.0));
    let mut s = Schedule::new(Executor::MultiThreaded);
    s.add_fn(SystemBuilder::new("twelve"), twelve).unwrap();
    s.add_fn(SystemBuilder::new("five"), five).unwrap();
    s.run(&mut w).unwrap();
    s.run(&mut w).unwrap();
    // twelve: 1 tagged x 10 + 2 untagged = 12 per run; five: 1 per run.
    assert_eq!(w.resource::<Hits>().unwrap().0, 26);
    assert_eq!(w.query::<&Vel, ()>().unwrap().count(), 1);
}

mod query_state {
    use super::*;
    use canopy_ecs::{Query, QueryFilter, QueryState};
    use std::cell::Cell;

    thread_local! {
        static MATCH_CALLS: Cell<usize> = const { Cell::new(0) };
    }

    /// Always matches, but counts how many archetypes were tested.
    struct Counting;
    // SAFETY: records nothing, matches everything, hands out no references.
    unsafe impl QueryFilter for Counting {
        type Fetch<'w> = ();
        fn access(_: &mut canopy_ecs::query::Access) -> Result<(), EcsError> {
            Ok(())
        }
        fn matches(_: &World, _: &canopy_ecs::world::Archetype) -> bool {
            MATCH_CALLS.with(|c| c.set(c.get() + 1));
            true
        }
        unsafe fn prepare<'w>(_: &'w World, _: &'w canopy_ecs::world::Archetype, _: Tick, _: Tick) {
        }
        unsafe fn filter(_: &mut (), _: usize) -> bool {
            true
        }
    }

    fn calls() -> usize {
        MATCH_CALLS.with(Cell::get)
    }

    #[test]
    fn only_new_archetypes_are_tested() {
        let mut w = World::new();
        w.spawn(Pos(1.0));
        w.spawn((Pos(2.0), Vel(1.0)));
        let mut st = QueryState::<Entity, Counting>::new();
        let before = calls();
        assert_eq!(st.iter(&mut w).unwrap().count(), 2);
        let n = w.archetypes().len();
        assert_eq!(calls() - before, n, "first use tests every archetype once");
        assert_eq!(st.cache().archetypes_seen(), n);
        // No new archetype: nothing is tested again.
        let before = calls();
        for _ in 0..5 {
            assert_eq!(st.iter(&mut w).unwrap().count(), 2);
        }
        assert_eq!(calls() - before, 0, "no per-query archetype scan");
        // A new archetype is tested exactly once, and its rows are found.
        w.spawn((Pos(3.0), Tag));
        let before = calls();
        assert_eq!(st.iter(&mut w).unwrap().count(), 3);
        assert_eq!(st.iter(&mut w).unwrap().count(), 3);
        assert_eq!(calls() - before, 1);
        assert_eq!(st.cache().archetypes_seen(), w.archetypes().len());
        // The uncached query scans every archetype each call.
        let before = calls();
        let _ = w.query::<Entity, Counting>().unwrap().count();
        assert_eq!(calls() - before, w.archetypes().len());
    }

    #[test]
    fn matches_world_query_with_filters_and_empty_archetypes() {
        let mut w = World::new();
        let es: Vec<_> = (0..N).map(|i| w.spawn((Pos(i as f32), Vel(1.0)))).collect();
        let mut st = QueryState::<&Pos, (With<Vel>, Without<Tag>)>::new();
        assert_eq!(st.iter(&mut w).unwrap().count(), N);
        for &e in es.iter().step_by(2) {
            w.insert(e, Tag).unwrap();
        }
        let want: Vec<f32> = w
            .query::<&Pos, (With<Vel>, Without<Tag>)>()
            .unwrap()
            .map(|p| p.0)
            .collect();
        let got: Vec<f32> = st.iter(&mut w).unwrap().map(|p| p.0).collect();
        assert_eq!(got, want);
        assert_eq!(got.len(), N / 2);
        // Emptying a matched archetype is fine.
        for &e in es.iter().skip(1).step_by(2) {
            w.despawn(e);
        }
        assert_eq!(st.iter(&mut w).unwrap().count(), 0);
    }

    #[test]
    fn state_rebuilds_for_another_world() {
        let mut a = World::new();
        a.spawn(Pos(1.0));
        let mut st = QueryState::<&Pos, ()>::new();
        assert_eq!(st.iter(&mut a).unwrap().count(), 1);
        // Same archetype count, different layout: a stale cache would pick wrong ids.
        let mut b = World::new();
        b.spawn(Vel(1.0));
        b.spawn((Pos(1.0), Vel(1.0)));
        b.spawn((Pos(2.0), Vel(1.0)));
        assert_eq!(st.iter(&mut b).unwrap().count(), 2);
    }

    #[derive(Default)]
    struct Total(usize);

    fn count_sys(mut q: Query<&Pos>, mut t: canopy_ecs::ResMut<Total>) {
        t.0 += q.iter_mut().count();
    }

    #[test]
    fn function_system_sees_archetypes_created_between_runs() {
        let mut w = World::new();
        w.insert_resource(Total::default());
        w.spawn(Pos(0.0));
        let mut s = Schedule::new(Executor::SingleThreaded);
        s.add_fn(SystemBuilder::new("count"), count_sys).unwrap();
        s.run(&mut w).unwrap();
        assert_eq!(w.resource::<Total>().unwrap().0, 1);
        w.spawn((Pos(0.0), Tag));
        w.spawn((Pos(0.0), Vel(0.0)));
        s.run(&mut w).unwrap();
        assert_eq!(w.resource::<Total>().unwrap().0, 1 + 3);
    }
}

// ---- Sparse-set storage ---------------------------------------------------------

mod sparse {
    use super::*;
    use canopy_ecs::{Added, Mut, QueryState, StorageType};

    #[derive(Debug, PartialEq, Clone)]
    struct Sp(u32);
    impl canopy_ecs::Component for Sp {
        const STORAGE: StorageType = StorageType::SparseSet;
    }

    /// Sparse component with a destructor and heap data (so Miri sees leaks and
    /// double frees); counts drops.
    struct SpDrop(Arc<AtomicUsize>, #[allow(dead_code)] Box<u32>);
    impl canopy_ecs::Component for SpDrop {
        const STORAGE: StorageType = StorageType::SparseSet;
    }
    impl Drop for SpDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn sp_id(w: &World) -> usize {
        w.component_id(std::any::TypeId::of::<Sp>()).unwrap()
    }

    fn set_len(w: &World) -> usize {
        w.sparse_set(sp_id(w)).unwrap().len()
    }

    #[test]
    fn insert_get_replace_remove_despawn() {
        let mut w = World::new();
        let e1 = w.spawn((Pos(1.0), Sp(1)));
        let e2 = w.spawn(Pos(2.0));
        w.insert(e2, Sp(2)).unwrap();
        let id = sp_id(&w);
        assert_eq!(w.storage_type(id), Some(StorageType::SparseSet));
        let pos = w.register::<Pos>();
        assert_eq!(w.storage_type(pos), Some(StorageType::Table));
        assert!(w.sparse_set(pos).is_none());
        // Membership is tracked by archetypes, but no archetype has an `Sp` column.
        assert!(w.archetypes().iter().any(|a| a.contains(id)));
        assert!(w.archetypes().iter().all(|a| a.column_of(id).is_none()));
        assert_eq!(w.get::<Sp>(e1), Some(&Sp(1)));
        assert_eq!(w.get::<Sp>(e2), Some(&Sp(2)));
        assert_eq!(set_len(&w), 2);

        w.insert(e1, Sp(10)).unwrap(); // replace in place
        assert_eq!(w.get::<Sp>(e1), Some(&Sp(10)));
        assert_eq!(set_len(&w), 2);
        w.get_mut::<Sp>(e2).unwrap().0 = 20;
        assert_eq!(w.get::<Sp>(e2), Some(&Sp(20)));

        assert_eq!(w.remove::<Sp>(e1), Ok(true));
        assert_eq!(w.remove::<Sp>(e1), Ok(false));
        assert_eq!(w.get::<Sp>(e1), None);
        assert_eq!(w.get::<Pos>(e1), Some(&Pos(1.0)));
        assert_eq!(set_len(&w), 1);
        assert!(w.sparse_set(id).unwrap().contains(e2));

        assert!(w.despawn(e2));
        assert_eq!(set_len(&w), 0);
        // The reused slot index carries no stale sparse value.
        let e3 = w.spawn(Pos(3.0));
        assert_eq!(e3.index(), e2.index());
        assert_eq!(w.get::<Sp>(e3), None);
        assert_eq!(w.get::<Sp>(e2), None);
        assert!(!w.sparse_set(id).unwrap().contains(e2));
        // Same index, new generation: the set tells the two ids apart.
        w.insert(e3, Sp(30)).unwrap();
        assert!(w.sparse_set(id).unwrap().contains(e3));
        assert!(!w.sparse_set(id).unwrap().contains(e2));
        assert_eq!(w.get::<Sp>(e3), Some(&Sp(30)));
    }

    #[test]
    fn dense_slots_follow_swap_removes() {
        let mut w = World::new();
        let n: u32 = if cfg!(miri) { 24 } else { 500 };
        let es: Vec<Entity> = (0..n)
            .map(|i| {
                if i % 2 == 0 {
                    w.spawn((Pos(i as f32), Sp(i)))
                } else {
                    w.spawn((Sp(i), Vel(0.0)))
                }
            })
            .collect();
        let mut expect: std::collections::HashMap<Entity, u32> =
            es.iter().copied().zip(0..).collect();
        for (i, &e) in es.iter().enumerate() {
            if i % 3 == 0 {
                assert!(w.despawn(e));
                expect.remove(&e);
            } else if i % 5 == 0 {
                assert_eq!(w.remove::<Sp>(e), Ok(true));
                expect.remove(&e);
            }
        }
        assert_eq!(set_len(&w), expect.len());
        for (&e, &v) in &expect {
            assert_eq!(w.get::<Sp>(e), Some(&Sp(v)));
        }
        let mut got: Vec<(Entity, u32)> = w
            .query::<(Entity, &Sp), ()>()
            .unwrap()
            .map(|(e, s)| (e, s.0))
            .collect();
        got.sort();
        let mut want: Vec<(Entity, u32)> = expect.into_iter().collect();
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn values_dropped_exactly_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let made;
        {
            let mut w = World::new();
            let mk = |d: &Arc<AtomicUsize>, v: u32| SpDrop(d.clone(), Box::new(v));
            let n: u32 = if cfg!(miri) { 12 } else { 200 };
            let es: Vec<Entity> = (0..n).map(|i| w.spawn((Pos(0.0), mk(&drops, i)))).collect();
            let mut count = n as usize;
            for (i, &e) in es.iter().enumerate() {
                match i % 4 {
                    0 => {
                        w.insert(e, mk(&drops, 99)).unwrap(); // replacing drops the old one
                        count += 1;
                    }
                    1 => assert_eq!(w.remove::<SpDrop>(e), Ok(true)),
                    2 => assert!(w.despawn(e)),
                    _ => {
                        // Archetype moves keep the sparse value alive and in place.
                        w.insert(e, Tag).unwrap();
                        w.remove::<Pos>(e).unwrap();
                    }
                }
            }
            made = count;
            assert_eq!(drops.load(Ordering::SeqCst), n as usize / 4 * 3);
        }
        assert_eq!(drops.load(Ordering::SeqCst), made);
    }

    #[test]
    fn queries_filters_and_change_detection() {
        let mut w = World::new();
        let a = w.spawn((Pos(1.0), Sp(1)));
        let b = w.spawn(Pos(2.0));
        let c = w.spawn(Sp(3));
        let d = w.spawn((Pos(4.0), Vel(0.0), Sp(4)));
        let sorted = |mut v: Vec<Entity>| {
            v.sort();
            v
        };
        let pairs: Vec<(f32, u32)> = w
            .query::<(&Pos, &Sp), ()>()
            .unwrap()
            .map(|(p, s)| (p.0, s.0))
            .collect();
        assert_eq!(pairs.len(), 2);
        assert!(pairs.contains(&(1.0, 1)) && pairs.contains(&(4.0, 4)));
        let with: Vec<Entity> = w.query::<Entity, With<Sp>>().unwrap().collect();
        assert_eq!(sorted(with), sorted(vec![a, c, d]));
        let without: Vec<Entity> = w.query::<Entity, Without<Sp>>().unwrap().collect();
        assert_eq!(without, vec![b]);
        let opt: Vec<(Entity, Option<u32>)> = w
            .query::<(Entity, Option<&Sp>), With<Pos>>()
            .unwrap()
            .map(|(e, s)| (e, s.map(|s| s.0)))
            .collect();
        assert_eq!(opt.len(), 3);
        assert!(opt.contains(&(b, None)) && opt.contains(&(d, Some(4))));
        assert_eq!(w.query_one::<&Sp>(c).unwrap(), Some(&Sp(3)));
        assert!(w.query_one::<&Sp>(b).unwrap().is_none());

        // Added/Changed: everything was just added.
        assert_eq!(w.query::<Entity, Added<Sp>>().unwrap().count(), 3);
        w.clear_trackers();
        assert_eq!(w.query::<Entity, Changed<Sp>>().unwrap().count(), 0);
        // Reading through `Mut` is not a change; writing is.
        for s in w.query::<&mut Sp, ()>().unwrap() {
            let s: Mut<'_, Sp> = s;
            assert!(s.0 > 0);
        }
        assert_eq!(w.query::<Entity, Changed<Sp>>().unwrap().count(), 0);
        for (e, mut s) in w.query::<(Entity, &mut Sp), ()>().unwrap() {
            if e == c {
                s.0 += 100;
            }
        }
        let changed: Vec<Entity> = w.query::<Entity, Changed<Sp>>().unwrap().collect();
        assert_eq!(changed, vec![c]);
        assert_eq!(w.get::<Sp>(c), Some(&Sp(103)));
        w.clear_trackers();
        w.insert(a, Sp(7)).unwrap(); // replacing is a change, not an add
        w.insert(b, Sp(8)).unwrap(); // a new value is both
        let changed: Vec<Entity> = w.query::<Entity, Changed<Sp>>().unwrap().collect();
        assert_eq!(sorted(changed), sorted(vec![a, b]));
        let added: Vec<Entity> = w.query::<Entity, Added<Sp>>().unwrap().collect();
        assert_eq!(added, vec![b]);
        w.clear_trackers();
        w.get_mut::<Sp>(d).unwrap();
        let changed: Vec<Entity> = w.query::<Entity, Changed<Sp>>().unwrap().collect();
        assert_eq!(changed, vec![d]);
    }

    #[test]
    fn cached_query_state_sees_new_sparse_archetypes() {
        let mut w = World::new();
        w.spawn((Pos(1.0), Sp(1)));
        let mut st = QueryState::<(&Pos, &Sp), ()>::new();
        let sum = |st: &mut QueryState<(&Pos, &Sp), ()>, w: &mut World| -> u32 {
            st.iter(w).unwrap().map(|(_, s)| s.0).sum()
        };
        assert_eq!(sum(&mut st, &mut w), 1);
        let e = w.spawn((Pos(2.0), Vel(0.0)));
        assert_eq!(sum(&mut st, &mut w), 1);
        w.insert(e, Sp(10)).unwrap(); // new archetype {Pos, Vel, Sp}
        w.spawn((Pos(3.0), Tag, Sp(100)));
        assert_eq!(sum(&mut st, &mut w), 111);
        assert_eq!(st.cache().archetypes_seen(), w.archetypes().len());
        w.remove::<Sp>(e).unwrap();
        assert_eq!(sum(&mut st, &mut w), 101);
    }

    #[test]
    fn par_for_each_over_sparse_components() {
        let mut w = World::new();
        let n: u32 = if cfg!(miri) { 12 } else { 10_000 };
        for i in 0..n {
            match i % 3 {
                0 => w.spawn(Sp(i)),
                1 => w.spawn((Sp(i), Pos(0.0))),
                _ => w.spawn((Sp(i), Vel(0.0), Tag)),
            };
        }
        w.par_for_each::<&mut Sp, ()>(|mut s| s.0 += 1).unwrap();
        let mut got: Vec<u32> = w.query::<&Sp, ()>().unwrap().map(|s| s.0).collect();
        got.sort();
        assert_eq!(got, (1..=n).collect::<Vec<_>>());
    }

    #[derive(Default)]
    struct SpHits(u32);
    fn count_sp_changed(mut q: Query<Entity, Changed<Sp>>, mut hits: canopy_ecs::ResMut<SpHits>) {
        hits.0 += q.iter_mut().count() as u32;
    }

    /// `check_change_ticks` must clamp sparse-set ticks too, or the value's tick wraps
    /// around and reads as changed again once the world tick laps it.
    #[test]
    fn sparse_change_ticks_are_clamped_across_a_full_u32_wrap() {
        let mut w = World::new();
        w.insert_resource(SpHits::default());
        let e = w.spawn((Pos(0.0), Sp(0)));
        let mut s = Schedule::new(Executor::SingleThreaded);
        s.add_fn(SystemBuilder::new("count"), count_sp_changed)
            .unwrap();
        s.run(&mut w).unwrap();
        assert_eq!(w.resource::<SpHits>().unwrap().0, 1);
        let step = 1u32 << 28;
        for _ in 0..(1u64 << 32) / u64::from(step) + 4 {
            let t = w.change_tick().0.wrapping_add(step);
            w.set_change_tick(t);
            s.run(&mut w).unwrap();
            assert_eq!(w.resource::<SpHits>().unwrap().0, 1, "spurious change");
        }
        w.get_mut::<Sp>(e).unwrap().0 = 1;
        s.run(&mut w).unwrap();
        s.run(&mut w).unwrap();
        assert_eq!(w.resource::<SpHits>().unwrap().0, 2);
    }

    // Two systems writing the same sparse set, made archetype-disjoint by `Tag`. They
    // share one dense column, so this is where disjointness has to hold per entity
    // slot rather than per column: they must run concurrently (rendezvous), and Miri's
    // race detector checks that the writes never touch the same slot.
    fn sp_tagged(mut q: Query<&mut Sp, With<Tag>>, m: Res<Meet>) {
        q.iter_mut().for_each(|mut s| s.0 += 1);
        rendezvous(&m);
        q.iter_mut().for_each(|mut s| s.0 += 1);
    }
    fn sp_untagged(mut q: Query<(&mut Sp, &Pos), Without<Tag>>, m: Res<Meet>) {
        q.iter_mut().for_each(|(mut s, _)| s.0 += 10);
        rendezvous(&m);
        q.iter_mut().for_each(|(mut s, _)| s.0 += 10);
    }
    fn sp_all(mut q: Query<&Sp>, mut t: canopy_ecs::ResMut<SpHits>) {
        t.0 = q.iter_mut().map(|s| s.0).sum();
    }
    fn sp_alias(_a: Query<&mut Sp, With<Tag>>, _b: Query<&Sp>) {}

    #[test]
    fn disjoint_sparse_writers_run_concurrently_and_overlaps_are_ordered() {
        let n: u32 = if cfg!(miri) { 6 } else { 200 };
        let mut w = World::new();
        w.insert_resource(Meet {
            arrived: AtomicUsize::new(0),
            met: AtomicUsize::new(0),
        });
        w.insert_resource(SpHits::default());
        for i in 0..n {
            if i % 2 == 0 {
                w.spawn((Pos(0.0), Tag, Sp(0)));
            } else {
                w.spawn((Pos(0.0), Sp(0)));
            }
        }
        let mut s = Schedule::new(Executor::MultiThreaded);
        s.add_fn(SystemBuilder::new("t"), sp_tagged).unwrap();
        s.add_fn(SystemBuilder::new("u"), sp_untagged).unwrap();
        s.add_fn(SystemBuilder::new("all").after("t").after("u"), sp_all)
            .unwrap();
        assert!(s.ambiguities().unwrap().is_empty());
        s.run(&mut w).unwrap();
        assert_eq!(w.resource::<Meet>().unwrap().met.load(Ordering::SeqCst), 2);
        assert_eq!(
            w.resource::<SpHits>().unwrap().0,
            (n / 2) * 2 + (n / 2) * 20
        );
        // A filtered writer and an unfiltered reader of the same sparse component
        // overlap: rejected inside one system, ordered (and reported) across systems.
        let mut s2 = Schedule::new(Executor::MultiThreaded);
        assert!(matches!(
            s2.add_fn(SystemBuilder::new("alias"), sp_alias),
            Err(EcsError::ConflictingQueryAccess(_))
        ));
        s2.add_fn(SystemBuilder::new("t"), sp_tagged).unwrap();
        s2.add_fn(SystemBuilder::new("all"), sp_all).unwrap();
        assert_eq!(
            s2.ambiguities().unwrap(),
            &[("t".to_string(), "all".to_string())]
        );
    }
}

mod bundles {
    use super::*;
    use canopy_ecs::{Added, StorageType};

    #[derive(Debug, PartialEq, Clone)]
    struct Sp(u32);
    impl canopy_ecs::Component for Sp {
        const STORAGE: StorageType = StorageType::SparseSet;
    }

    /// Heap-owning table component, so Miri sees a lost or doubled drop.
    #[derive(Debug, PartialEq)]
    struct Name(String);
    impl canopy_ecs::Component for Name {}

    fn name(w: &World, e: Entity) -> Option<&str> {
        w.get::<Name>(e).map(|n| n.0.as_str())
    }

    #[test]
    fn bundle_moves_the_entity_once() {
        let mut w = World::new();
        let e = w.spawn((Pos(1.0), Vel(2.0), Tag, Sp(3)));
        // Root plus the target only: no (Pos), (Pos, Vel), (Pos, Vel, Tag) waypoints.
        assert_eq!(w.archetypes().len(), 2);
        assert_eq!(w.get::<Pos>(e), Some(&Pos(1.0)));
        assert_eq!(w.get::<Vel>(e), Some(&Vel(2.0)));
        assert_eq!(w.get::<Tag>(e), Some(&Tag));
        assert_eq!(w.get::<Sp>(e), Some(&Sp(3)));
        // The cached edge is reused: same archetype, no new ones.
        let f = w.spawn((Pos(4.0), Vel(5.0), Tag, Sp(6)));
        assert_eq!(w.archetypes().len(), 2);
        assert_eq!(w.get::<Sp>(f), Some(&Sp(6)));
        assert_eq!(w.get::<Sp>(e), Some(&Sp(3)));
        // Inserting into an existing entity: one more archetype, not one per component.
        let g = w.spawn(Pos(7.0));
        assert_eq!(w.archetypes().len(), 3);
        w.insert(g, (Vel(8.0), Name("g".into()), Sp(9))).unwrap();
        assert_eq!(w.archetypes().len(), 4);
        assert_eq!(w.get::<Pos>(g), Some(&Pos(7.0)));
        assert_eq!(w.get::<Vel>(g), Some(&Vel(8.0)));
        assert_eq!(name(&w, g), Some("g"));
        assert_eq!(w.get::<Sp>(g), Some(&Sp(9)));
        assert_eq!(w.query::<&Pos, ()>().unwrap().count(), 3);
    }

    #[test]
    fn duplicate_components_in_a_bundle_keep_the_last_value() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut w = World::new();
        let e = w.spawn((
            Name("first".into()),
            Dropper(drops.clone()),
            Sp(1),
            Name("second".into()),
            Sp(2),
            Dropper(drops.clone()),
        ));
        assert_eq!(name(&w, e), Some("second"));
        assert_eq!(w.get::<Sp>(e), Some(&Sp(2)));
        // The first Dropper was replaced (dropped) by the second.
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let sp = w.component_id(std::any::TypeId::of::<Sp>()).unwrap();
        assert_eq!(w.sparse_set(sp).unwrap().len(), 1);
        // Duplicates of components the entity already has: both replace, last wins.
        w.insert(e, (Dropper(drops.clone()), Dropper(drops.clone())))
            .unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 3);
        assert!(w.despawn(e));
        assert_eq!(drops.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn bundle_ticks_mark_added_and_replaced_components() {
        let mut w = World::new();
        let e = w.spawn((Pos(1.0), Sp(1)));
        w.clear_trackers();
        // Pos and Sp are replaced, Vel and Name added, in one move.
        w.insert(e, (Vel(2.0), Pos(3.0), Name("n".into()), Sp(4)))
            .unwrap();
        let added = |w: &mut World| {
            let mut v = Vec::new();
            if w.query::<Entity, Added<Pos>>().unwrap().count() == 1 {
                v.push("Pos");
            }
            if w.query::<Entity, Added<Vel>>().unwrap().count() == 1 {
                v.push("Vel");
            }
            if w.query::<Entity, Added<Name>>().unwrap().count() == 1 {
                v.push("Name");
            }
            if w.query::<Entity, Added<Sp>>().unwrap().count() == 1 {
                v.push("Sp");
            }
            v
        };
        assert_eq!(added(&mut w), ["Vel", "Name"]);
        assert_eq!(w.query::<Entity, Changed<Pos>>().unwrap().count(), 1);
        assert_eq!(w.query::<Entity, Changed<Vel>>().unwrap().count(), 1);
        assert_eq!(w.query::<Entity, Changed<Name>>().unwrap().count(), 1);
        assert_eq!(w.query::<Entity, Changed<Sp>>().unwrap().count(), 1);
        assert_eq!(w.get::<Pos>(e), Some(&Pos(3.0)));
        assert_eq!(w.get::<Sp>(e), Some(&Sp(4)));
        // A pure replacement (no move) changes but does not add.
        w.clear_trackers();
        w.insert(e, (Vel(5.0), Sp(6))).unwrap();
        assert!(added(&mut w).is_empty());
        assert_eq!(w.query::<Entity, Changed<Vel>>().unwrap().count(), 1);
        assert_eq!(w.query::<Entity, Changed<Sp>>().unwrap().count(), 1);
        assert_eq!(w.query::<Entity, Changed<Pos>>().unwrap().count(), 0);
    }

    #[test]
    fn insert_on_a_dead_entity_drops_the_bundle() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut w = World::new();
        let e = w.spawn(Pos(0.0));
        assert!(w.despawn(e));
        let r = w.insert(
            e,
            (
                Dropper(drops.clone()),
                Name("x".into()),
                Dropper(drops.clone()),
            ),
        );
        assert!(matches!(r, Err(EcsError::NoSuchEntity(_))));
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn empty_bundle_spawns_a_component_less_entity() {
        let mut w = World::new();
        let e = w.spawn(());
        assert!(w.contains(e));
        assert_eq!(w.archetypes().len(), 1);
        w.insert(e, ()).unwrap();
        assert_eq!(w.query::<Entity, ()>().unwrap().count(), 1);
    }

    /// Panics in its destructor when `.1` is true; owns heap data for Miri.
    struct Bomb(#[allow(dead_code)] Box<u32>, bool);
    impl canopy_ecs::Component for Bomb {}
    impl Drop for Bomb {
        fn drop(&mut self) {
            if self.1 {
                panic!("bomb");
            }
        }
    }

    #[test]
    fn panicking_drop_on_replace_leaves_the_world_consistent() {
        let mut w = World::new();
        let e = w.spawn((Pos(1.0), Bomb(Box::new(1), true)));
        let other = w.spawn((Pos(2.0), Bomb(Box::new(2), false)));
        // Vel is added (a move) and Bomb replaced; the old Bomb's destructor panics.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            w.insert(e, (Bomb(Box::new(3), false), Vel(4.0), Name("n".into())))
        }));
        assert!(r.is_err());
        // Every added value landed before the replacement ran, so the row is complete.
        assert_eq!(w.get::<Vel>(e), Some(&Vel(4.0)));
        assert_eq!(name(&w, e), Some("n"));
        assert_eq!(w.get::<Pos>(e), Some(&Pos(1.0)));
        assert!(w.get::<Bomb>(e).is_some_and(|b| *b.0 == 3));
        let mut rows: Vec<(f32, u32)> = w
            .query::<(&Pos, &Bomb), ()>()
            .unwrap()
            .map(|(p, b)| (p.0, *b.0))
            .collect();
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert_eq!(rows, [(1.0, 3), (2.0, 2)]);
        assert!(w.despawn(e));
        assert!(w.despawn(other));
        assert_eq!(w.entity_count(), 0);
    }
}

mod query_cache {
    use super::*;

    #[derive(Default)]
    struct Seen(Vec<usize>);

    /// `World::query` and `World::par_for_each` remember matched archetypes per query
    /// type; archetypes created later must still be found.
    #[test]
    fn world_query_finds_archetypes_created_after_the_first_call() {
        let mut w = World::new();
        w.spawn(Pos(1.0));
        assert_eq!(w.query::<&Pos, ()>().unwrap().count(), 1);
        let par = |w: &mut World| {
            let n = AtomicUsize::new(0);
            w.par_for_each::<&Pos, ()>(|_| {
                n.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap();
            n.into_inner()
        };
        assert_eq!(par(&mut w), 1);
        let e = w.spawn((Pos(2.0), Vel(1.0)));
        w.spawn((Pos(3.0), Tag));
        w.spawn(Vel(9.0));
        assert_eq!(w.query::<&Pos, ()>().unwrap().count(), 3);
        assert_eq!(par(&mut w), 3);
        // A different query type has its own cache; filters are part of the key.
        assert_eq!(w.query::<&Pos, Without<Tag>>().unwrap().count(), 2);
        assert_eq!(w.query::<&Pos, With<Tag>>().unwrap().count(), 1);
        // Moving between existing archetypes needs no new match.
        w.remove::<Pos>(e).unwrap();
        assert_eq!(w.query::<&Pos, ()>().unwrap().count(), 2);
        assert_eq!(w.query::<&Vel, ()>().unwrap().count(), 2);
        // Writes through the cached path land on the right rows.
        for mut p in w.query::<&mut Pos, ()>().unwrap() {
            p.0 *= 10.0;
        }
        let mut xs: Vec<f32> = w.query::<&Pos, ()>().unwrap().map(|p| p.0).collect();
        xs.sort_by(f32::total_cmp);
        assert_eq!(xs, [10.0, 30.0]);
    }

    /// Closure systems share a per-world cache across runs and systems.
    #[test]
    fn system_queries_find_archetypes_created_between_runs() {
        for executor in [Executor::SingleThreaded, Executor::MultiThreaded] {
            let mut w = World::new();
            w.insert_resource(Seen::default());
            w.spawn(Pos(1.0));
            let mut s = Schedule::new(executor);
            s.add_system(
                SystemBuilder::new("count")
                    .read::<Pos>()
                    .res_write::<Seen>(),
                |ctx| {
                    let n = ctx.query::<&Pos, ()>().unwrap().count();
                    let m = AtomicUsize::new(0);
                    ctx.par_for_each::<&Pos, ()>(|_| {
                        m.fetch_add(1, Ordering::Relaxed);
                    })
                    .unwrap();
                    assert_eq!(n, m.into_inner());
                    ctx.resource_mut::<Seen>().unwrap().unwrap().0.push(n);
                },
            )
            .unwrap();
            s.add_system(SystemBuilder::new("other").read::<Pos>(), |ctx| {
                // Same query type from another system: same cache entry.
                let _ = ctx.query::<&Pos, ()>().unwrap().count();
            })
            .unwrap();
            s.run(&mut w).unwrap();
            w.spawn((Pos(2.0), Vel(0.0)));
            s.run(&mut w).unwrap();
            w.spawn((Pos(3.0), Tag));
            w.spawn((Pos(4.0), Vel(0.0)));
            s.run(&mut w).unwrap();
            assert_eq!(w.resource::<Seen>().unwrap().0, [1, 2, 4]);
        }
    }
}
