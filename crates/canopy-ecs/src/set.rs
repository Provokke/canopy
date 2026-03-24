//! System sets: interned, type-erased labels that group systems for ordering and
//! run conditions.
//!
//! Any `Clone + Eq + Hash + Debug + Send + Sync + 'static` type can be a set label
//! after `impl SystemSet for MySet {}`. Labels are interned into a process-wide
//! table, so an [`InternedSet`] is a cheap `Copy` id compared by identity; two
//! labels intern to the same id exactly when they have the same type and are `==`.
use std::any::{Any, TypeId};
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

/// Object-safe equality/hashing used to intern [`SystemSet`] labels. Implemented
/// for every `Eq + Hash + 'static` type; not meant to be implemented by hand.
pub trait DynSetLabel {
    /// Whether `other` has the same type and is `==` to `self`.
    fn dyn_eq(&self, other: &dyn Any) -> bool;
    /// Hashes the type id and value.
    fn dyn_hash(&self, state: &mut dyn Hasher);
    /// `self` as `Any`, for [`DynSetLabel::dyn_eq`].
    fn as_any(&self) -> &dyn Any;
}

impl<T: Eq + Hash + Any> DynSetLabel for T {
    fn dyn_eq(&self, other: &dyn Any) -> bool {
        other.downcast_ref::<T>().is_some_and(|o| o == self)
    }
    fn dyn_hash(&self, mut state: &mut dyn Hasher) {
        TypeId::of::<T>().hash(&mut state);
        self.hash(&mut state);
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A label naming a group of systems.
///
/// ```
/// #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
/// enum Physics { Forces, Integrate }
/// impl canopy_ecs::set::SystemSet for Physics {}
/// ```
pub trait SystemSet: DynSetLabel + Debug + Send + Sync + 'static {}

/// An interned [`SystemSet`] label: a `Copy` id plus the leaked label (for `Debug`).
#[derive(Clone, Copy)]
pub struct InternedSet {
    id: u32,
    label: &'static dyn SystemSet,
}

impl PartialEq for InternedSet {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for InternedSet {}
impl Hash for InternedSet {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}
impl Debug for InternedSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.label.fmt(f)
    }
}

impl InternedSet {
    /// The interned label.
    pub fn label(&self) -> &'static dyn SystemSet {
        self.label
    }
}

/// Interned labels: (hash, label). Ids are indices. Entries are leaked on purpose
/// (one per distinct label for the life of the process) and stay reachable from
/// this static.
static INTERNER: Mutex<Vec<(u64, &'static dyn SystemSet)>> = Mutex::new(Vec::new());

/// Interns a set label.
pub fn intern<S: SystemSet>(set: S) -> InternedSet {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    set.dyn_hash(&mut h);
    let hash = h.finish();
    let mut table = INTERNER.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((i, (_, label))) = table
        .iter()
        .enumerate()
        .find(|(_, (eh, l))| *eh == hash && l.dyn_eq(set.as_any()))
    {
        return InternedSet {
            id: i as u32,
            label: *label,
        };
    }
    let label: &'static dyn SystemSet = Box::leak(Box::new(set));
    table.push((hash, label));
    InternedSet {
        id: (table.len() - 1) as u32,
        label,
    }
}

impl<S: SystemSet> From<S> for InternedSet {
    fn from(s: S) -> Self {
        intern(s)
    }
}
