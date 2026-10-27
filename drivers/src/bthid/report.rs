//! HID report-protocol core for the Bluetooth HID host: report descriptor
//! parser, report -> evdev event mapper, gamepad quirks, and the SDP pieces
//! needed to fetch a device's report descriptor.
//!
//! Pure logic (alloc only, no kernel dependencies) so the host test crate
//! `drivers/bthid-tests` compiles it via `#[path]` and runs it against
//! captured descriptors and report streams.
//!
//! Output model: one [`HidDevice`] per remote device, split into *roles*
//! (gamepad / keyboard / mouse), one evdev node each. [`HidDevice::input`]
//! turns one input report (report id first when the descriptor numbers its
//! reports) into `(role, type, code, value)` events, each touched role ending
//! with a `SYN_REPORT`. A gamepad is normalised the way Linux's hid-playstation
//! and xpad present one, whatever the descriptor says: left stick
//! `ABS_X/ABS_Y` and right stick `ABS_RX/ABS_RY` in -32768..32767, analog
//! triggers `ABS_Z`/`ABS_RZ` in 0..255, d-pad `ABS_HAT0X/ABS_HAT0Y` in -1..1,
//! face buttons `BTN_SOUTH/EAST/NORTH/WEST` (positional), shoulders
//! `BTN_TL/TR/TL2/TR2`, `BTN_SELECT/START/MODE/THUMBL/THUMBR`.

use alloc::vec::Vec;

// ── evdev codes ─────────────────────────────────────────────────────────────
pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_REL: u16 = 2;
pub const EV_ABS: u16 = 3;

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_Z: u16 = 0x02;
pub const ABS_RX: u16 = 0x03;
pub const ABS_RY: u16 = 0x04;
pub const ABS_RZ: u16 = 0x05;
pub const ABS_HAT0X: u16 = 0x10;
pub const ABS_HAT0Y: u16 = 0x11;

pub const REL_X: u16 = 0;
pub const REL_Y: u16 = 1;
pub const REL_HWHEEL: u16 = 6;
pub const REL_WHEEL: u16 = 8;

pub const BTN_LEFT: u16 = 0x110;
pub const BTN_SOUTH: u16 = 0x130;
pub const BTN_EAST: u16 = 0x131;
pub const BTN_C: u16 = 0x132;
pub const BTN_NORTH: u16 = 0x133;
pub const BTN_WEST: u16 = 0x134;
pub const BTN_Z: u16 = 0x135;
pub const BTN_TL: u16 = 0x136;
pub const BTN_TR: u16 = 0x137;
pub const BTN_TL2: u16 = 0x138;
pub const BTN_TR2: u16 = 0x139;
pub const BTN_SELECT: u16 = 0x13a;
pub const BTN_START: u16 = 0x13b;
pub const BTN_MODE: u16 = 0x13c;
pub const BTN_THUMBL: u16 = 0x13d;
pub const BTN_THUMBR: u16 = 0x13e;
pub const BTN_TRIGGER_HAPPY1: u16 = 0x2c0;

/// Stick range advertised by the gamepad node.
pub const STICK_MIN: i32 = -32768;
pub const STICK_MAX: i32 = 32767;
/// Analog trigger range.
pub const TRIG_MAX: i32 = 255;

// ── Roles ───────────────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role { Gamepad = 0, Keyboard = 1, Mouse = 2 }
pub const ROLES: [Role; 3] = [Role::Gamepad, Role::Keyboard, Role::Mouse];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ev { pub role: Role, pub ty: u16, pub code: u16, pub value: i32 }

// ── Report descriptor parser ────────────────────────────────────────────────
const F_CONST: u32 = 1;
const F_VAR: u32 = 2;
const F_REL: u32 = 4;
const F_NULL: u32 = 0x40;

