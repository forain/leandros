//! SMP scheduler — context switching, task lifecycle, IPC blocking.
//!
//! Design: a single shared run queue (EEVDF policy, see runqueue.rs) served
//! by every online CPU.  Each CPU runs `scheduler_run_loop()` on its own
//! static scheduler context; tasks run until they call `yield_now()`,
//! `block_on_port_commit()`, `exit()`, or are preempted by their CPU's
//! local timer.
//!
//! SMP invariants:
//!  * `Task::on_cpu` guards against double dispatch: a task remains claimed
//!    from the moment it is picked until the owning CPU has switched back to
//!    its scheduler context (its saved registers are complete).
//!  * Preemption flags are per CPU; cross-CPU wake-ups go through
//!    `trigger_preempt` / `wake_up_an_idle_cpu`, which send architecture
//!    reschedule IPIs (x86-64 vector 0x40, AArch64 SGI 1).
//!  * APs park in `ap_entry()` until the BSP finishes kernel init and calls
//!    `run()`, preserving the pre-SMP boot ordering.
//!
//! Analogues: Linux kernel/sched/core.c (`schedule`, `switch_to`).

#![no_std]

extern crate alloc;

pub mod clone;
pub mod context;
pub mod futex;
pub mod runqueue;
pub mod signal;
pub mod task;

pub use clone::{fork_current, clone_thread};
pub use signal::{check_and_deliver_signals, restore_signal_frame, sys_sigaction, sys_sigprocmask, sys_sigaltstack, has_deliverable_signal};
pub use futex::{futex_wait, futex_wake, futex_requeue};

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use spin::Mutex;
use task::{Pid, Task, TaskState};
use context::CpuContext;
use runqueue::RunQueue;

static RUN_QUEUE:       Mutex<RunQueue> = Mutex::new(RunQueue::new());
static NEXT_PID:        Mutex<Pid>      = Mutex::new(1);
static TIMER_TICKS:     AtomicU64       = AtomicU64::new(0);

// ── System IPC port cache (set by kernel init, read by proc/self/auxv) ───────
static SYS_VFS_PORT:   AtomicU32 = AtomicU32::new(u32::MAX);
static SYS_NET_PORT:   AtomicU32 = AtomicU32::new(u32::MAX);
static SYS_AUDIO_PORT: AtomicU32 = AtomicU32::new(u32::MAX);

pub fn set_vfs_port(p: u32)   { SYS_VFS_PORT.store(p, Ordering::Relaxed); }
pub fn set_net_port(p: u32)   { SYS_NET_PORT.store(p, Ordering::Relaxed); }
pub fn set_audio_port(p: u32) { SYS_AUDIO_PORT.store(p, Ordering::Relaxed); }
pub fn get_vfs_port()   -> u32 { SYS_VFS_PORT.load(Ordering::Relaxed) }
pub fn get_net_port()   -> u32 { SYS_NET_PORT.load(Ordering::Relaxed) }
pub fn get_audio_port() -> u32 { SYS_AUDIO_PORT.load(Ordering::Relaxed) }
/// Per-CPU preemption flags: set by the CPU's own timer tick or by a remote
/// CPU via `trigger_preempt`; cleared and acted on by `preempt_check` on the
/// owning CPU.
static PREEMPT_NEEDED: [AtomicBool; MAX_CPUS] =
    [const { AtomicBool::new(false) }; MAX_CPUS];

/// Opened by `run()` on the BSP once kernel init is complete.  APs spin on
/// this in `ap_entry()` so no task can run on a secondary CPU while the BSP
/// is still bringing up servers and drivers.
static SCHED_ONLINE: AtomicBool = AtomicBool::new(false);
/// Optional hook called with a PID just before its task slot is reclaimed.
/// Registered by the IPC layer to release ports owned by the exiting task.
static TASK_EXIT_HOOK:  AtomicPtr<()>   = AtomicPtr::new(core::ptr::null_mut());
/// Optional hook called with a PID at the very top of [`exit`], while the
/// dying task is still current, still runnable, and still owns its address
/// space. Registered by the kernel to run the same fd/pipe/socket teardown
/// the `EXIT` syscall performs — see `register_exit_teardown_hook`.
static EXIT_TEARDOWN_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

// ── Exit-code log ────────────────────────────────────────────────────────────

const EXIT_LOG_LEN: usize = 256;

#[derive(Clone, Copy)]
struct ExitRecord {
    pid:  Pid,
    code: i32,
    /// tgid of the parent process, resolved at exit time (the forking thread
    /// may itself die before the child is waited on).
    parent_tgid: Pid,
    pgid: Pid,
    /// Thread-group leaders only — plain threads are never waitable.
    is_process: bool,
    /// Set once a wait4/waitid caller has been handed this record (or the
    /// zombie was already reported straight off the run queue), so the same
    /// child is never reported twice.
    consumed: bool,
}
static EXIT_LOG: Mutex<[Option<ExitRecord>; EXIT_LOG_LEN]> = Mutex::new([const { None }; EXIT_LOG_LEN]);
static EXIT_LOG_IDX: Mutex<usize> = Mutex::new(0);

fn log_exit(pid: Pid, code: i32, parent_tgid: Pid, pgid: Pid, is_process: bool, consumed: bool) {
    let mut log = EXIT_LOG.lock();
    let mut idx = EXIT_LOG_IDX.lock();
    log[*idx] = Some(ExitRecord { pid, code, parent_tgid, pgid, is_process, consumed });
    *idx = (*idx + 1) % EXIT_LOG_LEN;
}

pub fn get_exit_code(pid: Pid) -> Option<i32> {
    let log = EXIT_LOG.lock();
    for entry in log.iter().filter_map(|e| e.as_ref()) {
        if entry.pid == pid { return Some(entry.code); }
    }
    None
}

// ── Context switching ─────────────────────────────────────────────────────────

pub const MAX_CPUS: usize = 8;
static mut SCHEDULER_CTX: [CpuContext; MAX_CPUS] = [const { CpuContext::zeroed() }; MAX_CPUS];
static mut CURRENT_CTX:   [*mut CpuContext; MAX_CPUS] = [core::ptr::null_mut(); MAX_CPUS];
/// PID running on each CPU (0 = idle / in scheduler).  Atomic because
/// `wake_up_an_idle_cpu` reads other CPUs' slots to find an idle target.
static CURRENT_PID: [AtomicU32; MAX_CPUS] =
    [const { AtomicU32::new(0) }; MAX_CPUS];

extern "C" {
    fn arch_set_page_table(root: usize);
    /// Detach this CPU from any task page table (x86-64: reload the boot
    /// kernel CR3; AArch64: clear TTBR0).  Called after every switch-back so
    /// an exited task's freed page tables are never left live on a CPU.
    fn arch_load_kernel_page_table();
    fn arch_set_kernel_stack(rsp: u64);
    fn arch_cpu_id() -> usize;
    pub fn arch_alloc_page_table_root() -> usize;
    /// Send a reschedule IPI to `cpu` (x86-64: LAPIC vector 0x40; AArch64: SGI 1).
    fn arch_send_resched_ipi(cpu: usize);
    /// Number of CPUs that have entered the scheduler (BSP + booted APs).
    fn arch_active_cpu_count() -> usize;
    /// Physical core a logical CPU belongs to (SMT topology; identity when
    /// the platform exposes no SMT).
    fn arch_core_of(cpu: usize) -> usize;
}

pub unsafe fn cpu_id() -> usize {
    arch_cpu_id()
}

/// Mark `cpu` as needing a reschedule and, if it is a remote CPU, kick it
/// with a reschedule IPI so it acts on the flag promptly (an idle CPU is
/// sitting in `hlt`/`wfi`; a busy one preempts at the IPI return path).
pub fn trigger_preempt(cpu: usize) {
    if cpu >= MAX_CPUS { return; }
    PREEMPT_NEEDED[cpu].store(true, Ordering::Release);
    if cpu != unsafe { cpu_id() } {
        unsafe { arch_send_resched_ipi(cpu); }
    }
}

/// Find an idle CPU and send it a reschedule IPI.  Called whenever new work
/// becomes runnable (spawn, fork/clone, port unblock, futex wake, signal).
///
/// SMT-aware: prefers an idle CPU whose *whole core* is idle, so new work
/// lands on an unused physical core before doubling up on the hyperthread
/// sibling of a busy one — SMT siblings share execution resources, so two
/// tasks on one core run slower than two tasks on two cores.
pub fn wake_up_an_idle_cpu() {
    let n = unsafe { arch_active_cpu_count() }.min(MAX_CPUS);
    if n <= 1 { return; }
    let me = unsafe { cpu_id() };

    let mut fallback: Option<usize> = None;
    for i in 0..n {
        if i == me { continue; }
        if CURRENT_PID[i].load(Ordering::Relaxed) != 0 { continue; }

        // Idle candidate — check whether its SMT siblings are idle too.
        let core = unsafe { arch_core_of(i) };
        let mut core_idle = true;
        for j in 0..n {
            if j == i { continue; }
            if unsafe { arch_core_of(j) } != core { continue; }
            if j == me || CURRENT_PID[j].load(Ordering::Relaxed) != 0 {
                core_idle = false;
                break;
            }
        }
        if core_idle {
            trigger_preempt(i);
            return;
        }
        if fallback.is_none() { fallback = Some(i); }
    }
    if let Some(i) = fallback {
        trigger_preempt(i);
    }
}

