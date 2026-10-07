//! Kernel threads that take an argument, plus sleeping helpers.
//!
//! A kernel thread is an ordinary scheduler task with no address space. It
//! starts with IRQs masked, exactly like a syscall: every kernel lock assumes
//! that (spin::Mutex is an IRQ-off lock in practice), and the thread is
//! preempted only where it parks (`ksync`, `msleep`) or calls
//! [`cond_resched`]. A thread that computes for long stretches must call
//! `cond_resched()` now and then, or its CPU takes no tick and the `[WDOG]`
//! watchdog reports it after 2 s.
//!
//! When the closure returns the thread exits (`sched::exit(0)`) and its slot,
//! kernel stack and `/proc`-style name are released by the scheduler.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use crate::ksync::KCompletion;
use crate::task::Pid;

struct Start {
    f: Box<dyn FnOnce() + Send + 'static>,
    done: Arc<KCompletion>,
    stop: Arc<AtomicBool>,
}

/// Stop flags of live kthreads (pid -> flag), for [`should_stop`].
static STOP_FLAGS: spin::Mutex<Vec<(Pid, Arc<AtomicBool>)>> = spin::Mutex::new(Vec::new());

/// Handle to a spawned kernel thread. Dropping it detaches the thread.
pub struct KThread {
    pid: Pid,
    done: Arc<KCompletion>,
    stop: Arc<AtomicBool>,
}

impl KThread {
    pub fn pid(&self) -> Pid { self.pid }
    /// Ask the thread to stop: [`should_stop`] returns true in it from now on.
    pub fn request_stop(&self) { self.stop.store(true, Ordering::Release); }
    /// True once the thread's function has returned.
    pub fn finished(&self) -> bool { self.done.done() }
    /// Sleep until the thread's function has returned.
    #[track_caller]
    pub fn join(&self) { self.done.wait() }
    /// As [`Self::join`] with a timeout; false if it is still running.
    #[track_caller]
    pub fn join_timeout(&self, timeout_ns: u64) -> bool { self.done.wait_timeout(timeout_ns) }
    /// `request_stop` + `join`.
    #[track_caller]
    pub fn stop(&self) { self.request_stop(); self.join(); }
}

extern "C" fn kthread_main(arg: usize) -> ! {
    // SAFETY: `arg` is the `Box<Start>` leaked by `spawn`, handed over once.
    let st: Box<Start> = unsafe { Box::from_raw(arg as *mut Start) };
    let Start { f, done, stop } = *st;
    let _ = stop;
    f();
    let me = crate::current_pid();
    STOP_FLAGS.lock().retain(|(p, _)| *p != me);
    done.complete_all();
    drop(done);
    crate::exit(0)
}

/// Spawn a kernel thread running `f`. `name` is what task dumps show (as the
/// thread's exe path, e.g. `[kworker]`). Task context or boot context.
/// Returns None when the task table is full or memory is exhausted.
pub fn spawn<F: FnOnce() + Send + 'static>(name: &str, f: F) -> Option<KThread> {
    let done = Arc::new(KCompletion::new());
    let stop = Arc::new(AtomicBool::new(false));
    let st = Box::new(Start { f: Box::new(f), done: done.clone(), stop: stop.clone() });
    let raw = Box::into_raw(st);
    // Registered before the thread can run, under the same lock it reads.
    let mut flags = STOP_FLAGS.lock();
    let pid = match crate::spawn_with_arg(kthread_main, raw as usize) {
        Some(p) => p,
        None => {
            drop(flags);
            unsafe { drop(Box::from_raw(raw)); }
            return None;
        }
    };
    flags.push((pid, stop.clone()));
    drop(flags);
    let mut nm = [0u8; 32];
    let n = name.len().min(nm.len() - 2);
    nm[0] = b'[';
    nm[1..1 + n].copy_from_slice(&name.as_bytes()[..n]);
    nm[1 + n] = b']';
    crate::set_exe_path(pid, &nm[..n + 2]);
    Some(KThread { pid, done, stop })
}

/// C-friendly spawn: run `func(arg)` in a new kernel thread (detached).
/// Returns the pid, 0 on failure.
#[no_mangle]
pub extern "C" fn kfacil_kthread_spawn(func: extern "C" fn(usize), arg: usize) -> u32 {
    match spawn("kthread", move || func(arg)) { Some(t) => t.pid(), None => 0 }
}

/// True in a kthread whose handle called `request_stop`.
pub fn should_stop() -> bool {
    let me = crate::current_pid();
    STOP_FLAGS.lock().iter().find(|(p, _)| *p == me).map_or(false, |(_, f)| f.load(Ordering::Acquire))
}

/// Sleep until the absolute `monotonic_ns()` deadline. Task context; in
/// atomic/IRQ/boot context it busy-waits (and the atomic case is logged).
#[track_caller]
pub fn sleep_until(deadline: u64) {
    if !crate::irqctx::might_sleep() {
        while crate::monotonic_ns() < deadline {
            if crate::irqctx::in_interrupt() || crate::irqctx::atomic_depth() != 0 || !crate::irqs_masked() {
                core::hint::spin_loop();
            } else {
                crate::preempt_disable();
                crate::irq_window();
                crate::preempt_enable_no_resched();
            }
        }
        return;
    }
    let f = crate::ksync::irq_save();
    while crate::monotonic_ns() < deadline {
        crate::kwait_prepare(deadline);
        if crate::monotonic_ns() >= deadline { crate::kwait_cancel(); break; }
        crate::kwait_commit();
    }
    crate::ksync::irq_restore(f);
}

/// Sleep for `ns` nanoseconds (at least; wakes on the next one-shot timer
/// interrupt after the deadline).
#[track_caller]
pub fn sleep_ns(ns: u64) { sleep_until(crate::monotonic_ns().saturating_add(ns)) }

/// Sleep for `ms` milliseconds.
#[track_caller]
pub fn msleep(ms: u64) { sleep_ns(ms.saturating_mul(1_000_000)) }

/// Busy-wait `us` microseconds without parking (any context).
pub fn udelay(us: u64) {
    let end = crate::monotonic_ns().saturating_add(us.saturating_mul(1000));
    while crate::monotonic_ns() < end { core::hint::spin_loop(); }
}

/// Let pending interrupts in and give up the CPU if the tick asked for it.
/// For long-running loops in kernel threads. A no-op in atomic context.
pub fn cond_resched() {
    if crate::irqctx::can_sleep() && crate::irqs_masked() {
        crate::irq_window();
        crate::preempt_check();
    }
}

/// Yield the CPU to any other runnable task (task context).
pub fn yield_now() {
    if crate::irqctx::can_sleep() { crate::yield_now("kthread::yield_now"); }
}
