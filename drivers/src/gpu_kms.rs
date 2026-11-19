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
//!
//! TWO OPTIONAL HALVES (lane kmscursor), registered separately so a backend
//! that has neither (NVIDIA) is untouched:
//!   * `KmsFlip` — real-vblank flips. The atomic/PAGE_FLIP ioctl only QUEUES
//!     the flip (`queue`) and returns; the backend arms it once the render
//!     fence signalled and completes the commit's event itself, at the vblank
//!     that latched it, through `drm_device_interface::ext_event_complete`
//!     (any context). A commit while one is still pending gets EBUSY
//!     (nonblocking) or waits (blocking), as Linux. `present` stays the
//!     synchronous path for SETCRTC.
//!   * `KmsCursor` — a hardware cursor plane: the DRM cursor plane is exposed
//!     while a backend is registered only when this is.

use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

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
    /// For a backend that flips the plane to its own buffers (i915) rather
    /// than copying into the boot loader's surface: put the boot loader's
    /// surface back on screen because the console owns the scanout again.
    /// Must be lock-free (called from the panic path). `None`: the console's
    /// surface is always the one on screen (copy backends).
    pub console_restore: Option<fn()>,
}

static KMS: AtomicPtr<KmsBackend> = AtomicPtr::new(core::ptr::null_mut());

/// Real-vblank flips (see the module note). Every hook: task context, no DRM
/// lock held, except `busy` (any context, lock-free).
pub struct KmsFlip {
    /// Queue `token` for scanout from (src_x, src_y); `ev` (nonzero) is the
    /// event cookie to complete when the flip has latched. Validation errors
    /// come back here; a flip whose fence never signals is dropped later
    /// (its event still completes).
    pub queue: fn(token: u32, src_x: u32, src_y: u32, ev: u32) -> Result<(), DriverError>,
    /// A commit that flipped nothing (cursor only, or no change): complete
    /// `ev` at the next vblank.
    pub event: fn(ev: u32),
    /// A flip or event is queued or armed and has not completed.
    pub busy: fn() -> bool,
    /// Wait (task context, bounded) until not `busy`. False on timeout.
    pub wait_idle: fn(timeout_ns: u64) -> bool,
    /// `queue` as an ASYNCHRONOUS flip (DRM_MODE_PAGE_FLIP_ASYNC): the new
    /// surface is scanned out at once, mid-frame (tearing allowed); `ev`
    /// still completes at the next vblank. `None`: not supported (EINVAL).
    pub queue_async: Option<fn(token: u32, src_x: u32, src_y: u32, ev: u32) -> Result<(), DriverError>>,
    /// The hardware vblank counter and the monotonic time of the latest
    /// vblank start (DRM_IOCTL_WAIT_VBLANK / CRTC_GET_SEQUENCE). `None`: the
    /// DRM layer's own 60 Hz synthetic counter answers.
    pub frame: Option<fn() -> (u32, u64)>,
    /// Task context: sleep until the counter reaches `target` (wrapping
    /// compare), at most `timeout_ns`; the (counter, time) seen, or `None`
    /// on timeout.
    pub wait_frame: Option<fn(target: u32, timeout_ns: u64) -> Option<(u32, u64)>>,
}

/// Where a cursor image comes from: a backend framebuffer (`token`), or the
/// DRM layer's own physically contiguous buffer (`phys`, a dumb buffer).
#[derive(Clone, Copy, Debug)]
pub struct KmsCursorSrc {
    pub token: u32,
    pub phys: u64,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
}

/// A hardware cursor plane (ARGB8888). Task context, no DRM lock held.
pub struct KmsCursor {
    /// DRM_CAP_CURSOR_WIDTH/HEIGHT: the (square) size `set` takes.
    pub size: u32,
    /// Load a new image and show it with its top-left at (x, y) (CRTC
    /// coordinates, may be negative). False: refused, nothing changed.
    pub set: fn(src: &KmsCursorSrc, x: i32, y: i32) -> bool,
    /// Same image, new position.
    pub move_to: fn(x: i32, y: i32),
    pub hide: fn(),
}

static FLIP: AtomicPtr<KmsFlip> = AtomicPtr::new(core::ptr::null_mut());
static CURSOR: AtomicPtr<KmsCursor> = AtomicPtr::new(core::ptr::null_mut());

