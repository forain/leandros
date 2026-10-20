//! Lane percpu: `SchedSlot`, the scheduling state of every task slot, kept
//! OUTSIDE the boxed `Task` in a static side array.
//!
//! Why outside: pick, dispatch and the switch-back fast path run under the
//! per-CPU run-queue lock `RQ[c]` (`cpuq.rs`), not under RUN_QUEUE (TASKS),
//! so they must not form a `&mut Task` while another CPU holds TASKS and may
//! hold one too (aliasing UB across lock domains). Everything they need
//! lives here as atomics.
//!
//! Ownership (who may write what; reads are lock-free unless noted):
//! - `state`: mirror of `Task::state` (`TaskState as u8 + 1`, 0 = empty
//!   slot). Only `Task::set_state` writes it (see `note_state`): a wake
//!   (→ Ready) under TASKS; any other transition by the task's OWNER (the
//!   CPU holding its run guard: the task itself, or its switch-back) under
//!   TASKS; by anyone else under TASKS + `RQ[cpu]` (rule R4).
//!   `TaskState::Running` is never stored any more: "running" is
//!   `on_cpu != 0`, and a running task is `Ready` (or whatever it set
//!   itself to before switching out).
//! - `on_cpu`: the RUN GUARD, `cpu + 1` while dispatched, 0 otherwise.
//!   Claimed (CAS 0 → cpu+1, failure panics: double run) at dispatch and
//!   released at switch-back, both under `RQ[cpu]`.
//! - `cpu`, `on_rq`, `q_gen`, `vr`, `vd`: under `RQ[cpu]` (`cpu` of an
//!   unqueued, off-CPU task is also changed by wake placement, which holds
//!   TASKS and the target queue).
//! - `weight`: copy of `Task::weight` for the queues; written under TASKS +
//!   `RQ[cpu]` (renice) or before the task is inserted.
//! - `sb_slow`: "the switch-back must take TASKS" (stop_pending,
//!   group_killed, or a non-owner moved the task out of Ready while it ran);
//!   set under TASKS, cleared by the slow switch-back under TASKS.
//! - `cpu_ns`, `sys_ns`, `sc_*`: written by the run-guard owner only.
//! - `gen`: bumped every time the slot is freed (under TASKS + `RQ[cpu]`).
//!   Queue entries carry the gen they saw (`q_gen`); a mismatch is a
//!   use-after-reap (`gen_miss`). Freed tasks are also poisoned
//!   (`Task::magic`).
//! - The dispatch mirror (`m_*`): every value the dispatch reads from the
//!   task, re-published by `publish` next to every write of its source field
//!   (write-site table in the lane notes). Published only for the running
//!   task itself or for a task that cannot be dispatched (a Zombie), so a
//!   dispatch never sees a half-written mirror. The tick checker compares
//!   it with the task (`mirror_miss`, must stay 0).
//!
//! All counters are printed by `fmt_stats` into `/proc/rqhold`.

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use crate::runqueue::MAX_TASKS;
use crate::task::{Task, TaskState};

#[repr(C, align(64))]
pub struct SchedSlot {
    // ── line 1: queue / pick state ──
    pub state: AtomicU8,
    pub sb_slow: AtomicU8,
    pub on_cpu: AtomicU32,
    /// Home CPU: whose queue the task is on / joins when it becomes Ready.
    pub cpu: AtomicU32,
    /// `cpu + 1` of the CpuQueue holding the task, 0 = none.
    pub on_rq: AtomicU32,
    pub gen: AtomicU32,
    /// `gen` when the slot was queued.
    pub q_gen: AtomicU32,
    pub weight: AtomicU32,
    pub m_pid: AtomicU32,
    pub m_tgid: AtomicU32,
    /// EEVDF virtual runtime / deadline (NICE0 tick units, see task.rs).
    pub vr: AtomicU64,
    pub vd: AtomicU64,
    // ── line 2: dispatch mirror ──
    pub m_ctx: AtomicUsize,
    pub m_kst: AtomicUsize,
    pub m_pt: AtomicUsize,
    pub m_as: AtomicUsize,
    pub m_sighint: AtomicUsize,
    pub m_gs: AtomicU64,
    pub m_rseq_addr: AtomicUsize,
    pub m_rseq_sig: AtomicU32,
    pub m_reply: AtomicU32,
    // ── line 3: accounting ──
    pub ready_since: AtomicU64,
    /// Nanoseconds on a CPU (see the old `Task::cpu_ns`), and the part
    /// spent in system calls.
    pub cpu_ns: AtomicU64,
    pub sys_ns: AtomicU64,
    /// The syscall the task was switched out in and its on-CPU ns so far.
    pub sc_ns: AtomicU64,
    /// `monotonic_ns()` at that syscall's entry (lane procfix `sc_saved_t0`).
    pub sc_t0: AtomicU64,
    pub sc_nr: AtomicU32,
    // ── line 4: per-thread rusage (lane procfs) ──
    /// Voluntary / involuntary context switches: switch-backs that found the
    /// task Blocked or Stopped / still Ready (`ru_nvcsw`, `ru_nivcsw`).
    pub nvcsw: AtomicU64,
    pub nivcsw: AtomicU64,
    /// This thread's page faults without / with device I/O (`task/<tid>/stat`
    /// minflt/majflt, `RUSAGE_THREAD`); the process totals stay on the
    /// address space.
    pub minflt: AtomicU64,
    pub majflt: AtomicU64,
    /// Block-device reads this thread issued (`note_block_read`): what tells
    /// a major fault from a page-cache hit.
    pub io_rd: AtomicU64,
}

