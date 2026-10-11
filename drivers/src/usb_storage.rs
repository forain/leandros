//! USB mass storage as block devices: the glue between the xHCI driver's
//! bulk-only transports (`usb_hcd::mass_storage_devices`), the BOT/SCSI
//! class driver (`usb::msc::ScsiDisk`) and the block registry
//! (`blkdev::register`).
//!
//! [`init`] takes every bulk-only interface the xHCI driver configured, brings
//! the SCSI unit up (TEST UNIT READY / READ CAPACITY / MODE SENSE), and
//! registers it as `sdX`; the registry then scans its partition table, so a
//! stick shows up as `sda`, `sda1`, `sda2`… and `/dev/root` can resolve to it.
//!
//! [`init`] must run in task context after `blkdev::init()` and
//! `usb_hcd::init()`: `ScsiDisk::init` sleeps between TEST UNIT READY attempts
//! (up to ~10 s per LUN that never becomes ready).
//!
//! I/O runs in syscall/fault context (f2fs, the VFS block layer) with IRQs
//! masked, or in a kernel thread (the klog flusher). Two cases (lane fsio):
//!   * the caller may sleep (`sched::irqctx::can_sleep`: it holds only
//!     sleeping locks such as `F2FS_MOUNTS`): waiters for the disk park, and
//!     the xHCI poll hook (`usb_hcd::relax`) spins for a short budget and then
//!     parks between polls, so a long command (hundreds of milliseconds for a
//!     small write on a cheap stick) does not keep a CPU busy;
//!   * it may not (it holds a spinlock, a tracked lock, or has preemption
//!     off): the request runs inside an [`IoWindow`] (preemption disabled) and
//!     the poll hook spins with periodic `irq_window`s instead of yielding —
//!     the tick, tick hooks and poll deadlines stay alive, no lock is parked.
//!     This is the old contract (virtio-blk's spin wait, virtio-gpu's
//!     `SpinWindow`); yielding there deadlocked the first USB-root boot.
//!
//! Locking: `BlockDevice` methods take `&self` and may be called from several
//! CPUs; `ScsiDisk` needs `&mut`, so each disk sits behind its own sleeping
//! `KMutex`, held for one request's command sequence only.
//!
//! Reads first (lane nostall): a write or flush that may sleep does not queue
//! for the disk while a read is waiting for it; it parks (up to
//! `WRITE_YIELD_MAX_NS` per request) until the waiting reads have the disk.
//! A cheap stick needs 0.5-1.3 s per small write command (nvrm-boot13) and a
//! millisecond per read, and the reads are what somebody is waiting on — a
//! page fault, a path lookup — while the writes are background write-back
//! (f2fs checkpoints, the klog flusher) issued one command per request. So a
//! read waits for at most the one write command already on the wire, not for
//! every write queued behind it. Writes still go out in order among
//! themselves, and a read never overtakes a write the reader issued (both are
//! synchronous).
//!
//! Errors: every class-driver failure becomes `BlkErr::Io` (after the class
//! driver's own retry ladder, reset recovery included), bad arguments become
//! `BadBuffer`/`OutOfRange`. Each USB transfer has a deadline (5 s, 30 s for
//! SYNCHRONIZE CACHE), so a dead or unplugged stick yields EIO, never a hang.
//!
//! Diagnostics: [`stats_report`] prints one `[usbms]` line per busy disk
//! (command counts, bytes, per-command latency avg/max and histogram, device
//! busy %, disk-lock waits split by reads and writes), plus the three tasks
//! that spent the longest blocked on a disk lock, every 5 s from the kernel's
//! I/O statistics thread. A single wait of `IOWAIT_REPORT_NS` (300 ms) or more
//! prints an `[IOWAIT]` line (rate-limited) naming the waiter, what was on
//! the wire when it started waiting and how many writes and reads went first.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use sched::ksync::{KMutex, KMutexGuard, KSpinLock, LockClass};
use usb::msc::{MscErr, ScsiDisk};

use crate::blkdev::{self, BlkErr, BlockDevice};
use crate::pci::{serial_debug, serial_debug_dec as log_dec};
use crate::usb_hcd::{BulkTransport, XhciBulk};

/// Sleep for `ms` milliseconds without burning the CPU: park on the
/// scheduler's timed wait channel until the deadline (the same mechanism
/// `nanosleep` uses), re-checking the clock after every wake.
fn delay_ms(ms: u32) {
    sleep_until(sched::monotonic_ns().saturating_add(ms as u64 * 1_000_000));
}

/// Park until `deadline` (`monotonic_ns`). Task context that may sleep only.
pub fn sleep_until(deadline: u64) {
    while sched::monotonic_ns() < deadline {
        sched::block_on_poll_prepare_masked(deadline, sched::POLL_MASK_TIMED_ONLY);
        if sched::monotonic_ns() >= deadline {
            sched::block_on_poll_cancel();
        } else {
            sched::block_on_poll_commit();
        }
    }
}

// ── Emulated device latency (QEMU benchmarking) ────────────────────────────
//
// QEMU's usb-storage answers a command in microseconds; the PC's stick
// (13fe:4300) needs on the order of a millisecond per read command and,
// judging by the ~575 ms quantum of the lock waits in nvrm-boot7.log, far
// longer for a small write. `usbms.delay_us=N` adds N us to every read
// command and `usbms.wdelay_us=N` to every write command (both 0 by
// default), with the disk held, as a slow device holds the bus. Kernel
// command line, read once at init (`set_cmdline`).
static RDELAY_US: AtomicU64 = AtomicU64::new(0);
static WDELAY_US: AtomicU64 = AtomicU64::new(0);

