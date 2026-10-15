//! Relative pointing devices (USB boot mice, the PS/2 mouse, a Synaptics
//! touchpad and its pass-through pointing stick) → the absolute pointer node.
//!
//! The kernel has no relative pointer node: the libudev shim and libinput's
//! device set are built around event0/event1, and libinput rejects a node
//! that is both absolute and relative. So relative motion is integrated into
//! an absolute position scaled to the boot framebuffer (one device pixel per
//! count, before the caller's own gain), and buttons and wheels go out on
//! event1 exactly as virtio-tablet sends them. Every relative source calls
//! [`report`], so they all share one cursor position.
//!
//! Wheels are taken in v120 units (120 = one detent, Linux's hi-res wheel
//! convention, positive = up / right). `REL_WHEEL_HI_RES` / `REL_HWHEEL_HI_RES`
//! go out on every movement; the legacy `REL_WHEEL` / `REL_HWHEEL` once a whole
//! detent has accumulated, as Linux's hid-input does. The hi-res bits are
//! only advertised by a source that scrolls smoothly (the touchpad), because
//! libinput >= 1.19 ignores legacy wheel events once they are; a virtio
//! tablet on the same node sends only legacy events, so the two never mix in
//! practice (virtio-input is QEMU, the touchpad is a laptop).
//!
//! Callers run in IRQ context on the bootstrap CPU (USB from the tick, PS/2
//! from IRQ 1/12); state is atomics only.

use core::sync::atomic::{AtomicI32, AtomicU8, Ordering};

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
pub const REL_HWHEEL: u16 = 6;
pub const REL_WHEEL: u16 = 8;
pub const REL_WHEEL_HI_RES: u16 = 11;
pub const REL_HWHEEL_HI_RES: u16 = 12;
const ABS_MAX: i32 = 32767;

const EVDEV_POINTER: u32 = 1;

/// Integrated pointer position, in the absolute node's 0..=32767 units.
static POS_X: AtomicI32 = AtomicI32::new(ABS_MAX / 2);
static POS_Y: AtomicI32 = AtomicI32::new(ABS_MAX / 2);
/// Absolute units per device count, ×256 (fixed point), per axis.
static SCALE_X: AtomicI32 = AtomicI32::new(256 * ABS_MAX / 1920);
static SCALE_Y: AtomicI32 = AtomicI32::new(256 * ABS_MAX / 1080);
/// Sub-detent wheel remainders (v120 units).
static WHEEL_ACC: AtomicI32 = AtomicI32::new(0);
static HWHEEL_ACC: AtomicI32 = AtomicI32::new(0);
/// Buttons currently down, per source, so a release from one source does
/// not lift a button another source still holds.
static HELD: [AtomicU8; 4] = [const { AtomicU8::new(0) }; 4];

/// Source ids for [`report`].
pub const SRC_USB: usize = 0;
pub const SRC_PS2: usize = 1;
pub const SRC_STICK: usize = 2;

/// Size the motion scale to the framebuffer (idempotent).
pub fn init_scale() {
    if let Some((_, w, h, _)) = crate::framebuffer::get_hardware_fb_info() {
        if w > 0 && h > 0 {
            SCALE_X.store(256 * ABS_MAX / w as i32, Ordering::Relaxed);
            SCALE_Y.store(256 * ABS_MAX / h as i32, Ordering::Relaxed);
        }
    }
}

/// Advertise what a source can send on the pointer node: `buttons` is the
/// BTN_LEFT.. bitmap byte (bit 0 = BTN_LEFT), `hires` adds the hi-res axes.
pub fn advertise(buttons: u8, hwheel: bool, hires: bool) {
    let mut rel = 1u32 << REL_WHEEL;
    if hwheel { rel |= 1 << REL_HWHEEL; }
    if hires {
        rel |= 1 << REL_WHEEL_HI_RES;
        if hwheel { rel |= 1 << REL_HWHEEL_HI_RES; }
    }
    evdev_server::add_device_caps(EVDEV_POINTER, rel, buttons);
}

/// BTN_* code for button bit `bit` (0 left, 1 right, 2 middle, 3 side, 4 extra).
fn button_code(bit: u8) -> u16 {
    match bit { 0 => 0x110, 1 => 0x111, 2 => 0x112, 3 => 0x113, _ => 0x114 }
}

/// One report from source `src`: motion in device counts (screen
/// orientation: +x right, +y down), wheels in v120 units (+ = up / right),
/// and the full button state (bit 0 left .. bit 4 extra).
pub fn report(src: usize, dx: i32, dy: i32, wheel: i32, hwheel: i32, buttons: u8) {
    let mut any = false;
    if dx != 0 {
        let x = (POS_X.load(Ordering::Relaxed) + dx * SCALE_X.load(Ordering::Relaxed) / 256).clamp(0, ABS_MAX);
        POS_X.store(x, Ordering::Relaxed);
        evdev_server::push_event(EVDEV_POINTER, EV_ABS, ABS_X, x);
        any = true;
    }
    if dy != 0 {
        let y = (POS_Y.load(Ordering::Relaxed) + dy * SCALE_Y.load(Ordering::Relaxed) / 256).clamp(0, ABS_MAX);
        POS_Y.store(y, Ordering::Relaxed);
        evdev_server::push_event(EVDEV_POINTER, EV_ABS, ABS_Y, y);
        any = true;
    }
    let src = src.min(HELD.len() - 1);
    let before_all = HELD.iter().fold(0u8, |a, h| a | h.load(Ordering::Relaxed));
    HELD[src].store(buttons & 0x1F, Ordering::Relaxed);
    let after_all = HELD.iter().fold(0u8, |a, h| a | h.load(Ordering::Relaxed));
    let changed = before_all ^ after_all;
    for bit in 0..5u8 {
        if changed & (1 << bit) != 0 {
            let v = (after_all >> bit) & 1;
            evdev_server::push_event(EVDEV_POINTER, EV_KEY, button_code(bit), v as i32);
            any = true;
        }
    }
    if wheel != 0 {
        any |= wheel_axis(&WHEEL_ACC, wheel, REL_WHEEL, REL_WHEEL_HI_RES);
    }
    if hwheel != 0 {
        any |= wheel_axis(&HWHEEL_ACC, hwheel, REL_HWHEEL, REL_HWHEEL_HI_RES);
    }
    if any { evdev_server::push_event(EVDEV_POINTER, EV_SYN, 0, 0); }
}

fn wheel_axis(acc: &AtomicI32, v120: i32, lo: u16, hi: u16) -> bool {
    evdev_server::push_event(EVDEV_POINTER, EV_REL, hi, v120);
    let mut a = acc.load(Ordering::Relaxed) + v120;
    let detents = a / 120;
    a -= detents * 120;
    acc.store(a, Ordering::Relaxed);
    if detents != 0 { evdev_server::push_event(EVDEV_POINTER, EV_REL, lo, detents); }
    true
}

/// Drop a sub-detent wheel remainder (end of a scroll gesture).
pub fn wheel_reset() {
    WHEEL_ACC.store(0, Ordering::Relaxed);
    HWHEEL_ACC.store(0, Ordering::Relaxed);
}
