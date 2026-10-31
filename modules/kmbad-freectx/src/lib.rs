//! Fixture: a periodic hrtimer whose ctx is heap memory; exit frees the ctx WITHOUT cancelling the
//! timer. The kernel must have closed the lower gate and cancel the timer, so the callback never
//! touches freed memory.
#![no_std]
extern crate alloc;
use alloc::boxed::Box;
use core::sync::atomic::{AtomicUsize, Ordering};
use kmod_rt::kinfo;

kmod_rt::module! { name: "kmbad_freectx", init: init, exit: exit }

extern "C" {
    fn kapi_msleep(ms: u64);
    fn kapi_hrtimer_new(cb: Option<extern "C" fn(usize)>, ctx: usize, irq_mode: bool) -> usize;
    fn kapi_hrtimer_arm(h: usize, ns: u64);
}

static CTX: AtomicUsize = AtomicUsize::new(0);
static TIMER: AtomicUsize = AtomicUsize::new(0);

extern "C" fn tick(ctx: usize) {
    let p = ctx as *mut u64;
    unsafe { *p += 1; } // use-after-free if the kernel let this run after exit freed the box
    unsafe { kapi_hrtimer_arm(TIMER.load(Ordering::Relaxed), 5_000_000) };
}

fn init() -> i32 {
    let ctx = Box::into_raw(Box::new(0u64)) as usize;
    CTX.store(ctx, Ordering::SeqCst);
    let h = unsafe { kapi_hrtimer_new(Some(tick), ctx, false) };
    if h == 0 { return -12; }
    TIMER.store(h, Ordering::SeqCst);
    unsafe { kapi_hrtimer_arm(h, 5_000_000) };
    kinfo!("kmbad_freectx up");
    0
}

fn exit() {
    let ctx = CTX.swap(0, Ordering::SeqCst);
    drop(unsafe { Box::from_raw(ctx as *mut u64) });
    unsafe { kapi_msleep(50) };
    kinfo!("kmbad_freectx exit: ctx freed, timer left armed");
}
