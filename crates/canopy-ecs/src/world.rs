//! The [`World`]: entities, archetypes, columns and resources.
use crate::blob_vec::BlobVec;
use crate::entity::{Entities, Entity, Location};
use crate::error::EcsError;
use std::alloc::Layout;
use std::any::{Any, TypeId};
use std::cell::UnsafeCell;
use std::collections::HashMap;

/// Marker trait for component types.
pub trait Component: Send + Sync + 'static {
    /// Where values of this type live. [`StorageType::Table`] (the default) stores them
    /// in archetype columns: fastest to iterate, but adding/removing the component moves
    /// the entity's whole row. [`StorageType::SparseSet`] stores them in one per-type
    /// [`ComponentSparseSet`]: adding/removing touches only that set (the entity still
    /// changes archetype, but no table column of it moves for this component).
    const STORAGE: StorageType = StorageType::Table;
}

/// Storage kind of a component type (see [`Component::STORAGE`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StorageType {
    /// Archetype table columns.
    #[default]
    Table,
    /// One sparse set per component type, keyed by entity index.
    SparseSet,
}

/// A change-detection tick. Comparisons are wraparound-safe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Tick(pub u32);

/// Ticks older than this (relative to the current tick) are clamped to exactly this
/// age by [`World::check_change_ticks`] and when schedules run.
pub const MAX_CHANGE_AGE: u32 = 1 << 30;

/// [`World::increment_change_tick`] runs [`World::check_change_ticks`] whenever at
/// least this many ticks passed since the last check. Every stored tick is therefore
/// at most `MAX_CHANGE_AGE + CHECK_TICK_INTERVAL` (`3 * 2^29 < 2^31`) old, the range
/// in which [`Tick::is_newer_than`] is exact.
pub const CHECK_TICK_INTERVAL: u32 = 1 << 29;

impl Tick {
    /// Clamps this tick so it is at most [`MAX_CHANGE_AGE`] older than `now`.
    /// Returns whether it was clamped.
    pub fn check_tick(&mut self, now: Tick) -> bool {
        if now.0.wrapping_sub(self.0) > MAX_CHANGE_AGE {
            self.0 = now.0.wrapping_sub(MAX_CHANGE_AGE);
            true
        } else {
            false
        }
    }

    /// Whether this tick is newer than `last_run`, observed at `this_run`.
    ///
    /// Uses wrapping distances from `this_run`, so it stays correct across `u32`
    /// overflow as long as both ticks are less than `2^31` ticks old.
    pub fn is_newer_than(self, last_run: Tick, this_run: Tick) -> bool {
        let age = this_run.0.wrapping_sub(self.0);
        let since_last = this_run.0.wrapping_sub(last_run.0);
        since_last > age
    }
}

/// Identifier of a registered component type.
pub type ComponentId = usize;

pub(crate) struct ComponentInfo {
    pub layout: Layout,
    pub drop: Option<unsafe fn(*mut u8)>,
    pub storage: StorageType,
}

/// SAFETY contract: `p` must point to a live, owned `T`.
unsafe fn drop_ptr<T>(p: *mut u8) {
    // SAFETY: guaranteed by the function contract above.
    unsafe { std::ptr::drop_in_place(p.cast::<T>()) }
}

/// One column: values plus their change ticks.
pub struct Column {
    pub(crate) data: BlobVec,
    pub(crate) added: Vec<UnsafeCell<Tick>>,
    pub(crate) changed: Vec<UnsafeCell<Tick>>,
}

// SAFETY: tick cells are only written through `&mut World` or by a system that the
// scheduler granted exclusive write access to that component (see ARCHITECTURE.md).
unsafe impl Sync for Column {}

impl Column {
    fn new(info: &ComponentInfo) -> Self {
        Self {
            data: BlobVec::new(info.layout, info.drop),
            added: Vec::new(),
            changed: Vec::new(),
        }
    }

    /// Swap-removes row ticks and moves the value into the data spare slot.
    ///
    /// SAFETY contract: `row < len`.
    unsafe fn swap_remove_to_spare(&mut self, row: usize) {
        // SAFETY: forwarded from the caller.
        unsafe { self.data.swap_remove_to_spare(row) };
        self.added.swap_remove(row);
        self.changed.swap_remove(row);
    }
}

/// `sparse` entry of an entity index that has no value in the set.
const ABSENT: u32 = u32::MAX;

/// Storage of one [`StorageType::SparseSet`] component type for all entities.
///
/// `dense` holds the values (and their ticks) contiguously; `entities[d]` is the owner
/// of dense slot `d`; `sparse[entity.index()]` is that entity's dense slot or `ABSENT`.
///
/// Invariants (relied on by every unsafe access through [`World`] and the queries):
/// - `entities.len() <= dense.data.len()`, `<= dense.added.len()`, `<= dense.changed.len()`
///   (equal in practice; pushes grow `entities` last, so even an interrupted push keeps
///   this). `Self::dense_index` only returns `d` with `entities[d] == e`, hence
///   `d < entities.len()`, so every slot it yields is in bounds of all dense vectors.
/// - `e` has a slot iff `e`'s archetype lists the component in its sparse ids. Slots are
///   added/removed only by `World::insert_bundle`/`remove`/`despawn`, together with the
///   archetype move, so a despawned entity never leaves a stale slot behind.
pub struct ComponentSparseSet {
    dense: Column,
    entities: Vec<Entity>,
    sparse: Vec<u32>,
}

