//! Hardware half of the in-kernel vGIC (lane kvm-run): `VgicHw` over the GICv3
//! ICH_*_EL2 system registers, or over the GICv2 GICH MMIO frame.
//!
//! The pure state machine (`kvm::vgic`) calls `sync_in` before a guest entry
//! and `sync_out` after the exit; this module only reads and writes registers.
//! `save_v3`/`restore_v3` additionally carry ICH_VMCR/APxR, which the
//! `VgicHw` trait does not model, through `IchState`.
//!
//! All of it must run on the CPU that executes the vCPU, with IRQs masked.

use kvm_hw::hw::IchState;
use kvm_hw::vgic::lr::VgicHw;

macro_rules! rd {
    ($name:literal) => {{
        let v: u64;
        unsafe { core::arch::asm!(concat!("mrs {}, ", $name), out(reg) v, options(nomem, nostack)); }
        v
    }};
}
macro_rules! wr {
    ($name:literal, $v:expr) => {
        unsafe { core::arch::asm!(concat!("msr ", $name, ", {}"), in(reg) ($v) as u64, options(nomem, nostack)); }
    };
}

pub const ICH_HCR_EN: u64 = 1;

/// ICH_LR<n>_EL2 read, n in 0..16.
fn lr_rd(n: usize) -> u64 {
    match n {
        0 => rd!("S3_4_C12_C12_0"), 1 => rd!("S3_4_C12_C12_1"), 2 => rd!("S3_4_C12_C12_2"), 3 => rd!("S3_4_C12_C12_3"),
        4 => rd!("S3_4_C12_C12_4"), 5 => rd!("S3_4_C12_C12_5"), 6 => rd!("S3_4_C12_C12_6"), 7 => rd!("S3_4_C12_C12_7"),
        8 => rd!("S3_4_C12_C13_0"), 9 => rd!("S3_4_C12_C13_1"), 10 => rd!("S3_4_C12_C13_2"), 11 => rd!("S3_4_C12_C13_3"),
        12 => rd!("S3_4_C12_C13_4"), 13 => rd!("S3_4_C12_C13_5"), 14 => rd!("S3_4_C12_C13_6"), _ => rd!("S3_4_C12_C13_7"),
    }
}

fn lr_wr(n: usize, v: u64) {
    match n {
        0 => wr!("S3_4_C12_C12_0", v), 1 => wr!("S3_4_C12_C12_1", v), 2 => wr!("S3_4_C12_C12_2", v), 3 => wr!("S3_4_C12_C12_3", v),
        4 => wr!("S3_4_C12_C12_4", v), 5 => wr!("S3_4_C12_C12_5", v), 6 => wr!("S3_4_C12_C12_6", v), 7 => wr!("S3_4_C12_C12_7", v),
        8 => wr!("S3_4_C12_C13_0", v), 9 => wr!("S3_4_C12_C13_1", v), 10 => wr!("S3_4_C12_C13_2", v), 11 => wr!("S3_4_C12_C13_3", v),
        12 => wr!("S3_4_C12_C13_4", v), 13 => wr!("S3_4_C12_C13_5", v), 14 => wr!("S3_4_C12_C13_6", v), _ => wr!("S3_4_C12_C13_7", v),
    }
}

/// Number of list registers (ICH_VTR_EL2.ListRegs + 1).
pub fn v3_nr_lr() -> usize { ((rd!("S3_4_C12_C11_1") & 0x1f) as usize) + 1 }
/// Active-priority registers per group, from ICH_VTR_EL2.PRIbits (as Linux: 7 -> 4, 6 -> 2, else 1).
fn v3_nr_ap() -> usize { match (rd!("S3_4_C12_C11_1") >> 29) & 7 { 7 => 4, 6 => 2, _ => 1 } }

/// ICC_SRE_EL2 = SRE | DFB | DIB | Enable: lets the guest's EL1 use the ICC_* system
/// registers without trapping, and keeps the host's own SRE. Call once per CPU
/// before the first guest entry.
pub fn enable_v3_guest_access() { wr!("S3_4_C12_C9_5", 0xfu64); unsafe { core::arch::asm!("isb", options(nomem, nostack)); } }

/// GICv3 `VgicHw`: the ICH_* registers of the current CPU.
pub struct V3Hw { nr_lr: usize }
impl V3Hw {
    pub fn new() -> Self { V3Hw { nr_lr: v3_nr_lr().min(16) } }
}
impl VgicHw for V3Hw {
    fn nr_lr(&self) -> usize { self.nr_lr }
    fn lr(&self, i: usize) -> u64 { lr_rd(i) }
    fn set_lr(&mut self, i: usize, v: u64) { lr_wr(i, v) }
    fn elrsr(&self) -> u64 { rd!("S3_4_C12_C11_5") }
    fn misr(&self) -> u64 { rd!("S3_4_C12_C11_2") }
    fn hcr(&self) -> u64 { rd!("S3_4_C12_C11_0") }
    fn set_hcr(&mut self, v: u64) { wr!("S3_4_C12_C11_0", v); }
}