/// One Input main item.
#[derive(Clone, Debug)]
pub struct Field {
    pub report_id: u8,
    /// Bit offset in the report payload (after the report id byte).
    pub bit: u32,
    pub size: u8,
    pub count: u16,
    pub flags: u32,
    pub lmin: i32,
    pub lmax: i32,
    /// Extended usages (page << 16 | id), one per element for a variable
    /// field, the selectable set for an array.
    pub usages: Vec<u32>,
    /// Usage of the enclosing top-level application collection.
    pub app: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Descriptor {
    pub fields: Vec<Field>,
    /// Top-level application collection usages, in order.
    pub apps: Vec<u32>,
    /// True when the descriptor has Report ID items (reports start with one).
    pub numbered: bool,
}

#[derive(Clone, Copy, Default)]
struct Globals { page: u32, lmin: i32, lmax: i32, size: u32, count: u32, id: u8 }

fn sext(v: u32, n: usize) -> i32 {
    match n { 1 => v as u8 as i8 as i32, 2 => v as u16 as i16 as i32, _ => v as i32 }
}

/// Parse a HID report descriptor (HID 1.11 §6.2.2). Malformed trailing bytes
/// end the parse; whatever was understood is kept.
pub fn parse_descriptor(d: &[u8]) -> Descriptor {
    let mut out = Descriptor::default();
    let mut g = Globals::default();
    let mut stack: Vec<Globals> = Vec::new();
    let mut usages: Vec<u32> = Vec::new();
    let mut umin: Option<u32> = None;
    let mut umax: Option<u32> = None;
    let mut depth = 0u32;
    let mut app = 0u32;
    // Next free bit per report id.
    let mut next_bit = [0u32; 256];
    let mut lmin_n = 1usize;
    let mut i = 0;
    while i < d.len() {
        let b = d[i];
        if b == 0xFE { // long item: skip
            let Some(&n) = d.get(i + 1) else { break };
            i += 3 + n as usize;
            continue;
        }
        let n = [0usize, 1, 2, 4][(b & 3) as usize];
        if i + 1 + n > d.len() { break; }
        let mut v = 0u32;
        for k in 0..n { v |= (d[i + 1 + k] as u32) << (8 * k); }
        let (ty, tag) = ((b >> 2) & 3, b >> 4);
        i += 1 + n;
        let ext = |v: u32, page: u32| if n == 4 { v } else { (page << 16) | (v & 0xFFFF) };
        match (ty, tag) {
            (0, 0xA) => { // Collection
                if depth == 0 {
                    app = usages.first().copied().or(umin).unwrap_or(0);
                    if v == 1 { out.apps.push(app); }
                }
                depth += 1;
                usages.clear(); umin = None; umax = None;
            }
            (0, 0xC) => { depth = depth.saturating_sub(1); usages.clear(); umin = None; umax = None; }
            (0, 0x8) | (0, 0x9) | (0, 0xB) => { // Input / Output / Feature
                let count = g.count.min(1024) as u16;
                let size = g.size.min(32) as u8;
                if tag == 0x8 {
                    let mut us = usages.clone();
                    if let (Some(a), Some(z)) = (umin, umax) {
                        if z >= a && z - a < 1024 { for u in a..=z { us.push(u); } }
                    }
                    let mut lmax = g.lmax;
                    // Many descriptors write an unsigned max that sign-extends
                    // negative (e.g. 0x26 0xFF 0x00 is fine, 0x25 0xFF is not).
                    if g.lmin >= 0 && lmax < g.lmin { lmax = sext_fix(lmax, lmin_n); }
                    let bit = next_bit[g.id as usize];
                    out.fields.push(Field { report_id: g.id, bit, size, count, flags: v,
                        lmin: g.lmin, lmax, usages: us, app });
                    next_bit[g.id as usize] = bit + size as u32 * count as u32;
                }
                usages.clear(); umin = None; umax = None;
            }
            (1, 0x0) => g.page = v & 0xFFFF,
            (1, 0x1) => { g.lmin = sext(v, n); lmin_n = n; }
            (1, 0x2) => { g.lmax = sext(v, n); lmin_n = n; }
            (1, 0x7) => g.size = v,
            (1, 0x8) => { g.id = v as u8; out.numbered = true; }
            (1, 0x9) => g.count = v,
            (1, 0xA) => stack.push(g),
            (1, 0xB) => { if let Some(s) = stack.pop() { g = s; } }
            (2, 0x0) => usages.push(ext(v, g.page)),
            (2, 0x1) => umin = Some(ext(v, g.page)),
            (2, 0x2) => umax = Some(ext(v, g.page)),
            _ => {}
        }
    }
    out
}

fn sext_fix(v: i32, n: usize) -> i32 {
    match n { 1 => v as u8 as i32, 2 => v as u16 as i32, _ => v }
}

/// Extract `size` bits at `bit` (little-endian bit order) from `p`.
pub fn bits(p: &[u8], bit: u32, size: u8) -> u32 {
    let mut v = 0u32;
    for k in 0..size as u32 {
        let b = bit + k;
        let byte = (b / 8) as usize;
        if byte >= p.len() { break; }
        if p[byte] >> (b % 8) & 1 != 0 { v |= 1 << k; }
    }
    v
}

// ── Usages ──────────────────────────────────────────────────────────────────
const GD: u32 = 0x01 << 16;
const SIM: u32 = 0x02 << 16;
const KBD: u32 = 0x07 << 16;
const BUTTON: u32 = 0x09 << 16;
const CONSUMER: u32 = 0x0C << 16;

const GD_POINTER: u32 = GD | 0x01;
const GD_MOUSE: u32 = GD | 0x02;
const GD_JOYSTICK: u32 = GD | 0x04;
const GD_GAMEPAD: u32 = GD | 0x05;
const GD_KEYBOARD: u32 = GD | 0x06;
const GD_X: u32 = GD | 0x30;
const GD_Y: u32 = GD | 0x31;
const GD_Z: u32 = GD | 0x32;
const GD_RX: u32 = GD | 0x33;
const GD_RY: u32 = GD | 0x34;
const GD_RZ: u32 = GD | 0x35;
const GD_WHEEL: u32 = GD | 0x38;
const GD_HAT: u32 = GD | 0x39;
const GD_DPAD_UP: u32 = GD | 0x90;
const GD_DPAD_RIGHT: u32 = GD | 0x92;
const SIM_ACCEL: u32 = SIM | 0xC4;
const SIM_BRAKE: u32 = SIM | 0xC5;
const CONSUMER_CONTROL: u32 = CONSUMER | 0x01;
const AC_PAN: u32 = CONSUMER | 0x238;

fn role_of_app(app: u32) -> Option<Role> {
    match app {
        GD_JOYSTICK | GD_GAMEPAD => Some(Role::Gamepad),
        GD_KEYBOARD | CONSUMER_CONTROL => Some(Role::Keyboard),
        GD_MOUSE | GD_POINTER => Some(Role::Mouse),
        _ => None,
    }
}

fn consumer_key(u: u32) -> u16 {
    match u & 0xFFFF {
        0xE2 => 113, 0xE9 => 115, 0xEA => 114, 0xCD => 164, 0xB5 => 163, 0xB6 => 165,
        0xB7 => 166, 0x223 => 172, 0x224 => 158, 0x6F => 225, 0x70 => 224, _ => 0,
    }
}

fn kbd_modifier(u: u32) -> u16 {
    [29u16, 42, 56, 125, 97, 54, 100, 126][(u & 7) as usize]
}

/// Linux `hid-input` gamepad button numbering: Button n -> BTN_GAMEPAD + n-1
/// for n <= 15, then BTN_TRIGGER_HAPPY.
fn gamepad_button(n: u32) -> u16 {
    match n {
        1..=15 => BTN_SOUTH + (n - 1) as u16,
        16..=47 => BTN_TRIGGER_HAPPY1 + (n - 16) as u16,
        _ => 0,
    }
}

fn scale(v: i32, lmin: i32, lmax: i32, omin: i32, omax: i32) -> i32 {
    if lmax <= lmin { return omin; }
    let v = v.clamp(lmin, lmax) as i64;
    let r = (v - lmin as i64) * (omax as i64 - omin as i64) / (lmax as i64 - lmin as i64) + omin as i64;
    r as i32
}

/// Hat switch position (0 = N, clockwise) -> (x, y).
fn hat_xy(pos: Option<u32>) -> (i32, i32) {
    const T: [(i32, i32); 8] = [(0, -1), (1, -1), (1, 0), (1, 1), (0, 1), (-1, 1), (-1, 0), (-1, -1)];
    pos.and_then(|p| T.get(p as usize).copied()).unwrap_or((0, 0))
}

// ── Quirks ──────────────────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quirk {
    /// Descriptor-driven (8BitDo D-input/Android mode, Xbox One S classic BT,
    /// keyboards, mice).
    Generic,
    /// Sony DualShock 4 (054c:05c4, 054c:09cc).
    Ds4,
    /// Sony DualSense / DualSense Edge (054c:0ce6, 054c:0df2).
    DualSense,
    /// Nintendo Switch Pro controller (057e:2009), which 8BitDo pads in
    /// Switch mode (Start+Y on most models) also claim to be.
    SwitchPro,
}

