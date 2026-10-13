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

use core::sync::atomic::{AtomicBool, Ordering};
use usb::hid::Event;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;

const EVDEV_KEYBOARD: u32 = 0;

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// LED state for the keyboards' enumeration-time SET_REPORT. Call before
/// `usb_hcd::init()` probes.
pub fn prepare() {
    usb::hid::set_initial_leds(usb::hid::hid_leds_from_console(console_leds()));
}

/// After enumeration: advertise the mouse's buttons and wheel on the pointer
/// node, size the motion scale, and start polling.
pub fn init() {
    if !usb::hid::present() { return; }
    crate::rel_pointer::init_scale();
    // REL_WHEEL, and BTN_LEFT..BTN_EXTRA (0x110..0x114, bits 0..4 of the
    // 0x110 bitmap byte) — merged with whatever virtio-tablet reported.
    crate::rel_pointer::advertise(0x1F, false, false);
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
        Event::Mouse { dx, dy, wheel, buttons, .. } => mouse(dx, dy, wheel, buttons),
    });
}

fn mouse(dx: i32, dy: i32, wheel: i32, buttons: u8) {
    // Detents → v120; the shared relative-pointer path integrates motion
    // into event1 and emits REL_WHEEL (and REL_WHEEL_HI_RES) like hid-input.
    crate::rel_pointer::report(crate::rel_pointer::SRC_USB, dx, dy, wheel * 120, 0, buttons);
}
