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
pub mod cred;
pub mod futex;
pub mod lockwatch;
pub mod idlestat;
pub mod gdwake;
pub mod wakestat;
pub mod pcsample;
pub mod sysacct;
pub mod runqueue;
pub mod signal;
pub mod random;
pub mod rseq;
pub mod task;
pub mod uaccess;
// Kernel facilities for in-kernel drivers (lane kfacil, 2026-10-07): IRQ
// context queries, sleeping locks, kernel threads with arguments, a work
// queue and high-resolution timers. See artifacts/notes/lane-kfacil-2026-10-07.md.
pub mod irqctx;
pub mod ksync;
pub mod kthread;
pub mod workqueue;
pub mod hrtimer;
/// Sleeping FIFO mutex with hold/wait accounting for locks held across device I/O (F2FS_MOUNTS).
pub mod iolock;
pub mod percpu;
pub mod slot;
pub use percpu::{SchedStatsLine, wake_up_an_idle_cpu_sync, flush_sync_kick};

pub use clone::{fork_current, fork_current_with, ForkOpts, clone_thread};
pub use signal::{check_and_deliver_signals, restore_signal_frame, sys_sigaction, sys_sigprocmask, sys_sigaltstack, has_deliverable_signal, reset_handlers_on_exec, fault_signal};
pub use futex::{futex_wait, futex_wake, futex_requeue, futex_wait_keyed, futex_wait_intr, FUTEX_INTERRUPTED, futex_wake_keyed, futex_requeue_keyed};
pub use futex::{futex_has_waiters, task_exists as futex_task_exists};
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
            si_value:  0,
        }
    }
}

/// Exit records that are only kept for `get_exit_code` (thread exits, process
/// exits already reported to a waiter): a ring, the oldest overwritten first.
const EXIT_RECENT_LEN: usize = 256;
/// Unconsumed process exit records — zombies whose parent has not waited yet.
/// These count against `MAX_PROCESSES` (see `zombie_records`), so the pool
/// can only fill if fork's check was bypassed; then the record spills into the
/// ring (the old behaviour) with one serial line.
const MAX_ZOMBIE_RECORDS: usize = runqueue::MAX_PROCESSES;
const _: () = assert!(MAX_ZOMBIE_RECORDS % 64 == 0);

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
    /// What the waiter adds to its children totals when it collects this
    /// record (processes only; see [`ChildAcct`]).
    acct: ChildAcct,
}

/// A process's lifetime totals as its parent's `wait` collects them: CPU time
/// of the whole group (reaped threads and its own waited-for children
/// included) and page faults. Linux adds these to the waiter's
/// `signal->cutime/cstime/cmin_flt/cmaj_flt` in `wait_task_zombie`.
#[derive(Clone, Copy, Default)]
pub struct ChildAcct { pub cpu_ns: u64, pub sys_ns: u64, pub minflt: u64, pub majflt: u64 }

/// `tgid`'s totals as a parent's wait collects them (see [`ChildAcct`]):
/// the leader's time, its reaped threads', every thread still on the queue,
/// its waited-for children's, and the address space's fault counts.
fn group_acct(rq: &runqueue::RunQueue, tgid: Pid) -> ChildAcct {
    let mut a = ChildAcct::default();
    let Some(l) = rq.find_pid(tgid) else { return a };
    a.cpu_ns = l.dead_cpu_ns + l.c_cpu_ns;
    a.sys_ns = l.dead_sys_ns + l.c_sys_ns;
    a.minflt = l.c_minflt;
    a.majflt = l.c_majflt;
    if let Some(as_) = l.address_space.as_ref() {
        a.minflt += as_.min_flt.load(Ordering::Relaxed);
        a.majflt += as_.maj_flt.load(Ordering::Relaxed);
    }
    for i in rq.live_slots() {
        if let Some(t) = rq.get(i) {
            if t.tgid == tgid { a.cpu_ns += t.cpu_ns; a.sys_ns += t.sys_ns; }
        }
    }
    a
}

/// A waiter of `caller_tgid` collected a child: add its totals.
fn credit_child(rq: &mut runqueue::RunQueue, caller_tgid: Pid, a: ChildAcct) {
    if let Some(l) = rq.find_pid_mut(caller_tgid) {
        l.c_cpu_ns = l.c_cpu_ns.saturating_add(a.cpu_ns);
        l.c_sys_ns = l.c_sys_ns.saturating_add(a.sys_ns);
        l.c_minflt = l.c_minflt.saturating_add(a.minflt);
        l.c_majflt = l.c_majflt.saturating_add(a.majflt);
    }
}

/// Exit accounting for task `pid`, about to leave the run queue: a thread's
/// CPU time (and anything kept on it) moves to its group leader, so the
/// process totals survive the thread; a process's totals are returned for
/// its exit record.
fn exit_acct(rq: &mut runqueue::RunQueue, pid: Pid, is_process: bool) -> ChildAcct {
    let Some(t) = rq.find_pid(pid) else { return ChildAcct::default() };
    let tgid = t.tgid;
    if is_process || pid == tgid { return group_acct(rq, tgid); }
    let moved = (t.cpu_ns + t.dead_cpu_ns, t.sys_ns + t.dead_sys_ns,
                 ChildAcct { cpu_ns: t.c_cpu_ns, sys_ns: t.c_sys_ns, minflt: t.c_minflt, majflt: t.c_majflt });
    if let Some(l) = rq.find_pid_mut(tgid) {
        l.dead_cpu_ns = l.dead_cpu_ns.saturating_add(moved.0);
        l.dead_sys_ns = l.dead_sys_ns.saturating_add(moved.1);
    }
    credit_child(rq, tgid, moved.2);
    ChildAcct::default()
}

/// Where a process's exit status waits for its parent once the scheduler has
/// recycled the task's slot (a zombie leaves the run queue at once here).
///
/// Until lane procmax (2026-10-12) this was ONE 256-entry ring shared by every
/// exit, threads included, so a burst of more than 256 exits before the
/// parent's wait4 (a parallel build: each rustc exits with a dozen threads)
/// overwrote unconsumed process records. The parent's wait4 then answered
/// ECHILD for a child that had exited normally, and cargo reported "could not
/// execute process `rustc`". Unconsumed process records now live in their own
/// pool and are never overwritten; only already-reported records and thread
/// exits go through the ring.
struct ExitLog {
    zombies: [Option<ExitRecord>; MAX_ZOMBIE_RECORDS],
    /// Bit `i` set ⇔ `zombies[i].is_some()`.
    zused:   [u64; MAX_ZOMBIE_RECORDS / 64],
    recent:  [Option<ExitRecord>; EXIT_RECENT_LEN],
    ridx:    usize,
    /// A process record ever spilled into `recent` (pool full): from then on
    /// the waiters also search the ring for unconsumed records.
    spilled: bool,
}

impl ExitLog {
    const fn new() -> Self {
        Self {
            zombies: [const { None }; MAX_ZOMBIE_RECORDS],
            zused: [0; MAX_ZOMBIE_RECORDS / 64],
            recent: [const { None }; EXIT_RECENT_LEN],
            ridx: 0,
            spilled: false,
        }
    }

    fn push_recent(&mut self, rec: ExitRecord) {
        self.recent[self.ridx] = Some(rec);
        self.ridx = (self.ridx + 1) % EXIT_RECENT_LEN;
    }

    fn insert(&mut self, rec: ExitRecord) {
        if rec.is_process && !rec.consumed {
            for (k, w) in self.zused.iter().enumerate() {
                if *w != u64::MAX {
                    let i = k * 64 + (!*w).trailing_zeros() as usize;
                    self.zused[k] |= 1u64 << (i % 64);
                    self.zombies[i] = Some(rec);
                    ZOMBIE_RECORDS.fetch_add(1, Ordering::Relaxed);
                    percpu::pexit_bump();
                    return;
                }
            }
            if !self.spilled {
                extern "C" { fn arch_serial_putc(c: u8); }
                for &b in b"\n[SCHED] zombie record pool FULL -- unwaited exits now share the 256-entry ring\n" {
                    unsafe { arch_serial_putc(b) };
                }
            }
            self.spilled = true;
        }
        self.push_recent(rec);
        percpu::pexit_bump();
    }

    /// Indices of the occupied zombie slots.
    fn zombie_slots(&self) -> impl Iterator<Item = usize> + '_ {
        self.zused.iter().enumerate().flat_map(|(k, &w)| {
            let mut bits = w;
            core::iter::from_fn(move || {
                if bits == 0 { return None; }
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(k * 64 + b)
            })
        })
    }

    /// Hand zombie record `i` to a waiter: it leaves the pool and is kept in
    /// the ring (consumed) for `get_exit_code`.
    fn consume_zombie(&mut self, i: usize) {
        if let Some(mut rec) = self.zombies[i].take() {
            self.zused[i / 64] &= !(1u64 << (i % 64));
            ZOMBIE_RECORDS.fetch_sub(1, Ordering::Relaxed);
            rec.consumed = true;
            self.push_recent(rec);
            percpu::pexit_bump();
        }
    }

    /// Does any unconsumed process record (pool, plus the ring if a record
    /// ever spilled there) satisfy `pred`? Costs the pool's population, not
    /// its capacity.
    fn any_pending(&self, mut pred: impl FnMut(&ExitRecord) -> bool) -> bool {
        for i in self.zombie_slots() {
            if let Some(r) = &self.zombies[i] { if pred(r) { return true; } }
        }
        self.spilled && self.recent.iter().filter_map(|e| e.as_ref())
            .any(|r| r.is_process && !r.consumed && pred(r))
    }

    /// Every unconsumed process record, mutably (the pool, plus the ring if a
    /// record ever spilled there).
    fn for_each_pending(&mut self, mut f: impl FnMut(&mut ExitRecord)) {
        for k in 0..self.zused.len() {
            let mut bits = self.zused[k];
            while bits != 0 {
                let i = k * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if let Some(r) = self.zombies[i].as_mut() { f(r); }
            }
        }
        if self.spilled {
            for r in self.recent.iter_mut().filter_map(|e| e.as_mut()) {
                if r.is_process && !r.consumed { f(r); }
            }
        }
    }
}

static EXIT_LOG: Mutex<ExitLog> = Mutex::new(ExitLog::new());
/// Lock-free mirror of the zombie pool's population, for fork's limit check.
static ZOMBIE_RECORDS: AtomicUsize = AtomicUsize::new(0);

/// Exited processes whose parent has not collected the status yet (and whose
/// slot the scheduler has already recycled). Like Linux zombies, they count
/// toward the process limit: a parent that never waits cannot make the pool
/// overflow, its forks fail with EAGAIN first.
pub fn zombie_records() -> usize { ZOMBIE_RECORDS.load(Ordering::Relaxed) }

/// Does the leader of `parent_tgid` discard its children's exit statuses
/// (SIGCHLD set to SIG_IGN, or SA_NOCLDWAIT)? Then, as on Linux, a child is
/// reaped at exit and its record is born consumed.
fn parent_autoreaps(rq: &runqueue::RunQueue, parent_tgid: Pid) -> bool {
    const SIGCHLD: usize = 17;
    const SA_NOCLDWAIT: u32 = 2;
    rq.find_pid(parent_tgid).map_or(false, |l| {
        let a = &l.signal_actions[SIGCHLD - 1];
        a.handler == 1 || a.flags & SA_NOCLDWAIT != 0
    })
}

fn log_exit(rq: &mut runqueue::RunQueue, pid: Pid, status: ExitStatus, parent_tgid: Pid, pgid: Pid,
            is_process: bool, consumed: bool) {
    let acct = exit_acct(rq, pid, is_process);
    let consumed = consumed || (is_process && parent_autoreaps(rq, parent_tgid));
    EXIT_LOG.lock().insert(ExitRecord { pid, status, parent_tgid, pgid, is_process, consumed, acct });
    // A parent parked in wait4 on this child: every exit passes through here
    // on its way out of the run queue, so this is the one edge its park can
    // rely on (lock-free; the next tick pays it). The child's SIGCHLD wakes it
    // too, but only when one is sent.
    request_poll_wake_tagged(poll_tag(poll_class::WAIT, parent_tgid));
    if is_process { request_poll_wake_tagged(poll_tag(poll_class::PIDFD, pid)); }
}

/// What a pidfd for process `tgid` reports: `(exited, reaped)`. Exited means
/// no thread of the group is still running (Linux `thread_group_exited`);
/// reaped means a waiter has consumed the leader's exit, or the leader is gone
/// from the run queue altogether (pids are never reused, so an absent leader
/// is a dead one).
pub fn process_exit_state(tgid: Pid) -> (bool, bool) {
    // Lane percpu: pidfd probes (~9 k/s on the laptop desktop) answered from
    // the generation-tagged cache without RUN_QUEUE; one in 256 fast answers
    // is re-derived under the lock as a check (`pexit_miss`, must stay 0).
    let gen_then = percpu::pexit_gen();
    let fast = percpu::pexit_lookup(tgid);
    if let Some((a, false)) = fast { return a; }
    let rq = RUN_QUEUE.lock();
    let gen = percpu::pexit_gen();
    let a = process_exit_state_locked(&rq, tgid);
    match fast {
        Some((f, true)) => percpu::pexit_check(f, a, gen_then, gen),
        _ => percpu::pexit_store(tgid, gen, a),
    }
    a
}

fn process_exit_state_locked(rq: &RunQueue, tgid: Pid) -> (bool, bool) {
    let leader_zombie = match rq.find_pid(tgid) {
        // The scheduler recycles a zombie's slot eagerly; until a waiter
        // consumes its exit-log record it is still a zombie to userspace.
        None => return (true, !exit_logged_unconsumed(tgid)),
        Some(t) if t.state == TaskState::Zombie && t.wait_reported => return (true, true),
        Some(t) => t.state == TaskState::Zombie,
    };
    if !leader_zombie { return (false, false); }
    // The leader may exit before its threads; only scan in that case.
    for i in rq.live_slots() {
        if let Some(t) = rq.get(i) {
            if t.tgid == tgid && t.state != TaskState::Zombie { return (false, false); }
        }
    }
    (true, false)
}

/// pidfd_open's target check: `Ok(())` for a process that can still be named
/// (live, or a zombie no waiter has consumed), ESRCH for one that is gone,
/// EINVAL for a thread that is not a group leader.
pub fn pidfd_target_check(pid: Pid) -> Result<(), isize> {
    let rq = RUN_QUEUE.lock();
    match rq.find_pid(pid) {
        None => if exit_logged_unconsumed(pid) { Ok(()) } else { Err(-3) },
        Some(t) if t.tgid != pid => Err(-22),
        Some(t) if t.state == TaskState::Zombie && t.wait_reported => Err(-3),
        Some(_) => Ok(()),
    }
}

/// A process exit recorded in `EXIT_LOG` that no wait has consumed yet: the
/// zombie's slot is gone but the zombie, as userspace sees it, is not. Call
/// with RUN_QUEUE held (the lock order of `wait_scan`).
fn exit_logged_unconsumed(tgid: Pid) -> bool {
    EXIT_LOG.lock().any_pending(|e| e.pid == tgid)
}

/// True when an exited, not yet waited-for process of group `pgid` is in
/// `EXIT_LOG` — a zombie that still anchors its group (see
/// `cred::set_pgid_checked`). Call with RUN_QUEUE held.
pub(crate) fn exit_logged_pgid(pgid: Pid) -> bool {
    EXIT_LOG.lock().any_pending(|e| e.pgid == pgid)
}

