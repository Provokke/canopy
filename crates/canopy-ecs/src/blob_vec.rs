//! Type-erased, contiguous storage for values of a single (runtime-known) type.
//!
//! The allocation is always aligned to at least 16 bytes. Element `i` lives at
//! `data + i * layout.size()`; since a Rust type's size is a multiple of its
//! alignment, every element is correctly aligned.
use std::alloc::{self, Layout};
use std::ptr::NonNull;

/// Minimum alignment of every column allocation.
pub const MIN_ALIGN: usize = 16;

/// A vector of values of a type known only by its [`Layout`] and drop function.
pub struct BlobVec {
    item: Layout,
    align: usize,
    cap: usize,
    len: usize,
    data: NonNull<u8>,
    drop: Option<unsafe fn(*mut u8)>,
}

// SAFETY: a BlobVec is only ever created for `T: Send + Sync + 'static` component
// types (see `World::register`), so sending/sharing the raw storage is sound.
unsafe impl Send for BlobVec {}
// SAFETY: see the `Send` impl above; mutation requires `&mut self` or the
// scheduler's access discipline documented in ARCHITECTURE.md.
unsafe impl Sync for BlobVec {}

fn capacity_overflow() -> ! {
    // Justification: an allocation larger than isize::MAX cannot be satisfied;
    // aborting is the only option that preserves memory safety without panicking.
    std::process::abort()
}

impl BlobVec {
    /// Creates an empty vector for items of `item` layout.
    pub fn new(item: Layout, drop: Option<unsafe fn(*mut u8)>) -> Self {
        let align = item.align().max(MIN_ALIGN);
        // Dangling but well-aligned pointer; never dereferenced for non-ZSTs until allocated.
        let data = NonNull::new(std::ptr::without_provenance_mut::<u8>(align))
            .unwrap_or(NonNull::dangling());
        let cap = if item.size() == 0 { usize::MAX } else { 0 };
        Self {
            item,
            align,
            cap,
            len: 0,
            data,
            drop,
        }
    }

    /// Number of stored items.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the vector is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Base pointer of the allocation (aligned to at least [`MIN_ALIGN`]).
    pub fn as_ptr(&self) -> *mut u8 {
        self.data.as_ptr()
    }

    fn reserve_one(&mut self) {
        if self.len < self.cap {
            return;
        }
        let new_cap = self
            .cap
            .checked_mul(2)
            .unwrap_or_else(|| capacity_overflow())
            .max(4);
        let size = self
            .item
            .size()
            .checked_mul(new_cap)
            .unwrap_or_else(|| capacity_overflow());
        let new_layout =
            Layout::from_size_align(size, self.align).unwrap_or_else(|_| capacity_overflow());
        let ptr = if self.cap == 0 {
            // SAFETY: new_layout has non-zero size (item size > 0 since ZSTs have cap=MAX).
            unsafe { alloc::alloc(new_layout) }
        } else {
            let old = Layout::from_size_align(self.item.size() * self.cap, self.align)
                .unwrap_or_else(|_| capacity_overflow());
            // SAFETY: data was allocated with `old`; new size is non-zero and fits isize.
            unsafe { alloc::realloc(self.data.as_ptr(), old, size) }
        };
        self.data = NonNull::new(ptr).unwrap_or_else(|| alloc::handle_alloc_error(new_layout));
        self.cap = new_cap;
    }

    /// Pointer to item `index`.
    ///
    /// # Safety
    /// `index <= len` (one-past-the-end is allowed for spare-slot tricks) and
    /// `index < cap` unless the type is a ZST.
    pub unsafe fn get_ptr(&self, index: usize) -> *mut u8 {
        // SAFETY: caller guarantees index is within the allocation.
        unsafe { self.data.as_ptr().add(index * self.item.size()) }
    }

    /// Moves the value at `src` into a new slot at the end.
    ///
    /// # Safety
    /// `src` must point to a valid value of the item type; ownership moves into the
    /// vector, so the caller must not drop it.
    pub unsafe fn push(&mut self, src: *const u8) {
        self.reserve_one();
        // SAFETY: reserve_one guarantees `len < cap`; regions cannot overlap since src
        // is not inside our spare capacity (caller-owned value).
        unsafe { std::ptr::copy_nonoverlapping(src, self.get_ptr(self.len), self.item.size()) };
        self.len += 1;
    }

