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
pub mod lockwatch;
pub mod pcsample;
pub mod runqueue;
pub mod signal;
pub mod task;

pub use clone::{fork_current, clone_thread};
pub use signal::{check_and_deliver_signals, restore_signal_frame, sys_sigaction, sys_sigprocmask, sys_sigaltstack, has_deliverable_signal, reset_handlers_on_exec, fault_signal};
pub use futex::{futex_wait, futex_wake, futex_requeue};
pub use task::{SigInfo, SI_USER, SI_KERNEL, SI_TIMER, SI_TKILL, CLD_EXITED, CLD_KILLED, CLD_DUMPED,
               CLD_STOPPED, CLD_CONTINUED,
               SEGV_MAPERR, SEGV_ACCERR, BUS_ADRALN, BUS_ADRERR, ILL_ILLOPC, FPE_INTDIV, FPE_FLTINV,
               TRAP_BRKPT, TRAP_TRACE};

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;
use task::{Pid, Task, TaskState};
use context::CpuContext;
use runqueue::RunQueue;

static RUN_QUEUE:       lockwatch::TrackedMutex<RunQueue> =
    lockwatch::TrackedMutex::new(lockwatch::L_RUN_QUEUE, RunQueue::new());
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

/// Per-CPU preemption-disable nesting depth.  Non-zero means `preempt_check`
/// must leave `PREEMPT_NEEDED` SET and return without switching: the tick is
/// deferred, not lost.
///
/// This exists so a kernel wait loop can open interrupt windows — keeping the
/// 100 Hz tick, the tick hooks, poll deadlines, `nanosleep` and the audio pump
/// alive — *without* the window becoming a context-switch point.  That
/// distinction is load-bearing: `irq_window()` alone is already a scheduling
/// point, because `timer_irq` calls `preempt_check()` which calls `yield_now()`
/// even when the IRQ landed in kernel mode.  A loop that holds a `spin::Mutex`
/// and yields parks that mutex, and every other acquirer of it spins with IRQs
/// masked in syscall context and can never be descheduled — a hard hang on a
/// 1-vCPU guest.  `virtio_gpu::submit` is the first user.
static PREEMPT_DISABLE: [AtomicU32; MAX_CPUS] =
    [const { AtomicU32::new(0) }; MAX_CPUS];

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

/// How a task died, in the two pieces `wait4`/`waitid` have to report
/// separately: the value handed to `exit`/`exit_group`, and the signal that
/// terminated it (0 for a normal exit).
///
/// Both are needed because POSIX packs them into *one* `int` in a way that is
/// not a union of interchangeable values: `128 + signo` is the **shell's**
/// convention for surfacing a signal death in `$?`, not the kernel's
/// wait-status encoding. Storing only that (which is what the fatal-signal
/// path used to do) makes `WIFEXITED` read true and `WIFSIGNALED` read false
/// for every process killed by a signal — so `brush`'s job control, Rust's
/// `Command::status()`, and cosmic-term's `waitpid(WNOHANG)` reaper all see a
/// clean exit with an odd code instead of a kill.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    /// Value passed to `exit`/`exit_group`. For a signal death this is still
    /// `128 + signo`, kept for the serial exit log and `get_exit_code`; it is
    /// *not* what `wait_status()` reports in that case.
    pub code: i32,
    /// Terminating signal number, or 0 when the task exited normally.
    pub term_signal: u8,
}

impl ExitStatus {
    /// Placeholder for a report that carries no status at all — `waitid`'s
    /// WNOHANG "no state change" answer, whose `siginfo_t` is zeroed.
    pub const NONE: ExitStatus = ExitStatus { code: 0, term_signal: 0 };

    /// The POSIX packed wait status written through `wait4`'s `status` pointer.
    ///
    /// musl/Linux layout (relibc's `<sys/wait.h>` macros are the consumers):
    /// a normal termination is `(code & 0xff) << 8` with the low 7 bits zero,
    /// and a signal death puts `signo` in those low 7 bits — so
    /// `WIFEXITED(s)` is `(s & 0x7f) == 0` and `WIFSIGNALED(s)` is
    /// `((s & 0x7f) + 1) >> 1 > 0`.
    ///
    /// `WCOREDUMP`'s 0x80 bit is deliberately **never** set: it means "a core
    /// file was produced", and this kernel has no core-dump path at all. A
    /// process that reports a dump it did not write sends every consumer
    /// (shells printing "(core dumped)", CI harnesses collecting artifacts)
    /// looking for a file that does not exist. Revisit only if core dumping
    /// is ever implemented.
    pub fn wait_status(&self) -> i32 {
        if self.term_signal != 0 {
            (self.term_signal & 0x7f) as i32
        } else {
            (self.code & 0xff) << 8
        }
    }

    /// `siginfo_t.si_code` for the SIGCHLD report: `CLD_EXITED` (1) or
    /// `CLD_KILLED` (2). A `waitid` caller that switches on `si_code` instead
    /// of decoding the packed status must reach the same verdict as
    /// `wait_status()` — the two used to disagree, `si_code` being hardcoded
    /// to CLD_EXITED.
    pub fn si_code(&self) -> i32 {
        if self.term_signal != 0 { 2 } else { 1 }
    }

    /// `siginfo_t.si_status`: the exit code for a normal exit (matching
    /// `WEXITSTATUS`), the signal number for a kill (matching `WTERMSIG`).
    pub fn si_status(&self) -> i32 {
        if self.term_signal != 0 { self.term_signal as i32 } else { self.code & 0xff }
    }
}

impl SigInfo {
    /// The SIGCHLD payload for child process `pid` (real uid `uid`) ending with
    /// `status`.
    ///
    /// Defined here rather than beside the other constructors in `task.rs`
    /// because it must be *derived from* [`ExitStatus`] and not from a second
    /// reading of the same facts — see the call site in [`exit`].
    pub fn child(pid: Pid, uid: u32, status: ExitStatus) -> SigInfo {
        SigInfo {
            si_code:   status.si_code(),
            si_pid:    pid as i32,
            si_uid:    uid,
            si_status: status.si_status(),
            si_addr:   0,
        }
    }
}

const EXIT_LOG_LEN: usize = 256;

#[derive(Clone, Copy)]
struct ExitRecord {
    pid:  Pid,
    status: ExitStatus,
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

fn log_exit(pid: Pid, status: ExitStatus, parent_tgid: Pid, pgid: Pid, is_process: bool, consumed: bool) {
    let mut log = EXIT_LOG.lock();
    let mut idx = EXIT_LOG_IDX.lock();
    log[*idx] = Some(ExitRecord { pid, status, parent_tgid, pgid, is_process, consumed });
    *idx = (*idx + 1) % EXIT_LOG_LEN;
}

pub fn get_exit_code(pid: Pid) -> Option<i32> {
    let log = EXIT_LOG.lock();
    for entry in log.iter().filter_map(|e| e.as_ref()) {
        if entry.pid == pid { return Some(entry.status.code); }
    }
    None
}

// ── Kernel stacks ─────────────────────────────────────────────────────────────

/// Buddy order of a task's kernel stack: 2^5 = 32 pages = 128 KiB.
///
/// This lives in one place because it used to live in five, as a bare `4` next
/// to a bare `PAGE_SIZE * 16` — an allocation, a top-of-stack calculation and
/// two frees that all had to agree and that nothing checked.
///
/// 128 KiB rather than the 64 KiB it was through 2026-08-09. The stack has no
/// guard page: it is buddy-allocated physical memory reached through the HHDM,
/// so the frames underneath belong to other allocations and a frame that
/// outgrows the stack overwrites them without faulting. `f2fs::mount` did
/// exactly that with a 155,880 B frame — see TODO.md item 15 — and even after
/// that was fixed the largest legitimate frame in the kernel was 33,824 B,
/// i.e. 53 % of a 64 KiB stack consumed by a single frame with the whole call
/// chain below it still to pay for. `scripts/check-stack-frames.py` enforces
/// the ceiling that keeps this headroom real.
pub const KERNEL_STACK_ORDER: usize = 5;
/// Size of a task's kernel stack in bytes. Also its alignment: the buddy
/// allocator hands out naturally-aligned blocks, which is what lets
/// [`check_kernel_stack`] find the stack base from the stack pointer alone.
pub const KERNEL_STACK_SIZE: usize = mm::buddy::PAGE_SIZE << KERNEL_STACK_ORDER;

/// Written to the lowest word of every kernel stack at allocation and verified
/// on syscall return. An overflow walks down through it on its way into the
/// neighbouring allocation, so a mismatch means this task's frames went past
/// the bottom of its stack and corrupted memory that belongs to someone else.
const STACK_CANARY: u64 = 0x5354_4143_4B5F_4F4B; // "STACK_OK"

/// Stamp the canary into a freshly allocated kernel stack.
pub fn arm_kernel_stack(stack_base_phys: usize) {
    unsafe { (mm::phys_to_virt(stack_base_phys) as *mut u64).write(STACK_CANARY); }
}

/// Verify the canary of the kernel stack this call is running on.
///
/// Cheap enough for the syscall return path: one mask and one compare. The
/// mask is what makes it cheap — a naturally-aligned stack means the base is
/// just the stack pointer with the low bits cleared, so this needs no task
/// lookup and takes no lock. Returns `false` if the stack has been overrun.
///
/// Answers `true` when not running on a task kernel stack at all. The boot
/// stack is a `.bss` static in the kernel image — linked in the top-of-address
/// -space window, not the HHDM — with neither this alignment nor this canary,
/// so masking its stack pointer would read an unrelated word and report a
/// phantom overflow. The HHDM-window test is what distinguishes the two.
#[inline]
pub fn check_kernel_stack() -> bool {
    let probe = 0u64;
    let sp = &probe as *const u64 as usize;
    let hhdm = mm::phys_to_virt(0);
    // Task stacks are buddy frames reached through the HHDM, so they sit
    // within a physical-memory-sized window above its base. 1 TiB is far more
    // RAM than this kernel supports and far less than the ~128 TiB gap to the
    // kernel image, so it separates the two without needing the real size.
    if sp < hhdm || sp - hhdm >= (1usize << 40) { return true; }
    let base = sp & !(KERNEL_STACK_SIZE - 1);
    unsafe { (base as *const u64).read_volatile() == STACK_CANARY }
}

/// Report a blown kernel stack and kill the task that blew it.
///
/// Reported rather than ignored because the alternative is what item 15 cost:
/// the overflow lands in unrelated memory, and the failure surfaces later as
/// something with no visible connection to the stack — a page fault on a
/// different task, a corrupt page table, a boot that works on one host and not
/// another.
pub fn report_stack_overflow(pid: u32, syscall_no: usize) {
    extern "C" {
        fn arch_serial_putc(c: u8);
        fn print_hex(n: usize);
        fn print_number(n: u32);
    }
    fn s(msg: &str) { for &b in msg.as_bytes() { unsafe { arch_serial_putc(b) } } }
    s("\n*** KERNEL STACK OVERFLOW *** pid=");
    unsafe { print_number(pid) };
    s(" syscall=");
    unsafe { print_hex(syscall_no) };
    s("\n  A frame ran past the bottom of this task's ");
    unsafe { print_hex(KERNEL_STACK_SIZE) };
    s(" B kernel stack and\n  overwrote the allocation below it. Memory is already corrupt; the\n  task is being killed so the damage stops here. Run\n  scripts/check-stack-frames.py on the kernel to find the frame.\n");
}

// ── Context switching ─────────────────────────────────────────────────────────

pub const MAX_CPUS: usize = 8;
static mut SCHEDULER_CTX: [CpuContext; MAX_CPUS] = [const { CpuContext::zeroed() }; MAX_CPUS];
static mut CURRENT_CTX:   [*mut CpuContext; MAX_CPUS] = [core::ptr::null_mut(); MAX_CPUS];
/// PID running on each CPU (0 = idle / in scheduler).  Atomic because
/// `wake_up_an_idle_cpu` reads other CPUs' slots to find an idle target.
static CURRENT_PID: [AtomicU32; MAX_CPUS] =
    [const { AtomicU32::new(0) }; MAX_CPUS];
/// TGID of the task running on each CPU (0 = idle / in scheduler), published
/// by the dispatch path alongside [`CURRENT_PID`].
///
/// Exists purely so [`current_tgid`] does not have to call [`tgid_of`], which
/// takes the global `RUN_QUEUE` lock and linear-scans up to `MAX_TASKS` (256)
/// slots. That scan sits on hot syscall paths — the epoll ownership check runs
/// it on every `epoll_wait` — so four vCPUs end up contending the one lock to
/// answer a question about themselves. Profiling a live COSMIC session put
/// `tgid_of` at 10.6% of running guest CPU.
///
/// Safe to cache because a `Task`'s `tgid` is assigned once at creation and
/// never reassigned; only the pid→tgid lookup for *another* task still needs
/// the scan. The one exception — a non-leader `execve` whose leader is
/// already gone, promoted in `take_over_leader` — re-publishes both this
/// slot and the side table in the same `RUN_QUEUE` hold. (The ordinary
/// non-leader `execve` changes the thread's *pid*, never its tgid.)
static CURRENT_TGID: [AtomicU32; MAX_CPUS] =
    [const { AtomicU32::new(0) }; MAX_CPUS];
/// Address space of the task running on each CPU (its own `Arc`'s pointee;
/// null = none / not dispatched), published at dispatch, cleared at
/// switch-back and re-published by `replace_address_space` (execve). Lets
/// `lock_leader_address_space` serve the running task's own page faults and
/// mm syscalls without RUN_QUEUE: that site was ~296 k RUN_QUEUE holds/s
/// (8 % of a CPU held, the top site by far) while a COSMIC session starts.
///
/// Why the pointer stays valid while it is published: it points into the
/// `Arc<AddressSpace>` held by the task running on this CPU. A running task
/// is never reaped (reaping happens at its own switch-back), nothing else
/// ever drops or replaces a task's `address_space` except that task's own
/// `execve`, which updates this slot in the same RUN_QUEUE hold, and
/// syscalls / fault handlers run with IRQs masked, so the reader cannot
/// migrate between reading `cpu_id()` and using the slot.
static CURRENT_AS: [core::sync::atomic::AtomicPtr<mm::vmm::AddressSpace>; MAX_CPUS] =
    [const { core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()) }; MAX_CPUS];
/// Reply port of the task running on each CPU (`u32::MAX` = none yet),
/// published at dispatch and by [`set_current_reply_port`], so
/// [`current_reply_port`] — the first step of every synchronous server call —
/// does not take `RUN_QUEUE` and scan for its own task. Safe for the same
/// reason as [`CURRENT_TGID`]: only the task itself ever changes its
/// `reply_port`, and it does so through `set_current_reply_port`.
static CURRENT_REPLY_PORT: [AtomicU32; MAX_CPUS] =
    [const { AtomicU32::new(u32::MAX) }; MAX_CPUS];

