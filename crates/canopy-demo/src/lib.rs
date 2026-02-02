//! Boids on `canopy-ecs`: a deterministic flocking simulation driven by [`canopy_app::App`].
//!
//! Each frame runs three systems: `build_grid` snapshots positions/velocities into a
//! uniform grid, `steer` computes every boid's new velocity from that snapshot with
//! [`SystemContext::par_for_each`](canopy_ecs::SystemContext::par_for_each), and
//! `integrate` moves the boids. Every boid's steering is a sequential sum over a fixed
//! neighbour order read from an immutable snapshot, so the result does not depend on how
//! the work is split across threads: the state hash is identical for any thread count.
#![deny(unsafe_code)]
#![warn(missing_docs)]

use anyhow::{Context, Result};
use canopy_app::{App, Plugin, Position, Velocity, DT};
use canopy_ecs::schedule::{Executor, SystemBuilder};
use canopy_ecs::{Component, EcsError};

/// Half the side length of the square world.
pub const HALF: f32 = 100.0;
/// Neighbour perception radius (also the grid cell size).
const RADIUS: f32 = 5.0;
const SEPARATION_RADIUS: f32 = 1.5;
const MAX_SPEED: f32 = 8.0;
const MIN_SPEED: f32 = 2.0;
const CELLS: usize = (2.0 * HALF / RADIUS) as usize;

/// Stable per-boid index (spawn order), used to address the snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoidId(pub u32);
impl Component for BoidId {}

/// Run parameters.
#[derive(Clone, Debug)]
pub struct BoidsConfig {
    /// Number of boids.
    pub entities: usize,
    /// Frames to simulate.
    pub frames: usize,
    /// Worker threads (1 selects the single-threaded executor).
    pub threads: usize,
    /// RNG seed for the initial state.
    pub seed: u64,
}

impl Default for BoidsConfig {
    fn default() -> Self {
        Self {
            entities: 10_000,
            frames: 100,
            threads: 1,
            seed: 42,
        }
    }
}

/// Statistics for one simulated frame.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameStats {
    /// Frame number, from 1.
    pub frame: usize,
    /// Number of boids.
    pub boids: usize,
    /// Mean speed.
    pub mean_speed: f32,
    /// Centroid of all positions.
    pub centroid: [f32; 2],
    /// FNV-1a hash of the full state after this frame.
    pub hash: u64,
}

#[derive(Default)]
struct Grid {
    pos: Vec<[f32; 2]>,
    vel: Vec<[f32; 2]>,
    cells: Vec<Vec<u32>>,
}

fn cell_of(p: [f32; 2]) -> (usize, usize) {
    let f = |v: f32| (((v + HALF) / RADIUS).floor().max(0.0) as usize).min(CELLS - 1);
    (f(p[0]), f(p[1]))
}

fn splitmix(state: &mut u64) -> f32 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
}

/// Spawns the flock and registers the boid systems.
pub struct BoidsPlugin {
    /// Number of boids.
    pub count: usize,
    /// RNG seed.
    pub seed: u64,
}

impl Plugin for BoidsPlugin {
    fn build(&self, app: &mut App) -> Result<(), EcsError> {
        let (count, seed) = (self.count, self.seed);
        app.world.insert_resource(Grid::default());
        app.startup
            .add_system(SystemBuilder::new("spawn_boids"), move |ctx| {
                ctx.command(move |w| {
                    let mut s = seed;
                    for i in 0..count {
                        let p = [
                            splitmix(&mut s) * 2.0 * HALF - HALF,
                            splitmix(&mut s) * 2.0 * HALF - HALF,
                        ];
                        let v = [
                            splitmix(&mut s) * 2.0 * MAX_SPEED - MAX_SPEED,
                            splitmix(&mut s) * 2.0 * MAX_SPEED - MAX_SPEED,
                        ];
                        w.spawn((BoidId(i as u32), Position(p), Velocity(v)));
                    }
                });
            })?;
        app.update.add_system(
            SystemBuilder::new("build_grid")
                .read::<BoidId>()
                .read::<Position>()
                .read::<Velocity>()
                .res_write::<Grid>(),
            |ctx| {
                let (Ok(q), Ok(Some(grid))) = (
                    ctx.query::<(&BoidId, &Position, &Velocity), ()>(),
                    ctx.resource_mut::<Grid>(),
                ) else {
                    return;
                };
                grid.cells.resize_with(CELLS * CELLS, Vec::new);
                for c in &mut grid.cells {
                    c.clear();
                }
                for (id, p, v) in q {
                    let i = id.0 as usize;
                    if i >= grid.pos.len() {
                        grid.pos.resize(i + 1, [0.0; 2]);
                        grid.vel.resize(i + 1, [0.0; 2]);
                    }
                    grid.pos[i] = p.0;
                    grid.vel[i] = v.0;
                    let (cx, cy) = cell_of(p.0);
                    grid.cells[cy * CELLS + cx].push(id.0);
                }
            },
        )?;
        app.update.add_system(
            SystemBuilder::new("steer")
                .read::<BoidId>()
                .read::<Position>()
                .write::<Velocity>()
                .res_read::<Grid>()
                .after("build_grid"),
            |ctx| {
                let Ok(Some(grid)) = ctx.resource::<Grid>() else {
                    return;
                };
                let _ = ctx.par_for_each::<(&BoidId, &Position, &mut Velocity), ()>(
                    |(id, p, mut v)| steer(grid, id.0, p.0, &mut v.0),
                );
            },
        )?;
        app.update.add_system(
            SystemBuilder::new("integrate")
                .write::<Position>()
                .write::<Velocity>()
                .after("steer"),
            |ctx| {
                let Ok(q) = ctx.query::<(&mut Position, &mut Velocity), ()>() else {
                    return;
                };
                for (mut p, mut v) in q {
                    for i in 0..2 {
                        p.0[i] += v.0[i] * DT;
                        if p.0[i].abs() > HALF {
                            p.0[i] = p.0[i].clamp(-HALF, HALF);
                            v.0[i] = -v.0[i];
                        }
                    }
                }
            },
        )?;
        Ok(())
    }
}

