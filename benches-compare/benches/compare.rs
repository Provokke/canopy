//! canopy-ecs vs hecs on the same workloads, plus `par_for_each` thread scaling and
//! fragmented iteration. Run with `cargo bench -p benches-compare`.
use canopy_ecs::{Component, World};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use std::hint::black_box;

#[derive(Clone, Copy)]
struct Pos([f32; 3]);
impl Component for Pos {}
#[derive(Clone, Copy)]
struct Vel([f32; 3]);
impl Component for Vel {}
#[derive(Clone, Copy)]
struct Tag;
impl Component for Tag {}

const ENTITIES: usize = 1_000_000;

fn canopy_world(n: usize) -> World {
    let mut w = World::new();
    for _ in 0..n {
        w.spawn((Pos([0.0; 3]), Vel([1.0; 3])));
    }
    w
}

fn hecs_world(n: usize) -> hecs::World {
    let mut w = hecs::World::new();
    for _ in 0..n {
        w.spawn((Pos([0.0; 3]), Vel([1.0; 3])));
    }
    w
}

fn simple_iter(c: &mut Criterion) {
    let mut g = c.benchmark_group("simple_iter_1m");
    let mut w = canopy_world(ENTITIES);
    g.bench_function("canopy", |b| {
        b.iter(|| {
            for (mut p, v) in w.query::<(&mut Pos, &Vel), ()>().unwrap() {
                for i in 0..3 {
                    p.0[i] += v.0[i];
                }
            }
        })
    });
    let mut h = hecs_world(ENTITIES);
    g.bench_function("hecs", |b| {
        b.iter(|| {
            for (_, (p, v)) in h.query_mut::<(&mut Pos, &Vel)>() {
                for i in 0..3 {
                    p.0[i] += v.0[i];
                }
            }
        })
    });
    g.finish();
}

fn spawn_despawn(c: &mut Criterion) {
    let mut g = c.benchmark_group("spawn_despawn_10k");
    let mut w = World::new();
    g.bench_function("canopy", |b| {
        b.iter(|| {
            let es: Vec<_> = (0..10_000)
                .map(|_| w.spawn((Pos([0.0; 3]), Vel([1.0; 3]))))
                .collect();
            for e in es {
                w.despawn(e);
            }
        })
    });
    let mut h = hecs::World::new();
    g.bench_function("hecs", |b| {
        b.iter(|| {
            let es: Vec<_> = (0..10_000)
                .map(|_| h.spawn((Pos([0.0; 3]), Vel([1.0; 3]))))
                .collect();
            for e in es {
                h.despawn(e).unwrap();
            }
        })
    });
    g.finish();
}

fn add_remove(c: &mut Criterion) {
    let mut g = c.benchmark_group("add_remove_component_10k");
    let mut w = World::new();
    let es: Vec<_> = (0..10_000)
        .map(|_| w.spawn((Pos([0.0; 3]), Vel([1.0; 3]))))
        .collect();
    g.bench_function("canopy", |b| {
        b.iter(|| {
            for &e in &es {
                w.insert(e, Tag).unwrap();
            }
            for &e in &es {
                black_box(w.remove::<Tag>(e).unwrap());
            }
        })
    });
    let mut h = hecs::World::new();
    let hes: Vec<_> = (0..10_000)
        .map(|_| h.spawn((Pos([0.0; 3]), Vel([1.0; 3]))))
        .collect();
    g.bench_function("hecs", |b| {
        b.iter(|| {
            for &e in &hes {
                h.insert_one(e, Tag).unwrap();
            }
            for &e in &hes {
                black_box(h.remove_one::<Tag>(e).unwrap());
            }
        })
    });
    g.finish();
}

struct Marker<const N: usize>;
impl<const N: usize> Component for Marker<N> {}
#[derive(Clone, Copy)]
struct Data(f32);
impl Component for Data {}

const FRAG_PER_ARCHETYPE: usize = 100;

macro_rules! frag_spawn {
    ($w:ident, $($i:literal),*) => { $( for _ in 0..FRAG_PER_ARCHETYPE { $w.spawn((Data(1.0), Marker::<$i>)); } )* };
}

/// 26 archetypes `(Data, Marker<i>)` of 100 entities each: a query for `Data` must
/// walk all of them.
fn fragmented_iter(c: &mut Criterion) {
    let mut g = c.benchmark_group("fragmented_iter_26_archetypes");
    let mut w = World::new();
    frag_spawn!(
        w, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
        24, 25
    );
    g.bench_function("canopy_world_query", |b| {
        b.iter(|| {
            for mut d in w.query::<&mut Data, ()>().unwrap() {
                d.0 *= 2.0;
            }
        })
    });
    let mut st = canopy_ecs::QueryState::<&mut Data, ()>::new();
    g.bench_function("canopy_query_state", |b| {
        b.iter(|| {
            for mut d in st.iter(&mut w).unwrap() {
                d.0 *= 2.0;
            }
        })
    });
    let mut h = hecs::World::new();
    macro_rules! hecs_frag {
        ($($i:literal),*) => { $( for _ in 0..FRAG_PER_ARCHETYPE { h.spawn((Data(1.0), Marker::<$i>)); } )* };
    }
    hecs_frag!(
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25
    );
    g.bench_function("hecs", |b| {
        b.iter(|| {
            for (_, d) in h.query_mut::<&mut Data>() {
                d.0 *= 2.0;
            }
        })
    });
    g.finish();
}

/// Per-entity work heavy enough for threads to matter (about 200 flops).
#[inline(always)]
fn heavy(p: &mut Pos, v: &Vel) {
    let mut x = p.0[0];
    for _ in 0..50 {
        x = x * 0.999 + v.0[0] * 0.001 + 0.5;
    }
    p.0[0] = x;
}

fn par_scaling(c: &mut Criterion) {
    let mut g = c.benchmark_group("par_for_each_scaling_1m_heavy");
    g.sample_size(20);
    let mut w = canopy_world(ENTITIES);
    g.bench_function("canopy_sequential", |b| {
        b.iter(|| {
            for (mut p, v) in w.query::<(&mut Pos, &Vel), ()>().unwrap() {
                heavy(&mut p, v);
            }
        })
    });
    for threads in [1usize, 2, 4] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        g.bench_with_input(
            BenchmarkId::new("canopy_par_for_each", threads),
            &threads,
            |b, _| {
                b.iter(|| {
                    pool.install(|| {
                        w.par_for_each::<(&mut Pos, &Vel), ()>(|(mut p, v)| heavy(&mut p, v))
                            .unwrap()
                    })
                })
            },
        );
    }
    let mut h = hecs_world(ENTITIES);
    g.bench_function("hecs_sequential", |b| {
        b.iter(|| {
            for (_, (p, v)) in h.query_mut::<(&mut Pos, &Vel)>() {
                heavy(p, v);
            }
        })
    });
    g.finish();
}

criterion_group!(
    benches,
    simple_iter,
    spawn_despawn,
    add_remove,
    fragmented_iter,
    par_scaling
);
criterion_main!(benches);

