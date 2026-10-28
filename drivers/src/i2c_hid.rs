//! HID-over-I2C (spec 1.0) probe (x86_64), lane lapi2c phase 1.
//! Brings the LPSS I2C buses up, lists the ACPI slaves, and for every live
//! PNP0C50 slave reads the HID descriptor and the report descriptor and
//! counts input reports for a while. No input node, no Input Mode switch: the
//! default `i2c_hid=probe` changes no behaviour.
//! Knobs: `i2c=0` (nothing), `i2c_hid=probe|0` , `i2c.debug=1` (hex-dump transfers).

use crate::dw_i2c;
use crate::hid_desc;
use crate::i2c_acpi::{self, log, I2cSlave};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use spin::Mutex;

const LISTEN_NS: u64 = 120_000_000_000;

struct Listen {
    name: String,
    bus: usize,
    addr: u16,
    input_reg: u16,
    max_in: usize,
    start: u64,
    reports: u32,
    empty: u32,
    errs: u32,
    logged: u32,
}

static LISTEN: Mutex<Vec<Listen>> = Mutex::new(Vec::new());

fn word(knob: &str, cl: &str) -> Option<&'static str> {
    let _ = cl;
    crate::nvidia_probe::cmdline_word(knob)
}

fn crc32(d: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in d {
        c ^= b as u32;
        for _ in 0..8 { c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 }; }
    }
    !c
}

fn hex(b: &[u8]) -> String {
    let mut s = String::new();
    for x in b { s += &format!("{:02x} ", x); }
    s
}

fn spin_ms(ms: u64) {
    extern "C" { fn arch_monotonic_ns() -> u64; }
    let t0 = unsafe { arch_monotonic_ns() };
    while unsafe { arch_monotonic_ns() } - t0 < ms * 1_000_000 { core::hint::spin_loop(); }
}

fn short(path: &str) -> &str { path.rsplit('.').next().unwrap_or(path) }

fn probe_slave(s: &I2cSlave, bus: usize, reset: bool) -> Option<Listen> {
    let name = String::from(short(&s.path).trim_end_matches('_'));
    let reg = s.hid_desc_reg?;
    let mut d = [0u8; 30];
    if let Err(e) = dw_i2c::xfer(bus, s.addr, &[reg as u8, (reg >> 8) as u8], &mut d) {
        log(&format!("i2c-hid: {} HID descriptor read @{:#x} reg {:#x} failed: {:#x}", name, s.addr, reg, e));
        return None;
    }
    let w = |o: usize| u16::from_le_bytes([d[o], d[o + 1]]);
    log(&format!("i2c-hid: {} desc {}(30 B)", name, hex(&d)));
    if w(0) != 30 || w(2) != 0x0100 {
        log(&format!("i2c-hid: {} not a HID descriptor (len {} bcd {:#x})", name, w(0), w(2)));
        return None;
    }
    let (rd_len, rd_reg, in_reg, max_in, cmd_reg) = (w(4) as usize, w(6), w(8), w(10) as usize, w(16));
    log(&format!("i2c-hid: {} vid={:04x} pid={:04x} ver={:04x} rd_len={} in={:#x} max_in={} cmd={:#x} data={:#x}",
        name, w(20), w(22), w(24), rd_len, in_reg, max_in, cmd_reg, w(18)));
    if reset {
        // SET_POWER ON, then RESET and wait for the 0x0000 acknowledge.
        let c = [cmd_reg as u8, (cmd_reg >> 8) as u8];
        let _ = dw_i2c::xfer(bus, s.addr, &[c[0], c[1], 0x00, 0x08], &mut []);
        spin_ms(1);
        let rst = dw_i2c::xfer(bus, s.addr, &[c[0], c[1], 0x00, 0x01], &mut []);
        let mut acked = false;
        for _ in 0..100 {
            let mut a = [0xFFu8; 2];
            if dw_i2c::xfer(bus, s.addr, &[in_reg as u8, (in_reg >> 8) as u8], &mut a).is_ok() && a == [0, 0] { acked = true; break; }
            spin_ms(1);
        }
        log(&format!("i2c-hid: {} reset write={:?} ack={}", name, rst, acked));
    }
    if rd_len == 0 || rd_len > 4096 { return None; }
    let mut rep = alloc::vec![0u8; rd_len];
    if let Err(e) = dw_i2c::xfer(bus, s.addr, &[rd_reg as u8, (rd_reg >> 8) as u8], &mut rep) {
        log(&format!("i2c-hid: {} report descriptor read failed: {:#x}", name, e));
        return None;
    }
    let parsed = hid_desc::parse(&rep);
    log(&format!("i2c-hid: {} report desc {} B crc={:08x} {}", name, rd_len, crc32(&rep), hid_desc::summary(&parsed)));
    extern "C" { fn arch_monotonic_ns() -> u64; }
    Some(Listen { name, bus, addr: s.addr, input_reg: in_reg, max_in: max_in.clamp(2, 256), start: unsafe { arch_monotonic_ns() },
        reports: 0, empty: 0, errs: 0, logged: 0 })
}

