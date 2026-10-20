//! Restartable sequences — rseq(2).
//!
//! A thread registers a `struct rseq` in its own memory (glibc does it for
//! every thread at start-up; musl does not). The kernel then keeps two
//! promises about it:
//!
//!  1. `cpu_id_start` / `cpu_id` (and `node_id` / `mm_cid`) name the CPU the
//!     thread is running on whenever it is in user space, so per-CPU data can
//!     be indexed without a syscall.
//!  2. If the thread is preempted, migrated or handed a signal while its user
//!     PC lies inside the critical section `rseq_cs` describes, it resumes at
//!     that section's `abort_ip` instead of at the interrupted instruction.
//!     `abort_ip` must be preceded by the 4-byte signature given at
//!     registration, or the thread takes SIGSEGV (a forged descriptor must
//!     not be able to redirect execution anywhere it likes).
//!
//! # Cost on the return-to-user path
//!
//! Both promises are kept from `check_and_deliver_signals`, which every
//! return to user space already runs. A per-syscall cost there has meant 5x
//! slowdowns before, so the whole mechanism is gated on one per-CPU flag,
//! [`RESUME`]: the scheduler sets it when it dispatches a task that has an
//! area registered (a dispatch is exactly "the thread was off this CPU — it
//! may have been preempted or moved"), and the return path does a relaxed
//! load of it. Threads without a registration — every musl thread, and every
//! syscall that did not block — pay that load and nothing else. Signal
//! delivery reaches [`on_signal_deliver`] only when a handler frame is about
//! to be built, a path that already costs several user-memory writes.
//!
//! Lifetime rules (Linux): fork() inherits the parent thread's registration
//! (the child's memory is a copy, so the address stays valid); a CLONE_VM
//! child — thread or vfork — starts unregistered; execve clears it.
//!
//! Analogue: Linux kernel/rseq.c.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::MAX_CPUS;

/// One thread's registration. All-zero is "none" — tasks built in place from
/// zeroed memory are unregistered without further initialisation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RseqReg {
    /// User address of the thread's `struct rseq`, 0 = not registered.
    pub addr: usize,
    /// `rseq_len` as registered (unregister must repeat it).
    pub len: u32,
    /// The abort-handler signature.
    pub sig: u32,
}

impl RseqReg {
    pub const NONE: RseqReg = RseqReg { addr: 0, len: 0, sig: 0 };
}

/// `struct rseq` before the extensible tail: Linux's ORIG_RSEQ_SIZE, and the
/// alignment the ABI requires of the area.
pub const ORIG_RSEQ_SIZE: usize = 32;
/// `offsetof(struct rseq, end)` with `node_id` and `mm_cid` — the value of
/// `AT_RSEQ_FEATURE_SIZE`.
pub const RSEQ_FEATURE_SIZE: usize = 28;
/// `__alignof__(struct rseq)` — the value of `AT_RSEQ_ALIGN`.
pub const RSEQ_ALIGN: usize = 32;

const RSEQ_FLAG_UNREGISTER: usize = 1;
/// `cpu_id` of an unregistered area (Linux RSEQ_CPU_ID_UNINITIALIZED).
const RSEQ_CPU_ID_UNINITIALIZED: u32 = u32::MAX;

// Field offsets in `struct rseq`.
const OFF_CPU_ID_START: usize = 0;
const OFF_RSEQ_CS: usize = 8;
const OFF_NODE_ID: usize = 20;

/// Registration of the task currently dispatched on each CPU (address and
/// signature; the length is only needed by the syscall, which reads the
/// task). Written at dispatch under RUN_QUEUE, read lock-free by the CPU's
/// own return path.
static CUR_ADDR: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static CUR_SIG: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
/// "Refresh cpu_id and check for an interrupted critical section before the
/// next return to user space" (Linux TIF_NOTIFY_RESUME, rseq part).
static RESUME: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

#[inline]
fn cpu() -> usize { unsafe { crate::cpu_id() }.min(MAX_CPUS - 1) }