impl SchedSlot {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(0), sb_slow: AtomicU8::new(0), on_cpu: AtomicU32::new(0),
            cpu: AtomicU32::new(0), on_rq: AtomicU32::new(0), gen: AtomicU32::new(0),
            q_gen: AtomicU32::new(0), weight: AtomicU32::new(0),
            m_pid: AtomicU32::new(0), m_tgid: AtomicU32::new(0),
            vr: AtomicU64::new(0), vd: AtomicU64::new(0),
            m_ctx: AtomicUsize::new(0), m_kst: AtomicUsize::new(0), m_pt: AtomicUsize::new(0),
            m_as: AtomicUsize::new(0), m_sighint: AtomicUsize::new(0), m_gs: AtomicU64::new(0),
            m_rseq_addr: AtomicUsize::new(0), m_rseq_sig: AtomicU32::new(0), m_reply: AtomicU32::new(0),
            ready_since: AtomicU64::new(0), cpu_ns: AtomicU64::new(0), sys_ns: AtomicU64::new(0),
            sc_ns: AtomicU64::new(0), sc_t0: AtomicU64::new(0), sc_nr: AtomicU32::new(0),
            nvcsw: AtomicU64::new(0), nivcsw: AtomicU64::new(0), minflt: AtomicU64::new(0),
            majflt: AtomicU64::new(0), io_rd: AtomicU64::new(0),
        }
    }
}

/// All zero: lands in .bss (`on_insert` sets the non-zero defaults).
pub static SCHED: [SchedSlot; MAX_TASKS] = [const { SchedSlot::new() }; MAX_TASKS];

/// `Task::slot` of a task not (yet) in the table.
pub const NO_SLOT: u16 = u16::MAX;
/// `Task::magic` of a live task; `TASK_POISON` once its box is dropped.
pub const TASK_MAGIC: u32 = 0x5441_534B; // "TASK"
pub const TASK_POISON: u32 = 0xDEAD_7A5C;

pub static MIRROR_MISS: AtomicU64 = AtomicU64::new(0);
pub static MIRROR_MISS_FIELD: AtomicU32 = AtomicU32::new(0);
pub static STATE_MISS: AtomicU64 = AtomicU64::new(0);
pub static HUNG_READY: AtomicU64 = AtomicU64::new(0);
pub static HUNG_READY_PID: AtomicU32 = AtomicU32::new(0);
pub static DISPATCHES: AtomicU64 = AtomicU64::new(0);
/// Switch-backs that took the fast path (RQ[me] only) / the slow one (TASKS).
pub static SB_FAST: AtomicU64 = AtomicU64::new(0);
pub static SB_SLOW: AtomicU64 = AtomicU64::new(0);
/// Non-owner transitions out of Ready that dequeued / flagged a running task.
pub static R4_DEQ: AtomicU64 = AtomicU64::new(0);
pub static R4_RUNNING: AtomicU64 = AtomicU64::new(0);
static R4_LAST: AtomicUsize = AtomicUsize::new(0);
static R4_LAST_ST: AtomicU32 = AtomicU32::new(0);

#[inline]
pub fn state_code(s: TaskState) -> u8 { s as u8 + 1 }

