//! Amazon Elastic Network Adapter (ENA), the NIC of every EC2 Nitro instance
//! (PCI 1d0f:ec20). The driver core (admin queue, AENQ, readless MMIO, TX/RX
//! rings, watchdog) is `ena_hw.rs`, host-tested against a mock device in
//! `drivers/ena-tests`; this file is the PCI probe, the BAR/DMA glue and the
//! netdev entry points. Everything is polled (no MSI-X yet), host-placement
//! queues only. Inert when no ENA function exists: `init` logs one line.
//!
//! Cmdline: `ena.disable=1` skips the probe.
use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use spin::Mutex;

use crate::ena_hw::{self as hw, DmaBuf, Ena, EnaBus, Watchdog};
use crate::pci;

extern "C" {
    fn serial_print_str_raw(s: *const u8, len: usize);
}

fn log(msg: &str) {
    let mut s = String::with_capacity(msg.len() + 8);
    s.push_str("[ena] ");
    s.push_str(msg);
    s.push('\n');
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
}
macro_rules! elog { ($($a:tt)*) => { log(&format!($($a)*)) }; }

/// BAR0 and the kernel's DMA memory.
struct KBus {
    base: usize,
}

impl EnaBus for KBus {
    fn read32(&self, off: usize) -> u32 { unsafe { core::ptr::read_volatile((self.base + off) as *const u32) } }
    fn write32(&self, off: usize, v: u32) { unsafe { core::ptr::write_volatile((self.base + off) as *mut u32, v) } }
    fn dma_alloc(&self, len: usize) -> Option<DmaBuf> {
        if len > 4096 { return None; }
        let pa = mm::buddy::alloc_below(0, 1usize << 32)?;
        let va = mm::phys_to_virt(pa);
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, 4096); }
        Some(DmaBuf { va: va as *mut u8, pa: pa as u64, len: 4096 })
    }
    fn udelay(&self, us: u64) { sched::kthread::udelay(us) }
    fn now_ms(&self) -> u64 { sched::monotonic_ns() / 1_000_000 }
}

static DEV: Mutex<Option<Ena<KBus>>> = Mutex::new(None);
static PRESENT: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
static LINK_UP: AtomicBool = AtomicBool::new(false);
static LINK_GEN: AtomicU32 = AtomicU32::new(0);
static MAC: Mutex<[u8; 6]> = Mutex::new([0; 6]);
static BDF: Mutex<(u8, u8, u8, u16, u8)> = Mutex::new((0, 0, 0, 0, 0));

fn cmdline_val<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline.split_ascii_whitespace().filter_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('='))).last()
}

fn mirror(e: &Ena<KBus>) {
    LINK_UP.store(e.link_up(), Ordering::Relaxed);
    LINK_GEN.store(e.link_gen(), Ordering::Relaxed);
}