pub fn quirk_for(vendor: u16, product: u16) -> Quirk {
    match (vendor, product) {
        (0x054c, 0x05c4) | (0x054c, 0x09cc) | (0x054c, 0x0ba0) => Quirk::Ds4,
        (0x054c, 0x0ce6) | (0x054c, 0x0df2) => Quirk::DualSense,
        (0x057e, 0x2009) => Quirk::SwitchPro,
        _ => Quirk::Generic,
    }
}

/// Bytes to send on the HIDP *control* channel right after both channels are
/// up, to switch the device into full reports.
///
/// * DS4: GET_REPORT(feature 0x02) makes it switch from the reduced 0x01
///   report to the full 0x11 one (Linux hid-sony).
/// * DualSense: GET_REPORT(feature 0x05, calibration) switches 0x01 -> 0x31
///   (Linux hid-playstation).
/// The parser understands both forms either way, so a lost reply only costs
/// the analog triggers on a DS4.
pub fn init_control(q: Quirk) -> Option<[u8; 2]> {
    match q {
        Quirk::Ds4 => Some([HIDP_GET_REPORT | HIDP_FEATURE, 0x02]),
        Quirk::DualSense => Some([HIDP_GET_REPORT | HIDP_FEATURE, 0x05]),
        _ => None,
    }
}

