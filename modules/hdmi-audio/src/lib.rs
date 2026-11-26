//! Raspberry Pi 5 HDMI audio as a loadable module: the SAME driver source as the built-in
//! (`drivers/src/hdmi_audio/mod.rs`) with `kenv` pointing at the kmod-kapi shim (dma40 handle
//! exports, the boot-probe sink facts, SError checkpoints). The boot announces
//! `leandros,pi5-hdmi-audio` once its hdmi probe is done; the module claims it from the platform
//! registry and attaches the backend to the PipeWire server through `kapi_snd_register`.
#![no_std]

extern crate alloc;
use alloc::boxed::Box;
use kmod_abi::KPlatDev;
use kmod_kapi::snd::KSndOps;

#[allow(dead_code)]
mod kenv {
    pub mod hdmi {
        use kmod_kapi::dma40 as sys;
        pub use kmod_kapi::mm::{buddy, phys_to_virt};
        pub use kmod_kapi::pci::serial_debug;
        pub const TX_BUF_BYTES: usize = 512;

        // Mirrors drivers/src/hdmi_probe.rs (kernel/src/kmod/exports/dma40.rs asserts they agree).
        pub const PROBE_NOT_RUN: u32 = 0;
        pub const PROBE_SKIPPED: u32 = 1;
        pub const PROBE_DONE: u32 = 2;
        pub const PORT_VALID: u32 = 1 << 0;
        pub const PORT_HPD: u32 = 1 << 1;
        pub const PORT_HDMI_MODE: u32 = 1 << 2;
        pub const PORT_ACTIVE: u32 = 1 << 3;
        pub const SINK_HDMI: u32 = 1;
        pub const SINK_DVI: u32 = 2;

        pub use crate::decode::acr_n_cts;
        pub(crate) use crate::cb;

        pub struct Sink { pub state: u32, pub flags: u32, pub pix_khz: u32, pub sink: u32, pub d0: bool, pub d0_unknown: bool }
        pub fn sink() -> Sink {
            match sys::hdmi_sink_info() {
                Some(k) => Sink { state: k.state, flags: k.port_flags, pix_khz: k.pix_khz, sink: k.sink, d0: k.d0 != 0, d0_unknown: k.d0 == 2 },
                None => Sink { state: PROBE_NOT_RUN, flags: 0, pix_khz: 0, sink: 0, d0: false, d0_unknown: false },
            }
        }
        pub fn monotonic_ns() -> u64 { kmod_kapi::sched::monotonic_ns() }
        pub fn ioremap_uc(phys: u64, len: usize) -> usize {
            kmod_kapi::mm::kva::ioremap(phys, len, kmod_kapi::mm::kva::CacheMode::Uncached).unwrap_or(0)
        }
        pub fn iounmap(va: usize, len: usize) { kmod_kapi::sys_common::iounmap(va, len) }
        pub fn serror() -> Option<u64> { kmod_kapi::plat::serror_checkpoint() }
        /// The dma engine is not cache coherent: write the ring back before the engine reads it.
        pub fn dcache_clean_inval(va: usize, len: usize) { kmod_kapi::sys_common::dma_sync(va, len, 2) }

        pub mod dma {
            use super::sys;
            /// A claimed dma40 channel; the handle is released on drop.
            pub struct Channel { h: u32, id: u8 }
            pub fn claim(prefer: Option<u8>) -> Result<Channel, i32> {
                sys::claim(prefer).map(|h| Channel { h, id: prefer.unwrap_or(0) })
            }
            pub fn polled() -> bool { sys::polled() }
            impl Channel {
                pub fn id(&self) -> u8 { self.id }
                pub fn start_cyclic(&mut self, ring: u64, len: usize, period: usize, fifo: u64, dreq_cell: u32, irq: bool) -> Result<usize, i32> {
                    sys::start_cyclic(self.h, ring, len, period, fifo, dreq_cell, irq)
                }
                pub fn position(&self) -> Option<usize> { sys::position(self.h) }
                pub fn periods(&self) -> u64 { sys::periods(self.h) }
                pub fn poll(&self) -> bool { sys::poll(self.h) }
                pub fn stop(&mut self) -> bool { sys::stop(self.h) }
                /// No register view through the exports.
                pub fn status(&self) -> (u32, u32) { (0, 0) }
            }
            impl Drop for Channel { fn drop(&mut self) { sys::release(self.h) } }
        }
    }
}

