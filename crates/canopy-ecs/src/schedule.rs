//! Systems with declared access and a parallel, conflict-aware executor.
//!
//! Every system declares its component/resource reads and writes. When the
//! schedule is built, explicit `.after()` edges are added first; then every
//! pair of conflicting systems that is not already ordered gets an implicit
//! edge in insertion order (and is reported as an *ambiguity*). Hence any two
//! systems that can be ready at the same time never conflict, and running them
//! on separate threads is sound. Access is enforced at runtime: a system that
//! queries data it did not declare gets [`EcsError::UndeclaredAccess`].
//!
//! The executor is a ready queue: each system keeps a count of unfinished
//! dependencies, and finishing a system decrements its dependents' counts and
//! starts every system whose count reached zero (lowest index first), without
//! waiting for unrelated systems.
use crate::error::EcsError;
use crate::query::{Access, QueryData, QueryFilter, QueryIter, SystemAccess};
use crate::set::{InternedSet, SystemSet};
use crate::world::{Component, Tick, World};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::mpsc;
use std::sync::Mutex;

pub(crate) type Command = Box<dyn FnOnce(&mut World) + Send>;
type SystemFn = Box<dyn FnMut(&SystemContext<'_>) -> Result<(), EcsError> + Send>;

/// A run condition: evaluated on the scheduling thread with shared world access
/// while **no** system of the schedule is running, so it can read anything without
/// declaring access.
pub type Condition = Box<dyn FnMut(&World) -> bool + Send>;

/// Condition: true while resource `R` exists.
pub fn resource_exists<R: Send + Sync + 'static>() -> impl FnMut(&World) -> bool + Send + 'static {
    |w: &World| w.resource::<R>().is_some()
}

/// Condition: true on the first evaluation only.
pub fn run_once() -> impl FnMut(&World) -> bool + Send + 'static {
    let mut ran = false;
    move |_: &World| !std::mem::replace(&mut ran, true)
}

/// Target of `.before()` / `.after()`: a system name or a system set.
#[derive(Clone, Debug, PartialEq)]
pub enum OrderLabel {
    /// Every system with this name.
    System(String),
    /// Every system in this set (directly or through nested sets).
    Set(InternedSet),
}

/// Converts names (`&str`, `String`) and [`SystemSet`] labels into an [`OrderLabel`].
pub trait IntoOrderLabel {
    /// The label.
    fn into_order_label(self) -> OrderLabel;
}
impl IntoOrderLabel for &str {
    fn into_order_label(self) -> OrderLabel {
        OrderLabel::System(self.to_string())
    }
}
impl IntoOrderLabel for String {
    fn into_order_label(self) -> OrderLabel {
        OrderLabel::System(self)
    }
}
impl IntoOrderLabel for &String {
    fn into_order_label(self) -> OrderLabel {
        OrderLabel::System(self.clone())
    }
}
impl IntoOrderLabel for InternedSet {
    fn into_order_label(self) -> OrderLabel {
        OrderLabel::Set(self)
    }
}
impl<S: SystemSet> IntoOrderLabel for S {
    fn into_order_label(self) -> OrderLabel {
        OrderLabel::Set(crate::set::intern(self))
    }
}

/// Configuration of a system set: ordering against other sets/systems, parent
/// sets, and run conditions. Apply with [`Schedule::configure_set`].
pub struct SetConfig {
    set: InternedSet,
    after: Vec<OrderLabel>,
    before: Vec<OrderLabel>,
    parents: Vec<InternedSet>,
    conditions: Vec<Condition>,
}