extern "C" {
    fn arch_set_page_table(root: usize);
    /// Detach this CPU from any task page table (x86-64: reload the boot
    /// kernel CR3; AArch64: clear TTBR0).  Called after every switch-back so
    /// an exited task's freed page tables are never left live on a CPU.
    fn arch_load_kernel_page_table();
    fn arch_set_kernel_stack(rsp: u64);
    fn arch_cpu_id() -> usize;
    fn arch_timer_check_alive() -> bool;
    /// Arm this CPU's one-shot timer for the absolute `monotonic_ns()` instant
    /// `deadline_ns` if it is sooner than whatever is armed already.
    fn arch_timer_arm_deadline(deadline_ns: u64);
    /// Monotonic nanoseconds since boot (sub-tick), for CPU-time accounting.
    fn arch_monotonic_ns() -> u64;
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

/// Number of CPUs that have entered the scheduler (BSP + booted APs),
/// clamped to `MAX_CPUS` and to at least 1.
///
/// Callers outside this module (e.g. `sys_sched_getaffinity`) must not call
/// the raw `arch_active_cpu_count()` extern directly — this wrapper is the
/// safe, clamped entry point.
pub fn active_cpu_count() -> usize {
    let n = unsafe { arch_active_cpu_count() };
    n.clamp(1, MAX_CPUS)
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

/// A task being reaped while it owns the fork quiesce (it was killed by a
/// sibling's `exit_group` in the middle of its own fork) must release it,
/// or no process on the machine ever forks again — the CAS in
/// `quiesce_thread_group` would spin on a slot whose owner no longer exists.
fn release_quiesce_if_owner(tgid: Pid, pid: Pid) {
    if QUIESCE_TGID.load(Ordering::Acquire) == tgid
        && QUIESCE_EXCEPT.load(Ordering::Acquire) == pid
    {
        unquiesce_thread_group();
    }
}

pub fn alloc_pid() -> Pid {
    let mut pid_guard = NEXT_PID.lock();
    let pid = *pid_guard;
    *pid_guard += 1;
    pid
}

/// The highest pid handed out so far (`/proc/loadavg`'s last field), so a
/// userspace scan of `/proc/<pid>/` knows where to stop.
pub fn last_pid() -> Pid {
    NEXT_PID.lock().saturating_sub(1)
}

/// The userspace init process — the reaper every orphan is handed to. 0 until
/// `kernel::init` has spawned it; nothing is reparented before then.
static INIT_PID: AtomicU32 = AtomicU32::new(0);

pub fn set_init_pid(pid: Pid) { INIT_PID.store(pid, Ordering::Release); }
pub fn init_pid() -> Pid { INIT_PID.load(Ordering::Acquire) }

/// POSIX orphan reparenting: every child process of the dying thread group
/// `dead_tgid` becomes a child of init, so init's `wait4(-1)` reaps it when
/// it exits and — the reason this matters here — init can *see* it while it
/// lives. Until 2026-09-18 an orphan kept its dead parent's pid: its exit
/// record then named a parent that would never wait, and a supervisor had no
/// way to tell a stray from a dead subtree apart from anything else. That is
/// how every `cosmic-greeter` outlived its compositor, spinning on a broken
/// Wayland socket at ~180 MiB apiece, and what looked like a kernel leak per
/// compositor death.
///
/// Parentage is matched the way `wait_scan` matches it — through the parent
/// thread's group — and only process leaders are moved; a thread's `ppid`
/// names the sibling that created it and is not parentage at all. Must run
/// while the dying group's threads are still on the run queue (before the
/// group kill reaps them), or a child forked by a non-leader thread cannot be
/// resolved to the group any more.
fn reparent_children(dead_tgid: Pid) {
    let init = init_pid();
    if init == 0 || init == dead_tgid { return; }
    let mut rq = RUN_QUEUE.lock();
    let mut moved: [Pid; runqueue::MAX_TASKS] = [0; runqueue::MAX_TASKS];
    let mut n = 0usize;
    for i in 0..runqueue::MAX_TASKS {
        let (pid, tgid, ppid) = match rq.get(i) { Some(t) => (t.pid, t.tgid, t.ppid), None => continue };
        if pid != tgid || pid == dead_tgid { continue; }
        let parent_tgid = rq.find_pid(ppid).map(|p| p.tgid).unwrap_or(ppid);
        if parent_tgid == dead_tgid { moved[n] = pid; n += 1; }
    }
    for &pid in &moved[..n] {
        if let Some(t) = rq.find_pid_mut(pid) { t.ppid = init; }
    }
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

/// `(sid, pgid)` of the calling *process* in one run-queue acquisition — the
/// pair every terminal job-control check needs ("is this my controlling
/// terminal, and am I its foreground group?").
///
/// Read from the thread-group leader, not the calling thread. `setsid`/
/// `setpgid` update the calling task only, so a worker thread of a threaded
/// shell (brush's tokio runtime) can carry a stale `pgid` from before the
/// shell moved itself into its own group; judging it by that would let its
/// own terminal stop it with SIGTTIN. Process groups and sessions are
/// process attributes, and the leader is where every other consumer
/// (`kill_pgrp`, wait4's pgid match) reads them.
pub fn current_sid_pgid() -> (Pid, Pid) {
    let pid = current_pid();
    let rq = RUN_QUEUE.lock();
    let tgid = match rq.find_pid(pid) { Some(t) => t.tgid, None => return (0, 0) };
    rq.find_pid(tgid).map(|l| (l.sid, l.pgid)).unwrap_or((0, 0))
}

/// True when some live process (leader) of group `pgid` belongs to session
/// `sid` — the `tcsetpgrp(3)` precondition (EPERM otherwise): a terminal's
/// foreground group must be a group of the session it controls.
pub fn pgrp_in_session(pgid: Pid, sid: Pid) -> bool {
    if pgid == 0 { return false; }
    let rq = RUN_QUEUE.lock();
    for i in 0..runqueue::MAX_TASKS {
        if let Some(t) = rq.get(i) {
            if t.pid == t.tgid && t.pgid == pgid && t.sid == sid
                && t.state != TaskState::Zombie {
                return true;
            }
        }
    }
    false
}

/// `(ppid, pgid, sid, state)` of process `pid` for `/proc/<pid>/stat`, with
/// the state as its procps letter: `R` runnable, `S` sleeping on a wait
/// channel, `T` job-control stopped, `Z` zombie. A stop is reported from the
/// leader's `stop_signal` as well as the `Stopped` task state, so a threaded
/// process whose leader happens to be mid-park still reads `T`.
pub fn proc_stat_of(pid: Pid) -> Option<(Pid, Pid, Pid, u8)> {
    let rq = RUN_QUEUE.lock();
    let t = rq.find_pid(pid)?;
    let leader_stopped = rq.find_pid(t.tgid).map(|l| l.stop_signal != 0).unwrap_or(false);
    let letter = match t.state {
        TaskState::Zombie  => b'Z',
        TaskState::Stopped => b'T',
        _ if leader_stopped => b'T',
        TaskState::Blocked => b'S',
        TaskState::Ready | TaskState::Running => b'R',
    };
    Some((t.ppid, t.pgid, t.sid, letter))
}

/// The thread-group id of `pid`, or `pid` itself if it's not a live task
/// (matches every task's own fallback of being its own tgid at creation).
// ── pid → tgid side table ────────────────────────────────────────────────────
//
// `tgid_of` used to be `RUN_QUEUE.lock().find_pid(pid)`: the global scheduler
// spinlock plus a linear scan of all `MAX_TASKS` slots, to answer a question
// about one task. It is on the hot syscall path — profiling a live COSMIC
// session put it at **10.3% of running guest CPU**, rising to **21.7%** once GPU
// acceleration raised the syscall rate — and the lock it takes is the one every
// CPU needs to schedule, so the cost was contention as much as scanning.
//
// This is the same shape as the tgid-keyed exe-path table (`0aefc36`): a fixed
// open-addressed table, written only under the RUN_QUEUE lock (so writes are
// already serialised by the scheduler) and read lock-free.
//
// Sizing: 1024 slots against `runqueue::MAX_TASKS` = 256, so the table never
// exceeds a quarter load and linear probes stay short. pids are allocated
// sequentially from 1, so `pid & (SLOTS-1)` distributes perfectly — the
// pathological clustering open addressing is usually warned about needs a
// clumped key space, and this one is a counter.
const PID_TGID_SLOTS: usize = 1024;
/// Packed `(pid << 32) | tgid`. `0` = never used, `u64::MAX` = tombstone.
///
/// A tombstone rather than a zero on removal is load-bearing: zeroing a slot in
/// the middle of a probe chain would strand every later entry in that chain,
/// and the failure would be a *wrong tgid* (the `unwrap_or(pid)` fallback), not
/// a crash — precisely the silent kind. pid 0 is never allocated (`NEXT_PID`
/// starts at 1) and `u32::MAX` is not reachable, so neither sentinel collides
/// with a real key.
static PID_TGID: [AtomicU64; PID_TGID_SLOTS] =
    [const { AtomicU64::new(0) }; PID_TGID_SLOTS];
const PID_TGID_TOMBSTONE: u64 = u64::MAX;

#[inline]
fn pid_tgid_slot(pid: Pid) -> usize { (pid as usize) & (PID_TGID_SLOTS - 1) }

/// Publish `pid → tgid`. Call sites hold the RUN_QUEUE lock.
pub(crate) fn pid_tgid_insert(pid: Pid, tgid: Pid) {
    if pid == 0 { return; }
    let want = ((pid as u64) << 32) | tgid as u64;
    let start = pid_tgid_slot(pid);
    for i in 0..PID_TGID_SLOTS {
        let s = (start + i) & (PID_TGID_SLOTS - 1);
        let cur = PID_TGID[s].load(Ordering::Relaxed);
        // Free, recyclable, or already ours (a re-registration).
        if cur == 0 || cur == PID_TGID_TOMBSTONE || (cur >> 32) as u32 == pid {
            PID_TGID[s].store(want, Ordering::Release);
            return;
        }
    }
    // Full: leave the table alone. `tgid_of` then falls back to the scan, which
    // is slow but correct — never wrong.
}

/// Retire `pid`. Call sites hold the RUN_QUEUE lock.
pub(crate) fn pid_tgid_remove(pid: Pid) {
    if pid == 0 { return; }
    let start = pid_tgid_slot(pid);
    for i in 0..PID_TGID_SLOTS {
        let s = (start + i) & (PID_TGID_SLOTS - 1);
        let cur = PID_TGID[s].load(Ordering::Relaxed);
        if cur == 0 { return; }                       // end of chain, not present
        if cur != PID_TGID_TOMBSTONE && (cur >> 32) as u32 == pid {
            PID_TGID[s].store(PID_TGID_TOMBSTONE, Ordering::Release);
            return;
        }
    }
}

/// O(1) average lookup, no locks.
fn pid_tgid_get(pid: Pid) -> Option<Pid> {
    let start = pid_tgid_slot(pid);
    for i in 0..PID_TGID_SLOTS {
        let s = (start + i) & (PID_TGID_SLOTS - 1);
        let cur = PID_TGID[s].load(Ordering::Acquire);
        if cur == 0 { return None; }                  // end of chain
        if cur != PID_TGID_TOMBSTONE && (cur >> 32) as u32 == pid {
            return Some(cur as u32);
        }
    }
    None
}

/// The thread-group id of `pid`, or `pid` itself if no such task is live.
///
/// The fallback is not cosmetic — callers depend on it. See the `owner_tgid`
/// note in `sys_epoll_ctl`: resolving a *dead* thread's tgid this way is what
/// made an epoll fd fail EBADF for its surviving siblings, so anything relying
/// on `tgid_of` for a possibly-exited pid gets the same answer it always did.
pub fn tgid_of(pid: Pid) -> Pid {
    if let Some(t) = pid_tgid_get(pid) { return t; }
    // Miss: either the task is gone, or the table was full when it registered.
    // Fall back to the authoritative scan rather than guessing.
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.tgid).unwrap_or(pid)
}

pub fn current_tgid() -> Pid {
    // Fast path: the dispatch path published this CPU's tgid, so answering
    // "what thread group am I in" costs an atomic load instead of the global
    // RUN_QUEUE lock plus a 256-slot scan. See [`CURRENT_TGID`].
    let cached = CURRENT_TGID[unsafe { cpu_id() }].load(Ordering::Relaxed);
    if cached != 0 { return cached; }
    // 0 means "no task on this CPU" — the scheduler context itself, or a call
    // made before the first dispatch. Fall back rather than report tgid 0.
    tgid_of(current_pid())
}

pub fn ticks() -> u64 {
    TIMER_TICKS.load(Ordering::Relaxed)
}

/// CLOCK_MONOTONIC in nanoseconds: the architecture's free-running counter
/// (CNTVCT_EL0 / TSC) against the boot epoch, sub-tick, never decreasing.
///
/// This is THE monotonic source for every timed wait in the kernel. Every
/// `poll_deadline`, `NEXT_POLL_DEADLINE`, timerfd deadline and futex timeout
/// is an absolute reading of this clock, and `service_poll_deadlines` compares
/// them against it on each tick. Deadlines used to be tick counts: a relative
/// timeout was floored to whole ticks (so any wait under 10 ms became 0 and
/// returned at once) and a deadline of "tick N" was released at tick N's edge
/// — up to 10 ms before the requested interval had elapsed on the clock
/// userspace measures with. A deadline in nanoseconds keeps the POSIX "at
/// least this long" guarantee; the 100 Hz tick still bounds how *late* a wake
/// can be (≤ one tick), not how early.
#[inline]
pub fn monotonic_ns() -> u64 {
    unsafe { arch_monotonic_ns() }
}

/// `CLOCK_REALTIME - CLOCK_MONOTONIC`, in nanoseconds (signed: a wall clock
/// stepped backwards below the boot instant is representable). 0 until
/// `set_realtime_offset_ns` runs at boot (from the board's battery clock) or
/// `clock_settime`/`settimeofday` steps the wall clock, so on a board with no
/// RTC the realtime epoch is the boot instant.
static REALTIME_OFFSET_NS: core::sync::atomic::AtomicI64 = core::sync::atomic::AtomicI64::new(0);

/// CLOCK_REALTIME in nanoseconds since the Unix epoch: the ONE wall clock.
/// `clock_gettime(CLOCK_REALTIME)`, `gettimeofday`, `time` and every
/// CLOCK_REALTIME deadline (timerfd, clock_nanosleep, FUTEX_CLOCK_REALTIME)
/// derive from this, so they can never drift apart — they used to be two
/// clocks (`ticks × 10 ms` vs the counter), up to 10 ms apart.
#[inline]
pub fn realtime_ns() -> u64 {
    let off = REALTIME_OFFSET_NS.load(Ordering::Relaxed);
    (monotonic_ns() as i64).saturating_add(off).max(0) as u64
}

pub fn realtime_offset_ns() -> i64 { REALTIME_OFFSET_NS.load(Ordering::Relaxed) }

/// Step the wall clock so that CLOCK_REALTIME reads `realtime_ns` NOW.
pub fn set_realtime_ns(realtime_ns: u64) {
    let off = (realtime_ns as i64).saturating_sub(monotonic_ns() as i64);
    REALTIME_OFFSET_NS.store(off, Ordering::Relaxed);
}

/// Convert an absolute CLOCK_REALTIME instant into the monotonic reading the
/// wait machinery compares against (saturating at 0 = already due).
#[inline]
pub fn realtime_to_monotonic_ns(realtime_ns: u64) -> u64 {
    let off = REALTIME_OFFSET_NS.load(Ordering::Relaxed);
    (realtime_ns as i64).saturating_sub(off).max(0) as u64
}

/// Nanosecond clock source registered by the kernel at boot, used to stamp
/// filesystem inode times (UTIME_NOW etc.) so `stat` agrees with what
/// `clock_gettime(CLOCK_REALTIME)` hands userspace. The kernel wires this to
/// [`realtime_ns`] (`kernel::syscall::clock_ns_for_sched`) — this indirection
/// exists only so `servers/vfs` and `servers/f2fs`, which sit on the other
/// side of the syscall boundary, can read the same wall clock without
/// depending on `sched` directly. Falls back to the 10 ms tick until
/// registered.
static CLOCK_NS: AtomicUsize = AtomicUsize::new(0);

pub fn register_clock_ns(f: fn() -> u64) {
    CLOCK_NS.store(f as usize, Ordering::Release);
}

/// CLOCK_REALTIME in nanoseconds, as seen by filesystem timestamp callers.
pub fn clock_ns() -> u64 {
    let f = CLOCK_NS.load(Ordering::Acquire);
    if f != 0 {
        let f: fn() -> u64 = unsafe { core::mem::transmute(f) };
        f()
    } else {
        ticks() * 10_000_000
    }
}

/// `(sec, nsec)` of [`clock_ns`] — the shape a `struct timespec` wants.
pub fn clock_ts() -> (i64, i64) {
    let ns = clock_ns();
    ((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64)
}

/// Deliver `signo` to the single *thread* `pid`, carrying `info` as its
/// `siginfo_t` payload.
///
/// `info` is not optional and has no default on purpose: every producer has to
/// answer "where did this signal come from?", because the pre-`SigInfo`
/// behaviour — a zeroed payload — is indistinguishable from a genuine
/// `SI_USER` `kill()` and silently misreports every other origin.
pub fn deliver_signal(pid: Pid, signo: u32, info: task::SigInfo) -> isize {
    let mut woke = false;
    let mut resumed = None;
    let ret = {
        let mut rq = RUN_QUEUE.lock();
        if signo > 0 && signo <= 64 {
            if let Some(tgid) = rq.find_pid(pid).map(|t| t.tgid) {
                let (made_ready, r) = on_signal_generated(&mut rq, tgid, signo);
                resumed = r;
                woke |= made_ready;
            }
        }
        let min_vr = rq.min_vruntime();
        if let Some(t) = rq.find_pid_mut(pid) {
            if signo > 0 && signo <= 64 {
                // First-writer-wins: POSIX keeps exactly one pending instance
                // of a standard signal and discards later ones, so a second
                // arrival must not repaint the payload of the instance already
                // queued — the handler is only going to run once, for the
                // first one.
                if t.signal_pending & (1 << (signo - 1)) == 0 {
                    t.signal_info[(signo - 1) as usize] = info;
                }
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
    if let Some((tgid, ppid, uid)) = resumed { notify_continued(tgid, ppid, uid); }
    // A signalfd registered in this tgid may be parked in epoll_wait; the
    // new pending bit is a readiness edge for it. RUN_QUEUE is released above.
    wake_poll();
    ret
}

/// Job-control side effects of *generating* `signo` for thread group `tgid`,
/// applied under the RUN_QUEUE lock before the signal is queued.
///
/// * SIGCONT and SIGKILL make every `Stopped` thread of the group `Ready`
///   again and withdraw any outstanding stop request (`stop_pending`), so a
///   stopped process can be resumed or killed. SIGCONT additionally
///   discards pending stop signals (POSIX) and, if the group *was* stopped,
///   arms the `WIFCONTINUED` report and asks the caller to notify the
///   parent — returned as `Some((tgid, ppid, uid))` because the SIGCHLD has
///   to be sent after this lock is released.
/// * A stop signal discards a pending SIGCONT (the mirror rule).
///
/// The continue is performed at *send* time, not when the signal is
/// dequeued: a stopped thread never returns to user space, so it could never
/// dequeue the very signal that is supposed to wake it.
///
/// Returns `(made_ready, resumed)`: `made_ready` is true when at least one
/// `Stopped` thread became `Ready` (SIGCONT *or* SIGKILL), so the caller
/// kicks an idle CPU — the thread it goes on to target is no longer
/// `Blocked`, so the ordinary wake path would not.
fn on_signal_generated(rq: &mut runqueue::RunQueue, tgid: Pid, signo: u32)
    -> (bool, Option<(Pid, Pid, u32)>)
{
    let bit = 1u64 << (signo - 1);
    if signo == signal::SIGCONT || signo == signal::SIGKILL {
        let min_vr = rq.min_vruntime();
        let mut made_ready = false;
        for i in 0..runqueue::MAX_TASKS {
            if let Some(t) = rq.get_mut(i) {
                if t.tgid != tgid { continue; }
                t.stop_pending = false;
                if signo == signal::SIGCONT { t.signal_pending &= !signal::SIGDFL_STOP; }
                if t.state == TaskState::Stopped {
                    t.state = TaskState::Ready;
                    signal::clear_block_fields(t);
                    t.place(min_vr);
                    made_ready = true;
                }
            }
        }
        let mut resumed = None;
        if let Some(l) = rq.find_pid_mut(tgid) {
            if signo == signal::SIGCONT { l.shared_signal_pending &= !signal::SIGDFL_STOP; }
            if l.stop_signal != 0 {
                l.stop_signal   = 0;
                l.stop_reported = false;
                if signo == signal::SIGCONT {
                    l.cont_pending = true;
                    resumed = Some((tgid, l.ppid, l.uid));
                }
            }
        }
        return (made_ready, resumed);
    }
    if signal::SIGDFL_STOP & bit != 0 {
        let cont = 1u64 << (signal::SIGCONT - 1);
        for i in 0..runqueue::MAX_TASKS {
            if let Some(t) = rq.get_mut(i) {
                if t.tgid == tgid { t.signal_pending &= !cont; }
            }
        }
        if let Some(l) = rq.find_pid_mut(tgid) { l.shared_signal_pending &= !cont; }
    }
    (false, None)
}

/// SIGCHLD/CLD_CONTINUED to the parent of a just-resumed process. No locks
/// held.
fn notify_continued(tgid: Pid, ppid: Pid, uid: u32) {
    signal::notify_parent_state_change(tgid, ppid, uid,
        SigInfo::child_state(task::CLD_CONTINUED, tgid, uid, signal::SIGCONT));
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
pub fn deliver_signal_process(tgid: Pid, signo: u32, info: task::SigInfo) -> isize {
    if signo == 0 || signo > 64 { return -22; }
    let (ret, woke, resumed) = post_signal_process(&mut RUN_QUEUE.lock(), tgid, signo, info);
    if woke { wake_up_an_idle_cpu(); }
    if let Some((tgid, ppid, uid)) = resumed { notify_continued(tgid, ppid, uid); }
    // Wake any signalfd poller in the target tgid (RUN_QUEUE released above).
    wake_poll();
    ret
}

/// `deliver_signal_process` for IRQ context (a POSIX-timer / itimer expiry
/// serviced from the timer interrupt): bounded `try_lock_spin`, `None` when
/// RUN_QUEUE stayed contended (nothing was posted — the caller retries). The
/// poll-channel broadcast rides the same lock hold, which is what releases a
/// thread parked in pause/sigsuspend/sigtimedwait/nanosleep/poll. SIGCONT is
/// refused (`None`): its parent notification needs task context.
///
/// `Some(1)` = an instance was already pending somewhere in the group, so this
/// one coalesced into it (a POSIX timer counts that as an overrun).
pub fn try_deliver_signal_process(tgid: Pid, signo: u32, info: task::SigInfo) -> Option<isize> {
    if signo == 0 || signo > 64 { return Some(-22); }
    if signo == signal::SIGCONT { return None; }
    let bit = 1u64 << (signo - 1);
    let mut rq = RUN_QUEUE.try_lock_spin(TICK_LOCK_WAIT_NS)?;
    let already = (0..runqueue::MAX_TASKS).any(|i| rq.get(i).map_or(false, |t|
        t.tgid == tgid && (t.signal_pending | t.shared_signal_pending) & bit != 0));
    let (ret, woke, _) = post_signal_process(&mut rq, tgid, signo, info);
    let ret = if ret == 0 && already { 1 } else { ret };
    let woken = rq.unblock_port_tagged(POLL_WAIT_CHANNEL, POLL_TAG_ALL);
    drop(rq);
    if woke || woken > 0 { wake_up_an_idle_cpu(); }
    Some(ret)
}

/// The RUN_QUEUE-held half of `deliver_signal_process`: returns
/// `(ret, woke, resumed)` for the caller to act on after unlocking.
fn post_signal_process(rq: &mut runqueue::RunQueue, tgid: Pid, signo: u32, info: task::SigInfo)
    -> (isize, bool, Option<(Pid, Pid, u32)>)
{
    let bit = 1u64 << (signo - 1);
    let idx = (signo - 1) as usize;
    let mut woke = false;
    // SIGKILL and SIGSTOP cannot be blocked, so the "does this thread have it
    // unmasked?" test must not gate their delivery. `sys_sigprocmask` already
    // refuses to set these bits, but a stale mask (or a future path that sets
    // `signal_mask` directly) must not be able to strand them on the shared
    // pending set: for these two, any live thread in the group is a valid
    // target. Belt and braces on the one pair of signals that must never fail.
    let unblockable = signal::UNBLOCKABLE & bit != 0;
    let resumed;
    let ret = {
        // SIGCONT/SIGKILL: resume a stopped group first, so the thread chosen
        // below is runnable and can actually take the signal.
        let (made_ready, r) = on_signal_generated(rq, tgid, signo);
        resumed = r;
        woke |= made_ready;
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
            Some(slot) => {
                // A thread can take it now: deliver directly, waking it if blocked.
                if let Some(t) = rq.get_mut(slot) {
                    // Same first-writer-wins rule as `deliver_signal`.
                    if t.signal_pending & bit == 0 { t.signal_info[idx] = info; }
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
                    Some(li) => {
                        if let Some(l) = rq.get_mut(li) {
                            // The payload rides on the leader's own slot, which
                            // is the same array the leader's *thread-local*
                            // pending signals use. That is safe because the
                            // 0 → 1 test spans both bitmasks: if the leader
                            // already has this signal pending in either sense,
                            // this arrival is a duplicate of an already-pending
                            // standard signal and POSIX discards it, payload
                            // included. `check_and_deliver_signals` copies the
                            // slot to whichever thread claims the bit.
                            if (l.signal_pending | l.shared_signal_pending) & bit == 0 {
                                l.signal_info[idx] = info;
                            }
                            l.shared_signal_pending |= bit;
                        }
                        0
                    }
                    None => -3, // ESRCH
                }
            }
        }
    };
    (ret, woke, resumed)
}

/// Mark a CLONE_VFORK child as done borrowing the parent's address space
/// (called at successful execve and at exit) — releases the parent from its
/// vfork suspension loop in `clone_thread`.
pub fn vfork_complete(pid: Pid) {
    let mut rq = RUN_QUEUE.lock();
    let was = match rq.find_pid_mut(pid) {
        Some(t) => core::mem::replace(&mut t.vfork_pending, false),
        None => false,
    };
    let woken = if was { rq.unblock_port(VFORK_WAIT_CHANNEL) } else { 0 };
    drop(rq);
    if woken > 0 { wake_up_an_idle_cpu(); }
}

/// Wait-channel a CLONE_VFORK parent parks on while its child borrows the
/// address space (`clone.rs`). Released by `vfork_complete` (the child's
/// successful execve) and by the child's exit paths; like `POLL_WAIT_CHANNEL`
/// it is outside the real port-id range. Before 2026-09-16 the parent
/// yield-spun instead, one full CPU for every spawn's exec window.
pub const VFORK_WAIT_CHANNEL: u32 = 0xFFFF_FF02;

/// kill(0-probe): 0 if `pid` names a live task, else -ESRCH.
pub fn exists_probe(pid: Pid) -> isize {
    if RUN_QUEUE.lock().find_pid(pid).is_some() { 0 } else { -3 }
}

/// Deliver `signo` to every *process* (thread-group leader) in process group
/// `pgid` — kill(-pgid) / killpg semantics. One delivery per process; the
/// per-thread routing happens at handler-delivery time via the shared
/// TGID action table.
pub fn kill_pgrp(pgid: Pid, signo: u32, info: task::SigInfo) -> isize {
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
        let _ = deliver_signal_process(pid, signo, info);
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

/// Park `sigsuspend(2)`'s caller mask for the delivery pass on this syscall's
/// return (see `Task::saved_sigmask`).
pub fn stash_sigsuspend_mask(old_mask: u64) {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        t.saved_sigmask = Some(old_mask);
    }
}

pub fn pending_signals() -> u64 {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.signal_pending).unwrap_or(0)
}

/// Accept the pending instance of `signo` for the calling task the way
/// `signalfd`'s `read()` and `sigtimedwait(2)` do — consume the pending bit
/// *at whichever level it is parked* and hand back the payload it carried.
///
/// This replaced a `clear_pending_signal` that returned nothing and cleared
/// only the thread-local bit. Returning the [`task::SigInfo`] is the point,
/// but the level matters just as much: the signalfd read path reports
/// `pending_signals() | shared_pending_signals()`, so a process-directed
/// signal parked on the leader — which is exactly where a child-exit SIGCHLD
/// lands in a single-threaded process that has blocked it, i.e. the ordinary
/// signalfd case — was reported again on every read, forever, because the
/// shared half was never cleared. Two functions answering "consume this
/// signal" is how that happened; there is now one.
pub fn accept_pending_signal(signo: u32) -> task::SigInfo {
    if signo == 0 || signo > 64 { return task::SigInfo::NONE; }
    let bit = 1u64 << (signo - 1);
    let idx = (signo - 1) as usize;
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    let tgid = match rq.find_pid(pid) { Some(t) => t.tgid, None => return task::SigInfo::NONE };

    // Prefer the thread's own slot; fall back to the leader's parked one. When
    // the caller *is* the leader these are the same slot, which is why the
    // "which level" question only has to be answered for the bits, not the
    // payload.
    let mine = rq.find_pid(pid).map(|t| t.signal_pending & bit != 0).unwrap_or(false);
    let info = if mine {
        rq.find_pid(pid).map(|t| t.signal_info[idx]).unwrap_or(task::SigInfo::NONE)
    } else {
        rq.find_pid(tgid).map(|l| l.signal_info[idx]).unwrap_or(task::SigInfo::NONE)
    };

    if let Some(i) = rq.find_pid_idx(pid) {
        if let Some(t) = rq.get_mut(i) { t.signal_pending &= !bit; }
    }
    if let Some(i) = rq.find_pid_idx(tgid) {
        if let Some(l) = rq.get_mut(i) { l.shared_signal_pending &= !bit; }
    }
    info
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
    let id = unsafe { cpu_id() }.min(MAX_CPUS - 1);
    if CURRENT_PID[id].load(Ordering::Relaxed) == 0 { return u32::MAX; }
    CURRENT_REPLY_PORT[id].load(Ordering::Relaxed)
}

pub fn set_current_reply_port(port: u32) {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        t.reply_port = port;
    }
    let id = unsafe { cpu_id() }.min(MAX_CPUS - 1);
    CURRENT_REPLY_PORT[id].store(port, Ordering::Relaxed);
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

/// setpgid(2) core: move `pid` into process group `pgid`. Returns false if no
/// such process exists (ESRCH).
///
/// An exited-but-not-yet-waited-for child counts as existing, as it does on
/// Linux, where the zombie's task_struct lives until it is reaped. This
/// kernel takes a zombie off the run queue the moment it dies and keeps only
/// its `EXIT_LOG` record, so the group has to be rewritten there: the
/// job-control idiom `fork(); setpgid(child, child); waitpid(-child, ..)` is
/// otherwise a race the parent loses whenever the child exits first (ESRCH
/// from setpgid, or ECHILD from the wait because the record still carries
/// the old group). Lock order is RUN_QUEUE then EXIT_LOG, as in `wait_scan`.
pub fn set_pgid(pid: Pid, pgid: Pid) -> bool {
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        t.pgid = pgid;
        return true;
    }
    let mut log = EXIT_LOG.lock();
    let mut found = false;
    for rec in log.iter_mut().filter_map(|e| e.as_mut()) {
        if rec.pid == pid && rec.is_process && !rec.consumed {
            rec.pgid = pgid;
            found = true;
        }
    }
    found
}

pub fn euid_of(pid: Pid) -> u32 {
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.euid).unwrap_or(0)
}

pub fn egid_of(pid: Pid) -> u32 {
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.egid).unwrap_or(0)
}

/// The REAL (not effective) uid/gid — what `access(2)`/`faccessat(2)` without
/// `AT_EACCESS` must check against. Same fail-open-to-root default for an
/// unknown pid as `euid_of`/`egid_of`.
pub fn ruid_of(pid: Pid) -> u32 {
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.uid).unwrap_or(0)
}

pub fn rgid_of(pid: Pid) -> u32 {
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.gid).unwrap_or(0)
}

