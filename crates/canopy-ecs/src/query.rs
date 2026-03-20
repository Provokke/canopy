//! Typed queries over archetypes.
use crate::entity::Entity;
use crate::error::EcsError;
use crate::world::{Archetype, Component, ComponentRows, SparseRows, Tick, World};
use std::any::{type_name, TypeId};
use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;

/// Declared read/write access of a query or system, keyed by `TypeId`.
#[derive(Default, Clone, Debug)]
pub struct Access {
    /// Read-only accesses.
    pub reads: HashMap<TypeId, &'static str>,
    /// Exclusive accesses.
    pub writes: HashMap<TypeId, &'static str>,
}

impl Access {
    /// Adds a read; errors if the same type is already written.
    pub fn add_read<T: 'static>(&mut self) -> Result<(), EcsError> {
        if self.writes.contains_key(&TypeId::of::<T>()) {
            return Err(EcsError::ConflictingQueryAccess(type_name::<T>()));
        }
        self.reads.insert(TypeId::of::<T>(), type_name::<T>());
        Ok(())
    }
    /// Adds a write; errors if the type is already read or written.
    pub fn add_write<T: 'static>(&mut self) -> Result<(), EcsError> {
        let id = TypeId::of::<T>();
        if self.writes.contains_key(&id) || self.reads.contains_key(&id) {
            return Err(EcsError::ConflictingQueryAccess(type_name::<T>()));
        }
        self.writes.insert(id, type_name::<T>());
        Ok(())
    }
    /// Whether two accesses conflict (a write overlapping any access).
    pub fn conflicts(&self, other: &Access) -> bool {
        self.first_conflict(other).is_some()
    }
    /// Name of one type through which the two accesses conflict, if any.
    pub fn first_conflict(&self, other: &Access) -> Option<&'static str> {
        self.writes
            .iter()
            .find(|(k, _)| other.writes.contains_key(k) || other.reads.contains_key(k))
            .or_else(|| {
                other
                    .writes
                    .iter()
                    .find(|(k, _)| self.reads.contains_key(k))
            })
            .map(|(_, n)| *n)
    }
    /// Adds every read and write of `other` without conflict checks (a union; the
    /// result may hold the same type as both read and write).
    pub fn extend(&mut self, other: &Access) {
        self.reads.extend(other.reads.iter().map(|(k, n)| (*k, *n)));
        self.writes
            .extend(other.writes.iter().map(|(k, n)| (*k, *n)));
    }
    /// Whether `needed` is a subset of this declared access (writes cover reads).
    pub fn covers(&self, needed: &Access) -> Result<(), &'static str> {
        for (k, n) in &needed.writes {
            if !self.writes.contains_key(k) {
                return Err(n);
            }
        }
        for (k, n) in &needed.reads {
            if !self.writes.contains_key(k) && !self.reads.contains_key(k) {
                return Err(n);
            }
        }
        Ok(())
    }
}

/// The [`Access`] of one query (or resource parameter) plus archetype-level
/// guarantees about which entities it can touch.
///
/// `with` holds component types that **every** archetype the query can match
/// contains; `without` holds types that **no** archetype it can match contains.
/// Two filtered accesses are [disjoint](Self::is_disjoint) when one requires a
/// type the other excludes: then no archetype, hence no entity and no column row,
/// is reachable by both, so their reads/writes cannot alias even if the
/// component types overlap.
///
/// Only conjunctive positions add to the sets (`With<T>`, `Without<T>`, and the
/// required `&T`, `&mut T`, `Changed<T>`, `Added<T>`, through tuples). `Or<..>`
/// and `Option<&T>` add nothing, so they never make anything disjoint.
#[derive(Default, Clone, Debug)]
pub struct FilteredAccess {
    access: Access,
    with: HashSet<TypeId>,
    without: HashSet<TypeId>,
}

impl FilteredAccess {
    /// The filtered access of `Query<D, F>`; errors if `D`/`F` alias each other.
    pub fn of<D: QueryData, F: QueryFilter>() -> Result<Self, EcsError> {
        let mut fa = Self::default();
        D::access(&mut fa.access)?;
        F::access(&mut fa.access)?;
        D::archetype_filter(&mut fa);
        F::archetype_filter(&mut fa);
        Ok(fa)
    }
    /// An access with no archetype guarantees (resources, builder declarations).
    pub fn unfiltered(access: Access) -> Self {
        Self {
            access,
            ..Self::default()
        }
    }
    /// The component/resource reads and writes.
    pub fn access(&self) -> &Access {
        &self.access
    }
    /// Records that every matched archetype contains `T`.
    ///
    /// Recording a type the query does not actually require makes the
    /// disjointness proof wrong; it is only reachable from the `unsafe`
    /// [`QueryData`]/[`QueryFilter`]/`SystemParam` impls, whose contracts forbid it.
    pub fn add_with<T: 'static>(&mut self) {
        self.with.insert(TypeId::of::<T>());
    }
    /// Records that no matched archetype contains `T` (same caveat as [`Self::add_with`]).
    pub fn add_without<T: 'static>(&mut self) {
        self.without.insert(TypeId::of::<T>());
    }
    /// Whether this access carries no archetype guarantee at all.
    pub fn is_unfiltered(&self) -> bool {
        self.with.is_empty() && self.without.is_empty()
    }
    /// Whether no archetype can be matched by both: one requires a component
    /// type the other excludes.
    pub fn is_disjoint(&self, other: &FilteredAccess) -> bool {
        !self.with.is_disjoint(&other.without) || !self.without.is_disjoint(&other.with)
    }
    /// Whether the two may touch the same data with at least one write.
    pub fn conflicts(&self, other: &FilteredAccess) -> bool {
        self.access.conflicts(&other.access) && !self.is_disjoint(other)
    }
}

