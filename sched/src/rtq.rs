//! Real-time signal queue (SIGRTMIN..=SIGRTMAX, signals 32..=64).
//!
//! A standard signal has one pending instance: the `signal_pending` bit plus
//! the payload slot `Task::signal_info[signo - 1]`. A real-time signal must
//! queue: every `sigqueue`/`kill` of it is a separate instance with its own
//! `siginfo` (`si_value` above all), delivered in FIFO order (POSIX, Linux
//! `struct sigqueue`). The instances beyond what the per-task slot holds live
//! here, in one fixed pool inside `RunQueue`, guarded by RUN_QUEUE like every
//! other pending-signal field — no allocation, so a post from IRQ context
//! (a POSIX-timer expiry) is as safe as before.
//!
//! The layout rule, which every producer and consumer of an RT signal keeps:
//!
//! * **thread-private** instances (`shared == false`, `owner` = the thread
//!   id): the oldest is in the thread's `signal_info` slot with its
//!   `signal_pending` bit set, exactly like a standard signal; the newer
//!   ones queue here. Consuming the slot refills it from here
//!   ([`RtQueue::pop`]) and keeps the bit set while anything was refilled.
//! * **process-level** instances (`shared == true`, `owner` = the tgid):
//!   ALL of them queue here, the leader's `shared_signal_pending` bit set
//!   while any remains. The leader's slot is not used for them, because it
//!   is also the leader's own thread-private slot — sharing it is what made a
//!   standard signal's process-level and leader-private instances coalesce.
//!   Only when the pool is full does a process-level instance fall back to
//!   that slot (the old one-instance behaviour), and consumers fall back to
//!   reading the slot when the pool has no process-level entry.
//!
//! A POSIX timer (`SI_TIMER`) never queues a second instance of its own: it
//! owns at most one pending signal and counts further expiries as overruns,
//! as on Linux (`posix_timer_event` re-uses the timer's preallocated
//! sigqueue). Two timers sharing a signal each get theirs.

use crate::task::{Pid, SigInfo};

/// First real-time signal number (Linux kernel `SIGRTMIN`; libcs reserve the
/// first two or three for themselves and report a higher `SIGRTMIN()`).
pub const SIGRTMIN: u32 = 32;
/// Last signal number (`_NSIG`).
pub const SIGRTMAX: u32 = 64;

/// Queued instances across the whole system. POSIX's `_POSIX_SIGQUEUE_MAX`
/// floor is 32; Linux's per-user default is in the thousands. 56 bytes each.
pub const RTQ_MAX: usize = 1024;

#[inline]
pub const fn is_rt(signo: u32) -> bool { signo >= SIGRTMIN && signo <= SIGRTMAX }

#[derive(Clone, Copy)]
struct RtEntry {
    /// Thread id (private) or tgid (shared); 0 = free entry.
    owner:  Pid,
    shared: bool,
    sig:    u8,
    /// Arrival order (FIFO per (owner, shared, sig)).
    seq:    u64,
    info:   SigInfo,
}

impl RtEntry {
    const FREE: RtEntry = RtEntry { owner: 0, shared: false, sig: 0, seq: 0, info: SigInfo::NONE };
}

pub struct RtQueue {
    e:    [RtEntry; RTQ_MAX],
    used: usize,
    seq:  u64,
}

impl RtQueue {
    pub const fn new() -> Self { Self { e: [RtEntry::FREE; RTQ_MAX], used: 0, seq: 0 } }

    /// Queue one instance. False when the pool is full (the caller drops it,
    /// or reports EAGAIN to `sigqueue`).
    pub fn push(&mut self, owner: Pid, shared: bool, sig: u32, info: SigInfo) -> bool {
        if owner == 0 || self.used >= RTQ_MAX { return false; }
        let Some(i) = self.e.iter().position(|x| x.owner == 0) else { return false };
        self.seq += 1;
        self.e[i] = RtEntry { owner, shared, sig: sig as u8, seq: self.seq, info };
        self.used += 1;
        true
    }

    fn oldest(&self, owner: Pid, shared: bool, sig: u32) -> Option<usize> {
        if self.used == 0 { return None; }
        let mut best: Option<usize> = None;
        for (i, x) in self.e.iter().enumerate() {
            if x.owner == owner && x.shared == shared && x.sig as u32 == sig
                && best.map_or(true, |b| x.seq < self.e[b].seq) {
                best = Some(i);
            }
        }
        best
    }

    /// Dequeue the oldest instance of `sig` queued for `(owner, shared)`.
    pub fn pop(&mut self, owner: Pid, shared: bool, sig: u32) -> Option<SigInfo> {
        let i = self.oldest(owner, shared, sig)?;
        let info = self.e[i].info;
        self.e[i] = RtEntry::FREE;
        self.used -= 1;
        Some(info)
    }

    /// Whether any instance of `sig` is queued for `(owner, shared)`.
    pub fn has(&self, owner: Pid, shared: bool, sig: u32) -> bool {
        self.used != 0 && self.e.iter().any(|x|
            x.owner == owner && x.shared == shared && x.sig as u32 == sig)
    }

    /// Instances queued for `owner` (both levels), for diagnostics/tests.
    pub fn count(&self, owner: Pid) -> usize {
        if self.used == 0 { return 0; }
        self.e.iter().filter(|x| x.owner == owner).count()
    }

    /// Hand every entry owned by `from` to `to` (a non-leader exec taking
    /// over the leader's pid: its private queue follows it).
    pub fn rekey(&mut self, from: Pid, to: Pid) {
        if self.used == 0 || from == 0 || to == 0 { return; }
        for x in self.e.iter_mut() { if x.owner == from { x.owner = to; } }
    }

    /// Drop every entry owned by `owner` (its task slot is being freed: the
    /// pid may be reused, and nothing can dequeue them any more).
    pub fn purge(&mut self, owner: Pid) {
        if self.used == 0 || owner == 0 { return; }
        for x in self.e.iter_mut() {
            if x.owner == owner { *x = RtEntry::FREE; self.used -= 1; }
        }
    }

    /// Whether an instance of `sig` from POSIX timer `timerid` (SI_TIMER,
    /// `si_pid` = the timer id) is queued for `(owner, shared)`: a timer
    /// keeps at most one pending signal and counts the rest as overruns.
    pub fn has_timer(&self, owner: Pid, shared: bool, sig: u32, timerid: i32) -> bool {
        self.used != 0 && self.e.iter().any(|x|
            x.owner == owner && x.shared == shared && x.sig as u32 == sig
            && x.info.si_code == crate::task::SI_TIMER && x.info.si_pid == timerid)
    }
}
