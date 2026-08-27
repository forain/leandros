//! drmsmoke — K4 rung R0: raw-ioctl DRM smoke test on /dev/dri/card0.
//!
//! Exercises the buffer + legacy-KMS ioctl surface that Mesa/GBM (kms_swrast)
//! and a legacy Smithay backend issue, plus the st_rdev plumbing libdrm needs:
//!   fstat st_rdev == 226:0; VERSION; GET_CAP(DUMB_BUFFER/TIMESTAMP_MONOTONIC);
//!   GETRESOURCES; GETCONNECTOR (connected, >=1 mode); CREATE_DUMB; MAP_DUMB;
//!   mmap + fill gradient; ADDFB2; SETCRTC; DIRTYFB; DESTROY_DUMB.
//!
//! It also drives a real plane-only DRM_IOCTL_MODE_ATOMIC commit — hand-built
//! (obj, prop, value) triples, no libdrm — and checks both that its pixels
//! reach the scanout and that it claims the framebuffer console, which the
//! legacy-KMS path alone cannot prove.
//!
//! Prints "<name>: PASS"/"<name>: FAIL" per step; returns the failure count as
//! the exit code. On success the screen shows a full-screen gradient (the
//! screenshot accept criterion).
//!
//! `--hold` mode skips the PAGE_FLIP/PRIME/fork-with-a-device-mapping checks
//! and, once SETCRTC lands, paints a deterministic, screendump-checkable
//! image and holds it forever (never DESTROY_DUMB, never closes the fd,
//! never exits): the whole framebuffer is filled with the flat field colour
//! **0x181818**, then a **256x256 solid 0xFF0000 (pure red)** block is
//! painted with its top-left corner at pixel **(64, 64)**. Once that content
//! is flushed to the host it prints the sentinel line `DRMSMOKE: HOLD READY`
//! to stdout, so a QEMU `screendump` can be pixel-checked against those exact
//! colours/coordinates.
//!
//! It then **presents that frame in a loop, ~10 times a second, forever** —
//! PAGE_FLIP and DIRTYFB, silently. That is what makes `--hold` a stand-in for
//! a live compositor rather than a still image: the property worth testing
//! against a VT switch is whether a client that never stops presenting can take
//! the display back from a console, and a client that presents once cannot
//! demonstrate either answer. Switching away must leave the console readable
//! while this loop keeps running; switching back must show the frame again with
//! no help from the client.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

type c_int = i32;
type c_uint = u32;
type c_ulong = u64;
type size_t = usize;

const O_RDONLY: c_int = 0o0;
const O_RDWR: c_int = 0o2;

const PROT_READ: c_int = 0x1;
const PROT_WRITE: c_int = 0x2;
const MAP_SHARED: c_int = 0x1;
const MAP_ANONYMOUS: c_int = 0x20;

// ── DRM ioctl request codes (64-bit, authoritative) ──────────────────────────
const DRM_IOCTL_VERSION: c_ulong = 0xC0406400;
const DRM_IOCTL_GET_CAP: c_ulong = 0xC010640C;
const DRM_IOCTL_MODE_GETRESOURCES: c_ulong = 0xC04064A0;
const DRM_IOCTL_MODE_GETCONNECTOR: c_ulong = 0xC05064A7;
const DRM_IOCTL_MODE_CREATE_DUMB: c_ulong = 0xC02064B2;
const DRM_IOCTL_MODE_MAP_DUMB: c_ulong = 0xC01064B3;
const DRM_IOCTL_MODE_ADDFB2: c_ulong = 0xC06864B8;
const DRM_IOCTL_MODE_SETCRTC: c_ulong = 0xC06864A2;
const DRM_IOCTL_MODE_DIRTYFB: c_ulong = 0xC01864B1;
const DRM_IOCTL_MODE_DESTROY_DUMB: c_ulong = 0xC00464B4;
const DRM_IOCTL_MODE_PAGE_FLIP: c_ulong = 0xC01864B0;
const DRM_IOCTL_MODE_ATOMIC: c_ulong = 0xC03864BC;
const DRM_IOCTL_PRIME_HANDLE_TO_FD: c_ulong = 0xC00C642D;
const DRM_IOCTL_PRIME_FD_TO_HANDLE: c_ulong = 0xC00C642E;

// ── Sync objects ─────────────────────────────────────────────────────────────
// _IOWR('d', nr, struct) = 0xC0000000 | size<<16 | 0x6400 | nr. Sizes: create
// and destroy 8, array 16, wait 32, timeline_wait 40. Same arithmetic as
// ATOMIC's 0xC038_64BC above (nr 0xBC, 56-byte struct).
const DRM_IOCTL_SYNCOBJ_CREATE: c_ulong = 0xC00864BF;
const DRM_IOCTL_SYNCOBJ_DESTROY: c_ulong = 0xC00864C0;
const DRM_IOCTL_SYNCOBJ_WAIT: c_ulong = 0xC02064C3;
const DRM_IOCTL_SYNCOBJ_RESET: c_ulong = 0xC01064C4;
const DRM_IOCTL_SYNCOBJ_SIGNAL: c_ulong = 0xC01064C5;
const DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT: c_ulong = 0xC02864CA;

const DRM_SYNCOBJ_CREATE_SIGNALED: u32 = 1 << 0;
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL: u32 = 1 << 0;
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT: u32 = 1 << 1;

const DRM_CAP_SYNCOBJ: u64 = 0x13;
const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x14;

const EINVAL: i32 = 22;
const ENOENT: i32 = 2;
const ETIME: i32 = 62;
const ENOSYS: i32 = 38;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmSyncobjCreate { handle: u32, flags: u32 }

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmSyncobjDestroy { handle: u32, pad: u32 }

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmSyncobjArray { handles: u64, count_handles: u32, pad: u32 }

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmSyncobjWait {
    handles: u64,
    timeout_nsec: i64,
    count_handles: u32,
    flags: u32,
    first_signaled: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmSyncobjTimelineWait {
    handles: u64,
    points: u64,
    timeout_nsec: i64,
    count_handles: u32,
    flags: u32,
    first_signaled: u32,
    pad: u32,
}

const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;

// Atomic-commit flags. ALLOW_MODESET is deliberately never set below: the
// plane-only request names no ACTIVE / MODE_ID / connector CRTC_ID, so it does
// not change the modeset and the driver must accept it without one.
const DRM_MODE_ATOMIC_TEST_ONLY: u32 = 0x0100;

// Object and property ids of the atomic pipeline, as the driver hardcodes them
// (`DRM_PLANE_ID` / `PROP_*` in drivers/src/drm_device_interface.rs). There is
// no GETPROPERTY-by-name lookup here on purpose: the point of this test is to
// drive the same numbers the driver folds the request with.
const DRM_OBJ_PRIMARY_PLANE: u32 = 30;
const DRM_OBJ_CRTC: u32 = 1;
const PROP_PLANE_CRTC_ID: u32 = 41;
const PROP_FB_ID: u32 = 42;
const PROP_SRC_X: u32 = 43;
const PROP_SRC_Y: u32 = 44;
const PROP_SRC_W: u32 = 45;
const PROP_SRC_H: u32 = 46;
const PROP_CRTC_X: u32 = 47;
const PROP_CRTC_Y: u32 = 48;
const PROP_CRTC_W: u32 = 49;
const PROP_CRTC_H: u32 = 50;

const POLLIN: i16 = 0x001;

const DRM_CAP_DUMB_BUFFER: u64 = 0x1;
const DRM_CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;


// ── V3D (Broadcom VideoCore VI/VII 3D core) ──────────────────────────────────
// `_IOC(dir, type, nr, size) = dir<<30 | size<<16 | type<<8 | nr`, DRM's type
// 'd' = 0x64, `nr = DRM_COMMAND_BASE (0x40) + index`, _IOWR = 0xC000_0000 and
// _IOW = 0x4000_0000. Sizes are `sizeof` of the structs in Mesa's vendored
// `include/drm-uapi/v3d_drm.h`. Same arithmetic as the syncobj codes above.
//
//   SUBMIT_CL     idx 0x00 -> nr 0x40, 72 B -> 0xC0486440
//   WAIT_BO       idx 0x01 -> nr 0x41, 16 B -> 0xC0106441
//   CREATE_BO     idx 0x02 -> nr 0x42, 16 B -> 0xC0106442
//   MMAP_BO       idx 0x03 -> nr 0x43, 16 B -> 0xC0106443
//   GET_PARAM     idx 0x04 -> nr 0x44, 16 B -> 0xC0106444
//   GET_BO_OFFSET idx 0x05 -> nr 0x45,  8 B -> 0xC0086445
//   SUBMIT_TFU    idx 0x06 -> nr 0x46, 88 B, _IOW -> 0x40586446
//   PERFMON_CREATE idx 0x08 -> nr 0x48, 40 B -> 0xC0286448
//
// ⚠ WAIT_BO and MMAP_BO are BIT-IDENTICAL to VIRTGPU_MAP and VIRTGPU_GETPARAM
// respectively — both drivers number from DRM_COMMAND_BASE and the structs
// happen to match in size. The number does not identify the operation; the
// armed backend does. That is what V3D_ARM below is for.
const DRM_IOCTL_V3D_SUBMIT_CL: c_ulong = 0xC0486440;
const DRM_IOCTL_V3D_WAIT_BO: c_ulong = 0xC0106441;
const DRM_IOCTL_V3D_CREATE_BO: c_ulong = 0xC0106442;
const DRM_IOCTL_V3D_MMAP_BO: c_ulong = 0xC0106443;
const DRM_IOCTL_V3D_GET_PARAM: c_ulong = 0xC0106444;
const DRM_IOCTL_V3D_GET_BO_OFFSET: c_ulong = 0xC0086445;
const DRM_IOCTL_V3D_SUBMIT_TFU: c_ulong = 0x40586446;
const DRM_IOCTL_V3D_PERFMON_CREATE: c_ulong = 0xC0286448;

const DRM_IOCTL_SET_CLIENT_CAP: c_ulong = 0x4010640D;
/// Private client capability that arms the kernel's v3d backend. Not upstream:
/// see "Backend selection" in drivers/src/drm_device_interface.rs for why this
/// entry point rather than a new ioctl number.
const DRM_CLIENT_CAP_LEANDROS_V3D: u64 = 0x1000_0003;

const V3D_PARAM_HUB_IDENT3: u32 = 3;
const V3D_PARAM_CORE0_IDENT0: u32 = 4;
const V3D_PARAM_CORE0_IDENT1: u32 = 5;
const V3D_PARAM_SUPPORTS_TFU: u32 = 7;
const V3D_PARAM_SUPPORTS_CSD: u32 = 8;
const V3D_PARAM_SUPPORTS_PERFMON: u32 = 10;
const V3D_PARAM_SUPPORTS_MULTISYNC_EXT: u32 = 11;
const V3D_PARAM_MAX_PERF_COUNTERS: u32 = 13;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmSetClientCap { capability: u64, value: u64 }

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmV3dGetParam { param: u32, pad: u32, value: u64 }

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmV3dCreateBo { size: u32, flags: u32, handle: u32, offset: u32 }

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmV3dMmapBo { handle: u32, flags: u32, offset: u64 }

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmV3dGetBoOffset { handle: u32, offset: u32 }

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmV3dWaitBo { handle: u32, pad: u32, timeout_ns: u64 }

/// 72 bytes. Ten u32 then a naturally-aligned u64 at offset 40, four more u32,
/// and a u64 at offset 64 — no interior padding anywhere, which is why the
/// declaration order below IS the wire layout.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmV3dSubmitCl {
    bcl_start: u32,
    bcl_end: u32,
    rcl_start: u32,
    rcl_end: u32,
    in_sync_bcl: u32,
    in_sync_rcl: u32,
    out_sync: u32,
    qma: u32,
    qms: u32,
    qts: u32,
    bo_handles: u64,
    bo_handle_count: u32,
    flags: u32,
    perfmon_id: u32,
    pad: u32,
    extensions: u64,
}