/// Supplementary groups of `pid`, copied into `out`; returns the count.
/// An unknown pid (the boot-time mount path) has none.
pub fn groups_of(pid: Pid, out: &mut [u32; task::NGROUPS_MAX]) -> usize {
    let rq = RUN_QUEUE.lock();
    match rq.find_pid(pid) {
        Some(t) => {
            let n = (t.ngroups as usize).min(task::NGROUPS_MAX);
            out[..n].copy_from_slice(&t.groups[..n]);
            n
        }
        None => 0,
    }
}

/// setgroups(2): replace the calling thread's supplementary group list.
/// Privileged only (CAP_SETGID ≙ euid 0) — returns false (⇒ EPERM) otherwise.
/// `groups.len()` must not exceed `NGROUPS_MAX` (the caller maps that to EINVAL).
pub fn set_current_groups(groups: &[u32]) -> bool {
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        if t.euid != 0 { return false; }
        let n = groups.len().min(task::NGROUPS_MAX);
        t.groups[..n].copy_from_slice(&groups[..n]);
        t.ngroups = n as u8;
        return true;
    }
    false
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
        t.poll_mask     = POLL_TAG_ALL; // hygiene: never leave a narrow mask behind
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

// ── Targeted poll wakes (hash-bitmask filtering) ────────────────────────────
//
// `wake_poll` is a system-wide herd: every task parked on the single
// `POLL_WAIT_CHANNEL` is made Ready and re-probes its whole interest set. To
// wake only the pollers that care about the object that just changed, each
// parked poller records a `poll_mask: u64` — the OR of a one-bit hash tag per
// interest it holds — and each producer wakes with the SAME tag for the object
// it changed; `unblock_port_tagged` skips any task whose `poll_mask & tag == 0`.
//
// SAFETY RULE, because a lost wake is a hang: `POLL_TAG_ALL` (all ones) is the
// broadcast. Every producer that is not converted, and every consumer interest
// that cannot be hashed, MUST fall back to `POLL_TAG_ALL` — never to a narrower
// value. A broadcast wake (`tag == POLL_TAG_ALL`) reaches every non-zero mask,
// and a broadcast mask (`poll_mask == POLL_TAG_ALL`) is reached by every wake,
// so a forgotten site degrades to today's herd, never to a missed wake.
pub const POLL_TAG_ALL: u64 = u64::MAX;

/// Poll-object classes. The class disambiguates index spaces that would
/// otherwise collide (pipe ring 3 vs eventfd slot 3) before hashing.
pub mod poll_class {
    pub const PIPE:    u32 = 1;
    pub const PTY:     u32 = 2;
    pub const EVENTFD: u32 = 3;
    pub const TIMERFD: u32 = 4;
    pub const UNIX:    u32 = 5;
    pub const INET:    u32 = 6;
    pub const CONSOLE: u32 = 7;
    pub const DEVVT:   u32 = 8;
    pub const DRM:     u32 = 9;
    pub const EVDEV:   u32 = 10;
}

/// Hash a `(class, index)` object identity into a single-bit tag. A collision
/// only makes an extra poller wake (harmless — it re-probes and re-parks); it
/// can never suppress a needed wake. splitmix64 finalizer for good bit spread
/// across the 64 buckets; `const fn` so IRQ/tick producers can call it.
#[inline]
pub const fn poll_tag(class: u32, index: u32) -> u64 {
    let mut x = ((class as u64) << 32) | (index as u64);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^= x >> 33;
    1u64 << (x & 63)
}

/// Phase 1: publish Blocked-on-poll intent while still executing. No deadline
/// (an infinite / edge-only waiter). A timed waiter uses
/// `block_on_poll_prepare_until` instead so its deadline rides the SAME
/// RUN_QUEUE hold — no extra lock on the block path, and the deadline lives in
/// the task, not a clobber-prone global.
pub fn block_on_poll_prepare() { block_on_poll_prepare_until(u64::MAX) }

/// Phase 1 for a timed waiter: publish Blocked-on-poll AND the absolute
/// `monotonic_ns()` wake deadline atomically (one RUN_QUEUE hold), then fold the deadline into
/// the global `NEXT_POLL_DEADLINE` hint (lock-free) so the tick's fast path can
/// skip the run-queue scan while nothing is due. The task field is the
/// authority; the hint is only an optimisation the tick recomputes exactly.
///
/// This variant registers a `POLL_TAG_ALL` (broadcast) mask, so it is woken by
/// any wake_poll. Generic timed parks that are not epoll/poll/select interest
/// sets (wait4, the net daemon's 100 Hz cadence, nanosleep, drm retire waits)
/// use it as-is. The real poll/epoll/select park sites use
/// `block_on_poll_prepare_masked` to register a narrow mask.
pub fn block_on_poll_prepare_until(deadline: u64) {
    block_on_poll_prepare_masked(deadline, POLL_TAG_ALL)
}