// ── Stop-the-world fork (CoW quiesce) ────────────────────────────────────────
//
// fork() from a multithreaded process must not let sibling threads run while
// clone_as downgrades live PTEs: a sibling holding a stale writable TLB
// entry writes straight into a frame the child now shares — no fault, no
// copy, silent cross-process corruption (observed as std's Process struct
// getting zeroed under brush/tokio). Dispatch of the forking thread's
// siblings is suspended (pick_next filter) and every mid-flight sibling is
// IPI'd off its CPU before the address-space clone begins; the final
// broadcast TLB shootdown then closes the stale-entry window before any
// sibling can run again.
static QUIESCE_TGID:   AtomicU32 = AtomicU32::new(0);
static QUIESCE_EXCEPT: AtomicU32 = AtomicU32::new(0);

/// pick_next filter: true when `pid`/`tgid` must not be dispatched right now.
pub(crate) fn quiesce_filtered(tgid: Pid, pid: Pid) -> bool {
    let q = QUIESCE_TGID.load(Ordering::Acquire);
    q != 0 && tgid == q && pid != QUIESCE_EXCEPT.load(Ordering::Acquire)
}

/// Suspend dispatch of every other thread of `tgid` and wait until none is
/// mid-flight on any CPU. Returns false (no-op) for single-threaded
/// processes. Serializes concurrent forks via CAS on the quiesce slot.
/// Call `unquiesce_thread_group` when done (only if this returned true).
pub fn quiesce_thread_group(tgid: Pid, except: Pid) -> bool {
    {
        let rq = RUN_QUEUE.lock();
        let mut has_sibling = false;
        for i in 0..runqueue::MAX_TASKS {
            if let Some(t) = rq.get(i) {
                if t.tgid == tgid && t.pid != except { has_sibling = true; break; }
            }
        }
        if !has_sibling { return false; }
    }
    // One quiesce at a time system-wide.
    while QUIESCE_TGID
        .compare_exchange(0, tgid, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        irq_window();
        core::hint::spin_loop();
    }
    QUIESCE_EXCEPT.store(except, Ordering::Release);
    // Kick every sibling off its CPU and wait for the last one to land.
    loop {
        let mut running_cpu: Option<usize> = None;
        {
            let rq = RUN_QUEUE.lock();
            for i in 0..runqueue::MAX_TASKS {
                if let Some(t) = rq.get(i) {
                    if t.tgid == tgid && t.pid != except {
                        if let Some(c) = t.on_cpu { running_cpu = Some(c); break; }
                    }
                }
            }
        }
        match running_cpu {
            None => break,
            Some(c) => {
                trigger_preempt(c);
                irq_window();
                core::hint::spin_loop();
            }
        }
    }
    true
}

/// Release the dispatch suspension taken by `quiesce_thread_group`.
pub fn unquiesce_thread_group() {
    QUIESCE_EXCEPT.store(0, Ordering::Release);
    QUIESCE_TGID.store(0, Ordering::Release);
}

pub fn alloc_pid() -> Pid {
    let mut pid_guard = NEXT_PID.lock();
    let pid = *pid_guard;
    *pid_guard += 1;
    pid
}

pub fn current_pid() -> Pid {
    CURRENT_PID[unsafe { cpu_id() }].load(Ordering::Relaxed)
}

pub fn current_ppid() -> Pid {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.ppid).unwrap_or(0)
}

pub fn current_pgid() -> Pid {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.pgid).unwrap_or(0)
}

pub fn current_sid() -> Pid {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.sid).unwrap_or(0)
}

/// The process-group id of `pid`, or `None` if no such task is live.
///
/// `None` is the caller's cue to report ESRCH: `getpgid(2)` must distinguish
/// "that process is in group 0" from "that process does not exist", which a
/// bare `unwrap_or(0)` cannot.
pub fn pgid_of(pid: Pid) -> Option<Pid> {
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.pgid)
}

/// The thread-group id of `pid`, or `pid` itself if it's not a live task
/// (matches every task's own fallback of being its own tgid at creation).
pub fn tgid_of(pid: Pid) -> Pid {
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.tgid).unwrap_or(pid)
}

pub fn current_tgid() -> Pid {
    tgid_of(current_pid())
}

pub fn ticks() -> u64 {
    TIMER_TICKS.load(Ordering::Relaxed)
}

pub fn deliver_signal(pid: Pid, signo: u32) -> isize {
    let mut woke = false;
    let ret = {
        let mut rq = RUN_QUEUE.lock();
        let min_vr = rq.min_vruntime();
        if let Some(t) = rq.find_pid_mut(pid) {
            if signo > 0 && signo <= 64 {
                t.signal_pending |= 1 << (signo - 1);
                if t.state == TaskState::Blocked {
                    t.state = TaskState::Ready;
                    t.place(min_vr);
                    woke = true;
                }
                0
            } else {
                -22 // EINVAL
            }
        } else {
            -3 // ESRCH
        }
    };
    if woke { wake_up_an_idle_cpu(); }
    // A signalfd registered in this tgid may be parked in epoll_wait; the
    // new pending bit is a readiness edge for it. RUN_QUEUE is released above.
    wake_poll();
    ret
}

/// Deliver a *process-directed* signal (child-exit SIGCHLD, kill(pid), killpg)
/// to thread group `tgid`.
///
/// POSIX (signal(7)): a process-directed signal is delivered to *any one*
/// thread in the group that does not currently block it. Our per-thread
/// `signal_pending`/`signal_mask` model, plus a naive "always target the tgid
/// leader" delivery, breaks this: tokio/mio block SIGCHLD on the runtime's
/// main thread (the leader) and handle it on another thread, so a SIGCHLD
/// pinned onto the leader is never deliverable — `check_and_deliver_signals`
/// skips it (masked), the reaping handler never runs, and `child.wait().await`
/// hangs forever. Here we instead pick a thread that has the signal unmasked,
/// preferring one already Blocked (so the delivery also wakes it); only if
/// every thread blocks it do we fall back to the leader, leaving it pending
/// until some thread unblocks it — again exactly POSIX.
pub fn deliver_signal_process(tgid: Pid, signo: u32) -> isize {
    if signo == 0 || signo > 64 { return -22; }
    let bit = 1u64 << (signo - 1);
    let mut woke = false;
    // SIGKILL and SIGSTOP cannot be blocked, so the "does this thread have it
    // unmasked?" test must not gate their delivery. `sys_sigprocmask` already
    // refuses to set these bits, but a stale mask (or a future path that sets
    // `signal_mask` directly) must not be able to strand them on the shared
    // pending set: for these two, any live thread in the group is a valid
    // target. Belt and braces on the one pair of signals that must never fail.
    let unblockable = signal::UNBLOCKABLE & bit != 0;
    let ret = {
        let mut rq = RUN_QUEUE.lock();
        let min_vr = rq.min_vruntime();
        // Prefer a Blocked thread with the signal unmasked; otherwise any
        // unmasked thread; otherwise the leader.
        let mut chosen: Option<usize> = None;
        let mut chosen_blocked = false;
        for i in 0..runqueue::MAX_TASKS {
            if let Some(t) = rq.get(i) {
                if t.tgid == tgid && (unblockable || (t.signal_mask & bit) == 0) {
                    let blocked = t.state == TaskState::Blocked;
                    if chosen.is_none() || (blocked && !chosen_blocked) {
                        chosen = Some(i);
                        chosen_blocked = blocked;
                        if blocked { break; }
                    }
                }
            }
        }
        match chosen {
            Some(idx) => {
                // A thread can take it now: deliver directly, waking it if blocked.
                if let Some(t) = rq.get_mut(idx) {
                    t.signal_pending |= bit;
                    if t.state == TaskState::Blocked {
                        t.state = TaskState::Ready;
                        t.place(min_vr);
                        woke = true;
                    }
                    0
                } else { -3 }
            }
            None => {
                // Every thread currently masks it (e.g. std's fork() blocks all
                // signals). Park it at the process level on the leader; the next
                // thread to return to user space with it unmasked — typically
                // right after the fork thread's rt_sigprocmask re-unblock —
                // claims it in check_and_deliver_signals. No thread is woken:
                // the unblock is itself a syscall whose return runs the claim.
                match rq.find_pid_idx(tgid) {
                    Some(li) => { if let Some(l) = rq.get_mut(li) { l.shared_signal_pending |= bit; } 0 }
                    None => -3, // ESRCH
                }
            }
        }
    };
    if woke { wake_up_an_idle_cpu(); }
    // Wake any signalfd poller in the target tgid (RUN_QUEUE released above).
    wake_poll();
    ret
}

/// Mark a CLONE_VFORK child as done borrowing the parent's address space
/// (called at successful execve and at exit) — releases the parent from its
/// vfork suspension loop in `clone_thread`.
pub fn vfork_complete(pid: Pid) {
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        t.vfork_pending = false;
    }
}

/// kill(0-probe): 0 if `pid` names a live task, else -ESRCH.
pub fn exists_probe(pid: Pid) -> isize {
    if RUN_QUEUE.lock().find_pid(pid).is_some() { 0 } else { -3 }
}

/// Deliver `signo` to every *process* (thread-group leader) in process group
/// `pgid` — kill(-pgid) / killpg semantics. One delivery per process; the
/// per-thread routing happens at handler-delivery time via the shared
/// TGID action table.
pub fn kill_pgrp(pgid: Pid, signo: u32) -> isize {
    // Collect targets first: deliver_signal takes the RUN_QUEUE lock itself.
    let mut targets = [0 as Pid; runqueue::MAX_TASKS];
    let mut n = 0;
    {
        let rq = RUN_QUEUE.lock();
        for i in 0..runqueue::MAX_TASKS {
            if let Some(t) = rq.get(i) {
                if t.pgid == pgid && t.pid == t.tgid && n < targets.len() {
                    targets[n] = t.pid;
                    n += 1;
                }
            }
        }
    }
    if n == 0 { return -3; } // ESRCH — no such process group
    if signo == 0 { return 0; } // existence probe only
    for &pid in &targets[..n] {
        // Process-directed (killpg): route to an unmasked thread per process.
        let _ = deliver_signal_process(pid, signo);
    }
    0
}

