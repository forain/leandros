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
//! Errors: every class-driver failure becomes `BlkErr::Io` (after the class
//! driver's own retry ladder, reset recovery included), bad arguments become
//! `BadBuffer`/`OutOfRange`. Each USB transfer has a deadline (5 s, 30 s for
//! SYNCHRONIZE CACHE), so a dead or unplugged stick yields EIO, never a hang.
//!
//! Diagnostics: [`stats_report`] prints one `[usbms]` line per busy disk
//! (command counts, bytes, per-command latency avg/max and histogram, device
//! busy %), called every 5 s by the kernel's I/O statistics thread.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use sched::ksync::{KMutex, KMutexGuard};
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
pub(crate) fn sleep_until(deadline: u64) {
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

/// Parse `usbms.*` options from the kernel command line. Call before `init`.
pub fn set_cmdline(cmdline: &str) {
    for tok in cmdline.split_ascii_whitespace() {
        if let Some(v) = tok.strip_prefix("usbms.delay_us=") {
            if let Ok(n) = v.parse::<u64>() { RDELAY_US.store(n, Ordering::Relaxed); }
        } else if let Some(v) = tok.strip_prefix("usbms.wdelay_us=") {
            if let Ok(n) = v.parse::<u64>() { WDELAY_US.store(n, Ordering::Relaxed); }
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
    /// Longest wait for `disk` since the last summary (ns).
    lock_wait_max_ns: AtomicU64,
    /// The request in flight (hung-task reports, lane loginhang): op
    /// (`OP_*`, 0 = idle), start time, LBA, length, issuing pid.
    inflight_op: AtomicU8,
    inflight_t0: AtomicU64,
    inflight_lba: AtomicU64,
    inflight_bytes: AtomicU32,
    inflight_pid: AtomicU32,
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
            op: match op { OP_READ => "READ", OP_WRITE => "WRITE", _ => "SYNCHRONIZE CACHE" },
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
    /// Take the disk: parks when the caller may sleep (`ksync` spins with
    /// interrupt windows otherwise). Taken BEFORE the [`IoWindow`]: a waiter
    /// with preemption disabled could not park.
    fn lock(&self) -> KMutexGuard<'_, ScsiDisk<XhciBulk>> {
        if let Some(g) = self.disk.try_lock() { return g; }
        let t0 = sched::monotonic_ns();
        let g = self.disk.lock();
        let waited = sched::monotonic_ns().saturating_sub(t0);
        self.lock_wait_max_ns.fetch_max(waited, Ordering::Relaxed);
        note_task_wait(waited);
        g
    }

    fn begin(&self, op: u8, lba: u64, bytes: usize) {
        self.inflight_lba.store(lba, Ordering::Relaxed);
        self.inflight_bytes.store(bytes as u32, Ordering::Relaxed);
        self.inflight_pid.store(sched::current_pid(), Ordering::Relaxed);
        self.inflight_t0.store(sched::monotonic_ns(), Ordering::Relaxed);
        self.inflight_op.store(op, Ordering::Release);
    }
    fn end(&self) { self.inflight_op.store(0, Ordering::Release); }

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
        let mut g = self.lock();
        let w = IoWindow::new();
        let cmds = self.cmds_for(buf.len());
        let t0 = sched::monotonic_ns();
        self.begin(OP_READ, lba, buf.len());
        let r = g.read_sectors(lba, buf);
        emulate_latency(RDELAY_US.load(Ordering::Relaxed) * cmds, w.sleepable);
        self.end();
        self.rd.record(cmds, buf.len() as u64, sched::monotonic_ns().saturating_sub(t0));
        drop(w);
        drop(g);
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
        let mut g = self.lock();
        let w = IoWindow::new();
        let cmds = self.cmds_for(buf.len());
        let t0 = sched::monotonic_ns();
        self.begin(OP_WRITE, lba, buf.len());
        let r = g.write_sectors(lba, buf);
        emulate_latency(WDELAY_US.load(Ordering::Relaxed) * cmds, w.sleepable);
        self.end();
        self.wr.record(cmds, buf.len() as u64, sched::monotonic_ns().saturating_sub(t0));
        drop(w);
        drop(g);
        r.map_err(|e| {
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
        let mut g = self.lock();
        let _w = IoWindow::new();
        let was = g.sync_cache_unsupported();
        self.begin(OP_FLUSH, 0, 0);
        let r = g.flush();
        self.end();
        let r = r.map_err(|e| {
            log_err("flush failed", e);
            map_err(e)
        });
        if !was && g.sync_cache_unsupported() {
            serial_debug("[USB-MSC] SYNCHRONIZE CACHE not usable on this device; flush is a no-op\n");
        }
        r
    }

    fn removable(&self) -> bool { true }
    fn name_class(&self) -> &'static str { "sd" }
}

/// Print one `[usbms]` line per disk that saw I/O since the previous call,
/// covering `window_ns`. Task context, no lock held by the caller (printing
/// costs ~90 us per byte on the PC's 115200-baud UART).
///
///   [usbms] disk0 5000 ms: rd 120 cmds 3712 KiB avg 0.84 ms max 3.10 ms |
///     wr 4 cmds 16 KiB avg 575.00 ms max 590.00 ms | busy 64% |
///     lockwait max 1200 ms | lat <1ms:100 <4ms:20 ... (per command, rd+wr)
///
/// "busy" = command time / window: near 100% means the device is the
/// bottleneck. A large write average with a busy device means writes (klog
/// flushes, f2fs checkpoints) are what everything else queues behind.
pub fn stats_report(window_ns: u64) {
    let disks: Vec<&'static UsbDisk> = DISKS.lock().clone();
    for d in disks {
        let (rc, rb, rns, rmax, rh) = d.rd.take();
        let (wc, wb, wns, wmax, wh) = d.wr.take();
        let lwmax = d.lock_wait_max_ns.swap(0, Ordering::Relaxed);
        if rc == 0 && wc == 0 { continue; }
        let ms2 = |ns: u64| -> (u64, u64) { (ns / 1_000_000, (ns / 10_000) % 100) };
        let (ra, raf) = ms2(rns / rc.max(1));
        let (rm, rmf) = ms2(rmax);
        let (wa, waf) = ms2(wns / wc.max(1));
        let (wm, wmf) = ms2(wmax);
        let busy = ((rns + wns) * 100 / window_ns.max(1)).min(100);
        let mut s = alloc::string::String::new();
        let _ = core::fmt::Write::write_fmt(&mut s, format_args!(
            "[usbms] disk{} {} ms: rd {} cmds {} KiB avg {}.{:02} ms max {}.{:02} ms | wr {} cmds {} KiB avg {}.{:02} ms max {}.{:02} ms | busy {}% | lockwait max {} ms | lat",
            d.index, window_ns / 1_000_000, rc, rb / 1024, ra, raf, rm, rmf,
            wc, wb / 1024, wa, waf, wm, wmf, busy, lwmax / 1_000_000));
        for i in 0..7 {
            let c = rh[i] + wh[i];
            if c != 0 {
                let _ = core::fmt::Write::write_fmt(&mut s, format_args!(" {}:{}", HIST_NAMES[i], c));
            }
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
            lock_wait_max_ns: AtomicU64::new(0),
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
