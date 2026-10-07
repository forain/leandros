//! USB HID class driver: boot-protocol keyboards and mice.
//!
//! Scope: interfaces of class 3 with the boot subclass (1), protocol 1
//! (keyboard) or 2 (mouse), on devices attached directly to a root port.
//! Each is switched to the boot protocol at enumeration (SET_PROTOCOL 0) so
//! its reports have the fixed layout of HID 1.11 Appendix B and no report
//! descriptor has to be parsed:
//!
//! * keyboard: `[modifiers, reserved, key1..key6]` — diffed against the
//!   previous report into Linux key press/release edges;
//! * mouse: `[buttons, dx, dy, (wheel)]`.
//!
//! A report-protocol-only HID interface (subclass 0: many tablets, game
//! controllers, consumer-control keys) is logged as unsupported. A device with
//! several boot keyboard interfaces (some gaming keyboards) gets only the
//! first; its other interfaces are left unconfigured.
//!
//! The reports become [`Event`]s, delivered by [`poll`] from the BSP timer
//! tick to a callback the kernel supplies (the `usb` crate sits below the
//! evdev server). A held key is repeated as further value-1 presses (see
//! [`poll`]); a value-2 key event would collide with this tree's serial-byte
//! marker on the keyboard node.

use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;

use crate::xhci::{HidPipe, IntrPoll, IntrReports};
use crate::usb_log;

pub const HID_CLASS: u8 = 3;
pub const HID_SUBCLASS_BOOT: u8 = 1;
pub const HID_PROTO_KEYBOARD: u8 = 1;
pub const HID_PROTO_MOUSE: u8 = 2;

/// `UsbDeviceInfo::hid` bits.
pub const ROLE_KEYBOARD: u8 = 1;
pub const ROLE_MOUSE: u8 = 2;
/// A HID interface this driver does not handle (report protocol only).
pub const ROLE_HID_UNSUPPORTED: u8 = 0x80;

/// A boot HID interface found in a configuration descriptor.
#[derive(Clone, Copy, Debug, Default)]
pub struct HidIfaceDesc {
    pub iface: u8,
    pub alt: u8,
    pub protocol: u8,
    /// Interrupt IN endpoint address, wMaxPacketSize (raw, with the HS
    /// additional-transaction bits), bInterval.
    pub ep_addr: u8,
    pub mps: u16,
    pub interval: u8,
    /// SuperSpeed companion: bMaxBurst and wBytesPerInterval (0 if absent).
    pub ss_burst: u8,
    pub ss_bytes_per_interval: u16,
}

/// What [`parse_config`] found.
#[derive(Default)]
pub struct HidScan {
    /// Boot keyboard (first one only) and boot mouse interfaces, in
    /// descriptor order, each with an interrupt IN endpoint.
    pub boot: Vec<HidIfaceDesc>,
    /// Some HID interface could not be used (report protocol only, or no
    /// interrupt IN endpoint).
    pub unsupported: bool,
}