/// Process-level pending signals parked on the caller's thread-group leader
/// (see `Task::shared_signal_pending`) — sigpending(2) must report these too.
pub fn shared_pending_signals() -> u64 {
    let pid = current_pid();
    let rq = RUN_QUEUE.lock();
    let tgid = match rq.find_pid(pid) { Some(t) => t.tgid, None => return 0 };
    rq.find_pid(tgid).map(|l| l.shared_signal_pending).unwrap_or(0)
}

pub fn pending_signals() -> u64 {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.signal_pending).unwrap_or(0)
}

pub fn clear_pending_signal(signo: u32) {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        if signo > 0 && signo <= 64 {
            t.signal_pending &= !(1 << (signo - 1));
        }
    }
}

pub fn replace_signal_mask(new_mask: u64) -> u64 {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        let old = t.signal_mask;
        t.signal_mask = new_mask;
        old
    } else { 0 }
}

pub fn current_reply_port() -> u32 {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.reply_port).unwrap_or(u32::MAX)
}

pub fn set_current_reply_port(port: u32) {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        t.reply_port = port;
    }
}

pub fn current_cwd(buf: *mut u8, max_len: usize) -> isize {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid(pid) {
        let len = t.cwd_len.min(max_len);
        unsafe { core::ptr::copy_nonoverlapping(t.cwd.as_ptr(), buf, len); }
        return len as isize;
    }
    -1
}

/// Copy `pid`'s chroot root (host-absolute) into `buf`. Returns its length, or
/// 0 when the task is not chrooted (`root_len <= 1`) — the caller's cue to skip
/// all root handling. `-1` if there is no such task.
pub fn root_of(pid: Pid, buf: *mut u8, max_len: usize) -> isize {
    if let Some(t) = RUN_QUEUE.lock().find_pid(pid) {
        if t.root_len <= 1 { return 0; }
        let len = t.root_len.min(max_len);
        unsafe { core::ptr::copy_nonoverlapping(t.root.as_ptr(), buf, len); }
        return len as isize;
    }
    -1
}

/// Root of the calling task — the common case, used by kernel path resolution.
pub fn current_root(buf: *mut u8, max_len: usize) -> isize {
    root_of(current_pid(), buf, max_len)
}

/// Establish `path` (host-absolute) as the calling task's chroot root.
pub fn set_root(path: &[u8]) -> bool {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        let len = path.len().min(127);
        t.root[..len].copy_from_slice(&path[..len]);
        t.root_len = len;
        return true;
    }
    false
}

/// Which tasks a nice query/update applies to — the `which`/`who` pair of
/// `getpriority(2)`/`setpriority(2)`, already resolved against the caller.
///
/// Resolving "who == 0 means me" in the kernel keeps the PRIO_* constants out
/// of the scheduler, which has no business knowing the syscall ABI.
#[derive(Clone, Copy)]
pub enum NiceTarget {
    Process(Pid),
    Pgrp(Pid),
    User(u32),
}

impl NiceTarget {
    fn matches(&self, t: &task::Task) -> bool {
        match *self {
            NiceTarget::Process(pid) => t.pid  == pid,
            NiceTarget::Pgrp(pgid)   => t.pgid == pgid,
            NiceTarget::User(uid)    => t.uid  == uid,
        }
    }
}

/// Lowest nice value (i.e. most favourable priority) among the matching tasks,
/// or `None` when nothing matches — the caller's cue to report ESRCH.
///
/// "Lowest wins" is what `getpriority(2)` specifies for the group and user
/// forms; for `Process` there is at most one match, so it degenerates.
pub fn get_nice_for(target: NiceTarget) -> Option<i8> {
    let rq = RUN_QUEUE.lock();
    (0..runqueue::MAX_TASKS)
        .filter_map(|i| rq.get(i))
        .filter(|t| target.matches(t))
        .map(|t| t.priority)
        .min()
}

/// Apply `nice` to every matching task. Returns false when nothing matched.
///
/// Re-placing each task is the part that is easy to miss: `weight` feeds
/// `slice_vt`, and a task's deadline is only recomputed when the current one
/// expires (`charge_vruntime`). Without `place` a renice would sit inert for up
/// to a full slice, which reads as "the weight table is wrong" rather than
/// "the change has not landed yet".
///
/// Placement uses the queue's current `min_vruntime`, exactly as wake-up does,
/// so renicing cannot be abused to mint a fresh lag credit.
pub fn set_nice_for(target: NiceTarget, nice: i8) -> bool {
    let nice = nice.clamp(-20, 19);
    let weight = task::nice_to_weight(nice);
    let mut rq = RUN_QUEUE.lock();
    let min_vr = rq.min_vruntime();
    let mut found = false;
    for i in 0..runqueue::MAX_TASKS {
        let is_match = rq.get(i).map(|t| target.matches(t)).unwrap_or(false);
        if !is_match { continue; }
        if let Some(t) = rq.get_mut(i) {
            t.priority = nice;
            t.weight   = weight;
            t.place(min_vr);
            found = true;
        }
    }
    found
}

pub fn set_cwd(path: &[u8]) -> bool {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        let len = path.len().min(127);
        t.cwd[..len].copy_from_slice(&path[..len]);
        t.cwd_len = len;
        return true;
    }
    false
}

/// Returns the current thread's configured alternate signal stack as
/// `(ss_sp, ss_size, ss_flags)`. `ss_flags` is 0 (enabled) or `SS_DISABLE`
/// (2) — never `SS_ONSTACK`, which callers derive from the live user SP.
pub fn current_altstack() -> (usize, usize, u32) {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid)
        .map(|t| (t.altstack_sp, t.altstack_size, t.altstack_flags))
        .unwrap_or((0, 0, 2)) // SS_DISABLE
}

/// Sets the current thread's alternate signal stack.
pub fn set_current_altstack(sp: usize, size: usize, flags: u32) {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        t.altstack_sp    = sp;
        t.altstack_size  = size;
        t.altstack_flags = flags;
    }
}

pub fn set_pgid(pid: Pid, pgid: Pid) -> bool {
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        t.pgid = pgid;
        return true;
    }
    false
}

pub fn euid_of(pid: Pid) -> u32 {
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.euid).unwrap_or(0)
}

pub fn egid_of(pid: Pid) -> u32 {
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.egid).unwrap_or(0)
}

pub fn current_uid()  -> u32 { RUN_QUEUE.lock().find_pid(current_pid()).map(|t| t.uid).unwrap_or(0) }
pub fn current_euid() -> u32 { euid_of(current_pid()) }
pub fn current_suid() -> u32 { RUN_QUEUE.lock().find_pid(current_pid()).map(|t| t.suid).unwrap_or(0) }
pub fn current_gid()  -> u32 { RUN_QUEUE.lock().find_pid(current_pid()).map(|t| t.gid).unwrap_or(0) }
pub fn current_egid() -> u32 { egid_of(current_pid()) }
pub fn current_sgid() -> u32 { RUN_QUEUE.lock().find_pid(current_pid()).map(|t| t.sgid).unwrap_or(0) }

/// setresuid(2) semantics. Each argument u32::MAX (-1) means "leave unchanged".
/// A privileged caller (euid==0 on entry) may set each id to any value; an
/// unprivileged caller may set each id only to one of its current real,
/// effective or saved uid. All-or-nothing: any EPERM leaves every id intact.
/// Returns false (⇒ EPERM) on violation.
pub fn set_current_resuid(ruid: u32, euid: u32, suid: u32) -> bool {
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        let (cur_r, cur_e, cur_s) = (t.uid, t.euid, t.suid);
        let priv_ = cur_e == 0;
        let allowed = |v: u32| v == cur_r || v == cur_e || v == cur_s;
        if !priv_ {
            if ruid != u32::MAX && !allowed(ruid) { return false; }
            if euid != u32::MAX && !allowed(euid) { return false; }
            if suid != u32::MAX && !allowed(suid) { return false; }
        }
        if ruid != u32::MAX { t.uid  = ruid; }
        if euid != u32::MAX { t.euid = euid; }
        if suid != u32::MAX { t.suid = suid; }
        return true;
    }
    false
}

/// setresgid(2) semantics — mirrors [`set_current_resuid`] for the group identity.
pub fn set_current_resgid(rgid: u32, egid: u32, sgid: u32) -> bool {
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        let (cur_r, cur_e, cur_s) = (t.gid, t.egid, t.sgid);
        let priv_ = t.euid == 0;
        let allowed = |v: u32| v == cur_r || v == cur_e || v == cur_s;
        if !priv_ {
            if rgid != u32::MAX && !allowed(rgid) { return false; }
            if egid != u32::MAX && !allowed(egid) { return false; }
            if sgid != u32::MAX && !allowed(sgid) { return false; }
        }
        if rgid != u32::MAX { t.gid  = rgid; }
        if egid != u32::MAX { t.egid = egid; }
        if sgid != u32::MAX { t.sgid = sgid; }
        return true;
    }
    false
}

