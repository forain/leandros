//! PS/2 mouse / touchpad packet decoding (protocol half; the i8042 AUX port
//! I/O and the probe sequence live in `arch_x86_64::psmouse`).
//!
//! Protocols, chosen at probe time ([`set_protocol`]):
//!
//! * **bare** PS/2 (3-byte packets), **ImPS/2** (4 bytes, wheel) and
//!   **Explorer** (4 bytes, wheel + side/extra buttons): relative motion,
//!   buttons, wheel → [`crate::rel_pointer`]. A Synaptics touchpad left in
//!   relative mode (and a pointing stick behind it, merged by the pad) is
//!   driven this way too.
//! * **Synaptics absolute** (6-byte packets, W mode): the touchpad's
//!   absolute finger position is turned into relative pointer motion here,
//!   with tap-to-click (1/2/3 fingers = left/right/middle), clickpad button
//!   areas, and two-finger scrolling as hi-res wheel events. W = 3 packets
//!   carry the pass-through pointing stick's 3-byte packet: its motion and
//!   its three buttons, with middle-button scrolling (hold middle + move the
//!   stick = wheel; a middle press without motion is a middle click).
//!
//! Everything runs in IRQ context on the bootstrap CPU (vector 33 serves
//! both i8042 IRQ lines), one byte at a time. `psmouse.debug=1` dumps raw
//! packets (rate-limited); without it only the first few are logged.

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU8, Ordering};
use crate::pci::{serial_debug as log, serial_debug_dec as dec};
use crate::rel_pointer::{self as rp, SRC_PS2, SRC_STICK};

pub const PROTO_OFF: u8 = 0;
pub const PROTO_BARE: u8 = 1;
pub const PROTO_IMPS: u8 = 2;
pub const PROTO_EXPS: u8 = 3;
pub const PROTO_SYNAPTICS: u8 = 4;

static PROTO: AtomicU8 = AtomicU8::new(PROTO_OFF);
static DEBUG: AtomicBool = AtomicBool::new(false);
/// Touchpad / stick gain in percent (`psmouse.speed`, `psmouse.stickspeed`).
static SPEED: AtomicI32 = AtomicI32::new(100);
static STICK_SPEED: AtomicI32 = AtomicI32::new(100);
static TAP: AtomicBool = AtomicBool::new(true);
static PACKETS: AtomicU32 = AtomicU32::new(0);
static BAD_SYNC: AtomicU32 = AtomicU32::new(0);
/// Re-entrancy guard: the decoder state below is only touched under it.
static BUSY: AtomicBool = AtomicBool::new(false);

/// What the Synaptics probe found (filled in before PROTO_SYNAPTICS is set).
#[derive(Clone, Copy)]
pub struct SynInfo {
    pub x_min: i32, pub x_max: i32, pub y_min: i32, pub y_max: i32,
    pub multifinger: bool,
    pub clickpad: bool,
    pub middle_button: bool,
    pub passthrough: bool,
}

const SYN_DEFAULT: SynInfo = SynInfo {
    x_min: 1472, x_max: 5472, y_min: 1408, y_max: 4448,
    multifinger: true, clickpad: false, middle_button: false, passthrough: false,
};

struct State {
    buf: [u8; 6],
    pos: usize,
    syn: SynInfo,
    // Touchpad.
    fingers: u8,
    last: Option<(i32, i32)>,
    settle: u8,
    rem_x: i32, rem_y: i32,
    scroll_x: i32, scroll_y: i32, scrolling: bool,
    touch_pkts: u32, touch_moved: i32, touch_max_fingers: u8, touch_button: bool, touch_wide: bool,
    pad_buttons: u8,
    click_button: u8,
    // Stick.
    stick_buttons: u8,
    stick_scrolled: bool,
    stick_rem_x: i32, stick_rem_y: i32,
}

static mut ST: State = State {
    buf: [0; 6], pos: 0, syn: SYN_DEFAULT,
    fingers: 0, last: None, settle: 0, rem_x: 0, rem_y: 0,
    scroll_x: 0, scroll_y: 0, scrolling: false,
    touch_pkts: 0, touch_moved: 0, touch_max_fingers: 0, touch_button: false, touch_wide: false,
    pad_buttons: 0, click_button: 0,
    stick_buttons: 0, stick_scrolled: false, stick_rem_x: 0, stick_rem_y: 0,
};

/// Kernel command line knobs: `psmouse.debug=1`, `psmouse.speed=<pct>`,
/// `psmouse.stickspeed=<pct>`, `psmouse.tap=0`.
pub fn configure() {
    let opt = crate::nvidia_probe::cmdline_opt;
    if opt("psmouse.debug").unwrap_or(0) != 0 { DEBUG.store(true, Ordering::Relaxed); }
    if let Some(v) = opt("psmouse.speed") { SPEED.store((v as i32).clamp(10, 1000), Ordering::Relaxed); }
    if let Some(v) = opt("psmouse.stickspeed") { STICK_SPEED.store((v as i32).clamp(10, 1000), Ordering::Relaxed); }
    if opt("psmouse.tap") == Some(0) { TAP.store(false, Ordering::Relaxed); }
}

