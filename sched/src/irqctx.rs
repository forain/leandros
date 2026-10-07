//! Execution-context queries: "am I in an interrupt handler?", "may I sleep?"
//!
//! # Contexts this kernel has
//!
//! | context | how you get there | IRQs | may sleep |
//! |---|---|---|---|
//! | **hard IRQ** | an interrupt handler (timer tick, IPIs, device vectors, GIC lines) | masked | **no** |
//! | **task, atomic** | a task holding a [`crate::ksync::KSpinLock`], a tracked lock (`RUN_QUEUE`, `FD_TABLES`, ...), or inside `preempt_disable` | masked | **no** |
//! | **task** | a syscall, a fault handler, a kernel thread | masked (syscalls/kthreads) | **yes** |
//! | **boot** | `kernel_main` / `init_task_main` before `sched::run` (no current task) | either | **no** (waits spin) |
//!
//! "Sleeping" means parking the current task in the scheduler
//! (`ksync::KMutex::lock` contended, `KSemaphore::down`, `KWaitQueue::wait_event`,
//! `kthread::msleep`, ...). Syscalls already run with IRQs masked and every
//! `spin::Mutex` is effectively an IRQ-off spinlock, so the rule that matters is
//! the one `lane/usbinteg` found the hard way: **a task must never park while it
//! holds a spinlock**, because the next task on that CPU that wants the lock
//! spins at IF=0 forever.
//!
//! What is checked, and how (release build, so: cheap, log once per call site):
//!
//! * IRQ nesting: [`irq_enter`]/[`irq_exit`] are called by the arch interrupt
//!   entry paths on both architectures (x86_64: the tick/IPI path in
//!   `idt::fault_common`, the dynamic device vectors, the TLB shootdown IPI;
//!   aarch64: `exception::handle_irq`). The counter is per CPU and is always
//!   dropped *before* the IRQ path may reschedule (`preempt_check`), so it can
//!   never leak into another task.
//! * [`crate::ksync::KSpinLock`] holders: a per-CPU atomic depth.
//! * The `lockwatch` tracked locks (`RUN_QUEUE`, `FD_TABLES`, `PIPE_RINGS`,
//!   `VIRTIO_GPU`, ...): their holder CPU is published already.
//! * `preempt_disable` sections.
//! * No current task (boot / scheduler context).
//!
//! Plain `spin::Mutex` guards are NOT visible to these checks (there is no hook
//! in the `spin` crate). Code that wants to sleep while other code holds its
//! locks must use `KSpinLock` (or a sleeping lock) for those locks.
//!
//! A blocking primitive called where sleeping is not allowed does not park:
//! it logs `[KSYNC] sleep in atomic context` once for that call site and
//! degrades to a bounded-cost spin (the behaviour of the interim spinning
//! implementations). That turns a would-be silent whole-CPU deadlock into one
//! diagnostic line plus, at worst, a spin that the `[WDOG]` watchdog names.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use crate::MAX_CPUS;

/// Hard-IRQ nesting depth per CPU.
static IRQ_NEST: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
/// `KSpinLock` (and C-shim spinlock) hold depth per CPU.
static ATOMIC_DEPTH: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
/// Hard IRQs taken per CPU since boot (diagnostic; cheap).
static IRQ_COUNT: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];

#[inline(always)]
fn cpu() -> usize { unsafe { crate::cpu_id() }.min(MAX_CPUS - 1) }

/// Enter hard-IRQ context on this CPU. Called by the arch IRQ entry paths
/// only, with IRQs masked, after the CPU-specific entry work (swapgs) and
/// before any handler body runs.
#[inline]
pub fn irq_enter() {
    let c = cpu();
    IRQ_NEST[c].fetch_add(1, Ordering::Relaxed);
    IRQ_COUNT[c].fetch_add(1, Ordering::Relaxed);
}

/// Leave hard-IRQ context. Must be called before the IRQ path reschedules
/// (`preempt_check`): a context switch with the counter raised would carry it
/// into whatever task runs next on this CPU.
#[inline]
pub fn irq_exit() {
    let c = cpu();
    let v = IRQ_NEST[c].load(Ordering::Relaxed);
    if v == 0 {
        once(&UNDERFLOW_WARNED, "[KSYNC] irq_exit without irq_enter (counter underflow ignored)\n");
        return;
    }
    IRQ_NEST[c].store(v - 1, Ordering::Relaxed);
}

/// True inside a hard interrupt handler on this CPU (Linux `in_irq()` /
/// `in_hardirq()`; RM's `os_is_isr`).
#[inline]
pub fn in_irq() -> bool { IRQ_NEST[cpu()].load(Ordering::Relaxed) != 0 }

/// True in any interrupt context. This kernel has no softirq level (deferred
/// work runs in worker *threads*, see `workqueue`), so this equals [`in_irq`].
#[inline]
pub fn in_interrupt() -> bool { in_irq() }

/// Hard-IRQ nesting depth of this CPU (0 = task/boot context).
#[inline]
pub fn irq_depth() -> u32 { IRQ_NEST[cpu()].load(Ordering::Relaxed) }

