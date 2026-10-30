//! USB mass storage (bulk-only + SCSI) as a loadable module. The SCSI/BOT layer is the SAME source
//! as the built-in (`drivers/usb/src/msc`); the transport is the kapi bulk pipe. Bound by the
//! `usb * * 08 06 50` alias through the class registry; disks go through `kapi_blk_register`
//! (removable, unplug drains the in-flight request and `kapi_blk_remove`s the disk). The built-in
//! driver's latency knobs, IoWindow and iostat counters are not carried over: the module sleeps.
#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use kmod_abi::KUsbIf;
use kmod_kapi::blk::env::sched::ksync::KMutex;
use kmod_kapi::usb as kusb;

macro_rules! usb_log { ($($a:tt)*) => { kmod_rt::kinfo!($($a)*) } }
pub(crate) use usb_log;

mod platform {
    pub fn now_ns() -> u64 { kmod_kapi::sched::monotonic_ns() }
}

#[path = "../../../drivers/usb/src/transport.rs"]
mod transport;
#[path = "../../../drivers/usb/src/msc/mod.rs"]
mod msc;
#[path = "../../../drivers/src/blockdev.rs"]
mod blockdev;
#[path = "../../../drivers/src/blkmod.rs"]
mod blkdev;

use blockdev::{BlkErr, BlockDevice};
use msc::{MscErr, ScsiDisk};
use transport::{BulkTransport, UsbErr};

const PROBE_READY_MS: u32 = 3000;

/// The bulk-only transport over a kapi pipe.
struct KBulk { pipe: u64, iface: u8 }

fn uerr(e: i32) -> UsbErr {
    match e { -32 => UsbErr::Stall, -110 => UsbErr::Timeout, -75 => UsbErr::Babble, -19 => UsbErr::Disconnected, _ => UsbErr::Other(5) }
}

impl KBulk {
    fn xfer(&self, op: u32, arg: u64, buf: *mut u8, len: usize, ms: u32) -> Result<usize, UsbErr> {
        kusb::bulk(self.pipe, op, arg, buf, len, ms as u64 * 1_000_000).map_err(uerr)
    }
}

impl BulkTransport for KBulk {
    fn bulk_out(&mut self, data: &[u8], ms: u32) -> Result<usize, UsbErr> {
        self.xfer(kusb::BULK_OUT, 0, data.as_ptr() as *mut u8, data.len(), ms)
    }
    fn bulk_in(&mut self, buf: &mut [u8], ms: u32) -> Result<usize, UsbErr> {
        self.xfer(kusb::BULK_IN, 0, buf.as_mut_ptr(), buf.len(), ms)
    }
    fn class_control_out(&mut self, request: u8, value: u16, index: u16, data: &[u8]) -> Result<(), UsbErr> {
        let arg = request as u64 | (value as u64) << 8 | (index as u64) << 24;
        self.xfer(kusb::BULK_CTL_OUT, arg, data.as_ptr() as *mut u8, data.len(), 5000).map(|_| ())
    }
    fn class_control_in(&mut self, request: u8, value: u16, index: u16, buf: &mut [u8]) -> Result<usize, UsbErr> {
        let arg = request as u64 | (value as u64) << 8 | (index as u64) << 24;
        self.xfer(kusb::BULK_CTL_IN, arg, buf.as_mut_ptr(), buf.len(), 5000)
    }
    fn clear_halt_in(&mut self) -> Result<(), UsbErr> { self.xfer(kusb::BULK_CLEAR_IN, 0, core::ptr::null_mut(), 0, 5000).map(|_| ()) }
    fn clear_halt_out(&mut self) -> Result<(), UsbErr> { self.xfer(kusb::BULK_CLEAR_OUT, 0, core::ptr::null_mut(), 0, 5000).map(|_| ()) }
    fn interface_number(&self) -> u8 { self.iface }
}

struct UsbDisk {
    disk: KMutex<ScsiDisk<KBulk>>,
    sector_size: u32,
    sectors: u64,
    read_only: bool,
    gone: AtomicBool,
    blk_idx: AtomicUsize,
    handle: u64,
    pipe: u64,
}

static DISKS: spin::Mutex<Vec<&'static UsbDisk>> = spin::Mutex::new(Vec::new());

fn map_err(e: MscErr) -> BlkErr { match e { MscErr::InvalidArg => BlkErr::OutOfRange, _ => BlkErr::Io } }

impl UsbDisk {
    fn alive(&self) -> Result<(), BlkErr> {
        if self.gone.load(Ordering::Acquire) { Err(BlkErr::NoDevice) } else { Ok(()) }
    }
}

