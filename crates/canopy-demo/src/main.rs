//! `canopy-demo`: runs the boids simulation and prints per-frame statistics.
use anyhow::{bail, Context, Result};
use canopy_demo::{ascii_render, run, BoidsConfig};

const USAGE: &str = "\
canopy-demo: deterministic boids on canopy-ecs

USAGE:
    canopy-demo [OPTIONS]

OPTIONS:
    --entities <N>     number of boids            (default 10000)
    --frames <N>       frames to simulate         (default 100)
    --threads <N>      worker threads; 1 selects the single-threaded executor
                       (default: available parallelism)
    --seed <N>         initial-state seed         (default 42)
    --print-every <N>  print stats every N frames (default 1; 0 = only the final line)
    --ascii            print an ASCII density map of the final state
    -h, --help         show this help
";

struct Cli {
    cfg: BoidsConfig,
    print_every: usize,
    ascii: bool,
}

fn parse<T: std::str::FromStr>(flag: &str, value: Option<String>) -> Result<T>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let value = value.with_context(|| format!("{flag} needs a value"))?;
    value
        .parse()
        .with_context(|| format!("invalid value {value:?} for {flag}"))
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Option<Cli>> {
    let mut cli = Cli {
        cfg: BoidsConfig {
            threads: std::thread::available_parallelism().map_or(1, usize::from),
            ..BoidsConfig::default()
        },
        print_every: 1,
        ascii: false,
    };
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--entities" => cli.cfg.entities = parse(&arg, args.next())?,
            "--frames" => cli.cfg.frames = parse(&arg, args.next())?,
            "--threads" => cli.cfg.threads = parse(&arg, args.next())?,
            "--seed" => cli.cfg.seed = parse(&arg, args.next())?,
            "--print-every" => cli.print_every = parse(&arg, args.next())?,
            "--ascii" => cli.ascii = true,
            "-h" | "--help" => return Ok(None),
            other => bail!("unknown argument {other:?} (try --help)"),
        }
    }
    if cli.cfg.threads == 0 {
        bail!("--threads must be at least 1");
    }
    Ok(Some(cli))
}

fn main() -> Result<()> {
    let Some(cli) = parse_args(std::env::args().skip(1))? else {
        print!("{USAGE}");
        return Ok(());
    };
    let started = std::time::Instant::now();
    let mut last_hash = 0;
    let mut app = run(&cli.cfg, |s| {
        last_hash = s.hash;
        if cli.print_every != 0 && s.frame % cli.print_every == 0 {
            println!(
                "frame {:>5}  boids {}  mean_speed {:.4}  centroid ({:.3}, {:.3})  hash {:016x}",
                s.frame, s.boids, s.mean_speed, s.centroid[0], s.centroid[1], s.hash
            );
        }
    })?;
    if cli.ascii {
        print!("{}", ascii_render(&mut app, 72, 24)?);
    }
    println!("final hash {last_hash:016x}");
    eprintln!(
        "{} frames, {} boids, {} thread(s): {:.1} ms",
        cli.cfg.frames,
        cli.cfg.entities,
        cli.cfg.threads,
        started.elapsed().as_secs_f64() * 1e3
    );
    Ok(())
}
