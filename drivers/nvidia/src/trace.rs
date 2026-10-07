//! Callback tracing for the hardware bring-up: every hand-written callback
//! starts with `cb!("name")`, which
//!   - logs the first call (`[nvrm] +<ms> cb+ <name> [<context>]`), so the
//!     persisted log shows exactly how far RM got and in what order it
//!     reached each OS facility;
//!   - counts calls (dumped by the watchdog and at the end of bring-up);
//!   - records the call in a 32-entry "last callbacks" ring the watchdog
//!     prints when a step overruns.
//! Lock-free and allocation-free: usable from the RM ISR.

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use crate::nvlog;

pub struct Cb {
    pub name: &'static str,
    pub calls: AtomicU64,
    seen: AtomicBool,
}

impl Cb {
    pub const fn new(name: &'static str) -> Self {
        Self { name, calls: AtomicU64::new(0), seen: AtomicBool::new(false) }
    }
}

const MAX_CBS: usize = 512;
static TABLE: [AtomicPtr<Cb>; MAX_CBS] = [const { AtomicPtr::new(core::ptr::null_mut()) }; MAX_CBS];
static NTABLE: AtomicUsize = AtomicUsize::new(0);

const RING: usize = 32;
static LAST: [AtomicPtr<Cb>; RING] = [const { AtomicPtr::new(core::ptr::null_mut()) }; RING];
static LAST_T: [AtomicU64; RING] = [const { AtomicU64::new(0) }; RING];
static LAST_I: AtomicUsize = AtomicUsize::new(0);

pub fn context() -> &'static str {
    if sched::irqctx::in_irq() {
        "irq"
    } else if let Some(r) = sched::irqctx::sleep_forbidden_reason() {
        r
    } else {
        "task"
    }
}

#[inline]
pub fn hit(cb: &'static Cb) {
    cb.calls.fetch_add(1, Ordering::Relaxed);
    let i = LAST_I.fetch_add(1, Ordering::Relaxed) % RING;
    LAST[i].store(cb as *const Cb as *mut Cb, Ordering::Relaxed);
    LAST_T[i].store(sched::monotonic_ns(), Ordering::Relaxed);
    if !cb.seen.load(Ordering::Relaxed) && !cb.seen.swap(true, Ordering::AcqRel) {
        let n = NTABLE.fetch_add(1, Ordering::AcqRel);
        if n < MAX_CBS {
            TABLE[n].store(cb as *const Cb as *mut Cb, Ordering::Release);
        }
        nvlog!("[nvrm] +{}ms cb+ {} [{} cpu{}]\n", crate::ms(), cb.name, context(), unsafe { sched::cpu_id() });
    }
}

macro_rules! cb {
    ($name:literal) => {{
        static CB: $crate::trace::Cb = $crate::trace::Cb::new($name);
        $crate::trace::hit(&CB);
    }};
}

/// Number of distinct callbacks reached so far.
pub fn distinct() -> usize {
    NTABLE.load(Ordering::Acquire).min(MAX_CBS)
}

/// Print every callback reached, with its call count.
pub fn dump_counts() {
    let n = distinct();
    nvlog!("[nvrm] callbacks reached: {} distinct\n", n);
    let mut line = alloc::string::String::new();
    for slot in TABLE.iter().take(n) {
        let p = slot.load(Ordering::Acquire);
        if p.is_null() {
            continue;
        }
        let cb = unsafe { &*p };
        let item = alloc::format!(" {}={}", cb.name, cb.calls.load(Ordering::Relaxed));
        if line.len() + item.len() > 110 {
            nvlog!("[nvrm]  {}\n", line);
            line.clear();
        }
        line.push_str(&item);
    }
    if !line.is_empty() {
        nvlog!("[nvrm]  {}\n", line);
    }
}

/// Print the last callbacks, oldest first, with their age.
pub fn dump_last() {
    let now = sched::monotonic_ns();
    let end = LAST_I.load(Ordering::Relaxed);
    nvlog!("[nvrm] last {} callbacks (oldest first, age in ms):\n", RING);
    let mut line = alloc::string::String::new();
    for k in 0..RING {
        let i = (end + k) % RING;
        let p = LAST[i].load(Ordering::Relaxed);
        if p.is_null() {
            continue;
        }
        let age = now.saturating_sub(LAST_T[i].load(Ordering::Relaxed)) / 1_000_000;
        let item = alloc::format!(" {}@{}", unsafe { (*p).name }, age);
        if line.len() + item.len() > 110 {
            nvlog!("[nvrm]  {}\n", line);
            line.clear();
        }
        line.push_str(&item);
    }
    if !line.is_empty() {
        nvlog!("[nvrm]  {}\n", line);
    }
}
