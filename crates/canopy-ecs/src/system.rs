//! Plain-function systems: parameters (`Query`, `Res`, `ResMut`, ...) declare their
//! own [`FilteredAccess`], which the schedule collects into a [`SystemAccess`] and
//! checks for conflicts at add time (archetype-disjoint queries do not conflict).
use crate::error::EcsError;
use crate::query::{
    Access, ArchetypeCache, FilteredAccess, QueryData, QueryFilter, QueryIter, SystemAccess,
};
use crate::schedule::SystemContext;
use crate::world::{Tick, World};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};

/// A value a function system can take as a parameter.
///
/// # Safety
/// `access` must declare everything `fetch` touches, as one or more
/// [`FilteredAccess`] entries whose archetype guarantees (`with`/`without`) hold
/// for every archetype the parameter can reach. Entries of one system are checked
/// to be pairwise non-conflicting before the system can run.
pub unsafe trait SystemParam {
    /// The parameter as seen inside the system for world lifetime `'w`.
    type Item<'w>;
    /// Per-system persistent state.
    type State: Default + Send + 'static;
    /// Declares accessed components/resources.
    fn access(a: &mut SystemAccess) -> Result<(), EcsError>;
    /// Builds the parameter.
    ///
    /// # Safety
    /// The system's declared access (which includes this parameter's) must be
    /// honoured by the scheduler, and parameters must not conflict with each other.
    unsafe fn fetch<'w>(
        ctx: &SystemContext<'w>,
        state: &'w mut Self::State,
    ) -> Result<Self::Item<'w>, EcsError>;
}

/// Query parameter.
pub struct Query<'w, D: QueryData, F: QueryFilter = ()> {
    world: &'w World,
    cache: &'w mut ArchetypeCache,
    last_run: Tick,
    this_run: Tick,
    _p: PhantomData<fn() -> (D, F)>,
}

impl<D: QueryData, F: QueryFilter> Query<'_, D, F> {
    /// Iterates matching entities. Borrows `self` mutably so items can't alias
    /// those of another live iterator from the same query.
    ///
    /// Archetypes are matched incrementally: only those created since the previous
    /// call are tested (the cache lives in the system's `SystemParam::State`).
    pub fn iter_mut(&mut self) -> QueryIter<'_, D, F> {
        let matched = self.cache.update::<D, F>(self.world);
        // SAFETY: access declared and conflict-checked at system add time; `matched`
        // holds only archetypes matching D and F.
        unsafe {
            QueryIter::with_matched(
                self.world,
                std::borrow::Cow::Borrowed(matched),
                self.last_run,
                self.this_run,
            )
        }
    }
}

// SAFETY: declares exactly D's and F's access, with archetype guarantees taken from
// `D::archetype_filter`/`F::archetype_filter` (sound per those traits' contracts);
// iteration goes through QueryIter, which only visits archetypes matching D and F.
unsafe impl<D: QueryData, F: QueryFilter> SystemParam for Query<'_, D, F> {
    type Item<'w> = Query<'w, D, F>;
    type State = ArchetypeCache;
    fn access(a: &mut SystemAccess) -> Result<(), EcsError> {
        // Carries D's/F's archetype guarantees: iteration only visits archetypes
        // where both `D::matches` and `F::matches` hold.
        a.add(FilteredAccess::of::<D, F>()?)
    }
    unsafe fn fetch<'w>(
        ctx: &SystemContext<'w>,
        state: &'w mut ArchetypeCache,
    ) -> Result<Query<'w, D, F>, EcsError> {
        Ok(Query {
            world: ctx.world,
            cache: state,
            last_run: ctx.last_run,
            this_run: ctx.this_run,
            _p: PhantomData,
        })
    }
}

/// Shared resource parameter.
pub struct Res<'w, R>(&'w R);
impl<R> Deref for Res<'_, R> {
    type Target = R;
    fn deref(&self) -> &R {
        self.0
    }
}

// SAFETY: declares a read of R and only creates a shared reference.
unsafe impl<R: Send + Sync + 'static> SystemParam for Res<'_, R> {
    type Item<'w> = Res<'w, R>;
    type State = ();
    fn access(a: &mut SystemAccess) -> Result<(), EcsError> {
        let mut r = Access::default();
        r.add_read::<R>()?;
        a.add_unfiltered(r)
    }
    unsafe fn fetch<'w>(
        ctx: &SystemContext<'w>,
        _state: &'w mut (),
    ) -> Result<Res<'w, R>, EcsError> {
        ctx.world
            .resource::<R>()
            .map(Res)
            .ok_or(EcsError::MissingResource(std::any::type_name::<R>()))
    }
}

/// Exclusive resource parameter.
pub struct ResMut<'w, R>(&'w mut R);
impl<R> Deref for ResMut<'_, R> {
    type Target = R;
    fn deref(&self) -> &R {
        self.0
    }
}
impl<R> DerefMut for ResMut<'_, R> {
    fn deref_mut(&mut self) -> &mut R {
        self.0
    }
}