// ── DRM structs (fixed-width, identical on x86_64 == aarch64) ─────────────────
#[repr(C)]
#[derive(Default)]
struct DrmVersion {
    version_major: i32,
    version_minor: i32,
    version_patchlevel: i32,
    name_len: usize,
    name: u64,
    date_len: usize,
    date: u64,
    desc_len: usize,
    desc: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmGetCap {
    capability: u64,
    value: u64,
}

#[repr(C)]
#[derive(Default)]
struct DrmModeCardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Default)]
struct DrmModeModeinfo {
    clock: u32,
    hdisplay: u16, hsync_start: u16, hsync_end: u16, htotal: u16, hskew: u16,
    vdisplay: u16, vsync_start: u16, vsync_end: u16, vtotal: u16, vscan: u16,
    vrefresh: u32,
    flags: u32,
    type_: u32,
    name: [u8; 32],
}

#[repr(C)]
#[derive(Default)]
struct DrmModeGetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct DrmModeCreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Default)]
struct DrmModeMapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Default)]
struct DrmModeFbCmd2 {
    fb_id: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    handles: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
    modifier: [u64; 4],
}

#[repr(C)]
struct DrmModeCrtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: DrmModeModeinfo,
}

#[repr(C)]
#[derive(Default)]
struct DrmModeCrtcPageFlip {
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    reserved: u32,
    user_data: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmEventVblank {
    ev_type: u32,
    length: u32,
    user_data: u64,
    tv_sec: u32,
    tv_usec: u32,
    sequence: u32,
    crtc_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct pollfd {
    fd: c_int,
    events: i16,
    revents: i16,
}

#[repr(C)]
#[derive(Default)]
struct DrmPrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}

/// `struct drm_mode_atomic`, byte for byte as the driver reads it:
///   u32 flags@0, u32 count_objs@4, u64 objs_ptr@8, u64 count_props_ptr@16,
///   u64 props_ptr@24, u64 prop_values_ptr@32, u64 reserved@40, u64 user_data@48.
/// `objs_ptr` carries bare object ids with no type tag — the driver recovers
/// the object class from the property id, whose ranges are disjoint per class.
#[repr(C)]
#[derive(Default)]
struct DrmModeAtomic {
    flags: u32,
    count_objs: u32,
    objs_ptr: u64,
    count_props_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    reserved: u64,
    user_data: u64,
}

#[repr(C)]
#[derive(Default)]
struct DrmModeFbDirtyCmd {
    fb_id: u32,
    flags: u32,
    color: u32,
    num_clips: u32,
    clips_ptr: u64,
}

const DRM_FORMAT_XRGB8888: u32 = 0x34325258; // 'X''R''2''4'

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: c_int, buf: *const c_void, count: size_t) -> isize;
    pub fn exit(status: c_int) -> !;

    pub fn open(path: *const u8, oflag: c_int, ...) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    // Used only by the fork-with-a-device-mapping check; same relibc-linked
    // idiom as forktest.
    pub fn fork() -> i32;
    pub fn waitpid(pid: i32, stat_loc: *mut c_int, options: c_int) -> i32;
    pub fn _exit(status: c_int) -> !;
    pub fn usleep(usec: c_uint) -> c_int;
    pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    pub fn fstat(fildes: c_int, buf: *mut u8) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, count: size_t) -> isize;
    pub fn poll(fds: *mut pollfd, nfds: u64, timeout: c_int) -> c_int;
    pub fn mmap(addr: *mut c_void, len: size_t, prot: c_int, flags: c_int,
                fd: c_int, offset: i64) -> *mut c_void;
    // Syncobj timeouts are ABSOLUTE CLOCK_MONOTONIC nanoseconds, so the test
    // has to read the same clock the kernel compares against.
    pub fn clock_gettime(clk_id: c_int, tp: *mut timespec) -> c_int;
    // relibc's errno. The syncobj lane is the first part of this file where the
    // *errno value* is the thing under test (ETIME vs ENOENT vs EINVAL decide
    // what Mesa does next), not merely whether the ioctl failed.
    pub fn __errno_location() -> *mut c_int;
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct timespec { tv_sec: i64, tv_nsec: i64 }

const CLOCK_MONOTONIC: c_int = 1;

unsafe fn errno() -> i32 { *__errno_location() }

