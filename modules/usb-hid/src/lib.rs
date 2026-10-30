//! USB boot keyboards and mice as a loadable module: the SAME HID layer as the built-in
//! (`drivers/usb/src/hid/mod.rs`: report parsing, typematic, per-device registry), with a small
//! `xhci` shim mapping its interrupt pipes onto the kapi (`kapi_usb_intr_*`) and the glue of
//! `drivers/src/usb_hid.rs` pushing events through `kapi_input_*` / `kapi_rel_pointer_*`.
//!
//! Binding: `usb ffff ffff 03 01 01|02` aliases load this module when the enumerator meets a boot
//! keyboard or mouse (CONFIG_USB_HID=m leaves HID out of the enumerator, which hands the
//! configured function to the class registry like any other class). `probe` opens every boot
//! interface of the function; polling runs from `kapi_tick_register` (100 Hz, IRQ context).
#![no_std]
#![allow(dead_code, unused_imports)]

extern crate alloc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use kmod_abi::{KUsbIf, KUsbSetup, KmodDevId};
use kmod_kapi::{input, usb as kusb};

macro_rules! usb_log { ($($a:tt)*) => { kmod_rt::kinfo!($($a)*) } }
pub(crate) use usb_log;

mod platform {
    pub fn now_ns() -> u64 { kmod_kapi::sched::monotonic_ns() }
}

/// The names `hid/mod.rs` expects from the xHCI driver, over the kapi handles.
mod xhci {
    use crate::hid::HidIfaceDesc;
    use kmod_kapi::usb as kusb;

    pub const NTDS: usize = 8;
    pub const REPORT_MAX: usize = 64;

    pub struct Reports { pub n: usize, pub len: [u8; NTDS], pub data: [[u8; REPORT_MAX]; NTDS] }
    impl Reports {
        pub const fn new() -> Self { Reports { n: 0, len: [0; NTDS], data: [[0; REPORT_MAX]; NTDS] } }
        pub fn get(&self, i: usize) -> &[u8] { &self.data[i][..self.len[i] as usize] }
    }
    pub use Reports as IntrReports;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub enum Poll { Ok, Busy, Gone }
    pub use Poll as IntrPoll;

    pub struct IntrPipe { pub fh: u64, ph: u64, slot: u8, iface: u8, led_dead: bool, gone: bool }

    impl IntrPipe {
        pub fn open(fh: u64, slot: u8, d: &HidIfaceDesc) -> Result<IntrPipe, i32> {
            let ph = kusb::intr_open(fh, d.ep_addr, REPORT_MAX)?;
            Ok(IntrPipe { fh, ph, slot, iface: d.iface, led_dead: false, gone: false })
        }
        pub fn slot(&self) -> u8 { self.slot }
        pub fn interface(&self) -> u8 { self.iface }
        pub fn close(self) { kusb::intr_close(self.ph) }
        pub fn leds_usable(&self) -> bool { !self.led_dead }
        pub fn set_report_nowait(&mut self, data: &[u8]) -> bool {
            match kusb::intr_out(self.ph, data) {
                Ok(()) => true,
                Err(-95) => { self.led_dead = true; false }
                Err(_) => false,
            }
        }
        pub fn poll(&mut self, out: &mut Reports) -> Poll {
            out.n = 0;
            if self.gone { return Poll::Gone; }
            let mut buf = [0u8; REPORT_MAX];
            while out.n < NTDS {
                match kusb::intr_poll(self.ph, &mut buf) {
                    Ok(n) => {
                        let k = n.min(REPORT_MAX);
                        out.data[out.n][..k].copy_from_slice(&buf[..k]);
                        out.len[out.n] = k as u8;
                        out.n += 1;
                    }
                    Err(-16) => { if out.n == 0 { return Poll::Busy; } break; }
                    Err(-11) => break,
                    Err(_) => { self.gone = true; if out.n == 0 { return Poll::Gone; } break; }
                }
            }
            Poll::Ok
        }
    }

    pub struct HidPipe { pub desc: HidIfaceDesc, pub pipe: IntrPipe }
}

#[path = "../../../drivers/usb/src/hid/mod.rs"]
mod hid;

use hid::Event;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const EVDEV_KEYBOARD: u32 = 0;

static DONE: AtomicBool = AtomicBool::new(false);
static ADVERTISED: AtomicBool = AtomicBool::new(false);

const IDS: [KmodDevId; 2] = [
    kmod_rt::usb(0xffff, 0xffff, 0x03_01_01, 0xFF_FF_FF),
    kmod_rt::usb(0xffff, 0xffff, 0x03_01_02, 0xFF_FF_FF),
];

kmod_rt::module! {
    name: "usb_hid",
    init: init,
    ids: [kmod_rt::usb(0xffff, 0xffff, 0x03_01_01, 0xFF_FF_FF), kmod_rt::usb(0xffff, 0xffff, 0x03_01_02, 0xFF_FF_FF)],
}

fn node_attach(protocol: u8, name: &str, vendor: u16, product: u16) -> u32 {
    let kind = match protocol {
        hid::HID_PROTO_KEYBOARD => input::INPUT_DEV_KEYBOARD,
        hid::HID_PROTO_MOUSE => input::INPUT_DEV_MOUSE,
        _ => return hid::NO_NODE,
    };
    const BUS_USB: u16 = 0x03;
    let n = input::register(kind, name, BUS_USB, vendor, product);
    if n < 0 { hid::NO_NODE } else { n as u32 }
}