// ── mm_cid ──────────────────────────────────────────────────────────────────
//
// Linux's `mm_cid` is a concurrency id: unique among the threads of one mm
// that are running at the same moment, and compact — below the number of
// such threads — so user space (tcmalloc, glibc's per-CPU caches) can index
// a small array with it where the CPU number would need one slot per
// possible CPU. A single-threaded process always sees 0.
//
// Here a registered thread takes the lowest free id of its thread group at
// dispatch and gives it back at switch-back. A group's ids are one 32-bit
// mask in a small hashed table keyed by tgid (`CID_TAB`, entry = tgid << 32 |
// mask; a zero mask is a free entry whatever the key). A group whose entry
// is taken by another group's running threads (a hash collision), or with
// all 32 ids in use, falls back to `CID_FALLBACK + cpu`: still unique, no
// longer compact.

const CID_SLOTS: usize = 1024;
const CID_FALLBACK: u32 = 32;
static CID_TAB: [AtomicU64; CID_SLOTS] = [const { AtomicU64::new(0) }; CID_SLOTS];
/// The id the thread on each CPU holds (u32::MAX = none) and the `CID_TAB`
/// entry it came from (usize::MAX = a fallback id, nothing to give back).
static CUR_CID: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(u32::MAX) }; MAX_CPUS];
static CUR_CID_SLOT: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(usize::MAX) }; MAX_CPUS];

/// A registered thread of `tgid` is dispatched on `cpu`: take its group's
/// lowest free concurrency id (see the section comment).
pub(crate) fn cid_acquire(cpu: usize, tgid: u32) {
    let cpu = cpu.min(MAX_CPUS - 1);
    if CUR_CID[cpu].load(Ordering::Relaxed) != u32::MAX { return; } // held already
    let slot = ((tgid as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 54) as usize % CID_SLOTS;
    let e = &CID_TAB[slot];
    let mut v = e.load(Ordering::Acquire);
    loop {
        let (key, mask) = ((v >> 32) as u32, v as u32);
        if (mask != 0 && key != tgid) || mask == u32::MAX {
            CUR_CID_SLOT[cpu].store(usize::MAX, Ordering::Relaxed);
            CUR_CID[cpu].store(CID_FALLBACK + cpu as u32, Ordering::Relaxed);
            return;
        }
        let bit = (!mask).trailing_zeros();
        let new = ((tgid as u64) << 32) | (mask | (1u32 << bit)) as u64;
        match e.compare_exchange_weak(v, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                CUR_CID_SLOT[cpu].store(slot, Ordering::Relaxed);
                CUR_CID[cpu].store(bit, Ordering::Relaxed);
                return;
            }
            Err(cur) => v = cur,
        }
    }
}

/// The thread on `cpu` leaves it (switch-back, or unregistration): give its
/// concurrency id back.
pub(crate) fn cid_release(cpu: usize) {
    let cpu = cpu.min(MAX_CPUS - 1);
    let cid = CUR_CID[cpu].swap(u32::MAX, Ordering::Relaxed);
    let slot = CUR_CID_SLOT[cpu].swap(usize::MAX, Ordering::Relaxed);
    if cid == u32::MAX || slot >= CID_SLOTS || cid >= 32 { return; }
    CID_TAB[slot].fetch_and(!(1u64 << cid), Ordering::AcqRel);
}

/// `mm_cid` for the thread on `cpu` (its CPU number if it holds none).
fn cur_cid(cpu: usize) -> u32 {
    let c = CUR_CID[cpu.min(MAX_CPUS - 1)].load(Ordering::Relaxed);
    if c == u32::MAX { cpu as u32 } else { c }
}

/// Scheduler hook: `reg` is about to run on `cpu`. Three stores; the flag is
/// what makes the next return to user space look at the area at all.
#[inline]
pub(crate) fn on_dispatch(cpu: usize, reg: &RseqReg) {
    let cpu = cpu.min(MAX_CPUS - 1);
    CUR_ADDR[cpu].store(reg.addr, Ordering::Relaxed);
    CUR_SIG[cpu].store(reg.sig, Ordering::Relaxed);
    RESUME[cpu].store(reg.addr != 0, Ordering::Relaxed);
}

/// Return-to-user hook (from `check_and_deliver_signals`): the cheap gate.
#[inline]
pub(crate) fn notify_resume(frame_ptr: usize) {
    let c = cpu();
    if !RESUME[c].load(Ordering::Relaxed) { return; }
    RESUME[c].store(false, Ordering::Relaxed);
    handle(c, frame_ptr);
}