/// Phase 1 with an explicit interest-set `mask` (see `poll_tag`). Written in
/// the SAME RUN_QUEUE critical section that publishes Blocked + `blocked_on ==
/// POLL_WAIT_CHANNEL`, and read only under that lock with both still true, so a
/// stale mask is unreachable rather than defended against: every state
/// transition out of Blocked clears `blocked_on`, and the next park rewrites
/// the mask.
pub fn block_on_poll_prepare_masked(deadline: u64, mask: u64) {
    let pid = current_pid();
    RUN_QUEUE.lock().block_on_port_until(pid, POLL_WAIT_CHANNEL, deadline, mask);
    if deadline != u64::MAX {
        register_poll_deadline(deadline);
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
pub fn wake_poll() { wake_poll_tagged(POLL_TAG_ALL); }

/// Wake every poll-channel waiter whose `poll_mask` intersects `tag`. A
/// producer passes the `poll_tag(class, index)` of the object it changed; an
/// unconvertible producer passes `POLL_TAG_ALL` (== `wake_poll`). Same lock /
/// context contract as `wake_poll`.
pub fn wake_poll_tagged(tag: u64) {
    let woken = RUN_QUEUE.lock().unblock_port_tagged(POLL_WAIT_CHANNEL, tag);
    if woken > 0 { wake_up_an_idle_cpu(); }
}

/// A `wake_poll` that has been asked for but not yet paid for, as an OR of the
/// tags requested since the last service. `0` = nothing pending. See
/// `request_poll_wake`.
static POLL_WAKE_PENDING: AtomicU64 = AtomicU64::new(0);

/// Ask for a poll-channel wake to happen "soon" (within one 100 Hz tick)
/// instead of right now, and COALESCE every such request in that window into
/// a single wake.
///
/// `wake_poll` is a system-wide thundering herd, and a deliberately blunt one:
/// there is exactly one `POLL_WAIT_CHANNEL`, so it takes RUN_QUEUE, linear-
/// scans all `MAX_TASKS` slots and makes EVERY parked poller in the system
/// Ready at `min_vruntime` (so the herd also preempts the caller), and each
/// woken poller then re-probes its whole interest set through the global
/// `FD_TABLES` / `PIPE_RINGS` locks. Paying that once per `write(2)` is
/// affordable for a few large writes and ruinous for thousands of tiny ones.
///
/// Callers that publish a readiness *level* change (a pipe going empty ->
/// readable, or full -> writable) must still use `wake_poll`: a poller parked
/// with that level false has no other way to learn it flipped. Callers that
/// only advanced an object's edge `seq` without changing its level use this
/// instead. That case is not merely an optimisation — an `EPOLLET` interest
/// whose `last_seq` already equals the object's `seq` can be parked with data
/// still queued, and only a later `seq` change makes it fire again — so the
/// request must never be dropped, merely deferred. `service_deferred_poll_wake`
/// on the poll-deadline tick pays it, bounding the delay at one tick (~10 ms)
/// while collapsing an unbounded burst of writes into <= 100 herds per second.
pub fn request_poll_wake() { request_poll_wake_tagged(POLL_TAG_ALL); }

/// `request_poll_wake` carrying the tag of the object whose edge `seq` moved.
/// `fetch_or` merges every deferred request in the window into one tag mask, so
/// a request landing while a wake is already in flight is merged rather than
/// stranded.
pub fn request_poll_wake_tagged(tag: u64) {
    POLL_WAKE_PENDING.fetch_or(tag, Ordering::Release);
}

/// Pay any outstanding `request_poll_wake`. Tick / IRQ context only: this uses
/// `try_wake_poll`, which honors the tick hook's try-lock-only contract.
///
/// The swap happens BEFORE the wake, never after: a request published while
/// the wake is already in flight would otherwise be cleared by a store that
/// ran after it, and that request's `seq` bump would be stranded until some
/// unrelated edge happened along. Taking the flag first means such a request
/// re-arms the flag and is paid by the next tick. A contended `try_wake_poll`
/// re-arms it too, so contention costs <= 10 ms and never a lost edge.
pub fn service_deferred_poll_wake() {
    let tag = POLL_WAKE_PENDING.swap(0, Ordering::AcqRel);
    if tag == 0 { return; }
    let ok = try_wake_poll_tagged(tag);
    lockwatch::note_tick_try(lockwatch::L_RUN_QUEUE, ok);
    if !ok {
        // Contended try_lock: re-arm (OR back) so the next tick pays it.
        POLL_WAKE_PENDING.fetch_or(tag, Ordering::Release);
    }
}

/// Non-blocking `wake_poll` for IRQ / tick context: honors the tick hook's
/// no-indefinite-wait contract (bounded `try_lock_spin`). Returns false (wake
/// deferred) if RUN_QUEUE stays held on another CPU past the bound; the next
/// tick retries.
pub fn try_wake_poll() -> bool { try_wake_poll_tagged(POLL_TAG_ALL) }

/// How long an IRQ-context poll wake may wait for `RUN_QUEUE` before deferring
/// to the next tick. Holds are sub-microsecond; 50 µs covers all but host
/// vCPU preemption of the holder, and is bounded, so no deadlock-freedom is
/// lost (see `lockwatch::TrackedMutex::try_lock_spin`).
const TICK_LOCK_WAIT_NS: u64 = 50_000;

/// Non-blocking `wake_poll_tagged` for IRQ / tick context.
pub fn try_wake_poll_tagged(tag: u64) -> bool {
    match RUN_QUEUE.try_lock_spin(TICK_LOCK_WAIT_NS) {
        Some(mut rq) => {
            lockwatch::note_wake_try(true);
            let woken = rq.unblock_port_tagged(POLL_WAIT_CHANNEL, tag);
            drop(rq);
            if woken > 0 { wake_up_an_idle_cpu(); }
            true
        }
        None => { lockwatch::note_wake_try(false); false }
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
/// `now` is a `monotonic_ns()` reading; every deadline is one too.
///
/// Bounded wait (`try_lock_spin`, ≤ `TICK_LOCK_WAIT_NS`) for the tick's
/// contract; a tick that still finds the lock held leaves the hint and retries
/// next tick (≤10 ms defer, within the timeout granularity).
pub fn service_poll_deadlines(now: u64, timerfd_due: bool) -> bool {
    match RUN_QUEUE.try_lock_spin(TICK_LOCK_WAIT_NS) {
        Some(mut rq) => {
            lockwatch::note_tick_try(lockwatch::L_RUN_QUEUE, true);
            let (new_min, woken) =
                rq.wake_due_poll_deadlines(POLL_WAIT_CHANNEL, now, timerfd_due);
            NEXT_POLL_DEADLINE.store(new_min, Ordering::Relaxed); // exact, under the lock
            drop(rq);
            if woken > 0 { wake_up_an_idle_cpu(); }
            true
        }
        None => { lockwatch::note_tick_try(lockwatch::L_RUN_QUEUE, false); false }
    }
}

/// Earliest absolute `monotonic_ns()` instant at which a timed poll/select/
/// epoll_wait waiter wants to be re-woken (u64::MAX = no timed waiter). This is a lock-free HINT that
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
    // Arm this CPU's one-shot timer so the deadline is serviced when it falls
    // due, not at the next 100 Hz tick edge after it (up to 10 ms late: with
    // never-early absolute deadlines, a back-to-back `poll(10 ms)` loop re-arms
    // just past a tick edge and used to overshoot by a whole tick every time).
    // Cheap and idempotent: the arch keeps the earliest armed instant per CPU
    // and only reprograms the compare value when this one is sooner.
    if deadline != u64::MAX {
        unsafe { arch_timer_arm_deadline(deadline); }
    }
}

/// The deadline service run from a CPU's one-shot timer interrupt (see
/// `timer_deadline_irq`); 0 until the kernel registers it.
static DEADLINE_HOOK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Register the poll-deadline service for one-shot timer interrupts. `f(now)`
/// wakes every due timed waiter (same try-lock-only IRQ contract as a tick
/// hook) and returns the next pending deadline (`u64::MAX` = none).
pub fn register_deadline_hook(f: fn(u64) -> u64) {
    DEADLINE_HOOK.store(f as usize, Ordering::Release);
}

/// Called by the arch timer IRQ on ANY CPU whose one-shot deadline (armed by
/// `register_poll_deadline`) has come due. Services the due deadlines and
/// returns the next pending one so the arch can re-arm for it; `u64::MAX` when
/// nothing is pending or no hook is registered (the 100 Hz tick remains the
/// fallback either way).
pub fn timer_deadline_irq() -> u64 {
    let hook = DEADLINE_HOOK.load(Ordering::Acquire);
    if hook == 0 { return u64::MAX; }
    let f: fn(u64) -> u64 = unsafe { core::mem::transmute(hook) };
    f(monotonic_ns())
}

// ── Task census (zink-lane instrumentation, IRQ-safe, try_lock only) ────────
#[derive(Clone, Copy)]
pub struct TaskCensusRow {
    pub pid: Pid,
    pub tgid: Pid,
    /// 0 Ready, 1 Running, 2 Blocked, 3 Zombie, 4 Stopped.
    pub state: u8,
    pub blocked_on: u32,
    pub poll_deadline: u64,
    pub blocked_futex: usize,
    pub on_cpu: u8,
    /// User PC/SP from the trap frame at the top of the kernel stack (valid
    /// for a task parked inside a syscall), and the current value of the
    /// futex word it is parked on (`u64::MAX` = unreadable).
    pub urip: u64,
    pub ursp: u64,
    pub futex_val: u64,
}
impl TaskCensusRow {
    pub const fn empty() -> Self {
        Self { pid: 0, tgid: 0, state: 0, blocked_on: 0, poll_deadline: 0, blocked_futex: 0, on_cpu: 0xFF,
               urip: 0, ursp: 0, futex_val: u64::MAX }
    }
}
/// Snapshot every live (non-zombie) task. Returns `usize::MAX` when RUN_QUEUE
/// is contended (sample missed), else the number of rows written.
pub fn task_rows(out: &mut [TaskCensusRow]) -> usize {
    let rq = match RUN_QUEUE.try_lock() { Some(r) => r, None => return usize::MAX };
    let mut n = 0;
    for i in 0..runqueue::MAX_TASKS {
        if n >= out.len() { break; }
        if let Some(t) = rq.get(i) {
            if matches!(t.state, task::TaskState::Zombie) { continue; }
            out[n] = TaskCensusRow {
                pid: t.pid,
                tgid: t.tgid,
                state: match t.state {
                    task::TaskState::Ready => 0, task::TaskState::Running => 1,
                    task::TaskState::Blocked => 2, task::TaskState::Zombie => 3,
                    task::TaskState::Stopped => 4,
                },
                blocked_on: t.blocked_on.unwrap_or(0xFFFF_FFFF),
                poll_deadline: t.poll_deadline,
                blocked_futex: t.blocked_futex,
                on_cpu: t.on_cpu.map(|c| c as u8).unwrap_or(0xFF),
                urip: 0, ursp: 0, futex_val: u64::MAX,
            };
            // Trap frame: the syscall entry stub switches to the kernel stack
            // top and pushes [ss, rsp, rflags, cs, rip] then the GPRs, so the
            // UserFrame sits at top - SIZE. Only meaningful for a parked task.
            if matches!(t.state, task::TaskState::Blocked) && t.on_cpu.is_none() && t.kernel_stack != 0 {
                let top = mm::phys_to_virt(t.kernel_stack) + KERNEL_STACK_SIZE;
                let f = unsafe { &*((top - context::UserFrame::SIZE) as *const context::UserFrame) };
                #[cfg(target_arch = "x86_64")]
                { out[n].urip = f.rip; out[n].ursp = f.rsp; }
                #[cfg(target_arch = "aarch64")]
                { out[n].urip = f.elr_el1; out[n].ursp = f.sp_el0; }
                if t.blocked_futex != 0 {
                    if let Some(leader) = rq.find_pid(t.tgid) {
                        if let Some(as_) = leader.address_space.as_ref() {
                            if let Some(pa) = as_.virt_to_phys(t.blocked_futex) {
                                out[n].futex_val = unsafe {
                                    core::ptr::read_volatile(mm::phys_to_virt(pa) as *const u32) } as u64;
                            }
                        }
                    }
                }
            }
            n += 1;
        }
    }
    n
}

/// Poor man's backtrace for every parked thread of `tgid`: scan up to `words`
/// u64s above the saved user SP and report those that land inside an
/// executable VMA of the group's address space, plus every VMA once.
/// Physical reads only (page-table walk via `virt_to_phys`); never faults.
pub fn dump_group_stacks(tgid: Pid, words: usize) -> bool {
    let rq = match RUN_QUEUE.try_lock() { Some(r) => r, None => return false };
    let leader = match rq.find_pid(tgid) { Some(t) => t, None => return true };
    let as_ = match leader.address_space.as_ref() { Some(a) => a, None => return true };
    for r in as_.regions.iter().filter_map(|r| r.as_ref()) {
        mm::gap2::s("[VMAP] "); mm::gap2::h(r.start); mm::gap2::s("-"); mm::gap2::h(r.end);
        mm::gap2::kv(" prot=", r.prot as usize); mm::gap2::kv(" cap=", r.file_cap);
        mm::gap2::kv(" foff=", r.file_off as usize); mm::gap2::kv(" lazy=", r.lazy as usize);
        mm::gap2::nl();
    }
    for i in 0..runqueue::MAX_TASKS {
        let t = match rq.get(i) { Some(t) => t, None => continue };
        if t.tgid != tgid || !matches!(t.state, task::TaskState::Blocked) || t.on_cpu.is_some() || t.kernel_stack == 0 { continue; }
        let top = mm::phys_to_virt(t.kernel_stack) + KERNEL_STACK_SIZE;
        let f = unsafe { &*((top - context::UserFrame::SIZE) as *const context::UserFrame) };
        #[cfg(target_arch = "x86_64")]
        let (rip, sp) = (f.rip as usize, f.rsp as usize);
        #[cfg(target_arch = "aarch64")]
        let (rip, sp) = (f.elr_el1 as usize, f.sp_el0 as usize);
        mm::gap2::s("[BT] pid="); mm::gap2::h(t.pid as usize);
        mm::gap2::kv(" rip=", rip); mm::gap2::kv(" sp=", sp); mm::gap2::s(" :");
        let mut n = 0usize;
        let mut va = sp & !7;
        while n < words {
            let pa = match as_.virt_to_phys(va) { Some(p) => p, None => break };
            let w = unsafe { core::ptr::read_volatile(mm::phys_to_virt(pa) as *const usize) };
            if w >= 0x1000 && as_.find(w).map(|r| r.prot & 0x4 != 0).unwrap_or(false) {
                mm::gap2::s(" "); mm::gap2::h(w);
            }
            va += 8; n += 1;
        }
        mm::gap2::nl();
    }
    true
}

/// Which tgid the `[SCSTAT]` per-syscall census follows (0 = none). Set by
/// `sys_execve` when the image path ends in `cosmic-comp`.
pub static SC_FOCUS_TGID: AtomicU32 = AtomicU32::new(0);
/// Second `[SCSTAT]` focus: the tgid whose image ends in `cosmic-greeter-login`.
pub static SC_FOCUS2_TGID: AtomicU32 = AtomicU32::new(0);

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
    /// A matching child terminated: (pid, status). The child is marked
    /// reaped and will not be reported again. Callers encode the status with
    /// [`ExitStatus::wait_status`] (wait4) or [`ExitStatus::si_code`] /
    /// [`ExitStatus::si_status`] (waitid) — never by hand.
    Reaped(Pid, ExitStatus),
    /// A matching child is stopped by job control: (pid, stop signal). Only
    /// reported when [`WaitWhat::stopped`] is set; consumed once per stop.
    Stopped(Pid, u32),
    /// A matching stopped child was resumed by SIGCONT. Only reported when
    /// [`WaitWhat::continued`] is set; consumed once per continue.
    Continued(Pid),
    /// Matching children exist but none has a reportable state change.
    StillRunning,
    /// The caller has no matching children at all — wait4 returns ECHILD.
    NoChildren,
}

/// Which child state changes a wait call is interested in — wait4's
/// `WUNTRACED`/`WCONTINUED` and waitid's `WEXITED`/`WSTOPPED`/`WCONTINUED`.
#[derive(Clone, Copy)]
pub struct WaitWhat {
    pub exited:    bool,
    pub stopped:   bool,
    pub continued: bool,
}

impl WaitWhat {
    /// Terminations only — what every wait call before job control asked for.
    pub const EXITED: WaitWhat = WaitWhat { exited: true, stopped: false, continued: false };
}

/// Non-consuming variant of `wait_try`: reports whether a matching child
/// exists / has a reportable state change without consuming the report.
/// Used by the blocking paths' re-check and by waitid() calls that don't
/// include WEXITED (stopped/continued-only waits must leave exit statuses
/// for a later wait4 to collect).
pub fn wait_peek(sel: WaitSel, caller_tgid: Pid, what: WaitWhat) -> WaitTry {
    wait_scan(sel, caller_tgid, false, what)
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
pub fn wait_try(sel: WaitSel, caller_tgid: Pid, what: WaitWhat) -> WaitTry {
    wait_scan(sel, caller_tgid, true, what)
}

fn wait_scan(sel: WaitSel, caller_tgid: Pid, consume: bool, what: WaitWhat) -> WaitTry {
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
    let mut zombie: Option<(usize, Pid, ExitStatus)> = None;
    let mut stopped: Option<(usize, Pid, u32)> = None;
    let mut continued: Option<(usize, Pid)> = None;
    for i in 0..runqueue::MAX_TASKS {
        let (pid, tgid, ppid, pgid, state, status, reported, stop_sig, stop_reported, cont_pending) =
            match rq.get(i) {
                Some(t) => (t.pid, t.tgid, t.ppid, t.pgid, t.state,
                            ExitStatus { code: t.exit_code, term_signal: t.term_signal },
                            t.wait_reported, t.stop_signal, t.stop_reported, t.cont_pending),
                None => continue,
            };
        if pid != tgid { continue; } // threads are not waitable children
        let parent_tgid = rq.find_pid(ppid).map(|p| p.tgid).unwrap_or(ppid);
        if parent_tgid != caller_tgid || !matches(pid, pgid) { continue; }
        if state == TaskState::Zombie {
            if !reported && zombie.is_none() {
                zombie = Some((i, pid, status));
            }
            // Reported zombies are logically reaped already — skip.
        } else {
            found_live = true;
            // Job-control reports live on the leader: a stop not yet handed
            // to a waiter, or a resume not yet collected with WCONTINUED.
            if what.stopped && stop_sig != 0 && !stop_reported && stopped.is_none() {
                stopped = Some((i, pid, stop_sig as u32));
            }
            if what.continued && cont_pending && continued.is_none() {
                continued = Some((i, pid));
            }
        }
    }
    if what.exited {
        if let Some((i, pid, status)) = zombie {
            if consume {
                if let Some(t) = rq.get_mut(i) { t.wait_reported = true; }
            }
            return WaitTry::Reaped(pid, status);
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
                    return WaitTry::Reaped(entry.pid, entry.status);
                }
            }
        }
    } else if zombie.is_some() {
        // A terminated child the caller did not ask about (waitid without
        // WEXITED) still counts as an existing child.
        found_live = true;
    }
    if let Some((i, pid, sig)) = stopped {
        if consume {
            if let Some(t) = rq.get_mut(i) { t.stop_reported = true; }
        }
        return WaitTry::Stopped(pid, sig);
    }
    if let Some((i, pid)) = continued {
        if consume {
            if let Some(t) = rq.get_mut(i) { t.cont_pending = false; }
        }
        return WaitTry::Continued(pid);
    }

    if found_live { WaitTry::StillRunning } else { WaitTry::NoChildren }
}

/// CPU nanoseconds consumed by one task (`Task::cpu_ns`), 0 if unknown.
pub fn thread_cpu_ns(pid: Pid) -> u64 {
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.cpu_ns).unwrap_or(0)
}