fn node_detach(node: u32) { input::unregister(node); }

fn mouse_node(node: u32, dx: i32, dy: i32, wheel: i32, buttons: u8, changed: u8) {
    if dx != 0 { input::push_event(node, EV_REL, 0, dx); }
    if dy != 0 { input::push_event(node, EV_REL, 1, dy); }
    if wheel != 0 { input::push_event(node, EV_REL, 8, wheel); }
    for bit in 0..5 {
        if changed & (1 << bit) != 0 {
            input::push_event(node, EV_KEY, hid::mouse_button_code(bit), (buttons >> bit & 1) as i32);
        }
    }
    if dx != 0 || dy != 0 || wheel != 0 || changed != 0 { input::push_event(node, EV_SYN, 0, 0); }
}

extern "C" fn tick(_: usize) {
    if !hid::present() { return; }
    let leds = input::leds();
    let repeat = input::repeat();
    hid::poll(leds, repeat, &mut |node, ev| match ev {
        Event::Key { code, value } if node != hid::NO_NODE => {
            input::push_console(EV_KEY, code, value);
            input::push_event(node, EV_KEY, code, value);
        }
        Event::KeySyn if node != hid::NO_NODE => {
            input::push_console(EV_SYN, 0, 0);
            input::push_event(node, EV_SYN, 0, 0);
        }
        Event::Key { code, value } => input::push_event(EVDEV_KEYBOARD, EV_KEY, code, value),
        Event::KeySyn => input::push_event(EVDEV_KEYBOARD, EV_SYN, 0, 0),
        Event::Mouse { dx, dy, wheel, buttons, changed } => {
            if node != hid::NO_NODE { mouse_node(node, dx, dy, wheel, buttons, changed); }
            else { input::rel_report(input::REL_SRC_USB, dx, dy, wheel * 120, 0, buttons); }
        }
    });
}

fn setup(handle: u64, rt: u8, req: u8, value: u16, iface: u8, data: &mut [u8]) -> Result<usize, i32> {
    let s = KUsbSetup { request_type: rt, request: req, value, index: iface as u16, length: data.len() as u16 };
    kusb::ctrl(handle, &s, data, 5000)
}

extern "C" fn probe(intf: *const KUsbIf, _ctx: usize) -> i32 {
    let i = unsafe { &*intf };
    let cfg = match kusb::config_descriptor(i.handle) { Ok(c) => c, Err(_) => return -5 };
    let scan = hid::parse_config(&cfg);
    let leds = input::leds();
    let mut pipes: Vec<xhci::HidPipe> = Vec::new();
    for d in &scan.boot {
        let pipe = match xhci::IntrPipe::open(i.handle, i.slot, d) {
            Ok(p) => p,
            Err(e) => { kmod_rt::kerr!("[USB-HID] slot {} interface {}: interrupt open failed ({})", i.slot, d.iface, e); continue; }
        };
        // SET_PROTOCOL(boot) is mandatory for a boot-subclass interface; without it the reports
        // may not be in boot format. SET_IDLE(0) may legitimately STALL on mice.
        if setup(i.handle, 0x21, 0x0B, 0, d.iface, &mut []).is_err() {
            kmod_rt::kinfo!("[USB-HID] slot {} interface {}: SET_PROTOCOL(boot) failed, interface skipped", i.slot, d.iface);
            pipe.close();
            continue;
        }
        let _ = setup(i.handle, 0x21, 0x0A, 0, d.iface, &mut []);
        if d.protocol == hid::HID_PROTO_KEYBOARD {
            let _ = setup(i.handle, 0x21, 0x09, 0x0200, d.iface, &mut [leds]);
        }
        pipes.push(xhci::HidPipe { desc: *d, pipe });
    }
    if pipes.is_empty() { return -19; }
    let mouse = pipes.iter().any(|p| p.desc.protocol == hid::HID_PROTO_MOUSE);
    let name = match (pipes.iter().any(|p| p.desc.protocol == hid::HID_PROTO_KEYBOARD), mouse) {
        (true, true) => "USB HID keyboard+mouse",
        (true, false) => "USB HID keyboard",
        _ => "USB HID mouse",
    };
    if ADVERTISED.swap(true, Ordering::SeqCst) == false {
        // REL_WHEEL, and BTN_LEFT..BTN_EXTRA, merged with what the virtio tablet reported.
        input::rel_advertise(0x1F, false, false);
    }
    kmod_rt::kinfo!("[USB-HID] {} (slot {}), {} interface(s)", name, i.slot, pipes.len());
    hid::register(pipes, name, leds, i.vid, i.pid);
    0
}

extern "C" fn detach(handle: u64, _ctx: usize) { hid::detach_where(|p| p.pipe.fh == handle); }

fn init() -> i32 {
    if DONE.swap(true, Ordering::SeqCst) { return -16; }
    hid::set_node_hooks(hid::NodeHooks { attach: node_attach, detach: node_detach });
    hid::set_initial_leds(input::leds());
    if kmod_kapi::tick::register(tick, 0).is_err() { return -12; }
    if let Err(e) = kusb::class_register(&IDS, probe, Some(detach), 0) {
        kmod_rt::kerr!("usb_class_register failed: {}", e);
        return e;
    }
    0
}