#[inline]
pub fn slot(idx: usize) -> &'static SchedSlot { &SCHED[idx] }

#[inline]
fn my_cpu() -> u32 { unsafe { crate::cpu_id() as u32 } }

/// Slot `idx` is claimed by the calling CPU: the caller is the task itself
/// (in a syscall, fault or kernel wait) or its switch-back on this CPU.
#[inline]
pub(crate) fn owned_here(idx: usize) -> bool {
    SCHED[idx].on_cpu.load(Ordering::Relaxed) == my_cpu() + 1
}

/// `on_cpu` as an option (None = off-CPU).
#[inline]
pub fn on_cpu(idx: usize) -> Option<usize> {
    match SCHED[idx].on_cpu.load(Ordering::Acquire) { 0 => None, c => Some(c as usize - 1) }
}

#[inline]
pub(crate) fn state_of(idx: usize) -> u8 { SCHED[idx].state.load(Ordering::Acquire) }

/// Mirror a `Task::state` change (called by `Task::set_state` only, after
/// the task's own field is written).
///
/// - → Ready from anything but Ready: the wake owes the task a queue
///   placement (`WOKEN`, drained by `RunQueue::flush_woken` — at the latest
///   when the TASKS guard is released). Not for the owner: the switch-back
///   queues it.
/// - → anything else by a non-owner (R4): under `RQ[cpu]`, so a pick cannot
///   dispatch it in between and the switch-back fast path sees the state:
///   an off-CPU task is dequeued (it can never be dispatched again until a
///   TASKS holder makes it Ready), a running one gets `sb_slow`.
#[track_caller]
pub(crate) fn note_state(idx: u16, old: TaskState, new: TaskState) {
    if idx == NO_SLOT { return; }
    let i = idx as usize;
    let s = &SCHED[i];
    if new == TaskState::Ready {
        s.state.store(state_code(new), Ordering::Release);
        if old != TaskState::Ready {
            s.ready_since.store(crate::monotonic_ns(), Ordering::Relaxed);
            if !owned_here(i) { mark_woken(i); }
        }
        return;
    }
    if owned_here(i) {
        s.state.store(state_code(new), Ordering::Release);
        return;
    }
    if crate::cpuq::held_here() != 0 {
        panic!("[SCHED] slot {} -> {:?} by a non-owner holding an RQ lock", i, new);
    }
    let mut g = crate::cpuq::lock_slot(i);
    s.state.store(state_code(new), Ordering::Release);
    if s.on_cpu.load(Ordering::Acquire) != 0 {
        s.sb_slow.store(1, Ordering::Relaxed);
        R4_RUNNING.fetch_add(1, Ordering::Relaxed);
        R4_LAST.store(core::panic::Location::caller() as *const _ as usize, Ordering::Relaxed);
        R4_LAST_ST.store(((old as u32) << 8) | new as u32, Ordering::Relaxed);
    } else if s.on_rq.load(Ordering::Relaxed) != 0 {
        g.remove(i);
        R4_DEQ.fetch_add(1, Ordering::Relaxed);
    }
    drop(g);
}

/// The caller holds `RQ[cpu]` of `idx` (see `RunQueue::try_park`): store
/// the state without the R4 dance (the caller dequeues).
pub(crate) fn note_state_held(idx: u16, new: TaskState) {
    if idx == NO_SLOT { return; }
    SCHED[idx as usize].state.store(state_code(new), Ordering::Release);
}

/// The switch-back must take TASKS (see `sb_slow`).
#[inline]
pub(crate) fn mark_sb_slow(idx: u16) {
    if idx != NO_SLOT { SCHED[idx as usize].sb_slow.store(1, Ordering::Relaxed); }
}

// ── Wakes awaiting queue placement ──────────────────────────────────────────

const WOKEN_WORDS: usize = MAX_TASKS / 64;
/// Bit `i`: slot `i` was made Ready and is not yet placed on a CpuQueue.
/// Written and drained under RUN_QUEUE only (atomics because `Task::set_state`
/// reaches it through `&mut Task`, without the `RunQueue`).
static WOKEN: [AtomicU64; WOKEN_WORDS] = [const { AtomicU64::new(0) }; WOKEN_WORDS];
static WOKEN_ANY: AtomicU32 = AtomicU32::new(0);

#[inline]
pub(crate) fn mark_woken(i: usize) {
    WOKEN[i / 64].fetch_or(1u64 << (i % 64), Ordering::Relaxed);
    WOKEN_ANY.store(1, Ordering::Relaxed);
}