/// New velocity for boid `id` from the grid snapshot (separation, alignment, cohesion).
fn steer(grid: &Grid, id: u32, p: [f32; 2], v: &mut [f32; 2]) {
    let (cx, cy) = cell_of(p);
    let (mut sep, mut ali, mut coh, mut n) = ([0.0f32; 2], [0.0f32; 2], [0.0f32; 2], 0.0f32);
    for ny in cy.saturating_sub(1)..=(cy + 1).min(CELLS - 1) {
        for nx in cx.saturating_sub(1)..=(cx + 1).min(CELLS - 1) {
            for &j in &grid.cells[ny * CELLS + nx] {
                if j == id {
                    continue;
                }
                let (q, w) = (grid.pos[j as usize], grid.vel[j as usize]);
                let (dx, dy) = (p[0] - q[0], p[1] - q[1]);
                let d2 = dx * dx + dy * dy;
                if d2 >= RADIUS * RADIUS {
                    continue;
                }
                n += 1.0;
                ali[0] += w[0];
                ali[1] += w[1];
                coh[0] += q[0];
                coh[1] += q[1];
                if d2 < SEPARATION_RADIUS * SEPARATION_RADIUS {
                    let k = 1.0 / d2.max(1e-2);
                    sep[0] += dx * k;
                    sep[1] += dy * k;
                }
            }
        }
    }
    if n > 0.0 {
        for i in 0..2 {
            let align = ali[i] / n - v[i];
            let cohere = coh[i] / n - p[i];
            v[i] += (sep[i] * 4.0 + align * 2.0 + cohere * 0.5) * DT;
        }
    }
    let speed = (v[0] * v[0] + v[1] * v[1]).sqrt();
    if speed > MAX_SPEED {
        let k = MAX_SPEED / speed;
        v[0] *= k;
        v[1] *= k;
    } else if speed < MIN_SPEED && speed > 0.0 {
        let k = MIN_SPEED / speed;
        v[0] *= k;
        v[1] *= k;
    }
}

/// Per-frame statistics and the FNV-1a state hash (ordered by [`BoidId`]).
pub fn frame_stats(app: &mut App, frame: usize) -> Result<FrameStats> {
    let mut rows: Vec<(u32, [f32; 2], [f32; 2])> = app
        .world
        .query::<(&BoidId, &Position, &Velocity), ()>()
        .context("querying boids")?
        .map(|(id, p, v)| (id.0, p.0, v.0))
        .collect();
    rows.sort_unstable_by_key(|r| r.0);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |x: u32| {
        for b in x.to_le_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    };
    let (mut speed, mut cx, mut cy) = (0.0f64, 0.0f64, 0.0f64);
    for (id, p, v) in &rows {
        eat(*id);
        for x in p.iter().chain(v.iter()) {
            eat(x.to_bits());
        }
        speed += f64::from((v[0] * v[0] + v[1] * v[1]).sqrt());
        cx += f64::from(p[0]);
        cy += f64::from(p[1]);
    }
    let n = rows.len().max(1) as f64;
    Ok(FrameStats {
        frame,
        boids: rows.len(),
        mean_speed: (speed / n) as f32,
        centroid: [(cx / n) as f32, (cy / n) as f32],
        hash: h,
    })
}

/// Renders boid positions as an ASCII density map (`width` x `height` characters).
pub fn ascii_render(app: &mut App, width: usize, height: usize) -> Result<String> {
    let mut counts = vec![0u32; width * height];
    for p in app
        .world
        .query::<&Position, ()>()
        .context("querying positions")?
        .map(|p| p.0)
    {
        let x = (((p[0] + HALF) / (2.0 * HALF)) * width as f32) as usize;
        let y = (((p[1] + HALF) / (2.0 * HALF)) * height as f32) as usize;
        counts[y.min(height - 1) * width + x.min(width - 1)] += 1;
    }
    let max = counts.iter().copied().max().unwrap_or(0).max(1);
    const RAMP: &[u8] = b" .:-=+*#%@";
    let mut out = String::with_capacity((width + 1) * height);
    for row in counts.chunks(width).rev() {
        for &c in row {
            let level = (c as usize * (RAMP.len() - 1)).div_ceil(max as usize);
            out.push(RAMP[level.min(RAMP.len() - 1)] as char);
        }
        out.push('\n');
    }
    Ok(out)
}

/// Builds the app for `cfg`.
pub fn build_app(cfg: &BoidsConfig) -> Result<App> {
    let executor = if cfg.threads <= 1 {
        Executor::SingleThreaded
    } else {
        Executor::MultiThreaded
    };
    let mut app = App::new(executor);
    app.add_plugin(BoidsPlugin {
        count: cfg.entities,
        seed: cfg.seed,
    })
    .context("building boids plugin")?;
    Ok(app)
}

/// Runs the simulation on a dedicated pool of `cfg.threads` workers, calling
/// `on_frame` with each frame's statistics, and returns the final app.
pub fn run(cfg: &BoidsConfig, mut on_frame: impl FnMut(&FrameStats) + Send) -> Result<App> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cfg.threads.max(1))
        .build()
        .context("building thread pool")?;
    pool.install(|| {
        let mut app = build_app(cfg)?;
        for frame in 1..=cfg.frames {
            app.update().context("running frame")?;
            on_frame(&frame_stats(&mut app, frame)?);
        }
        Ok(app)
    })
}