/// Access of a whole system: one [`FilteredAccess`] per parameter (or one
/// unfiltered entry for a builder-declared system).
///
/// Entries of one system are pairwise non-conflicting (checked by [`Self::add`]).
/// Two systems conflict when any entry of one conflicts with any entry of the other.
#[derive(Default, Clone, Debug)]
pub struct SystemAccess {
    combined: Access,
    unfiltered: Access,
    entries: Vec<FilteredAccess>,
}

impl SystemAccess {
    /// A system access with a single unfiltered entry.
    pub fn from_unfiltered(access: Access) -> Self {
        let mut s = Self::default();
        s.combined.extend(&access);
        s.unfiltered.extend(&access);
        s.entries.push(FilteredAccess::unfiltered(access));
        s
    }
    /// Adds an entry; errors with [`EcsError::ConflictingQueryAccess`] if it
    /// conflicts with an entry already present.
    pub fn add(&mut self, fa: FilteredAccess) -> Result<(), EcsError> {
        for e in &self.entries {
            if e.conflicts(&fa) {
                return Err(EcsError::ConflictingQueryAccess(
                    e.access.first_conflict(&fa.access).unwrap_or("<unknown>"),
                ));
            }
        }
        self.combined.extend(&fa.access);
        if fa.is_unfiltered() {
            self.unfiltered.extend(&fa.access);
        }
        self.entries.push(fa);
        Ok(())
    }
    /// Adds an unfiltered entry.
    pub fn add_unfiltered(&mut self, access: Access) -> Result<(), EcsError> {
        self.add(FilteredAccess::unfiltered(access))
    }
    /// Union of every entry's reads and writes, ignoring filters.
    pub fn combined(&self) -> &Access {
        &self.combined
    }
    /// The per-parameter entries.
    pub fn entries(&self) -> &[FilteredAccess] {
        &self.entries
    }
    /// Whether the two systems may touch the same data with at least one write.
    pub fn conflicts(&self, other: &SystemAccess) -> bool {
        // Fast path: if even the unions do not conflict, no pair of entries does.
        self.combined.conflicts(&other.combined)
            && self
                .entries
                .iter()
                .any(|a| other.entries.iter().any(|b| a.conflicts(b)))
    }
    /// Whether `needed` is covered by this system's **unfiltered** entries.
    ///
    /// Filtered entries are deliberately excluded: a filtered entry only licenses
    /// the matching archetypes, and an ad-hoc query checked here carries no filter.
    pub fn covers(&self, needed: &Access) -> Result<(), &'static str> {
        self.unfiltered.covers(needed)
    }
}

/// Data fetched by a query.
///
/// # Safety
/// `access` must declare every component that `fetch` reads or writes, and
/// `fetch` must only hand out `&mut` to components declared as writes.
/// `archetype_filter` may record `T` as *with* only if `matches` is false for every
/// archetype lacking `T`, and as *without* only if `matches` is false for every
/// archetype containing `T` (recording nothing is always sound).
pub unsafe trait QueryData {
    /// Item yielded per entity.
    type Item<'w>;
    /// Per-archetype cursor.
    type Fetch<'w>;
    /// Records accessed components.
    fn access(access: &mut Access) -> Result<(), EcsError>;
    /// Records archetype-level guarantees (see [`FilteredAccess`]). Default: none.
    fn archetype_filter(_filter: &mut FilteredAccess) {}
    /// Whether an archetype can produce items.
    fn matches(world: &World, arch: &Archetype) -> bool;
    /// Prepares a cursor for a matching archetype.
    ///
    /// # Safety
    /// `arch` must match; access must be valid for `'w`.
    unsafe fn prepare<'w>(world: &'w World, arch: &'w Archetype, this_run: Tick)
        -> Self::Fetch<'w>;
    /// Fetches row `row`.
    ///
    /// # Safety
    /// `row < arch.len()`, and each row is fetched at most once per iteration for mutable data.
    unsafe fn fetch<'w>(f: &mut Self::Fetch<'w>, row: usize) -> Self::Item<'w>;
}

/// Row filters.
///
/// # Safety
/// Same contract as [`QueryData`] (including `archetype_filter`); filters never
/// hand out references.
pub unsafe trait QueryFilter {
    /// Per-archetype state.
    type Fetch<'w>;
    /// Records read components.
    fn access(access: &mut Access) -> Result<(), EcsError>;
    /// Records archetype-level guarantees (see [`FilteredAccess`]). Default: none.
    fn archetype_filter(_filter: &mut FilteredAccess) {}
    /// Archetype-level match.
    fn matches(world: &World, arch: &Archetype) -> bool;
    /// Prepares per-archetype state.
    ///
    /// # Safety
    /// `arch` must match.
    unsafe fn prepare<'w>(
        world: &'w World,
        arch: &'w Archetype,
        last_run: Tick,
        this_run: Tick,
    ) -> Self::Fetch<'w>;
    /// Row-level match.
    ///
    /// # Safety
    /// `row < arch.len()`.
    unsafe fn filter(f: &mut Self::Fetch<'_>, row: usize) -> bool;
}

