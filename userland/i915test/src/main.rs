//! i915test — the G3 i915 UAPI, end to end, the way Mesa iris drives it.
//!
//! Runs against whatever answers /dev/dri/renderD128 as `i915`: in QEMU that
//! is the fake device (`i915.fake=2`, a soft command streamer executes the
//! batches); on the laptop it is the real node (`i915.uapi=1`), where every
//! step up to the first EXECBUFFER2 must pass and execution is expected to
//! answer ENODEV until hardware submission is wired (the test says so).
//!
//! What is asserted (identity and data, not just "ioctl returned 0"):
//!   identity: VERSION name, sysfs vendor/device, GETPARAM chipset/EUs/
//!             softpin/timestamp, QUERY topology/engines/memory regions with
//!             the two-pass length protocol, hwconfig -> -ENODEV;
//!   memory:   GEM_CREATE(+EXT system region), MMAP_OFFSET + mmap, PWRITE
//!             visible through the mapping and PREAD seeing CPU writes;
//!             tiling/caching/madvise/set-domain round trips;
//!   contexts: VM_CREATE, CONTEXT_CREATE_EXT with SETPARAM VM + ENGINES,
//!             GETPARAM VM/GTT_SIZE/ENGINES, DESTROY + second DESTROY;
//!   execution: a softpinned batch at a 48-bit address storing into a BO at
//!             another 48-bit address (the CPU mapping must see the value:
//!             proves the VM binding), syncobj signal (fence array) + WAIT,
//!             timeline-fence extension + TIMELINE_WAIT, GEM_BUSY/WAIT, a
//!             faulting batch counted in GET_RESET_STATS, unpinned -> EINVAL;
//!   sharing:  PRIME export/import round trip.
//!
//! Prints "<name>: PASS/FAIL"; exit code = failures. `i915test -v` adds detail.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

type c_int = i32;
type c_ulong = u64;

const O_RDWR: c_int = 0o2;
const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_SHARED: c_int = 1;
const ENOENT: i32 = 2;
const EINVAL: i32 = 22;
const ENODEV: i32 = 19;

extern "C" {
    pub fn relibc_start_v1(sp: *const c_void, main: unsafe extern "C" fn(isize, *mut *mut u8, *mut *mut u8) -> i32) -> !;
    pub fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    pub fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    pub fn open(path: *const u8, oflag: c_int, ...) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    pub fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, offset: i64) -> *mut c_void;
    pub fn munmap(addr: *mut c_void, len: usize) -> c_int;
    pub fn __errno_location() -> *mut c_int;
    pub fn fork() -> c_int;
    pub fn pipe(fds: *mut c_int) -> c_int;
    pub fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    pub fn _exit(code: c_int) -> !;
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start", ".global _start", "_start:",
    "   xor rbp, rbp", "   mov rdi, rsp", "   mov rsi, offset i915_main", "   and rsp, -16",
    "   call relibc_start_v1", "   ud2"
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text._start", ".global _start", "_start:",
    "   mov x29, #0", "   mov x30, #0", "   mov x0, sp", "   adrp x1, i915_main", "   add x1, x1, :lo12:i915_main",
    "   and sp, x0, #-16", "   bl relibc_start_v1", "   brk #0"
);

// ── ioctl codes ──────────────────────────────────────────────────────────────

const DRM_IOCTL_VERSION: c_ulong = 0xC040_6400;
const DRM_IOCTL_GET_CAP: c_ulong = 0xC010_640C;
const DRM_IOCTL_MODE_ADDFB2: c_ulong = 0xC068_64B8;
const DRM_IOCTL_MODE_RMFB: c_ulong = 0xC004_64AF;
const DRM_IOCTL_GEM_CLOSE: c_ulong = 0x4008_6409;
const DRM_IOCTL_PRIME_HANDLE_TO_FD: c_ulong = 0xC00C_642D;
const DRM_IOCTL_PRIME_FD_TO_HANDLE: c_ulong = 0xC00C_642E;
const DRM_IOCTL_SYNCOBJ_CREATE: c_ulong = 0xC008_64BF;
const DRM_IOCTL_SYNCOBJ_DESTROY: c_ulong = 0xC008_64C0;
const DRM_IOCTL_SYNCOBJ_WAIT: c_ulong = 0xC020_64C3;
const DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT: c_ulong = 0xC028_64CA;

const I915_GETPARAM: c_ulong = 0xC010_6446;
const I915_QUERY: c_ulong = 0xC010_6479;
const I915_GEM_CREATE: c_ulong = 0xC010_645B;
const I915_GEM_CREATE_EXT: c_ulong = 0xC018_647C;
const I915_GEM_MMAP_OFFSET: c_ulong = 0xC020_6464;
const I915_GEM_PWRITE: c_ulong = 0x4020_645D;
const I915_GEM_PREAD: c_ulong = 0x4020_645C;
const I915_GEM_SET_DOMAIN: c_ulong = 0x400C_645F;
const I915_GEM_SET_TILING: c_ulong = 0xC010_6461;
const I915_GEM_GET_TILING: c_ulong = 0xC010_6462;
const I915_GEM_GET_CACHING: c_ulong = 0xC008_6470;
const I915_GEM_MADVISE: c_ulong = 0xC00C_6466;
const I915_GEM_BUSY: c_ulong = 0xC008_6457;
const I915_GEM_WAIT: c_ulong = 0xC010_646C;
const I915_GEM_EXECBUFFER2_WR: c_ulong = 0xC040_6469;
const I915_GEM_CONTEXT_CREATE_EXT: c_ulong = 0xC010_646D;
const I915_GEM_CONTEXT_DESTROY: c_ulong = 0x4008_646E;
const I915_GEM_CONTEXT_GETPARAM: c_ulong = 0xC018_6474;
const I915_GEM_VM_CREATE: c_ulong = 0xC010_647A;
const I915_GEM_VM_DESTROY: c_ulong = 0x4010_647B;
const I915_REG_READ: c_ulong = 0xC010_6471;
const I915_GET_RESET_STATS: c_ulong = 0xC018_6472;
const I915_GEM_GET_APERTURE: c_ulong = 0x8010_6463;

// ── structures (i915_drm.h / drm.h) ──────────────────────────────────────────

#[repr(C)] #[derive(Default)]
struct DrmVersion { major: i32, minor: i32, patch: i32, _pad: i32, name_len: usize, name: u64, date_len: usize, date: u64, desc_len: usize, desc: u64 }
#[repr(C)] #[derive(Default)]
struct Getparam { param: i32, _pad: u32, value: u64 }
#[repr(C)] #[derive(Default)]
struct Query { num_items: u32, flags: u32, items: u64 }
#[repr(C)] #[derive(Default, Clone, Copy)]
struct QueryItem { id: u64, length: i32, flags: u32, data: u64 }
#[repr(C)] #[derive(Default)]
struct GemCreate { size: u64, handle: u32, _pad: u32 }
#[repr(C)] #[derive(Default)]
struct GetCap { capability: u64, value: u64 }
#[repr(C)] #[derive(Default)]
struct FbCmd2 { fb_id: u32, width: u32, height: u32, pixel_format: u32, flags: u32, handles: [u32; 4], pitches: [u32; 4],
    offsets: [u32; 4], _pad: u32, modifier: [u64; 4] }
