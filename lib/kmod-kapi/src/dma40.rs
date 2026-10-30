//! BCM2712 dma40 channel handles and HDMI sink facts (kmodpi H1).

use kmod_abi::plat::sys_dma40 as sys;
pub use kmod_abi::KHdmiSink;

/// Claim a channel (`prefer` None = any). Handle or -errno.
pub fn claim(prefer: Option<u8>) -> Result<u32, i32> {
    match unsafe { sys::kapi_dma40_claim(prefer.map_or(-1, |p| p as i32)) } { h if h > 0 => Ok(h as u32), e => Err(e as i32) }
}
pub fn release(h: u32) { unsafe { sys::kapi_dma40_release(h); } }
/// Start a cyclic ring; returns the period count.
pub fn start_cyclic(h: u32, ring: u64, len: usize, period: usize, fifo: u64, dreq_cell: u32, irq: bool) -> Result<usize, i32> {
    match unsafe { sys::kapi_dma40_start_cyclic(h, ring, len, period, fifo, dreq_cell, irq as u32) } { n if n >= 0 => Ok(n as usize), e => Err(e as i32) }
}
pub fn stop(h: u32) -> bool { unsafe { sys::kapi_dma40_stop(h) == 0 } }
pub fn position(h: u32) -> Option<usize> { match unsafe { sys::kapi_dma40_position(h) } { p if p >= 0 => Some(p as usize), _ => None } }
pub fn periods(h: u32) -> u64 { unsafe { sys::kapi_dma40_periods(h) } }
pub fn poll(h: u32) -> bool { unsafe { sys::kapi_dma40_poll(h) == 1 } }
pub fn polled() -> bool { unsafe { sys::kapi_dma40_polled() == 1 } }
/// The boot probe's view of hdmi0.
pub fn hdmi_sink_info() -> Option<KHdmiSink> {
    let mut k = KHdmiSink { size: core::mem::size_of::<KHdmiSink>() as u32, ..Default::default() };
    if unsafe { sys::kapi_hdmi_sink_info(&mut k) } == 0 { Some(k) } else { None }
}