/// Probe the first ENA function and bring it up synchronously (a few hundred
/// microseconds to milliseconds of register polling; the device resets in
/// well under a second). Boot context.
pub fn init(cmdline: &str) {
    if cmdline_val(cmdline, "ena.disable") == Some("1") {
        log("disabled (ena.disable=1)");
        return;
    }
    let dev = match hw::DEVICE_IDS.iter().find_map(|&d| pci::find_device(hw::VENDOR, d)) {
        Some(d) => d,
        None => { log("no Amazon ENA (1d0f:ec20) found; driver inert"); return; }
    };
    let func = dev.function();
    let rev = func.read8(0x08);
    // Revision bit 0 (ENA_MMIO_DISABLE_REG_READ): plain register reads are forbidden.
    let readless = rev & 1 != 0;
    elog!("found 1d0f:{:04x} at {:02x}:{:02x}.{} (rev {:02x}{})", dev.device_id, dev.bus, dev.dev, dev.func, rev,
          if readless { ", readless MMIO" } else { "" });
    func.enable_mem_and_bus_master();
    func.set_intx_disable(true);
    let (va, size) = match func.map_bar(0, mm::kva::CacheMode::Uncached) {
        Some(x) => x,
        None => { log("BAR0 not mappable; giving up"); return; }
    };
    let mut ena = match Ena::new(KBus { base: va }, readless) {
        Ok(e) => e,
        Err(e) => { elog!("out of DMA memory ({:?}); giving up", e); return; }
    };
    if let Err(e) = ena.init() {
        elog!("bring-up failed: {:?}", e);
        return;
    }
    let i = ena.info;
    let m = ena.mac;
    elog!("BAR0 {} KiB; device v{}.{} ctrl {:08x}, DMA {} bit, max MTU {}, TX/RX depth {}/{}, AENQ groups {:#x}",
          size / 1024, (i.version >> 8) & 0xff, i.version & 0xff, i.controller_version, i.dma_width, i.max_mtu,
          i.tx_depth, i.rx_depth, i.enabled_groups);
    match i.llq {
        Some(l) => elog!("LLQ offered (caps {:08x} {:08x} {:08x} {:08x}); using host placement", l[0], l[1], l[2], l[3]),
        None => log("LLQ not offered; host placement"),
    }
    elog!("MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5]);
    *MAC.lock() = m;
    *BDF.lock() = (dev.bus, dev.dev, dev.func, dev.device_id, rev);
    mirror(&ena);
    *DEV.lock() = Some(ena);
    PRESENT.store(true, Ordering::SeqCst);
    if sched::kthread::spawn("ena", watchdog_thread).is_none() {
        log("could not start the watchdog thread; no keep-alive monitoring");
    }
}

/// ~10 Hz: AENQ events (link, keep-alive), device health, resets.
fn watchdog_thread() {
    while !STOP.load(Ordering::Relaxed) {
        sched::kthread::msleep(100);
        let mut g = DEV.lock();
        let e = match g.as_mut() { Some(e) => e, None => continue };
        let w = e.watchdog();
        mirror(e);
        drop(g);
        match w {
            Watchdog::Ok => {}
            Watchdog::Reset(why) => elog!("device reset ({}), queues rebuilt", why),
            Watchdog::ResetFailed(why) => elog!("device reset ({}) FAILED; retrying", why),
        }
        if w != Watchdog::Ok { crate::netdev::kick(); }
    }
}

/// Quiesce the device (poweroff / kexec): reset stops its DMA.
pub fn shutdown() -> Option<String> {
    if !PRESENT.load(Ordering::Relaxed) { return None; }
    STOP.store(true, Ordering::SeqCst);
    let mut g = DEV.lock();
    let e = g.as_mut()?;
    let r = e.shutdown();
    mirror(e);
    Some(format!("ena: device reset ({})", match r { Ok(()) => String::from("ok"), Err(x) => format!("{:?}", x) }))
}

pub fn present() -> bool { PRESENT.load(Ordering::Relaxed) }
pub fn mac_address() -> Option<[u8; 6]> { if present() { Some(*MAC.lock()) } else { None } }
/// (up, Mbit/s, full duplex). ENA reports no speed; 10 Gbit/s is nominal.
pub fn link() -> (bool, u32, bool) {
    let up = LINK_UP.load(Ordering::Relaxed);
    (up, if up { 10000 } else { 0 }, true)
}
pub fn link_generation() -> u32 { LINK_GEN.load(Ordering::Relaxed) }
pub fn send_packet(buf: &[u8]) -> bool { DEV.lock().as_mut().map_or(false, |e| e.send(buf)) }
pub fn tx_ready() -> bool { DEV.lock().as_mut().map_or(false, |e| e.tx_ready()) }
pub fn poll_receive(out: &mut [u8]) -> Option<usize> { DEV.lock().as_mut()?.recv(out) }
/// Polled: report whether a frame is already waiting.
pub fn rx_rearm() -> bool { DEV.lock().as_ref().map_or(false, |e| e.rx_pending()) }
pub fn rx_irq_driven() -> bool { false }

pub fn info_lines(out: &mut String) {
    use core::fmt::Write;
    let g = DEV.lock();
    let e = match g.as_ref() { Some(e) => e, None => return };
    let (b, d, f, did, rev) = *BDF.lock();
    let m = e.mac;
    let (i, s) = (e.info, e.stats);
    let _ = writeln!(out, "driver: ena");
    let _ = writeln!(out, "pci: {:02x}:{:02x}.{} 1d0f:{:04x} rev {:02x}", b, d, f, did, rev);
    let _ = writeln!(out, "mac: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5]);
    let _ = writeln!(out, "running: {}", if e.running() { "yes" } else { "no" });
    let _ = writeln!(out, "link: {}", if e.link_up() { "up" } else { "down" });
    let _ = writeln!(out, "readless_mmio: {}", if e.readless() { "yes" } else { "no" });
    let _ = writeln!(out, "device_version: {}.{}", (i.version >> 8) & 0xff, i.version & 0xff);
    let _ = writeln!(out, "queues: tx {} rx {} (host placement, polled)", i.tx_depth, i.rx_depth);
    let _ = writeln!(out, "llq_offered: {}", if i.llq.is_some() { "yes" } else { "no" });
    let _ = writeln!(out, "tx_pkts: {}\ntx_bytes: {}\ntx_full: {}", s.tx_pkts, s.tx_bytes, s.tx_full);
    let _ = writeln!(out, "rx_pkts: {}\nrx_bytes: {}\nrx_drops: {}", s.rx_pkts, s.rx_bytes, s.rx_drops);
    let _ = writeln!(out, "aenq_events: {}\nkeepalives: {}\nresets: {}", s.aenq_events, s.keepalives, s.resets);
    let _ = writeln!(out, "admin_cmds: {}\nadmin_timeouts: {}", s.admin_cmds, s.admin_timeouts);
}