impl SetConfig {
    /// Starts configuring `set`.
    pub fn new(set: impl SystemSet) -> Self {
        Self {
            set: crate::set::intern(set),
            after: Vec::new(),
            before: Vec::new(),
            parents: Vec::new(),
            conditions: Vec::new(),
        }
    }
    /// Every member of this set runs after every system matched by `label`.
    pub fn after(mut self, label: impl IntoOrderLabel) -> Self {
        self.after.push(label.into_order_label());
        self
    }
    /// Every member of this set runs before every system matched by `label`.
    pub fn before(mut self, label: impl IntoOrderLabel) -> Self {
        self.before.push(label.into_order_label());
        self
    }
    /// Nests this set inside `parent`: its members are members of `parent` (for
    /// ordering and for `parent`'s run conditions).
    pub fn in_set(mut self, parent: impl SystemSet) -> Self {
        self.parents.push(crate::set::intern(parent));
        self
    }
    /// Members run only if `cond` is true. Evaluated at most once per schedule run,
    /// when the first member is about to run.
    pub fn run_if(mut self, cond: impl FnMut(&World) -> bool + Send + 'static) -> Self {
        self.conditions.push(Box::new(cond));
        self
    }
}

/// Execution context handed to a running system.
pub struct SystemContext<'w> {
    pub(crate) world: &'w World,
    name: &'w str,
    access: &'w SystemAccess,
    pub(crate) commands: std::sync::Arc<Mutex<Vec<Command>>>,
    pub(crate) last_run: Tick,
    pub(crate) this_run: Tick,
    // Set once a mutable query has been created; prevents two live mutable
    // iterators over the same declared data inside one system.
    mutable_borrowed: std::sync::atomic::AtomicBool,
}

impl<'w> SystemContext<'w> {
    fn check(&self, needed: &Access) -> Result<(), EcsError> {
        self.access
            .covers(needed)
            .map_err(|c| EcsError::UndeclaredAccess {
                system: self.name.to_string(),
                component: c,
            })
    }

    /// Runs a query. Queries with writes may be created at most once per system run
    /// (a second one returns [`EcsError::ConflictingQueryAccess`]), so two live
    /// mutable iterators can never alias. Matching archetypes come from a per-world
    /// cache shared by all systems (behind a mutex held only while it is updated), so
    /// only archetypes created since the last such query are tested.
    pub fn query<D: QueryData + 'static, F: QueryFilter + 'static>(
        &self,
    ) -> Result<QueryIter<'w, D, F>, EcsError> {
        let (world, last, this) = self.claim::<D, F>()?;
        let matched = world.cached_matches_shared::<D, F>();
        // SAFETY: the schedule guarantees no concurrently running system conflicts with
        // this system's declared access, which covers the query (checked in `claim`);
        // only one writing query is created per run, so `&mut` items cannot alias;
        // `matched` is this world's cache for `(D, F)`.
        Ok(unsafe { QueryIter::with_matched(world, std::borrow::Cow::Owned(matched), last, this) })
    }

    /// Checks access for a query and claims the per-run mutable slot if it writes.
    pub(crate) fn claim<D: QueryData, F: QueryFilter>(
        &self,
    ) -> Result<(&'w World, Tick, Tick), EcsError> {
        let mut a = Access::default();
        D::access(&mut a)?;
        F::access(&mut a)?;
        self.check(&a)?;
        if !a.writes.is_empty()
            && self
                .mutable_borrowed
                .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return Err(EcsError::ConflictingQueryAccess(
                "second mutable query in one system",
            ));
        }
        Ok((self.world, self.last_run, self.this_run))
    }

    /// Reads a resource (must be declared with `res_read` or `res_write`).
    pub fn resource<R: Send + Sync + 'static>(&self) -> Result<Option<&'w R>, EcsError> {
        let mut a = Access::default();
        a.add_read::<R>()?;
        self.check(&a)?;
        Ok(self.world.resource::<R>())
    }

    /// Mutably accesses a resource (must be declared with `res_write`). Like
    /// mutable queries, at most one mutable borrow per system run.
    pub fn resource_mut<R: Send + Sync + 'static>(&self) -> Result<Option<&'w mut R>, EcsError> {
        let mut a = Access::default();
        a.add_write::<R>()?;
        self.check(&a)?;
        if self
            .mutable_borrowed
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return Err(EcsError::ConflictingQueryAccess(std::any::type_name::<R>()));
        }
        // SAFETY: declared write access; scheduler excludes concurrent conflicting
        // systems; only one mutable borrow is handed out per run.
        Ok(unsafe { self.world.resource_unchecked::<R>() })
    }

    /// Defers a world mutation until the end of the schedule run.
    pub fn command(&self, f: impl FnOnce(&mut World) + Send + 'static) {
        if let Ok(mut q) = self.commands.lock() {
            q.push(Box::new(f));
        }
    }
}