/// setuid(2) semantics: a privileged (euid==0) caller sets real, effective and
/// saved uid; an unprivileged caller may set only its effective uid, and only
/// to its current real or saved uid. Returns false (⇒ EPERM) on violation.
pub fn set_current_uid(new_uid: u32) -> bool {
    if current_euid() == 0 {
        return set_current_resuid(new_uid, new_uid, new_uid);
    }
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        if new_uid == t.uid || new_uid == t.suid {
            t.euid = new_uid;
            return true;
        }
    }
    false
}

/// setgid(2) semantics — mirrors [`set_current_uid`] for the group identity.
pub fn set_current_gid(new_gid: u32) -> bool {
    if current_euid() == 0 {
        return set_current_resgid(new_gid, new_gid, new_gid);
    }
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        if new_gid == t.gid || new_gid == t.sgid {
            t.egid = new_gid;
            return true;
        }
    }
    false
}

pub fn setsid() -> Pid {
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        t.sid  = pid;
        t.pgid = pid;
        return pid;
    }
    0
}

/// Three-phase blocking on an IPC port, closing the check-then-block
/// lost-wake race:
///
///   1. `block_on_port_prepare(port)` — publish intent: mark the current
///      task Blocked-on-`port` while it is still executing.
///   2. Caller re-checks the message queue.  A sender that enqueues after
///      the caller's last (empty) look at the queue now already sees the
///      task Blocked, so its `unblock_port()` flips it back to Ready and
///      the wake cannot be lost.  If the re-check finds a message, call
///      `block_on_port_cancel()` and consume it instead of sleeping.
///   3. `block_on_port_commit()` — actually yield.  If a wake raced in
///      between, the task is already Ready and the scheduler simply
///      re-dispatches it.
///
/// Publishing Blocked while still executing is safe: syscalls run with
/// IRQs masked, and the `on_cpu` claim prevents any other CPU from
/// dispatching this task until it really enters the scheduler.
pub fn block_on_port_prepare(port: u32) {
    let pid = current_pid();
    RUN_QUEUE.lock().block_on_port(pid, port);
}

/// Undo `block_on_port_prepare` — the queue re-check found a message (or
/// the port is gone), so the task keeps running instead of sleeping.
pub fn block_on_port_cancel() {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        t.state         = TaskState::Running;
        t.blocked_on    = None;
        t.poll_deadline = u64::MAX;
    }
}

/// Complete a prepared block by yielding to the scheduler.
pub fn block_on_port_commit() {
    yield_now("block_on_port");
}

// ── Global poll/epoll wait-channel (K2 event-loop blocking) ─────────────────
//
// poll/ppoll/select/epoll_wait share one wait-channel instead of per-object
// waitqueues: the workload is a few dozen tasks, so a global wake that each
// blocked poller re-probes against is cheaper than the register/deregister
// bookkeeping a per-fd model needs, and it reuses the proven three-phase
// `block_on_port` protocol verbatim. The sentinel is outside the real port-id
// range (`port::alloc` only returns 1..MAX_PORTS), so `unblock_port` from a
// genuine IPC never touches pollers and `wake_poll` never touches IPC waiters.
pub const POLL_WAIT_CHANNEL: u32 = 0xFFFF_FF01;

/// Phase 1: publish Blocked-on-poll intent while still executing. No deadline
/// (an infinite / edge-only waiter). A timed waiter uses
/// `block_on_poll_prepare_until` instead so its deadline rides the SAME
/// RUN_QUEUE hold — no extra lock on the block path, and the deadline lives in
/// the task, not a clobber-prone global.
pub fn block_on_poll_prepare() { block_on_poll_prepare_until(u64::MAX) }

/// Phase 1 for a timed waiter: publish Blocked-on-poll AND the absolute-tick
/// wake deadline atomically (one RUN_QUEUE hold), then fold the deadline into
/// the global `NEXT_POLL_DEADLINE` hint (lock-free) so the tick's fast path can
/// skip the run-queue scan while nothing is due. The task field is the
/// authority; the hint is only an optimisation the tick recomputes exactly.
pub fn block_on_poll_prepare_until(deadline: u64) {
    let pid = current_pid();
    RUN_QUEUE.lock().block_on_port_until(pid, POLL_WAIT_CHANNEL, deadline);
    if deadline != u64::MAX {
        NEXT_POLL_DEADLINE.fetch_min(deadline, Ordering::Relaxed);
    }
}
/// Undo a prepared poll-block (the re-probe found readiness or a signal).
pub fn block_on_poll_cancel()  { block_on_port_cancel() }
/// Phase 3: yield; woken by `wake_poll`, a signal, or the deadline tick.
pub fn block_on_poll_commit()  { block_on_port_commit() }

/// Wake every task blocked on the poll wait-channel. Callers (edge publishers
/// in net/vfs, the deadline tick, signal delivery) MUST hold no server lock —
/// this takes RUN_QUEUE. Task context only (blocking lock); IRQ context uses
/// `try_wake_poll`.
pub fn wake_poll() { unblock_port(POLL_WAIT_CHANNEL); }

/// Non-blocking `wake_poll` for IRQ / tick context: honors the tick hook's
/// try_lock-only contract. Returns false (wake deferred) if RUN_QUEUE is
/// momentarily contended on another CPU; the next tick retries.
pub fn try_wake_poll() -> bool {
    match RUN_QUEUE.try_lock() {
        Some(mut rq) => {
            let woken = rq.unblock_port(POLL_WAIT_CHANNEL);
            drop(rq);
            if woken > 0 { wake_up_an_idle_cpu(); }
            true
        }
        None => false,
    }
}

/// Poll-deadline tick service (IRQ/tick context): wake every poll-channel
/// waiter whose per-task deadline is due (or all, when a timerfd has expired),
/// then republish `NEXT_POLL_DEADLINE` to the EXACT earliest remaining deadline.
///
/// This replaces the old wake-then-`store(u64::MAX)` reset. That reset raced
/// `register_poll_deadline`'s lock-free `fetch_min`: a deadline published in the
/// window between the wake and the store was wiped to `u64::MAX`, stranding the
/// waiter (nanosleep / finite epoll_wait have no edge source) until an unrelated
/// deadline coincidentally fired the tick — seconds late, or forever (M7). The
/// per-task deadline is now the authority: the wake and the exact recompute run
/// under ONE RUN_QUEUE hold, so a woken task can only re-register (again
/// lock-free, but its authoritative value is its own task field, set under this
/// same lock next time it parks) after the recompute — nothing to clobber.
///
/// Non-blocking (try_lock) for the tick's contract; a contended tick leaves the
/// hint and retries next tick (≤10 ms defer, within the timeout granularity).
pub fn service_poll_deadlines(now: u64, timerfd_due: bool) -> bool {
    match RUN_QUEUE.try_lock() {
        Some(mut rq) => {
            let (new_min, woken) =
                rq.wake_due_poll_deadlines(POLL_WAIT_CHANNEL, now, timerfd_due);
            NEXT_POLL_DEADLINE.store(new_min, Ordering::Relaxed); // exact, under the lock
            drop(rq);
            if woken > 0 { wake_up_an_idle_cpu(); }
            true
        }
        None => false,
    }
}

/// Earliest absolute tick at which a timed poll/select/epoll_wait waiter wants
/// to be re-woken (u64::MAX = no timed waiter). This is a lock-free HINT that
/// lets `poll_deadline_tick` skip the run-queue scan while nothing is due; the
/// authoritative deadlines live in each `Task::poll_deadline`, and the tick
/// recomputes this hint exactly under RUN_QUEUE in `service_poll_deadlines`. A
/// waiter folds its deadline in via `block_on_poll_prepare_until` (or
/// `register_poll_deadline` for a timerfd publish).
pub static NEXT_POLL_DEADLINE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Publish a wake deadline into the lock-free hint (monotone-minimum). Used by
/// the timerfd arm path — a timerfd is not a parked task, so it has no
/// `Task::poll_deadline`; the hint (and `vfs::earliest_timerfd_deadline`, which
/// the tick also consults) carry it. Parked timed waiters use
/// `block_on_poll_prepare_until` instead, which records the deadline in the task
/// AND folds it into this hint.
pub fn register_poll_deadline(deadline: u64) {
    NEXT_POLL_DEADLINE.fetch_min(deadline, Ordering::Relaxed);
}

// ── Per-process executable path (/proc/self/exe) ────────────────────────────
//
// A tgid-keyed side table rather than a `Task` field, so fork/clone's raw-copy
// task layout is untouched. `sys_execve` sets it on success; a process leader's
// exit clears it; fork inherits the parent's until the child execs; unset falls
// back to "/bin/init" (correct for the boot-loaded PID1, which never execs).
const MAX_EXE_PATHS: usize = 64;
const EXE_PATH_MAX: usize = 256;
struct ExePathEntry { tgid: Pid, len: u16, path: [u8; EXE_PATH_MAX] }
impl ExePathEntry {
    const fn empty() -> Self { Self { tgid: 0, len: 0, path: [0u8; EXE_PATH_MAX] } }
}
static EXE_PATHS: Mutex<[ExePathEntry; MAX_EXE_PATHS]> =
    Mutex::new([const { ExePathEntry::empty() }; MAX_EXE_PATHS]);