impl ComponentSparseSet {
    fn new(info: &ComponentInfo) -> Self {
        Self {
            dense: Column::new(info),
            entities: Vec::new(),
            sparse: Vec::new(),
        }
    }

    /// Number of entities with a value in this set.
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Whether no entity has a value in this set.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Whether `e` (this exact generation) has a value in this set.
    pub fn contains(&self, e: Entity) -> bool {
        self.dense_index(e).is_some()
    }

    /// Dense slot of `e`'s value. A returned `d` always satisfies `d < self.len()`
    /// (checked: `entities[d] == e`), so it indexes every dense vector in bounds.
    pub(crate) fn dense_index(&self, e: Entity) -> Option<usize> {
        let d = *self.sparse.get(e.index() as usize)?;
        if d == ABSENT {
            return None;
        }
        let d = d as usize;
        (self.entities.get(d) == Some(&e)).then_some(d)
    }

    /// Appends a value for `e`, which must not already have one.
    ///
    /// # Safety
    /// `src` points to a valid value of this set's component type whose ownership
    /// moves into the set (the caller must not drop it).
    unsafe fn push(&mut self, e: Entity, src: *const u8, tick: Tick) {
        debug_assert!(!self.contains(e));
        let d = self.entities.len();
        // Justification: more than u32::MAX - 1 values in one set is unsupported;
        // truncating would alias slots, so abort like `Entities::alloc` does.
        let d32 = u32::try_from(d)
            .ok()
            .filter(|&x| x != ABSENT)
            .unwrap_or_else(|| std::process::abort());
        let i = e.index() as usize;
        if self.sparse.len() <= i {
            self.sparse.resize(i + 1, ABSENT);
        }
        // SAFETY: forwarded from the caller; the value moves into the dense column.
        unsafe { self.dense.data.push(src) };
        self.dense.added.push(UnsafeCell::new(tick));
        self.dense.changed.push(UnsafeCell::new(tick));
        // `entities` grows last (see the type-level invariant), then the slot is published.
        self.entities.push(e);
        self.sparse[i] = d32;
    }

    /// Replaces `e`'s value (dropping the old one) and marks it changed at `tick`.
    /// Returns `false` (and leaves `src` untouched) if `e` has no value here.
    ///
    /// # Safety
    /// As [`Self::push`]; on `true` ownership of `*src` moved into the set.
    unsafe fn replace(&mut self, e: Entity, src: *const u8, tick: Tick) -> bool {
        let Some(d) = self.dense_index(e) else {
            return false;
        };
        *self.dense.changed[d].get_mut() = tick;
        // SAFETY: d < entities.len() <= data.len(); value ownership moves in. The old
        // value goes to the spare slot before its destructor runs.
        unsafe { self.dense.data.replace(d, src) };
        true
    }

    /// Unlinks `e`'s value and moves it into the dense column's spare slot without
    /// dropping it. Returns whether `e` had a value. After `true`, the caller must
    /// call [`Self::drop_spare`] (before any other push) once bookkeeping is consistent.
    fn remove_to_spare(&mut self, e: Entity) -> bool {
        let Some(d) = self.dense_index(e) else {
            return false;
        };
        // SAFETY: d < entities.len() <= every dense vector's len.
        unsafe { self.dense.swap_remove_to_spare(d) };
        self.entities.swap_remove(d);
        if let Some(&moved) = self.entities.get(d) {
            // `d` fits in u32: it was stored in `sparse` before.
            self.sparse[moved.index() as usize] = d as u32;
        }
        self.sparse[e.index() as usize] = ABSENT;
        true
    }

    /// Drops the value parked by the last successful [`Self::remove_to_spare`].
    ///
    /// # Safety
    /// `remove_to_spare` returned `true` and nothing was pushed or dropped since.
    unsafe fn drop_spare(&mut self) {
        // SAFETY: forwarded from the caller: the spare slot holds the removed value.
        unsafe { self.dense.data.drop_spare() };
    }

    /// Value pointer of `e`'s component.
    fn get_ptr(&self, e: Entity) -> Option<*mut u8> {
        let d = self.dense_index(e)?;
        // SAFETY: d < entities.len() <= data.len().
        Some(unsafe { self.dense.data.get_ptr(d) })
    }
}

/// Maps archetype rows to dense slots of one sparse set (used by query fetches).
#[derive(Clone, Copy)]
pub(crate) struct SparseRows<'w> {
    set: &'w ComponentSparseSet,
    entities: &'w [Entity],
}

impl SparseRows<'_> {
    /// Dense slot of the entity in archetype row `row`; always `< set.len()`.
    ///
    /// Panics if `row` is out of bounds or the entity has no slot, both of which a
    /// consistent world rules out (the archetype lists this sparse component).
    #[inline]
    pub(crate) fn dense(&self, row: usize) -> usize {
        match self.set.dense_index(self.entities[row]) {
            Some(d) => d,
            None => unreachable!("entity of a sparse-set archetype has no slot in the set"),
        }
    }
}

/// Where one archetype keeps component values: its own table column, or a sparse
/// set's dense column plus the row-to-slot mapping.
#[derive(Clone, Copy)]
pub(crate) struct ComponentRows<'w> {
    pub column: &'w Column,
    pub sparse: Option<SparseRows<'w>>,
}