// Allocation-unit merge model (lane readfirst). nvrm-boot16's writes are
// bimodal: ~4500 under 16 ms, ~790 at 0.25-1.1 s, 152 at >= 1 s with a
// quantum of ~1.07 s — what a cheap stick's controller does when a write
// lands outside the few allocation units it keeps open and it has to merge
// (copy) one. `usbms.au_kib=K usbms.au_open=N usbms.merge_us=M`: the disk is
// cut into K KiB units; a write to one of the N most recently written units
// costs `usbms.au_hit_us` (default 4000 us), any other one costs M us and
// becomes the most recent unit. Off unless `au_kib` and `merge_us` are set.
// E.g. `usbms.delay_us=1500 usbms.au_kib=4096 usbms.au_open=2
// usbms.merge_us=1000000` reproduces the PC's pattern in QEMU.
static AU_KIB: AtomicU64 = AtomicU64::new(0);
static AU_OPEN: AtomicU64 = AtomicU64::new(2);
static AU_MERGE_US: AtomicU64 = AtomicU64::new(0);
static AU_HIT_US: AtomicU64 = AtomicU64::new(4000);
/// Most open units the model tracks.
const AU_MAX_OPEN: usize = 16;

/// Parse `usbms.*` options from the kernel command line. Call before `init`.
pub fn set_cmdline(cmdline: &str) {
    for tok in cmdline.split_ascii_whitespace() {
        if let Some(v) = tok.strip_prefix("usbms.delay_us=") {
            if let Ok(n) = v.parse::<u64>() { RDELAY_US.store(n, Ordering::Relaxed); }
        } else if let Some(v) = tok.strip_prefix("usbms.wdelay_us=") {
            if let Ok(n) = v.parse::<u64>() { WDELAY_US.store(n, Ordering::Relaxed); }
        } else if let Some(v) = tok.strip_prefix("usbms.au_kib=") {
            if let Ok(n) = v.parse::<u64>() { AU_KIB.store(n, Ordering::Relaxed); }
        } else if let Some(v) = tok.strip_prefix("usbms.au_open=") {
            if let Ok(n) = v.parse::<u64>() { AU_OPEN.store(n.clamp(1, AU_MAX_OPEN as u64), Ordering::Relaxed); }
        } else if let Some(v) = tok.strip_prefix("usbms.merge_us=") {
            if let Ok(n) = v.parse::<u64>() { AU_MERGE_US.store(n, Ordering::Relaxed); }
        } else if let Some(v) = tok.strip_prefix("usbms.au_hit_us=") {
            if let Ok(n) = v.parse::<u64>() { AU_HIT_US.store(n, Ordering::Relaxed); }
        }
    }
}

/// Wait `us` microseconds: parked when the caller may sleep, else a spin that
/// keeps the tick alive (the caller is inside an [`IoWindow`]).
fn emulate_latency(us: u64, sleepable: bool) {
    if us == 0 { return; }
    let deadline = sched::monotonic_ns().saturating_add(us * 1000);
    // Same wait policy as a real command's poll loop (`usb_hcd::relax`):
    // spin for the first 300 us, then (if allowed) park for 1/8 of the time
    // waited so far, so the emulation charges what a real device would.
    let t0 = sched::monotonic_ns();
    let mut spins = 0u32;
    loop {
        let now = sched::monotonic_ns();
        if now >= deadline { return; }
        let waited = now - t0;
        if sleepable && waited > 300_000 {
            sleep_until(deadline.min(now + (waited / 8).clamp(100_000, 2_000_000)));
            continue;
        }
        spins = spins.wrapping_add(1);
        if spins % 256 == 0 && sched::preempt_disabled() { sched::irq_window(); }
        core::hint::spin_loop();
    }
}

// ── Per-disk command statistics (`[usbms]` summary line) ───────────────────

/// Latency histogram bucket upper bounds, in microseconds (last = rest).
const HIST_US: [u64; 7] = [1_000, 4_000, 16_000, 64_000, 256_000, 1_024_000, u64::MAX];
const HIST_NAMES: [&str; 7] = ["<1ms", "<4ms", "<16ms", "<64ms", "<256ms", "<1s", ">=1s"];

struct DirStats {
    cmds: AtomicU64,
    bytes: AtomicU64,
    ns: AtomicU64,
    max_ns: AtomicU64,
    hist: [AtomicU32; 7],
}

