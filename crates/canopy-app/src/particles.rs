//! A headless, deterministic particle demo.
use crate::{App, Plugin, DT};
use canopy_ecs::schedule::SystemBuilder;
use canopy_ecs::{Component, EcsError, World};

/// Particle position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Position(pub [f32; 2]);
impl Component for Position {}
/// Particle velocity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Velocity(pub [f32; 2]);
impl Component for Velocity {}

const GRAVITY: f32 = -9.8;
const HALF: f32 = 50.0;

/// Spawns `count` particles from a seeded generator; integrates gravity and bounces
/// them inside a box.
pub struct ParticlePlugin {
    /// Number of particles.
    pub count: usize,
    /// RNG seed.
    pub seed: u64,
}

fn splitmix(state: &mut u64) -> f32 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
}

impl Plugin for ParticlePlugin {
    fn build(&self, app: &mut App) -> Result<(), EcsError> {
        let (count, seed) = (self.count, self.seed);
        app.startup
            .add_system(SystemBuilder::new("spawn_particles"), move |ctx| {
                ctx.command(move |w| {
                    let mut s = seed;
                    for _ in 0..count {
                        let p = [
                            splitmix(&mut s) * 2.0 * HALF - HALF,
                            splitmix(&mut s) * 2.0 * HALF - HALF,
                        ];
                        let v = [
                            splitmix(&mut s) * 20.0 - 10.0,
                            splitmix(&mut s) * 20.0 - 10.0,
                        ];
                        w.spawn((Position(p), Velocity(v)));
                    }
                });
            })?;
        app.update
            .add_system(SystemBuilder::new("gravity").write::<Velocity>(), |ctx| {
                if let Ok(q) = ctx.query::<&mut Velocity, ()>() {
                    for mut v in q {
                        v.0[1] += GRAVITY * DT;
                    }
                }
            })?;
        app.update.add_system(
            SystemBuilder::new("integrate")
                .write::<Position>()
                .write::<Velocity>()
                .after("gravity"),
            |ctx| {
                if let Ok(q) = ctx.query::<(&mut Position, &mut Velocity), ()>() {
                    for (mut p, mut v) in q {
                        for i in 0..2 {
                            p.0[i] += v.0[i] * DT;
                            if p.0[i].abs() > HALF {
                                p.0[i] = p.0[i].clamp(-HALF, HALF);
                                v.0[i] = -v.0[i] * 0.9;
                            }
                        }
                    }
                }
            },
        )?;
        Ok(())
    }
}

/// FNV-1a hash of all particle state, ordered by entity id (thread-independent).
pub fn state_hash(world: &mut World) -> Result<u64, EcsError> {
    let mut rows: Vec<(u64, [u32; 4])> = world
        .query::<(canopy_ecs::Entity, &Position, &Velocity), ()>()?
        .map(|(e, p, v)| {
            (
                e.to_bits(),
                [
                    p.0[0].to_bits(),
                    p.0[1].to_bits(),
                    v.0[0].to_bits(),
                    v.0[1].to_bits(),
                ],
            )
        })
        .collect();
    rows.sort_unstable_by_key(|r| r.0);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (e, vals) in rows {
        for x in std::iter::once(e).chain(vals.iter().map(|&v| u64::from(v))) {
            for b in x.to_le_bytes() {
                h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
            }
        }
    }
    Ok(h)
}