/// Builder describing a system.
pub struct SystemBuilder {
    name: String,
    access: Access,
    after: Vec<OrderLabel>,
    before: Vec<OrderLabel>,
    sets: Vec<InternedSet>,
    conditions: Vec<Condition>,
    error: Option<EcsError>,
}

impl SystemBuilder {
    /// Starts a system description.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            access: Access::default(),
            after: Vec::new(),
            before: Vec::new(),
            sets: Vec::new(),
            conditions: Vec::new(),
            error: None,
        }
    }
    fn record(mut self, r: Result<(), EcsError>) -> Self {
        if let Err(e) = r {
            self.error.get_or_insert(e);
        }
        self
    }
    /// Declares a component read.
    pub fn read<T: Component>(mut self) -> Self {
        let r = self.access.add_read::<T>();
        self.record(r)
    }
    /// Declares a component write.
    pub fn write<T: Component>(mut self) -> Self {
        let r = self.access.add_write::<T>();
        self.record(r)
    }
    /// Declares a resource read.
    pub fn res_read<R: Send + Sync + 'static>(mut self) -> Self {
        let r = self.access.add_read::<R>();
        self.record(r)
    }
    /// Declares a resource write.
    pub fn res_write<R: Send + Sync + 'static>(mut self) -> Self {
        let r = self.access.add_write::<R>();
        self.record(r)
    }
    /// Orders this system before the named system, or before every member of a set.
    pub fn before(mut self, label: impl IntoOrderLabel) -> Self {
        self.before.push(label.into_order_label());
        self
    }
    /// Orders this system after the named system, or after every member of a set.
    pub fn after(mut self, label: impl IntoOrderLabel) -> Self {
        self.after.push(label.into_order_label());
        self
    }
    /// Adds this system to a set.
    pub fn in_set(mut self, set: impl SystemSet) -> Self {
        self.sets.push(crate::set::intern(set));
        self
    }
    /// Runs the system only if `cond` is true. Evaluated once per schedule run, when
    /// the system becomes ready (every system ordered before it has finished) and no
    /// other system of the schedule is running; the executor starts nothing new until
    /// it has been evaluated. Unordered systems may or may not have run by then. A
    /// skipped system still counts as done for ordering and does not advance its
    /// change-detection `last_run`.
    pub fn run_if(mut self, cond: impl FnMut(&World) -> bool + Send + 'static) -> Self {
        self.conditions.push(Box::new(cond));
        self
    }
}

struct SystemNode {
    name: String,
    access: SystemAccess,
    after: Vec<OrderLabel>,
    before: Vec<OrderLabel>,
    sets: Vec<InternedSet>,
    conditions: Vec<Condition>,
    run: SystemFn,
    last_run: Tick,
}

/// How a schedule executes ready systems.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Executor {
    /// One at a time, in dependency order.
    SingleThreaded,
    /// Ready systems run on the rayon pool as soon as their dependencies finish.
    MultiThreaded,
}

/// An ordered collection of systems.
pub struct Schedule {
    systems: Vec<SystemNode>,
    executor: Executor,
    sets: HashMap<InternedSet, SetConfig>,
    // deps[i]: systems that must finish before i (deduplicated); dependents is the
    // reverse adjacency, used by the ready queue.
    deps: Vec<Vec<usize>>,
    dependents: Vec<Vec<usize>>,
    // Every set each system belongs to (direct and through nesting), from `build`.
    system_sets: Vec<Vec<InternedSet>>,
    ambiguities: Vec<(String, String)>,
    dirty: bool,
}