#[inline]
pub(crate) fn woken_any() -> bool { WOKEN_ANY.load(Ordering::Relaxed) != 0 }

/// Take every pending wake (call under RUN_QUEUE): `f(slot)` for each.
#[inline]
pub(crate) fn drain_woken(mut f: impl FnMut(usize)) {
    if WOKEN_ANY.load(Ordering::Relaxed) == 0 { return; }
    WOKEN_ANY.store(0, Ordering::Relaxed);
    for w in 0..WOKEN_WORDS {
        if WOKEN[w].load(Ordering::Relaxed) == 0 { continue; }
        let mut bits = WOKEN[w].swap(0, Ordering::Relaxed);
        while bits != 0 {
            let b = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            f(w * 64 + b);
        }
    }
}

/// Put a drained wake back (the drain's batch was full).
#[inline]
pub(crate) fn requeue_woken(i: usize) { mark_woken(i); }

/// Is a wake of slot `i` pending placement?
#[inline]
pub(crate) fn woken_pending(i: usize) -> bool {
    WOKEN[i / 64].load(Ordering::Relaxed) & (1u64 << (i % 64)) != 0
}

/// Forget a pending wake of a slot being freed.
#[inline]
pub(crate) fn clear_woken(i: usize) {
    WOKEN[i / 64].fetch_and(!(1u64 << (i % 64)), Ordering::Relaxed);
}

// Queue counters (see `cpuq.rs` and `RunQueue::check_all`).
pub static RQMISS: AtomicU64 = AtomicU64::new(0);
pub static RQMISS_PID: AtomicU32 = AtomicU32::new(0);
pub static WOKEN_LATE: AtomicU64 = AtomicU64::new(0);
pub static RQ_BAD: AtomicU64 = AtomicU64::new(0);
pub static STALE: AtomicU64 = AtomicU64::new(0);
pub static GEN_MISS: AtomicU64 = AtomicU64::new(0);

// ── EEVDF entity (caller holds RQ[cpu], or the task is unqueued and off-CPU
//    under TASKS) ─────────────────────────────────────────────────────────

/// Nanoseconds per 100 Hz tick: vruntime is charged `VT_PER_TICK` per tick
/// of real CPU time (nice 0), measured in ns.
const NS_PER_TICK: u64 = 10_000_000;

#[inline]
pub(crate) fn vr(idx: usize) -> u64 { SCHED[idx].vr.load(Ordering::Relaxed) }
#[inline]
pub(crate) fn vd(idx: usize) -> u64 { SCHED[idx].vd.load(Ordering::Relaxed) }
#[inline]
pub(crate) fn weight(idx: usize) -> u32 { SCHED[idx].weight.load(Ordering::Relaxed).max(1) }

/// Clamp `vruntime` up to `min_vr` (lag limiting) and grant a fresh deadline.
#[inline]
pub(crate) fn place(idx: usize, min_vr: u64) {
    let s = &SCHED[idx];
    let v = s.vr.load(Ordering::Relaxed).max(min_vr);
    s.vr.store(v, Ordering::Relaxed);
    s.vd.store(v + Task::slice_vt(weight(idx)), Ordering::Relaxed);
}

/// Charge a dispatch that ran `run_ns`. A minimum of one unit (~10 µs) per
/// dispatch (see `VT_PER_TICK`). Charged in ns, not ticks (lane percpu):
/// with per-CPU queues the virtual time is compared ACROSS CPUs by the tick
/// balance, so it must follow real CPU time, and a tick-granular charge
/// billed a dispatch that started mid-tick a whole tick or nothing.
#[inline]
pub(crate) fn charge(idx: usize, run_ns: u64) {
    let s = &SCHED[idx];
    let w = weight(idx) as u64;
    let c = (run_ns.min(1u64 << 40) as u128 * (crate::task::VT_PER_TICK * crate::task::NICE0_WEIGHT) as u128
        / (NS_PER_TICK as u128 * w as u128)) as u64;
    let v = s.vr.load(Ordering::Relaxed) + c.max(1);
    s.vr.store(v, Ordering::Relaxed);
    if v >= s.vd.load(Ordering::Relaxed) {
        s.vd.store(v + Task::slice_vt(w as u32), Ordering::Relaxed);
    }
}

// ── Run-guard owner accounting ──────────────────────────────────────────────

