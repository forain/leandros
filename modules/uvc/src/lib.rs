//! USB Video Class as a loadable module: the SAME driver source as the built-in
//! (`drivers/usb/src/uvc.rs`), with `uvcenv` mapping its USB needs onto the kapi handle calls
//! (`kapi_usb_ctrl`, `kapi_usb_set_alt`, the isochronous open/submit/reap/close).
//!
//! Binding: the `usb ffff ffff 0e 02 *` alias makes the kernel load this module when the
//! enumerator meets a video-streaming interface nobody wants yet; `init` registers the class
//! driver, the enumerator then configures the device and offers it. Cameras reach /dev/video0
//! through `kapi_v4l2_register`. The `uvc.fake=1` camera (no USB device) is registered from
//! `init`, so `modules.load=uvc uvc.fake=1` exercises the module without hardware.
#![no_std]
#![allow(dead_code, unused_imports)]

extern crate alloc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use kmod_abi::{KUsbIf, KmodDevId, KmodId};

#[path = "../../../drivers/usb/src/isocdesc.rs"]
mod isocdesc;
#[path = "../../../drivers/usb/src/transport.rs"]
mod transport;

/// The module's view of the USB stack, same names as the kernel's `usb::uvcenv`.
mod uvcenv {
    use alloc::vec::Vec;
    use kmod_kapi::usb as kusb;
    pub use crate::isocdesc::{scan_isoch, IsochEpDesc};
    pub use crate::transport::UsbErr;

    macro_rules! usb_log { ($($a:tt)*) => { kmod_rt::kinfo!($($a)*) } }
    pub(crate) use usb_log;

    pub mod platform {
        pub fn now_ns() -> u64 { kmod_kapi::sched::monotonic_ns() }
        pub fn relax() { kmod_kapi::sched::kthread::udelay(50) }
    }

    /// Timeout of 3 s class requests, as in the kernel build (xhci `CLASS_CTRL_TIMEOUT_MS`).
    const CTRL_TIMEOUT_MS: u32 = 5000;

    pub struct Deadline(u64);
    impl Deadline {
        pub fn after_ms(ms: u32) -> Self { Deadline(platform::now_ns() + ms as u64 * 1_000_000) }
        pub fn expired(&self) -> bool { platform::now_ns() >= self.0 }
    }

    /// What the probe was told about the interface that matched.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct Match { pub bus: u8, pub vid: u16, pub pid: u16, pub class: u8, pub subclass: u8, pub protocol: u8, pub iface: u8 }

    fn err(e: i32) -> UsbErr {
        match e { -32 => UsbErr::Stall, -110 => UsbErr::Timeout, -75 => UsbErr::Babble, -19 => UsbErr::Disconnected, _ => UsbErr::Other(0xFF) }
    }

    /// A claimed device function: the kernel's handle plus its configuration descriptor.
    pub struct UsbFunction { pub handle: u64, pub slot: u8, pub cfg: Vec<u8> }

    impl UsbFunction {
        fn setup(rt: u8, req: u8, value: u16, index: u16, len: usize) -> kmod_abi::KUsbSetup {
            kmod_abi::KUsbSetup { request_type: rt, request: req, value, index, length: len as u16 }
        }
        pub fn control_out(&self, rt: u8, req: u8, value: u16, index: u16, data: &[u8]) -> Result<(), UsbErr> {
            let mut b = Vec::from(data);
            kusb::ctrl(self.handle, &Self::setup(rt & 0x7F, req, value, index, b.len()), &mut b, CTRL_TIMEOUT_MS)
                .map(|_| ()).map_err(err)
        }
        pub fn control_in(&self, rt: u8, req: u8, value: u16, index: u16, buf: &mut [u8]) -> Result<usize, UsbErr> {
            kusb::ctrl(self.handle, &Self::setup(rt | 0x80, req, value, index, buf.len()), buf, CTRL_TIMEOUT_MS).map_err(err)
        }
        pub fn set_interface(&self, iface: u8, alt: u8) -> Result<(), UsbErr> {
            kusb::set_alt(self.handle, iface, alt).map_err(err)
        }
    }

