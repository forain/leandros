//! INTERIM sleeping-lock and time callbacks, enough for rm_init_rm to run in
//! the boot-time selftest. The kernel has no sleeping mutex / rwlock /
//! semaphore yet (see the lane notes), so these spin. That is correct but not
//! acceptable for a live GPU: RM holds its API lock across GSP RPCs that wait
//! milliseconds for firmware. Replace with real sleeping primitives in G2.
//!
//! Every object is one AtomicI64 from os_alloc_mem:
//!   semaphore/mutex: available count (mutex = semaphore(1))
//!   rwlock:          readers (>0), free (0), writer (-1)

use core::ffi::c_void;
use core::sync::atomic::{AtomicI64, Ordering};

use crate::basic::{os_alloc_mem, os_free_mem};
use crate::NV_OK;

const NV_ERR_TIMEOUT_RETRY: u32 = 0x0000_0066;

unsafe fn new_cell(v: i64) -> *mut AtomicI64 {
    let mut p: *mut c_void = core::ptr::null_mut();
    if os_alloc_mem(&mut p, 8) != NV_OK {
        return core::ptr::null_mut();
    }
    let c = p as *mut AtomicI64;
    c.write(AtomicI64::new(v));
    c
}

unsafe fn cell<'a>(p: *mut c_void) -> &'a AtomicI64 {
    &*(p as *const AtomicI64)
}

fn try_down(c: &AtomicI64) -> bool {
    c.fetch_update(Ordering::Acquire, Ordering::Relaxed, |v| if v > 0 { Some(v - 1) } else { None })
        .is_ok()
}

fn down(c: &AtomicI64) {
    while !try_down(c) {
        core::hint::spin_loop();
    }
}

// ----------------------------------------------------- mutex / semaphore --

#[no_mangle]
pub unsafe extern "C" fn os_alloc_mutex(m: *mut *mut c_void) -> u32 {
    let c = new_cell(1);
    *m = c as *mut c_void;
    if c.is_null() { 0x51 } else { NV_OK }
}

#[no_mangle]
pub unsafe extern "C" fn os_free_mutex(m: *mut c_void) {
    os_free_mem(m);
}

#[no_mangle]
pub unsafe extern "C" fn os_acquire_mutex(m: *mut c_void) -> u32 {
    down(cell(m));
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_cond_acquire_mutex(m: *mut c_void) -> u32 {
    if try_down(cell(m)) { NV_OK } else { NV_ERR_TIMEOUT_RETRY }
}

#[no_mangle]
pub unsafe extern "C" fn os_release_mutex(m: *mut c_void) {
    cell(m).fetch_add(1, Ordering::Release);
}

#[no_mangle]
pub unsafe extern "C" fn os_alloc_semaphore(initial: u32) -> *mut c_void {
    new_cell(initial as i64) as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn os_free_semaphore(s: *mut c_void) {
    os_free_mem(s);
}

#[no_mangle]
pub unsafe extern "C" fn os_acquire_semaphore(s: *mut c_void) -> u32 {
    down(cell(s));
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_cond_acquire_semaphore(s: *mut c_void) -> u32 {
    if try_down(cell(s)) { NV_OK } else { NV_ERR_TIMEOUT_RETRY }
}

#[no_mangle]
pub unsafe extern "C" fn os_release_semaphore(s: *mut c_void) -> u32 {
    cell(s).fetch_add(1, Ordering::Release);
    NV_OK
}

/// Linux: !irqs_disabled() && !in_interrupt(). Kernel code here always runs
/// with IRQs masked (syscall model), so this needs a real "in ISR" query.
#[no_mangle]
pub unsafe extern "C" fn os_semaphore_may_sleep() -> u8 {
    1
}

// ---------------------------------------------------------------- rwlock --

#[no_mangle]
pub unsafe extern "C" fn os_alloc_rwlock() -> *mut c_void {
    new_cell(0) as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn os_free_rwlock(l: *mut c_void) {
    os_free_mem(l);
}

fn try_read(c: &AtomicI64) -> bool {
    c.fetch_update(Ordering::Acquire, Ordering::Relaxed, |v| if v >= 0 { Some(v + 1) } else { None })
        .is_ok()
}

fn try_write(c: &AtomicI64) -> bool {
    c.compare_exchange(0, -1, Ordering::Acquire, Ordering::Relaxed).is_ok()
}

#[no_mangle]
pub unsafe extern "C" fn os_acquire_rwlock_read(l: *mut c_void) -> u32 {
    while !try_read(cell(l)) {
        core::hint::spin_loop();
    }
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_acquire_rwlock_write(l: *mut c_void) -> u32 {
    while !try_write(cell(l)) {
        core::hint::spin_loop();
    }
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_cond_acquire_rwlock_read(l: *mut c_void) -> u32 {
    if try_read(cell(l)) { NV_OK } else { NV_ERR_TIMEOUT_RETRY }
}

#[no_mangle]
pub unsafe extern "C" fn os_cond_acquire_rwlock_write(l: *mut c_void) -> u32 {
    if try_write(cell(l)) { NV_OK } else { NV_ERR_TIMEOUT_RETRY }
}

#[no_mangle]
pub unsafe extern "C" fn os_release_rwlock_read(l: *mut c_void) {
    cell(l).fetch_sub(1, Ordering::Release);
}

#[no_mangle]
pub unsafe extern "C" fn os_release_rwlock_write(l: *mut c_void) {
    cell(l).store(0, Ordering::Release);
}

// ------------------------------------------------------------------ time --

#[no_mangle]
pub unsafe extern "C" fn os_get_current_time(sec: *mut u32, usec: *mut u32) -> u32 {
    let ns = sched::realtime_ns();
    *sec = (ns / 1_000_000_000) as u32;
    *usec = ((ns % 1_000_000_000) / 1000) as u32;
    NV_OK
}

/// Linux returns jiffies in ns; ours is the TSC/CNTVCT-backed monotonic clock.
#[no_mangle]
pub unsafe extern "C" fn os_get_current_tick() -> u64 {
    sched::monotonic_ns()
}

#[no_mangle]
pub unsafe extern "C" fn os_get_current_tick_hr() -> u64 {
    sched::monotonic_ns()
}

#[no_mangle]
pub unsafe extern "C" fn os_get_tick_resolution() -> u64 {
    1000
}

/// Busy-wait. TODO(G2): sleep for multi-ms delays (os_delay) instead.
#[no_mangle]
pub unsafe extern "C" fn os_delay_us(us: u32) -> u32 {
    let end = sched::monotonic_ns() + us as u64 * 1000;
    while sched::monotonic_ns() < end {
        core::hint::spin_loop();
    }
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_delay(ms: u32) -> u32 {
    os_delay_us(ms.saturating_mul(1000))
}

// -------------------------------------------------------------- platform --

#[no_mangle]
pub unsafe extern "C" fn nv_is_chassis_notebook() -> u8 {
    0
}

#[no_mangle]
pub unsafe extern "C" fn nv_acpi_is_battery_present() -> u8 {
    0
}