/// Switch-back accounting of a dispatch that ran `run_ns`, `sys` of it in
/// syscalls; the syscall it was switched out in (`nr`, `acc` ns so far).
#[inline]
pub(crate) fn account(idx: usize, run_ns: u64, sys: u64, nr: u32, acc: u64, t0: u64) {
    let s = &SCHED[idx];
    let c = s.cpu_ns.load(Ordering::Relaxed) + run_ns;
    s.cpu_ns.store(c, Ordering::Relaxed);
    s.sys_ns.store((s.sys_ns.load(Ordering::Relaxed) + sys).min(c), Ordering::Relaxed);
    s.sc_nr.store(nr, Ordering::Relaxed);
    s.sc_ns.store(acc, Ordering::Relaxed);
    s.sc_t0.store(t0, Ordering::Relaxed);
}

// ── Dispatch mirror ─────────────────────────────────────────────────────────

/// The dispatch values derived from `t`, in mirror order.
#[inline]
fn values(t: &Task) -> [u64; 10] {
    let as_ptr = t.address_space.as_ref()
        .map_or(0usize, |a| alloc::sync::Arc::as_ptr(a) as usize);
    [
        &t.ctx as *const _ as u64,
        t.kernel_stack as u64,
        t.page_table as u64,
        as_ptr as u64,
        ((t.rseq.addr as u64) ^ ((t.rseq.sig as u64) << 48)),
        &t.sig_hint as *const _ as u64,
        t.user_gs_base,
        t.reply_port as u64,
        t.pid as u64,
        t.tgid as u64,
    ]
}

#[inline]
fn mirror(idx: usize) -> [u64; 10] {
    let s = &SCHED[idx];
    [
        s.m_ctx.load(Ordering::Relaxed) as u64,
        s.m_kst.load(Ordering::Relaxed) as u64,
        s.m_pt.load(Ordering::Relaxed) as u64,
        s.m_as.load(Ordering::Relaxed) as u64,
        (s.m_rseq_addr.load(Ordering::Relaxed) as u64) ^ ((s.m_rseq_sig.load(Ordering::Relaxed) as u64) << 48),
        s.m_sighint.load(Ordering::Relaxed) as u64,
        s.m_gs.load(Ordering::Relaxed),
        s.m_reply.load(Ordering::Relaxed) as u64,
        s.m_pid.load(Ordering::Relaxed) as u64,
        s.m_tgid.load(Ordering::Relaxed) as u64,
    ]
}

/// Re-publish the whole dispatch mirror of `t` (a no-op for a task not in
/// the table). Called next to every write of a mirrored field.
pub fn publish(t: &Task) {
    if t.slot == NO_SLOT { return; }
    let s = &SCHED[t.slot as usize];
    let as_ptr = t.address_space.as_ref()
        .map_or(0usize, |a| alloc::sync::Arc::as_ptr(a) as usize);
    s.m_ctx.store(&t.ctx as *const _ as usize, Ordering::Relaxed);
    s.m_kst.store(t.kernel_stack, Ordering::Relaxed);
    s.m_pt.store(t.page_table, Ordering::Relaxed);
    s.m_as.store(as_ptr, Ordering::Relaxed);
    s.m_rseq_addr.store(t.rseq.addr, Ordering::Relaxed);
    s.m_rseq_sig.store(t.rseq.sig, Ordering::Relaxed);
    s.m_sighint.store(&t.sig_hint as *const _ as usize, Ordering::Relaxed);
    s.m_gs.store(t.user_gs_base, Ordering::Relaxed);
    s.m_reply.store(t.reply_port, Ordering::Relaxed);
    s.m_pid.store(t.pid, Ordering::Relaxed);
    s.m_tgid.store(t.tgid, Ordering::Release);
}

/// Tick-checker comparison: the mirror must equal what the task says. A
/// mismatch is a write site that forgot `publish`: counted (first field
/// index kept), healed.
pub fn check_mirror(idx: usize, t: &Task) {
    let v = values(t);
    let m = mirror(idx);
    if v != m {
        let f = (0..10).find(|&i| v[i] != m[i]).unwrap_or(0);
        MIRROR_MISS.fetch_add(1, Ordering::Relaxed);
        MIRROR_MISS_FIELD.store(f as u32 + 1, Ordering::Relaxed);
        publish(t);
    }
}