/// Whether `arch`'s entities have component `T` (table column or sparse set).
fn has<T: 'static>(world: &World, arch: &Archetype) -> bool {
    world
        .component_id(TypeId::of::<T>())
        .is_some_and(|id| arch.contains(id))
}

/// Storage of `T` for `arch`'s entities, if they have it.
fn rows<'w, T: 'static>(world: &'w World, arch: &'w Archetype) -> Option<ComponentRows<'w>> {
    world.component_rows(arch, world.component_id(TypeId::of::<T>())?)
}

/// Archetype row -> storage slot: identity for table columns, the entity's dense
/// slot for sparse sets (always `< len` of the dense column, see [`SparseRows`]).
#[inline]
fn slot(sparse: &Option<SparseRows<'_>>, row: usize) -> usize {
    match sparse {
        None => row,
        Some(s) => s.dense(row),
    }
}

/// Cursor for `&T` and `Option<&T>`: the value column base (table column or sparse
/// dense column; null if absent) plus the sparse row mapping.
pub struct ReadFetch<'w, T> {
    data: *const T,
    sparse: Option<SparseRows<'w>>,
}

impl<'w, T: 'static> ReadFetch<'w, T> {
    fn new(w: &'w World, arch: &'w Archetype) -> Self {
        match rows::<T>(w, arch) {
            Some(r) => Self {
                data: r.column.data.as_ptr().cast::<T>(),
                sparse: r.sparse,
            },
            None => Self {
                data: std::ptr::null(),
                sparse: None,
            },
        }
    }
}

// SAFETY: declares a read of T; fetch only creates shared references.
unsafe impl<T: Component> QueryData for &T {
    type Item<'w> = &'w T;
    type Fetch<'w> = ReadFetch<'w, T>;
    fn access(a: &mut Access) -> Result<(), EcsError> {
        a.add_read::<T>()
    }
    // `matches` requires the archetype to contain `T`.
    fn archetype_filter(f: &mut FilteredAccess) {
        f.add_with::<T>();
    }
    fn matches(w: &World, arch: &Archetype) -> bool {
        has::<T>(w, arch)
    }
    unsafe fn prepare<'w>(w: &'w World, arch: &'w Archetype, _: Tick) -> ReadFetch<'w, T> {
        ReadFetch::new(w, arch)
    }
    unsafe fn fetch<'w>(f: &mut Self::Fetch<'w>, row: usize) -> &'w T {
        // SAFETY: `data` is T's column base for a matching archetype; `slot` is `row`
        // (< len) for a table column or a checked in-bounds dense slot.
        unsafe { &*f.data.add(slot(&f.sparse, row)) }
    }
}

/// Mutable access to one component, yielded by `&mut T` queries. Reading through
/// it (`Deref`) leaves change detection alone; the component is marked changed (its
/// tick set to the current run) only by an actual mutable access: `DerefMut`,
/// [`Mut::set_changed`] or [`Mut::into_inner`].
pub struct Mut<'w, T> {
    value: &'w mut T,
    changed: &'w mut Tick,
    this_run: Tick,
}

impl<'w, T> Mut<'w, T> {
    /// Marks the component changed without touching it.
    pub fn set_changed(&mut self) {
        *self.changed = self.this_run;
    }
    /// Mutable access that does **not** mark the component changed.
    pub fn bypass_change_detection(&mut self) -> &mut T {
        self.value
    }
    /// Converts into a plain `&mut T` for `'w`, marking the component changed.
    pub fn into_inner(self) -> &'w mut T {
        *self.changed = self.this_run;
        self.value
    }
}

impl<T> std::ops::Deref for Mut<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T> std::ops::DerefMut for Mut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        *self.changed = self.this_run;
        self.value
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Mut<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(f)
    }
}

/// Cursor for `&mut T`.
pub struct WriteFetch<'w, T> {
    data: *mut T,
    changed: *const std::cell::UnsafeCell<Tick>,
    sparse: Option<SparseRows<'w>>,
    tick: Tick,
    _p: PhantomData<&'w mut T>,
}