/// Record `tgid`'s executable path (bytes truncated to EXE_PATH_MAX). Replaces
/// any existing entry for the tgid; allocates a free slot otherwise.
pub fn set_exe_path(tgid: Pid, bytes: &[u8]) {
    let n = bytes.len().min(EXE_PATH_MAX);
    let mut t = EXE_PATHS.lock();
    // Reuse the tgid's existing slot, else the first free one.
    let idx = t.iter().position(|e| e.tgid == tgid)
        .or_else(|| t.iter().position(|e| e.tgid == 0));
    if let Some(i) = idx {
        t[i].tgid = tgid;
        t[i].len = n as u16;
        t[i].path[..n].copy_from_slice(&bytes[..n]);
    }
}

/// Copy `tgid`'s executable path into `out`; returns its length, or None if
/// unset (caller falls back to "/bin/init").
pub fn exe_path(tgid: Pid, out: &mut [u8]) -> Option<usize> {
    let t = EXE_PATHS.lock();
    let e = t.iter().find(|e| e.tgid == tgid)?;
    let n = (e.len as usize).min(out.len());
    out[..n].copy_from_slice(&e.path[..n]);
    Some(n)
}

/// Release `tgid`'s executable-path slot (process leader exit).
pub fn clear_exe_path(tgid: Pid) {
    let mut t = EXE_PATHS.lock();
    if let Some(e) = t.iter_mut().find(|e| e.tgid == tgid) { *e = ExePathEntry::empty(); }
}

/// Child (a new tgid) inherits the parent's exe path until it execs.
pub fn inherit_exe_path(parent_tgid: Pid, child_tgid: Pid) {
    let mut buf = [0u8; EXE_PATH_MAX];
    if let Some(n) = exe_path(parent_tgid, &mut buf) {
        set_exe_path(child_tgid, &buf[..n]);
    }
}

pub fn umask(mask: u32) -> u32 {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        let old = t.umask;
        if mask != u32::MAX { t.umask = mask & 0o777; }
        return old;
    }
    0
}

pub fn heap_end() -> usize {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.heap_end).unwrap_or(0)
}

pub fn init() {}

/// Block until child `pid` terminates, then reap it and return
/// `(reaped_pid, exit_code)`.
///
/// Returns the pid alongside the code because POSIX `waitpid()` must report
/// *which* child was reaped (its return value), not just the status. The
/// zombie may already have been auto-reaped into `EXIT_LOG` by the scheduler
/// before we get here, so both the live-zombie and the logged paths are
/// handled — in either case the pid is the one we were asked to wait on.
///
/// NB: a `pid` of `u32::MAX` (POSIX `-1`, "any child") is not resolved here —
/// `find_pid_idx`/`get_exit_code` can't match it — so it yields `None`
/// (ECHILD). Waiting for an unspecified child is a separate feature.
/// Child selector for `wait_try` — mirrors wait4(2)'s pid argument.
#[derive(Clone, Copy)]
pub enum WaitSel {
    Pid(Pid),
    Any,
    Pgid(Pid),
}

/// One non-blocking wait attempt's outcome.
pub enum WaitTry {
    /// A matching child terminated: (pid, exit_code). The child is marked
    /// reaped and will not be reported again.
    Reaped(Pid, i32),
    /// Matching children exist but none has terminated yet.
    StillRunning,
    /// The caller has no matching children at all — wait4 returns ECHILD.
    NoChildren,
}

/// Non-consuming variant of `wait_try`: reports whether a matching child
/// exists / has terminated without reaping it. Used by waitid() calls that
/// don't include WEXITED (stopped/continued-only waits must leave exit
/// statuses for a later wait4 to collect).
pub fn wait_peek(sel: WaitSel, caller_tgid: Pid) -> WaitTry {
    wait_scan(sel, caller_tgid, false)
}

/// Single non-blocking scan for a terminated child of `caller_tgid`.
///
/// Children forked from *any* thread of the caller count (their `ppid` is
/// the forking thread's pid, so parentage is matched through the parent's
/// tgid). Zombies still occupying a run-queue slot are reported in place —
/// marking `wait_reported` — because only the owning CPU's scheduler loop
/// may physically reap the slot (see `wait_pid`'s comment); the eventual
/// `EXIT_LOG` record is then born consumed. This closes the
/// SIGCHLD→wait4(WNOHANG) race: the child is waitable the moment it is a
/// zombie, not only once the scheduler has recycled its slot.
pub fn wait_try(sel: WaitSel, caller_tgid: Pid) -> WaitTry {
    wait_scan(sel, caller_tgid, true)
}

fn wait_scan(sel: WaitSel, caller_tgid: Pid, consume: bool) -> WaitTry {
    let matches = |pid: Pid, pgid: Pid| -> bool {
        match sel {
            WaitSel::Pid(p)  => pid == p,
            WaitSel::Any     => true,
            WaitSel::Pgid(g) => pgid == g,
        }
    };

    // Both phases run under the RUN_QUEUE lock, taking EXIT_LOG inside it —
    // the same lock order as the scheduler's zombie reap (which logs the
    // exit and removes the slot under RUN_QUEUE). Scanning the two under
    // separate lock acquisitions had a TOCTOU: a child reaped between the
    // exit-log read and the queue read appeared in neither, and a blocking
    // wait4 spuriously returned ECHILD for a child that just exited.
    let mut rq = RUN_QUEUE.lock();

    // Phase 1: live (or zombie-but-unrecycled) children on the run queue.
    let mut found_live = false;
    let mut zombie: Option<(usize, Pid, i32)> = None;
    for i in 0..runqueue::MAX_TASKS {
        let (pid, tgid, ppid, pgid, state, code, reported) = match rq.get(i) {
            Some(t) => (t.pid, t.tgid, t.ppid, t.pgid, t.state, t.exit_code, t.wait_reported),
            None => continue,
        };
        if pid != tgid { continue; } // threads are not waitable children
        let parent_tgid = rq.find_pid(ppid).map(|p| p.tgid).unwrap_or(ppid);
        if parent_tgid != caller_tgid || !matches(pid, pgid) { continue; }
        if state == TaskState::Zombie {
            if !reported && zombie.is_none() {
                zombie = Some((i, pid, code));
            }
            // Reported zombies are logically reaped already — skip.
        } else {
            found_live = true;
        }
    }
    if let Some((i, pid, code)) = zombie {
        if consume {
            if let Some(t) = rq.get_mut(i) { t.wait_reported = true; }
        }
        return WaitTry::Reaped(pid, code);
    }

    // Phase 2: already-recycled children in the exit log (still under the
    // run-queue lock, so a concurrent reap cannot slip between the phases).
    {
        let mut log = EXIT_LOG.lock();
        for entry in log.iter_mut().filter_map(|e| e.as_mut()) {
            if entry.consumed || !entry.is_process { continue; }
            if entry.parent_tgid != caller_tgid { continue; }
            if matches(entry.pid, entry.pgid) {
                if consume { entry.consumed = true; }
                return WaitTry::Reaped(entry.pid, entry.code);
            }
        }
    }

    if found_live { WaitTry::StillRunning } else { WaitTry::NoChildren }
}

/// The calling task's blocked-signal mask.
pub fn current_sigmask() -> u64 {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.signal_mask).unwrap_or(0)
}

pub fn wait_pid(pid: Pid) -> Option<(Pid, i32)> {
    loop {
        {
            let rq = RUN_QUEUE.lock();
            if rq.find_pid_idx(pid).is_none() {
                drop(rq);
                if let Some(code) = get_exit_code(pid) { return Some((pid, code)); }
                return None;
            }
            // Task still occupies a slot.  Even if it is already a Zombie we
            // must NOT reap it here: on SMP its owning CPU may still be inside
            // cpu_switch_to, actively saving registers into the Task and
            // running on the kernel stack / page tables we would free.  The
            // owning CPU's scheduler loop is the single reaper — it removes
            // the slot and records the exit code in EXIT_LOG, which the
            // `find_pid_idx == None` branch above then picks up.
        }
        
        irq_window();

        yield_now("wait_pid");
    }
}

/// Briefly open an interrupt window so pended IRQs (local timer tick,
/// reschedule IPI) get delivered, then mask again.  Every kernel-context
/// wait loop must call this each iteration: syscalls run with IRQs
/// masked, so a task that yield-polls in kernel mode otherwise keeps its
/// CPU IF=0 indefinitely.  CPU 0 is the global timekeeper (TIMER_TICKS)
/// and input drain, so starving it freezes nanosleep/poll deadlines and
/// stdin system-wide.
///
/// x86-64: the window must be `sti; pause; cli`, NOT `sti; nop; cli`.
/// Under QEMU TCG the `sti` interrupt shadow suppresses exactly one
/// delivery check (at the TB boundary sti forces), and a plain `nop`
/// then runs on in chained translated code with no further check before
/// `cli` closes the window — a pended interrupt is essentially never
/// taken, and the global tick freezes for seconds whenever CPU 0 hosts
/// only kernel-mode pollers (observed as doom/shell multi-second
/// stalls; LAPIC dump showed vector 32 stuck in IRR with IF=0).
/// `pause` exits the TCG execution loop *after* the shadow is consumed,
/// creating a real delivery point; on hardware it is the standard
/// spin-wait hint.  AArch64 has no interrupt shadow — the DAIF write
/// itself ends the translation block and the next entry delivers.
#[inline(always)]
pub fn irq_window() {
    unsafe {
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!("sti; pause; cli");
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("msr daifclr, #2; nop; msr daifset, #2");
    }
}

pub fn yield_now(reason: &str) {
    let _ = reason;
    let id = unsafe { cpu_id() };
    unsafe {
        if let Some(ctx_ptr) = CURRENT_CTX[id].as_mut() {
            context::cpu_switch_to(
                ctx_ptr,
                core::ptr::addr_of!(SCHEDULER_CTX[id]),
            );
        }
    }
}

