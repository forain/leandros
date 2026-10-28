//! HID report-descriptor parser (pure logic, alloc only; host-tested by
//! `drivers/hid-desc-tests`). Keeps the item kind (Input/Output/Feature),
//! logical and physical extents, units and the logical-collection link, which
//! is what Precision Touchpad (PTP) extraction needs. Lane lapi2c.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

pub const KIND_INPUT: u8 = 0;
pub const KIND_OUTPUT: u8 = 1;
pub const KIND_FEATURE: u8 = 2;

/// Usage constants as `(page << 16) | usage`.
pub const APP_TOUCHPAD: u32 = 0x000D_0005;
pub const APP_MOUSE: u32 = 0x0001_0002;
pub const APP_CONFIG: u32 = 0x000D_000E;
pub const U_TIP: u32 = 0x000D_0042;
pub const U_CONFIDENCE: u32 = 0x000D_0047;
pub const U_CONTACT_ID: u32 = 0x000D_0051;
pub const U_CONTACT_COUNT: u32 = 0x000D_0054;
pub const U_SCAN_TIME: u32 = 0x000D_0056;
pub const U_INPUT_MODE: u32 = 0x000D_0052;
pub const U_MAX_CONTACTS: u32 = 0x000D_0055;
pub const U_X: u32 = 0x0001_0030;
pub const U_Y: u32 = 0x0001_0031;
pub const U_BUTTON1: u32 = 0x0009_0001;

#[derive(Clone, Debug)]
pub struct Field {
    pub kind: u8,
    pub report_id: u8,
    /// Bit offset inside the report payload (after the report id byte).
    pub bit: u32,
    pub size: u32,
    pub count: u32,
    pub flags: u32,
    pub lmin: i32,
    pub lmax: i32,
    pub pmin: i32,
    pub pmax: i32,
    pub unit: u32,
    pub unit_exp: i32,
    pub usages: Vec<u32>,
    /// Application collection usage (page<<16 | usage), 0 if none.
    pub app: u32,
    /// Index of the innermost Logical collection, 0xFFFF if none.
    pub link: u16,
}

#[derive(Clone, Debug, Default)]
pub struct Descriptor {
    pub fields: Vec<Field>,
    /// Application collections in order of appearance.
    pub apps: Vec<u32>,
    pub uses_report_ids: bool,
}

#[derive(Clone, Copy, Default)]
struct Global {
    page: u32,
    lmin: i32,
    lmax: i32,
    pmin: i32,
    pmax: i32,
    unit: u32,
    unit_exp: i32,
    size: u32,
    count: u32,
    id: u8,
}

fn sx(v: u32, n: usize) -> i32 {
    match n {
        1 => v as u8 as i8 as i32,
        2 => v as u16 as i16 as i32,
        _ => v as i32,
    }
}

fn nibble_exp(v: u32) -> i32 {
    let n = (v & 0xF) as i32;
    if n > 7 { n - 16 } else { n }
}

