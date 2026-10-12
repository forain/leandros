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

#[no_mangle]
pub unsafe extern "C" fn i915_main(argc: isize, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    if argc > 1 && !argv.is_null() {
        let a = *argv.add(1);
        if !a.is_null() && *a == b'-' && *a.add(1) == b'v' { VERBOSE = true; }
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
    report(b"gem_close_bogus_enoent", gem_close(fd, 0x4FFF_FFF0) == Err(ENOENT));

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

            // A store to an address nothing is bound at: the soft engine stops,
            // the context's GET_RESET_STATS counts it.
            let b = core::slice::from_raw_parts_mut(bp, 1024);
            let i = sdi(b, 0, 0x7000_0000, 1);
            b[i] = 0x0A << 23;
            let mut eb = Execbuf2 { buffers: objs.as_mut_ptr() as u64, count: 2,
                flags: EXEC_NO_RELOC | EXEC_BATCH_FIRST, rsvd1: ctx as u64, ..Default::default() };
            let _ = io(fd, I915_GEM_EXECBUFFER2_WR, &mut eb);
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

    // ── sharing ──────────────────────────────────────────────────────────────
    let mut ph = PrimeHandle { handle: th, flags: 0x80002 /* RDWR|CLOEXEC */, fd: -1 };
    let ex = io(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut ph).is_ok() && ph.fd >= 0;
    let mut im = PrimeHandle { handle: 0, flags: 0, fd: ph.fd };
    report(b"prime_export_import_same_handle", ex && io(fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &mut im).is_ok() && im.handle == th);
    if ph.fd >= 0 { close(ph.fd); }

    // ── teardown ─────────────────────────────────────────────────────────────
    if !bp.is_null() { munmap(bp as *mut c_void, 4096); }
    if !tp.is_null() { munmap(tp as *mut c_void, 4096); }
    if !p.is_null() { munmap(p as *mut c_void, 8192); }
    let closes = gem_close(fd, bh).is_ok() && gem_close(fd, th).is_ok() && gem_close(fd, h).is_ok() && gem_close(fd, cx.handle).is_ok();
    report(b"gem_close_all", closes && gem_close(fd, h) == Err(ENOENT));
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

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    out(b"i915test: PANIC\n");
    loop {}
}
