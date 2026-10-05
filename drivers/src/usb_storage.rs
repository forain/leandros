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
//! I/O runs in syscall context (f2fs, the VFS block layer) with IRQs masked
//! and, usually, f2fs/VFS spinlocks held. It must therefore never switch
//! tasks: every request runs inside an [`IoWindow`] (preemption disabled), and
//! the xHCI poll hook (`usb_hcd::relax`) spins with periodic `irq_window`s
//! instead of yielding — the tick, tick hooks and poll deadlines stay alive,
//! no lock is parked. This is the same contract as virtio-blk's spin wait and
//! virtio-gpu's `SpinWindow`. (Yielding there deadlocked the whole guest on
//! the first USB-root boot: a parked f2fs lock, four CPUs spinning on it.)
//!
//! Locking: `BlockDevice` methods take `&self` and may be called from several
//! CPUs; `ScsiDisk` needs `&mut`, so each disk sits behind its own
//! `spin::Mutex`, held for one request's command sequence only.
//!
//! Errors: every class-driver failure becomes `BlkErr::Io` (after the class
//! driver's own retry ladder, reset recovery included), bad arguments become
//! `BadBuffer`/`OutOfRange`. Each USB transfer has a deadline (5 s, 30 s for
//! SYNCHRONIZE CACHE), so a dead or unplugged stick yields EIO, never a hang.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, Ordering};

use spin::{Mutex, MutexGuard};
use usb::msc::{MscErr, ScsiDisk};

use crate::blkdev::{self, BlkErr, BlockDevice};
use crate::pci::{serial_debug, serial_debug_dec as log_dec};
use crate::usb_hcd::{BulkTransport, XhciBulk};

/// Sleep for `ms` milliseconds without burning the CPU: park on the
/// scheduler's timed wait channel until the deadline (the same mechanism
/// `nanosleep` uses), re-checking the clock after every wake.
fn delay_ms(ms: u32) {
    let deadline = sched::monotonic_ns().saturating_add(ms as u64 * 1_000_000);
    while sched::monotonic_ns() < deadline {
        sched::block_on_poll_prepare_masked(deadline, sched::POLL_MASK_TIMED_ONLY);
        if sched::monotonic_ns() >= deadline {
            sched::block_on_poll_cancel();
        } else {
            sched::block_on_poll_commit();
        }
    }
}

struct UsbDisk {
    disk: Mutex<ScsiDisk<XhciBulk>>,
    sector_size: u32,
    sectors: u64,
    read_only: bool,
    /// One "write refused" log line per disk, not one per block.
    warned_ro: AtomicBool,
}

/// Preemption disabled for one block request when it runs with IRQs masked
/// (syscall context), so the `irq_window`s the poll loop opens take the tick
/// but never switch tasks. Released without a resched on every exit; the
/// deferred switch happens at the next tick or syscall return, where no lock
/// is held. At boot (init task, IRQs on) it is a no-op and polls may yield.
struct IoWindow { open: bool }
impl IoWindow {
    fn new() -> Self {
        let open = sched::irqs_masked() && sched::current_pid() != 0;
        if open { sched::preempt_disable(); }
        IoWindow { open }
    }
}
impl Drop for IoWindow {
    fn drop(&mut self) {
        if self.open { sched::preempt_enable_no_resched(); }
    }
}

impl UsbDisk {
    /// Take the disk. Holders never switch tasks with IRQs masked (see
    /// [`IoWindow`]), so a short spin is the right wait; at boot, with IRQs
    /// on, yield instead.
    fn lock(&self) -> MutexGuard<'_, ScsiDisk<XhciBulk>> {
        loop {
            if let Some(g) = self.disk.try_lock() {
                return g;
            }
            if sched::irqs_masked() {
                core::hint::spin_loop();
            } else {
                sched::yield_now("usb-disk");
            }
        }
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
        let _w = IoWindow::new();
        self.lock().read_sectors(lba, buf).map_err(|e| {
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
        let _w = IoWindow::new();
        self.lock().write_sectors(lba, buf).map_err(|e| {
            log_err("write failed", e);
            map_err(e)
        })
    }

    fn flush(&self) -> Result<(), BlkErr> {
        if self.read_only { return Ok(()); }
        let _w = IoWindow::new();
        self.lock().flush().map_err(|e| {
            log_err("flush failed", e);
            map_err(e)
        })
    }

    fn removable(&self) -> bool { true }
    fn name_class(&self) -> &'static str { "sd" }
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
        let dev: &'static UsbDisk = Box::leak(Box::new(UsbDisk {
            disk: Mutex::new(disk),
            sector_size: ss,
            sectors,
            read_only: ro,
            warned_ro: AtomicBool::new(false),
        }));
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