// SAFETY: declares a write of T; each row is handed out at most once per iteration
// (iterators visit rows exactly once) so the `&mut` value and `&mut` tick inside each
// `Mut` never alias.
unsafe impl<T: Component> QueryData for &mut T {
    type Item<'w> = Mut<'w, T>;
    type Fetch<'w> = WriteFetch<'w, T>;
    fn access(a: &mut Access) -> Result<(), EcsError> {
        a.add_write::<T>()
    }
    // `matches` requires the archetype to contain `T`.
    fn archetype_filter(f: &mut FilteredAccess) {
        f.add_with::<T>();
    }
    fn matches(w: &World, arch: &Archetype) -> bool {
        has::<T>(w, arch)
    }
    unsafe fn prepare<'w>(w: &'w World, arch: &'w Archetype, this_run: Tick) -> WriteFetch<'w, T> {
        match rows::<T>(w, arch) {
            Some(r) => WriteFetch {
                data: r.column.data.as_ptr().cast::<T>(),
                changed: r.column.changed.as_ptr(),
                sparse: r.sparse,
                tick: this_run,
                _p: PhantomData,
            },
            None => WriteFetch {
                data: std::ptr::null_mut(),
                changed: std::ptr::null(),
                sparse: None,
                tick: this_run,
                _p: PhantomData,
            },
        }
    }
    unsafe fn fetch<'w>(f: &mut Self::Fetch<'w>, row: usize) -> Self::Item<'w> {
        // `slot` is `row` (< len) for a table column, or the entity's checked in-bounds
        // dense slot in T's sparse set. Distinct rows are distinct entities, hence
        // distinct slots in either storage.
        let i = slot(&f.sparse, row);
        // SAFETY: i < len, exclusive write access was granted for T (which covers T's
        // tick cells: readers of them declare a read of T), and the row is visited
        // once, so both references are unique for 'w. Archetype-disjoint accesses that
        // share a sparse set's dense column touch only their own entities' slots. The
        // tick is written only when the `Mut` is mutably dereferenced.
        unsafe {
            Mut {
                value: &mut *f.data.add(i),
                changed: &mut *(*f.changed.add(i)).get(),
                this_run: f.tick,
            }
        }
    }
}

// SAFETY: no component access.
unsafe impl QueryData for Entity {
    type Item<'w> = Entity;
    type Fetch<'w> = &'w [Entity];
    fn access(_: &mut Access) -> Result<(), EcsError> {
        Ok(())
    }
    fn matches(_: &World, _: &Archetype) -> bool {
        true
    }
    unsafe fn prepare<'w>(_: &'w World, arch: &'w Archetype, _: Tick) -> &'w [Entity] {
        arch.entities()
    }
    unsafe fn fetch<'w>(f: &mut Self::Fetch<'w>, row: usize) -> Self::Item<'w> {
        f[row]
    }
}

// SAFETY: delegates to `&T`; null pointer means absent.
unsafe impl<T: Component> QueryData for Option<&T> {
    type Item<'w> = Option<&'w T>;
    type Fetch<'w> = ReadFetch<'w, T>;
    fn access(a: &mut Access) -> Result<(), EcsError> {
        a.add_read::<T>()
    }
    fn matches(_: &World, _: &Archetype) -> bool {
        true
    }
    unsafe fn prepare<'w>(w: &'w World, arch: &'w Archetype, _: Tick) -> ReadFetch<'w, T> {
        // Null `data` when the archetype lacks T.
        ReadFetch::new(w, arch)
    }
    unsafe fn fetch<'w>(f: &mut Self::Fetch<'w>, row: usize) -> Option<&'w T> {
        // SAFETY: non-null means the archetype has T, so as for `&T`.
        (!f.data.is_null()).then(|| unsafe { &*f.data.add(slot(&f.sparse, row)) })
    }
}

macro_rules! impl_data_tuple {
    ($($n:ident),*) => {
        // SAFETY: access of all members is merged (conflicts rejected), fetch delegates.
        unsafe impl<$($n: QueryData),*> QueryData for ($($n,)*) {
            type Item<'w> = ($($n::Item<'w>,)*);
            type Fetch<'w> = ($($n::Fetch<'w>,)*);
            fn access(a: &mut Access) -> Result<(), EcsError> { $($n::access(a)?;)* Ok(()) }
            // AND of members: each member's guarantee holds for the tuple.
            fn archetype_filter(f: &mut FilteredAccess) { $($n::archetype_filter(f);)* }
            fn matches(w: &World, arch: &Archetype) -> bool { true $(&& $n::matches(w, arch))* }
            unsafe fn prepare<'w>(w: &'w World, arch: &'w Archetype, t: Tick) -> Self::Fetch<'w> {
                // SAFETY: forwarded contract.
                unsafe { ($($n::prepare(w, arch, t),)*) }
            }
            #[allow(non_snake_case)]
            unsafe fn fetch<'w>(f: &mut Self::Fetch<'w>, row: usize) -> Self::Item<'w> {
                let ($($n,)*) = f;
                // SAFETY: forwarded contract.
                unsafe { ($($n::fetch($n, row),)*) }
            }
        }
        // SAFETY: AND of member filters.
        unsafe impl<$($n: QueryFilter),*> QueryFilter for ($($n,)*) {
            type Fetch<'w> = ($($n::Fetch<'w>,)*);
            fn access(a: &mut Access) -> Result<(), EcsError> { $($n::access(a)?;)* Ok(()) }
            // AND of members: each member's guarantee holds for the tuple.
            fn archetype_filter(f: &mut FilteredAccess) { $($n::archetype_filter(f);)* }
            fn matches(w: &World, arch: &Archetype) -> bool { true $(&& $n::matches(w, arch))* }
            unsafe fn prepare<'w>(w: &'w World, arch: &'w Archetype, l: Tick, t: Tick) -> Self::Fetch<'w> {
                // SAFETY: forwarded contract.
                unsafe { ($($n::prepare(w, arch, l, t),)*) }
            }
            #[allow(non_snake_case)]
            unsafe fn filter(f: &mut Self::Fetch<'_>, row: usize) -> bool {
                let ($($n,)*) = f;
                // SAFETY: forwarded contract.
                unsafe { true $(&& $n::filter($n, row))* }
            }
        }
    };
}
impl_data_tuple!(A);
impl_data_tuple!(A, B);
impl_data_tuple!(A, B, C);
impl_data_tuple!(A, B, C, D);
impl_data_tuple!(A, B, C, D, E);