impl Default for Schedule {
    fn default() -> Self {
        Self::new(Executor::MultiThreaded)
    }
}

impl Schedule {
    /// Creates an empty schedule.
    pub fn new(executor: Executor) -> Self {
        Self {
            systems: Vec::new(),
            executor,
            sets: HashMap::new(),
            deps: Vec::new(),
            dependents: Vec::new(),
            system_sets: Vec::new(),
            ambiguities: Vec::new(),
            dirty: true,
        }
    }

    /// Configures a system set (ordering, nesting, run conditions). Calling it again
    /// for the same set adds to the existing configuration.
    pub fn configure_set(&mut self, cfg: SetConfig) -> &mut Self {
        match self.sets.get_mut(&cfg.set) {
            Some(e) => {
                e.after.extend(cfg.after);
                e.before.extend(cfg.before);
                e.parents.extend(cfg.parents);
                e.conditions.extend(cfg.conditions);
            }
            None => {
                self.sets.insert(cfg.set, cfg);
            }
        }
        self.dirty = true;
        self
    }

    /// Adds a system. Fails if its declared access is self-conflicting.
    pub fn add_system(
        &mut self,
        builder: SystemBuilder,
        mut run: impl FnMut(&SystemContext<'_>) + Send + 'static,
    ) -> Result<&mut Self, EcsError> {
        if let Some(e) = builder.error {
            return Err(e);
        }
        self.systems.push(SystemNode {
            name: builder.name,
            access: SystemAccess::from_unfiltered(builder.access),
            after: builder.after,
            before: builder.before,
            sets: builder.sets,
            conditions: builder.conditions,
            run: Box::new(move |ctx: &SystemContext<'_>| {
                run(ctx);
                Ok(())
            }),
            last_run: Tick(0),
        });
        self.dirty = true;
        Ok(self)
    }

