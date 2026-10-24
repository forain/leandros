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

/// Keyed wakes (`wake_poll_key`), per candidate task whose bloom mask had the
/// key's bit: `KEY_SKIPPED` = not woken because only keyed interests explain
/// the bit and none is this key (a herd wake saved); `KEY_FP_HIT` = woken on a
/// fingerprint match; `KEY_UNREFINED` = woken because some unkeyed interest
/// (or a legacy / broadcast park) also contributed the bit. Read through
/// `/proc/wakestat` (`# keyed ...`).
pub static KEY_SKIPPED: AtomicU64 = AtomicU64::new(0);
pub static KEY_FP_HIT: AtomicU64 = AtomicU64::new(0);
pub static KEY_UNREFINED: AtomicU64 = AtomicU64::new(0);

/// epoll ready list (lane epollready), read through `/proc/wakestat`
/// (`# epoll ...`): epoll_wait calls on the ready-list path, calls whose
/// snapshot was stale (full first pass), interest probes, wakes with no
/// recorded reason (full re-probe), parks.
pub static EP_CALLS: AtomicU64 = AtomicU64::new(0);
pub static EP_MISS: AtomicU64 = AtomicU64::new(0);
pub static EP_PROBES: AtomicU64 = AtomicU64::new(0);
pub static EP_UNKNOWN: AtomicU64 = AtomicU64::new(0);
pub static EP_PARKS: AtomicU64 = AtomicU64::new(0);

/// Where an epoll_wait's kernel time goes (lane epollcost), read through
/// `/proc/wakestat` (`# epollx ...`): `EP_INNER` probes of a nested
/// instance's registrations made on behalf of an outer wait (readiness, tag
/// and filter collection); `EP_PASSES` probe passes; `EP_RECHECK` passes
/// caused only by a changed answer under a narrow filter; `EP_GENCANCEL`
/// parks cancelled because EPOLL_CTL_GEN moved; `EP_USELESS` wakes after
/// which the wait found nothing and parked again; `EP_PROBE_NS` time spent
/// in the probes of the ready-list path (inner probes included);
/// `EP_FILT_NS` time spent building and installing listener filters.
pub static EP_INNER: AtomicU64 = AtomicU64::new(0);
pub static EP_PASSES: AtomicU64 = AtomicU64::new(0);
pub static EP_RECHECK: AtomicU64 = AtomicU64::new(0);
pub static EP_GENCANCEL: AtomicU64 = AtomicU64::new(0);
pub static EP_USELESS: AtomicU64 = AtomicU64::new(0);
pub static EP_PROBE_NS: AtomicU64 = AtomicU64::new(0);
pub static EP_FILT_NS: AtomicU64 = AtomicU64::new(0);

/// EPOLLEXCLUSIVE (lane epollabi): parked tasks a keyed wake left parked
/// because another exclusive parker of the same key took it.
pub static EXCL_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Poll-channel narrow wake scans (lane wakescan, `RunQueue::wx_row`), read
/// through `/proc/wakestat` (`# wakeidx ...`): scans, of those indexed;
/// slots visited, Tasks dereferenced, tasks woken; candidates a keyed wake
/// dropped by the mirrors (bloom/kbits) without a dereference; and, under
/// `sched.wakeidx=2`, wakes/records the index would have lost (must be 0).
pub static WX_SCANS: AtomicU64 = AtomicU64::new(0);
pub static WX_INDEXED: AtomicU64 = AtomicU64::new(0);
pub static WX_VISITED: AtomicU64 = AtomicU64::new(0);
pub static WX_DEREFS: AtomicU64 = AtomicU64::new(0);
pub static WX_WOKEN: AtomicU64 = AtomicU64::new(0);
pub static WX_MSKIP: AtomicU64 = AtomicU64::new(0);
pub static WX_MISS: AtomicU64 = AtomicU64::new(0);

#[inline]
pub(crate) fn note_scan(indexed: bool, visited: u32, derefs: u32, woken: u32, mskip: u32, misses: u32) {
    WX_SCANS.fetch_add(1, Relaxed);
    if indexed { WX_INDEXED.fetch_add(1, Relaxed); }
    WX_VISITED.fetch_add(visited as u64, Relaxed);
    WX_DEREFS.fetch_add(derefs as u64, Relaxed);
    if woken != 0 { WX_WOKEN.fetch_add(woken as u64, Relaxed); }
    if mskip != 0 { WX_MSKIP.fetch_add(mskip as u64, Relaxed); }
    if misses != 0 { WX_MISS.fetch_add(misses as u64, Relaxed); }
}

