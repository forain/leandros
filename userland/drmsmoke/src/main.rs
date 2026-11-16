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
#![feature(c_variadic)]
#![allow(non_camel_case_types)]

#[path = "../../muslrt/rt.rs"]
mod rt;

use core::ffi::c_void;

type c_int = i32;
type c_uint = u32;
type c_ulong = u64;
type size_t = usize;

const O_RDONLY: c_int = 0o0;
const O_RDWR: c_int = 0o2;
const O_CREAT: c_int = 0o100;
const LOCK_EX: c_int = 2;

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
const DRM_IOCTL_MODE_RMFB: c_ulong = 0xC00464AF;
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
const DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD: c_ulong = 0xC01064C1;
const DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE: c_ulong = 0xC01064C2;
const DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE: u32 = 1 << 0;
const DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE: u32 = 1 << 0;
const DMA_BUF_IOCTL_SYNC: c_ulong = 0x40086200;
const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: c_ulong = 0xC0086202;
const DMA_BUF_IOCTL_IMPORT_SYNC_FILE: c_ulong = 0x40086203;
const SYNC_IOC_MERGE: c_ulong = 0xC0303E03;
const SYNC_IOC_FILE_INFO: c_ulong = 0xC0383E04;
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL: u32 = 1 << 0;
const DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT: u32 = 1 << 1;

const DRM_CAP_SYNCOBJ: u64 = 0x13;
const DRM_CAP_SYNCOBJ_TIMELINE: u64 = 0x14;

const EINVAL: i32 = 22;
const ENOENT: i32 = 2;
const ETIME: i32 = 62;
const ENOSYS: i32 = 38;
const EACCES: i32 = 13;

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

/// `struct drm_syncobj_handle` (HANDLE_TO_FD / FD_TO_HANDLE), 16 bytes.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmSyncobjHandle { handle: u32, flags: u32, fd: i32, pad: u32 }

/// `struct dma_buf_export_sync_file` (also the import struct), 8 bytes.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DmaBufSyncFile { flags: u32, fd: i32 }

/// `struct sync_merge_data`, 48 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct SyncMergeData { name: [u8; 32], fd2: i32, fence: i32, flags: u32, pad: u32 }

/// `struct sync_file_info`, 56 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct SyncFileInfo { name: [u8; 32], status: i32, flags: u32, num_fences: u32, pad: u32, sync_fence_info: u64 }

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

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: c_int, buf: *const c_void, count: size_t) -> isize;
    pub fn exit(status: c_int) -> !;

    pub fn open(path: *const u8, oflag: c_int, ...) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn flock(fd: c_int, operation: c_int) -> c_int;
    pub fn dup(fd: c_int) -> c_int;
    // Used only by the fork-with-a-device-mapping check; same relibc-linked
    // idiom as forktest.
    pub fn fork() -> i32;
    pub fn waitpid(pid: i32, stat_loc: *mut c_int, options: c_int) -> i32;
    pub fn _exit(status: c_int) -> !;
    pub fn usleep(usec: c_uint) -> c_int;
    // `--leak`: the parent SIGKILLs half of its children mid-allocation and
    // syncs on a pipe, so the death is exactly a compositor's `kill -9`.
    pub fn kill(pid: i32, sig: c_int) -> c_int;
    pub fn pipe(fds: *mut c_int) -> c_int;
    pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    pub fn fstat(fildes: c_int, buf: *mut u8) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, count: size_t) -> isize;
    pub fn poll(fds: *mut pollfd, nfds: u64, timeout: c_int) -> c_int;
    pub fn mmap(addr: *mut c_void, len: size_t, prot: c_int, flags: c_int,
                fd: c_int, offset: i64) -> *mut c_void;
    pub fn munmap(addr: *mut c_void, len: size_t) -> c_int;
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

// Same as report(), but on FAIL also prints the errno the failing ioctl/read
// left behind, so a cascade of generic FAILs (e.g. every master-gated KMS
// ioctl answering EACCES because another DRM master is running) is
// diagnosable from the log alone instead of needing a debugger. `err` must be
// captured by the caller immediately after the syscall under test -- any
// syscall issued between that call and this one (fb0_census, provoke_console,
// print_dec, ...) will have overwritten relibc's errno.
fn report_errno(name: &[u8], ok: bool, err: i32) -> bool {
    if !ok {
        unsafe { print_dec(b"  errno=", err as u64); }
    }
    report(name, ok)
}

// Prints "<name>: SKIP\n" for a case that was never attempted because a
// precondition it depends on (a live DRM master, in practice) was not met.
// Deliberately NOT routed through report()/failures: a SKIP is neither a PASS
// nor a FAIL, and counting it as one or the other would either hide a real
// regression behind a "just skipped" excuse or fail a run for a condition
// (another compositor holding card0) that has nothing to do with drmsmoke's
// own correctness.
fn report_skip(name: &[u8]) {
    unsafe {
        write(1, name.as_ptr() as *const c_void, name.len());
        write(1, b": SKIP\n".as_ptr() as *const c_void, 7);
    }
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

// ── `--leak [N]`: N compositor-shaped deaths must cost ~0 kernel pages ──────
//
// The regression this guards (2026-09-18): a compositor that died holding its
// swapchain leaked every dumb buffer it had, because a dumb gem handle had no
// owner and `drm_release_open` never retired one; on top of that every
// process leaked its intermediate page tables. Killing the greeter's
// cosmic-comp 15 times ended in `[BUDDY] Allocation failed`.
//
// Each iteration forks a child that does what cosmic-comp's GBM/kms_swrast
// path does at start-up — CREATE_DUMB x3 (two swapchain buffers and a cursor),
// MAP_DUMB + mmap + touch, ADDFB2, one PRIME export mmapped through its fd —
// and then dies WITHOUT any DESTROY_DUMB/RMFB/GEM_CLOSE: odd iterations
// `_exit`, even ones are SIGKILLed by the parent mid-flight. No master, no
// SETCRTC, so it is safe to run next to a live compositor. The parent reads
// MemFree before and after and demands the per-death cost stay under
// LEAK_BUDGET_KIB — a single leaked 4 MiB buffer is 16x over it.
const LEAK_BUDGET_KIB: u64 = 256;

/// `MemFree:` from /proc/meminfo, in KiB (buddy free pages x 4).
unsafe fn mem_free_kib() -> u64 {
    let fd = open(b"/proc/meminfo\0".as_ptr(), O_RDONLY);
    if fd < 0 { return 0; }
    let mut buf = [0u8; 512];
    let mut n = 0usize;
    loop {
        let r = read(fd, buf.as_mut_ptr().add(n) as *mut c_void, buf.len() - n);
        if r <= 0 { break; }
        n += r as usize;
        if n >= buf.len() { break; }
    }
    close(fd);
    let text = &buf[..n];
    let key = b"MemFree:";
    let mut i = 0usize;
    while i + key.len() <= text.len() {
        if &text[i..i + key.len()] == key {
            let mut j = i + key.len();
            while j < text.len() && text[j] == b' ' { j += 1; }
            let mut v = 0u64;
            while j < text.len() && text[j].is_ascii_digit() {
                v = v * 10 + (text[j] - b'0') as u64;
                j += 1;
            }
            return v;
        }
        i += 1;
    }
    0
}

/// The child half of one `--leak` iteration. Never returns.
unsafe fn leak_child(ready_wr: c_int, wait_for_kill: bool) -> ! {
    let fd = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
    if fd < 0 { _exit(2); }
    let mut handles = [0u32; 3];
    let dims: [(u32, u32); 3] = [(1280, 800), (1280, 800), (64, 64)];
    for (i, &(w, h)) in dims.iter().enumerate() {
        let mut cd = DrmModeCreateDumb { height: h, width: w, bpp: 32, flags: 0, handle: 0, pitch: 0, size: 0 };
        if ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut cd as *mut _) != 0 { _exit(3); }
        handles[i] = cd.handle;
        let mut md = DrmModeMapDumb { handle: cd.handle, pad: 0, offset: 0 };
        if ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut md as *mut _) != 0 { _exit(4); }
        let len = cd.size as usize;
        let p = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, md.offset as i64);
        if (p as isize) == -1 { _exit(5); }
        // Touch every page the way a renderer would, then leave it mapped.
        let mut off = 0usize;
        while off < len { *(p as *mut u8).add(off) = 0x18; off += 4096; }
        let mut fb = DrmModeFbCmd2::default();
        fb.width = w; fb.height = h; fb.pixel_format = DRM_FORMAT_XRGB8888;
        fb.handles[0] = cd.handle; fb.pitches[0] = cd.pitch;
        if ioctl(fd, DRM_IOCTL_MODE_ADDFB2, &mut fb as *mut _) != 0 { _exit(6); }
    }
    // One buffer exported over PRIME and mapped through the dmabuf fd — the
    // exported-while-owned shape smithay's software renderer leaves behind.
    let mut ph = DrmPrimeHandle { handle: handles[0], flags: 0, fd: -1 };
    if ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut ph as *mut _) != 0 || ph.fd < 0 { _exit(7); }
    let p = mmap(core::ptr::null_mut(), 4096 * 16, PROT_READ | PROT_WRITE, MAP_SHARED, ph.fd, 0);
    if (p as isize) == -1 { _exit(8); }
    *(p as *mut u8) = 1;
    // Tell the parent every allocation is in place, then die holding all of it.
    let b = [b'k'];
    write(ready_wr, b.as_ptr() as *const c_void, 1);
    if wait_for_kill {
        loop { usleep(10_000); }
    }
    _exit(0);
}

unsafe fn leak_mode(iters: u32) -> i32 {
    let mut failures = 0i32;
    let before = mem_free_kib();
    if before == 0 {
        report(b"leak_meminfo_readable", false);
        return 1;
    }
    let mut child_failures = 0u32;
    let mut i = 0u32;
    while i < iters {
        let mut fds = [0 as c_int; 2];
        if pipe(fds.as_mut_ptr()) != 0 { child_failures += 1; i += 1; continue; }
        let by_signal = i % 2 == 1;
        let pid = fork();
        if pid == 0 {
            close(fds[0]);
            leak_child(fds[1], by_signal);
        }
        close(fds[1]);
        if pid < 0 { close(fds[0]); child_failures += 1; i += 1; continue; }
        let mut tok = [0u8; 1];
        let got = read(fds[0], tok.as_mut_ptr() as *mut c_void, 1);
        close(fds[0]);
        if got != 1 { child_failures += 1; }
        if by_signal { kill(pid, 9); }
        let mut status = 0 as c_int;
        waitpid(pid, &mut status, 0);
        if !by_signal && got == 1 && status != 0 { child_failures += 1; }
        i += 1;
    }
    // Let the last reap's teardown land before sampling.
    usleep(200_000);
    let after = mem_free_kib();
    let lost_kib = before.saturating_sub(after);
    let per_death = if iters > 0 { lost_kib / iters as u64 } else { 0 };
    print_dec(b"  leak: deaths=", iters as u64);
    print_dec(b"  leak: memfree_before_kib=", before);
    print_dec(b"  leak: memfree_after_kib=", after);
    print_dec(b"  leak: kib_lost_per_death=", per_death);
    if !report(b"leak_children_alloc_ok", child_failures == 0) { failures += 1; }
    if !report(b"leak_per_death_under_budget", per_death < LEAK_BUDGET_KIB) { failures += 1; }
    failures
}

// ── GPU robustness: out-fence atomicity and framebuffer lifetime ─────────────
//
// Lane gpufencefb (2026-10-01). Two driver contracts, each of which once cost a
// COSMIC session its rendering:
//
//  1. EXECBUFFER with FENCE_FD_OUT is ATOMIC. When the out-fence fd cannot be
//     created (fd table full, EMFILE), the ioctl fails and NOTHING is
//     submitted — upstream's `virtio_gpu_execbuffer_ioctl` reserves the fd
//     before it submits. Observable through the open's last fence
//     (`VIRTGPU_PARAM_LEANDROS_LAST_FENCE`): unchanged by the failed call.
//
//  2. A KMS framebuffer holds a reference on its BO, and an open's
//     framebuffers die with it (`drm_fb_release`). Observable through the
//     test's own object's reference count (`VIRTGPU_PARAM_LEANDROS_DUMB_OBJ_OF`
//     / `_DUMB_OBJ_REFS`): a DESTROY_DUMB under a live framebuffer leaves it
//     alive, RMFB / close destroys it. (Not the device-wide count
//     `VIRTGPU_PARAM_LEANDROS_DUMB_OBJS`: other clients move that.)

const DRM_IOCTL_VIRTGPU_EXECBUFFER: c_ulong = 0xC0406442;
const DRM_IOCTL_VIRTGPU_GETPARAM: c_ulong = 0xC0106443;
const DRM_IOCTL_MODE_GETCRTC: c_ulong = 0xC06864A1;
const VIRTGPU_PARAM_3D_FEATURES: u64 = 1;
const VIRTGPU_PARAM_LEANDROS_LAST_FENCE: u64 = 0x1000_0004;
const VIRTGPU_PARAM_LEANDROS_DUMB_OBJS: u64 = 0x1000_0006;
const VIRTGPU_PARAM_LEANDROS_FB_STATS: u64 = 0x1000_0007;
/// In/out through `value`: gem handle -> object id, object id -> refcount
/// (0 = destroyed). Per-object, so another client allocating at the same time
/// (the greeter's compositor starting up) cannot move the answer.
const VIRTGPU_PARAM_LEANDROS_DUMB_OBJ_OF: u64 = 0x1000_0008;
const VIRTGPU_PARAM_LEANDROS_DUMB_OBJ_REFS: u64 = 0x1000_0009;
const VIRTGPU_EXECBUF_FENCE_FD_OUT: u32 = 0x02;
const EMFILE: i32 = 24;

#[repr(C)]
#[derive(Default)]
struct DrmVirtgpuGetparam { param: u64, value: u64 }

#[repr(C)]
#[derive(Default)]
struct DrmVirtgpuExecbuffer {
    flags: u32,
    size: u32,
    command: u64,
    bo_handles: u64,
    num_bo_handles: u32,
    fence_fd: i32,
    ring_idx: u32,
    syncobj_stride: u32,
    num_in_syncobjs: u32,
    num_out_syncobjs: u32,
    in_syncobjs: u64,
    out_syncobjs: u64,
}

/// GETPARAM through a user pointer, as upstream: `value` is where the int goes.
unsafe fn gp(fd: c_int, param: u64) -> Option<u32> {
    let mut out: u32 = 0;
    let mut g = DrmVirtgpuGetparam { param, value: &mut out as *mut u32 as u64 };
    if ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &mut g as *mut _) == 0 { Some(out) } else { None }
}

/// GETPARAM whose `value` target carries an input as well as the answer.
unsafe fn gp_in(fd: c_int, param: u64, input: u32) -> Option<u32> {
    let mut io: u32 = input;
    let mut g = DrmVirtgpuGetparam { param, value: &mut io as *mut u32 as u64 };
    if ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &mut g as *mut _) == 0 { Some(io) } else { None }
}

/// Object id behind dumb handle `h` (0 = none).
unsafe fn obj_of(fd: c_int, h: u32) -> u32 {
    gp_in(fd, VIRTGPU_PARAM_LEANDROS_DUMB_OBJ_OF, h).unwrap_or(0)
}