    /// Adds a plain function system; its access is derived from the parameter types
    /// (`Query`, `Res`, `ResMut`, ...), one filtered entry per parameter. Fails if two
    /// parameters (or a parameter and the builder's declared access) conflict. Two
    /// queries over the same component do not conflict when their filters make them
    /// archetype-disjoint, e.g. `Query<&mut A, With<B>>` and `Query<&mut A, Without<B>>`.
    pub fn add_fn<M>(
        &mut self,
        mut builder: SystemBuilder,
        mut func: impl crate::system::SystemParamFunction<M>,
    ) -> Result<&mut Self, EcsError> {
        if let Some(e) = builder.error.take() {
            return Err(e);
        }
        let mut access = SystemAccess::from_unfiltered(builder.access);
        func.access(&mut access)?;
        self.systems.push(SystemNode {
            name: builder.name,
            access,
            after: builder.after,
            before: builder.before,
            sets: builder.sets,
            conditions: builder.conditions,
            run: {
                let mut state = Default::default();
                Box::new(move |ctx: &SystemContext<'_>| func.run(ctx, &mut state))
            },
            last_run: Tick(0),
        });
        self.dirty = true;
        Ok(self)
    }

    /// Pairs of conflicting systems that had no explicit ordering (they are run in
    /// insertion order).
    pub fn ambiguities(&mut self) -> Result<&[(String, String)], EcsError> {
        self.build()?;
        Ok(&self.ambiguities)
    }

    fn build(&mut self) -> Result<(), EcsError> {
        if !self.dirty {
            return Ok(());
        }
        let n = self.systems.len();
        // Transitive set membership of every system (nesting via `SetConfig::in_set`;
        // a nesting cycle just makes the sets share members).
        let system_sets: Vec<Vec<InternedSet>> = self
            .systems
            .iter()
            .map(|s| {
                let mut all: Vec<InternedSet> = Vec::new();
                let mut stack = s.sets.clone();
                while let Some(set) = stack.pop() {
                    if all.contains(&set) {
                        continue;
                    }
                    all.push(set);
                    if let Some(cfg) = self.sets.get(&set) {
                        stack.extend(cfg.parents.iter().copied());
                    }
                }
                all
            })
            .collect();
        let members = |l: &OrderLabel| -> Vec<usize> {
            (0..n)
                .filter(|&j| match l {
                    OrderLabel::System(name) => &self.systems[j].name == name,
                    OrderLabel::Set(set) => system_sets[j].contains(set),
                })
                .collect()
        };
        let mut deps = vec![Vec::new(); n];
        // Every system in `xs` must run before every (other) system in `ys`.
        let order = |deps: &mut Vec<Vec<usize>>, xs: &[usize], ys: &[usize]| {
            for &x in xs {
                for &y in ys {
                    if x != y {
                        deps[y].push(x);
                    }
                }
            }
        };
        for (i, s) in self.systems.iter().enumerate() {
            for a in &s.after {
                order(&mut deps, &members(a), &[i]);
            }
            for b in &s.before {
                order(&mut deps, &[i], &members(b));
            }
        }
        for cfg in self.sets.values() {
            let mine = members(&OrderLabel::Set(cfg.set));
            for a in &cfg.after {
                order(&mut deps, &members(a), &mine);
            }
            for b in &cfg.before {
                order(&mut deps, &mine, &members(b));
            }
        }
        // reach[i][j]: j must run before i.
        let closure = |deps: &Vec<Vec<usize>>| -> Result<Vec<Vec<bool>>, EcsError> {
            let mut reach = vec![vec![false; n]; n];
            let mut state = vec![0u8; n];
            fn visit(
                i: usize,
                deps: &Vec<Vec<usize>>,
                reach: &mut Vec<Vec<bool>>,
                state: &mut Vec<u8>,
            ) -> Result<(), EcsError> {
                match state[i] {
                    2 => return Ok(()),
                    1 => return Err(EcsError::OrderingCycle),
                    _ => {}
                }
                state[i] = 1;
                for &d in &deps[i] {
                    visit(d, deps, reach, state)?;
                    reach[i][d] = true;
                    let row = reach[d].clone();
                    for (k, v) in row.into_iter().enumerate() {
                        reach[i][k] |= v;
                    }
                }
                state[i] = 2;
                Ok(())
            }
            for i in 0..n {
                visit(i, deps, &mut reach, &mut state)?;
            }
            Ok(reach)
        };
        let explicit = closure(&deps)?;
        self.ambiguities.clear();
        for j in 0..n {
            for i in 0..j {
                if self.systems[i].access.conflicts(&self.systems[j].access) {
                    if !explicit[i][j] && !explicit[j][i] {
                        self.ambiguities
                            .push((self.systems[i].name.clone(), self.systems[j].name.clone()));
                    }
                    let reach = closure(&deps)?;
                    if !reach[i][j] && !reach[j][i] {
                        deps[j].push(i);
                    }
                }
            }
        }
        let mut dependents = vec![Vec::new(); n];
        for (i, d) in deps.iter_mut().enumerate() {
            d.sort_unstable();
            d.dedup();
            for &j in d.iter() {
                dependents[j].push(i);
            }
        }
        self.deps = deps;
        self.dependents = dependents;
        self.system_sets = system_sets;
        self.dirty = false;
        Ok(())
    }

    /// Runs every system once, then applies deferred commands in system order.
    ///
    /// Errors (a system returning `Err`, a panicking system or run condition) do not
    /// stop the run; the one with the lowest system index is returned.
    pub fn run(&mut self, world: &mut World) -> Result<(), EcsError> {
        self.build()?;
        let this_run = world.increment_change_tick();
        // Keep every system's last-seen tick within the exact comparison range
        // (the world clamps its own component ticks in `increment_change_tick`).
        for s in &mut self.systems {
            s.last_run.check_tick(this_run);
        }
        let parallel = self.executor == Executor::MultiThreaded && self.systems.len() > 1;
        let Schedule {
            systems,
            sets,
            deps,
            dependents,
            system_sets,
            ..
        } = self;
        let graph = Graph {
            deps,
            dependents,
            system_sets,
        };
        let world_ref: &World = world;
        let outcome = if parallel {
            in_scope(|spawn| drive(world_ref, this_run, systems, sets, &graph, Some(spawn)))
        } else {
            drive(world_ref, this_run, systems, sets, &graph, None)
        };
        let Outcome {
            mut commands,
            mut errors,
        } = outcome?;
        world.increment_change_tick();
        commands.sort_by_key(|(i, _)| *i);
        // Entities reserved by `Commands::spawn` become alive before any command runs.
        world.flush_entities();
        for (_, cmds) in commands {
            for c in cmds {
                c(world);
            }
        }
        errors.sort_by_key(|(i, _)| *i);
        match errors.into_iter().next() {
            Some((_, e)) => Err(e),
            None => Ok(()),
        }
    }
}

/// The dependency graph and set membership computed by `build`.
struct Graph<'a> {
    deps: &'a [Vec<usize>],
    dependents: &'a [Vec<usize>],
    system_sets: &'a [Vec<InternedSet>],
}