/// A set of entities sharing exactly the same component types. Table components are
/// stored column-wise here; sparse-set components only count towards membership.
pub struct Archetype {
    /// Sorted table component ids; `columns[i]` stores `table_components[i]`.
    pub(crate) table_components: Vec<ComponentId>,
    /// Sorted sparse-set component ids (values live in the world's sparse sets).
    pub(crate) sparse_components: Vec<ComponentId>,
    pub(crate) columns: Vec<Column>,
    pub(crate) entities: Vec<Entity>,
    remove_edges: HashMap<ComponentId, usize>,
    /// Indexed by bundle id: index into `World::bundle_edges` of the edge for inserting
    /// that bundle here, or `NO_EDGE` if not computed yet.
    bundle_edges: Vec<u32>,
}

/// `Archetype::bundle_edges` entry of a bundle whose edge is not computed yet.
const NO_EDGE: u32 = u32::MAX;

/// What inserting one bundle field does to an entity of the edge's source archetype.
/// Column indices are into the edge's target archetype.
#[derive(Clone, Copy, Debug)]
enum FieldOp {
    /// Push a new value into table column `.0` (the entity lacked the component).
    TableAdd(usize),
    /// Replace the value in table column `.0` (the entity had it, or an earlier field of
    /// the same bundle added it).
    TableReplace(usize),
    /// Push a new value into the sparse set of component `.0`.
    SparseAdd(ComponentId),
    /// Replace the value in the sparse set of component `.0`.
    SparseReplace(ComponentId),
}

/// The cached result of inserting one bundle type into one archetype.
struct BundleEdge {
    /// Target archetype: the source's components plus the bundle's.
    to: usize,
    /// One op per bundle field, in field order.
    ops: Box<[FieldOp]>,
}

impl Archetype {
    /// Table column index of a component, if it is a table component of this archetype.
    pub fn column_of(&self, id: ComponentId) -> Option<usize> {
        self.table_components.binary_search(&id).ok()
    }
    /// Whether this archetype's entities have the component (table or sparse set).
    pub fn contains(&self, id: ComponentId) -> bool {
        self.column_of(id).is_some() || self.has_sparse(id)
    }
    /// Whether the component is a sparse-set component of this archetype.
    pub fn has_sparse(&self, id: ComponentId) -> bool {
        self.sparse_components.binary_search(&id).is_ok()
    }
    /// Table component ids (sorted, in column order).
    pub fn table_components(&self) -> &[ComponentId] {
        &self.table_components
    }
    /// Sparse-set component ids (sorted).
    pub fn sparse_components(&self) -> &[ComponentId] {
        &self.sparse_components
    }
    /// Entities in row order.
    pub fn entities(&self) -> &[Entity] {
        &self.entities
    }
    /// Number of entities.
    pub fn len(&self) -> usize {
        self.entities.len()
    }
    /// Whether the archetype has no entities.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }
}

struct ResourceCell(UnsafeCell<Box<dyn Any + Send + Sync>>);
// SAFETY: values are Send + Sync; mutable access only through `&mut World` or a
// system holding declared exclusive resource access.
unsafe impl Sync for ResourceCell {}

/// The container for all ECS data.
pub struct World {
    id: u64,
    entities: Entities,
    infos: Vec<ComponentInfo>,
    ids: HashMap<TypeId, ComponentId>,
    names: Vec<&'static str>,
    /// Indexed by [`ComponentId`]; `Some` exactly for sparse-set component types.
    sparse_sets: Vec<Option<ComponentSparseSet>>,
    pub(crate) archetypes: Vec<Archetype>,
    archetype_index: HashMap<Vec<ComponentId>, usize>,
    /// Bundle type -> bundle id (index into `bundle_components`).
    bundle_ids: HashMap<TypeId, usize>,
    /// Component ids of each bundle id, in field order (duplicates kept).
    bundle_components: Vec<Box<[ComponentId]>>,
    /// Every computed bundle edge (`Archetype::bundle_edges` indexes into it).
    bundle_edges: Vec<BundleEdge>,
    resources: HashMap<TypeId, ResourceCell>,
    pub(crate) change_tick: u32,
    pub(crate) last_change_tick: u32,
    last_check_tick: u32,
    /// Ids of [`Parent`](crate::hierarchy::Parent) and
    /// [`Children`](crate::hierarchy::Children), registered by [`World::new`] so
    /// [`World::despawn`] can test archetype membership without a type lookup.
    hierarchy_ids: [ComponentId; 2],
    /// Archetype caches of `World::query`/`par_for_each` (exclusive access).
    pub(crate) query_caches: crate::query::QueryCaches,
    /// Archetype caches of closure-system queries (shared access, so behind a mutex).
    pub(crate) shared_query_caches: std::sync::Mutex<crate::query::QueryCaches>,
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

/// A group of components inserted together: any [`Component`], and tuples of up to six
/// components (including `()`). Inserting a bundle moves the entity at most once, along
/// an edge cached per (archetype, bundle type). A component that appears twice in one
/// bundle is inserted twice, so the later value replaces (and drops) the earlier one.
///
/// # Safety
/// Implementations must make [`Bundle::component_ids`] append, for each field in order,
/// `world.register::<T>()` of that field's type `T`, and [`Bundle::for_each_ptr`] call
/// `f` once per field, in the same order, with a pointer to that field of `*this`. The
/// world moves every field out through those pointers and never drops `*this`.
pub unsafe trait Bundle: Send + Sync + 'static {
    /// Registers the field types and appends their ids, one per field, in field order.
    #[doc(hidden)]
    fn component_ids(world: &mut World, ids: &mut Vec<ComponentId>);

