//! Memslots and the stage-2 fault path (kvm-plan D4).
//!
//! Locking: address space (AS busy) -> `Vm::s2`. The range-invalidate hook runs
//! with the AS busy and never takes the AS lock; `slots` is never held across
//! either, so it is taken, read, and dropped first.
use super::*;
use ::kvm::memslot::Change;
use super::vm::{NVMS, VMS};
use ::kvm::s2::{Attr, Perm, S2Err};

#[derive(Debug, PartialEq, Eq)]
pub enum FaultErr {
    /// No memslot covers the IPA: MMIO (kvm-run exits to userspace).
    NoSlot,
    /// A write to a KVM_MEM_READONLY slot: MMIO exit as well.
    ReadOnly,
    /// The slot's host range cannot be pinned (unmapped / no access / device).
    Bad,
}

fn dcache_line() -> usize {
    let ctr: u64;
    unsafe { core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr, options(nomem, nostack)); }
    4usize << ((ctr >> 16) & 0xf)
}

/// Clean+invalidate a frame to the PoC: a guest with its MMU off reads non-cacheable.
fn clean_to_poc(pa: usize) {
    let (va, line) = (mm::phys_to_virt(pa), dcache_line());
    let mut p = va;
    while p < va + 4096 {
        unsafe { core::arch::asm!("dc civac, {}", in(reg) p, options(nostack)); }
        p += line;
    }
    unsafe { core::arch::asm!("dsb ish", options(nostack)); }
}

fn sync_icache() {
    unsafe { core::arch::asm!("dsb ish", "ic ialluis", "dsb ish", "isb", options(nostack)); }
}

impl Vm {
    /// KVM_SET_USER_MEMORY_REGION. No mapping happens here; stage 2 fills lazily.
    pub(super) fn set_memory_region(&self, r: &KvmUserspaceMemoryRegion) -> isize {
        let change = self.slots.lock().set(r, 1u64 << self.ipa_bits);
        match change {
            Err(e) => -(e as isize),
            Ok(Change::Deleted(old)) | Ok(Change::Moved(old)) => { self.unmap_ipa(old.gpa, old.size); 0 }
            Ok(_) => 0,
        }
    }

    /// Map the page of `ipa` into stage 2 (called by kvm-run on a stage-2 abort
    /// with the vCPU thread inside the VM's address space). `exec` = instruction abort.
    pub fn fault(&self, ipa: u64, write: bool, exec: bool) -> Result<(), FaultErr> {
        let page = ipa & !0xfff;
        let (hva, slot) = self.slots.lock().gpa_to_hva(page).ok_or(FaultErr::NoSlot)?;
        if write && slot.readonly() { return Err(FaultErr::ReadOnly); }
        let cur = self.s2.lock().walk(page);
        if let Some((_, perm)) = cur {
            let (need_w, need_x) = (write && !perm.write, exec && !perm.exec);
            if !need_w && !need_x { return Ok(()); } // raced with another vCPU
            if !need_w {
                sync_icache();
                let mut s2 = self.s2.lock();
                if s2.protect(page, Perm { read: true, write: perm.write, exec: true }) {
                    unsafe { arch_aarch64::kvm::tlb::tlbi_ipa(self.vttbr(), page); }
                }
                return Ok(());
            }
            // Read-only mapping, write fault: the frame may be shared (CoW), so drop it and re-pin for write.
            self.unmap_ipa(page, 4096);
        }
        for attempt in 0..2 {
            let done = sched::with_current_address_space_mut(|as_| {
                let pa = as_.pin_user_page(hva as usize, write)?;
                let mut s2 = self.s2.lock();
                if let Some((old, operm)) = s2.walk(page) {
                    // Another vCPU mapped it between our walk and the pin: keep one pin only.
                    mm::vmm::unpin_user_page(pa);
                    if old == pa as u64 {
                        let np = Perm { read: true, write: operm.write || write, exec: operm.exec || exec };
                        if np != operm && s2.protect(page, np) {
                            unsafe { arch_aarch64::kvm::tlb::tlbi_ipa(self.vttbr(), page); }
                        }
                    }
                    return Some(Ok(()));
                }
                clean_to_poc(pa);
                match s2.map(page, pa as u64, Perm { read: true, write, exec }, Attr::Normal) {
                    Ok(()) => Some(Ok(())),
                    Err(S2Err::Exists) => { mm::vmm::unpin_user_page(pa); Some(Ok(())) }
                    Err(_) => { mm::vmm::unpin_user_page(pa); Some(Err(FaultErr::Bad)) }
                }
            });
            match done {
                Some(Some(r)) => { if r.is_ok() && exec { sync_icache(); } return r; }
                _ => {}
            }
            // Absent page (file-backed): fault it in with the AS unlocked, then retry once.
            if attempt == 0 { sched::prefault_user_page(hva as usize); }
        }
        Err(FaultErr::Bad)
    }
}

/// mm range-invalidate hook (`as_id` = page-table root): unmap the stage-2
/// ranges backed by host `[va, va+len)` of every VM bound to that address space.
pub(super) fn invalidate(as_id: usize, va: usize, len: usize) {
    if NVMS.load(Ordering::Acquire) == 0 { return; }
    let vms: Vec<Arc<Vm>> = VMS.lock().iter().filter_map(|w| w.upgrade()).filter(|v| v.as_root == as_id).collect();
    for vm in vms {
        let mut out = [(0u64, 0u64); KVM_NR_MEMSLOTS as usize];
        let n = vm.slots.lock().ranges_for_hva(va as u64, len as u64, &mut out);
        for &(ipa, l) in &out[..n] { vm.unmap_ipa(ipa, l); }
    }
}