/// Walk a configuration descriptor for boot HID interfaces (alt setting 0
/// preferred; a later alt is used only for an interface number not seen yet).
pub fn parse_config(d: &[u8]) -> HidScan {
    let mut scan = HidScan::default();
    // (descriptor, is a boot kbd/mouse interface, is any HID interface)
    let mut cur: Option<(HidIfaceDesc, bool, bool)> = None;
    let mut last_was_our_ep = false;
    let mut have_kbd = false;
    let mut off = 0usize;
    let finish = |cur: &Option<(HidIfaceDesc, bool, bool)>, scan: &mut HidScan, have_kbd: &mut bool| {
        let Some((desc, boot, hid)) = cur else { return };
        if !*hid { return; }
        if !*boot || desc.ep_addr == 0 {
            scan.unsupported = true;
            return;
        }
        if scan.boot.iter().any(|b| b.iface == desc.iface) { return; }
        if desc.protocol == HID_PROTO_KEYBOARD {
            if *have_kbd { return; }
            *have_kbd = true;
        }
        scan.boot.push(*desc);
    };
    while off + 2 <= d.len() {
        let len = d[off] as usize;
        let ty = d[off + 1];
        if len < 2 || off + len > d.len() { break; }
        let b = &d[off..off + len];
        match ty {
            crate::descriptor::DT_INTERFACE if len >= 9 => {
                finish(&cur, &mut scan, &mut have_kbd);
                let (class, sub, proto) = (b[5], b[6], b[7]);
                let hid = class == HID_CLASS;
                let boot = hid && sub == HID_SUBCLASS_BOOT
                    && (proto == HID_PROTO_KEYBOARD || proto == HID_PROTO_MOUSE);
                cur = Some((HidIfaceDesc { iface: b[2], alt: b[3], protocol: proto, ..Default::default() }, boot, hid));
                last_was_our_ep = false;
            }
            crate::descriptor::DT_ENDPOINT if len >= 7 => {
                last_was_our_ep = false;
                if let Some((c, _, true)) = cur.as_mut() {
                    // Interrupt IN, first one only.
                    if b[3] & 0x3 == 3 && b[2] & 0x80 != 0 && c.ep_addr == 0 {
                        c.ep_addr = b[2];
                        c.mps = u16::from_le_bytes([b[4], b[5]]);
                        c.interval = b[6];
                        last_was_our_ep = true;
                    }
                }
            }
            crate::descriptor::DT_SS_ENDPOINT if len >= 6 => {
                if last_was_our_ep {
                    if let Some((c, _, _)) = cur.as_mut() {
                        c.ss_burst = b[2].min(15);
                        c.ss_bytes_per_interval = u16::from_le_bytes([b[4], b[5]]);
                    }
                }
                last_was_our_ep = false;
            }
            _ => {}
        }
        off += len;
    }
    finish(&cur, &mut scan, &mut have_kbd);
    scan
}

// ── HID usage → Linux key code ──────────────────────────────────────────────

/// Keyboard/Keypad page (0x07) usage → Linux `KEY_*`, the table Linux's
/// usbkbd/hid-input use for boot keyboards. 0 = no mapping.
#[rustfmt::skip]
static USAGE_TO_KEY: [u8; 256] = [
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
      0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,
      0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,
      0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,
     29, 42, 56,125, 97, 54,100,126,164,166,165,163,161,115,114,113,
    150,158,159,128,136,177,178,176,142,152,173,140,  0,  0,  0,  0,
];

pub fn usage_to_key(usage: u8) -> u16 { USAGE_TO_KEY[usage as usize] as u16 }

/// Linux button codes for boot-mouse button bits 0..4.
const MOUSE_BUTTONS: [u16; 5] = [0x110, 0x111, 0x112, 0x113, 0x114];

/// Input produced by a HID report.
#[derive(Clone, Copy, Debug)]
pub enum Event {
    /// Keyboard `EV_KEY` edge (value 0 or 1).
    Key { code: u16, value: i32 },
    /// End of one keyboard report's edges (`SYN_REPORT`).
    KeySyn,
    /// One mouse report: relative motion, wheel, and button edges
    /// (`changed` bits of `buttons`, Linux codes via [`mouse_button_code`]).
    Mouse { dx: i32, dy: i32, wheel: i32, buttons: u8, changed: u8 },
}

pub fn mouse_button_code(bit: usize) -> u16 { MOUSE_BUTTONS.get(bit).copied().unwrap_or(0) }