// SAFETY: matches everything.
unsafe impl QueryFilter for () {
    type Fetch<'w> = ();
    fn access(_: &mut Access) -> Result<(), EcsError> {
        Ok(())
    }
    fn matches(_: &World, _: &Archetype) -> bool {
        true
    }
    unsafe fn prepare<'w>(_: &'w World, _: &'w Archetype, _: Tick, _: Tick) {}
    unsafe fn filter(_: &mut (), _: usize) -> bool {
        true
    }
}

/// Matches entities that have `T`.
pub struct With<T>(PhantomData<T>);
/// Matches entities that lack `T`.
pub struct Without<T>(PhantomData<T>);
/// Matches entities whose `T` changed (or was added) since the observer last ran.
pub struct Changed<T>(PhantomData<T>);

// SAFETY: archetype-level only; `matches` requires the archetype to contain `T`, so
// `with T` holds.
unsafe impl<T: Component> QueryFilter for With<T> {
    type Fetch<'w> = ();
    fn access(_: &mut Access) -> Result<(), EcsError> {
        Ok(())
    }
    fn archetype_filter(f: &mut FilteredAccess) {
        f.add_with::<T>();
    }
    fn matches(w: &World, arch: &Archetype) -> bool {
        has::<T>(w, arch)
    }
    unsafe fn prepare<'w>(_: &'w World, _: &'w Archetype, _: Tick, _: Tick) {}
    unsafe fn filter(_: &mut (), _: usize) -> bool {
        true
    }
}

// SAFETY: archetype-level only; `matches` rejects any archetype containing `T`
// (table column or sparse set), so `without T` holds.
unsafe impl<T: Component> QueryFilter for Without<T> {
    type Fetch<'w> = ();
    fn access(_: &mut Access) -> Result<(), EcsError> {
        Ok(())
    }
    fn archetype_filter(f: &mut FilteredAccess) {
        f.add_without::<T>();
    }
    fn matches(w: &World, arch: &Archetype) -> bool {
        !has::<T>(w, arch)
    }
    unsafe fn prepare<'w>(_: &'w World, _: &'w Archetype, _: Tick, _: Tick) {}
    unsafe fn filter(_: &mut (), _: usize) -> bool {
        true
    }
}

/// Cursor for [`Changed`] and [`Added`].
pub struct ChangedFetch<'w> {
    ticks: &'w [std::cell::UnsafeCell<Tick>],
    sparse: Option<SparseRows<'w>>,
    last_run: Tick,
    this_run: Tick,
}

impl<'w> ChangedFetch<'w> {
    fn new<T: 'static>(
        w: &'w World,
        arch: &'w Archetype,
        added: bool,
        last_run: Tick,
        this_run: Tick,
    ) -> Self {
        let (ticks, sparse) = match rows::<T>(w, arch) {
            Some(r) if added => (r.column.added.as_slice(), r.sparse),
            Some(r) => (r.column.changed.as_slice(), r.sparse),
            None => (&[][..], None),
        };
        Self {
            ticks,
            sparse,
            last_run,
            this_run,
        }
    }
}

// SAFETY: declares a read of T and only reads its tick cells; `matches` requires
// the archetype to contain `T`, so `with T` holds.
unsafe impl<T: Component> QueryFilter for Changed<T> {
    type Fetch<'w> = ChangedFetch<'w>;
    fn access(a: &mut Access) -> Result<(), EcsError> {
        a.add_read::<T>()
    }
    fn archetype_filter(f: &mut FilteredAccess) {
        f.add_with::<T>();
    }
    fn matches(w: &World, arch: &Archetype) -> bool {
        has::<T>(w, arch)
    }
    unsafe fn prepare<'w>(
        w: &'w World,
        arch: &'w Archetype,
        last_run: Tick,
        this_run: Tick,
    ) -> ChangedFetch<'w> {
        ChangedFetch::new::<T>(w, arch, false, last_run, this_run)
    }
    unsafe fn filter(f: &mut ChangedFetch<'_>, row: usize) -> bool {
        // Bounds-checked slot (identity for tables, the entity's dense slot otherwise).
        let cell = &f.ticks[slot(&f.sparse, row)];
        // SAFETY: read access to T is declared, so no concurrent writer of this cell
        // exists (writers of other entities' slots in a shared sparse column touch
        // other cells).
        let t = unsafe { *cell.get() };
        t.is_newer_than(f.last_run, f.this_run)
    }
}

/// Matches entities whose `T` was added since the observer last ran.
pub struct Added<T>(PhantomData<T>);

