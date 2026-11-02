//! Intel High Definition Audio (+ capture) as a loadable module: the SAME driver source as the
//! built-in (`drivers/src/hda.rs`, `hda_capture.rs`), with `kenv` pointing at the kmod-kapi shim.
//! Autoloads by PCI class alias (class 04, subclass 03, any prog-if) and attaches to the pipewire
//! server through `kapi_snd_register` with `KSND_CAPTURE`.
#![no_std]

extern crate alloc;
use spin::Mutex;

mod kenv {
    pub use kmod_kapi::{mm, pci, sched, serial_raw};
    #[derive(Debug, Clone, Copy)]
    pub enum DriverError { NotFound, Io, Unsupported }
}

#[path = "../../../drivers/src/hda.rs"]
#[allow(dead_code)]
mod hda;

kmod_rt::module! {
    name: "hda",
    init: init,
    exit: exit,
    ids: [kmod_rt::pci_class(0x040300, 0xffff00)]
}

static DEV: Mutex<hda::Hda> = Mutex::new(hda::Hda::new());

fn copy_out(s: &str, buf: *mut u8, cap: usize) -> usize {
    let n = s.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n); }
    n
}

extern "C" fn configure(_: usize, id: u32, freq: u32, ch: u8) { DEV.lock().reconfigure_stream(id, freq, ch) }
extern "C" fn write(_: usize, b: *const u8, len: usize) -> usize {
    DEV.lock().send_pcm_data(unsafe { core::slice::from_raw_parts(b, len) })
}
extern "C" fn queued_bytes(_: usize) -> usize { DEV.lock().queued_bytes() as usize }
extern "C" fn tx_used_idx(_: usize) -> u32 { DEV.lock().tx_used_idx() as u32 }
extern "C" fn top_up_silence(_: usize) { DEV.lock().top_up_silence() }
extern "C" fn recover(_: usize) { DEV.lock().recover_stream() }
extern "C" fn cap_ensure(_: usize) -> i32 { match DEV.lock().capture_ensure() { Ok(()) => 0, Err(e) => e } }
extern "C" fn cap_restart(_: usize) { DEV.lock().capture_restart(); }
extern "C" fn cap_pending(_: usize) -> usize { DEV.lock().capture_pending() }
extern "C" fn cap_read(_: usize, b: *mut u8, cap: usize) -> i32 {
    match DEV.lock().capture_read(unsafe { core::slice::from_raw_parts_mut(b, cap) }) {
        Ok(n) => n as i32,
        Err(e) => if e > 0 { -e } else { e },
    }
}
extern "C" fn cap_close(_: usize) { DEV.lock().capture_close() }
extern "C" fn jack_service(_: usize) { DEV.lock().jack_service() }
extern "C" fn info(_: usize, buf: *mut u8, cap: usize) -> usize {
    let s = DEV.lock().info();
    copy_out(&s, buf, cap)
}
extern "C" fn shutdown(_: usize, link_reset: u32, buf: *mut u8, cap: usize) -> usize {
    let mut d = DEV.lock();
    let s = d.shutdown(link_reset != 0, 300_000);
    // The built-in path hands the PCI function back so the caller clears bus mastering; a module
    // has no such channel, so clear it here, after the controller is stopped.
    if let Some(f) = d.pci_function() {
        let c = f.read16(0x04);
        f.write16(0x04, c & !0x4);
    }
    copy_out(&s, buf, cap)
}

static OPS: kmod_kapi::snd::KSndOps = kmod_kapi::snd::KSndOps {
    flags: kmod_kapi::snd::KSND_CAPTURE, configure, write, queued_bytes, tx_used_idx, top_up_silence, recover,
    cap_ensure, cap_restart, cap_pending, cap_read, cap_close, jack_service, info, shutdown,
};

fn init() -> i32 {
    if DEV.lock().probe().is_err() { return -19; }
    match kmod_kapi::snd::register(&OPS, 0) {
        Ok(()) => { kmod_rt::kinfo!("hda attached"); 0 }
        Err(e) => { kmod_rt::kerr!("snd_register failed: {}", e); e }
    }
}

/// Unload: the pipewire server already dropped this backend (registry::pre_exit) and the kernel stopped
/// bus mastering; stop the controller and return the DMA pages.
fn exit() {
    let mut d = DEV.lock();
    let _ = d.shutdown(false, 300_000);
    let n = d.free_dma();
    kmod_rt::kinfo!("hda: freed {} DMA pages", n);
}
