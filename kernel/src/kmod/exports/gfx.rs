//! kapi exports: graphics support (K2b). Boot framebuffer description, module backlight providers
//! (per-owner, withdrawn again if init fails), page-reference helpers for GEM pinning, memory
//! totals, and IRQ save/restore for module spin locks that are also taken in an ISR.
use super::*;
use kmod_abi::{KBlGetFn, KBlSetFn, KBootFb};

static BOOT_FB: spin::Mutex<Option<KBootFb>> = spin::Mutex::new(None);

/// Boot-time hook: remember the framebuffer the bootloader handed over.
pub fn set_boot_fb(phys: u64, width: u32, height: u32, pitch: u32) {
    *BOOT_FB.lock() = Some(KBootFb { phys, width, height, pitch, _pad: 0 });
}

/// Fill `out` with the boot framebuffer. False when there is none (or `out` is null).
#[no_mangle]
pub extern "C" fn kapi_boot_fb(out: *mut KBootFb) -> bool {
    if out.is_null() { return false; }
    match *BOOT_FB.lock() { Some(f) => { unsafe { *out = f; } true } None => false }
}

/// Register a backlight provider for `/sys/class/backlight/intel_backlight`. 0 ok, -22 no callbacks.
/// Removed again if the module's init fails.
#[no_mangle]
pub extern "C" fn kapi_backlight_register(owner: KmodId, get: Option<KBlGetFn>, set: Option<KBlSetFn>, ctx: usize) -> i32 {
    if get.is_none() && set.is_none() { return -22; }
    drivers::power_sysfs::bl_register(drivers::power_sysfs::ModBl { owner: owner.0 as u32, get, set, ctx });
    crate::kmod::registry::note_registration(owner, RegKind::BACKLIGHT, ctx as u64);
    0
}

/// Failed init: drop every backlight provider of `owner`.
pub fn backlight_unregister_owner(owner: KmodId) { drivers::power_sysfs::bl_unregister_owner(owner.0 as u32) }

/// Take one extra reference on each frame (physical addresses of order-0 pages).
#[no_mangle]
pub extern "C" fn kapi_page_ref_many(frames: *const usize, n: usize) {
    if frames.is_null() || n == 0 { return; }
    mm::pageref::inc_many(unsafe { core::slice::from_raw_parts(frames, n) });
}

/// Release an order-`order` block that mmap pins may still reference. True when some page was still held.
#[no_mangle]
pub extern "C" fn kapi_page_free_pinned(phys: u64, order: u32) -> bool { mm::pageref::free_block_pinned(phys as usize, order as usize) }

/// `out[0]` = total pages, `out[1]` = free pages, `out[2]` = page size.
#[no_mangle]
pub extern "C" fn kapi_mem_info(out: *mut u64) {
    if out.is_null() { return; }
    unsafe {
        *out = mm::buddy::total_pages() as u64;
        *out.add(1) = mm::buddy::free_pages() as u64;
        *out.add(2) = mm::buddy::PAGE_SIZE as u64;
    }
}

/// Mask interrupts on this CPU; returns the previous state for `kapi_irq_restore`.
#[no_mangle]
pub extern "C" fn kapi_irq_save() -> u64 {
    #[cfg(target_arch = "x86_64")]
    { let f: u64; unsafe { core::arch::asm!("pushfq; pop {0}; cli", out(reg) f, options(nomem)) } f }
    #[cfg(target_arch = "aarch64")]
    { let f: u64; unsafe { core::arch::asm!("mrs {0}, daif", "msr daifset, #2", out(reg) f, options(nomem, nostack)) } f }
}

/// Undo `kapi_irq_save`: re-enable interrupts only if they were enabled before.
#[no_mangle]
pub extern "C" fn kapi_irq_restore(flags: u64) {
    #[cfg(target_arch = "x86_64")]
    if flags & 1 << 9 != 0 { unsafe { core::arch::asm!("sti", options(nomem, nostack)) } }
    #[cfg(target_arch = "aarch64")]
    if flags & 1 << 7 == 0 { unsafe { core::arch::asm!("msr daifclr, #2", options(nomem, nostack)) } }
}