/// Diff two boot keyboard reports into key edges. Returns false (and emits
/// nothing) for a phantom/rollover report, which must not change the state.
pub fn diff_keyboard(prev: &[u8; 8], new: &[u8; 8], out: &mut dyn FnMut(Event)) -> bool {
    // ErrorRollOver (usage 1) in every key slot: too many keys held; the
    // report says nothing reliable about which.
    if new[2..8].iter().all(|&k| k == 0x01) { return false; }
    let mut any = false;
    for bit in 0..8 {
        let m = 1u8 << bit;
        if (prev[0] ^ new[0]) & m != 0 {
            out(Event::Key { code: usage_to_key(0xE0 + bit as u8), value: (new[0] & m != 0) as i32 });
            any = true;
        }
    }
    for &k in &prev[2..8] {
        if k > 3 && !new[2..8].contains(&k) {
            let code = usage_to_key(k);
            if code != 0 { out(Event::Key { code, value: 0 }); any = true; }
        }
    }
    for &k in &new[2..8] {
        if k > 3 && !prev[2..8].contains(&k) {
            let code = usage_to_key(k);
            if code != 0 { out(Event::Key { code, value: 1 }); any = true; }
        }
    }
    if any { out(Event::KeySyn); }
    true
}

/// HID LED report bits from the console's `LED_*` flags (`KDGETLED` layout:
/// Scroll=1, Num=2, Caps=4) — HID orders them Num=1, Caps=2, Scroll=4.
pub fn hid_leds_from_console(console: u8) -> u8 {
    ((console & 0x02 != 0) as u8) | ((console & 0x04 != 0) as u8) << 1 | ((console & 0x01 != 0) as u8) << 2
}

// ── Device registry, polled from the tick ───────────────────────────────────

struct HidDev {
    hp: HidPipe,
    name: String,
    prev: [u8; 8],
    buttons: u8,
    leds_sent: u8,
    gone: bool,
    /// Typematic: key being repeated (Linux code, 0 = none) and when next.
    rep_code: u16,
    rep_next: u64,
}

static DEVS: Mutex<Vec<HidDev>> = Mutex::new(Vec::new());
static HAVE_DEVS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Take ownership of configured HID pipes (enumeration, task context).
pub(crate) fn register(pipes: Vec<HidPipe>, name: &str, leds: u8) {
    if pipes.is_empty() { return; }
    let mut d = DEVS.lock();
    for hp in pipes {
        d.push(HidDev { hp, name: String::from(name), prev: [0; 8], buttons: 0, leds_sent: leds, gone: false,
                           rep_code: 0, rep_next: 0 });
    }
    HAVE_DEVS.store(true, core::sync::atomic::Ordering::Release);
}

/// Any HID device registered (cheap check for the tick).
pub fn present() -> bool { HAVE_DEVS.load(core::sync::atomic::Ordering::Acquire) }

/// Keys that never repeat: the locks (a held Caps Lock must not toggle) and
/// Pause.
fn repeats(usage: u8) -> bool { !matches!(usage, 0x39 | 0x47 | 0x48 | 0x53) }

