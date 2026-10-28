//! USB subsystem.
//!
//! Architecture:
//!   platform   — clock / yield / log hooks installed by the kernel
//!   dma        — `DmaBuf`, the only memory ever handed to a controller
//!   transport  — `BulkTransport` + `UsbErr`, the seam to the class driver
//!   xhci       — xHCI host controller driver (polled, root ports only)
//!   selftest   — raw BOT self-test over any `BulkTransport`
//!   hid        — boot-protocol keyboard / mouse class driver, polled from
//!                the timer tick through xHCI interrupt pipes
//!   descriptor, endpoint, device, transfer, hub, hcd — protocol types and
//!                the older (not yet wired) hub/HCD model
//!
//! Entry points for the kernel: [`probe_xhci`] once per controller, then
//! [`mass_storage_devices`] to take the bulk-only transports and
//! [`devices`] for `lsusb`.

#![no_std]

extern crate alloc;

pub mod platform;
pub mod dma;
pub mod transport;
pub mod selftest;
pub mod descriptor;
pub mod device;
pub mod endpoint;
pub mod hcd;
pub mod hid;
pub mod hub;
pub mod hubproto;
pub mod msc;
pub mod transfer;
pub mod xhci;
pub mod uvc;

pub use descriptor::*;
pub use device::UsbDevice;
pub use endpoint::{Endpoint, TransferType, Direction};
pub use hcd::HostControllerDriver;
pub use transfer::{Urb, UrbStatus, TransferFlags};
pub use transport::{BulkTransport, UsbErr};
pub use xhci::XhciBulk;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;

/// One enumerated device (for `lsusb` and logs).
#[derive(Clone, Debug)]
pub struct UsbDeviceInfo {
    /// Controller number, 1-based.
    pub bus: u8,
    /// Root-hub port, 1-based.
    pub port: u8,
    /// Route string below the root port (0 = on it; one nibble per hub tier).
    pub route: u32,
    pub slot: u8,
    /// USB device address assigned by the controller.
    pub address: u8,
    pub vendor: u16,
    pub product: u16,
    /// Port speed ID (xhci::context::SPEED_*).
    pub speed: u8,
    /// bDeviceClass, or the first interface's class when that is 0.
    pub class: u8,
    pub manufacturer: String,
    pub product_name: String,
    /// A bulk-only mass-storage interface was configured.
    pub mass_storage: bool,
    pub interface: u8,
    /// HID roles configured (`hid::ROLE_*` bits; 0 = not a HID device).
    pub hid: u8,
}

type Controller = Arc<Mutex<xhci::Xhci>>;

static CONTROLLERS: Mutex<Vec<Controller>> = Mutex::new(Vec::new());
static DEVICES: Mutex<Vec<UsbDeviceInfo>> = Mutex::new(Vec::new());
static MASS_STORAGE: Mutex<Vec<XhciBulk>> = Mutex::new(Vec::new());

/// Bring up the xHCI controller whose BAR is mapped at `base` (`len`
/// bytes), enumerate its root ports, print one lsusb-style line per device,
/// run the BOT self-test if enabled, and register the results. Returns the
/// number of devices found.
///
/// # Safety
/// `base..base+len` must be the controller's BAR mapped as device memory,
/// with PCI memory decoding and bus mastering enabled.
pub unsafe fn probe_xhci(base: usize, len: usize) -> Result<usize, &'static str> {
    let bus = (CONTROLLERS.lock().len() + 1) as u8;
    let (hc, found) = xhci::probe(base, len, bus)?;
    let mut n = found.len();
    let mut bulks = Vec::new();
    for e in found {
        if let Some(b) = adopt(bus, e, false) { bulks.push(b); }
    }
    CONTROLLERS.lock().push(hc.clone());
    // Devices behind hubs found at boot (nested hubs come up in the same pass).
    let mut ev = Vec::new();
    n += scan_hubs(bus, &hc, &mut ev);
    for e in ev {
        if let HotEvent::Added(b) = e { bulks.push(b); }
    }
    for mut b in bulks {
        if platform::selftest() {
            let (bus, slot) = b.location();
            let mut label = String::new();
            let _ = core::fmt::Write::write_fmt(&mut label, format_args!("bus {} slot {}", bus, slot));
            selftest::run(&mut b, &label);
        }
        MASS_STORAGE.lock().push(b);
    }
    Ok(n)
}

/// "port 3", or "port 3.1.2" behind hubs (Linux's sysfs port path).
fn loc_name(port: u8, route: u32) -> String {
    let mut s = String::new();
    let _ = core::fmt::Write::write_fmt(&mut s, format_args!("port {}", port));
    let mut r = route;
    while r & 0xF != 0 {
        let _ = core::fmt::Write::write_fmt(&mut s, format_args!(".{}", r & 0xF));
        r >>= 4;
    }
    s
}