// SAFETY: declares a read of T and only reads its tick cells; `matches` requires
// the archetype to contain `T`, so `with T` holds.
unsafe impl<T: Component> QueryFilter for Added<T> {
    type Fetch<'w> = ChangedFetch<'w>;
    fn access(a: &mut Access) -> Result<(), EcsError> {
        a.add_read::<T>()
    }
    fn archetype_filter(f: &mut FilteredAccess) {
        f.add_with::<T>();
    }
    fn matches(w: &World, arch: &Archetype) -> bool {
        has::<T>(w, arch)
    }
    unsafe fn prepare<'w>(
        w: &'w World,
        arch: &'w Archetype,
        last_run: Tick,
        this_run: Tick,
    ) -> ChangedFetch<'w> {
        ChangedFetch::new::<T>(w, arch, true, last_run, this_run)
    }
    unsafe fn filter(f: &mut ChangedFetch<'_>, row: usize) -> bool {
        // SAFETY: forwarded; same reasoning as `Changed`.
        unsafe { <Changed<T> as QueryFilter>::filter(f, row) }
    }
}

/// Matches entities satisfying at least one of the tuple's filters.
pub struct Or<T>(PhantomData<T>);

macro_rules! impl_or {
    ($($n:ident),*) => {
        // SAFETY: union of member accesses; a member's row filter is only evaluated
        // when that member matched the archetype (so its fetch is valid).
        unsafe impl<$($n: QueryFilter),*> QueryFilter for Or<($($n,)*)> {
            type Fetch<'w> = ($(Option<$n::Fetch<'w>>,)*);
            fn access(a: &mut Access) -> Result<(), EcsError> { $($n::access(a)?;)* Ok(()) }
            // Deliberately records nothing: a member's guarantee need not hold for the
            // union, so `Or` never makes two accesses disjoint (conservative).
            fn archetype_filter(_: &mut FilteredAccess) {}
            fn matches(w: &World, arch: &Archetype) -> bool { false $(|| $n::matches(w, arch))* }
            unsafe fn prepare<'w>(w: &'w World, arch: &'w Archetype, l: Tick, t: Tick) -> Self::Fetch<'w> {
                // SAFETY: only prepared for members that match.
                ($( $n::matches(w, arch).then(|| unsafe { $n::prepare(w, arch, l, t) }), )*)
            }
            #[allow(non_snake_case)]
            unsafe fn filter(f: &mut Self::Fetch<'_>, row: usize) -> bool {
                let ($($n,)*) = f;
                // SAFETY: forwarded contract for matched members.
                false $(|| $n.as_mut().is_some_and(|x| unsafe { $n::filter(x, row) }))*
            }
        }
    };
}
impl_or!(A, B);
impl_or!(A, B, C);

/// Archetype ids matching a query, extended incrementally.
///
/// Archetypes are only ever appended to a world, and a query's archetype-level match
/// depends only on an archetype's component set, so a cached id never goes stale.
/// `seen` is the `archetypes().len()` already examined: `ArchetypeCache::update`
/// tests only the archetypes created since, and does nothing when none were. The
/// cache is tied to one [`World::id`]; used with another world it starts over.
///
/// It is untyped so it can live in a [`SystemParam::State`](crate::system::SystemParam)
/// (which must be `'static`); it must only ever be updated with a single `(D, F)`,
/// which [`QueryState`] and the `Query` parameter guarantee by construction.
#[derive(Default, Debug)]
pub struct ArchetypeCache {
    world_id: u64,
    seen: usize,
    matched: Vec<usize>,
}

impl ArchetypeCache {
    /// Number of archetypes already examined.
    pub fn archetypes_seen(&self) -> usize {
        self.seen
    }

    /// Ids of archetypes matched so far (ascending; may include empty ones).
    pub fn matched(&self) -> &[usize] {
        &self.matched
    }

    /// Examines archetypes created since the last call and returns all matches.
    pub(crate) fn update<D: QueryData, F: QueryFilter>(&mut self, world: &World) -> &[usize] {
        if self.world_id != world.id() || self.seen > world.archetypes.len() {
            self.world_id = world.id();
            self.seen = 0;
            self.matched.clear();
        }
        for (i, a) in world.archetypes.iter().enumerate().skip(self.seen) {
            if D::matches(world, a) && F::matches(world, a) {
                self.matched.push(i);
            }
        }
        self.seen = world.archetypes.len();
        &self.matched
    }
}

/// One [`ArchetypeCache`] per query type `(D, F)`, owned by a [`World`] and used by
/// [`World::query`], [`World::par_for_each`] and the closure-system
/// [`SystemContext::query`](crate::schedule::SystemContext::query) /
/// [`par_for_each`](crate::schedule::SystemContext::par_for_each), so none of them
/// scans every archetype per call. Each cache is only ever updated with its own key's
/// `(D, F)`, as [`ArchetypeCache`] requires.
#[derive(Default)]
pub(crate) struct QueryCaches {
    index: std::collections::HashMap<std::any::TypeId, usize>,
    caches: Vec<ArchetypeCache>,
}

impl QueryCaches {
    /// Slot of `(D, F)`'s cache, creating an empty one on first use.
    fn slot<D: QueryData + 'static, F: QueryFilter + 'static>(&mut self) -> usize {
        let caches = &mut self.caches;
        *self
            .index
            .entry(std::any::TypeId::of::<(D, F)>())
            .or_insert_with(|| {
                caches.push(ArchetypeCache::default());
                caches.len() - 1
            })
    }
}

