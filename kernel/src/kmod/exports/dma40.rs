//! kapi exports: BCM2712 dma40 channel handles and the HDMI sink facts (kmodpi H1).
use super::*;
use drivers::bcm2712_dma40::handles as h;

#[no_mangle]
pub extern "C" fn kapi_dma40_claim(prefer: i32) -> i64 { h::claim_handle(prefer) }
#[no_mangle]
pub extern "C" fn kapi_dma40_release(hd: u32) -> i32 { h::release(hd) }
#[no_mangle]
pub extern "C" fn kapi_dma40_start_cyclic(hd: u32, ring: u64, len: usize, period: usize, fifo: u64, dreq_cell: u32, irq_per_period: u32) -> i64 {
    h::start_cyclic(hd, ring, len, period, fifo, dreq_cell, irq_per_period != 0)
}
#[no_mangle]
pub extern "C" fn kapi_dma40_stop(hd: u32) -> i32 { h::stop(hd) }
#[no_mangle]
pub extern "C" fn kapi_dma40_position(hd: u32) -> i64 { h::position(hd) }
#[no_mangle]
pub extern "C" fn kapi_dma40_periods(hd: u32) -> u64 { h::periods(hd) }
#[no_mangle]
pub extern "C" fn kapi_dma40_poll(hd: u32) -> i32 { h::poll(hd) }
#[no_mangle]
pub extern "C" fn kapi_dma40_polled() -> i32 { h::is_polled() }

/// What the boot probe learned about hdmi0 (see `KHdmiSink`). -22 on a null or short buffer.
#[no_mangle]
pub extern "C" fn kapi_hdmi_sink_info(out: *mut kmod_abi::KHdmiSink) -> i32 {
    if out.is_null() { return -22; }
    let (st, fl, px, sk) = drivers::hdmi_plat::sink_info();
    let want = core::mem::size_of::<kmod_abi::KHdmiSink>() as u32;
    unsafe {
        if (*out).size < want { return -22; }
        *out = kmod_abi::KHdmiSink { size: want, state: st, port_flags: fl, pix_khz: px, sink: sk, d0: drivers::hdmi_plat::is_d0() as u32 };
    }
    0
}