impl BlockDevice for UsbDisk {
    fn sector_size(&self) -> u32 { self.sector_size }
    fn capacity_sectors(&self) -> u64 { self.sectors }
    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlkErr> {
        self.alive()?;
        if buf.is_empty() || buf.len() % self.sector_size as usize != 0 { return Err(BlkErr::BadBuffer); }
        let mut g = self.disk.lock();
        self.alive()?;
        g.read_sectors(lba, buf).map_err(map_err)
    }
    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlkErr> {
        self.alive()?;
        if buf.is_empty() || buf.len() % self.sector_size as usize != 0 { return Err(BlkErr::BadBuffer); }
        if self.read_only { return Err(BlkErr::Io); }
        let mut g = self.disk.lock();
        self.alive()?;
        g.write_sectors(lba, buf).map_err(map_err)
    }
    fn try_write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlkErr> {
        self.alive()?;
        if self.read_only || buf.is_empty() || buf.len() % self.sector_size as usize != 0 { return Err(BlkErr::Io); }
        match self.disk.try_lock() {
            Some(mut g) => g.write_sectors(lba, buf).map_err(map_err),
            None => Err(BlkErr::Io),
        }
    }
    fn flush(&self) -> Result<(), BlkErr> {
        self.alive()?;
        let mut g = self.disk.lock();
        self.alive()?;
        if g.sync_cache_unsupported() { return Ok(()); }
        g.flush().map_err(map_err)
    }
    fn removable(&self) -> bool { true }
    fn name_class(&self) -> &'static str { "sd" }
    fn read_only(&self) -> bool { self.read_only }
}

fn delay_ms(ms: u32) {
    if kmod_kapi::blk::env::sched::preempt_disabled() { return; }
    kmod_kapi::blk::env::sleep_until(kmod_kapi::sched::monotonic_ns().saturating_add(ms as u64 * 1_000_000));
}

extern "C" fn probe(intf: *const KUsbIf, _ctx: usize) -> i32 {
    let i = unsafe { &*intf };
    let pipe = match kusb::bulk_open(i.handle, i.iface) {
        Ok(p) => p,
        Err(e) => { kmod_rt::kerr!("[USB-MSC] slot {} interface {}: bulk open failed ({})", i.slot, i.iface, e); return -5; }
    };
    let mut disk = ScsiDisk::new(KBulk { pipe, iface: i.iface }, delay_ms);
    disk.set_atomic_probe(|| !kmod_kapi::blk::env::sched::irqctx::can_sleep());
    disk.set_ready_timeout_ms(PROBE_READY_MS);
    if let Err(e) = disk.init() {
        kmod_rt::kerr!("[USB-MSC] slot {}: no usable LUN ({:?})", i.slot, e);
        kusb::bulk_close(pipe);
        return -19;
    }
    let (ss, sectors, ro, lun) = (disk.sector_size(), disk.capacity_sectors(), disk.read_only(), disk.lun());
    kmod_rt::kinfo!("[USB-MSC] interface {} LUN {}: {} x {}-byte sectors{}", i.iface, lun, sectors, ss,
                    if ro { ", write-protected" } else { "" });
    let dev: &'static UsbDisk = Box::leak(Box::new(UsbDisk {
        disk: KMutex::new(disk), sector_size: ss, sectors, read_only: ro,
        gone: AtomicBool::new(false), blk_idx: AtomicUsize::new(usize::MAX), handle: i.handle, pipe,
    }));
    DISKS.lock().push(dev);
    match blkdev::register(dev) {
        Ok(idx) => { dev.blk_idx.store(idx, Ordering::Release); 0 }
        Err(e) => {
            kmod_rt::kerr!("[USB-MSC] block registration failed: {:?}", e);
            dev.gone.store(true, Ordering::Release);
            -5
        }
    }
}

/// The device is gone: fail the disk, wait for the request on the wire (it sees the disconnect and
/// fails), drop the block entry, close the pipe.
extern "C" fn detach(handle: u64, _ctx: usize) {
    let disks: Vec<&'static UsbDisk> = DISKS.lock().iter().copied().filter(|d| d.handle == handle).collect();
    for d in disks {
        d.gone.store(true, Ordering::Release);
        let t0 = kmod_kapi::sched::monotonic_ns();
        drop(d.disk.lock());
        let idx = d.blk_idx.load(Ordering::Acquire);
        if idx != usize::MAX { kmod_kapi::blk::remove(idx as i64); }
        kusb::bulk_close(d.pipe);
        DISKS.lock().retain(|x| !core::ptr::eq(*x, d));
        kmod_rt::kinfo!("[USB-MSC] disk (slot handle {}) unplugged: block entry removed, drained in {} ms", handle,
                        kmod_kapi::sched::monotonic_ns().saturating_sub(t0) / 1_000_000);
    }
}

const IDS: [kmod_abi::KmodDevId; 1] = [kmod_rt::usb(0xffff, 0xffff, 0x08_06_50, 0xFF_FF_FF)];

kmod_rt::module! {
    name: "usb_storage",
    init: init,
    ids: [kmod_rt::usb(0xffff, 0xffff, 0x08_06_50, 0xFF_FF_FF)],
}

fn init() -> i32 {
    if let Err(e) = kusb::class_register(&IDS, probe, Some(detach), 0) {
        kmod_rt::kerr!("usb_class_register failed: {}", e);
        return e;
    }
    0
}