impl World {
    /// Updates `(D, F)`'s cache in the world's exclusive caches with archetypes
    /// created since the last call; returns its slot in `query_caches.caches`.
    fn cached_slot<D: QueryData + 'static, F: QueryFilter + 'static>(&mut self) -> usize {
        let slot = self.query_caches.slot::<D, F>();
        // Taken out while it is updated against `&self` (moves a Vec; no allocation).
        let mut cache = std::mem::take(&mut self.query_caches.caches[slot]);
        cache.update::<D, F>(self);
        self.query_caches.caches[slot] = cache;
        slot
    }

    /// Like [`Self::cached_slot`] through `&self` (concurrent systems): uses the
    /// shared, mutex-guarded cache and returns a copy of the matched ids.
    pub(crate) fn cached_matches_shared<D: QueryData + 'static, F: QueryFilter + 'static>(
        &self,
    ) -> Vec<usize> {
        // Nothing panics while the lock is held (only archetype matching runs), and a
        // poisoned cache is still valid: every state of it is a consistent prefix.
        let mut caches = self
            .shared_query_caches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let slot = caches.slot::<D, F>();
        caches.caches[slot].update::<D, F>(self).to_vec()
    }
}

/// A reusable query that remembers which archetypes match.
///
/// [`World::query`] tests every archetype on each call; a `QueryState` tests each
/// archetype once and afterwards only the ones created since (see [`ArchetypeCache`]).
pub struct QueryState<D: QueryData, F: QueryFilter = ()> {
    cache: ArchetypeCache,
    _p: PhantomData<fn() -> (D, F)>,
}

impl<D: QueryData, F: QueryFilter> Default for QueryState<D, F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: QueryData, F: QueryFilter> QueryState<D, F> {
    /// An empty state; it matches archetypes lazily on first use.
    pub fn new() -> Self {
        Self {
            cache: ArchetypeCache::default(),
            _p: PhantomData,
        }
    }

    /// The cached archetype bookkeeping (for inspection and tests).
    pub fn cache(&self) -> &ArchetypeCache {
        &self.cache
    }

    /// Like [`World::query`], but without scanning archetypes already examined.
    pub fn iter<'w>(&'w mut self, world: &'w mut World) -> Result<QueryIter<'w, D, F>, EcsError> {
        let mut a = Access::default();
        D::access(&mut a)?;
        F::access(&mut a)?;
        let (last, this) = (Tick(world.last_change_tick), Tick(world.change_tick));
        let world: &'w World = world;
        let matched = self.cache.update::<D, F>(world);
        // SAFETY: `&mut World` gives exclusive access; `matched` holds only archetypes
        // that match `D` and `F` for this world.
        Ok(unsafe {
            QueryIter::with_matched(world, std::borrow::Cow::Borrowed(matched), last, this)
        })
    }
}

/// Iterator over query results.
pub struct QueryIter<'w, D: QueryData, F: QueryFilter = ()> {
    world: &'w World,
    archetypes: std::borrow::Cow<'w, [usize]>,
    next_archetype: usize,
    current: Option<(D::Fetch<'w>, F::Fetch<'w>, usize, usize)>,
    last_run: Tick,
    this_run: Tick,
}

impl<'w, D: QueryData, F: QueryFilter> QueryIter<'w, D, F> {
    /// Iterates pre-matched archetypes.
    ///
    /// # Safety
    /// The caller must hold the access `D`/`F` declare for `'w`, and no other
    /// iterator may hand out the same mutable rows concurrently. Every id in `matched`
    /// must be an archetype of `world` that matches `D` and `F`.
    pub(crate) unsafe fn with_matched(
        world: &'w World,
        matched: std::borrow::Cow<'w, [usize]>,
        last_run: Tick,
        this_run: Tick,
    ) -> Self {
        Self {
            world,
            archetypes: matched,
            next_archetype: 0,
            current: None,
            last_run,
            this_run,
        }
    }
}

impl<'w, D: QueryData, F: QueryFilter> Iterator for QueryIter<'w, D, F> {
    type Item = D::Item<'w>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some((d, f, row, len)) = &mut self.current {
                while *row < *len {
                    let r = *row;
                    *row += 1;
                    // SAFETY: r < archetype len; each row visited once; access held per `new`.
                    unsafe {
                        if F::filter(f, r) {
                            return Some(D::fetch(d, r));
                        }
                    }
                }
            }
            let a = *self.archetypes.get(self.next_archetype)?;
            self.next_archetype += 1;
            let arch = &self.world.archetypes[a];
            if arch.is_empty() {
                continue;
            }
            // SAFETY: arch matches D and F (`with_matched` contract).
            self.current = Some(unsafe {
                (
                    D::prepare(self.world, arch, self.this_run),
                    F::prepare(self.world, arch, self.last_run, self.this_run),
                    0,
                    arch.len(),
                )
            });
        }
    }
}