/// Harvest every HID device's reports and deliver them as [`Event`]s;
/// `leds` (HID bit order) is pushed to keyboards whose LEDs differ.
/// `repeat` = (delay, period) in ms for the keyboard typematic: a held key is
/// re-sent as another press (value 1), the way a PS/2 keyboard and QEMU's
/// virtio keyboard deliver repeats. The console on VT 1 has no repeat of its
/// own and relies on that; text VTs and evdev clients see one more press of
/// a key that is already down. (0, _) disables it. IRQ context: try_lock
/// only, never waits.
pub fn poll(leds: u8, repeat: (u32, u32), out: &mut dyn FnMut(Event)) {
    let Some(mut devs) = DEVS.try_lock() else { return };
    let now = crate::platform::now_ns();
    let mut reports = IntrReports::new();
    for d in devs.iter_mut() {
        if d.gone { continue; }
        reports.n = 0;
        match d.hp.pipe.poll(&mut reports) {
            IntrPoll::Busy => continue,
            IntrPoll::Gone => {
                d.gone = true;
                usb_log!("[USB-HID] {} (slot {} interface {}) removed\n", d.name, d.hp.pipe.slot(), d.hp.pipe.interface());
                release_all(d, out);
                continue;
            }
            IntrPoll::Ok => {}
        }
        for i in 0..reports.n {
            let r = reports.get(i);
            match d.hp.desc.protocol {
                HID_PROTO_KEYBOARD => {
                    let mut new = [0u8; 8];
                    let n = r.len().min(8);
                    new[..n].copy_from_slice(&r[..n]);
                    if diff_keyboard(&d.prev, &new, out) {
                        // The newest newly pressed key repeats; its release
                        // (or another key going down) stops or re-targets it.
                        if let Some(&k) = new[2..8].iter().rev()
                            .find(|&&k| k > 3 && !d.prev[2..8].contains(&k)) {
                            d.rep_code = if repeats(k) { usage_to_key(k) } else { 0 };
                            d.rep_next = now + repeat.0 as u64 * 1_000_000;
                        } else if d.rep_code != 0
                            && !new[2..8].iter().any(|&k| k > 3 && usage_to_key(k) == d.rep_code) {
                            d.rep_code = 0;
                        }
                        d.prev = new;
                    }
                }
                HID_PROTO_MOUSE if r.len() >= 3 => {
                    let buttons = r[0] & 0x1F;
                    let wheel = if r.len() >= 4 { r[3] as i8 as i32 } else { 0 };
                    out(Event::Mouse {
                        dx: r[1] as i8 as i32, dy: r[2] as i8 as i32, wheel,
                        buttons, changed: buttons ^ d.buttons,
                    });
                    d.buttons = buttons;
                }
                _ => {}
            }
        }
        if d.rep_code != 0 && repeat.0 != 0 && repeat.1 != 0 && now >= d.rep_next {
            out(Event::Key { code: d.rep_code, value: 1 });
            out(Event::KeySyn);
            let p = repeat.1 as u64 * 1_000_000;
            d.rep_next = if d.rep_next + p > now { d.rep_next + p } else { now + p };
        }
        if d.hp.desc.protocol == HID_PROTO_KEYBOARD && d.leds_sent != leds && d.hp.pipe.leds_usable()
            && d.hp.pipe.set_report_nowait(&[leds]) {
            d.leds_sent = leds;
        }
    }
}

/// A device went away: release everything it was holding down.
fn release_all(d: &mut HidDev, out: &mut dyn FnMut(Event)) {
    match d.hp.desc.protocol {
        HID_PROTO_KEYBOARD => {
            let _ = diff_keyboard(&d.prev, &[0; 8], out);
            d.prev = [0; 8];
            d.rep_code = 0;
        }
        HID_PROTO_MOUSE if d.buttons != 0 => {
            out(Event::Mouse { dx: 0, dy: 0, wheel: 0, buttons: 0, changed: d.buttons });
            d.buttons = 0;
        }
        _ => {}
    }
}

/// LED state (HID bit order) sent to a keyboard at enumeration; the kernel
/// sets it from the console's lock state before probing.
static INITIAL_LEDS: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0x01);

pub fn set_initial_leds(leds: u8) { INITIAL_LEDS.store(leds, core::sync::atomic::Ordering::Relaxed); }
pub fn initial_leds() -> u8 { INITIAL_LEDS.load(core::sync::atomic::Ordering::Relaxed) }

/// Short description of a device's HID roles, for logs ("" if none).
pub fn role_name(hid: u8) -> &'static str {
    match hid & (ROLE_KEYBOARD | ROLE_MOUSE) {
        x if x == ROLE_KEYBOARD | ROLE_MOUSE => "HID keyboard+mouse",
        ROLE_KEYBOARD => "HID keyboard",
        ROLE_MOUSE => "HID mouse",
        _ if hid & ROLE_HID_UNSUPPORTED != 0 => "HID (report protocol only, unsupported)",
        _ => "",
    }
}