/// References on object `o` (0 = destroyed; u32::MAX = no object / query failed).
unsafe fn obj_refs(fd: c_int, o: u32) -> u32 {
    if o == 0 { return u32::MAX; }
    gp_in(fd, VIRTGPU_PARAM_LEANDROS_DUMB_OBJ_REFS, o).unwrap_or(u32::MAX)
}

/// EXECBUFFER of `words` with FENCE_FD_OUT. Returns (rc, errno, fence_fd).
unsafe fn exec_out_fence(fd: c_int, words: &[u32]) -> (c_int, i32, i32) {
    let mut eb = DrmVirtgpuExecbuffer::default();
    eb.flags = VIRTGPU_EXECBUF_FENCE_FD_OUT;
    eb.size = (words.len() * 4) as u32;
    eb.command = words.as_ptr() as u64;
    eb.fence_fd = 0x5a5a; // must come back overwritten on every path
    let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &mut eb as *mut _);
    let e = if rc != 0 { errno() } else { 0 };
    (rc, e, eb.fence_fd)
}

/// Fill this process's fd table with dups of `fd` until EMFILE. Returns the
/// dups (so they can be closed) and whether the table actually filled.
unsafe fn fill_fd_table(fd: c_int, out: &mut [c_int]) -> (usize, bool) {
    let mut n = 0usize;
    while n < out.len() {
        let d = dup(fd);
        if d < 0 { return (n, errno() == EMFILE); }
        out[n] = d;
        n += 1;
    }
    (n, false)
}

unsafe fn dumb_fb(fd: c_int, w: u32, h: u32) -> (u32, u32) {
    let mut cd = DrmModeCreateDumb::default();
    cd.width = w; cd.height = h; cd.bpp = 32;
    if ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut cd as *mut _) != 0 { return (0, 0); }
    let mut f = DrmModeFbCmd2::default();
    f.width = w; f.height = h; f.pixel_format = DRM_FORMAT_XRGB8888;
    f.handles[0] = cd.handle; f.pitches[0] = cd.pitch;
    if ioctl(fd, DRM_IOCTL_MODE_ADDFB2, &mut f as *mut _) != 0 { return (cd.handle, 0); }
    (cd.handle, f.fb_id)
}

unsafe fn rmfb(fd: c_int, fb_id: u32) -> bool {
    let mut id = fb_id;
    ioctl(fd, DRM_IOCTL_MODE_RMFB, &mut id as *mut u32) == 0
}

unsafe fn destroy_dumb(fd: c_int, handle: u32) {
    let mut h = handle;
    ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut h as *mut u32);
}

unsafe fn setcrtc(fd: c_int, fb_id: u32, connector_id: u32, w: u32, h: u32) -> bool {
    let mut set = core::mem::zeroed::<DrmModeCrtc>();
    set.crtc_id = 1;
    set.fb_id = fb_id;
    let connectors = [connector_id];
    set.set_connectors_ptr = connectors.as_ptr() as u64;
    set.count_connectors = 1;
    set.mode.hdisplay = w as u16;
    set.mode.vdisplay = h as u16;
    set.mode.vrefresh = 60;
    set.mode_valid = 1;
    ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &mut set as *mut _) == 0
}

unsafe fn getcrtc_fb(fd: c_int) -> Option<u32> {
    let mut c = core::mem::zeroed::<DrmModeCrtc>();
    c.crtc_id = 1;
    if ioctl(fd, DRM_IOCTL_MODE_GETCRTC, &mut c as *mut _) == 0 { Some(c.fb_id) } else { None }
}

/// The framebuffer-lifetime cases. `kms` = this process holds the scanout
/// (SETCRTC worked), so the scanned-out-RMFB case can run; `restore_fb` is
/// put back on screen afterwards.
unsafe fn fb_lifetime_cases(fd: c_int, kms: bool, restore_fb: u32,
                            connector_id: u32, w: u32, h: u32) -> (i32, i32) {
    let (mut failures, mut skips) = (0i32, 0i32);
    // Every assertion below follows ONE object's reference count. They used
    // to compare the device-wide DUMB_OBJS count before and after, which any
    // other client moves: on a virgl boot the greeter's compositor is still
    // allocating when the first test command runs, and FB_SWEPT_ON_CLOSE
    // failed with before/mid/after = 79/81/80 although the sweep had run.
    if gp_in(fd, VIRTGPU_PARAM_LEANDROS_DUMB_OBJ_OF, 0).is_none() {
        for c in [&b"FB_HOLDS_BO_REF"[..], b"FB_RMFB_DROPS_BO_REF", b"FB_RMFB_TWICE_FAILS",
                  b"FB_RMFB_OTHER_OPEN_REFUSED", b"FB_SWEPT_ON_CLOSE",
                  b"FB_RMFB_SCANOUT_DISABLES_PLANE"] {
            report_skip(c); skips += 1;
        }
        return (0, skips);
    }

    // ADDFB2, then DESTROY_DUMB: the framebuffer's reference keeps the BO.
    // Live handle + framebuffer = 2 references; the framebuffer's alone = 1.
    let (h1, fb1) = dumb_fb(fd, 64, 64);
    let o1 = obj_of(fd, h1);
    let r1 = obj_refs(fd, o1);
    destroy_dumb(fd, h1);
    let r2 = obj_refs(fd, o1);
    let ok = fb1 != 0 && o1 != 0 && r1 == 2 && r2 == 1;
    if !ok { print_dec(b"  obj/refs addfb/refs destroy: ", ((o1 as u64) << 32) | ((r1 as u64 & 0xFFFF) << 16) | (r2 as u64 & 0xFFFF)); }
    if !report(b"FB_HOLDS_BO_REF", ok) { failures += 1; }

    // RMFB drops the last reference: the object is destroyed.
    let rm_ok = rmfb(fd, fb1);
    let r3 = obj_refs(fd, o1);
    if !report(b"FB_RMFB_DROPS_BO_REF", fb1 != 0 && rm_ok && r3 == 0) { failures += 1; }
    if !report(b"FB_RMFB_TWICE_FAILS", fb1 != 0 && !rmfb(fd, fb1)) { failures += 1; }

    // Only the creating open may RMFB (upstream: -ENOENT for anyone else).
    let fd3 = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
    let (h3, fb3) = if fd3 >= 0 { dumb_fb(fd3, 64, 64) } else { (0, 0) };
    let refused = fb3 != 0 && !rmfb(fd, fb3);
    let own_ok = fb3 != 0 && rmfb(fd3, fb3);
    if !report(b"FB_RMFB_OTHER_OPEN_REFUSED", refused && own_ok) { failures += 1; }
    if fd3 >= 0 { destroy_dumb(fd3, h3); close(fd3); }

    // A second open creates a framebuffer and closes without RMFB or
    // DESTROY_DUMB: the close must remove the framebuffer and both
    // references (handle + framebuffer) must go, destroying the object. The
    // swept-framebuffer counter is device-wide (another open may be swept
    // meanwhile), so it must have advanced by AT LEAST this one.
    let s0 = gp(fd, VIRTGPU_PARAM_LEANDROS_FB_STATS).unwrap_or(0) >> 16;
    let fd2 = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
    let (h2, fb2) = if fd2 >= 0 { dumb_fb(fd2, 64, 64) } else { (0, 0) };
    let o2 = obj_of(fd, h2);
    let mid = obj_refs(fd, o2);
    if fd2 >= 0 { close(fd2); }
    let after = obj_refs(fd, o2);
    let s1 = gp(fd, VIRTGPU_PARAM_LEANDROS_FB_STATS).unwrap_or(0) >> 16;
    let d = s1.wrapping_sub(s0) & 0xFFFF;
    let swept = fb2 != 0 && o2 != 0 && mid == 2 && after == 0 && d >= 1 && d < 0x8000;
    if !swept { print_dec(b"  sweep obj/refs mid/refs after/swept: ", ((o2 as u64) << 48) | ((mid as u64 & 0xFFFF) << 32) | ((after as u64 & 0xFFFF) << 16) | d as u64); }
    if !report(b"FB_SWEPT_ON_CLOSE", swept) { failures += 1; }

    // RMFB of the framebuffer on screen disables the plane (upstream
    // `drm_framebuffer_remove`), so the scanout can never be left naming the
    // resource the RMFB is about to release.
    if !kms {
        report_skip(b"FB_RMFB_SCANOUT_DISABLES_PLANE"); skips += 1;
    } else {
        let (h4, fb4) = dumb_fb(fd, w, h);
        let shown = fb4 != 0 && setcrtc(fd, fb4, connector_id, w, h)
            && getcrtc_fb(fd) == Some(fb4);
        let removed = shown && rmfb(fd, fb4);
        let after = getcrtc_fb(fd);
        destroy_dumb(fd, h4);
        let back = restore_fb != 0 && setcrtc(fd, restore_fb, connector_id, w, h);
        let ok = shown && removed && after == Some(0) && back;
        if !ok { print_dec(b"  crtc fb after RMFB: ", after.unwrap_or(u32::MAX) as u64); }
        if !report(b"FB_RMFB_SCANOUT_DISABLES_PLANE", ok) { failures += 1; }
    }
    (failures, skips)
}

/// Lane vrr: the Linux VRR KMS properties. Linux's atomic core stores
/// VRR_ENABLED without checking the sink (drivers just leave adaptive sync
/// off), so a 1 on our non-capable connector is accepted; out-of-range values
/// and writes to the immutable vrr_capable are EINVAL.
unsafe fn vrr_props(fd: c_int, obj: u32, ty: u32) -> ([u32; 16], [u64; 16], usize) {
    let mut ids = [0u32; 16];
    let mut vals = [0u64; 16];
    let mut g: [u64; 4] = [ids.as_mut_ptr() as u64, vals.as_mut_ptr() as u64, 16, 0];
    g[2] = 16 | ((obj as u64) << 32);
    g[3] = ty as u64;
    let rc = ioctl(fd, 0xC02064B9, &mut g as *mut _);
    (ids, vals, if rc == 0 { (g[2] & 0xFFFF_FFFF) as usize } else { 0 })
}

/// (id, flags, min, max) of the property named `name` on the object, or None.
unsafe fn vrr_find(fd: c_int, obj: u32, ty: u32, name: &[u8]) -> Option<(u32, u32, u64, u64, u64)> {
    let (ids, vals, n) = vrr_props(fd, obj, ty);
    for i in 0..n.min(16) {
        let mut rng = [0u64; 2];
        let mut g = [0u64; 8];
        g[0] = rng.as_mut_ptr() as u64;
        *(g.as_mut_ptr() as *mut u32).add(4) = ids[i];
        *(g.as_mut_ptr() as *mut u32).add(14) = 2;
        if ioctl(fd, 0xC04064AA, &mut g as *mut _) != 0 { continue; }
        let flags = *(g.as_ptr() as *const u32).add(5);
        let nm = (g.as_ptr() as *const u8).add(24);
        let mut eq = true;
        for (j, &b) in name.iter().enumerate() { if *nm.add(j) != b { eq = false; } }
        if eq && *nm.add(name.len()) == 0 { return Some((ids[i], flags, rng[0], rng[1], vals[i])); }
    }
    None
}

unsafe fn vrr_cases(fd: c_int) -> i32 {
    let mut failures = 0i32;
    const CRTC_T: u32 = 0xcccc_cccc;
    const CONN_T: u32 = 0xc0c0_c0c0;
    let en = vrr_find(fd, 1, CRTC_T, b"VRR_ENABLED");
    let cap = vrr_find(fd, 1, CONN_T, b"vrr_capable");
    let props_ok = matches!(en, Some((_, fl, 0, 1, 0)) if fl & 4 == 0 && fl & 2 != 0)
        && matches!(cap, Some((_, fl, 0, 1, 0)) if fl & 4 != 0 && fl & 2 != 0);
    if !report(b"VRR_PROPS_ENUMERATE", props_ok) { failures += 1; return failures; }
    let (en_id, cap_id) = (en.unwrap().0, cap.unwrap().0);
    let a = |flags: u32, obj: u32, p: u32, v: u64| c2_atomic(fd, flags, &[obj], &[1], &[p], &[v]);
    let t_ok = a(DRM_MODE_ATOMIC_TEST_ONLY, 1, en_id, 1) == 0 && a(DRM_MODE_ATOMIC_TEST_ONLY, 1, en_id, 0) == 0;
    if !report(b"VRR_ENABLED_TEST_ONLY_ACCEPTED", t_ok) { failures += 1; }
    let still0 = matches!(vrr_find(fd, 1, CRTC_T, b"VRR_ENABLED"), Some((_, _, _, _, 0)));
    if !report(b"VRR_TEST_ONLY_DOES_NOT_STICK", still0) { failures += 1; }
    let r2 = a(DRM_MODE_ATOMIC_TEST_ONLY, 1, en_id, 2) != 0 && errno() == EINVAL;
    if !report(b"VRR_ENABLED_OUT_OF_RANGE_EINVAL", r2) { failures += 1; }
    let r3 = a(DRM_MODE_ATOMIC_TEST_ONLY, 1, cap_id, 1) != 0 && errno() == EINVAL;
    if !report(b"VRR_CAPABLE_IMMUTABLE_EINVAL", r3) { failures += 1; }
    // Legacy OBJ_SETPROPERTY: {value, prop_id, obj_id, obj_type}.
    let mut sp: [u64; 3] = [1, (en_id as u64) | (1u64 << 32), CRTC_T as u64];
    let set1 = ioctl(fd, 0xC01864BA, &mut sp as *mut _) == 0
        && matches!(vrr_find(fd, 1, CRTC_T, b"VRR_ENABLED"), Some((_, _, _, _, 1)));
    sp[0] = 0;
    let set0 = ioctl(fd, 0xC01864BA, &mut sp as *mut _) == 0
        && matches!(vrr_find(fd, 1, CRTC_T, b"VRR_ENABLED"), Some((_, _, _, _, 0)));
    if !report(b"VRR_LEGACY_SETPROPERTY_ROUNDTRIP", set1 && set0) { failures += 1; }
    sp[0] = 1; sp[1] = (cap_id as u64) | (1u64 << 32);
    let bad = ioctl(fd, 0xC01864BA, &mut sp as *mut _) != 0 && errno() == EINVAL;
    if !report(b"VRR_LEGACY_IMMUTABLE_EINVAL", bad) { failures += 1; }
    failures
}