// ── HIDP transaction headers (Bluetooth HID profile §3.1) ───────────────────
pub const HIDP_HANDSHAKE: u8 = 0x00;
pub const HIDP_CONTROL: u8 = 0x10;
pub const HIDP_GET_REPORT: u8 = 0x40;
pub const HIDP_SET_REPORT: u8 = 0x50;
pub const HIDP_SET_PROTOCOL: u8 = 0x70;
pub const HIDP_DATA: u8 = 0xA0;
pub const HIDP_INPUT: u8 = 0x01;
pub const HIDP_OUTPUT: u8 = 0x02;
pub const HIDP_FEATURE: u8 = 0x03;
/// HID_CONTROL VIRTUAL_CABLE_UNPLUG.
pub const HIDP_VC_UNPLUG: u8 = HIDP_CONTROL | 0x05;

/// Unwrap an interrupt-channel frame: `DATA|Input` header -> report bytes.
pub fn hidp_input(frame: &[u8]) -> Option<&[u8]> {
    match frame.split_first() {
        Some((&h, rest)) if h == HIDP_DATA | HIDP_INPUT => Some(rest),
        _ => None,
    }
}

// ── Device state ────────────────────────────────────────────────────────────
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Axis { Lx, Ly, Rx, Ry, L2, R2 }

pub struct HidDevice {
    pub quirk: Quirk,
    pub desc: Descriptor,
    /// Roles present (index = Role as usize).
    pub roles: [bool; 3],
    /// Right stick on Z/Rz (no Rx/Ry axes in a gamepad application).
    z_is_right: bool,
    /// Last emitted value per (role, type, code) key; keys/abs dedup.
    last: Vec<(u8, u16, u16, i32)>,
    /// Pressed array-field keys from the previous report, per field index.
    arr: Vec<(usize, Vec<u16>)>,
}

impl HidDevice {
    pub fn new(vendor: u16, product: u16, descriptor: &[u8]) -> HidDevice {
        let quirk = quirk_for(vendor, product);
        let desc = parse_descriptor(descriptor);
        let mut roles = [false; 3];
        if quirk != Quirk::Generic { roles[Role::Gamepad as usize] = true; } else {
            for &a in &desc.apps { if let Some(r) = role_of_app(a) { roles[r as usize] = true; } }
        }
        let gp = |u: u32| desc.fields.iter().any(|f| role_of_app(f.app) == Some(Role::Gamepad)
            && f.flags & F_VAR != 0 && f.usages.contains(&u));
        let z_is_right = !(gp(GD_RX) && gp(GD_RY));
        HidDevice { quirk, desc, roles, z_is_right, last: Vec::new(), arr: Vec::new() }
    }

    pub fn has(&self, r: Role) -> bool { self.roles[r as usize] }

    fn put(&mut self, out: &mut Vec<Ev>, role: Role, ty: u16, code: u16, value: i32) {
        if code == 0 && ty == EV_KEY { return; }
        if ty == EV_REL {
            if value != 0 { out.push(Ev { role, ty, code, value }); }
            return;
        }
        let k = (role as u8, ty, code);
        match self.last.iter_mut().find(|e| (e.0, e.1, e.2) == k) {
            Some(e) if e.3 == value => return,
            Some(e) => e.3 = value,
            None => {
                self.last.push((k.0, k.1, k.2, value));
                // A first report at rest is not news.
                if value == 0 && !(ty == EV_ABS && matches!(code, ABS_X | ABS_Y | ABS_RX | ABS_RY)) { return; }
            }
        }
        out.push(Ev { role, ty, code, value });
    }

    /// Events for a release of everything (disconnect).
    pub fn release_all(&mut self) -> Vec<Ev> {
        let mut out = Vec::new();
        let held: Vec<(u8, u16, u16, i32)> = self.last.iter().copied()
            .filter(|e| e.1 == EV_KEY && e.3 != 0).collect();
        for (r, ty, code, _) in held { self.put(&mut out, ROLES[r as usize], ty, code, 0); }
        for (_, keys) in self.arr.iter_mut() { keys.clear(); }
        finish(&mut out);
        out
    }

    /// Map one input report (report id first when numbered / for quirk
    /// devices) to events. Unknown reports produce nothing.
    pub fn input(&mut self, r: &[u8]) -> Vec<Ev> {
        let mut out = Vec::new();
        match self.quirk {
            Quirk::Ds4 | Quirk::DualSense => self.sony(r, &mut out),
            Quirk::SwitchPro => if !self.switch(r, &mut out) { self.generic(r, &mut out) },
            Quirk::Generic => self.generic(r, &mut out),
        }
        finish(&mut out);
        out
    }