/// Up to 4 optional 100 Hz hooks run from the BSP timer IRQ (see
/// register_tick_hook). Audio owns one slot (its queue pump is load-bearing for
/// MAME sound latency — never regress it); K2 registers the poll-deadline hook.
const MAX_TICK_HOOKS: usize = 4;
static TICK_HOOKS: [core::sync::atomic::AtomicUsize; MAX_TICK_HOOKS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; MAX_TICK_HOOKS];

/// Register a function to run on every BSP timer tick, in IRQ context.
/// The hook must be non-blocking (try_lock only, no sleeps) and fast.
/// Used by the audio server (queue pump) and the K2 poll-deadline waker.
/// Silently ignored past MAX_TICK_HOOKS registrations.
pub fn register_tick_hook(f: fn()) {
    for h in TICK_HOOKS.iter() {
        if h.compare_exchange(0, f as usize, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            return;
        }
    }
}

pub fn timer_tick_irq() {
    let id = unsafe { cpu_id() };
    // Every CPU has its own local timer; only the BSP advances global time so
    // TIMER_TICKS keeps its 100 Hz meaning regardless of CPU count.
    if id == 0 {
        TIMER_TICKS.fetch_add(1, Ordering::Relaxed);
        for h in TICK_HOOKS.iter() {
            let hook = h.load(Ordering::Acquire);
            if hook != 0 {
                let f: fn() = unsafe { core::mem::transmute(hook) };
                f();
            }
        }
    }
    PREEMPT_NEEDED[id.min(MAX_CPUS - 1)].store(true, Ordering::Relaxed);
}

pub fn preempt_check() {
    let id = unsafe { cpu_id() };
    if PREEMPT_NEEDED[id.min(MAX_CPUS - 1)].swap(false, Ordering::Relaxed) {
        yield_now("preempt");
    }
}

/// Acquire exclusive access to the address space of `pid`'s thread-group
/// leader and return a raw pointer to it, with the run-queue lock
/// RELEASED.  Pair with `unlock_address_space`.
///
/// Servicing a fault or mutating mappings allocates, copies whole 4 KiB
/// pages, and can wait on TLB-shootdown acknowledgements.  Doing that
/// while holding RUN_QUEUE stalls every other CPU's scheduler loop and
/// convoys with the shootdown wait itself: a target CPU spinning on
/// RUN_QUEUE with IRQs masked can never take the shootdown IPI (see the
/// timeout note in arch/x86_64/src/paging.rs).  The per-address-space
/// `busy` flag keeps AddressSpace access exclusive without pinning the
/// scheduler.
///
/// Why handing out a raw pointer is sound:
///  * Every caller acts on behalf of a *running* member of the group, and
///    a leader must outlive its threads — each running thread's CR3/TTBR0
///    points into the leader's page tables, so a reaped leader would
///    already mean freed page tables in live use.
///  * Holders never yield or block while the flag is set (syscalls and
///    fault handlers run with IRQs masked, and no holder sleeps), so hold
///    times are bounded and spinning on `busy` cannot deadlock.
///  * `replace_address_space` (execve) waits for `busy` to clear before
///    dropping the displaced address space.
pub(crate) fn lock_leader_address_space(pid: Pid) -> Option<*mut mm::vmm::AddressSpace> {
    loop {
        {
            let mut rq = RUN_QUEUE.lock();
            let tgid = rq.find_pid(pid)?.tgid;
            let leader = rq.find_pid_mut(tgid)?;
            let as_ = leader.address_space.as_ref()?;
            if as_
                .busy
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                // `as_` is a shared `&Arc<AddressSpace>` now (see
                // `Task::address_space`'s doc comment for why it's an `Arc`,
                // not a `Box`) — the cast to `*mut` is the same "exclusivity
                // comes from `busy`, not from the reference type" contract
                // this function already documents; nothing else observes
                // this pointer while `busy` is held.
                return Some(&**as_ as *const mm::vmm::AddressSpace as *mut mm::vmm::AddressSpace);
            }
        }
        // Another CPU holds the address space (fault, mm syscall, or fork
        // clone of the same process).  Retry with the run-queue lock
        // dropped so schedulers stay unblocked while we wait.
        core::hint::spin_loop();
    }
}

/// Release exclusive access taken by `lock_leader_address_space`.
pub(crate) unsafe fn unlock_address_space(as_ptr: *mut mm::vmm::AddressSpace) {
    (*as_ptr).busy.store(false, Ordering::Release);
}

pub fn handle_page_fault(addr: usize, is_write: bool) -> bool {
    fn print_str(s: &str) {
        extern "C" { fn arch_serial_putc(c: u8); }
        for &b in s.as_bytes() {
            unsafe { arch_serial_putc(b); }
        }
    }

    let pid = current_pid();
    if pid == 0 { return false; }

    // Service the fault under the per-address-space lock, not RUN_QUEUE:
    // the fault path allocates and may copy a full page, easily tens of
    // microseconds during fork/CoW storms, and pinning the scheduler lock
    // for that long stalls every other CPU (and convoys with shootdown
    // waits — see lock_leader_address_space).
    let as_ptr = match lock_leader_address_space(pid) {
        Some(p) => p,
        None => {
            print_str("[PF] no address space for faulting task\n");
            return false;
        }
    };
    let ok = unsafe { (*as_ptr).handle_user_page_fault(addr, is_write) };
    unsafe { unlock_address_space(as_ptr); }
    if !ok {
        print_str("[PF] handle_user_page_fault returned false\n");
    }
    ok
}

pub fn ap_entry() -> ! {
    // Park until the BSP finishes kernel init and calls run().  This keeps
    // the pre-SMP guarantee that no task executes before all servers and
    // drivers are registered.
    while !SCHED_ONLINE.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    scheduler_run_loop()
}

pub fn unblock_port(port: u32) {
    let woken = RUN_QUEUE.lock().unblock_port(port);
    if woken > 0 { wake_up_an_idle_cpu(); }
}

pub fn spawn(entry: fn() -> !, _flags: usize) -> Option<Pid> {
    let pid = alloc_pid();

    // Allocate stack for the kernel task (64KB)
    let stack_base = mm::buddy::alloc(4)?; 
    let stack_size = mm::buddy::PAGE_SIZE * 16;

    let task = Task::new_kernel(pid, entry as usize, stack_base, stack_size, 0);
    let ok = RUN_QUEUE.lock().enqueue(task);
    if ok {
        wake_up_an_idle_cpu();
        Some(pid)
    } else {
        mm::buddy::free(stack_base, 4);
        None
    }
}

pub fn spawn_user_with_address_space(entry_point: usize, sp: usize, as_: mm::vmm::AddressSpace) -> Option<Pid> {
    extern "C" { 
        fn serial_print(s: *const u8, len: usize); 
        fn print_hex(n: usize);
        fn print_number(n: u32);
    }
    unsafe {
        let msg = b"[SCHED] spawn_user: entry=";
        serial_print(msg.as_ptr(), msg.len());
        print_hex(entry_point);
        let msg2 = b" sp=";
        serial_print(msg2.as_ptr(), msg2.len());
        print_hex(sp);
        serial_print(b"\n".as_ptr(), 1);
    }

    let pid = alloc_pid();

    unsafe {
        let msg = b"[SCHED] Allocating kernel stack...\n";
        serial_print(msg.as_ptr(), msg.len());
    }
    let stack_phys = mm::buddy::alloc(4)?; // 64KB kernel stack
    let _stack_virt = mm::phys_to_virt(stack_phys);
    let stack_size = mm::buddy::PAGE_SIZE * 16;
    let page_table = as_.page_table_root;

    unsafe {
        let msg = b"[SCHED] Creating task struct for PID ";
        serial_print(msg.as_ptr(), msg.len());
        print_number(pid);
        serial_print(b"\n".as_ptr(), 1);
    }

    let mut task = Task::new_userspace(pid, entry_point, sp, stack_phys, stack_size, page_table);
    task.kernel_stack = stack_phys;
    task.address_space = Some(alloc::sync::Arc::new(as_));

    let ok = RUN_QUEUE.lock().enqueue(task);
    if ok {
        wake_up_an_idle_cpu();
        Some(pid)
    } else {
        mm::buddy::free(stack_phys, 4);
        None
    }
}

pub fn run() -> ! {
    // Release any parked APs — kernel init is complete.
    SCHED_ONLINE.store(true, Ordering::Release);
    scheduler_run_loop()
}