// SAFETY: declares a write of R; exclusivity is guaranteed by the scheduler and the
// per-system conflict check.
unsafe impl<R: Send + Sync + 'static> SystemParam for ResMut<'_, R> {
    type Item<'w> = ResMut<'w, R>;
    type State = ();
    fn access(a: &mut SystemAccess) -> Result<(), EcsError> {
        let mut w = Access::default();
        w.add_write::<R>()?;
        a.add_unfiltered(w)
    }
    unsafe fn fetch<'w>(
        ctx: &SystemContext<'w>,
        _state: &'w mut (),
    ) -> Result<ResMut<'w, R>, EcsError> {
        // SAFETY: forwarded from the trait contract.
        unsafe { ctx.world.resource_unchecked::<R>() }
            .map(ResMut)
            .ok_or(EcsError::MissingResource(std::any::type_name::<R>()))
    }
}

/// Deferred world mutations, applied in system order after the schedule run.
///
/// [`Commands::spawn`] returns the new entity's id immediately: the id is *reserved*
/// atomically ([`World::reserve_entity`]) and becomes alive when the schedule applies
/// the queued commands, so later commands (and other systems' commands) can refer to it.
pub struct Commands<'w> {
    queue: std::sync::Arc<std::sync::Mutex<Vec<crate::schedule::Command>>>,
    world: &'w World,
}

impl Commands<'_> {
    /// Queues an arbitrary world mutation.
    pub fn add(&mut self, f: impl FnOnce(&mut World) + Send + 'static) {
        if let Ok(mut q) = self.queue.lock() {
            q.push(Box::new(f));
        }
    }
    /// Reserves an id and queues the spawn of an entity with `bundle` on it.
    pub fn spawn(&mut self, bundle: impl crate::world::Bundle) -> crate::Entity {
        let e = self.world.reserve_entity();
        self.add(move |w| {
            let _ = w.insert(e, bundle);
        });
        e
    }
    /// Reserves an id for a component-less entity (alive once commands are applied).
    pub fn spawn_empty(&mut self) -> crate::Entity {
        self.world.reserve_entity()
    }
    /// Queues inserting `bundle` on `e` (no-op if `e` is gone by then).
    pub fn insert(&mut self, e: crate::Entity, bundle: impl crate::world::Bundle) {
        self.add(move |w| {
            let _ = w.insert(e, bundle);
        });
    }
    /// Queues a despawn (no-op if already gone). Children are left alone; see
    /// [`Commands::despawn_recursive`].
    pub fn despawn(&mut self, e: crate::Entity) {
        self.add(move |w| {
            w.despawn(e);
        });
    }
    /// Queues a despawn of `e` and all its descendants.
    pub fn despawn_recursive(&mut self, e: crate::Entity) {
        self.add(move |w| {
            w.despawn_recursive(e);
        });
    }
    /// Queues making `child` a child of `parent` (no-op on error, e.g. a cycle).
    pub fn set_parent(&mut self, child: crate::Entity, parent: crate::Entity) {
        self.add(move |w| {
            let _ = w.set_parent(child, parent);
        });
    }
}

// SAFETY: no component or resource access; only pushes into the system's own command
// queue and reserves entity ids through the world's atomic cursor (`&World`).
unsafe impl SystemParam for Commands<'_> {
    type Item<'w> = Commands<'w>;
    type State = ();
    fn access(_: &mut SystemAccess) -> Result<(), EcsError> {
        Ok(())
    }
    unsafe fn fetch<'w>(
        ctx: &SystemContext<'w>,
        _state: &'w mut (),
    ) -> Result<Commands<'w>, EcsError> {
        Ok(Commands {
            queue: ctx.commands.clone(),
            world: ctx.world,
        })
    }
}

/// Sends events of type `E` (requires an `Events<E>` resource).
pub struct EventWriter<'w, E: Send + Sync + 'static>(ResMut<'w, crate::event::Events<E>>);

impl<E: Send + Sync + 'static> EventWriter<'_, E> {
    /// Sends one event.
    pub fn send(&mut self, e: E) {
        self.0.send(e);
    }
}

// SAFETY: identical access to `ResMut<Events<E>>`, which it wraps.
unsafe impl<E: Send + Sync + 'static> SystemParam for EventWriter<'_, E> {
    type Item<'w> = EventWriter<'w, E>;
    type State = ();
    fn access(a: &mut SystemAccess) -> Result<(), EcsError> {
        <ResMut<crate::event::Events<E>> as SystemParam>::access(a)
    }
    unsafe fn fetch<'w>(
        ctx: &SystemContext<'w>,
        _state: &'w mut (),
    ) -> Result<EventWriter<'w, E>, EcsError> {
        // SAFETY: forwarded contract.
        unsafe { <ResMut<crate::event::Events<E>> as SystemParam>::fetch(ctx, _state) }
            .map(EventWriter)
    }
}

