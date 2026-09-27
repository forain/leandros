//! Wake attribution for parked pollers: WHO made a poll-channel waiter Ready.
//!
//! `idlestat` counts dispatches per thread and wake calls per site, but not
//! the pairing that matters for an idle event loop: a poller woken by site X
//! that then re-probes, finds nothing ready and parks again is a wake the
//! kernel spent for nothing (a spurious wake), while one that returns events
//! to user space is real work. `unblock_port_tagged` and the deadline service
//! stamp each woken task with its waker here; the epoll/poll loops in
//! `kernel::syscall` read the stamp back after the park and classify the wake.
//! Compile-time gated like `idlestat`: off, every hook is a constant-false
//! branch.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub const ENABLED: bool = false;

/// The waker of the task in slot `pid & 1023`: a `&'static Location` address
/// for a tagged wake, or one of the small codes below.
static WOKE: [AtomicU64; 1024] = [const { AtomicU64::new(0) }; 1024];
/// The site of the unblock currently running under RUN_QUEUE (set by the
/// caller while it holds the lock; reset to `UNATTRIBUTED` after each use).
pub static SITE: AtomicU64 = AtomicU64::new(UNATTRIBUTED);

pub const DL_OWN: u64 = 1;
pub const DL_TFD: u64 = 2;
pub const UNATTRIBUTED: u64 = 3;

#[inline]
pub fn set_site(loc: &'static core::panic::Location<'static>) {
    if ENABLED { SITE.store(loc as *const _ as u64, Relaxed); }
}

#[inline]
pub fn mark(pid: u32, v: u64) {
    if ENABLED { WOKE[pid as usize & 1023].store(v, Relaxed); }
}

/// The waker recorded for `pid` since its last park, cleared (0 = none).
#[inline]
pub fn take(pid: u32) -> u64 {
    if !ENABLED { return 0; }
    WOKE[pid as usize & 1023].swap(0, Relaxed)
}
