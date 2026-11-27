//! Layout contract between `switch.s` and Rust, plus the `enter()` entry point.
//!
//! `enter()` runs one guest slice: it saves the host's callee-saved registers
//! and the EL2 state a guest run changes, loads the guest (`VcpuHw`) and erets.
//! On the next exception from the guest the `kvm_guest_vectors` table saves the
//! guest back into `VcpuHw`, restores the host and returns from `enter()` with
//! the syndrome in a `RawExit`.
//!
//! Requirements on the caller: running at EL2 with VHE (`hyp::vhe()`), on
//! SP_EL2 (SPSel = 1, the normal kernel state), IRQs/FIQs masked is not
//! required (DAIF is masked inside and restored on return). TPIDR_EL2 is
//! used as the switch-frame pointer while the guest runs and restored after.
//! FP/SIMD state is not switched here: the run loop swaps it once per
//! KVM_RUN (`fpsimd_save_live`/load) and the guest runs with CPTR FPEN=11.
//! The Spectre-BHB exit loop (plan D-sec) is NOT in this lane.

use core::mem::offset_of;
pub use kvm_hw::hw::*;

extern "C" {
    fn kvm_enter_raw(hw: *mut VcpuHw, out: *mut RawExit);
}

/// Run the guest until the next exit.
///
/// # Safety
/// EL2/VHE host, SPSel=1, `hw.vttbr` must reference a valid stage-2 for the
/// VMID in its high bits, and `hw.hcr` must have TGE clear.
pub unsafe fn enter(hw: &mut VcpuHw) -> RawExit {
    let mut out = RawExit::default();
    kvm_enter_raw(hw, &mut out);
    out
}

// switch.s hard-codes these offsets (HW_* / EL1 base / RawExit fields).
const _: () = {
    assert!(offset_of!(VcpuHw, sp_el0) == 248);
    assert!(offset_of!(VcpuHw, sp_el1) == 256);
    assert!(offset_of!(VcpuHw, pc) == 264);
    assert!(offset_of!(VcpuHw, pstate) == 272);
        assert!(offset_of!(VcpuHw, el1) == 288);
    assert!(offset_of!(VcpuHw, hcr) == 1056);
    assert!(offset_of!(VcpuHw, vttbr) == 1064);
    assert!(offset_of!(VcpuHw, vmpidr) == 1072);
    assert!(offset_of!(VcpuHw, cntvoff) == 1080);
    // switched EL1 file: indices used by switch.s (G_SAVE/G_LOAD n)
    assert!(offset_of!(El1Regs, sctlr) == 0 && offset_of!(El1Regs, tpidr_el1) == 13 * 8);
    assert!(offset_of!(El1Regs, par) == 16 * 8 && offset_of!(El1Regs, mdscr) == 19 * 8);
    assert!(offset_of!(El1Regs, elr_el1) == 20 * 8 && offset_of!(El1Regs, spsr) == 21 * 8);
    assert!(offset_of!(RawExit, esr) == 8 && offset_of!(RawExit, far) == 16 && offset_of!(RawExit, hpfar) == 24);
};