    #[derive(Clone, Copy, Debug)]
    pub struct StreamFormat { pub rate: u32, pub frame_bytes: u32 }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Poll { Ok, Busy, Gone }

    /// The kernel keeps the counters; the module has none of its own.
    #[derive(Clone, Copy, Default)]
    pub struct IsochStats { pub tds: u64, pub missed: u64, pub underrun: u64, pub silence: u64, pub errors: u64 }

    pub struct IsochPipe { h: u64, pub local: IsochStats }

    impl IsochPipe {
        pub fn open_ex(f: &UsbFunction, d: &IsochEpDesc, _fmt: StreamFormat, fifo_bytes: usize, ntds: usize,
                       framed: bool) -> Result<IsochPipe, &'static str> {
            match kusb::isoch_open(f.handle, d.iface, d.alt, d.ep_addr, fifo_bytes, ntds, framed) {
                Ok(h) => Ok(IsochPipe { h, local: IsochStats::default() }),
                Err(-22) => Err("endpoint not in the configuration"),
                Err(_) => Err("isochronous open failed"),
            }
        }
        pub fn poll(&mut self) -> Poll {
            match kusb::isoch_submit(self.h) { kusb::Poll::Ok => Poll::Ok, kusb::Poll::Busy => Poll::Busy, kusb::Poll::Gone => Poll::Gone }
        }
        pub fn read_packet(&mut self, out: &mut [u8]) -> Option<usize> { kusb::isoch_reap(self.h, out) }
        pub fn close(self) { kusb::isoch_close(self.h) }
    }

    /// The device behind `handle` left: drop its cameras.
    pub fn detach(handle: u64) { crate::uvc::detach_where(|f| f.handle == handle); }
}

#[path = "../../../drivers/usb/src/uvc.rs"]
mod uvc;

use uvcenv::{Match, UsbFunction};

static DONE: AtomicBool = AtomicBool::new(false);

const IDS: [KmodDevId; 1] = [kmod_rt::usb(0xffff, 0xffff, 0x0E_02_00, 0xFF_FF_00)];

kmod_rt::module! {
    name: "uvc",
    init: init,
    ids: [kmod_rt::usb(0xffff, 0xffff, 0x0E_02_00, 0xFF_FF_00)],
}

extern "C" fn probe(intf: *const KUsbIf, _ctx: usize) -> i32 {
    let i = unsafe { &*intf };
    let cfg = match kmod_kapi::usb::config_descriptor(i.handle) { Ok(c) => c, Err(_) => return -5 };
    let m = Match { bus: 0, vid: i.vid, pid: i.pid, class: i.class, subclass: i.subclass, protocol: i.protocol, iface: i.iface };
    let f = UsbFunction { handle: i.handle, slot: i.slot, cfg };
    match uvc::class_probe(f, &m, 0) { Ok(()) => 0, Err(_) => -1 }
}

extern "C" fn detach(handle: u64, _ctx: usize) { uvcenv::detach(handle); }

fn init() -> i32 {
    if DONE.swap(true, Ordering::SeqCst) { return -16; }
    uvc::set_cmdline(kmod_kapi::cmdline());
    // Cameras announced by uvc.rs reach the V4L2 layer through the kapi.
    uvc::set_camera_sink(uvc::CameraSink {
        add: |_owner, ops, ctx| {
            if let Err(e) = kmod_kapi::v4l2::register(ops, ctx) { kmod_rt::kerr!("v4l2_register failed: {}", e); }
        },
        remove: kmod_kapi::v4l2::unregister,
    });
    if let Err(e) = kmod_kapi::usb::class_register(&IDS, probe, Some(detach), 0) {
        kmod_rt::kerr!("usb_class_register failed: {}", e);
        return e;
    }
    uvc::register_fake();
    0
}
