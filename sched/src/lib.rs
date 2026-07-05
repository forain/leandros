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
pub use signal::{check_and_deliver_signals, restore_signal_frame, sys_sigaction, sys_sigprocmask, sys_sigaltstack};
pub use futex::{futex_wait, futex_wake};

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

// ── Exit-code log ────────────────────────────────────────────────────────────

const EXIT_LOG_LEN: usize = 256;

#[derive(Clone, Copy)]
struct ExitRecord { pid: Pid, code: i32 }
static EXIT_LOG: Mutex<[Option<ExitRecord>; EXIT_LOG_LEN]> = Mutex::new([const { None }; EXIT_LOG_LEN]);
static EXIT_LOG_IDX: Mutex<usize> = Mutex::new(0);

fn log_exit(pid: Pid, code: i32) {
    let mut log = EXIT_LOG.lock();
    let mut idx = EXIT_LOG_IDX.lock();
    log[*idx] = Some(ExitRecord { pid, code });
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
    ret
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
pub fn current_gid()  -> u32 { RUN_QUEUE.lock().find_pid(current_pid()).map(|t| t.gid).unwrap_or(0) }
pub fn current_egid() -> u32 { egid_of(current_pid()) }

/// setuid(2) semantics: a privileged (euid==0) caller sets uid/euid unconditionally;
/// an unprivileged caller may only set euid to its current real or effective uid.
/// Returns false (⇒ EPERM) if the unprivileged case is violated.
pub fn set_current_uid(new_uid: u32) -> bool {
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        if t.euid == 0 {
            t.uid = new_uid;
            t.euid = new_uid;
            return true;
        }
        if new_uid == t.uid || new_uid == t.euid {
            t.euid = new_uid;
            return true;
        }
    }
    false
}

/// setgid(2) semantics — mirrors [`set_current_uid`] for the group identity.
pub fn set_current_gid(new_gid: u32) -> bool {
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        if t.euid == 0 {
            t.gid = new_gid;
            t.egid = new_gid;
            return true;
        }
        if new_gid == t.gid || new_gid == t.egid {
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
        t.state      = TaskState::Running;
        t.blocked_on = None;
    }
}

/// Complete a prepared block by yielding to the scheduler.
pub fn block_on_port_commit() {
    yield_now("block_on_port");
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

pub fn timer_tick_irq() {
    let id = unsafe { cpu_id() };
    // Every CPU has its own local timer; only the BSP advances global time so
    // TIMER_TICKS keeps its 100 Hz meaning regardless of CPU count.
    if id == 0 {
        TIMER_TICKS.fetch_add(1, Ordering::Relaxed);
    }
    PREEMPT_NEEDED[id.min(MAX_CPUS - 1)].store(true, Ordering::Relaxed);
}

pub fn preempt_check() {
    let id = unsafe { cpu_id() };
    if PREEMPT_NEEDED[id.min(MAX_CPUS - 1)].swap(false, Ordering::Relaxed) {
        yield_now("preempt");
    }
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

    let mut rq = RUN_QUEUE.lock();
    let tgid = match rq.find_pid(pid) {
        Some(t) => t.tgid,
        None => {
            print_str("[PF] find_pid failed\n");
            return false;
        }
    };
    if let Some(t) = rq.find_pid_mut(tgid) {
        if let Some(ref mut as_) = t.address_space {
            let ok = as_.handle_user_page_fault(addr, is_write);
            if !ok {
                print_str("[PF] handle_user_page_fault returned false\n");
            }
            return ok;
        } else {
            print_str("[PF] leader address_space is None\n");
        }
    } else {
        print_str("[PF] find_pid_mut for leader failed\n");
    }
    false
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
    task.address_space = Some(alloc::boxed::Box::new(as_));

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
                match rq.get_mut(idx) {
                    Some(t) if t.pid == pid => {
                        let delta = ticks().saturating_sub(dispatched_at);
                        t.charge_vruntime(delta);
                        t.on_cpu = None;
                        if t.state == TaskState::Running {
                            t.state = TaskState::Ready;
                        }
                        if t.state == TaskState::Zombie {
                            // Log the exit code BEFORE the slot disappears:
                            // a parent polling wait_pid on another CPU must
                            // always find the pid either in the queue or in
                            // EXIT_LOG — a gap between remove() and a later
                            // log_exit() makes waitpid return ECHILD.
                            log_exit(t.pid, t.exit_code);
                            rq.remove(idx)
                        } else {
                            None
                        }
                    }
                    _ => None,
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

pub fn exit(code: i32) -> ! {
    extern "C" {
        fn serial_print(s: *const u8, len: usize);
        fn print_number(n: u32);
    }
    let pid = current_pid();
    unsafe {
        let msg = b"[EXIT] pid=";
        serial_print(msg.as_ptr(), msg.len());
        print_number(pid);
        let msg2 = b" code=";
        serial_print(msg2.as_ptr(), msg2.len());
        print_number(code as u32);
        serial_print(b"\n".as_ptr(), 1);
    }
    {
        let mut rq = RUN_QUEUE.lock();
        if let Some(t) = rq.find_pid_mut(pid) {
            t.state = TaskState::Zombie;
            t.exit_code = code;
        }
    }
    yield_now("exit");
    loop { core::hint::spin_loop(); }
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
    {
        let mut rq = RUN_QUEUE.lock();
        if let Some(t) = rq.find_pid_mut(pid) {
            t.address_space = Some(alloc::boxed::Box::new(new_as));
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
        }
    }

    extern "C" {
        fn arch_execve_return(entry: usize, user_sp: usize) -> !;
    }

    unsafe {
        arch_set_page_table(pt_root);
        arch_execve_return(entry, user_sp);
    }
}

pub fn spawn_user(_entry_va: usize, _stack_va: usize, _priority: i8) -> Option<Pid> {
    None
}

pub fn with_address_space<F, R>(pid: Pid, f: F) -> Option<R>
where F: FnOnce(&mm::vmm::AddressSpace) -> R {
    let rq = RUN_QUEUE.lock();
    let t = rq.find_pid(pid)?;
    let leader = rq.find_pid(t.tgid)?;
    match leader.address_space {
        Some(ref as_) => Some(f(as_)),
        None => None,
    }
}

pub fn with_address_space_mut<F, R>(pid: Pid, f: F) -> Option<R>
where F: FnOnce(&mut mm::vmm::AddressSpace) -> R {
    let mut rq = RUN_QUEUE.lock();
    let tgid = rq.find_pid(pid)?.tgid;
    let leader = rq.find_pid_mut(tgid)?;
    match leader.address_space {
        Some(ref mut as_) => Some(f(as_)),
        None => None,
    }
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
    let pid = current_pid();
    let rq = RUN_QUEUE.lock();
    let t = rq.find_pid(pid)?;
    let leader = rq.find_pid(t.tgid)?;
    match leader.address_space {
        Some(ref as_) => Some(f(as_)),
        None => None,
    }
}

pub fn with_current_address_space_mut<F, R>(f: F) -> Option<R>
where F: FnOnce(&mut mm::vmm::AddressSpace) -> R {
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    let tgid = rq.find_pid(pid)?.tgid;
    let leader = rq.find_pid_mut(tgid)?;
    match leader.address_space {
        Some(ref mut as_) => Some(f(as_)),
        None => None,
    }
}

pub fn register_task_exit_hook(hook: fn(u32)) {
    TASK_EXIT_HOOK.store(hook as *mut (), Ordering::Release);
}