/// What a dispatch needs, read from the mirror.
pub(crate) struct Dispatch {
    pub ctx: usize,
    pub kst: usize,
    pub pt: usize,
    pub as_ptr: usize,
    pub sighint: usize,
    pub gs: u64,
    pub rseq_addr: usize,
    pub rseq_sig: u32,
    pub reply: u32,
    pub pid: u32,
    pub tgid: u32,
}

#[inline]
pub(crate) fn dispatch_values(idx: usize) -> Dispatch {
    let s = &SCHED[idx];
    let tgid = s.m_tgid.load(Ordering::Acquire);
    Dispatch {
        ctx: s.m_ctx.load(Ordering::Relaxed), kst: s.m_kst.load(Ordering::Relaxed),
        pt: s.m_pt.load(Ordering::Relaxed), as_ptr: s.m_as.load(Ordering::Relaxed),
        sighint: s.m_sighint.load(Ordering::Relaxed), gs: s.m_gs.load(Ordering::Relaxed),
        rseq_addr: s.m_rseq_addr.load(Ordering::Relaxed), rseq_sig: s.m_rseq_sig.load(Ordering::Relaxed),
        reply: s.m_reply.load(Ordering::Relaxed), pid: s.m_pid.load(Ordering::Relaxed), tgid,
    }
}

/// `Task::magic` of the task in slot `idx`, through the mirror's context
/// pointer (the box never moves; the caller's queue/claim keeps it alive).
#[inline]
fn magic_of(idx: usize) -> u32 {
    let ctx = SCHED[idx].m_ctx.load(Ordering::Relaxed);
    if ctx == 0 { return 0; }
    let t = (ctx - core::mem::offset_of!(Task, ctx)) as *const Task;
    unsafe { core::ptr::read_volatile(core::ptr::addr_of!((*t).magic)) }
}

/// A task joins the table at `idx`.
pub(crate) fn on_insert(idx: usize, t: &mut Task) {
    t.slot = idx as u16;
    let s = &SCHED[idx];
    s.on_cpu.store(0, Ordering::Relaxed);
    s.on_rq.store(0, Ordering::Relaxed);
    s.sb_slow.store(0, Ordering::Relaxed);
    s.weight.store(t.weight.max(1), Ordering::Relaxed);
    s.vr.store(0, Ordering::Relaxed);
    s.vd.store(0, Ordering::Relaxed);
    s.cpu_ns.store(0, Ordering::Relaxed);
    s.sys_ns.store(0, Ordering::Relaxed);
    s.sc_ns.store(0, Ordering::Relaxed);
    s.sc_t0.store(0, Ordering::Relaxed);
    s.sc_nr.store(crate::NO_SYSCALL, Ordering::Relaxed);
    for c in [&s.nvcsw, &s.nivcsw, &s.minflt, &s.majflt, &s.io_rd] { c.store(0, Ordering::Relaxed); }
    s.state.store(state_code(t.state), Ordering::Release);
    s.ready_since.store(crate::monotonic_ns(), Ordering::Relaxed);
    publish(t);
}

/// The task at `idx` leaves the table (its box goes to the reaper); the
/// caller holds TASKS and `RQ[cpu]`. The run guard must be clear: a slot
/// freed while its task is still claimed by a CPU is a use-after-reap in
/// the making.
pub(crate) fn on_remove(idx: usize, t: &mut Task) {
    let s = &SCHED[idx];
    let oc = s.on_cpu.load(Ordering::Acquire);
    if oc != 0 {
        panic!("[SCHED] slot {} (pid {}) freed while claimed by cpu {}", idx, t.pid, oc - 1);
    }
    s.gen.fetch_add(1, Ordering::Release);
    s.state.store(0, Ordering::Release);
    t.slot = NO_SLOT;
}

/// Dispatch claims the run guard of `idx` for `cpu` (under `RQ[cpu]`).
/// Panics on a double run or a poisoned task.
#[inline]
pub(crate) fn claim(idx: usize, cpu: usize) {
    let m = magic_of(idx);
    if m != TASK_MAGIC {
        panic!("[SCHED] dispatch of slot {} (pid {}): task poisoned/corrupt (magic {:#x})",
               idx, SCHED[idx].m_pid.load(Ordering::Relaxed), m);
    }
    if let Err(other) = SCHED[idx].on_cpu.compare_exchange(0, cpu as u32 + 1, Ordering::AcqRel, Ordering::Acquire) {
        panic!("[SCHED] double run: slot {} (pid {}) dispatched on cpu {} while claimed by cpu {}",
               idx, SCHED[idx].m_pid.load(Ordering::Relaxed), cpu, other.wrapping_sub(1));
    }
    DISPATCHES.fetch_add(1, Ordering::Relaxed);
}