fn scheduler_run_loop() -> ! {
    extern "C" { 
        fn serial_print(s: *const u8, len: usize); 
        fn print_number(n: u32);
    }
    unsafe {
        let msg = b"[SCHED] scheduler_run_loop started...\n";
        serial_print(msg.as_ptr(), msg.len());
    }
    let id = unsafe { cpu_id() };
    unsafe {
        let msg = b"[SCHED] CPU ID: ";
        serial_print(msg.as_ptr(), msg.len());
        print_number(id as u32);
        serial_print(b"\n".as_ptr(), 1);
    }
    loop {
        // Deliver any pended IRQs (timer tick, resched IPI) once per
        // scheduling cycle.  This is the systemic guarantee that a CPU
        // hosting only kernel-context wait loops still takes its local
        // timer: every yield passes through here, whereas per-loop
        // irq_window() calls depend on each wait site remembering to open
        // one (several — init's poll loops, VFS lock waits — did not).
        // Safe here: this is the scheduler context, not an IRQ handler, and
        // a tick arriving in the window only sets PREEMPT_NEEDED (its
        // preempt_check sees CURRENT_CTX == null and returns).
        irq_window();

        // Pick, claim (on_cpu) and mark Running under a single lock so no
        // other CPU can dispatch the same task between pick and claim.
        let picked = {
            let mut rq = RUN_QUEUE.lock();
            match rq.pick_next() {
                Some(idx) => {
                    let t = rq.get_mut(idx).unwrap();
                    t.on_cpu = Some(id);
                    t.state  = TaskState::Running;
                    let kst = mm::phys_to_virt(t.kernel_stack) + mm::buddy::PAGE_SIZE * 16;
                    Some((idx, &t.ctx as *const CpuContext, t.pid, kst, t.page_table))
                }
                None => None,
            }
        };

        if let Some((idx, ctx_ptr, pid, kernel_stack_top_virt, page_table)) = picked {
            let dispatched_at = ticks();

            unsafe {
                CURRENT_CTX[id] = ctx_ptr as *mut CpuContext;
                CURRENT_PID[id].store(pid, Ordering::Relaxed);

                arch_set_kernel_stack(kernel_stack_top_virt as u64);
                if page_table != 0 {
                    arch_set_page_table(page_table);
                }

                context::cpu_switch_to(
                    core::ptr::addr_of_mut!(SCHEDULER_CTX[id]),
                    ctx_ptr,
                );

                // When we return here, we are in the scheduler context.
                CURRENT_CTX[id] = core::ptr::null_mut();
                CURRENT_PID[id].store(0, Ordering::Relaxed);

                // Detach from the task's page table before it can be freed:
                // if this task exits (reaped below) or exits later on another
                // CPU, its tables are released and reused — a CPU still
                // holding them in CR3/TTBR0 faults on its next TLB miss.
                arch_load_kernel_page_table();
            }

            // Charge CPU time, release the on_cpu claim, and — if the task
            // exited — atomically take its slot out of the queue.  All under
            // one lock: once on_cpu is None other CPUs may dispatch or (for
            // zombies) observe the slot, so the claim must drop in the same
            // critical section that resolves the task's final state.
            let zombie = {
                let mut rq = RUN_QUEUE.lock();
                let zinfo = match rq.get_mut(idx) {
                    Some(t) if t.pid == pid => {
                        let delta = ticks().saturating_sub(dispatched_at);
                        t.charge_vruntime(delta);
                        t.on_cpu = None;
                        if t.state == TaskState::Running {
                            t.state = TaskState::Ready;
                        }
                        if t.state == TaskState::Zombie {
                            Some((t.pid, t.exit_code, t.ppid, t.pgid,
                                  t.pid == t.tgid, t.wait_reported))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some((zpid, code, ppid, pgid, is_proc, reported)) = zinfo {
                    // Log the exit code BEFORE the slot disappears:
                    // a parent polling wait_pid/wait_try on another CPU must
                    // always find the pid either in the queue or in
                    // EXIT_LOG — a gap between remove() and a later
                    // log_exit() makes waitpid return ECHILD.
                    let parent_tgid = rq.find_pid(ppid).map(|p| p.tgid).unwrap_or(ppid);
                    log_exit(zpid, code, parent_tgid, pgid, is_proc, reported);
                    rq.remove(idx)
                } else {
                    None
                }
            };

            if let Some(t) = zombie {
                // This CPU ran the task to its death and holds the only
                // reference now — safe to run the exit hook and free its
                // kernel stack.  Dropping the Box releases the address space.
                let hook_ptr = TASK_EXIT_HOOK.load(Ordering::Acquire);
                if !hook_ptr.is_null() {
                    let hook: fn(u32) = unsafe { core::mem::transmute(hook_ptr) };
                    hook(t.pid);
                }
                mm::buddy::free(t.kernel_stack, 4);
            }
        } else {
            unsafe {
                #[cfg(target_arch = "x86_64")]
                core::arch::asm!("sti; hlt; cli");
                #[cfg(target_arch = "aarch64")]
                core::arch::asm!("msr daifclr, #2; wfi; msr daifset, #2");
            }
        }
    }
}

/// Run the registered fd/pipe/socket teardown for `pid`, if one is installed.
///
/// Factored out of [`exit`] so the group-kill paths can run it for every
/// sibling they reap: `kill_next_group_member` stops a thread without that
/// thread ever entering its own `exit`, so nothing else would release its fds.
fn run_exit_teardown(pid: Pid) {
    let hook_ptr = EXIT_TEARDOWN_HOOK.load(Ordering::Acquire);
    if !hook_ptr.is_null() {
        let hook: fn(u32) = unsafe { core::mem::transmute(hook_ptr) };
        hook(pid);
    }
}

/// Terminate the *whole* thread group, then the caller — Linux `exit_group(2)`
/// semantics.
///
/// [`exit`] kills only the calling thread. That is the right primitive for a
/// thread returning from its start routine and the wrong one for anything that
/// is meant to end a *process*. A fatal signal (`SIG_DFL` terminate) used to
/// call `exit` directly, so when it landed on a non-leader thread it reaped
/// just that thread and left the process running.
///
/// That was not a rare corner: `deliver_signal_process` deliberately prefers a
/// *blocked* thread as the delivery target (so the signal also wakes it), and
/// in a tokio program the blocked thread is almost always a worker parked in
/// epoll rather than the leader. The result was that `kill -9` on a threaded
/// process succeeded or did nothing depending on which thread happened to be
/// parked at that instant.
///
/// The loop-then-`exit` ordering is required rather than stylistic — see
/// [`kill_next_group_member`]: every sibling must have actually *stopped*
/// before the leader's shared `AddressSpace` may be dropped.
pub fn exit_group(code: i32) -> ! {
    loop {
        match kill_next_group_member(code) {
            GroupKillStep::Done        => break,
            GroupKillStep::Reaped(pid) => run_exit_teardown(pid),
            GroupKillStep::Kicking     => core::hint::spin_loop(),
        }
    }
    exit(code)
}

pub fn exit(code: i32) -> ! {
    extern "C" {
        fn serial_print(s: *const u8, len: usize);
        fn print_number(n: u32);
    }
    let pid = current_pid();

    // Release this task's fds *before* it becomes a zombie.
    //
    // The `EXIT`/`EXIT_GROUP` syscalls already call `vfs_close_all_current()`
    // on the way in, but they are not the only way a task dies: a default-
    // action signal (`SIG_DFL` terminate), a failed signal-frame write, and a
    // corrupt `rt_sigreturn` frame all call straight into here from
    // `sched::signal`. Those paths used to skip fd teardown entirely, so a
    // process killed by a signal while holding a pipe's write end left the
    // ring's writer count above zero forever — the reader at the other end
    // then blocked permanently instead of seeing EOF, which is the shell-wedge
    // signature, and the ring slot leaked from a pool of only MAX_PIPES = 16.
    // Ctrl-C'ing enough pipelines would exhaust it.
    //
    // Running it here rather than from the reap hook is deliberate: this is
    // still the dying task's own context, with its address space live and no
    // scheduler lock held, so a blocking IPC call into the VFS is as safe as
    // it is from the `EXIT` syscall. The reap hook has none of those
    // properties. Re-running teardown for a task that came through `EXIT` is
    // harmless — the second pass finds an empty fd table and does nothing.
    run_exit_teardown(pid);

    let clear_addr = {
        let rq = RUN_QUEUE.lock();
        rq.find_pid(pid).map(|t| t.clear_child_tid).unwrap_or(0)
    };
    if clear_addr != 0 {
        let zero = 0u32;
        let written = with_current_address_space(|as_| {
            as_.write_user_buf(clear_addr, &zero.to_ne_bytes())
        }).unwrap_or(false);
        if written {
            futex_wake(clear_addr, 1);
        }
    }

    let (tgid, ppid) = {
        let mut rq = RUN_QUEUE.lock();
        match rq.find_pid_mut(pid) {
            Some(t) => {
                t.state = TaskState::Zombie;
                t.exit_code = code;
                t.vfork_pending = false; // release a vfork-suspended parent
                (t.tgid, t.ppid)
            }
            None => (pid, 0),
        }
    };
    // Release the /proc/self/exe side-table slot when the process leader dies.
    if pid == tgid { clear_exe_path(tgid); }
    // POSIX: the parent gets SIGCHLD when a child *process* terminates.
    // Threads (pid != tgid) don't signal, and the signal goes to the parent's
    // thread-group leader since the signal-action table is TGID-shared.
    // Ignored by default (SIGCHLD is in the default-ignore set), but it wakes
    // a parent blocked in read/epoll_wait with EINTR — which is exactly how
    // tokio's child-reaping learns the child is gone.
    if pid == tgid && ppid != 0 {
        // Process-directed: deliver to any parent thread that hasn't masked
        // SIGCHLD (tokio blocks it on the main thread), not blindly the leader.
        let _ = deliver_signal_process(tgid_of(ppid), 17);
    }
    yield_now("exit");
    loop { core::hint::spin_loop(); }
}

/// Outcome of one `kill_next_group_member` step.
pub enum GroupKillStep {
    /// No other thread-group member remains — only the caller is left.
    Done,
    /// `pid` was off-CPU and has been fully reaped (removed from the run
    /// queue, kernel stack freed, exit hook run). The caller should still
    /// release any kernel-side resources it owns that the reap hook doesn't
    /// know about (e.g. VFS fds — see `vfs_close_all_for` in syscall.rs).
    Reaped(Pid),
    /// A member is mid-flight on another CPU — it has been marked to die
    /// and that CPU kicked with a reschedule IPI. Call again after a short
    /// spin; its own dispatch loop will reap it once it stops running.
    Kicking,
}

/// Terminate one other member of the calling task's thread group.
///
/// Linux's `exit_group(2)` kills every thread in the process, not just the
/// caller. Before this existed, `EXIT_GROUP` only called [`exit`] for the
/// calling task, so e.g. a Rust `std::thread` worker that outlived `main()`
/// (a common pattern — `bottom`'s data-collection threads do exactly this)
/// kept running after the thread-group leader was reaped. The leader owns
/// the shared `AddressSpace` (see `Task::address_space`); reaping it drops
/// the page tables out from under any sibling still mid-execution on
/// another CPU, which then takes an instruction-fetch page fault that
/// `handle_page_fault` can't service — its `tgid` lookup fails because the
/// leader is already gone (`lock_leader_address_space` returns `None`,
/// logged as "no address space for faulting task").
///
/// Call this in a loop (see `EXIT_GROUP` in `kernel/src/syscall.rs`) until
/// it returns `Done`, *then* call `exit` for the caller — that ordering
/// guarantees every sibling has actually stopped running (not merely been
/// asked to) before the leader's `AddressSpace` can be dropped.
pub fn kill_next_group_member(exit_code: i32) -> GroupKillStep {
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    let tgid = match rq.find_pid(pid) {
        Some(t) => t.tgid,
        None => return GroupKillStep::Done,
    };

    let mut target: Option<usize> = None;
    for i in 0..runqueue::MAX_TASKS {
        if let Some(t) = rq.get(i) {
            if t.tgid == tgid && t.pid != pid {
                target = Some(i);
                break;
            }
        }
    }
    let idx = match target {
        Some(i) => i,
        None => return GroupKillStep::Done,
    };

    let (tpid, on_cpu, tppid, tpgid, t_is_proc, t_reported) = {
        let t = rq.get_mut(idx).unwrap();
        t.state = TaskState::Zombie;
        t.exit_code = exit_code;
        t.vfork_pending = false; // release a vfork-suspended parent
        (t.pid, t.on_cpu, t.ppid, t.pgid, t.pid == t.tgid, t.wait_reported)
    };

    if let Some(cpu) = on_cpu {
        // Still running on another core — cannot touch its kernel stack or
        // the shared address space yet. Its own dispatch loop reaps it
        // (scheduler_run_loop's post-`cpu_switch_to` check) the moment it
        // actually stops, which the resched IPI hastens.
        drop(rq);
        trigger_preempt(cpu);
        return GroupKillStep::Kicking;
    }

    // Ready/Blocked and not on any CPU: nothing is executing its code, so
    // it's safe to reap right now — mirrors scheduler_run_loop's own
    // post-dispatch reap exactly (log, remove, hook, free kernel stack).
    let parent_tgid = rq.find_pid(tppid).map(|p| p.tgid).unwrap_or(tppid);
    log_exit(tpid, exit_code, parent_tgid, tpgid, t_is_proc, t_reported);
    let reaped = rq.remove(idx);
    drop(rq);

    futex::remove_waiter(tpid);

    if let Some(t) = reaped {
        let hook_ptr = TASK_EXIT_HOOK.load(Ordering::Acquire);
        if !hook_ptr.is_null() {
            let hook: fn(u32) = unsafe { core::mem::transmute(hook_ptr) };
            hook(t.pid);
        }
        mm::buddy::free(t.kernel_stack, 4);
    }
    GroupKillStep::Reaped(tpid)
}

pub fn set_clear_child_tid(tidptr: usize) {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        t.clear_child_tid = tidptr;
    }
}

pub fn set_fs_base(addr: u64) {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        t.tls_base = addr;
        #[cfg(target_arch = "x86_64")]
        { t.ctx.fs_base = addr; }
        #[cfg(target_arch = "aarch64")]
        { t.ctx.tpidr_el0 = addr; }
    }
}

pub fn get_fs_base() -> u64 {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.tls_base).unwrap_or(0)
}

