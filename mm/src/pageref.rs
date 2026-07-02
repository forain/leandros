//! Physical-page reference counts for copy-on-write sharing.
//!
//! Only pages that have ever been shared (by `fork()`) are tracked here —
//! absence from the map means refcount 1 (sole owner), which is the common
//! case for memory that's never been through `clone_as`.  This keeps the
//! non-forking fast path exactly as cheap as it was before CoW existed.

extern crate alloc;
use alloc::collections::BTreeMap;
use spin::Mutex;

static PAGE_REFS: Mutex<BTreeMap<usize, u32>> = Mutex::new(BTreeMap::new());

/// Current reference count for `phys`. `1` if untracked (sole owner).
pub fn get(phys: usize) -> u32 {
    PAGE_REFS.lock().get(&phys).copied().unwrap_or(1)
}

/// Record a new reference to `phys` (called once per additional owner, e.g.
/// once per sibling created by `clone_as`).
pub fn inc(phys: usize) {
    let mut refs = PAGE_REFS.lock();
    let count = refs.entry(phys).or_insert(1);
    *count += 1;
}

/// Drop one reference to `phys` without freeing it. Removes the tracking
/// entry once the count returns to 1 (back to sole ownership, untracked).
pub fn dec(phys: usize) {
    let mut refs = PAGE_REFS.lock();
    if let Some(count) = refs.get_mut(&phys) {
        if *count > 1 { *count -= 1; }
        if *count <= 1 { refs.remove(&phys); }
    }
}

/// Release a reference to a single physical page, freeing it back to the
/// buddy allocator only if this was the last reference.
pub fn unref_or_free(phys: usize, order: usize) {
    let mut refs = PAGE_REFS.lock();
    match refs.get_mut(&phys) {
        Some(count) if *count > 1 => {
            *count -= 1;
            if *count <= 1 { refs.remove(&phys); }
        }
        _ => {
            refs.remove(&phys);
            drop(refs);
            crate::buddy::free(phys, order);
        }
    }
}
