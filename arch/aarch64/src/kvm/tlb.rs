//! Stage-2 TLB maintenance. Runs at EL2 with the kernel in VHE mode
//! (E2H=1, host TGE=1): the `*E1` TLBIs then address the EL1&0 regime only
//! while TGE is clear, so each helper loads the VM's VTTBR_EL2, clears
//! HCR_EL2.TGE, invalidates, and restores both, all with interrupts masked.
//! Not usable before kvm-vhe has put the kernel at EL2.

#[cfg(target_arch = "aarch64")]
use core::arch::asm;

const HCR_TGE: u64 = 1 << 27;

/// Run `body` (the TLBIs) in the guest translation regime of `vttbr`.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn in_guest_regime(vttbr: u64, body: impl FnOnce()) {
    let (daif, old_vttbr, old_hcr): (u64, u64, u64);
    asm!("mrs {}, daif", "msr daifset, #0xf", out(reg) daif, options(nomem, nostack));
    asm!("mrs {}, vttbr_el2", "mrs {}, hcr_el2", out(reg) old_vttbr, out(reg) old_hcr,
         options(nomem, nostack));
    // Make earlier stage-2 table writes visible to the walker, then switch.
    asm!("dsb ishst", "msr vttbr_el2, {v}", "msr hcr_el2, {h}", "isb",
         v = in(reg) vttbr, h = in(reg) old_hcr & !HCR_TGE, options(nostack));
    body();
    asm!("dsb ish", "isb", options(nostack));
    asm!("msr hcr_el2, {h}", "msr vttbr_el2, {v}", "isb",
         h = in(reg) old_hcr, v = in(reg) old_vttbr, options(nostack));
    asm!("msr daif, {}", in(reg) daif, options(nomem, nostack));
}

/// Invalidate every stage-1+2 TLB entry of the VMID in `vttbr`, all CPUs.
///
/// # Safety
/// EL2 only, kernel in VHE mode, `vttbr` a VTTBR_EL2 value (VMID in 63:48).
#[cfg(target_arch = "aarch64")]
pub unsafe fn tlbi_vmid(vttbr: u64) {
    in_guest_regime(vttbr, || {
        asm!("tlbi vmalls12e1is", "dsb ish", options(nostack));
    });
}

/// Invalidate the stage-2 (and combined) entries for one IPA page of the VM
/// in `vttbr`, all CPUs. Also drops intermediate walk caches.
///
/// # Safety
/// As [`tlbi_vmid`].
#[cfg(target_arch = "aarch64")]
pub unsafe fn tlbi_ipa(vttbr: u64, ipa: u64) {
    let arg = (ipa >> 12) & 0x0000_0FFF_FFFF_FFFF;
    in_guest_regime(vttbr, || {
        asm!("tlbi ipas2e1is, {a}", "dsb ish", "tlbi vmalle1is", "dsb ish",
             a = in(reg) arg, options(nostack));
    });
}

/// Invalidate a page-aligned IPA range page by page (a whole-VMID flush is
/// cheaper beyond `FLUSH_ALL_PAGES`).
///
/// # Safety
/// As [`tlbi_vmid`].
#[cfg(target_arch = "aarch64")]
pub unsafe fn tlbi_range(vttbr: u64, ipa: u64, len: u64) {
    const FLUSH_ALL_PAGES: u64 = 64;
    let pages = (len + 4095) / 4096;
    if pages > FLUSH_ALL_PAGES { return tlbi_vmid(vttbr); }
    // One guest-regime switch: IPAS2E1IS per page, then a single VMALLE1IS
    // (drops the combined stage-1+2 entries) once the stage-2 ones are gone.
    in_guest_regime(vttbr, || {
        for i in 0..pages {
            let arg = ((ipa + i * 4096) >> 12) & 0x0000_0FFF_FFFF_FFFF;
            asm!("tlbi ipas2e1is, {a}", a = in(reg) arg, options(nostack));
        }
        asm!("dsb ish", "tlbi vmalle1is", "dsb ish", options(nostack));
    });
}

/// Invalidate the whole stage-1+2 TLB of every VMID on all CPUs (VMID
/// rollover). Valid with TGE set: VMALLS12E1IS needs no VMID selection.
///
/// # Safety
/// As [`tlbi_vmid`].
#[cfg(target_arch = "aarch64")]
pub unsafe fn tlbi_all_vmids() {
    in_guest_regime(0, || {
        asm!("tlbi alle1is", "dsb ish", options(nostack));
    });
}