/// The out-fence atomicity cases. Run on a fresh open, so the context they
/// create (and, in `demo` mode, deliberately break) is theirs alone.
///
/// `demo`: the refused submission carries a virgl CREATE_OBJECT(DSA) and the
/// next, accepted one BINDs it — the exact shape of the lost-CREATE_OBJECT
/// failure. The host then reports `Illegal handle 4660` for this context in
/// QEMU's stderr, which is the end-to-end proof that a refused EXECBUFFER
/// reached nothing. A control open does the same with the fd table free and
/// must produce no host error.
unsafe fn out_fence_cases(demo: bool) -> (i32, i32) {
    let (mut failures, mut skips) = (0i32, 0i32);
    let names: [&[u8]; 3] = [b"EXECBUF_OUT_FENCE_SIGNALS", b"EXECBUF_OUT_FENCE_EMFILE_ATOMIC",
                             b"EXECBUF_OUT_FENCE_AFTER_FREE"];
    let fx = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
    let virgl = fx >= 0 && gp(fx, VIRTGPU_PARAM_3D_FEATURES).unwrap_or(0) != 0;
    if !virgl {
        for n in names { report_skip(n); skips += 1; }
        if fx >= 0 { close(fx); }
        return (0, skips);
    }
    const H: u32 = 0x1234;
    // VIRGL_CMD0(cmd, obj, len) = cmd | obj << 8 | len << 16; DSA = 3.
    let nop: [u32; 1] = [0];
    let create: [u32; 6] = [0x0005_0301, H, 0, 0, 0, 0];
    let bind: [u32; 2] = [0x0001_0302, H];

    if demo {
        // Control: same stream pair, nothing refused.
        let fc = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
        if fc >= 0 {
            let (r1, _, f1) = exec_out_fence(fc, &create);
            let (r2, _, f2) = exec_out_fence(fc, &bind);
            print_dec(b"drmsmoke: demo control create rc+1=", (r1 + 1) as u64);
            print_dec(b"drmsmoke: demo control bind rc+1=", (r2 + 1) as u64);
            if f1 >= 0 { close(f1); }
            if f2 >= 0 { close(f2); }
            usleep(200_000);
            close(fc);
        }
    }

    // A first, ordinary submission: the out-fence fd is real and signals.
    let (rc, e, ffd) = exec_out_fence(fx, &nop);
    let mut signalled = false;
    if rc == 0 && ffd >= 0 {
        let mut p = pollfd { fd: ffd, events: POLLIN, revents: 0 };
        signalled = poll(&mut p as *mut _, 1, 2000) == 1 && (p.revents & POLLIN) != 0;
        close(ffd);
    } else {
        print_dec(b"  first submit errno=", e as u64);
    }
    if !report(names[0], rc == 0 && ffd >= 0 && signalled) { failures += 1; }
    let l1 = gp(fx, VIRTGPU_PARAM_LEANDROS_LAST_FENCE).unwrap_or(0);

    // Fill the fd table, then submit: EMFILE, fence_fd -1, nothing submitted.
    let mut dups = [-1i32; 1100];
    let (nd, full) = fill_fd_table(fx, &mut dups);
    let (rc2, e2, ffd2) = exec_out_fence(fx, if demo { &create[..] } else { &nop[..] });
    let l2 = gp(fx, VIRTGPU_PARAM_LEANDROS_LAST_FENCE).unwrap_or(u32::MAX);
    let atomic = full && rc2 != 0 && e2 == EMFILE && ffd2 == -1 && l2 == l1;
    if !atomic {
        print_dec(b"  table full=", full as u64);
        print_dec(b"  dups=", nd as u64);
        print_dec(b"  errno=", e2 as u64);
        print_dec(b"  fence_fd+1=", (ffd2 + 1) as u64);
        print_dec(b"  last fence before=", l1 as u64);
        print_dec(b"  last fence after=", l2 as u64);
    }
    if !report(names[1], atomic) { failures += 1; }

    // One fd free again: the same call now succeeds and does submit.
    if nd > 0 { close(dups[nd - 1]); dups[nd - 1] = -1; }
    let (rc3, _, ffd3) = exec_out_fence(fx, if demo { &bind[..] } else { &nop[..] });
    let l3 = gp(fx, VIRTGPU_PARAM_LEANDROS_LAST_FENCE).unwrap_or(0);
    if !report(names[2], rc3 == 0 && ffd3 >= 0 && l3 != l1) { failures += 1; }
    if ffd3 >= 0 { close(ffd3); }
    let mut i = 0usize;
    while i < nd { if dups[i] >= 0 { close(dups[i]); } i += 1; }
    if demo {
        puts(b"drmsmoke: demo done -- expect host 'Illegal handle 4660' for the lossy context only\n\0".as_ptr());
        usleep(300_000);
    }
    close(fx);
    (failures, skips)
}

// ── Lane ffmagenta: TRANSFER_TO/FROM_HOST and VIRTGPU_WAIT's errno ──────────
//
// 1. TRANSFER_ROUNDTRIP: a 4 KiB virgl buffer resource gets a pattern written
//    into its guest backing, TRANSFER_TO_HOST, the backing zeroed,
//    TRANSFER_FROM_HOST, WAIT; the pattern must be back. The two ioctls used to
//    ignore their argument (a bare header the host refused, answered 0), so
//    the backing stayed zero.
// 2. VIRTGPU_WAIT_NOWAIT_EBUSY: Mesa's virgl winsys counts a BO busy ONLY on
//    errno EBUSY. Each NOWAIT probe straight after a fenced transfer must
//    either succeed or fail with EBUSY; it used to fail with errno 1, which
//    Mesa read as "idle" and wrote into buffers the host had yet to read.
const DRM_IOCTL_VIRTGPU_RESOURCE_CREATE: c_ulong = 0xC0386444;
const DRM_IOCTL_VIRTGPU_MAP: c_ulong = 0xC0106441;
const DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST: c_ulong = 0xC02C6446;
const DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST: c_ulong = 0xC02C6447;
const DRM_IOCTL_VIRTGPU_WAIT: c_ulong = 0xC0086448;
const DRM_IOCTL_GEM_CLOSE_FF: c_ulong = 0x40086409;
const VIRTGPU_WAIT_NOWAIT: u32 = 1;
const EBUSY_FF: i32 = 16;

#[repr(C)]
#[derive(Default)]
struct VirtgpuResourceCreate {
    target: u32, format: u32, bind: u32, width: u32, height: u32, depth: u32,
    array_size: u32, last_level: u32, nr_samples: u32, flags: u32,
    bo_handle: u32, res_handle: u32, size: u32, stride: u32,
}
#[repr(C)]
#[derive(Default)]
struct VirtgpuMap { offset: u64, handle: u32, pad: u32 }
#[repr(C)]
#[derive(Default)]
struct VirtgpuTransfer {
    bo_handle: u32, x: u32, y: u32, z: u32, w: u32, h: u32, d: u32,
    level: u32, offset: u32, stride: u32, layer_stride: u32,
}
#[repr(C)]
#[derive(Default)]
struct VirtgpuWait { handle: u32, flags: u32 }

unsafe fn transfer_cases(fd: c_int) -> (i32, i32) {
    const SIZE: u32 = 4096;
    let mut rc = VirtgpuResourceCreate {
        target: 0,            // PIPE_BUFFER
        format: 64,           // VIRGL_FORMAT_R8_UNORM
        bind: 1 << 4,         // VIRGL_BIND_VERTEX_BUFFER
        width: SIZE, height: 1, depth: 1, array_size: 1,
        size: SIZE,
        ..Default::default()
    };
    if ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE, &mut rc as *mut _) != 0 || rc.bo_handle == 0 {
        report_skip(b"TRANSFER_ROUNDTRIP (no virgl RESOURCE_CREATE)");
        report_skip(b"VIRTGPU_WAIT_NOWAIT_EBUSY");
        return (0, 2);
    }
    let mut failures = 0;
    let mut mp = VirtgpuMap { handle: rc.bo_handle, ..Default::default() };
    let mut base: *mut u8 = core::ptr::null_mut();
    if ioctl(fd, DRM_IOCTL_VIRTGPU_MAP, &mut mp as *mut _) == 0 {
        let m = mmap(core::ptr::null_mut(), SIZE as size_t, PROT_READ | PROT_WRITE,
                     MAP_SHARED, fd, mp.offset as i64);
        if m as isize != -1 { base = m as *mut u8; }
    }
    if base.is_null() {
        report(b"TRANSFER_ROUNDTRIP (map)", false);
        failures += 1;
    } else {
        let pat = |i: usize| (i as u32).wrapping_mul(2654435761).rotate_right(13) as u8 | 1;
        for i in 0..SIZE as usize { *base.add(i) = pat(i); }
        let mut t = VirtgpuTransfer { bo_handle: rc.bo_handle, w: SIZE, h: 1, d: 1, ..Default::default() };
        let up = ioctl(fd, DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST, &mut t as *mut _) == 0;
        let mut w = VirtgpuWait { handle: rc.bo_handle, flags: 0 };
        let w1 = ioctl(fd, DRM_IOCTL_VIRTGPU_WAIT, &mut w as *mut _) == 0;
        for i in 0..SIZE as usize { *base.add(i) = 0; }
        let mut t2 = VirtgpuTransfer { bo_handle: rc.bo_handle, w: SIZE, h: 1, d: 1, ..Default::default() };
        let down = ioctl(fd, DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST, &mut t2 as *mut _) == 0;
        // NOWAIT straight after a fenced transfer: success or EBUSY, nothing else.
        let mut wn = VirtgpuWait { handle: rc.bo_handle, flags: VIRTGPU_WAIT_NOWAIT };
        let r = ioctl(fd, DRM_IOCTL_VIRTGPU_WAIT, &mut wn as *mut _);
        let e = if r != 0 { errno() } else { 0 };
        let mut errno_ok = r == 0 || e == EBUSY_FF;
        let mut busy_seen = (r != 0) as u64;
        let w2 = ioctl(fd, DRM_IOCTL_VIRTGPU_WAIT, &mut w as *mut _) == 0;
        let mut bad = 0u64;
        for i in 0..SIZE as usize { if *base.add(i) != pat(i) { bad += 1; } }
        let ok = up && w1 && down && w2 && bad == 0;
        if !ok { print_dec(b"  mismatched bytes=", bad); }
        if !report(b"TRANSFER_ROUNDTRIP", ok) { failures += 1; }
        // More probes, to give a busy answer a chance to show up.
        let mut n = 0;
        while n < 64 && errno_ok {
            let mut t3 = VirtgpuTransfer { bo_handle: rc.bo_handle, w: SIZE, h: 1, d: 1, ..Default::default() };
            ioctl(fd, DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST, &mut t3 as *mut _);
            let mut wn = VirtgpuWait { handle: rc.bo_handle, flags: VIRTGPU_WAIT_NOWAIT };
            let r = ioctl(fd, DRM_IOCTL_VIRTGPU_WAIT, &mut wn as *mut _);
            if r != 0 {
                let e = errno();
                if e != EBUSY_FF { errno_ok = false; print_dec(b"  NOWAIT errno=", e as u64); }
                busy_seen += 1;
            }
            n += 1;
        }
        ioctl(fd, DRM_IOCTL_VIRTGPU_WAIT, &mut w as *mut _);
        print_dec(b"drmsmoke: nowait_busy_answers=", busy_seen);
        if !report(b"VIRTGPU_WAIT_NOWAIT_EBUSY", errno_ok) { failures += 1; }
        munmap(base as *mut c_void, SIZE as size_t);
    }
    let mut gc = [rc.bo_handle, 0u32];
    ioctl(fd, DRM_IOCTL_GEM_CLOSE_FF, gc.as_mut_ptr());
    (failures, 0)
}

// ── Lane virglscat (L1.10): CPU mmap + PRIME of a SCATTERED virgl resource ──
//
// VIRTGPU_PARAM_LEANDROS_SCATTER selector 3 forces the frame-list backing
// (2 MiB chunks plus single pages) that RESOURCE_CREATE otherwise uses only
// when no contiguous block is free. A 2 MiB + 12 KiB buffer then spans two
// frame runs. The case writes a pattern through VIRTGPU_MAP + mmap, sends it
// to the host and back (TRANSFER_TO/FROM_HOST, so the host reads the same
// frames the CPU mapping wrote), and checks a second mmap and a PRIME dma-buf
// mmap see the same bytes. Scattered buffers used to refuse VIRTGPU_MAP.
const VIRTGPU_PARAM_LEANDROS_SCATTER: u64 = 0x1000_000B;

// L1.14 BO MAPPING LIFETIME. A device mmap of a BO is a reference on the BO's
// pages, as on Linux (the VMA holds the GEM object until the last unmap): a
// GEM_CLOSE while the mapping is live must not return the frames to the
// allocator. The case maps a BO, writes a pattern, closes the handle, then
// creates and scribbles over fresh BOs of the same size (the allocator's first
// choice for the freed block) and checks (a) no new BO got the old token,
// (b) the old mapping still reads the pattern, (c) after a partial munmap the
// rest still does. Run for a dumb buffer and for a scattered virgl resource.
unsafe fn maplife_check(fd: c_int, name: &[u8], base: *mut u8, n: usize, token: u64,
                        mut new_bo: impl FnMut() -> Option<(u32, u64)>) -> i32 {
    let pat = |i: usize| ((i as u32).wrapping_mul(2654435761).rotate_right(11) as u8) | 1;
    let mut fails = 0;
    let mut reused = 0u64;
    let mut handles = [0u32; 6];
    for h in handles.iter_mut() {
        let Some((bh, off)) = new_bo() else { break };
        *h = bh;
        if off == token { reused += 1; }
        let m = mmap(core::ptr::null_mut(), n as size_t, PROT_READ | PROT_WRITE, MAP_SHARED, fd, off as i64);
        if m as isize != -1 {
            for i in 0..n { *(m as *mut u8).add(i) = 0xEE; }
            munmap(m, n as size_t);
        }
    }
    let mut bad = 0u64;
    for i in 0..n { if *base.add(i) != pat(i) { bad += 1; } }
    if reused != 0 { print_dec(b"  token reused by new BOs=", reused); }
    if bad != 0 { print_dec(b"  mismatched bytes after close=", bad); }
    puts(name.as_ptr());
    if !report(b"MAPLIFE_SURVIVES_GEM_CLOSE", reused == 0 && bad == 0) { fails += 1; }
    // Partial munmap: the head page goes, the rest must stay intact.
    munmap(base as *mut c_void, 4096);
    let mut bad2 = 0u64;
    for i in 4096..n { if *base.add(i) != pat(i) { bad2 += 1; } }
    if !report(b"MAPLIFE_PARTIAL_MUNMAP", bad2 == 0) { fails += 1; }
    munmap(base.add(4096) as *mut c_void, (n - 4096) as size_t);
    for &h in handles.iter() {
        if h != 0 { let mut c = [h, 0u32]; ioctl(fd, DRM_IOCTL_GEM_CLOSE_FF, &mut c as *mut _); }
    }
    fails
}

