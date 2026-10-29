//! kapi exports shared by several driver classes (kmodW2 W2-1). Add-only.
use super::*;

/// Physical address of a DMA-able kernel virtual address. `u64::MAX` for anything outside the
/// direct map: module statics and bss live in the module window and cannot be DMA targets.
#[no_mangle]
pub extern "C" fn kapi_virt_to_phys(va: usize) -> u64 {
    let base = mm::mm_get_hhdm_offset() as usize;
    if va < base || va - base >= (1usize << 44) { return u64::MAX; }
    mm::virt_to_phys(va) as u64
}

/// True when the caller may block.
#[no_mangle]
pub extern "C" fn kapi_can_sleep() -> bool { sched::irqctx::can_sleep() }

/// Sleep until the monotonic clock reaches `ns`.
#[no_mangle]
pub extern "C" fn kapi_sleep_until(ns: u64) { sched::kthread::sleep_until(ns) }

#[no_mangle]
pub extern "C" fn kapi_yield() { sched::kthread::yield_now() }

#[no_mangle]
pub extern "C" fn kapi_irqs_masked() -> bool { sched::irqs_masked() }

#[no_mangle]
pub extern "C" fn kapi_current_pid() -> u32 { sched::current_pid() as u32 }