impl World {
    /// Iterates `D` over all entities matching filter `F`.
    ///
    /// The matching archetypes are cached per `(D, F)` in the world, so a call only
    /// tests archetypes created since the previous call with the same types (`D` and
    /// `F` must be `'static`; written-out query types such as `(&mut A, &B)` are).
    ///
    /// Fails at runtime with [`EcsError::ConflictingQueryAccess`] when `D` aliases
    /// (e.g. `(&mut A, &mut A)` or `(&A, &mut A)`).
    pub fn query<D: QueryData + 'static, F: QueryFilter + 'static>(
        &mut self,
    ) -> Result<QueryIter<'_, D, F>, EcsError> {
        let mut a = Access::default();
        D::access(&mut a)?;
        F::access(&mut a)?;
        let (last, this) = (Tick(self.last_change_tick), Tick(self.change_tick));
        let slot = self.cached_slot::<D, F>();
        let world: &World = self;
        let matched = &world.query_caches.caches[slot].matched;
        // SAFETY: `&mut self` gives exclusive access to everything for the lifetime;
        // `matched` holds only archetypes of this world matching `D` and `F`.
        Ok(unsafe {
            QueryIter::with_matched(world, std::borrow::Cow::Borrowed(matched), last, this)
        })
    }

    /// Fetches `D` for one entity.
    pub fn query_one<D: QueryData>(&mut self, e: Entity) -> Result<Option<D::Item<'_>>, EcsError> {
        let mut a = Access::default();
        D::access(&mut a)?;
        let loc = self.location(e).ok_or(EcsError::NoSuchEntity(e))?;
        let arch = &self.archetypes[loc.archetype];
        if !D::matches(self, arch) {
            return Ok(None);
        }
        let this = Tick(self.change_tick);
        // SAFETY: exclusive world borrow; row < len by location invariant.
        Ok(Some(unsafe {
            let mut f = D::prepare(self, arch, this);
            D::fetch(&mut f, loc.row)
        }))
    }
}

/// Rows per parallel task.
const PAR_CHUNK: usize = if cfg!(miri) { 4 } else { 4096 };

/// Runs `f` for every matching entity, splitting archetypes into row chunks that
/// execute in parallel (rayon pool; scoped std threads under Miri).
///
/// # Safety
/// Same as [`QueryIter::new`]: the caller holds the declared access for `'w` and
/// no other code accesses mutable rows concurrently. Chunks are disjoint row ranges,
/// so `&mut` items handed to different threads never alias.
/// Every id in `matched` must be an archetype of `world` matching `D` and `F`.
unsafe fn par_for_each_unchecked<'w, D: QueryData, F: QueryFilter>(
    world: &'w World,
    matched: &[usize],
    last_run: Tick,
    this_run: Tick,
    f: &(impl Fn(D::Item<'w>) + Sync),
) {
    let mut chunks = Vec::new();
    for &a in matched {
        let arch = &world.archetypes[a];
        if arch.is_empty() {
            continue;
        }
        let mut s = 0;
        while s < arch.len() {
            let e = (s + PAR_CHUNK).min(arch.len());
            chunks.push((a, s, e));
            s = e;
        }
    }
    let run = |(a, s, e): (usize, usize, usize)| {
        let arch = &world.archetypes[a];
        // SAFETY: arch matches D and F; rows s..e are < len and owned by this chunk only.
        unsafe {
            let mut d = D::prepare(world, arch, this_run);
            let mut fl = F::prepare(world, arch, last_run, this_run);
            for r in s..e {
                if F::filter(&mut fl, r) {
                    f(D::fetch(&mut d, r));
                }
            }
        }
    };
    #[cfg(not(miri))]
    {
        use rayon::prelude::*;
        chunks.into_par_iter().for_each(run);
    }
    #[cfg(miri)]
    std::thread::scope(|scope| {
        for c in chunks {
            let run = &run;
            scope.spawn(move || run(c));
        }
    });
}

impl World {
    /// Parallel version of [`World::query`] + `for_each` (same archetype cache).
    pub fn par_for_each<'w, D: QueryData + 'static, F: QueryFilter + 'static>(
        &'w mut self,
        f: impl Fn(D::Item<'w>) + Sync,
    ) -> Result<(), EcsError> {
        let mut a = Access::default();
        D::access(&mut a)?;
        F::access(&mut a)?;
        let (last, this) = (Tick(self.last_change_tick), Tick(self.change_tick));
        let slot = self.cached_slot::<D, F>();
        let world: &'w World = self;
        let matched = &world.query_caches.caches[slot].matched;
        // SAFETY: `&mut self` gives exclusive access for 'w; `matched` is this world's
        // cache for `(D, F)`.
        unsafe { par_for_each_unchecked::<D, F>(world, matched, last, this, &f) };
        Ok(())
    }
}

impl<'w> crate::schedule::SystemContext<'w> {
    /// Parallel query iteration inside a system; same access rules (and archetype
    /// cache) as `query`.
    pub fn par_for_each<D: QueryData + 'static, F: QueryFilter + 'static>(
        &self,
        f: impl Fn(D::Item<'w>) + Sync,
    ) -> Result<(), EcsError> {
        let (world, last, this) = self.claim::<D, F>()?;
        let matched = world.cached_matches_shared::<D, F>();
        // SAFETY: `claim` verified declared access and the one-mutable-query rule;
        // `matched` is this world's cache for `(D, F)`.
        unsafe { par_for_each_unchecked::<D, F>(world, &matched, last, this, &f) };
        Ok(())
    }
}
