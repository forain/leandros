//! Memslot bookkeeping (pure; the kernel glue is `kernel/src/kvm/vm.rs`).
//!
//! Rules follow Linux `__kvm_set_memory_region` as far as QEMU can observe:
//! slot id < 32 and address space 0 only, page alignment, size 0 deletes,
//! overlap with another slot is EEXIST, a live slot may only move (same hva,
//! size and READONLY) and LOG_DIRTY_PAGES is refused (M1-M3 have no dirty log).
use crate::abi::*;

pub const EINVAL: i32 = 22;
pub const EEXIST: i32 = 17;
const PAGE: u64 = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Slot {
    pub used: bool,
    pub gpa: u64,
    pub size: u64,
    pub hva: u64,
    pub flags: u32,
}

impl Slot {
    pub fn readonly(&self) -> bool { self.flags & KVM_MEM_READONLY != 0 }
    pub fn gpa_end(&self) -> u64 { self.gpa + self.size }
}

/// What a successful `set` did; the caller unmaps `old` (stage 2) for Deleted/Moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Created,
    Deleted(Slot),
    Moved(Slot),
    Nothing,
}

pub struct SlotTable {
    slots: [Slot; KVM_NR_MEMSLOTS as usize],
}

impl SlotTable {
    pub const fn new() -> Self {
        SlotTable { slots: [Slot { used: false, gpa: 0, size: 0, hva: 0, flags: 0 }; KVM_NR_MEMSLOTS as usize] }
    }

    pub fn get(&self, i: usize) -> Option<Slot> {
        self.slots.get(i).copied().filter(|s| s.used)
    }

    pub fn count(&self) -> usize { self.slots.iter().filter(|s| s.used).count() }

    /// `ipa_limit` = 1 << ipa_bits. Errors are positive errno values.
    pub fn set(&mut self, r: &KvmUserspaceMemoryRegion, ipa_limit: u64) -> Result<Change, i32> {
        if r.slot >> 16 != 0 { return Err(EINVAL); } // address space id != 0
        let id = (r.slot & 0xffff) as usize;
        if id >= KVM_NR_MEMSLOTS as usize { return Err(EINVAL); }
        if r.flags & !(KVM_MEM_LOG_DIRTY_PAGES | KVM_MEM_READONLY) != 0 { return Err(EINVAL); }
        if r.flags & KVM_MEM_LOG_DIRTY_PAGES != 0 { return Err(EINVAL); }
        if r.guest_phys_addr % PAGE != 0 || r.memory_size % PAGE != 0 || r.userspace_addr % PAGE != 0 {
            return Err(EINVAL);
        }
        let old = self.slots[id];
        if r.memory_size == 0 {
            if !old.used { return Err(EINVAL); }
            self.slots[id] = Slot::default();
            return Ok(Change::Deleted(old));
        }
        let gend = r.guest_phys_addr.checked_add(r.memory_size).ok_or(EINVAL)?;
        r.userspace_addr.checked_add(r.memory_size).ok_or(EINVAL)?;
        if gend > ipa_limit { return Err(EINVAL); }
        for (i, s) in self.slots.iter().enumerate() {
            if i != id && s.used && r.guest_phys_addr < s.gpa_end() && s.gpa < gend { return Err(EEXIST); }
        }
        let new = Slot { used: true, gpa: r.guest_phys_addr, size: r.memory_size, hva: r.userspace_addr, flags: r.flags };
        if !old.used {
            self.slots[id] = new;
            return Ok(Change::Created);
        }
        if new.hva != old.hva || new.size != old.size || (new.flags ^ old.flags) & KVM_MEM_READONLY != 0 {
            return Err(EINVAL);
        }
        if new.gpa != old.gpa {
            self.slots[id] = new;
            return Ok(Change::Moved(old));
        }
        Ok(Change::Nothing)
    }

    /// The slot covering `gpa`: (index, slot).
    pub fn find_gpa(&self, gpa: u64) -> Option<(usize, Slot)> {
        self.slots.iter().enumerate()
            .find(|(_, s)| s.used && gpa >= s.gpa && gpa < s.gpa_end())
            .map(|(i, s)| (i, *s))
    }

    /// Host virtual address backing `gpa`.
    pub fn gpa_to_hva(&self, gpa: u64) -> Option<(u64, Slot)> {
        self.find_gpa(gpa).map(|(_, s)| (s.hva + (gpa - s.gpa), s))
    }

    /// Every slot overlapping host range `[va, va+len)`, as the stage-2 range
    /// `(ipa, len)` it backs. Fills `out`, returns the count.
    pub fn ranges_for_hva(&self, va: u64, len: u64, out: &mut [(u64, u64); KVM_NR_MEMSLOTS as usize]) -> usize {
        let end = va.saturating_add(len);
        let mut n = 0;
        for s in self.slots.iter().filter(|s| s.used) {
            let (s0, s1) = (s.hva, s.hva + s.size);
            if va < s1 && s0 < end {
                let (a, b) = (va.max(s0), end.min(s1));
                out[n] = (s.gpa + (a - s0), b - a);
                n += 1;
            }
        }
        n
    }
}