    /// Calls `f` with a pointer to each field of `*this`, in field order.
    ///
    /// # Safety
    /// `this` points to a live `Self`.
    #[doc(hidden)]
    unsafe fn for_each_ptr<F: FnMut(*mut u8)>(this: *mut Self, f: &mut F);
}

// SAFETY: one id and one pointer, both for `C`.
unsafe impl<C: Component> Bundle for C {
    fn component_ids(world: &mut World, ids: &mut Vec<ComponentId>) {
        ids.push(world.register::<C>());
    }
    unsafe fn for_each_ptr<F: FnMut(*mut u8)>(this: *mut Self, f: &mut F) {
        f(this.cast());
    }
}

macro_rules! impl_bundle {
    ($($n:ident $i:tt),*) => {
        // SAFETY: ids and pointers are emitted field by field in the same order, and each
        // pointer addresses the field whose type the id was registered for.
        unsafe impl<$($n: Component),*> Bundle for ($($n,)*) {
            #[allow(unused_variables)]
            fn component_ids(world: &mut World, ids: &mut Vec<ComponentId>) {
                $( ids.push(world.register::<$n>()); )*
            }
            #[allow(unused_variables)]
            unsafe fn for_each_ptr<Func: FnMut(*mut u8)>(this: *mut Self, f: &mut Func) {
                // SAFETY: `this` points to a live tuple (caller contract); no reference
                // is created, only field addresses.
                $( f(unsafe { std::ptr::addr_of_mut!((*this).$i) }.cast()); )*
            }
        }
    };
}
impl_bundle!();
impl_bundle!(A 0);
impl_bundle!(A 0, B 1);
impl_bundle!(A 0, B 1, C 2);
impl_bundle!(A 0, B 1, C 2, D 3);
impl_bundle!(A 0, B 1, C 2, D 3, E 4);
impl_bundle!(A 0, B 1, C 2, D 3, E 4, F 5);

fn two_mut<T>(v: &mut [T], a: usize, b: usize) -> (&mut T, &mut T) {
    debug_assert_ne!(a, b);
    if a < b {
        let (l, r) = v.split_at_mut(b);
        (&mut l[a], &mut r[0])
    } else {
        let (l, r) = v.split_at_mut(a);
        (&mut r[0], &mut l[b])
    }
}

impl World {
    /// Creates an empty world.
    pub fn new() -> Self {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let mut w = Self {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            entities: Entities::default(),
            infos: Vec::new(),
            ids: HashMap::new(),
            names: Vec::new(),
            sparse_sets: Vec::new(),
            archetypes: Vec::new(),
            archetype_index: HashMap::new(),
            bundle_ids: HashMap::new(),
            bundle_components: Vec::new(),
            bundle_edges: Vec::new(),
            resources: HashMap::new(),
            change_tick: 1,
            last_change_tick: 0,
            last_check_tick: 1,
            hierarchy_ids: [0, 0],
            query_caches: Default::default(),
            shared_query_caches: Default::default(),
        };
        w.archetype_for(Vec::new());
        w.hierarchy_ids = [
            w.register::<crate::hierarchy::Parent>(),
            w.register::<crate::hierarchy::Children>(),
        ];
        w
    }

    /// Process-unique id of this world (cached query state is tied to it).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Registers `T` (idempotent) and returns its id.
    pub fn register<T: Component>(&mut self) -> ComponentId {
        let infos = &mut self.infos;
        let names = &mut self.names;
        let sparse_sets = &mut self.sparse_sets;
        *self.ids.entry(TypeId::of::<T>()).or_insert_with(|| {
            let info = ComponentInfo {
                layout: Layout::new::<T>(),
                drop: std::mem::needs_drop::<T>().then_some(drop_ptr::<T> as unsafe fn(*mut u8)),
                storage: T::STORAGE,
            };
            sparse_sets.push(
                (T::STORAGE == StorageType::SparseSet).then(|| ComponentSparseSet::new(&info)),
            );
            infos.push(info);
            names.push(std::any::type_name::<T>());
            infos.len() - 1
        })
    }

    /// Id of a registered component type.
    pub fn component_id(&self, t: TypeId) -> Option<ComponentId> {
        self.ids.get(&t).copied()
    }

    /// Storage kind of a registered component.
    pub fn storage_type(&self, id: ComponentId) -> Option<StorageType> {
        self.infos.get(id).map(|i| i.storage)
    }

    /// The sparse set of a [`StorageType::SparseSet`] component (`None` for table
    /// components and unknown ids).
    pub fn sparse_set(&self, id: ComponentId) -> Option<&ComponentSparseSet> {
        self.sparse_sets.get(id)?.as_ref()
    }

    fn sparse_set_mut(&mut self, id: ComponentId) -> Option<&mut ComponentSparseSet> {
        self.sparse_sets.get_mut(id)?.as_mut()
    }

