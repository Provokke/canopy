use canopy_demo::{run, BoidsConfig, FrameStats};
use std::process::Command;

const N: usize = if cfg!(miri) { 50 } else { 2000 };

fn stats(threads: usize, seed: u64) -> Vec<FrameStats> {
    let cfg = BoidsConfig {
        entities: N,
        frames: 25,
        threads,
        seed,
    };
    let mut out = Vec::new();
    run(&cfg, |s| out.push(s.clone())).unwrap();
    out
}

#[test]
fn every_frame_hash_is_identical_for_one_and_four_threads() {
    let a = stats(1, 7);
    let b = stats(4, 7);
    assert_eq!(a.len(), 25);
    assert_eq!(a, b);
}

#[test]
fn the_simulation_actually_evolves_and_depends_on_the_seed() {
    let a = stats(1, 7);
    assert_ne!(a[0].hash, a[24].hash, "state must change over frames");
    assert_ne!(a[0].hash, stats(1, 8)[0].hash, "seed must matter");
    assert!(a.iter().all(|s| s.boids == N && s.mean_speed > 0.0));
}

fn cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_canopy-demo"))
        .args(args)
        .output()
        .unwrap()
}

fn final_hash(args: &[&str]) -> String {
    let out = cli(args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    stdout
        .lines()
        .find_map(|l| l.strip_prefix("final hash ").map(str::to_owned))
        .unwrap()
}

#[test]
fn cli_prints_identical_final_hash_for_one_and_four_threads() {
    let base = ["--entities", "3000", "--frames", "30", "--print-every", "0"];
    let h1 = final_hash(&[&base[..], &["--threads", "1"]].concat());
    let h4 = final_hash(&[&base[..], &["--threads", "4"]].concat());
    assert_eq!(h1, h4);
    assert_eq!(h1.len(), 16);
}

#[test]
fn cli_prints_per_frame_stats_and_rejects_bad_arguments() {
    let out = cli(&["--entities", "100", "--frames", "3", "--threads", "2"]);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        stdout.lines().filter(|l| l.starts_with("frame ")).count(),
        3
    );
    assert!(stdout.contains("boids 100") && stdout.contains("hash "));
    assert!(!cli(&["--frames", "many"]).status.success());
    assert!(!cli(&["--bogus"]).status.success());
    assert!(!cli(&["--threads", "0"]).status.success());
    assert!(cli(&["--help"]).status.success());
}

fn boid0(frames: usize) -> [f32; 2] {
    let cfg = BoidsConfig {
        entities: 200,
        frames,
        threads: 2,
        seed: 3,
    };
    let mut app = run(&cfg, |_| {}).unwrap();
    let found = app
        .world
        .query::<(&canopy_demo::BoidId, &canopy_app::Position), ()>()
        .unwrap()
        .find(|(id, _)| id.0 == 0)
        .map(|(_, p)| p.0);
    found.unwrap()
}

#[test]
fn boids_move_by_at_least_their_minimum_speed() {
    let (a, b) = (boid0(1), boid0(21));
    let moved = ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt();
    // 20 frames at >= 2 units/s and dt = 1/60 s, minus bounces off the walls.
    assert!(moved > 0.3, "moved only {moved}");
}