// ── Lane drmtoken (L1.14): mmap tokens are scoped to the drm_file ─────────────
// A creates a dumb BO and writes a secret; a forked child opens the node AFRESH
// (its own drm_file, no handle on the BO), receives the token over a pipe and
// mmaps it: refused, as Linux's drm_vma_node_is_allowed refuses it. The same
// child then PRIME-imports the BO (the inherited dma-buf fd) on its own open:
// that open now holds a handle, so MAP_DUMB + mmap must work and see the secret.
unsafe fn tokenscope_cases(fd: c_int) -> (i32, i32) {
    let mut fails = 0;
    let secret: u32 = 0x5EC2_E7A1;
    let mut c = DrmModeCreateDumb { width: 64, height: 64, bpp: 32, ..Default::default() };
    if ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut c as *mut _) != 0 {
        report_skip(b"TOKENSCOPE (no CREATE_DUMB)");
        return (0, 1);
    }
    let n = 64 * 64 * 4;
    let mut m = DrmModeMapDumb { handle: c.handle, ..Default::default() };
    let p = if ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut m as *mut _) == 0 {
        mmap(core::ptr::null_mut(), n as size_t, PROT_READ | PROT_WRITE, MAP_SHARED, fd, m.offset as i64)
    } else { -1isize as *mut c_void };
    let mut ph = DrmPrimeHandle::default();
    ph.handle = c.handle;
    let mut fds = [0 as c_int; 2];
    if p as isize == -1 || ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut ph as *mut _) != 0 || ph.fd < 0
        || pipe(fds.as_mut_ptr()) != 0 {
        report(b"TOKENSCOPE_SETUP", false);
        return (1, 0);
    }
    *(p as *mut u32) = secret;
    let child = fork();
    if child == 0 {
        close(fds[1]);
        let mut tok: u64 = 0;
        if read(fds[0], &mut tok as *mut u64 as *mut c_void, 8) != 8 { _exit(4); }
        let mut b = open(b"/dev/dri/renderD128\0".as_ptr(), O_RDWR);
        if b < 0 { b = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR); }
        if b < 0 { _exit(4); }
        let mut rc = 0;
        // B holds no handle on A's BO: the token must not map.
        let q = mmap(core::ptr::null_mut(), n as size_t, PROT_READ, MAP_SHARED, b, tok as i64);
        if q as isize != -1 {
            if *(q as *const u32) == secret { puts(b"drmsmoke: tokenscope: foreign open READ the secret\n\0".as_ptr()); }
            rc |= 1;
        }
        // After a PRIME import B holds its own handle: map through it.
        let mut pi = DrmPrimeHandle::default();
        pi.fd = ph.fd;
        let mut mi = DrmModeMapDumb::default();
        let ok = ioctl(b, DRM_IOCTL_PRIME_FD_TO_HANDLE, &mut pi as *mut _) == 0 && {
            mi.handle = pi.handle;
            ioctl(b, DRM_IOCTL_MODE_MAP_DUMB, &mut mi as *mut _) == 0
        } && {
            let r = mmap(core::ptr::null_mut(), n as size_t, PROT_READ, MAP_SHARED, b, mi.offset as i64);
            r as isize != -1 && *(r as *const u32) == secret
        };
        if !ok { rc |= 2; }
        _exit(rc);
    }
    close(fds[0]);
    let tok = m.offset;
    write(fds[1], &tok as *const u64 as *const c_void, 8);
    close(fds[1]);
    let mut st: c_int = 0;
    let rc = if child > 0 && waitpid(child, &mut st, 0) == child && st & 0x7f == 0 { (st >> 8) & 0xff } else { 8 };
    if !report(b"TOKENSCOPE_FOREIGN_OPEN_REFUSED", rc & (1 | 4 | 8) == 0) { fails += 1; }
    if !report(b"TOKENSCOPE_PRIME_IMPORT_MAPS", rc & (2 | 4 | 8) == 0) { fails += 1; }
    close(ph.fd);
    let mut h = c.handle;
    ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut h as *mut u32);
    (fails, 0)
}

const FAKE_OFF_BASE: u64 = 1 << 44;
const FAKE_OFF_END: u64 = 1 << 45;
fn opaque(off: u64) -> bool { off >= FAKE_OFF_BASE && off < FAKE_OFF_END && off & 0xFFF == 0 }
const MAP_FAILED_P: isize = -1;
const DRM_IOCTL_SET_MASTER_T: c_ulong = 0x641E;
const DRM_IOCTL_DROP_MASTER_T: c_ulong = 0x641F;
const DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB: c_ulong = 0xC030644A;
const DRM_IOCTL_VIRTGPU_CONTEXT_INIT: c_ulong = 0xC010644B;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VgGetParam { param: u64, value: u64 }
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VgCtxSetParam { param: u64, value: u64 }
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VgCtxInit { num_params: u32, pad: u32, ctx_set_params: u64 }
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct VgCreateBlob {
    blob_mem: u32, blob_flags: u32, bo_handle: u32, res_handle: u32,
    size: u64, pad: u32, cmd_size: u32, cmd: u64, blob_id: u64,
}

unsafe fn map_at(fd: c_int, len: usize, off: u64) -> *mut c_void {
    mmap(core::ptr::null_mut(), len as size_t, PROT_READ | PROT_WRITE, MAP_SHARED, fd, off as i64)
}

unsafe fn fakeoff_cases(fd: c_int) -> i32 {
    let mut fails = 0;
    let mut c1 = DrmModeCreateDumb { width: 64, height: 64, bpp: 32, ..Default::default() };
    let mut c2 = DrmModeCreateDumb { width: 64, height: 64, bpp: 32, ..Default::default() };
    if ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut c1 as *mut _) != 0
        || ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut c2 as *mut _) != 0 {
        report(b"FAKEOFF_SETUP", false);
        return 1;
    }
    let n = (c1.size as usize).max(64 * 64 * 4);
    let mut m1 = DrmModeMapDumb { handle: c1.handle, ..Default::default() };
    let r1 = ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut m1 as *mut _);
    if !report(b"FAKEOFF_DUMB_OPAQUE", r1 == 0 && opaque(m1.offset)) { fails += 1; }
    print_dec(b"  offset=", m1.offset);
    let p = if r1 == 0 { map_at(fd, n, m1.offset) } else { MAP_FAILED_P as *mut c_void };
    let mut ok = p as isize != MAP_FAILED_P;
    if ok {
        let w = p as *mut u32;
        for i in 0..n / 4 { core::ptr::write_volatile(w.add(i), (i as u32).wrapping_mul(0x9E37_79B1) ^ 0xA5); }
        for i in 0..n / 4 {
            if core::ptr::read_volatile(w.add(i)) != (i as u32).wrapping_mul(0x9E37_79B1) ^ 0xA5 { ok = false; break; }
        }
        munmap(p, n as size_t);
    }
    if !report(b"FAKEOFF_DUMB_MAPS", ok) { fails += 1; }
    let mut m1b = DrmModeMapDumb { handle: c1.handle, ..Default::default() };
    let rb = ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut m1b as *mut _);
    if !report(b"FAKEOFF_STABLE", rb == 0 && m1b.offset == m1.offset) { fails += 1; }
    let mut m2 = DrmModeMapDumb { handle: c2.handle, ..Default::default() };
    let r2 = ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut m2 as *mut _);
    let n2 = (c2.size as u64).max(64 * 64 * 4);
    let disjoint = m2.offset + n2 <= m1.offset || m1.offset + n as u64 <= m2.offset;
    if !report(b"FAKEOFF_DISTINCT", r2 == 0 && opaque(m2.offset) && m2.offset != m1.offset && disjoint) { fails += 1; }
    let mut h1 = c1.handle;
    let mut h2 = c2.handle;
    ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut h1 as *mut u32);
    ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut h2 as *mut u32);
    let s = map_at(fd, n, m1.offset);
    let refused = s as isize == MAP_FAILED_P;
    if !refused { munmap(s, n as size_t); }
    if !report(b"FAKEOFF_STALE_REFUSED", refused) { fails += 1; }
    fails
}

unsafe fn fbtoken_cases(fd: c_int) -> i32 {
    let mut fails = 0;
    let rfd = open(b"/dev/dri/renderD128\0".as_ptr(), O_RDWR);
    if rfd < 0 {
        puts(b"FBTOKEN_RENDER_REFUSED: SKIP (no renderD128)\n\0".as_ptr());
    } else {
        let q = map_at(rfd, 4096, 0);
        let refused = q as isize == MAP_FAILED_P;
        if !refused { munmap(q, 4096); }
        if !report(b"FBTOKEN_RENDER_REFUSED", refused) { fails += 1; }
        close(rfd);
    }
    let rc = ioctl(fd, DRM_IOCTL_SET_MASTER_T, 0usize);
    print_dec(b"  SET_MASTER rc=", rc as i64 as u64);
    let child = fork();
    if child == 0 {
        let b = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
        if b < 0 { _exit(2); }
        let q = map_at(b, 4096, 0);
        _exit(if q as isize == MAP_FAILED_P { 0 } else { 1 });
    }
    let mut st: c_int = 0;
    let crc = if child > 0 && waitpid(child, &mut st, 0) == child && st & 0x7f == 0 { (st >> 8) & 0xff } else { 8 };
    if !report(b"FBTOKEN_NONMASTER_REFUSED", crc == 0) { fails += 1; }
    let q = map_at(fd, 4096, 0);
    if q as isize == MAP_FAILED_P {
        puts(b"FBTOKEN_MASTER_MAPS: SKIP (no firmware fb)\n\0".as_ptr());
    } else {
        munmap(q, 4096);
        report(b"FBTOKEN_MASTER_MAPS", true);
    }
    ioctl(fd, DRM_IOCTL_DROP_MASTER_T, 0usize);
    fails
}

unsafe fn hostvis_cases(fd: c_int) -> i32 {
    const SIZE: usize = 65536;
    let mut fails = 0;
    let mut gp = [0u64; 2];
    for (i, pr) in [3u64, 4u64].iter().enumerate() {
        let mut g = VgGetParam { param: *pr, value: &mut gp[i] as *mut u64 as u64 };
        if ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &mut g as *mut _) != 0 { gp[i] = 0; }
    }
    if gp[0] == 0 || gp[1] == 0 {
        puts(b"HOSTVIS: SKIP (RESOURCE_BLOB/HOST_VISIBLE unsupported)\n\0".as_ptr());
        return 0;
    }
    let mut sp = [VgCtxSetParam { param: 1, value: 2 }];
    let mut ci = VgCtxInit { num_params: 1, pad: 0, ctx_set_params: sp.as_mut_ptr() as u64 };
    let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_CONTEXT_INIT, &mut ci as *mut _);
    print_dec(b"  CONTEXT_INIT rc=", rc as i64 as u64);
    // VIRGL_CCMD_PIPE_RESOURCE_CREATE: target PIPE_BUFFER, format R8_UNORM,
    // bind VERTEX_BUFFER, flags MAP_PERSISTENT|MAP_COHERENT, blob_id 1. The
    // bind must give the buffer GL storage: vrend leaves a VIRGL_BIND_STAGING
    // (1 << 19) buffer guest-memory-only, so its HOST3D blob creates fine but
    // RESOURCE_MAP_BLOB has nothing to glMapBufferRange and fails (0x1200).
    let cmd: [u32; 12] = [48 | (11 << 16), 0, 64, 1 << 4, SIZE as u32, 1, 1, 1, 0, 0, (1 << 1) | (1 << 2), 1];
    let mut b = VgCreateBlob {
        blob_mem: 2, blob_flags: 1, size: SIZE as u64, cmd_size: 48,
        cmd: cmd.as_ptr() as u64, blob_id: 1, ..Default::default()
    };
    let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB, &mut b as *mut _);
    if rc != 0 { print_dec(b"  create_blob rc=", rc as i64 as u64); }
    if !report(b"HOSTVIS_CREATE", rc == 0 && b.bo_handle != 0) { return fails + 1; }
    let mut mp = VirtgpuMap { handle: b.bo_handle, ..Default::default() };
    let mrc = ioctl(fd, DRM_IOCTL_VIRTGPU_MAP, &mut mp as *mut _);
    if !report(b"HOSTVIS_MAP_OPAQUE", mrc == 0 && opaque(mp.offset)) { fails += 1; }
    let off = mp.offset;
    let pat = |i: usize| (i as u32).wrapping_mul(0x9E37_79B1);
    let p = map_at(fd, SIZE, off);
    let mut ok = p as isize != MAP_FAILED_P;
    if ok {
        let w = p as *mut u32;
        for i in 0..SIZE / 4 { core::ptr::write_volatile(w.add(i), pat(i)); }
        munmap(p, SIZE as size_t);
        let p2 = map_at(fd, SIZE, off);
        if p2 as isize == MAP_FAILED_P { ok = false; } else {
            let w = p2 as *mut u32;
            for i in 0..SIZE / 4 {
                if core::ptr::read_volatile(w.add(i)) != pat(i) { ok = false; break; }
            }
            munmap(p2, SIZE as size_t);
        }
    }
    if !report(b"HOSTVIS_READBACK", ok) { fails += 1; }
    let q = map_at(fd, 4096, off + 4096);
    let ok = q as isize != MAP_FAILED_P && core::ptr::read_volatile(q as *const u32) == pat(1024);
    if q as isize != MAP_FAILED_P { munmap(q, 4096); }
    if !report(b"HOSTVIS_PARTIAL", ok) { fails += 1; }
    let mut h = [b.bo_handle, 0u32];
    ioctl(fd, DRM_IOCTL_GEM_CLOSE_FF, h.as_mut_ptr());
    let s = map_at(fd, 4096, off);
    let refused = s as isize == MAP_FAILED_P;
    if !refused { munmap(s, 4096); }
    if !report(b"HOSTVIS_STALE_REFUSED", refused) { fails += 1; }
    fails
}

unsafe fn maplife_cases(fd: c_int) -> (i32, i32) {
    let pat = |i: usize| ((i as u32).wrapping_mul(2654435761).rotate_right(11) as u8) | 1;
    let mut fails = 0;
    let mut skips = 0;
    // Dumb buffer, 256x256x32 = 256 KiB contiguous.
    let dumb = || -> Option<(u32, u64)> {
        let mut c = DrmModeCreateDumb { width: 256, height: 256, bpp: 32, ..Default::default() };
        if ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut c as *mut _) != 0 { return None; }
        let mut m = DrmModeMapDumb { handle: c.handle, ..Default::default() };
        if ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut m as *mut _) != 0 { return None; }
        Some((c.handle, m.offset))
    };
    let n = 256 * 256 * 4;
    match dumb() {
        None => { report_skip(b"MAPLIFE dumb (no CREATE_DUMB)"); skips += 1; }
        Some((h, tok)) => {
            let m = mmap(core::ptr::null_mut(), n as size_t, PROT_READ | PROT_WRITE, MAP_SHARED, fd, tok as i64);
            if !report(b"MAPLIFE_DUMB_MMAP", m as isize != -1) { fails += 1; } else {
                let base = m as *mut u8;
                for i in 0..n { *base.add(i) = pat(i); }
                let mut c = [h, 0u32];
                ioctl(fd, DRM_IOCTL_GEM_CLOSE_FF, &mut c as *mut _);
                fails += maplife_check(fd, b"drmsmoke: maplife dumb\0", base, n, tok, dumb);
            }
        }
    }
    // Scattered virgl resource (frame-list backing forced by the knob).
    const SIZE: u32 = (2 << 20) + 3 * 4096;
    let Some(was) = gp_in(fd, VIRTGPU_PARAM_LEANDROS_SCATTER, 3) else {
        report_skip(b"MAPLIFE scatter (no scatter knob)");
        return (fails, skips + 1);
    };
    let sg = || -> Option<(u32, u64)> {
        let mut rc = VirtgpuResourceCreate {
            target: 0, format: 64, bind: 1 << 4,
            width: SIZE, height: 1, depth: 1, array_size: 1, size: SIZE,
            ..Default::default()
        };
        if ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE, &mut rc as *mut _) != 0 || rc.bo_handle == 0 { return None; }
        let mut mp = VirtgpuMap { handle: rc.bo_handle, ..Default::default() };
        if ioctl(fd, DRM_IOCTL_VIRTGPU_MAP, &mut mp as *mut _) != 0 { return None; }
        Some((rc.bo_handle, mp.offset))
    };
    let n = SIZE as usize;
    match sg() {
        None => { report_skip(b"MAPLIFE scatter (no virgl RESOURCE_CREATE)"); skips += 1; }
        Some((h, tok)) => {
            let m = mmap(core::ptr::null_mut(), n as size_t, PROT_READ | PROT_WRITE, MAP_SHARED, fd, tok as i64);
            if !report(b"MAPLIFE_SCATTER_MMAP", m as isize != -1) { fails += 1; } else {
                let base = m as *mut u8;
                for i in 0..n { *base.add(i) = pat(i); }
                let mut c = [h, 0u32];
                ioctl(fd, DRM_IOCTL_GEM_CLOSE_FF, &mut c as *mut _);
                fails += maplife_check(fd, b"drmsmoke: maplife scatter\0", base, n, tok, sg);
            }
        }
    }
    if was == 0 { gp_in(fd, VIRTGPU_PARAM_LEANDROS_SCATTER, 4); }
    (fails, skips)
}