#[repr(C)] #[derive(Default)]
struct GemCreateExt { size: u64, handle: u32, flags: u32, ext: u64 }
#[repr(C)] #[derive(Default)]
struct UserExt { next: u64, name: u32, flags: u32, rsvd: [u32; 4] }
#[repr(C)] #[derive(Default)]
struct ExtRegions { base: UserExt, pad: u32, num: u32, regions: u64 }
#[repr(C)] #[derive(Default)]
struct MmapOffset { handle: u32, _pad: u32, offset: u64, flags: u64, ext: u64 }
#[repr(C)] #[derive(Default)]
struct Prw { handle: u32, _pad: u32, offset: u64, size: u64, data: u64 }
#[repr(C)] #[derive(Default)]
struct SetDomain { handle: u32, read: u32, write: u32 }
#[repr(C)] #[derive(Default)]
struct Tiling { handle: u32, mode: u32, stride: u32, swizzle: u32 }
#[repr(C)] #[derive(Default)]
struct HandleVal { handle: u32, val: u32 }
#[repr(C)] #[derive(Default)]
struct Madvise { handle: u32, madv: u32, retained: u32 }
#[repr(C)] #[derive(Default)]
struct GemWait { handle: u32, flags: u32, timeout: i64 }
#[repr(C)] #[derive(Default)]
struct Execbuf2 { buffers: u64, count: u32, start: u32, len: u32, dr1: u32, dr4: u32, ncliprects: u32, cliprects: u64, flags: u64, rsvd1: u64, rsvd2: u64 }
#[repr(C)] #[derive(Default, Clone, Copy)]
struct ExecObj { handle: u32, nreloc: u32, relocs: u64, align: u64, offset: u64, flags: u64, rsvd1: u64, rsvd2: u64 }
#[repr(C)] #[derive(Default, Clone, Copy)]
struct ExecFence { handle: u32, flags: u32 }
#[repr(C)] #[derive(Default)]
struct TimelineExt { base: UserExt, count: u64, handles: u64, values: u64 }
#[repr(C)] #[derive(Default)]
struct CtxCreateExt { id: u32, flags: u32, ext: u64 }
#[repr(C)] #[derive(Default, Clone, Copy)]
struct CtxParam { ctx: u32, size: u32, param: u64, value: u64 }
#[repr(C)] #[derive(Default)]
struct CtxSetparamExt { base: UserExt, p: CtxParam }
#[repr(C)] #[derive(Default)]
struct VmControl { ext: u64, flags: u32, id: u32 }
#[repr(C)] #[derive(Default)]
struct RegRead { offset: u64, val: u64 }
#[repr(C)] #[derive(Default)]
struct ResetStats { ctx: u32, flags: u32, resets: u32, active: u32, pending: u32, pad: u32 }
#[repr(C)] #[derive(Default)]
struct Aperture { size: u64, avail: u64 }
#[repr(C)] #[derive(Default)]
struct PrimeHandle { handle: u32, flags: u32, fd: i32 }
#[repr(C)] #[derive(Default)]
struct SyncobjCreate { handle: u32, flags: u32 }
#[repr(C)] #[derive(Default)]
struct SyncobjWait { handles: u64, timeout: i64, count: u32, flags: u32, first: u32, pad: u32 }
#[repr(C)] #[derive(Default)]
struct TimelineWait { handles: u64, points: u64, timeout: i64, count: u32, flags: u32, first: u32, pad: u32 }

// ── output ───────────────────────────────────────────────────────────────────

static mut VERBOSE: bool = false;
static mut FAILS: i32 = 0;

fn out(s: &[u8]) { unsafe { write(1, s.as_ptr() as *const c_void, s.len()); } }
fn out_num(v: u64, hex: bool) {
    let mut b = [0u8; 20];
    let mut i = b.len();
    let mut v = v;
    let base = if hex { 16 } else { 10 };
    if v == 0 { out(b"0"); return; }
    while v > 0 { i -= 1; b[i] = b"0123456789abcdef"[(v % base) as usize]; v /= base; }
    if hex { out(b"0x"); }
    out(&b[i..]);
}
fn note(what: &[u8], v: u64) {
    if unsafe { VERBOSE } { out(b"    "); out(what); out(b" = "); out_num(v, true); out(b"\n"); }
}
fn report(name: &[u8], ok: bool) {
    out(name);
    out(if ok { b": PASS\n" } else { b": FAIL\n" });
    if !ok { unsafe { FAILS += 1; } }
}
fn errno() -> i32 { unsafe { *__errno_location() } }

/// ioctl -> Ok or Err(errno).
fn io<T>(fd: c_int, req: c_ulong, arg: &mut T) -> Result<(), i32> {
    let r = unsafe { ioctl(fd, req, arg as *mut T) };
    if r == 0 { Ok(()) } else { Err(errno()) }
}

fn getparam(fd: c_int, p: i32) -> Option<i32> {
    let mut v: i32 = -1;
    let mut g = Getparam { param: p, _pad: 0, value: &mut v as *mut i32 as u64 };
    io(fd, I915_GETPARAM, &mut g).ok().map(|_| v)
}

/// Two-pass QUERY of one item: (length or -errno, data).
fn query(fd: c_int, id: u64, buf: &mut [u8]) -> i32 {
    let mut it = QueryItem { id, ..Default::default() };
    let mut q = Query { num_items: 1, flags: 0, items: &mut it as *mut QueryItem as u64 };
    if io(fd, I915_QUERY, &mut q).is_err() { return i32::MIN; }
    if it.length <= 0 || it.length as usize > buf.len() { return it.length; }
    it.data = buf.as_mut_ptr() as u64;
    if io(fd, I915_QUERY, &mut q).is_err() { return i32::MIN; }
    it.length
}

