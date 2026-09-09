//! Criterion benchmarks for core ECS operations.
use canopy_ecs::schedule::{Executor, SystemBuilder};
use canopy_ecs::{Component, QueryState, Schedule, StorageType, World};
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use std::hint::black_box;

#[derive(Clone, Copy)]
struct Pos([f32; 3]);
impl Component for Pos {}
#[derive(Clone, Copy)]
struct Vel([f32; 3]);
impl Component for Vel {}
struct Tag;
impl Component for Tag {}
struct Marker<const N: usize>;
impl<const N: usize> Component for Marker<N> {}
struct Data<const N: usize>(#[allow(dead_code)] u64);
impl<const N: usize> Component for Data<N> {}
struct SparseTag(#[allow(dead_code)] u64);
impl Component for SparseTag {
    const STORAGE: StorageType = StorageType::SparseSet;
}

fn spawn_despawn(c: &mut Criterion) {
    c.bench_function("spawn_despawn_10k", |b| {
        let mut w = World::new();
        b.iter(|| {
            let es: Vec<_> = (0..10_000)
                .map(|_| w.spawn((Pos([0.0; 3]), Vel([1.0; 3]))))
                .collect();
            for e in es {
                w.despawn(e);
            }
        })
    });
}

fn simple_iter(c: &mut Criterion) {
    let mut w = World::new();
    for _ in 0..1_000_000 {
        w.spawn((Pos([0.0; 3]), Vel([1.0; 3])));
    }
    c.bench_function("simple_iter_1m", |b| {
        b.iter(|| {
            for (mut p, v) in w.query::<(&mut Pos, &Vel), ()>().unwrap() {
                for i in 0..3 {
                    p.0[i] += v.0[i];
                }
            }
        })
    });
}

fn par_iter(c: &mut Criterion) {
    let mut w = World::new();
    for _ in 0..1_000_000 {
        w.spawn((Pos([0.0; 3]), Vel([1.0; 3])));
    }
    c.bench_function("par_for_each_1m", |b| {
        b.iter(|| {
            w.par_for_each::<(&mut Pos, &Vel), ()>(|(mut p, v)| {
                for i in 0..3 {
                    p.0[i] += v.0[i];
                }
            })
            .unwrap()
        })
    });
}

fn add_remove(c: &mut Criterion) {
    let mut w = World::new();
    let es: Vec<_> = (0..10_000)
        .map(|_| w.spawn((Pos([0.0; 3]), Vel([1.0; 3]))))
        .collect();
    c.bench_function("add_remove_component_10k", |b| {
        b.iter(|| {
            for &e in &es {
                w.insert(e, Tag).unwrap();
            }
            for &e in &es {
                black_box(w.remove::<Tag>(e).unwrap());
            }
        })
    });
    // The same with a sparse-set component: the archetype still changes, but no table
    // column of it moves.
    c.bench_function("add_remove_sparse_component_10k", |b| {
        b.iter(|| {
            for &e in &es {
                w.insert(e, SparseTag(1)).unwrap();
            }
            for &e in &es {
                black_box(w.remove::<SparseTag>(e).unwrap());
            }
        })
    });
}

/// Bundle insertion: one archetype move per bundle (cached bundle edge) versus one per
/// component. `spawn_bundle6_10k` spawns and despawns 10k six-component entities;
/// `insert_bundle4_10k` inserts a four-component bundle on 10k `(Pos,)` entities (world
/// setup is not timed).
fn bundles(c: &mut Criterion) {
    c.bench_function("spawn_bundle6_10k", |b| {
        let mut w = World::new();
        b.iter(|| {
            let es: Vec<_> = (0..10_000u64)
                .map(|i| {
                    w.spawn((
                        Pos([0.0; 3]),
                        Vel([1.0; 3]),
                        Data::<0>(i),
                        Data::<1>(i),
                        Data::<2>(i),
                        Data::<3>(i),
                    ))
                })
                .collect();
            for e in es {
                w.despawn(e);
            }
        })
    });
    c.bench_function("insert_bundle4_10k", |b| {
        b.iter_batched(
            || {
                let mut w = World::new();
                let es: Vec<_> = (0..10_000).map(|_| w.spawn((Pos([0.0; 3]),))).collect();
                (w, es)
            },
            |(mut w, es)| {
                for (i, &e) in es.iter().enumerate() {
                    let i = i as u64;
                    w.insert(e, (Vel([1.0; 3]), Data::<0>(i), Data::<1>(i), Data::<2>(i)))
                        .unwrap();
                }
                (w, es)
            },
            BatchSize::LargeInput,
        )
    });
}

/// A world with 1023 archetypes (every non-empty subset of 10 markers), of which only
/// one holds `(Pos, Vel)`; queries for it should not pay for the other 1022.
fn many_archetypes_world() -> World {
    let mut w = World::new();
    for mask in 1u32..1024 {
        let e = w.spawn_empty();
        macro_rules! m {
            ($($i:literal),*) => { $( if mask & (1 << $i) != 0 { w.insert(e, Marker::<$i>).unwrap(); } )* };
        }
        m!(0, 1, 2, 3, 4, 5, 6, 7, 8, 9);
    }
    for _ in 0..100 {
        w.spawn((Pos([0.0; 3]), Vel([1.0; 3])));
    }
    w
}

fn query_many_archetypes(c: &mut Criterion) {
    let mut w = many_archetypes_world();
    let mut g = c.benchmark_group("query_1024_archetypes_100_matching_rows");
    g.bench_function("world_query_scan", |b| {
        b.iter(|| black_box(w.query::<(&Pos, &Vel), ()>().unwrap().count()))
    });
    let mut st = QueryState::<(&Pos, &Vel), ()>::new();
    g.bench_function("query_state_cached", |b| {
        b.iter(|| black_box(st.iter(&mut w).unwrap().count()))
    });
    g.finish();
}

fn scheduler_overhead(c: &mut Criterion) {
    for (name, ex) in [
        ("single", Executor::SingleThreaded),
        ("multi", Executor::MultiThreaded),
    ] {
        let mut w = World::new();
        let mut s = Schedule::new(ex);
        for i in 0..100 {
            s.add_system(SystemBuilder::new(format!("s{i}")).read::<Pos>(), |_| {})
                .unwrap();
        }
        c.bench_function(&format!("schedule_100_empty_systems_{name}"), |b| {
            b.iter(|| s.run(&mut w).unwrap())
        });
    }
}

/// Busy-waits `micros` (a system with real work, without touching the world).
fn work(micros: u64) {
    let t = std::time::Instant::now();
    while t.elapsed() < std::time::Duration::from_micros(micros) {
        std::hint::spin_loop();
    }
}

/// One 200 us system next to a chain of ten 20 us systems. Without a barrier the
/// chain can run alongside the long system (ideally ~200 us per run); with one
/// barrier per wave the first link waits for the long system (at least ~380 us).
fn scheduler_chain_beside_slow_system(c: &mut Criterion) {
    let mut w = World::new();
    let mut s = Schedule::new(Executor::MultiThreaded);
    s.add_system(SystemBuilder::new("slow"), |_| work(200))
        .unwrap();
    for i in 0..10 {
        let mut b = SystemBuilder::new(format!("c{i}"));
        if i > 0 {
            b = b.after(format!("c{}", i - 1));
        }
        s.add_system(b, |_| work(20)).unwrap();
    }
    c.bench_function("schedule_chain_10x20us_beside_200us", |b| {
        b.iter(|| s.run(&mut w).unwrap())
    });
}

criterion_group!(
    benches,
    spawn_despawn,
    simple_iter,
    par_iter,
    add_remove,
    bundles,
    query_many_archetypes,
    scheduler_overhead,
    scheduler_chain_beside_slow_system
);
criterion_main!(benches);
