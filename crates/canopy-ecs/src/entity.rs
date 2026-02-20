//! Generational entity identifiers with free-list reuse.
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicIsize, Ordering};

/// An entity id: an index plus a generation that is bumped on each reuse.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Entity {
    index: u32,
    generation: NonZeroU32,
}

impl Entity {
    /// Slot index.
    pub fn index(self) -> u32 {
        self.index
    }
    /// Generation of the slot when this id was issued.
    pub fn generation(self) -> u32 {
        self.generation.get()
    }
    /// Packs the id into a `u64`.
    pub fn to_bits(self) -> u64 {
        (u64::from(self.generation.get()) << 32) | u64::from(self.index)
    }
}

/// Where an entity's data lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Location {
    pub archetype: usize,
    pub row: usize,
}

#[derive(Clone, Copy, Debug)]
struct Meta {
    generation: NonZeroU32,
    location: Option<Location>,
}

/// Allocator for entity ids.
///
/// Ids can be *reserved* through a shared reference (`Entities::reserve`): a
/// lock-free atomic cursor hands out free-list slots first, then fresh indices past
/// the end of `meta`. A reserved id is not alive (no location) until
/// `flush` materialises it, which needs exclusive access.
#[derive(Default, Debug)]
pub struct Entities {
    meta: Vec<Meta>,
    free: Vec<u32>,
    len: usize,
    /// While `>= 0`: `free[..cursor]` is still unreserved. While `< 0`: the whole free
    /// list and `-cursor` fresh indices beyond `meta.len()` are reserved. Equals
    /// `free.len()` when nothing is pending.
    free_cursor: AtomicIsize,
}

impl Entities {
    /// Reserves an id without exclusive access; unique across threads. The id is not
    /// alive until [`Entities::flush`].
    pub(crate) fn reserve(&self) -> Entity {
        let n = self.free_cursor.fetch_sub(1, Ordering::Relaxed);
        if n > 0 {
            // n - 1 >= 0 and <= free.len(): the cursor never exceeds the free list.
            let index = self.free[(n - 1) as usize];
            Entity {
                index,
                generation: self.meta[index as usize].generation,
            }
        } else {
            // Justification: more than u32::MAX slots is unsupported (see `alloc`).
            let index = u32::try_from(self.meta.len() as u64 + n.unsigned_abs() as u64)
                .unwrap_or_else(|_| std::process::abort());
            Entity {
                index,
                generation: NonZeroU32::MIN,
            }
        }
    }

    /// Whether reserved ids are waiting to be flushed.
    pub(crate) fn has_pending(&self) -> bool {
        self.free_cursor.load(Ordering::Relaxed) != self.free.len() as isize
    }

    /// Materialises every reserved id (as an entity without a location yet) and
    /// returns them; the caller must give each one a location.
    pub(crate) fn flush(&mut self) -> Vec<Entity> {
        let cursor = *self.free_cursor.get_mut();
        let mut out = Vec::new();
        let taken = if cursor >= 0 {
            self.free.split_off(cursor as usize)
        } else {
            std::mem::take(&mut self.free)
        };
        for index in taken {
            self.len += 1;
            out.push(Entity {
                index,
                generation: self.meta[index as usize].generation,
            });
        }
        if cursor < 0 {
            for _ in 0..cursor.unsigned_abs() {
                // Justification: see `alloc`.
                let index =
                    u32::try_from(self.meta.len()).unwrap_or_else(|_| std::process::abort());
                self.meta.push(Meta {
                    generation: NonZeroU32::MIN,
                    location: None,
                });
                self.len += 1;
                out.push(Entity {
                    index,
                    generation: NonZeroU32::MIN,
                });
            }
        }
        *self.free_cursor.get_mut() = self.free.len() as isize;
        out
    }

    /// Allocates a new id, reusing freed slots (LIFO) with a bumped generation.
    /// Reserved ids must be flushed first.
    pub(crate) fn alloc(&mut self) -> Entity {
        debug_assert!(!self.has_pending());
        self.len += 1;
        if let Some(index) = self.free.pop() {
            *self.free_cursor.get_mut() = self.free.len() as isize;
            let generation = self.meta[index as usize].generation;
            Entity { index, generation }
        } else {
            // Justification: more than u32::MAX live slots is unsupported; saturating
            // would alias ids, so abort instead of panicking.
            let index = u32::try_from(self.meta.len()).unwrap_or_else(|_| std::process::abort());
            self.meta.push(Meta {
                generation: NonZeroU32::MIN,
                location: None,
            });
            Entity {
                index,
                generation: NonZeroU32::MIN,
            }
        }
    }

    /// Frees an id; returns its last location if it was alive.
    pub(crate) fn free(&mut self, e: Entity) -> Option<Location> {
        debug_assert!(!self.has_pending());
        let meta = self.meta.get_mut(e.index as usize)?;
        if meta.generation != e.generation {
            return None;
        }
        let loc = meta.location.take();
        // Wrapping generation (skipping 0). After 2^32-1 reuses of one slot a stale
        // id could alias again; documented limitation.
        meta.generation =
            NonZeroU32::new(meta.generation.get().wrapping_add(1)).unwrap_or(NonZeroU32::MIN);
        self.free.push(e.index);
        *self.free_cursor.get_mut() = self.free.len() as isize;
        self.len -= 1;
        loc
    }

    pub(crate) fn get(&self, e: Entity) -> Option<Location> {
        let meta = self.meta.get(e.index as usize)?;
        if meta.generation == e.generation {
            meta.location
        } else {
            None
        }
    }

    pub(crate) fn set(&mut self, index: u32, loc: Location) {
        if let Some(m) = self.meta.get_mut(index as usize) {
            m.location = Some(loc);
        }
    }

    /// Number of live entities.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there are no live entities.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}