/// CPU nanoseconds consumed by every live task of a thread group. Exited
/// threads are not folded in (no accounting survives a slot's recycling), so
/// this is a floor for a group that has lost threads.
pub fn process_cpu_ns(tgid: Pid) -> u64 {
    let rq = RUN_QUEUE.lock();
    let mut total = 0u64;
    for i in 0..runqueue::MAX_TASKS {
        if let Some(t) = rq.get(i) {
            if t.tgid == tgid { total = total.saturating_add(t.cpu_ns); }
        }
    }
    total
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
    // A CPU that keeps opening windows and never ticks has a dead local
    // timer, not a busy one; the arch hook re-arms it (one counter read
    // otherwise). Silent-wedge hardening, 2026-09-15.
    unsafe { arch_timer_check_alive(); }
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

// ── Per-CPU tick watchdog ────────────────────────────────────────────────────
//
// The kernel runs with IRQs masked from syscall entry to exit, and every
// `spin::Mutex` in it is therefore an IRQ-off spinlock. A CPU that deadlocks
// on one, or loops forever in kernel mode, stops taking its own timer IRQ and
// produces no output at all; the machine either limps (a stuck AP) or freezes
// (a stuck BSP: TIMER_TICKS, tick hooks and every deadline stop). Both were
// observed as a silent whole-guest wedge with nothing on serial (2026-09-15,
// snd validation). Each CPU counts its own local ticks here; every
// `WD_SCAN_TICKS` of its own ticks it looks at every other online CPU's
// counter, and a counter that has not moved for `WD_STALL_SCANS` scans is
// reported once, then every `WD_REPEAT_SCANS`, on the raw UART (no lock, no
// allocation — this runs in IRQ context on whatever CPU is still alive).
// A CPU parked in `wfi`/`hlt` still ticks (the idle loop unmasks IRQs), so a
// frozen counter means "IRQs masked for seconds" or "this CPU's timer died",
// never "idle". Cost: one relaxed add per tick and a ≤8-slot scan twice a
// second.
static LOCAL_TICKS: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
/// `WD_SEEN[observer][target]`: the target's `LOCAL_TICKS` value at the
/// observer's last scan; `WD_AGE[observer][target]`: scans it has been
/// unchanged. Only the observer writes its own row.
static WD_SEEN: [[AtomicU32; MAX_CPUS]; MAX_CPUS] =
    [const { [const { AtomicU32::new(0) }; MAX_CPUS] }; MAX_CPUS];
static WD_AGE: [[AtomicU32; MAX_CPUS]; MAX_CPUS] =
    [const { [const { AtomicU32::new(0) }; MAX_CPUS] }; MAX_CPUS];
const WD_SCAN_TICKS: u32 = 50;   // scan every 0.5 s of the observer's ticks
const WD_STALL_SCANS: u32 = 4;   // report after 2 s without a tick
const WD_REPEAT_SCANS: u32 = 20; // then every 10 s while it lasts
/// Syscall number being serviced on each CPU, `NO_SYSCALL` outside one.
/// Published by the dispatcher so the watchdog can name what a stuck CPU was
/// doing; two relaxed stores per syscall.
pub const NO_SYSCALL: u32 = u32::MAX;
static CUR_SYSCALL: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(NO_SYSCALL) }; MAX_CPUS];
/// Syscall number `cpu` is servicing (`NO_SYSCALL` outside one). For lockwatch.
pub(crate) fn syscall_on_cpu(cpu: usize) -> u32 { CUR_SYSCALL[cpu.min(MAX_CPUS - 1)].load(Ordering::Relaxed) }
/// PID running on `cpu` (0 = idle / in scheduler). For lockwatch.
pub(crate) fn pid_on_cpu(cpu: usize) -> u32 { CURRENT_PID[cpu.min(MAX_CPUS - 1)].load(Ordering::Relaxed) }

/// Per-pid last syscall, `(pid << 32) | (nr << 1) | in_syscall`, indexed by
/// `pid & 1023` (pids are a sequential counter). The Ctrl-T dump prints it
/// for every task: a Running/Ready task that is "in" the same syscall on
/// every dump is a kernel-side yield-spin (the idle-CPU floor of 2026-09-16
/// was three of those), one that is "out" is a userland busy loop. One
/// relaxed store on entry and exit; the pid load is a per-CPU atomic.
static LAST_SYSCALL: [AtomicU64; 1024] = [const { AtomicU64::new(0) }; 1024];

#[inline]
pub fn note_syscall_enter(number: usize) {
    let id = unsafe { cpu_id() }.min(MAX_CPUS - 1);
    CUR_SYSCALL[id].store(number as u32, Ordering::Relaxed);
    let pid = CURRENT_PID[id].load(Ordering::Relaxed);
    LAST_SYSCALL[(pid as usize) & 1023]
        .store(((pid as u64) << 32) | ((number as u64) << 1) | 1, Ordering::Relaxed);
}
#[inline]
pub fn note_syscall_exit() {
    let id = unsafe { cpu_id() }.min(MAX_CPUS - 1);
    CUR_SYSCALL[id].store(NO_SYSCALL, Ordering::Relaxed);
    let pid = CURRENT_PID[id].load(Ordering::Relaxed);
    let slot = &LAST_SYSCALL[(pid as usize) & 1023];
    let v = slot.load(Ordering::Relaxed);
    if (v >> 32) as u32 == pid { slot.store(v & !1, Ordering::Relaxed); }
}

/// Local tick count of `cpu` (for tests and the monitor: a live CPU's count
/// advances at the tick rate).
pub fn local_ticks(cpu: usize) -> u32 {
    LOCAL_TICKS[cpu.min(MAX_CPUS - 1)].load(Ordering::Relaxed)
}

fn watchdog_scan(me: usize) {
    extern "C" {
        fn arch_serial_putc(c: u8);
        fn print_number(n: u32);
        fn print_hex(n: usize);
    }
    fn s(msg: &str) { for &b in msg.as_bytes() { unsafe { arch_serial_putc(b) } } }
    let n = active_cpu_count();
    // Pass 1: age every other CPU's counter as seen from here.
    for j in 0..n {
        if j == me { continue; }
        let cur = LOCAL_TICKS[j].load(Ordering::Relaxed);
        if cur != WD_SEEN[me][j].load(Ordering::Relaxed) {
            WD_SEEN[me][j].store(cur, Ordering::Relaxed);
            WD_AGE[me][j].store(0, Ordering::Relaxed);
        } else {
            let age = WD_AGE[me][j].load(Ordering::Relaxed).wrapping_add(1);
            WD_AGE[me][j].store(age, Ordering::Relaxed);
        }
    }
    // Pass 2: report. Only the lowest-numbered CPU that saw everything below
    // it tick in this scan speaks, so a stall is one line per period rather
    // than one per surviving CPU; if every lower CPU is stalled too, this
    // one reports them all.
    for k in 0..me {
        if WD_AGE[me][k].load(Ordering::Relaxed) == 0 { return; }
    }
    for j in 0..n {
        if j == me { continue; }
        let age = WD_AGE[me][j].load(Ordering::Relaxed);
        if age < WD_STALL_SCANS { continue; }
        // Kick the silent CPU with a reschedule IPI on every scan. If it is
        // parked in `wfi` with a dead local timer (the HVF case: nothing else
        // ever wakes an idle CPU), the IPI runs its idle loop once, and the
        // `irq_window` there re-arms the timer via `arch_timer_check_alive`.
        // If it is spinning with IRQs masked the IPI just stays pending.
        unsafe { arch_send_resched_ipi(j); }
        if (age - WD_STALL_SCANS) % WD_REPEAT_SCANS != 0 { continue; }
        s("\n[WDOG] cpu"); unsafe { print_number(j as u32) };
        s(" took no timer tick for ~"); unsafe { print_number(age * WD_SCAN_TICKS / 100) };
        s(" s (IRQs masked or timer dead): pid="); unsafe { print_number(CURRENT_PID[j].load(Ordering::Relaxed)) };
        // Name the process if the path table is free (try_lock: this is IRQ
        // context on a CPU that may itself be the one holding it).
        let tgid = CURRENT_TGID[j].load(Ordering::Relaxed);
        if let Some(t) = EXE_PATHS.try_lock() {
            if let Some(e) = t.iter().find(|e| e.tgid == tgid) {
                s(" ("); for &b in &e.path[..(e.len as usize).min(e.path.len())] { unsafe { arch_serial_putc(b) } } s(")");
            }
        }
        let sc = CUR_SYSCALL[j].load(Ordering::Relaxed);
        if sc == NO_SYSCALL { s(" not in a syscall"); } else { s(" last syscall "); unsafe { print_hex(sc as usize) }; }
        s(" preempt_disable="); unsafe { print_number(PREEMPT_DISABLE[j].load(Ordering::Relaxed)) };
        let want = lockwatch::wanted_by(j);
        if want != 0 {
            s(" spinning for "); s(lockwatch::name(want));
            match lockwatch::holder(want) {
                Some(h) => { s(" held by cpu"); unsafe { print_number(h as u32) }; }
                None => s(" (free now)"),
            }
        }
        s(" local_ticks="); unsafe { print_number(WD_SEEN[me][j].load(Ordering::Relaxed)) };
        s(" (seen from cpu"); unsafe { print_number(me as u32) }; s(")\n");
        // Every tracked lock this CPU holds: the other half of a deadlock.
        for id in 1..lockwatch::N_LOCKS as u8 {
            if lockwatch::holder(id) == Some(j) {
                s("[WDOG]   cpu"); unsafe { print_number(j as u32) }; s(" holds "); s(lockwatch::name(id)); s("\n");
            }
        }
    }
}

/// `[TLBSTAT]` period: 10 s of 100 Hz ticks.
const TLBSTAT_PERIOD_TICKS: u64 = 1000;

/// Print the TLB-shootdown / CoW-promotion counters of `mm::paging::tlbstat`
/// as deltas over the last period, when anything changed. BSP timer IRQ
/// only; raw UART, no locks.
fn tlbstat_tick(now: u64) {
    use mm::paging::tlbstat as ts;
    use core::sync::atomic::AtomicU64;
    extern "C" { fn arch_serial_putc(c: u8); }
    fn s(msg: &str) { for &b in msg.as_bytes() { unsafe { arch_serial_putc(b) } } }
    fn n(mut v: u64) {
        let mut buf = [0u8; 20]; let mut i = 0;
        if v == 0 { s("0"); return; }
        while v > 0 { buf[i] = b'0' + (v % 10) as u8; v /= 10; i += 1; }
        for j in (0..i).rev() { unsafe { arch_serial_putc(buf[j]) } }
    }
    const K: usize = 11;
    static PREV: [AtomicU64; K] = [const { AtomicU64::new(0) }; K];
    let cur: [&AtomicU64; K] = [&ts::FLUSHES, &ts::REMOTE_FLUSHES, &ts::IPIS, &ts::WAIT_NS,
        &ts::TIMEOUTS, &ts::SERVICED, &ts::COW_COPY, &ts::COW_COPY_NS, &ts::COW_REUSE,
        &ts::EXEC_PRE, &ts::EXEC_PRE_NS];
    let mut d = [0u64; K];
    let mut any = false;
    for i in 0..K {
        let c = cur[i].load(Ordering::Relaxed);
        d[i] = c.wrapping_sub(PREV[i].swap(c, Ordering::Relaxed));
        if d[i] != 0 && i != 3 && i != 7 && i != 10 { any = true; }
    }
    if !any { return; }
    s("[TLBSTAT] t="); n(now / 100);
    s(" flush="); n(d[0]); s(" remote="); n(d[1]); s(" ipi="); n(d[2]);
    s(" wait_us="); n(d[3] / 1000); s(" wait_max_us="); n(ts::WAIT_MAX_NS.swap(0, Ordering::Relaxed) / 1000);
    s(" timeout="); n(d[4]); s(" serviced="); n(d[5]);
    s(" cow_copy="); n(d[6]); s(" cow_us="); n(d[7] / 1000);
    s(" cow_max_us="); n(ts::COW_COPY_MAX_NS.swap(0, Ordering::Relaxed) / 1000);
    s(" cow_reuse="); n(d[8]);
    s(" exec="); n(d[9]); s(" exec_pre_us="); n(d[10] / 1000);
    s(" exec_pre_max_us="); n(ts::EXEC_PRE_MAX_NS.swap(0, Ordering::Relaxed) / 1000);
    s("\n");
}

/// The local timer interrupt on this CPU.
///
/// `elapsed` is how many 10 ms ticks of real time the arch timer found had
/// passed since the last one it accounted — 1 for a tick that arrived on
/// time, more when the interrupt was late by whole intervals (a long
/// IRQ-masked stretch, a stalled vCPU, a lost timer edge re-armed by
/// `arch_timer_check_alive`), 0 for a spurious interrupt. `TIMER_TICKS`
/// advances by that amount, so it stays a count of real time and every
/// tick-based deadline in the kernel expires when it should, while the tick
/// hooks run once per interrupt: a catch-up is a jump, never a storm.
pub fn timer_tick_irq(elapsed: u64) {
    let id = unsafe { cpu_id() };
    // The watchdog counts INTERRUPTS taken (is this CPU alive?), not time.
    let mine = LOCAL_TICKS[id.min(MAX_CPUS - 1)].fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    if mine % WD_SCAN_TICKS == 0 && id < MAX_CPUS {
        watchdog_scan(id);
    }
    // Every CPU has its own local timer; only the BSP advances global time so
    // TIMER_TICKS keeps its 100 Hz meaning regardless of CPU count.
    if id == 0 {
        let before = TIMER_TICKS.fetch_add(elapsed, Ordering::Relaxed);
        if before / TLBSTAT_PERIOD_TICKS != (before + elapsed) / TLBSTAT_PERIOD_TICKS {
            tlbstat_tick(before + elapsed);
        }
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

/// Suppress preemption on this CPU until the matching `preempt_enable*`.
///
/// Ticks still fire and tick hooks still run; only the context switch is
/// deferred.  Nests.  See [`PREEMPT_DISABLE`] for why the two are separable.
///
/// INVARIANT this creates, and the reason it is spelled out here: a section
/// that pairs `preempt_disable` with `irq_window` runs the **tick hooks on
/// this CPU with whatever locks that section holds**.  `virtio_gpu::submit`
/// holds `VIRTIO_GPU`, so every registered tick hook must stay `try_lock`-only
/// (a time-bounded `try_lock_spin` counts: it can stall, never deadlock)
/// and must never touch `VIRTIO_GPU` or the framebuffer console (`fb_flush`,
/// `println!`, `serial_print*`).  Hooks that need to log must use
/// `pci::serial_debug`, which writes the UART directly and takes no lock.
/// All four current hooks satisfy this — `drm_tick`, `poll_deadline_tick`,
/// `pipewire::tick_pump`, and the audio pump — and a new one that does not
/// would deadlock on a non-reentrant `spin::Mutex` against its own CPU.
#[inline]
pub fn preempt_disable() {
    let id = unsafe { cpu_id() }.min(MAX_CPUS - 1);
    PREEMPT_DISABLE[id].fetch_add(1, Ordering::Relaxed);
}

/// Undo one `preempt_disable` and honour any tick that arrived meanwhile.
///
/// Only safe where a context switch is safe — i.e. no `spin::Mutex` is held.
/// A section that holds one wants [`preempt_enable_no_resched`].
#[inline]
pub fn preempt_enable() {
    let id = unsafe { cpu_id() }.min(MAX_CPUS - 1);
    PREEMPT_DISABLE[id].fetch_sub(1, Ordering::Relaxed);
    preempt_check();
}

/// Undo one `preempt_disable` WITHOUT resched-ing here.
///
/// For sections whose caller still holds a lock when the section ends — the
/// `virtio_gpu::submit` case, where `VIRTIO_GPU` is held by the caller, so
/// switching on the way out would be the very yield-under-the-mutex this
/// mechanism exists to avoid.  Nothing is lost: `PREEMPT_NEEDED` was left set,
/// so the next `preempt_check` — the next timer IRQ, ≤10 ms away, or the
/// syscall return path — performs the switch at a point where no lock is held.
#[inline]
pub fn preempt_enable_no_resched() {
    let id = unsafe { cpu_id() }.min(MAX_CPUS - 1);
    PREEMPT_DISABLE[id].fetch_sub(1, Ordering::Relaxed);
}

/// True if IRQs are masked on this CPU right now.
///
/// Lets a wait loop tell "I was entered from a syscall, IRQs are off, opening
/// a window is the whole point" from "I was entered with IRQs already on".
/// `irq_window()` ends by MASKING, so calling it in the latter case would hand
/// the caller back a CPU with interrupts off — a silent state change for the
/// boot paths (`kms::detect_and_configure`, the early console) that reach the
/// same code.
#[inline(always)]
pub fn irqs_masked() -> bool {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let rflags: usize;
        core::arch::asm!("pushfq; pop {}", out(reg) rflags, options(nomem, preserves_flags));
        rflags & (1 << 9) == 0 // IF
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let daif: usize;
        core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack, preserves_flags));
        daif & (1 << 7) != 0 // I
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    { true }
}