/// Install the real-vblank flip half (before `register`).
pub fn register_flip(f: &'static KmsFlip) {
    FLIP.store(f as *const KmsFlip as *mut KmsFlip, Ordering::Release);
}

/// Install the hardware cursor half (before `register`).
pub fn register_cursor(c: &'static KmsCursor) {
    CURSOR.store(c as *const KmsCursor as *mut KmsCursor, Ordering::Release);
}

/// The flip half, while a backend is registered.
#[inline]
pub fn flip() -> Option<&'static KmsFlip> {
    if !active() { return None; }
    let p = FLIP.load(Ordering::Acquire);
    if p.is_null() { None } else { Some(unsafe { &*p }) }
}

/// The cursor half, while a backend is registered.
#[inline]
pub fn cursor() -> Option<&'static KmsCursor> {
    if !active() { return None; }
    let p = CURSOR.load(Ordering::Acquire);
    if p.is_null() { None } else { Some(unsafe { &*p }) }
}

/// Is the DRM cursor plane exposed? Always without a backend (virtio-gpu's
/// host cursor), only with a cursor half with one.
#[inline]
pub fn cursor_plane_exposed() -> bool {
    !active() || cursor().is_some()
}

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

/// The console owns the scanout again: let a flipping backend put the boot
/// loader's surface back. Lock-free.
pub fn console_restore() {
    if let Some(f) = backend().and_then(|b| b.console_restore) { f(); }
}

// ── Extra connectors (lane lapdisp) ─────────────────────────────────────────
//
// A backend with physical outputs beside its scanout (i915: the HDMI and
// DP/USB-C ports) lists them as extra connectors after connector 1. They have
// no CRTC yet (possible_crtcs 0, encoder unbound), so a compositor enumerates
// them with their true status and EDID modes but cannot light them: inert
// until a second-pipe modeset exists. Unregistered: nothing changes.

/// First extra connector id; extra encoder `i` is `EXT_ENC_BASE + i`. Clear of
/// the property ids (40..=70) and of the single CRTC/connector/encoder (1).
pub const EXT_CONN_BASE: u32 = 0x100;
pub const EXT_ENC_BASE: u32 = 0x180;
pub const EXT_MAX: usize = 8;

/// One timing, DRM units (kHz, pixels, lines; flags = DRM_MODE_FLAG_*).
#[derive(Clone, Copy, Default, Debug)]
pub struct KmsTiming {
    pub clock_khz: u32,
    pub h: [u16; 4],
    pub v: [u16; 4],
    pub flags: u32,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct KmsConnInfo {
    /// DRM_MODE_CONNECTOR_* (10 DisplayPort, 11 HDMI-A).
    pub conn_type: u32,
    pub type_id: u32,
    /// DRM_MODE_ENCODER_* for its encoder (2 TMDS, as Linux i915 for DP/HDMI).
    pub enc_type: u32,
    pub connected: bool,
    pub mm_w: u32,
    pub mm_h: u32,
    pub modes: [KmsTiming; 4],
    pub nmodes: usize,
    /// EDID block 0 as read over DDC/AUX (`[[u8; 16]; 8]` = 128 bytes, kept
    /// nested so `Default` derives); `edid_len` 0 = none / invalid.
    pub edid: [[u8; 16]; 8],
    pub edid_len: usize,
}

impl KmsConnInfo {
    /// The raw EDID bytes, empty when the sink gave no valid one.
    pub fn edid_vec(&self) -> alloc::vec::Vec<u8> {
        if self.edid_len == 0 { return alloc::vec::Vec::new(); }
        self.edid.iter().flatten().copied().take(self.edid_len.min(128)).collect()
    }
    pub fn set_edid(&mut self, e: &[u8; 128]) {
        for (k, c) in self.edid.iter_mut().enumerate() { c.copy_from_slice(&e[k * 16..k * 16 + 16]); }
        self.edid_len = 128;
    }
}

pub struct KmsOutputs {
    pub count: fn() -> usize,
    /// Task context, no DRM lock held (GETCONNECTOR): may probe the port.
    pub info: fn(idx: usize) -> Option<KmsConnInfo>,
    /// Lock-free cached view (GETENCODER / GETRESOURCES, under the DRM lock).
    pub cached: fn(idx: usize) -> Option<KmsConnInfo>,
    /// Possible-CRTC bitmask of extra `idx` (0: no CRTC can drive it).
    pub crtc_mask: fn(idx: usize) -> u32,
}

static INTERNAL_PANEL: AtomicBool = AtomicBool::new(false);

/// The backend scans out an internal panel (eDP): connector 1 reports eDP.
pub fn set_internal_panel(v: bool) { INTERNAL_PANEL.store(v, Ordering::Release); }
pub fn internal_panel() -> bool { INTERNAL_PANEL.load(Ordering::Acquire) }

/// Possible-CRTC mask of extra `idx`, 0 when no backend/CRTC.
pub fn ext_crtc_mask(idx: usize) -> u32 { outputs().map_or(0, |o| (o.crtc_mask)(idx)) | pipe2_mask(idx) }

static OUTPUTS: AtomicPtr<KmsOutputs> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_outputs(o: &'static KmsOutputs) {
    OUTPUTS.store(o as *const KmsOutputs as *mut KmsOutputs, Ordering::Release);
}

pub fn outputs() -> Option<&'static KmsOutputs> {
    let p = OUTPUTS.load(Ordering::Acquire);
    if p.is_null() { None } else { Some(unsafe { &*p }) }
}