impl DirStats {
    const fn new() -> Self {
        Self { cmds: AtomicU64::new(0), bytes: AtomicU64::new(0), ns: AtomicU64::new(0),
               max_ns: AtomicU64::new(0), hist: [const { AtomicU32::new(0) }; 7] }
    }
    /// One request of `bytes` that took `ns` and was issued as `cmds` SCSI
    /// commands; each command is charged `ns / cmds`.
    fn record(&self, cmds: u64, bytes: u64, ns: u64) {
        let cmds = cmds.max(1);
        let per = ns / cmds;
        self.cmds.fetch_add(cmds, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        self.ns.fetch_add(ns, Ordering::Relaxed);
        self.max_ns.fetch_max(per, Ordering::Relaxed);
        let us = per / 1000;
        let b = HIST_US.iter().position(|&lim| us < lim).unwrap_or(HIST_US.len() - 1);
        self.hist[b].fetch_add(cmds as u32, Ordering::Relaxed);
    }
    /// (cmds, bytes, ns, max per-command ns, histogram) since the last take.
    fn take(&self) -> (u64, u64, u64, u64, [u32; 7]) {
        let mut h = [0u32; 7];
        for (i, c) in self.hist.iter().enumerate() { h[i] = c.swap(0, Ordering::Relaxed); }
        (self.cmds.swap(0, Ordering::Relaxed), self.bytes.swap(0, Ordering::Relaxed),
         self.ns.swap(0, Ordering::Relaxed), self.max_ns.swap(0, Ordering::Relaxed), h)
    }
}

struct UsbDisk {
    /// Sleeping: a request holds it for one command sequence, milliseconds for
    /// a read on a USB 2.0 stick and possibly hundreds of milliseconds for a
    /// small write on a cheap one. Waiters park instead of spinning.
    disk: KMutex<ScsiDisk<XhciBulk>>,
    sector_size: u32,
    sectors: u64,
    read_only: bool,
    /// One "write refused" log line per disk, not one per block.
    warned_ro: AtomicBool,
    index: usize,
    rd: DirStats,
    wr: DirStats,
    /// Waits for `disk` since the last summary, by direction (flushes count
    /// as writes): number of contended acquisitions, total and longest (ns).
    rd_wait: WaitStats,
    wr_wait: WaitStats,
    /// Most write commands a read waited behind (the one on the wire
    /// included) since the last summary: 1 is the design goal.
    rd_wait_wr_ahead_max: AtomicU32,
    /// Waits >= `IOWAIT_REPORT_NS` since the last summary, and how many of
    /// them were not printed (rate limit).
    iowaits: AtomicU32,
    iowaits_quiet: AtomicU32,
    iowait_next_ns: AtomicU64,
    /// Lock holds completed, by direction (write = write command or flush):
    /// running counters, a waiter takes the difference.
    rd_done: AtomicU64,
    wr_done: AtomicU64,
    /// Reads waiting for `disk` right now (a long write hands off to them).
    readers_waiting: AtomicU32,
    /// Reads that waited for an overlapping write first, write hand-offs
    /// to waiting reads, requests that ran in a context that could not sleep,
    /// since the last summary.
    overlap_waits: AtomicU32,
    handoffs: AtomicU32,
    nosleep: AtomicU32,
    /// Write ranges registered but not finished (reads must not overtake).
    pending: KSpinLock<Pending>,
    /// AU model: most recently written units first (`u64::MAX` = empty),
    /// touched only with `disk` held; merges since the last summary.
    au_lru: [AtomicU64; AU_MAX_OPEN],
    au_merges: AtomicU32,
    /// The request in flight (hung-task reports, lane loginhang): op
    /// (`OP_*`, 0 = idle), start time, LBA, length, issuing pid.
    inflight_op: AtomicU8,
    inflight_t0: AtomicU64,
    inflight_lba: AtomicU64,
    inflight_bytes: AtomicU32,
    inflight_pid: AtomicU32,
}

/// `usbms.wryield=0` on the kernel command line turns reads-first off (one
/// FIFO queue for the disk, whole requests, as before lane nostall): a
/// one-boot A/B on the PC (lane fwintegrity). Read once.
fn reads_first_enabled() -> bool {
    static STATE: AtomicU8 = AtomicU8::new(0); // 0 unknown, 1 on, 2 off
    match STATE.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = crate::nvidia_probe::cmdline_opt("usbms.wryield").map_or(true, |v| v != 0);
            STATE.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

/// Most reads that may overtake one queued write (`LockClass::Prio`):
/// a few milliseconds each, so a write is held back for ~0.1-0.3 s at most.
const READ_BYPASS_MAX: u16 = 64;

/// A disk-lock wait this long prints an `[IOWAIT]` line.
const IOWAIT_REPORT_NS: u64 = 300_000_000;
/// At most one `[IOWAIT]` line per disk per this long (each costs ~15 ms of
/// UART on the PC); the others are counted in the summary.
const IOWAIT_REPORT_GAP_NS: u64 = 1_000_000_000;

/// Contended lock acquisitions of one direction.
struct WaitStats { n: AtomicU32, ns: AtomicU64, max_ns: AtomicU64 }
impl WaitStats {
    const fn new() -> Self { Self { n: AtomicU32::new(0), ns: AtomicU64::new(0), max_ns: AtomicU64::new(0) } }
    fn record(&self, ns: u64) {
        self.n.fetch_add(1, Ordering::Relaxed);
        self.ns.fetch_add(ns, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
    }
    /// (count, avg ms, max ms) since the last take.
    fn take(&self) -> (u32, u64, u64) {
        let n = self.n.swap(0, Ordering::Relaxed);
        let ns = self.ns.swap(0, Ordering::Relaxed);
        let max = self.max_ns.swap(0, Ordering::Relaxed);
        (n, ns / (n.max(1) as u64) / 1_000_000, max / 1_000_000)
    }
}

/// Write requests in progress: `[start, end)` LBA ranges. More than
/// `PENDING_SLOTS` at once count in `overflow`, which makes every read wait
/// as if it overlapped (correct, just conservative).
const PENDING_SLOTS: usize = 16;
struct Pending { start: [u64; PENDING_SLOTS], end: [u64; PENDING_SLOTS], used: u32, overflow: u32 }
impl Pending {
    const fn new() -> Self { Self { start: [0; PENDING_SLOTS], end: [0; PENDING_SLOTS], used: 0, overflow: 0 } }
    fn overlaps(&self, start: u64, end: u64) -> bool {
        if self.overflow != 0 { return true; }
        (0..PENDING_SLOTS).any(|i| self.used & (1 << i) != 0 && self.start[i] < end && start < self.end[i])
    }
}

/// What a long disk-lock wait saw, printed after the request (never with
/// the disk held: the UART is slow).
struct WaitNote {
    op: u8,
    lba: u64,
    bytes: usize,
    waited_ns: u64,
    wr_ahead: u64,
    rd_ahead: u64,
    /// The request on the wire when the wait began (op 0 = none).
    in_op: u8,
    in_lba: u64,
    in_bytes: u32,
    in_pid: u32,
    in_age_ns: u64,
}

/// ` (exe)` of `pid`, or ` (kernel)` for a task without an executable.
fn push_comm(s: &mut alloc::string::String, pid: u32) {
    let mut exe = [0u8; 64];
    let name = sched::task_brief(pid)
        .and_then(|b| sched::exe_path_try(b.tgid, &mut exe))
        .map(|n| core::str::from_utf8(&exe[..n]).unwrap_or("?"));
    let _ = core::fmt::Write::write_fmt(s, format_args!(" ({})", name.unwrap_or("kernel")));
}

fn op_name(op: u8) -> &'static str {
    match op { OP_READ => "READ", OP_WRITE => "WRITE", OP_FLUSH => "SYNCHRONIZE CACHE", _ => "none" }
}

const OP_READ: u8 = 1;
const OP_WRITE: u8 = 2;
const OP_FLUSH: u8 = 3;

/// One in-flight USB disk request, for the hung-task detector.
pub struct Inflight {
    pub disk: usize,
    pub op: &'static str,
    pub lba: u64,
    pub bytes: u32,
    pub age_ns: u64,
    pub pid: u32,
}

/// Every USB disk request in flight right now (lock-free snapshot).
pub fn inflight(now: u64) -> Vec<Inflight> {
    let mut v = Vec::new();
    let disks: Vec<&'static UsbDisk> = match DISKS.try_lock() { Some(d) => d.clone(), None => return v };
    for d in disks {
        let op = d.inflight_op.load(Ordering::Acquire);
        if op == 0 { continue; }
        v.push(Inflight {
            disk: d.index,
            op: op_name(op),
            lba: d.inflight_lba.load(Ordering::Relaxed),
            bytes: d.inflight_bytes.load(Ordering::Relaxed),
            age_ns: now.saturating_sub(d.inflight_t0.load(Ordering::Relaxed)),
            pid: d.inflight_pid.load(Ordering::Relaxed),
        });
    }
    v
}

/// `(disk, owner pid, waiters)` of every USB disk lock that is held.
pub fn lock_owners() -> Vec<(usize, u32, usize)> {
    let mut v = Vec::new();
    let disks: Vec<&'static UsbDisk> = match DISKS.try_lock() { Some(d) => d.clone(), None => return v };
    for d in disks {
        let o = d.disk.owner_raw();
        if o != 0 { v.push((d.index, o - 1, d.disk.waiters_snapshot().0)); }
    }
    v
}

// ── Per-task disk-lock wait (lane nostall) ─────────────────────────────────
//
// How long each task has waited for a USB disk lock in total, so a caller
// that holds a lock of its own across a block request (f2fs's `[F2FS] long
// hold` report) can tell "the device was slow" from "the request queued
// behind somebody else's command". Hashed by pid into a small table: a
// collision resets the slot, which only costs a report one sample.
const TASK_WAIT_SLOTS: usize = 64;
static TASK_WAIT_PID: [AtomicU32; TASK_WAIT_SLOTS] = [const { AtomicU32::new(0) }; TASK_WAIT_SLOTS];
static TASK_WAIT_NS: [AtomicU64; TASK_WAIT_SLOTS] = [const { AtomicU64::new(0) }; TASK_WAIT_SLOTS];

fn note_task_wait(ns: u64) {
    let pid = sched::current_pid();
    if pid == 0 { return; }
    let i = pid as usize % TASK_WAIT_SLOTS;
    if TASK_WAIT_PID[i].swap(pid, Ordering::Relaxed) != pid {
        TASK_WAIT_NS[i].store(0, Ordering::Relaxed);
    }
    TASK_WAIT_NS[i].fetch_add(ns, Ordering::Relaxed);
}

/// Total time task `pid` has waited for USB disk locks (a running counter:
/// callers take the difference of two readings).
pub fn task_disk_wait_ns(pid: u32) -> u64 {
    let i = pid as usize % TASK_WAIT_SLOTS;
    if pid == 0 || TASK_WAIT_PID[i].load(Ordering::Relaxed) != pid { return 0; }
    TASK_WAIT_NS[i].load(Ordering::Relaxed)
}

/// Every registered disk, for the periodic summary.
static DISKS: spin::Mutex<Vec<&'static UsbDisk>> = spin::Mutex::new(Vec::new());

/// Preemption disabled for one block request when it runs with IRQs masked
/// in a context that may NOT sleep, so the `irq_window`s the poll loop opens
/// take the tick but never switch tasks. Released without a resched on every
/// exit; the deferred switch happens at the next tick or syscall return. A
/// caller that may sleep gets no window (see the module comment). At boot
/// (init task, IRQs on) it is a no-op and polls may yield.
struct IoWindow { open: bool, sleepable: bool }
impl IoWindow {
    fn new() -> Self {
        let masked = sched::irqs_masked() && sched::current_pid() != 0;
        let sleepable = masked && sched::irqctx::can_sleep();
        let open = masked && !sleepable;
        if open { sched::preempt_disable(); }
        IoWindow { open, sleepable }
    }
}
impl Drop for IoWindow {
    fn drop(&mut self) {
        if self.open { sched::preempt_enable_no_resched(); }
    }
}

impl UsbDisk {
    /// Take the disk for one `op` (`OP_*`), queueing as `class` says (see
    /// the module comment); parks when the caller may sleep, otherwise spins
    /// on its queue entry. Taken BEFORE the [`IoWindow`]: a waiter with
    /// preemption disabled could not park. A wait of `IOWAIT_REPORT_NS` or
    /// more returns a note for [`Self::report_wait`], which the caller
    /// prints after dropping the disk.
    fn lock(&self, op: u8, lba: u64, bytes: usize, class: LockClass)
        -> (KMutexGuard<'_, ScsiDisk<XhciBulk>>, Option<WaitNote>)
    {
        if let Some(g) = self.disk.try_lock() { return (g, None); }
        let t0 = sched::monotonic_ns();
        let in_op = self.inflight_op.load(Ordering::Acquire);
        let (in_lba, in_bytes, in_pid, in_t0) = (
            self.inflight_lba.load(Ordering::Relaxed), self.inflight_bytes.load(Ordering::Relaxed),
            self.inflight_pid.load(Ordering::Relaxed), self.inflight_t0.load(Ordering::Relaxed));
        let rd0 = self.rd_done.load(Ordering::Relaxed);
        let wr0 = self.wr_done.load(Ordering::Relaxed);
        let g = self.disk.lock_class(class);
        let waited = sched::monotonic_ns().saturating_sub(t0);
        let wr_ahead = self.wr_done.load(Ordering::Relaxed).wrapping_sub(wr0);
        let rd_ahead = self.rd_done.load(Ordering::Relaxed).wrapping_sub(rd0);
        if op == OP_READ {
            self.rd_wait.record(waited);
            self.rd_wait_wr_ahead_max.fetch_max(wr_ahead.min(u32::MAX as u64) as u32, Ordering::Relaxed);
        } else {
            self.wr_wait.record(waited);
        }
        note_task_wait(waited);
        let note = if waited >= IOWAIT_REPORT_NS {
            Some(WaitNote { op, lba, bytes, waited_ns: waited, wr_ahead, rd_ahead, in_op, in_lba, in_bytes,
                            in_pid, in_age_ns: if in_op == 0 { 0 } else { t0.saturating_sub(in_t0) } })
        } else { None };
        (g, note)
    }

    /// Print a long wait (disk NOT held), at most one line per
    /// `IOWAIT_REPORT_GAP_NS`; the rest only count.
    fn report_wait(&self, note: Option<WaitNote>) {
        let n = match note { Some(n) => n, None => return };
        self.iowaits.fetch_add(1, Ordering::Relaxed);
        let now = sched::monotonic_ns();
        let next = self.iowait_next_ns.load(Ordering::Relaxed);
        if now < next || self.iowait_next_ns
            .compare_exchange(next, now + IOWAIT_REPORT_GAP_NS, Ordering::Relaxed, Ordering::Relaxed).is_err()
        {
            self.iowaits_quiet.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let pid = sched::current_pid();
        let mut s = alloc::string::String::new();
        let _ = core::fmt::Write::write_fmt(&mut s, format_args!("[IOWAIT] disk{} pid {}", self.index, pid));
        push_comm(&mut s, pid);
        let _ = core::fmt::Write::write_fmt(&mut s, format_args!(
            " {} lba {} {} KiB waited {} ms behind {} wr + {} rd; on the wire then: {}",
            if n.op == OP_READ { 'R' } else if n.op == OP_FLUSH { 'F' } else { 'W' },
            n.lba, n.bytes / 1024, n.waited_ns / 1_000_000, n.wr_ahead, n.rd_ahead, op_name(n.in_op)));
        if n.in_op != 0 {
            let _ = core::fmt::Write::write_fmt(&mut s, format_args!(
                " lba {} {} KiB age {} ms pid {}", n.in_lba, n.in_bytes / 1024, n.in_age_ns / 1_000_000, n.in_pid));
            push_comm(&mut s, n.in_pid);
        }
        s.push('\n');
        serial_debug(&s);
    }

    /// Class a read queues in: ahead of queued writes unless reads-first is
    /// off.
    fn read_class() -> LockClass { LockClass::Fifo }

    /// Before a write or flush takes the disk: let the reads already waiting
    /// for it go first (see the module comment). Only a caller that may sleep
    /// yields; bounded so a stream of reads cannot starve write-back.
    fn yield_to_readers(&self) {
        if self.readers_waiting.load(Ordering::Acquire) == 0 { return; }
        if !reads_first_enabled() { return; }
        if !sched::irqctx::can_sleep() || sched::current_pid() == 0 { return; }
        let t0 = sched::monotonic_ns();
        let limit = t0.saturating_add(50_000_000);
        while self.readers_waiting.load(Ordering::Acquire) != 0 {
            let now = sched::monotonic_ns();
            if now >= limit { break; }
            sleep_until(limit.min(now + 200_000));
        }
        self.handoffs.fetch_add(1, Ordering::Relaxed);
    }

    /// Register a write of `[lba, lba + n)` before it queues; returns the
    /// slot for [`Self::pending_del`] (`None`: counted in `overflow`).
    fn pending_add(&self, lba: u64, n: u64) -> Option<usize> {
        let mut p = self.pending.lock();
        let free = (!p.used).trailing_zeros() as usize;
        if free < PENDING_SLOTS {
            p.used |= 1 << free;
            p.start[free] = lba;
            p.end[free] = lba.saturating_add(n);
            Some(free)
        } else {
            p.overflow += 1;
            None
        }
    }

    fn pending_del(&self, slot: Option<usize>) {
        let mut p = self.pending.lock();
        match slot {
            Some(i) => p.used &= !(1 << i),
            None => p.overflow = p.overflow.saturating_sub(1),
        }
    }

    /// Before a read queues: wait until no registered write overlaps
    /// `[lba, lba + n)`, so the read cannot overtake it (see the module
    /// comment). Parks when the caller may sleep, else spins with interrupt
    /// windows.
    fn wait_overlapping_writes(&self, lba: u64, n: u64) {
        let end = lba.saturating_add(n);
        if !self.pending.lock().overlaps(lba, end) { return; }
        self.overlap_waits.fetch_add(1, Ordering::Relaxed);
        let sleepable = sched::irqctx::can_sleep() && sched::current_pid() != 0;
        let mut spins = 0u32;
        while self.pending.lock().overlaps(lba, end) {
            if sleepable {
                sleep_until(sched::monotonic_ns() + 200_000);
            } else {
                spins = spins.wrapping_add(1);
                if spins % 256 == 0 && sched::irqs_masked() {
                    sched::preempt_disable();
                    sched::irq_window();
                    sched::preempt_enable_no_resched();
                }
                core::hint::spin_loop();
            }
        }
    }

    /// Emulated cost of a write of `n` sectors at `lba` under the AU model
    /// (us; 0 when the model is off). Disk held.
    fn au_cost_us(&self, lba: u64, n: u64) -> u64 {
        let au_kib = AU_KIB.load(Ordering::Relaxed);
        let merge = AU_MERGE_US.load(Ordering::Relaxed);
        if au_kib == 0 || merge == 0 || n == 0 { return 0; }
        let open = (AU_OPEN.load(Ordering::Relaxed) as usize).clamp(1, AU_MAX_OPEN);
        let au_bytes = au_kib * 1024;
        let ss = self.sector_size as u64;
        let first = lba * ss / au_bytes;
        let last = ((lba + n) * ss - 1) / au_bytes;
        let mut cost = 0;
        let lru = &self.au_lru[..open];
        for au in first..=last {
            let hit = lru.iter().position(|e| e.load(Ordering::Relaxed) == au);
            // Move to the front (shift the more recent ones down by one).
            let from = match hit {
                Some(i) => { cost += AU_HIT_US.load(Ordering::Relaxed); i }
                None => { cost += merge; self.au_merges.fetch_add(1, Ordering::Relaxed); open - 1 }
            };
            for k in (1..=from).rev() {
                lru[k].store(lru[k - 1].load(Ordering::Relaxed), Ordering::Relaxed);
            }
            lru[0].store(au, Ordering::Relaxed);
        }
        cost
    }

    fn begin(&self, op: u8, lba: u64, bytes: usize) {
        self.inflight_lba.store(lba, Ordering::Relaxed);
        self.inflight_bytes.store(bytes as u32, Ordering::Relaxed);
        self.inflight_pid.store(sched::current_pid(), Ordering::Relaxed);
        self.inflight_t0.store(sched::monotonic_ns(), Ordering::Relaxed);
        self.inflight_op.store(op, Ordering::Release);
    }
    fn end(&self) {
        let op = self.inflight_op.swap(0, Ordering::Release);
        if op == OP_READ { self.rd_done.fetch_add(1, Ordering::Relaxed); } else { self.wr_done.fetch_add(1, Ordering::Relaxed); }
    }

    fn cmds_for(&self, len: usize) -> u64 {
        len.div_ceil(usb::msc::scsi::MAX_CHUNK_BYTES).max(1) as u64
    }
}

fn map_err(e: MscErr) -> BlkErr {
    match e {
        MscErr::InvalidArg => BlkErr::OutOfRange,
        _ => BlkErr::Io,
    }
}

fn log_err(what: &str, e: MscErr) {
    let mut s = alloc::string::String::new();
    let _ = core::fmt::Write::write_fmt(&mut s, format_args!("[USB-MSC] {}: {:?}\n", what, e));
    serial_debug(&s);
}

impl BlockDevice for UsbDisk {
    fn sector_size(&self) -> u32 { self.sector_size }
    fn capacity_sectors(&self) -> u64 { self.sectors }

    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkErr> {
        if buf.is_empty() || buf.len() % self.sector_size as usize != 0 {
            return Err(BlkErr::BadBuffer);
        }
        // One call per request: ScsiDisk issues it as READ(10)/(16) commands
        // of up to `usb::msc::scsi::MAX_CHUNK_BYTES` each.
        let nsec = (buf.len() / self.sector_size as usize) as u64;
        let _ = nsec;
        self.readers_waiting.fetch_add(1, Ordering::AcqRel);
        let (mut g, note) = self.lock(OP_READ, lba, buf.len(), Self::read_class());
        self.readers_waiting.fetch_sub(1, Ordering::AcqRel);
        let w = IoWindow::new();
        if w.open { self.nosleep.fetch_add(1, Ordering::Relaxed); }
        let cmds = self.cmds_for(buf.len());
        let t0 = sched::monotonic_ns();
        self.begin(OP_READ, lba, buf.len());
        let r = g.read_sectors(lba, buf);
        emulate_latency(RDELAY_US.load(Ordering::Relaxed) * cmds, w.sleepable);
        self.end();
        self.rd.record(cmds, buf.len() as u64, sched::monotonic_ns().saturating_sub(t0));
        drop(w);
        drop(g);
        self.report_wait(note);
        r.map_err(|e| {
            log_err("read failed", e);
            map_err(e)
        })
    }

    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlkErr> {
        if buf.is_empty() || buf.len() % self.sector_size as usize != 0 {
            return Err(BlkErr::BadBuffer);
        }
        if self.read_only {
            if !self.warned_ro.swap(true, Ordering::Relaxed) {
                serial_debug("[USB-MSC] write refused: medium is write-protected\n");
            }
            return Err(BlkErr::Io);
        }
        let ss = self.sector_size as usize;
        let slot = self.pending_add(lba, (buf.len() / ss) as u64);
        // A sleepable writer goes one SCSI command at a time and lets the
        // reads queued meanwhile in between (module comment); a writer that
        // may not sleep keeps the disk for the whole request, as before.
        let split = false;
        self.yield_to_readers();
        let step = if split { (usb::msc::scsi::MAX_CHUNK_BYTES / ss).max(1) * ss } else { buf.len() };
        let (mut g, mut note) = self.lock(OP_WRITE, lba, buf.len(), LockClass::Fifo);
        let mut off = 0usize;
        let mut res = Ok(());
        loop {
            let end = (off + step).min(buf.len());
            let clba = lba + (off / ss) as u64;
            let w = IoWindow::new();
            if w.open { self.nosleep.fetch_add(1, Ordering::Relaxed); }
            let cmds = self.cmds_for(end - off);
            let t0 = sched::monotonic_ns();
            self.begin(OP_WRITE, clba, end - off);
            let r = g.write_sectors(clba, &buf[off..end]);
            let us = WDELAY_US.load(Ordering::Relaxed) * cmds + self.au_cost_us(clba, ((end - off) / ss) as u64);
            emulate_latency(us, w.sleepable);
            self.end();
            self.wr.record(cmds, (end - off) as u64, sched::monotonic_ns().saturating_sub(t0));
            drop(w);
            if let Err(e) = r { res = Err(e); break; }
            off = end;
            if off >= buf.len() { break; }
            if self.readers_waiting.load(Ordering::Acquire) != 0 {
                // Hand the disk to the waiting reads, then continue ahead of
                // the writes queued meanwhile.
                drop(g);
                self.report_wait(note.take());
                self.handoffs.fetch_add(1, Ordering::Relaxed);
                let (g2, n2) = self.lock(OP_WRITE, lba + (off / ss) as u64, buf.len() - off, LockClass::Resume);
                g = g2;
                note = n2;
            }
        }
        drop(g);
        self.pending_del(slot);
        self.report_wait(note);
        res.map_err(|e| {
            log_err("write failed", e);
            map_err(e)
        })
    }

    /// Panic path: take the disk only if it is free right now, never spin on
    /// it, and skip the IoWindow (the panicking CPU is not preemptible
    /// anyway). If a request was in flight on another CPU the controller
    /// state is unknowable, so the write is refused.
    fn try_write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlkErr> {
        if buf.is_empty() || buf.len() % self.sector_size as usize != 0 || self.read_only {
            return Err(BlkErr::BadBuffer);
        }
        match self.disk.try_lock() {
            Some(mut g) => g.write_sectors(lba, buf).map_err(map_err),
            None => Err(BlkErr::Io),
        }
    }

    fn flush(&self) -> Result<(), BlkErr> {
        if self.read_only { return Ok(()); }
        self.yield_to_readers();
        let (mut g, note) = self.lock(OP_FLUSH, 0, 0, LockClass::Fifo);
        let w = IoWindow::new();
        if w.open { self.nosleep.fetch_add(1, Ordering::Relaxed); }
        let was = g.sync_cache_unsupported();
        self.begin(OP_FLUSH, 0, 0);
        let r = g.flush();
        self.end();
        let r = r.map_err(|e| {
            log_err("flush failed", e);
            map_err(e)
        });
        let now_unsupported = !was && g.sync_cache_unsupported();
        drop(w);
        drop(g);
        self.report_wait(note);
        if now_unsupported {
            serial_debug("[USB-MSC] SYNCHRONIZE CACHE not usable on this device; flush is a no-op\n");
        }
        r
    }

    fn removable(&self) -> bool { true }
    fn name_class(&self) -> &'static str { "sd" }
}

/// Per-slot `TASK_WAIT_NS` at the previous summary (top waiters).
static TASK_WAIT_PREV_PID: [AtomicU32; TASK_WAIT_SLOTS] = [const { AtomicU32::new(0) }; TASK_WAIT_SLOTS];
static TASK_WAIT_PREV_NS: [AtomicU64; TASK_WAIT_SLOTS] = [const { AtomicU64::new(0) }; TASK_WAIT_SLOTS];
/// Top waiters are printed when the worst one waited at least this long.
const TOP_WAIT_REPORT_NS: u64 = 100_000_000;

/// Print one `[usbms]` line per disk that saw I/O since the previous call,
/// covering `window_ns`, and the tasks that waited longest for a disk lock.
/// Task context, no lock held by the caller (printing costs ~90 us per byte
/// on the PC's 115200-baud UART).
///
///   [usbms] disk0 5000 ms: rd 120 cmds 3712 KiB avg 0.84 ms max 3.10 ms |
///     wr 4 cmds 16 KiB avg 575.00 ms max 590.00 ms | busy 64% |
///     lockwait rd 12 avg 40 max 600 ms (behind <= 1 wr) wr 3 avg 2 max 5 ms |
///     iowait>300ms 1 (0 unprinted) | handoffs 2 | lat <1ms:100 <4ms:20 ...
///   [usbms] blocked on disk 5000 ms: pid 123 (/bin/x) 1200 ms, pid 7 (kernel) 300 ms
///
/// "busy" = command time / window: near 100% means the device is the
/// bottleneck. "behind <= N wr": the most write commands any read waited
/// for (1 = only the one already on the wire, the design goal).
pub fn stats_report(window_ns: u64) {
    let disks: Vec<&'static UsbDisk> = DISKS.lock().clone();
    for d in disks {
        let (rc, rb, rns, rmax, rh) = d.rd.take();
        let (wc, wb, wns, wmax, wh) = d.wr.take();
        let (rwn, rwa, rwm) = d.rd_wait.take();
        let (wwn, wwa, wwm) = d.wr_wait.take();
        let ahead = d.rd_wait_wr_ahead_max.swap(0, Ordering::Relaxed);
        let iow = d.iowaits.swap(0, Ordering::Relaxed);
        let iowq = d.iowaits_quiet.swap(0, Ordering::Relaxed);
        let handoffs = d.handoffs.swap(0, Ordering::Relaxed);
        let overlaps = d.overlap_waits.swap(0, Ordering::Relaxed);
        let nosleep = d.nosleep.swap(0, Ordering::Relaxed);
        let merges = d.au_merges.swap(0, Ordering::Relaxed);
        if rc == 0 && wc == 0 { continue; }
        let ms2 = |ns: u64| -> (u64, u64) { (ns / 1_000_000, (ns / 10_000) % 100) };
        let (ra, raf) = ms2(rns / rc.max(1));
        let (rm, rmf) = ms2(rmax);
        let (wa, waf) = ms2(wns / wc.max(1));
        let (wm, wmf) = ms2(wmax);
        let busy = ((rns + wns) * 100 / window_ns.max(1)).min(100);
        let mut s = alloc::string::String::new();
        let _ = core::fmt::Write::write_fmt(&mut s, format_args!(
            "[usbms] disk{} {} ms: rd {} cmds {} KiB avg {}.{:02} ms max {}.{:02} ms | wr {} cmds {} KiB avg {}.{:02} ms max {}.{:02} ms | busy {}% | lockwait rd {} avg {} max {} ms (behind <= {} wr) wr {} avg {} max {} ms",
            d.index, window_ns / 1_000_000, rc, rb / 1024, ra, raf, rm, rmf,
            wc, wb / 1024, wa, waf, wm, wmf, busy, rwn, rwa, rwm, ahead, wwn, wwa, wwm));
        if iow != 0 { let _ = core::fmt::Write::write_fmt(&mut s, format_args!(" | iowait>300ms {} ({} unprinted)", iow, iowq)); }
        if handoffs != 0 { let _ = core::fmt::Write::write_fmt(&mut s, format_args!(" | handoffs {}", handoffs)); }
        if overlaps != 0 { let _ = core::fmt::Write::write_fmt(&mut s, format_args!(" | overlap waits {}", overlaps)); }
        if nosleep != 0 { let _ = core::fmt::Write::write_fmt(&mut s, format_args!(" | nosleep {}", nosleep)); }
        if merges != 0 { let _ = core::fmt::Write::write_fmt(&mut s, format_args!(" | au merges {}", merges)); }
        s.push_str(" | lat");
        for i in 0..7 {
            let c = rh[i] + wh[i];
            if c != 0 {
                let _ = core::fmt::Write::write_fmt(&mut s, format_args!(" {}:{}", HIST_NAMES[i], c));
            }
        }
        s.push('\n');
        serial_debug(&s);
    }
    // Top 3 tasks by time blocked on a disk lock in this window.
    let mut top: [(u64, u32); 3] = [(0, 0); 3];
    for i in 0..TASK_WAIT_SLOTS {
        let pid = TASK_WAIT_PID[i].load(Ordering::Relaxed);
        let ns = TASK_WAIT_NS[i].load(Ordering::Relaxed);
        let prev = if TASK_WAIT_PREV_PID[i].swap(pid, Ordering::Relaxed) == pid {
            TASK_WAIT_PREV_NS[i].load(Ordering::Relaxed)
        } else { 0 };
        TASK_WAIT_PREV_NS[i].store(ns, Ordering::Relaxed);
        let delta = ns.saturating_sub(prev);
        if pid == 0 || delta == 0 { continue; }
        if let Some(k) = top.iter().position(|t| delta > t.0) {
            for m in (k + 1..3).rev() { top[m] = top[m - 1]; }
            top[k] = (delta, pid);
        }
    }
    if top[0].0 >= TOP_WAIT_REPORT_NS {
        let mut s = alloc::string::String::new();
        let _ = core::fmt::Write::write_fmt(&mut s, format_args!("[usbms] blocked on disk {} ms:", window_ns / 1_000_000));
        for (k, &(ns, pid)) in top.iter().enumerate() {
            if ns == 0 { break; }
            let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{} pid {}", if k == 0 { "" } else { "," }, pid));
            push_comm(&mut s, pid);
            let _ = core::fmt::Write::write_fmt(&mut s, format_args!(" {} ms", ns / 1_000_000));
        }
        s.push('\n');
        serial_debug(&s);
    }
}

/// Bring up every USB mass-storage interface found by `usb_hcd::init()` and
/// register it with the block layer. Returns the number of disks registered.
pub fn init() -> usize {
    let mut n = 0;
    for t in crate::usb_hcd::mass_storage_devices() {
        let iface = t.interface_number();
        let mut disk = ScsiDisk::new(t, delay_ms);
        if let Err(e) = disk.init() {
            log_err("no usable LUN", e);
            continue;
        }
        let (ss, sectors, ro, lun) = (disk.sector_size(), disk.capacity_sectors(), disk.read_only(), disk.lun());
        serial_debug("[USB-MSC] interface ");
        log_dec(iface as u64);
        serial_debug(" LUN ");
        log_dec(lun as u64);
        serial_debug(": ");
        log_dec(sectors);
        serial_debug(" x ");
        log_dec(ss as u64);
        serial_debug(if ro { "-byte sectors, write-protected\n" } else { "-byte sectors\n" });
        let index = DISKS.lock().len();
        let dev: &'static UsbDisk = Box::leak(Box::new(UsbDisk {
            disk: KMutex::new(disk),
            sector_size: ss,
            sectors,
            read_only: ro,
            warned_ro: AtomicBool::new(false),
            index,
            rd: DirStats::new(),
            wr: DirStats::new(),
            rd_wait: WaitStats::new(),
            wr_wait: WaitStats::new(),
            rd_wait_wr_ahead_max: AtomicU32::new(0),
            iowaits: AtomicU32::new(0),
            iowaits_quiet: AtomicU32::new(0),
            iowait_next_ns: AtomicU64::new(0),
            rd_done: AtomicU64::new(0),
            wr_done: AtomicU64::new(0),
            readers_waiting: AtomicU32::new(0),
            overlap_waits: AtomicU32::new(0),
            handoffs: AtomicU32::new(0),
            nosleep: AtomicU32::new(0),
            pending: KSpinLock::new(Pending::new()),
            au_lru: [const { AtomicU64::new(u64::MAX) }; AU_MAX_OPEN],
            au_merges: AtomicU32::new(0),
            inflight_op: AtomicU8::new(0),
            inflight_t0: AtomicU64::new(0),
            inflight_lba: AtomicU64::new(0),
            inflight_bytes: AtomicU32::new(0),
            inflight_pid: AtomicU32::new(0),
        }));
        DISKS.lock().push(dev);
        match blkdev::register(dev) {
            Ok(_) => n += 1,
            Err(e) => {
                let mut s = alloc::string::String::new();
                let _ = core::fmt::Write::write_fmt(&mut s, format_args!("[USB-MSC] block registration failed: {:?}\n", e));
                serial_debug(&s);
            }
        }
    }
    n
}
