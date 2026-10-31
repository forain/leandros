//! The built-in Pi 5 HDMI audio backend, presented through `KSndOps` exactly as a sound module
//! would be (`kapi_snd_register`), so the `=y` path exercises the same adapter as `=m`.

use alloc::boxed::Box;
use drivers::hdmi_audio::HdmiAudio;
use kmod_abi::KSndOps;

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

/// Probe the HDMI sink; on success the backend is leaked (never torn down) and returned as ops + ctx.
pub fn probe(cmdline: &str) -> Option<(&'static KSndOps, usize)> {
    let mut h = HdmiAudio::new();
    h.probe(cmdline).ok()?;
    Some((&OPS, Box::into_raw(Box::new(h)) as usize))
}