pub fn debug() -> bool { DEBUG.load(Ordering::Relaxed) }

/// `psmouse.proto=imps|exps|bare|synaptics|off`: None = auto.
pub fn forced_protocol() -> Option<u8> {
    match crate::nvidia_probe::cmdline_word("psmouse.proto")? {
        "off" | "none" => Some(PROTO_OFF),
        "bare" | "ps2" => Some(PROTO_BARE),
        "imps" => Some(PROTO_IMPS),
        "exps" => Some(PROTO_EXPS),
        "synaptics" => Some(PROTO_SYNAPTICS),
        _ => None,
    }
}

/// Start decoding `proto` packets (after the device is enabled). For
/// Synaptics, `syn` describes the pad. Advertises the pointer capabilities.
pub fn set_protocol(proto: u8, syn: Option<SynInfo>) {
    PROTO.store(PROTO_OFF, Ordering::Release);
    unsafe {
        let st = &mut *core::ptr::addr_of_mut!(ST);
        st.pos = 0;
        if let Some(s) = syn { st.syn = s; }
    }
    if proto == PROTO_OFF { return; }
    rp::init_scale();
    match proto {
        PROTO_SYNAPTICS => rp::advertise(0x07, true, true),
        PROTO_EXPS => rp::advertise(0x1F, true, false),
        _ => rp::advertise(0x07, false, false),
    }
    PROTO.store(proto, Ordering::Release);
}

pub fn protocol() -> u8 { PROTO.load(Ordering::Acquire) }

fn packet_len(proto: u8) -> usize {
    match proto { PROTO_BARE => 3, PROTO_SYNAPTICS => 6, _ => 4 }
}

/// One byte from the AUX port (IRQ context). `err`: parity/timeout flagged.
pub fn on_byte(b: u8, err: bool) {
    let proto = protocol();
    if proto == PROTO_OFF { return; }
    if BUSY.swap(true, Ordering::Acquire) { return; }
    let st = unsafe { &mut *core::ptr::addr_of_mut!(ST) };
    if err {
        st.pos = 0;
    } else {
        byte(st, proto, b);
    }
    BUSY.store(false, Ordering::Release);
}

fn byte(st: &mut State, proto: u8, b: u8) {
    // Sync checks on the bytes whose fixed bits are known.
    let ok = match (proto, st.pos) {
        (PROTO_SYNAPTICS, 0) => b & 0xC8 == 0x80,
        (PROTO_SYNAPTICS, 3) => b & 0xC8 == 0xC0,
        (_, 0) => b & 0x08 != 0,
        _ => true,
    };
    if !ok {
        let n = BAD_SYNC.fetch_add(1, Ordering::Relaxed);
        if n < 4 || debug() && n % 64 == 0 {
            log("[PSMOUSE] lost sync at byte "); dec(st.pos as u64);
            log(" (0x"); hex8(b); log("), resyncing\n");
        }
        st.pos = 0;
        // A Synaptics byte 3 mismatch may be the start of the next packet.
        if proto == PROTO_SYNAPTICS && b & 0xC8 == 0x80 { st.buf[0] = b; st.pos = 1; }
        return;
    }
    st.buf[st.pos] = b;
    st.pos += 1;
    let len = packet_len(proto);
    if st.pos < len { return; }
    st.pos = 0;
    let n = PACKETS.fetch_add(1, Ordering::Relaxed);
    if n < 4 || debug() && (n < 200 || n % 25 == 0) {
        log("[PSMOUSE] pkt "); dec(n as u64); log(":");
        for &x in &st.buf[..len] { log(" "); hex8(x); }
        log("\n");
    }
    let pkt = st.buf;
    if proto == PROTO_SYNAPTICS { synaptics(st, &pkt); } else { relative(proto, &pkt); }
}

fn hex8(v: u8) {
    let d = |x: u8| if x < 10 { b'0' + x } else { b'a' + x - 10 };
    let s = [d(v >> 4), d(v & 15)];
    log(core::str::from_utf8(&s).unwrap_or("??"));
}

/// Standard 3-byte PS/2 motion: (buttons, dx, dy) with dy positive = down.
fn ps2_motion(p0: u8, p1: u8, p2: u8) -> (u8, i32, i32) {
    let mut dx = p1 as i32 - (((p0 as i32) << 4) & 0x100);
    let mut dy = p2 as i32 - (((p0 as i32) << 3) & 0x100);
    if p0 & 0x40 != 0 { dx = 0; }
    if p0 & 0x80 != 0 { dy = 0; }
    (p0 & 0x07, dx, -dy)
}