/// Per-system local state, persisted across runs.
pub struct Local<'s, T>(&'s mut T);
impl<T> Deref for Local<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.0
    }
}
impl<T> DerefMut for Local<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.0
    }
}

// SAFETY: touches only its own state, never the world.
unsafe impl<T: Default + Send + 'static> SystemParam for Local<'_, T> {
    type Item<'w> = Local<'w, T>;
    type State = T;
    fn access(_: &mut SystemAccess) -> Result<(), EcsError> {
        Ok(())
    }
    unsafe fn fetch<'w>(_: &SystemContext<'w>, state: &'w mut T) -> Result<Local<'w, T>, EcsError> {
        Ok(Local(state))
    }
}

/// Reads events of type `E` not yet seen by this system.
pub struct EventReader<'w, E: Send + Sync + 'static> {
    events: &'w crate::event::Events<E>,
    cursor: &'w mut crate::event::EventCursor,
}

impl<'w, E: Send + Sync + 'static> EventReader<'w, E> {
    /// Iterates unseen events and marks them seen.
    pub fn read(&mut self) -> impl Iterator<Item = &'w E> + 'w {
        self.events.read(self.cursor)
    }
}

// SAFETY: declares a read of `Events<E>`; the cursor is per-system state.
unsafe impl<E: Send + Sync + 'static> SystemParam for EventReader<'_, E> {
    type Item<'w> = EventReader<'w, E>;
    type State = crate::event::EventCursor;
    fn access(a: &mut SystemAccess) -> Result<(), EcsError> {
        <Res<crate::event::Events<E>> as SystemParam>::access(a)
    }
    unsafe fn fetch<'w>(
        ctx: &SystemContext<'w>,
        state: &'w mut crate::event::EventCursor,
    ) -> Result<EventReader<'w, E>, EcsError> {
        let events =
            ctx.world
                .resource::<crate::event::Events<E>>()
                .ok_or(EcsError::MissingResource(std::any::type_name::<
                    crate::event::Events<E>,
                >()))?;
        Ok(EventReader {
            events,
            cursor: state,
        })
    }
}

/// A function usable as a system. Implemented for `fn(P0, .., Pn)` with
/// `SystemParam` arguments (up to 12).
pub trait SystemParamFunction<Marker>: Send + 'static {
    /// Adds every parameter's access to `set`; fails if two conflict.
    fn access(&self, set: &mut SystemAccess) -> Result<(), EcsError>;
    /// Per-system state of all parameters.
    type State: Default + Send + 'static;
    /// Fetches parameters and calls the function.
    fn run(&mut self, ctx: &SystemContext<'_>, state: &mut Self::State) -> Result<(), EcsError>;
}

macro_rules! impl_fn {
    ($($p:ident),*) => {
        impl<Func, $($p: SystemParam),*> SystemParamFunction<fn($($p,)*)> for Func
        where
            Func: Send + 'static,
            for<'a> &'a mut Func: FnMut($($p),*) + FnMut($($p::Item<'_>),*),
        {
            type State = ($($p::State,)*);
            fn access(&self, _set: &mut SystemAccess) -> Result<(), EcsError> {
                $($p::access(_set)?;)*
                Ok(())
            }
            #[allow(non_snake_case, clippy::too_many_arguments)]
            fn run(&mut self, _ctx: &SystemContext<'_>, _state: &mut Self::State) -> Result<(), EcsError> {
                fn call_inner<$($p),*>(mut f: impl FnMut($($p),*), $($p: $p),*) {
                    f($($p),*)
                }
                let ($($p,)*) = _state;
                $(
                    // SAFETY: `access` was merged into the system's declared access (so
                    // the scheduler honours it) and conflicts between parameters were rejected.
                    let $p = unsafe { $p::fetch(_ctx, $p)? };
                )*
                call_inner(self, $($p),*);
                Ok(())
            }
        }
    };
}
impl_fn!();
impl_fn!(P0);
impl_fn!(P0, P1);
impl_fn!(P0, P1, P2);
impl_fn!(P0, P1, P2, P3);
impl_fn!(P0, P1, P2, P3, P4);
impl_fn!(P0, P1, P2, P3, P4, P5);
impl_fn!(P0, P1, P2, P3, P4, P5, P6);
impl_fn!(P0, P1, P2, P3, P4, P5, P6, P7);
impl_fn!(P0, P1, P2, P3, P4, P5, P6, P7, P8);
impl_fn!(P0, P1, P2, P3, P4, P5, P6, P7, P8, P9);
impl_fn!(P0, P1, P2, P3, P4, P5, P6, P7, P8, P9, P10);
impl_fn!(P0, P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11);