pub fn replace_address_space(
    new_as: mm::vmm::AddressSpace,
    pt_root: usize,
    heap_start: usize,
    entry: usize,
    user_sp: usize
) -> ! {
    let pid = current_pid();
    let old_as = {
        let mut rq = RUN_QUEUE.lock();
        if let Some(t) = rq.find_pid_mut(pid) {
            let displaced = t.address_space.replace(alloc::sync::Arc::new(new_as));
            t.page_table    = pt_root;
            t.heap_start    = heap_start;
            t.heap_end      = heap_start;
            // Reset the TLS base so the new program starts with a clean slate.
            // The hardware register is zeroed in arch_execve_return, but ctx.tpidr_el0
            // (AArch64) / ctx.fs_base (x86-64) holds the kernel-visible copy used by
            // cpu_switch_to.  If a timer IRQ fires between eret and the new program's
            // static_init, cpu_switch_to restores from ctx, overwriting the zeroed
            // hardware register with the previous program's stale TLS pointer.
            t.tls_base = 0;
            #[cfg(target_arch = "aarch64")]
            { t.ctx.tpidr_el0 = 0; }
            #[cfg(target_arch = "x86_64")]
            { t.ctx.fs_base = 0; }
            displaced
        } else {
            None
        }
    };

    extern "C" {
        fn arch_execve_return(entry: usize, user_sp: usize) -> !;
    }

    unsafe {
        // Switch to the new root BEFORE dropping the old address space:
        // its Drop frees the old page tables, and this CPU must not be
        // executing on a freed root even briefly.
        arch_set_page_table(pt_root);

        // Drop the displaced address space outside the run-queue lock —
        // its Drop frees every backing page and broadcasts a TLB shootdown
        // whose ack wait must not pin the scheduler lock.  Wait for any
        // in-flight holder of its busy flag (a fault on another CPU by a
        // thread of the pre-exec image) to finish first.
        if let Some(old) = old_as {
            while old.busy.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
            drop(old);
        }

        arch_execve_return(entry, user_sp);
    }
}

pub fn spawn_user(_entry_va: usize, _stack_va: usize, _priority: i8) -> Option<Pid> {
    None
}

// The with_*_address_space accessors all funnel through
// lock_leader_address_space so the closure runs WITHOUT the run-queue
// lock held (see that function's doc for why).  The `busy` flag is
// exclusive, so the shared (&) variants serialize against the mutable
// ones as well.  Closures must not yield, block, or take the run-queue
// lock-and-wait on another address space.

pub fn with_address_space<F, R>(pid: Pid, f: F) -> Option<R>
where F: FnOnce(&mm::vmm::AddressSpace) -> R {
    let as_ptr = lock_leader_address_space(pid)?;
    let r = f(unsafe { &*as_ptr });
    unsafe { unlock_address_space(as_ptr); }
    Some(r)
}

pub fn with_address_space_mut<F, R>(pid: Pid, f: F) -> Option<R>
where F: FnOnce(&mut mm::vmm::AddressSpace) -> R {
    let as_ptr = lock_leader_address_space(pid)?;
    let r = f(unsafe { &mut *as_ptr });
    unsafe { unlock_address_space(as_ptr); }
    Some(r)
}

pub fn with_task_address_space<F, R>(pid: Pid, f: F) -> Option<R>
where F: FnOnce() -> R {
    let rq = RUN_QUEUE.lock();
    let t = rq.find_pid(pid)?;
    let leader = rq.find_pid(t.tgid)?;
    let pt_root = leader.address_space.as_ref()?.root();
    drop(rq);

    extern "C" {
        fn arch_get_current_root() -> usize;
        fn arch_set_page_table(root: usize);
        fn arch_interrupt_save() -> usize;
        fn arch_interrupt_restore(flags: usize);
    }

    unsafe {
        // The borrowed root must not outlive this block on this CPU: if a
        // timer IRQ preempts the caller mid-copy, the scheduler switches
        // CR3/TTBR0 away and — kernel tasks being dispatched without a page
        // table load — the copy would resume against the wrong (or no)
        // address space and corrupt memory.  Keep the window IRQ-atomic.
        let irq = arch_interrupt_save();
        let old_root = arch_get_current_root();
        arch_set_page_table(pt_root);
        let res = f();
        arch_set_page_table(old_root);
        arch_interrupt_restore(irq);
        Some(res)
    }
}

pub fn with_current_address_space<F, R>(f: F) -> Option<R>
where F: FnOnce(&mm::vmm::AddressSpace) -> R {
    with_address_space(current_pid(), f)
}

pub fn with_current_address_space_mut<F, R>(f: F) -> Option<R>
where F: FnOnce(&mut mm::vmm::AddressSpace) -> R {
    with_address_space_mut(current_pid(), f)
}

pub fn register_task_exit_hook(hook: fn(u32)) {
    TASK_EXIT_HOOK.store(hook as *mut (), Ordering::Release);
}

/// Register the fd/pipe/socket teardown that [`exit`] must run for *every*
/// dying task, however it came to die.
///
/// Distinct from [`register_task_exit_hook`], which fires from the scheduler's
/// reap path — that runs under the run-queue lock, on some other CPU's
/// dispatch loop, with the dead task's address space already gone. It is the
/// right place to free a kernel stack and release IPC ports, and the wrong
/// place to make a blocking IPC call into the VFS. This hook instead runs in
/// the dying task's own context, before it becomes a zombie, which is exactly
/// the context the `EXIT` syscall already calls `vfs_close_all_current()` from.
pub fn register_exit_teardown_hook(hook: fn(u32)) {
    EXIT_TEARDOWN_HOOK.store(hook as *mut (), Ordering::Release);
}
