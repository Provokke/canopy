use canopy_app::App;
use canopy_ecs::schedule::{Executor, SystemBuilder};
use canopy_ecs::ResMut;

#[derive(Default)]
struct Counts {
    startup: u32,
    fixed: u32,
    update: u32,
}

fn fixed(mut c: ResMut<Counts>) {
    c.fixed += 1;
}

#[test]
fn startup_once_fixed_by_accumulator_update_per_frame() {
    let mut app = App::new(Executor::MultiThreaded);
    app.fixed_step = 0.25;
    app.world.insert_resource(Counts::default());
    app.startup
        .add_system(SystemBuilder::new("s").res_write::<Counts>(), |ctx| {
            ctx.resource_mut::<Counts>().unwrap().unwrap().startup += 1;
        })
        .unwrap();
    app.fixed_update
        .add_fn(SystemBuilder::new("fixed"), fixed)
        .unwrap();
    app.update
        .add_system(SystemBuilder::new("u").res_write::<Counts>(), |ctx| {
            ctx.resource_mut::<Counts>().unwrap().unwrap().update += 1;
        })
        .unwrap();
    app.advance(0.625).unwrap(); // 2 steps, 0.125 left
    app.advance(0.125).unwrap(); // 0.25 -> 1 step
    app.advance(0.0).unwrap(); // none
    app.advance(100.0).unwrap(); // capped at max_fixed_steps (8)
    let c = app.world.resource::<Counts>().unwrap();
    assert_eq!((c.startup, c.fixed, c.update), (1, 2 + 1 + 8, 4));
}

#[derive(Default)]
struct Log(Vec<&'static str>);

fn log_to(s: &mut canopy_ecs::Schedule, name: &'static str) -> Result<(), canopy_ecs::EcsError> {
    s.add_system(SystemBuilder::new(name).res_write::<Log>(), move |ctx| {
        ctx.resource_mut::<Log>().unwrap().unwrap().0.push(name);
    })?;
    Ok(())
}

#[test]
fn schedules_run_startup_first_fixed_update_post_update_in_order() {
    let mut app = App::new(Executor::MultiThreaded);
    app.fixed_step = 0.5;
    app.world.insert_resource(Log::default());
    // Registered out of order on purpose.
    log_to(&mut app.post_update, "post_update").unwrap();
    log_to(&mut app.update, "update").unwrap();
    log_to(&mut app.fixed_update, "fixed").unwrap();
    log_to(&mut app.first, "first").unwrap();
    log_to(&mut app.startup, "startup").unwrap();
    app.advance(1.0).unwrap(); // two fixed steps
    app.advance(0.0).unwrap(); // none
    assert_eq!(
        app.world.resource::<Log>().unwrap().0,
        vec![
            "startup",
            "first",
            "fixed",
            "fixed",
            "update",
            "post_update",
            "first",
            "update",
            "post_update"
        ]
    );
}
