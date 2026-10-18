//! Lane percpu phase 1: `SchedSlot`, the scheduling state of every task
//! slot, kept OUTSIDE the boxed `Task` in a static side array.
//!
//! Why outside: once pick and switch-back run under a per-CPU lock instead of
//! RUN_QUEUE (phase 3), they must not form a `&mut Task` while another CPU
//! holds RUN_QUEUE and may hold one too (aliasing UB across lock domains).
//! Everything pick and dispatch need lives here as atomics:
//!
//! - `state`: mirror of `Task::state` (`TaskState as u8 + 1`, 0 = empty
//!   slot). The only writer is `Task::set_state`, which writes both.
//! - `on_cpu`: the RUN GUARD, `cpu + 1` while dispatched, 0 otherwise. The
//!   dispatch claims it with a CAS 0 → cpu+1; a failed CAS means a task is
//!   about to run on two CPUs at once and panics (always on).
//! - `gen`: bumped every time the slot is freed. Phase 2's queue entries and
//!   any later lock-free reader carry the gen they saw; a mismatch is a
//!   use-after-reap. Freed tasks are also poisoned (`Task::magic`).
//! - `ready_since`: monotonic ns of the last transition to Ready; the
//!   hung-ready watchdog flags a task Ready and off-CPU for over a second
//!   while a CPU idles.
//! - The dispatch mirror (`m_*`): every value the dispatch reads from the
//!   task, re-published by `publish` next to every write of its source field
//!   (write-site table in the lane notes). Phase 1 still dispatches from the
//!   `Task`, compares the mirror and counts `mirror_miss` (must stay 0).
//!
//! All counters are printed by `fmt_stats` into `/proc/rqhold`.

use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use crate::runqueue::MAX_TASKS;
use crate::task::{Task, TaskState};

#[repr(C, align(64))]
pub struct SchedSlot {
    pub state: AtomicU8,
    pub on_cpu: AtomicU32,
    /// Home CPU (phase 2: whose queue the task joins when it becomes Ready).
    pub cpu: AtomicU32,
    /// Phase 2: `cpu + 1` of the CpuQueue holding the task, 0 = none.
    pub on_rq: AtomicU32,
    pub gen: AtomicU32,
    pub ready_since: AtomicU64,
    pub m_ctx: AtomicUsize,
    pub m_kst: AtomicUsize,
    pub m_pt: AtomicUsize,
    pub m_as: AtomicUsize,
    pub m_rseq: AtomicUsize,
    pub m_sighint: AtomicUsize,
    pub m_gs: AtomicU64,
    pub m_reply: AtomicU32,
    pub m_pid: AtomicU32,
    pub m_tgid: AtomicU32,
}

impl SchedSlot {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(0), on_cpu: AtomicU32::new(0), cpu: AtomicU32::new(0),
            on_rq: AtomicU32::new(0), gen: AtomicU32::new(0), ready_since: AtomicU64::new(0),
            m_ctx: AtomicUsize::new(0), m_kst: AtomicUsize::new(0), m_pt: AtomicUsize::new(0),
            m_as: AtomicUsize::new(0), m_rseq: AtomicUsize::new(0), m_sighint: AtomicUsize::new(0),
            m_gs: AtomicU64::new(0), m_reply: AtomicU32::new(0), m_pid: AtomicU32::new(0),
            m_tgid: AtomicU32::new(0),
        }
    }
}

/// All zero: lands in .bss.
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

#[inline]
pub fn state_code(s: TaskState) -> u8 { s as u8 + 1 }

#[inline]
pub fn slot(idx: usize) -> &'static SchedSlot { &SCHED[idx] }

/// Mirror `Task::state` (called by `Task::set_state` only).
#[inline]
pub(crate) fn note_state(idx: u16, old: TaskState, new: TaskState) {
    if idx == NO_SLOT { return; }
    let s = &SCHED[idx as usize];
    s.state.store(state_code(new), Ordering::Release);
    if new == TaskState::Ready && old != TaskState::Ready {
        s.ready_since.store(crate::monotonic_ns(), Ordering::Relaxed);
    }
}

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
        &t.rseq as *const _ as u64,
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
        s.m_rseq.load(Ordering::Relaxed) as u64,
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
    let v = values(t);
    let s = &SCHED[t.slot as usize];
    s.m_ctx.store(v[0] as usize, Ordering::Relaxed);
    s.m_kst.store(v[1] as usize, Ordering::Relaxed);
    s.m_pt.store(v[2] as usize, Ordering::Relaxed);
    s.m_as.store(v[3] as usize, Ordering::Relaxed);
    s.m_rseq.store(v[4] as usize, Ordering::Relaxed);
    s.m_sighint.store(v[5] as usize, Ordering::Relaxed);
    s.m_gs.store(v[6], Ordering::Relaxed);
    s.m_reply.store(v[7] as u32, Ordering::Relaxed);
    s.m_pid.store(v[8] as u32, Ordering::Relaxed);
    s.m_tgid.store(v[9] as u32, Ordering::Relaxed);
}

