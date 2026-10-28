//! HID-over-I2C (spec 1.0) probe (x86_64), lane lapi2c phase 1.
//! Brings the LPSS I2C buses up, lists the ACPI slaves, and for every live
//! PNP0C50 slave reads the HID descriptor and the report descriptor and
//! counts input reports for a while. No input node, no Input Mode switch: the
//! default `i2c_hid=probe` changes no behaviour.
//! `touchpad=ps2|i2c|auto` (default ps2): `i2c` switches the Precision Touchpad to
//! Input Mode 3 and feeds an evdev MT node (lane lapptp); any failure puts the
//! pad back on PS/2. `touchpad.fail=1` forces that fallback 3 s after set-up.
//! Knobs: `i2c=0` (nothing), `i2c_hid=probe|0` , `i2c.debug=1` (hex-dump transfers).

use crate::dw_i2c;
use crate::hid_desc::{self, Descriptor};
use crate::hid_ptp::{self, Assembler, MouseLayout, Mt};
use crate::i2c_acpi::{self, log, I2cSlave};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;

const LISTEN_NS: u64 = 120_000_000_000;

/// PS/2 re-initialisation hook (set by the kernel; x86 psmouse::init).
static PS2_REINIT: AtomicUsize = AtomicUsize::new(0);
pub fn set_ps2_reinit(f: fn()) { PS2_REINIT.store(f as usize, Ordering::Release); }

const POLL_ACTIVE_NS: u64 = 8_000_000;
static POLL_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(10_000_000);
/// Delay the kernel thread should sleep between [`listen_once`] calls.
pub fn poll_interval_ns() -> u64 { POLL_NS.load(Ordering::Relaxed) }

/// An I2C-driven touchpad (touchpad=i2c, set-up committed).
struct Pad {
    ptp: hid_desc::Ptp,
    mouse: Option<MouseLayout>,
    uses_ids: bool,
    dev: u32,
    cmd_reg: u16,
    mt: Mt,
    asm: Assembler,
    err_run: u32,
    t0: u64,
    force_fail: bool,
    frames: u32,
    mode_confirmed: bool,
    resent: bool,
    mode_id: u8,
    mode_pl: Vec<u8>,
    data_reg: u16,
}

