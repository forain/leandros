//! Clocks, delays and timers.
//!
//! RM's busy-wait loops (the GSP RPC poll `_kgspRpcRecvPoll`, register polls
//! through `gpuCheckTimeout`) spin on `os_get_current_tick` with
//! `osSpinLoop()` being empty. Kernel threads run with IRQs masked, so such a
//! loop would starve its CPU of ticks for seconds (the `[WDOG]` watchdog fires
//! after 2 s). `relax()` opens an IRQ window / reschedule point at most once
//! per millisecond from those clock reads, when the caller may sleep, and is
//! where the bring-up watchdog can park a runaway RM thread.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, Ordering};

use sched::hrtimer::{HrTimer, TimerMode};

use crate::{nvlog, NV_OK};

extern "C" {
    fn arch_tsc_khz() -> u64;
}

static LAST_RELAX: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];

/// Let interrupts in and the scheduler run, at most once per ms per CPU, if
/// this context may sleep. Also the watchdog's abort point.
pub(crate) fn relax(site: &str) {
    if !sched::irqctx::can_sleep() {
        return;
    }
    let cpu = unsafe { sched::cpu_id() } & 63;
    let now = sched::monotonic_ns();
    if now.saturating_sub(LAST_RELAX[cpu].load(Ordering::Relaxed)) < 1_000_000 {
        return;
    }
    LAST_RELAX[cpu].store(now, Ordering::Relaxed);
    crate::device::abort_point(site);
    sched::kthread::cond_resched();
}

#[no_mangle]
pub unsafe extern "C" fn os_get_current_time(sec: *mut u32, usec: *mut u32) -> u32 {
    cb!("os_get_current_time");
    let ns = sched::realtime_ns();
    *sec = (ns / 1_000_000_000) as u32;
    *usec = ((ns % 1_000_000_000) / 1000) as u32;
    NV_OK
}

/// Linux returns jiffies in ns; ours is the TSC-backed monotonic clock.
#[no_mangle]
pub unsafe extern "C" fn os_get_current_tick() -> u64 {
    cb!("os_get_current_tick");
    relax("os_get_current_tick");
    sched::monotonic_ns()
}

#[no_mangle]
pub unsafe extern "C" fn os_get_current_tick_hr() -> u64 {
    cb!("os_get_current_tick_hr");
    relax("os_get_current_tick_hr");
    sched::monotonic_ns()
}

#[no_mangle]
pub unsafe extern "C" fn os_get_tick_resolution() -> u64 {
    cb!("os_get_tick_resolution");
    1000
}

#[no_mangle]
pub unsafe extern "C" fn os_get_cpu_frequency() -> u64 {
    cb!("os_get_cpu_frequency");
    let khz = arch_tsc_khz();
    if khz == 0 { 1_000_000_000 } else { khz * 1000 }
}

fn spin_us(us: u64) {
    let end = sched::monotonic_ns().saturating_add(us.saturating_mul(1000));
    while sched::monotonic_ns() < end {
        core::hint::spin_loop();
        relax("os_delay_us");
    }
}

/// Short delays spin (as Linux udelay); from 1 ms up, sleep if allowed.
#[no_mangle]
pub unsafe extern "C" fn os_delay_us(us: u32) -> u32 {
    cb!("os_delay_us");
    if us >= 1000 && sched::irqctx::can_sleep() {
        crate::device::abort_point("os_delay_us");
        sched::kthread::sleep_ns(us as u64 * 1000);
    } else {
        spin_us(us as u64);
    }
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_delay(ms: u32) -> u32 {
    cb!("os_delay");
    if sched::irqctx::can_sleep() {
        crate::device::abort_point("os_delay");
        sched::kthread::msleep(ms as u64);
    } else {
        spin_us(ms as u64 * 1000);
    }
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn os_schedule() -> u32 {
    cb!("os_schedule");
    crate::device::abort_point("os_schedule");
    sched::kthread::cond_resched();
    sched::kthread::yield_now();
    NV_OK
}

// --------------------------------------------------------- nano timers --
//
// Linux (nv-nano-timer.c): an hrtimer whose expiry runs
// rm_run_nano_timer_callback(nv, pTmrEvent) from a kthread. Here: a
// worker-mode HrTimer (task context on the system work queue).

struct NanoTimer {
    nv: *mut c_void,
    event: *mut c_void,
    timer: Option<alloc::boxed::Box<HrTimer>>,
}

extern "C" fn nano_fire(ctx: usize) {
    let t = unsafe { &*(ctx as *const NanoTimer) };
    let st = unsafe { crate::rm::rm_run_nano_timer_callback(core::ptr::null_mut(), t.nv as _, t.event) };
    if st != NV_OK {
        nvlog!("[nvrm] nano timer callback failed: {:#x}\n", st);
    }
}

#[no_mangle]
pub unsafe extern "C" fn nv_create_nano_timer(nv: *mut c_void, event: *mut c_void, out: *mut *mut c_void) {
    cb!("nv_create_nano_timer");
    let t = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(NanoTimer { nv, event, timer: None }));
    (*t).timer = Some(HrTimer::new(nano_fire, t as usize, TimerMode::Worker));
    *out = t as *mut c_void;
}

#[no_mangle]
pub unsafe extern "C" fn nv_start_nano_timer(_nv: *mut c_void, t: *mut c_void, time_ns: u64) {
    cb!("nv_start_nano_timer");
    if let Some(tm) = (*(t as *mut NanoTimer)).timer.as_ref() {
        if !tm.start(time_ns, 0) {
            nvlog!("[nvrm] nv_start_nano_timer: hrtimer table full\n");
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn nv_cancel_nano_timer(_nv: *mut c_void, t: *mut c_void) {
    cb!("nv_cancel_nano_timer");
    if let Some(tm) = (*(t as *mut NanoTimer)).timer.as_ref() {
        tm.cancel();
    }
}

#[no_mangle]
pub unsafe extern "C" fn nv_destroy_nano_timer(_nv: *mut c_void, t: *mut c_void) {
    cb!("nv_destroy_nano_timer");
    if !t.is_null() {
        // Drop = cancel_sync.
        drop(alloc::boxed::Box::from_raw(t as *mut NanoTimer));
    }
}

// --------------------------------------------------------------- RC timer --
// nvidia-haiku leaves the 1 Hz robust-channel watchdog off (returns 0); so do
// we for the bring-up: it only matters once channels run.

#[no_mangle]
pub unsafe extern "C" fn nv_start_rc_timer(_nv: *mut c_void) -> i32 {
    cb!("nv_start_rc_timer");
    0
}

#[no_mangle]
pub unsafe extern "C" fn nv_stop_rc_timer(_nv: *mut c_void) -> i32 {
    cb!("nv_stop_rc_timer");
    0
}
