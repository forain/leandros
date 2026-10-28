//! Synopsys DesignWare I2C master behind the Intel LPSS PCI functions
//! (Sunrise Point-LP 8086:9d60..9d63), polled, no interrupts (x86_64).
//! Lane lapi2c.

use crate::i2c_acpi::{log, Timing};
use crate::pci::{self, PciDevice};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

extern "C" {
    fn arch_monotonic_ns() -> u64;
}

const CON: usize = 0x00;
const TAR: usize = 0x04;
const DATA_CMD: usize = 0x10;
const SS_HCNT: usize = 0x14;
const SS_LCNT: usize = 0x18;
const FS_HCNT: usize = 0x1C;
const FS_LCNT: usize = 0x20;
const INTR_MASK: usize = 0x30;
const RAW_INTR: usize = 0x34;
const RX_TL: usize = 0x38;
const TX_TL: usize = 0x3C;
const CLR_INTR: usize = 0x40;
const CLR_TX_ABRT: usize = 0x54;
const ENABLE: usize = 0x6C;
const STATUS: usize = 0x70;
const TXFLR: usize = 0x74;
const RXFLR: usize = 0x78;
const SDA_HOLD: usize = 0x7C;
const TX_ABRT_SRC: usize = 0x80;
const ENABLE_STATUS: usize = 0x9C;
const COMP_PARAM_1: usize = 0xF4;
const COMP_TYPE: usize = 0xFC;
const DW_COMP_TYPE: u32 = 0x4457_0140;

const RAW_TX_ABRT: u32 = 1 << 6;
const RAW_STOP_DET: u32 = 1 << 9;

pub const ETIMEDOUT: u32 = 110;

pub static DEBUG: AtomicBool = AtomicBool::new(false);

struct Bus {
    base: usize,
    dev: u8,
    func: u8,
    tx_depth: u32,
    rx_depth: u32,
}

static BUSES: Mutex<Vec<Bus>> = Mutex::new(Vec::new());

fn now() -> u64 { unsafe { arch_monotonic_ns() } }

fn spin_ns(ns: u64) {
    let t0 = now();
    while now() - t0 < ns { core::hint::spin_loop(); }
}

impl Bus {
    fn rd(&self, off: usize) -> u32 { unsafe { core::ptr::read_volatile((self.base + off) as *const u32) } }
    fn wr(&self, off: usize, v: u32) { unsafe { core::ptr::write_volatile((self.base + off) as *mut u32, v) } }

    fn set_enable(&self, on: bool) -> bool {
        self.wr(ENABLE, on as u32);
        let t0 = now();
        while now() - t0 < 25_000_000 {
            if self.rd(ENABLE_STATUS) & 1 == on as u32 { return true; }
            core::hint::spin_loop();
        }
        false
    }

    fn xfer(&self, addr: u16, w: &[u8], r: &mut [u8]) -> Result<(), u32> {
        if !self.set_enable(false) { return Err(ETIMEDOUT); }
        self.wr(TAR, addr as u32 & 0x3FF);
        self.wr(INTR_MASK, 0);
        let _ = self.rd(CLR_INTR);
        if !self.set_enable(true) { return Err(ETIMEDOUT); }
        let mut cmds: Vec<u32> = Vec::with_capacity(w.len() + r.len());
        for (i, &b) in w.iter().enumerate() {
            let stop = if i + 1 == w.len() && r.is_empty() { 1 << 9 } else { 0 };
            cmds.push(b as u32 | stop);
        }
        for i in 0..r.len() {
            let mut c = 1u32 << 8;
            if i == 0 && !w.is_empty() { c |= 1 << 10; }
            if i + 1 == r.len() { c |= 1 << 9; }
            cmds.push(c);
        }
        let (mut idx, mut rx, mut reads_pushed) = (0usize, 0usize, 0u32);
        let t0 = now();
        let res = loop {
            if self.rd(RAW_INTR) & RAW_TX_ABRT != 0 {
                let src = self.rd(TX_ABRT_SRC);
                let _ = self.rd(CLR_TX_ABRT);
                break Err(if src == 0 { 1 } else { src });
            }
            while rx < r.len() && self.rd(RXFLR) > 0 {
                r[rx] = self.rd(DATA_CMD) as u8;
                rx += 1;
            }
            while idx < cmds.len() && self.rd(TXFLR) < self.tx_depth {
                let is_read = cmds[idx] & (1 << 8) != 0;
                if is_read && reads_pushed - rx as u32 >= self.rx_depth { break; }
                self.wr(DATA_CMD, cmds[idx]);
                if is_read { reads_pushed += 1; }
                idx += 1;
            }
            if idx == cmds.len() && rx == r.len() && self.rd(RAW_INTR) & RAW_STOP_DET != 0 { break Ok(()); }
            if now() - t0 > 50_000_000 { break Err(ETIMEDOUT); }
            core::hint::spin_loop();
        };
        let _ = self.rd(CLR_INTR);
        self.set_enable(false);
        res
    }
}

