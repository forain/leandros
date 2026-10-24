//! Console and exception vectors for the window between the Limine hand-off
//! and `arch_aarch64::init` (L8.12).
//!
//! Until `init` maps the PL011 at its HHDM address, the UART is only reachable
//! at its physical address — and only if the bootloader's lower-half tables
//! happen to translate it. Under HVF they do; under TCG Limine's TTBR0 maps
//! nothing, so the first early print took a level-0 translation fault and,
//! with VBAR_EL1 still 0, spun forever at offset 0x200.
//!
//! `setup` runs first thing in `kernel_main`:
//!  * it installs the kernel's vector table in "early" mode (the EL1 sync
//!    handler prints ESR/ELR/FAR and halts instead of consulting the
//!    scheduler, which does not exist yet);
//!  * it makes the physical UART address usable: if an `AT S1E1W` probe
//!    already translates it nothing is touched; otherwise, when the TTBR0
//!    regime is a plain 4 KiB / 48-bit one whose L0[0] slot is empty, a
//!    static L1 table with one 1 GiB Device-nGnRnE block (PA 0..1 GiB) is
//!    hooked into L0[0]. If neither works, early UART output is dropped
//!    rather than faulting.
//!
//! The lower-half hook is removed again (`teardown`) the moment `init`
//! switches the UART to its HHDM mapping, so no process can ever inherit it.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

#[repr(C, align(4096))]
struct Table([u64; 512]);

static mut EARLY_L1: Table = Table([0; 512]);

/// HHDM virtual address of the TTBR0 L0 slot we wrote, or 0.
static HOOKED_SLOT: AtomicU64 = AtomicU64::new(0);

/// Probe a stage-1 EL1 write translation of `va`. True when it would succeed.
unsafe fn translates(va: usize) -> bool {
    let par: u64;
    core::arch::asm!(
        "at s1e1w, {va}",
        "isb",
        "mrs {par}, par_el1",
        va = in(reg) va,
        par = out(reg) par,
    );
    par & 1 == 0
}

/// Index (0..8) of a Device-nGnRnE (0x00) or Device-nGnRE (0x04) MAIR slot.
unsafe fn device_attr_index() -> Option<u64> {
    let mair: u64;
    core::arch::asm!("mrs {}, mair_el1", out(reg) mair);
    (0..8u64).find(|i| matches!((mair >> (i * 8)) & 0xff, 0x00 | 0x04))
}

/// Make the physical UART reachable and arm the early vectors. `hhdm` and the
/// kernel's physical/virtual load bases come from the Limine responses.
pub unsafe fn setup(hhdm: usize, kphys: usize, kvirt: usize) {
    crate::exception::init_early();

    let uart = crate::uart::BASE;
    if translates(uart) {
        crate::uart::set_early_mode(crate::uart::EARLY_IDENT);
        return;
    }
    if hook_identity(hhdm, kphys, kvirt) && translates(uart) {
        crate::uart::set_early_mode(crate::uart::EARLY_IDENT);
    } else {
        crate::uart::set_early_mode(crate::uart::EARLY_SILENT);
    }
}

unsafe fn hook_identity(hhdm: usize, kphys: usize, kvirt: usize) -> bool {
    if hhdm == 0 || kvirt == 0 || crate::uart::BASE >= 1 << 30 { return false; }
    let tcr: u64;
    let ttbr0: u64;
    core::arch::asm!("mrs {}, tcr_el1", out(reg) tcr);
    core::arch::asm!("mrs {}, ttbr0_el1", out(reg) ttbr0);
    let t0sz = tcr & 0x3f;
    let tg0 = (tcr >> 14) & 3;
    let epd0 = (tcr >> 7) & 1;
    // 4 KiB granule, 48-bit VA (walk starts at L0), walks enabled.
    if tg0 != 0 || t0sz != 16 || epd0 != 0 { return false; }
    let root = (ttbr0 & 0x0000_FFFF_FFFF_F000) as usize;
    if root == 0 { return false; }
    let attr = match device_attr_index() { Some(i) => i, None => return false };

    let slot = (root + hhdm) as *mut u64;
    if core::ptr::read_volatile(slot) & 1 != 0 { return false; }

    let l1_va = core::ptr::addr_of_mut!(EARLY_L1) as usize;
    let l1_pa = l1_va - kvirt + kphys;
    // Block: PA 0, AF, inner-shareable, EL1-only RW, PXN|UXN, AttrIndx.
    let block: u64 = 1 | (attr << 2) | (1 << 10) | (3 << 8) | (1 << 53) | (1 << 54);
    let l1 = l1_va as *mut u64;
    core::ptr::write_volatile(l1, block);
    core::ptr::write_volatile(slot, l1_pa as u64 | 3);
    core::arch::asm!("dsb ishst", "tlbi vmalle1", "dsb ish", "isb");
    HOOKED_SLOT.store(slot as u64, Relaxed);
    true
}

/// Remove the lower-half UART hook installed by `setup`, if any.
pub unsafe fn teardown() {
    let slot = HOOKED_SLOT.swap(0, Relaxed);
    if slot == 0 { return; }
    core::ptr::write_volatile(slot as *mut u64, 0);
    core::ptr::write_volatile(core::ptr::addr_of_mut!(EARLY_L1) as *mut u64, 0);
    core::arch::asm!("dsb ishst", "tlbi vmalle1", "dsb ish", "isb");
}