/// What one run produced: deferred commands and errors, each tagged with the index
/// of the system they belong to.
struct Outcome {
    commands: Vec<(usize, Vec<Command>)>,
    errors: Vec<(usize, EcsError)>,
}

/// A system ready to run on a worker thread.
type Job<'env> = Box<dyn FnOnce() + Send + 'env>;

/// Starts a batch of jobs that became ready together.
type Spawner<'s, 'env> = &'s mut dyn FnMut(Vec<Job<'env>>);

/// Finished system: its index, deferred commands and result.
type Done = (usize, Vec<Command>, Result<(), EcsError>);

/// The ready-queue executor. `spawn` is `None` for the single-threaded executor,
/// which runs every system inline, lowest ready index first.
///
/// Soundness: a system is started only when every system it depends on has
/// finished, and `build` ordered every conflicting pair, so systems running at the
/// same time never conflict. Run conditions take `&mut` to their closures and read
/// the world with no declared access, so they are evaluated only while no system
/// is in flight; dispatch stops at the first ready system that needs one until the
/// running systems have drained.
fn drive<'env>(
    world: &'env World,
    this_run: Tick,
    systems: &'env mut [SystemNode],
    sets: &mut HashMap<InternedSet, SetConfig>,
    graph: &Graph<'_>,
    mut spawn: Option<Spawner<'_, 'env>>,
) -> Result<Outcome, EcsError> {
    let n = systems.len();
    let mut slots: Vec<Option<&'env mut SystemNode>> = systems.iter_mut().map(Some).collect();
    let mut pending: Vec<usize> = graph.deps.iter().map(Vec::len).collect();
    let mut ready: BinaryHeap<Reverse<usize>> =
        (0..n).filter(|&i| pending[i] == 0).map(Reverse).collect();
    let mut set_ok: HashMap<InternedSet, bool> = HashMap::new();
    let mut out = Outcome {
        commands: Vec::with_capacity(n),
        errors: Vec::new(),
    };
    let mut finished = 0;
    let mut in_flight = 0;
    let (tx, rx) = mpsc::channel::<Done>();
    let mut batch: Vec<Job<'env>> = Vec::new();
    // Marks `i` finished and queues every dependent whose count reaches zero.
    let release = |i: usize, pending: &mut [usize], ready: &mut BinaryHeap<Reverse<usize>>| {
        for &d in &graph.dependents[i] {
            pending[d] -= 1;
            if pending[d] == 0 {
                ready.push(Reverse(d));
            }
        }
    };
    while finished < n {
        while let Some(&Reverse(i)) = ready.peek() {
            let Some(node) = slots[i].as_deref_mut() else {
                // Every system is queued exactly once (its count reaches zero once).
                unreachable!("system {i} queued twice");
            };
            if in_flight > 0 && needs_conditions(node, &graph.system_sets[i], sets, &set_ok) {
                break;
            }
            ready.pop();
            // With nothing left to evaluate this only reads cached set results.
            if !should_run(
                i,
                node,
                world,
                sets,
                &graph.system_sets[i],
                &mut set_ok,
                &mut out.errors,
            ) {
                slots[i] = None;
                finished += 1;
                release(i, &mut pending, &mut ready);
                continue;
            }
            let Some(node) = slots[i].take() else {
                unreachable!("system {i} queued twice");
            };
            // Alone and nothing running: no other thread could help, run it here.
            if spawn.is_none() || (in_flight == 0 && ready.is_empty()) {
                let (cmds, r) = run_system(node, world, this_run);
                record(&mut out, i, cmds, r);
                finished += 1;
                release(i, &mut pending, &mut ready);
            } else {
                let tx = tx.clone();
                batch.push(Box::new(move || {
                    let (cmds, r) = run_system(node, world, this_run);
                    // The receiver outlives every job: `drive` waits for all of them.
                    let _ = tx.send((i, cmds, r));
                }));
                in_flight += 1;
            }
        }
        if let Some(spawn) = spawn.as_deref_mut() {
            if !batch.is_empty() {
                spawn(std::mem::take(&mut batch));
            }
        }
        if finished == n {
            break;
        }
        if in_flight == 0 {
            // Unreachable for a graph `build` accepted (it rejects cycles); kept so a
            // broken invariant reports an error instead of hanging.
            return Err(EcsError::OrderingCycle);
        }
        let (i, cmds, r) = wait(&rx);
        in_flight -= 1;
        record(&mut out, i, cmds, r);
        finished += 1;
        release(i, &mut pending, &mut ready);
    }
    Ok(out)
}

