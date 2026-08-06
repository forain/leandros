//! AArch64 Memory Management Unit (MMU) initialization.

use boot::BootInfo;

/// MAIR_EL1 attribute byte for Normal memory, Inner and Outer Non-cacheable —
/// arm64's write-combining mapping, and what Linux calls MT_NORMAL_NC.
/// Installed at index 2, which `PageDescFlags::ATTR_NORMAL_NC` selects.
const MAIR_ATTR_NORMAL_NC: u64 = 0x44;

/// Which MAIR_EL1 attribute index `MAIR_ATTR_NORMAL_NC` is installed at.
const MAIR_IDX_NORMAL_NC: u32 = 2;

/// Minimal initialization: enable FPU/SIMD, ensure correct stack selection, and
/// install the one memory attribute we cannot inherit.
///
/// We otherwise keep the bootloader's MMU configuration (TCR, MAIR, TTBR1) for
/// stability.
pub unsafe fn enable_identity(_boot_info: &BootInfo) {
    // 1. Enable FP/SIMD access (CPACR_EL1.FPEN = 0b11)
    let mut cpacr: u64;
    core::arch::asm!("mrs {}, cpacr_el1", out(reg) cpacr);
    cpacr |= 3 << 20;
    core::arch::asm!("msr cpacr_el1, {}", "isb", in(reg) cpacr);

    // 2. Ensure we are using SP_EL1 for the kernel
    core::arch::asm!("msr SPSel, #1", "isb");

    // 3. Install Normal Non-cacheable at MAIR index 2.
    //
    // Every other MAIR attribute is inherited, and that is fine — but there is
    // no non-cached *Normal* attribute to inherit. Limine 11.4.1's BOOTAA64.EFI
    // writes MAIR_EL1 = 0xFF | (dev << 8), and our own direct-boot path
    // (kernel/src/entry_aarch64.s) writes 0x04FF; attributes 2..7 are zero in
    // both, and a zero attribute byte is Device-nGnRnE. Device memory cannot
    // back a buffer userspace memcpys through — it faults on the first
    // unaligned access — so a host-visible virtio-gpu blob the host asked to be
    // WC had nothing correct to be mapped with, and was mapped write-back
    // instead. See `mm::paging::PageFlags::WRITECOMBINE`.
    //
    // Read-modify-write, so attributes 0 and 1 stay exactly as the loader left
    // them: every mapping the loader made uses them, and this runs with its
    // page tables live. Index 2 is the safe one to claim — the flag that named
    // it (`PageDescFlags::ATTR_STRICT`) had no users anywhere in the tree, so
    // no live translation can be reinterpreted by this write. The TLB
    // invalidate is belt-and-braces for the same reason.
    //
    // Placed here rather than anywhere later because `arch::init` maps the
    // UART, the GIC and the framebuffer immediately after this returns, and
    // `smp::smp_init` snapshots MAIR_EL1 from the BSP for every AP — both must
    // see the finished value.
    let mut mair: u64;
    core::arch::asm!("mrs {}, mair_el1", out(reg) mair, options(nomem, nostack));
    MAIR_BEFORE = mair;
    let shift = MAIR_IDX_NORMAL_NC * 8;
    mair = (mair & !(0xFFu64 << shift)) | (MAIR_ATTR_NORMAL_NC << shift);
    core::arch::asm!(
        "msr mair_el1, {}",
        "dsb ish",
        "tlbi vmalle1is",
        "dsb ish",
        "isb",
        in(reg) mair,
        options(nostack)
    );
    let mut readback: u64;
    core::arch::asm!("mrs {}, mair_el1", out(reg) readback, options(nomem, nostack));
    MAIR_AFTER = readback;
}

/// MAIR_EL1 as the bootloader left it, and as this function leaves it.
///
/// Recorded rather than printed because the UART is not mapped yet when
/// `enable_identity` runs; `arch::init` prints both as soon as it is. The
/// "before" value is the evidence for the claim this whole flag rests on —
/// that attributes 2..7 are zero on the Limine path, so MAIR index 3 (the old
/// `ATTR_NOCACHE`) is Device-nGnRnE and not Normal-NC.
pub static mut MAIR_BEFORE: u64 = 0;
pub static mut MAIR_AFTER: u64 = 0;
