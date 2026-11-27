//! VMID allocator with generation rollover (Linux-style), pure logic.
//!
//! VMID 0 is never handed out. Every VM holds a [`VmidSlot`] (vmid, gen).
//! `get` is called, under the caller's lock, before each vCPU entry: a slot of
//! the current generation is returned as is; otherwise a free vmid is taken.
//! When the space is exhausted the generation is bumped, the in-use bitmap is
//! cleared except for VMIDs of vCPUs that are *running right now*
//! (`enter`/`exit` counts), and `flush = true` tells the caller to execute
//! `TLBI VMALLS12E1IS` (+ DSB ISH) before using any new vmid, still under the
//! lock. A running VM keeps its vmid across the rollover (it adopts it again
//! on its next `get`), so one VM never owns two vmids at once.
//!
//! `release` (VM destroy) must be preceded by the TLBI of that vmid.

pub const MAX_VMIDS: usize = 1 << 16;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmidSlot {
    pub vmid: u16,
    /// 0 = never allocated (the allocator's generation starts at 1).
    pub gen: u64,
}

pub struct VmidAlloc {
    bits: u32,
    gen: u64,
    next: usize,
    in_use: [u64; MAX_VMIDS / 64],
    /// Running vmids carried over a rollover, waiting for their owner.
    carried: [u64; MAX_VMIDS / 64],
    running: [u8; MAX_VMIDS],
}

fn test(b: &[u64], i: usize) -> bool { b[i / 64] >> (i % 64) & 1 != 0 }
fn set(b: &mut [u64], i: usize) { b[i / 64] |= 1 << (i % 64); }
fn clr(b: &mut [u64], i: usize) { b[i / 64] &= !(1 << (i % 64)); }

impl VmidAlloc {
    /// `bits` = 8 or 16 (ID_AA64MMFR1_EL1.VMIDBits).
    pub const fn new(bits: u32) -> Self {
        let mut a = VmidAlloc {
            bits,
            gen: 1,
            next: 1,
            in_use: [0; MAX_VMIDS / 64],
            carried: [0; MAX_VMIDS / 64],
            running: [0; MAX_VMIDS],
        };
        a.in_use[0] = 1; // vmid 0 reserved
        a
    }

    fn limit(&self) -> usize { 1usize << self.bits }
    pub fn generation(&self) -> u64 { self.gen }

    pub fn in_use_count(&self) -> usize {
        self.in_use.iter().map(|w| w.count_ones() as usize).sum()
    }

    fn find_free(&mut self) -> Option<usize> {
        let lim = self.limit();
        for k in 0..lim {
            let i = (self.next + k) % lim;
            if i != 0 && !test(&self.in_use, i) {
                self.next = i + 1;
                return Some(i);
            }
        }
        None
    }

    fn rollover(&mut self) {
        self.gen += 1;
        self.in_use = [0; MAX_VMIDS / 64];
        self.carried = [0; MAX_VMIDS / 64];
        set(&mut self.in_use, 0);
        for i in 1..self.limit() {
            if self.running[i] > 0 {
                set(&mut self.in_use, i);
                set(&mut self.carried, i);
            }
        }
        self.next = 1;
    }

    /// Returns (vmid, flush). On `flush` the caller must TLBI VMALLS12E1IS on
    /// all CPUs before the vmid is used. `None` only if 255/65535 VMs are live
    /// and running at once (cannot happen with <= 8 vCPUs per VM and few VMs).
    pub fn get(&mut self, slot: &mut VmidSlot) -> Option<(u16, bool)> {
        if slot.gen == self.gen { return Some((slot.vmid, false)); }
        // A running vmid carried across a rollover returns to its owner.
        if slot.gen != 0 && slot.gen + 1 == self.gen && slot.vmid != 0
            && test(&self.carried, slot.vmid as usize)
        {
            clr(&mut self.carried, slot.vmid as usize);
            slot.gen = self.gen;
            return Some((slot.vmid, false));
        }
        let mut flush = false;
        let id = match self.find_free() {
            Some(i) => i,
            None => {
                self.rollover();
                flush = true;
                self.find_free()?
            }
        };
        set(&mut self.in_use, id);
        slot.vmid = id as u16;
        slot.gen = self.gen;
        Some((slot.vmid, flush))
    }

    /// A VMID that stays with its VM for the VM's whole life (never rolled over, so
    /// the VM needs no per-entry `get`). `None` when every VMID is taken: the caller
    /// fails VM creation (ENOMEM). The generation never advances through this path.
    pub fn alloc_fixed(&mut self, slot: &mut VmidSlot) -> Option<u16> {
        let id = self.find_free()?;
        set(&mut self.in_use, id);
        slot.vmid = id as u16;
        slot.gen = self.gen;
        Some(slot.vmid)
    }

    /// A vCPU of the VM owning `vmid` is about to enter the guest.
    pub fn enter(&mut self, vmid: u16) {
        let r = &mut self.running[vmid as usize];
        *r = r.saturating_add(1);
    }

    /// ...and has left it again.
    pub fn exit(&mut self, vmid: u16) {
        let r = &mut self.running[vmid as usize];
        *r = r.saturating_sub(1);
    }

    /// VM destroyed (after its TLBI): the vmid may be reused in this generation.
    pub fn release(&mut self, slot: &mut VmidSlot) {
        if slot.gen != 0 && slot.vmid != 0 {
            if slot.gen == self.gen
                || (slot.gen + 1 == self.gen && test(&self.carried, slot.vmid as usize))
            {
                clr(&mut self.in_use, slot.vmid as usize);
                clr(&mut self.carried, slot.vmid as usize);
            }
        }
        *slot = VmidSlot::default();
    }
}
