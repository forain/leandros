//! ACPI discovery of I2C slaves under the LPSS I2C controllers (x86_64).
//! Read-only apart from the optional `_PS0` of live devices; everything runs
//! inside one `acpi_power::with_ns` (task context). Lane lapi2c.

use crate::acpi_power::aml::{Ns, Host, V};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

extern "C" {
    fn serial_print_str_raw(s: *const u8, len: usize);
}

pub fn log(msg: &str) {
    let s = format!("[I2C] {}\n", msg);
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
}

#[derive(Clone, Debug, Default)]
pub struct I2cSlave {
    pub path: String,
    pub hid: String,
    pub cid: String,
    /// `_ADR` of the parent controller node (0x0015_0000 = I2C0).
    pub ctrl_adr: u32,
    pub ctrl_name: String,
    pub addr: u16,
    pub ten_bit: bool,
    pub speed_hz: u32,
    /// (controller path, pin, flags)
    pub gpio: Option<(String, u16, u16)>,
    pub irq: Option<u32>,
    pub hid_desc_reg: Option<u16>,
    pub sta: u64,
    pub ps0_ran: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Timing {
    pub ctrl_adr: u32,
    pub ss: Option<[u32; 3]>,
    pub fs: Option<[u32; 3]>,
}

pub struct Scan {
    pub slaves: Vec<I2cSlave>,
    pub timings: Vec<Timing>,
}

/// Decode a compressed EISA id integer (`PNP0C50`).
pub fn eisa_str(x: u64) -> String {
    let v = (x as u32).swap_bytes();
    let c = |s: u32| (((v >> s) & 0x1F) as u8 + 0x40) as char;
    format!("{}{}{}{:04X}", c(26), c(21), c(16), v & 0xFFFF)
}

fn id_strings(v: &V) -> Vec<String> {
    let one = |v: &V| -> Option<String> {
        match v {
            V::Int(i) => Some(eisa_str(*i)),
            o => o.as_bytes().map(|b| String::from_utf8_lossy(&b).into_owned()),
        }
    };
    match v {
        V::Pkg(p) => p.borrow().iter().filter_map(|e| one(e)).collect(),
        o => one(o).into_iter().collect(),
    }
}

fn cstr(b: &[u8], off: usize) -> String {
    if off >= b.len() { return String::new(); }
    let e = b[off..].iter().position(|&c| c == 0).map_or(b.len(), |p| off + p);
    String::from_utf8_lossy(&b[off..e]).into_owned()
}

fn u16le(b: &[u8], o: usize) -> u16 { b.get(o..o + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]])) }
fn u32le(b: &[u8], o: usize) -> u32 { b.get(o..o + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])) }

/// Walk a `_CRS` buffer and fill the connection fields of `s`.
pub fn parse_crs(b: &[u8], s: &mut I2cSlave) {
    let mut i = 0;
    while i < b.len() {
        let t = b[i];
        if t & 0x80 == 0 {
            let len = (t & 7) as usize;
            if (t >> 3) & 0xF == 0xF { break; }
            i += 1 + len;
            continue;
        }
        let len = u16le(b, i + 1) as usize;
        let d = match b.get(i..i + 3 + len) { Some(d) => d, None => break };
        match t {
            0x8E if d.len() >= 18 && d[5] == 1 => {
                s.ten_bit = u16le(d, 7) & 1 != 0;
                s.speed_hz = u32le(d, 12);
                s.addr = u16le(d, 16);
                let src = cstr(d, 18);
                if !src.is_empty() { s.ctrl_name = src; }
            }
            0x8C if d.len() >= 19 && d[4] == 0 && s.gpio.is_none() => {
                let flags = u16le(d, 7);
                let pin_off = u16le(d, 14) as usize;
                let src_off = u16le(d, 17) as usize;
                s.gpio = Some((cstr(d, src_off), u16le(d, pin_off), flags));
            }
            0x89 if d.len() >= 9 && s.irq.is_none() => s.irq = Some(u32le(d, 5)),
            _ => {}
        }
        i += 3 + len;
    }
}

fn adr_of(ns: &mut Ns, h: &mut dyn Host, n: usize) -> Option<u32> {
    ns.eval_child(h, n, "_ADR", &[]).and_then(|v| v.as_int()).map(|a| a as u32)
}