    fn pad(&mut self, out: &mut Vec<Ev>, axis: Axis, v: i32, lmin: i32, lmax: i32) {
        let (code, o0, o1) = match axis {
            Axis::Lx => (ABS_X, STICK_MIN, STICK_MAX), Axis::Ly => (ABS_Y, STICK_MIN, STICK_MAX),
            Axis::Rx => (ABS_RX, STICK_MIN, STICK_MAX), Axis::Ry => (ABS_RY, STICK_MIN, STICK_MAX),
            Axis::L2 => (ABS_Z, 0, TRIG_MAX), Axis::R2 => (ABS_RZ, 0, TRIG_MAX),
        };
        let s = scale(v, lmin, lmax, o0, o1);
        self.put(out, Role::Gamepad, EV_ABS, code, s);
    }

    fn hat(&mut self, out: &mut Vec<Ev>, pos: Option<u32>) {
        let (x, y) = hat_xy(pos);
        self.put(out, Role::Gamepad, EV_ABS, ABS_HAT0X, x);
        self.put(out, Role::Gamepad, EV_ABS, ABS_HAT0Y, y);
    }

    fn btns(&mut self, out: &mut Vec<Ev>, byte: u8, map: &[u16; 8]) {
        for (i, &c) in map.iter().enumerate() {
            self.put(out, Role::Gamepad, EV_KEY, c, (byte >> i & 1) as i32);
        }
    }

    /// DS4 0x01/0x11 and DualSense 0x01/0x31: sticks, hat + face buttons,
    /// shoulder/menu buttons, PS, analog triggers.
    fn sony(&mut self, r: &[u8], out: &mut Vec<Ev>) {
        // (base of LX, offset of the first button byte, offsets of L2/R2)
        let (base, b0, l2) = match (self.quirk, r.first()) {
            (_, Some(0x01)) if r.len() >= 10 => (1, 5, 8),
            (Quirk::Ds4, Some(0x11)) if r.len() >= 12 => (3, 7, 10),
            (Quirk::DualSense, Some(0x31)) if r.len() >= 12 => (2, 9, 6),
            _ => return,
        };
        for (k, ax) in [Axis::Lx, Axis::Ly, Axis::Rx, Axis::Ry].into_iter().enumerate() {
            self.pad(out, ax, r[base + k] as i32, 0, 255);
        }
        self.pad(out, Axis::L2, r[l2] as i32, 0, 255);
        self.pad(out, Axis::R2, r[l2 + 1] as i32, 0, 255);
        let h = r[b0] & 0x0F;
        self.hat(out, if h < 8 { Some(h as u32) } else { None });
        self.btns(out, r[b0] & 0xF0, &[0, 0, 0, 0, BTN_WEST, BTN_SOUTH, BTN_EAST, BTN_NORTH]);
        self.btns(out, r[b0 + 1], &[BTN_TL, BTN_TR, BTN_TL2, BTN_TR2, BTN_SELECT, BTN_START, BTN_THUMBL, BTN_THUMBR]);
        self.btns(out, r[b0 + 2] & 1, &[BTN_MODE, 0, 0, 0, 0, 0, 0, 0]);
    }

    /// Switch Pro (and 8BitDo in Switch mode): simple-HID 0x3F and full 0x30.
    fn switch(&mut self, r: &[u8], out: &mut Vec<Ev>) -> bool {
        match r.first() {
            Some(0x3F) if r.len() >= 12 => {
                self.btns(out, r[1], &[BTN_SOUTH, BTN_EAST, BTN_WEST, BTN_NORTH, BTN_TL, BTN_TR, BTN_TL2, BTN_TR2]);
                self.btns(out, r[2] & 0x3F, &[BTN_SELECT, BTN_START, BTN_THUMBL, BTN_THUMBR, BTN_MODE, BTN_TRIGGER_HAPPY1, 0, 0]);
                self.hat(out, if r[3] < 8 { Some(r[3] as u32) } else { None });
                let le = |o: usize| u16::from_le_bytes([r[o], r[o + 1]]) as i32;
                for (k, ax) in [Axis::Lx, Axis::Ly, Axis::Rx, Axis::Ry].into_iter().enumerate() {
                    self.pad(out, ax, le(4 + 2 * k), 0, 0xFFFF);
                }
                true
            }
            Some(0x30) if r.len() >= 12 => {
                self.btns(out, r[3], &[BTN_WEST, BTN_NORTH, BTN_SOUTH, BTN_EAST, 0, 0, BTN_TR, BTN_TR2]);
                self.btns(out, r[4] & 0x3F, &[BTN_SELECT, BTN_START, BTN_THUMBR, BTN_THUMBL, BTN_MODE, BTN_TRIGGER_HAPPY1, 0, 0]);
                self.btns(out, r[5] & 0xC0, &[0, 0, 0, 0, 0, 0, BTN_TL, BTN_TL2]);
                let (dn, up, rt, lf) = (r[5] & 1 != 0, r[5] & 2 != 0, r[5] & 4 != 0, r[5] & 8 != 0);
                let x = rt as i32 - lf as i32;
                let y = dn as i32 - up as i32;
                self.put(out, Role::Gamepad, EV_ABS, ABS_HAT0X, x);
                self.put(out, Role::Gamepad, EV_ABS, ABS_HAT0Y, y);
                let s12 = |o: usize| ((r[o] as i32 | ((r[o + 1] as i32 & 0xF) << 8)),
                                      ((r[o + 1] as i32 >> 4) | ((r[o + 2] as i32) << 4)));
                let (lx, ly) = s12(6);
                let (rx, ry) = s12(9);
                // Stick Y grows upward on Switch controllers; evdev grows down.
                self.pad(out, Axis::Lx, lx, 0, 4095);
                self.pad(out, Axis::Ly, 4095 - ly, 0, 4095);
                self.pad(out, Axis::Rx, rx, 0, 4095);
                self.pad(out, Axis::Ry, 4095 - ry, 0, 4095);
                true
            }
            _ => false,
        }
    }

