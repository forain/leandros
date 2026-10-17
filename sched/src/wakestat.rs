//! Always-on poll-channel wake census, read through `/proc/wakestat`.
//!
//! WHY IT EXISTS. A poll-channel wake is a scan of every parked task under
//! RUN_QUEUE, and each task it makes Ready re-probes its interest set through
//! the global FD / pipe / epoll locks. On the real laptop during YouTube (all
//! eight CPUs busy, RUN_QUEUE ~516k acquisitions/s and half of them
//! contended) the cause was GPU fence wakes issued as broadcasts — the herd
//! was only visible after rebuilding with `idlestat::ENABLED` and reading the
//! serial log. This keeps the per-call-site part of that census on all the
//! time, cumulative since boot (diff two reads around a workload):
//!
//!   `calls`   wake calls from the site;
//!   `tagged`  of those, how many carried a narrow tag (not `POLL_TAG_ALL`);
//!   `woken`   tasks made Ready;
//!   `allmask` of those, tasks whose own mask was the broadcast mask, i.e.
//!             woken although they never named the object (only meaningful
//!             for a tagged wake; a broadcast wakes everyone anyway).
//!
//! COST. One open-addressed lookup in a 64-entry table and four relaxed
//! atomic adds per wake call — noise beside the RUN_QUEUE scan the call
//! already paid for.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

const SITES: usize = 64;
static SITE_FILE: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static SITE_LINE: [AtomicU32; SITES] = [const { AtomicU32::new(0) }; SITES];
static SITE_LEN: [AtomicU32; SITES] = [const { AtomicU32::new(0) }; SITES];
static SITE_CALLS: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static SITE_TAGGED: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static SITE_WOKEN: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
static SITE_ALLMASK: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];
/// Wake calls that found the table full (never expected: sites are static).
static OVERFLOW: AtomicU64 = AtomicU64::new(0);

/// Tasks with a broadcast (`POLL_TAG_ALL`) mask woken by the unblock that
/// just ran. Written by `RunQueue::unblock_port_tagged` and read by its caller
/// while both still hold RUN_QUEUE, so it is never torn between two scans.
pub(crate) static LAST_ALLMASK: AtomicU32 = AtomicU32::new(0);

/// Record a poll-channel wake from `loc`: `tagged` = not a broadcast tag,
/// `woken` tasks made Ready, `allmask` of them parked with the broadcast mask.
#[inline]
pub fn note(loc: &'static core::panic::Location<'static>, tagged: bool, woken: usize, allmask: usize) {
    let f = loc.file().as_ptr() as u64;
    let l = loc.line();
    let h = ((f >> 3) as usize ^ (l as usize).wrapping_mul(31)) % SITES;
    for k in 0..SITES {
        let i = (h + k) % SITES;
        if SITE_FILE[i].load(Relaxed) == 0
            && SITE_FILE[i].compare_exchange(0, f, Relaxed, Relaxed).is_ok()
        {
            SITE_LEN[i].store(loc.file().len() as u32, Relaxed);
            SITE_LINE[i].store(l, core::sync::atomic::Ordering::Release);
        }
        if SITE_FILE[i].load(Relaxed) == f && SITE_LINE[i].load(Relaxed) == l {
            SITE_CALLS[i].fetch_add(1, Relaxed);
            if tagged { SITE_TAGGED[i].fetch_add(1, Relaxed); }
            if woken != 0 { SITE_WOKEN[i].fetch_add(woken as u64, Relaxed); }
            if allmask != 0 { SITE_ALLMASK[i].fetch_add(allmask as u64, Relaxed); }
            return;
        }
        // A slot claimed by another (file, line) whose line is not published
        // yet reads as line 0 here; it simply probes on, at worst recording
        // this site twice. Diagnostics only.
    }
    OVERFLOW.fetch_add(1, Relaxed);
}

/// One recorded site: `(file, line, calls, tagged, woken, allmask)`.
pub type Site = (&'static str, u32, u64, u64, u64, u64);

/// Visit every recorded site, busiest (by tasks woken) not guaranteed first.
pub fn for_each(mut f: impl FnMut(Site)) {
    for i in 0..SITES {
        let p = SITE_FILE[i].load(Relaxed);
        let line = SITE_LINE[i].load(core::sync::atomic::Ordering::Acquire);
        if p == 0 || line == 0 { continue; }
        let n = SITE_LEN[i].load(Relaxed) as usize;
        // SAFETY: `p`/`n` are the pointer and length of a `&'static str`
        // (a `Location::file()`), published before `line` became non-zero.
        let file = unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(p as *const u8, n)) };
        f((file, line,
           SITE_CALLS[i].load(Relaxed), SITE_TAGGED[i].load(Relaxed),
           SITE_WOKEN[i].load(Relaxed), SITE_ALLMASK[i].load(Relaxed)));
    }
}

pub fn overflow() -> u64 { OVERFLOW.load(Relaxed) }