/// Dispatch-time check: the mirror must equal what the task says. A
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

/// A task joins the table at `idx`.
pub(crate) fn on_insert(idx: usize, t: &mut Task) {
    t.slot = idx as u16;
    let s = &SCHED[idx];
    s.on_cpu.store(0, Ordering::Relaxed);
    s.on_rq.store(0, Ordering::Relaxed);
    s.state.store(state_code(t.state), Ordering::Release);
    s.ready_since.store(crate::monotonic_ns(), Ordering::Relaxed);
    publish(t);
}

/// The task at `idx` leaves the table (its box goes to the reaper). The run
/// guard must be clear: a slot freed while its task is still claimed by a CPU
/// is a use-after-reap in the making.
pub(crate) fn on_remove(idx: usize, t: &mut Task) {
    let s = &SCHED[idx];
    let oc = s.on_cpu.load(Ordering::Acquire);
    if oc != 0 && t.on_cpu.is_some() {
        panic!("[SCHED] slot {} (pid {}) freed while claimed by cpu {}", idx, t.pid, oc - 1);
    }
    s.gen.fetch_add(1, Ordering::Release);
    s.state.store(0, Ordering::Release);
    t.slot = NO_SLOT;
}

/// Dispatch claims the run guard of `idx` for `cpu`. Panics on a double run.
#[inline]
pub(crate) fn claim(idx: usize, cpu: usize, t: &Task) {
    if t.magic != TASK_MAGIC {
        panic!("[SCHED] dispatch of slot {} (pid {}): task poisoned/corrupt (magic {:#x})", idx, t.pid, t.magic);
    }
    if let Err(other) = SCHED[idx].on_cpu.compare_exchange(0, cpu as u32 + 1, Ordering::AcqRel, Ordering::Acquire) {
        panic!("[SCHED] double run: slot {} (pid {}) dispatched on cpu {} while claimed by cpu {}",
               idx, t.pid, cpu, other.wrapping_sub(1));
    }
    DISPATCHES.fetch_add(1, Ordering::Relaxed);
}

/// Switch-back releases the run guard of `idx`, which `cpu` must hold.
#[inline]
pub(crate) fn release(idx: usize, cpu: usize, t: &Task) {
    if t.magic != TASK_MAGIC {
        panic!("[SCHED] switch-back of slot {} (pid {}): task poisoned/corrupt (magic {:#x})", idx, t.pid, t.magic);
    }
    if let Err(other) = SCHED[idx].on_cpu.compare_exchange(cpu as u32 + 1, 0, Ordering::AcqRel, Ordering::Acquire) {
        panic!("[SCHED] switch-back: slot {} (pid {}) run guard held by {} not cpu {}",
               idx, t.pid, other.wrapping_sub(1), cpu);
    }
}

/// Full-scan check of one live task (once per tick, under RUN_QUEUE): the
/// state mirror agrees, and a Ready task off-CPU has not waited over a
/// second while `idle_cpu` is set.
pub(crate) fn scan_check(idx: usize, t: &Task, now_ns: u64, idle_cpu: bool, filtered: bool) {
    let s = &SCHED[idx];
    if s.state.load(Ordering::Relaxed) != state_code(t.state) {
        STATE_MISS.fetch_add(1, Ordering::Relaxed);
        s.state.store(state_code(t.state), Ordering::Release);
    }
    if t.state == TaskState::Ready && t.on_cpu.is_none() && idle_cpu && !filtered {
        let since = s.ready_since.load(Ordering::Relaxed);
        if now_ns.saturating_sub(since) > 1_000_000_000 {
            HUNG_READY.fetch_add(1, Ordering::Relaxed);
            HUNG_READY_PID.store(t.pid, Ordering::Relaxed);
            s.ready_since.store(now_ns, Ordering::Relaxed); // once per second per task
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
    write!(f, " dispatches {} mirror_miss {} mirror_field {} state_miss {} hung_ready {} hung_pid {}",
        l(&DISPATCHES), l(&MIRROR_MISS), MIRROR_MISS_FIELD.load(Ordering::Relaxed),
        l(&STATE_MISS), l(&HUNG_READY), HUNG_READY_PID.load(Ordering::Relaxed))
}
