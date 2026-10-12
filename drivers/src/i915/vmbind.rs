//! Softpin bookkeeping of one i915 VM: which object is bound where.
//!
//! The i915 rule (`eb_pin_vma` / `i915_vma_misplaced`): an object has at most
//! one binding per VM. EXECBUFFER2 names the address it wants for every
//! object (EXEC_OBJECT_PINNED); when the object is already there nothing
//! happens, when it is elsewhere it moves, and whatever else occupies the
//! requested range is evicted (unbound). Iris and ANV manage their own VA
//! heaps, so in practice a range is only re-used after its object was freed;
//! the eviction path is the safety net.
//!
//! Pure: `plan` says what to unbind and whether to bind; the caller applies
//! it to the page tables and then calls `commit`.

#![allow(dead_code)]

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub obj: u32,
    pub len: u64,
}

#[derive(Default, Debug)]
pub struct VmMap {
    /// start -> binding, non-overlapping.
    by_start: BTreeMap<u64, Binding>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// (start, len) ranges to unbind first.
    pub unbind: Vec<(u64, u64)>,
    /// Whether [start, start + len) must be (re)written.
    pub bind: bool,
}

impl VmMap {
    pub fn new() -> Self { Self::default() }
    pub fn len(&self) -> usize { self.by_start.len() }
    pub fn bound_bytes(&self) -> u64 { self.by_start.values().map(|b| b.len).sum() }
    pub fn iter(&self) -> impl Iterator<Item = (&u64, &Binding)> { self.by_start.iter() }

    /// Where `obj` is bound, if anywhere.
    pub fn find(&self, obj: u32) -> Option<(u64, u64)> {
        self.by_start.iter().find(|(_, b)| b.obj == obj).map(|(&s, b)| (s, b.len))
    }

    /// The binding covering `va`.
    pub fn covering(&self, va: u64) -> Option<(u64, Binding)> {
        let (&s, b) = self.by_start.range(..=va).next_back()?;
        if va - s < b.len { Some((s, *b)) } else { None }
    }

    /// What binding `obj` (`len` bytes) at `start` takes.
    pub fn plan(&self, obj: u32, start: u64, len: u64) -> Plan {
        if let Some(b) = self.by_start.get(&start) {
            if b.obj == obj && b.len == len {
                return Plan { unbind: Vec::new(), bind: false };
            }
        }
        let end = start + len;
        let mut unbind = Vec::new();
        // Overlaps: the last binding starting before `start` and every
        // binding starting inside the range.
        if let Some((&s, b)) = self.by_start.range(..start).next_back() {
            if s + b.len > start { unbind.push((s, b.len)); }
        }
        for (&s, b) in self.by_start.range(start..end) { unbind.push((s, b.len)); }
        // The object's binding elsewhere moves.
        if let Some((s, l)) = self.find(obj) {
            if !unbind.contains(&(s, l)) { unbind.push((s, l)); }
        }
        Plan { unbind, bind: true }
    }

    /// Record a plan the caller carried out.
    pub fn commit(&mut self, obj: u32, start: u64, len: u64, p: &Plan) {
        for (s, _) in &p.unbind { self.by_start.remove(s); }
        if p.bind { self.by_start.insert(start, Binding { obj, len }); }
    }

    /// Forget `obj` (it is being freed); its range, if it had one.
    pub fn remove_obj(&mut self, obj: u32) -> Option<(u64, u64)> {
        let (s, l) = self.find(obj)?;
        self.by_start.remove(&s);
        Some((s, l))
    }

    /// Every binding, for teardown.
    pub fn drain(&mut self) -> Vec<(u64, Binding)> {
        core::mem::take(&mut self.by_start).into_iter().collect()
    }
}