/// Number of extra connectors (0 when none registered).
pub fn ext_count() -> usize { outputs().map_or(0, |o| (o.count)().min(EXT_MAX)) }

/// `id` -> extra index when `id` is `base + i` for a registered extra.
pub fn ext_index(id: u32, base: u32) -> Option<usize> {
    let i = id.checked_sub(base)? as usize;
    if i < ext_count() { Some(i) } else { None }
}

// ── second pipe (lane lapdisp C1) ───────────────────────────────────────────

/// The second CRTC's backend hooks. Pipe A's `KmsBackend`/`KmsFlip` stay as
/// they are; a backend with a second pipe registers this beside them. Every
/// hook runs in task context with no DRM lock held, except `busy` (any
/// context, lock-free).
pub struct KmsPipe2 {
    pub name: &'static str,
    /// Light extra connector `conn_idx` with `t`.
    pub modeset: fn(conn_idx: usize, t: &KmsTiming) -> Result<(), DriverError>,
    /// Turn the pipe off.
    pub disable: fn(),
    /// Queue `token` for scanout; `ev` (nonzero) completes when it latched.
    pub queue: fn(token: u32, src_x: u32, src_y: u32, ev: u32) -> Result<(), DriverError>,
    /// A commit that flipped nothing: complete `ev` at the next vblank.
    pub event: fn(ev: u32),
    pub busy: fn() -> bool,
    pub wait_idle: fn(timeout_ns: u64) -> bool,
    /// The cursor plane of pipe B (None: plane 33 is not exposed).
    pub cursor: Option<&'static KmsCursor>,
    /// The current mode, `(w, h, refresh)`, while the pipe is on.
    pub mode: fn() -> Option<(u32, u32, u32)>,
    /// Accept plain dumb framebuffers (no backend token): the stub only.
    pub dumb_ok: bool,
}

static PIPE2: AtomicPtr<KmsPipe2> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_pipe2(p: &'static KmsPipe2) {
    PIPE2.store(p as *const KmsPipe2 as *mut KmsPipe2, Ordering::Release);
}

/// The second pipe's hooks, while a backend and a second pipe are registered.
#[inline]
pub fn pipe2() -> Option<&'static KmsPipe2> {
    if !active() { return None; }
    let p = PIPE2.load(Ordering::Acquire);
    if p.is_null() { None } else { Some(unsafe { &*p }) }
}

/// CRTCs beyond the first (0 unless a second pipe is registered).
#[inline]
pub fn extra_crtcs() -> usize { pipe2().map_or(0, |_| 1) }

/// `possible_crtcs` of extra connector `idx`'s encoder.
#[inline]
pub fn pipe2_mask(_idx: usize) -> u32 { if pipe2().is_some() { 0b10 } else { 0 } }