fn is_lpss_adr(a: u32) -> bool {
    (0x0015_0000..=0x0015_0003).contains(&a) || (0x0019_0000..=0x0019_0002).contains(&a)
}

fn timing_pkg(ns: &mut Ns, h: &mut dyn Host, ctrl: usize, name: &str) -> Option<[u32; 3]> {
    let v = ns.eval_child(h, ctrl, name, &[])?;
    let el = v.elements()?;
    if el.len() < 3 { return None; }
    let mut out = [0u32; 3];
    for k in 0..3 { out[k] = ns.int_of(h, &el[k])? as u32; }
    Some(out)
}

const HID_I2C_DSM: [u8; 16] = [0xf7, 0xf6, 0xdf, 0x3c, 0x67, 0x42, 0x55, 0x45, 0xad, 0x05, 0xb3, 0x0a, 0x3d, 0x89, 0x38, 0xde];

/// Scan the namespace for children of the LPSS I2C controllers. `run_ps0`
/// evaluates `_PS0` of the controller and of every live device.
pub fn scan(run_ps0: bool) -> Option<Scan> {
    crate::acpi_power::with_ns(|ns, h| {
        let mut out = Scan { slaves: Vec::new(), timings: Vec::new() };
        let mut ctrl_done: Vec<usize> = Vec::new();
        for n in 1..ns.len() {
            if !ns.is_real_device(n) { continue; }
            let p = ns.parent(n);
            if p == 0 || !ns.is_real_device(p) { continue; }
            let Some(adr) = adr_of(ns, h, p) else { continue };
            if !is_lpss_adr(adr) { continue; }
            if !ctrl_done.contains(&p) {
                ctrl_done.push(p);
                out.timings.push(Timing {
                    ctrl_adr: adr,
                    ss: timing_pkg(ns, h, p, "SSCN"),
                    fs: timing_pkg(ns, h, p, "FMCN"),
                });
            }
            let mut s = I2cSlave { path: ns.path(n), ctrl_adr: adr, sta: 0xF, ..Default::default() };
            s.ctrl_name = ns.path(p);
            if let Some(v) = ns.eval_child(h, n, "_STA", &[]) { s.sta = v.as_int().unwrap_or(0); }
            if let Some(v) = ns.eval_child(h, n, "_HID", &[]) { s.hid = id_strings(&v).join(","); }
            if let Some(v) = ns.eval_child(h, n, "_CID", &[]) { s.cid = id_strings(&v).join(","); }
            let live = s.sta & 0xB == 0xB;
            if live {
                if run_ps0 {
                    if !ctrl_done.contains(&(usize::MAX - p)) {
                        ctrl_done.push(usize::MAX - p);
                        let _ = ns.eval_child(h, p, "_PS0", &[]);
                    }
                    s.ps0_ran = ns.eval_child(h, n, "_PS0", &[]).is_some();
                }
                if let Some(V::Buf(b)) = ns.eval_child(h, n, "_CRS", &[]) {
                    let b = b.borrow().clone();
                    parse_crs(&b, &mut s);
                }
                let is_hid = |x: &str| x.split(',').any(|t| t == "PNP0C50" || t == "ACPI0C50");
                if is_hid(&s.hid) || is_hid(&s.cid) {
                    let args = [V::buf(HID_I2C_DSM.to_vec()), V::Int(1), V::Int(1), V::pkg(Vec::new())];
                    if let Some(v) = ns.eval_child(h, n, "_DSM", &args) { s.hid_desc_reg = v.as_int().map(|r| r as u16); }
                }
            }
            out.slaves.push(s);
        }
        out
    })
}

/// One log line per slave.
pub fn describe(s: &I2cSlave) -> String {
    let mut l = format!("i2c-acpi: {} hid={} cid={} sta={:#x} ctrl={:#x}", s.path, s.hid, s.cid, s.sta, s.ctrl_adr);
    if s.sta & 0xB == 0xB {
        l += &format!(" addr={:#x}{} speed={}", s.addr, if s.ten_bit { "(10b)" } else { "" }, s.speed_hz);
        if let Some((src, pin, fl)) = &s.gpio { l += &format!(" gpio={}:pin{} flags={:#x}", src, pin, fl); }
        if let Some(i) = s.irq { l += &format!(" irq={}", i); }
        if let Some(r) = s.hid_desc_reg { l += &format!(" desc={:#x}", r); }
    }
    l
}
