//! LeandrOS patch: held-spinlock accounting hook.
//!
//! Every `SpinMutex` acquire (`lock`, successful `try_lock`) and guard drop
//! calls the hook installed with [`set_hook`] (`true` = acquired, `false` =
//! released). The kernel's scheduler keeps a per-CPU count from it, so
//! `might_sleep()` and the voluntary-switch path can see a plain
//! `spin::Mutex` held across a sleep, and preemption can be deferred while
//! one is held. Without a hook (host builds, early boot) it costs one
//! relaxed load. See LEANDROS-PATCH.md.

use core::sync::atomic::{AtomicUsize, Ordering};

static HOOK: AtomicUsize = AtomicUsize::new(0);

/// Install the accounting hook (once, at boot).
pub fn set_hook(hook: fn(bool)) {
    HOOK.store(hook as usize, Ordering::Release);
}

#[inline(always)]
fn call(acquired: bool) {
    let h = HOOK.load(Ordering::Relaxed);
    if h != 0 {
        let f: fn(bool) = unsafe { core::mem::transmute(h) };
        f(acquired);
    }
}

#[inline(always)]
pub(crate) fn acquired() { call(true) }

#[inline(always)]
pub(crate) fn released() { call(false) }

static PARK: AtomicUsize = AtomicUsize::new(0);

/// Install the kexec park hook (called from `RelaxStrategy::relax`).
pub fn set_park_hook(f: fn()) {
    PARK.store(f as usize, Ordering::Release);
}

/// One relaxed load; calls the park hook if set. Lets a CPU spinning on a
/// lock whose holder already parked reach a park point.
#[inline]
pub fn park_poll() {
    let h = PARK.load(Ordering::Relaxed);
    if h != 0 {
        let f: fn() = unsafe { core::mem::transmute(h) };
        f();
    }
}