unsafe fn monotonic_ns() -> u64 {
    let mut ts = timespec::default();
    if clock_gettime(CLOCK_MONOTONIC, &mut ts as *mut _) != 0 { return 0; }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset drm_main",
    "   and rsp, -16",
    "   call relibc_start_v1",
    "   ud2"
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   mov x29, #0",
    "   mov x30, #0",
    "   mov x0, sp",
    "   adrp x1, drm_main",
    "   add x1, x1, :lo12:drm_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

// st_rdev byte offset differs by arch (matches vfs write_stat_full_rdev).
#[cfg(target_arch = "x86_64")]
const ST_RDEV_OFF: usize = 40;
#[cfg(target_arch = "aarch64")]
const ST_RDEV_OFF: usize = 32;

// Matches argv[i] (a NUL-terminated C string) against a Rust byte-string
// literal (no embedded NUL needed in `s`).
fn arg_is(p: *const u8, s: &[u8]) -> bool {
    if p.is_null() { return false; }
    unsafe {
        let mut i = 0usize;
        loop {
            let c = *p.add(i);
            let want = if i < s.len() { s[i] } else { 0 };
            if c != want { return false; }
            if c == 0 { return true; }
            i += 1;
        }
    }
}

// Fills the whole [0,w)x[0,h) framebuffer with the flat field colour
// 0x181818, then overpaints a 256x256 pure-red (0xFF0000) block whose
// top-left corner sits at (64, 64) — clamped so it never runs past the
// buffer on a smaller-than-expected mode. XRGB8888 byte order matches the
// gradient fill above: byte0=B, byte1=G, byte2=R, byte3=pad.
unsafe fn paint_field_and_block(base: *mut u8, pitch: usize, w: usize, h: usize) {
    let mut y = 0usize;
    while y < h {
        let mut x = 0usize;
        while x < w {
            let off = y * pitch + x * 4;
            *base.add(off) = 0x18;     // B
            *base.add(off + 1) = 0x18; // G
            *base.add(off + 2) = 0x18; // R
            *base.add(off + 3) = 0;    // X
            x += 1;
        }
        y += 1;
    }

    let bx0 = 64usize;
    let by0 = 64usize;
    let bw = 256usize.min(w.saturating_sub(bx0));
    let bh = 256usize.min(h.saturating_sub(by0));
    let mut y = 0usize;
    while y < bh {
        let mut x = 0usize;
        while x < bw {
            let off = (by0 + y) * pitch + (bx0 + x) * 4;
            *base.add(off) = 0x00;     // B
            *base.add(off + 1) = 0x00; // G
            *base.add(off + 2) = 0xFF; // R
            *base.add(off + 3) = 0;    // X
            x += 1;
        }
        y += 1;
    }
}

/// Fills a buffer with the ATOMIC-lane pattern: red pinned at `ATOMIC_ROW_R`,
/// green flat, blue ramping left to right. Same shape as the SETCRTC gradient
/// but a different constant red, which is what lets `fb0_census` say WHICH of
/// the two is currently on the scanout rather than merely "something is".
unsafe fn paint_atomic_pattern(base: *mut u8, pitch: usize, w: usize, h: usize) {
    let mut y = 0usize;
    while y < h {
        let mut x = 0usize;
        while x < w {
            let off = y * pitch + x * 4;
            *base.add(off) = (x * 255 / w) as u8; // B — ramps, same as the gradient
            *base.add(off + 1) = 0x20;            // G
            *base.add(off + 2) = ATOMIC_ROW_R;    // R — the discriminator
            *base.add(off + 3) = 0;               // X
            x += 1;
        }
        y += 1;
    }
}

/// Issue one plane-only atomic commit for `fb_id` on the primary plane.
///
/// The request is built by hand from the flattened (obj, prop, value) triples
/// `std_handle_atomic` folds — one object (the primary plane), ten properties,
/// no CRTC or connector object at all. Naming no ACTIVE / MODE_ID / connector
/// CRTC_ID is what makes it a non-modeset commit, so it is legal without
/// ALLOW_MODESET; that is exactly the shape a compositor's steady-state frame
/// has. SRC_* are 16.16 fixed point (the driver shifts them right by 16),
/// CRTC_* are plain integers.
unsafe fn atomic_plane_commit(fd: c_int, fb_id: u32, w: u32, h: u32, flags: u32) -> c_int {
    let objs: [u32; 1] = [DRM_OBJ_PRIMARY_PLANE];
    let counts: [u32; 1] = [10];
    let props: [u32; 10] = [
        PROP_PLANE_CRTC_ID, PROP_FB_ID,
        PROP_SRC_X, PROP_SRC_Y, PROP_SRC_W, PROP_SRC_H,
        PROP_CRTC_X, PROP_CRTC_Y, PROP_CRTC_W, PROP_CRTC_H,
    ];
    let vals: [u64; 10] = [
        DRM_OBJ_CRTC as u64,
        fb_id as u64,
        0,
        0,
        (w as u64) << 16,
        (h as u64) << 16,
        0,
        0,
        w as u64,
        h as u64,
    ];

    let mut req = DrmModeAtomic::default();
    req.flags = flags;
    req.count_objs = objs.len() as u32;
    req.objs_ptr = objs.as_ptr() as u64;
    req.count_props_ptr = counts.as_ptr() as u64;
    req.props_ptr = props.as_ptr() as u64;
    req.prop_values_ptr = vals.as_ptr() as u64;
    ioctl(fd, DRM_IOCTL_MODE_ATOMIC, &mut req as *mut _)
}

fn report(name: &[u8], ok: bool) -> bool {
    unsafe {
        write(1, name.as_ptr() as *const c_void, name.len());
        if ok {
            write(1, b": PASS\n".as_ptr() as *const c_void, 7);
        } else {
            write(1, b": FAIL\n".as_ptr() as *const c_void, 7);
        }
    }
    ok
}

// Prints "<label><v>\n" in decimal — used by FLIP_TS_SUBTICK to put the raw
// observed tv_sec/tv_usec values in the serial log so a human can see the
// actual numbers, not just PASS/FAIL.
unsafe fn print_dec(label: &[u8], v: u64) {
    write(1, label.as_ptr() as *const c_void, label.len());
    let mut buf = [0u8; 20];
    let mut n = 0usize;
    let mut x = v;
    if x == 0 {
        buf[0] = b'0';
        n = 1;
    } else {
        while x > 0 {
            buf[n] = b'0' + (x % 10) as u8;
            n += 1;
            x /= 10;
        }
    }
    let mut out = [0u8; 20];
    for i in 0..n { out[i] = buf[n - 1 - i]; }
    write(1, out.as_ptr() as *const c_void, n);
    write(1, b"\n".as_ptr() as *const c_void, 1);
}

// ── Console-vs-scanout census ────────────────────────────────────────────────
//
// /dev/fb0 reads the live hardware framebuffer — the SAME surface the DRM
// present path composites into and the SAME surface the kernel's framebuffer
// console draws and scrolls. That makes the whole check possible in-guest: take
// the scanout, fingerprint the surface, provoke the console, fingerprint again.
// Byte-identity is the invariant, so it needs no per-arch pixel constants and
// does not care what the pitch is.

const FB_CHUNK: usize = 8192;
static mut FB_BUF: [u8; FB_CHUNK] = [0u8; FB_CHUNK];

/// Constant red channel of the SETCRTC gradient, and of the ATOMIC-lane
/// pattern. Two patterns that differ ONLY in this byte let one row check
/// answer "which of the two presents landed", so neither lane can pass on the
/// other's pixels: the atomic guard demands 0x80 on screen at a point where
/// SETCRTC has not run yet, and FB0_SHOWS_SCANOUT afterwards demands 0x40,
/// which the atomic present cannot supply.
const GRADIENT_ROW_R: u8 = 0x40;
const ATOMIC_ROW_R: u8 = 0x80;

/// FNV-1a 64 over every byte /dev/fb0 will hand us, plus the byte count.
/// Returns (hash, bytes, first_row_ok) where `first_row_ok` is the plumbing
/// self-check: the first chunk must look like the pattern the caller expects to
/// be scanned out right now (XRGB8888, R fixed at `want_r`, B ramping with x).
/// If that is false the census is reading something other than the scanout —
/// or the present under test never landed — and every later verdict built on
/// this read is meaningless.
unsafe fn fb0_census(width: usize, want_r: u8) -> (u64, u64, bool) {
    let fd = open(b"/dev/fb0\0".as_ptr(), O_RDONLY);
    if fd < 0 { return (0, 0, false); }

    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut total: u64 = 0;
    let mut first = true;
    let mut row_ok = false;

    loop {
        let n = read(fd, FB_BUF.as_mut_ptr() as *mut c_void, FB_CHUNK);
        if n <= 0 { break; }
        let n = n as usize;
        if first {
            first = false;
            // The whole first row fits in one chunk for every mode we run
            // (1280*4 and 1920*4 are both under 8 KiB).
            let row_bytes = width * 4;
            if row_bytes >= 8 && row_bytes <= n {
                let r_left = FB_BUF[2];
                let r_right = FB_BUF[row_bytes - 2];
                let b_left = FB_BUF[0];
                let b_right = FB_BUF[row_bytes - 4];
                row_ok = r_left == want_r && r_right == want_r && b_right > b_left;
            }
        }
        let mut i = 0usize;
        while i < n {
            hash ^= FB_BUF[i] as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            i += 1;
        }
        total += n as u64;
    }
    close(fd);
    (hash, total, row_ok)
}

/// Everything a guest program does that used to walk over a live display:
/// a short-lived second open of card0 (its close used to hand the console
/// back — and reclaim CLEARS the screen), then enough console output to scroll
/// the framebuffer well past a full screen.
unsafe fn provoke_console(fd_hold: c_int) {
    let probe = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
    if probe >= 0 && probe != fd_hold { close(probe); }

    // 1280x800 is 50 text rows and 1920x1080 is 67; 160 lines scrolls the
    // whole surface off at least twice on either.
    let mut i = 0u64;
    while i < 160 {
        print_dec(b"  drmsmoke console noise ", i);
        i += 1;
    }
}

// Sink for spin_delay's accumulator, written with a volatile store so a
// release build cannot prove the busy-loop is dead and elide it.
static mut SPIN_SINK: u64 = 0;

// Pure CPU busy-work — NOT a sleep. FLIP_TS_SUBTICK uses this instead of
// usleep() to shift the real-time offset at which each flip ioctl is issued:
// usleep()/nanosleep() round every nonzero request UP to a whole tick (see
// sys_nanosleep in kernel/src/syscall.rs), so a sleep between samples just
// resyncs to the next tick boundary — it cannot break a tick-phase lock, it
// reinforces one. A plain instruction-counted spin runs at wall-clock speed
// instead, so varying `iters` across samples varies the wall-clock phase at
// which the next ioctl lands within its tick.
unsafe fn spin_delay(iters: u64) {
    let mut acc: u64 = 0;
    let mut n = 0u64;
    while n < iters {
        acc = acc.wrapping_add(n ^ 0x9E37_79B9);
        n += 1;
    }
    core::ptr::write_volatile(core::ptr::addr_of_mut!(SPIN_SINK), acc);
}

#[no_mangle]
pub unsafe extern "C" fn drm_main(argc: isize, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut failures = 0i32;

    let hold_mode = argc > 1 && arg_is(*argv.add(1) as *const u8, b"--hold");

    let fd = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
    if fd < 0 {
        report(b"open_card0", false);
        puts(b"--- drmsmoke done (open failed) ---\n\0".as_ptr());
        return 1;
    }
    report(b"open_card0", true);

    // ── `--arm-v3d` / `--disarm-v3d`: set the backend flag and leave ─────────
    //
    // The v3d backend flag is DEVICE-GLOBAL and survives the fd that set it,
    // which is not an accident — it changes the `DRM_IOCTL_VERSION` identity,
    // and Mesa reads that identity in `pipe_loader_drm.c` before it has created
    // anything. So the only way to point a SEPARATE process (Mesa, kmscube,
    // cosmic-comp) at the v3d ABI is for something to arm it first and exit.
    // That is what this mode is: two ioctls and nothing else.
    //
    // Kept out of the main suite, which arms and disarms around its own block,
    // so a normal `drmsmoke` run still leaves the machine exactly as it found
    // it. Leaving the device armed by accident would send the next Mesa process
    // hunting for a driver that cannot work under QEMU.
    if argc > 1 {
        let a1 = *argv.add(1) as *const u8;
        let arm = arg_is(a1, b"--arm-v3d");
        if arm || arg_is(a1, b"--disarm-v3d") {
            let mut cc = DrmSetClientCap {
                capability: DRM_CLIENT_CAP_LEANDROS_V3D,
                value: if arm { 1 } else { 0 },
            };
            let ok = ioctl(fd, DRM_IOCTL_SET_CLIENT_CAP, &mut cc as *mut _) == 0;
            // Read the identity back, so the line printed is what the device
            // will actually tell Mesa rather than what we asked for.
            let mut nb = [0u8; 32];
            let mut vv = DrmVersion::default();
            vv.name_len = nb.len();
            vv.name = nb.as_mut_ptr() as u64;
            ioctl(fd, DRM_IOCTL_VERSION, &mut vv as *mut _);
            write(1, b"drmsmoke: DRM driver name is now \"".as_ptr() as *const c_void, 34);
            write(1, nb.as_ptr() as *const c_void, vv.name_len);
            write(1, b"\"\n".as_ptr() as *const c_void, 2);
            close(fd);
            return if ok { 0 } else { 1 };
        }
    }

    // st_rdev == 226:0 == 0xE200
    let mut stbuf = [0u8; 160];
    let rdev_ok = if fstat(fd, stbuf.as_mut_ptr()) == 0 {
        let rdev = core::ptr::read_unaligned(stbuf.as_ptr().add(ST_RDEV_OFF) as *const u64);
        rdev == 0xE200
    } else {
        false
    };
    if !report(b"st_rdev_226_0", rdev_ok) { failures += 1; }

    // VERSION
    let mut namebuf = [0u8; 64];
    let mut ver = DrmVersion::default();
    ver.name = namebuf.as_mut_ptr() as u64;
    ver.name_len = namebuf.len();
    let ver_ok = ioctl(fd, DRM_IOCTL_VERSION, &mut ver as *mut _) == 0 && namebuf[0] != 0;
    if !report(b"VERSION", ver_ok) { failures += 1; }

    // GET_CAP(DUMB_BUFFER) == 1
    let mut cap = DrmGetCap { capability: DRM_CAP_DUMB_BUFFER, value: 0 };
    let cap_dumb_ok = ioctl(fd, DRM_IOCTL_GET_CAP, &mut cap as *mut _) == 0 && cap.value == 1;
    if !report(b"GET_CAP_DUMB_BUFFER", cap_dumb_ok) { failures += 1; }

    // GET_CAP(TIMESTAMP_MONOTONIC) == 1
    let mut cap2 = DrmGetCap { capability: DRM_CAP_TIMESTAMP_MONOTONIC, value: 0 };
    let cap_ts_ok = ioctl(fd, DRM_IOCTL_GET_CAP, &mut cap2 as *mut _) == 0 && cap2.value == 1;
    if !report(b"GET_CAP_TIMESTAMP_MONOTONIC", cap_ts_ok) { failures += 1; }

    // GETRESOURCES — expect >=1 crtc/connector/encoder, sane min/max
    let mut res = DrmModeCardRes::default();
    let res_ok = ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut res as *mut _) == 0
        && res.count_crtcs >= 1 && res.count_connectors >= 1
        && res.max_width >= res.min_width && res.max_height >= res.min_height;
    if !report(b"GETRESOURCES", res_ok) { failures += 1; }

    // GETCONNECTOR — connected + >=1 mode. Two-pass: count then fill.
    let connector_id = 1u32; // GETRESOURCES reports connector id 1
    let mut conn = DrmModeGetConnector::default();
    conn.connector_id = connector_id;
    ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &mut conn as *mut _);
    let mut modes = [DrmModeModeinfo::default(); 1];
    conn.modes_ptr = modes.as_mut_ptr() as u64;
    conn.count_modes = 1;
    let mut conn2 = DrmModeGetConnector::default();
    conn2.connector_id = connector_id;
    conn2.modes_ptr = modes.as_mut_ptr() as u64;
    conn2.count_modes = 1;
    let conn_ok = ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &mut conn2 as *mut _) == 0
        && conn2.connection == 1 && conn2.count_modes >= 1
        && modes[0].hdisplay > 0 && modes[0].vdisplay > 0;
    if !report(b"GETCONNECTOR", conn_ok) { failures += 1; }

    let w = if modes[0].hdisplay > 0 { modes[0].hdisplay as u32 } else { 256 };
    let h = if modes[0].vdisplay > 0 { modes[0].vdisplay as u32 } else { 256 };

    // CREATE_DUMB (full display size so SETCRTC scans it out)
    let mut cd = DrmModeCreateDumb::default();
    cd.width = w;
    cd.height = h;
    cd.bpp = 32;
    let create_ok = ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut cd as *mut _) == 0 && cd.handle != 0;
    if !report(b"CREATE_DUMB", create_ok) { failures += 1; }

    // MAP_DUMB
    let mut md = DrmModeMapDumb::default();
    md.handle = cd.handle;
    let map_ok = ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut md as *mut _) == 0;
    if !report(b"MAP_DUMB", map_ok) { failures += 1; }

    // mmap + fill gradient
    let mut mmap_ok = false;
    let mut fb_ptr: *mut u8 = core::ptr::null_mut();
    if map_ok && cd.size > 0 {
        let p = mmap(core::ptr::null_mut(), cd.size as usize, PROT_READ | PROT_WRITE,
                     MAP_SHARED, fd, md.offset as i64);
        if p as isize > 0 {
            let pitch = cd.pitch as usize;
            let base = p as *mut u8;
            fb_ptr = base;
            let mut y = 0usize;
            while y < h as usize {
                let mut x = 0usize;
                while x < w as usize {
                    let off = y * pitch + x * 4;
                    // XRGB8888: gradient (blue by x, green by y)
                    *base.add(off) = (x * 255 / w as usize) as u8;       // B
                    *base.add(off + 1) = (y * 255 / h as usize) as u8;   // G
                    *base.add(off + 2) = GRADIENT_ROW_R;                 // R
                    *base.add(off + 3) = 0;                              // X
                    x += 1;
                }
                y += 1;
            }
            mmap_ok = true;
        }
    }
    if !report(b"MMAP_FILL", mmap_ok) { failures += 1; }

    // ADDFB2
    let mut fb = DrmModeFbCmd2::default();
    fb.width = w;
    fb.height = h;
    fb.pixel_format = DRM_FORMAT_XRGB8888;
    fb.handles[0] = cd.handle;
    fb.pitches[0] = cd.pitch;
    let addfb_ok = ioctl(fd, DRM_IOCTL_MODE_ADDFB2, &mut fb as *mut _) == 0 && fb.fb_id != 0;
    if !report(b"ADDFB2", addfb_ok) { failures += 1; }

    // ── Atomic KMS: present, and the console yield that present must produce ──
    //
    // This runs BEFORE SETCRTC, and that ordering is the whole point.
    //
    // The console gate used to be a hardcoded list of "console-killing" ioctl
    // numbers — two custom codes, SETCRTC and PAGE_FLIP — and
    // DRM_IOCTL_MODE_ATOMIC was never on it. So when the compositor moved to
    // the atomic path the gate silently stopped firing and the fb console
    // scrolled a live session's pixels away. A console-yield check placed
    // AFTER SETCRTC cannot see any of that: SETCRTC has already claimed the
    // scanout, so the console is silent before the atomic commit is even
    // issued and a missing ATOMIC arm costs nothing. Run it here and nothing
    // has presented yet, so the atomic commit is the only thing that can
    // silence the console — an implementation that does not treat it as a
    // present fails, which is precisely the regression that got shipped.
    //
    // Its own framebuffer, painted with a DIFFERENT constant red than the
    // SETCRTC gradient, so the two lanes cannot pass on each other's pixels:
    // the check below demands ATOMIC_ROW_R on screen while SETCRTC has not
    // run, and FB0_SHOWS_SCANOUT afterwards demands GRADIENT_ROW_R, which the
    // atomic present cannot supply.
    let mut acd = DrmModeCreateDumb::default();
    acd.width = w;
    acd.height = h;
    acd.bpp = 32;
    let mut afb = DrmModeFbCmd2::default();
    let mut atomic_setup_ok = false;
    if ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut acd as *mut _) == 0 && acd.handle != 0 {
        let mut amd = DrmModeMapDumb::default();
        amd.handle = acd.handle;
        if ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut amd as *mut _) == 0 && acd.size > 0 {
            let p = mmap(core::ptr::null_mut(), acd.size as usize, PROT_READ | PROT_WRITE,
                         MAP_SHARED, fd, amd.offset as i64);
            if p as isize > 0 {
                paint_atomic_pattern(p as *mut u8, acd.pitch as usize, w as usize, h as usize);
                afb.width = w;
                afb.height = h;
                afb.pixel_format = DRM_FORMAT_XRGB8888;
                afb.handles[0] = acd.handle;
                afb.pitches[0] = acd.pitch;
                atomic_setup_ok = ioctl(fd, DRM_IOCTL_MODE_ADDFB2, &mut afb as *mut _) == 0
                    && afb.fb_id != 0;
            }
        }
    }

    if atomic_setup_ok {
        // TEST_ONLY is validation only and must present nothing — smithay
        // issues these constantly, and one that presents would make every
        // probe a frame. Proven positively: the pattern must still be absent
        // from the scanout afterwards. (Byte-identity is deliberately NOT the
        // criterion here — the console is still live at this point and its own
        // output would move the hash. `row_ok` is a content fingerprint and is
        // immune to that.)
        let test_rc = atomic_plane_commit(fd, afb.fb_id, w, h, DRM_MODE_ATOMIC_TEST_ONLY);
        let (hash_t, bytes_t, row_t) = fb0_census(w as usize, ATOMIC_ROW_R);
        let test_only_ok = test_rc == 0 && bytes_t > 0 && !row_t;
        if !report(b"ATOMIC_TEST_ONLY_NO_PRESENT", test_only_ok) { failures += 1; }

        // The real thing: one plane-only commit, no modeset, no event.
        let commit_rc = atomic_plane_commit(fd, afb.fb_id, w, h, 0);
        if !report(b"ATOMIC_COMMIT", commit_rc == 0) { failures += 1; }

        let (hash_a, bytes_a, row_a) = fb0_census(w as usize, ATOMIC_ROW_R);
        print_dec(b"  ATOMIC fnv_test_only=", hash_t);
        print_dec(b"  ATOMIC fnv_after_commit=", hash_a);
        let px_ok = commit_rc == 0 && bytes_a > 0 && bytes_a == bytes_t && row_a;
        if !px_ok {
            puts(b"  ATOMIC_PRESENTS_PIXELS: FAIL the atomic commit returned but its pixels are not on the scanout\n\0".as_ptr());
        }
        if !report(b"ATOMIC_PRESENTS_PIXELS", px_ok) { failures += 1; }

        // ── CONSOLE_YIELDS_TO_ATOMIC ─────────────────────────────────────────
        // An atomic-only present must claim the console exactly as SETCRTC
        // does. Same provocation and same byte-identity verdict as
        // CONSOLE_YIELDS_TO_SCANOUT below, but reached without any legacy
        // KMS ioctl ever having been issued on this fd.
        provoke_console(fd);
        let (hash_b, bytes_b, _) = fb0_census(w as usize, ATOMIC_ROW_R);
        print_dec(b"  CONSOLE_YIELDS_TO_ATOMIC bytes_before=", bytes_a);
        print_dec(b"  CONSOLE_YIELDS_TO_ATOMIC bytes_after=", bytes_b);
        print_dec(b"  CONSOLE_YIELDS_TO_ATOMIC fnv_before=", hash_a);
        print_dec(b"  CONSOLE_YIELDS_TO_ATOMIC fnv_after=", hash_b);
        let held = px_ok && bytes_a == bytes_b && hash_a == hash_b;
        if !held {
            puts(b"  CONSOLE_YIELDS_TO_ATOMIC: FAIL the scanout changed after an ATOMIC-only present -- the console was never claimed from the atomic path, or an unrelated card0 close handed it back\n\0".as_ptr());
        }
        if !report(b"CONSOLE_YIELDS_TO_ATOMIC", held) { failures += 1; }
    } else {
        if !report(b"ATOMIC_TEST_ONLY_NO_PRESENT", false) { failures += 1; }
        if !report(b"ATOMIC_COMMIT", false) { failures += 1; }
        if !report(b"ATOMIC_PRESENTS_PIXELS", false) { failures += 1; }
        if !report(b"CONSOLE_YIELDS_TO_ATOMIC", false) { failures += 1; }
    }

    // SETCRTC — scan out the fb on crtc 1 with the connector's mode
    let mut set = core::mem::zeroed::<DrmModeCrtc>();
    set.crtc_id = 1;
    set.fb_id = fb.fb_id;
    let connectors = [connector_id];
    set.set_connectors_ptr = connectors.as_ptr() as u64;
    set.count_connectors = 1;
    set.mode.hdisplay = w as u16;
    set.mode.vdisplay = h as u16;
    set.mode.vrefresh = 60;
    set.mode_valid = 1;
    let setcrtc_ok = ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &mut set as *mut _) == 0;
    if !report(b"SETCRTC", setcrtc_ok) { failures += 1; }

    // ── CONSOLE_YIELDS_TO_SCANOUT ────────────────────────────────────────────
    //
    // The framebuffer console and the DRM scanout are one buffer, so a guest
    // program that merely prints while a master is scanning out used to destroy
    // the display: `scroll_vector` memmoves the entire surface up one text row
    // on every line, and a compositor only repaints what it damaged, so
    // whatever was static is scrolled away and never redrawn. Measured on a
    // COSMIC session as 334503 distinct colours collapsing to 177 with 79% of
    // the screen left black.
    //
    // This is the LEGACY-KMS half of the pair. Its counterpart above reaches
    // the same invariant through DRM_IOCTL_MODE_ATOMIC and nothing else; the
    // two together are what separate "the console was never silenced" from
    // "it was silenced and then handed back by an unrelated card0 close",
    // which additionally CLEARS the screen and prints a banner. A gate that
    // knows only about the legacy ioctl numbers passes here and fails there.
    //
    // The verdict is byte-identity of the scanout across the provocation, which
    // needs no per-arch pixel constants. Anything that repaints the surface
    // between the two reads — including the kernel's own log output — fails it.
    if setcrtc_ok {
        // GRADIENT_ROW_R, not ATOMIC_ROW_R: the surface currently holds the
        // atomic lane's pattern, so this row check only passes if SETCRTC's
        // own present replaced it. That keeps FB0_SHOWS_SCANOUT a real
        // plumbing self-check — it is what caught /dev/fb0 reading a stale
        // buffer on x86_64, where both builds otherwise returned identical
        // fingerprints and the census certified a vacuous pass.
        let (hash_a, bytes_a, row_ok) = fb0_census(w as usize, GRADIENT_ROW_R);
        if !report(b"FB0_SHOWS_SCANOUT", row_ok && bytes_a > 0) { failures += 1; }

        provoke_console(fd);

        let (hash_b, bytes_b, _) = fb0_census(w as usize, GRADIENT_ROW_R);
        print_dec(b"  CONSOLE_YIELDS bytes_before=", bytes_a);
        print_dec(b"  CONSOLE_YIELDS bytes_after=", bytes_b);
        print_dec(b"  CONSOLE_YIELDS fnv_before=", hash_a);
        print_dec(b"  CONSOLE_YIELDS fnv_after=", hash_b);
        let held = bytes_a > 0 && bytes_a == bytes_b && hash_a == hash_b;
        if !held {
            puts(b"  CONSOLE_YIELDS_TO_SCANOUT: FAIL the scanout changed while a DRM master held it -- the fb console painted or scrolled over it\n\0".as_ptr());
        }
        if !report(b"CONSOLE_YIELDS_TO_SCANOUT", held) { failures += 1; }
    } else {
        if !report(b"FB0_SHOWS_SCANOUT", false) { failures += 1; }
        if !report(b"CONSOLE_YIELDS_TO_SCANOUT", false) { failures += 1; }
    }

    if hold_mode {
        if setcrtc_ok && !fb_ptr.is_null() {
            paint_field_and_block(fb_ptr, cd.pitch as usize, w as usize, h as usize);
        }
        let mut hold_dirty = DrmModeFbDirtyCmd::default();
        hold_dirty.fb_id = fb.fb_id;
        ioctl(fd, DRM_IOCTL_MODE_DIRTYFB, &mut hold_dirty as *mut _);
        puts(b"DRMSMOKE: HOLD READY\n\0".as_ptr());
        // PAGE_FLIP, not DIRTYFB alone: only a present moves pixels into the
        // shared surface, and only a present claims the scanout back off the
        // framebuffer console. A DIRTYFB-only loop looks like a holding client
        // and behaves like a still image. No completion event is requested —
        // nothing reads this fd, and an unread event queue is not what is being
        // measured here. Every return code is ignored on purpose: once a VT
        // switch suspends this client's master these all answer EACCES, and
        // stopping (or complaining, onto the console we just handed back) is
        // the opposite of what a wedged compositor would do.
        let mut hold_flip = DrmModeCrtcPageFlip::default();
        hold_flip.crtc_id = 1;
        hold_flip.fb_id = fb.fb_id;
        loop {
            usleep(100_000);
            ioctl(fd, DRM_IOCTL_MODE_PAGE_FLIP, &mut hold_flip as *mut _);
            ioctl(fd, DRM_IOCTL_MODE_DIRTYFB, &mut hold_dirty as *mut _);
        }
    }

    // DIRTYFB — flush CPU render to host
    let mut dirty = DrmModeFbDirtyCmd::default();
    dirty.fb_id = fb.fb_id;
    let dirty_ok = ioctl(fd, DRM_IOCTL_MODE_DIRTYFB, &mut dirty as *mut _) == 0;
    if !report(b"DIRTYFB", dirty_ok) { failures += 1; }

    // PAGE_FLIP with a completion event, then poll + read the drm_event_vblank.
    // This exercises the K4 event channel (commit 3): the flip queues an event,
    // the ~vblank-throttled tick promotes it to readable, poll(POLLIN) fires,
    // and read() returns a 32-byte FLIP_COMPLETE with our user_data echoed.
    let magic: u64 = 0xF00D_BEEF_1234_5678;
    let mut flip = DrmModeCrtcPageFlip::default();
    flip.crtc_id = 1;
    flip.fb_id = fb.fb_id;
    flip.flags = DRM_MODE_PAGE_FLIP_EVENT;
    flip.user_data = magic;
    let flip_ok = ioctl(fd, DRM_IOCTL_MODE_PAGE_FLIP, &mut flip as *mut _) == 0;
    if !report(b"PAGE_FLIP_EVENT", flip_ok) { failures += 1; }

    // poll for readiness (throttled delivery is up to ~20 ms out; allow 500 ms).
    let mut pfd = pollfd { fd, events: POLLIN, revents: 0 };
    let poll_rc = poll(&mut pfd as *mut _, 1, 500);
    let poll_ok = poll_rc == 1 && (pfd.revents & POLLIN) != 0;
    if !report(b"POLL_CARD0_READABLE", poll_ok) { failures += 1; }

    // read the event back and validate it
    let mut ev = DrmEventVblank::default();
    let rn = read(fd, &mut ev as *mut _ as *mut c_void, core::mem::size_of::<DrmEventVblank>());
    let read_ok = rn == 32 && ev.ev_type == DRM_EVENT_FLIP_COMPLETE
        && ev.length == 32 && ev.user_data == magic;
    if !report(b"READ_FLIP_EVENT", read_ok) { failures += 1; }

    // FLIP_TS_SUBTICK — proves the flip-event timestamp is actually being
    // built from the interpolated arch_monotonic_ns() clock (queue_flip_event
    // in drivers/src/drm_device_interface.rs) and not the old coarse 100 Hz
    // tick. Reuses the PAGE_FLIP_EVENT -> POLL_CARD0_READABLE -> READ_FLIP_EVENT
    // machinery above, just driven several times in a row.
    //
    // The discriminator: under the OLD code, tv_usec = (ticks % 100) * 10_000,
    // so it could only ever land on one of 100 values — an EXACT multiple of
    // 10_000 — by construction. Under the NEW sub-tick code it should land on
    // arbitrary microsecond values instead.
    //
    // CLAMP ALPHABET (do not invert this logic either): arch_monotonic_ns()'s
    // sub-tick interpolation is clamped in arch/{x86_64,aarch64}/src/timer.rs
    // — `break base + frac.min(9_999_999);` — so a stamp taken while the
    // fractional part has already overrun the tick (timer IRQ running late)
    // saturates at exactly base_ns + 9_999_999, i.e. tv_usec = (t % 100) *
    // 10_000 + 9_999. Every value in that clamped alphabet {9999, 19999,
    // 29999, ..., 999999} ends in "9999". Measured 10/48 samples saturated on
    // x86_64/TCG (20.8%), 0/40 on aarch64/HVF, and one run of six was 8/8
    // saturated on identical clamped constants.
    //
    // A saturated sample is NOT "no signal" — it is positive evidence, and
    // this is the one place it is easy to get backwards. The OLD code computed
    // tv_usec = (ticks % 100) * 10_000, which is ALWAYS congruent to 0 mod
    // 10_000, so it could never produce a value ending in 9999. Reaching the
    // clamp at all means base + frac.min(9_999_999) executed, i.e. the
    // interpolated path ran. Saturation says "the timer IRQ was late", not
    // "the timestamp math regressed" — those are different claims.
    //
    // Every sample is classified into exactly one of three buckets:
    //   - tick-multiple:   tv_usec % 10_000 == 0      (old coarse clock)
    //   - saturated:       tv_usec % 10_000 == 9_999  (clamp engaged, IRQ late)
    //   - genuine sub-tick: anything else
    // PASS requires at least one NON-tick-multiple sample, so both genuine and
    // saturated count. FAIL therefore means every sample was a tick-multiple —
    // which is exactly, and only, the old clock's signature. Requiring a
    // *genuine* sample instead would be flaky rather than strict: a measured
    // x86_64/TCG run came in at 15 saturated / 1 genuine, one sample away from
    // a spurious failure that would have indicated nothing about the math.
    //
    // PHASE-ALIGNMENT / FLAKE FIX: the 8/8-saturated run above happened
    // because consecutive flips were spaced exactly one tick apart, so every
    // ioctl landed at the same far edge of its tick, sample after sample.
    // usleep()/nanosleep() cannot break that: sys_nanosleep rounds ANY
    // nonzero request UP to a whole number of ticks (ticks_needed =
    // total_ns.div_ceil(10_000_000) in kernel/src/syscall.rs), so a sleep
    // between flips just resyncs us to the next tick boundary — reinforcing
    // the phase-lock, not breaking it. Instead, spin_delay() below is a pure
    // CPU busy-loop (no syscall) whose iteration count is stepped by a large,
    // non-round increment every sample, so consecutive flips are issued at
    // different real-time offsets within their tick and cannot all pin to
    // the same edge. The sample count is also raised from 8 to 16 so more
    // independent phases are swept.
    const FLIP_TS_SAMPLES: usize = 16;
    const FLIP_TS_SPIN_BASE: u64 = 5_000;
    const FLIP_TS_SPIN_STEP: u64 = 47_777; // deliberately not a round number
    let mut subtick_all_read_ok = true;
    let mut n_tick_multiple = 0u32;
    let mut n_saturated = 0u32;
    let mut n_genuine = 0u32;
    for i in 0..FLIP_TS_SAMPLES {
        spin_delay(FLIP_TS_SPIN_BASE + (i as u64) * FLIP_TS_SPIN_STEP);

        let mut sflip = DrmModeCrtcPageFlip::default();
        sflip.crtc_id = 1;
        sflip.fb_id = fb.fb_id;
        sflip.flags = DRM_MODE_PAGE_FLIP_EVENT;
        sflip.user_data = magic.wrapping_add(i as u64 + 1);
        let sflip_ok = ioctl(fd, DRM_IOCTL_MODE_PAGE_FLIP, &mut sflip as *mut _) == 0;

        let mut spfd = pollfd { fd, events: POLLIN, revents: 0 };
        let spoll_rc = poll(&mut spfd as *mut _, 1, 500);
        let spoll_ok = spoll_rc == 1 && (spfd.revents & POLLIN) != 0;

        let mut sev = DrmEventVblank::default();
        let srn = read(fd, &mut sev as *mut _ as *mut c_void, core::mem::size_of::<DrmEventVblank>());
        let sread_ok = srn == 32 && sev.ev_type == DRM_EVENT_FLIP_COMPLETE
            && sev.length == 32 && sev.user_data == sflip.user_data;

        if !sflip_ok || !spoll_ok || !sread_ok {
            subtick_all_read_ok = false;
            continue;
        }

        print_dec(b"  FLIP_TS_SUBTICK tv_sec=", sev.tv_sec as u64);
        print_dec(b"  FLIP_TS_SUBTICK tv_usec=", sev.tv_usec as u64);
        let rem = sev.tv_usec % 10_000;
        if rem == 0 {
            n_tick_multiple += 1;
            puts(b"  FLIP_TS_SUBTICK bucket=tick-multiple\n\0".as_ptr());
        } else if rem == 9_999 {
            n_saturated += 1;
            puts(b"  FLIP_TS_SUBTICK bucket=saturated\n\0".as_ptr());
        } else {
            n_genuine += 1;
            puts(b"  FLIP_TS_SUBTICK bucket=genuine-sub-tick\n\0".as_ptr());
        }
    }
    print_dec(b"  FLIP_TS_SUBTICK n_tick_multiple=", n_tick_multiple as u64);
    print_dec(b"  FLIP_TS_SUBTICK n_saturated=", n_saturated as u64);
    print_dec(b"  FLIP_TS_SUBTICK n_genuine=", n_genuine as u64);
    // Both genuine and saturated samples prove the interpolated path ran; only
    // an all-tick-multiple result is the old clock's signature.
    let subtick_ok = subtick_all_read_ok && (n_genuine + n_saturated) > 0;
    if !subtick_ok && subtick_all_read_ok {
        puts(b"  FLIP_TS_SUBTICK: FAIL every sample was an exact multiple of 10_000 -- timestamp math regressed to the coarse 100 Hz clock\n\0".as_ptr());
    }
    // Not a failure, but worth saying out loud: an all-saturated run still
    // passes (the clamp cannot be reached from the coarse clock), yet it means
    // the timer IRQ was late on every sample, which is worth knowing on its own.
    if subtick_all_read_ok && n_genuine == 0 && n_saturated > 0 {
        puts(b"  FLIP_TS_SUBTICK: note - every sample hit the clamp; interpolation is live but the timer IRQ ran late throughout\n\0".as_ptr());
    }
    if !report(b"FLIP_TS_SUBTICK", subtick_ok) { failures += 1; }

    // ── Sync objects (DRM_IOCTL_SYNCOBJ_*) ──────────────────────────────────
    //
    // Binary syncobjs, which v3d's `drm_v3d_submit_cl` needs unconditionally
    // (`in_sync_bcl` / `in_sync_rcl` / `out_sync` are syncobj handles and there
    // is no simulate path). Every check below is errno-exact, because the errno
    // is what Mesa branches on: ETIME means "not yet, ask again", ENOENT means
    // "your handle is gone", EINVAL means "you asked for something impossible".
    //
    // The two checks that are not merely surface coverage:
    //   * WAIT_ALL_BLOCKS_ETIME measures wall time across a wait that must
    //     time out, so a WAIT that returned instantly (a broken deadline
    //     conversion — the `fb398c7` nanosleep-truncation shape) fails here
    //     rather than passing quietly.
    //   * WAIT_WOKEN_BY_FORKED_SIGNAL parks the parent with no deadline
    //     pressure and has a forked child signal the syncobj on the INHERITED
    //     fd. That proves the park/wake path, which a poll-only test cannot:
    //     a WAIT that busy-spun, or one that slept and was never woken, both
    //     fail it. It also proves syncobj handles follow the open-file
    //     description across fork, which is what "per-open, not per-process"
    //     has to mean.
    let mut cap_so = DrmGetCap { capability: DRM_CAP_SYNCOBJ, value: 0 };
    let cap_so_ok = ioctl(fd, DRM_IOCTL_GET_CAP, &mut cap_so as *mut _) == 0 && cap_so.value == 1;
    if !report(b"GET_CAP_SYNCOBJ", cap_so_ok) { failures += 1; }

    // Deliberately 0: timeline syncobjs are ENOSYS, and this is the flag Mesa
    // reads to fall back to emulating them on binary syncobjs. Reporting 1
    // here would be the actual bug.
    let mut cap_tl = DrmGetCap { capability: DRM_CAP_SYNCOBJ_TIMELINE, value: 0 };
    let cap_tl_ok = ioctl(fd, DRM_IOCTL_GET_CAP, &mut cap_tl as *mut _) == 0 && cap_tl.value == 0;
    if !report(b"GET_CAP_SYNCOBJ_TIMELINE_IS_ZERO", cap_tl_ok) { failures += 1; }

    let mut sc_a = DrmSyncobjCreate::default();
    let mut sc_b = DrmSyncobjCreate::default();
    let mut sc_c = DrmSyncobjCreate { handle: 0, flags: DRM_SYNCOBJ_CREATE_SIGNALED };
    let create_so_ok = ioctl(fd, DRM_IOCTL_SYNCOBJ_CREATE, &mut sc_a as *mut _) == 0
        && ioctl(fd, DRM_IOCTL_SYNCOBJ_CREATE, &mut sc_b as *mut _) == 0
        && ioctl(fd, DRM_IOCTL_SYNCOBJ_CREATE, &mut sc_c as *mut _) == 0
        && sc_a.handle != 0 && sc_b.handle != 0 && sc_c.handle != 0
        && sc_a.handle != sc_b.handle && sc_b.handle != sc_c.handle;
    if !report(b"SYNCOBJ_CREATE", create_so_ok) { failures += 1; }

    let ha = sc_a.handle;
    let hb = sc_b.handle;
    let hc = sc_c.handle;

    // Helper-free inline WAIT: a fresh syncobj holds the NULL fence, so waiting
    // on it WITHOUT WAIT_FOR_SUBMIT is EINVAL — upstream refuses rather than
    // waiting forever on a container nothing has submitted into.
    let handles_a: [u32; 1] = [ha];
    let mut w = DrmSyncobjWait {
        handles: handles_a.as_ptr() as u64,
        timeout_nsec: 0,
        count_handles: 1,
        flags: 0,
        first_signaled: 0,
        pad: 0,
    };
    let null_fence_einval = ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &mut w as *mut _) == -1
        && errno() == EINVAL;
    if !report(b"SYNCOBJ_WAIT_NULL_FENCE_EINVAL", null_fence_einval) { failures += 1; }

    // With WAIT_FOR_SUBMIT and a zero timeout it is a pure poll, so it must
    // come back ETIME immediately rather than block.
    w.flags = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT;
    w.timeout_nsec = 0;
    let poll_etime = ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &mut w as *mut _) == -1
        && errno() == ETIME;
    if !report(b"SYNCOBJ_WAIT_ZERO_TIMEOUT_ETIME", poll_etime) { failures += 1; }

    // CREATE_SIGNALED really did create a signalled one.
    let handles_c: [u32; 1] = [hc];
    let mut wc = DrmSyncobjWait {
        handles: handles_c.as_ptr() as u64,
        timeout_nsec: 0,
        count_handles: 1,
        flags: 0,
        first_signaled: 0xFFFF_FFFF,
        pad: 0,
    };
    let created_signaled = ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &mut wc as *mut _) == 0
        && wc.first_signaled == 0;
    if !report(b"SYNCOBJ_CREATE_SIGNALED", created_signaled) { failures += 1; }

    // SIGNAL then poll: succeeds, and first_signaled is the ARRAY INDEX of the
    // first signalled handle, not the handle value. [hb, ha] with only ha
    // signalled must report 1.
    let sig_arr: [u32; 1] = [ha];
    let mut sa = DrmSyncobjArray {
        handles: sig_arr.as_ptr() as u64,
        count_handles: 1,
        pad: 0,
    };
    let signal_ok = ioctl(fd, DRM_IOCTL_SYNCOBJ_SIGNAL, &mut sa as *mut _) == 0;
    if !report(b"SYNCOBJ_SIGNAL", signal_ok) { failures += 1; }

    let handles_ba: [u32; 2] = [hb, ha];
    let mut w2 = DrmSyncobjWait {
        handles: handles_ba.as_ptr() as u64,
        timeout_nsec: 0,
        count_handles: 2,
        // hb still holds the NULL fence, so WAIT_FOR_SUBMIT is required for the
        // call to be legal at all; ANY (no WAIT_ALL) is satisfied by ha.
        flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,
        first_signaled: 0xFFFF_FFFF,
        pad: 0,
    };
    let first_idx_ok = ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &mut w2 as *mut _) == 0
        && w2.first_signaled == 1;
    if !report(b"SYNCOBJ_WAIT_ANY_FIRST_SIGNALED", first_idx_ok) { failures += 1; }

    // WAIT_ALL over [hb, ha] cannot be satisfied (hb is unsignalled), so it
    // must sleep to its deadline and then answer ETIME. ~150 ms of wall clock
    // is the evidence that it really parked: a driver that returned instantly,
    // or one whose ns->tick conversion truncated to zero, comes back in ~0 ms
    // and fails here even though its errno is right.
    let t0 = monotonic_ns();
    let mut w3 = DrmSyncobjWait {
        handles: handles_ba.as_ptr() as u64,
        timeout_nsec: (t0 + 150_000_000) as i64, // ABSOLUTE, +150 ms
        count_handles: 2,
        flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT | DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL,
        first_signaled: 0,
        pad: 0,
    };
    let all_rc = ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &mut w3 as *mut _);
    let all_errno = errno();
    let elapsed_ms = (monotonic_ns().saturating_sub(t0)) / 1_000_000;
    print_dec(b"  SYNCOBJ_WAIT_ALL elapsed_ms=", elapsed_ms);
    // Lower bound only. An upper bound would be a flake generator under TCG.
    let wait_all_ok = all_rc == -1 && all_errno == ETIME && elapsed_ms >= 100;
    if !report(b"SYNCOBJ_WAIT_ALL_BLOCKS_ETIME", wait_all_ok) { failures += 1; }

    // RESET installs the NULL fence again, so ha goes back to being illegal to
    // wait on without WAIT_FOR_SUBMIT.
    let mut ra = DrmSyncobjArray {
        handles: sig_arr.as_ptr() as u64,
        count_handles: 1,
        pad: 0,
    };
    let reset_rc = ioctl(fd, DRM_IOCTL_SYNCOBJ_RESET, &mut ra as *mut _);
    let mut w4 = DrmSyncobjWait {
        handles: handles_a.as_ptr() as u64,
        timeout_nsec: 0,
        count_handles: 1,
        flags: 0,
        first_signaled: 0,
        pad: 0,
    };
    let reset_ok = reset_rc == 0
        && ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &mut w4 as *mut _) == -1
        && errno() == EINVAL;
    if !report(b"SYNCOBJ_RESET", reset_ok) { failures += 1; }

    // A handle that was never allocated is ENOENT from WAIT (upstream's
    // drm_syncobj_array_find), and EINVAL from DESTROY (upstream's failed
    // idr_remove). The two differ on purpose; getting them backwards is
    // exactly the kind of drift this check exists to catch.
    let bogus: [u32; 1] = [0xDEAD_BEEF];
    let mut w5 = DrmSyncobjWait {
        handles: bogus.as_ptr() as u64,
        timeout_nsec: 0,
        count_handles: 1,
        flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,
        first_signaled: 0,
        pad: 0,
    };
    let enoent_ok = ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &mut w5 as *mut _) == -1
        && errno() == ENOENT;
    if !report(b"SYNCOBJ_WAIT_BAD_HANDLE_ENOENT", enoent_ok) { failures += 1; }

    // Timeline family: ENOSYS, explicitly, not a generic failure.
    let tl_points: [u64; 1] = [1];
    let mut tw = DrmSyncobjTimelineWait {
        handles: handles_a.as_ptr() as u64,
        points: tl_points.as_ptr() as u64,
        timeout_nsec: 0,
        count_handles: 1,
        flags: 0,
        first_signaled: 0,
        pad: 0,
    };
    let tl_enosys = ioctl(fd, DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &mut tw as *mut _) == -1
        && errno() == ENOSYS;
    if !report(b"SYNCOBJ_TIMELINE_WAIT_ENOSYS", tl_enosys) { failures += 1; }

    // ── The blocking wake, proven ───────────────────────────────────────────
    // Parent waits on hb with a 5 s deadline it must NOT reach; the child
    // signals hb ~200 ms in, over the fd it inherited. Success means the wait
    // returned 0 well inside the deadline, which is only possible if the
    // parent actually parked and something actually woke it.
    let wake_ok;
    {
        let handles_b: [u32; 1] = [hb];
        let child = fork();
        if child == 0 {
            usleep(200_000);
            let sig_b: [u32; 1] = [hb];
            let mut sb = DrmSyncobjArray {
                handles: sig_b.as_ptr() as u64,
                count_handles: 1,
                pad: 0,
            };
            ioctl(fd, DRM_IOCTL_SYNCOBJ_SIGNAL, &mut sb as *mut _);
            _exit(0);
        } else if child < 0 {
            wake_ok = false;
        } else {
            let s0 = monotonic_ns();
            let mut w6 = DrmSyncobjWait {
                handles: handles_b.as_ptr() as u64,
                timeout_nsec: (s0 + 5_000_000_000) as i64,
                count_handles: 1,
                flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,
                first_signaled: 0xFFFF_FFFF,
                pad: 0,
            };
            let rc = ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &mut w6 as *mut _);
            let waited_ms = (monotonic_ns().saturating_sub(s0)) / 1_000_000;
            let mut st: c_int = 0;
            waitpid(child, &mut st as *mut c_int, 0);
            print_dec(b"  SYNCOBJ_WAKE waited_ms=", waited_ms);
            wake_ok = rc == 0 && w6.first_signaled == 0 && waited_ms < 4_000;
        }
    }
    if !report(b"SYNCOBJ_WAIT_WOKEN_BY_FORKED_SIGNAL", wake_ok) { failures += 1; }

    // DESTROY retires the handle; destroying it twice is EINVAL.
    let mut d_a = DrmSyncobjDestroy { handle: ha, pad: 0 };
    let mut d_b = DrmSyncobjDestroy { handle: hb, pad: 0 };
    let mut d_c = DrmSyncobjDestroy { handle: hc, pad: 0 };
    let mut d_again = DrmSyncobjDestroy { handle: ha, pad: 0 };
    let destroy_ok = ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &mut d_a as *mut _) == 0
        && ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &mut d_b as *mut _) == 0
        && ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &mut d_c as *mut _) == 0
        && ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &mut d_again as *mut _) == -1
        && errno() == EINVAL;
    if !report(b"SYNCOBJ_DESTROY", destroy_ok) { failures += 1; }

    // ── V3D UAPI decode layer (stub backend) ────────────────────────────────
    //
    // QEMU models no V3D on any machine type, so none of this can be exercised
    // against a device. What CAN be exercised — and is, here — is everything
    // between Mesa and the device: the request codes, the struct layouts, the
    // parameter values Mesa gates screen creation on, the BO/VA bookkeeping,
    // and the *shape* of an asynchronous submission. That is roughly half the
    // ioctl surface, closed before a Pi is on the desk.
    //
    // THE BACKEND IS ARMED HERE AND DISARMED AT THE END OF THE BLOCK, on
    // purpose. It is device-global (it has to be: it changes the
    // DRM_IOCTL_VERSION identity, which Mesa reads before it has created
    // anything), and two of the v3d request codes are bit-identical to virtgpu
    // ones. Leaving it armed would change what every other client on this
    // machine sees. Every check above this point therefore ran against the
    // unarmed device, and every check below it does too.
    {
        let mut ccap = DrmSetClientCap {
            capability: DRM_CLIENT_CAP_LEANDROS_V3D,
            value: 1,
        };
        let arm_ok = ioctl(fd, DRM_IOCTL_SET_CLIENT_CAP, &mut ccap as *mut _) == 0;
        if !report(b"V3D_ARM", arm_ok) { failures += 1; }

        // ── The Mesa loader handshake ───────────────────────────────────────
        // `pipe_loader_drm.c:276` reads this name and `:98` plain-`strcmp`s it
        // against every driver descriptor. Not "starts with", not "contains" —
        // an unrecognised name silently selects the software backend, which is
        // the exact failure the virgl lane already paid for with `leandros-drm`.
        // So the test is byte-exact, including the length.
        //
        // `name_len == 3`, the STRLEN, is half the assertion and not a detail.
        // libdrm's `drmGetVersion` asks once with null pointers, allocates
        // `name_len + 1`, and passes `name_len` back UNCHANGED on the second
        // call — so a driver that reports a length including its own NUL and
        // then guards the copy on that same inflated length happens to work,
        // while one that mixes the two conventions silently copies nothing.
        // Checking the reported length here is what pins the convention: this
        // check failing with the right bytes in the buffer is exactly the bug.
        //
        // The two passes are done separately, as libdrm does them, so a driver
        // that only works when told a generous capacity up front fails here.
        let mut probe = DrmVersion::default();
        let probe_ok = ioctl(fd, DRM_IOCTL_VERSION, &mut probe as *mut _) == 0
            && probe.name_len == 3;
        let mut vnamebuf = [0u8; 8];
        let mut vver = DrmVersion::default();
        vver.name_len = probe.name_len;
        vver.name = vnamebuf.as_mut_ptr() as u64;
        let name_ok = probe_ok
            && ioctl(fd, DRM_IOCTL_VERSION, &mut vver as *mut _) == 0
            && vver.name_len == 3
            && vnamebuf[0] == b'v' && vnamebuf[1] == b'3' && vnamebuf[2] == b'd'
            && vver.version_major == 1;
        if !report(b"V3D_VERSION_NAME_IS_EXACTLY_V3D", name_ok) { failures += 1; }

        // ── GET_PARAM: the gate on screen creation ──────────────────────────
        // `v3d_device_info.c:32` hard-fails if CORE0_IDENT0 or IDENT1 errors,
        // and `v3d_screen.c:796` turns that into a NULL screen. The decode
        // reproduced here is Mesa's, character for character, so a value that
        // would make Mesa reject the device fails HERE instead of inside a
        // driver with no error message.
        let mut p0 = DrmV3dGetParam { param: V3D_PARAM_CORE0_IDENT0, pad: 0, value: 0 };
        let mut p1 = DrmV3dGetParam { param: V3D_PARAM_CORE0_IDENT1, pad: 0, value: 0 };
        let mut p3 = DrmV3dGetParam { param: V3D_PARAM_HUB_IDENT3, pad: 0, value: 0 };
        let ident_read = ioctl(fd, DRM_IOCTL_V3D_GET_PARAM, &mut p0 as *mut _) == 0
            && ioctl(fd, DRM_IOCTL_V3D_GET_PARAM, &mut p1 as *mut _) == 0
            && ioctl(fd, DRM_IOCTL_V3D_GET_PARAM, &mut p3 as *mut _) == 0;
        let major = ((p0.value >> 24) & 0xff) as u32;
        let minor = (p1.value & 0xf) as u32;
        let ver = major * 10 + minor;
        let vpm_size = (((p1.value >> 28) & 0xf) as u32) * 8192;
        let nslc = ((p1.value >> 4) & 0xf) as u32;
        let qups = ((p1.value >> 8) & 0xf) as u32;
        let qpu_count = nslc * qups;
        let rev = ((p3.value >> 8) & 0xff) as u32;
        print_dec(b"  V3D ver=", ver as u64);
        print_dec(b"  V3D vpm_size=", vpm_size as u64);
        print_dec(b"  V3D qpu_count=", qpu_count as u64);
        print_dec(b"  V3D rev=", rev as u64);
        // ver 71 exactly: Mesa compiles support for 42 and 71 and prints
        // "V3D %d.%d not supported by this version of Mesa" for anything else.
        // vpm_size and qpu_count feed real arithmetic (a division in
        // `vir.c:2456`, a spill-BO size in `v3d_program.c:542`), so zero in
        // either is a divide-by-zero or a zero-sized allocation later.
        let ident_ok = ident_read && ver == 71 && vpm_size > 0 && qpu_count > 0;
        if !report(b"V3D_GET_PARAM_IDENT_IS_COHERENT_V3D_71", ident_ok) { failures += 1; }

        // Feature bits, each of which selects a Mesa code path.
        let mut feat = [0u64; 5];
        let feat_ids = [
            V3D_PARAM_SUPPORTS_TFU,
            V3D_PARAM_SUPPORTS_CSD,
            V3D_PARAM_SUPPORTS_PERFMON,
            V3D_PARAM_SUPPORTS_MULTISYNC_EXT,
            V3D_PARAM_MAX_PERF_COUNTERS,
        ];
        let mut feat_read = true;
        let mut fi = 0usize;
        while fi < feat_ids.len() {
            let mut pf = DrmV3dGetParam { param: feat_ids[fi], pad: 0, value: 0 };
            if ioctl(fd, DRM_IOCTL_V3D_GET_PARAM, &mut pf as *mut _) != 0 { feat_read = false; }
            feat[fi] = pf.value;
            fi += 1;
        }
        // MAX_PERF_COUNTERS == 0 is the load-bearing one: non-zero makes
        // `v3dx_counter.c:41` issue PERFMON_GET_COUNTER, which is ENOSYS, and
        // `v3d_perfcntrs_init` failing is a hard `goto fail` in screen creation.
        // MULTISYNC == 0 keeps submits on the flat sync fields instead of an
        // extension chain.
        let feat_ok = feat_read
            && feat[0] == 1  // TFU
            && feat[1] == 0  // CSD
            && feat[2] == 0  // PERFMON
            && feat[3] == 0  // MULTISYNC_EXT
            && feat[4] == 0; // MAX_PERF_COUNTERS
        if !report(b"V3D_GET_PARAM_FEATURE_BITS", feat_ok) { failures += 1; }

        // An id we have never heard of is EINVAL, not a successful zero:
        // `v3d_has_feature` reads the RETURN CODE, so answering 0 successfully
        // would claim we understood the question.
        let mut pbad = DrmV3dGetParam { param: 0xDEAD, pad: 0, value: 0 };
        let param_einval = ioctl(fd, DRM_IOCTL_V3D_GET_PARAM, &mut pbad as *mut _) == -1
            && errno() == EINVAL;
        if !report(b"V3D_GET_PARAM_UNKNOWN_EINVAL", param_einval) { failures += 1; }

        // ── CREATE_BO with the size Mesa actually asks for ──────────────────
        // `v3d_resource.c:113-116` pads EVERY resource: +64 (V3D_TFU_READAHEAD_SIZE)
        // for a texture, +4 for a PIPE_BUFFER, so the TFU's and `ldunifa`'s
        // read-ahead cannot run off the last page. A page-sized texture
        // therefore arrives as 4160 bytes and MUST become a two-page
        // allocation. This is the common case, not an edge case.
        const BO_REQ: u32 = 4096 + 64;
        let mut cbo = DrmV3dCreateBo { size: BO_REQ, flags: 0, handle: 0, offset: 0 };
        let create_ok = ioctl(fd, DRM_IOCTL_V3D_CREATE_BO, &mut cbo as *mut _) == 0
            && cbo.handle != 0
            // "This offset value will always be nonzero, since various HW units
            // treat 0 specially" — the UAPI header's own promise, which is why
            // the VA allocator leaves page 0 unallocated.
            && cbo.offset != 0
            // The V3D MMU's page size. A BO that did not start on one could not
            // be mapped independently.
            && cbo.offset % 4096 == 0;
        if !report(b"V3D_CREATE_BO_UNROUNDED_SIZE", create_ok) { failures += 1; }

        let mut cbo_bad = DrmV3dCreateBo { size: 4096, flags: 1, handle: 0, offset: 0 };
        let flags_einval = ioctl(fd, DRM_IOCTL_V3D_CREATE_BO, &mut cbo_bad as *mut _) == -1
            && errno() == EINVAL;
        if !report(b"V3D_CREATE_BO_FLAGS_EINVAL", flags_einval) { failures += 1; }

        // GET_BO_OFFSET must answer the SAME address CREATE_BO did, for the
        // life of the handle — Mesa's BO cache re-reads it rather than
        // remembering it, and a moving address would be baked into an already
        // built command list.
        let mut gbo = DrmV3dGetBoOffset { handle: cbo.handle, offset: 0 };
        let offset_ok = create_ok
            && ioctl(fd, DRM_IOCTL_V3D_GET_BO_OFFSET, &mut gbo as *mut _) == 0
            && gbo.offset == cbo.offset;
        if !report(b"V3D_GET_BO_OFFSET_MATCHES_CREATE", offset_ok) { failures += 1; }

        // ── MMAP_BO round-trip ──────────────────────────────────────────────
        // The returned offset is this driver's mmap token (a guest-physical
        // base), validated on the way back in so a caller cannot map memory the
        // device never handed out. The sentinel is written at the LAST byte of
        // the SECOND page, which is the part a one-page allocation would not
        // have: this is what actually proves the non-round size was rounded UP.
        let mut mbo = DrmV3dMmapBo { handle: cbo.handle, flags: 0, offset: 0 };
        let mut map_ok = false;
        if create_ok && ioctl(fd, DRM_IOCTL_V3D_MMAP_BO, &mut mbo as *mut _) == 0 && mbo.offset != 0 {
            let span = 8192usize; // two pages: 4160 bytes rounds up to two
            let p = mmap(core::ptr::null_mut(), span, PROT_READ | PROT_WRITE,
                         MAP_SHARED, fd, mbo.offset as i64);
            if p as isize > 0 {
                let b = p as *mut u8;
                // Zeroed at creation, so this also checks we do not hand a
                // client whatever the buddy allocator last had in these pages.
                let was_zero = *b.add(0) == 0 && *b.add(span - 1) == 0;
                *b.add(0) = 0x5A;
                *b.add(span - 1) = 0xA5;
                map_ok = was_zero && *b.add(0) == 0x5A && *b.add(span - 1) == 0xA5;
            }
        }
        if !report(b"V3D_MMAP_BO_ROUNDTRIP", map_ok) { failures += 1; }

        let mut mbo_bad = DrmV3dMmapBo { handle: 0xDEAD_BEEF, flags: 0, offset: 0 };
        let mmap_enoent = ioctl(fd, DRM_IOCTL_V3D_MMAP_BO, &mut mbo_bad as *mut _) == -1
            && errno() == ENOENT;
        if !report(b"V3D_MMAP_BO_BAD_HANDLE_ENOENT", mmap_enoent) { failures += 1; }

        // ── SUBMIT_CL: the fence must NOT be retired when submit returns ─────
        //
        // THE POINT OF THE WHOLE STUB. `TODO.md:3251` records virtgpu signalling
        // its out-fence at *creation*, which is only correct there because its
        // submit is a synchronous busy-spin. A stub that copied that would
        // expose a fence that is never once observably outstanding, and every
        // consumer built against it would be tested against a shape real
        // hardware does not have. So: submit, then IMMEDIATELY poll the
        // out-sync with a zero timeout, and require **ETIME** — the fence is
        // still in flight. Then wait properly and require it to complete.
        //
        // A submit that retired instantly passes every other check in this file
        // and fails exactly this one.
        let mut so = DrmSyncobjCreate::default();
        let so_ok = ioctl(fd, DRM_IOCTL_SYNCOBJ_CREATE, &mut so as *mut _) == 0 && so.handle != 0;

        let bo_list: [u32; 1] = [cbo.handle];
        let mut sub = DrmV3dSubmitCl::default();
        sub.bcl_start = cbo.offset;
        sub.bcl_end = cbo.offset + 64;
        sub.rcl_start = cbo.offset + 64;
        sub.rcl_end = cbo.offset + 128;
        sub.out_sync = so.handle;
        sub.bo_handles = bo_list.as_ptr() as u64;
        sub.bo_handle_count = 1;
        let submit_ok = so_ok && create_ok
            && ioctl(fd, DRM_IOCTL_V3D_SUBMIT_CL, &mut sub as *mut _) == 0;
        if !report(b"V3D_SUBMIT_CL", submit_ok) { failures += 1; }

        let so_handles: [u32; 1] = [so.handle];
        let mut poll_w = DrmSyncobjWait {
            handles: so_handles.as_ptr() as u64,
            timeout_nsec: 0,
            count_handles: 1,
            flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,
            first_signaled: 0,
            pad: 0,
        };
        let outstanding = submit_ok
            && ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &mut poll_w as *mut _) == -1
            && errno() == ETIME;
        if !report(b"V3D_SUBMIT_CL_FENCE_NOT_RETIRED_AT_SUBMIT", outstanding) { failures += 1; }

        // ...and it does retire, without anyone poking it — the deferral is a
        // real completion path, not a value nothing ever changes. A generous
        // 2 s deadline; the stub retires within two 100 Hz ticks.
        let s0 = monotonic_ns();
        let mut done_w = DrmSyncobjWait {
            handles: so_handles.as_ptr() as u64,
            timeout_nsec: (s0 + 2_000_000_000) as i64,
            count_handles: 1,
            flags: DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT,
            first_signaled: 0xFFFF_FFFF,
            pad: 0,
        };
        let retired = submit_ok
            && ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &mut done_w as *mut _) == 0
            && done_w.first_signaled == 0;
        print_dec(b"  V3D fence_retire_ms=", monotonic_ns().saturating_sub(s0) / 1_000_000);
        if !report(b"V3D_SUBMIT_CL_FENCE_RETIRES_ASYNCHRONOUSLY", retired) { failures += 1; }

        // A handle that names no BO fails the WHOLE submit, before anything is
        // fenced — upstream's `v3d_lookup_bos` behaviour, and the reason the
        // list is validated in full before a fence is allocated.
        let bad_list: [u32; 2] = [cbo.handle, 0xDEAD_BEEF];
        let mut sub_bad = sub;
        sub_bad.out_sync = 0;
        sub_bad.bo_handles = bad_list.as_ptr() as u64;
        sub_bad.bo_handle_count = 2;
        let bad_bo_enoent = ioctl(fd, DRM_IOCTL_V3D_SUBMIT_CL, &mut sub_bad as *mut _) == -1
            && errno() == ENOENT;
        if !report(b"V3D_SUBMIT_CL_BAD_BO_ENOENT", bad_bo_enoent) { failures += 1; }

        // We advertise SUPPORTS_MULTISYNC_EXT = 0, so an extension chain is
        // refused rather than silently ignored — ignoring it would drop the
        // caller's wait/signal dependencies and show up days later as a race.
        let mut sub_ext = sub;
        sub_ext.out_sync = 0;
        sub_ext.bo_handle_count = 0;
        sub_ext.bo_handles = 0;
        sub_ext.flags = 0x02; // DRM_V3D_SUBMIT_EXTENSION
        let ext_einval = ioctl(fd, DRM_IOCTL_V3D_SUBMIT_CL, &mut sub_ext as *mut _) == -1
            && errno() == EINVAL;
        if !report(b"V3D_SUBMIT_CL_EXTENSION_EINVAL", ext_einval) { failures += 1; }

        // ── WAIT_BO ─────────────────────────────────────────────────────────
        // Same two-phase property as the syncobj checks, on the per-BO fence
        // instead: a fresh submit leaves the BO busy, and `timeout_ns` here is a
        // RELATIVE duration (upstream v3d runs it through
        // `nsecs_to_jiffies_timeout`), unlike drm_syncobj_wait's absolute one.
        // Getting that backwards is a wait that returns instantly forever.
        let mut sub2 = sub;
        sub2.out_sync = 0;
        let submit2_ok = create_ok
            && ioctl(fd, DRM_IOCTL_V3D_SUBMIT_CL, &mut sub2 as *mut _) == 0;
        let mut wb0 = DrmV3dWaitBo { handle: cbo.handle, pad: 0, timeout_ns: 0 };
        let bo_busy = submit2_ok
            && ioctl(fd, DRM_IOCTL_V3D_WAIT_BO, &mut wb0 as *mut _) == -1
            && errno() == ETIME;
        if !report(b"V3D_WAIT_BO_ZERO_TIMEOUT_ETIME_WHILE_BUSY", bo_busy) { failures += 1; }

        let mut wb1 = DrmV3dWaitBo { handle: cbo.handle, pad: 0, timeout_ns: 2_000_000_000 };
        let bo_wait_ok = submit2_ok
            && ioctl(fd, DRM_IOCTL_V3D_WAIT_BO, &mut wb1 as *mut _) == 0
            // Upstream decrements the caller's timeout by the elapsed time on
            // the way out, so a restarted wait waits only the remainder. It
            // therefore cannot come back untouched.
            && wb1.timeout_ns < 2_000_000_000;
        if !report(b"V3D_WAIT_BO_BLOCKS_THEN_COMPLETES", bo_wait_ok) { failures += 1; }

        // Upstream's `drm_gem_dma_resv_wait` answers EINVAL for a handle that
        // does not resolve — NOT ENOENT, which is what SUBMIT_CL answers for
        // the same mistake. The two differ on purpose.
        let mut wbb = DrmV3dWaitBo { handle: 0xDEAD_BEEF, pad: 0, timeout_ns: 0 };
        let wait_einval = ioctl(fd, DRM_IOCTL_V3D_WAIT_BO, &mut wbb as *mut _) == -1
            && errno() == EINVAL;
        if !report(b"V3D_WAIT_BO_BAD_HANDLE_EINVAL", wait_einval) { failures += 1; }

        // ── The engines that are not implemented ────────────────────────────
        // ENOSYS explicitly, so a caller can tell "absent" from "broken".
        let mut tfu = [0u8; 88];
        let tfu_enosys = ioctl(fd, DRM_IOCTL_V3D_SUBMIT_TFU, tfu.as_mut_ptr()) == -1
            && errno() == ENOSYS;
        if !report(b"V3D_SUBMIT_TFU_ENOSYS", tfu_enosys) { failures += 1; }

        let mut pm = [0u8; 40];
        let pm_enosys = ioctl(fd, DRM_IOCTL_V3D_PERFMON_CREATE, pm.as_mut_ptr()) == -1
            && errno() == ENOSYS;
        if !report(b"V3D_PERFMON_CREATE_ENOSYS", pm_enosys) { failures += 1; }

        // ── GEM_CLOSE returns the GPU address space ─────────────────────────
        // The VA allocator is first-fit over live spans, so a BO created after
        // the only live one is closed must land back at the same address. A
        // bump pointer passes every other check here and fails this one — and
        // would then walk off the end of a 32-bit space in a long session.
        let mut hclose = cbo.handle;
        let closed = ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut hclose as *mut u32) == 0;
        let mut cbo2 = DrmV3dCreateBo { size: BO_REQ, flags: 0, handle: 0, offset: 0 };
        let recycled = closed && create_ok
            && ioctl(fd, DRM_IOCTL_V3D_CREATE_BO, &mut cbo2 as *mut _) == 0
            && cbo2.offset == cbo.offset
            // A fresh gem handle, though: handles are never reused within a
            // boot, so a stale one resolves to nothing rather than to somebody
            // else's buffer.
            && cbo2.handle != cbo.handle;
        if !report(b"V3D_GEM_CLOSE_RECYCLES_GPU_VA", recycled) { failures += 1; }
        if cbo2.handle != 0 {
            let mut h2 = cbo2.handle;
            ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut h2 as *mut u32);
        }
        if so_ok {
            let mut dso = DrmSyncobjDestroy { handle: so.handle, pad: 0 };
            ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &mut dso as *mut _);
        }

        // ── Disarm, and prove it took ───────────────────────────────────────
        // The identity must go back to what every other client on this machine
        // expects. Leaving the device claiming to be v3d would send the next
        // Mesa process hunting for a driver that cannot work here.
        ccap.value = 0;
        let disarm_ok = ioctl(fd, DRM_IOCTL_SET_CLIENT_CAP, &mut ccap as *mut _) == 0;
        let mut vnamebuf2 = [0u8; 32];
        let mut vver2 = DrmVersion::default();
        vver2.name_len = vnamebuf2.len();
        vver2.name = vnamebuf2.as_mut_ptr() as u64;
        let restored = disarm_ok
            && ioctl(fd, DRM_IOCTL_VERSION, &mut vver2 as *mut _) == 0
            // Whatever this device called itself before the block (the name
            // depends on whether the host negotiated virgl), it is not `v3d`.
            && !(vnamebuf2[0] == b'v' && vnamebuf2[1] == b'3'
                 && vnamebuf2[2] == b'd' && vnamebuf2[3] == 0);
        if !report(b"V3D_DISARM_RESTORES_IDENTITY", restored) { failures += 1; }
    }

    // ── PRIME / dmabuf export + import round-trip (K5) ──────────────────────
    // Export the dumb buffer as a dmabuf fd, mmap that fd, and confirm it
    // aliases the SAME physical pages as a fresh MAP_DUMB mapping (coherent),
    // then round-trip the fd back to the original GEM handle.
    let mut ph = DrmPrimeHandle::default();
    ph.handle = cd.handle;
    let export_ok = ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut ph as *mut _) == 0 && ph.fd >= 0;
    if !report(b"PRIME_HANDLE_TO_FD", export_ok) { failures += 1; }

    let mut alias_ok = false;
    if export_ok && cd.size > 0 {
        let dp = mmap(core::ptr::null_mut(), cd.size as usize, PROT_READ | PROT_WRITE,
                      MAP_SHARED, ph.fd, 0);
        let cp = mmap(core::ptr::null_mut(), cd.size as usize, PROT_READ | PROT_WRITE,
                      MAP_SHARED, fd, md.offset as i64);
        if dp as isize > 0 && cp as isize > 0 {
            let sentinel: u32 = 0xA5C3_1E2F;
            *(dp as *mut u32) = sentinel;                // write via dmabuf mapping
            let seen = *(cp as *const u32);              // read via dumb mapping
            *(dp as *mut u32) = 0x0040_0000;             // restore gradient pixel (0,0)
            alias_ok = seen == sentinel;
        }
    }
    if !report(b"PRIME_MMAP_ALIAS", alias_ok) { failures += 1; }

    // FD_TO_HANDLE round-trip: the exported fd resolves back to cd.handle.
    let mut ph2 = DrmPrimeHandle::default();
    ph2.fd = ph.fd;
    let import_ok = ioctl(fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &mut ph2 as *mut _) == 0
        && ph2.handle == cd.handle;
    if !report(b"PRIME_FD_TO_HANDLE", import_ok) { failures += 1; }

    if export_ok { close(ph.fd); }

    // ── fork() with a device mapping live ────────────────────────────────────
    //
    // A dumb buffer's mmap is a DEVICE VMA: the kernel records the physical
    // range with the `file_cap == usize::MAX` sentinel and, unlike ordinary
    // memory, does not own those pages — teardown drops the PTEs and frees
    // nothing (mm/src/vmm.rs). fork used to duplicate such a VMA by COPYING it
    // into a fresh buddy allocation, which is wrong in two different ways:
    //
    //   * the child was handed a private snapshot instead of the device, so its
    //     writes went nowhere and the parent's writes were invisible to it;
    //   * where the physical range is not RAM at all — a host-visible virtio-gpu
    //     blob lives in the shared-memory BAR — the copy's source address is
    //     outside the kernel's direct map and the memcpy took the whole machine
    //     down (`Vector=0x0E RIP=memcpy+0xe`).
    //
    // This runs on EVERY host, including one with no 3D and no blob support: a
    // dumb buffer needs neither. The second assertion is what catches the
    // copying fork here — a machine whose device ranges are all RAM-backed
    // cannot reproduce the panic, but it can absolutely prove the mapping is
    // shared rather than copied. (venustest carries the same check over a
    // host-visible blob, for hosts that can make one.)
    //
    // Its own small buffer, so the on-screen gradient below is untouched.
    {
        let mut fd_cd = DrmModeCreateDumb::default();
        fd_cd.width = 64;
        fd_cd.height = 64;
        fd_cd.bpp = 32;
        let fk_create = ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut fd_cd as *mut _) == 0
            && fd_cd.handle != 0 && fd_cd.size > 0;
        let mut fk_md = DrmModeMapDumb::default();
        fk_md.handle = fd_cd.handle;
        let fk_map = fk_create && ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut fk_md as *mut _) == 0;
        let mut dev: *mut u8 = core::ptr::null_mut();
        if fk_map {
            let p = mmap(core::ptr::null_mut(), fd_cd.size as usize,
                         PROT_READ | PROT_WRITE, MAP_SHARED, fd, fk_md.offset as i64);
            if p as isize > 0 { dev = p as *mut u8; }
        }
        // The child's only way to answer. Ordinary MAP_SHARED anonymous memory
        // — deliberately, since that is the fork path the whole Wayland stack
        // depends on, so it doubles as a check that it still works.
        let sh = mmap(core::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE,
                      MAP_SHARED | MAP_ANONYMOUS, -1, 0);
        if dev.is_null() || sh as isize <= 0 {
            if !report(b"FORK_DEVMAP_CHILD_SEES_IT", false) { failures += 1; }
            if !report(b"FORK_DEVMAP_SHARED_NOT_COPIED", false) { failures += 1; }
        } else {
            const HEAD: u8 = 0xA7;
            const TAIL: u8 = 0x5C;
            const CHILD: u8 = 0x3E;
            let last = fd_cd.size as usize - 1;
            let verdict = sh as *mut u8;
            *verdict = 0;
            *dev.add(0) = HEAD;
            *dev.add(last) = TAIL;

            let r = fork();
            if r == 0 {
                // Every access here is through a mapping that exists only
                // because fork built it.
                let seen = *dev.add(0) == HEAD && *dev.add(last) == TAIL;
                *verdict = if seen { 1 } else { 2 };
                *dev.add(0) = CHILD;
                _exit(0);
            }
            if r < 0 {
                if !report(b"FORK_DEVMAP_CHILD_SEES_IT", false) { failures += 1; }
                if !report(b"FORK_DEVMAP_SHARED_NOT_COPIED", false) { failures += 1; }
            } else {
                let mut status: c_int = 0;
                waitpid(r, &mut status, 0);
                // A child that faulted on the mapping does not exit 0 — and a
                // kernel that faulted *building* it never gets here at all.
                let reaped = (status & 0x7f) == 0 && ((status >> 8) & 0xff) == 0;
                if !report(b"FORK_DEVMAP_CHILD_SEES_IT", reaped && *verdict == 1) {
                    failures += 1;
                }
                // The child is gone and its address space was torn down; for a
                // device VMA that must drop PTEs and free nothing, so the
                // parent's mapping is both intact and carrying the child's store.
                let shared = reaped && *dev.add(0) == CHILD && *dev.add(last) == TAIL;
                if !report(b"FORK_DEVMAP_SHARED_NOT_COPIED", shared) { failures += 1; }
            }
        }
        if fd_cd.handle != 0 {
            let mut h = fd_cd.handle;
            ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut h as *mut u32);
        }
    }

    // Hold the gradient on-screen so a screenshot can confirm the present path
    // actually reached the host (Risk R5). Re-flush each pass in case the host
    // needs a repeated transfer. The fb console stays disabled (SETCRTC did it)
    // until close(), so nothing repaints over the gradient during this window.
    puts(b"drmsmoke: holding gradient for screenshot...\n\0".as_ptr());
    let mut n = 0;
    while n < 40 {
        ioctl(fd, DRM_IOCTL_MODE_DIRTYFB, &mut dirty as *mut _);
        usleep(100_000);
        n += 1;
    }

    // DESTROY_DUMB
    let mut dd = DrmModeCreateDumb::default();
    dd.handle = cd.handle;
    let destroy_ok = ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut (dd.handle) as *mut u32) == 0;
    if !report(b"DESTROY_DUMB", destroy_ok) { failures += 1; }

    close(fd);
    puts(b"--- drmsmoke done ---\n\0".as_ptr());
    failures
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}
