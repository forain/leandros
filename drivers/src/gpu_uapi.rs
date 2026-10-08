//! The seam between the DRM device (`drm_device_interface`) and a GPU driver
//! that lives in another crate and answers a driver-private UAPI on the same
//! two nodes — today the NVIDIA nouveau-UAPI layer in drivers/nvidia
//! (`nvidia_rm::uapi`), which this crate cannot depend on (it depends on us).
//!
//! WHAT A BACKEND IS. One `&'static UapiBackend` of function pointers,
//! registered once by the driver when its hardware is up. Until then this is
//! one relaxed load per DRM ioctl and every existing path (virtio-gpu, the
//! v3d stub, the GOP framebuffer KMS emulation) runs exactly as before:
//!
//!   * DRM_IOCTL_VERSION on card0 and renderD128 answers the backend's
//!     identity (Mesa's loaders pick a driver by this name);
//!   * the driver-private ioctl block (nr >= DRM_COMMAND_BASE) goes to
//!     `ioctl` first, the way the v3d block does;
//!   * GEM_CLOSE, the mmap token resolution, PRIME export/import, the
//!     exported-fd release and the per-open teardown consult it for the
//!     handles/objects it owns;
//!   * DRM_CAP_SYNCOBJ_TIMELINE and the timeline syncobj ioctls are on;
//!   * the VFS answers the backend's PCI identity for
//!     /sys/dev/char/226:{0,128}/device/{vendor,device,...} (libdrm's
//!     drmGetDevices2 classifies the device from those files).
//!
//! FENCES. A syncobj payload holds opaque fence ids (see `syncobj_core`). Ids
//! with [`EXT_FENCE`] set belong to the backend, which answers "has it
//! signalled?" through `fence_done`; every other id is a virtio/v3d fence and
//! keeps the old in-order watermark. A backend fence must be cheap to test
//! (a semaphore read) and must report `true` once its producer is gone (a
//! destroyed or dead channel), so no waiter can sleep on it forever.

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::DriverError;

/// Bit 63 of a fence id: the id belongs to the registered backend.
pub const EXT_FENCE: u64 = 1 << 63;

/// PRIME export answer (see `drm_device_interface::PrimeExport`).
pub struct UapiPrime {
    /// Physical base of a contiguous buddy-style block backing the BO, or 0
    /// for a token-only export (no CPU pages, e.g. VRAM).
    pub phys: usize,
    pub order: usize,
    pub len: usize,
    /// Lifetime identity; one reference was taken for the fd.
    pub obj: u32,
}

pub struct UapiBackend {
    /// DRM_IOCTL_VERSION identity (exact string, no NUL).
    pub name: &'static str,
    pub version: (i32, i32, i32),
    pub date: &'static str,
    pub desc: &'static str,
    /// A driver-private ioctl (nr >= DRM_COMMAND_BASE). `None`: not one of
    /// ours (falls through to the generic dispatch and its unknown-ioctl
    /// report). Runs on the caller's thread, in its address space.
    pub ioctl: fn(cmd: u32, arg: usize, open_id: u32) -> Option<Result<usize, DriverError>>,
    /// GEM_CLOSE of `handle`, if the handle is ours.
    pub gem_close: fn(handle: u32, open_id: u32) -> Option<Result<usize, DriverError>>,
    /// mmap token -> (physical address, map uncached), if the token is ours.
    pub mmap: fn(token: u64, open_id: u32) -> Option<(u64, bool)>,
    /// The last fd of an open closed.
    pub release_open: fn(open_id: u32),
    /// PRIME_HANDLE_TO_FD of one of our handles (takes a reference).
    pub prime_export: fn(handle: u32, open_id: u32) -> Option<UapiPrime>,
    /// PRIME_FD_TO_HANDLE of one of our objects: a handle for `open_id`.
    pub prime_import: fn(obj: u32, open_id: u32) -> Option<u32>,
    /// Drop the reference an exported fd held; false if `obj` is not ours.
    pub bo_release: fn(obj: u32) -> bool,
    /// Has backend fence `fence` (EXT_FENCE set) signalled? Any context,
    /// lock-free.
    pub fence_done: fn(fence: u64) -> bool,
    /// Contents of PCI attribute `attr` ("vendor", "device", "uevent", ...)
    /// for the DRM nodes' sysfs directory; `None`: leave the on-disk file.
    pub sysfs_attr: fn(attr: &[u8], out: &mut [u8]) -> Option<usize>,
}

static BACKEND: AtomicPtr<UapiBackend> = AtomicPtr::new(core::ptr::null_mut());

/// Install the backend. Called once, from the driver, when its device is up.
pub fn register(b: &'static UapiBackend) {
    BACKEND.store(b as *const UapiBackend as *mut UapiBackend, Ordering::Release);
}

/// The registered backend, if any. One relaxed-acquire load.
#[inline]
pub fn backend() -> Option<&'static UapiBackend> {
    let p = BACKEND.load(Ordering::Acquire);
    if p.is_null() { None } else { Some(unsafe { &*p }) }
}

#[inline]
pub fn active() -> bool {
    !BACKEND.load(Ordering::Relaxed).is_null()
}

/// The PCI-attribute override the VFS consults for
/// `/sys/dev/char/226:<minor>/device/<attr>` (registered with the VFS by the
/// DRM server). `None` whenever no backend is registered, so the on-disk
/// virtio stubs answer exactly as before.
pub fn drm_sysfs_attr(path: &[u8], out: &mut [u8]) -> Option<usize> {
    let b = backend()?;
    let rest = path.strip_prefix(b"/sys/dev/char/226:")?;
    let (minor, attr) = {
        let slash = rest.iter().position(|&c| c == b'/')?;
        (&rest[..slash], &rest[slash..])
    };
    if minor != b"0" && minor != b"128" {
        return None;
    }
    let attr = attr.strip_prefix(b"/device/")?;
    if attr.is_empty() || attr.contains(&b'/') {
        return None;
    }
    (b.sysfs_attr)(attr, out)
}

/// A physical range (the backend's VRAM aperture) whose device mappings must
/// be write-combined, never write-back. 0/0 when none.
static WC_BASE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static WC_SIZE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn set_wc_range(base: u64, size: u64) {
    WC_SIZE.store(0, Ordering::Release);
    WC_BASE.store(base, Ordering::Release);
    WC_SIZE.store(size, Ordering::Release);
}

pub fn in_wc_range(phys: u64) -> bool {
    let size = WC_SIZE.load(Ordering::Acquire);
    if size == 0 {
        return false;
    }
    let base = WC_BASE.load(Ordering::Acquire);
    phys >= base && phys - base < size
}