fn read_file(path: &[u8], buf: &mut [u8]) -> usize {
    let fd = unsafe { open(path.as_ptr(), 0) };
    if fd < 0 { return 0; }
    let n = unsafe { read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
    unsafe { close(fd); }
    if n < 0 { 0 } else { n as usize }
}

fn gem_create(fd: c_int, size: u64) -> u32 {
    let mut c = GemCreate { size, ..Default::default() };
    if io(fd, I915_GEM_CREATE, &mut c).is_ok() { c.handle } else { 0 }
}

fn gem_close(fd: c_int, h: u32) -> Result<(), i32> {
    let mut c = HandleVal { handle: h, val: 0 };
    io(fd, DRM_IOCTL_GEM_CLOSE, &mut c)
}

fn map(fd: c_int, h: u32, size: usize) -> *mut u32 {
    let mut m = MmapOffset { handle: h, flags: 2 /* WB */, ..Default::default() };
    if io(fd, I915_GEM_MMAP_OFFSET, &mut m).is_err() { return core::ptr::null_mut(); }
    let p = unsafe { mmap(core::ptr::null_mut(), size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, m.offset as i64) };
    if p as isize == -1 { core::ptr::null_mut() } else { p as *mut u32 }
}

fn syncobj(fd: c_int) -> u32 {
    let mut c = SyncobjCreate::default();
    if io(fd, DRM_IOCTL_SYNCOBJ_CREATE, &mut c).is_ok() { c.handle } else { 0 }
}

const PINNED: u64 = 1 << 4;
const B48: u64 = 1 << 3;
const WRITE: u64 = 1 << 2;
const EXEC_NO_RELOC: u64 = 1 << 11;
const EXEC_HANDLE_LUT: u64 = 1 << 12;
const EXEC_BATCH_FIRST: u64 = 1 << 18;
const EXEC_FENCE_ARRAY: u64 = 1 << 19;
const EXEC_USE_EXTENSIONS: u64 = 1 << 21;

/// Canonical form of a 48-bit GPU address.
fn canon(a: u64) -> u64 { (((a << 16) as i64) >> 16) as u64 }

fn sdi(b: &mut [u32], at: usize, va: u64, v: u32) -> usize {
    b[at] = (0x20 << 23) | 2;
    b[at + 1] = va as u32;
    b[at + 2] = (va >> 32) as u32;
    b[at + 3] = v;
    at + 4
}

// ── coherency (lane/gpucoh) ──────────────────────────────────────────────────
//
// What Mesa iris relies on, on an LLC part, with changing data so a stale
// line cannot pass by accident:
//   coh_cpu_gpu_cpu_{rcs,bcs}: the CPU writes a new pattern through a WB mmap
//     (and a fresh batch), the GPU copies it (MI_COPY_MEM_MEM) to another
//     object, the CPU reads the copy after GEM_WAIT: CPU->GPU and GPU->CPU,
//     every iteration, 64 cache lines spread over 64 KiB;
//   coh_cross_engine_implicit_sync: a bcs batch writes, an rcs batch queued
//     right behind it (no wait in between) must see the write;
//   coh_dmabuf_two_process: a forked producer (its own open and VM) writes a
//     PRIME-shared object on the GPU and returns at once; the consumer
//     copies it on the GPU without waiting: implicit sync across processes.
// Each test prints the iterations that saw stale data (expected 0).
// Not covered: the sampler/L3 path (textures) and render-target writes.

static mut COH_ONLY: bool = false;
const COH_ITERS: u32 = 200;
const SRC_VA: u64 = 0x0000_0001_0000_0000;
const DST_VA: u64 = 0x0000_0001_0010_0000;
const CB_VA: u64 = 0x0000_0001_0020_0000;
const SH_VA: u64 = 0x0000_0001_0030_0000;
const COH_SIZE: u64 = 64 << 10;

fn mix(i: u32, k: u32) -> u32 { (i.wrapping_mul(0x9E37_79B9) ^ k.wrapping_mul(0x85EB_CA6B)).rotate_left(k & 31) | 1 }

fn copy_mm(b: &mut [u32], at: usize, dst: u64, src: u64) -> usize {
    b[at] = (0x2E << 23) | 3; // MI_COPY_MEM_MEM, PPGTT
    b[at + 1] = dst as u32;
    b[at + 2] = (dst >> 32) as u32;
    b[at + 3] = src as u32;
    b[at + 4] = (src >> 32) as u32;
    at + 5
}

/// Submit `objs` (batch first) on the default context; `ring` 1 = rcs, 3 = bcs.
fn exec(fd: c_int, objs: &mut [ExecObj], ring: u64) -> bool {
    let mut eb = Execbuf2 { buffers: objs.as_mut_ptr() as u64, count: objs.len() as u32,
        flags: EXEC_NO_RELOC | EXEC_BATCH_FIRST | ring, ..Default::default() };
    io(fd, I915_GEM_EXECBUFFER2_WR, &mut eb).is_ok()
}

fn wait_bo(fd: c_int, h: u32) -> bool {
    let mut gw = GemWait { handle: h, flags: 0, timeout: 5_000_000_000 };
    io(fd, I915_GEM_WAIT, &mut gw).is_ok()
}

fn obj(h: u32, va: u64, write: bool) -> ExecObj {
    ExecObj { handle: h, offset: canon(va), flags: PINNED | B48 | if write { WRITE } else { 0 }, ..Default::default() }
}

/// Dword index of line `k` in iteration `i`: 64 lines 1 KiB apart, the dword
/// inside the line moving with `i`.
fn slot(i: u32, k: u32) -> usize { (k * 256 + (i * 5 + k) % 16) as usize }

unsafe fn coh_copy(fd: c_int, ring: u64, name: &[u8]) {
    let (sh, dh, bh) = (gem_create(fd, COH_SIZE), gem_create(fd, COH_SIZE), gem_create(fd, 8192));
    let (sp, dp, bp) = (map(fd, sh, COH_SIZE as usize), map(fd, dh, COH_SIZE as usize), map(fd, bh, 8192));
    if sh == 0 || dh == 0 || bh == 0 || sp.is_null() || dp.is_null() || bp.is_null() { report(name, false); return; }
    let (mut bad, mut errs) = (0u32, 0u32);
    for i in 0..COH_ITERS {
        for k in 0..64 { *sp.add(slot(i, k)) = mix(i, k); }
        let b = core::slice::from_raw_parts_mut(bp, 2048);
        let mut at = 0;
        for k in 0..64 {
            let off = slot(i, k) as u64 * 4;
            at = copy_mm(b, at, DST_VA + off, SRC_VA + off);
        }
        b[at] = 0x0A << 23;
        let mut objs = [obj(bh, CB_VA, false), obj(sh, SRC_VA, false), obj(dh, DST_VA, true)];
        if !exec(fd, &mut objs, ring) || !wait_bo(fd, dh) { errs += 1; continue; }
        let wrong = (0..64).filter(|&k| *dp.add(slot(i, k)) != mix(i, k)).count();
        if wrong != 0 {
            if bad < 3 { note(b"  iteration << 32 | stale lines", (i as u64) << 32 | wrong as u64); }
            bad += 1;
        }
    }
    note(b"  iterations with stale data", bad as u64);
    if errs != 0 { note(b"  exec/wait errors", errs as u64); }
    report(name, bad == 0 && errs == 0);
    munmap(sp as *mut c_void, COH_SIZE as usize);
    munmap(dp as *mut c_void, COH_SIZE as usize);
    munmap(bp as *mut c_void, 8192);
    let _ = (gem_close(fd, sh), gem_close(fd, dh), gem_close(fd, bh));
}

unsafe fn coh_cross_engine(fd: c_int) {
    let name = b"coh_cross_engine_implicit_sync";
    let (sh, dh, b1, b2) = (gem_create(fd, COH_SIZE), gem_create(fd, COH_SIZE), gem_create(fd, 8192), gem_create(fd, 8192));
    let (dp, p1, p2) = (map(fd, dh, COH_SIZE as usize), map(fd, b1, 8192), map(fd, b2, 8192));
    if sh == 0 || dh == 0 || b1 == 0 || b2 == 0 || dp.is_null() || p1.is_null() || p2.is_null() { report(name, false); return; }
    let (mut bad, mut errs) = (0u32, 0u32);
    for i in 0..COH_ITERS {
        let w = core::slice::from_raw_parts_mut(p1, 2048);
        let c = core::slice::from_raw_parts_mut(p2, 2048);
        let (mut a, mut z) = (0, 0);
        for r in 0..4 {
            for k in 0..16 {
                let off = slot(i, k) as u64 * 4;
                a = sdi(w, a, SRC_VA + off, if r == 3 { mix(i, k) } else { !0 });
            }
        }
        for k in 0..16 {
            let off = slot(i, k) as u64 * 4;
            z = copy_mm(c, z, DST_VA + off, SRC_VA + off);
        }
        w[a] = 0x0A << 23;
        c[z] = 0x0A << 23;
        let mut o1 = [obj(b1, CB_VA, false), obj(sh, SRC_VA, true)];
        let mut o2 = [obj(b2, CB_VA + 0x10000, false), obj(sh, SRC_VA, false), obj(dh, DST_VA, true)];
        // bcs writes, rcs reads right behind it: the kernel must order them.
        if !exec(fd, &mut o1, 3) || !exec(fd, &mut o2, 1) || !wait_bo(fd, dh) || !wait_bo(fd, sh) { errs += 1; continue; }
        if (0..16).any(|k| *dp.add(slot(i, k)) != mix(i, k)) { bad += 1; }
    }
    note(b"  iterations with stale data", bad as u64);
    if errs != 0 { note(b"  exec/wait errors", errs as u64); }
    report(name, bad == 0 && errs == 0);
    munmap(dp as *mut c_void, COH_SIZE as usize);
    munmap(p1 as *mut c_void, 8192);
    munmap(p2 as *mut c_void, 8192);
    let _ = (gem_close(fd, sh), gem_close(fd, dh), gem_close(fd, b1), gem_close(fd, b2));
}

unsafe fn coh_two_process(fd: c_int) {
    let name = b"coh_dmabuf_two_process";
    let (sh, dh, bh) = (gem_create(fd, COH_SIZE), gem_create(fd, COH_SIZE), gem_create(fd, 8192));
    let (dp, bp) = (map(fd, dh, COH_SIZE as usize), map(fd, bh, 8192));
    let mut ph = PrimeHandle { handle: sh, flags: 2 /* RDWR, inherited by the child */, fd: -1 };
    let mut c2p = [-1; 2];
    let mut p2c = [-1; 2];
    if sh == 0 || dh == 0 || bh == 0 || dp.is_null() || bp.is_null() || io(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut ph).is_err()
        || pipe(c2p.as_mut_ptr()) != 0 || pipe(p2c.as_mut_ptr()) != 0 {
        report(name, false);
        return;
    }
    let pid = fork();
    if pid == 0 {
        // Producer: its own open (own VM), the object through PRIME.
        let cfd = open(b"/dev/dri/renderD128\0".as_ptr(), O_RDWR);
        let mut im = PrimeHandle { handle: 0, flags: 0, fd: ph.fd };
        if cfd < 0 || io(cfd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &mut im).is_err() { _exit(2); }
        let cb = gem_create(cfd, 8192);
        let cp = map(cfd, cb, 8192);
        if cb == 0 || cp.is_null() { _exit(2); }
        let mut tok = [0u8; 4];
        for i in 0..COH_ITERS {
            let b = core::slice::from_raw_parts_mut(cp, 2048);
            let mut a = 0;
            // Several passes so the consumer's copy is queued while this runs.
            for r in 0..4 {
                for k in 0..16 { a = sdi(b, a, SH_VA + slot(i, k) as u64 * 4, if r == 3 { mix(i, k) } else { !0 }); }
            }
            b[a] = 0x0A << 23;
            let mut o = [obj(cb, CB_VA, false), obj(im.handle, SH_VA, true)];
            if !exec(cfd, &mut o, 1) { _exit(3); }
            if write(c2p[1], tok.as_ptr() as *const c_void, 4) != 4 { _exit(4); }
            if read(p2c[0], tok.as_mut_ptr() as *mut c_void, 4) != 4 { _exit(5); }
            // The batch buffer is rewritten next: our own request must be done.
            let _ = wait_bo(cfd, cb);
        }
        _exit(0);
    }
    let (mut bad, mut errs) = (0u32, 0u32);
    if pid > 0 {
        let mut tok = [0u8; 4];
        for i in 0..COH_ITERS {
            if read(c2p[0], tok.as_mut_ptr() as *mut c_void, 4) != 4 { errs += 1; break; }
            let b = core::slice::from_raw_parts_mut(bp, 2048);
            let mut z = 0;
            for k in 0..16 {
                let off = slot(i, k) as u64 * 4;
                z = copy_mm(b, z, DST_VA + off, SH_VA + off);
            }
            b[z] = 0x0A << 23;
            // No wait on the producer: the copy must still see its writes.
            let mut o = [obj(bh, CB_VA, false), obj(sh, SH_VA, false), obj(dh, DST_VA, true)];
            if !exec(fd, &mut o, 1) || !wait_bo(fd, dh) { errs += 1; }
            else if (0..16).any(|k| *dp.add(slot(i, k)) != mix(i, k)) { bad += 1; }
            if write(p2c[1], tok.as_ptr() as *const c_void, 4) != 4 { errs += 1; break; }
        }
        let mut st = 0;
        waitpid(pid, &mut st, 0);
        if st != 0 { note(b"  producer exit status", st as u64); errs += 1; }
    } else {
        errs += 1;
    }
    note(b"  iterations with stale data", bad as u64);
    if errs != 0 { note(b"  exec/wait/pipe errors", errs as u64); }
    report(name, pid > 0 && bad == 0 && errs == 0);
    for f in [c2p[0], c2p[1], p2c[0], p2c[1], ph.fd] { close(f); }
    munmap(dp as *mut c_void, COH_SIZE as usize);
    munmap(bp as *mut c_void, 8192);
    let _ = (gem_close(fd, sh), gem_close(fd, dh), gem_close(fd, bh));
}

unsafe fn coherency(fd: c_int) {
    out(b"--- i915test: coherency (CPU WB <-> GPU, cross-engine, cross-process) ---\n");
    coh_copy(fd, 1, b"coh_cpu_gpu_cpu_rcs");
    coh_copy(fd, 3, b"coh_cpu_gpu_cpu_bcs");
    coh_cross_engine(fd);
    coh_two_process(fd);
    ctx_engine_slots(fd);
}

/// One logical context per engine-map slot (lane/compfix). iris on gen9
/// creates ONE context with the engine map [rcs0, rcs0]: render batches on
/// slot 0, compute batches on slot 1, each relying on its own hardware state
/// (PIPELINE_SELECT, STATE_BASE_ADDRESS emitted once). Linux gives each slot
/// its own LRC image; sharing one made every render batch after the first
/// compute flush draw and blit nothing (boot 16, i915gltest comp-*). Here
/// slot 0 loads CS_GPR0 = A, slot 1 loads B, the default (map-less) context
/// loads C — all context-saved — then each stores its CS_GPR0: every one
/// must still see its own value.
unsafe fn ctx_engine_slots(fd: c_int) {
    let name = b"ctx_engine_slots_own_state";
    const GPR0: u32 = 0x2600; // RCS CS_GPR0 (low dword)
    let mut engines = [0u16; 8]; // u64 extensions, then (class, instance) x2: rcs0, rcs0
    let mut sp_eng = CtxSetparamExt { base: UserExt::default(),
        p: CtxParam { ctx: 0, size: 16, param: 0xa, value: engines.as_mut_ptr() as u64 } };
    let mut cc = CtxCreateExt { id: 0, flags: 1, ext: &mut sp_eng as *mut CtxSetparamExt as u64 };
    let (bh, dh) = (gem_create(fd, 8192), gem_create(fd, 4096));
    let (bp, dp) = (map(fd, bh, 8192), map(fd, dh, 4096));
    if io(fd, I915_GEM_CONTEXT_CREATE_EXT, &mut cc).is_err() || cc.id == 0 || bh == 0 || dh == 0 || bp.is_null() || dp.is_null() {
        report(name, false);
        return;
    }
    let ctx = cc.id;
    let b = core::slice::from_raw_parts_mut(bp, 2048);
    let lri = |b: &mut [u32], at: usize, v: u32| {
        b[at] = (0x22 << 23) | 1; b[at + 1] = GPR0; b[at + 2] = v; b[at + 3] = 0x0A << 23;
    };
    let srm = |b: &mut [u32], at: usize, va: u64| {
        b[at] = (0x24 << 23) | 2; b[at + 1] = GPR0; b[at + 2] = va as u32; b[at + 3] = (va >> 32) as u32; b[at + 4] = 0x0A << 23;
    };
    // Six batches 256 bytes apart in one object (batch_start_offset).
    lri(b, 0, 0xA0A0_0001);   // slot 0
    lri(b, 64, 0xB1B1_0002);  // slot 1
    lri(b, 128, 0xC0C0_0003); // default context, legacy rcs
    srm(b, 192, DST_VA);
    srm(b, 256, DST_VA + 4);
    srm(b, 320, DST_VA + 8);
    for i in 0..4 { *dp.add(i) = 0xDEAD_0000 | i as u32; }
    // (batch_start_offset, ring / map slot, context)
    let steps: [(u32, u64, u32); 6] = [(0, 0, ctx), (256, 1, ctx), (512, 1, 0), (768, 0, ctx), (1024, 1, ctx), (1280, 1, 0)];
    let mut errs = 0u32;
    for &(start, ring, c) in steps.iter() {
        let mut objs = [obj(bh, CB_VA, false), obj(dh, DST_VA, true)];
        let mut eb = Execbuf2 { buffers: objs.as_mut_ptr() as u64, count: 2, start,
            flags: EXEC_NO_RELOC | EXEC_BATCH_FIRST | ring, rsvd1: c as u64, ..Default::default() };
        if io(fd, I915_GEM_EXECBUFFER2_WR, &mut eb).is_err() { errs += 1; }
    }
    if !wait_bo(fd, dh) { errs += 1; }
    let got = [*dp, *dp.add(1), *dp.add(2)];
    note(b"  slot 0 CS_GPR0", got[0] as u64);
    note(b"  slot 1 CS_GPR0", got[1] as u64);
    note(b"  default context CS_GPR0", got[2] as u64);
    if errs != 0 { note(b"  exec/wait errors", errs as u64); }
    let ok = errs == 0 && got == [0xA0A0_0001, 0xB1B1_0002, 0xC0C0_0003];
    if !ok && got[0] == 0xB1B1_0002 {
        out(b"    slot 0 read slot 1's CS_GPR0: the engine-map slots share one context image\n");
    }
    report(name, ok);
    let mut cd = HandleVal { handle: ctx, val: 0 };
    let _ = io(fd, I915_GEM_CONTEXT_DESTROY, &mut cd);
    munmap(bp as *mut c_void, 8192);
    munmap(dp as *mut c_void, 4096);
    let _ = (gem_close(fd, bh), gem_close(fd, dh));
}

#[no_mangle]
pub unsafe extern "C" fn i915_main(argc: isize, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    if argc > 1 && !argv.is_null() {
        let a = *argv.add(1);
        if !a.is_null() && *a == b'-' && *a.add(1) == b'v' { VERBOSE = true; }
        if !a.is_null() && *a == b'c' && *a.add(1) == b'o' && *a.add(2) == b'h' { COH_ONLY = true; }
    }
    out(b"--- i915test: G3 i915 UAPI ---\n");
    let fd = open(b"/dev/dri/renderD128\0".as_ptr(), O_RDWR);
    report(b"open_render_node", fd >= 0);
    if fd < 0 { return 1; }

    // ── identity ─────────────────────────────────────────────────────────────
    let mut name = [0u8; 32];
    let mut v = DrmVersion { name_len: name.len(), name: name.as_mut_ptr() as u64, ..Default::default() };
    let ok = io(fd, DRM_IOCTL_VERSION, &mut v).is_ok() && &name[..v.name_len.min(32)] == b"i915";
    report(b"version_name_i915", ok);
    if !ok { out(b"    (not an i915 node: boot with i915.fake=2 in QEMU, i915.uapi=1 on the laptop)\n"); return 1; }
    if COH_ONLY {
        // `i915test coh` (init runs it for i915.autotest=1): only the
        // coherency checks, nothing that hangs or resets an engine.
        coherency(fd);
        close(fd);
        out(b"--- i915test coh done, failures = ");
        out_num(FAILS as u64, false);
        out(b" ---\n");
        return FAILS;
    }
    let mut buf = [0u8; 64];
    let n = read_file(b"/sys/dev/char/226:128/device/vendor\0", &mut buf);
    report(b"sysfs_vendor_8086", n >= 6 && &buf[..6] == b"0x8086");
    let n = read_file(b"/sys/dev/char/226:128/device/device\0", &mut buf);
    let chip = getparam(fd, 4).unwrap_or(-1);
    let mut want = [0u8; 6];
    want[..2].copy_from_slice(b"0x");
    for i in 0..4 { want[2 + i] = b"0123456789abcdef"[((chip >> (12 - 4 * i)) & 0xF) as usize]; }
    report(b"sysfs_device_matches_chipset_id", n >= 6 && buf[..6] == want);
    note(b"CHIPSET_ID", chip as u64);
    report(b"getparam_chipset_id", chip > 0);
    let eus = getparam(fd, 34).unwrap_or(0);
    note(b"EU_TOTAL", eus as u64);
    report(b"getparam_eu_total", eus > 0);
    report(b"getparam_softpin_fence_array_batch_first",
           getparam(fd, 37) == Some(1) && getparam(fd, 49) == Some(1) && getparam(fd, 48) == Some(1));
    report(b"getparam_mmap_gtt_version_4", getparam(fd, 40) == Some(4));
    let ts_hz = getparam(fd, 51).unwrap_or(0);
    note(b"CS_TIMESTAMP_FREQUENCY", ts_hz as u64);
    report(b"getparam_cs_timestamp_frequency", ts_hz > 0);
    let mut gp = Getparam { param: 9999, _pad: 0, value: &mut 0i32 as *mut i32 as u64 };
    report(b"getparam_unknown_einval", io(fd, I915_GETPARAM, &mut gp) == Err(EINVAL));

    let mut qb = [0u8; 512];
    let l = query(fd, 1, &mut qb); // topology
    let slice_mask = qb[16] as u32; // data[] follows the 16-byte header
    report(b"query_topology", l > 0 && slice_mask & 1 == 1);
    let l = query(fd, 2, &mut qb); // engines
    let neng = u32::from_le_bytes([qb[0], qb[1], qb[2], qb[3]]);
    note(b"engines", neng as u64);
    report(b"query_engine_info_rcs_first", l == 16 + 56 * neng as i32 && neng >= 1 && qb[16] == 0 && qb[17] == 0);
    let l = query(fd, 4, &mut qb); // memory regions
    let total = u64::from_le_bytes(qb[24..32].try_into().unwrap());
    note(b"system memory", total);
    report(b"query_memory_regions_system", l > 0 && qb[0] == 1 && qb[16] == 0 && total > 0);
    report(b"query_hwconfig_enodev", query(fd, 5, &mut qb) == -ENODEV);
    let mut ap = Aperture::default();
    report(b"get_aperture", io(fd, I915_GEM_GET_APERTURE, &mut ap).is_ok() && ap.size > 0);

    // ── memory ───────────────────────────────────────────────────────────────
    let h = gem_create(fd, 5000);
    report(b"gem_create", h != 0);
    let mut regions = [0u32; 1]; // class 0 (system), instance 0
    let mut rx = ExtRegions { base: UserExt { name: 0, ..Default::default() }, num: 1, regions: regions.as_mut_ptr() as u64, ..Default::default() };
    let mut cx = GemCreateExt { size: 4096, ext: &mut rx as *mut ExtRegions as u64, ..Default::default() };
    let r = io(fd, I915_GEM_CREATE_EXT, &mut cx);
    report(b"gem_create_ext_system_region", r.is_ok() && cx.handle != 0 && cx.size == 4096);
    let p = map(fd, h, 8192);
    report(b"mmap_offset_wb", !p.is_null());
    if !p.is_null() {
        let mut data = [0u32; 4];
        data.copy_from_slice(&[0x11, 0x22, 0x33, 0x44]);
        let mut w = Prw { handle: h, offset: 4096, size: 16, data: data.as_mut_ptr() as u64, ..Default::default() };
        let pw = io(fd, I915_GEM_PWRITE, &mut w).is_ok();
        report(b"pwrite_visible_in_mapping", pw && *p.add(1024) == 0x11 && *p.add(1027) == 0x44 && *p == 0);
        *p.add(5) = 0xABCD;
        let mut back = [0u32; 1];
        let mut rd = Prw { handle: h, offset: 20, size: 4, data: back.as_mut_ptr() as u64, ..Default::default() };
        report(b"pread_sees_cpu_write", io(fd, I915_GEM_PREAD, &mut rd).is_ok() && back[0] == 0xABCD);
        let mut oob = Prw { handle: h, offset: 8190, size: 16, data: back.as_mut_ptr() as u64, ..Default::default() };
        report(b"pread_out_of_range_einval", io(fd, I915_GEM_PREAD, &mut oob) == Err(EINVAL));
    }
    let mut t = Tiling { handle: h, mode: 1, stride: 512, swizzle: 7 };
    let set = io(fd, I915_GEM_SET_TILING, &mut t).is_ok() && t.swizzle == 0;
    let mut gt = Tiling { handle: h, ..Default::default() };
    report(b"tiling_round_trip", set && io(fd, I915_GEM_GET_TILING, &mut gt).is_ok() && gt.mode == 1);
    let mut c = HandleVal { handle: h, val: 99 };
    report(b"get_caching_cached", io(fd, I915_GEM_GET_CACHING, &mut c).is_ok() && c.val == 1);
    let mut m = Madvise { handle: h, madv: 1, retained: 0 };
    report(b"madvise_retained", io(fd, I915_GEM_MADVISE, &mut m).is_ok() && m.retained == 1);
    let mut d = SetDomain { handle: h, read: 1, write: 1 };
    report(b"set_domain_cpu", io(fd, I915_GEM_SET_DOMAIN, &mut d).is_ok());
    // Handles are small and dense from 1 (iris sizes arrays by the largest).
    report(b"gem_handles_dense", h >= 1 && h < 16 && cx.handle == h + 1);

    // ── contexts and VMs ─────────────────────────────────────────────────────
    let mut vm = VmControl::default();
    let vm_ok = io(fd, I915_GEM_VM_CREATE, &mut vm).is_ok() && vm.id != 0;
    report(b"vm_create", vm_ok);
    // Engine map: rcs0, bcs0 (class/instance pairs after the u64 extensions).
    let mut engines = [0u16; 8];
    engines[4] = 0; engines[5] = 0; engines[6] = 1; engines[7] = 0;
    let mut sp_eng = CtxSetparamExt { base: UserExt::default(),
        p: CtxParam { ctx: 0, size: 16, param: 0xa, value: engines.as_mut_ptr() as u64 } };
    let mut sp_vm = CtxSetparamExt { base: UserExt { next: &mut sp_eng as *mut CtxSetparamExt as u64, ..Default::default() },
        p: CtxParam { ctx: 0, size: 0, param: 0x9, value: vm.id as u64 } };
    let mut cc = CtxCreateExt { id: 0, flags: 1, ext: &mut sp_vm as *mut CtxSetparamExt as u64 };
    let ctx_ok = io(fd, I915_GEM_CONTEXT_CREATE_EXT, &mut cc).is_ok() && cc.id != 0;
    report(b"context_create_ext_vm_engines", ctx_ok);
    let ctx = cc.id;
    let mut gp = CtxParam { ctx, param: 0x3, ..Default::default() };
    report(b"context_getparam_gtt_size_2_48", io(fd, I915_GEM_CONTEXT_GETPARAM, &mut gp).is_ok() && gp.value == 1 << 48);
    let mut e2 = [0u16; 8];
    let mut gp = CtxParam { ctx, param: 0xa, size: 16, value: e2.as_mut_ptr() as u64 };
    report(b"context_getparam_engines", io(fd, I915_GEM_CONTEXT_GETPARAM, &mut gp).is_ok() && gp.size == 16 && e2[6] == 1);
    let mut gp = CtxParam { ctx: 0, param: 0x9, ..Default::default() };
    report(b"context0_getparam_vm", io(fd, I915_GEM_CONTEXT_GETPARAM, &mut gp).is_ok() && gp.value != 0);

    // ── execution ────────────────────────────────────────────────────────────
    let batch_va: u64 = 0x0000_7FFF_0000_0000; // high, 48-bit
    let tgt_va: u64 = 0x0000_8000_0010_0000;   // bit 47 set: canonical form needed
    let bh = gem_create(fd, 4096);
    let th = gem_create(fd, 4096);
    let bp = map(fd, bh, 4096);
    let tp = map(fd, th, 4096);
    let so = syncobj(fd);
    report(b"exec_setup", bh != 0 && th != 0 && !bp.is_null() && !tp.is_null() && so != 0);
    let mut exec_ran = false;
    if !bp.is_null() && !tp.is_null() {
        let b = core::slice::from_raw_parts_mut(bp, 1024);
        let mut i = sdi(b, 0, tgt_va, 0xC0FF_EE01);
        i = sdi(b, i, tgt_va + 8, 0x5EED_0002);
        // PIPE_CONTROL write-immediate (qword) at +16.
        b[i..i + 6].copy_from_slice(&[0x7A00_0004, 1 << 14, (tgt_va + 16) as u32, (tgt_va >> 32) as u32, 0x1234_5678, 0x9ABC_DEF0]);
        i += 6;
        b[i] = 0x0A << 23; // MI_BATCH_BUFFER_END
        let mut objs = [
            ExecObj { handle: bh, offset: canon(batch_va), flags: PINNED | B48, ..Default::default() },
            ExecObj { handle: th, offset: canon(tgt_va), flags: PINNED | B48 | WRITE, ..Default::default() },
        ];
        let mut fences = [ExecFence { handle: so, flags: 2 /* SIGNAL */ }];
        let mut eb = Execbuf2 { buffers: objs.as_mut_ptr() as u64, count: 2, len: 0, ncliprects: 1,
            cliprects: fences.as_mut_ptr() as u64,
            flags: EXEC_NO_RELOC | EXEC_HANDLE_LUT | EXEC_BATCH_FIRST | EXEC_FENCE_ARRAY, rsvd1: ctx as u64, ..Default::default() };
        match io(fd, I915_GEM_EXECBUFFER2_WR, &mut eb) {
            Ok(()) => {
                exec_ran = true;
                report(b"execbuffer2_rcs_softpin_48bit", true);
            }
            Err(ENODEV) => {
                out(b"    EXECBUFFER2 -> ENODEV: hardware execution not wired on this node (expected with i915.uapi=1)\n");
                report(b"execbuffer2_enodev_on_hw", true);
            }
            Err(e) => { note(b"EXECBUFFER2 errno", e as u64); report(b"execbuffer2_rcs_softpin_48bit", false); }
        }
        if exec_ran {
            let mut hs = [so];
            let mut w = SyncobjWait { handles: hs.as_mut_ptr() as u64, timeout: i64::MAX, count: 1, ..Default::default() };
            report(b"syncobj_wait_out_fence", io(fd, DRM_IOCTL_SYNCOBJ_WAIT, &mut w).is_ok());
            let mut gw = GemWait { handle: th, flags: 0, timeout: 1_000_000_000 };
            let mut bz = HandleVal { handle: th, val: 99 };
            report(b"gem_wait_and_busy_idle", io(fd, I915_GEM_WAIT, &mut gw).is_ok()
                   && io(fd, I915_GEM_BUSY, &mut bz).is_ok() && bz.val == 0);
            note(b"target[0]", *tp as u64);
            report(b"gpu_store_landed_in_bo", *tp == 0xC0FF_EE01 && *tp.add(2) == 0x5EED_0002);
            report(b"pipe_control_qword_write", *tp.add(4) == 0x1234_5678 && *tp.add(5) == 0x9ABC_DEF0);

            // Same objects on the blitter (engine map index 1) with a timeline
            // out-fence at point 7.
            let b = core::slice::from_raw_parts_mut(bp, 1024);
            let i = sdi(b, 0, tgt_va + 32, 0xB17_0003);
            b[i] = 0x0A << 23;
            let tl = syncobj(fd);
            let mut tf = [ExecFence { handle: tl, flags: 2 }];
            let mut pts = [7u64];
            let mut ext = TimelineExt { base: UserExt::default(), count: 1, handles: tf.as_mut_ptr() as u64, values: pts.as_mut_ptr() as u64 };
            let mut eb = Execbuf2 { buffers: objs.as_mut_ptr() as u64, count: 2, cliprects: &mut ext as *mut TimelineExt as u64,
                flags: EXEC_NO_RELOC | EXEC_BATCH_FIRST | EXEC_USE_EXTENSIONS | 1, rsvd1: ctx as u64, ..Default::default() };
            let ok = io(fd, I915_GEM_EXECBUFFER2_WR, &mut eb).is_ok();
            let mut hs = [tl];
            let mut tw = TimelineWait { handles: hs.as_mut_ptr() as u64, points: pts.as_mut_ptr() as u64, timeout: i64::MAX, count: 1, ..Default::default() };
            report(b"execbuffer2_bcs_timeline_fence", ok && io(fd, DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &mut tw).is_ok()
                   && *tp.add(8) == 0xB17_0003);
            let mut dd = HandleVal { handle: tl, val: 0 };
            let _ = io(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &mut dd);

            // A batch that jumps to itself forever: a real hang on hardware
            // (hangcheck resets the engine after i915.hang_ms) and a runaway on
            // the soft engine. Either way the context's GET_RESET_STATS counts
            // it. (A store to an unbound address is NOT a hang on gen9: the
            // write is dropped and the batch completes, as on Linux.)
            let b = core::slice::from_raw_parts_mut(bp, 1024);
            b[0] = (0x31 << 23) | (1 << 8) | 1; // MI_BATCH_BUFFER_START, PPGTT
            b[1] = batch_va as u32;
            b[2] = (batch_va >> 32) as u32;
            b[3] = 0x0A << 23;
            let mut eb = Execbuf2 { buffers: objs.as_mut_ptr() as u64, count: 2,
                flags: EXEC_NO_RELOC | EXEC_BATCH_FIRST, rsvd1: ctx as u64, ..Default::default() };
            let _ = io(fd, I915_GEM_EXECBUFFER2_WR, &mut eb);
            let mut gw = GemWait { handle: bh, flags: 0, timeout: 30_000_000_000 };
            let _ = io(fd, I915_GEM_WAIT, &mut gw);
            let mut rs = ResetStats { ctx, ..Default::default() };
            report(b"faulting_batch_in_reset_stats", io(fd, I915_GET_RESET_STATS, &mut rs).is_ok() && rs.active == 1);
            let mut rs0 = ResetStats { ctx: 0, ..Default::default() };
            report(b"default_context_innocent", io(fd, I915_GET_RESET_STATS, &mut rs0).is_ok() && rs0.active == 0);
        }
        // Validation (answered before execution on any node).
        let mut o1 = objs;
        o1[1].flags = B48; // not pinned
        let mut eb = Execbuf2 { buffers: o1.as_mut_ptr() as u64, count: 2, flags: EXEC_NO_RELOC | EXEC_BATCH_FIRST,
            rsvd1: ctx as u64, ..Default::default() };
        report(b"execbuffer2_unpinned_einval", io(fd, I915_GEM_EXECBUFFER2_WR, &mut eb) == Err(EINVAL));
        let mut o2 = objs;
        o2[1].offset = tgt_va; // non-canonical
        let mut eb = Execbuf2 { buffers: o2.as_mut_ptr() as u64, count: 2, flags: EXEC_NO_RELOC | EXEC_BATCH_FIRST,
            rsvd1: ctx as u64, ..Default::default() };
        report(b"execbuffer2_noncanonical_einval", io(fd, I915_GEM_EXECBUFFER2_WR, &mut eb) == Err(EINVAL));
        let mut eb = Execbuf2 { buffers: objs.as_mut_ptr() as u64, count: 2, flags: EXEC_NO_RELOC | EXEC_BATCH_FIRST,
            rsvd1: 4242, ..Default::default() };
        report(b"execbuffer2_unknown_context_enoent", io(fd, I915_GEM_EXECBUFFER2_WR, &mut eb) == Err(ENOENT));
    }
    let mut rr = RegRead { offset: 0x2358 | 1, val: 0 };
    let a = io(fd, I915_REG_READ, &mut rr).is_ok();
    let t0 = rr.val;
    let _ = io(fd, I915_REG_READ, &mut rr);
    note(b"RCS timestamp", rr.val);
    report(b"reg_read_timestamp", a && rr.val > 0 && rr.val >= t0);

    // ── KMS (G4a, only when the i915 KMS backend registered: i915.kms=1) ─────
    // ADDFB2 of an i915 object (LINEAR XR24, larger than any GOP mode the
    // laptop or QEMU boots with) binds it into the scanout GGTT window; an
    // X-tiled modifier is refused (IN_FORMATS offers LINEAR only); RMFB
    // unbinds and drops the framebuffer's reference.
    let mut cap = GetCap { capability: 0x10 /* ADDFB2_MODIFIERS */, value: 0 };
    if io(fd, DRM_IOCTL_GET_CAP, &mut cap).is_ok() && cap.value == 1 {
        let (w, hgt, pitch) = (2560u32, 1600u32, 10240u32);
        let fbh = gem_create(fd, pitch as u64 * hgt as u64);
        let mut fc = FbCmd2 { width: w, height: hgt, pixel_format: 0x3432_5258, flags: 2, ..Default::default() };
        fc.handles[0] = fbh;
        fc.pitches[0] = pitch;
        let added = io(fd, DRM_IOCTL_MODE_ADDFB2, &mut fc).is_ok() && fc.fb_id != 0;
        report(b"kms_addfb2_linear", added);
        let mut ft = FbCmd2 { width: w, height: hgt, pixel_format: 0x3432_5258, flags: 2, ..Default::default() };
        ft.handles[0] = fbh;
        ft.pitches[0] = pitch;
        ft.modifier[0] = 0x0100_0000_0000_0001; // I915_FORMAT_MOD_X_TILED
        report(b"kms_addfb2_xtiled_einval", io(fd, DRM_IOCTL_MODE_ADDFB2, &mut ft) == Err(EINVAL));
        let mut id = fc.fb_id;
        report(b"kms_rmfb", added && io(fd, DRM_IOCTL_MODE_RMFB, &mut id).is_ok());
        let _ = gem_close(fd, fbh);
    } else {
        out(b"    (no i915 KMS backend: boot with i915.kms=1 to test ADDFB2)\n");
    }

    // ── sharing ──────────────────────────────────────────────────────────────
    let mut ph = PrimeHandle { handle: th, flags: 0x80002 /* RDWR|CLOEXEC */, fd: -1 };
    let ex = io(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut ph).is_ok() && ph.fd >= 0;
    let mut im = PrimeHandle { handle: 0, flags: 0, fd: ph.fd };
    report(b"prime_export_import_same_handle", ex && io(fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &mut im).is_ok() && im.handle == th);
    if ph.fd >= 0 { close(ph.fd); }

    scanout_mmap(fd);

    if exec_ran { coherency(fd); }

    // ── teardown ─────────────────────────────────────────────────────────────
    if !bp.is_null() { munmap(bp as *mut c_void, 4096); }
    if !tp.is_null() { munmap(tp as *mut c_void, 4096); }
    if !p.is_null() { munmap(p as *mut c_void, 8192); }
    let closes = gem_close(fd, bh).is_ok() && gem_close(fd, th).is_ok() && gem_close(fd, h).is_ok() && gem_close(fd, cx.handle).is_ok();
    let mut gone = MmapOffset { handle: h, flags: 2, ..Default::default() };
    report(b"gem_close_all", closes && io(fd, I915_GEM_MMAP_OFFSET, &mut gone) == Err(ENOENT));
    let mut cd = HandleVal { handle: ctx, val: 0 };
    let first = io(fd, I915_GEM_CONTEXT_DESTROY, &mut cd).is_ok();
    report(b"context_destroy_twice", first && io(fd, I915_GEM_CONTEXT_DESTROY, &mut cd) == Err(ENOENT));
    let mut vd = VmControl { id: vm.id, ..Default::default() };
    report(b"vm_destroy", io(fd, I915_GEM_VM_DESTROY, &mut vd).is_ok() && io(fd, I915_GEM_VM_DESTROY, &mut vd) == Err(ENOENT));
    close(fd);

    out(b"--- i915test done, failures = ");
    out_num(FAILS as u64, false);
    out(b" ---\n");
    FAILS
}

// ── scanout-style object: GEM mmap vs dma-buf mmap vs PREAD ─────────────────
//
// What iris does for a gbm_bo_create(LINEAR, SCANOUT|RENDERING) that the same
// process re-imports through eglCreateImage: GEM_CREATE_EXT {system region},
// SET_DOMAIN(CPU), PRIME export, FD_TO_HANDLE (same handle), then its CPU
// reads/writes go through MMAP_OFFSET(WB or WC) + mmap(drm fd, token) while a
// compositor-style client uses mmap(dma-buf fd). All views must alias the
// object's own pages, on the render node and on card0, at sizes whose buddy
// order exceeds the request.
fn fill(p: *mut u32, words: usize, v: u32) { for i in 0..words { unsafe { p.add(i).write_volatile(v ^ i as u32); } } }
fn check(p: *const u32, words: usize, v: u32) -> bool { (0..words).all(|i| unsafe { p.add(i).read_volatile() } == v ^ i as u32) }

unsafe fn gem_view(fd: c_int, h: u32, size: usize, wc: bool) -> *mut u32 {
    let mut m = MmapOffset { handle: h, flags: if wc { 1 } else { 2 }, ..Default::default() };
    if io(fd, I915_GEM_MMAP_OFFSET, &mut m).is_err() { return core::ptr::null_mut(); }
    let p = mmap(core::ptr::null_mut(), size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, m.offset as i64);
    if p as isize == -1 { core::ptr::null_mut() } else { p as *mut u32 }
}

unsafe fn scanout_views(rfd: c_int, cfd: c_int, size: u64, tag: u32) -> bool {
    let mut regions = [0u32; 1];
    let mut rx = ExtRegions { base: UserExt { name: 0, ..Default::default() }, num: 1, regions: regions.as_mut_ptr() as u64, ..Default::default() };
    let mut cx = GemCreateExt { size, ext: &mut rx as *mut ExtRegions as u64, ..Default::default() };
    if io(rfd, I915_GEM_CREATE_EXT, &mut cx).is_err() || cx.handle == 0 { note(b"  create_ext failed", size); return false; }
    let h = cx.handle;
    let len = cx.size as usize;
    let words = len / 4;
    let mut sd = SetDomain { handle: h, read: 1 /* CPU */, write: 0 };
    let _ = io(rfd, I915_GEM_SET_DOMAIN, &mut sd);
    let mut ph = PrimeHandle { handle: h, flags: 0x80002, fd: -1 };
    if io(rfd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut ph).is_err() || ph.fd < 0 { note(b"  export failed", size); return false; }
    let mut im = PrimeHandle { handle: 0, flags: 0, fd: ph.fd };
    let same = io(rfd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &mut im).is_ok() && im.handle == h;
    let mut ic = PrimeHandle { handle: 0, flags: 0, fd: ph.fd };
    let card_h = if cfd >= 0 && io(cfd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &mut ic).is_ok() { ic.handle } else { 0 };
    let d = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, ph.fd, 0);
    let d = if d as isize == -1 { core::ptr::null_mut() } else { d as *mut u32 };
    let wb = gem_view(rfd, h, len, false);
    let wc = gem_view(rfd, h, len, true);
    let cw = if card_h != 0 { gem_view(cfd, card_h, len, false) } else { core::ptr::null_mut() };
    let mut ok = same && !d.is_null() && !wb.is_null() && !wc.is_null() && (cfd < 0 || !cw.is_null());
    if !ok { note(b"  views missing for size", size); }
    let back = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, 0x22 /* PRIVATE|ANON */, -1, 0) as *mut u32;
    if back as isize == -1 { return false; }
    let pread = |v: u32| -> bool {
        let mut rd = Prw { handle: h, offset: 0, size: len as u64, data: back as u64, ..Default::default() };
        io(rfd, I915_GEM_PREAD, &mut rd).is_ok() && check(back, words, v)
    };
    if ok {
        // fresh object: zeros everywhere
        ok &= (0..words).all(|i| d.add(i).read_volatile() == 0 && wb.add(i).read_volatile() == 0);
        if !ok { note(b"  fresh not zero, size", size); }
        // GEM WB write -> dma-buf, WC, card, PREAD
        let v1 = 0x1122_0000 | tag;
        fill(wb, words, v1);
        let a = check(d, words, v1) && check(wc, words, v1) && (cw.is_null() || check(cw, words, v1)) && pread(v1);
        if !a { note(b"  gem-wb write not seen, size", size); }
        // dma-buf write -> GEM WB/WC/card, PREAD
        let v2 = 0x4455_0000 | tag;
        fill(d, words, v2);
        let b = check(wb, words, v2) && check(wc, words, v2) && (cw.is_null() || check(cw, words, v2)) && pread(v2);
        if !b { note(b"  dma-buf write not seen, size", size); }
        // card-node GEM write -> render-node views
        let v3 = 0x7788_0000 | tag;
        let c = cw.is_null() || { fill(cw, words, v3); check(wb, words, v3) && check(d, words, v3) && pread(v3) };
        if !c { note(b"  card-node write not seen, size", size); }
        ok &= a && b && c;
    }
    for p in [d, wb, wc, cw] { if !p.is_null() { munmap(p as *mut c_void, len); } }
    munmap(back as *mut c_void, len);
    if card_h != 0 { let _ = gem_close(cfd, card_h); }
    close(ph.fd);
    let _ = gem_close(rfd, h);
    ok
}

unsafe fn scanout_mmap(fd: c_int) {
    let cfd = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
    let mut ok = true;
    // 256x256 XRGB (exact order), +1 page and 1920x1080 XRGB (order > size).
    for (i, &sz) in [262_144u64, 266_240, 8_294_400].iter().enumerate() { ok &= scanout_views(fd, cfd, sz, i as u32); }
    report(b"scanout_gem_mmap_aliases_dmabuf", ok);
    // A token this device never handed out must fail, never map other memory.
    let p = mmap(core::ptr::null_mut(), 4096, PROT_READ, MAP_SHARED, fd, 0x7_0000_0000);
    report(b"mmap_unknown_token_fails", p as isize == -1);
    if p as isize != -1 { munmap(p, 4096); }
    if cfd >= 0 { close(cfd); }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    out(b"i915test: PANIC\n");
    loop {}
}