/// Register a freshly enumerated device: log it, record it, hand its HID
/// pipes to the HID poll and its hub ports to the hot-plug scan. Returns its
/// mass-storage transport.
fn adopt(bus: u8, e: xhci::Enumerated, hot: bool) -> Option<XhciBulk> {
    let xhci::Enumerated { info: i, bulk, hid: pipes, hub, audio } = e;
    let hid_role = hid::role_name(i.hid);
    usb_log!("[USB] Bus {:03} Device {:03}: ID {:04x}:{:04x} {} {} [{}{}, {}, class {:02x}{}{}{}]\n",
             i.bus, i.address, i.vendor, i.product,
             if i.manufacturer.is_empty() { "?" } else { &i.manufacturer },
             if i.product_name.is_empty() { "?" } else { &i.product_name },
             if hot { "connected on " } else { "" }, loc_name(i.port, i.route),
             xhci::context::speed_name(i.speed), i.class,
             if i.mass_storage { ", bulk-only mass storage" } else { "" },
             if hid_role.is_empty() { "" } else { ", " }, hid_role);
    if let Some(h) = hub {
        usb_log!("[USB] Bus {:03} Device {:03}: hub with {} ports\n", i.bus, i.address, h.nports);
        HUBS.lock().push((bus, h));
    }
    if !pipes.is_empty() {
        let name = if i.product_name.is_empty() { "USB HID device" } else { i.product_name.as_str() };
        hid::register(pipes, name, hid::initial_leds(), i.vendor, i.product);
    }
    DEVICES.lock().push(i);
    // A camera arrives through the same isochronous-function path.
    let (video, audio) = match audio {
        Some(f) if uvc::has_video_streaming(&f.cfg) => (Some(f), None),
        a => (None, a),
    };
    if let Some(f) = video {
        usb_log!("[USB] bus {} slot {}: video function (UVC)\n", bus, f.slot);
        uvc::register(f);
    }
    if let Some(f) = audio {
        usb_log!("[USB] bus {} slot {}: audio function, {} isochronous endpoint(s)\n",
                 bus, f.slot, f.isoch_endpoints().len());
        xhci::isoch::register_audio(f);
    }
    bulk
}

/// A change seen by [`hotplug_scan`].
pub enum HotEvent {
    /// The mass-storage device on (bus, slot) is gone. Its slot is still
    /// allocated (transfers on it already fail): the class driver retires
    /// its disk, then calls [`release_slot`].
    Gone { bus: u8, slot: u8 },
    /// A new device came up with a bulk-only mass-storage interface.
    Added(XhciBulk),
}

/// Configured hubs (bus, ports), polled by [`hotplug_scan`].
static HUBS: Mutex<Vec<(u8, xhci::HubPorts)>> = Mutex::new(Vec::new());

/// (bus, root port, route) whose device failed to enumerate: not retried
/// until the port reports a disconnect or a connection change.
static HOT_FAILED: Mutex<Vec<(u8, u8, u32)>> = Mutex::new(Vec::new());

/// Whether `route` is `at` or a port below it (same root port).
fn under(route: u32, at: u32) -> bool {
    let tiers = (0..5).take_while(|i| (at >> (4 * i)) & 0xF != 0).count() as u32;
    let mask = if tiers == 0 { 0 } else { (1u32 << (4 * tiers)) - 1 };
    route & mask == at
}

/// The device at (bus, root port, route) and everything behind it left: their
/// slots are marked gone (transfers fail at once), HID pipes detached and
/// their slots freed, hubs dropped from the scan, and mass storage reported
/// to its driver.
fn detach(bus: u8, hc: &Controller, port: u8, route: u32, ev: &mut Vec<HotEvent>) {
    let gone: Vec<UsbDeviceInfo> = {
        let mut d = DEVICES.lock();
        let (g, keep): (Vec<_>, Vec<_>) = d.drain(..)
            .partition(|x| x.bus == bus && x.port == port && under(x.route, route));
        *d = keep;
        g
    };
    HUBS.lock().retain(|(b, h)| !(*b == bus && h.root_port == port && under(h.route, route)));
    for i in &gone {
        xhci::mark_slot_gone(hc, i.slot);
        usb_log!("[USB] Bus {:03} Device {:03}: disconnected from {}\n", bus, i.address, loc_name(port, i.route));
        if i.mass_storage {
            ev.push(HotEvent::Gone { bus, slot: i.slot });
        } else {
            if i.hid != 0 { hid::detach(hc, i.slot); }
            xhci::isoch::detach(hc, i.slot);
            uvc::detach(hc, i.slot);
            xhci::release_slot(hc, i.slot);
            usb_log!("[USB] bus {} slot {}: released\n", bus, i.slot);
        }
    }
}

