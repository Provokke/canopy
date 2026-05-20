//! The [`App`]: a world plus its schedules.
use crate::{Plugin, PluginGroup, PluginGroupBuilder, Time};
use canopy_ecs::event::update_events;
use canopy_ecs::schedule::{Executor, SystemBuilder};
use canopy_ecs::{EcsError, Events, Schedule, World};
use std::any::TypeId;
use std::collections::HashSet;

/// Default fixed time step (deterministic, not wall-clock).
pub const DT: f32 = 1.0 / 60.0;

/// World plus Startup (once), First, FixedUpdate (accumulator-driven), Update and
/// PostUpdate schedules. Per frame they run in that order.
pub struct App {
    /// The ECS world.
    pub world: World,
    /// Runs once before the first update.
    pub startup: Schedule,
    /// Runs every frame, first (before FixedUpdate).
    pub first: Schedule,
    /// Runs zero or more times per frame, once per elapsed `fixed_step`.
    pub fixed_update: Schedule,
    /// Runs every frame, after FixedUpdate.
    pub update: Schedule,
    /// Runs every frame, after Update.
    pub post_update: Schedule,
    /// Fixed time step in seconds.
    pub fixed_step: f32,
    /// Cap on fixed steps per frame (avoids a spiral of death).
    pub max_fixed_steps: u32,
    accumulator: f32,
    started: bool,
    plugins: HashSet<TypeId>,
}

impl App {
    /// Creates an app whose schedules use `executor`.
    pub fn new(executor: Executor) -> Self {
        Self {
            world: World::new(),
            startup: Schedule::new(executor),
            first: Schedule::new(executor),
            fixed_update: Schedule::new(executor),
            update: Schedule::new(executor),
            post_update: Schedule::new(executor),
            fixed_step: DT,
            max_fixed_steps: 8,
            accumulator: 0.0,
            started: false,
            plugins: HashSet::new(),
        }
    }

    /// Applies a plugin. Adding a plugin type that was already added is a no-op, so
    /// shared dependencies can be requested by several plugins.
    pub fn add_plugin<P: Plugin>(&mut self, p: P) -> Result<&mut Self, EcsError> {
        self.add_boxed_plugin(TypeId::of::<P>(), &p)?;
        Ok(self)
    }

    pub(crate) fn add_boxed_plugin(&mut self, id: TypeId, p: &dyn Plugin) -> Result<(), EcsError> {
        if self.plugins.insert(id) {
            p.build(self)?;
        }
        Ok(())
    }

    /// Applies every plugin of a group, in the group's order.
    pub fn add_plugins(&mut self, group: impl PluginGroup) -> Result<&mut Self, EcsError> {
        let mut builder = PluginGroupBuilder::new();
        group.build(&mut builder);
        builder.finish(self)?;
        Ok(self)
    }

    /// Applies a group after letting `customise` edit its plugin list.
    pub fn add_plugins_with(
        &mut self,
        group: impl PluginGroup,
        customise: impl FnOnce(PluginGroupBuilder) -> PluginGroupBuilder,
    ) -> Result<&mut Self, EcsError> {
        let mut builder = PluginGroupBuilder::new();
        group.build(&mut builder);
        customise(builder).finish(self)?;
        Ok(self)
    }

    /// Registers event type `E`: inserts its [`Events`] resource and advances the
    /// buffers once per frame in the `First` schedule, so an event is readable for the
    /// frame it was sent in and the next one.
    pub fn add_event<E: Send + Sync + 'static>(&mut self) -> Result<&mut Self, EcsError> {
        if self.world.resource::<Events<E>>().is_none() {
            self.world.insert_resource(Events::<E>::default());
            self.first.add_fn(
                SystemBuilder::new(format!("update_events<{}>", std::any::type_name::<E>())),
                update_events::<E>,
            )?;
        }
        Ok(self)
    }

    /// Advances one frame of `fixed_step` seconds (deterministic).
    pub fn update(&mut self) -> Result<(), EcsError> {
        self.advance(self.fixed_step)
    }

    /// Advances one frame of `dt` seconds: updates [`Time`] (if present), then Startup
    /// (first call only), First, FixedUpdate as many times as whole `fixed_step`s have
    /// accumulated (capped), Update, then PostUpdate. Returns after the first failing
    /// schedule.
    pub fn advance(&mut self, dt: f32) -> Result<(), EcsError> {
        if let Some(t) = self.world.resource_mut::<Time>() {
            t.advance(dt);
            t.fixed_step = self.fixed_step;
        }
        if !self.started {
            self.startup.run(&mut self.world)?;
            self.started = true;
        }
        self.first.run(&mut self.world)?;
        self.accumulator += dt;
        let mut steps = 0;
        while self.accumulator >= self.fixed_step && steps < self.max_fixed_steps {
            self.fixed_update.run(&mut self.world)?;
            self.accumulator -= self.fixed_step;
            steps += 1;
        }
        if steps == self.max_fixed_steps {
            self.accumulator = self.accumulator.min(self.fixed_step);
        }
        self.update.run(&mut self.world)?;
        self.post_update.run(&mut self.world)
    }
}