unsafe fn scatter_cases(fd: c_int) -> (i32, i32) {
    const SIZE: u32 = (2 << 20) + 3 * 4096;
    let Some(was) = gp_in(fd, VIRTGPU_PARAM_LEANDROS_SCATTER, 3) else {
        report_skip(b"SCATTER_MMAP_ROUNDTRIP (no scatter knob)");
        return (0, 1);
    };
    let before = gp_in(fd, VIRTGPU_PARAM_LEANDROS_SCATTER, 0).unwrap_or(0);
    let mut rc = VirtgpuResourceCreate {
        target: 0, format: 64, bind: 1 << 4,
        width: SIZE, height: 1, depth: 1, array_size: 1, size: SIZE,
        ..Default::default()
    };
    let created = ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE, &mut rc as *mut _) == 0 && rc.bo_handle != 0;
    if was == 0 { gp_in(fd, VIRTGPU_PARAM_LEANDROS_SCATTER, 4); }
    if !created {
        report_skip(b"SCATTER_MMAP_ROUNDTRIP (no virgl RESOURCE_CREATE)");
        return (0, 1);
    }
    let after = gp_in(fd, VIRTGPU_PARAM_LEANDROS_SCATTER, 0).unwrap_or(0);
    print_dec(b"drmsmoke: scatter_creates=", after as u64);
    let mut failures = 0;
    if !report(b"SCATTER_FORCED (counter moved)", after == before + 1) { failures += 1; }
    let n = SIZE as usize;
    let pat = |i: usize| ((i as u32).wrapping_mul(2246822519).rotate_right(7) as u8) | 1;
    let mut mp = VirtgpuMap { handle: rc.bo_handle, ..Default::default() };
    let map_rc = ioctl(fd, DRM_IOCTL_VIRTGPU_MAP, &mut mp as *mut _);
    let mut base: *mut u8 = core::ptr::null_mut();
    if map_rc == 0 {
        let m = mmap(core::ptr::null_mut(), n as size_t, PROT_READ | PROT_WRITE, MAP_SHARED, fd, mp.offset as i64);
        if m as isize != -1 { base = m as *mut u8; }
    } else {
        print_dec(b"  VIRTGPU_MAP errno=", errno() as u64);
    }
    if !report(b"SCATTER_MMAP", !base.is_null()) {
        failures += 1;
    } else {
        for i in 0..n { *base.add(i) = pat(i); }
        let mut t = VirtgpuTransfer { bo_handle: rc.bo_handle, w: SIZE, h: 1, d: 1, ..Default::default() };
        let up = ioctl(fd, DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST, &mut t as *mut _) == 0;
        let mut w = VirtgpuWait { handle: rc.bo_handle, flags: 0 };
        let w1 = ioctl(fd, DRM_IOCTL_VIRTGPU_WAIT, &mut w as *mut _) == 0;
        for i in 0..n { *base.add(i) = 0; }
        let mut t2 = VirtgpuTransfer { bo_handle: rc.bo_handle, w: SIZE, h: 1, d: 1, ..Default::default() };
        let down = ioctl(fd, DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST, &mut t2 as *mut _) == 0;
        let w2 = ioctl(fd, DRM_IOCTL_VIRTGPU_WAIT, &mut w as *mut _) == 0;
        let mut bad = 0u64;
        for i in 0..n { if *base.add(i) != pat(i) { bad += 1; } }
        if bad != 0 { print_dec(b"  mismatched bytes=", bad); }
        if !report(b"SCATTER_MMAP_ROUNDTRIP", up && w1 && down && w2 && bad == 0) { failures += 1; }
        // A second mapping of the same token aliases the same frames.
        let m2 = mmap(core::ptr::null_mut(), n as size_t, PROT_READ, MAP_SHARED, fd, mp.offset as i64);
        let mut bad2 = u64::MAX;
        if m2 as isize != -1 {
            *base.add(n - 1) = 0x5A;
            bad2 = 0;
            for i in 0..n - 1 { if *(m2 as *const u8).add(i) != pat(i) { bad2 += 1; } }
            if *(m2 as *const u8).add(n - 1) != 0x5A { bad2 += 1; }
            *base.add(n - 1) = pat(n - 1);
            munmap(m2, n as size_t);
        }
        if !report(b"SCATTER_SECOND_MMAP", bad2 == 0) { failures += 1; }
        // PRIME export: the dma-buf maps the same frames.
        let mut ph = DrmPrimeHandle { handle: rc.bo_handle, flags: 0, fd: -1 };
        let mut bad3 = u64::MAX;
        if ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut ph as *mut _) == 0 && ph.fd >= 0 {
            let m3 = mmap(core::ptr::null_mut(), n as size_t, PROT_READ, MAP_SHARED, ph.fd, 0);
            if m3 as isize != -1 {
                bad3 = 0;
                for i in 0..n { if *(m3 as *const u8).add(i) != pat(i) { bad3 += 1; } }
                munmap(m3, n as size_t);
            }
            close(ph.fd);
        }
        if !report(b"SCATTER_PRIME_MMAP", bad3 == 0) { failures += 1; }
        munmap(base as *mut c_void, n as size_t);
    }
    let mut gc = [rc.bo_handle, 0u32];
    ioctl(fd, DRM_IOCTL_GEM_CLOSE_FF, gc.as_mut_ptr());
    (failures, 0)
}

// ── Connector enumeration (lane lapdisp) ─────────────────────────────────────
// Every connector GETRESOURCES lists must answer GETCONNECTOR, and every
// encoder GETENCODER. With `verbose` one line per connector:
// "connector <id> <TYPE>-<n> <status> modes=<n> [<w>x<h>@<hz>] encoder=<id> possible_crtcs=0x<m>".
const DRM_IOCTL_MODE_GETENCODER_L: c_ulong = 0xC01464A6;
#[repr(C)]
#[derive(Default)]
struct DrmModeGetEncoderL { encoder_id: u32, encoder_type: u32, crtc_id: u32, possible_crtcs: u32, possible_clones: u32 }

struct LineBuf { b: [u8; 256], n: usize }
impl LineBuf {
    fn s(&mut self, t: &[u8]) { for &c in t { if self.n < 254 { self.b[self.n] = c; self.n += 1; } } }
    fn d(&mut self, v: u64) {
        let mut t = [0u8; 20]; let mut k = 0; let mut x = v;
        loop { t[k] = b'0' + (x % 10) as u8; k += 1; x /= 10; if x == 0 { break; } }
        while k > 0 { k -= 1; self.s(&[t[k]]); }
    }
    fn x(&mut self, v: u32) { for i in (0..8).rev() { let n = ((v >> (4 * i)) & 0xf) as u8; self.s(&[if n < 10 { b'0' + n } else { b'a' + n - 10 }]); } }
    unsafe fn put(&mut self) { self.b[self.n] = 0; puts(self.b.as_ptr()); self.n = 0; }
}

unsafe fn enum_connectors(fd: c_int, verbose: bool) -> bool {
    let mut res = DrmModeCardRes::default();
    if ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut res as *mut _) != 0 { return false; }
    let mut ids = [0u32; 16];
    let mut encs = [0u32; 16];
    let (nc, ne) = ((res.count_connectors as usize).min(16), (res.count_encoders as usize).min(16));
    let mut r2 = DrmModeCardRes::default();
    r2.connector_id_ptr = ids.as_mut_ptr() as u64; r2.count_connectors = nc as u32;
    r2.encoder_id_ptr = encs.as_mut_ptr() as u64; r2.count_encoders = ne as u32;
    if ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut r2 as *mut _) != 0 { return false; }
    let mut ok = true;
    for &id in &ids[..nc] {
        let mut modes: [DrmModeModeinfo; 4] = core::array::from_fn(|_| DrmModeModeinfo::default());
        let mut encid = [0u32; 4];
        let mut c = DrmModeGetConnector::default();
        c.connector_id = id;
        c.modes_ptr = modes.as_mut_ptr() as u64; c.count_modes = 4;
        c.encoders_ptr = encid.as_mut_ptr() as u64; c.count_encoders = 4;
        if ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &mut c as *mut _) != 0 || c.connection == 0 || c.connection > 3 { ok = false; continue; }
        let mut e = DrmModeGetEncoderL { encoder_id: encid[0], ..Default::default() };
        let eok = c.count_encoders == 0 || ioctl(fd, DRM_IOCTL_MODE_GETENCODER_L, &mut e as *mut _) == 0;
        if !eok { ok = false; }
        if verbose {
            let mut l = LineBuf { b: [0; 256], n: 0 };
            l.s(b"connector "); l.d(id as u64); l.s(b" ");
            l.s(match c.connector_type { 10 => b"DP" as &[u8], 11 => b"HDMI-A", 14 => b"eDP", 15 => b"Virtual", _ => b"type?" });
            l.s(b"-"); l.d(c.connector_type_id as u64);
            l.s(match c.connection { 1 => b" connected" as &[u8], 2 => b" disconnected", _ => b" unknown" });
            l.s(b" modes="); l.d(c.count_modes as u64);
            if c.count_modes > 0 { l.s(b" "); l.d(modes[0].hdisplay as u64); l.s(b"x"); l.d(modes[0].vdisplay as u64); l.s(b"@"); l.d(modes[0].vrefresh as u64); }
            l.s(b" mm="); l.d(c.mm_width as u64); l.s(b"x"); l.d(c.mm_height as u64);
            l.s(b" encoder="); l.d(encid[0] as u64); l.s(b" possible_crtcs=0x"); l.x(e.possible_crtcs);
            l.s(if eok { b"" as &[u8] } else { b" GETENCODER-FAILED" });
            l.put();
        }
    }
    ok
}


// ── lane lapdisp C1: two-CRTC DRM core against the stub pipe B ─────────────
// Needs `i915.fake=2 i915.kms=1 i915.pipeb_stub=1` (QEMU). Object ids are the
// driver's: CRTC 2, planes 32/33, extra connector 0x100, extra encoder 0x180.
const DRM_IOCTL_MODE_GETPLANE_C2: c_ulong = 0xC02064B6;
const DRM_IOCTL_MODE_CREATEPROPBLOB_C2: c_ulong = 0xC01064BD;
const C2_PROP_ACTIVE: u32 = 60;
const C2_PROP_MODE_ID: u32 = 61;
const C2_PROP_CONN_CRTC_ID: u32 = 70;
const C2_ALLOW_MODESET: u32 = 0x0400;

#[repr(C)]
#[derive(Default)]
struct C2GetPlane { plane_id: u32, crtc_id: u32, fb_id: u32, possible_crtcs: u32, gamma_size: u32, count_fmt: u32, fmt_ptr: u64 }
#[repr(C)]
#[derive(Default)]
struct C2Blob { data: u64, length: u32, blob_id: u32 }

unsafe fn c2_atomic(fd: c_int, flags: u32, objs: &[u32], counts: &[u32], props: &[u32], vals: &[u64]) -> c_int {
    let mut req = DrmModeAtomic::default();
    req.flags = flags;
    req.count_objs = objs.len() as u32;
    req.objs_ptr = objs.as_ptr() as u64;
    req.count_props_ptr = counts.as_ptr() as u64;
    req.props_ptr = props.as_ptr() as u64;
    req.prop_values_ptr = vals.as_ptr() as u64;
    ioctl(fd, DRM_IOCTL_MODE_ATOMIC, &mut req as *mut _)
}

/// One FLIP_COMPLETE event's crtc_id, or 0 on timeout.
unsafe fn c2_event_crtc(fd: c_int) -> u32 {
    let mut pfd = pollfd { fd, events: POLLIN, revents: 0 };
    if poll(&mut pfd as *mut _, 1, 2000) <= 0 { return 0; }
    let mut b = [0u8; 32];
    if read(fd, b.as_mut_ptr() as *mut c_void, 32) != 32 { return 0; }
    if u32::from_le_bytes([b[0], b[1], b[2], b[3]]) != DRM_EVENT_FLIP_COMPLETE { return 0; }
    u32::from_le_bytes([b[28], b[29], b[30], b[31]])
}