pub fn preempt_check() {
    let id = unsafe { cpu_id() };
    // Hot path is one relaxed load. When preemption is disabled the flag is
    // deliberately LEFT SET rather than swapped out: the tick is deferred to
    // the next check, never dropped.
    if PREEMPT_DISABLE[id.min(MAX_CPUS - 1)].load(Ordering::Relaxed) != 0 {
        return;
    }
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
    // Fast path: the running task's own address space (page faults, its own
    // mm syscalls — nearly every call), via the per-CPU slot, no RUN_QUEUE.
    // See `CURRENT_AS` for why the pointer is live here.
    let t0 = lockwatch::as_clock();
    let cpu = unsafe { cpu_id() };
    if pid != 0 && CURRENT_PID[cpu].load(Ordering::Relaxed) == pid {
        let p = CURRENT_AS[cpu].load(Ordering::Acquire);
        if !p.is_null() {
            let as_ = unsafe { &*p };
            let mut spins: u32 = 0;
            loop {
                if as_.busy.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                    lockwatch::note_wait(0);
                    lockwatch::note_hold(lockwatch::L_AS_BUSY, true);
                    lockwatch::note_as_acquired(t0, spins != 0);
                    return Some(p);
                }
                spins = spins.wrapping_add(1);
                if spins == 1 { lockwatch::note_wait(lockwatch::L_AS_BUSY); }
                // The holder may be waiting for this CPU's TLB flush (it
                // shares this address space, and IRQs are masked here).
                mm::paging::tlb_service_pending();
                core::hint::spin_loop();
            }
        }
    }
    let mut spins: u32 = 0;
    loop {
        {
            let mut rq = RUN_QUEUE.lock();
            // Every thread of a group holds the same `Arc` (see clone_thread),
            // so a thread's own reference is preferred: it stays valid
            // while the group is being torn down and the leader's slot may
            // already be gone. The leader lookup covers tasks created before
            // that (the leader itself, kernel-spawned tasks) and is the same
            // object anyway.
            let (tgid, own) = match rq.find_pid(pid) {
                Some(t) => (t.tgid, t.address_space.is_some()),
                None => { lockwatch::note_wait(0); return None; }
            };
            let holder = if own { rq.find_pid_mut(pid) } else { rq.find_pid_mut(tgid) };
            let leader = match holder { Some(l) => l, None => { lockwatch::note_wait(0); return None; } };
            let as_ = match leader.address_space.as_ref() { Some(a) => a, None => { lockwatch::note_wait(0); return None; } };
            if as_
                .busy
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                lockwatch::note_wait(0);
                lockwatch::note_hold(lockwatch::L_AS_BUSY, true);
                lockwatch::note_as_acquired(t0, spins != 0);
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
        spins = spins.wrapping_add(1);
        if spins == 1 { lockwatch::note_wait(lockwatch::L_AS_BUSY); }
        mm::paging::tlb_service_pending();
        core::hint::spin_loop();
    }
}

/// Release exclusive access taken by `lock_leader_address_space`.
pub(crate) unsafe fn unlock_address_space(as_ptr: *mut mm::vmm::AddressSpace) {
    lockwatch::note_hold(lockwatch::L_AS_BUSY, false);
    lockwatch::note_as_released();
    (*as_ptr).busy.store(false, Ordering::Release);
}

pub fn handle_page_fault(addr: usize, is_write: bool) -> bool {
    page_fault(addr, is_write, false)
}

/// Fault in one page of the current process for a kernel access about to
/// happen (syscall-layer prefault), through the same path as a real fault —
/// so a file-backed page is read with the address space unlocked. Silent
/// on failure: the caller's own access reports the bad pointer.
pub fn prefault_user_page(addr: usize) -> bool {
    page_fault(addr, false, true)
}

fn page_fault(addr: usize, is_write: bool, quiet: bool) -> bool {
    fn print_str(s: &str) {
        extern "C" { fn arch_serial_putc(c: u8); }
        for &b in s.as_bytes() {
            unsafe { arch_serial_putc(b); }
        }
    }

    let pid = current_pid();
    FAULT_SIGBUS[unsafe { cpu_id() } % MAX_CPUS].store(false, Ordering::Relaxed);
    if pid == 0 { return false; }

    // Service the fault under the per-address-space lock, not RUN_QUEUE:
    // the fault path allocates and may copy a full page, easily tens of
    // microseconds during fork/CoW storms, and pinning the scheduler lock
    // for that long stalls every other CPU (and convoys with shootdown
    // waits — see lock_leader_address_space).
    let as_ptr = match lock_leader_address_space(pid) {
        Some(p) => p,
        None => {
            // A thread that `exit_group` has already marked Zombie (its
            // reschedule IPI is in flight) must not be told to die again —
            // that would enter the group kill a second time and spin for
            // the first killer to leave its CPU while the first killer
            // spins for this one: the CPU wedge behind `[PF] no address
            // space for faulting task` + `[WDOG]`. Yield instead: the
            // scheduler's post-dispatch check reaps a Zombie the moment it
            // is off the CPU, so this never returns.
            let dying = RUN_QUEUE.lock().find_pid(pid)
                .map_or(false, |t| t.state == TaskState::Zombie);
            if dying {
                yield_now("page fault on a dying thread");
                loop { core::hint::spin_loop(); }
            }
            print_str("[PF] no address space for faulting task\n");
            return false;
        }
    };
    // A file-backed page is read with the address space UNLOCKED: the read
    // is milliseconds of filesystem/device work, and holding `busy` across
    // it stalls every sibling thread's faults and mm syscalls (runqlock saw
    // one 1.5 s hold at session start). The file is pinned by an extra
    // reference for the duration; `install_file_fault` re-validates the VMA
    // after relocking, so a concurrent munmap/MAP_FIXED/mprotect costs at
    // most a retried access.
    let plan = unsafe { (*as_ptr).plan_user_page_fault(addr, is_write) };
    let outcome = match plan {
        mm::vmm::FaultPlan::Done(f) => {
            unsafe { unlock_address_space(as_ptr); }
            f
        }
        mm::vmm::FaultPlan::Read(p) => {
            mm::vmm::file_retain(p.cap);
            unsafe { unlock_address_space(as_ptr); }
            let mut bounce: alloc::vec::Vec<u8> = alloc::vec![0u8; p.len];
            let got = if p.len == 0 { 0 } else {
                mm::vmm::file_read(p.cap, p.pos, bounce.as_mut_ptr(), p.len)
            };
            let f = match lock_leader_address_space(pid) {
                Some(a) => {
                    let f = unsafe { (*a).install_file_fault(&p, &bounce, got) };
                    unsafe { unlock_address_space(a); }
                    f
                }
                // Exiting meanwhile: retrying the access lands in the dying
                // path above.
                None => mm::vmm::Fault::Handled,
            };
            drop(bounce);
            mm::vmm::file_release(p.cap);
            f
        }
    };
    match outcome {
        mm::vmm::Fault::Handled => true,
        mm::vmm::Fault::Bus => {
            FAULT_SIGBUS[unsafe { cpu_id() } % MAX_CPUS].store(true, Ordering::Relaxed);
            false
        }
        mm::vmm::Fault::Segv => {
            if !quiet { print_str("[PF] handle_user_page_fault returned false\n"); }
            false
        }
    }
}

/// Set by `handle_page_fault` when the fault it refused was a file page past
/// end of file (SIGBUS, not SIGSEGV); consumed by the arch fault handler on
/// the same CPU, which runs straight on without rescheduling.
static FAULT_SIGBUS: [core::sync::atomic::AtomicBool; MAX_CPUS] =
    [const { core::sync::atomic::AtomicBool::new(false) }; MAX_CPUS];

/// True (once) if the page fault this CPU just failed to resolve was a
/// file-mapping access past end of file — deliver SIGBUS/BUS_ADRERR.
pub fn take_fault_sigbus() -> bool {
    FAULT_SIGBUS[unsafe { cpu_id() } % MAX_CPUS].swap(false, Ordering::Relaxed)
}

/// Number printers for the Ctrl-T dumps: raw UART only. The kernel's
/// print_number/print_hex go through serial_write_byte, which mirrors every
/// byte into the VT screen buffer under the VT lock — and the dumps run in
/// IRQ context holding RUN_QUEUE (and, for the buddy census, FREE_LISTS), so
/// a CPU holding the VT lock while waiting on either deadlocks with the dump
/// (observed: the census wedged all four CPUs at its first digit).
mod dump_raw {
    extern "C" { fn arch_serial_putc_dump(c: u8); }
    pub fn ph(n: usize) {
        let d = b"0123456789ABCDEF";
        for i in (0..16).rev() { unsafe { arch_serial_putc_dump(d[(n >> (i * 4)) & 0xF]); } }
    }
    pub fn pn(n: u32) {
        if n == 0 { unsafe { arch_serial_putc_dump(b'0'); } return; }
        let mut buf = [0u8; 10]; let mut i = 0; let mut v = n;
        while v > 0 { buf[i] = b'0' + (v % 10) as u8; v /= 10; i += 1; }
        for j in (0..i).rev() { unsafe { arch_serial_putc_dump(buf[j]); } }
    }
}

/// Diagnostic: dump every task in the run queue to the serial console — the
/// kernel's equivalent of SysRq-t. Wired to Ctrl-T (0x14) on the serial line
/// by `tty_server::console_intercept_byte`, so a wedged userspace can be
/// asked where its threads are parked without a rebuild: `state`, the CPU it
/// is on, the IPC port it is blocked on (`POLL_WAIT_CHANNEL` for
/// poll/epoll/select/nanosleep), its poll deadline and the futex address it
/// is waiting on. Runs from IRQ context, so the run-queue lock is only tried:
/// the interrupted context may be holding it, and a spin here would be the
/// deadlock the dump exists to diagnose.
pub fn dump_tasks() {
    fn print_str(s: &str) {
        extern "C" { fn arch_serial_putc_dump(c: u8); }
        for &b in s.as_bytes() { unsafe { arch_serial_putc_dump(b); } }
    }
    use dump_raw::{ph, pn};
    pcsample::request_drain();
    lockwatch::dump_profile();
    let rq = match RUN_QUEUE.try_lock() {
        Some(rq) => rq,
        None => { print_str("[TASKS] run queue busy, try again\n"); return; }
    };
    print_str("[TASKS] tick="); pn(ticks() as u32);
    print_str(" len="); pn(rq.len() as u32);
    print_str(" quiesce_tgid="); pn(QUIESCE_TGID.load(Ordering::Relaxed));
    print_str("\n[TASKS] buddy free_pages="); pn(mm::buddy::free_pages() as u32);
    print_str(" refused_frees="); pn(mm::buddy::bad_frees() as u32);
    print_str(" blocks/order:");
    // Collected first, printed after: the census holds FREE_LISTS.
    let mut census = [0usize; mm::buddy::MAX_ORDER];
    let ok = mm::buddy::free_list_census(&mut |o, n| census[o] = n);
    if ok {
        for (o, &n) in census.iter().enumerate() {
            print_str(" "); pn(o as u32); print_str(":");
            if n == usize::MAX { print_str("CYCLE"); } else { pn(n as u32); }
        }
    } else { print_str(" (busy)"); }
    print_str("\n");
    for i in 0..runqueue::MAX_TASKS {
        let t = match rq.get(i) { Some(t) => t, None => continue };
        print_str("[TASKS] pid="); pn(t.pid);
        print_str(" tgid="); pn(t.tgid);
        print_str(" ppid="); pn(t.ppid);
        print_str(match t.state {
            TaskState::Ready => " Ready",
            TaskState::Running => " Running",
            TaskState::Blocked => " Blocked",
            TaskState::Zombie => " Zombie",
            TaskState::Stopped => " Stopped",
        });
        if let Some(c) = t.on_cpu { print_str(" cpu="); pn(c as u32); }
        if let Some(port) = t.blocked_on {
            if port == POLL_WAIT_CHANNEL {
                print_str(" on=poll dl_ms=");
                if t.poll_deadline == u64::MAX { print_str("inf"); } else { pn((t.poll_deadline / 1_000_000) as u32); }
            } else {
                print_str(" on=port:"); pn(port);
            }
        }
        if t.blocked_futex != 0 {
            print_str(" futex="); ph(t.blocked_futex);
            // The word's current value — for a musl lock that is the holder's
            // tid, which is what turns "blocked on a futex" into "blocked on
            // THAT thread". Read through the leader's VMA tables (no fault
            // possible), never through the user mapping.
            let leader_as = rq.find_pid(t.tgid).and_then(|l| l.address_space.as_ref());
            if let Some(phys) = leader_as.and_then(|a| a.virt_to_phys(t.blocked_futex)) {
                let v = unsafe { (mm::phys_to_virt(phys) as *const u32).read_volatile() };
                print_str("="); ph(v as usize);
            }
            // A timed waiter's deadline, in ms of CLOCK_MONOTONIC: against
            // `tick=` × 10 at the top of the dump this shows a wait that will
            // outlive its caller's intent (the FUTEX_WAIT_BITSET absolute/
            // relative confusion made every std timed wait last uptime +
            // timeout).
            if t.poll_deadline != u64::MAX { print_str(" dl_ms="); pn((t.poll_deadline / 1_000_000) as u32); }
        }
        if t.vfork_pending { print_str(" vfork_pending"); }
        // Last syscall entered by this pid and whether it is still inside it.
        let l = LAST_SYSCALL[(t.pid as usize) & 1023].load(Ordering::Relaxed);
        if (l >> 32) as u32 == t.pid {
            print_str(" last=0x"); ph(((l >> 1) & 0x7FFF_FFFF) as usize);
            print_str(if l & 1 != 0 { " in" } else { " out" });
        }
        // The group leader's executable, when the path table is free.
        if let Some(tbl) = EXE_PATHS.try_lock() {
            if let Some(e) = tbl.iter().find(|e| e.tgid == t.tgid) {
                print_str(" (");
                for &b in &e.path[..(e.len as usize).min(e.path.len())] { print_str(core::str::from_utf8(&[b]).unwrap_or("?")); }
                print_str(")");
            }
        }
        // Where in userspace the task stopped: the EL0 frame the exception
        // stub saved at the top of its kernel stack (`sub sp, sp, #288` below
        // `tpidr_el1`). For a Blocked task that is the syscall it parked in;
        // for a Running/Ready one it is its most recent EL0 exception — a
        // syscall, a fault or the tick — so a task spinning in a fault or
        // syscall loop shows that loop (printed as `last=`, since the task
        // may since have moved on). aarch64 only.
        // x86_64: the same frame lives at top - UserFrame::SIZE (see task_rows).
        // For a Running task it is its most recent kernel entry (syscall or
        // IRQ) — enough to place a userland busy loop.
        #[cfg(target_arch = "x86_64")]
        if t.kernel_stack != 0 && t.state != TaskState::Zombie && rq.find_pid(t.tgid).map_or(false, |l| l.address_space.is_some()) {
            let top = mm::phys_to_virt(t.kernel_stack) + KERNEL_STACK_SIZE;
            let f = unsafe { &*((top - context::UserFrame::SIZE) as *const context::UserFrame) };
            print_str(if t.state == TaskState::Blocked { " pc=" } else { " last pc=" }); ph(f.rip as usize);
            print_str(" sp="); ph(f.rsp as usize);
            print_str(" rax="); ph(f.rax as usize);
        }
        #[cfg(target_arch = "aarch64")]
        if t.kernel_stack != 0 && t.state != TaskState::Zombie && rq.find_pid(t.tgid).map_or(false, |l| l.address_space.is_some()) {
            let frame = mm::phys_to_virt(t.kernel_stack) + KERNEL_STACK_SIZE - 288;
            let f = unsafe { &*(frame as *const crate::context::UserFrame) };
            print_str(if t.state == TaskState::Blocked { " pc=" } else { " last pc=" }); ph(f.elr_el1 as usize);
            print_str(" lr="); ph(f.x[30] as usize);
            print_str(" sp="); ph(f.sp_el0 as usize);
            print_str(" x8="); ph(f.x[8] as usize);
            print_str(" x0="); ph(f.x[0] as usize);
            // For a task parked on a futex (a lock-contention wedge, the case
            // this dump exists for), also spill a slice of the user stack so a
            // deadlock can be walked back to its callers offline. Read through
            // the leader's VMA tables, so a missing page reads as nothing
            // rather than faulting. Values that look like text/stack addresses
            // only, to keep the noise down.
            if t.blocked_futex != 0 {
                let leader_as = rq.find_pid(t.tgid).and_then(|l| l.address_space.as_ref());
                print_str("\n[TASKS]   stack:");
                for i in 0..48usize {
                    let va = f.sp_el0 as usize + i * 8;
                    if let Some(phys) = leader_as.and_then(|a| a.virt_to_phys(va)) {
                        let v = unsafe { (mm::phys_to_virt(phys) as *const u64).read_volatile() };
                        if v >= 0x10000 && v < 0x0000_8000_0000_0000 { print_str(" "); ph(v as usize); }
                    }
                }
            }
        }
        print_str("\n");
    }
    drop(rq);
    // Subsystem dumps (the net server's socket tables) run after the run
    // queue is released: they take their own locks, by try_lock only.
    for h in DUMP_HOOKS.iter() {
        let f = h.load(Ordering::Acquire);
        if f != 0 {
            let f: fn() = unsafe { core::mem::transmute(f) };
            f();
        }
    }
}

/// Up to 4 subsystem hooks appended to the Ctrl-T task dump. Same contract as
/// a tick hook: IRQ context, try_lock only, no allocation.
const MAX_DUMP_HOOKS: usize = 4;
static DUMP_HOOKS: [core::sync::atomic::AtomicUsize; MAX_DUMP_HOOKS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; MAX_DUMP_HOOKS];

/// Register a function to run at the end of every `dump_tasks`. Silently
/// ignored past MAX_DUMP_HOOKS registrations.
pub fn register_dump_hook(f: fn()) {
    for h in DUMP_HOOKS.iter() {
        if h.compare_exchange(0, f as usize, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            return;
        }
    }
}

/// Diagnostic: print the faulting task's identity — its pid, tgid, its own
/// saved page-table root, and the *leader* it resolves to (pid + AS root +
/// region count + VA span). Reveals whether a worker thread's fault is being
/// serviced against the correct thread-group address space.
pub fn dump_task_ident() {
    fn print_str(s: &str) {
        extern "C" { fn arch_serial_putc(c: u8); }
        for &b in s.as_bytes() { unsafe { arch_serial_putc(b); } }
    }
    use dump_raw::{ph, pn};
    let pid = current_pid();
    let rq = RUN_QUEUE.lock();
    let (tgid, own_pt) = match rq.find_pid(pid) {
        Some(t) => (t.tgid, t.page_table),
        None => { drop(rq); print_str("[IDENT] no task\n"); return; }
    };
    print_str("[IDENT] pid="); pn(pid);
    print_str(" tgid="); pn(tgid);
    print_str(" own_pt="); ph(own_pt);
    if let Some(leader) = rq.find_pid(tgid) {
        print_str(" leader_pt="); ph(leader.page_table);
        if let Some(as_) = leader.address_space.as_ref() {
            print_str(" as_root="); ph(as_.root());
            let mut count = 0usize; let mut lo = usize::MAX; let mut hi = 0usize;
            for r in as_.regions.iter().filter_map(|r| r.as_ref()) {
                count += 1;
                if r.start < lo { lo = r.start; }
                if r.end   > hi { hi = r.end; }
            }
            print_str(" regions="); pn(count as u32);
            print_str(" lo="); ph(lo);
            print_str(" hi="); ph(hi);
        } else {
            print_str(" leader_as=NONE");
        }
    } else {
        print_str(" leader=NOTFOUND");
    }
    print_str("\n");
    drop(rq);
}

