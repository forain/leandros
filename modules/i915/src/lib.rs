//! Intel i915 (Gen9 display + render) as a loadable module: the SAME driver source as the built-in
//! (`drivers/src/i915/mod.rs`), with the kernel seam (`kenv`) pointing at the kmod-kapi shim. The KMS
//! and DRM uapi tables the driver registers are handed to the kernel as `extern "C"` tables
//! (`kapi_kms_register` / `kapi_drm_register`), and the backlight and power-management hooks go through
//! `kapi_backlight_register` / `kapi_pm_register2`.
//!
//! None of those tables can be unregistered, so the module is PERMANENT once anything is registered:
//! `init` returns -19 only before the first registration (i915.off=1, no Intel display function, or a
//! function the driver does not know), after that it always returns 0. Autoloads by PCI alias
//! `8086:* class 030000` (exactly the built-in probe filter).
#![no_std]

extern crate alloc;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering};

pub mod kenv;
pub use kenv::DriverError;

#[path = "../../../drivers/src/i915/mod.rs"]
#[allow(dead_code, unused_imports, unused_variables, unused_mut, unused_macros)]
pub mod i915;

kmod_rt::module! {
    name: "i915",
    init: init,
    ids: [ kmod_rt::pci_vendor_class(0x8086, 0x030000, 0xff0000) ]
}

/// Copy a hook message into `buf`; `Ok` = its length, `Err` = minus its length (at least -1).
fn pm_msg(r: Result<String, String>, buf: *mut u8, cap: usize) -> i32 {
    let (s, neg) = match r { Ok(s) => (s, false), Err(s) => (s, true) };
    let n = s.len().min(cap);
    if n > 0 { unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n); } }
    if neg { -(n.max(1) as i32) } else { n as i32 }
}
extern "C" fn pm_suspend(_: usize, buf: *mut u8, cap: usize) -> i32 { pm_msg(i915::pm_suspend(), buf, cap) }
extern "C" fn pm_resume(_: usize, buf: *mut u8, cap: usize) -> i32 { pm_msg(i915::pm_resume(), buf, cap) }
extern "C" fn pm_kexec(_: usize, buf: *mut u8, cap: usize) -> i32 {
    let v = i915::kexec_quiesce();
    pm_msg(Ok(v.join("\n")), buf, cap)
}

extern "C" fn bl_get(_: usize, duty: *mut u32, period: *mut u32) -> bool {
    match i915::backlight::get() { Some((d, p)) => { unsafe { *duty = d; *period = p; } true } None => false }
}
extern "C" fn bl_set(_: usize, duty: u32) -> bool { i915::backlight::set(duty) }

static STARTED: AtomicBool = AtomicBool::new(false);

fn init() -> i32 {
    if STARTED.load(Ordering::SeqCst) { return -16; }
    let off = kenv::cmdline_opt("i915.off") == Some(1);
    let fake = matches!(kenv::cmdline_opt("i915.fake"), Some(1) | Some(2));
    let has_dev = fake || kmod_kapi::pci::scan().iter().any(|d| d.vendor_id == 0x8086 && d.class == 0x03);
    if off || !has_dev { return -19; }
    let env = match kmod_kapi::gfx::boot_fb() {
        Some(f) => i915::BootEnv { fb_phys: f.phys, fb_width: f.width, fb_height: f.height, fb_pitch: f.pitch },
        None => i915::BootEnv { fb_phys: 0, fb_width: 0, fb_height: 0, fb_pitch: 0 },
    };
    // The VFS tmpfs census lives in the kernel (servers/drm registers it with drm_class): reach it through the kapi.
    i915::set_tmpfs_census(kmod_kapi::drm::tmpfs_census);
    i915::run(&env);
    if !kenv::registered() { return -19; }
    STARTED.store(true, Ordering::SeqCst);
    let ops = kmod_kapi::pm::KPmOps {
        size: core::mem::size_of::<kmod_kapi::pm::KPmOps>() as u32, phase: kmod_kapi::pm::pm_phase::GPU,
        suspend: Some(pm_suspend), resume: Some(pm_resume), freeze: None, thaw: None, shutdown: None,
        kexec_quiesce: Some(pm_kexec), resume_check: None, unload_quiesce: None,
    };
    static OPS: spin::Once<kmod_kapi::pm::KPmOps> = spin::Once::new();
    let _ = kmod_kapi::pm::register2(OPS.call_once(|| ops), 0);
    let _ = kmod_kapi::gfx::backlight_register(Some(bl_get), Some(bl_set), 0);
    0
}