    fn generic(&mut self, r: &[u8], out: &mut Vec<Ev>) {
        let (id, payload) = if self.desc.numbered {
            match r.split_first() { Some((&id, p)) => (id, p), None => return }
        } else { (0, r) };
        let nf = self.desc.fields.len();
        for fi in 0..nf {
            let f = &self.desc.fields[fi];
            if f.report_id != id || f.flags & F_CONST != 0 || f.size == 0 { continue; }
            let Some(role) = role_of_app(f.app) else { continue };
            if !self.roles[role as usize] { continue; }
            let (bit, size, count, flags, lmin, lmax) = (f.bit, f.size, f.count, f.flags, f.lmin, f.lmax);
            let signed = lmin < 0;
            let get = |k: u32| {
                let raw = bits(payload, bit + k * size as u32, size);
                if signed && size < 32 && raw >> (size - 1) & 1 != 0 { (raw | (!0u32 << size)) as i32 } else { raw as i32 }
            };
            if flags & F_VAR == 0 {
                // Array: the set of selected usages.
                let usages = f.usages.clone();
                let mut now: Vec<u16> = Vec::new();
                let mut rollover = false;
                for k in 0..count as u32 {
                    let v = get(k);
                    if v < lmin || v > lmax { continue; }
                    let Some(&u) = usages.get((v - lmin) as usize) else { continue };
                    if u == KBD | 1 { rollover = true; }
                    let code = self.array_code(role, u);
                    if code != 0 && !now.contains(&code) { now.push(code); }
                }
                if rollover { continue; }
                let prev = match self.arr.iter().position(|e| e.0 == fi) {
                    Some(p) => core::mem::take(&mut self.arr[p].1),
                    None => { self.arr.push((fi, Vec::new())); Vec::new() }
                };
                for &c in prev.iter().filter(|c| !now.contains(c)) { self.put(out, role, EV_KEY, c, 0); }
                for &c in now.iter().filter(|c| !prev.contains(c)) { self.put(out, role, EV_KEY, c, 1); }
                let p = self.arr.iter().position(|e| e.0 == fi).unwrap();
                self.arr[p].1 = now;
                continue;
            }
            let usages = f.usages.clone();
            for k in 0..count as u32 {
                let Some(&u) = usages.get(k as usize).or(usages.last()) else { break };
                let v = get(k);
                self.var(out, role, u, v, lmin, lmax, flags);
            }
        }
    }

    fn array_code(&self, role: Role, u: u32) -> u16 {
        match u & 0xFFFF_0000 {
            KBD => match u & 0xFFFF { 0xE0..=0xE7 => kbd_modifier(u), x if x < 0x100 => usb_key(x as u8), _ => 0 },
            CONSUMER => consumer_key(u),
            BUTTON if role == Role::Gamepad => gamepad_button(u & 0xFFFF),
            _ => 0,
        }
    }