/// True when a dispatch since the last [`notify_resume`] asked for another
/// one (the signal-delivery loop can stop and resume the thread elsewhere).
#[inline]
pub(crate) fn resume_pending() -> bool { RESUME[cpu()].load(Ordering::Relaxed) }

/// Signal-delivery hook: a handler frame is about to capture the user
/// context, so an interrupted critical section must be aborted first — the
/// frame then saves `abort_ip`, and the handler's return lands there.
#[inline]
pub(crate) fn on_signal_deliver(frame_ptr: usize) {
    let c = cpu();
    if CUR_ADDR[c].load(Ordering::Relaxed) == 0 { return; }
    handle(c, frame_ptr);
}

/// The registration fork() hands its child: the calling thread's own.
pub(crate) fn current_registration() -> RseqReg {
    let pid = crate::current_pid();
    crate::RUN_QUEUE.lock().find_pid(pid).map(|t| t.rseq).unwrap_or(RseqReg::NONE)
}

/// execve: the area named memory of the image being replaced.
pub(crate) fn clear_on_exec(t: &mut crate::task::Task) {
    t.rseq = RseqReg::NONE;
    crate::slot::publish(t);
}

/// After the registration was cleared, forget the per-CPU copy too so the
/// return path stops looking at the old address. Call with RUN_QUEUE held
/// (see `sys_rseq`).
pub(crate) fn forget_current() {
    let c = cpu();
    CUR_ADDR[c].store(0, Ordering::Relaxed);
    CUR_SIG[c].store(0, Ordering::Relaxed);
    RESUME[c].store(false, Ordering::Relaxed);
}

/// Program counter of the user context in `frame_ptr`.
fn frame_pc(frame_ptr: usize) -> u64 {
    let f = unsafe { &*(frame_ptr as *const crate::context::UserFrame) };
    #[cfg(target_arch = "x86_64")]
    { f.rip }
    #[cfg(target_arch = "aarch64")]
    { f.elr_el1 }
}

fn set_frame_pc(frame_ptr: usize, pc: u64) {
    let f = unsafe { &mut *(frame_ptr as *mut crate::context::UserFrame) };
    // Every return to ring 3 is an iretq through the frame's rip; rcx is a
    // live user register on the IRQ/fault paths and must not be touched.
    #[cfg(target_arch = "x86_64")]
    { f.rip = pc; }
    #[cfg(target_arch = "aarch64")]
    { f.elr_el1 = pc; }
}

/// Write the CPU fields of the area at `addr`: cpu_id_start, cpu_id,
/// node_id (always 0, one NUMA node) and mm_cid (see `cid_acquire`).
fn write_cpu_fields(addr: usize, cpu: u32) -> bool {
    crate::uaccess::write_user::<[u32; 2]>(addr + OFF_CPU_ID_START, [cpu, cpu])
        && crate::uaccess::write_user::<[u32; 2]>(addr + OFF_NODE_ID, [0, cur_cid(cpu as usize)])
}

/// Refresh the area's CPU fields and abort an interrupted critical section.
/// Any malformed state (an unreadable area or descriptor, a descriptor out of
/// range, a signature mismatch) is a SIGSEGV, as on Linux.
fn handle(cpu: usize, frame_ptr: usize) {
    let addr = CUR_ADDR[cpu].load(Ordering::Relaxed);
    if addr == 0 || frame_ptr == 0 { return; }
    if fixup_ip(addr, CUR_SIG[cpu].load(Ordering::Relaxed), frame_ptr).is_err()
        || !write_cpu_fields(addr, cpu as u32)
    {
        force_sigsegv();
    }
}

