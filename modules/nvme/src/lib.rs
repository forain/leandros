//! NVMe as a loadable module: the SAME driver source as the built-in (`drivers/src/nvme.rs`),
//! with `kenv` pointing at the kmod-kapi shim. Namespaces go through `kapi_blk_register`; the
//! controller shutdown and kexec quiesce run from the BLOCK pm phase. Autoloads by PCI class
//! 01:08:02 (and is listed in modules/BOOT).
#![no_std]

extern crate alloc;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering};

mod kenv {
    pub use kmod_kapi::blk::env::*;
}

// The driver source names the kernel's `crate::irq`.
mod irq {
    pub use kmod_kapi::irq::*;
}

#[path = "../../../drivers/src/blockdev.rs"]
mod blockdev;
#[path = "../../../drivers/src/partscan.rs"]
mod partscan;
#[path = "../../../drivers/src/flush_logic.rs"]
mod flush_logic;
#[path = "../../../drivers/src/blkmod.rs"]
mod blkdev;
#[path = "../../../drivers/src/nvme.rs"]
mod nvme;

kmod_rt::module! {
    name: "nvme",
    init: init,
    exit: exit,
    ids: [kmod_rt::pci_class(0x010802, 0xffffff)]
}

fn copy_out(s: &str, buf: *mut u8, cap: usize) -> usize {
    let n = s.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n); }
    n
}

extern "C" fn pm_shutdown(_: usize, _buf: *mut u8, _cap: usize) -> i32 { nvme::shutdown_all(); 0 }
extern "C" fn pm_kexec(_: usize, buf: *mut u8, cap: usize) -> i32 {
    let mut s = String::new();
    for l in nvme::kexec_disable() { s.push_str(&l); s.push('\n'); }
    copy_out(&s, buf, cap) as i32
}

static PM: kmod_kapi::pm::KPmOps = kmod_kapi::pm::KPmOps {
    size: core::mem::size_of::<kmod_kapi::pm::KPmOps>() as u32,
    phase: kmod_kapi::pm::pm_phase::BLOCK,
    suspend: None, resume: None, freeze: None, thaw: None,
    shutdown: Some(pm_shutdown), kexec_quiesce: Some(pm_kexec), resume_check: None, unload_quiesce: None,
};

static DONE: AtomicBool = AtomicBool::new(false);

fn init() -> i32 {
    if DONE.swap(true, Ordering::SeqCst) { return -16; }
    if nvme::init(kmod_kapi::cmdline()) == 0 { return -19; }
    let _ = kmod_kapi::pm::register2(&PM, 0);
    0
}

fn exit() {
    nvme::unload();
    DONE.store(false, Ordering::SeqCst);
}