    fn var(&mut self, out: &mut Vec<Ev>, role: Role, u: u32, v: i32, lmin: i32, lmax: i32, flags: u32) {
        let on = (v != 0) as i32;
        match role {
            Role::Gamepad => match u {
                GD_X => self.pad(out, Axis::Lx, v, lmin, lmax),
                GD_Y => self.pad(out, Axis::Ly, v, lmin, lmax),
                GD_RX => self.pad(out, Axis::Rx, v, lmin, lmax),
                GD_RY => self.pad(out, Axis::Ry, v, lmin, lmax),
                GD_Z => self.pad(out, if self.z_is_right { Axis::Rx } else { Axis::L2 }, v, lmin, lmax),
                GD_RZ => self.pad(out, if self.z_is_right { Axis::Ry } else { Axis::R2 }, v, lmin, lmax),
                SIM_BRAKE => self.pad(out, Axis::L2, v, lmin, lmax),
                SIM_ACCEL => self.pad(out, Axis::R2, v, lmin, lmax),
                GD_HAT => {
                    let span = lmax - lmin + 1;
                    let pos = if v < lmin || v > lmax || (flags & F_NULL != 0 && span < 1) { None }
                        else if span == 4 { Some(((v - lmin) * 2) as u32) }
                        else if span == 8 { Some((v - lmin) as u32) } else { None };
                    self.hat(out, pos);
                }
                x if (GD_DPAD_UP..=GD_DPAD_UP + 3).contains(&x) => {
                    // Up, Down, Right, Left as separate bits.
                    let (code, dir) = match x - GD_DPAD_UP { 0 => (ABS_HAT0Y, -1), 1 => (ABS_HAT0Y, 1),
                                                             2 => (ABS_HAT0X, 1), _ => (ABS_HAT0X, -1) };
                    debug_assert_eq!(GD_DPAD_RIGHT - GD_DPAD_UP, 2);
                    // Opposite directions share an axis: a press writes its
                    // direction, a release clears only its own direction.
                    if on != 0 { self.put(out, role, EV_ABS, code, dir); }
                    else if self.last.iter().any(|e| e.0 == role as u8 && e.1 == EV_ABS && e.2 == code && e.3 == dir) {
                        self.put(out, role, EV_ABS, code, 0);
                    }
                }
                x if x & 0xFFFF_0000 == BUTTON => self.put(out, role, EV_KEY, gamepad_button(x & 0xFFFF), on),
                x if x & 0xFFFF_0000 == CONSUMER && x & 0xFFFF == 0x223 => self.put(out, role, EV_KEY, BTN_MODE, on),
                _ => {}
            },
            Role::Keyboard => match u & 0xFFFF_0000 {
                KBD if (0xE0..=0xE7).contains(&(u & 0xFFFF)) => self.put(out, role, EV_KEY, kbd_modifier(u), on),
                KBD if u & 0xFFFF < 0x100 && u & 0xFFFF >= 4 => self.put(out, role, EV_KEY, usb_key((u & 0xFF) as u8), on),
                CONSUMER => self.put(out, role, EV_KEY, consumer_key(u), on),
                _ => {}
            },
            Role::Mouse => match u {
                GD_X if flags & F_REL != 0 => self.put(out, role, EV_REL, REL_X, v),
                GD_Y if flags & F_REL != 0 => self.put(out, role, EV_REL, REL_Y, v),
                GD_WHEEL => self.put(out, role, EV_REL, REL_WHEEL, v),
                AC_PAN => self.put(out, role, EV_REL, REL_HWHEEL, v),
                x if x & 0xFFFF_0000 == BUTTON && (1..=8).contains(&(x & 0xFFFF)) =>
                    self.put(out, role, EV_KEY, BTN_LEFT + (x & 0xFFFF) as u16 - 1, on),
                _ => {}
            },
        }
    }
}

/// Append a SYN_REPORT after the last event of each role present.
fn finish(out: &mut Vec<Ev>) {
    for r in ROLES {
        if out.iter().any(|e| e.role == r && e.ty != EV_SYN) {
            out.push(Ev { role: r, ty: EV_SYN, code: 0, value: 0 });
        }
    }
}

/// HID keyboard usage (page 7) -> Linux key code. Same table as the USB
/// boot-keyboard driver (Linux `hid_keyboard[]`).
pub fn usb_key(u: u8) -> u16 { USB_KEYS[u as usize] as u16 }

#[rustfmt::skip]
static USB_KEYS: [u8; 256] = [
      0,  0,  0,  0, 30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38,
     50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44,  2,  3,
      4,  5,  6,  7,  8,  9, 10, 11, 28,  1, 14, 15, 57, 12, 13, 26,
     27, 43, 43, 39, 40, 41, 51, 52, 53, 58, 59, 60, 61, 62, 63, 64,
     65, 66, 67, 68, 87, 88, 99, 70,119,110,102,104,111,107,109,106,
    105,108,103, 69, 98, 55, 74, 78, 96, 79, 80, 81, 75, 76, 77, 71,
     72, 73, 82, 83, 86,127,116,117,183,184,185,186,187,188,189,190,
    191,192,193,194,134,138,130,132,128,129,131,137,133,135,136,113,
    115,114,  0,  0,  0,121,  0, 89, 93,124, 92, 94, 95,  0,  0,  0,
    122,123, 90, 91, 85,  0,  0,  0,  0,  0,  0,  0,111,  0,  0,  0,
      0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,
      0,  0,  0,  0,  0,  0,179,180,  0,  0,  0,  0,  0,  0,  0,  0,
      0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,
      0,  0,  0,  0,  0,  0,  0,  0,111,  0,  0,  0,  0,  0,  0,  0,
     29, 42, 56,125, 97, 54,100,126,164,166,165,163,161,115,114,113,
    150,158,159,128,136,177,178,176,142,152,173,140,  0,  0,  0,  0,
];