#[path = "../../../drivers/src/hdmi_probe/decode.rs"]
#[allow(dead_code)]
mod decode;

#[path = "../../../drivers/src/bcm2712_dma40/cb.rs"]
#[allow(dead_code)]
mod cb;

#[path = "../../../drivers/src/hdmi_audio/mod.rs"]
#[allow(dead_code)]
mod hdmi_audio;
use hdmi_audio::HdmiAudio;

kmod_rt::module! {
    name: "hdmi_audio",
    init: init,
    of: ["leandros,pi5-hdmi-audio"]
}

fn dev<'a>(ctx: usize) -> &'a mut HdmiAudio { unsafe { &mut *(ctx as *mut HdmiAudio) } }

fn put(text: &str, buf: *mut u8, cap: usize) -> usize {
    let n = text.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(text.as_ptr(), buf, n); }
    n
}

extern "C" fn configure(ctx: usize, stream: u32, freq: u32, channels: u8) { dev(ctx).reconfigure_stream(stream, freq, channels) }
extern "C" fn write(ctx: usize, buf: *const u8, len: usize) -> usize {
    dev(ctx).send_pcm_data(unsafe { core::slice::from_raw_parts(buf, len) })
}
extern "C" fn queued_bytes(ctx: usize) -> usize { dev(ctx).queued_bytes() as usize }
extern "C" fn tx_used_idx(ctx: usize) -> u32 { dev(ctx).tx_used_idx() as u32 }
extern "C" fn top_up_silence(ctx: usize) { dev(ctx).top_up_silence() }
extern "C" fn recover(ctx: usize) { dev(ctx).recover_stream() }
extern "C" fn cap_ensure(_ctx: usize) -> i32 { -19 }
extern "C" fn cap_restart(_ctx: usize) {}
extern "C" fn cap_pending(_ctx: usize) -> usize { 0 }
extern "C" fn cap_read(_ctx: usize, _buf: *mut u8, _cap: usize) -> i32 { -19 }
extern "C" fn cap_close(_ctx: usize) {}
extern "C" fn jack_service(_ctx: usize) {}
extern "C" fn info(ctx: usize, buf: *mut u8, cap: usize) -> usize { put(&dev(ctx).info(), buf, cap) }
extern "C" fn shutdown(_ctx: usize, _link_reset: u32, buf: *mut u8, cap: usize) -> usize {
    put("hdmi audio: nothing to do", buf, cap)
}

static OPS: KSndOps = KSndOps {
    flags: 0, configure, write, queued_bytes, tx_used_idx, top_up_silence, recover, cap_ensure, cap_restart,
    cap_pending, cap_read, cap_close, jack_service, info, shutdown,
};

/// Platform probe: bring the backend up on the announced function and hand it to PipeWire.
extern "C" fn probe(_ctx: usize, _dev: *const KPlatDev) -> bool {
    let mut h = HdmiAudio::new();
    if let Err(why) = h.probe(kmod_kapi::cmdline()) {
        kmod_rt::kinfo!("hdmi audio inert: {}", why);
        return false;
    }
    let ctx = Box::into_raw(Box::new(h)) as usize;
    match kmod_kapi::snd::register(&OPS, ctx) {
        Ok(()) => { kmod_rt::kinfo!("hdmi audio attached"); true }
        Err(e) => { kmod_rt::kerr!("snd_register failed: {}", e); drop(unsafe { Box::from_raw(ctx as *mut HdmiAudio) }); false }
    }
}

static CLAIMED: spin::Mutex<bool> = spin::Mutex::new(false);

fn init() -> i32 {
    // Without the Pi 5 boot probe (the virt machines) there is nothing to attach to.
    match kmod_kapi::dma40::hdmi_sink_info() {
        Some(k) if k.state != 0 => {}
        _ => return -19,
    }
    if kmod_kapi::plat::register(&["leandros,pi5-hdmi-audio"], claim, 0).is_err() { return -22; }
    if *CLAIMED.lock() { 0 } else { kmod_kapi::plat::unregister(); -19 }
}

extern "C" fn claim(ctx: usize, dev: *const KPlatDev) -> bool {
    let ok = probe(ctx, dev);
    if ok { *CLAIMED.lock() = true; }
    ok
}