    /// Where `arch` keeps component `id`, if its entities have it.
    pub(crate) fn component_rows<'w>(
        &'w self,
        arch: &'w Archetype,
        id: ComponentId,
    ) -> Option<ComponentRows<'w>> {
        if let Some(ci) = arch.column_of(id) {
            return Some(ComponentRows {
                column: &arch.columns[ci],
                sparse: None,
            });
        }
        if !arch.has_sparse(id) {
            return None;
        }
        let set = self.sparse_set(id)?;
        Some(ComponentRows {
            column: &set.dense,
            sparse: Some(SparseRows {
                set,
                entities: &arch.entities,
            }),
        })
    }

    /// All archetypes.
    pub fn archetypes(&self) -> &[Archetype] {
        &self.archetypes
    }

    /// Current change tick.
    pub fn change_tick(&self) -> Tick {
        Tick(self.change_tick)
    }

    /// Advances the change tick (wrapping) and returns the new value. Runs
    /// [`World::check_change_ticks`] once at least [`CHECK_TICK_INTERVAL`] ticks have
    /// passed since the previous check.
    pub fn increment_change_tick(&mut self) -> Tick {
        self.change_tick = self.change_tick.wrapping_add(1);
        if self.change_tick.wrapping_sub(self.last_check_tick) >= CHECK_TICK_INTERVAL {
            self.check_change_ticks();
        }
        Tick(self.change_tick)
    }

    /// Clamps every stored component tick (and the world's last-observed tick) to at
    /// most [`MAX_CHANGE_AGE`] old, so wrapping comparisons stay exact forever.
    /// A clamped tick reads as "not changed" for any observer that ran after it.
    pub fn check_change_ticks(&mut self) {
        let now = Tick(self.change_tick);
        let tables = self
            .archetypes
            .iter_mut()
            .flat_map(|a| a.columns.iter_mut());
        let sparse = self.sparse_sets.iter_mut().flatten().map(|s| &mut s.dense);
        for col in tables.chain(sparse) {
            for cell in col.added.iter_mut().chain(col.changed.iter_mut()) {
                cell.get_mut().check_tick(now);
            }
        }
        let mut last = Tick(self.last_change_tick);
        last.check_tick(now);
        self.last_change_tick = last.0;
        self.last_check_tick = self.change_tick;
    }

    /// Sets the change tick (used to test wraparound). Jumps longer than
    /// [`CHECK_TICK_INTERVAL`] skip clamping checks the jumped-over ticks would have
    /// triggered; keep simulated steps at or below it.
    pub fn set_change_tick(&mut self, t: u32) {
        self.change_tick = t;
        self.last_change_tick = t.wrapping_sub(1);
    }

    /// Marks all current changes as observed by `World::query` change filters.
    pub fn clear_trackers(&mut self) {
        self.last_change_tick = self.change_tick;
        self.increment_change_tick();
    }

    fn archetype_for(&mut self, comps: Vec<ComponentId>) -> usize {
        if let Some(&i) = self.archetype_index.get(&comps) {
            return i;
        }
        let (sparse_components, table_components): (Vec<ComponentId>, Vec<ComponentId>) = comps
            .iter()
            .partition(|&&c| self.infos[c].storage == StorageType::SparseSet);
        let columns = table_components
            .iter()
            .map(|&c| Column::new(&self.infos[c]))
            .collect();
        self.archetypes.push(Archetype {
            table_components,
            sparse_components,
            columns,
            entities: Vec::new(),
            remove_edges: HashMap::new(),
            bundle_edges: Vec::new(),
        });
        let i = self.archetypes.len() - 1;
        self.archetype_index.insert(comps, i);
        i
    }

    /// Sorted ids of every component (table and sparse) of archetype `a`.
    fn all_components(&self, a: usize) -> Vec<ComponentId> {
        let a = &self.archetypes[a];
        let mut comps = a.table_components.clone();
        comps.extend_from_slice(&a.sparse_components);
        comps.sort_unstable();
        comps
    }

    /// The archetype reached by removing `id` from archetype `from` (cached).
    fn remove_edge(&mut self, from: usize, id: ComponentId) -> usize {
        if let Some(&t) = self.archetypes[from].remove_edges.get(&id) {
            return t;
        }
        let mut comps = self.all_components(from);
        if let Ok(pos) = comps.binary_search(&id) {
            comps.remove(pos);
        }
        let to = self.archetype_for(comps);
        self.archetypes[from].remove_edges.insert(id, to);
        to
    }

    /// Id of bundle type `B`, registering it (and its component types) on first use.
    fn bundle_id<B: Bundle>(&mut self) -> usize {
        if let Some(&b) = self.bundle_ids.get(&TypeId::of::<B>()) {
            return b;
        }
        let mut ids = Vec::new();
        B::component_ids(self, &mut ids);
        self.bundle_components.push(ids.into_boxed_slice());
        let b = self.bundle_components.len() - 1;
        self.bundle_ids.insert(TypeId::of::<B>(), b);
        b
    }

    /// Index into `self.bundle_edges` of the edge for inserting bundle `b` into
    /// archetype `from` (computed and cached on first use).
    fn bundle_edge(&mut self, from: usize, b: usize) -> usize {
        if let Some(&ix) = self.archetypes[from].bundle_edges.get(b) {
            if ix != NO_EDGE {
                return ix as usize;
            }
        }
        let fields = self.bundle_components[b].clone();
        let mut comps = self.all_components(from);
        for &id in fields.iter() {
            if let Err(pos) = comps.binary_search(&id) {
                comps.insert(pos, id);
            }
        }
        let to = self.archetype_for(comps);
        let mut ops = Vec::with_capacity(fields.len());
        for (k, &id) in fields.iter().enumerate() {
            // Present before this field: in the source archetype, or added by an earlier
            // field of the same bundle (a duplicate).
            let present = self.archetypes[from].contains(id) || fields[..k].contains(&id);
            let op = match (self.infos[id].storage, present) {
                (StorageType::SparseSet, false) => FieldOp::SparseAdd(id),
                (StorageType::SparseSet, true) => FieldOp::SparseReplace(id),
                (StorageType::Table, present) => {
                    // `to` lists every bundle component, and table ones get columns.
                    let ci = self.archetypes[to]
                        .column_of(id)
                        .unwrap_or_else(|| unreachable!("bundle target lacks a column"));
                    if present {
                        FieldOp::TableReplace(ci)
                    } else {
                        FieldOp::TableAdd(ci)
                    }
                }
            };
            ops.push(op);
        }
        self.bundle_edges.push(BundleEdge {
            to,
            ops: ops.into_boxed_slice(),
        });
        let ix = self.bundle_edges.len() - 1;
        // Justification: more than u32::MAX - 1 cached edges is unsupported; truncating
        // would alias edges, so abort like `Entities::alloc` does.
        let ix32 = u32::try_from(ix)
            .ok()
            .filter(|&x| x != NO_EDGE)
            .unwrap_or_else(|| std::process::abort());
        let edges = &mut self.archetypes[from].bundle_edges;
        if edges.len() <= b {
            edges.resize(b + 1, NO_EDGE);
        }
        edges[b] = ix32;
        ix
    }

    /// Spawns an entity with the given components.
    pub fn spawn(&mut self, bundle: impl Bundle) -> Entity {
        let e = self.spawn_empty();
        // Cannot fail: the entity was just created.
        let _ = self.insert_bundle(e, bundle);
        e
    }

    /// Spawns an entity with no components.
    pub fn spawn_empty(&mut self) -> Entity {
        self.flush_entities();
        let e = self.entities.alloc();
        self.place_in_root(e);
        e
    }

    fn place_in_root(&mut self, e: Entity) {
        let arch = &mut self.archetypes[0];
        arch.entities.push(e);
        self.entities.set(
            e.index(),
            Location {
                archetype: 0,
                row: arch.entities.len() - 1,
            },
        );
    }

    /// Reserves an entity id through a shared reference (thread-safe, lock-free).
    /// The id is not alive ([`World::contains`] is false) until
    /// [`World::flush_entities`] runs, which every `&mut World` entity operation and
    /// the schedule's command application do first. Concurrent callers get distinct
    /// ids, but which caller gets which id is not deterministic.
    pub fn reserve_entity(&self) -> Entity {
        self.entities.reserve()
    }

    /// Turns every reserved id into a live, component-less entity.
    pub fn flush_entities(&mut self) {
        if !self.entities.has_pending() {
            return;
        }
        for e in self.entities.flush() {
            self.place_in_root(e);
        }
    }

    /// Whether `e` is alive.
    pub fn contains(&self, e: Entity) -> bool {
        self.entities.get(e).is_some()
    }

    /// Number of live entities.
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// Inserts (or replaces) components on an existing entity. The entity moves to its
    /// new archetype at most once, however many components the bundle adds. Added
    /// components get `added` and `changed` ticks of now, replaced ones a `changed` tick.
    /// On error (no such entity) the bundle is dropped.
    pub fn insert(&mut self, e: Entity, bundle: impl Bundle) -> Result<(), EcsError> {
        self.insert_bundle(e, bundle)
    }

    /// Moves row `row` of archetype `from` to archetype `to`. Columns missing in
    /// `to` are left in their spare slots; returns their column indices so the
    /// caller can drop them after bookkeeping is consistent.
    fn move_entity(&mut self, e: Entity, loc: Location, to: usize) -> (usize, Vec<usize>) {
        let (src, dst) = two_mut(&mut self.archetypes, loc.archetype, to);
        let mut pending = Vec::new();
        for (ci, col) in src.columns.iter_mut().enumerate() {
            let id = src.table_components[ci];
            if let Some(di) = dst.column_of(id) {
                let dcol = &mut dst.columns[di];
                // SAFETY: loc.row < len (entity location invariant); bytes are moved
                // into dst and the source slot becomes spare (not dropped).
                unsafe {
                    dcol.data.push(col.data.get_ptr(loc.row));
                    dcol.added.push(UnsafeCell::new(*col.added[loc.row].get()));
                    dcol.changed
                        .push(UnsafeCell::new(*col.changed[loc.row].get()));
                    col.swap_remove_to_spare(loc.row);
                }
            } else {
                // SAFETY: loc.row < len.
                unsafe { col.swap_remove_to_spare(loc.row) };
                pending.push(ci);
            }
        }
        src.entities.swap_remove(loc.row);
        if let Some(&moved) = src.entities.get(loc.row) {
            self.entities.set(moved.index(), loc);
        }
        dst.entities.push(e);
        let new_row = dst.entities.len() - 1;
        self.entities.set(
            e.index(),
            Location {
                archetype: to,
                row: new_row,
            },
        );
        (new_row, pending)
    }

    fn drop_pending(&mut self, archetype: usize, pending: &[usize]) {
        for &ci in pending {
            // SAFETY: each pending column holds exactly one moved-out-but-live value in
            // its spare slot and nothing has been pushed since.
            unsafe { self.archetypes[archetype].columns[ci].data.drop_spare() };
        }
    }

    /// See [`World::insert`].
    fn insert_bundle<B: Bundle>(&mut self, e: Entity, bundle: B) -> Result<(), EcsError> {
        let b = self.bundle_id::<B>();
        // Checked before taking ownership: on error `bundle` drops normally.
        let loc = self.entities.get(e).ok_or(EcsError::NoSuchEntity(e))?;
        let edge = self.bundle_edge(loc.archetype, b);
        let to = self.bundle_edges[edge].to;
        let tick = Tick(self.change_tick);
        let row = if to == loc.archetype {
            loc.row
        } else {
            let (row, pending) = self.move_entity(e, loc, to);
            // `to` holds every component of the source archetype.
            debug_assert!(pending.is_empty());
            row
        };
        let mut bundle = std::mem::ManuallyDrop::new(bundle);
        let this: *mut B = &mut *bundle;
        // Pass 1: complete the new row by pushing every added value. Only bytes move
        // here (no user code runs), so afterwards every column and sparse set agrees
        // with the entity's archetype and location.
        let mut k = 0;
        let mut add = |src: *mut u8| {
            match self.bundle_edges[edge].ops[k] {
                FieldOp::TableAdd(ci) => {
                    let col = &mut self.archetypes[to].columns[ci];
                    debug_assert_eq!(col.data.len(), row);
                    // SAFETY: `src` is a live value of this column's type (`Bundle`
                    // contract); ownership moves into the column, completing row `row`.
                    unsafe { col.data.push(src) };
                    col.added.push(UnsafeCell::new(tick));
                    col.changed.push(UnsafeCell::new(tick));
                }
                FieldOp::SparseAdd(id) => {
                    if let Some(set) = self.sparse_set_mut(id) {
                        // SAFETY: as above; `e` had no slot, since its old archetype
                        // lacked `id` and no earlier field added it (that would be a
                        // `SparseReplace`).
                        unsafe { set.push(e, src, tick) };
                    }
                }
                FieldOp::TableReplace(_) | FieldOp::SparseReplace(_) => {}
            }
            k += 1;
        };
        // SAFETY: `this` points to the live bundle, which is never dropped.
        unsafe { B::for_each_ptr(this, &mut add) };
        // Pass 2: replacements, in field order. Each drops the value it replaces, which
        // may panic; the world is already consistent, so a panic only leaks the values
        // of the later replaced fields.
        let mut k = 0;
        let mut replace = |src: *mut u8| {
            match self.bundle_edges[edge].ops[k] {
                FieldOp::TableReplace(ci) => {
                    let col = &mut self.archetypes[to].columns[ci];
                    *col.changed[row].get_mut() = tick;
                    // SAFETY: row < len (pass 1 completed the row); ownership of the
                    // live value at `src` moves in; the old value is parked in the
                    // spare slot before its destructor runs.
                    unsafe { col.data.replace(row, src) };
                }
                FieldOp::SparseReplace(id) => {
                    if let Some(set) = self.sparse_set_mut(id) {
                        // SAFETY: as above.
                        let replaced = unsafe { set.replace(e, src, tick) };
                        debug_assert!(replaced, "archetype lists a sparse component the set lacks");
                    }
                }
                FieldOp::TableAdd(_) | FieldOp::SparseAdd(_) => {}
            }
            k += 1;
        };
        // SAFETY: as above; pass 1 moved out only the fields it pushed, so the fields
        // replaced here are still live.
        unsafe { B::for_each_ptr(this, &mut replace) };
        Ok(())
    }

    /// Removes component `T` from `e` (dropping it). Returns whether it was present.
    pub fn remove<T: Component>(&mut self, e: Entity) -> Result<bool, EcsError> {
        let loc = self.entities.get(e).ok_or(EcsError::NoSuchEntity(e))?;
        let Some(id) = self.component_id(TypeId::of::<T>()) else {
            return Ok(false);
        };
        if !self.archetypes[loc.archetype].contains(id) {
            return Ok(false);
        }
        let to = self.remove_edge(loc.archetype, id);
        let (_, pending) = self.move_entity(e, loc, to);
        // Every location is consistent again; unlink a sparse value before any drop runs.
        let sparse = self
            .sparse_set_mut(id)
            .is_some_and(|set| set.remove_to_spare(e));
        self.drop_pending(loc.archetype, &pending);
        if sparse {
            if let Some(set) = self.sparse_set_mut(id) {
                // SAFETY: `remove_to_spare` returned true just above and nothing was
                // pushed to this set since (`drop_pending` only touches table columns).
                unsafe { set.drop_spare() };
            }
        }
        Ok(true)
    }

    /// Despawns `e`, dropping all its components. Returns whether it was alive.
    ///
    /// Keeps the hierarchy consistent: `e` is removed from its parent's
    /// [`Children`](crate::hierarchy::Children) (dropping that component if it empties),
    /// and `e`'s children lose their [`Parent`](crate::hierarchy::Parent), becoming
    /// roots. Use [`World::despawn_recursive`] to despawn them too.
    pub fn despawn(&mut self, e: Entity) -> bool {
        self.flush_entities();
        let Some(loc) = self.entities.get(e) else {
            return false;
        };
        let arch = &self.archetypes[loc.archetype];
        if self.hierarchy_ids.iter().any(|&id| arch.contains(id)) {
            self.detach_for_despawn(e);
        }
        self.despawn_raw(e)
    }

    /// Despawns `e` without touching the hierarchy (its parent's `Children` and its
    /// children's `Parent` are left as they are). Returns whether it was alive.
    pub(crate) fn despawn_raw(&mut self, e: Entity) -> bool {
        self.flush_entities();
        let Some(loc) = self.entities.free(e) else {
            return false;
        };
        let arch = &mut self.archetypes[loc.archetype];
        for col in &mut arch.columns {
            // SAFETY: loc.row < len.
            unsafe { col.swap_remove_to_spare(loc.row) };
        }
        arch.entities.swap_remove(loc.row);
        if let Some(&moved) = arch.entities.get(loc.row) {
            self.entities.set(moved.index(), loc);
        }
        // Unlink every sparse value first, so all bookkeeping is consistent before the
        // first destructor runs (a panicking one then only leaks the rest).
        let sparse_ids = self.archetypes[loc.archetype].sparse_components.clone();
        let mut unlinked = Vec::with_capacity(sparse_ids.len());
        for id in sparse_ids {
            if self
                .sparse_set_mut(id)
                .is_some_and(|set| set.remove_to_spare(e))
            {
                unlinked.push(id);
            }
        }
        let all: Vec<usize> = (0..self.archetypes[loc.archetype].columns.len()).collect();
        self.drop_pending(loc.archetype, &all);
        for id in unlinked {
            if let Some(set) = self.sparse_set_mut(id) {
                // SAFETY: `remove_to_spare` returned true for this set above and nothing
                // was pushed to any set since.
                unsafe { set.drop_spare() };
            }
        }
        true
    }

    /// Shared reference to a component.
    pub fn get<T: Component>(&self, e: Entity) -> Option<&T> {
        let loc = self.entities.get(e)?;
        let arch = &self.archetypes[loc.archetype];
        let id = self.component_id(TypeId::of::<T>())?;
        let ptr = match arch.column_of(id) {
            // SAFETY: row < len (location invariant).
            Some(ci) => unsafe { arch.columns[ci].data.get_ptr(loc.row) },
            None if arch.has_sparse(id) => self.sparse_set(id)?.get_ptr(e)?,
            None => return None,
        };
        // SAFETY: the pointer addresses a live T (column/set of T's id); the shared
        // borrow of self prevents writes for the returned lifetime.
        Some(unsafe { &*ptr.cast::<T>() })
    }

    /// Mutable reference to a component; marks it changed.
    pub fn get_mut<T: Component>(&mut self, e: Entity) -> Option<&mut T> {
        let loc = self.entities.get(e)?;
        let id = self.component_id(TypeId::of::<T>())?;
        let tick = Tick(self.change_tick);
        let arch = &mut self.archetypes[loc.archetype];
        if let Some(ci) = arch.column_of(id) {
            let col = &mut arch.columns[ci];
            *col.changed[loc.row].get_mut() = tick;
            // SAFETY: column stores T; row < len; &mut self guarantees uniqueness.
            return Some(unsafe { &mut *col.data.get_ptr(loc.row).cast::<T>() });
        }
        if !arch.has_sparse(id) {
            return None;
        }
        let set = self.sparse_set_mut(id)?;
        let d = set.dense_index(e)?;
        *set.dense.changed[d].get_mut() = tick;
        // SAFETY: the set stores T; d < len (`dense_index`); &mut self guarantees
        // uniqueness.
        Some(unsafe { &mut *set.dense.data.get_ptr(d).cast::<T>() })
    }

    pub(crate) fn location(&self, e: Entity) -> Option<Location> {
        self.entities.get(e)
    }

    /// Inserts or replaces a resource.
    pub fn insert_resource<R: Send + Sync + 'static>(&mut self, r: R) {
        self.resources.insert(
            TypeId::of::<R>(),
            ResourceCell(UnsafeCell::new(Box::new(r))),
        );
    }

    /// Removes a resource.
    pub fn remove_resource<R: Send + Sync + 'static>(&mut self) -> Option<R> {
        let cell = self.resources.remove(&TypeId::of::<R>())?;
        cell.0.into_inner().downcast::<R>().ok().map(|b| *b)
    }

    /// Shared access to a resource.
    pub fn resource<R: Send + Sync + 'static>(&self) -> Option<&R> {
        let cell = self.resources.get(&TypeId::of::<R>())?;
        // SAFETY: &self; mutable access requires &mut self or a declared exclusive
        // system access, which the scheduler never grants concurrently with this.
        unsafe { (*cell.0.get()).downcast_ref() }
    }

    /// Mutable access to a resource.
    pub fn resource_mut<R: Send + Sync + 'static>(&mut self) -> Option<&mut R> {
        self.resources
            .get_mut(&TypeId::of::<R>())?
            .0
            .get_mut()
            .downcast_mut()
    }

    /// Mutable resource access through a shared world.
    ///
    /// # Safety
    /// Caller must guarantee no other reference to this resource is live.
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn resource_unchecked<R: Send + Sync + 'static>(&self) -> Option<&mut R> {
        let cell = self.resources.get(&TypeId::of::<R>())?;
        // SAFETY: caller guarantees exclusivity.
        unsafe { (*cell.0.get()).downcast_mut() }
    }

    /// Type name of a component id (for diagnostics).
    pub fn component_name(&self, id: ComponentId) -> &'static str {
        self.names.get(id).copied().unwrap_or("<unknown>")
    }
}
