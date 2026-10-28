//! Precision Touchpad input-report decoding and evdev MT (protocol B) frame
//! synthesis, on top of the layouts from `hid_desc`. Pure: host-tested by
//! drivers/hid-desc-tests. Lane lapptp.

use crate::hid_desc::{self, Descriptor, Loc, Ptp, APP_MOUSE, KIND_INPUT};
use alloc::vec::Vec;

pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_ABS: u16 = 3;
pub const ABS_X: u16 = 0;
pub const ABS_Y: u16 = 1;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_TOUCH: u16 = 0x14a;
const TOOLS: [u16; 5] = [0x145, 0x14d, 0x14e, 0x14f, 0x148];
pub const MAX_SLOTS: usize = 5;

/// `size` (<= 32) bits at `bit` of a report payload (little-endian bit order).
pub fn bits(p: &[u8], bit: u32, size: u32) -> u32 {
    let mut v = 0u32;
    for i in 0..size.min(32) {
        let b = bit + i;
        let byte = p.get((b / 8) as usize).copied().unwrap_or(0);
        if (byte >> (b % 8)) & 1 != 0 { v |= 1 << i; }
    }
    v
}

pub fn sbits(p: &[u8], bit: u32, size: u32) -> i32 {
    let v = bits(p, bit, size);
    if size > 0 && size < 32 && v & (1 << (size - 1)) != 0 { (v | !((1u32 << size) - 1)) as i32 } else { v as i32 }
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Contact {
    pub cid: u8,
    pub tip: bool,
    pub conf: bool,
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Frame {
    pub contacts: Vec<Contact>,
    pub count: u8,
    pub button: bool,
}

/// Split an input report (without the 2-byte length) into (id, payload).
pub fn split_id(uses_ids: bool, rep: &[u8]) -> Option<(u8, &[u8])> {
    if uses_ids { rep.split_first().map(|(i, r)| (*i, r)) } else { Some((0, rep)) }
}

/// Decode one PTP report. `rep` starts at the report id (when the descriptor
/// uses ids). None when the id is not the touchpad's.
pub fn parse_report(p: &Ptp, uses_ids: bool, rep: &[u8]) -> Option<(u8, Frame)> {
    let (id, pl) = split_id(uses_ids, rep)?;
    if id != p.input_id { return None; }
    let mut f = Frame::default();
    for fg in &p.fingers {
        let (Some(x), Some(y)) = (fg.x, fg.y) else { continue };
        let tip = fg.tip.map(|l| bits(pl, l.bit, l.size) != 0).unwrap_or(true);
        let conf = fg.conf.map(|l| bits(pl, l.bit, l.size) != 0).unwrap_or(true);
        let cid = fg.cid.map(|l| bits(pl, l.bit, l.size) as u8).unwrap_or(0);
        let rd = |a: &hid_desc::Axis| if a.lmin < 0 { sbits(pl, a.loc.bit, a.loc.size) } else { bits(pl, a.loc.bit, a.loc.size) as i32 };
        f.contacts.push(Contact { cid, tip, conf, x: rd(&x), y: rd(&y) });
    }
    f.count = p.contact_count.map(|l| bits(pl, l.bit, l.size) as u8).unwrap_or(f.contacts.len() as u8);
    f.button = p.button1.map(|l| bits(pl, l.bit, l.size) != 0).unwrap_or(false);
    Some((id, f))
}

/// Hybrid mode: a frame with more contacts than one report holds spans
/// several reports; the first carries the total count, the rest carry 0.
#[derive(Default)]
pub struct Assembler {
    pending: Vec<Contact>,
    expect: usize,
    button: bool,
}

impl Assembler {
    /// Feed a report; returns a complete frame when one is ready.
    pub fn push(&mut self, f: Frame, per_report: usize) -> Option<Frame> {
        if f.count > 0 {
            self.pending.clear();
            self.expect = f.count as usize;
            self.button = f.button;
        } else if self.expect == 0 {
            // a report outside any hybrid frame stands alone
            return Some(f);
        }
        let take = (self.expect - self.pending.len().min(self.expect)).min(per_report);
        self.pending.extend(f.contacts.iter().copied().take(take));
        if self.pending.len() >= self.expect {
            let c = core::mem::take(&mut self.pending);
            self.expect = 0;
            return Some(Frame { count: c.len() as u8, contacts: c, button: self.button });
        }
        None
    }
}

/// Protocol-B state: which slot holds which tracking id.
pub struct Mt {
    tid: [i32; MAX_SLOTS],
    pos: [(i32, i32); MAX_SLOTS],
    next: i32,
    tool: usize,
    btn: bool,
    touching: bool,
}

impl Default for Mt {
    fn default() -> Self { Mt { tid: [-1; MAX_SLOTS], pos: [(0, 0); MAX_SLOTS], next: 0, tool: usize::MAX, btn: false, touching: false } }
}

impl Mt {
    /// Turn a frame into evdev events (type, code, value), ending in SYN_REPORT.
    /// Contacts without tip or without confidence (palm) count as lifted.
    pub fn frame(&mut self, f: &Frame, out: &mut Vec<(u16, u16, i32)>) {
        let mut present = [None::<(i32, i32)>; MAX_SLOTS];
        for c in &f.contacts {
            if c.tip && c.conf { present[c.cid as usize % MAX_SLOTS] = Some((c.x, c.y)); }
        }
        for s in 0..MAX_SLOTS {
            match (present[s], self.tid[s] >= 0) {
                (Some((x, y)), false) => {
                    out.push((EV_ABS, ABS_MT_SLOT, s as i32));
                    self.tid[s] = self.next;
                    self.next = (self.next + 1) & 0xFFFF;
                    out.push((EV_ABS, ABS_MT_TRACKING_ID, self.tid[s]));
                    out.push((EV_ABS, ABS_MT_POSITION_X, x));
                    out.push((EV_ABS, ABS_MT_POSITION_Y, y));
                    self.pos[s] = (x, y);
                }
                (Some((x, y)), true) => {
                    if self.pos[s] != (x, y) {
                        out.push((EV_ABS, ABS_MT_SLOT, s as i32));
                        if self.pos[s].0 != x { out.push((EV_ABS, ABS_MT_POSITION_X, x)); }
                        if self.pos[s].1 != y { out.push((EV_ABS, ABS_MT_POSITION_Y, y)); }
                        self.pos[s] = (x, y);
                    }
                }
                (None, true) => {
                    out.push((EV_ABS, ABS_MT_SLOT, s as i32));
                    out.push((EV_ABS, ABS_MT_TRACKING_ID, -1));
                    self.tid[s] = -1;
                }
                (None, false) => {}
            }
        }
        // Single-touch emulation follows the oldest (lowest tracking id) contact.
        let n = self.tid.iter().filter(|&&t| t >= 0).count();
        let oldest = (0..MAX_SLOTS).filter(|&s| self.tid[s] >= 0).min_by_key(|&s| self.tid[s]);
        let touching = n > 0;
        if touching != self.touching { out.push((EV_KEY, BTN_TOUCH, touching as i32)); self.touching = touching; }
        let tool = if n == 0 { usize::MAX } else { n.min(5) - 1 };
        if tool != self.tool {
            if self.tool < 5 { out.push((EV_KEY, TOOLS[self.tool], 0)); }
            if tool < 5 { out.push((EV_KEY, TOOLS[tool], 1)); }
            self.tool = tool;
        }
        if let Some(s) = oldest {
            out.push((EV_ABS, ABS_X, self.pos[s].0));
            out.push((EV_ABS, ABS_Y, self.pos[s].1));
        }
        if f.button != self.btn { out.push((EV_KEY, BTN_LEFT, f.button as i32)); self.btn = f.button; }
        out.push((EV_SYN, 0, 0));
    }

    pub fn active(&self) -> usize { self.tid.iter().filter(|&&t| t >= 0).count() }
}

/// Mouse application collection (the pad's non-PTP output or the pointing stick).
#[derive(Clone, Debug, Default)]
pub struct MouseLayout {
    pub id: u8,
    pub buttons: Vec<Loc>,
    pub x: Option<Loc>,
    pub y: Option<Loc>,
    pub wheel: Option<Loc>,
}

pub fn mouse_layout(d: &Descriptor) -> Option<MouseLayout> {
    if !d.apps.contains(&APP_MOUSE) { return None; }
    let mut m = MouseLayout::default();
    let mut seen = false;
    for f in d.fields.iter().filter(|f| f.app == APP_MOUSE && f.kind == KIND_INPUT) {
        for (i, &u) in f.usages.iter().enumerate() {
            let loc = Loc { id: f.report_id, bit: f.bit + i as u32 * f.size, size: f.size };
            match u {
                0x0009_0001..=0x0009_0008 => { m.buttons.push(loc); seen = true; }
                hid_desc::U_X => { m.x = Some(loc); m.id = f.report_id; }
                hid_desc::U_Y => m.y = Some(loc),
                0x0001_0038 => m.wheel = Some(loc),
                _ => {}
            }
        }
    }
    if seen || m.x.is_some() { if m.x.is_some() { m.id = m.x.unwrap().id; } else if let Some(b) = m.buttons.first() { m.id = b.id; } Some(m) } else { None }
}

/// (dx, dy, wheel, buttons bitmap) of a mouse report; None for another id.
pub fn parse_mouse(m: &MouseLayout, uses_ids: bool, rep: &[u8]) -> Option<(i32, i32, i32, u8)> {
    let (id, pl) = split_id(uses_ids, rep)?;
    if id != m.id { return None; }
    let rd = |l: &Option<Loc>| l.map(|l| sbits(pl, l.bit, l.size)).unwrap_or(0);
    let mut b = 0u8;
    for (i, l) in m.buttons.iter().take(5).enumerate() { if bits(pl, l.bit, l.size) != 0 { b |= 1 << i; } }
    Some((rd(&m.x), rd(&m.y), rd(&m.wheel), b))
}

/// Feature report payload (without id) that sets `value` in `loc`.
pub fn feature_payload(loc: Loc, value: u32) -> Vec<u8> {
    let n = ((loc.bit + loc.size + 7) / 8) as usize;
    let mut v = alloc::vec![0u8; n];
    for i in 0..loc.size.min(32) {
        if (value >> i) & 1 != 0 { let b = loc.bit + i; v[(b / 8) as usize] |= 1 << (b % 8); }
    }
    v
}
