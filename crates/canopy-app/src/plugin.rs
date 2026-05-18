//! Plugins and plugin groups.
use crate::{App, TimePlugin, TransformPlugin};
use canopy_ecs::EcsError;
use std::any::TypeId;

/// A unit of app configuration.
pub trait Plugin: 'static {
    /// Registers systems/resources on the app.
    fn build(&self, app: &mut App) -> Result<(), EcsError>;
}

/// An ordered collection of plugins that is added to an app as one unit.
pub trait PluginGroup {
    /// Adds this group's plugins, in order, to `group`.
    fn build(self, group: &mut PluginGroupBuilder);
}

/// Collects the plugins of a [`PluginGroup`] so callers can adjust them before the
/// group is applied with [`App::add_plugins`].
#[derive(Default)]
pub struct PluginGroupBuilder {
    plugins: Vec<(TypeId, Box<dyn Plugin>)>,
}

impl PluginGroupBuilder {
    /// An empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a plugin. A plugin type that is already present is replaced in
    /// place (keeping its position), so groups can be customised.
    #[allow(clippy::should_implement_trait)]
    pub fn add<P: Plugin>(mut self, plugin: P) -> Self {
        let id = TypeId::of::<P>();
        match self.plugins.iter_mut().find(|(t, _)| *t == id) {
            Some(slot) => slot.1 = Box::new(plugin),
            None => self.plugins.push((id, Box::new(plugin))),
        }
        self
    }

    /// Removes the plugin of type `P`, if present.
    pub fn disable<P: Plugin>(mut self) -> Self {
        let id = TypeId::of::<P>();
        self.plugins.retain(|(t, _)| *t != id);
        self
    }

    /// Whether a plugin of type `P` is in the group.
    pub fn contains<P: Plugin>(&self) -> bool {
        self.plugins.iter().any(|(t, _)| *t == TypeId::of::<P>())
    }

    /// Number of plugins.
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Whether the group has no plugins.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Builds every plugin, in order, onto `app`.
    pub(crate) fn finish(self, app: &mut App) -> Result<(), EcsError> {
        for (id, plugin) in self.plugins {
            app.add_boxed_plugin(id, plugin.as_ref())?;
        }
        Ok(())
    }
}

/// The plugins every app wants: [`TimePlugin`] then [`TransformPlugin`].
#[derive(Default)]
pub struct CorePlugins;

impl PluginGroup for CorePlugins {
    fn build(self, group: &mut PluginGroupBuilder) {
        let g = std::mem::take(group);
        *group = g.add(TimePlugin).add(TransformPlugin);
    }
}