/// Load the per-vCPU ICH_VMCR/AP0R/AP1R (LRs and HCR go through `sync_in`).
pub fn restore_v3(st: &IchState) {
    wr!("S3_4_C12_C11_7", st.vmcr);
    let n = v3_nr_ap();
    if n > 0 { wr!("S3_4_C12_C8_0", st.ap0r[0]); wr!("S3_4_C12_C9_0", st.ap1r[0]); }
    if n > 1 { wr!("S3_4_C12_C8_1", st.ap0r[1]); wr!("S3_4_C12_C9_1", st.ap1r[1]); }
    if n > 2 {
        wr!("S3_4_C12_C8_2", st.ap0r[2]); wr!("S3_4_C12_C9_2", st.ap1r[2]);
        wr!("S3_4_C12_C8_3", st.ap0r[3]); wr!("S3_4_C12_C9_3", st.ap1r[3]);
    }
}

/// Save ICH_VMCR/AP0R/AP1R and the latched ELRSR/HCR into `st`.
pub fn save_v3(st: &mut IchState) {
    st.vmcr = rd!("S3_4_C12_C11_7");
    st.elrsr = rd!("S3_4_C12_C11_5");
    let n = v3_nr_ap();
    if n > 0 { st.ap0r[0] = rd!("S3_4_C12_C8_0"); st.ap1r[0] = rd!("S3_4_C12_C9_0"); }
    if n > 1 { st.ap0r[1] = rd!("S3_4_C12_C8_1"); st.ap1r[1] = rd!("S3_4_C12_C9_1"); }
    if n > 2 {
        st.ap0r[2] = rd!("S3_4_C12_C8_2"); st.ap1r[2] = rd!("S3_4_C12_C9_2");
        st.ap0r[3] = rd!("S3_4_C12_C8_3"); st.ap1r[3] = rd!("S3_4_C12_C9_3");
    }
}

// ---- GICv2: GICH MMIO frame (virtual interface control) -------------------

const GICH_HCR: usize = 0x0;
const GICH_VTR: usize = 0x4;
const GICH_VMCR: usize = 0x8;
const GICH_MISR: usize = 0x10;
const GICH_ELRSR0: usize = 0x30;
const GICH_APR: usize = 0xf0;
const GICH_LR0: usize = 0x100;

/// GICv2 `VgicHw` over the GICH frame mapped at `base` (a kernel virtual address).
pub struct V2Hw { base: usize, nr_lr: usize }
impl V2Hw {
    /// # Safety
    /// `base` must be a mapped Device-memory GICH frame of this CPU's interface.
    pub unsafe fn new(base: usize) -> Self {
        let vtr = ((base + GICH_VTR) as *const u32).read_volatile();
        V2Hw { base, nr_lr: ((vtr & 0x3f) as usize + 1).min(64) }
    }
    fn r(&self, off: usize) -> u32 { unsafe { ((self.base + off) as *const u32).read_volatile() } }
    fn w(&self, off: usize, v: u32) { unsafe { ((self.base + off) as *mut u32).write_volatile(v) } }
    pub fn restore(&self, st: &IchState) { self.w(GICH_VMCR, st.vmcr as u32); self.w(GICH_APR, st.ap0r[0] as u32); }
    pub fn save(&self, st: &mut IchState) { st.vmcr = self.r(GICH_VMCR) as u64; st.ap0r[0] = self.r(GICH_APR) as u64; }
}
impl VgicHw for V2Hw {
    fn nr_lr(&self) -> usize { self.nr_lr }
    fn lr(&self, i: usize) -> u64 { self.r(GICH_LR0 + 4 * i) as u64 }
    fn set_lr(&mut self, i: usize, v: u64) { self.w(GICH_LR0 + 4 * i, v as u32) }
    fn elrsr(&self) -> u64 { self.r(GICH_ELRSR0) as u64 | (self.r(GICH_ELRSR0 + 4) as u64) << 32 }
    fn misr(&self) -> u64 { self.r(GICH_MISR) as u64 }
    fn hcr(&self) -> u64 { self.r(GICH_HCR) as u64 }
    fn set_hcr(&mut self, v: u64) { self.w(GICH_HCR, v as u32) }
}

/// Kernel virtual address of this platform's GICH frame (GICv2 hosts), 0 = unknown.
/// TODO(kvm-run): kvm-vhe's `gic::vgic_info()`/PlatformGic (GICH/GICV/maint from the
/// ACPI MADT or the DT, plan section 3 item 6) never landed; until the platform
/// code calls `set_gich_va`, the v2 vGIC device is not offered (`supports` -> false).
static GICH_VA: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
pub fn set_gich_va(va: usize) { GICH_VA.store(va, core::sync::atomic::Ordering::Release); }
pub fn gich_va() -> usize { GICH_VA.load(core::sync::atomic::Ordering::Acquire) }