/// Every LPSS I2C PCI function present.
pub fn find() -> Vec<PciDevice> {
    let mut v = Vec::new();
    for id in 0x9d60u16..=0x9d63 { v.extend(pci::find_all_devices(0x8086, id)); }
    v
}

/// Bring one controller out of reset and register it. Returns the bus index.
pub fn bring_up(d: &PciDevice, tm: Option<&Timing>) -> Option<usize> {
    let name = format!("{:02x}:{:02x}.{}", d.bus, d.dev, d.func);
    let f = d.function();
    let cmd = f.read16(0x04);
    f.write16(0x04, cmd | pci::CMD_MEMORY | pci::CMD_BUS_MASTER);
    if let Some(pm) = f.find_capability(0x01) {
        let pmcsr = f.read16(pm + 4);
        if pmcsr & 3 != 0 {
            f.write16(pm + 4, pmcsr & !3);
            spin_ns(10_000_000);
        }
    }
    let Some((va, size)) = f.map_bar(0, mm::kva::CacheMode::Uncached) else {
        log(&format!("dw-i2c: {} BAR0 not mappable", name));
        return None;
    };
    if size < 0x1000 { log(&format!("dw-i2c: {} BAR0 too small ({:#x})", name, size)); return None; }
    let mut b = Bus { base: va, dev: d.dev, func: d.func, tx_depth: 8, rx_depth: 8 };
    // LPSS private block: take the function and its iDMA out of reset.
    b.wr(0x204, 0x7);
    let ty = b.rd(COMP_TYPE);
    if ty != DW_COMP_TYPE {
        log(&format!("dw-i2c: {} not DesignWare (comp_type {:#x})", name, ty));
        return None;
    }
    let caps = b.rd(0x2FC);
    let p1 = b.rd(COMP_PARAM_1);
    b.tx_depth = ((p1 >> 16) & 0xFF) + 1;
    b.rx_depth = ((p1 >> 8) & 0xFF) + 1;
    if !b.set_enable(false) { log(&format!("dw-i2c: {} would not disable", name)); return None; }
    b.wr(CON, 0x65);
    let fs = tm.and_then(|t| t.fs).filter(|t| t[0] != 0 && t[1] != 0);
    let ss = tm.and_then(|t| t.ss).filter(|t| t[0] != 0 && t[1] != 0);
    let (fh, fl, fsh, src) = match fs { Some(t) => (t[0], t[1], t[2], "FMCN"), None => (0x48, 0xA0, 0x1E, "default") };
    let (sh, sl) = match ss { Some(t) => (t[0], t[1]), None => (0x1B0, 0x1FB) };
    b.wr(FS_HCNT, fh);
    b.wr(FS_LCNT, fl);
    b.wr(SS_HCNT, sh);
    b.wr(SS_LCNT, sl);
    b.wr(SDA_HOLD, if fsh != 0 { fsh } else { 0x1E });
    b.wr(INTR_MASK, 0);
    b.wr(RX_TL, 0);
    b.wr(TX_TL, 0);
    let _ = b.rd(CLR_INTR);
    let (dev, func, tx, rx) = (b.dev, b.func, b.tx_depth, b.rx_depth);
    let mut g = BUSES.lock();
    g.push(b);
    log(&format!("dw-i2c: {} up comp={:#x} caps={:#x} fifo={}/{} fs={:#x}/{:#x}/{:#x} ({})", name, ty, caps, tx, rx, fh, fl, fsh, src));
    let _ = (dev, func);
    Some(g.len() - 1)
}

/// Bus index for a controller `_ADR` (0x00150001 = dev 0x15 fn 1).
pub fn bus_for_adr(adr: u32) -> Option<usize> {
    let (dev, func) = ((adr >> 16) as u8, adr as u8);
    BUSES.lock().iter().position(|b| b.dev == dev && b.func == func)
}

pub fn bus_count() -> usize { BUSES.lock().len() }

fn hex(b: &[u8]) -> String {
    let mut s = String::new();
    for x in b { s += &format!("{:02x} ", x); }
    s
}

/// One write-then-read transaction (repeated start between the phases).
/// `Err` carries TX_ABRT_SOURCE or `ETIMEDOUT`.
pub fn xfer(bus: usize, addr: u16, w: &[u8], r: &mut [u8]) -> Result<(), u32> {
    if w.len() + r.len() > 4096 { return Err(22); }
    let g = BUSES.lock();
    let b = g.get(bus).ok_or(19u32)?;
    let res = b.xfer(addr, w, r);
    if DEBUG.load(Ordering::Relaxed) {
        let n = r.len().min(32);
        log(&format!("dw-i2c: bus{} @{:#x} w[{}]={} r[{}]={}-> {:?}", bus, addr, w.len(), hex(&w[..w.len().min(32)]), r.len(), hex(&r[..n]), res));
    }
    res
}
