//! USB HID keyboards and mice → the evdev nodes virtio-input feeds.
//!
//! `usb::hid` turns boot-protocol reports into key edges and mouse motion;
//! this glue pushes them into the same evdev devices the virtio-input driver
//! uses, so the console getty, Ctrl+Alt+Fn VT switching (recognised inside
//! `evdev_server::push_event`) and COSMIC see a USB keyboard exactly like the
//! virtio one:
//!
//! * keyboard → event0 (`EV_KEY` 0/1 + `SYN_REPORT`). A held key is
//!   repeated as further presses at the console's KDKBDREP rate, the way
//!   PS/2 typematic and QEMU's virtio keyboard deliver repeats (value 2 on
//!   the keyboard node is this tree's serial-byte marker, so never that);
//! * mouse → event1, the absolute pointer node. The kernel has no relative
//!   pointer node (the libudev shim and libinput's device set are built
//!   around event0/event1, and libinput rejects a node that is both absolute
//!   and relative), so relative motion is integrated into an absolute
//!   position scaled to the boot framebuffer, one device pixel per count.
//!   Buttons and the wheel go out as on the virtio tablet.
//!
//! Keyboard LEDs follow the lock state of the VT on screen
//! (`tty_server::keyboard::get_led`) or, while a compositor owns the keyboard,
//! the EV_LED events it writes to event0 (`evdev_server::client_leds`); they
//! are sent as a HID output report whenever they change.
//!
//! [`poll`] runs on every BSP timer tick (100 Hz, IRQ context), next to
//! `virtio_keyboard::poll_events` and before the tick's
//! `flush_pending_wake`. It only try-locks; a tick that finds the xHCI
//! controller busy (block I/O in progress) is skipped and the reports wait,
//! queued in the controller, for the next one.

use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use usb::hid::Event;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EV_ABS: u16 = 3;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const REL_WHEEL: u16 = 8;
const ABS_MAX: i32 = 32767;

const EVDEV_KEYBOARD: u32 = 0;
const EVDEV_POINTER: u32 = 1;

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Integrated pointer position, in the absolute node's 0..=32767 units.
static POS_X: AtomicI32 = AtomicI32::new(ABS_MAX / 2);
static POS_Y: AtomicI32 = AtomicI32::new(ABS_MAX / 2);
/// Absolute units per device count, ×256 (fixed point), per axis.
static SCALE_X: AtomicI32 = AtomicI32::new(256 * ABS_MAX / 1920);
static SCALE_Y: AtomicI32 = AtomicI32::new(256 * ABS_MAX / 1080);

/// LED state for the keyboards' enumeration-time SET_REPORT. Call before
/// `usb_hcd::init()` probes.
pub fn prepare() {
    usb::hid::set_initial_leds(usb::hid::hid_leds_from_console(console_leds()));
}

/// After enumeration: advertise the mouse's buttons and wheel on the pointer
/// node, size the motion scale, and start polling.
pub fn init() {
    if !usb::hid::present() { return; }
    if let Some((_, w, h, _)) = crate::framebuffer::get_hardware_fb_info() {
        if w > 0 && h > 0 {
            SCALE_X.store(256 * ABS_MAX / w as i32, Ordering::Relaxed);
            SCALE_Y.store(256 * ABS_MAX / h as i32, Ordering::Relaxed);
        }
    }
    // REL_WHEEL, and BTN_LEFT..BTN_EXTRA (0x110..0x114, bits 0..4 of the
    // 0x110 bitmap byte) — merged with whatever virtio-tablet reported.
    evdev_server::add_device_caps(EVDEV_POINTER, 1 << REL_WHEEL, 0x1F);
    ACTIVE.store(true, Ordering::Release);
}

fn console_leds() -> u8 {
    let vt = tty_server::vt::active().saturating_sub(1);
    tty_server::keyboard::get_led(vt)
}

/// The LEDs to show (HID bit order): the text console's lock state while it
/// owns the keyboard, else what the compositor last wrote to the node.
fn wanted_leds() -> u8 {
    let console = usb::hid::hid_leds_from_console(console_leds());
    if tty_server::vt::console_keyboard_active() { return console; }
    evdev_server::client_leds().unwrap_or(console)
}

/// Timer-tick hook (BSP, IRQ context).
pub fn poll() {
    if !ACTIVE.load(Ordering::Acquire) { return; }
    let leds = wanted_leds();
    let repeat = tty_server::keyboard::repeat_params();
    usb::hid::poll(leds, repeat, &mut |ev| match ev {
        Event::Key { code, value } => evdev_server::push_event(EVDEV_KEYBOARD, EV_KEY, code, value),
        Event::KeySyn => evdev_server::push_event(EVDEV_KEYBOARD, EV_SYN, 0, 0),
        Event::Mouse { dx, dy, wheel, buttons, changed } => mouse(dx, dy, wheel, buttons, changed),
    });
}

fn mouse(dx: i32, dy: i32, wheel: i32, buttons: u8, changed: u8) {
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
    for bit in 0..5 {
        if changed & (1 << bit) != 0 {
            let v = (buttons >> bit) & 1;
            evdev_server::push_event(EVDEV_POINTER, EV_KEY, usb::hid::mouse_button_code(bit), v as i32);
            any = true;
        }
    }
    if wheel != 0 {
        evdev_server::push_event(EVDEV_POINTER, EV_REL, REL_WHEEL, wheel);
        any = true;
    }
    if any { evdev_server::push_event(EVDEV_POINTER, EV_SYN, 0, 0); }
}