pub fn parse(d: &[u8]) -> Descriptor {
    let mut out = Descriptor::default();
    let mut g = Global::default();
    let mut stack: Vec<Global> = Vec::new();
    let mut usages: Vec<u32> = Vec::new();
    let (mut umin, mut umax) = (None::<u32>, None::<u32>);
    // (collection type, link id, app usage)
    let mut cols: Vec<(u8, u16, u32)> = Vec::new();
    let mut next_link: u16 = 0;
    // running bit offset per (kind, report id)
    let mut offs: Vec<(u8, u8, u32)> = Vec::new();
    let mut i = 0;
    while i < d.len() {
        let b = d[i];
        if b == 0xFE {
            let l = *d.get(i + 1).unwrap_or(&0) as usize;
            i += 3 + l;
            continue;
        }
        let sz = match b & 3 { 3 => 4, n => n as usize };
        if i + 1 + sz > d.len() { break; }
        let mut v: u32 = 0;
        for k in 0..sz { v |= (d[i + 1 + k] as u32) << (8 * k); }
        let ty = (b >> 2) & 3;
        let tag = b >> 4;
        i += 1 + sz;
        match ty {
            0 => match tag {
                0x8 | 0x9 | 0xB => {
                    let kind = match tag { 0x8 => KIND_INPUT, 0x9 => KIND_OUTPUT, _ => KIND_FEATURE };
                    let mut us = usages.clone();
                    if us.is_empty() {
                        if let (Some(a), Some(z)) = (umin, umax) {
                            let (a, z) = (a & 0xFFFF, z & 0xFFFF);
                            if z >= a && z - a < 1024 {
                                for u in a..=z { us.push((g.page << 16) | u); }
                            }
                        }
                    }
                    let slot = match offs.iter().position(|o| o.0 == kind && o.1 == g.id) {
                        Some(p) => p,
                        None => { offs.push((kind, g.id, 0)); offs.len() - 1 }
                    };
                    let bit = offs[slot].2;
                    offs[slot].2 += g.size * g.count;
                    let app = cols.iter().find(|c| c.0 == 1).map(|c| c.2).unwrap_or(0);
                    let link = cols.iter().rev().find(|c| c.0 == 2).map(|c| c.1).unwrap_or(0xFFFF);
                    let (mut lmax, lmin) = (g.lmax, g.lmin);
                    if lmax < lmin && lmin >= 0 && g.size < 32 { lmax = ((g.lmax as u32) & ((1u32 << g.size) - 1)) as i32; }
                    out.fields.push(Field {
                        kind, report_id: g.id, bit, size: g.size, count: g.count, flags: v,
                        lmin, lmax, pmin: g.pmin, pmax: g.pmax, unit: g.unit, unit_exp: g.unit_exp,
                        usages: us, app, link,
                    });
                    usages.clear();
                    umin = None;
                    umax = None;
                }
                0xA => {
                    let u = usages.first().copied().unwrap_or(0);
                    let mut lk = 0xFFFF;
                    if v as u8 == 2 { lk = next_link; next_link += 1; }
                    if v as u8 == 1 { out.apps.push(u); }
                    cols.push((v as u8, lk, u));
                    usages.clear();
                    umin = None;
                    umax = None;
                }
                0xC => {
                    cols.pop();
                    usages.clear();
                    umin = None;
                    umax = None;
                }
                _ => {}
            },
            1 => match tag {
                0x0 => g.page = v,
                0x1 => g.lmin = sx(v, sz),
                0x2 => g.lmax = sx(v, sz),
                0x3 => g.pmin = sx(v, sz),
                0x4 => g.pmax = sx(v, sz),
                0x5 => g.unit_exp = nibble_exp(v),
                0x6 => g.unit = v,
                0x7 => g.size = v,
                0x8 => { g.id = v as u8; out.uses_report_ids = true; }
                0x9 => g.count = v,
                0xA => stack.push(g),
                0xB => { if let Some(p) = stack.pop() { g = p; } }
                _ => {}
            },
            2 => {
                let full = if sz == 4 { v } else { (g.page << 16) | (v & 0xFFFF) };
                match tag {
                    0x0 => usages.push(full),
                    0x1 => umin = Some(full),
                    0x2 => umax = Some(full),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    out
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Loc {
    pub id: u8,
    pub bit: u32,
    pub size: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Axis {
    pub loc: Loc,
    pub lmin: i32,
    pub lmax: i32,
    pub pmin: i32,
    pub pmax: i32,
    pub unit: u32,
    pub unit_exp: i32,
}

#[derive(Clone, Debug, Default)]
pub struct Finger {
    pub tip: Option<Loc>,
    pub conf: Option<Loc>,
    pub cid: Option<Loc>,
    pub x: Option<Axis>,
    pub y: Option<Axis>,
}

#[derive(Clone, Debug, Default)]
pub struct Ptp {
    pub input_id: u8,
    pub fingers: Vec<Finger>,
    pub contact_count: Option<Loc>,
    pub button1: Option<Loc>,
    pub scan_time: Option<Loc>,
    pub x: Option<Axis>,
    pub y: Option<Axis>,
    /// Feature report carrying Input Mode (0x000D0052).
    pub mode_feature: Option<Loc>,
    /// Feature report carrying Contact Count Maximum (0x000D0055).
    pub max_contacts_feature: Option<Loc>,
}

fn axis(f: &Field, loc: Loc) -> Axis {
    Axis { loc, lmin: f.lmin, lmax: f.lmax, pmin: f.pmin, pmax: f.pmax, unit: f.unit, unit_exp: f.unit_exp }
}

/// Extract the Precision Touchpad layout, `None` when there is no touchpad
/// application collection.
pub fn ptp(d: &Descriptor) -> Option<Ptp> {
    if !d.apps.contains(&APP_TOUCHPAD) { return None; }
    let mut p = Ptp::default();
    let mut links: Vec<u16> = Vec::new();
    for f in d.fields.iter().filter(|f| f.app == APP_TOUCHPAD || (f.app == APP_CONFIG && f.kind == KIND_FEATURE)) {
        for (i, &u) in f.usages.iter().enumerate() {
            let loc = Loc { id: f.report_id, bit: f.bit + i as u32 * f.size, size: f.size };
            match f.kind {
                KIND_INPUT => {
                    p.input_id = f.report_id;
                    match u {
                        U_TIP | U_CONFIDENCE | U_CONTACT_ID | U_X | U_Y if f.link != 0xFFFF => {
                            let idx = match links.iter().position(|&l| l == f.link) {
                                Some(k) => k,
                                None => { links.push(f.link); p.fingers.push(Finger::default()); links.len() - 1 }
                            };
                            let fg = &mut p.fingers[idx];
                            match u {
                                U_TIP => fg.tip = Some(loc),
                                U_CONFIDENCE => fg.conf = Some(loc),
                                U_CONTACT_ID => fg.cid = Some(loc),
                                U_X => fg.x = Some(axis(f, loc)),
                                _ => fg.y = Some(axis(f, loc)),
                            }
                        }
                        U_CONTACT_COUNT => p.contact_count = Some(loc),
                        U_SCAN_TIME => p.scan_time = Some(loc),
                        U_BUTTON1 => p.button1 = Some(loc),
                        _ => {}
                    }
                }
                KIND_FEATURE => match u {
                    U_INPUT_MODE => p.mode_feature = Some(loc),
                    U_MAX_CONTACTS => p.max_contacts_feature = Some(loc),
                    _ => {}
                },
                _ => {}
            }
        }
    }
    p.x = p.fingers.iter().find_map(|f| f.x);
    p.y = p.fingers.iter().find_map(|f| f.y);
    Some(p)
}

/// One-line human summary for the kernel log.
pub fn summary(d: &Descriptor) -> String {
    let mut s = String::from("apps=[");
    for (i, a) in d.apps.iter().enumerate() {
        if i > 0 { s.push(' '); }
        let nm = match *a { APP_TOUCHPAD => " touchpad", APP_MOUSE => " mouse", 0x000D_0004 => " touchscreen", 0x0001_0006 => " keyboard", _ => "" };
        s += &format!("{:02x}:{:02x}{}", a >> 16, a & 0xFFFF, nm);
    }
    s.push(']');
    if let Some(p) = ptp(d) {
        s += &format!(" ptp fingers={} in_id={}", p.fingers.len(), p.input_id);
        if let Some(x) = p.x { s += &format!(" x={}..{} (phys {}..{} unit={:#x} exp={})", x.lmin, x.lmax, x.pmin, x.pmax, x.unit, x.unit_exp); }
        if let Some(y) = p.y { s += &format!(" y={}..{} (phys {}..{})", y.lmin, y.lmax, y.pmin, y.pmax); }
        match p.mode_feature { Some(m) => s += &format!(" mode_feature id={} bit={}", m.id, m.bit), None => s += " mode_feature=none" }
        if let Some(m) = p.max_contacts_feature { s += &format!(" maxc_feature id={}", m.id); }
    }
    s
}