/// Linux `rseq_ip_fixup`: if the interrupted PC is inside the critical
/// section, clear `rseq_cs` and resume at `abort_ip`; if it is outside, just
/// clear a stale `rseq_cs` (the lazy clear the ABI relies on).
fn fixup_ip(addr: usize, sig: u32, frame_ptr: usize) -> Result<(), ()> {
    use crate::uaccess::{read_user, write_user};
    const USER_END: u64 = crate::uaccess::USER_END as u64;
    let cs_ptr: u64 = read_user(addr + OFF_RSEQ_CS).ok_or(())?;
    if cs_ptr == 0 { return Ok(()); }
    if cs_ptr >= USER_END { return Err(()); }
    // struct rseq_cs { u32 version; u32 flags; u64 start_ip;
    //                  u64 post_commit_offset; u64 abort_ip; }
    let cs: [u64; 4] = read_user(cs_ptr as usize).ok_or(())?;
    let version = cs[0] as u32;
    let (start_ip, post_commit_offset, abort_ip) = (cs[1], cs[2], cs[3]);
    let end = start_ip.checked_add(post_commit_offset).ok_or(())?;
    if version != 0 || start_ip >= USER_END || end >= USER_END || abort_ip >= USER_END {
        return Err(());
    }
    // The abort handler may not lie inside the section it aborts.
    if abort_ip.wrapping_sub(start_ip) < post_commit_offset { return Err(()); }
    if abort_ip < 4 { return Err(()); }
    let found: u32 = read_user((abort_ip - 4) as usize).ok_or(())?;
    if found != sig { return Err(()); }
    let pc = frame_pc(frame_ptr);
    // Clear rseq_cs either way: outside the section it is stale, inside it
    // is consumed by this abort.
    if !write_user::<u64>(addr + OFF_RSEQ_CS, 0) { return Err(()); }
    if pc.wrapping_sub(start_ip) < post_commit_offset {
        set_frame_pc(frame_ptr, abort_ip);
    }
    Ok(())
}

fn force_sigsegv() {
    const SIGSEGV: u32 = 11;
    if !crate::fault_signal(SIGSEGV, crate::task::SEGV_ACCERR, 0) {
        crate::exit_group_signal(SIGSEGV);
    }
}

/// rseq(rseq, rseq_len, flags, sig).
pub fn sys_rseq(addr: usize, len: usize, flags: usize, sig: usize) -> isize {
    let sig = sig as u32;
    let pid = crate::current_pid();
    let cur = crate::RUN_QUEUE.lock().find_pid(pid).map(|t| t.rseq).unwrap_or(RseqReg::NONE);

    if flags & RSEQ_FLAG_UNREGISTER != 0 {
        if flags & !RSEQ_FLAG_UNREGISTER != 0 { return -22; }
        if cur.addr == 0 || cur.addr != addr { return -22; }    // EINVAL
        if cur.len as usize != len { return -22; }
        if cur.sig != sig { return -1; }                         // EPERM
        // Linux resets the CPU fields so a stale value is never mistaken
        // for a live one.
        let _ = crate::uaccess::write_user::<[u32; 2]>(addr + OFF_CPU_ID_START, [0, RSEQ_CPU_ID_UNINITIALIZED]);
        let _ = crate::uaccess::write_user::<[u32; 2]>(addr + OFF_NODE_ID, [0, 0]);
        let mut rq = crate::RUN_QUEUE.lock();
        if let Some(t) = rq.find_pid_mut(pid) { t.rseq = RseqReg::NONE; crate::slot::publish(t); }
        forget_current();
        cid_release(cpu());
        drop(rq);
        return 0;
    }
    if flags != 0 { return -22; }

    if cur.addr != 0 {
        if cur.addr != addr || cur.len as usize != len { return -22; }
        if cur.sig != sig { return -1; }
        return -16; // EBUSY: already registered
    }
    if len < ORIG_RSEQ_SIZE || len > u32::MAX as usize || addr % RSEQ_ALIGN != 0 { return -22; }
    if !crate::uaccess::user_range_ok(addr, len) { return -14; }

    // Fault the area in and check it is writable now, so a bad pointer is
    // EFAULT here rather than a SIGSEGV on the way out.
    crate::uaccess::prefault(addr, ORIG_RSEQ_SIZE, false);
    if !write_cpu_fields(addr, cpu() as u32) { return -14; }
    let reg = RseqReg { addr, len: len as u32, sig };
    // Task and per-CPU copy change in one RUN_QUEUE hold: nothing can yield
    // under the spinlock, so the CPU read here is the one the stores land on
    // (a stale index would point another task's return path at this area).
    // RESUME makes this very syscall's return publish the current CPU.
    let mut rq = crate::RUN_QUEUE.lock();
    if let Some(t) = rq.find_pid_mut(pid) { t.rseq = reg; crate::slot::publish(t); }
    on_dispatch(cpu(), &reg);
    cid_acquire(cpu(), crate::current_tgid());
    drop(rq);
    0
}
