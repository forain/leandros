//! DRM class table for loadable GPU drivers (kmod kapi, W2-7a).
//!
//! `KDrmOps` is the `extern "C"` mirror of `gpu_uapi::UapiBackend`. `register` installs a static
//! `UapiBackend` whose fns forward to the registered C table, so the DRM layer (drm_device_interface)
//! stays unchanged and a built-in driver can keep registering its Rust backend directly.
//! Return conventions: ioctl / gem_close give `>= 0` = result, `KDRM_NOT_MINE` = not ours,
//! `-errno` = error. Optional callbacks may be `None`.

use alloc::boxed::Box;
use alloc::string::String;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use crate::gpu_uapi::{self, UapiBackend, UapiPrime};
use crate::DriverError;

pub const KDRM_NOT_MINE: i64 = i64::MIN;

/// PRIME export answer, C layout (mirror of `UapiPrime`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct KDrmPrime { pub phys: usize, pub order: usize, pub pages: usize, pub len: usize, pub obj: u32 }

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KDrmOps {
    /// `size_of::<KDrmOps>()`.
    pub size: u32,
    pub ver_major: i32,
    pub ver_minor: i32,
    pub ver_patch: i32,
    pub user_args: bool,
    pub name: *const u8, pub name_len: usize,
    pub date: *const u8, pub date_len: usize,
    pub desc: *const u8, pub desc_len: usize,
    pub ioctl: extern "C" fn(ctx: usize, cmd: u32, arg: usize, open_id: u32) -> i64,
    pub gem_close: extern "C" fn(ctx: usize, handle: u32, open_id: u32) -> i64,
    /// Physical address for an mmap token (0 = not ours); `*uncached` set when the map must be uncached.
    pub mmap: extern "C" fn(ctx: usize, token: u64, open_id: u32, uncached: *mut bool) -> u64,
    pub release_open: extern "C" fn(ctx: usize, open_id: u32),
    pub prime_export: extern "C" fn(ctx: usize, handle: u32, open_id: u32, out: *mut KDrmPrime) -> bool,
    /// A handle for `open_id`, 0 = not ours.
    pub prime_import: extern "C" fn(ctx: usize, obj: u32, open_id: u32) -> u32,
    pub bo_release: extern "C" fn(ctx: usize, obj: u32) -> bool,
    pub fence_done: extern "C" fn(ctx: usize, fence: u64) -> bool,
    /// Bytes written to `out`, or -1 = leave the on-disk file.
    pub sysfs_attr: extern "C" fn(ctx: usize, attr: *const u8, attr_len: usize, out: *mut u8, cap: usize) -> i64,
}
unsafe impl Send for KDrmOps {}
unsafe impl Sync for KDrmOps {}

static TABLE: AtomicPtr<KDrmOps> = AtomicPtr::new(core::ptr::null_mut());
static CTX: AtomicUsize = AtomicUsize::new(0);

fn t() -> &'static KDrmOps { unsafe { &*TABLE.load(Ordering::Acquire) } }
fn c() -> usize { CTX.load(Ordering::Acquire) }

fn strn(p: *const u8, n: usize) -> &'static str {
    if p.is_null() { return ""; }
    let b = unsafe { core::slice::from_raw_parts(p, n) };
    Box::leak(String::from_utf8_lossy(b).into_owned().into_boxed_str())
}

fn res(r: i64) -> Option<Result<usize, DriverError>> {
    if r == KDRM_NOT_MINE { None }
    else if r >= 0 { Some(Ok(r as usize)) }
    else { Some(Err(DriverError::Errno((-r).min(i32::MAX as i64) as i32))) }
}

fn a_ioctl(cmd: u32, arg: usize, open_id: u32) -> Option<Result<usize, DriverError>> { res((t().ioctl)(c(), cmd, arg, open_id)) }
fn a_gem_close(h: u32, open_id: u32) -> Option<Result<usize, DriverError>> { res((t().gem_close)(c(), h, open_id)) }
fn a_mmap(token: u64, open_id: u32) -> Option<(u64, bool)> {
    let mut unc = false;
    let p = (t().mmap)(c(), token, open_id, &mut unc);
    if p == 0 { None } else { Some((p, unc)) }
}
fn a_release_open(open_id: u32) { (t().release_open)(c(), open_id) }
fn a_prime_export(h: u32, open_id: u32) -> Option<UapiPrime> {
    let mut o = KDrmPrime::default();
    if !(t().prime_export)(c(), h, open_id, &mut o) { return None; }
    Some(UapiPrime { phys: o.phys, order: o.order, pages: o.pages, len: o.len, obj: o.obj })
}
fn a_prime_import(obj: u32, open_id: u32) -> Option<u32> {
    match (t().prime_import)(c(), obj, open_id) { 0 => None, h => Some(h) }
}
fn a_bo_release(obj: u32) -> bool { (t().bo_release)(c(), obj) }
fn a_fence_done(f: u64) -> bool { (t().fence_done)(c(), f) }
fn a_sysfs(attr: &[u8], out: &mut [u8]) -> Option<usize> {
    let n = (t().sysfs_attr)(c(), attr.as_ptr(), attr.len(), out.as_mut_ptr(), out.len());
    if n < 0 { None } else { Some((n as usize).min(out.len())) }
}

static ADAPTER: spin::Once<UapiBackend> = spin::Once::new();

/// Install a module's DRM table as the `UapiBackend`. 0 ok, -22 bad table, -17 a backend is already
/// registered (built-in or another module).
pub fn register(ops: *const KDrmOps, ctx: usize) -> i32 {
    if ops.is_null() { return -22; }
    let o = unsafe { &*ops };
    if (o.size as usize) < core::mem::size_of::<KDrmOps>() || o.name.is_null() || o.name_len == 0 { return -22; }
    if gpu_uapi::active() { return -17; }
    if TABLE.compare_exchange(core::ptr::null_mut(), ops as *mut KDrmOps, Ordering::AcqRel, Ordering::Acquire).is_err() { return -17; }
    CTX.store(ctx, Ordering::Release);
    let b = ADAPTER.call_once(|| UapiBackend {
        name: strn(o.name, o.name_len),
        version: (o.ver_major, o.ver_minor, o.ver_patch),
        date: strn(o.date, o.date_len),
        desc: strn(o.desc, o.desc_len),
        ioctl: a_ioctl, user_args: o.user_args, gem_close: a_gem_close, mmap: a_mmap,
        release_open: a_release_open, prime_export: a_prime_export, prime_import: a_prime_import,
        bo_release: a_bo_release, fence_done: a_fence_done, sysfs_attr: a_sysfs,
    });
    gpu_uapi::register(b);
    0
}

/// Whether a module table is the active backend.
pub fn registered() -> bool { !TABLE.load(Ordering::Acquire).is_null() }
