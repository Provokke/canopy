<div align="center">

# canopy

A from-scratch, archetype-based Entity Component System in Rust. No bevy_ecs, hecs, legion or specs underneath, just std, rayon and a lot of `unsafe` that I've tried to keep honest.

![License](https://img.shields.io/badge/LICENSE-MIT-blue?style=for-the-badge)  ![Language](https://img.shields.io/badge/RUST-dea584?style=for-the-badge&logo=rust&logoColor=white)  ![Tests](https://img.shields.io/badge/TESTS-98-brightgreen?style=for-the-badge)

Refer to [LICENSE](LICENSE) for the legal bits.

</div>

## What's in it

Three crates in the workspace:

- **canopy-ecs** is the core.
  - Generational entity ids with a LIFO free list and lock-free reservation, so `Commands::spawn` can hand out an id before the command is applied.
  - `BlobVec` type-erased columns, 16 byte aligned, with panic-safe drops.
  - Archetypes with cached add/remove edges and cached bundle edges, so spawning or inserting a bundle moves the entity once however many components it has.
  - Two storages per component type: table columns (default) or a per-type sparse set for components that get added and removed a lot.
  - Typed queries (`&T`, `&mut T`, `Option<&T>`, `Entity`, tuples) with `With`, `Without`, `Added`, `Changed` and `Or` filters. `&mut T` items are `Mut<T>`, and only `DerefMut`/`set_changed` count as a change. Tick comparison is wraparound-safe.
  - `FilteredAccess` disjointness: two queries that touch the same component but are made disjoint by `With`/`Without` don't conflict, and the scheduler will run those systems at the same time.
  - A scheduler with explicit `.before()`/`.after()` ordering, implicit edges for every conflicting pair, system sets with run conditions, and an ambiguity report. The executor is a ready queue on rayon: a system starts as soon as its dependencies finish, with no per-wave barrier. Deferred commands are applied in system order, and panics are caught per system.
  - Plain-function systems (`fn mover(q: Query<(&mut Pos, &Vel)>, c: ResMut<Counter>)`) with access derived from the parameter types.
  - Double-buffered `Events<E>` with per-reader cursors.
  - `Parent`/`Children` hierarchy with cycle checks, `despawn_recursive`, and `Transform`/`GlobalTransform` propagation.
  - `par_for_each`, which iterates archetype row chunks in parallel.
- **canopy-app** is an `App` plus `Plugin`/`PluginGroup` layer. Schedules are Startup, First, FixedUpdate (fixed-step accumulator), Update and PostUpdate. It has `Time`, transform propagation, and a `PhysicsPlugin` that does axis-aligned box collision (sweep and prune on x, push-out along the axis of least penetration, `Collision` events).
- **canopy-demo** is a headless particle demo. It prints a stats line per frame and a final state hash, which comes out identical for any `--threads` value.

There's also a `fuzz` crate (a cargo-fuzz target that throws random spawn/insert/remove/despawn/query at the world and compares against a model) and `benches-compare`, which benchmarks against hecs.

```rust
use canopy_ecs::{World, Component};
struct Pos(f32); impl Component for Pos {}
struct Vel(f32); impl Component for Vel {}
let mut w = World::new();
w.spawn((Pos(0.0), Vel(1.0)));
for (mut p, v) in w.query::<(&mut Pos, &Vel), ()>().unwrap() { p.0 += v.0; }
```

## 🏗️ Architecture

```
 World
  ├─ Entities  (index, generation) -> Location{archetype,row}
  ├─ components registry  TypeId -> ComponentId (layout, drop fn)
  ├─ archetypes[]  sorted ComponentIds (table + sparse), remove edges, bundle edges
  │     └─ columns[]  BlobVec (16B-aligned) + added/changed ticks (table components)
  ├─ sparse_sets[]  per sparse component: dense column + ticks, owner entities, sparse index
  └─ resources  TypeId -> UnsafeCell<Box<dyn Any>>
 Schedule ── build(): explicit edges + conflict edges ── run(): ready queue on the rayon pool
```

## 🚀 Quick start

```
cargo build --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

The demo:

```
cargo run --release -p canopy-demo -- --entities 10000 --frames 100 --threads 4
```

`--print-every N` thins out the per-frame output and `--ascii` prints a density map of the final state. On my shared 4-core VM the default run took about 316 ms on one thread and 155 ms on four.

## 🧪 Testing

98 tests, all passing:

- canopy-ecs: 3 unit, 56 integration in `tests/core.rs` (scheduler, sparse sets, bundles, query caches, change detection), 13 in `tests/hierarchy.rs` (hierarchy, commands, events), 2 proptest model checks in `tests/model.rs`, 1 trybuild compile-fail test (3 cases) and 1 doctest.
- canopy-app: 1 determinism test (60 frames single- vs multi-threaded, same hash), 7 plugin tests, 7 physics tests, 2 schedule tests.
- canopy-demo: 5 determinism/CLI tests.

The model tests run random spawn/insert/remove/despawn sequences, including bundles that mix table, sparse and duplicate components, against a simple reference model.

The compile-fail tests check that a `!Send` component, two live mutable query iterators, and a `&mut` item outliving the world borrow are all rejected by the compiler. Aliasing inside a single query, like `(&mut A, &mut A)`, is a runtime `EcsError::ConflictingQueryAccess` instead.

Miri covers the ecs crate (`MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p canopy-ecs`) with reduced sizes under `cfg(miri)`, including two systems writing the same component through disjoint queries at the same time. I also forced the disjointness rule to "always disjoint" once as a check, and Miri did report the data race, so the test can fail. I've done the same sort of thing for most of the later features: break the code on purpose and make sure a test goes red. ASan passes as well, and the fuzz target ran for a minute or so with no crashes.

## ⏱️ Benchmarks

Criterion, on a shared 4-core VM with other builds running, so treat these as rough.

| bench | time (median) |
|---|---|
| spawn + despawn 10k entities (Pos, Vel) | 1.08 ms |
| iterate 1M entities, `pos += vel` | 1.50 ms |
| same via `par_for_each` (4096-row chunks) | 0.71 ms |
| insert + remove a component on 10k entities | 1.32 ms |
| 100 empty non-conflicting systems, single-threaded | 3.14 µs |

Against hecs 0.10.5, hecs is faster: about 1.6x on straight iteration, 3.8x on spawn/despawn and 1.9x on archetype moves. Fragmented iteration across 26 archetypes is level. `par_for_each` scales close to linearly (1.0x, 2.0x, 3.8x on 1, 2 and 4 threads) on a compute-heavy workload.

Two things that mattered a lot: bundle edges took a 6-component spawn/despawn of 10k entities from about 2.9 ms to 0.8 ms, and the per-world query cache took `World::query` on 1023 archetypes from about 10 µs to under 200 ns.

I haven't compared against bevy_ecs. It would pull a big dependency tree and a `Cargo.lock` rewrite into the `--locked` build.

## ⚠️ Honest limits

- `Or` filters don't record access guarantees, so they never prove two queries disjoint. Same for `Option<&T>`.
- Run conditions are `FnMut(&World) -> bool`, not system-parameter conditions.
- Cycles in nested system sets aren't reported properly.
- A schedule that sits idle for 2^32 ticks can miss changes.
- Entity ids are nondeterministic when several systems spawn through `Commands` concurrently.
- Plugins can't declare dependencies on each other.
- Bundles are limited to 6 fields.
- If you hand-edit `Parent`/`Children` components you can leave stale ids behind.
- The closure-query cache copies and takes a lock.
- Physics is boxes only: no rotation or scale, no gravity, friction or continuous collision, equal masses, and parented bodies are treated as world space.
- The demo's ASCII renderer is one-shot and the CLI is hand-rolled, with no clap.
- Miri runs scoped std threads in place of rayon, because crossbeam-epoch trips Miri. The real rayon path passed once under tree borrows, but that isn't part of any gate.
- The fuzz target isn't built by the CI gate.
- The compile-fail tests skip on nightly and beta, because the pinned `.stderr` files are stable diagnostics.

## Licence

MIT OR Apache-2.0.
