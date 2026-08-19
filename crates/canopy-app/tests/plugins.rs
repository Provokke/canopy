use canopy_app::{App, CorePlugins, Plugin, PluginGroup, PluginGroupBuilder, Time, TimePlugin};
use canopy_app::{TransformPlugin, DT};
use canopy_ecs::schedule::{Executor, SystemBuilder};
use canopy_ecs::{EcsError, Events, GlobalTransform, Transform};

#[derive(Default)]
struct Log(Vec<&'static str>);

macro_rules! logging_plugin {
    ($name:ident) => {
        struct $name;
        impl Plugin for $name {
            fn build(&self, app: &mut App) -> Result<(), EcsError> {
                if app.world.resource::<Log>().is_none() {
                    app.world.insert_resource(Log::default());
                }
                if let Some(l) = app.world.resource_mut::<Log>() {
                    l.0.push(stringify!($name));
                }
                Ok(())
            }
        }
    };
}
logging_plugin!(A);
logging_plugin!(B);
logging_plugin!(C);

struct Abc;
impl PluginGroup for Abc {
    fn build(self, group: &mut PluginGroupBuilder) {
        let g = std::mem::take(group);
        *group = g.add(A).add(B).add(C);
    }
}

fn log(app: &App) -> Vec<&'static str> {
    app.world.resource::<Log>().unwrap().0.clone()
}

#[test]
fn plugin_groups_build_in_order() {
    let mut app = App::new(Executor::SingleThreaded);
    app.add_plugins(Abc).unwrap();
    assert_eq!(log(&app), ["A", "B", "C"]);
}

#[test]
fn groups_can_be_customised() {
    let mut app = App::new(Executor::SingleThreaded);
    app.add_plugins_with(Abc, |g| {
        assert!(g.contains::<B>());
        g.disable::<B>().add(A) // re-adding A keeps its slot
    })
    .unwrap();
    assert_eq!(log(&app), ["A", "C"]);
}

#[test]
fn adding_a_plugin_twice_builds_it_once() {
    let mut app = App::new(Executor::SingleThreaded);
    app.add_plugin(A).unwrap();
    app.add_plugin(A).unwrap();
    app.add_plugins(Abc).unwrap(); // A already present: only B and C are new
    assert_eq!(log(&app), ["A", "B", "C"]);
}

#[test]
fn time_plugin_tracks_the_frames_advanced() {
    let mut app = App::new(Executor::SingleThreaded);
    app.add_plugin(TimePlugin).unwrap();
    app.advance(0.5).unwrap();
    app.advance(0.25).unwrap();
    app.advance(0.25).unwrap();
    let t = *app.world.resource::<Time>().unwrap();
    assert_eq!((t.frame, t.elapsed, t.delta), (3, 1.0, 0.25));
    assert_eq!(t.fixed_step, DT);
    // A system sees the time of the frame it runs in.
    app.update
        .add_system(
            SystemBuilder::new("t")
                .res_read::<Time>()
                .res_write::<Log>(),
            |ctx| {
                let t = ctx.resource::<Time>().ok().flatten().map_or(0, |t| t.frame);
                if let Ok(Some(l)) = ctx.resource_mut::<Log>() {
                    l.0.push(if t == 4 { "frame4" } else { "other" });
                }
            },
        )
        .unwrap();
    app.world.insert_resource(Log::default());
    app.advance(0.1).unwrap();
    assert_eq!(log(&app), ["frame4"]);
}

#[test]
fn transform_plugin_propagates_every_frame() {
    let mut app = App::new(Executor::MultiThreaded);
    app.add_plugins(CorePlugins).unwrap();
    let parent = app
        .world
        .spawn(Transform::from_translation([5.0, 0.0, 0.0]));
    let child = app
        .world
        .spawn(Transform::from_translation([0.0, 2.0, 0.0]));
    app.world.set_parent(child, parent).unwrap();
    app.update().unwrap();
    let g = |app: &App| {
        app.world
            .get::<GlobalTransform>(child)
            .unwrap()
            .0
            .translation
    };
    assert_eq!(g(&app), [5.0, 2.0, 0.0]);
    app.world.get_mut::<Transform>(parent).unwrap().translation[0] = 7.0;
    app.update().unwrap();
    assert_eq!(g(&app), [7.0, 2.0, 0.0]);
}

#[test]
fn transform_plugin_alone_does_not_need_time() {
    let mut app = App::new(Executor::SingleThreaded);
    app.add_plugin(TransformPlugin).unwrap();
    let e = app.world.spawn(Transform::IDENTITY);
    app.update().unwrap();
    assert!(app.world.get::<GlobalTransform>(e).is_some());
    assert!(app.world.resource::<Time>().is_none());
}

#[test]
fn add_event_advances_buffers_each_frame() {
    let mut app = App::new(Executor::SingleThreaded);
    app.add_event::<u32>().unwrap();
    app.add_event::<u32>().unwrap(); // idempotent: still one update system
    app.world.resource_mut::<Events<u32>>().unwrap().send(1);
    let len = |app: &App| app.world.resource::<Events<u32>>().unwrap().len();
    assert_eq!(len(&app), 1);
    app.update().unwrap();
    assert_eq!(len(&app), 1, "alive one update later");
    app.update().unwrap();
    assert_eq!(len(&app), 0, "gone after two updates");
}