struct Listen {
    pad: Option<Pad>,
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

fn cmd_hdr(cmd_reg: u16, id: u8, ty: u8, op: u8) -> Vec<u8> {
    let mut v = alloc::vec![cmd_reg as u8, (cmd_reg >> 8) as u8];
    if id >= 15 { v.push((ty << 4) | 0x0F); v.push(op); v.push(id); } else { v.push((ty << 4) | id); v.push(op); }
    v
}

/// GET_REPORT(feature): the payload without the length word and id byte.
fn get_feature(bus: usize, addr: u16, cmd_reg: u16, data_reg: u16, id: u8, plen: usize) -> Result<Vec<u8>, u32> {
    let mut w = cmd_hdr(cmd_reg, id, 3, 2);
    w.extend_from_slice(&[data_reg as u8, (data_reg >> 8) as u8]);
    let extra = if id != 0 { 1 } else { 0 };
    let mut r = alloc::vec![0u8; 2 + extra + plen];
    dw_i2c::xfer(bus, addr, &w, &mut r)?;
    let n = u16::from_le_bytes([r[0], r[1]]) as usize;
    if n < 2 + extra + plen { return Err(5); }
    Ok(r[2 + extra..].to_vec())
}

fn set_feature(bus: usize, addr: u16, cmd_reg: u16, data_reg: u16, id: u8, pl: &[u8]) -> Result<(), u32> {
    let mut w = cmd_hdr(cmd_reg, id, 3, 3);
    w.extend_from_slice(&[data_reg as u8, (data_reg >> 8) as u8]);
    let n = 2 + if id != 0 { 1 } else { 0 } + pl.len();
    w.extend_from_slice(&[n as u8, (n >> 8) as u8]);
    if id != 0 { w.push(id); }
    w.extend_from_slice(pl);
    dw_i2c::xfer(bus, addr, &w, &mut [])
}

/// Plain input read (Linux i2c_hid_get_input): returns the LE16 length word.
fn read_input(bus: usize, addr: u16, in_reg: u16, buf: &mut [u8]) -> Result<usize, u32> {
    if word("i2c_hid.inreg", "") == Some("1") {
        dw_i2c::xfer(bus, addr, &[in_reg as u8, (in_reg >> 8) as u8], buf)?;
    } else {
        dw_i2c::xfer(bus, addr, &[], buf)?;
    }
    Ok(u16::from_le_bytes([buf[0], buf[1]]) as usize)
}

/// Read pending reports until the length word is <= 2. Returns (reports, errors).
fn drain(bus: usize, addr: u16, in_reg: u16, max_in: usize) -> (u32, u32) {
    let (mut r, mut e) = (0, 0);
    let mut buf = [0u8; 256];
    let n = max_in.clamp(2, 256);
    for _ in 0..8 {
        match read_input(bus, addr, in_reg, &mut buf[..n]) {
            Ok(len) if len > 2 => r += 1,
            Ok(_) => break,
            Err(_) => { e += 1; break; }
        }
    }
    (r, e)
}

/// After RESET: wait 100 ms, read until the 0x0000 sentinel, drain the rest.
fn reset_wait(bus: usize, addr: u16, in_reg: u16, max_in: usize) -> (bool, u32, u64) {
    spin_ms(100);
    let n = max_in.clamp(2, 256);
    let mut buf = [0u8; 256];
    let mut got = false;
    let mut ms = 100;
    while ms < 1000 {
        if let Ok(0) = read_input(bus, addr, in_reg, &mut buf[..n]) { got = true; break; }
        spin_ms(5);
        ms += 5;
    }
    let (r, _) = drain(bus, addr, in_reg, max_in);
    (got, r, ms)
}

fn power(bus: usize, addr: u16, cmd_reg: u16, on: bool) -> Result<(), u32> {
    dw_i2c::xfer(bus, addr, &[cmd_reg as u8, (cmd_reg >> 8) as u8, if on { 0 } else { 1 }, 0x08], &mut [])
}

/// touchpad=i2c set-up after the descriptors parsed. Err(true) = the pad was
/// touched (RESET sent) and needs the PS/2 fallback; Err(false) = left alone.
fn setup_pad(name: &str, bus: usize, s: &I2cSlave, d: &Descriptor, ids: (u16, u16), regs: (u16, u16, u16), max_in: usize) -> Result<Pad, (bool, String)> {
    let (vid, pid) = ids;
    let (in_reg, cmd_reg, data_reg) = regs;
    let dbg = word("touchpad.debug", "") == Some("1");
    let r = setup_inner(name, bus, s, d, vid, pid, in_reg, cmd_reg, data_reg, max_in, dbg);
    r
}

fn setup_inner(name: &str, bus: usize, s: &I2cSlave, d: &Descriptor, vid: u16, pid: u16, in_reg: u16, cmd_reg: u16, data_reg: u16, max_in: usize, dbg: bool) -> Result<Pad, (bool, String)> {
    let Some(ptp) = hid_desc::ptp(d) else { return Err((false, String::from("no touchpad collection"))) };
    let ok = ptp.fingers.iter().any(|f| f.x.is_some() && f.y.is_some());
    let Some(mode) = ptp.mode_feature.filter(|_| ok) else { return Err((false, String::from("no finger X/Y or Input Mode feature"))) };
    let title = format!("{} {:04X}:{:04X} Touchpad", name, vid, pid);
    let Some(dev) = evdev_server::register_input_device(evdev_server::DEVKIND_TOUCHPAD, &title, 0x18, vid, pid) else {
        return Err((false, String::from("evdev node")))
    };
    let fail = |why: String| { evdev_server::unregister_input_device(dev); (true, why) };
    if let Err(e) = power(bus, s.addr, cmd_reg, true) { return Err(fail(format!("SET_POWER ON {:#x}", e))); }
    spin_ms(1);
    if let Err(e) = dw_i2c::xfer(bus, s.addr, &[cmd_reg as u8, (cmd_reg >> 8) as u8, 0x00, 0x01], &mut []) { return Err(fail(format!("RESET {:#x}", e))); }
    if dbg { dw_i2c::DEBUG.store(true, Ordering::Relaxed); }
    let res = (|| -> Result<bool, String> {
        let (sent, rep, ms) = reset_wait(bus, s.addr, in_reg, max_in);
        log(&format!("i2c-hid: {} reset done in {} ms (sentinel {}), drained {}", name, ms, sent, rep));
        let pl = hid_ptp::config_report(d, mode.id);
        if let Err(e) = set_feature(bus, s.addr, cmd_reg, data_reg, mode.id, &pl) { return Err(format!("SET_REPORT mode {:#x}", e)); }
        spin_ms(10);
        let (r, _) = drain(bus, s.addr, in_reg, max_in);
        log(&format!("i2c-hid: {} SET_REPORT mode ok, drained {}", name, r));
        if word("touchpad.verify", "") == Some("0") {
            log(&format!("i2c-hid: {} GET_REPORT skipped (touchpad.verify=0)", name));
            return Ok(false);
        }
        let mut got = get_feature(bus, s.addr, cmd_reg, data_reg, mode.id, pl.len());
        for i in 0..5 {
            let Err(e) = got else { break };
            if i == 0 { log(&format!("i2c-hid: {} GET_REPORT mode first try {:#x}, retrying", name, e)); }
            spin_ms(10);
            drain(bus, s.addr, in_reg, max_in);
            got = get_feature(bus, s.addr, cmd_reg, data_reg, mode.id, pl.len());
        }
        match got {
            Ok(r) => {
                let v = hid_ptp::bits(&r, mode.bit, mode.size);
                log(&format!("i2c-hid: {} touchpad Input Mode read back {} (feature id {})", name, v, mode.id));
                if v != 3 { return Err(format!("Input Mode reads {}", v)); }
                Ok(true)
            }
            Err(e) => {
                let mc = ptp.max_contacts_feature.map(|m| (m.id, hid_ptp::config_report(d, m.id).len()));
                let Some((id, plen)) = mc else { return Err(format!("GET_REPORT mode {:#x}, no maxc feature", e)) };
                match get_feature(bus, s.addr, cmd_reg, data_reg, id, plen) {
                    Ok(v) => { log(&format!("i2c-hid: {} GET mode NAK {:#x}, GET maxc (id {}) ok {}", name, e, id, hex(&v))); Ok(false) }
                    Err(e2) => { log(&format!("i2c-hid: {} GET mode NAK {:#x}, GET maxc (id {}) NAK {:#x}", name, e, id, e2)); Err(format!("GET_REPORT mode {:#x}, maxc {:#x}", e, e2)) }
                }
            }
        }
    })();
    if dbg { dw_i2c::DEBUG.store(word("i2c.debug", "") == Some("1"), Ordering::Relaxed); }
    let confirmed = match res { Ok(c) => c, Err(w) => return Err(fail(w)) };
    extern "C" { fn arch_monotonic_ns() -> u64; }
    Ok(Pad { mouse: hid_ptp::mouse_layout(d), ptp, uses_ids: d.uses_report_ids, dev, cmd_reg, mt: Mt::default(), asm: Assembler::default(),
        err_run: 0, t0: unsafe { arch_monotonic_ns() }, force_fail: word("touchpad.fail", "") == Some("1"), frames: 0,
        mode_confirmed: confirmed, resent: false, mode_id: mode.id, mode_pl: hid_ptp::config_report(d, mode.id), data_reg })
}

/// Put the pad back on PS/2: SLEEP the I2C side, drop the node, re-init psmouse.
fn fallback(p: &mut Pad, bus: usize, addr: u16, why: &str) {
    log(&format!("touchpad: i2c failed ({}), back to ps2", why));
    let _ = power(bus, addr, p.cmd_reg, false);
    evdev_server::unregister_input_device(p.dev);
    let f = PS2_REINIT.load(Ordering::Acquire);
    if f != 0 { let f: fn() = unsafe { core::mem::transmute(f) }; f(); }
}

fn probe_slave(s: &I2cSlave, bus: usize, reset: bool, tp_i2c: bool) -> Option<Listen> {
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
        let (acked, _, _) = reset_wait(bus, s.addr, in_reg, max_in.clamp(2, 256));
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
    let mut pad = None;
    if tp_i2c && hid_desc::ptp(&parsed).is_some() {
        match setup_pad(&name, bus, s, &parsed, (w(20), w(22)), (in_reg, cmd_reg, w(18)), max_in.clamp(2, 256)) {
            Ok(p) => { log(&format!("touchpad: i2c active ({}), polling every 8 ms", name)); POLL_NS.store(POLL_ACTIVE_NS, Ordering::Relaxed); pad = Some(p); }
            Err((touched, why)) => {
                log(&format!("touchpad: i2c failed ({}), back to ps2", why));
                if touched {
                    let _ = power(bus, s.addr, cmd_reg, false);
                    let f = PS2_REINIT.load(Ordering::Acquire);
                    if f != 0 { let f: fn() = unsafe { core::mem::transmute(f) }; f(); }
                }
            }
        }
    }
    Some(Listen { pad, name, bus, addr: s.addr, input_reg: in_reg, max_in: max_in.clamp(2, 256), start: unsafe { arch_monotonic_ns() },
        reports: 0, empty: 0, errs: 0, logged: 0 })
}

/// Probe everything. Returns true when a listen thread should poll
/// [`listen_once`]. Task context, after ACPI power init.
pub fn init(cmdline: &str) -> bool {
    if word("i2c", cmdline) == Some("0") { return false; }
    dw_i2c::DEBUG.store(word("i2c.debug", cmdline) == Some("1"), Ordering::Relaxed);
    let mode = word("i2c_hid", cmdline).unwrap_or("probe");
    let tp = word("touchpad", cmdline).unwrap_or("ps2");
    if tp == "auto" { log("touchpad=auto: not enabled yet, using ps2"); }
    let tp_i2c = tp == "i2c";
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
    if mode == "0" && !tp_i2c { return false; }
    let mut l = LISTEN.lock();
    for s in sc.slaves.iter().filter(|s| s.sta & 0xB == 0xB && s.hid_desc_reg.is_some()) {
        match dw_i2c::bus_for_adr(s.ctrl_adr) {
            Some(bus) => { if let Some(p) = probe_slave(s, bus, mode == "probe+reset", tp_i2c) { l.push(p); } }
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
        if p.pad.is_some() { pad_poll(p, now_ns); live = true; continue; }
        if p.start == 0 { continue; }
        if now_ns.saturating_sub(p.start) > LISTEN_NS {
            log(&format!("i2c-hid: {} input reports in {} s: {} (empty {}, errors {})", p.name, LISTEN_NS / 1_000_000_000, p.reports, p.empty, p.errs));
            p.start = 0;
            continue;
        }
        live = true;
        let mut buf = [0u8; 256];
        let n = p.max_in;
        match read_input(p.bus, p.addr, p.input_reg, &mut buf[..n]) {
            Ok(len) => {
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

/// One poll of an I2C touchpad: drain pending input reports.
fn pad_poll(l: &mut Listen, now_ns: u64) {
    let mut buf = [0u8; 256];
    let n = l.max_in;
    let mut why: Option<String> = None;
    let pad = l.pad.as_mut().unwrap();
    if pad.force_fail && now_ns.saturating_sub(pad.t0) > 3_000_000_000 { why = Some(String::from("touchpad.fail=1")); }
    for _ in 0..4 {
        if why.is_some() { break; }
        match read_input(l.bus, l.addr, l.input_reg, &mut buf[..n]) {
            Ok(len) => {
                pad.err_run = 0;
                if len <= 2 || len > n { l.empty += 1; break; }
                l.reports += 1;
                let rep = &buf[2..len];
                if let Some((_, f)) = hid_ptp::parse_report(&pad.ptp, pad.uses_ids, rep) {
                    if !pad.mode_confirmed { pad.mode_confirmed = true; log("touchpad: PTP reports confirmed"); }
                    if let Some(f) = pad.asm.push(f, pad.ptp.fingers.len()) {
                        let mut ev = Vec::new();
                        pad.mt.frame(&f, &mut ev);
                        pad.frames += 1;
                        for (t, c, v) in ev { evdev_server::push_event(pad.dev, t, c, v); }
                    }
                } else if let Some(m) = &pad.mouse {
                    if !pad.mode_confirmed {
                        if !pad.resent {
                            log("touchpad: pad still in mouse mode, resending SET_REPORT");
                            pad.resent = true;
                            let _ = set_feature(l.bus, l.addr, pad.cmd_reg, pad.data_reg, pad.mode_id, &pad.mode_pl);
                        }
                    }
                    if let Some((dx, dy, wh, b)) = hid_ptp::parse_mouse(m, pad.uses_ids, rep) {
                        crate::rel_pointer::report(crate::rel_pointer::SRC_STICK, dx, dy, wh * 120, 0, b);
                    }
                }
            }
            Err(_) => {
                l.errs += 1;
                pad.err_run += 1;
                if pad.err_run >= 20 { why = Some(String::from("20 consecutive transfer errors")); }
                break;
            }
        }
    }
    if let Some(w) = why {
        let mut p = l.pad.take().unwrap();
        fallback(&mut p, l.bus, l.addr, &w);
        POLL_NS.store(10_000_000, Ordering::Relaxed);
        l.start = 0;
    }
}
