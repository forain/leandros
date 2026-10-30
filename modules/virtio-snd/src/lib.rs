//! virtio-sound as a loadable module: the SAME driver source as the built-in
//! (`drivers/src/snd.rs`), with `kenv` pointing at the kmod-kapi shim. Autoloads by PCI alias
//! (1af4:1059) and attaches to the pipewire server through `kapi_snd_register`.
#![no_std]

extern crate alloc;
use spin::Mutex;

mod kenv {
    pub use kmod_kapi::{mm, pci, sched, serial_raw};
    #[derive(Debug, Clone, Copy)]
    pub enum DriverError { NotFound, Io, Unsupported }
}

#[path = "../../../drivers/src/snd.rs"]
#[allow(dead_code)]
mod snd;

kmod_rt::module! {
    name: "virtio_snd",
    init: init,
    ids: [kmod_rt::pci(0x1af4, 0x1059)]
}

static DEV: Mutex<snd::VirtioSnd> = Mutex::new(snd::VirtioSnd::new());

fn copy_out(s: &str, buf: *mut u8, cap: usize) -> usize {
    let n = s.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n); }
    n
}

extern "C" fn configure(_: usize, id: u32, freq: u32, ch: u8) { DEV.lock().reconfigure_stream(id, freq, ch) }
extern "C" fn write(_: usize, b: *const u8, len: usize) -> usize {
    DEV.lock().send_pcm_data(unsafe { core::slice::from_raw_parts(b, len) })
}
extern "C" fn queued_bytes(_: usize) -> usize { DEV.lock().tx_level() as usize * snd::TX_BUF_BYTES }
extern "C" fn tx_used_idx(_: usize) -> u32 { DEV.lock().tx_used_idx() as u32 }
extern "C" fn top_up_silence(_: usize) { DEV.lock().top_up_silence() }
extern "C" fn recover(_: usize) { DEV.lock().recover_stream() }
extern "C" fn cap_ensure(_: usize) -> i32 { -19 }
extern "C" fn cap_restart(_: usize) {}
extern "C" fn cap_pending(_: usize) -> usize { 0 }
extern "C" fn cap_read(_: usize, _: *mut u8, _: usize) -> i32 { -19 }
extern "C" fn cap_close(_: usize) {}
extern "C" fn jack_service(_: usize) {}
extern "C" fn info(_: usize, buf: *mut u8, cap: usize) -> usize { copy_out("backend: virtio-sound\n", buf, cap) }
extern "C" fn shutdown(_: usize, _: u32, buf: *mut u8, cap: usize) -> usize {
    copy_out("virtio-sound: nothing to do", buf, cap)
}

static OPS: kmod_kapi::snd::KSndOps = kmod_kapi::snd::KSndOps {
    flags: 0, configure, write, queued_bytes, tx_used_idx, top_up_silence, recover,
    cap_ensure, cap_restart, cap_pending, cap_read, cap_close, jack_service, info, shutdown,
};

fn init() -> i32 {
    if DEV.lock().probe().is_err() { return -19; }
    match kmod_kapi::snd::register(&OPS, 0) {
        Ok(()) => { kmod_rt::kinfo!("virtio-sound attached"); 0 }
        Err(e) => { kmod_rt::kerr!("snd_register failed: {}", e); e }
    }
}
