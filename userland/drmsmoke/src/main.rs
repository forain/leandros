//! drmsmoke — K4 rung R0: raw-ioctl DRM smoke test on /dev/dri/card0.
//!
//! Exercises the buffer + legacy-KMS ioctl surface that Mesa/GBM (kms_swrast)
//! and a legacy Smithay backend issue, plus the st_rdev plumbing libdrm needs:
//!   fstat st_rdev == 226:0; VERSION; GET_CAP(DUMB_BUFFER/TIMESTAMP_MONOTONIC);
//!   GETRESOURCES; GETCONNECTOR (connected, >=1 mode); CREATE_DUMB; MAP_DUMB;
//!   mmap + fill gradient; ADDFB2; SETCRTC; DIRTYFB; DESTROY_DUMB.
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
//! to stdout and sleeps in >=1s chunks forever, so a QEMU `screendump` can be
//! pixel-checked against those exact colours/coordinates.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

type c_int = i32;
type c_uint = u32;
type c_ulong = u64;
type size_t = usize;

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
const DRM_IOCTL_PRIME_HANDLE_TO_FD: c_ulong = 0xC00C642D;
const DRM_IOCTL_PRIME_FD_TO_HANDLE: c_ulong = 0xC00C642E;

const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;

const POLLIN: i16 = 0x001;

const DRM_CAP_DUMB_BUFFER: u64 = 0x1;
const DRM_CAP_TIMESTAMP_MONOTONIC: u64 = 0x6;

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
                    *base.add(off + 2) = 0x40;                           // R
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

    if hold_mode {
        if setcrtc_ok && !fb_ptr.is_null() {
            paint_field_and_block(fb_ptr, cd.pitch as usize, w as usize, h as usize);
        }
        let mut hold_dirty = DrmModeFbDirtyCmd::default();
        hold_dirty.fb_id = fb.fb_id;
        ioctl(fd, DRM_IOCTL_MODE_DIRTYFB, &mut hold_dirty as *mut _);
        puts(b"DRMSMOKE: HOLD READY\n\0".as_ptr());
        loop {
            usleep(1_000_000);
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
    // STATISTICAL CAVEAT (do not invert this logic): a genuine sub-tick
    // timestamp CAN legitimately land on an exact multiple of 10_000 by pure
    // chance (~1 in 10_000 per sample). A single non-multiple sample proves
    // liveness; a single multiple sample proves nothing either way. That is
    // why this drives several flips and passes on ANY non-multiple sample
    // rather than requiring ALL samples to be non-multiples: with 8+
    // independent samples, an ALL-multiples result is overwhelming evidence
    // the old tick-derived code is still what's running, not bad luck
    // (chance of that happening under genuinely live sub-tick timestamps is
    // roughly 1 in 10_000^8).
    const FLIP_TS_SAMPLES: usize = 8;
    let mut subtick_all_read_ok = true;
    let mut subtick_seen = false;
    for i in 0..FLIP_TS_SAMPLES {
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
        if sev.tv_usec % 10_000 != 0 {
            subtick_seen = true;
        }
    }
    let subtick_ok = subtick_all_read_ok && subtick_seen;
    if !report(b"FLIP_TS_SUBTICK", subtick_ok) { failures += 1; }

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