fn relative(proto: u8, p: &[u8; 6]) {
    let (mut buttons, dx, dy) = ps2_motion(p[0], p[1], p[2]);
    let (mut wheel, mut hwheel) = (0i32, 0i32);
    match proto {
        PROTO_IMPS => wheel = -(p[3] as i8 as i32),
        PROTO_EXPS => match p[3] & 0xC0 {
            0x80 => wheel = (p[3] & 32) as i32 - (p[3] & 31) as i32,
            0x40 => hwheel = (p[3] & 32) as i32 - (p[3] & 31) as i32,
            _ => {
                wheel = (p[3] & 8) as i32 - (p[3] & 7) as i32;
                if p[3] & 0x10 != 0 { buttons |= 0x08; }
                if p[3] & 0x20 != 0 { buttons |= 0x10; }
            }
        },
        _ => {}
    }
    let s = SPEED.load(Ordering::Relaxed);
    rp::report(SRC_PS2, dx * s / 100, dy * s / 100, wheel * 120, hwheel * 120, buttons);
}

// ── Synaptics absolute mode ─────────────────────────────────────────────────

/// Touch hysteresis on Z (pressure), Synaptics' usual thresholds.
const Z_DOWN: i32 = 30;
const Z_UP: i32 = 25;
/// Tap: at most this many packets (80 packets/s) and this much travel.
const TAP_MAX_PKTS: u32 = 18;
const TAP_MAX_MOVE: i32 = 120;
/// Touchpad units per wheel detent for two-finger scrolling.
const SCROLL_UNITS: i32 = 80;
/// Travel before a two-finger touch starts scrolling.
const SCROLL_START: i32 = 24;

fn synaptics(st: &mut State, p: &[u8; 6]) {
    let w = (((p[0] & 0x30) >> 2) | ((p[0] & 0x04) >> 1) | ((p[3] & 0x04) >> 2)) as i32;
    if w == 3 && st.syn.passthrough {
        stick(st, p[1], p[4], p[5]);
        return;
    }
    let x = ((((p[3] & 0x10) as i32) << 8) | (((p[1] & 0x0F) as i32) << 8) | p[4] as i32) as i32;
    let y = ((((p[3] & 0x20) as i32) << 7) | (((p[1] & 0xF0) as i32) << 4) | p[5] as i32) as i32;
    let z = p[2] as i32;

    // Physical buttons.
    let mut phys = 0u8;
    if st.syn.clickpad {
        let down = (p[0] ^ p[3]) & 0x01 != 0;
        if down && st.click_button == 0 {
            // Clickpad: two fingers or the bottom-right area = right button.
            let s = &st.syn;
            let bottom = y < s.y_min + (s.y_max - s.y_min) * 18 / 100;
            let right = x > s.x_min + (s.x_max - s.x_min) * 55 / 100;
            st.click_button = if st.fingers >= 2 || (bottom && right && z > 0) { 0x02 } else { 0x01 };
        } else if !down {
            st.click_button = 0;
        }
        phys = st.click_button;
    } else {
        phys |= p[0] & 0x03;
        if st.syn.middle_button && (p[0] ^ p[3]) & 0x01 != 0 { phys |= 0x04; }
    }

    // Fingers.
    let touching = if st.fingers > 0 { z >= Z_UP } else { z >= Z_DOWN };
    let fingers: u8 = if !touching {
        0
    } else if st.syn.multifinger && w == 0 {
        2
    } else if st.syn.multifinger && w == 1 {
        3
    } else {
        1
    };

    let (mut dx, mut dy, mut wheel, mut hwheel) = (0, 0, 0, 0);
    if fingers != st.fingers {
        // Finger count changed: the reported position jumps; re-anchor and
        // ignore a couple of packets.
        if st.fingers == 0 && fingers > 0 {
            st.touch_pkts = 0; st.touch_moved = 0; st.touch_max_fingers = 0;
            st.touch_button = false; st.touch_wide = false;
        }
        if fingers == 0 { end_touch(st); }
        st.last = None;
        st.settle = 2;
        st.scrolling = false; st.scroll_x = 0; st.scroll_y = 0;
        rp::wheel_reset();
        st.fingers = fingers;
    }
    if fingers > 0 {
        st.touch_pkts += 1;
        if fingers > st.touch_max_fingers { st.touch_max_fingers = fingers; }
        if phys != 0 { st.touch_button = true; }
        if w >= 12 { st.touch_wide = true; }
        if let Some((lx, ly)) = st.last {
            let (ux, uy) = (x - lx, y - ly);
            if st.settle > 0 {
                st.settle -= 1;
            } else if ux.abs() < 800 && uy.abs() < 800 {
                st.touch_moved += ux.abs() + uy.abs();
                if fingers == 1 {
                    let (mx, my) = pad_gain(st, ux, -uy);
                    dx = mx; dy = my;
                } else {
                    st.scroll_x += ux; st.scroll_y += uy;
                    if !st.scrolling && st.scroll_x.abs() + st.scroll_y.abs() >= SCROLL_START {
                        st.scrolling = true;
                    }
                    if st.scrolling {
                        // Lock to the dominant axis of this movement.
                        if uy.abs() >= ux.abs() { wheel = uy * 120 / SCROLL_UNITS; }
                        else { hwheel = ux * 120 / SCROLL_UNITS; }
                    }
                }
            }
        }
        st.last = Some((x, y));
    }

    let buttons = phys;
    if dx != 0 || dy != 0 || wheel != 0 || hwheel != 0 || buttons != st.pad_buttons {
        rp::report(SRC_PS2, dx, dy, wheel, hwheel, buttons);
        st.pad_buttons = buttons;
    }
}