    /// Replaces the value at `index`, dropping the old one.
    ///
    /// The old value is first moved into the spare slot past `len`, so if its
    /// destructor panics the vector is already consistent (the old value leaks).
    ///
    /// # Safety
    /// `index < len`; `src` as for [`push`](Self::push).
    pub unsafe fn replace(&mut self, index: usize, src: *const u8) {
        self.reserve_one();
        let size = self.item.size();
        // SAFETY: index < len < cap, so both slots are inside the allocation and distinct.
        unsafe {
            let slot = self.get_ptr(index);
            let spare = self.get_ptr(self.len);
            std::ptr::copy_nonoverlapping(slot, spare, size);
            std::ptr::copy_nonoverlapping(src, slot, size);
            if let Some(d) = self.drop {
                d(spare);
            }
        }
    }

    /// Swap-removes `index`, leaving the removed value in the spare slot at the new
    /// `len` *without dropping it*. Call [`drop_spare`](Self::drop_spare) afterwards
    /// (once all bookkeeping is consistent) or move the bytes out first.
    ///
    /// # Safety
    /// `index < len`.
    pub unsafe fn swap_remove_to_spare(&mut self, index: usize) {
        let last = self.len - 1;
        if index != last {
            // SAFETY: both indices < len, distinct; swap keeps both values valid.
            unsafe {
                std::ptr::swap_nonoverlapping(
                    self.get_ptr(index),
                    self.get_ptr(last),
                    self.item.size(),
                )
            };
        }
        self.len = last;
    }

    /// Drops the value in the spare slot at `len` left by
    /// [`swap_remove_to_spare`](Self::swap_remove_to_spare).
    ///
    /// # Safety
    /// The spare slot must hold a live value that nothing else will drop.
    pub unsafe fn drop_spare(&mut self) {
        if let Some(d) = self.drop {
            // SAFETY: caller guarantees the spare slot holds an owned, live value.
            unsafe { d(self.get_ptr(self.len)) };
        }
    }

    /// Drops all items.
    pub fn clear(&mut self) {
        let len = self.len;
        // Set len first: if a destructor panics, remaining items leak instead of
        // being dropped twice.
        self.len = 0;
        if let Some(d) = self.drop {
            for i in 0..len {
                // SAFETY: i < old len; each value is dropped exactly once.
                unsafe { d(self.get_ptr(i)) };
            }
        }
    }
}

impl Drop for BlobVec {
    fn drop(&mut self) {
        self.clear();
        if self.item.size() != 0 && self.cap != 0 {
            if let Ok(layout) = Layout::from_size_align(self.item.size() * self.cap, self.align) {
                // SAFETY: allocated in reserve_one with exactly this layout.
                unsafe { alloc::dealloc(self.data.as_ptr(), layout) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    unsafe fn drop_rc(p: *mut u8) {
        // SAFETY: test only stores Rc<()>.
        unsafe { std::ptr::drop_in_place(p.cast::<Rc<()>>()) }
    }

    #[test]
    fn alignment_and_drop_counts() {
        let rc = Rc::new(());
        let mut v = BlobVec::new(Layout::new::<Rc<()>>(), Some(drop_rc));
        for _ in 0..10 {
            let x = std::mem::ManuallyDrop::new(rc.clone());
            // SAFETY: x is a valid Rc whose ownership moves into v.
            unsafe { v.push((&*x as *const Rc<()>).cast()) };
        }
        assert_eq!(v.as_ptr() as usize % MIN_ALIGN, 0);
        assert_eq!(Rc::strong_count(&rc), 11);
        // SAFETY: 3 < len; the spare value is then dropped exactly once.
        unsafe {
            v.swap_remove_to_spare(3);
            v.drop_spare();
        }
        assert_eq!(Rc::strong_count(&rc), 10);
        let x = std::mem::ManuallyDrop::new(rc.clone());
        // SAFETY: 0 < len, x moves in.
        unsafe { v.replace(0, (&*x as *const Rc<()>).cast()) };
        assert_eq!(Rc::strong_count(&rc), 10);
        drop(v);
        assert_eq!(Rc::strong_count(&rc), 1);
    }

    #[test]
    fn zst() {
        let mut v = BlobVec::new(Layout::new::<()>(), None);
        for _ in 0..100 {
            // SAFETY: ZST, any aligned pointer is valid.
            unsafe { v.push(std::ptr::NonNull::<()>::dangling().as_ptr().cast()) };
        }
        assert_eq!(v.len(), 100);
    }
}
