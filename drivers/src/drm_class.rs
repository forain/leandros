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
    // ---- size-gated tail (K2a): a table whose `size` ends before a field leaves it unset ----
    /// sys_mmap's page pin for a token: `KDRM_NOT_MINE` = not ours, else the number of frames written to
    /// `out` (0 = refuse the mmap). `cap` frames fit.
    pub mmap_pin: Option<extern "C" fn(ctx: usize, token: usize, len: usize, open_id: u32, out: *mut usize, cap: usize) -> i64>,
    /// `/proc/i915` text: bytes written (or the total needed when `> cap`), `-1` = no file.
    pub proc_text: Option<extern "C" fn(ctx: usize, out: *mut u8, cap: usize) -> i64>,
    pub prime_dedup: Option<extern "C" fn(ctx: usize, obj: u32) -> bool>,
    pub ptecheck_mmap: Option<extern "C" fn(ctx: usize, obj: u32, off: u64, frames: *const usize, n: usize)>,
    /// `obj` is `u32::MAX` for none.
    pub ptecheck_fd_to_handle: Option<extern "C" fn(ctx: usize, fd: i32, open_id: u32, obj: u32, handle: u32)>,
}
unsafe impl Send for KDrmOps {}
unsafe impl Sync for KDrmOps {}

static TABLE: AtomicPtr<KDrmOps> = AtomicPtr::new(core::ptr::null_mut());
static CTX: AtomicUsize = AtomicUsize::new(0);

fn t() -> &'static KDrmOps { unsafe { &*TABLE.load(Ordering::Acquire) } }
fn tn() -> Option<&'static KDrmOps> { let p = TABLE.load(Ordering::Acquire); if p.is_null() { None } else { Some(unsafe { &*p }) } }

/// Offset just past the base (pre-tail) table; the minimum acceptable `size`.
const BASE_SIZE: usize = core::mem::offset_of!(KDrmOps, mmap_pin);

/// A tail field, `None` when the module's table ends before it.
macro_rules! tail { ($o:expr, $f:ident) => {{
    let o: &KDrmOps = $o;
    if (o.size as usize) >= core::mem::offset_of!(KDrmOps, $f) + core::mem::size_of_val(&o.$f) { o.$f } else { None }
}}; }
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
    if (o.size as usize) < BASE_SIZE || o.name.is_null() || o.name_len == 0 { return -22; }
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
    // Resolved through the registered table at use time, so a module registered after drm_server init is honoured.
    if tail!(o, mmap_pin).is_some() { *crate::drm_device_interface::BACKEND_MMAP_PIN.lock() = Some(a_mmap_pin); }
    0
}

fn a_mmap_pin(token: usize, len: usize, open_id: u32) -> Option<alloc::vec::Vec<usize>> {
    let o = tn()?;
    let f = tail!(o, mmap_pin)?;
    let cap = (len + 4095) / 4096;
    let mut v = alloc::vec![0usize; cap];
    match f(c(), token, len, open_id, v.as_mut_ptr(), cap) {
        KDRM_NOT_MINE => None,
        n if n < 0 => Some(alloc::vec::Vec::new()),
        n => { v.truncate((n as usize).min(cap)); Some(v) }
    }
}

/// `/proc/i915` through the registered table; `None` = no module / no file.
pub fn proc_text() -> Option<String> {
    let o = tn()?;
    let f = tail!(o, proc_text)?;
    let mut cap = 16 * 1024;
    for _ in 0..2 {
        let mut b = alloc::vec![0u8; cap];
        let n = f(c(), b.as_mut_ptr(), cap);
        if n < 0 { return None; }
        if (n as usize) <= cap { b.truncate(n as usize); return Some(String::from_utf8_lossy(&b).into_owned()); }
        cap = n as usize;
    }
    None
}

pub fn prime_dedup(obj: u32) -> bool {
    match tn().and_then(|o| tail!(o, prime_dedup)) { Some(f) => f(c(), obj), None => false }
}

pub fn ptecheck_dmabuf_mmap(obj: u32, off: u64, frames: &[usize]) {
    if let Some(f) = tn().and_then(|o| tail!(o, ptecheck_mmap)) { f(c(), obj, off, frames.as_ptr(), frames.len()) }
}

pub fn ptecheck_fd_to_handle(fd: i32, open_id: u32, obj: Option<u32>, handle: u32) {
    if let Some(f) = tn().and_then(|o| tail!(o, ptecheck_fd_to_handle)) { f(c(), fd, open_id, obj.unwrap_or(u32::MAX), handle) }
}

static CENSUS: AtomicUsize = AtomicUsize::new(0);
/// servers/drm hands its tmpfs census here at init (before any module exists).
pub fn set_tmpfs_census(f: fn() -> Option<[u64; 9]>) { CENSUS.store(f as usize, Ordering::Release); }
pub fn tmpfs_census() -> Option<[u64; 9]> {
    let f = CENSUS.load(Ordering::Acquire);
    if f == 0 { return None; }
    let f: fn() -> Option<[u64; 9]> = unsafe { core::mem::transmute(f) };
    f()
}

/// Whether a module table is the active backend.
pub fn registered() -> bool { !TABLE.load(Ordering::Acquire).is_null() }