unsafe fn crtc2_mode() -> i32 {
    let mut failures = 0i32;
    let fd = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
    if fd < 0 { puts(b"crtc2: open failed\n\0".as_ptr()); return 1; }

    // CRTC_ENUM
    let mut res = DrmModeCardRes::default();
    let mut ids = [0u32; 4];
    let ok0 = ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut res as *mut _) == 0 && res.count_crtcs == 2;
    let mut r2 = DrmModeCardRes::default();
    r2.crtc_id_ptr = ids.as_mut_ptr() as u64; r2.count_crtcs = 2;
    let ok1 = ok0 && ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut r2 as *mut _) == 0 && ids[0] == 1 && ids[1] == 2;
    let mut pl = |id: u32| -> u32 {
        let mut g = C2GetPlane { plane_id: id, ..Default::default() };
        if ioctl(fd, DRM_IOCTL_MODE_GETPLANE_C2, &mut g as *mut _) != 0 { 0xFF } else { g.possible_crtcs }
    };
    let (pa, pb) = (pl(30), pl(32));
    let mut e = DrmModeGetEncoderL { encoder_id: 0x180, ..Default::default() };
    let eok = ioctl(fd, DRM_IOCTL_MODE_GETENCODER_L, &mut e as *mut _) == 0 && e.possible_crtcs == 2;
    let mut e1 = DrmModeGetEncoderL { encoder_id: 1, ..Default::default() };
    let e1ok = ioctl(fd, DRM_IOCTL_MODE_GETENCODER_L, &mut e1 as *mut _) == 0 && e1.possible_crtcs == 1;
    if !report(b"CRTC_ENUM", ok1 && pa == 1 && pb == 2 && eok && e1ok) { failures += 1; }

    // Mode blob 1280x720@60.
    let mut mi = DrmModeModeinfo::default();
    mi.clock = 74250; mi.hdisplay = 1280; mi.hsync_start = 1390; mi.hsync_end = 1430; mi.htotal = 1650;
    mi.vdisplay = 720; mi.vsync_start = 725; mi.vsync_end = 730; mi.vtotal = 750; mi.vrefresh = 60;
    let mut blob = C2Blob { data: &mi as *const _ as u64, length: core::mem::size_of::<DrmModeModeinfo>() as u32, blob_id: 0 };
    let blob_ok = ioctl(fd, DRM_IOCTL_MODE_CREATEPROPBLOB_C2, &mut blob as *mut _) == 0;
    let (h0, fb0) = dumb_fb(fd, 1280, 720);
    let (h1, fb1) = dumb_fb(fd, 1280, 720);
    let (_ha, fba) = dumb_fb(fd, 1280, 720);
    let _ = (h0, h1);

    // ATOMIC_CRTC2: connector 0x100 + CRTC 2 + plane 32.
    let objs = [0x100u32, 2, 32];
    let counts = [1u32, 2, 10];
    let mut props = [0u32; 13];
    props[0] = C2_PROP_CONN_CRTC_ID; props[1] = C2_PROP_ACTIVE; props[2] = C2_PROP_MODE_ID;
    let pp = [PROP_PLANE_CRTC_ID, PROP_FB_ID, PROP_SRC_X, PROP_SRC_Y, PROP_SRC_W, PROP_SRC_H, PROP_CRTC_X, PROP_CRTC_Y, PROP_CRTC_W, PROP_CRTC_H];
    for k in 0..10 { props[3 + k] = pp[k]; }
    let vals: [u64; 13] = [2, 1, blob.blob_id as u64, 2, fb0 as u64, 0, 0, 1280 << 16, 720 << 16, 0, 0, 1280, 720];
    let t_ok = blob_ok && fb0 != 0 && c2_atomic(fd, C2_ALLOW_MODESET | DRM_MODE_ATOMIC_TEST_ONLY, &objs, &counts, &props, &vals) == 0;
    let c_ok = t_ok && c2_atomic(fd, C2_ALLOW_MODESET | DRM_MODE_PAGE_FLIP_EVENT, &objs, &counts, &props, &vals) == 0
        && c2_event_crtc(fd) == 2;
    // 10 flips on B with concurrent flips on A; every event carries its own CRTC.
    let (mut nb, mut na) = (0, 0);
    if c_ok {
        for i in 0..10 {
            let fb = if i & 1 == 0 { fb1 } else { fb0 };
            let o = [32u32]; let c = [10u32];
            let v: [u64; 10] = [2, fb as u64, 0, 0, 1280 << 16, 720 << 16, 0, 0, 1280, 720];
            let rb = c2_atomic(fd, DRM_MODE_PAGE_FLIP_EVENT, &o, &c, &pp, &v);
            let ra = atomic_plane_commit(fd, fba, 1280, 720, DRM_MODE_PAGE_FLIP_EVENT);
            if rb != 0 { break; }
            let mut want = 1 + (ra == 0) as u32;
            while want > 0 {
                match c2_event_crtc(fd) { 2 => nb += 1, 1 => na += 1, _ => break }
                want -= 1;
            }
        }
    }
    if !report(b"ATOMIC_CRTC2", c_ok && nb == 10 && na >= 1) { failures += 1; }

    // ATOMIC_BAD: eDP connector 1 -> CRTC 2 is refused; so is a second extra on CRTC 2.
    let o = [1u32]; let c = [1u32]; let p = [C2_PROP_CONN_CRTC_ID]; let v = [2u64];
    let bad1 = c2_atomic(fd, C2_ALLOW_MODESET | DRM_MODE_ATOMIC_TEST_ONLY, &o, &c, &p, &v) != 0 && errno() == EINVAL;
    let o2 = [0x101u32]; 
    let bad2 = c2_atomic(fd, C2_ALLOW_MODESET | DRM_MODE_ATOMIC_TEST_ONLY, &o2, &c, &p, &v) != 0 && errno() == EINVAL;
    if !report(b"ATOMIC_BAD", bad1 && bad2) { failures += 1; }

    // Tear down B.
    let o = [2u32, 0x100]; let c = [2u32, 1]; let p = [C2_PROP_ACTIVE, C2_PROP_MODE_ID, C2_PROP_CONN_CRTC_ID]; let v = [0u64, 0, 0];
    let off = c2_atomic(fd, C2_ALLOW_MODESET, &o, &c, &p, &v) == 0;
    if !report(b"ATOMIC_CRTC2_OFF", off) { failures += 1; }
    puts(b"--- drmsmoke crtc2 done ---\n\0".as_ptr());
    failures
}