fn record(out: &mut Outcome, i: usize, cmds: Vec<Command>, r: Result<(), EcsError>) {
    out.commands.push((i, cmds));
    if let Err(e) = r {
        out.errors.push((i, e));
    }
}

/// Runs one system with shared world access and collects its deferred commands.
fn run_system(
    s: &mut SystemNode,
    world: &World,
    this_run: Tick,
) -> (Vec<Command>, Result<(), EcsError>) {
    let ctx = SystemContext {
        world,
        name: &s.name,
        access: &s.access,
        commands: Default::default(),
        last_run: s.last_run,
        this_run,
        mutable_borrowed: Default::default(),
    };
    let f = &mut s.run;
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&ctx)));
    s.last_run = this_run;
    let cmds = ctx
        .commands
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default();
    (
        cmds,
        r.map_err(|_| EcsError::SystemPanicked(s.name.clone()))
            .and_then(|x| x),
    )
}

/// True if starting `node` requires evaluating a run condition: its own, or one of
/// a set it belongs to that has not been evaluated in this run yet.
fn needs_conditions(
    node: &SystemNode,
    system_sets: &[InternedSet],
    sets: &HashMap<InternedSet, SetConfig>,
    set_ok: &HashMap<InternedSet, bool>,
) -> bool {
    !node.conditions.is_empty()
        || system_sets.iter().any(|s| {
            !set_ok.contains_key(s) && sets.get(s).is_some_and(|c| !c.conditions.is_empty())
        })
}