pub fn get_exit_code(pid: Pid) -> Option<i32> {
    let log = EXIT_LOG.lock();
    for i in log.zombie_slots() {
        if let Some(r) = &log.zombies[i] { if r.pid == pid { return Some(r.status.code); } }
    }
    for entry in log.recent.iter().filter_map(|e| e.as_ref()) {
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

/// Capacity of every per-CPU table, indexed by the dense logical CPU index
/// `cpu_id()` (0 = BSP; the online CPUs are exactly `0..active_cpu_count()`).
/// The arch layer maps hardware IDs (x86 APIC IDs, Arm MPIDRs — both sparse)
/// onto it and leaves any processor beyond it parked. 64 covers every
/// desktop part; the bound is also the width of the x86 TLB-shootdown ack
/// mask (a `u64`). Per-CPU static cost is a few KiB per slot (dominated by
/// the x86 double-fault stack and the scheduler context).
pub const MAX_CPUS: usize = 64;
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
/// `&Task::sig_hint` of the task running on each CPU (null = none),
/// published at dispatch and cleared at switch-back, so the signal checks on
/// every syscall return and in every `interrupted()` can test "nothing to do"
/// without RUN_QUEUE (lane sigfast). Valid while published for the same
/// reason as [`CURRENT_AS`]: it points into the running task's own boxed
/// `Task`, which is never freed or moved while it runs (reaping happens at
/// its own switch-back, after this slot is cleared; `take_over_leader`
/// exchanges pids, not boxes), and the readers run with IRQs masked.
static CURRENT_SIGHINT: [core::sync::atomic::AtomicPtr<AtomicU32>; MAX_CPUS] =
    [const { core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()) }; MAX_CPUS];

/// A user task (one with an address space) is dispatched on `cpu` — the
/// caller, from task context on `cpu`, is that task inside a syscall or
/// fault, which leaves through a syscall return or a switch-out.
#[inline]
pub(crate) fn current_is_user_task(cpu: usize) -> bool {
    cpu < MAX_CPUS && !CURRENT_AS[cpu].load(Ordering::Relaxed).is_null()
}

/// Active CPU count (phase 2 queue placement / steal bounds).
#[inline]
pub(crate) fn active_cpus() -> usize {
    unsafe { arch_active_cpu_count() }.min(MAX_CPUS).max(1)
}

/// No task is dispatched on `cpu` (idle, or between two tasks).
#[inline]
pub(crate) fn cpu_is_idle(cpu: usize) -> bool {
    cpu < MAX_CPUS && CURRENT_PID[cpu].load(Ordering::Relaxed) == 0
}

/// Every SMT sibling of `cpu` is idle too (and is not the caller's CPU).
pub(crate) fn core_idle_except(cpu: usize) -> bool {
    let n = active_cpus();
    let me = unsafe { cpu_id() };
    let core = unsafe { arch_core_of(cpu) };
    (0..n).all(|j| j == cpu || unsafe { arch_core_of(j) } != core
        || (j != me && CURRENT_PID[j].load(Ordering::Relaxed) == 0))
}

/// Some CPU other than the caller's is idle (no task dispatched): the
/// hung-ready watchdog only fires when a Ready task could have run.
///
/// "Idle" = has found nothing to run for at least `IDLE_FOR_NS` (it went
/// through several empty picks and ticks), not merely "between two tasks":
/// under load every CPU passes through CURRENT_PID == 0 at each switch, and
/// that made the watchdog fire for every waiting task of a 2048-process
/// fork storm on x86 TCG (phase 1, first run).
pub(crate) fn some_other_cpu_idle() -> bool {
    const IDLE_FOR_NS: u64 = 50_000_000;
    let n = unsafe { arch_active_cpu_count() }.min(MAX_CPUS);
    let me = unsafe { cpu_id() };
    let now = monotonic_ns();
    (0..n).any(|i| {
        let since = IDLE_SINCE[i].load(Ordering::Relaxed);
        i != me && since != 0 && now.saturating_sub(since) >= IDLE_FOR_NS
    })
}

/// Per CPU: `monotonic_ns()` when it last found nothing to run after
/// running something (0 = it is running tasks). Set by the idle branch of
/// `scheduler_run_loop`, cleared at every dispatch.
static IDLE_SINCE: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// The running task's signal hint is clear: no signal work can be pending
/// for it (see `Task::sig_hint`). `false` = unknown, check under RUN_QUEUE.
#[inline]
pub(crate) fn current_sig_quiet() -> bool {
    let cpu = unsafe { cpu_id() }.min(MAX_CPUS - 1);
    let p = CURRENT_SIGHINT[cpu].load(Ordering::Relaxed);
    !p.is_null() && unsafe { (*p).load(Ordering::Acquire) } == 0
}

extern "C" {
    fn arch_set_page_table(root: usize);
    /// Detach this CPU from any task page table (x86-64: reload the boot
    /// kernel CR3; AArch64: clear TTBR0).  Called after every switch-back so
    /// an exited task's freed page tables are never left live on a CPU.
    fn arch_load_kernel_page_table();
    fn arch_set_kernel_stack(rsp: u64);
    /// x86-64: the user GS.base the next return to user mode on this CPU
    /// loads. AArch64: no-op.
    fn arch_set_user_gs(base: u64);
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
        for i in rq.live_slots() {
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
            for i in rq.live_slots() {
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
    // Phase 2: the filtered siblings stayed queued on their CPUs, which may
    // have gone idle meanwhile.
    RunQueue::kick_idle_with_work();
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
///
/// The move also decides the orphaned-process-group rule for the children's
/// groups (see `signal::kill_orphaned_pgrps`): once a child's `ppid` names
/// init, nothing can tell any more that the dying process was what anchored
/// that child's group, so the child half of the rule is evaluated here, from
/// the moved set, and the dying process's own group is left to `exit`. Before
/// 2026-09-26 the whole rule ran after this move and its child scan matched
/// `ppid == exiting` — which by then was never true — so a stopped job whose
/// shell died (jobtest's TOSTOP writer, left behind on both arches) stayed
/// stopped for ever instead of getting SIGHUP + SIGCONT.
fn reparent_children(dead_tgid: Pid) {
    let init = init_pid();
    if init == 0 || init == dead_tgid { return; }
    let mut cands = [0 as Pid; signal::ORPHAN_MAX_GROUPS];
    let mut nc = 0usize;
    // Children found are moved in batches of `BATCH` per RUN_QUEUE hold: a
    // moved child names init, so the next pass skips it, and the stack cost
    // stays 256 B where a `[Pid; MAX_TASKS]` buffer was 16 KiB at 4096 tasks.
    // Nearly every exit has no children at all and finishes in one pass.
    const BATCH: usize = 64;
    let mut first = true;
    loop {
        let mut batch = [0 as Pid; BATCH];
        let mut n = 0usize;
        let mut more = false;
        let mut rq = RUN_QUEUE.lock();
        let (my_pgid, my_sid) = rq.find_pid(dead_tgid).map(|t| (t.pgid, t.sid)).unwrap_or((0, 0));
        for i in rq.live_slots() {
            let (pid, tgid, ppid) = match rq.get(i) { Some(t) => (t.pid, t.tgid, t.ppid), None => continue };
            if pid != tgid || pid == dead_tgid { continue; }
            let parent_tgid = rq.find_pid(ppid).map(|p| p.tgid).unwrap_or(ppid);
            if parent_tgid == dead_tgid {
                if n < BATCH { batch[n] = pid; n += 1; } else { more = true; break; }
            }
        }
        if first {
            // Children that already exited but were never waited for: init
            // collects those too, as it does on Linux (their records would
            // otherwise hold zombie-pool slots for ever).
            first = false;
            let mut handed = false;
            EXIT_LOG.lock().for_each_pending(|rec| {
                if rec.parent_tgid == dead_tgid { rec.parent_tgid = init; handed = true; }
            });
            if handed { request_poll_wake_tagged(poll_tag(poll_class::WAIT, init)); }
        }
        for &pid in &batch[..n] {
            if let Some(t) = rq.find_pid_mut(pid) {
                t.ppid = init;
                // A child in the dying process's session but another group:
                // that group may just have lost its last outside anchor.
                if my_sid != 0 && t.sid == my_sid && t.pgid != 0 && t.pgid != my_pgid
                    && t.state != TaskState::Zombie
                    && !cands[..nc].contains(&t.pgid) && nc < cands.len()
                {
                    cands[nc] = t.pgid;
                    nc += 1;
                }
            }
        }
        drop(rq);
        if !more { break; }
    }
    if nc != 0 { signal::hup_orphaned_stopped_pgrps(&cands[..nc]); }
}

pub fn current_pid() -> Pid {
    CURRENT_PID[unsafe { cpu_id() }].load(Ordering::Relaxed)
}

pub fn current_ppid() -> Pid {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.ppid).unwrap_or(0)
}

/// The recorded parent of task `pid` (a thread id), if the task is live.
pub fn task_ppid(pid: Pid) -> Option<Pid> {
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.ppid)
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
    for i in rq.live_slots() {
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
    let leader = rq.find_pid(t.tgid);
    let leader_stopped = leader.map(|l| l.stop_signal != 0).unwrap_or(false);
    let letter = match t.state {
        TaskState::Zombie  => b'Z',
        TaskState::Stopped => b'T',
        _ if leader_stopped => b'T',
        TaskState::Blocked => b'S',
        TaskState::Ready | TaskState::Running => b'R',
    };
    // The parent *process*, as Linux reports it for every thread of a group
    // (a CLONE_THREAD child shares the leader's real_parent): a thread's own
    // `ppid` names the thread that created it, and a process forked from a
    // non-main thread records that thread, so go through the leader and name
    // the parent by its tgid (getppid(2) does the same).
    let ppid = leader.map_or(t.ppid, |l| l.ppid);
    let ppid = rq.find_pid(ppid).map_or(ppid, |p| p.tgid);
    Some((ppid, t.pgid, t.sid, letter))
}

// ── Per-process memory figures (/proc/<pid>/status, statm, smaps) ──────────

/// Run `f` on `pid`'s address space with its `busy` flag held, and return
/// its result plus the number of live threads in the group.
///
/// Unlike `with_address_space`, this is for looking at ANOTHER process,
/// which may exit and be reaped meanwhile: the `Arc` is cloned under the
/// run-queue lock, so the address space outlives the read whatever its
/// owner does; `busy` then orders the read against that owner's faults and
/// mm syscalls. `f` must not block (the usual `busy` rule).
fn with_process_as<R>(pid: Pid, f: impl FnOnce(&mm::vmm::AddressSpace) -> R) -> Option<(R, u32)> {
    let (as_arc, threads) = {
        let rq = RUN_QUEUE.lock();
        let t = rq.find_pid(pid)?;
        let tgid = t.tgid;
        let a = match t.address_space.as_ref() {
            Some(a) => a.clone(),
            None => rq.find_pid(tgid)?.address_space.as_ref()?.clone(),
        };
        let mut n = 0u32;
        for i in rq.live_slots() {
            if let Some(x) = rq.get(i) {
                if x.tgid == tgid && x.state != TaskState::Zombie { n += 1; }
            }
        }
        (a, n)
    };
    while as_arc.busy
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        // The holder may be waiting for this CPU's TLB flush acknowledgement.
        mm::paging::tlb_service_pending();
        core::hint::spin_loop();
    }
    let r = f(&as_arc);
    as_arc.busy.store(false, Ordering::Release);
    drop(as_arc);
    Some((r, threads))
}

/// Memory totals and live thread count of process `pid` (`None`: no such
/// task, or it has no address space — a kernel task or a reaped zombie).
pub fn proc_mem_of(pid: Pid) -> Option<(mm::vmm::MemCounters, u32)> {
    with_process_as(pid, |a| a.mem_counters())
}

/// `/proc/<pid>/smaps`: one `emit` per VMA, in address order.
pub fn proc_vma_census(pid: Pid, emit: &mut dyn FnMut(&mm::vmm::VmaStat)) -> bool {
    with_process_as(pid, |a| a.vma_census(emit)).is_some()
}

/// Pids of every process (thread-group leader, zombies included) that is
/// `>= from`, ascending, into `out`; returns how many were written. The
/// `/proc` directory listing.
pub fn process_pids_from(from: Pid, out: &mut [Pid]) -> usize {
    let mut n = 0usize;
    {
        let rq = RUN_QUEUE.lock();
        for i in rq.live_slots() {
            if let Some(t) = rq.get(i) {
                if t.pid == t.tgid && t.pid >= from && t.pid != 0 {
                    if n < out.len() { out[n] = t.pid; n += 1; }
                }
            }
        }
    }
    out[..n].sort_unstable();
    n
}

/// Task ids of thread group `tgid` that are `>= from`, sorted: the
/// `/proc/<pid>/task` listing. Every live (not yet reaped) task counts.
pub fn thread_ids_from(tgid: Pid, from: Pid, out: &mut [Pid]) -> usize {
    let mut n = 0usize;
    {
        let rq = RUN_QUEUE.lock();
        for i in rq.live_slots() {
            if let Some(t) = rq.get(i) {
                if t.tgid == tgid && t.pid >= from && t.pid != 0 && n < out.len() {
                    out[n] = t.pid; n += 1;
                }
            }
        }
    }
    out[..n].sort_unstable();
    n
}

/// True for a kernel task: no address space of its own nor its leader's.
pub fn is_kernel_task(pid: Pid) -> bool {
    let rq = RUN_QUEUE.lock();
    match rq.find_pid(pid) {
        Some(t) => t.address_space.is_none()
            && rq.find_pid(t.tgid).map_or(true, |l| l.address_space.is_none()),
        None => false,
    }
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
// Sizing: four slots per task (`4 × runqueue::MAX_TASKS`), so the table never
// exceeds a quarter load and linear probes stay short. pids are allocated
// sequentially from 1, so `pid & (SLOTS-1)` distributes perfectly — the
// pathological clustering open addressing is usually warned about needs a
// clumped key space, and this one is a counter.
const PID_TGID_SLOTS: usize = 4 * runqueue::MAX_TASKS;
const _: () = assert!(PID_TGID_SLOTS.is_power_of_two());
/// Packed `(pid << 32) | tgid`. `0` = empty (pid 0 is never allocated).
///
/// Removal closes the hole by BACKWARD SHIFT (lane procmax, 2026-10-12). It
/// used to leave a tombstone, and tombstones were only ever recycled by an
/// insert that probed onto them: pids are sequential, so after one pass of
/// the pid counter over the table nearly every slot was a tombstone or live,
/// and every lookup of a pid not in the table (a dead pid, the common
/// `tgid_of` miss) walked all 4096 slots before giving up. With backward
/// shift there are no tombstones and a miss stops at the first empty slot.
///
/// Lock-free readers stay safe: an entry is moved by storing it at the hole
/// first and clearing its old slot second, so a reader can at worst miss an
/// entry that is mid-move — and a miss is never wrong, `tgid_of` falls back
/// to the run queue. A reader never sees a pid paired with a wrong tgid,
/// because the pair is one atomic word.
static PID_TGID: [AtomicU64; PID_TGID_SLOTS] =
    [const { AtomicU64::new(0) }; PID_TGID_SLOTS];

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
        // Free, or already ours (a re-registration).
        if cur == 0 || (cur >> 32) as u32 == pid {
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
    const MASK: usize = PID_TGID_SLOTS - 1;
    let start = pid_tgid_slot(pid);
    let mut hole = None;
    for i in 0..PID_TGID_SLOTS {
        let s = (start + i) & MASK;
        let cur = PID_TGID[s].load(Ordering::Relaxed);
        if cur == 0 { return; }                       // end of chain, not present
        if (cur >> 32) as u32 == pid { hole = Some(s); break; }
    }
    let Some(mut hole) = hole else { return };
    let mut j = hole;
    loop {
        j = (j + 1) & MASK;
        let e = PID_TGID[j].load(Ordering::Relaxed);
        if e == 0 { break; }
        let home = pid_tgid_slot((e >> 32) as u32);
        // `e` may move into the hole unless its home lies cyclically in
        // (hole, j]. Copy first, then let the next step (or the final store)
        // clear `j`: a concurrent reader can only miss, never mis-read.
        if (j.wrapping_sub(home) & MASK) >= (j.wrapping_sub(hole) & MASK) {
            PID_TGID[hole].store(e, Ordering::Release);
            hole = j;
        }
    }
    PID_TGID[hole].store(0, Ordering::Release);
}

/// O(1) average lookup, no locks.
fn pid_tgid_get(pid: Pid) -> Option<Pid> {
    let start = pid_tgid_slot(pid);
    for i in 0..PID_TGID_SLOTS {
        let s = (start + i) & (PID_TGID_SLOTS - 1);
        let cur = PID_TGID[s].load(Ordering::Acquire);
        if cur == 0 { return None; }                  // end of chain
        if (cur >> 32) as u32 == pid {
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
    let (ret, woke, resumed) = post_signal_thread(&mut RUN_QUEUE.lock(), pid, signo, info);
    if woke { wake_up_an_idle_cpu(); }
    if let Some((tgid, ppid, uid)) = resumed { notify_continued(tgid, ppid, uid); }
    // A signalfd registered in this tgid may be parked in epoll_wait, and a
    // sibling may sit in sigtimedwait/sigsuspend; the new pending bit is a
    // readiness edge for them (RUN_QUEUE is released above). Tagged, not a
    // broadcast — see `signal_wake_tag`.
    wake_poll_tagged(signal_wake_tag(tgid_of(pid)));
    ret
}

/// What a signal posted to thread group `tgid` wakes on the poll channel: the
/// group's signalfd tag (`poll_tag(SIGNALFD, tgid)`, what a signalfd probe
/// reports and what `sigsuspend`/`sigtimedwait` park with). The thread a
/// signal is queued on is made Ready directly by the post itself. Until lane
/// deskbusy2 every post was a BROADCAST that woke all ~600 parked pollers of
/// the laptop desktop (~180 per post, ~2 000/s during video: Firefox's
/// processes signal each other) to re-probe and re-park.
#[inline]
pub fn signal_wake_tag(tgid: Pid) -> u64 { poll_tag(poll_class::SIGNALFD, tgid) }

/// `deliver_signal` for IRQ context (a POSIX-timer SIGEV_THREAD_ID expiry
/// targeting a specific thread): bounded `try_lock_spin`, `None` when
/// RUN_QUEUE stayed contended (the caller retries) — the single-thread
/// counterpart of [`try_deliver_signal_process`], for exactly the same
/// reason (never take a full lock from IRQ context). SIGCONT is refused
/// (`None`): its parent notification needs task context, as for the process
/// variant. `Some(1)` = an instance was already pending on this thread, so
/// this one coalesced into it (a POSIX timer counts that as an overrun).
pub fn try_deliver_signal(pid: Pid, signo: u32, info: task::SigInfo) -> Option<isize> {
    if signo == 0 || signo > 64 { return Some(-22); }
    if signo == signal::SIGCONT { return None; }
    let bit = 1u64 << (signo - 1);
    let mut rq = RUN_QUEUE.try_lock_spin(TICK_LOCK_WAIT_NS)?;
    let already = rq.find_pid(pid).map_or(false, |t| t.signal_pending & bit != 0);
    let tgid = rq.find_pid(pid).map_or(pid, |t| t.tgid);
    let (ret, woke, _) = post_signal_thread(&mut rq, pid, signo, info);
    let ret = if ret == 0 && already { 1 } else { ret };
    let woken = rq.unblock_port_tagged(POLL_WAIT_CHANNEL, signal_wake_tag(tgid));
    drop(rq);
    wakestat::note(core::panic::Location::caller(), false, woken, 0);
    if woke || woken > 0 { wake_up_an_idle_cpu(); }
    Some(ret)
}

/// The RUN_QUEUE-held half of `deliver_signal`/`try_deliver_signal`: returns
/// `(ret, woke, resumed)` for the caller to act on after unlocking. Split out
/// exactly as `post_signal_process` is split from `deliver_signal_process`,
/// so the IRQ-safe try-lock variant can share the same logic.
fn post_signal_thread(rq: &mut runqueue::RunQueue, pid: Pid, signo: u32, info: task::SigInfo)
    -> (isize, bool, Option<(Pid, Pid, u32)>)
{
    let mut woke = false;
    let mut resumed = None;
    if signo > 0 && signo <= 64 {
        if let Some(tgid) = rq.find_pid(pid).map(|t| t.tgid) {
            let (made_ready, r) = on_signal_generated(rq, tgid, signo);
            resumed = r;
            woke |= made_ready;
        }
    }
    let ret = if let Some(t) = rq.find_pid_mut(pid) {
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
            t.sig_kick();
            if t.state == TaskState::Blocked {
                t.set_state(TaskState::Ready);
                woke = true;
            }
            0
        } else {
            -22 // EINVAL
        }
    } else {
        -3 // ESRCH
    };
    (ret, woke, resumed)
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
        let mut made_ready = false;
        for i in rq.live_slots() {
            if let Some(t) = rq.get_mut(i) {
                if t.tgid != tgid { continue; }
                t.stop_pending = false;
                if signo == signal::SIGCONT { t.signal_pending &= !signal::SIGDFL_STOP; }
                if t.state == TaskState::Stopped {
                    t.set_state(TaskState::Ready);
                    signal::clear_block_fields(t);
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
        for i in rq.live_slots() {
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
    // Wake any signalfd poller / sigwaiter in the target tgid (RUN_QUEUE
    // released above). Tagged, see `signal_wake_tag`.
    wake_poll_tagged(signal_wake_tag(tgid));
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
    let already = rq.live_slots().any(|i| rq.get(i).map_or(false, |t|
        t.tgid == tgid && (t.signal_pending | t.shared_signal_pending) & bit != 0));
    let (ret, woke, _) = post_signal_process(&mut rq, tgid, signo, info);
    let ret = if ret == 0 && already { 1 } else { ret };
    let woken = rq.unblock_port_tagged(POLL_WAIT_CHANNEL, signal_wake_tag(tgid));
    drop(rq);
    wakestat::note(core::panic::Location::caller(), false, woken, 0);
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
        // Prefer a Blocked thread with the signal unmasked; otherwise any
        // unmasked thread; otherwise the leader.
        let mut chosen: Option<usize> = None;
        let mut chosen_blocked = false;
        for i in rq.live_slots() {
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
                    t.sig_kick();
                    if t.state == TaskState::Blocked {
                        t.set_state(TaskState::Ready);
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
                        // Every thread reads the leader's parked set: a
                        // thread that unblocks it later kicks itself (its
                        // sigprocmask), but one created meanwhile, or one
                        // whose mask changed by another path, must look too.
                        rq.sig_kick_group(tgid);
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
    // On the heap, reserved before the lock (16 KiB as a stack array).
    let mut targets: alloc::vec::Vec<Pid> = alloc::vec::Vec::new();
    if targets.try_reserve_exact(runqueue::MAX_TASKS).is_err() { return -12; } // ENOMEM
    {
        let rq = RUN_QUEUE.lock();
        for i in rq.live_slots() {
            if let Some(t) = rq.get(i) {
                if t.pgid == pgid && t.pid == t.tgid && targets.len() < targets.capacity() {
                    targets.push(t.pid);
                }
            }
        }
    }
    if targets.is_empty() { return -3; } // ESRCH — no such process group
    if signo == 0 { return 0; } // existence probe only
    for &pid in targets.iter() {
        // Process-directed (killpg): route to an unmasked thread per process.
        let _ = deliver_signal_process(pid, signo, info);
    }
    0
}

/// kill(-1, sig) — see [`cred::kill_all_checked`], which owns the
/// permission rule shared with kill(pid) and kill(-pgrp).
pub fn kill_all(signo: u32, info: task::SigInfo) -> isize {
    cred::kill_all_checked(signo, info)
}

/// Process-level pending signals parked on the caller's thread-group leader
/// (see `Task::shared_signal_pending`) — sigpending(2) must report these too.
pub fn shared_pending_signals() -> u64 {
    let pid = current_pid();
    let rq = RUN_QUEUE.lock();
    let tgid = match rq.find_pid(pid) { Some(t) => t.tgid, None => return 0 };
    rq.find_pid(tgid).map(|l| l.shared_signal_pending).unwrap_or(0)
}

/// Park `sigsuspend(2)`'s — or an interrupted ppoll/pselect6's — caller mask
/// for the delivery pass on this syscall's return (see `Task::saved_sigmask`).
pub fn stash_sigsuspend_mask(old_mask: u64) {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        t.saved_sigmask = Some(old_mask);
        t.sig_kick();
    }
}

/// Store the calling thread's `restart_block` (see `Task::restart_block`).
pub fn set_restart_block(block: [u64; 8]) {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        t.restart_block = block;
    }
}

/// Take (and clear) the calling thread's `restart_block`; `[0] == 0` means
/// there is nothing to resume.
pub fn take_restart_block() -> [u64; 8] {
    let pid = current_pid();
    match RUN_QUEUE.lock().find_pid_mut(pid) {
        Some(t) => core::mem::replace(&mut t.restart_block, [0; 8]),
        None => [0; 8],
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
        t.sig_kick(); // an unblock can make a pending signal deliverable
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
        slot::publish(t);
    }
    let id = unsafe { cpu_id() }.min(MAX_CPUS - 1);
    CURRENT_REPLY_PORT[id].store(port, Ordering::Relaxed);
}

/// Copy the calling task's cwd (host-absolute) into `buf`, truncated to
/// `max_len`. Returns the full length (which may exceed `max_len`: callers
/// that care compare), or -1 if there is no current task. Prefer
/// [`current_cwd_arc`], which has no length limit.
pub fn current_cwd(buf: *mut u8, max_len: usize) -> isize {
    match current_cwd_arc() {
        Some(c) => {
            let len = c.len().min(max_len);
            unsafe { core::ptr::copy_nonoverlapping(c.as_ptr(), buf, len); }
            c.len() as isize
        }
        None => -1,
    }
}

/// The calling task's cwd, host-absolute ("/" when never changed). `None`
/// only when there is no current task.
pub fn current_cwd_arc() -> Option<alloc::sync::Arc<[u8]>> {
    cwd_arc_of(current_pid())
}

/// `pid`'s cwd, host-absolute ("/" when never changed); `None` for no such
/// task. Cheap: a refcount bump under the run-queue lock.
pub fn cwd_arc_of(pid: Pid) -> Option<alloc::sync::Arc<[u8]>> {
    let cwd = {
        let rq = RUN_QUEUE.lock();
        rq.find_pid(pid)?.cwd.clone()
    };
    Some(cwd.unwrap_or_else(|| alloc::sync::Arc::from(&b"/"[..])))
}

/// Copy `pid`'s chroot root (host-absolute) into `buf`, truncated to
/// `max_len`. Returns its full length, or 0 when the task is not chrooted —
/// the caller's cue to skip all root handling. `-1` if there is no such task.
pub fn root_of(pid: Pid, buf: *mut u8, max_len: usize) -> isize {
    let rq = RUN_QUEUE.lock();
    match rq.find_pid(pid) {
        Some(t) => match &t.root {
            Some(r) => {
                let len = r.len().min(max_len);
                unsafe { core::ptr::copy_nonoverlapping(r.as_ptr(), buf, len); }
                r.len() as isize
            }
            None => 0,
        },
        None => -1,
    }
}

/// `pid`'s chroot root, host-absolute; `None` when it is not chrooted (or
/// does not exist).
pub fn root_arc_of(pid: Pid) -> Option<alloc::sync::Arc<[u8]>> {
    RUN_QUEUE.lock().find_pid(pid)?.root.clone()
}

/// Root of the calling task — the common case, used by kernel path resolution.
pub fn current_root(buf: *mut u8, max_len: usize) -> isize {
    root_of(current_pid(), buf, max_len)
}

/// [`root_arc_of`] for the calling task.
pub fn current_root_arc() -> Option<alloc::sync::Arc<[u8]>> {
    root_arc_of(current_pid())
}

/// Establish `path` (host-absolute) as the calling task's chroot root. A root
/// of "/" (or empty) is the same as not being chrooted.
pub fn set_root(path: &[u8]) -> bool {
    let new: Option<alloc::sync::Arc<[u8]>> =
        if path.len() <= 1 { None } else { Some(alloc::sync::Arc::from(path)) };
    let pid = current_pid();
    let old;
    {
        let mut rq = RUN_QUEUE.lock();
        match rq.find_pid_mut(pid) {
            Some(t) => old = core::mem::replace(&mut t.root, new),
            None => return false,
        }
    }
    drop(old); // free outside RUN_QUEUE
    true
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
    pub fn matches(&self, t: &task::Task) -> bool {
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
    rq.live_slots()
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
    let mut found = false;
    for i in rq.live_slots() {
        let is_match = rq.get(i).map(|t| target.matches(t)).unwrap_or(false);
        if !is_match { continue; }
        if let Some(t) = rq.get_mut(i) {
            t.priority = nice;
            t.weight   = weight;
            found = true;
        }
        rq.place_home(i);
    }
    found
}

/// Make `path` (host-absolute, normalised) the calling task's cwd.
pub fn set_cwd(path: &[u8]) -> bool {
    let new: Option<alloc::sync::Arc<[u8]>> =
        if path == b"/" || path.is_empty() { None } else { Some(alloc::sync::Arc::from(path)) };
    let pid = current_pid();
    let old;
    {
        let mut rq = RUN_QUEUE.lock();
        match rq.find_pid_mut(pid) {
            Some(t) => old = core::mem::replace(&mut t.cwd, new),
            None => return false,
        }
    }
    drop(old); // free outside RUN_QUEUE
    true
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
/// setpgid on a child that has already exited but is not yet reaped: only the
/// exit record remains (see `cred::set_pgid_checked`).
pub(crate) fn set_pgid_exited_child(pid: Pid, pgid: Pid, parent_tgid: Pid) -> bool {
    let mut found = false;
    EXIT_LOG.lock().for_each_pending(|rec| {
        if rec.pid == pid && rec.parent_tgid == parent_tgid {
            rec.pgid = pgid;
            found = true;
        }
    });
    found
}

pub fn set_pgid(pid: Pid, pgid: Pid) -> bool {
    let mut rq = RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) {
        t.pgid = pgid;
        return true;
    }
    let mut found = false;
    EXIT_LOG.lock().for_each_pending(|rec| {
        if rec.pid == pid {
            rec.pgid = pgid;
            found = true;
        }
    });
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

/// `prctl(PR_SET_NO_NEW_PRIVS, 1)` on the calling thread (one-way).
pub fn set_no_new_privs() {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) { t.no_new_privs = true; }
}

/// `prctl(PR_GET_NO_NEW_PRIVS)` of the calling thread.
pub fn no_new_privs() -> bool {
    RUN_QUEUE.lock().find_pid(current_pid()).map(|t| t.no_new_privs).unwrap_or(false)
}

/// The credential step of execve (Linux `bprm_fill_uid` + `commit_creds`):
/// a set-user-ID / set-group-ID binary makes its owner / group the effective
/// id (`None` keeps the current one), and the saved ids then follow the
/// effective ones, setuid binary or not. Called past execve's point of no
/// return, on the only thread left in the group.
pub fn exec_commit_ids(euid: Option<u32>, egid: Option<u32>) {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        if let Some(u) = euid { t.euid = u; }
        if let Some(g) = egid { t.egid = g; }
        t.suid = t.euid;
        t.sgid = t.egid;
    }
}

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
        t.set_state(TaskState::Running);
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

/// The mask of a park that waits only for its deadline or a signal (nanosleep,
/// sigsuspend, sigtimedwait): no object tag reaches it, only the broadcast
/// (`POLL_TAG_ALL`, which every signal delivery issues) and the deadline tick.
/// Such parks used to register `POLL_TAG_ALL` themselves, so every targeted
/// wake in the system (each socket write, eventfd, epoll_ctl) woke every
/// sleeping thread to re-check its clock: ~55 wakes/s for one `sleep 60` on
/// the idle greeter, the same for init's getty-loop sleep.
pub const POLL_MASK_TIMED_ONLY: u64 = 0;

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
    /// A parent parked in wait4/waitid, indexed by its tgid.
    pub const WAIT:    u32 = 11;
    /// inotify fds: nothing produces their events, so nothing wakes this tag.
    pub const INOTIFY: u32 = 12;
    /// An epoll instance itself, indexed by its slot: what `epoll_ctl` wakes.
    pub const EPOLL:   u32 = 13;
    /// A connected AF_UNIX end became readable (data or fds queued for it),
    /// indexed `conn * 2 + side` (side 0 = end A, 1 = end B).
    pub const UNIX_RD: u32 = 14;
    /// A connected AF_UNIX end gained write space (its peer drained), same
    /// index. Data edges wake only these, so a reader parked for POLLIN is
    /// not woken by its peer draining what it sent, nor by its own sends.
    pub const UNIX_WR: u32 = 15;
    /// A pidfd's process exited, indexed by its tgid (see `exit`).
    pub const PIDFD:   u32 = 16;
    /// A `/dev/netctl` open's event queue became readable, indexed by its
    /// VFS open id (servers/net/src/netctl).
    pub const NETCTL:  u32 = 17;
    /// GPU fence / syncobj waiters (DRM_IOCTL_SYNCOBJ_*WAIT, the backends'
    /// `wait_fences`, v3d BO waits): one object, index 0 — see
    /// `POLL_TAG_SYNCOBJ`.
    pub const SYNCOBJ: u32 = 18;
    /// A signalfd, indexed by the polling thread group. No producer wakes it
    /// by tag: every signal post (`deliver_signal*`, `try_deliver_signal*`)
    /// issues a BROADCAST poll wake, which reaches every non-zero mask, so a
    /// narrow tag loses nothing — it only stops the signalfd holder (every
    /// PipeWire/SPA loop) from being woken by every other wake in the system.
    pub const SIGNALFD: u32 = 19;
}

/// The tag every syncobj / GPU-fence waiter parks with and every fence
/// retirement, GPU completion interrupt and syncobj install/signal wakes. One
/// tag for all of them: a waiter re-probes its own fences, and the point is
/// only that a GPU completion stops waking the ~600 unrelated pollers.
pub const POLL_TAG_SYNCOBJ: u64 = poll_tag(poll_class::SYNCOBJ, 0);
/// The exact key of `POLL_TAG_SYNCOBJ` (same bit). GPU completion wakes are
/// keyed (`try_wake_poll_exclusive_key`): syncobj waiters park with the bare
/// bit (no fingerprints, so never refined — always reached), while a poller
/// that merely shares the bit through its own keyed or foreign interests is
/// skipped. The bit-only version woke ~14 tasks per GPU interrupt on the
/// laptop during video, nearly all of them bloom collisions (lane deskbusy2).
pub const POLL_KEY_SYNCOBJ: u64 = poll_key(poll_class::SYNCOBJ, 0);

/// Hash a `(class, index)` object identity into a single-bit tag. A collision
/// only makes an extra poller wake (harmless — it re-probes and re-parks); it
/// can never suppress a needed wake. splitmix64 finalizer for good bit spread
/// across the 64 buckets; `const fn` so IRQ/tick producers can call it.
#[inline]
pub const fn poll_tag(class: u32, index: u32) -> u64 {
    poll_key_bit(poll_key(class, index))
}

/// The exact identity of a poll object `(class, index)`: the full 64-bit
/// splitmix64 finalizer whose low 6 bits pick `poll_tag`'s bit. Carried by
/// keyed wakes (`wake_poll_key`) and keyed probe answers (`PollTag::keys`) so a
/// parked poller whose bloom mask happens to share the bit, but which never
/// named the object, is not woken. See `PollSet`.
#[inline]
pub const fn poll_key(class: u32, index: u32) -> u64 {
    let mut x = ((class as u64) << 32) | (index as u64);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^= x >> 33;
    x
}

/// The bloom bit of a `poll_key` (== `poll_tag` of the same object).
#[inline]
pub const fn poll_key_bit(key: u64) -> u64 { 1u64 << (key & 63) }

/// The fingerprint a parked task stores per keyed interest: 16 bits
/// independent of the bloom bit. A fingerprint collision only adds a wake
/// (odds `nfp / 65536` per refined candidate).
#[inline]
pub const fn poll_key_fp(key: u64) -> u16 { (key >> 32) as u16 }

// ── Exact keys on top of the bloom mask ─────────────────────────────────────
//
// WHY. A 64-bit bloom mask saturates: a tokio reactor watching N AF_UNIX
// sockets registers EPOLLIN|EPOLLOUT, i.e. three tags per socket (state, RD,
// WR). busd with ~40 clients sets ~55 of 64 bits, so ~85 % of every AF_UNIX
// send/recv anywhere in the system woke it to re-probe ~40 interests and park
// again. On the laptop, YouTube in Firefox (whose processes talk over AF_UNIX
// all the time) made busd, cosmic-panel, cosmic-session and
// cosmic-settings-daemon burn ~125 % of a CPU this way; /proc/wakestat showed
// 8-14 tasks woken per AF_UNIX send/recv that should wake at most one.
//
// HOW. Producers of the hottest objects (connected AF_UNIX data edges, pipes,
// eventfds) wake with the object's full `poll_key` (`wake_poll_key`); the probes
// of those objects answer with the same keys (`PollTag::keys`); a poller that
// parks with a `PollSet` records, besides the bloom mask, which bits are
// explained ONLY by keyed interests (`poll_kbits`) and the fingerprints of those
// keys. A keyed wake whose bit is in `poll_kbits` is delivered only on a
// fingerprint match.
//
// SAFETY (a lost wake is a hang): a bit is put in `kbits` only if EVERY
// interest that contributed it did so with a key; any untagged / bit-only
// contribution, overflow of `POLL_FP_MAX`, `widen_poll_masks`, or a legacy
// `block_on_poll_prepare_masked` park leaves the bit unrefined. Bit-only wakes
// (`wake_poll_tagged`) ignore the refinement entirely. So a keyed wake can only
// be filtered for a task none of whose interests names that object — provided
// a producer's key and the probe's key for the same object come from the same
// `poll_key(class, index)`, which is why the servers derive both from one
// helper per object kind.

/// data[33] of a VFS_POLL / NET_POLL reply whose data[34] key count and
/// data[40..64] keys are valid (see the kernel's `reply_poll_tag`).
pub const POLL_REPLY_KEYS: u8 = 0x4b;

/// data[33] of a VFS_POLL / NET_POLL reply (instead of `POLL_REPLY_KEYS`) whose
/// object is never the target of a keyed wake — see `PollTag::foreign`.
pub const POLL_REPLY_FOREIGN: u8 = 0x4c;

/// Write `keys` (<= 3) into a poll reply message's data (see `POLL_REPLY_KEYS`).
pub fn poll_reply_put_keys(data: &mut [u8], keys: &[u64]) {
    let n = keys.len().min(3);
    data[33] = POLL_REPLY_KEYS;
    data[34] = n as u8;
    for (i, k) in keys[..n].iter().enumerate() {
        data[40 + i * 8..48 + i * 8].copy_from_slice(&k.to_le_bytes());
    }
}

/// Fingerprints a parked task can hold; beyond this, keyed bits stay unrefined.
/// A connected AF_UNIX interest takes three (state, RD, WR), so 512 covers a
/// reactor with ~170 sockets; busd on the desktop holds ~45 (135 keys), which
/// overflowed the first cut's 128 and left it unrefined. 1 KiB per task.
pub const POLL_FP_MAX: usize = 512;

/// The targeted-wake answer of one probe: the bloom `bits` it contributes and,
/// optionally, up to three exact object keys explaining (some of) those bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PollTag {
    pub bits: u64,
    pub n:    u8,
    pub keys: [u64; 3],
    /// `slot + 1` when this answer is a nested epoll instance's (its `bits`
    /// then OR its interests' bits, keyless): the park collector expands it
    /// into the inner interests' own tags instead (see the kernel's
    /// `epoll_collect`). 0 = not an epoll fd.
    pub nested: u32,
    /// The object's readiness is never published by a keyed wake
    /// (`wake_poll_key`): only bit-only wakes (`wake_poll_tagged`, the
    /// timerfd/deadline tick, broadcasts) reach it. Its bits then need no
    /// key to be refined — see `PollSet::add`. Set only by probes that know
    /// their object's class (timerfd, signalfd, pty, pidfd, inotify, device
    /// servers, AF_UNIX listeners, an epoll instance's own tag).
    pub foreign: bool,
}

impl PollTag {
    /// Broadcast: no per-object identity.
    pub const ALL: PollTag = PollTag { bits: POLL_TAG_ALL, n: 0, keys: [0; 3], nested: 0, foreign: false };
    /// Contributes nothing (a dead registration).
    pub const NONE: PollTag = PollTag { bits: 0, n: 0, keys: [0; 3], nested: 0, foreign: false };
    /// Bloom bits only (no keys).
    pub const fn bits(bits: u64) -> PollTag { PollTag { bits, n: 0, keys: [0; 3], nested: 0, foreign: false } }
    /// Bloom bits of an object no keyed wake ever targets (see `foreign`).
    pub const fn foreign(bits: u64) -> PollTag { PollTag { bits, n: 0, keys: [0; 3], nested: 0, foreign: true } }
    /// Exact keys; `bits` is their bloom bits OR any extra unkeyed `bits`.
    pub fn keyed(keys: &[u64], extra_bits: u64) -> PollTag {
        let mut t = PollTag::bits(extra_bits);
        for &k in keys.iter().take(3) {
            t.keys[t.n as usize] = k;
            t.n += 1;
            t.bits |= poll_key_bit(k);
        }
        t
    }
}

/// An interest set being collected for a park: the bloom mask, the bits that
/// some interest contributed WITHOUT a key (never refined), and the keyed
/// interests' fingerprints. Lives on the parking thread's kernel stack.
pub struct PollSet {
    mask:    u64,
    unkeyed: u64,
    nfp:     usize,
    fps:     [u16; POLL_FP_MAX],
    /// Interests added with / without keys (diagnostics, `gdwake::note_park`).
    n_keyed:   u16,
    n_unkeyed: u16,
}

impl PollSet {
    pub const fn new() -> PollSet {
        PollSet { mask: 0, unkeyed: 0, nfp: 0, fps: [0; POLL_FP_MAX], n_keyed: 0, n_unkeyed: 0 }
    }
    /// Add one interest's probe answer.
    pub fn add(&mut self, t: &PollTag) {
        if t.foreign && t.n == 0 && t.nested == 0 && t.bits != POLL_TAG_ALL {
            self.add_foreign_bits(t.bits);
            return;
        }
        self.mask |= t.bits;
        let mut keyed = 0u64;
        for &k in &t.keys[..(t.n as usize).min(3)] {
            let b = poll_key_bit(k);
            if t.bits & b == 0 { continue; } // inconsistent answer: ignore the key
            if self.nfp < POLL_FP_MAX {
                self.fps[self.nfp] = poll_key_fp(k);
                self.nfp += 1;
                keyed |= b;
            }
        }
        self.unkeyed |= t.bits & !keyed;
        if keyed != 0 { self.n_keyed = self.n_keyed.saturating_add(1); }
        if t.bits & !keyed != 0 { self.n_unkeyed = self.n_unkeyed.saturating_add(1); }
    }
    /// Add bloom bits with no key (an epoll instance's own tag, a nested set).
    pub fn add_bits(&mut self, bits: u64) {
        self.mask |= bits;
        self.unkeyed |= bits;
        self.n_unkeyed = self.n_unkeyed.saturating_add(1);
    }
    /// Add the bits of an object no keyed wake targets (`PollTag::foreign`):
    /// they join the mask but not `unkeyed`, so a KEYED wake that lands on one
    /// of them is still filtered by fingerprint. Sound because keyed wakes are
    /// only ever issued for pipes, eventfds and connected AF_UNIX data edges,
    /// whose probes always carry their keys — a foreign object can only be
    /// signalled by a bit-only wake, and those ignore the refinement. Before
    /// lane deskbusy2 every such bit poisoned refinement: a typical iced/tokio
    /// thread (an eventfd + its epoll's own tag + a timerfd) had 2 unkeyed bits
    /// of ~3, so ~3 % of all AF_UNIX/pipe/eventfd wakes in the system woke it.
    pub fn add_foreign_bits(&mut self, bits: u64) {
        if bits == POLL_TAG_ALL { self.add_bits(bits); return; }
        self.mask |= bits;
        self.n_unkeyed = self.n_unkeyed.saturating_add(1);
    }
    /// `(keyed interests, unkeyed interests)` added so far.
    pub fn counts(&self) -> (u16, u16) { (self.n_keyed, self.n_unkeyed) }
    pub fn mask(&self) -> u64 { self.mask }
    /// The bits only keyed interests explain (see `Task::poll_kbits`).
    pub fn kbits(&self) -> u64 { self.mask & !self.unkeyed }
    pub fn fps(&self) -> &[u16] { &self.fps[..self.nfp] }
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
/// Phase 1 with a collected `PollSet` (bloom mask plus exact keys). A set
/// with no bits at all parks broadcast, like `block_on_poll_prepare_masked`'s
/// callers do with a zero mask, so it is never immune to every wake.
pub fn block_on_poll_prepare_set(deadline: u64, set: &PollSet) {
    let pid = current_pid();
    if gdwake::enabled() {
        let (k, u) = set.counts();
        gdwake::note_park(pid, current_tgid(), set.mask(), set.kbits(), k, u);
    }
    if set.mask() == 0 {
        RUN_QUEUE.lock().block_on_port_until(pid, POLL_WAIT_CHANNEL, deadline, POLL_TAG_ALL);
    } else {
        RUN_QUEUE.lock().block_on_port_until_keyed(pid, POLL_WAIT_CHANNEL, deadline,
            set.mask(), set.kbits(), set.fps());
    }
    if deadline != u64::MAX {
        register_poll_deadline(deadline);
    }
}
/// Undo a prepared poll-block (the re-probe found readiness or a signal).
pub fn block_on_poll_cancel()  { block_on_port_cancel() }
/// Phase 3: yield; woken by `wake_poll`, a signal, or the deadline tick.
pub fn block_on_poll_commit()  { block_on_port_commit() }

// ── epoll ready list: poll-wake listeners (lane epollready) ─────────────────
//
// WHY. A woken epoll_wait used to re-probe EVERY interest (a VFS_POLL or
// NET_POLL round trip each) and, finding nothing, re-probe them all again
// between prepare and commit before parking: ~2N probes per wake. On the
// laptop desktop during video that was the bulk of busd/cosmic-comp/panel
// system time (cosmic-comp 613 µs per epoll_pwait over ~41 interests).
//
// HOW. A thread in epoll_wait (and, between calls, a thread whose last
// epoll_wait left a valid snapshot) is a LISTENER: the wake scans record
// every poll wake that matches its interest filter — the same bloom mask /
// keyed-bit / fingerprint filter a parked poller uses — in a small per-task
// ring (`WakeRing`), whether the thread is parked, running, or blocked
// elsewhere. epoll_wait then re-probes only the interests the recorded wakes
// can concern (exact keys, or the bits of bit-only wakes); an empty ring after
// a real sleep (a wake path that does not record: signals, stop/continue)
// means "reason unknown" and re-probes everything.
//
// THE INVARIANT (a lost wake is a hang): from `poll_listen_begin` until
// `poll_listen_end` (or until a non-listener poll park replaces the filter
// — `block_on_port_until_keyed` ends listening), every `wake_poll*`,
// deadline-tick timerfd expiry and deferred wake whose filter matches is in
// the ring, or the ring's `tags` carries its bits (key overflow). Readers
// take the ring under RUN_QUEUE and `poll_listen_park` parks only if the
// ring is empty in the SAME hold, so a wake can never fall between "looked"
// and "slept".
//
// The filter covers the interests the thread has probed. One added after
// that (epoll_ctl) is covered by the next pass's diff of the interest list
// (EPOLL_CTL_GEN is bumped before the ADD's `widen_poll_masks`, and a parking
// listener re-checks it after parking); `widen_poll_masks` widens a PARKED
// listener's park mask only, never the filter itself (see
// `RunQueue::widen_port_masks` for why).

/// Exact keys a `WakeRing` holds before folding further keys into its bits.
pub const WAKE_RING_KEYS: usize = 8;
/// `WakeRing::flags`: the task's own poll deadline woke it.
pub const WAKE_DEADLINE: u8 = 1;

/// The wakes a listener has received since it last looked.
#[derive(Clone, Copy)]
pub struct WakeRing {
    pub keys:  [u64; WAKE_RING_KEYS],
    pub n:     u8,
    /// OR of bit-only wake tags (and of overflowed keys' bits), already
    /// narrowed to the listener's mask. `POLL_TAG_ALL` = everything.
    pub tags:  u64,
    pub flags: u8,
}

impl WakeRing {
    pub const EMPTY: WakeRing = WakeRing { keys: [0; WAKE_RING_KEYS], n: 0, tags: 0, flags: 0 };
    pub fn is_empty(&self) -> bool { self.n == 0 && self.tags == 0 && self.flags == 0 }
    pub fn keys(&self) -> &[u64] { &self.keys[..self.n as usize] }
    pub fn push_key(&mut self, k: u64) {
        if self.keys().contains(&k) { return; }
        if (self.n as usize) < WAKE_RING_KEYS { self.keys[self.n as usize] = k; self.n += 1; }
        else { self.tags |= poll_key_bit(k); }
    }
    pub fn push_tags(&mut self, t: u64) { self.tags |= t; }
}

/// Per-task listener state (`Task::listen`). The filter is `mask` / `kbits`
/// plus `Task::poll_fps[..nfp]` (shared with the park filter: a listener's
/// park installs exactly this filter, and any other keyed poll park ends the
/// listener before it overwrites `poll_fps`). `RunQueue::ls_mask` mirrors
/// `mask` for the scans' prefilter.
#[derive(Clone, Copy)]
pub struct PollListen {
    pub on:    bool,
    /// Bumped by every `poll_listen_begin`: a snapshot taken under another id
    /// (or after `poll_listen_end`) is stale.
    pub id:    u32,
    pub mask:  u64,
    pub kbits: u64,
    pub nfp:   u16,
    pub ring:  WakeRing,
}

impl PollListen {
    pub const OFF: PollListen = PollListen { on: false, id: 0, mask: 0, kbits: 0, nfp: 0, ring: WakeRing::EMPTY };
}

/// What `poll_listen_park` did.
pub enum ListenPark {
    /// Wakes were waiting: not parked; here they are (the ring is now empty).
    Taken(WakeRing),
    /// Parked (Blocked on the poll channel): commit or cancel as usual.
    Parked,
    /// Not listening (any more) under that id: the caller must fall back.
    Off,
}

/// Start listening with `set`'s filter (`None` = everything: used while the
/// first probe pass of a call has no filter yet). Clears the ring. Returns the
/// new listener id.
pub fn poll_listen_begin(set: Option<&PollSet>) -> u32 {
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    match set {
        Some(s) => rq.listen_begin(pid, s.mask(), s.kbits(), s.fps()),
        None    => rq.listen_begin(pid, POLL_TAG_ALL, 0, &[]),
    }
}

/// Replace the listener's filter with `set`'s (same id) and take the ring.
/// `None` when not listening under `id`.
pub fn poll_listen_update(id: u32, set: &PollSet) -> Option<WakeRing> {
    let pid = current_pid();
    RUN_QUEUE.lock().listen_update(pid, id, set.mask(), set.kbits(), set.fps())
}

/// Take the ring (`None` when not listening under `id`).
pub fn poll_listen_take(id: u32) -> Option<WakeRing> {
    let pid = current_pid();
    RUN_QUEUE.lock().listen_take(pid, id)
}

/// Park on the poll channel with the listener's filter, unless wakes are
/// already waiting (then take them instead) — one RUN_QUEUE hold.
pub fn poll_listen_park(id: u32, deadline: u64) -> ListenPark {
    let pid = current_pid();
    let r = RUN_QUEUE.lock().listen_park(pid, id, deadline);
    if matches!(r, ListenPark::Parked) && deadline != u64::MAX {
        register_poll_deadline(deadline);
    }
    r
}

/// Stop listening (the snapshot it served is gone or stale).
pub fn poll_listen_end() {
    let pid = current_pid();
    RUN_QUEUE.lock().listen_end(pid);
}

/// Entry of an epoll_wait: take the thread's epoll snapshot (`Task::ep_cache`)
/// and, when listening, its id and ring — one RUN_QUEUE hold.
pub fn poll_listen_enter() -> (Option<alloc::boxed::Box<dyn core::any::Any + Send>>, Option<(u32, WakeRing)>) {
    let pid = current_pid();
    let mut rq = RUN_QUEUE.lock();
    match rq.find_pid_mut(pid) {
        Some(t) => {
            let c = t.ep_cache.take();
            let l = if t.listen.on {
                let r = t.listen.ring;
                t.listen.ring = WakeRing::EMPTY;
                Some((t.listen.id, r))
            } else { None };
            (c, l)
        }
        None => (None, None),
    }
}

/// Store the thread's epoll snapshot back (see `poll_listen_enter`).
pub fn poll_listen_store(c: Option<alloc::boxed::Box<dyn core::any::Any + Send>>) {
    let pid = current_pid();
    let old = {
        let mut rq = RUN_QUEUE.lock();
        match rq.find_pid_mut(pid) {
            Some(t) => core::mem::replace(&mut t.ep_cache, c),
            None => c,
        }
    };
    drop(old); // freed outside RUN_QUEUE
}

/// Wake every task blocked on the poll wait-channel. Callers (edge publishers
/// in net/vfs, the deadline tick, signal delivery) MUST hold no server lock —
/// this takes RUN_QUEUE. Task context only (blocking lock); IRQ context uses
/// `try_wake_poll`.
#[track_caller]
pub fn wake_poll() { wake_poll_tagged(POLL_TAG_ALL); }

/// Wake every poll-channel waiter whose `poll_mask` intersects `tag`. A
/// producer passes the `poll_tag(class, index)` of the object it changed; an
/// unconvertible producer passes `POLL_TAG_ALL` (== `wake_poll`). Same lock /
/// context contract as `wake_poll`.
#[track_caller]
pub fn wake_poll_tagged(tag: u64) {
    let woken = {
        let mut rq = RUN_QUEUE.lock();
        gdwake::set_site(core::panic::Location::caller());
        let w = rq.unblock_port_tagged(POLL_WAIT_CHANNEL, tag);
        (w, wakestat::LAST_ALLMASK.load(Ordering::Relaxed))
    };
    let (woken, allmask) = woken;
    wakestat::note(core::panic::Location::caller(), tag != POLL_TAG_ALL, woken, allmask as usize);
    if idlestat::ENABLED {
        idlestat::note_wake(core::panic::Location::caller(), tag == POLL_TAG_ALL, woken);
    }
    if woken > 0 { wake_up_an_idle_cpu(); }
}

/// Keyed `wake_poll_tagged`: wake the pollers whose interest set names the
/// object `key` (`poll_key(class, index)`), skipping those whose bloom mask only
/// collides with its bit (see `PollSet`). Same lock / context contract.
#[track_caller]
pub fn wake_poll_key(key: u64) {
    let woken = {
        let mut rq = RUN_QUEUE.lock();
        gdwake::set_site(core::panic::Location::caller());
        let w = rq.unblock_port_keyed(POLL_WAIT_CHANNEL, key);
        (w, wakestat::LAST_ALLMASK.load(Ordering::Relaxed))
    };
    let (woken, allmask) = woken;
    wakestat::note(core::panic::Location::caller(), true, woken, allmask as usize);
    if idlestat::ENABLED {
        idlestat::note_wake(core::panic::Location::caller(), false, woken);
    }
    if woken > 0 { wake_up_an_idle_cpu(); }
}

/// Non-blocking `wake_poll_key` for IRQ / tick context; false = RUN_QUEUE was
/// contended (the caller defers with `request_poll_wake_tagged(poll_key_bit)`).
#[track_caller]
pub fn try_wake_poll_key(key: u64) -> bool {
    match RUN_QUEUE.try_lock_spin(TICK_LOCK_WAIT_NS) {
        Some(mut rq) => {
            lockwatch::note_wake_try(true);
            gdwake::set_site(core::panic::Location::caller());
            let woken = rq.unblock_port_keyed(POLL_WAIT_CHANNEL, key);
            let allmask = wakestat::LAST_ALLMASK.load(Ordering::Relaxed);
            drop(rq);
            wakestat::note(core::panic::Location::caller(), true, woken, allmask as usize);
            if woken > 0 { wake_up_an_idle_cpu(); }
            true
        }
        None => { lockwatch::note_wake_try(false); false }
    }
}

/// OR `add` into the interest mask of every task parked on the poll channel
/// whose mask intersects `match_tag`, without waking it. `sys_epoll_ctl` uses
/// it when it (re-)arms an interest that is not ready: the instance's parked
/// waiters (tag `poll_tag(EPOLL, slot)`) computed their masks without the new
/// fd, and must be reachable by that fd's own later wake. Widening a mask can
/// only add wakes, never lose one. Task context (takes RUN_QUEUE).
pub fn widen_poll_masks(match_tag: u64, add: u64) {
    if add == 0 { return; }
    RUN_QUEUE.lock().widen_port_masks(POLL_WAIT_CHANNEL, match_tag, add);
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

/// Deferred keyed wakes (lane deskbusy2): `request_poll_wake_key` parks the
/// exact key here so the tick pays it as a keyed wake. A bit-only deferral of
/// a keyed object woke every poller sharing its bloom bit — with RUN_QUEUE
/// contended, IRQ-context try-wakes (eventfd sync files, the net daemon kick,
/// GPU completions) deferred so often that the deferred-wake service was the
/// top source of spurious wakes on the laptop (~2 600/s during video).
const DEFERRED_KEYS: usize = 32;
static DEFERRED_KEY: [AtomicU64; DEFERRED_KEYS] = [const { AtomicU64::new(0) }; DEFERRED_KEYS];

/// `request_poll_wake_tagged(poll_key_bit(key))`, kept exact when there is
/// room (a full table falls back to the bit, which over-wakes, never loses).
pub fn request_poll_wake_key(key: u64) {
    // 0 marks a free slot: a (vanishingly unlikely) zero key goes by bit.
    if key == 0 { request_poll_wake_tagged(poll_key_bit(0)); return; }
    let k = key;
    let start = (k >> 6) as usize % DEFERRED_KEYS;
    for j in 0..DEFERRED_KEYS {
        let slot = &DEFERRED_KEY[(start + j) % DEFERRED_KEYS];
        match slot.compare_exchange(0, k, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return,
            Err(cur) if cur == k => return, // already pending
            Err(_) => {}
        }
    }
    request_poll_wake_tagged(poll_key_bit(k));
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
    for slot in DEFERRED_KEY.iter() {
        if slot.load(Ordering::Relaxed) == 0 { continue; }
        let k = slot.swap(0, Ordering::AcqRel);
        if k == 0 { continue; }
        if !try_wake_poll_key(k) {
            // Contended: keep it for the next tick (as a bit if its slot was
            // re-taken meanwhile — over-wakes, never loses).
            if slot.compare_exchange(0, k, Ordering::AcqRel, Ordering::Acquire).is_err() {
                POLL_WAKE_PENDING.fetch_or(poll_key_bit(k), Ordering::Release);
            }
            break;
        }
    }
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
#[track_caller]
pub fn try_wake_poll() -> bool { try_wake_poll_tagged(POLL_TAG_ALL) }

/// How long an IRQ-context poll wake may wait for `RUN_QUEUE` before deferring
/// to the next tick. Holds are sub-microsecond; 50 µs covers all but host
/// vCPU preemption of the holder, and is bounded, so no deadlock-freedom is
/// lost (see `lockwatch::TrackedMutex::try_lock_spin`).
const TICK_LOCK_WAIT_NS: u64 = 50_000;

/// Non-blocking `wake_poll_tagged` for IRQ / tick context.
#[track_caller]
pub fn try_wake_poll_tagged(tag: u64) -> bool {
    match RUN_QUEUE.try_lock_spin(TICK_LOCK_WAIT_NS) {
        Some(mut rq) => {
            lockwatch::note_wake_try(true);
            gdwake::set_site(core::panic::Location::caller());
            let woken = rq.unblock_port_tagged(POLL_WAIT_CHANNEL, tag);
            let allmask = wakestat::LAST_ALLMASK.load(Ordering::Relaxed);
            drop(rq);
            wakestat::note(core::panic::Location::caller(), tag != POLL_TAG_ALL, woken, allmask as usize);
            if idlestat::ENABLED {
                idlestat::note_wake(core::panic::Location::caller(), tag == POLL_TAG_ALL, woken);
            }
            if woken > 0 { wake_up_an_idle_cpu(); }
            true
        }
        None => { lockwatch::note_wake_try(false); false }
    }
}

/// Non-blocking EXCLUSIVE tagged wake: like `try_wake_poll_tagged`, but a
/// task parked with the broadcast mask (`POLL_TAG_ALL`) is NOT woken — only
/// tasks whose own mask names `tag`.
///
/// WHEN THIS IS SOUND (and it is not, in general — see the safety rule above
/// `POLL_TAG_ALL`). A broadcast-mask park means "my interest set holds
/// something I could not name, wake me for anything", and that is only a
/// promise worth keeping for tags whose producer might be what that unnamed
/// thing waits on. `POLL_TAG_SYNCOBJ` is not such a tag: the only parks that
/// wait on syncobj / GPU-fence state are the syncobj WAIT ioctls,
/// `wait_fences` and the v3d BO wait, and they all park with exactly
/// `POLL_TAG_SYNCOBJ`; nothing pollable has its readiness published by a
/// syncobj wake (a sync file is an eventfd signalled with its own EVENTFD tag,
/// DRM events use the open's DRM tag). Without this, every GPU submit and
/// retirement still woke every broadcast-mask poller in the system.
///
/// A contended RUN_QUEUE returns false; the caller defers with
/// `request_poll_wake_tagged` (which is paid non-exclusively: harmless).
#[track_caller]
pub fn try_wake_poll_exclusive(tag: u64) -> bool {
    match RUN_QUEUE.try_lock_spin(TICK_LOCK_WAIT_NS) {
        Some(mut rq) => {
            lockwatch::note_wake_try(true);
            gdwake::set_site(core::panic::Location::caller());
            let woken = rq.unblock_port_exclusive(POLL_WAIT_CHANNEL, tag);
            drop(rq);
            wakestat::note(core::panic::Location::caller(), true, woken, 0);
            if woken > 0 { wake_up_an_idle_cpu(); }
            true
        }
        None => { lockwatch::note_wake_try(false); false }
    }
}

/// `try_wake_poll_exclusive` as a keyed wake (`POLL_KEY_SYNCOBJ`): also skips
/// tasks whose bit is explained only by other objects' keys or foreign tags.
#[track_caller]
pub fn try_wake_poll_exclusive_key(key: u64) -> bool {
    match RUN_QUEUE.try_lock_spin(TICK_LOCK_WAIT_NS) {
        Some(mut rq) => {
            lockwatch::note_wake_try(true);
            gdwake::set_site(core::panic::Location::caller());
            let woken = rq.unblock_port_exclusive_keyed(POLL_WAIT_CHANNEL, key);
            drop(rq);
            wakestat::note(core::panic::Location::caller(), true, woken, 0);
            if woken > 0 { wake_up_an_idle_cpu(); }
            true
        }
        None => { lockwatch::note_wake_try(false); false }
    }
}

/// Poll-deadline tick service (IRQ/tick context): wake every poll-channel
/// waiter whose per-task deadline is due, or whose interest mask names a
/// timerfd that expired on this pass (`timerfd_tags`),
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
pub fn service_poll_deadlines(now: u64, timerfd_tags: u64) -> bool {
    service_poll_deadlines_keyed(now, timerfd_tags, &WakeKeys::new())
}

/// The exact keys of the timerfds that expired on one deadline-tick pass
/// (see `service_poll_deadlines_keyed`). Past `WAKE_KEYS_MAX` it marks
/// itself overflowed and the pass wakes by bit, as before.
pub struct WakeKeys {
    keys: [u64; WAKE_KEYS_MAX],
    n: usize,
    overflow: bool,
}
const WAKE_KEYS_MAX: usize = 16;

impl WakeKeys {
    pub const fn new() -> Self { WakeKeys { keys: [0; WAKE_KEYS_MAX], n: 0, overflow: false } }
    pub fn push(&mut self, k: u64) {
        if self.n < WAKE_KEYS_MAX { self.keys[self.n] = k; self.n += 1; } else { self.overflow = true; }
    }
    /// The keys, if they are the complete set (else `None`: wake by bit).
    pub fn exact(&self) -> Option<&[u64]> {
        if self.overflow || self.n == 0 { None } else { Some(&self.keys[..self.n]) }
    }
}

/// `service_poll_deadlines` whose timerfd expiry wake is keyed: a poller whose
/// timerfd bits are explained only by keyed/foreign interests is woken only
/// if it holds one of `keys`' fingerprints (lane deskbusy2: ~270 expiries/s
/// on the laptop desktop woke every poller sharing one of their bits).
pub fn service_poll_deadlines_keyed(now: u64, timerfd_tags: u64, keys: &WakeKeys) -> bool {
    match RUN_QUEUE.try_lock_spin(TICK_LOCK_WAIT_NS) {
        Some(mut rq) => {
            lockwatch::note_tick_try(lockwatch::L_RUN_QUEUE, true);
            let (new_min, woken) =
                rq.wake_due_poll_deadlines_keyed(POLL_WAIT_CHANNEL, now, timerfd_tags, keys.exact());
            NEXT_POLL_DEADLINE.store(new_min, Ordering::Relaxed); // exact, under the lock
            drop(rq);
            if woken > 0 {
                wakestat::note(core::panic::Location::caller(), timerfd_tags != POLL_TAG_ALL, woken, 0);
            }
            if idlestat::ENABLED && woken > 0 {
                idlestat::note_wake(core::panic::Location::caller(), timerfd_tags == POLL_TAG_ALL, woken);
            }
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
    let now = monotonic_ns();
    // High-resolution timers share the per-CPU one-shot: service them first,
    // then fold their next expiry into what the arch re-arms for.
    let next_hr = hrtimer::service(now);
    let hook = DEADLINE_HOOK.load(Ordering::Acquire);
    if hook == 0 { return next_hr; }
    let f: fn(u64) -> u64 = unsafe { core::mem::transmute(hook) };
    f(now).min(next_hr)
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
    for i in rq.live_slots() {
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
    for i in rq.live_slots() {
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
//
// A map of heap paths, so memory follows the live processes and the lengths
// of their paths: until lane fdgaps it was a static array of 264-byte entries
// sized by MAX_PROCESSES (536 KiB of .bss at 2048 processes).
// PATH_MAX, as the kernel's path layer allows since lane chrootgaps: a deeper
// executable must not read back from /proc/self/exe cut short.
const EXE_PATH_MAX: usize = 4096;
struct ExePaths {
    map: alloc::collections::BTreeMap<Pid, alloc::boxed::Box<[u8]>>,
}
impl ExePaths {
    #[inline]
    fn get(&self, tgid: Pid) -> Option<&[u8]> {
        if tgid == 0 { return None; }
        self.map.get(&tgid).map(|b| &b[..])
    }
}
/// Diagnostics read this with `try_lock` from IRQ context. Path buffers are
/// built before the lock is taken and replaced or removed ones are freed
/// after it is dropped; the only allocation under it is a map node.
static EXE_PATHS: Mutex<ExePaths> = Mutex::new(ExePaths { map: alloc::collections::BTreeMap::new() });

/// Record `tgid`'s executable path (bytes truncated to EXE_PATH_MAX). Replaces
/// any existing entry for the tgid.
pub fn set_exe_path(tgid: Pid, bytes: &[u8]) {
    if tgid == 0 { return; }
    clear_comm(tgid); // exec resets the thread name to the new image's basename
    let n = bytes.len().min(EXE_PATH_MAX);
    let path: alloc::boxed::Box<[u8]> = bytes[..n].into();
    let old = EXE_PATHS.lock().map.insert(tgid, path);
    drop(old);
}

/// Copy `tgid`'s executable path into `out`; returns its length, or None if
/// unset (caller falls back to "/bin/init").
pub fn exe_path(tgid: Pid, out: &mut [u8]) -> Option<usize> {
    let t = EXE_PATHS.lock();
    let e = t.get(tgid)?;
    let n = e.len().min(out.len());
    out[..n].copy_from_slice(&e[..n]);
    Some(n)
}

/// `exe_path` for diagnostics that must not wait (hung-task detector):
/// try-locks the table, gives up (None) when it is busy.
pub fn exe_path_try(tgid: Pid, out: &mut [u8]) -> Option<usize> {
    let t = EXE_PATHS.try_lock()?;
    let e = t.get(tgid)?;
    let n = e.len().min(out.len());
    out[..n].copy_from_slice(&e[..n]);
    Some(n)
}

/// Processes with a recorded executable path (diagnostics; 0 when busy).
pub fn exe_path_count() -> usize {
    EXE_PATHS.try_lock().map_or(0, |t| t.map.len())
}

/// A task as the hung-task detector reports it.
pub struct TaskBrief {
    pub tgid: Pid,
    /// R running/ready, S blocked, T stopped, Z zombie, ? run queue busy.
    pub state: u8,
    pub on_cpu: Option<usize>,
    pub blocked_on: Option<u32>,
}

/// Snapshot of `pid` without waiting on the run queue for long (a bounded
/// number of try-locks). None: no such task. A busy run queue yields state '?'.
pub fn task_brief(pid: Pid) -> Option<TaskBrief> {
    for _ in 0..1000 {
        if let Some(rq) = RUN_QUEUE.try_lock() {
            let t = rq.find_pid(pid)?;
            let state = match t.state {
                TaskState::Zombie => b'Z',
                TaskState::Stopped => b'T',
                TaskState::Blocked => b'S',
                TaskState::Ready | TaskState::Running => b'R',
            };
            return Some(TaskBrief { tgid: t.tgid, state, on_cpu: t.on_cpu, blocked_on: t.blocked_on });
        }
        core::hint::spin_loop();
    }
    Some(TaskBrief { tgid: pid, state: b'?', on_cpu: None, blocked_on: None })
}

// ── Thread names (/proc/<tid>/comm, prctl PR_SET_NAME/PR_GET_NAME) ──────────
//
// A tid-keyed side table of explicitly set names (15 bytes + length). Unset
// means "basename of the thread group's executable", the Linux default after
// exec. Cleared on exec (`set_exe_path`) and task exit.
static COMMS: Mutex<alloc::collections::BTreeMap<Pid, ([u8; 15], u8)>> =
    Mutex::new(alloc::collections::BTreeMap::new());

/// Set thread `tid`'s name (truncated to 15 bytes, stops at NUL).
pub fn set_comm(tid: Pid, name: &[u8]) {
    let name = name.split(|&b| b == 0).next().unwrap_or(&[]);
    let n = name.len().min(15);
    let mut b = [0u8; 15];
    b[..n].copy_from_slice(&name[..n]);
    COMMS.lock().insert(tid, (b, n as u8));
}

pub fn clear_comm(tid: Pid) {
    COMMS.lock().remove(&tid);
}

/// Thread `tid`'s name into `out` (<= 15 bytes); returns the length.
pub fn comm_of(tid: Pid, out: &mut [u8; 15]) -> usize {
    let set = COMMS.lock().get(&tid).copied();
    if let Some((b, n)) = set {
        *out = b;
        return n as usize;
    }
    let mut exe = [0u8; 256];
    let tgid = tgid_of(tid);
    let n = exe_path(tgid, &mut exe).unwrap_or(0);
    let path: &[u8] = if n > 0 {
        &exe[..n]
    } else if tgid == 1 || !is_kernel_task(tid) {
        b"/bin/init" // the boot-loaded PID 1 never execs
    } else {
        b"kthread" // an unnamed kernel task (named ones call set_comm)
    };
    // `kthread::spawn` records "[name]" as the exe path; comm is the name.
    // (Some callers pass a name already in brackets: "[[netctl]]".)
    let mut path = path;
    while let Some(inner) = path.strip_prefix(b"[").and_then(|p| p.strip_suffix(b"]")) { path = inner; }
    let base = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    let m = base.len().min(15);
    out[..m].copy_from_slice(&base[..m]);
    m
}

// ── argv / envp areas (/proc/<pid>/cmdline, /proc/<pid>/environ) ────────────
//
// `sys_execve` records where it laid the argv and envp strings out on the new
// user stack (Linux's mm->arg_start..arg_end, env_start..env_end); procfs
// reads them back through the target's address space, so a setproctitle-style
// rewrite of argv in place shows up. tgid-keyed like EXE_PATHS: inherited by
// fork, cleared at leader exit, replaced by the next exec.
#[derive(Clone, Copy)]
pub struct ArgArea { pub arg_start: usize, pub arg_end: usize, pub env_start: usize, pub env_end: usize }
static ARG_AREAS: Mutex<alloc::collections::BTreeMap<Pid, ArgArea>> =
    Mutex::new(alloc::collections::BTreeMap::new());

pub fn set_arg_area(tgid: Pid, a: ArgArea) {
    if tgid == 0 { return; }
    ARG_AREAS.lock().insert(tgid, a);
}

/// Read `pid`'s argv (or, with `environ`, envp) strings as procfs shows them,
/// at most `cap` bytes, into `out`. `None`: a kernel task or no such task
/// (an empty file, as on Linux); `Some(false)`: a user process with no
/// recorded area (the boot-loaded PID 1 never exec'd). Linux's setproctitle
/// rule: when the argv area no longer ends in NUL, the title runs on into the
/// env area up to its first NUL.
pub fn read_proc_args(pid: Pid, environ: bool, cap: usize, out: &mut alloc::vec::Vec<u8>) -> Option<bool> {
    let area = ARG_AREAS.lock().get(&tgid_of(pid)).copied();
    let area = match area {
        Some(a) => a,
        None => return if is_kernel_task(pid) || proc_stat_of(pid).is_none() { None } else { Some(false) },
    };
    // Linux `get_mm_cmdline`: an empty or inverted argv area reads empty, and
    // the title may only run on past arg_end when the env area starts right
    // there (prctl(PR_SET_MM) can move either one anywhere).
    if !environ && area.arg_start >= area.arg_end { out.clear(); return Some(true); }
    let env_follows = area.env_start == area.arg_end && area.env_end >= area.env_start;
    let (lo, hi) = if environ { (area.env_start, area.env_end) }
                   else { (area.arg_start, if env_follows { area.env_end } else { area.arg_end }) };
    let want = hi.saturating_sub(lo).min(cap);
    out.clear();
    out.resize(want, 0); // allocated before the address-space `busy` flag is taken
    let got = with_process_as(pid, |a| {
        let mut done = 0usize;
        while done < want {
            let va = lo + done;
            let chunk = (mm::buddy::PAGE_SIZE - va % mm::buddy::PAGE_SIZE).min(want - done);
            if !a.read_user_buf(va, &mut out[done..done + chunk]) { break; }
            done += chunk;
        }
        done
    });
    let got = match got { Some((g, _)) => g, None => return None };
    out.truncate(got);
    if !environ {
        let alen = area.arg_end.saturating_sub(area.arg_start).min(out.len());
        if alen > 0 && out[alen - 1] == 0 {
            out.truncate(alen);
        } else {
            // setproctitle: the title may run past arg_end; it ends at a NUL.
            let e = out.iter().position(|&b| b == 0).map_or(out.len(), |i| i + 1);
            out.truncate(e);
        }
    }
    Some(true)
}

/// Which field of the argv/envp area `prctl(PR_SET_MM, ...)` sets.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MmArgField { ArgStart, ArgEnd, EnvStart, EnvEnd }

/// `prctl(PR_SET_MM, PR_SET_MM_{ARG,ENV}_{START,END}, addr)` on the calling
/// process, with Linux's `prctl_set_mm` rules: CAP_SYS_RESOURCE (here: euid
/// 0) or EPERM; `addr` below `mmap_min_addr` (4096) or outside user space is
/// EINVAL; the area must stay ordered (start <= end, the other bounds as
/// they are now) or EINVAL; and some VMA must end above `addr` (Linux
/// `find_vma`) or EFAULT. The new bounds are what /proc/<pid>/cmdline and
/// environ read from.
pub fn set_mm_arg_field(field: MmArgField, addr: usize) -> isize {
    const MMAP_MIN_ADDR: usize = 4096;
    const TASK_SIZE: usize = 0x0000_8000_0000_0000;
    if current_euid() != 0 { return -1; } // EPERM
    if addr >= TASK_SIZE || addr < MMAP_MIN_ADDR { return -22; }
    let tgid = current_tgid();
    let mut a = ARG_AREAS.lock().get(&tgid).copied()
        .unwrap_or(ArgArea { arg_start: 0, arg_end: 0, env_start: 0, env_end: 0 });
    match field {
        MmArgField::ArgStart => a.arg_start = addr,
        MmArgField::ArgEnd => a.arg_end = addr,
        MmArgField::EnvStart => a.env_start = addr,
        MmArgField::EnvEnd => a.env_end = addr,
    }
    if a.arg_start > a.arg_end || a.env_start > a.env_end { return -22; }
    let mapped = with_current_address_space(|as_| {
        as_.regions.iter().filter_map(|r| r.as_ref()).any(|r| r.end > addr)
    }).unwrap_or(false);
    if !mapped { return -14; } // EFAULT
    ARG_AREAS.lock().insert(tgid, a);
    0
}

/// Release `tgid`'s executable-path slot (process leader exit).
pub fn clear_exe_path(tgid: Pid) {
    if tgid == 0 { return; }
    ARG_AREAS.lock().remove(&tgid);
    let old = EXE_PATHS.lock().map.remove(&tgid);
    drop(old); // freed outside the lock
}

/// Child (a new tgid) inherits the parent's exe path until it execs.
pub fn inherit_exe_path(parent_tgid: Pid, child_tgid: Pid) {
    if child_tgid == 0 { return; }
    let c = COMMS.lock().get(&parent_tgid).copied();
    if let Some(c) = c { COMMS.lock().insert(child_tgid, c); }
    let a = ARG_AREAS.lock().get(&parent_tgid).copied();
    if let Some(a) = a { ARG_AREAS.lock().insert(child_tgid, a); }
    // Copied on the heap: a PATH_MAX buffer is too much for a kernel stack.
    let path = EXE_PATHS.lock().get(parent_tgid).map(alloc::boxed::Box::<[u8]>::from);
    if let Some(path) = path {
        let old = EXE_PATHS.lock().map.insert(child_tgid, path);
        drop(old);
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
    for i in rq.live_slots() {
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
                percpu::pexit_bump();
                let a = group_acct(&rq, pid);
                credit_child(&mut rq, caller_tgid, a);
            }
            return WaitTry::Reaped(pid, status);
        }

        // Phase 2: already-recycled children in the exit log (still under the
        // run-queue lock, so a concurrent reap cannot slip between the phases).
        {
            let mut log = EXIT_LOG.lock();
            let mut hit = None;
            for i in log.zombie_slots() {
                if let Some(e) = &log.zombies[i] {
                    if e.parent_tgid == caller_tgid && matches(e.pid, e.pgid) {
                        hit = Some((i, e.pid, e.status, e.acct));
                        break;
                    }
                }
            }
            if let Some((i, pid, status, acct)) = hit {
                if consume {
                    log.consume_zombie(i);
                    drop(log);
                    credit_child(&mut rq, caller_tgid, acct);
                }
                return WaitTry::Reaped(pid, status);
            }
            if log.spilled {
                for entry in log.recent.iter_mut().filter_map(|e| e.as_mut()) {
                    if entry.consumed || !entry.is_process { continue; }
                    if entry.parent_tgid != caller_tgid { continue; }
                    if matches(entry.pid, entry.pgid) {
                        let (pid, status, acct) = (entry.pid, entry.status, entry.acct);
                        if consume {
                            entry.consumed = true;
                            percpu::pexit_bump();
                            drop(log);
                            credit_child(&mut rq, caller_tgid, acct);
                        }
                        return WaitTry::Reaped(pid, status);
                    }
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

/// `(cpu_ns, sys_ns)` of every live task of a thread group, summed (see
/// `process_cpu_ns`; `sys_ns` is the in-syscall part, `sysacct`).
pub fn process_cpu_sys_ns(tgid: Pid) -> (u64, u64) {
    let rq = RUN_QUEUE.lock();
    // Threads already reaped are kept on the leader (`exit_acct`).
    let (mut c, mut s) = rq.find_pid(tgid).map_or((0, 0), |l| (l.dead_cpu_ns, l.dead_sys_ns));
    for i in rq.live_slots() {
        if let Some(t) = rq.get(i) {
            if t.tgid == tgid { c = c.saturating_add(t.cpu_ns); s = s.saturating_add(t.sys_ns); }
        }
    }
    (c, s)
}

/// Totals of the children `tgid` has waited for (`cutime`/`cstime`/
/// `cminflt`/`cmajflt`; see [`ChildAcct`]).
pub fn process_child_acct(tgid: Pid) -> ChildAcct {
    RUN_QUEUE.lock().find_pid(tgid).map_or(ChildAcct::default(), |l| ChildAcct {
        cpu_ns: l.c_cpu_ns, sys_ns: l.c_sys_ns, minflt: l.c_minflt, majflt: l.c_majflt })
}

/// `(minflt, majflt)` of `pid`'s address space (no walk, no `busy`).
pub fn process_faults(pid: Pid) -> (u64, u64) {
    let rq = RUN_QUEUE.lock();
    let Some(t) = rq.find_pid(pid) else { return (0, 0) };
    let a = t.address_space.as_ref().or_else(|| rq.find_pid(t.tgid).and_then(|l| l.address_space.as_ref()));
    a.map_or((0, 0), |a| (a.min_flt.load(Ordering::Relaxed), a.maj_flt.load(Ordering::Relaxed)))
}

/// `monotonic_ns()` when task `pid` (a thread id) was created.
pub fn task_start_ns(pid: Pid) -> Option<u64> {
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.start_ns)
}

/// Every live task's `(pid, tgid, cpu_ns, sys_ns)`, appended to `out` up to
/// its spare capacity (reserve first: nothing is allocated under the lock).
pub fn task_times(out: &mut alloc::vec::Vec<(Pid, Pid, u64, u64)>) {
    let rq = RUN_QUEUE.lock();
    for i in rq.live_slots() {
        if out.len() == out.capacity() { break; }
        if let Some(t) = rq.get(i) { out.push((t.pid, t.tgid, t.cpu_ns, t.sys_ns)); }
    }
}

/// One `/proc/sysstat` `w` line: where a live task waits.
#[derive(Clone, Copy)]
pub struct TaskWait {
    pub pid: Pid,
    pub tgid: Pid,
    /// `R` ready, `C` on a CPU, `S` blocked, `T` stopped, `Z` zombie.
    pub state: u8,
    /// The syscall it was switched out in (`NO_SYSCALL`: none, or on a CPU).
    pub sc: u32,
    /// Wall-clock ns since that syscall's entry (0 without one).
    pub in_sc_ns: u64,
    /// The IPC port it is blocked on (0: none).
    pub port: u32,
    /// Parked on the poll listener.
    pub listen: bool,
    pub poll_mask: u64,
}

/// Where every live task waits, for `/proc/sysstat`'s `w` lines; appended to
/// `out` up to its spare capacity (reserve first: nothing is allocated under
/// the lock).
pub fn task_waits(out: &mut alloc::vec::Vec<TaskWait>) {
    let now = monotonic_ns();
    let rq = RUN_QUEUE.lock();
    for i in rq.live_slots() {
        if out.len() == out.capacity() { break; }
        if let Some(t) = rq.get(i) {
            let state = match t.state {
                TaskState::Zombie => b'Z', TaskState::Stopped => b'T', TaskState::Blocked => b'S',
                TaskState::Ready => b'R', TaskState::Running => b'C',
            };
            let in_sc_ns = if t.sc_saved_nr != NO_SYSCALL && t.sc_saved_t0 != 0 {
                now.saturating_sub(t.sc_saved_t0)
            } else { 0 };
            out.push(TaskWait { pid: t.pid, tgid: t.tgid, state, sc: t.sc_saved_nr, in_sc_ns,
                port: t.blocked_on.unwrap_or(0), listen: t.listen.on, poll_mask: t.poll_mask });
        }
    }
}

/// `(cpu_ns, sys_ns)` of one task, zeros if unknown.
pub fn thread_cpu_sys_ns(pid: Pid) -> (u64, u64) {
    RUN_QUEUE.lock().find_pid(pid).map(|t| (t.cpu_ns, t.sys_ns)).unwrap_or((0, 0))
}

/// CPU nanoseconds consumed by every live task of a thread group. Exited
/// threads are not folded in (no accounting survives a slot's recycling), so
/// this is a floor for a group that has lost threads.
pub fn process_cpu_ns(tgid: Pid) -> u64 {
    let rq = RUN_QUEUE.lock();
    let mut total = 0u64;
    for i in rq.live_slots() {
        if let Some(t) = rq.get(i) {
            if t.tgid == tgid { total = total.saturating_add(t.cpu_ns); }
        }
    }
    total
}

/// When process `tgid` was created, in `monotonic_ns()` nanoseconds: its
/// thread-group leader's `start_ns` (Linux `task->group_leader->start_time`).
/// None when no task of that pid exists.
pub fn process_start_ns(tgid: Pid) -> Option<u64> {
    RUN_QUEUE.lock().find_pid(tgid).map(|t| t.start_ns)
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
pub fn syscall_on_cpu(cpu: usize) -> u32 { CUR_SYSCALL[cpu.min(MAX_CPUS - 1)].load(Ordering::Relaxed) }
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
    sysacct::enter(id);
}
/// Syscall exit; `ret` is the value going back to user space (the census
/// counts returns of 0 per syscall, see `sysacct`).
#[inline]
pub fn note_syscall_exit(ret: isize) {
    let id = unsafe { cpu_id() }.min(MAX_CPUS - 1);
    let nr = CUR_SYSCALL[id].swap(NO_SYSCALL, Ordering::Relaxed);
    if nr != NO_SYSCALL {
        sysacct::exit(id, CURRENT_TGID[id].load(Ordering::Relaxed), nr, ret);
    }
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

/// Set by reboot(2) (kernel/src/power.rs) once the machine is being halted,
/// powered off or reset: the CPU doing it masks its interrupts for good, so
/// the stall watchdog must stop reporting it as wedged.
static SYSTEM_DOWN: AtomicBool = AtomicBool::new(false);
pub fn mark_system_down() { SYSTEM_DOWN.store(true, Ordering::Release); }

// ── NMI stall probe (x86_64) ─────────────────────────────────────────────────
//
// A CPU with IRQs masked cannot be asked anything, except through an NMI.
// When the watchdog reports a stalled CPU it sends it one (`arch_send_nmi`;
// a no-op on AArch64); the NMI handler (arch idt.rs `exc_nmi`) records the
// interrupted RIP, RSP, RFLAGS, CS and up to eight frame-pointer return
// addresses here, and the observer prints them under the `[WDOG]` line:
//   [WDOG]   cpu2 NMI sample: kernel RIP=0x... RSP=0x... IF=0 bt: 0x... 0x...
// which names the code that holds the CPU deaf (resolve with
// `llvm-addr2line -fe <kernel ELF>` or `nm -n`). Lane nvstall, 2026-10-09.
static NMI_REQ: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static NMI_SEQ: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// rip, rsp, rflags, cs, n_bt, bt[0..8]
static NMI_DATA: [[AtomicU64; 13]; MAX_CPUS] = [const { [const { AtomicU64::new(0) }; 13] }; MAX_CPUS];

/// NMI handler: was this NMI the watchdog's probe for `cpu`? (Consumes it.)
pub fn nmi_probe_take(cpu: usize) -> bool {
    NMI_REQ[cpu.min(MAX_CPUS - 1)].swap(false, Ordering::AcqRel)
}

/// NMI handler: publish where `cpu` was. Atomics only.
pub fn nmi_probe_record(cpu: usize, rip: u64, rsp: u64, rflags: u64, cs: u64, bt: &[u64]) {
    let c = cpu.min(MAX_CPUS - 1);
    let d = &NMI_DATA[c];
    d[0].store(rip, Ordering::Relaxed);
    d[1].store(rsp, Ordering::Relaxed);
    d[2].store(rflags, Ordering::Relaxed);
    d[3].store(cs, Ordering::Relaxed);
    let n = bt.len().min(8);
    d[4].store(n as u64, Ordering::Relaxed);
    for i in 0..n { d[5 + i].store(bt[i], Ordering::Relaxed); }
    NMI_SEQ[c].fetch_add(1, Ordering::Release);
}

/// Ask stalled `cpu` for an NMI sample and print it (watchdog context, raw
/// UART). Waits at most ~2 ms for the sample.
fn nmi_probe(cpu: usize) {
    extern "C" {
        fn arch_send_nmi(cpu: usize) -> bool;
        fn arch_serial_putc(c: u8);
        fn print_number(n: u32);
        fn print_hex(n: usize);
    }
    fn s(msg: &str) { for &b in msg.as_bytes() { unsafe { arch_serial_putc(b) } } }
    let seq0 = NMI_SEQ[cpu].load(Ordering::Acquire);
    NMI_REQ[cpu].store(true, Ordering::Release);
    if !unsafe { arch_send_nmi(cpu) } {
        NMI_REQ[cpu].store(false, Ordering::Release);
        return;
    }
    let t0 = monotonic_ns();
    while NMI_SEQ[cpu].load(Ordering::Acquire) == seq0 {
        if monotonic_ns().wrapping_sub(t0) > 2_000_000 {
            NMI_REQ[cpu].store(false, Ordering::Release);
            s("[WDOG]   cpu"); unsafe { print_number(cpu as u32) }; s(" NMI sample: no answer in 2 ms\n");
            return;
        }
        core::hint::spin_loop();
    }
    let d = &NMI_DATA[cpu];
    let cs = d[3].load(Ordering::Relaxed);
    s("[WDOG]   cpu"); unsafe { print_number(cpu as u32) };
    s(if cs & 3 == 0 { " NMI sample: kernel RIP=" } else { " NMI sample: USER RIP=" });
    unsafe { print_hex(d[0].load(Ordering::Relaxed) as usize) };
    s(" RSP="); unsafe { print_hex(d[1].load(Ordering::Relaxed) as usize) };
    s(if d[2].load(Ordering::Relaxed) & (1 << 9) != 0 { " IF=1" } else { " IF=0" });
    let n = d[4].load(Ordering::Relaxed).min(8) as usize;
    if n != 0 {
        s(" bt:");
        for i in 0..n { s(" "); unsafe { print_hex(d[5 + i].load(Ordering::Relaxed) as usize) }; }
    }
    s("\n");
}

/// `wdog.selftest=1` (kernel command line): cpu1 stalls once, IRQs masked,
/// at its 3000th local tick (~30 s after boot) until the watchdog has
/// reported it (at most 20 s; the watchdog counts the observers' timer
/// interrupts, which run slower than real time under TCG), so the boot log
/// shows what a real stall report looks like — the `[WDOG]` line and, on
/// x86_64, an NMI sample whose RIP is in `wdog_selftest_stall`. 0 = off.
static WDOG_SELFTEST_TICK: AtomicU32 = AtomicU32::new(0);
static WDOG_SELFTEST_SEEN: AtomicBool = AtomicBool::new(false);
pub fn wdog_selftest_arm() { WDOG_SELFTEST_TICK.store(3000, Ordering::Relaxed); }

#[inline(never)]
fn wdog_selftest_stall() {
    extern "C" { fn arch_serial_putc(c: u8); }
    for &b in b"\n[WDOG] selftest: cpu1 stalls with IRQs masked until reported (wdog.selftest=1)\n" {
        unsafe { arch_serial_putc(b) }
    }
    let t0 = monotonic_ns();
    while !WDOG_SELFTEST_SEEN.load(Ordering::Acquire) && monotonic_ns().wrapping_sub(t0) < 20_000_000_000 {
        core::hint::spin_loop();
    }
}

fn watchdog_scan(me: usize) {
    if SYSTEM_DOWN.load(Ordering::Acquire) { return; }
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
            if let Some(e) = t.get(tgid) {
                s(" ("); for &b in e { unsafe { arch_serial_putc(b) } } s(")");
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
        // Where exactly it is (x86_64: an NMI gets through masked IRQs).
        nmi_probe(j);
        if j == 1 { WDOG_SELFTEST_SEEN.store(true, Ordering::Release); }
    }
}

/// Emit the periodic `[TLBSTAT]` line. Off by default: raw UART writes from
/// the timer IRQ interleave with the serial console and break harnesses that
/// expect the shell prompt at the end of output.
const TLBSTAT_PRINT: bool = false;

/// `[TLBSTAT]` period: 10 s of 100 Hz ticks.
const TLBSTAT_PERIOD_TICKS: u64 = 1000;

/// Print the TLB-shootdown / CoW-promotion counters of `mm::paging::tlbstat`
/// as deltas over the last period, when there was real activity. BSP timer IRQ
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
    for i in 0..K {
        let c = cur[i].load(Ordering::Relaxed);
        d[i] = c.wrapping_sub(PREV[i].swap(c, Ordering::Relaxed));
    }
    // Quiet on an idle system (a desktop still does a few flushes per
    // period): only periods with an exec, an ack timeout or a burst of CoW
    // copies are printed.
    if d[9] == 0 && d[4] == 0 && d[6] < 64 { return; }
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
    if id == 1 && mine == WDOG_SELFTEST_TICK.load(Ordering::Relaxed) {
        wdog_selftest_stall();
    }
    // Every CPU has its own local timer; only the BSP advances global time so
    // TIMER_TICKS keeps its 100 Hz meaning regardless of CPU count.
    if id == 0 {
        let before = TIMER_TICKS.fetch_add(elapsed, Ordering::Relaxed);
        if TLBSTAT_PRINT
            && before / TLBSTAT_PERIOD_TICKS != (before + elapsed) / TLBSTAT_PERIOD_TICKS
        {
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
    // Sync-wake kicks a waker on this CPU deferred (lane percpu): the tick
    // bounds the deferral even for a waker that neither blocks nor returns.
    percpu::flush_sync_kick();
    // ksync wakes an IRQ could not deliver, and hrtimers whose one-shot
    // interrupt was armed on a CPU that has not taken it yet (fallback: at
    // most one tick late). Each is one relaxed load when idle.
    service_deferred_kwakes();
    hrtimer::service(monotonic_ns());
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

/// True while this CPU is inside a `preempt_disable` section (so an
/// `irq_window` here takes the tick but cannot switch tasks).
#[inline]
pub fn preempt_disabled() -> bool {
    let id = unsafe { cpu_id() }.min(MAX_CPUS - 1);
    PREEMPT_DISABLE[id].load(Ordering::Relaxed) != 0
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

/// Make `[ptr, ptr+len)` of the current process safe for a kernel access:
/// every page present, and (unless `read_only`) no page still shared
/// copy-on-write. File-backed pages are read with the address space
/// UNLOCKED, through the real fault path: `prefault_range` stops at each
/// absent file page, the lock is dropped, the page is faulted in, and the
/// walk resumes after it. So no prefault ever holds `busy` across file I/O.
/// A page that cannot be populated (past EOF, bad address) is skipped and
/// left to the caller's own access to report.
pub fn prefault_current_range(ptr: usize, len: usize, read_only: bool) {
    if ptr == 0 || len == 0 { return; }
    let end = match ptr.checked_add(len) { Some(e) => e, None => return };
    let mut from = ptr;
    while from < end {
        let pid = current_pid();
        let next = with_address_space_mut(pid, |as_| {
            if read_only { as_.prefault_range_ro(from, end - from) }
            else { as_.prefault_range(from, end - from) }
        }).flatten();
        match next {
            None => return,
            Some(va) => {
                let _ = prefault_user_page(va);
                from = va + mm::buddy::PAGE_SIZE;
            }
        }
    }
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
            // minflt: counted under `busy`, so the address space is ours.
            if matches!(f, mm::vmm::Fault::Handled) {
                unsafe { (*as_ptr).min_flt.fetch_add(1, Ordering::Relaxed); }
            }
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
                    if matches!(f, mm::vmm::Fault::Handled) {
                        unsafe { (*a).maj_flt.fetch_add(1, Ordering::Relaxed); }
                    }
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
            if !quiet { report_segv(pid, addr, is_write); }
            false
        }
    }
}

/// The faulting PC, recorded by the arch fault handler just before it calls
/// `handle_page_fault`, so a refused fault can name the instruction.
static FAULT_PC: [core::sync::atomic::AtomicUsize; MAX_CPUS] =
    [const { core::sync::atomic::AtomicUsize::new(0) }; MAX_CPUS];

/// Record the PC of the fault this CPU is about to resolve (user or kernel).
pub fn note_fault_pc(pc: usize) {
    FAULT_PC[unsafe { cpu_id() } % MAX_CPUS].store(pc, Ordering::Relaxed);
}

/// One line for a page fault the address space refused: which process, the
/// address and access, the PC (a kernel PC means a user pointer met inside a
/// syscall — the caller gets EFAULT or the task dies, see the arch handler),
/// and the VMA it hit or the ones either side of it. Until 2026-10-01 this
/// was a bare "returned false" that named nothing.
fn report_segv(pid: Pid, addr: usize, is_write: bool) {
    fn ps(s: &str) {
        extern "C" { fn arch_serial_putc(c: u8); }
        for &b in s.as_bytes() { unsafe { arch_serial_putc(b); } }
    }
    fn ph(n: usize) {
        let d = b"0123456789abcdef";
        let mut started = false;
        ps("0x");
        for i in (0..16).rev() {
            let c = d[(n >> (i * 4)) & 0xF];
            if c != b'0' || started || i == 0 { started = true; ps(unsafe { core::str::from_utf8_unchecked(&[c]) }); }
        }
    }
    fn pn(n: usize) {
        let mut buf = [0u8; 20]; let mut i = 0; let mut v = n;
        if v == 0 { ps("0"); return; }
        while v > 0 { buf[i] = b'0' + (v % 10) as u8; v /= 10; i += 1; }
        buf[..i].reverse();
        ps(unsafe { core::str::from_utf8_unchecked(&buf[..i]) });
    }
    fn pr(r: &mm::vmm::VmaRegion) {
        ph(r.start); ps("-"); ph(r.end);
        ps(" prot="); ph(r.prot as usize);
        ps(if r.lazy { " lazy" } else { " eager" });
        if r.cow { ps(" cow"); }
        if mm::vmm::is_file_backed(r.file_cap) { ps(" file+"); ph(r.file_off as usize); }
    }
    let pc = FAULT_PC[unsafe { cpu_id() } % MAX_CPUS].load(Ordering::Relaxed);
    let (tgid, state, owner, nthreads) = {
        let rq = RUN_QUEUE.lock();
        let (tgid, state) = rq.find_pid(pid).map(|t| (t.tgid, t.state as u32)).unwrap_or((pid, 9));
        let owner = rq.find_pid(tgid).map(|l| l.group_exit_owner).unwrap_or(u32::MAX as Pid);
        let mut n = 0usize;
        for i in rq.live_slots() {
            if let Some(t) = rq.get(i) { if t.tgid == tgid { n += 1; } }
        }
        (tgid, state, owner, n)
    };
    ps("[PF] SEGV pid="); pn(pid as usize);
    ps(" tgid="); pn(tgid as usize);
    ps(" st="); pn(state as usize);
    ps(" thr="); pn(nthreads);
    ps(" exit_owner="); pn(owner as usize);
    // What the leader (main thread) last entered: an exiting process's main
    // thread sits in exit_group (0xe7 x86 / 0x5e arm) or in its atexit work.
    let l = LAST_SYSCALL[(tgid as usize) & 1023].load(Ordering::Relaxed);
    if (l >> 32) as u32 == tgid as u32 {
        ps(" leader_last="); ph(((l >> 1) & 0x7FFF_FFFF) as usize);
        ps(if l & 1 != 0 { "/in" } else { "/out" });
    }
    if let Some(t) = EXE_PATHS.try_lock() {
        if let Some(e) = t.get(tgid) {
            ps(" ("); ps(core::str::from_utf8(e).unwrap_or("?")); ps(")");
        }
    }
    ps(" addr="); ph(addr);
    ps(if is_write { " W" } else { " R" });
    ps(" pc="); ph(pc);
    ps(if pc >= 0xffff_0000_0000_0000 { " (kernel)" } else { " (user)" });
    if let Some(a) = lock_leader_address_space(pid) {
        let as_ = unsafe { &*a };
        let mut below: Option<&mm::vmm::VmaRegion> = None;
        let mut above: Option<&mm::vmm::VmaRegion> = None;
        let mut hit: Option<&mm::vmm::VmaRegion> = None;
        let mut n = 0usize;
        for r in as_.regions.iter().filter_map(|r| r.as_ref()) {
            n += 1;
            if addr >= r.start && addr < r.end { hit = Some(r); }
            else if r.end <= addr { if below.map_or(true, |b| r.end > b.end) { below = Some(r); } }
            else if above.map_or(true, |b| r.start < b.start) { above = Some(r); }
        }
        ps(" nvma="); pn(n);
        if pc < 0xffff_0000_0000_0000 {
            if let Some(r) = as_.regions.iter().filter_map(|r| r.as_ref()).find(|r| pc >= r.start && pc < r.end) {
                ps(" pc_in["); pr(r); ps("]");
            }
        }
        if let Some(r) = hit {
            ps(" in["); pr(r);
            let idx = ((addr & !(mm::buddy::PAGE_SIZE - 1)) - r.start) / mm::buddy::PAGE_SIZE;
            ps(" page="); ph(r.lazy_pages.get(idx).copied().unwrap_or(0));
            ps("]");
        } else {
            ps(" below["); if let Some(r) = below { pr(r); } ps("]");
            ps(" above["); if let Some(r) = above { pr(r); } ps("]");
        }
        unsafe { unlock_address_space(a); }
    } else {
        ps(" no-as");
    }
    ps("\n");
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
/// Ctrl-T (x86_64): return addresses of cosmic-term threads parked in
/// epoll_pwait/futex, for offline symbolisation (diagnostic, off by default).
const DUMP_X86_RET: bool = false;

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
    for i in rq.live_slots() {
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
            if let Some(e) = tbl.get(t.tgid) {
                print_str(" (");
                for &b in e { print_str(core::str::from_utf8(&[b]).unwrap_or("?")); }
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
            print_str(" rdi="); ph(f.rdi as usize);
            // Diagnostic (termsegv): return addresses in executable file
            // mappings for a task parked in epoll_pwait (0x119) or futex
            // (0xca), as `addr@vma_start+file_off`, to symbolise offline.
            let is_term = DUMP_X86_RET && EXE_PATHS.try_lock().map_or(false, |tb| tb.get(t.tgid).map_or(false, |e|
                e.ends_with(b"cosmic-term")));
            if DUMP_X86_RET && is_term && t.state == TaskState::Blocked && (f.rax == 0x119 || f.rax == 0xca) {
                if let Some(a) = rq.find_pid(t.tgid).and_then(|l| l.address_space.as_ref()) {
                    print_str("\n[TASKS]   ret:");
                    let mut shown = 0;
                    for i in 0..2048usize {
                        let va = f.rsp as usize + i * 8;
                        let phys = match a.virt_to_phys(va) { Some(p) => p, None => break };
                        let v = unsafe { (mm::phys_to_virt(phys) as *const u64).read_volatile() } as usize;
                        if let Some(r) = a.find(v) {
                            if r.prot & mm::vmm::PROT_EXEC != 0 && mm::vmm::is_file_backed(r.file_cap) {
                                print_str(" "); ph(v); print_str("@"); ph(r.start);
                                print_str("+"); ph(r.file_off as usize);
                                shown += 1;
                                if shown >= 32 { break; }
                            }
                        }
                    }
                }
            }
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
            // A task parked in ppoll with no deadline: its pollfd array
            // (fd:events) and the return addresses on its stack that land in
            // executable file mappings, as `addr@vma_start+file_off`, so the
            // caller can be symbolised offline against the library it came
            // from. The wait with no wake is then named by fd and by frame.
            if t.state == TaskState::Blocked && f.x[8] == 0x49 && t.poll_deadline == u64::MAX {
                if let Some(a) = rq.find_pid(t.tgid).and_then(|l| l.address_space.as_ref()) {
                    print_str("\n[TASKS]   pollfds n="); pn(f.x[1] as u32); print_str(":");
                    for i in 0..(f.x[1] as usize).min(16) {
                        let mut pf = [0u8; 8];
                        if !a.read_user_buf(f.x[0] as usize + i * 8, &mut pf) { break; }
                        print_str(" "); pn(i32::from_le_bytes([pf[0], pf[1], pf[2], pf[3]]) as u32);
                        print_str(":"); ph(u16::from_le_bytes([pf[4], pf[5]]) as usize);
                    }
                    print_str("\n[TASKS]   ret:");
                    let mut shown = 0;
                    for i in 0..1024usize {
                        let va = f.sp_el0 as usize + i * 8;
                        let phys = match a.virt_to_phys(va) { Some(p) => p, None => break };
                        let v = unsafe { (mm::phys_to_virt(phys) as *const u64).read_volatile() } as usize;
                        if let Some(r) = a.find(v) {
                            if r.prot & mm::vmm::PROT_EXEC != 0 && mm::vmm::is_file_backed(r.file_cap) {
                                print_str(" "); ph(v); print_str("@"); ph(r.start);
                                print_str("+"); ph(r.file_off as usize);
                                shown += 1;
                                if shown >= 24 { break; }
                            }
                        }
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
const MAX_DUMP_HOOKS: usize = 8;
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

// ── Kernel wait channel (ksync) ──────────────────────────────────────────────
//
// `ksync`'s sleeping primitives park a task on this sentinel channel and wake
// it by pid (`kwake`), not by channel broadcast: each wait queue keeps its own
// waiter list. Same three-phase protocol as `block_on_port_*`: publish
// Blocked (prepare), register with the queue and re-check the condition, then
// yield (commit) — a wake landing anywhere after prepare flips the task back
// to Ready, so the commit returns at once and nothing is lost. Callers keep
// IRQs masked from prepare to commit (ksync does), so no preemption can split
// the sequence. Timed parks are released by the poll-deadline service
// (`wake_due_poll_deadlines` treats this channel like the poll channel).

/// Sentinel `blocked_on` value of a task parked by `ksync`.
pub const KWAIT_CHANNEL: u32 = 0xFFFF_FF03;

/// Phase 1 of a ksync park: mark the current task Blocked on
/// [`KWAIT_CHANNEL`] with an absolute `monotonic_ns()` deadline
/// (`u64::MAX` = none). IRQs must be masked by the caller.
pub(crate) fn kwait_prepare(deadline: u64) {
    let pid = current_pid();
    RUN_QUEUE.lock().block_on_port_until(pid, KWAIT_CHANNEL, deadline, 0);
    if deadline != u64::MAX {
        register_poll_deadline(deadline);
    }
}

/// Undo [`kwait_prepare`] (the re-check found the condition true).
pub(crate) fn kwait_cancel() { block_on_port_cancel() }

/// Phase 3: yield. Returns when woken, at the deadline, or on a signal.
pub(crate) fn kwait_commit() { yield_now("kwait") }

/// Pids whose wake could not be delivered at once (RUN_QUEUE contended past
/// the bound in IRQ context); paid by the next tick on any CPU.
const KWAKE_DEFER_SLOTS: usize = 64;
static KWAKE_DEFERRED: [AtomicU32; KWAKE_DEFER_SLOTS] = [const { AtomicU32::new(0) }; KWAKE_DEFER_SLOTS];
/// Set when anything is in `KWAKE_DEFERRED`, or when it overflowed (then the
/// service wakes every kwait task: spurious wakes are harmless).
static KWAKE_PENDING: AtomicBool = AtomicBool::new(false);
static KWAKE_OVERFLOW: AtomicBool = AtomicBool::new(false);

/// Make `pid` runnable if it is parked on [`KWAIT_CHANNEL`]. Callable from
/// any context, IRQ handlers included: RUN_QUEUE is only try-locked (bounded
/// spin); a contended wake is recorded and paid by the next tick (<= 10 ms).
pub fn kwake(pid: Pid) {
    if pid == 0 { return; }
    KWAKE_STATS[0].fetch_add(1, Ordering::Relaxed);
    match RUN_QUEUE.try_lock_spin(TICK_LOCK_WAIT_NS) {
        Some(mut rq) => {
            let woke = rq.wake_kwaiter(pid);
            drop(rq);
            if woke { KWAKE_STATS[1].fetch_add(1, Ordering::Relaxed); wake_up_an_idle_cpu(); }
        }
        None => { KWAKE_STATS[2].fetch_add(1, Ordering::Relaxed); kwake_defer(pid) }
    }
}

/// kwake counters: calls, woke a parked task, deferred, deferred paid.
static KWAKE_STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// `(calls, woke, deferred, deferred_woke)` since boot.
pub fn kwake_stats() -> (u64, u64, u64, u64) {
    (KWAKE_STATS[0].load(Ordering::Relaxed), KWAKE_STATS[1].load(Ordering::Relaxed),
     KWAKE_STATS[2].load(Ordering::Relaxed), KWAKE_STATS[3].load(Ordering::Relaxed))
}

fn kwake_defer(pid: Pid) {
    for s in KWAKE_DEFERRED.iter() {
        if s.compare_exchange(0, pid, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            KWAKE_PENDING.store(true, Ordering::Release);
            return;
        }
    }
    KWAKE_OVERFLOW.store(true, Ordering::Release);
    KWAKE_PENDING.store(true, Ordering::Release);
}

/// Pay deferred `kwake`s. Tick context (try-lock only).
fn service_deferred_kwakes() {
    if !KWAKE_PENDING.swap(false, Ordering::AcqRel) { return; }
    let mut rq = match RUN_QUEUE.try_lock_spin(TICK_LOCK_WAIT_NS) {
        Some(rq) => rq,
        None => { KWAKE_PENDING.store(true, Ordering::Release); return; }
    };
    let mut woken = 0usize;
    if KWAKE_OVERFLOW.swap(false, Ordering::AcqRel) {
        woken += rq.wake_all_kwaiters();
    }
    for s in KWAKE_DEFERRED.iter() {
        let pid = s.swap(0, Ordering::AcqRel);
        if pid != 0 && rq.wake_kwaiter(pid) { woken += 1; KWAKE_STATS[3].fetch_add(1, Ordering::Relaxed); }
    }
    drop(rq);
    if woken > 0 { wake_up_an_idle_cpu(); }
}

pub fn unblock_port(port: u32) {
    let woken = RUN_QUEUE.lock().unblock_port(port);
    if woken > 0 { wake_up_an_idle_cpu(); }
}

/// Live tasks, and the fixed table size they are competing for.
///
/// A LeandrOS thread is a task, so this counts threads, not processes: it is
/// the number that binds against `runqueue::MAX_TASKS`. `fork`/`clone` return
/// EAGAIN when the two are equal, however much physical memory is free.
pub fn task_census() -> (usize, usize) {
    let rq = RUN_QUEUE.lock();
    (rq.len(), rq.capacity())
}

/// Admission check for `fork`/`clone`, run before anything is allocated:
/// `-11` (EAGAIN, as Linux answers a fork over a limit) when
///
/// * the task table is full (`runqueue::MAX_TASKS`);
/// * a new process would exceed `runqueue::MAX_PROCESSES` — live leaders plus
///   unwaited zombie records, which count like Linux zombies do;
/// * the caller's RLIMIT_NPROC is exceeded: tasks (threads included, as on
///   Linux) whose real uid is the caller's. Root is exempt, as Linux exempts
///   CAP_SYS_ADMIN/CAP_SYS_RESOURCE. This walk is O(live tasks) and runs only
///   for a finite limit and a non-root caller.
///
/// The process and task limits are checked again under the enqueue hold
/// (`admit_locked`), which is what makes them exact; this early pass only
/// spares a doomed fork its stack and address-space copy.
pub fn fork_admission(new_process: bool) -> isize {
    let pid = current_pid();
    let rq = RUN_QUEUE.lock();
    if admit_locked(&rq, new_process) != 0 { return -11; }
    let Some(me) = rq.find_pid(pid) else { return 0 };
    if me.uid == 0 || me.euid == 0 { return 0; }
    let uid = me.uid;
    let limit = rq.find_pid(me.tgid).map_or(task::RLIM_INFINITY, |l| l.rlimits[task::RLIMIT_NPROC][0]);
    if limit == task::RLIM_INFINITY { return 0; }
    let mut n = 0u64;
    for i in rq.live_slots() {
        if let Some(t) = rq.get(i) {
            if t.uid == uid && t.state != TaskState::Zombie { n += 1; }
        }
    }
    if n >= limit { -11 } else { 0 }
}

/// The exact task/process limit test, under the RUN_QUEUE hold that enqueues
/// the child. 0 = admit, -11 = EAGAIN. Unwaited zombie records only change
/// under RUN_QUEUE (`log_exit` and `wait_scan` both hold it), so the sum is
/// stable here.
pub fn admit_locked(rq: &runqueue::RunQueue, new_process: bool) -> isize {
    if rq.len() >= runqueue::MAX_TASKS { return -11; }
    if new_process && rq.process_count() + zombie_records() >= runqueue::MAX_PROCESSES { return -11; }
    0
}

/// Live processes: thread-group leaders in the table (a counter the run
/// queue maintains, so O(1)).
pub fn process_count() -> usize {
    RUN_QUEUE.lock().process_count()
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

/// Spawn a kernel task that runs `func(arg)`; `func` must not return (end
/// with `exit`). The building block of `kthread::spawn`.
pub fn spawn_with_arg(func: extern "C" fn(usize) -> !, arg: usize) -> Option<Pid> {
    let pid = alloc_pid();
    let stack_base = mm::buddy::alloc(KERNEL_STACK_ORDER)?;
    arm_kernel_stack(stack_base);
    let mut task = Task::new_kernel(pid, 0, stack_base, KERNEL_STACK_SIZE, 0);
    task.ctx = CpuContext::new_task_with_arg(func as usize, arg,
        mm::phys_to_virt(stack_base) + KERNEL_STACK_SIZE);
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
    // Lane sigfast2: the switch-back hold of the previous cycle, carried into
    // this cycle's pick so one context switch takes RUN_QUEUE once, not
    // twice. Set only when the switched-out task was not reaped (a zombie's
    // teardown must run between the two holds, unlocked). Merging is safe
    // because it only removes interleavings: "switch-back; pick" with no
    // other CPU's hold in between was always a possible schedule.
    let mut held: Option<lockwatch::TrackedGuard<'static, RunQueue>> = None;
    // Lane percpu sync wake: the switched-out task deferred one idle-CPU kick
    // and blocked, so this CPU's pick is expected to take the wakee. If that
    // pick leaves other candidates waiting, the kick is paid after all.
    let mut sync_dropped = false;
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
        // With a carried hold the window already opened just before the
        // switch-back (below), never under the lock.
        if held.is_none() { irq_window(); }

        // Pick, claim (on_cpu) and mark Running under a single lock so no
        // other CPU can dispatch the same task between pick and claim.
        let picked = {
            let mut rq = match held.take() { Some(g) => g, None => RUN_QUEUE.lock() };
            let pick = rq.pick_next(id);
            if sync_dropped {
                sync_dropped = false;
                if pick.is_some() && rq.last_pick_cands > 1 { wake_up_an_idle_cpu(); }
            }
            match pick {
                Some(idx) => {
                    let t = rq.get_mut(idx).unwrap();
                    // Lane percpu: run guard (CAS, panics on a double run)
                    // and the dispatch-mirror check (`mirror_miss`).
                    slot::claim(idx, id, t);
                    slot::check_mirror(idx, t);
                    t.on_cpu = Some(id);
                    t.set_state(TaskState::Running);
                    let kst = mm::phys_to_virt(t.kernel_stack) + KERNEL_STACK_SIZE;
                    let as_ptr = t.address_space.as_ref()
                        .map_or(core::ptr::null_mut(), |a| alloc::sync::Arc::as_ptr(a) as *mut mm::vmm::AddressSpace);
                    unsafe { arch_set_user_gs(t.user_gs_base); }
                    rseq::on_dispatch(id, &t.rseq);
                    let sc_saved = (t.sc_saved_nr, t.sc_saved_ns, t.sc_saved_t0);
                    t.sc_saved_nr = NO_SYSCALL;
                    CURRENT_SIGHINT[id].store(&t.sig_hint as *const AtomicU32 as *mut AtomicU32, Ordering::Relaxed);
                    Some((idx, &t.ctx as *const CpuContext, t.pid, kst, t.page_table, t.tgid, t.reply_port, as_ptr, sc_saved))
                }
                None => None,
            }
        };

        if let Some((idx, ctx_ptr, dispatched_pid, kernel_stack_top_virt, page_table, tgid, reply_port, as_ptr, sc_saved)) = picked {
            IDLE_SINCE[id].store(0, Ordering::Relaxed);
            let dispatched_at = ticks();
            let dispatched_ns = unsafe { arch_monotonic_ns() };
            // Reopen the syscall the task was switched out in (if any): the
            // per-CPU syscall number follows the task, so the watchdog, the
            // PC sampler and the syscall-time census name the right one.
            CUR_SYSCALL[id].store(sc_saved.0, Ordering::Relaxed);
            sysacct::on_dispatch(id, sc_saved.0, sc_saved.1, sc_saved.2, dispatched_ns);
            if idlestat::ENABLED {
                let l = LAST_SYSCALL[(dispatched_pid as usize) & 1023].load(Ordering::Relaxed);
                let sc = if (l >> 32) as u32 == dispatched_pid && l & 1 != 0 {
                    ((l >> 1) & 0x7FFF_FFFF) as u32
                } else { u32::MAX };
                idlestat::on_dispatch(dispatched_pid, tgid, sc);
            }
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
                CURRENT_SIGHINT[id].store(core::ptr::null_mut(), Ordering::Relaxed);

                // Detach from the task's page table before it can be freed:
                // if this task exits (reaped below) or exits later on another
                // CPU, its tables are released and reused — a CPU still
                // holding them in CR3/TTBR0 faults on its next TLB miss.
                arch_load_kernel_page_table();
            }

            // This cycle's IRQ window (see the top of the loop), opened
            // before the switch-back hold because that hold may be carried
            // straight into the next pick. The task still holds its on_cpu
            // claim here, as it does for the whole stretch from
            // cpu_switch_to's return to the lock: an IRQ on this CPU sees it
            // exactly as another CPU always could.
            irq_window();

            // Charge CPU time, release the on_cpu claim, and — if the task
            // exited — atomically take its slot out of the queue.  All under
            // one lock: once on_cpu is None other CPUs may dispatch or (for
            // zombies) observe the slot, so the claim must drop in the same
            // critical section that resolves the task's final state.
            // A task that is not reaped leaves the guard in `held` for the
            // next pick (lane sigfast2).
            let zombie = {
                let mut rq = RUN_QUEUE.lock();
                let owed = percpu::take_sync_kicks(id);
                let mut runnable = true;
                let zinfo = match rq.get_mut(idx) {
                    Some(t) if t.pid == pid => {
                        let delta = ticks().saturating_sub(dispatched_at);
                        t.charge_vruntime(delta);
                        let out_ns = unsafe { arch_monotonic_ns() };
                        t.cpu_ns += out_ns.saturating_sub(dispatched_ns);
                        slot::note_run(idx, out_ns.saturating_sub(dispatched_ns));
                        let cur_sc = CUR_SYSCALL[id].swap(NO_SYSCALL, Ordering::Relaxed);
                        let (sys, nr, acc, t0) = sysacct::on_switch_back(id, cur_sc, out_ns);
                        t.sys_ns = (t.sys_ns + sys).min(t.cpu_ns);
                        t.sc_saved_nr = nr;
                        t.sc_saved_ns = acc;
                        t.sc_saved_t0 = t0;
                        slot::release(idx, id, t);
                        t.on_cpu = None;
                        if t.state == TaskState::Running {
                            t.set_state(TaskState::Ready);
                        }
                        // A sibling took SIGSTOP while this thread was on
                        // the CPU (see signal::do_signal_stop): now that
                        // its registers are saved, park it. Ready or
                        // Blocked alike — a Blocked thread resumes as a
                        // spurious wake on SIGCONT and re-parks itself.
                        //
                        // Lane loginhang: not while it holds (or is queued
                        // for) a sleeping lock — a stopped holder would stall
                        // every task that needs the lock (the filesystem!)
                        // until SIGCONT. The flag stays set; the stop happens
                        // at a later switch-out or at the signal check on the
                        // way back to user space.
                        let klocks = ksync::klocks_held(t.pid);
                        if t.stop_pending && klocks == 0
                            && matches!(t.state, TaskState::Ready | TaskState::Blocked)
                        {
                            t.set_state(TaskState::Stopped);
                            signal::clear_block_fields(t);
                        }
                        // A sibling's group kill marked this thread Zombie
                        // while it ran, and it has since taken a sleeping
                        // lock (or queued for one) and parked: reaping it now
                        // would leak the lock. Let it run on (a spurious wake
                        // for whatever wait it parked in); it parks for good
                        // at its return to user space and the killer's next
                        // round reaps it (`GroupKillStep::Deferred`).
                        if t.state == TaskState::Zombie && t.group_killed && klocks != 0 {
                            t.set_state(TaskState::Ready);
                            percpu::pexit_bump();
                        }
                        runnable = t.state == TaskState::Ready;
                        // (phase 2: queued on this CPU below, `switch_back`)
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
                // Phase 2: off this CPU; queued here if still Ready.
                rq.switch_back(id, idx);
                // Deferred sync-wake kicks: a waker still runnable keeps this
                // CPU busy, so pay them; one that blocked (or died) leaves
                // the CPU to its single wakee.
                if owed > 1 || (owed == 1 && runnable) {
                    wake_up_an_idle_cpu();
                } else if owed == 1 {
                    sync_dropped = true;
                    percpu::bump(&percpu::SYNC_DROP);
                }
                if let Some((zpid, status, ppid, pgid, is_proc, reported)) = zinfo {
                    // Log the exit code BEFORE the slot disappears:
                    // a parent polling wait_pid/wait_try on another CPU must
                    // always find the pid either in the queue or in
                    // EXIT_LOG — a gap between remove() and a later
                    // log_exit() makes waitpid return ECHILD.
                    let parent_tgid = rq.find_pid(ppid).map(|p| p.tgid).unwrap_or(ppid);
                    log_exit(&mut rq, zpid, status, parent_tgid, pgid, is_proc, reported);
                    let ztgid = rq.get(idx).map(|t| t.tgid).unwrap_or(zpid);
                    release_quiesce_if_owner(ztgid, zpid);
                    rq.remove(idx)
                } else {
                    held = Some(rq);
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
                // or its path leaks (and a reused tgid would inherit a stale name).
                if t.pid == t.tgid { clear_exe_path(t.pid); }
        clear_comm(t.pid);
                mm::buddy::free(t.kernel_stack, KERNEL_STACK_ORDER);
            }
        } else {
            let t_idle = if idlestat::ENABLED { unsafe { arch_monotonic_ns() } } else { 0 };
            if IDLE_SINCE[id].load(Ordering::Relaxed) == 0 {
                IDLE_SINCE[id].store(monotonic_ns().max(1), Ordering::Relaxed);
            }
            unsafe {
                #[cfg(target_arch = "x86_64")]
                core::arch::asm!("sti; hlt; cli");
                #[cfg(target_arch = "aarch64")]
                core::arch::asm!("msr daifclr, #2; wfi; msr daifset, #2");
            }
            if idlestat::ENABLED {
                idlestat::on_idle(id, unsafe { arch_monotonic_ns() }.saturating_sub(t_idle));
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
    // Every other thread must now see the owner on its way back to user
    // space (and park for the kill loop): wake its signal check.
    rq.sig_kick_group(tgid);
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
    let mut deferred = DeferWait::new();
    loop {
        match kill_next_group_member(code) {
            GroupKillStep::Done        => break,
            GroupKillStep::Reaped(pid) => run_exit_teardown(pid),
            GroupKillStep::Kicking     => { irq_window(); core::hint::spin_loop(); }
            GroupKillStep::Deferred(t) => deferred.wait(t),
        }
    }
}

/// Pacing for a group kill (or exec) that is waiting for a sibling to leave
/// the kernel (`GroupKillStep::Deferred`, lane loginhang): the sibling holds a
/// sleeping lock (or is queued for one) and may need a device command or a
/// queue of filesystem requests — up to seconds on a slow USB stick — to get
/// out, so the caller sleeps 1 ms per round instead of spinning, and says
/// once every 5 s whom it is waiting for.
struct DeferWait { since: u64, last_report: u64 }

impl DeferWait {
    fn new() -> Self { DeferWait { since: 0, last_report: 0 } }

    fn wait(&mut self, target: Pid) {
        let now = monotonic_ns();
        if self.since == 0 { self.since = now; self.last_report = now; }
        if now.saturating_sub(self.last_report) >= 5_000_000_000 {
            self.last_report = now;
            irqctx::raw_log("[EXIT] pid "); irqctx::raw_dec(current_pid() as u64);
            irqctx::raw_log(": waiting "); irqctx::raw_dec(now.saturating_sub(self.since) / 1_000_000);
            irqctx::raw_log(" ms for thread "); irqctx::raw_dec(target as u64);
            irqctx::raw_log(" to leave the kernel (");
            irqctx::raw_dec(ksync::klocks_held(target) as u64);
            irqctx::raw_log(" sleeping lock(s) held/queued)\n");
        }
        let deadline = now.saturating_add(1_000_000);
        if irqctx::can_sleep() {
            block_on_poll_prepare_masked(deadline, POLL_MASK_TIMED_ONLY);
            if monotonic_ns() >= deadline { block_on_poll_cancel(); } else { block_on_poll_commit(); }
        } else {
            while monotonic_ns() < deadline { irq_window(); core::hint::spin_loop(); }
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
            t.set_state(TaskState::Zombie);
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
        for i in rq.live_slots() {
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
    let mut deferred = DeferWait::new();
    loop {
        match kill_next_group_member_except(0, tgid) {
            GroupKillStep::Done        => break,
            GroupKillStep::Reaped(p)   => run_exit_teardown(p),
            GroupKillStep::Kicking     => { irq_window(); core::hint::spin_loop(); }
            GroupKillStep::Deferred(t) => deferred.wait(t),
        }
    }
    loop {
        match take_over_leader(pid, tgid) {
            GroupKillStep::Done        => break,
            GroupKillStep::Reaped(old) => { run_exit_teardown(old); break; }
            GroupKillStep::Kicking     => { irq_window(); core::hint::spin_loop(); }
            GroupKillStep::Deferred(t) => deferred.wait(t),
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
                slot::publish(me);
                me.group_exit_owner = 0;
                me.sig_kick();
            }
            rq.note_leader_promoted();
            pid_tgid_insert(pid, pid);
            CURRENT_TGID[cpu].store(pid, Ordering::Relaxed);
            return GroupKillStep::Done;
        }
    };

    // Lane loginhang: a leader inside kernel code holding (or queued for) a
    // sleeping lock is not taken over in place (see `GroupKillStep::Deferred`).
    if let Some(l) = rq.get(lidx) {
        if ksync::klocks_held(l.pid) != 0 { return GroupKillStep::Deferred(l.pid); }
    }
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
            leader.sig_kick();
            drop(rq);
            trigger_preempt(lcpu);
            return GroupKillStep::Kicking;
        }
        // Off-CPU (Ready, Blocked or parked Stopped): nothing executes on
        // its stack. Zombie from here on, in the same hold that removes it.
        leader.set_state(TaskState::Zombie);
        leader.exit_code = 0;
        core::mem::replace(&mut leader.vfork_pending, false)
    };
    if had_vfork { rq.unblock_port(VFORK_WAIT_CHANNEL); }

    // Copy out what the caller inherits, then exchange the pids.
    let (ppid, pgid, sid, creds, actions, shared_pending, pending, info, stop, reply_port, wait_reported) = {
        let l = rq.get(lidx).unwrap();
        (l.ppid, l.pgid, l.sid,
         (l.uid, l.gid, l.euid, l.egid, l.suid, l.sgid, l.no_new_privs),
         l.signal_actions, l.shared_signal_pending, l.signal_pending, l.signal_info,
         (l.stop_signal, l.stop_reported, l.cont_pending),
         l.reply_port, l.wait_reported)
    };
    let rlimits = rq.get(lidx).unwrap().rlimits; // per process: lives on the leader
    let old_pid = pid;
    {
        let me = rq.get_mut(me_idx).unwrap();
        me.rlimits = rlimits;
        me.pid  = tgid;
        me.ppid = ppid; me.pgid = pgid; me.sid = sid;
        (me.uid, me.gid, me.euid, me.egid, me.suid, me.sgid, me.no_new_privs) = creds;
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
        percpu::pexit_bump();
        me.group_exit_owner = 0;
        // Pending sets were merged in: re-evaluate on the way out.
        me.sig_kick();
        // The caller's own reply port is owned by `old_pid` in the port
        // table and is released with it below; the leader's is owned by
        // `tgid` — the caller's pid from here on.
        me.reply_port = reply_port;
        slot::publish(me);
    }
    {
        let leader = rq.get_mut(lidx).unwrap();
        leader.pid = old_pid;
        percpu::pexit_bump();
        slot::publish(leader);
    }
    // Keep the run queue's pid → slot hint current (a stale hint is only
    // slower, never wrong — see `RunQueue::pid_index`).
    rq.reindex(me_idx);
    rq.reindex(lidx);
    // Children forked by this thread name its old tid as parent.
    for i in rq.live_slots() {
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
    log_exit(&mut rq, old_pid, ExitStatus { code: 0, term_signal: 0 }, parent_tgid, pgid, false, false);
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
                t.set_state(TaskState::Zombie);
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
    clear_comm(pid);
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
    // A pidfd poller waits for the whole group to be gone. The leader is the
    // common last thread out and gets the immediate wake; any other thread's
    // exit may be the last one too (the leader can pthread_exit first), and
    // that rarer case is paid by the next tick. `log_exit` backs both.
    let pidfd_tag = poll_tag(poll_class::PIDFD, tgid);
    if pid == tgid { wake_poll_tagged(pidfd_tag); } else { request_poll_wake_tagged(pidfd_tag); }
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
        for i in rq.live_slots() {
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
        // Its fault counts outlive it, for the parent's wait (`group_acct`).
        if let Some(a) = taken.as_ref() {
            t.c_minflt += a.min_flt.load(Ordering::Relaxed);
            t.c_majflt += a.maj_flt.load(Ordering::Relaxed);
        }
        slot::publish(t);
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
    /// Every remaining member is inside kernel code holding (or queued for) a
    /// sleeping lock (`ksync::klocks_held`): `F2FS_MOUNTS`, a USB disk lock,
    /// an RM mutex. Reaping it in place would leave that lock owned by a dead
    /// pid (or its wait node linked from a freed kernel stack) for ever. It is
    /// left running; it finishes its kernel work, and parks at its return to
    /// user space (`signal::deliver_pending_signals` sees the group dying),
    /// where the next call reaps it. Call again after a short sleep.
    Deferred(Pid),
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

    // Lane loginhang: a member holding (or queued for) a sleeping kernel lock
    // is skipped (`Deferred`) — see `GroupKillStep::Deferred`. With the run
    // queue held, an off-CPU member cannot start running, so its count cannot
    // rise between this check and the reap; an on-CPU member that takes a lock
    // after being marked is caught by the dispatch loop's reap (`group_killed`).
    //
    // The leader goes last: reaping it tears down the process's fd table
    // (`run_exit_teardown(tgid)`), and a deferred sibling finishing an `open`
    // afterwards would find no table to install into, leaking the file
    // system's open-file slot (killmt fs_io: the f2fs pool of 256 ran dry after
    // ~100 kills).
    let mut target: Option<usize> = None;
    let mut leader: Option<usize> = None;
    let mut deferred: Pid = 0;
    for i in rq.live_slots() {
        if let Some(t) = rq.get(i) {
            if t.tgid == tgid && t.pid != pid && t.pid != skip {
                if ksync::klocks_held(t.pid) != 0 {
                    if deferred == 0 { deferred = t.pid; }
                    continue;
                }
                if t.pid == tgid { leader = Some(i); continue; }
                target = Some(i);
                break;
            }
        }
    }
    let idx = match (target, leader) {
        (Some(i), _) => i,
        (None, _) if deferred != 0 => return GroupKillStep::Deferred(deferred),
        (None, Some(l)) => l,
        (None, None) => return GroupKillStep::Done,
    };

    let (tpid, on_cpu, tppid, tpgid, t_is_proc, t_reported, t_term_signal, had_vfork) = {
        let t = rq.get_mut(idx).unwrap();
        t.set_state(TaskState::Zombie);
        t.group_killed = true;
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
    log_exit(&mut rq, tpid, ExitStatus { code: exit_code, term_signal: t_term_signal },
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
        // or its path leaks (and a reused tgid would inherit a stale name).
        if t.pid == t.tgid { clear_exe_path(t.pid); }
        clear_comm(t.pid);
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

/// arch_prctl(ARCH_SET_GS): record the calling thread's user GS base and make
/// this CPU's next return to user mode load it.
pub fn set_user_gs_base(addr: u64) {
    let pid = current_pid();
    if let Some(t) = RUN_QUEUE.lock().find_pid_mut(pid) {
        t.user_gs_base = addr;
        slot::publish(t);
    }
    unsafe { arch_set_user_gs(addr); }
}

pub fn get_user_gs_base() -> u64 {
    let pid = current_pid();
    RUN_QUEUE.lock().find_pid(pid).map(|t| t.user_gs_base).unwrap_or(0)
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
            // Page-fault counts belong to the process, not the image: carry
            // them across exec (Linux keeps them in the task).
            if let Some(old) = t.address_space.as_ref() {
                new_as.min_flt.store(old.min_flt.load(Ordering::Relaxed), Ordering::Relaxed);
                new_as.maj_flt.store(old.maj_flt.load(Ordering::Relaxed), Ordering::Relaxed);
            }
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
            t.user_gs_base = 0;
            slot::publish(t);
            unsafe { arch_set_user_gs(0); }
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

/// Write the current task's executable path to the raw UART as ` (<path>)`
/// (nothing when unknown or the path table is busy). For diagnostics such as
/// `iolock`'s `[LOCKHOLD]`: unlike `CUR_SYSCALL`, which is per CPU and stale
/// once a task has slept and resumed on another CPU, the current tgid is exact.
pub fn print_current_exe() {
    extern "C" { fn arch_serial_putc(c: u8); }
    let cpu = unsafe { cpu_id() }.min(MAX_CPUS - 1);
    let tgid = CURRENT_TGID[cpu].load(Ordering::Relaxed);
    if let Some(t) = EXE_PATHS.try_lock() {
        if let Some(e) = t.get(tgid) {
            unsafe {
                arch_serial_putc(b' '); arch_serial_putc(b'(');
                for &b in e { arch_serial_putc(b) }
                arch_serial_putc(b')');
            }
        }
    }
}

/// State of the NVIDIA (OpenRM) bring-up, for `/proc/nvrm_state`: userspace
/// (gpu-env, init) must not decide "no GPU" while the card is still booting
/// GSP. Written by drivers/nvidia, read by the VFS. Lives here only because
/// both already depend on this crate.
pub mod nvrm_state {
    use core::sync::atomic::{AtomicU8, Ordering};
    pub const ABSENT: u8 = 0;
    pub const PROBING: u8 = 1;
    pub const READY: u8 = 2;
    pub const FAILED: u8 = 3;
    static STATE: AtomicU8 = AtomicU8::new(ABSENT);
    pub fn get() -> u8 { STATE.load(Ordering::Acquire) }
    pub fn set(s: u8) { STATE.store(s, Ordering::Release) }
    /// PROBING -> `to`, only if still probing (a later READY is not undone).
    pub fn finish_probing(to: u8) {
        let _ = STATE.compare_exchange(PROBING, to, Ordering::AcqRel, Ordering::Acquire);
    }
    pub fn name(s: u8) -> &'static str {
        match s { PROBING => "probing", READY => "ready", FAILED => "failed", _ => "absent" }
    }
}
