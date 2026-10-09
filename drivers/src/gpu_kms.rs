//! The KMS half of a GPU backend that lives in another crate: what lets the
//! single synthetic CRTC/plane of `drm_device_interface` scan out buffers the
//! backend owns (today the NVIDIA nouveau-UAPI layer, `nvidia_rm::uapi_kms`).
//!
//! WHY A SECOND SEAM. `gpu_uapi` is the render half (driver-private ioctls,
//! GEM handles, PRIME, fences). KMS is the DRM core's (ioctl nr < 0x40), and
//! the object model — one connector, one CRTC, one primary plane, atomic
//! properties, page-flip events, DRM master, the console hand-off — already
//! exists for virtio-gpu and the plain GOP framebuffer. What a backend adds is
//! narrow: wrap one of its GEM handles in a framebuffer (ADDFB/ADDFB2), put a
//! framebuffer on the screen (a copy-engine blit into the boot loader's
//! surface on NVIDIA: no modeset), forget a framebuffer (RMFB / its creator
//! closed). Kept apart from `UapiBackend` so the render half can change
//! without touching this one.
//!
//! UNREGISTERED (every QEMU build, and the PC when `nvrm.kms=0` or the copy
//! engine path could not be verified) every hook is one relaxed load and the
//! existing paths run exactly as before: dumb buffers keep their CPU copy into
//! the GOP surface, IN_FORMATS is not advertised, the cursor plane stays.
//!
//! CALLING RULES. Every hook runs in task context (the ioctl's thread, or the
//! closing process), with NO DRM lock held: the backend sleeps on its own
//! locks and on RM calls. `present` returns once the pixels are in the scanout
//! surface (the DRM layer then queues the flip event, which the 100 Hz tick
//! delivers — the synthetic vblank).

use core::sync::atomic::{AtomicPtr, Ordering};

use crate::DriverError;

/// What `fb_create` is asked to wrap (ADDFB2's plane 0).
#[derive(Clone, Copy, Debug)]
pub struct KmsFbReq {
    pub handle: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub offset: u32,
    pub fourcc: u32,
    /// `Some` when the caller passed DRM_MODE_FB_MODIFIERS.
    pub modifier: Option<u64>,
}

pub struct KmsBackend {
    /// Short name for log lines ("nvidia-ce").
    pub name: &'static str,
    /// The one mode the scanout has: (width, height, refresh Hz). Any other
    /// mode is refused (there is no modeset).
    pub mode: fn() -> (u32, u32, u32),
    /// Modifiers IN_FORMATS advertises for every format (LINEAR first).
    pub modifiers: &'static [u64],
    /// ADDFB/ADDFB2 of a GEM handle. `None`: not one of the backend's handles
    /// (the DRM layer's own dumb/blob paths take it). `Some(Ok(token))`: a
    /// backend framebuffer, presented and destroyed through `token` (nonzero).
    pub fb_create: fn(req: &KmsFbReq, open_id: u32) -> Option<Result<u32, DriverError>>,
    /// The framebuffer went away (RMFB, or its creator closed).
    pub fb_destroy: fn(token: u32),
    /// Put `token` on screen: copy from (src_x, src_y) of the framebuffer to
    /// the top-left of the scanout, the whole visible area, or only `rects`
    /// (FB_DAMAGE_CLIPS: x1, y1, x2, y2 in framebuffer coordinates, half
    /// open) when given. Returns once the copy has completed.
    pub present: fn(token: u32, src_x: u32, src_y: u32, rects: Option<&[(i32, i32, i32, i32)]>) -> Result<(), DriverError>,
}

static KMS: AtomicPtr<KmsBackend> = AtomicPtr::new(core::ptr::null_mut());

/// Install the KMS backend. Called once, from the driver, when its scanout
/// path is verified.
pub fn register(b: &'static KmsBackend) {
    KMS.store(b as *const KmsBackend as *mut KmsBackend, Ordering::Release);
}

#[inline]
pub fn backend() -> Option<&'static KmsBackend> {
    let p = KMS.load(Ordering::Acquire);
    if p.is_null() { None } else { Some(unsafe { &*p }) }
}

#[inline]
pub fn active() -> bool {
    !KMS.load(Ordering::Relaxed).is_null()
}

/// The fixed scanout mode while a backend is registered.
pub fn fixed_mode() -> Option<(u32, u32, u32)> {
    backend().map(|b| (b.mode)())
}

/// May the CRTC be given a `w` x `h` mode? Always while no backend is
/// registered (the existing paths scale); only the scanout's own mode while
/// one is.
pub fn mode_allowed(w: u32, h: u32) -> bool {
    match fixed_mode() {
        Some((mw, mh, _)) => w == mw && h == mh,
        None => true,
    }
}