#[no_mangle]
pub unsafe extern "C" fn main(argc_c: i32, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let argc = argc_c as isize;
    let mut failures = 0i32;
    let mut skips = 0i32;

    let hold_mode = argc > 1 && arg_is(*argv.add(1) as *const u8, b"--hold");

    if argc > 1 && arg_is(*argv.add(1) as *const u8, b"--crtc2") {
        return crtc2_mode();
    }
    if argc > 1 && arg_is(*argv.add(1) as *const u8, b"--list") {
        let fd = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
        let ok = fd >= 0 && enum_connectors(fd, true);
        report(b"CONNECTOR_ENUM", ok);
        puts(b"--- drmsmoke done ---\n\0".as_ptr());
        return if ok { 0 } else { 1 };
    }

    if argc > 1 && arg_is(*argv.add(1) as *const u8, b"--hotplug") {
        // `--hotplug [--fake]`: wait up to 60 s for a DRM `change` record on
        // /dev/input/hotplug, then list the connectors. `--fake` fires it with the
        // root test ioctl 0x100B (DRM_DEBUG_FAKE_HPD) so no monitor is needed.
        let fake = argc > 2 && arg_is(*argv.add(2) as *const u8, b"--fake");
        const O_NONBLOCK: c_int = 0o4000;
        let hp = open(b"/dev/input/hotplug\0".as_ptr(), O_RDONLY | O_NONBLOCK);
        let fd = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
        let mut ok = hp >= 0 && fd >= 0;
        let mut buf = [0u8; 96];
        if ok {
            // Drain the input coldplug records so only later events count.
            while read(hp, buf.as_mut_ptr() as *mut c_void, buf.len()) > 0 {}
            if fake { ok = ioctl(fd, 0x100B as c_ulong, 0usize) == 0; if !ok { puts(b"drmsmoke: fake HPD ioctl 0x100B failed\0".as_ptr()); } }
        } else {
            if hp < 0 { puts(b"drmsmoke: cannot open /dev/input/hotplug\0".as_ptr()); }
            if fd < 0 { puts(b"drmsmoke: cannot open /dev/dri/card0\0".as_ptr()); }
        }
        let mut got = false;
        let mut waited = 0;
        while ok && !got && waited < 60_000 {
            let mut p = pollfd { fd: hp, events: POLLIN, revents: 0 };
            poll(&mut p, 1, 1000);
            waited += 1000;
            loop {
                let r = read(hp, buf.as_mut_ptr() as *mut c_void, buf.len() - 1);
                if r <= 0 { break; }
                if r >= 18 && &buf[..11] == b"change card" && &buf[12..16] == b" drm" { got = true; }
            }
        }
        report(b"HOTPLUG_DRM_RECORD", ok && got);
        let listed = got && enum_connectors(fd, true);
        report(b"CONNECTOR_ENUM", listed);
        puts(b"--- drmsmoke done ---\0".as_ptr());
        return if got && listed { 0 } else { 1 };
    }

    if argc > 1 && arg_is(*argv.add(1) as *const u8, b"--leak") {
        let mut iters = 20u32;
        if argc > 2 {
            let p = *argv.add(2) as *const u8;
            let mut v = 0u32; let mut k = 0usize;
            while !p.is_null() && (*p.add(k)).is_ascii_digit() { v = v * 10 + (*p.add(k) - b'0') as u32; k += 1; }
            if v > 0 { iters = v; }
        }
        let f = leak_mode(iters);
        puts(b"--- drmsmoke done ---\n\0".as_ptr());
        return f;
    }

    // `--fence-loss-demo`: the out-fence cases with a CREATE_OBJECT in the
    // refused submission and its BIND in the next one, so QEMU's stderr shows
    // what a refused EXECBUFFER costs Mesa (see `out_fence_cases`).
    // `--fb-stats`: print the framebuffer-lifetime counters and live BO
    // counts, for session harnesses (greeter handoff, terminal churn).
    if argc > 1 && arg_is(*argv.add(1) as *const u8, b"--fb-stats") {
        let f = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
        if f < 0 { return 1; }
        let st = gp(f, VIRTGPU_PARAM_LEANDROS_FB_STATS).unwrap_or(u32::MAX);
        print_dec(b"drmsmoke: fb_scanout_disables=", (st & 0xFFFF) as u64);
        print_dec(b"drmsmoke: fb_swept_on_close=", (st >> 16) as u64);
        print_dec(b"drmsmoke: dumb_objs=", gp(f, VIRTGPU_PARAM_LEANDROS_DUMB_OBJS).unwrap_or(u32::MAX) as u64);
        print_dec(b"drmsmoke: blob_objs=", gp(f, 0x1000_0005).unwrap_or(u32::MAX) as u64);
        close(f);
        return 0;
    }
    // `--force-scatter`: every virgl RESOURCE_CREATE of this run gets a
    // scattered backing (runtime knob; restored at the end), then the normal
    // suite runs. The guard for L1.10 is the whole suite passing like this.
    let force_scatter = argc > 1 && arg_is(*argv.add(1) as *const u8, b"--force-scatter");
    // `--scatter`: the virgl transfer cases and the scatter cases with every
    // RESOURCE_CREATE forced onto a scattered backing (L1.10 guard).
    if argc > 1 && arg_is(*argv.add(1) as *const u8, b"--scatter") {
        let f = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
        if f < 0 { return 1; }
        let was = gp_in(f, VIRTGPU_PARAM_LEANDROS_SCATTER, 3).unwrap_or(0);
        let (f1, _) = transfer_cases(f);
        let (f2, _) = scatter_cases(f);
        if was == 0 { gp_in(f, VIRTGPU_PARAM_LEANDROS_SCATTER, 4); }
        print_dec(b"drmsmoke: scatter_creates_total=", gp_in(f, VIRTGPU_PARAM_LEANDROS_SCATTER, 0).unwrap_or(0) as u64);
        close(f);
        print_dec(b"drmsmoke: failed=", (f1 + f2) as u64);
        puts(b"--- drmsmoke done ---\n\0".as_ptr());
        return f1 + f2;
    }
    // `--tokenscope`: only the L1.14 per-open token cases.
    if argc > 1 && arg_is(*argv.add(1) as *const u8, b"--tokenscope") {
        let f = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
        if f < 0 { return 1; }
        let (fails, _) = tokenscope_cases(f);
        close(f);
        print_dec(b"drmsmoke: failed=", fails as u64);
        puts(b"--- drmsmoke done ---\n\0".as_ptr());
        return fails;
    }
    // `--fakeoff` / `--fbtoken` / `--hostvis`: opaque mmap-token cases.
    for (flag, which) in [(&b"--fakeoff"[..], 0), (&b"--fbtoken"[..], 1), (&b"--hostvis"[..], 2)] {
        if argc > 1 && arg_is(*argv.add(1) as *const u8, flag) {
            let f = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
            if f < 0 { return 1; }
            let fails = match which { 0 => fakeoff_cases(f), 1 => fbtoken_cases(f), _ => hostvis_cases(f) };
            close(f);
            print_dec(b"drmsmoke: failed=", fails as u64);
            puts(b"--- drmsmoke done ---\n\0".as_ptr());
            return fails;
        }
    }
    // `--maplife`: only the L1.14 mapping-lifetime cases.
    if argc > 1 && arg_is(*argv.add(1) as *const u8, b"--maplife") {
        let f = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
        if f < 0 { return 1; }
        let (fails, _) = maplife_cases(f);
        close(f);
        print_dec(b"drmsmoke: failed=", fails as u64);
        puts(b"--- drmsmoke done ---\n\0".as_ptr());
        return fails;
    }
    if argc > 1 && arg_is(*argv.add(1) as *const u8, b"--transfer") {
        let f = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
        if f < 0 { return 1; }
        let (fails, _) = transfer_cases(f);
        close(f);
        print_dec(b"drmsmoke: failed=", fails as u64);
        puts(b"--- drmsmoke done ---\n\0".as_ptr());
        return fails;
    }
    if argc > 1 && arg_is(*argv.add(1) as *const u8, b"--fence-loss-demo") {
        let (f, _) = out_fence_cases(true);
        print_dec(b"drmsmoke: failed=", f as u64);
        puts(b"--- drmsmoke done ---\n\0".as_ptr());
        return f;
    }

    let fd = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
    if fd < 0 {
        report(b"open_card0", false);
        puts(b"--- drmsmoke done (open failed) ---\n\0".as_ptr());
        return 1;
    }
    if force_scatter { gp_in(fd, VIRTGPU_PARAM_LEANDROS_SCATTER, 3); }
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
        // `--ctx-drain-us N`: set how long a closing open waits for its GPU
        // context's fenced work before destroying it (root ioctl 0x100A;
        // `-1` or no N restores the default). 0 forces the abandon path.
        if arg_is(a1, b"--ctx-drain-us") {
            let mut v = u64::MAX;
            if argc > 2 {
                let p = *argv.add(2) as *const u8;
                if !p.is_null() && (*p).is_ascii_digit() {
                    v = 0; let mut k = 0usize;
                    while (*p.add(k)).is_ascii_digit() { v = v * 10 + (*p.add(k) - b'0') as u64; k += 1; }
                }
            }
            let mut val = v;
            let ok = ioctl(fd, 0x100A as c_ulong, &mut val as *mut u64) == 0;
            print_dec(b"drmsmoke: ctx drain override old=", val);
            close(fd);
            return if ok { 0 } else { 1 };
        }
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
    let ver_errno = errno();
    if !report_errno(b"VERSION", ver_ok, ver_errno) { failures += 1; }

    // GET_CAP(DUMB_BUFFER) == 1
    let mut cap = DrmGetCap { capability: DRM_CAP_DUMB_BUFFER, value: 0 };
    let cap_dumb_ok = ioctl(fd, DRM_IOCTL_GET_CAP, &mut cap as *mut _) == 0 && cap.value == 1;
    let cap_dumb_errno = errno();
    if !report_errno(b"GET_CAP_DUMB_BUFFER", cap_dumb_ok, cap_dumb_errno) { failures += 1; }

    // GET_CAP(TIMESTAMP_MONOTONIC) == 1
    let mut cap2 = DrmGetCap { capability: DRM_CAP_TIMESTAMP_MONOTONIC, value: 0 };
    let cap_ts_ok = ioctl(fd, DRM_IOCTL_GET_CAP, &mut cap2 as *mut _) == 0 && cap2.value == 1;
    let cap_ts_errno = errno();
    if !report_errno(b"GET_CAP_TIMESTAMP_MONOTONIC", cap_ts_ok, cap_ts_errno) { failures += 1; }

    // GETRESOURCES — expect >=1 crtc/connector/encoder, sane min/max
    let mut res = DrmModeCardRes::default();
    let res_ok = ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &mut res as *mut _) == 0
        && res.count_crtcs >= 1 && res.count_connectors >= 1
        && res.max_width >= res.min_width && res.max_height >= res.min_height;
    let res_errno = errno();
    if !report_errno(b"GETRESOURCES", res_ok, res_errno) { failures += 1; }
    // Every listed connector/encoder answers (extra outputs, lane lapdisp).
    if !report(b"CONNECTOR_ENUM", enum_connectors(fd, false)) { failures += 1; }

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
    let conn_errno = errno();
    if !report_errno(b"GETCONNECTOR", conn_ok, conn_errno) { failures += 1; }

    let w = if modes[0].hdisplay > 0 { modes[0].hdisplay as u32 } else { 256 };
    let h = if modes[0].vdisplay > 0 { modes[0].vdisplay as u32 } else { 256 };
    let (disp_w, disp_h) = (w, h);

    // CREATE_DUMB (full display size so SETCRTC scans it out)
    let mut cd = DrmModeCreateDumb::default();
    cd.width = w;
    cd.height = h;
    cd.bpp = 32;
    let create_ok = ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut cd as *mut _) == 0 && cd.handle != 0;
    let create_errno = errno();
    if !report_errno(b"CREATE_DUMB", create_ok, create_errno) { failures += 1; }

    // MAP_DUMB
    let mut md = DrmModeMapDumb::default();
    md.handle = cd.handle;
    let map_ok = ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut md as *mut _) == 0;
    let map_errno = errno();
    if !report_errno(b"MAP_DUMB", map_ok, map_errno) { failures += 1; }

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
    let addfb_errno = errno();
    if !report_errno(b"ADDFB2", addfb_ok, addfb_errno) { failures += 1; }

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

    // DRM_IOCTL_MODE_ATOMIC(TEST_ONLY) below is the FIRST master-gated ioctl
    // this test issues. If some other process (a compositor) already holds
    // DRM master on card0, every master-gated ioctl from here through
    // FLIP_TS_SUBTICK answers EACCES, and letting each one cascade into its
    // own generic FAIL just buries the one real diagnosis under ~10 lookalike
    // ones. So this first attempt doubles as the probe: read its errno, and if
    // it is EACCES, print one diagnosis line and mark every remaining
    // master-gated case SKIP instead of FAIL, while still running the
    // non-master cases (PRIME_*, V3D_*, SYNCOBJ_*, FORK_DEVMAP_*,
    // DESTROY_DUMB) that follow.
    let mut master_conflict = false;

    if atomic_setup_ok {
        // TEST_ONLY is validation only and must present nothing — smithay
        // issues these constantly, and one that presents would make every
        // probe a frame. Proven positively: the pattern must still be absent
        // from the scanout afterwards. (Byte-identity is deliberately NOT the
        // criterion here — the console is still live at this point and its own
        // output would move the hash. `row_ok` is a content fingerprint and is
        // immune to that.)
        let test_rc = atomic_plane_commit(fd, afb.fb_id, w, h, DRM_MODE_ATOMIC_TEST_ONLY);
        let test_errno = errno();
        master_conflict = test_rc != 0 && test_errno == EACCES;

        if master_conflict {
            puts(b"drmsmoke: card0 has another DRM master (a compositor is running) -- skipping 12 KMS cases; stop the compositor to run them (touch /etc/leandros/text-login; kill $(cat /run/greetd-init.pid))\n\0".as_ptr());
            report_skip(b"ATOMIC_TEST_ONLY_NO_PRESENT"); skips += 1;
            report_skip(b"ATOMIC_COMMIT"); skips += 1;
            report_skip(b"ATOMIC_PRESENTS_PIXELS"); skips += 1;
            report_skip(b"CONSOLE_YIELDS_TO_ATOMIC"); skips += 1;
        } else {
            let (hash_t, bytes_t, row_t) = fb0_census(w as usize, ATOMIC_ROW_R);
            let test_only_ok = test_rc == 0 && bytes_t > 0 && !row_t;
            if !report_errno(b"ATOMIC_TEST_ONLY_NO_PRESENT", test_only_ok, test_errno) { failures += 1; }

            // The real thing: one plane-only commit, no modeset, no event.
            let commit_rc = atomic_plane_commit(fd, afb.fb_id, w, h, 0);
            let commit_errno = errno();
            if !report_errno(b"ATOMIC_COMMIT", commit_rc == 0, commit_errno) { failures += 1; }

            let (hash_a, bytes_a, row_a) = fb0_census(w as usize, ATOMIC_ROW_R);
            print_dec(b"  ATOMIC fnv_test_only=", hash_t);
            print_dec(b"  ATOMIC fnv_after_commit=", hash_a);
            let px_ok = commit_rc == 0 && bytes_a > 0 && bytes_a == bytes_t && row_a;
            if !px_ok {
                puts(b"  ATOMIC_PRESENTS_PIXELS: FAIL the atomic commit returned but its pixels are not on the scanout\n\0".as_ptr());
            }
            if !report(b"ATOMIC_PRESENTS_PIXELS", px_ok) { failures += 1; }

            // ── CONSOLE_YIELDS_TO_ATOMIC ─────────────────────────────────────
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
        }
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
    let setcrtc_ok = if master_conflict {
        report_skip(b"SETCRTC");
        skips += 1;
        false
    } else {
        let ok = ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &mut set as *mut _) == 0;
        let err = errno();
        if !report_errno(b"SETCRTC", ok, err) { failures += 1; }
        ok
    };

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
    } else if master_conflict {
        report_skip(b"FB0_SHOWS_SCANOUT"); skips += 1;
        report_skip(b"CONSOLE_YIELDS_TO_SCANOUT"); skips += 1;
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
    let dirty_ok = if master_conflict {
        report_skip(b"DIRTYFB");
        skips += 1;
        false
    } else {
        let ok = ioctl(fd, DRM_IOCTL_MODE_DIRTYFB, &mut dirty as *mut _) == 0;
        let err = errno();
        if !report_errno(b"DIRTYFB", ok, err) { failures += 1; }
        ok
    };

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
    let _flip_ok = if master_conflict {
        report_skip(b"PAGE_FLIP_EVENT");
        skips += 1;
        false
    } else {
        let ok = ioctl(fd, DRM_IOCTL_MODE_PAGE_FLIP, &mut flip as *mut _) == 0;
        let err = errno();
        if !report_errno(b"PAGE_FLIP_EVENT", ok, err) { failures += 1; }
        ok
    };

    // poll for readiness (throttled delivery is up to ~20 ms out; allow 500 ms).
    let mut pfd = pollfd { fd, events: POLLIN, revents: 0 };
    let poll_ok = if master_conflict {
        report_skip(b"POLL_CARD0_READABLE");
        skips += 1;
        false
    } else {
        let poll_rc = poll(&mut pfd as *mut _, 1, 500);
        let ok = poll_rc == 1 && (pfd.revents & POLLIN) != 0;
        if !report(b"POLL_CARD0_READABLE", ok) { failures += 1; }
        ok
    };

    // read the event back and validate it. Gated on poll_ok: read() used to run
    // unconditionally here, so a poll() that timed out (or, under a master
    // conflict, was never going to see an event because PAGE_FLIP above never
    // queued one) fell straight into a read() with nothing to read — on a
    // blocking fd that hangs the test forever instead of failing it. Treat
    // "poll never said readable" as an immediate FAIL for this case instead of
    // gambling on read() anyway.
    let mut ev = DrmEventVblank::default();
    let _read_ok = if master_conflict {
        report_skip(b"READ_FLIP_EVENT");
        skips += 1;
        false
    } else if poll_ok {
        let rn = read(fd, &mut ev as *mut _ as *mut c_void, core::mem::size_of::<DrmEventVblank>());
        let ok = rn == 32 && ev.ev_type == DRM_EVENT_FLIP_COMPLETE
            && ev.length == 32 && ev.user_data == magic;
        if !report(b"READ_FLIP_EVENT", ok) { failures += 1; }
        ok
    } else {
        if !report(b"READ_FLIP_EVENT", false) { failures += 1; }
        false
    };

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
    if master_conflict {
        report_skip(b"FLIP_TS_SUBTICK");
        skips += 1;
    } else {
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

        // Gated on spoll_ok, same fix as READ_FLIP_EVENT above: this read()
        // used to run unconditionally after poll(), so a timed-out poll (no
        // event ever queued) fell into a read() with nothing there -- on a
        // blocking fd, forever. A poll miss is now an immediate per-sample
        // FAIL instead of a hang.
        let mut sev = DrmEventVblank::default();
        let sread_ok = if spoll_ok {
            let srn = read(fd, &mut sev as *mut _ as *mut c_void, core::mem::size_of::<DrmEventVblank>());
            srn == 32 && sev.ev_type == DRM_EVENT_FLIP_COMPLETE
                && sev.length == 32 && sev.user_data == sflip.user_data
        } else {
            false
        };

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
    }

    // ── GPU completion interrupt (LeandrOS 0x1008 census) ───────────────────
    //
    // The virtio-gpu control queue completes on a device interrupt on both
    // QEMU targets — MSI-X vector 0x41 on x86_64, INTx on the virt board's
    // PCIe line (GIC SPI, INTID 35..38) on aarch64 — with the 100 Hz tick
    // poller as the fallback. These checks read the kernel's own counters
    // through ioctl 0x1008 before and after a burst of presents, so they are
    // deterministic where a latency histogram would only be suggestive:
    //   * GPU_IRQ_ARMED — the device took the interrupt at probe. A build
    //     that fell back to polling fails here, loudly, rather than being
    //     10 ms slower per completion forever.
    //   * GPU_IRQ_COMPLETIONS_COUNTED — the interrupt actually fires: the
    //     census advances across presents whose fences the host answers.
    //   * FLIP_EVENT_DELIVERED_ON_FENCE — flip-complete events rode the
    //     fence path (delivered when the host retired the present) rather
    //     than the tick fallback, for every flip of the burst.
    //   * SYNC_CMD_PARKED — a reply-needing command (ADDFB2's
    //     RESOURCE_CREATE_2D + ATTACH_BACKING) really parked the CPU on the
    //     interrupt and was woken. The counter counts waits that actually
    //     parked, and a normal wait finishes inside its spin-before-park
    //     window, so the spin is set to 0 (ioctl 0x1009, root) around the
    //     ADDFB2 and restored after.
    //   * GPU_IRQ_NO_TIMEOUTS / INTX_NOT_STORMING — nothing wedged and the
    //     shared level line is not being held by another function.
    const DRM_IOCTL_LEANDROS_GPU_IRQ_STATS: c_ulong = 0x1008;
    const DRM_IOCTL_LEANDROS_GPU_PARK_SPIN: c_ulong = 0x1009;
    if master_conflict {
        report_skip(b"GPU_IRQ_ARMED");
        report_skip(b"GPU_IRQ_COMPLETIONS_COUNTED");
        report_skip(b"FLIP_EVENT_DELIVERED_ON_FENCE");
        report_skip(b"SYNC_CMD_PARKED");
        report_skip(b"GPU_IRQ_NO_TIMEOUTS");
        report_skip(b"INTX_NOT_STORMING");
        skips += 6;
    } else {
        let mut st0 = [0u64; 8];
        let st0_ok = ioctl(fd, DRM_IOCTL_LEANDROS_GPU_IRQ_STATS, st0.as_mut_ptr()) == 0;
        let armed = st0_ok && st0[0] == 1;
        print_dec(b"  GPU_IRQ armed=", st0[0]);
        print_dec(b"  GPU_IRQ irqs_before=", st0[1]);
        if !report(b"GPU_IRQ_ARMED", armed) { failures += 1; }

        // A burst of presents with events, each read back before the next,
        // so every one of them is a fenced RESOURCE_FLUSH the host answers.
        // The submit-to-event latency of each is the measurement that tells
        // the two delivery paths apart: on the tick it is uniform over
        // 0..10 ms (mean ~5 ms), on the fence it is the host's present time
        // plus interrupt latency. Printed, not judged — the pass/fail below
        // reads the kernel's own counters instead.
        const IRQ_BURST: usize = 32;
        let mut burst_ok = true;
        let mut lat_sum_us = 0u64;
        let mut lat_max_us = 0u64;
        let mut lat_min_us = u64::MAX;
        let burst_t0 = monotonic_ns();
        for i in 0..IRQ_BURST {
            let mut bflip = DrmModeCrtcPageFlip::default();
            bflip.crtc_id = 1;
            bflip.fb_id = fb.fb_id;
            bflip.flags = DRM_MODE_PAGE_FLIP_EVENT;
            bflip.user_data = 0x1A5E_0000 + i as u64;
            let t0 = monotonic_ns();
            if ioctl(fd, DRM_IOCTL_MODE_PAGE_FLIP, &mut bflip as *mut _) != 0 { burst_ok = false; break; }
            let mut bpfd = pollfd { fd, events: POLLIN, revents: 0 };
            let bpoll = poll(&mut bpfd as *mut _, 1, 500);
            let t1 = monotonic_ns();
            if bpoll != 1 || (bpfd.revents & POLLIN) == 0 { burst_ok = false; break; }
            let mut bev = DrmEventVblank::default();
            let brn = read(fd, &mut bev as *mut _ as *mut c_void, core::mem::size_of::<DrmEventVblank>());
            if brn != 32 || bev.user_data != bflip.user_data { burst_ok = false; break; }
            let lat = t1.wrapping_sub(t0) / 1000;
            lat_sum_us += lat;
            if lat > lat_max_us { lat_max_us = lat; }
            if lat < lat_min_us { lat_min_us = lat; }
        }
        let burst_us = monotonic_ns().wrapping_sub(burst_t0) / 1000;
        if burst_ok {
            print_dec(b"  GPU_IRQ flip_event_latency_mean_us=", lat_sum_us / IRQ_BURST as u64);
            print_dec(b"  GPU_IRQ flip_event_latency_min_us=", lat_min_us);
            print_dec(b"  GPU_IRQ flip_event_latency_max_us=", lat_max_us);
            // Presents per second a client that waits for each event can reach.
            print_dec(b"  GPU_IRQ flip_rate_per_s=", IRQ_BURST as u64 * 1_000_000 / burst_us.max(1));
        }
        // Reply-needing commands: ADDFB2 over a fresh dumb buffer issues
        // RESOURCE_CREATE_2D and ATTACH_BACKING synchronously (the reply is
        // checked before the fb exists).
        let mut sd = DrmModeCreateDumb::default();
        sd.width = 64; sd.height = 64; sd.bpp = 32;
        let mut sd_ok = ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut sd as *mut _) == 0;
        let mut spin_saved: u64 = 0;
        let spin_forced = ioctl(fd, DRM_IOCTL_LEANDROS_GPU_PARK_SPIN, &mut spin_saved as *mut u64) == 0;
        let sync_t0 = monotonic_ns();
        if sd_ok {
            let mut sfb = DrmModeFbCmd2::default();
            sfb.width = 64;
            sfb.height = 64;
            sfb.pixel_format = DRM_FORMAT_XRGB8888;
            sfb.handles[0] = sd.handle;
            sfb.pitches[0] = sd.pitch;
            sd_ok = ioctl(fd, DRM_IOCTL_MODE_ADDFB2, &mut sfb as *mut _) == 0 && sfb.fb_id != 0;
            if sd_ok {
                let mut rm = sfb.fb_id;
                let _ = ioctl(fd, DRM_IOCTL_MODE_RMFB, &mut rm as *mut u32);
            }
            let mut dd = sd.handle;
            let _ = ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut dd as *mut u32);
        }
        let sync_us = monotonic_ns().wrapping_sub(sync_t0) / 1000;
        if spin_forced {
            let mut v = spin_saved;
            let _ = ioctl(fd, DRM_IOCTL_LEANDROS_GPU_PARK_SPIN, &mut v as *mut u64);
        }
        print_dec(b"  GPU_IRQ park_spin_forced_to_0=", spin_forced as u64);
        print_dec(b"  GPU_IRQ park_spin_restored_us=", spin_saved);
        print_dec(b"  GPU_IRQ sync_addfb_rmfb_us=", sync_us);

        let mut st1 = [0u64; 8];
        let st1_ok = ioctl(fd, DRM_IOCTL_LEANDROS_GPU_IRQ_STATS, st1.as_mut_ptr()) == 0;
        let d_irqs   = st1[1].wrapping_sub(st0[1]);
        let d_fflips = st1[2].wrapping_sub(st0[2]);
        let d_parked = st1[3].wrapping_sub(st0[3]);
        let d_spur   = st1[4].wrapping_sub(st0[4]);
        let d_deliv  = st1[5].wrapping_sub(st0[5]);
        let d_sync   = st1[6].wrapping_sub(st0[6]);
        let d_to     = st1[7].wrapping_sub(st0[7]);
        print_dec(b"  GPU_IRQ burst_flips=", IRQ_BURST as u64);
        print_dec(b"  GPU_IRQ d_irqs=", d_irqs);
        print_dec(b"  GPU_IRQ d_flips_delivered=", d_deliv);
        print_dec(b"  GPU_IRQ d_flips_on_fence=", d_fflips);
        print_dec(b"  GPU_IRQ d_sync_cmds=", d_sync);
        print_dec(b"  GPU_IRQ d_parked=", d_parked);
        print_dec(b"  GPU_IRQ d_intx_spurious=", d_spur);
        print_dec(b"  GPU_IRQ d_timeouts=", d_to);
        let ok_all = st0_ok && st1_ok && burst_ok && sd_ok;
        if !report(b"GPU_IRQ_COMPLETIONS_COUNTED", ok_all && armed && d_irqs >= 1) { failures += 1; }
        if !report(b"FLIP_EVENT_DELIVERED_ON_FENCE", ok_all && armed && d_fflips >= IRQ_BURST as u64) { failures += 1; }
        if !report(b"SYNC_CMD_PARKED", ok_all && armed && d_sync >= 1 && d_parked >= 1) { failures += 1; }
        if !report(b"GPU_IRQ_NO_TIMEOUTS", ok_all && d_to == 0) { failures += 1; }
        if !report(b"INTX_NOT_STORMING", ok_all && d_spur < 64) { failures += 1; }
    }

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

    // ── Sync files (syncobj <-> sync_file, dma-buf implicit fences) ─────────
    //
    // What zink needs from the kernel on the NVIDIA card: vkGetSemaphoreFdKHR /
    // vkImportSemaphoreFdKHR with SYNC_FD (HANDLE_TO_FD / FD_TO_HANDLE with the
    // sync-file flags) and DMA_BUF_IOCTL_{EXPORT,IMPORT}_SYNC_FILE on shared
    // images. Gated in the kernel (drivers::sync_file::enabled): ENOSYS on a
    // plain virtio-gpu boot, so the desktop paths keep their old behaviour;
    // `drm.syncfile=1` on the kernel command line opens the gate for this test.
    // hb was signalled by the child above, so it holds a signalled fence.
    {
        let mut h2f = DrmSyncobjHandle { handle: hb, flags: DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE, fd: -1, pad: 0 };
        let rc = ioctl(fd, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &mut h2f as *mut _);
        let err = errno();
        if rc == -1 && err == ENOSYS {
            report_skip(b"SYNC_FILE (gate closed; boot with drm.syncfile=1 or on a GPU UAPI backend)");
        } else {
            let sfd = h2f.fd;
            let mut pf = pollfd { fd: sfd, events: 1 /* POLLIN */, revents: 0 };
            let exp_ok = rc == 0 && sfd >= 0 && poll(&mut pf as *mut _, 1, 0) == 1 && pf.revents & 1 != 0;
            if !report_errno(b"SYNC_FILE_EXPORT_SIGNALED_POLLIN", exp_ok, err) { failures += 1; }

            let mut sn = DrmSyncobjCreate::default();
            let created = ioctl(fd, DRM_IOCTL_SYNCOBJ_CREATE, &mut sn as *mut _) == 0;
            let mut h2n = DrmSyncobjHandle { handle: sn.handle, flags: DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE, fd: -1, pad: 0 };
            let null_einval = created && ioctl(fd, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &mut h2n as *mut _) == -1 && errno() == EINVAL;
            if !report(b"SYNC_FILE_EXPORT_NULL_FENCE_EINVAL", null_einval) { failures += 1; }

            let mut f2h = DrmSyncobjHandle { handle: sn.handle, flags: DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE, fd: sfd, pad: 0 };
            let imp_rc = ioctl(fd, DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &mut f2h as *mut _);
            let hn: [u32; 1] = [sn.handle];
            let mut wn = DrmSyncobjWait { handles: hn.as_ptr() as u64, timeout_nsec: 0, count_handles: 1, flags: 0, first_signaled: 0, pad: 0 };
            let imp_ok = imp_rc == 0 && ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &mut wn as *mut _) == 0;
            if !report(b"SYNC_FILE_IMPORT_SIGNALS_SYNCOBJ", imp_ok) { failures += 1; }

            // A DRM fd is not a sync file.
            let mut f2bad = DrmSyncobjHandle { handle: sn.handle, flags: DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE, fd, pad: 0 };
            let bad_einval = ioctl(fd, DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &mut f2bad as *mut _) == -1 && errno() == EINVAL;
            if !report(b"SYNC_FILE_IMPORT_NOT_A_SYNC_FILE_EINVAL", bad_einval) { failures += 1; }

            let mut info = SyncFileInfo { name: [0; 32], status: -1, flags: 0, num_fences: 0, pad: 0, sync_fence_info: 0 };
            let info_ok = ioctl(sfd, SYNC_IOC_FILE_INFO, &mut info as *mut _) == 0 && info.status == 1 && info.num_fences >= 1;
            if !report(b"SYNC_IOC_FILE_INFO", info_ok) { failures += 1; }

            let mut md = SyncMergeData { name: [0; 32], fd2: sfd, fence: -1, flags: 0, pad: 0 };
            let mrc = ioctl(sfd, SYNC_IOC_MERGE, &mut md as *mut _);
            let mut pm = pollfd { fd: md.fence, events: 1, revents: 0 };
            let merge_ok = mrc == 0 && md.fence >= 0 && poll(&mut pm as *mut _, 1, 0) == 1;
            if !report(b"SYNC_IOC_MERGE", merge_ok) { failures += 1; }
            if md.fence >= 0 { close(md.fence); }

            // dma-buf implicit fences on an exported dumb buffer.
            let mut cd = DrmModeCreateDumb { height: 64, width: 64, bpp: 32, flags: 0, handle: 0, pitch: 0, size: 0 };
            let mut dmabuf_ok = false;
            let mut idle_ok = false;
            if ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut cd as *mut _) == 0 {
                let mut ph = DrmPrimeHandle { handle: cd.handle, flags: 0, fd: -1 };
                if ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut ph as *mut _) == 0 && ph.fd >= 0 {
                    let mut ex = DmaBufSyncFile { flags: 3 /* RW */, fd: -1 };
                    let erc = ioctl(ph.fd, DMA_BUF_IOCTL_EXPORT_SYNC_FILE, &mut ex as *mut _);
                    let mut pe = pollfd { fd: ex.fd, events: 1, revents: 0 };
                    idle_ok = erc == 0 && ex.fd >= 0 && poll(&mut pe as *mut _, 1, 0) == 1;
                    if ex.fd >= 0 { close(ex.fd); }
                    let mut im = DmaBufSyncFile { flags: 2 /* WRITE */, fd: sfd };
                    let mut im0 = DmaBufSyncFile { flags: 0, fd: sfd };
                    let mut sy: u64 = 1 | 4; // SYNC_READ | SYNC_END
                    dmabuf_ok = ioctl(ph.fd, DMA_BUF_IOCTL_IMPORT_SYNC_FILE, &mut im as *mut _) == 0
                        && ioctl(ph.fd, DMA_BUF_IOCTL_IMPORT_SYNC_FILE, &mut im0 as *mut _) == -1 && errno() == EINVAL
                        && ioctl(ph.fd, DMA_BUF_IOCTL_SYNC, &mut sy as *mut _) == 0;
                    close(ph.fd);
                }
                let mut dd: u32 = cd.handle;
                ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut dd as *mut u32);
            }
            if !report(b"DMABUF_EXPORT_SYNC_FILE_IDLE", idle_ok) { failures += 1; }
            if !report(b"DMABUF_IMPORT_SYNC_FILE", dmabuf_ok) { failures += 1; }

            if sfd >= 0 { close(sfd); }
            if created {
                let mut dn = DrmSyncobjDestroy { handle: sn.handle, pad: 0 };
                ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &mut dn as *mut _);
            }
        }
    }

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
    //
    // Two drmsmoke processes must not overlap here: one's disarm pulls the
    // backend out from under the other's v3d checks (seen as V3D_* failures
    // when a background drmsmoke loop ran beside a foreground one). An
    // exclusive flock on a well-known file serialises the block; the kernel
    // drops it if the holder dies, so a killed run cannot wedge the next.
    let v3d_lock = open(b"/tmp/.drmsmoke-v3d.lock\0".as_ptr(), O_RDWR | O_CREAT, 0o666 as c_int);
    if v3d_lock >= 0 { flock(v3d_lock, LOCK_EX); }
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
    if v3d_lock >= 0 { close(v3d_lock); }

    // ── PRIME / dmabuf export + import round-trip (K5) ──────────────────────
    // Export the dumb buffer as a dmabuf fd, mmap that fd, and confirm it
    // aliases the SAME physical pages as a fresh MAP_DUMB mapping (coherent),
    // then round-trip the fd back to the original GEM handle.
    let mut ph = DrmPrimeHandle::default();
    ph.handle = cd.handle;
    let export_ok = ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut ph as *mut _) == 0 && ph.fd >= 0;
    let export_errno = errno();
    if !report_errno(b"PRIME_HANDLE_TO_FD", export_ok, export_errno) { failures += 1; }

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
    let import_errno = errno();
    if !report_errno(b"PRIME_FD_TO_HANDLE", import_ok, import_errno) { failures += 1; }

    // Importing the fd on a SECOND open mints that open its own handle, as
    // upstream's per-drm_file tables do (`prime_import_dumb`). It used to
    // echo the exporter's handle, and the importer's DESTROY_DUMB/GEM_CLOSE
    // then retired the EXPORTER's handle — under virgl, cosmic-comp closing
    // an imported cosmic-panel buffer killed the panel's own handle, and the
    // panel never appeared. Asserted: a distinct handle; a re-import on the
    // same open answers with it again; it maps the same pages; and closing it
    // leaves the exporter's handle alive.
    let fd_other = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
    let mut ph3 = DrmPrimeHandle::default();
    ph3.fd = ph.fd;
    let mut other_ok = fd_other >= 0 && export_ok
        && ioctl(fd_other, DRM_IOCTL_PRIME_FD_TO_HANDLE, &mut ph3 as *mut _) == 0
        && ph3.handle != 0 && ph3.handle != cd.handle;
    let other_errno = errno();
    if other_ok {
        let mut ph4 = DrmPrimeHandle::default();
        ph4.fd = ph.fd;
        other_ok = ioctl(fd_other, DRM_IOCTL_PRIME_FD_TO_HANDLE, &mut ph4 as *mut _) == 0
            && ph4.handle == ph3.handle;
    }
    if other_ok && cd.size > 0 {
        let mut mo = DrmModeMapDumb { handle: ph3.handle, ..Default::default() };
        other_ok = ioctl(fd_other, DRM_IOCTL_MODE_MAP_DUMB, &mut mo as *mut _) == 0;
        if other_ok {
            let op = mmap(core::ptr::null_mut(), cd.size as usize, PROT_READ,
                          MAP_SHARED, fd_other, mo.offset as i64);
            other_ok = op as isize > 0 && *(op as *const u32) == 0x0040_0000;
        }
    }
    if other_ok {
        let mut hc = ph3.handle;
        other_ok = ioctl(fd_other, DRM_IOCTL_MODE_DESTROY_DUMB, &mut hc as *mut u32) == 0;
        let mut mo = DrmModeMapDumb { handle: cd.handle, ..Default::default() };
        other_ok = other_ok && ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut mo as *mut _) == 0;
    }
    if !report_errno(b"PRIME_FD_TO_HANDLE_OTHER_OPEN_DUMB", other_ok, other_errno) { failures += 1; }
    if fd_other >= 0 { close(fd_other); }

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

    // Lane gpufencefb: framebuffer lifetime and out-fence atomicity.
    {
        let (f, sk) = fb_lifetime_cases(fd, setcrtc_ok, fb.fb_id, connector_id, disp_w, disp_h);
        failures += f; skips += sk;
        let (f, sk) = out_fence_cases(false);
        failures += f; skips += sk;
    }

    // Lane vrr: VRR_ENABLED / vrr_capable properties.
    failures += vrr_cases(fd);

    // Lane ffmagenta: virgl transfers and VIRTGPU_WAIT's busy errno.
    {
        let (f, sk) = transfer_cases(fd);
        failures += f; skips += sk;
    }
    // Lane virglscat: scattered virgl resource mmap / PRIME.
    {
        let (f, sk) = scatter_cases(fd);
        failures += f; skips += sk;
    }
    // Lane drmtoken (L1.14): mmap tokens are per open.
    {
        let (f, sk) = tokenscope_cases(fd);
        failures += f; skips += sk;
    }
    // Lane bomapref (L1.14): a live mmap keeps the BO's pages.
    {
        let (f, sk) = maplife_cases(fd);
        failures += f; skips += sk;
    }
    if force_scatter {
        gp_in(fd, VIRTGPU_PARAM_LEANDROS_SCATTER, 4);
        print_dec(b"drmsmoke: scatter_creates_total=", gp_in(fd, VIRTGPU_PARAM_LEANDROS_SCATTER, 0).unwrap_or(0) as u64);
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
    let destroy_errno = errno();
    if !report_errno(b"DESTROY_DUMB", destroy_ok, destroy_errno) { failures += 1; }

    // close() is what hands the framebuffer console back — SETCRTC (when it
    // ran) disabled it, and this restores it. Reached on every path out of the
    // checks above, including the master-conflict SKIP path (SETCRTC itself
    // was never called, so there is nothing to restore, but every fd this
    // process opened is still closed the same way) and any early per-case
    // failure, since none of those branches return out of this function.
    close(fd);

    print_dec(b"drmsmoke: failed=", failures as u64);
    print_dec(b"drmsmoke: skipped=", skips as u64);
    puts(b"--- drmsmoke done ---\n\0".as_ptr());
    failures
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}