/// Switch-back releases the run guard of `idx`, which `cpu` must hold
/// (under `RQ[cpu]`).
#[inline]
pub(crate) fn release(idx: usize, cpu: usize) {
    let m = magic_of(idx);
    if m != TASK_MAGIC {
        panic!("[SCHED] switch-back of slot {} (pid {}): task poisoned/corrupt (magic {:#x})",
               idx, SCHED[idx].m_pid.load(Ordering::Relaxed), m);
    }
    if let Err(other) = SCHED[idx].on_cpu.compare_exchange(cpu as u32 + 1, 0, Ordering::AcqRel, Ordering::Acquire) {
        panic!("[SCHED] switch-back: slot {} (pid {}) run guard held by {} not cpu {}",
               idx, SCHED[idx].m_pid.load(Ordering::Relaxed), other.wrapping_sub(1), cpu);
    }
}

/// Tick-checker check of one live task (under TASKS and every RQ): the
/// state mirror agrees, and a Ready task off-CPU has not waited over a
/// second while `idle_cpu` is set.
pub(crate) fn scan_check(idx: usize, t: &Task, now_ns: u64, idle_cpu: bool, filtered: bool) {
    let s = &SCHED[idx];
    if s.state.load(Ordering::Relaxed) != state_code(t.state) {
        STATE_MISS.fetch_add(1, Ordering::Relaxed);
        s.state.store(state_code(t.state), Ordering::Release);
    }
    check_mirror(idx, t);
    if t.state == TaskState::Ready && s.on_cpu.load(Ordering::Relaxed) == 0 && idle_cpu && !filtered {
        let since = s.ready_since.load(Ordering::Relaxed);
        if now_ns.saturating_sub(since) > 1_000_000_000 {
            HUNG_READY.fetch_add(1, Ordering::Relaxed);
            HUNG_READY_PID.store(t.pid, Ordering::Relaxed);
            s.ready_since.store(now_ns, Ordering::Relaxed); // once per second per task
            // At most one console line a second in all: this runs under
            // every scheduler lock and a serial line is slow (TCG).
            static LAST_PRINT: AtomicU64 = AtomicU64::new(0);
            let last = LAST_PRINT.load(Ordering::Relaxed);
            if last != 0 && now_ns.saturating_sub(last) < 1_000_000_000 { return; }
            LAST_PRINT.store(now_ns.max(1), Ordering::Relaxed);
            extern "C" { fn serial_print(s: *const u8, len: usize); fn print_number(n: u32); }
            unsafe {
                let m = b"[SCHED] hung-ready: pid ";
                serial_print(m.as_ptr(), m.len());
                print_number(t.pid);
                let m2 = b" Ready >1 s with a CPU idle\n";
                serial_print(m2.as_ptr(), m2.len());
            }
        }
    }
}

pub fn fmt_stats(f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    let l = |c: &AtomicU64| c.load(Ordering::Relaxed);
    write!(f, " dispatches {} mirror_miss {} mirror_field {} state_miss {} hung_ready {} hung_pid {} rqmiss {} rqmiss_pid {} woken_late {} rq_bad {} gen_miss {} stale {} sb_fast {} sb_slow {} r4_deq {} r4_running {}",
        l(&DISPATCHES), l(&MIRROR_MISS), MIRROR_MISS_FIELD.load(Ordering::Relaxed),
        l(&STATE_MISS), l(&HUNG_READY), HUNG_READY_PID.load(Ordering::Relaxed),
        l(&RQMISS), RQMISS_PID.load(Ordering::Relaxed), l(&WOKEN_LATE), l(&RQ_BAD), l(&GEN_MISS),
        l(&STALE), l(&SB_FAST), l(&SB_SLOW), l(&R4_DEQ), l(&R4_RUNNING))?;
    let l4 = R4_LAST.load(Ordering::Relaxed);
    if l4 != 0 {
        let loc = unsafe { &*(l4 as *const core::panic::Location<'static>) };
        write!(f, " r4_last {}:{} st {:#x}", loc.file(), loc.line(), R4_LAST_ST.load(Ordering::Relaxed))?;
    }
    crate::cpuq::fmt_stats(f)
}