/// Diagnostic (gated by caller): dump the faulting task's file-backed and
/// executable VMAs so an EL0 fault address can be mapped to its module base +
/// file offset (identifying which .so a runtime PC lives in). Prints metadata
/// only (kernel Vec), never touches user memory, under the per-AS `busy` lock.
pub fn dump_user_vma(fault_addr: usize) {
    fn print_str(s: &str) {
        extern "C" { fn arch_serial_putc(c: u8); }
        for &b in s.as_bytes() { unsafe { arch_serial_putc(b); } }
    }
    extern "C" { fn print_hex(n: usize); }
    let pid = current_pid();
    if pid == 0 { return; }
    let as_ptr = match lock_leader_address_space(pid) {
        Some(p) => p,
        None => { print_str("[VMA] no address space\n"); return; }
    };
    unsafe {
        for r in (*as_ptr).regions.iter().filter_map(|r| r.as_ref()) {
            let is_exec = r.prot & 0x4 != 0;
            let file_backed = r.file_cap != 0 && r.file_cap != usize::MAX;
            // Only the interesting regions: executable and/or file-backed, plus
            // whichever region contains the fault address.
            let contains = fault_addr >= r.start && fault_addr < r.end;
            if !(is_exec || file_backed || contains) { continue; }
            print_str(if contains { "[VMA]* " } else { "[VMA]  " });
            print_str("start="); print_hex(r.start);
            print_str(" end=");  print_hex(r.end);
            print_str(" prot="); print_hex(r.prot as usize);
            print_str(" cap=");  print_hex(r.file_cap);
            print_str(" foff="); print_hex(r.file_off as usize);
            print_str(" flen="); print_hex(r.file_len as usize);
            print_str("\n");
        }
    }
    unsafe { unlock_address_space(as_ptr); }
    print_str("[VMA] end\n");
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

/// Live tasks, and the fixed table size they are competing for.
///
/// A LeandrOS thread is a task, so this counts threads, not processes: it is
/// the number that binds against `runqueue::MAX_TASKS`. `fork`/`clone` return
/// ENOMEM when the two are equal, however much physical memory is free.
pub fn task_census() -> (usize, usize) {
    let rq = RUN_QUEUE.lock();
    (rq.len(), rq.capacity())
}

/// Live processes: distinct thread-group ids among the tasks in the table.
pub fn process_count() -> usize {
    let rq = RUN_QUEUE.lock();
    let mut n = 0usize;
    for i in 0..runqueue::MAX_TASKS {
        if let Some(t) = rq.get(i) {
            // Count each thread group once, at its leader.
            if t.tgid == t.pid {
                n += 1;
            }
        }
    }
    n
}

pub fn spawn(entry: fn() -> !, _flags: usize) -> Option<Pid> {
    let pid = alloc_pid();

    let stack_base = mm::buddy::alloc(KERNEL_STACK_ORDER)?;
    let stack_size = KERNEL_STACK_SIZE;
    arm_kernel_stack(stack_base);

    let task = Task::new_kernel(pid, entry as usize, stack_base, stack_size, 0);
    let ok = RUN_QUEUE.lock().enqueue(task);
    if ok {
        wake_up_an_idle_cpu();
        Some(pid)
    } else {
        mm::buddy::free(stack_base, KERNEL_STACK_ORDER);
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
    let stack_phys = mm::buddy::alloc(KERNEL_STACK_ORDER)?;
    let _stack_virt = mm::phys_to_virt(stack_phys);
    let stack_size = KERNEL_STACK_SIZE;
    arm_kernel_stack(stack_phys);
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
        mm::buddy::free(stack_phys, KERNEL_STACK_ORDER);
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
                    let kst = mm::phys_to_virt(t.kernel_stack) + KERNEL_STACK_SIZE;
                    let as_ptr = t.address_space.as_ref()
                        .map_or(core::ptr::null_mut(), |a| alloc::sync::Arc::as_ptr(a) as *mut mm::vmm::AddressSpace);
                    Some((idx, &t.ctx as *const CpuContext, t.pid, kst, t.page_table, t.tgid, t.reply_port, as_ptr))
                }
                None => None,
            }
        };

        if let Some((idx, ctx_ptr, dispatched_pid, kernel_stack_top_virt, page_table, tgid, reply_port, as_ptr)) = picked {
            let dispatched_at = ticks();
            let dispatched_ns = unsafe { arch_monotonic_ns() };
            let pid;

            unsafe {
                CURRENT_CTX[id] = ctx_ptr as *mut CpuContext;
                CURRENT_PID[id].store(dispatched_pid, Ordering::Relaxed);
                CURRENT_TGID[id].store(tgid, Ordering::Relaxed);
                CURRENT_REPLY_PORT[id].store(reply_port, Ordering::Relaxed);
                CURRENT_AS[id].store(as_ptr, Ordering::Release);

                arch_set_kernel_stack(kernel_stack_top_virt as u64);
                if page_table != 0 {
                    arch_set_page_table(page_table);
                }

                context::cpu_switch_to(
                    core::ptr::addr_of_mut!(SCHEDULER_CTX[id]),
                    ctx_ptr,
                );

                // When we return here, we are in the scheduler context.
                // The task's pid is re-read from the per-CPU slot rather
                // than the copy taken at dispatch: a non-leader execve
                // exchanges the running thread's pid for the leader's
                // (`take_over_leader`) and publishes the new one there, and
                // the identity check below must follow it or the task would
                // never have its on_cpu claim released.
                pid = CURRENT_PID[id].load(Ordering::Relaxed);
                CURRENT_CTX[id] = core::ptr::null_mut();
                CURRENT_PID[id].store(0, Ordering::Relaxed);
                CURRENT_TGID[id].store(0, Ordering::Relaxed);
                CURRENT_AS[id].store(core::ptr::null_mut(), Ordering::Release);

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
                        t.cpu_ns += unsafe { arch_monotonic_ns() }.saturating_sub(dispatched_ns);
                        t.on_cpu = None;
                        if t.state == TaskState::Running {
                            t.state = TaskState::Ready;
                        }
                        // A sibling took SIGSTOP while this thread was on
                        // the CPU (see signal::do_signal_stop): now that
                        // its registers are saved, park it. Ready or
                        // Blocked alike — a Blocked thread resumes as a
                        // spurious wake on SIGCONT and re-parks itself.
                        if t.stop_pending
                            && matches!(t.state, TaskState::Ready | TaskState::Blocked)
                        {
                            t.state = TaskState::Stopped;
                            signal::clear_block_fields(t);
                        }
                        if t.state == TaskState::Zombie {
                            Some((t.pid,
                                  ExitStatus { code: t.exit_code, term_signal: t.term_signal },
                                  t.ppid, t.pgid, t.pid == t.tgid, t.wait_reported))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some((zpid, status, ppid, pgid, is_proc, reported)) = zinfo {
                    // Log the exit code BEFORE the slot disappears:
                    // a parent polling wait_pid/wait_try on another CPU must
                    // always find the pid either in the queue or in
                    // EXIT_LOG — a gap between remove() and a later
                    // log_exit() makes waitpid return ECHILD.
                    let parent_tgid = rq.find_pid(ppid).map(|p| p.tgid).unwrap_or(ppid);
                    log_exit(zpid, status, parent_tgid, pgid, is_proc, reported);
                    let ztgid = rq.get(idx).map(|t| t.tgid).unwrap_or(zpid);
                    release_quiesce_if_owner(ztgid, zpid);
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
                // A leader reaped here never ran `exit` itself when a sibling's
                // group kill took it off-CPU: release its /proc/self/exe slot,
                // or the 64-entry table fills and every later exec reads /bin/init.
                if t.pid == t.tgid { clear_exe_path(t.pid); }
                mm::buddy::free(t.kernel_stack, KERNEL_STACK_ORDER);
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
    let pid = current_pid();
    let (tgid, ppid, uid) = match claim_group_exit(pid) {
        GroupExitClaim::Owner { tgid, ppid, uid } => (tgid, ppid, uid),
        GroupExitClaim::AlreadyDying => exit_dying_thread(code),
    };
    // Before the group kill, while every thread that may have forked a child
    // is still resolvable to this group (see `reparent_children`).
    reparent_children(tgid);
    run_group_kill(code);
    if pid != tgid {
        // The leader died on some other CPU (or was reaped off-CPU above)
        // without ever running its own `exit`, and the fd table, sockets,
        // epoll instances, VT and evdev grabs are all keyed by *its* pid:
        // release them here, from a task context, or a `kill -9` of a
        // threaded compositor leaves the DRM master fd open forever.
        // Re-running this for a pid whose table is already empty is a no-op.
        run_exit_teardown(tgid);
        // Likewise the parent's SIGCHLD, which only `exit` on the leader
        // itself would have sent.
        if ppid != 0 {
            let term_signal = RUN_QUEUE.lock().find_pid(pid).map(|t| t.term_signal).unwrap_or(0);
            let status = ExitStatus { code, term_signal };
            let _ = deliver_signal_process(tgid_of(ppid), 17, SigInfo::child(tgid, uid, status));
        }
    }
    exit(code)
}

/// Result of [`claim_group_exit`].
enum GroupExitClaim {
    /// The caller runs the kill loop. `ppid`/`uid` are the leader's, captured
    /// while its slot still exists so a non-leader owner can send SIGCHLD.
    Owner { tgid: Pid, ppid: Pid, uid: u32 },
    /// Another thread is already tearing the group down (or the leader is
    /// already gone). The caller must not run the kill loop.
    AlreadyDying,
}

/// Make the calling thread the one and only thread that runs the group kill
/// loop for its thread group.
///
/// Why one: a thread that finds itself in `exit_group` while a sibling is
/// already running the loop — because that sibling marked it Zombie and its
/// page tables are on the way out, so it faulted; or because two threads
/// took fatal signals at once — would mark the sibling Zombie in turn and
/// spin (IRQs masked) for it to leave its CPU, while the sibling spins for
/// it. Each CPU takes no timer tick from then on; that is the `[WDOG]` wedge
/// behind `kill -9` of a multithreaded process. The second thread must
/// instead just stop, and let the owner's loop find it gone.
fn claim_group_exit(pid: Pid) -> GroupExitClaim {
    let mut rq = RUN_QUEUE.lock();
    let tgid = match rq.find_pid(pid) { Some(t) => t.tgid, None => return GroupExitClaim::AlreadyDying };
    let leader = match rq.find_pid_mut(tgid) { Some(l) => l, None => return GroupExitClaim::AlreadyDying };
    if leader.group_exit_owner != 0 && leader.group_exit_owner != pid {
        return GroupExitClaim::AlreadyDying;
    }
    leader.group_exit_owner = pid;
    let (ppid, uid) = (leader.ppid, leader.uid);
    // A stop request from a sibling must not park the owner mid-teardown.
    if let Some(me) = rq.find_pid_mut(pid) { me.stop_pending = false; }
    GroupExitClaim::Owner { tgid, ppid, uid }
}

/// Reap every other member of the calling thread's group (see
/// [`kill_next_group_member`]). Waiting for a member that is mid-flight on
/// another CPU keeps an IRQ window open: the kick is a reschedule IPI, and
/// the other CPU may in turn be waiting on *this* CPU for a TLB-shootdown
/// acknowledgement — a bare IRQ-off spin here would deadlock the pair.
fn run_group_kill(code: i32) {
    loop {
        match kill_next_group_member(code) {
            GroupKillStep::Done        => break,
            GroupKillStep::Reaped(pid) => run_exit_teardown(pid),
            GroupKillStep::Kicking     => { irq_window(); core::hint::spin_loop(); }
        }
    }
}

/// Terminate the calling thread because its group is already being torn
/// down by another thread (see [`claim_group_exit`]). Marks the thread a
/// Zombie and leaves the CPU; the scheduler's post-dispatch check reaps it,
/// and the owner's loop then finds it gone. No fd teardown, no
/// `clear_child_tid`, no SIGCHLD: those are process-level and the owner
/// does them exactly once.
fn exit_dying_thread(code: i32) -> ! {
    let pid = current_pid();
    {
        let mut rq = RUN_QUEUE.lock();
        if let Some(t) = rq.find_pid_mut(pid) {
            if t.state != TaskState::Zombie { t.exit_code = code; }
            t.state = TaskState::Zombie;
            t.vfork_pending = false;
        }
    }
    yield_now("exit: group already dying");
    loop { core::hint::spin_loop(); }
}

/// Terminate the whole thread group *because of a fatal signal*, recording
/// `signo` so `wait4`/`waitid` can report `WIFSIGNALED`/`WTERMSIG`.
///
/// Every default-action kill must come through here rather than
/// `exit_group(128 + signo)`. `128 + signo` is the shell's way of showing a
/// signal death in `$?`; the kernel's wait status has its own encoding (see
/// [`ExitStatus::wait_status`]), and passing the shell convention off as an
/// exit code makes `WIFEXITED` true for a killed process. The `128 + signo`
/// code is still stored, because the serial exit log and `get_exit_code`
/// have always shown it and it is unambiguous alongside `term_signal`.
///
/// The stamping pass runs *before* the group kill: `kill_next_group_member`
/// can reap and log a sibling immediately, and once logged its `ExitRecord`
/// is immutable, so a signal recorded afterwards would be lost for exactly
/// the members that died first.
pub fn exit_group_signal(signo: u32) -> ! {
    if signo != 0 && signo <= 64 {
        let pid = current_pid();
        let mut rq = RUN_QUEUE.lock();
        let tgid = rq.find_pid(pid).map(|t| t.tgid).unwrap_or(pid);
        for i in 0..runqueue::MAX_TASKS {
            if let Some(t) = rq.get_mut(i) {
                if t.tgid == tgid { t.term_signal = signo as u8; }
            }
        }
    }
    exit_group(128 + signo as i32)
}

/// POSIX `execve` thread-group teardown ("de-thread"): terminate **every other
/// thread** in the calling thread's group, then make the caller the sole
/// surviving thread and its own group leader.
///
/// When any thread `execve`s, the entire old program image is replaced, so all
/// sibling threads — which are executing code and standing on stacks that
/// belong to the image about to vanish — must cease to exist (POSIX
/// "all threads other than the calling thread are terminated"). LeandrOS
/// previously skipped this: [`sys_execve`] called `replace_address_space`
/// directly, giving the *caller* a fresh address space while leaving every
/// sibling running on the now-orphaned old one. A COSMIC component that spawned
/// a worker thread and then re-exec'd (leader `execve` with `others > 0`) left
/// that worker running cosmic-comp code on a stale address space; the worker's
/// next page fault resolved against the *leader's new* AS
/// (`lock_leader_address_space` → tgid → leader), never found its stack, and
/// was killed — and if the orphan owned the old AS, its teardown freed the page
/// tables under any still-running sibling (an external-abort-on-table-walk).
///
/// Reuses [`kill_next_group_member`]'s loop, so the same "every sibling has
/// actually *stopped* before its address space may change" ordering guarantee
/// applies. The caller keeps its own kernel stack and continues into
/// `replace_address_space`.
///
/// **A non-leader caller takes over the leader's pid** (Linux `de_thread`:
/// the exec'ing thread becomes the group leader and inherits its tid, so the
/// process keeps its pid across the exec). This is what keeps every
/// per-process table valid: fd tables, sockets, epoll instances, the exe
/// path, POSIX timers, the tty/VT tables, signal dispositions and the
/// `pid → tgid` side table are all keyed by the tgid, and the tgid never
/// changes. The alternative — promoting the caller to a *new* single-thread
/// group under its own pid — is what this used to do, and it went wrong in
/// three ways at once: the kill loop reaped the old leader as a *process*
/// (its whole fd table torn down, the parent handed a spurious exit), the
/// side table and this CPU's cached tgid still said "old leader" until the
/// next dispatch, and once they caught up the new image found an empty fd
/// table under its new key. See [`take_over_leader`] for the exchange.
pub fn dethread_current_group() {
    let pid = current_pid();
    // An execve racing a fatal signal on a sibling: the process is dying,
    // so the exec never happens — this thread dies with the rest.
    let tgid = match claim_group_exit(pid) {
        GroupExitClaim::Owner { tgid, .. } => tgid,
        GroupExitClaim::AlreadyDying => exit_dying_thread(0),
    };
    if pid == tgid {
        run_group_kill(0);
        let mut rq = RUN_QUEUE.lock();
        if let Some(t) = rq.find_pid_mut(pid) {
            // The caller lives on as a single-thread group and is not exiting.
            t.group_exit_owner = 0;
        }
        return;
    }
    // Non-leader: every *other* sibling first, the leader last — its Task
    // carries the process-level state the caller inherits in the exchange,
    // and it must still be there when that happens.
    loop {
        match kill_next_group_member_except(0, tgid) {
            GroupKillStep::Done        => break,
            GroupKillStep::Reaped(p)   => run_exit_teardown(p),
            GroupKillStep::Kicking     => { irq_window(); core::hint::spin_loop(); }
        }
    }
    loop {
        match take_over_leader(pid, tgid) {
            GroupKillStep::Done        => break,
            GroupKillStep::Reaped(old) => { run_exit_teardown(old); break; }
            GroupKillStep::Kicking     => { irq_window(); core::hint::spin_loop(); }
        }
    }
}

/// The pid exchange behind a non-leader `execve` (see
/// [`dethread_current_group`]): the calling thread `pid` becomes the thread
/// whose pid is `tgid`, and the old leader's `Task` — already off every CPU —
/// retires under the caller's old pid, exactly as a plain thread exit would.
///
/// Returns `Kicking` while the leader is still on another CPU (its registers
/// are live there; the caller spins as for any other sibling), `Reaped(old)`
/// once the exchange is done and the old leader's Task has been reaped under
/// the caller's old pid `old`, or `Done` if the leader was already gone (a
/// `pthread_exit` from `main` racing the exec) — then the caller simply
/// becomes a fresh single-thread group under its own pid, and the cached
/// tgid and the side table are re-keyed with it.
///
/// What the caller inherits from the leader's slot is exactly the state this
/// kernel keeps *leader-only*: `ppid`/`pgid`/`sid` (`setpgid` writes the
/// leader), the credentials, the signal disposition table (`sigaction` and
/// `reset_handlers_on_exec` write the leader), the process-level pending set
/// with its payloads, the job-control stop/continue bookkeeping, and the
/// leader's reply port (its owner in the port table is `tgid`, which is now
/// the caller). The leader's own thread-pending signals are merged in too:
/// after the exchange the caller *is* that tid, and a SIGKILL that was on
/// its way to the leader must still end the process. Per-thread state — the
/// signal mask, cwd, umask, altstack, TLS, the kernel stack — stays the
/// caller's own. Children the caller forked carry its *thread* pid as
/// `ppid` (`fork_current` records the forking thread), so they are
/// re-parented to `tgid` or `wait4` would never find them again.
///
/// The exchange happens under one `RUN_QUEUE` hold together with the
/// leader's removal, so no lookup can observe two tasks with the same pid.
/// `CURRENT_PID` for this CPU is updated in the same hold — the dispatch
/// loop's switch-back identity check reads it (not a copy taken at
/// dispatch) for exactly this reason.
fn take_over_leader(pid: Pid, tgid: Pid) -> GroupKillStep {
    let cpu = unsafe { cpu_id() };
    let mut rq = RUN_QUEUE.lock();
    let me_idx = match rq.find_pid_idx(pid) { Some(i) => i, None => return GroupKillStep::Done };
    let lidx = match rq.find_pid_idx(tgid) {
        Some(i) => i,
        None => {
            // Leader already reaped: promote in place. The tgid changes, so
            // the two caches that mirror it must follow (this is the "never
            // reassigned" assumption `CURRENT_TGID` documents — this is the
            // one exception, and it is re-published here).
            if let Some(me) = rq.get_mut(me_idx) {
                me.tgid = pid;
                me.group_exit_owner = 0;
            }
            pid_tgid_insert(pid, pid);
            CURRENT_TGID[cpu].store(pid, Ordering::Relaxed);
            return GroupKillStep::Done;
        }
    };

    let had_vfork = {
        let leader = rq.get_mut(lidx).unwrap();
        if let Some(lcpu) = leader.on_cpu {
            // Its registers are live on another CPU. It is NOT marked Zombie
            // the way `kill_next_group_member` does: that CPU's switch-back
            // reaps a Zombie on the spot, and with `pid == tgid` it would log
            // a *process* exit (the parent's wait4 would see the process die
            // while it is in fact exec'ing) and free the very slot this
            // exchange needs. Ask it to park instead — `stop_pending` is the
            // one flag the switch-back honours from another CPU — and come
            // back once it is off the CPU.
            leader.stop_pending = true;
            drop(rq);
            trigger_preempt(lcpu);
            return GroupKillStep::Kicking;
        }
        // Off-CPU (Ready, Blocked or parked Stopped): nothing executes on
        // its stack. Zombie from here on, in the same hold that removes it.
        leader.state = TaskState::Zombie;
        leader.exit_code = 0;
        core::mem::replace(&mut leader.vfork_pending, false)
    };
    if had_vfork { rq.unblock_port(VFORK_WAIT_CHANNEL); }

    // Copy out what the caller inherits, then exchange the pids.
    let (ppid, pgid, sid, creds, actions, shared_pending, pending, info, stop, reply_port, wait_reported) = {
        let l = rq.get(lidx).unwrap();
        (l.ppid, l.pgid, l.sid,
         (l.uid, l.gid, l.euid, l.egid, l.suid, l.sgid),
         l.signal_actions, l.shared_signal_pending, l.signal_pending, l.signal_info,
         (l.stop_signal, l.stop_reported, l.cont_pending),
         l.reply_port, l.wait_reported)
    };
    let old_pid = pid;
    {
        let me = rq.get_mut(me_idx).unwrap();
        me.pid  = tgid;
        me.ppid = ppid; me.pgid = pgid; me.sid = sid;
        (me.uid, me.gid, me.euid, me.egid, me.suid, me.sgid) = creds;
        me.signal_actions = actions;
        // Only the payload slots whose bits are actually taken over move,
        // never the whole array (see `Task::signal_info`).
        me.shared_signal_pending |= shared_pending;
        let new_bits = pending & !me.signal_pending;
        me.signal_pending |= pending;
        for bit in 0..64 {
            if (shared_pending | new_bits) & (1u64 << bit) != 0 {
                me.signal_info[bit] = info[bit];
            }
        }
        (me.stop_signal, me.stop_reported, me.cont_pending) = stop;
        me.stop_pending = false;
        me.wait_reported = wait_reported;
        me.group_exit_owner = 0;
        // The caller's own reply port is owned by `old_pid` in the port
        // table and is released with it below; the leader's is owned by
        // `tgid` — the caller's pid from here on.
        me.reply_port = reply_port;
    }
    {
        let leader = rq.get_mut(lidx).unwrap();
        leader.pid = old_pid;
    }
    // Keep the run queue's pid → slot hint current (a stale hint is only
    // slower, never wrong — see `RunQueue::pid_index`).
    rq.reindex(me_idx);
    rq.reindex(lidx);
    // Children forked by this thread name its old tid as parent.
    for i in 0..runqueue::MAX_TASKS {
        if let Some(t) = rq.get_mut(i) {
            if t.ppid == old_pid && t.pid != old_pid { t.ppid = tgid; }
        }
    }
    CURRENT_PID[cpu].store(tgid, Ordering::Relaxed);
    // `me.reply_port` was just overwritten with the leader's (above): the
    // per-CPU cache `current_reply_port()`/IPC reply path reads is keyed by
    // CPU, not by pid, so it must follow the exchange here too — the same
    // pairing `set_current_reply_port` keeps. Leaving the pre-exec value
    // cached would silently reply on the old thread's port after exec.
    CURRENT_REPLY_PORT[cpu].store(reply_port, Ordering::Relaxed);

    // Reap the old leader's Task under the caller's old pid: a *thread*
    // exit — no process exit record, no fd teardown for `tgid`, no SIGCHLD.
    // `remove` retires `old_pid` from the pid→tgid side table; the
    // `tgid → tgid` entry the leader registered at creation is the caller's
    // now and stays.
    let parent_tgid = rq.find_pid(ppid).map(|p| p.tgid).unwrap_or(ppid);
    log_exit(old_pid, ExitStatus { code: 0, term_signal: 0 }, parent_tgid, pgid, false, false);
    release_quiesce_if_owner(tgid, tgid);
    let reaped = rq.remove(lidx);
    drop(rq);

    // The leader may have been parked in futex_wait: that registration is
    // keyed by `tgid`, which now names the (running) caller, and a stale
    // slot would swallow a future wake for the new image.
    futex::remove_waiter(tgid);

    if let Some(t) = reaped {
        let hook_ptr = TASK_EXIT_HOOK.load(Ordering::Acquire);
        if !hook_ptr.is_null() {
            let hook: fn(u32) = unsafe { core::mem::transmute(hook_ptr) };
            hook(t.pid);   // == old_pid: releases the caller's old reply port
        }
        mm::buddy::free(t.kernel_stack, KERNEL_STACK_ORDER);
    }
    GroupKillStep::Reaped(old_pid)
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
        let written = with_current_address_space_mut(|as_| {
            as_.write_user_buf(clear_addr, &zero.to_ne_bytes())
        }).unwrap_or(false);
        if written {
            futex_wake(clear_addr, 1);
        }
    }

    // Last use of user memory is above: release the address space now, before
    // the task turns Zombie and becomes waitable (Linux: `exit_mm` precedes
    // `exit_notify`).
    release_exiting_address_space(pid);

    let (tgid, ppid, status, uid) = {
        let mut rq = RUN_QUEUE.lock();
        let (r, had_vfork) = match rq.find_pid_mut(pid) {
            Some(t) => {
                t.state = TaskState::Zombie;
                t.exit_code = code;
                let had_vfork = core::mem::replace(&mut t.vfork_pending, false); // release a vfork-suspended parent
                ((t.tgid, t.ppid,
                  ExitStatus { code: t.exit_code, term_signal: t.term_signal },
                  t.uid), had_vfork)
            }
            None => ((pid, 0, ExitStatus::NONE, 0), false),
        };
        if had_vfork { rq.unblock_port(VFORK_WAIT_CHANNEL); }
        r
    };
    // Release the /proc/self/exe side-table slot when the process leader dies.
    if pid == tgid { clear_exe_path(tgid); }
    // Hand this process's children to init. `exit_group` already did this
    // with the whole group live; a second pass finds nothing and is harmless.
    if pid == tgid { reparent_children(tgid); }
    // A process leaving can orphan a stopped job: POSIX says that job gets
    // SIGHUP + SIGCONT rather than staying stopped with no shell left to
    // continue it. Must run after the zombie marking above so the scan does
    // not count the exiting process as a live anchor.
    if pid == tgid { signal::kill_orphaned_pgrps(tgid); }
    // POSIX: the parent gets SIGCHLD when a child *process* terminates.
    // Threads (pid != tgid) don't signal, and the signal goes to the parent's
    // thread-group leader since the signal-action table is TGID-shared.
    // Ignored by default (SIGCHLD is in the default-ignore set), but it wakes
    // a parent blocked in read/epoll_wait with EINTR — which is exactly how
    // tokio's child-reaping learns the child is gone.
    if pid == tgid && ppid != 0 {
        // Process-directed: deliver to any parent thread that hasn't masked
        // SIGCHLD (tokio blocks it on the main thread), not blindly the leader.
        //
        // `si_code`/`si_status` come from `ExitStatus`, never recomputed here.
        // This is the *third* consumer of "did the child exit or was it
        // killed?", after `wait4`'s packed status and `waitid`'s siginfo, and
        // those two already drifted once (si_code was hardcoded `CLD_EXITED`
        // while `wait_status()` reported the kill). A SIGCHLD handler that
        // switches on `si_code` and a `waitpid` caller that decodes the packed
        // status are looking at the same death and must agree.
        let _ = deliver_signal_process(tgid_of(ppid), 17, SigInfo::child(pid, uid, status));
    }
    yield_now("exit");
    loop { core::hint::spin_loop(); }
}

/// Free a dying process's address space from `exit` itself, before the task
/// is marked Zombie.
///
/// `wait_scan` reports a Zombie the moment it is marked, but the reaping
/// CPU's scheduler loop only drops the `Task` — and with it the address
/// space — after it has switched away from it. A parent's `wait4` therefore
/// returned while the child's whole footprint (an eager copy of every
/// writable page of the forker, its page tables, its user stack) was still
/// allocated, and `sysinfo`/`MemFree` read right after the wait counted a
/// dead child as live memory: memtest's `lost_kib_per_death` ≈ 470–500 and
/// killmt's `exec_worker` "121 KiB/exec" were exactly that one in-flight
/// child, not leaked pages (the buddy census before/after the same runs
/// differs by 0–4 pages).
///
/// Only the sole owner releases early: the caller must be the last task of
/// its thread group — the leader with every sibling gone, or the thread
/// that ran a group kill after the leader was reaped (siblings resolve their
/// faults through the leader's `address_space`, see
/// `lock_leader_address_space`, so a leader with live threads must keep
/// it) — and the Arc must have no other holder (a `CLONE_VM`/vfork sharer
/// keeps it alive). Anything else keeps the old behaviour: the reaper drops
/// it.
///
/// The `Task` keeps running kernel code after this, so its root is detached
/// first (in the same RUN_QUEUE hold `page_table` is cleared, so a
/// re-dispatch after a preemption never loads the freed root), and the drop
/// waits out any in-flight `busy` holder exactly as `replace_address_space`
/// does.
fn release_exiting_address_space(pid: Pid) {
    let taken = {
        let mut rq = RUN_QUEUE.lock();
        let tgid = match rq.find_pid(pid) { Some(t) => t.tgid, None => return };
        for i in 0..runqueue::MAX_TASKS {
            if let Some(t) = rq.get(i) {
                if t.tgid == tgid && t.pid != pid { return; }
            }
        }
        let t = match rq.find_pid_mut(pid) { Some(t) => t, None => return };
        match t.address_space.as_ref() {
            Some(a) if alloc::sync::Arc::strong_count(a) == 1 => {}
            _ => return,
        }
        t.page_table = 0;
        let taken = t.address_space.take();
        unsafe {
            // x86_64: back onto the kernel CR3; aarch64: TTBR0 := 0 + local
            // TLB flush (the kernel runs from TTBR1 either way).
            arch_load_kernel_page_table();
            arch_set_page_table(0);
        }
        taken
    };
    if let Some(old) = taken {
        if old.busy.load(Ordering::Acquire) {
            lockwatch::note_wait(lockwatch::L_AS_BUSY);
            while old.busy.load(Ordering::Acquire) { core::hint::spin_loop(); }
            lockwatch::note_wait(0);
        }
        drop(old);
    }
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
/// kept running after the thread-group leader was reaped.
///
/// Every thread holds a reference to the shared `AddressSpace` (see
/// `Task::address_space` and `clone_thread`), so a sibling that is still
/// mid-execution on another CPU keeps its page tables alive until its own
/// CPU has reaped it. That was not always so: only the leader used to hold
/// one, and a kill loop run by a non-leader thread — whichever thread the
/// fatal signal landed on — reaped the leader while siblings still ran on
/// its tables. Their next TLB miss was "no address space for faulting
/// task", each entered its own kill loop, and the loops waited on each
/// other with IRQs off (`[WDOG]` on every CPU involved).
///
/// Only the group's designated owner may call this (see `claim_group_exit`
/// / [`exit_group`]), in a loop until it returns `Done`, *then* `exit` for
/// the caller — that ordering guarantees every sibling has actually stopped
/// running (not merely been asked to) before the last reference drops.
pub fn kill_next_group_member(exit_code: i32) -> GroupKillStep {
    kill_next_group_member_except(exit_code, 0)
}

/// [`kill_next_group_member`] that leaves the member `skip` alone (0 = none).
/// A non-leader `execve` uses it to reap every sibling *except* the leader,
/// whose slot it takes over afterwards (see [`take_over_leader`]).
fn kill_next_group_member_except(exit_code: i32, skip: Pid) -> GroupKillStep {
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    let tgid = match rq.find_pid(pid) {
        Some(t) => t.tgid,
        None => return GroupKillStep::Done,
    };

    let mut target: Option<usize> = None;
    for i in 0..runqueue::MAX_TASKS {
        if let Some(t) = rq.get(i) {
            if t.tgid == tgid && t.pid != pid && t.pid != skip {
                target = Some(i);
                break;
            }
        }
    }
    let idx = match target {
        Some(i) => i,
        None => return GroupKillStep::Done,
    };

    let (tpid, on_cpu, tppid, tpgid, t_is_proc, t_reported, t_term_signal, had_vfork) = {
        let t = rq.get_mut(idx).unwrap();
        t.state = TaskState::Zombie;
        t.exit_code = exit_code;
        let had_vfork = core::mem::replace(&mut t.vfork_pending, false); // release a vfork-suspended parent
        // `term_signal` is NOT set here: `exit_group_signal` stamps it on
        // every member of the group before this loop starts, so a member
        // reaped through this path already carries it.
        (t.pid, t.on_cpu, t.ppid, t.pgid, t.pid == t.tgid, t.wait_reported, t.term_signal, had_vfork)
    };
    if had_vfork { rq.unblock_port(VFORK_WAIT_CHANNEL); }

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
    log_exit(tpid, ExitStatus { code: exit_code, term_signal: t_term_signal },
             parent_tgid, tpgid, t_is_proc, t_reported);
    release_quiesce_if_owner(tgid, tpid);
    let reaped = rq.remove(idx);
    drop(rq);

    futex::remove_waiter(tpid);

    if let Some(t) = reaped {
        let hook_ptr = TASK_EXIT_HOOK.load(Ordering::Acquire);
        if !hook_ptr.is_null() {
            let hook: fn(u32) = unsafe { core::mem::transmute(hook_ptr) };
            hook(t.pid);
        }
        // A leader reaped here never ran `exit` itself when a sibling's
        // group kill took it off-CPU: release its /proc/self/exe slot,
        // or the 64-entry table fills and every later exec reads /bin/init.
        if t.pid == t.tgid { clear_exe_path(t.pid); }
        mm::buddy::free(t.kernel_stack, KERNEL_STACK_ORDER);
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
            // Re-publish before the displaced one can be dropped (below).
            let cur = t.address_space.as_ref()
                .map_or(core::ptr::null_mut(), |a| alloc::sync::Arc::as_ptr(a) as *mut mm::vmm::AddressSpace);
            CURRENT_AS[unsafe { cpu_id() }].store(cur, Ordering::Release);
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
            // The TID-clear address belongs to the old image (Linux drops it
            // in `mm_release` on exec). Left in place, `exit` would write a
            // zero into whatever the new image put at that address; the new
            // image's libc registers its own with set_tid_address.
            t.clear_child_tid = 0;
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
            if old.busy.load(Ordering::Acquire) {
                lockwatch::note_wait(lockwatch::L_AS_BUSY);
                while old.busy.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }
                lockwatch::note_wait(0);
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
    let pt_root = match t.address_space.as_ref() {
        Some(a) => a.root(),
        None => rq.find_pid(t.tgid)?.address_space.as_ref()?.root(),
    };
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