/// Evaluates the run conditions of system `i`: those of every set it belongs to
/// (each set at most once per run, cached in `set_ok`) and its own. Every
/// condition reached is evaluated (no short-circuit). A panicking condition counts
/// as false and is reported as [`EcsError::SystemPanicked`].
fn should_run(
    i: usize,
    node: &mut SystemNode,
    world: &World,
    sets: &mut HashMap<InternedSet, SetConfig>,
    system_sets: &[InternedSet],
    set_ok: &mut HashMap<InternedSet, bool>,
    errors: &mut Vec<(usize, EcsError)>,
) -> bool {
    let mut eval = |c: &mut Condition, what: &dyn Fn() -> String| match std::panic::catch_unwind(
        std::panic::AssertUnwindSafe(|| c(world)),
    ) {
        Ok(b) => b,
        Err(_) => {
            errors.push((i, EcsError::SystemPanicked(what())));
            false
        }
    };
    let mut ok = true;
    for set in system_sets {
        let r = match set_ok.get(set) {
            Some(&r) => r,
            None => {
                let mut r = true;
                if let Some(cfg) = sets.get_mut(set) {
                    for c in &mut cfg.conditions {
                        r &= eval(c, &|| format!("run condition of set {set:?}"));
                    }
                }
                set_ok.insert(*set, r);
                r
            }
        };
        ok &= r;
    }
    for c in &mut node.conditions {
        ok &= eval(c, &|| format!("run condition of {}", node.name));
    }
    ok
}

/// Runs `body` with a spawner whose jobs may borrow anything that outlives the
/// call; returns after every spawned job has finished. Jobs run on the persistent
/// rayon pool; `body` (the scheduler loop) runs on the calling thread, so (unless
/// that thread is itself a pool worker, see [`wait`]) a long system never occupies
/// the thread that must start the next ready ones.
///
/// A batch is spread over the pool the way a parallel iterator splits its input
/// (each split may wake one more worker), rather than as one spawn per job, each
/// of which would wake a worker from the scheduler thread.
#[cfg(not(miri))]
fn in_scope<'env, R>(body: impl FnOnce(Spawner<'_, 'env>) -> R) -> R {
    use rayon::prelude::*;
    rayon::in_place_scope(|s| {
        body(&mut |mut jobs: Vec<Job<'env>>| {
            if jobs.len() == 1 {
                if let Some(job) = jobs.pop() {
                    s.spawn(move |_| job());
                }
            } else {
                s.spawn(move |_| jobs.into_par_iter().for_each(|job| job()));
            }
        })
    })
}

/// Under Miri, rayon's crossbeam-epoch dependency reports a Stacked Borrows
/// violation inside crossbeam itself, so Miri runs the same jobs on scoped std
/// threads to keep checking *our* unsafe code.
#[cfg(miri)]
fn in_scope<'env, R>(body: impl FnOnce(Spawner<'_, 'env>) -> R) -> R {
    std::thread::scope(|s| {
        body(&mut |jobs: Vec<Job<'env>>| {
            for job in jobs {
                s.spawn(job);
            }
        })
    })
}

/// Waits for the next finished system. Called from outside the pool it blocks.
/// Called on a rayon worker (a schedule run from inside the pool) it runs pending
/// pool work meanwhile, which may be the very job it waits for, so a one-thread
/// pool cannot deadlock; that may delay starting newly ready systems until the job
/// it picked up returns.
#[cfg(not(miri))]
fn wait(rx: &mpsc::Receiver<Done>) -> Done {
    loop {
        if let Ok(d) = rx.try_recv() {
            return d;
        }
        if rayon::yield_now() != Some(rayon::Yield::Executed) {
            break;
        }
    }
    // `drive` holds a sender, so `recv` cannot fail; a job that is in flight sends.
    match rx.recv() {
        Ok(d) => d,
        Err(_) => unreachable!("schedule executor: job channel closed"),
    }
}

#[cfg(miri)]
fn wait(rx: &mpsc::Receiver<Done>) -> Done {
    match rx.recv() {
        Ok(d) => d,
        Err(_) => unreachable!("schedule executor: job channel closed"),
    }
}
