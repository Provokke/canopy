use canopy_app::{state_hash, App, ParticlePlugin};
use canopy_ecs::schedule::Executor;

fn run(ex: Executor) -> u64 {
    let mut app = App::new(ex);
    app.add_plugin(ParticlePlugin {
        count: 10_000,
        seed: 42,
    })
    .unwrap();
    for _ in 0..60 {
        app.update().unwrap();
    }
    assert_eq!(app.world.entity_count(), 10_000);
    state_hash(&mut app.world).unwrap()
}

#[test]
fn single_and_multi_threaded_produce_identical_state() {
    let a = run(Executor::SingleThreaded);
    let b = run(Executor::MultiThreaded);
    assert_eq!(a, b);
    println!("state hash after 60 frames: {a:016x}");
}