/// poll()/ppoll() (lane epollabi), read through `/proc/wakestat`
/// (`# pollx ...`): calls, per-fd probes, real parks, and wakes after which
/// the call found nothing and parked again.
pub static PO_CALLS: AtomicU64 = AtomicU64::new(0);
pub static PO_PROBES: AtomicU64 = AtomicU64::new(0);
pub static PO_PARKS: AtomicU64 = AtomicU64::new(0);
pub static PO_USELESS: AtomicU64 = AtomicU64::new(0);
/// Parked poll() calls whose first pass's tags were installed directly
/// (no wake since its epoch read: the second full pass skipped).
pub static PO_PRESKIP: AtomicU64 = AtomicU64::new(0);

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

/// Per epoll instance attribution (lane epattr), read through `/proc/epollstat`.
/// A fixed table keyed by `EpollInstance::id & 255` (stamped with the id; a
/// colliding newer instance resets the row). Updated with relaxed atomics only:
/// no lock, no allocation on the wake path.
pub struct EpStat {
    pub id: AtomicU64,
    pub tgid: AtomicU32,
    pub epfd: AtomicU32,
    pub parks: AtomicU64,
    /// Wakes after which the wait returned events.
    pub useful: AtomicU64,
    /// Wakes after which the wait found nothing and parked again.
    pub useless: AtomicU64,
    /// Useless wakes that could not be pinned on registrations.
    pub overflow: AtomicU64,
    wfd: [AtomicU32; 8],
    wcnt: [AtomicU64; 8],
}

impl EpStat {
    /// Pseudo fd: the wake carried no registration list (full re-probe).
    pub const ALL_FD: u32 = 0xffff_fffe;

    const fn new() -> Self {
        Self { id: AtomicU64::new(0), tgid: AtomicU32::new(0), epfd: AtomicU32::new(0),
               parks: AtomicU64::new(0), useful: AtomicU64::new(0), useless: AtomicU64::new(0),
               overflow: AtomicU64::new(0),
               wfd: [const { AtomicU32::new(0) }; 8], wcnt: [const { AtomicU64::new(0) }; 8] }
    }

    /// Count one useless wake against registered fd `fd`.
    #[inline]
    pub fn note_fd(&self, fd: u32) {
        let key = fd.wrapping_add(1);
        for i in 0..8 {
            let mut k = self.wfd[i].load(Relaxed);
            if k == 0 {
                k = match self.wfd[i].compare_exchange(0, key, Relaxed, Relaxed) {
                    Ok(_) => key, Err(o) => o,
                };
            }
            if k == key { self.wcnt[i].fetch_add(1, Relaxed); return; }
        }
        self.overflow.fetch_add(1, Relaxed);
    }

    /// (fd, useless wakes) pairs recorded for this instance.
    pub fn top(&self) -> [(u32, u64); 8] {
        let mut o = [(0u32, 0u64); 8];
        for i in 0..8 {
            let k = self.wfd[i].load(Relaxed);
            if k != 0 { o[i] = (k.wrapping_sub(1), self.wcnt[i].load(Relaxed)); }
        }
        o
    }

    fn reset(&self, id: u64, tgid: u32, epfd: u32) {
        self.parks.store(0, Relaxed); self.useful.store(0, Relaxed);
        self.useless.store(0, Relaxed); self.overflow.store(0, Relaxed);
        for i in 0..8 { self.wfd[i].store(0, Relaxed); self.wcnt[i].store(0, Relaxed); }
        self.tgid.store(tgid, Relaxed); self.epfd.store(epfd, Relaxed);
        self.id.store(id, Relaxed);
    }
}

const EP_STAT_ROWS: usize = 256;
static EP_STATS: [EpStat; EP_STAT_ROWS] = [const { EpStat::new() }; EP_STAT_ROWS];

/// The stats row of epoll instance `id`, claimed (and reset) if it held another.
#[inline]
pub fn ep_stat(id: u64, tgid: u32, epfd: u32) -> &'static EpStat {
    let r = &EP_STATS[(id as usize) & (EP_STAT_ROWS - 1)];
    if r.id.load(Relaxed) != id { r.reset(id, tgid, epfd); }
    else if r.epfd.load(Relaxed) != epfd { r.epfd.store(epfd, Relaxed); }
    r
}

/// Visit every claimed row.
pub fn ep_stats_each(mut f: impl FnMut(&EpStat)) {
    for r in EP_STATS.iter() { if r.id.load(Relaxed) != 0 { f(r); } }
}
