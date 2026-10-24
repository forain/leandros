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
    usb::hid::set_node_hooks(usb::hid::NodeHooks { attach: node_attach, detach: node_detach });
    usb::hid::set_initial_leds(usb::hid::hid_leds_from_console(console_leds()));
}

/// After enumeration: advertise the mouse's buttons and wheel on the pointer
/// node, size the motion scale, and start polling.
pub fn init() {
    let present = usb::hid::present();
    // A controller with nothing plugged in at boot is still polled: its
    // first keyboard or mouse arrives by hot-plug (lane udevhot2).
    if !present && usb::controller_count() == 0 { return; }
    crate::rel_pointer::init_scale();
    if present {
        // REL_WHEEL, and BTN_LEFT..BTN_EXTRA (0x110..0x114, bits 0..4 of the
        // 0x110 bitmap byte) — merged with whatever virtio-tablet reported.
        crate::rel_pointer::advertise(0x1F, false, false);
    }
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
    usb::hid::poll(leds, repeat, &mut |node, ev| match ev {
        // A device with its own node reaches userspace only through it (the
        // libudev stand-in hotplugs it into libinput); event0 then feeds just
        // the kernel's console/VT keyboard. Without a node (node pool full)
        // it is mirrored into event0/event1 as before.
        Event::Key { code, value } if node != usb::hid::NO_NODE => {
            evdev_server::push_event_console(EVDEV_KEYBOARD, EV_KEY, code, value);
            evdev_server::push_event(node, EV_KEY, code, value);
        }
        Event::KeySyn if node != usb::hid::NO_NODE => {
            evdev_server::push_event_console(EVDEV_KEYBOARD, EV_SYN, 0, 0);
            evdev_server::push_event(node, EV_SYN, 0, 0);
        }
        Event::Key { code, value } => evdev_server::push_event(EVDEV_KEYBOARD, EV_KEY, code, value),
        Event::KeySyn => evdev_server::push_event(EVDEV_KEYBOARD, EV_SYN, 0, 0),
        Event::Mouse { dx, dy, wheel, buttons, changed } => {
            if node != usb::hid::NO_NODE { mouse_node(node, dx, dy, wheel, buttons, changed); }
            else { mouse(dx, dy, wheel, buttons); }
        }
    });
}

/// Every HID interface also gets its own /dev/input/eventN (N >= 2) that
/// carries only its events and disappears on unplug, as on Linux. event0
/// (keyboards) and event1 (pointer) keep aggregating all of them: the
/// libudev shim hands libinput a fixed event0/event1 table and its monitor
/// never reports hot-plug, so the per-device nodes are for direct readers.
fn node_attach(protocol: u8, name: &str, vendor: u16, product: u16) -> u32 {
    let kind = match protocol {
        usb::hid::HID_PROTO_KEYBOARD => evdev_server::DEVKIND_KEYBOARD,
        usb::hid::HID_PROTO_MOUSE => evdev_server::DEVKIND_MOUSE,
        _ => return usb::hid::NO_NODE,
    };
    const BUS_USB: u16 = 0x03;
    evdev_server::register_input_device(kind, name, BUS_USB, vendor, product)
        .unwrap_or(usb::hid::NO_NODE)
}

fn node_detach(node: u32) { evdev_server::unregister_input_device(node); }

/// A boot-mouse report on the device's own node: raw REL_X/REL_Y/REL_WHEEL
/// and button edges, like hid-input on Linux.
fn mouse_node(node: u32, dx: i32, dy: i32, wheel: i32, buttons: u8, changed: u8) {
    const EV_REL: u16 = 2;
    if dx != 0 { evdev_server::push_event(node, EV_REL, 0, dx); }
    if dy != 0 { evdev_server::push_event(node, EV_REL, 1, dy); }
    if wheel != 0 { evdev_server::push_event(node, EV_REL, 8, wheel); }
    for bit in 0..5 {
        if changed & (1 << bit) != 0 {
            evdev_server::push_event(node, EV_KEY, usb::hid::mouse_button_code(bit),
                                     (buttons >> bit & 1) as i32);
        }
    }
    if dx != 0 || dy != 0 || wheel != 0 || changed != 0 { evdev_server::push_event(node, EV_SYN, 0, 0); }
}

fn mouse(dx: i32, dy: i32, wheel: i32, buttons: u8) {
    // Detents → v120; the shared relative-pointer path integrates motion
    // into event1 and emits REL_WHEEL (and REL_WHEEL_HI_RES) like hid-input.
    crate::rel_pointer::report(crate::rel_pointer::SRC_USB, dx, dy, wheel * 120, 0, buttons);
}