/// Touchpad units → pixels: a base gain normalised to the pad width (a full
/// stroke = one framebuffer width at the slowest speed), times a mild speed
/// ramp (at most 2x). Fixed point with remainders so slow motion is not lost.
fn pad_gain(st: &mut State, ux: i32, uy: i32) -> (i32, i32) {
    let width = (st.syn.x_max - st.syn.x_min).max(1000);
    let speed = ux.abs().max(uy.abs());
    // ×1024: base = 1.0 * 1920 / width px per unit (one full stroke = one
    // screen width), ramping conservatively to 2x by 48 units/packet.
    let base = 1024 * 1920 / width;
    let ramp = 1024 + (speed * 1024 / 48).min(1024);
    let g = base * ramp / 1024 * SPEED.load(Ordering::Relaxed) / 100;
    st.rem_x += ux * g; st.rem_y += uy * g;
    let (px, py) = (st.rem_x / 1024, st.rem_y / 1024);
    st.rem_x -= px * 1024; st.rem_y -= py * 1024;
    (px, py)
}

fn end_touch(st: &mut State) {
    let tap = TAP.load(Ordering::Relaxed)
        && st.touch_pkts > 0 && st.touch_pkts <= TAP_MAX_PKTS
        && st.touch_moved < TAP_MAX_MOVE && !st.touch_button && !st.touch_wide;
    if tap {
        let b = match st.touch_max_fingers { 1 => 0x01, 2 => 0x02, _ => 0x04 };
        rp::report(SRC_PS2, 0, 0, 0, 0, st.pad_buttons | b);
        rp::report(SRC_PS2, 0, 0, 0, 0, st.pad_buttons);
        if debug() { log("[PSMOUSE] tap "); dec(st.touch_max_fingers as u64); log("\n"); }
    }
}

// ── Pass-through pointing stick ─────────────────────────────────────────────

/// Wheel v120 units per stick count while middle-scrolling.
const STICK_SCROLL: i32 = 10;

fn stick(st: &mut State, p0: u8, p1: u8, p2: u8) {
    // The guest's ACK/ID bytes also arrive as W=3 packets; a real motion
    // packet always has bit 3 of its first byte set.
    if p0 & 0x08 == 0 { return; }
    let (buttons, ux, uy) = ps2_motion(p0, p1, p2);
    let mid = buttons & 0x04 != 0;
    let was_mid = st.stick_buttons & 0x04 != 0;
    let (mut dx, mut dy, mut wheel, mut hwheel) = (0, 0, 0, 0);
    let mut out = buttons & !0x04;
    if mid {
        if !was_mid { st.stick_scrolled = false; rp::wheel_reset(); }
        if ux != 0 || uy != 0 {
            st.stick_scrolled = true;
            if uy.abs() >= ux.abs() { wheel = -uy * STICK_SCROLL; } else { hwheel = ux * STICK_SCROLL; }
        }
    } else {
        let s = STICK_SPEED.load(Ordering::Relaxed);
        // Mild acceleration: gain 1 at rest up to 3 at 16 counts/packet.
        let g = |d: i32| (d * (256 + (d.abs() * 32).min(512)) * s / 100) as i32;
        st.stick_rem_x += g(ux); st.stick_rem_y += g(uy);
        dx = st.stick_rem_x / 256; dy = st.stick_rem_y / 256;
        st.stick_rem_x -= dx * 256; st.stick_rem_y -= dy * 256;
    }
    if was_mid && !mid && !st.stick_scrolled {
        // Middle press without motion: a middle click.
        rp::report(SRC_STICK, 0, 0, 0, 0, out | 0x04);
    }
    if mid { out &= !0x04; }
    rp::report(SRC_STICK, dx, dy, wheel, hwheel, out);
    st.stick_buttons = buttons;
}