/// Interrupts this CPU has taken through `irq_enter` since boot.
pub fn irq_count(cpu: usize) -> u32 { IRQ_COUNT[cpu.min(MAX_CPUS - 1)].load(Ordering::Relaxed) }

/// `extern "C"` view of [`in_interrupt`] for crates below `sched` (the `mm`
/// allocator routes interrupt-context allocations to its atomic pool).
#[no_mangle]
pub extern "C" fn sched_in_interrupt() -> bool { in_interrupt() }

/// Raise this CPU's atomic depth (a spinlock was taken). Pair with
/// [`atomic_dec`]. Callers must have IRQs masked between the two, so the task
/// cannot migrate CPUs while the depth is raised.
#[inline]
pub fn atomic_inc() { ATOMIC_DEPTH[cpu()].fetch_add(1, Ordering::Relaxed); }

/// Undo one [`atomic_inc`].
#[inline]
pub fn atomic_dec() {
    let c = cpu();
    let v = ATOMIC_DEPTH[c].load(Ordering::Relaxed);
    if v != 0 { ATOMIC_DEPTH[c].store(v - 1, Ordering::Relaxed); }
}

/// Spinlock hold depth of this CPU.
#[inline]
pub fn atomic_depth() -> u32 { ATOMIC_DEPTH[cpu()].load(Ordering::Relaxed) }

/// True when there is a current task on this CPU (not boot / scheduler
/// context).
#[inline]
pub fn has_task() -> bool {
    let c = cpu();
    unsafe { !(*core::ptr::addr_of!(crate::CURRENT_CTX))[c].is_null() }
}

/// Why sleeping is not allowed right now, or `None` when it is.
pub fn sleep_forbidden_reason() -> Option<&'static str> {
    if in_interrupt() { return Some("in IRQ context"); }
    if !has_task() { return Some("no current task (boot/scheduler context)"); }
    if atomic_depth() != 0 { return Some("holding a KSpinLock"); }
    if crate::preempt_disabled() { return Some("preemption disabled"); }
    let me = cpu();
    for id in 1..crate::lockwatch::N_LOCKS as u8 {
        if crate::lockwatch::holder(id) == Some(me) {
            return Some(crate::lockwatch::name(id));
        }
    }
    None
}

/// True when the current context may park the task (Linux `!in_atomic()`
/// plus "there is a task"; RM's `os_semaphore_may_sleep`).
#[inline]
pub fn can_sleep() -> bool { sleep_forbidden_reason().is_none() }

/// True in any context that must not sleep (the negation of [`can_sleep`]).
#[inline]
pub fn in_atomic() -> bool { !can_sleep() }

// ── log-once machinery ───────────────────────────────────────────────────────

static UNDERFLOW_WARNED: AtomicUsize = AtomicUsize::new(0);

fn once(flag: &AtomicUsize, msg: &str) {
    if flag.swap(1, Ordering::Relaxed) == 0 { raw_log(msg); }
}

/// Lock-free UART write (safe from any context, including IRQ handlers and
/// sections that hold the console lock).
pub(crate) fn raw_log(msg: &str) {
    extern "C" { fn arch_serial_putc(c: u8); }
    for &b in msg.as_bytes() { unsafe { arch_serial_putc(b) } }
}

pub(crate) fn raw_dec(mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop { i -= 1; buf[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
    raw_log(unsafe { core::str::from_utf8_unchecked(&buf[i..]) });
}

/// Call sites (`Location` addresses) already reported by [`might_sleep`].
const SITES: usize = 64;
static REPORTED: [AtomicUsize; SITES] = [const { AtomicUsize::new(0) }; SITES];
/// Total detections (all sites), for tests.
static DETECTIONS: AtomicU32 = AtomicU32::new(0);

/// Number of sleep-in-atomic detections since boot.
pub fn sleep_violations() -> u32 { DETECTIONS.load(Ordering::Relaxed) }

/// Check that the caller may sleep. Returns true if it may. Otherwise logs
/// `[KSYNC] sleep in atomic context (<reason>) at <file>:<line>` once per
/// call site and returns false; the caller must then not park (the ksync
/// primitives fall back to spinning).
#[track_caller]
pub fn might_sleep() -> bool {
    match sleep_forbidden_reason() {
        None => true,
        Some(reason) => {
            // "no current task" is the boot path, where spinning is simply how
            // waiting works (the scheduler is not running yet); not a bug.
            if !has_task() && !in_interrupt() { return false; }
            DETECTIONS.fetch_add(1, Ordering::Relaxed);
            let loc = core::panic::Location::caller();
            let key = loc as *const _ as usize;
            for s in REPORTED.iter() {
                let v = s.load(Ordering::Relaxed);
                if v == key { return false; }
                if v == 0 {
                    if s.compare_exchange(0, key, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                        raw_log("[KSYNC] sleep in atomic context (");
                        raw_log(reason);
                        raw_log(") at ");
                        raw_log(loc.file());
                        raw_log(":");
                        raw_dec(loc.line() as u64);
                        raw_log("; spinning instead\n");
                        return false;
                    }
                    if s.load(Ordering::Relaxed) == key { return false; }
                }
            }
            false
        }
    }
}