// ── SDP: fetch the HID descriptor (attribute 0x0206 of service 0x1124) ─────
pub const SDP_PSM: u16 = 0x0001;
pub const HID_CONTROL_PSM: u16 = 0x0011;
pub const HID_INTERRUPT_PSM: u16 = 0x0013;

/// SDP_ServiceSearchAttributeRequest for UUID16 0x1124 (HID), attributes
/// 0x0201 (vendor) .. 0x0206 (HIDDescriptorList) plus PnP is a separate
/// service, so this asks for the whole 0x0000-0xFFFF range of the HID record.
/// `cont` is the continuation state from the previous response (empty first).
pub fn sdp_hid_request(tid: u16, cont: &[u8]) -> Vec<u8> {
    let mut p = Vec::new();
    p.push(0x06);
    p.extend_from_slice(&tid.to_be_bytes());
    let params_len = 5 + 2 + 7 + 1 + cont.len();
    p.extend_from_slice(&(params_len as u16).to_be_bytes());
    p.extend_from_slice(&[0x35, 0x03, 0x19, 0x11, 0x24]); // seq{UUID16 0x1124}
    p.extend_from_slice(&0x0400u16.to_be_bytes());       // max attribute bytes
    p.extend_from_slice(&[0x35, 0x05, 0x0A, 0x00, 0x00, 0xFF, 0xFF]); // seq{range 0000-FFFF}
    p.push(cont.len() as u8);
    p.extend_from_slice(cont);
    p
}

/// One SDP_ServiceSearchAttributeResponse PDU: append its attribute-list
/// bytes to `acc` and return the continuation state (empty = done).
/// None for a malformed or error PDU.
pub fn sdp_response(pdu: &[u8], acc: &mut Vec<u8>) -> Option<Vec<u8>> {
    if pdu.len() < 7 || pdu[0] != 0x07 { return None; }
    let n = u16::from_be_bytes([pdu[5], pdu[6]]) as usize;
    let body = pdu.get(7..7 + n)?;
    acc.extend_from_slice(body);
    let cl = *pdu.get(7 + n)? as usize;
    Some(pdu.get(8 + n..8 + n + cl)?.to_vec())
}

/// Data element header -> (type, header length, payload length).
fn de_hdr(b: &[u8]) -> Option<(u8, usize, usize)> {
    let h = *b.first()?;
    let (ty, sz) = (h >> 3, h & 7);
    Some(match sz {
        0 => (ty, 1, if ty == 0 { 0 } else { 1 }),
        1 => (ty, 1, 2), 2 => (ty, 1, 4), 3 => (ty, 1, 8), 4 => (ty, 1, 16),
        5 => (ty, 2, *b.get(1)? as usize),
        6 => (ty, 3, u16::from_be_bytes([*b.get(1)?, *b.get(2)?]) as usize),
        _ => (ty, 5, u32::from_be_bytes([*b.get(1)?, *b.get(2)?, *b.get(3)?, *b.get(4)?]) as usize),
    })
}

/// Children of a data element sequence.
fn de_seq(b: &[u8]) -> Vec<&[u8]> {
    let mut v = Vec::new();
    let Some((ty, h, n)) = de_hdr(b) else { return v };
    if ty != 6 && ty != 7 { return v; }
    let Some(mut rest) = b.get(h..h + n) else { return v };
    while let Some((_, ch, cn)) = de_hdr(rest) {
        let Some(e) = rest.get(..ch + cn) else { break };
        v.push(e);
        rest = &rest[ch + cn..];
    }
    v
}

fn de_uint(b: &[u8]) -> Option<u32> {
    let (ty, h, n) = de_hdr(b)?;
    if ty != 1 || n > 4 { return None; }
    Some(b.get(h..h + n)?.iter().fold(0u32, |a, &x| a << 8 | x as u32))
}

/// From the reassembled attribute lists (sequence of per-record attribute
/// sequences), the first HIDDescriptorList report descriptor (type 0x22).
pub fn sdp_hid_descriptor(lists: &[u8]) -> Option<Vec<u8>> {
    for rec in de_seq(lists) {
        let attrs = de_seq(rec);
        for pair in attrs.chunks(2) {
            if pair.len() != 2 || de_uint(pair[0]) != Some(0x0206) { continue; }
            for d in de_seq(pair[1]) {
                let kv = de_seq(d);
                if kv.len() >= 2 && de_uint(kv[0]) == Some(0x22) {
                    let (ty, h, n) = de_hdr(kv[1])?;
                    if ty == 4 { return kv[1].get(h..h + n).map(|s| s.to_vec()); }
                }
            }
        }
    }
    None
}