/// Probe everything. Returns true when a listen thread should poll
/// [`listen_once`]. Task context, after ACPI power init.
pub fn init(cmdline: &str) -> bool {
    if word("i2c", cmdline) == Some("0") { return false; }
    dw_i2c::DEBUG.store(word("i2c.debug", cmdline) == Some("1"), Ordering::Relaxed);
    let mode = word("i2c_hid", cmdline).unwrap_or("probe");
    let devs = dw_i2c::find();
    if devs.is_empty() {
        log("i2c: no LPSS I2C controllers");
        return false;
    }
    if mode == "1" { log("i2c-hid: i2c_hid=1 (input node) is not in this build, probing only"); }
    let Some(sc) = i2c_acpi::scan(true) else {
        log("i2c-acpi: no ACPI namespace, skipping slave scan");
        return false;
    };
    for d in &devs {
        let adr = ((d.dev as u32) << 16) | d.func as u32;
        let tm = sc.timings.iter().find(|t| t.ctrl_adr == adr);
        dw_i2c::bring_up(d, tm);
    }
    for s in &sc.slaves { log(&i2c_acpi::describe(s)); }
    if mode == "0" { return false; }
    let mut l = LISTEN.lock();
    for s in sc.slaves.iter().filter(|s| s.sta & 0xB == 0xB && s.hid_desc_reg.is_some()) {
        match dw_i2c::bus_for_adr(s.ctrl_adr) {
            Some(bus) => { if let Some(p) = probe_slave(s, bus, mode == "probe+reset") { l.push(p); } }
            None => log(&format!("i2c-hid: {} controller {:#x} not up", s.path, s.ctrl_adr)),
        }
    }
    !l.is_empty()
}

/// One poll step of the input-report counter; false when the window is over.
pub fn listen_once(now_ns: u64) -> bool {
    let mut l = LISTEN.lock();
    let mut live = false;
    for p in l.iter_mut() {
        if p.start == 0 { continue; }
        if now_ns.saturating_sub(p.start) > LISTEN_NS {
            log(&format!("i2c-hid: {} input reports in {} s: {} (empty {}, errors {})", p.name, LISTEN_NS / 1_000_000_000, p.reports, p.empty, p.errs));
            p.start = 0;
            continue;
        }
        live = true;
        let mut buf = [0u8; 256];
        let n = p.max_in;
        match dw_i2c::xfer(p.bus, p.addr, &[p.input_reg as u8, (p.input_reg >> 8) as u8], &mut buf[..n]) {
            Ok(()) => {
                let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
                if len <= 2 || len > n { p.empty += 1; continue; }
                p.reports += 1;
                if p.logged < 4 {
                    p.logged += 1;
                    log(&format!("i2c-hid: {} input report #{} len={} {}", p.name, p.reports, len, hex(&buf[..len.min(24)])));
                }
            }
            Err(_) => p.errs += 1,
        }
    }
    live
}