/// One port (root port, or hub port at `route`): `ccs` = connected now,
/// `changed` = its connect-change latch was set (and has been cleared).
fn scan_one(bus: u8, hc: &Controller, port: u8, route: u32, ccs: bool, changed: bool,
            ev: &mut Vec<HotEvent>, enumerate: impl FnOnce() -> Result<xhci::Enumerated, &'static str>) -> bool {
    let known = DEVICES.lock().iter().any(|d| d.bus == bus && d.port == port && d.route == route);
    if known {
        if ccs && !changed { return false; }
        if ccs {
            usb_log!("[USB] bus {} {}: connection changed since the last scan (re-plugged)\n", bus, loc_name(port, route));
        }
        detach(bus, hc, port, route, ev);
    }
    if changed || !ccs { HOT_FAILED.lock().retain(|&f| f != (bus, port, route)); }
    if !ccs || HOT_FAILED.lock().contains(&(bus, port, route)) { return false; }
    match enumerate() {
        Ok(e) => {
            if let Some(b) = adopt(bus, e, true) { ev.push(HotEvent::Added(b)); }
            true
        }
        Err(why) => {
            usb_log!("[USB] bus {} {}: hot-plug enumeration failed: {}\n", bus, loc_name(port, route), why);
            HOT_FAILED.lock().push((bus, port, route));
            false
        }
    }
}

/// Poll every hub port of `bus` (GET_STATUS). A hub enumerated during the
/// pass is scanned in the same pass. Returns the number of devices added.
fn scan_hubs(bus: u8, hc: &Controller, ev: &mut Vec<HotEvent>) -> usize {
    let mut done: Vec<u8> = Vec::new();
    let mut added = 0;
    loop {
        let next = HUBS.lock().iter().find(|(b, h)| *b == bus && !done.contains(&h.slot)).map(|x| x.1);
        let Some(h) = next else { break };
        done.push(h.slot);
        for p in 1..=h.nports {
            if !HUBS.lock().iter().any(|(b, x)| *b == bus && x.slot == h.slot) { break; }
            let Some((st, ch)) = xhci::hub_port_status(hc, &h, p) else { break };
            let changed = ch & xhci::HUB_PC_CONNECTION != 0;
            if changed { xhci::hub_clear_port_feature(hc, &h, p, xhci::HUB_FEAT_C_CONNECTION); }
            let ccs = st & xhci::HUB_PS_CONNECTION != 0;
            if scan_one(bus, hc, h.root_port, h.child_route(p), ccs, changed, ev,
                        || xhci::enumerate_hub_port(hc, &h, p)) {
                added += 1;
            }
        }
    }
    added
}

/// Compare every root port and hub port with the device list (lanes
/// usbunplug, usbhot2): a port whose connection dropped, or whose
/// connect-change latch (PORTSC.CSC, hub C_PORT_CONNECTION) shows a
/// disconnect + re-connect since the last scan, loses its device and
/// everything behind it; a newly connected port is enumerated.
/// Sleeps (port reset, enumeration): task context only.
pub fn hotplug_scan() -> Vec<HotEvent> {
    let ctrls: Vec<Controller> = CONTROLLERS.lock().clone();
    let mut ev = Vec::new();
    for (i, hc) in ctrls.iter().enumerate() {
        let bus = (i + 1) as u8;
        for port in 1..=xhci::root_ports(hc) {
            let changed = xhci::root_port_take_csc(hc, port);
            let ccs = xhci::root_port_connected(hc, port);
            scan_one(bus, hc, port, 0, ccs, changed, &mut ev, || xhci::enumerate_hotplug(hc, port));
        }
        scan_hubs(bus, hc, &mut ev);
    }
    ev
}

/// Free the slot of a device reported by [`HotEvent::Gone`].
pub fn release_slot(bus: u8, slot: u8) {
    let hc = CONTROLLERS.lock().get(bus as usize - 1).cloned();
    if let Some(hc) = hc {
        if xhci::release_slot(&hc, slot) {
            usb_log!("[USB] bus {} slot {}: released\n", bus, slot);
        }
    }
}

/// Controllers brought up by [`probe_xhci`], with or without devices.
pub fn controller_count() -> usize {
    CONTROLLERS.lock().len()
}

pub fn devices() -> Vec<UsbDeviceInfo> {
    DEVICES.lock().clone()
}

/// Number of bulk-only mass-storage transports not yet taken.
pub fn mass_storage_count() -> usize {
    MASS_STORAGE.lock().len()
}

/// Take the transports of every enumerated bulk-only mass-storage
/// interface. Each transport owns its endpoints, so they are handed out
/// once: a second call returns only interfaces enumerated since.
pub fn mass_storage_devices() -> Vec<XhciBulk> {
    core::mem::take(&mut *MASS_STORAGE.lock())
}
