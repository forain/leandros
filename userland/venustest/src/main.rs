//! venustest — M1 smoke test for the Venus (virtio-gpu 3D) transport.
//!
//! Proves the *transport*, not Vulkan: that the guest driver and the host's
//! virglrenderer agree on the virtio-gpu wire protocol well enough to create a
//! Venus context and read back host-populated capability data.
//!
//! The load-bearing assertion is `get_caps`: a NON-EMPTY, host-populated capset
//! blob for capset id 4 (Venus). "The ioctl returned 0" proves nothing here —
//! the entire risk in this milestone is a wire protocol that is silently wrong
//! while every call reports success — so the capset check counts non-zero bytes
//! and fails on an all-zero buffer even when the ioctl succeeded.
//!
//! Steps: open card0; GETPARAM probes; CONTEXT_INIT(capset=Venus);
//! GET_CAPS + non-zero assertion; RESOURCE_CREATE_BLOB + MAP + mmap
//! write/readback; RESOURCE_INFO (fields checked against what was requested,
//! plus a never-allocated handle that must be refused); EXECBUFFER + WAIT.
//!
//! Phase 2 is the regression for per-open-file 3D contexts, and it asserts on
//! context IDENTITY, not on liveness. That distinction is the whole point:
//! `ioctl(...) == 0` cannot see this bug. With one process-global context slot,
//! fd B's CONTEXT_INIT leaves a *live, valid* context in the global, so a
//! submission on fd A still returns 0 — while executing in fd B's context. So
//! phase 2 reads the context id bound to each open back out of the kernel
//! (`VIRTGPU_PARAM_LEANDROS_CTX_ID`, a LeandrOS-private GETPARAM that answers
//! for the CALLING open) and asserts that two opens get different ids and that
//! one open's id is unperturbed by anything the other open does — a second
//! independent open, a same-fd re-init, a close of the *other* fd, a dup(), and
//! a fork(). The liveness EXECBUFFER checks are kept alongside; they are cheap,
//! but they are not what catches the regression.
//!
//! Phase 3 exhausts the per-open context table (MAX_GPU_CTXS = 16 slots): 16
//! opens must get 16 distinct contexts, the 17th must be refused cleanly rather
//! than panic or alias, and closing them all must return the slots.
//!
//! Phase 4 covers BO handles: that a submission's `bo_handles` are resolved
//! (a handle naming nothing fails the whole EXECBUFFER) and fenced, that
//! VIRTGPU_WAIT answers about the BO its handle names rather than about some
//! global, that a BO belonging to one open is unreachable from another through
//! all four handle-consuming ioctls, and that the "most recent submission"
//! fence is per-open. As in phase 2, identity is what is asserted: submission
//! is synchronous, so every WAIT succeeds either way and only the REFUSALS and
//! the cross-open comparisons can distinguish a correct kernel from the
//! process-global one that preceded it.
//!
//! Prints "<name>: PASS"/"<name>: FAIL"; exit code is the failure count.

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

// ── virtgpu_drm.h ioctl codes ────────────────────────────────────────────────
// DRM_IOWR(DRM_COMMAND_BASE + nr, struct):
//   (3 << 30) | (size << 16) | ('d' << 8) | (0x40 + nr)
const DRM_IOCTL_VIRTGPU_MAP: c_ulong = 0xC0106441;
const DRM_IOCTL_VIRTGPU_EXECBUFFER: c_ulong = 0xC0406442;
const DRM_IOCTL_VIRTGPU_GETPARAM: c_ulong = 0xC0106443;
const DRM_IOCTL_VIRTGPU_RESOURCE_INFO: c_ulong = 0xC0106445;
const DRM_IOCTL_VIRTGPU_WAIT: c_ulong = 0xC0086448;
const DRM_IOCTL_VIRTGPU_GET_CAPS: c_ulong = 0xC0186449;
const DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB: c_ulong = 0xC030644A;
const DRM_IOCTL_VIRTGPU_CONTEXT_INIT: c_ulong = 0xC010644B;

const VIRTGPU_PARAM_3D_FEATURES: u64 = 1;
const VIRTGPU_PARAM_CAPSET_QUERY_FIX: u64 = 2;
const VIRTGPU_PARAM_RESOURCE_BLOB: u64 = 3;
const VIRTGPU_PARAM_HOST_VISIBLE: u64 = 4;
const VIRTGPU_PARAM_CONTEXT_INIT: u64 = 6;
const VIRTGPU_PARAM_SUPPORTED_CAPSET_IDs: u64 = 7;
/// LeandrOS-private (NOT upstream): the 3D context id bound to the calling
/// open, 0 if that open has none. See drivers/src/drm_device_interface.rs.
const VIRTGPU_PARAM_LEANDROS_CTX_ID: u64 = 0x1000_0001;
/// LeandrOS-private: live host-visible window reservations. The only way to see
/// whether the kernel gives back the shared-memory window space a HOST3D blob
/// took — see section 4c.
const VIRTGPU_PARAM_LEANDROS_HOSTVIS_SPANS: u64 = 0x1000_0002;
/// LeandrOS-private: host-visible window length in MiB (0 = no window).
const VIRTGPU_PARAM_LEANDROS_HOSTVIS_MIB: u64 = 0x1000_0003;
/// Low 32 bits of the fence id of the most recent EXECBUFFER *on this open*.
/// Exists so the de-globalization of that fence is assertable — see phase 3.
const VIRTGPU_PARAM_LEANDROS_LAST_FENCE: u64 = 0x1000_0004;

/// A BO handle no allocator here ever hands out: blob handles start at 0x4000
/// and dumb handles far below that. Every "must be refused" assertion uses this
/// one value so they are all testing the same premise.
const NO_SUCH_HANDLE: u32 = 0x7FFF_FFFF;

const VIRTGPU_DRM_CAPSET_VENUS: u32 = 4;

const VIRTGPU_CONTEXT_PARAM_CAPSET_ID: u64 = 0x0001;
const VIRTGPU_CONTEXT_PARAM_NUM_RINGS: u64 = 0x0002;

const VIRTGPU_BLOB_MEM_GUEST: u32 = 0x0001;
/// Host-side blob memory. The storage lives in the host, not in guest RAM; the
/// guest reaches it only through RESOURCE_MAP_BLOB into the virtio-gpu
/// shared-memory BAR window. This is what Mesa's Venus ICD allocates its command
/// ring as, so this is the allocation whose map gates `vkCreateInstance`.
const VIRTGPU_BLOB_MEM_HOST3D: u32 = 0x0002;
const VIRTGPU_BLOB_FLAG_USE_MAPPABLE: u32 = 0x0001;

// ── virtgpu_drm.h structs (fixed width; identical on x86_64 and aarch64) ─────
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmVirtgpuGetparam {
    param: u64,
    value: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmVirtgpuGetCaps {
    cap_set_id: u32,
    cap_set_ver: u32,
    addr: u64,
    size: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmVirtgpuContextSetParam {
    param: u64,
    value: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmVirtgpuContextInit {
    num_params: u32,
    pad: u32,
    ctx_set_params: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmVirtgpuResourceCreateBlob {
    blob_mem: u32,
    blob_flags: u32,
    bo_handle: u32,
    res_handle: u32,
    size: u64,
    pad: u32,
    cmd_size: u32,
    cmd: u64,
    blob_id: u64,
}

/// `struct drm_virtgpu_resource_info` — bo_handle in, the other three out.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmVirtgpuResourceInfo {
    bo_handle: u32,
    res_handle: u32,
    size: u32,
    blob_mem: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmVirtgpuMap {
    offset: u64,
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
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

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmVirtgpu3dWait {
    handle: u32,
    flags: u32,
}

type pid_t = i32;

/// `struct pollfd`.
#[repr(C)]
pub struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

const POLLIN: i16 = 0x0001;
const F_DUPFD_CLOEXEC: c_int = 1030;

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: c_int, buf: *const c_void, count: size_t) -> isize;
    pub fn open(path: *const u8, oflag: c_int, ...) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn lseek(fd: c_int, offset: i64, whence: c_int) -> i64;
    pub fn read(fd: c_int, buf: *mut c_void, count: size_t) -> isize;
    pub fn ftruncate(fd: c_int, length: i64) -> c_int;
    pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    pub fn mmap(addr: *mut c_void, len: size_t, prot: c_int, flags: c_int,
                fd: c_int, offset: i64) -> *mut c_void;
    /// Phase 6 needs this: it closes the exported fd and then asserts the
    /// buffer was released, so it must not still hold a MAP_SHARED view of the
    /// frames when they go back to the allocator.
    pub fn munmap(addr: *mut c_void, len: size_t) -> c_int;

    // Phase 7 (SIMULATE_SYNCOBJ) only. `poll` and `fcntl(F_DUPFD_CLOEXEC)` are
    // there because they are *exactly* what Mesa does with an out-fence fd
    // (`sim_syncobj_poll`, `os_dupfd_cloexec`) — the subtests assert the fd is
    // usable the way its only consumer uses it, not merely that it is a number.
    pub fn poll(fds: *mut PollFd, nfds: u64, timeout: c_int) -> c_int;
    pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;

    // Used only by phase 2 (multi-fd context isolation); same relibc-linked
    // idiom as forktest/polltest (fork/waitpid, dup) rather than raw syscalls.
    pub fn dup(fildes: c_int) -> c_int;
    pub fn fork() -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
    pub fn _exit(status: c_int) -> !;
}

// WEXITSTATUS / WIFEXITED on the musl/Linux wait-status encoding (same as
// forktest): a normal exit is `(code & 0xff) << 8`, low 7 bits = signal.
fn wifexited(status: c_int) -> bool { (status & 0x7f) == 0 }
fn wexitstatus(status: c_int) -> c_int { (status >> 8) & 0xff }

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset venus_main",
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
    "   adrp x1, venus_main",
    "   add x1, x1, :lo12:venus_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

fn out(s: &[u8]) {
    unsafe { write(1, s.as_ptr() as *const c_void, s.len()) };
}

fn out_u64(mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if v == 0 {
        out(b"0");
        return;
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    out(&buf[i..]);
}

fn out_hex(v: u64) {
    let digits = b"0123456789abcdef";
    let mut buf = [0u8; 16];
    for i in 0..16 {
        buf[15 - i] = digits[((v >> (i * 4)) & 0xF) as usize];
    }
    out(b"0x");
    out(&buf);
}

fn report(name: &[u8], ok: bool) -> bool {
    out(name);
    out(if ok { b": PASS\n" } else { b": FAIL\n" });
    ok
}

/// fork() while `map` (a live *device* mapping of `len` bytes, obtained by
/// mmap()ing a VIRTGPU_MAP token) is in the address space, and check both
/// things fork owes such a mapping. Returns the number of FAILs.
///
/// WHY THIS EXISTS. A device VMA is the one mapping class whose pages the
/// kernel does not own: `map_device` records the physical range with the
/// `file_cap == usize::MAX` sentinel, and both teardown paths deliberately drop
/// the PTEs without freeing the frames. fork used to duplicate such a VMA by
/// *copying* it into a fresh buddy allocation, which is wrong twice over:
///
///   1. It cannot work at all when the range is not RAM. For a host-visible
///      (HOST3D) blob the physical base is a PCI BAR address inside the
///      virtio-gpu shared-memory window; the kernel's phys→virt of it lands
///      outside the HHDM, and the copy took the whole machine down in memcpy
///      (`Vector=0x0E RIP=memcpy+0xe CR2=<HHDM base + window offset>`).
///   2. Even where the copy succeeded — a dumb buffer, a guest-backed blob —
///      it silently disconnected the child from the device, handing it a
///      private snapshot of the pixels/ring instead of the thing itself.
///
/// So the two assertions are "the child survived and can read it" and "it is
/// the same memory on both sides". Note only the *second* would have caught the
/// bug on a machine whose device mappings are all RAM-backed; the first needs a
/// range outside RAM, i.e. a host that offers 3D.
unsafe fn check_fork_with_device_mapping(
    map: *mut u8,
    len: usize,
    n_survive: &[u8],
    n_shared: &[u8],
) -> i32 {
    let mut failures = 0i32;
    // The child's only way to report back. Ordinary MAP_SHARED anonymous
    // memory, deliberately: it is also the fork path COSMIC depends on, so it
    // doubles as a check that this change did not disturb it.
    let sh = mmap(
        core::ptr::null_mut(),
        4096,
        PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_ANONYMOUS,
        -1,
        0,
    );
    if sh as isize == -1 || sh.is_null() {
        if !report(n_survive, false) { failures += 1; }
        if !report(n_shared, false) { failures += 1; }
        return failures;
    }
    let verdict = sh as *mut u8;
    *verdict = 0;

    // Stamp both ends of the range before forking, so the child's read covers
    // the first and last page rather than one lucky page.
    const PARENT_HEAD: u8 = 0xA7;
    const PARENT_TAIL: u8 = 0x5C;
    const CHILD_MARK: u8 = 0x3E;
    let tail = len - 1;
    *map.add(0) = PARENT_HEAD;
    *map.add(tail) = PARENT_TAIL;

    let r = fork();
    if r == 0 {
        // Child. Every load and store below is through a mapping that only
        // exists because fork built it.
        let seen = *map.add(0) == PARENT_HEAD && *map.add(tail) == PARENT_TAIL;
        *verdict = if seen { 1 } else { 2 };
        *map.add(0) = CHILD_MARK;
        _exit(0);
    }
    if r < 0 {
        if !report(n_survive, false) { failures += 1; }
        if !report(n_shared, false) { failures += 1; }
        return failures;
    }

    let mut status: c_int = 0;
    waitpid(r, &mut status, 0);
    // A child that faulted on the mapping does not exit 0 — and a kernel that
    // faulted *building* it never gets here at all.
    let reaped = wifexited(status) && wexitstatus(status) == 0;
    if !reaped || *verdict != 1 {
        out(b"  child exit ok = ");
        out_u64(reaped as u64);
        out(b", child read verdict = ");
        out_u64(*verdict as u64);
        out(b" (1 = saw the parent's stamps)\n");
    }
    if !report(n_survive, reaped && *verdict == 1) { failures += 1; }

    // The child is gone: its address space was torn down, which for a device
    // VMA must drop PTEs and free nothing. The parent's mapping therefore has
    // to be both intact and carrying the child's store.
    let head_now = *map.add(0);
    let tail_now = *map.add(tail);
    let shared = reaped && head_now == CHILD_MARK && tail_now == PARENT_TAIL;
    if !shared {
        out(b"  after child exit head = ");
        out_hex(head_now as u64);
        out(b" (want ");
        out_hex(CHILD_MARK as u64);
        out(b"), tail = ");
        out_hex(tail_now as u64);
        out(b" (want ");
        out_hex(PARENT_TAIL as u64);
        out(b")\n");
    }
    if !report(n_shared, shared) { failures += 1; }
    failures
}

/// A GETPARAM probe: prints the value and returns it (u64::MAX on ioctl error).
unsafe fn getparam(fd: c_int, param: u64, name: &[u8]) -> u64 {
    let mut val: u64 = 0;
    let mut gp = DrmVirtgpuGetparam { param, value: &mut val as *mut u64 as u64 };
    let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &mut gp as *mut _);
    gp.value = val;
    out(b"  param ");
    out(name);
    out(b" = ");
    if rc != 0 {
        out(b"<ioctl failed>\n");
        return u64::MAX;
    }
    out_u64(gp.value);
    out(b" (");
    out_hex(gp.value);
    out(b")\n");
    gp.value
}

/// A GETPARAM probe that prints nothing. Same contract as `getparam`:
/// `u64::MAX` means the ioctl itself failed.
unsafe fn getparam_quiet(fd: c_int, param: u64) -> u64 {
    let mut val: u64 = 0;
    let mut gp = DrmVirtgpuGetparam { param, value: &mut val as *mut u64 as u64 };
    if ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &mut gp as *mut _) != 0 {
        return u64::MAX;
    }
    val
}

/// `struct drm_gem_close { u32 handle; u32 pad; }` — DRM_IOW('d', 0x09, …).
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmGemClose {
    handle: u32,
    pad: u32,
}
const DRM_IOCTL_GEM_CLOSE: c_ulong = 0x40086409;

unsafe fn gem_close(fd: c_int, handle: u32) -> c_int {
    let mut gc = DrmGemClose { handle, pad: 0 };
    ioctl(fd, DRM_IOCTL_GEM_CLOSE, &mut gc as *mut _)
}

/// `struct drm_prime_handle { __u32 handle; __u32 flags; __s32 fd; }`.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmPrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}
const DRM_IOCTL_PRIME_HANDLE_TO_FD: c_ulong = 0xC00C642D;
const DRM_IOCTL_PRIME_FD_TO_HANDLE: c_ulong = 0xC00C642E;
const SEEK_END: c_int = 2;
const SEEK_SET: c_int = 0;

// ── Dumb buffers, for the phase-6 dumb half ──────────────────────────────────
/// `struct drm_mode_create_dumb { u32 height, width, bpp, flags; u32 handle;
///                                u32 pitch; u64 size; }`
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmModeCreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}
/// `struct drm_mode_map_dumb { u32 handle; u32 pad; u64 offset; }`
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmModeMapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}
const DRM_IOCTL_MODE_CREATE_DUMB: c_ulong = 0xC02064B2;
const DRM_IOCTL_MODE_MAP_DUMB: c_ulong = 0xC01064B3;
const DRM_IOCTL_MODE_DESTROY_DUMB: c_ulong = 0xC00464B4;

/// LeandrOS-private GETPARAM: live blob **objects** (not handles, not fds).
/// See the note on `VIRTGPU_PARAM_LEANDROS_BLOB_OBJS` in the driver.
const VIRTGPU_PARAM_LEANDROS_BLOB_OBJS: u64 = 0x1000_0005;

/// The byte a dmabuf export of `size` bytes should hold at offset `i`.
///
/// Position-dependent on purpose. A constant fill would be satisfied by any
/// buffer that happened to hold that constant — including, on a freshly zeroed
/// recycled block, by a fill of zero if the constant were zero. The stride also
/// catches a buffer that is the right memory but at the wrong offset, which is
/// what a partially-recycled or re-split buddy block would look like.
fn pat_byte(i: usize) -> u8 {
    ((i.wrapping_mul(7) ^ 0x5A) & 0xFF) as u8
}

/// Print a GETPARAM readback, distinguishing "the ioctl failed" from a value.
fn out_param(v: u64) {
    if v == u64::MAX {
        out(b"<readback ioctl failed>");
    } else {
        out_u64(v);
    }
}

/// Returned by `ctx_id` when the readback ioctl itself failed. Distinct from a
/// legitimate 0 ("this open has no context"), and never equal to a real id.
const CTX_UNKNOWN: u64 = u64::MAX;

/// The 3D context id the kernel has bound to *this fd's open*. This is the one
/// observation that distinguishes per-open contexts from a global one: on a
/// kernel with a single global slot every fd reports the same (most recently
/// created) id, no matter which fd asks.
unsafe fn ctx_id(fd: c_int) -> u64 {
    let mut val: u64 = 0;
    let mut gp = DrmVirtgpuGetparam {
        param: VIRTGPU_PARAM_LEANDROS_CTX_ID,
        value: &mut val as *mut u64 as u64,
    };
    if ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &mut gp as *mut _) != 0 {
        return CTX_UNKNOWN;
    }
    val
}

fn out_ctx(v: u64) {
    out_param(v);
}

/// Identity assertion: the context id `actual` must be exactly `expected`.
///
/// An `expected` of 0 or CTX_UNKNOWN can never satisfy this — otherwise a
/// kernel that failed every readback would "match" itself and PASS. Both values
/// are printed on failure so a FAIL says which id was seen instead.
fn report_ctx_eq(name: &[u8], expected: u64, actual: u64) -> bool {
    let ok = expected != 0 && expected != CTX_UNKNOWN && actual == expected;
    if !ok {
        out(b"  expected ctx_id = ");
        out_ctx(expected);
        out(b", actual ctx_id = ");
        out_ctx(actual);
        out(b"\n");
    }
    report(name, ok)
}

/// The open must own a real context: a readback of 0 (none) or a failed
/// readback is a failure.
fn report_ctx_real(name: &[u8], actual: u64) -> bool {
    let ok = actual != 0 && actual != CTX_UNKNOWN;
    if !ok {
        out(b"  expected a non-zero ctx_id, got ");
        out_ctx(actual);
        out(b"\n");
    }
    report(name, ok)
}

/// Two opens must not share a context: `actual` must be a real id AND differ
/// from `other`.
fn report_ctx_ne(name: &[u8], other: u64, actual: u64) -> bool {
    let ok = actual != 0 && actual != CTX_UNKNOWN && actual != other;
    if !ok {
        out(b"  ctx_id must differ from ");
        out_ctx(other);
        out(b", got ");
        out_ctx(actual);
        out(b"\n");
    }
    report(name, ok)
}

/// Same CONTEXT_INIT(capset=Venus) call as the single-fd phase, factored out
/// so phase 2 can issue it against several fds.
unsafe fn ctx_init_venus(fd: c_int) -> bool {
    let params = [
        DrmVirtgpuContextSetParam {
            param: VIRTGPU_CONTEXT_PARAM_CAPSET_ID,
            value: VIRTGPU_DRM_CAPSET_VENUS as u64,
        },
        DrmVirtgpuContextSetParam {
            param: VIRTGPU_CONTEXT_PARAM_NUM_RINGS,
            value: 1,
        },
    ];
    let mut init = DrmVirtgpuContextInit {
        num_params: 2,
        pad: 0,
        ctx_set_params: params.as_ptr() as u64,
    };
    ioctl(fd, DRM_IOCTL_VIRTGPU_CONTEXT_INIT, &mut init as *mut _) == 0
}

/// Same not-a-real-Venus-stream EXECBUFFER payload as the single-fd phase.
/// The only assertion is "the guest-side ioctl returned 0" — the host may
/// reject the stream content, that risk belongs to M2, not here.
unsafe fn exec_noop(fd: c_int) -> bool {
    let cmd_stream: [u32; 8] = [0, 0, 0, 0, 0, 0, 0, 0];
    let mut exec = DrmVirtgpuExecbuffer {
        flags: 0,
        size: (cmd_stream.len() * 4) as u32,
        command: cmd_stream.as_ptr() as u64,
        bo_handles: 0,
        num_bo_handles: 0,
        fence_fd: -1,
        ring_idx: 0,
        syncobj_stride: 0,
        num_in_syncobjs: 0,
        num_out_syncobjs: 0,
        in_syncobjs: 0,
        out_syncobjs: 0,
    };
    ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &mut exec as *mut _) == 0
}

/// EXECBUFFER naming `handles` in `bo_handles`. Returns the raw ioctl result so
/// callers can assert on refusal as well as on success.
///
/// Upstream resolves the whole array to GEM objects before submitting and fails
/// the ioctl with -ENOENT if any handle names nothing, so both outcomes are part
/// of the contract and both are tested below.
unsafe fn exec_with_bos(fd: c_int, handles: &[u32]) -> c_int {
    let cmd_stream: [u32; 8] = [0, 0, 0, 0, 0, 0, 0, 0];
    let mut exec = DrmVirtgpuExecbuffer {
        flags: 0,
        size: (cmd_stream.len() * 4) as u32,
        command: cmd_stream.as_ptr() as u64,
        bo_handles: if handles.is_empty() { 0 } else { handles.as_ptr() as u64 },
        num_bo_handles: handles.len() as u32,
        fence_fd: -1,
        ring_idx: 0,
        syncobj_stride: 0,
        num_in_syncobjs: 0,
        num_out_syncobjs: 0,
        in_syncobjs: 0,
        out_syncobjs: 0,
    };
    ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &mut exec as *mut _)
}

// ── Phase 7 helpers: SIMULATE_SYNCOBJ / out-fence fd ─────────────────────────

const EXECBUF_FENCE_FD_OUT: u32 = 0x02;
const EXECBUF_RING_IDX: u32 = 0x04;

/// A sentinel no kernel can legitimately write into `fence_fd`, used to prove
/// "the kernel touched this field" as distinct from "the field happened to hold
/// an acceptable value".
const FENCE_FD_SENTINEL: i32 = -424_242;

/// An execbuffer request with everything at Mesa's designated-initialiser zero
/// except what the caller names. `fence_fd` starts at `fence_fd_seed` — the whole
/// point of several subtests is what that field holds on the way back out.
unsafe fn exec_req(
    flags: u32,
    cmd: &[u32],
    bos: &[u32],
    fence_fd_seed: i32,
) -> DrmVirtgpuExecbuffer {
    DrmVirtgpuExecbuffer {
        flags,
        size: (cmd.len() * 4) as u32,
        command: if cmd.is_empty() { 0 } else { cmd.as_ptr() as u64 },
        bo_handles: if bos.is_empty() { 0 } else { bos.as_ptr() as u64 },
        num_bo_handles: bos.len() as u32,
        fence_fd: fence_fd_seed,
        ring_idx: 0,
        syncobj_stride: 0,
        num_in_syncobjs: 0,
        num_out_syncobjs: 0,
        in_syncobjs: 0,
        out_syncobjs: 0,
    }
}

/// Is `fd` readable right now, without blocking? This is *precisely*
/// `sim_syncobj_poll` (vn_renderer_virtgpu.c:218): `poll(POLLIN)`, nothing else.
/// Mesa never `read`s a fence fd, so "signalled" means exactly this.
unsafe fn poll_in_ready(fd: c_int) -> bool {
    let mut p = PollFd { fd, events: POLLIN, revents: 0 };
    poll(&mut p as *mut PollFd, 1, 0) == 1 && (p.revents & POLLIN) != 0
}

/// SIMULATE_SYNCOBJ: the out-fence fd Mesa's venus backend demands.
///
/// WHY THIS PHASE EXISTS, AND WHAT EACH SUBTEST WOULD CATCH.
///
/// Mesa 25.3.6 compiles SIMULATE_SYNCOBJ/SIMULATE_SUBMIT unconditionally. Two
/// code paths matter and they are coupled:
///
///   * `sim_syncobj_create` (:145) probes once per process with a zero-size,
///     zero-command execbuffer carrying FENCE_FD_OUT, and requires `ret == 0 &&
///     fence_fd >= 0`. A kernel that refuses it disables every
///     `vn_renderer_sync`, which is how `vn_ring_destroy`'s ring-teardown
///     submit gets skipped — a host-side ring leaked per Venus instance.
///   * `sim_submit` (:517) sets FENCE_FD_OUT whenever `batch->sync_count != 0`
///     and then unconditionally `close(args.fence_fd)`. `args` is a designated
///     initialiser, so an unwritten `fence_fd` is **0**: `close(0)` — the
///     process's stdin. That path is reachable only once the probe succeeds, so
///     the two halves must be right together or not at all.
///
/// EVERY SUBTEST BELOW IS ONE OF TWO KINDS, AND THEY ARE LABELLED:
///
///   [GUARD]      fails against a kernel without this fix. The hazard window is
///                open on such a kernel — it refuses the probe outright and
///                never writes `fence_fd` on any path — so these cannot pass
///                vacuously.
///   [NON-REGR]   passes on both. Present to pin behaviour the fix must NOT
///                change (no fd when none was asked for; malformed requests
///                still refused). Not a guard, and not counted as one.
///
/// WHY THE REAL-SUBMIT SUBTESTS BELOW DO NOT SET `VIRTGPU_EXECBUF_RING_IDX`.
///
/// `sim_submit` sets it on every batch, so copying Mesa's flag word verbatim is
/// the obvious thing to do. It hangs, and the reason is about the STREAM, not
/// the flag. This file's command stream is 32 zero bytes — not a dispatchable
/// Venus stream; the host says so out loud (`vkr: submit_cmd:
/// vn_dispatch_command failed` / `failed to dispatch context op 5`). With
/// RING_IDX the guest sets `VIRTIO_GPU_FLAG_INFO_RING_IDX`, which makes the host
/// retire the completion fence through the *renderer context*
/// (`virgl_renderer_context_create_fence`) instead of the global timeline
/// (`virgl_renderer_create_fence`). A context whose dispatch just failed never
/// retires it, the SUBMIT_3D descriptor is never returned, and
/// `VirtioGpu::submit` spins out to `[GPU] control-queue TIMEOUT, cmd=0x207`.
/// Unringed, the fence lands on the global timeline and retires whatever the
/// host made of the bytes — which is why every other synthetic submission in
/// this file (phases 3 and 5) is unringed too.
///
/// Ring 0 is NOT the variable, and this is not a hole in the coverage of Mesa's
/// real flag word: `vn_renderer_submit_simple_sync` (vn_renderer_util.c:24)
/// submits with `ring_idx = 0 /* CPU ring */` AND `sync_count = 1` — i.e.
/// literally `RING_IDX | FENCE_FD_OUT` over a real stream — on every
/// `vkDestroyInstance`, and that submission completes with no timeout. `vktest`,
/// `vkrender` and `vkswap` in the same suite are the coverage for the ringed
/// variant. What is asserted below is the *fd contract*, which `sys_ioctl`
/// decides from FENCE_FD_OUT alone; it never reads `ring_idx`, so dropping the
/// flag removes nothing from the must-fail-unpatched argument (the unpatched
/// kernel writes offset 28 on no path at all, ringed or not).
///
/// Submissions that are REFUSED BEFORE REACHING THE HOST keep the flag, because
/// there it is free and pins that the refusal is flag-independent: the probe
/// (answered by the fence-only early return and never submitted), the
/// bad-BO-handle submissions (refused at `bo_handles` validation) and the
/// half-zero shapes.
///
/// Deliberately NOT tested: the literal `close(args.fence_fd)` consequence, by
/// closing fd 0 and probing it. `sys_fcntl` answers F_GETFD for fd <= 2 from a
/// constant without consulting the fd table (kernel/src/syscall.rs), so that
/// probe would report "stdin fine" either way — a guard that cannot fail. The
/// property is asserted directly instead: `fence_fd >= 3` on success and `== -1`
/// on failure means `close(fence_fd)` can never name a stdio descriptor.
unsafe fn phase7_simulate_syncobj(fd: c_int) -> i32 {
    let mut failures = 0i32;
    out(b"-- phase 7: SIMULATE_SYNCOBJ out-fence fd --\n");

    // Not a valid Venus stream; as everywhere else in this file, the assertion
    // is about the guest-side ioctl contract, not about host execution.
    let stream: [u32; 8] = [0; 8];

    // ── 1. The probe, byte-for-byte as sim_syncobj_create issues it ──────────
    // [GUARD] An unpatched kernel returns EINVAL from
    // `exec.command == 0 || exec.size == 0` and leaves fence_fd at the seed.
    let mut probe = exec_req(EXECBUF_RING_IDX | EXECBUF_FENCE_FD_OUT, &[], &[], 0);
    let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &mut probe as *mut _);
    if !report(b"phase7_syncobj_probe_accepted", rc == 0) { failures += 1; }

    // [GUARD] The seed is 0 — Mesa's own initial value — so this fails on an
    // unpatched kernel *and* states the safety property: never a stdio fd.
    let probe_fd = probe.fence_fd;
    if !report(b"phase7_syncobj_probe_fence_fd_written", probe_fd >= 3) { failures += 1; }

    // [GUARD] Guarded on `probe_fd >= 3` so an unpatched kernel fails here
    // without this test ever polling fd 0.
    let signalled = probe_fd >= 3 && poll_in_ready(probe_fd);
    if !report(b"phase7_syncobj_probe_fd_signalled", signalled) { failures += 1; }

    // [GUARD] `sim_syncobj_submit`/`sim_syncobj_export` reach the fd only
    // through `os_dupfd_cloexec`, and the dup must alias the same signalled
    // object — otherwise every wait on a simulated syncobj would hang.
    let mut dup_ok = false;
    if probe_fd >= 3 {
        let d = fcntl(probe_fd, F_DUPFD_CLOEXEC, 3 as c_int);
        dup_ok = d >= 3 && d != probe_fd && poll_in_ready(d);
        if d >= 3 { close(d); }
    }
    if !report(b"phase7_syncobj_probe_fd_dupable", dup_ok) { failures += 1; }
    if probe_fd >= 3 { close(probe_fd); }

    // ── 2. The sim_submit shape: a REAL stream plus FENCE_FD_OUT ─────────────
    // [GUARD] This is the subtest that denies `close(0)`. An unpatched kernel
    // ACCEPTS this ioctl (the stream is non-empty, so nothing refuses it) and
    // still leaves fence_fd at the 0 it came in with — which is exactly the
    // value Mesa then passes to close(). rc == 0 with fence_fd >= 3 is the only
    // combination under which `sim_submit` is safe.
    //
    // Unringed on purpose: this submission has to reach the host and complete,
    // and our stream is not dispatchable. See the RING_IDX paragraph above.
    let mut sub = exec_req(EXECBUF_FENCE_FD_OUT, &stream, &[], 0);
    let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &mut sub as *mut _);
    let sub_ok = rc == 0 && sub.fence_fd >= 3;
    if !report(b"phase7_submit_fence_fd_out_written", sub_ok) { failures += 1; }
    let first_fd = sub.fence_fd;
    if first_fd >= 3 {
        if !report(b"phase7_submit_fence_fd_signalled", poll_in_ready(first_fd)) {
            failures += 1;
        }
        close(first_fd);
    } else if !report(b"phase7_submit_fence_fd_signalled", false) {
        failures += 1;
    }

    // ── 3. Lifetime: 64 submits must consume no fds on net ───────────────────
    // [GUARD] Fails on an unpatched kernel because `first_fd` is 0, not >= 3.
    // Fails on a LEAKING fix because lowest-free-fd allocation (`alloc_fd`,
    // servers/vfs) hands back the same number every iteration only if each
    // close really released it; a fix that leaked would make the numbers climb
    // and would exhaust the 256-entry eventfd pool within a few frames of real
    // compositing. Nothing else in this loop opens an fd, so `last == first` is
    // the exact expectation, not an approximation.
    let mut loop_ok = first_fd >= 3;
    let mut last_fd = first_fd;
    if loop_ok {
        for _ in 0..64 {
            let mut e = exec_req(EXECBUF_FENCE_FD_OUT, &stream, &[], 0);
            if ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &mut e as *mut _) != 0 || e.fence_fd < 3 {
                loop_ok = false;
                break;
            }
            last_fd = e.fence_fd;
            close(e.fence_fd);
        }
    }
    if !report(b"phase7_fence_fd_recycled_over_64_submits", loop_ok && last_fd == first_fd) {
        failures += 1;
    }

    // ── 4. The failure path must release the reservation and write -1 ────────
    // [GUARD] A submission naming a handle that was never allocated is refused
    // (phase 5 already pins that). An unpatched kernel refuses it too — but
    // leaves fence_fd at 0, so a caller that ignores the return code (or any
    // future one that does not) still closes stdin. -1 is the only value that
    // makes `close(fence_fd)` harmless on a failed submission.
    let mut bad = exec_req(
        EXECBUF_RING_IDX | EXECBUF_FENCE_FD_OUT,
        &stream,
        &[NO_SUCH_HANDLE],
        0,
    );
    let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &mut bad as *mut _);
    if !report(b"phase7_failed_submit_writes_minus_one", rc != 0 && bad.fence_fd == -1) {
        failures += 1;
    }

    // [GUARD] …and the fd reserved for that failed submission must have been
    // released, not leaked: the next successful submit gets the same lowest-free
    // number the first one did. Guarded on `first_fd >= 3`, so an unpatched
    // kernel fails rather than comparing 0 against 0.
    let mut reclaim_ok = false;
    if first_fd >= 3 {
        for _ in 0..8 {
            let mut e = exec_req(
                EXECBUF_RING_IDX | EXECBUF_FENCE_FD_OUT,
                &stream,
                &[NO_SUCH_HANDLE],
                0,
            );
            let _ = ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &mut e as *mut _);
        }
        // The eight above are refused at `bo_handles` validation and never reach
        // the host, so they keep RING_IDX. This one must complete, so it does
        // not — same reason as subtest 2.
        let mut e = exec_req(EXECBUF_FENCE_FD_OUT, &stream, &[], 0);
        if ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &mut e as *mut _) == 0 {
            reclaim_ok = e.fence_fd == first_fd;
            if e.fence_fd >= 3 { close(e.fence_fd); }
        }
    }
    if !report(b"phase7_failed_submit_releases_fence_fd", reclaim_ok) { failures += 1; }

    // ── 5. What must NOT change ──────────────────────────────────────────────
    // [NON-REGR] No FENCE_FD_OUT, no fd. Handing one out unasked would leak an
    // eventfd per submission for every existing caller, none of which close it.
    // This passes on an unpatched kernel by construction — it guards the fix
    // against over-allocating, not the kernel against the old bug.
    let mut plain = exec_req(0, &stream, &[], FENCE_FD_SENTINEL);
    let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &mut plain as *mut _);
    if !report(b"phase7_no_fence_fd_when_not_requested",
               rc == 0 && plain.fence_fd == FENCE_FD_SENTINEL) {
        failures += 1;
    }

    // [NON-REGR] Only BOTH-zero is a fence-only request. A size with no command,
    // or a command with no size, stays malformed and stays refused — the accept
    // must not have widened into "any zero field is fine".
    let mut half_a = exec_req(EXECBUF_RING_IDX | EXECBUF_FENCE_FD_OUT, &[], &[], 0);
    half_a.size = 32; // command still 0
    let rc_a = ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &mut half_a as *mut _);
    let mut half_b = exec_req(EXECBUF_RING_IDX | EXECBUF_FENCE_FD_OUT, &stream, &[], 0);
    half_b.size = 0; // command still non-NULL
    let rc_b = ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &mut half_b as *mut _);
    if !report(b"phase7_half_zero_execbuffer_still_refused", rc_a != 0 && rc_b != 0) {
        failures += 1;
    }
    // Both were refused, so both must carry -1 rather than the incoming 0.
    if half_a.fence_fd >= 3 { close(half_a.fence_fd); }
    if half_b.fence_fd >= 3 { close(half_b.fence_fd); }

    failures
}

/// VIRTGPU_WAIT on one BO handle, raw result.
unsafe fn wait_bo(fd: c_int, handle: u32) -> c_int {
    let mut w = DrmVirtgpu3dWait { handle, flags: 0 };
    ioctl(fd, DRM_IOCTL_VIRTGPU_WAIT, &mut w as *mut _)
}

/// RESOURCE_INFO on one BO handle, raw result.
unsafe fn resource_info_rc(fd: c_int, handle: u32) -> c_int {
    let mut info = DrmVirtgpuResourceInfo {
        bo_handle: handle,
        res_handle: 0,
        size: 0,
        blob_mem: 0,
    };
    ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_INFO, &mut info as *mut _)
}

/// VIRTGPU_MAP on one BO handle, raw result.
unsafe fn map_bo_rc(fd: c_int, handle: u32) -> c_int {
    let mut m = DrmVirtgpuMap { offset: 0, handle, pad: 0 };
    ioctl(fd, DRM_IOCTL_VIRTGPU_MAP, &mut m as *mut _)
}

#[no_mangle]
pub unsafe extern "C" fn venus_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut failures = 0i32;

    out(b"--- venustest: virtio-gpu Venus transport (M1) ---\n");

    let fd = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
    if fd < 0 {
        report(b"open_card0", false);
        return 1;
    }
    report(b"open_card0", true);

    // ── 1. GETPARAM probes ───────────────────────────────────────────────────
    // Mesa's venus backend refuses to proceed unless these read back correctly,
    // so print every one rather than only asserting.
    let p_3d = getparam(fd, VIRTGPU_PARAM_3D_FEATURES, b"3D_FEATURES");
    let p_fix = getparam(fd, VIRTGPU_PARAM_CAPSET_QUERY_FIX, b"CAPSET_QUERY_FIX");
    let p_blob = getparam(fd, VIRTGPU_PARAM_RESOURCE_BLOB, b"RESOURCE_BLOB");
    let p_hv = getparam(fd, VIRTGPU_PARAM_HOST_VISIBLE, b"HOST_VISIBLE");
    let p_ctx = getparam(fd, VIRTGPU_PARAM_CONTEXT_INIT, b"CONTEXT_INIT");
    let p_caps = getparam(fd, VIRTGPU_PARAM_SUPPORTED_CAPSET_IDs, b"SUPPORTED_CAPSET_IDs");
    // p_hv is asserted in section 4c, against the window geometry it claims.

    if !report(b"getparam_3d_features", p_3d == 1) { failures += 1; }
    if !report(b"getparam_capset_query_fix", p_fix == 1) { failures += 1; }
    if !report(b"getparam_resource_blob", p_blob == 1) { failures += 1; }
    if !report(b"getparam_context_init", p_ctx == 1) { failures += 1; }

    // Bit 4 of the mask = capset id 4 = Venus. This bit is set only if the
    // kernel's GET_CAPSET_INFO walk found Venus in the *host's* capset table,
    // so it is already host-derived evidence.
    let venus_advertised = p_caps != u64::MAX && (p_caps & (1u64 << VIRTGPU_DRM_CAPSET_VENUS)) != 0;
    if !report(b"host_advertises_venus_capset", venus_advertised) { failures += 1; }

    // ── 2. CONTEXT_INIT with the Venus capset ────────────────────────────────
    let params = [
        DrmVirtgpuContextSetParam {
            param: VIRTGPU_CONTEXT_PARAM_CAPSET_ID,
            value: VIRTGPU_DRM_CAPSET_VENUS as u64,
        },
        DrmVirtgpuContextSetParam {
            param: VIRTGPU_CONTEXT_PARAM_NUM_RINGS,
            value: 1,
        },
    ];
    let mut init = DrmVirtgpuContextInit {
        num_params: 2,
        pad: 0,
        ctx_set_params: params.as_ptr() as u64,
    };
    let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_CONTEXT_INIT, &mut init as *mut _);
    let ctx_ok = rc == 0;
    if !report(b"context_init_venus", ctx_ok) { failures += 1; }

    // ── 3. GET_CAPS — THE load-bearing check ─────────────────────────────────
    // Ask for the Venus capset and require actual non-zero content. A zeroed
    // buffer with rc == 0 is a FAILURE, not a pass: it is exactly what a
    // silently-wrong wire protocol produces.
    const CAPS_BUF: usize = 4096;
    static mut CAPS: [u8; CAPS_BUF] = [0u8; CAPS_BUF];
    let caps_ptr = &raw mut CAPS as *mut u8;
    // Poison first, so we can also tell how much the kernel wrote.
    for i in 0..CAPS_BUF {
        *caps_ptr.add(i) = 0xA5;
    }
    let mut gc = DrmVirtgpuGetCaps {
        cap_set_id: VIRTGPU_DRM_CAPSET_VENUS,
        cap_set_ver: 0,
        addr: caps_ptr as u64,
        size: CAPS_BUF as u32,
        pad: 0,
    };
    let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_GET_CAPS, &mut gc as *mut _);
    if rc != 0 {
        out(b"  GET_CAPS ioctl failed\n");
        if !report(b"get_caps_venus", false) { failures += 1; }
    } else {
        // Bytes the kernel actually overwrote (poison replaced).
        let mut written = 0usize;
        let mut nonzero = 0usize;
        let mut last_nonzero = 0usize;
        for i in 0..CAPS_BUF {
            let b = *caps_ptr.add(i);
            if b != 0xA5 {
                written = i + 1;
            }
            if b != 0 && b != 0xA5 {
                nonzero += 1;
                last_nonzero = i + 1;
            }
        }
        out(b"  capset bytes written by kernel = ");
        out_u64(written as u64);
        out(b"\n  capset non-zero bytes = ");
        out_u64(nonzero as u64);
        out(b"\n  capset last non-zero offset = ");
        out_u64(last_nonzero as u64);
        out(b"\n  capset first 32 bytes: ");
        for i in 0..32 {
            let b = *caps_ptr.add(i);
            let d = b"0123456789abcdef";
            let pair = [d[(b >> 4) as usize], d[(b & 0xF) as usize], b' '];
            out(&pair);
        }
        out(b"\n");
        // PASS requires host-populated content, not merely a successful ioctl.
        if !report(b"get_caps_venus_nonempty", written > 0 && nonzero > 0) {
            failures += 1;
        }
    }

    // ── 4. RESOURCE_CREATE_BLOB + MAP + mmap write/readback ──────────────────
    const BLOB_SIZE: usize = 64 * 1024;
    let mut blob = DrmVirtgpuResourceCreateBlob {
        blob_mem: VIRTGPU_BLOB_MEM_GUEST,
        blob_flags: VIRTGPU_BLOB_FLAG_USE_MAPPABLE,
        bo_handle: 0,
        res_handle: 0,
        size: BLOB_SIZE as u64,
        pad: 0,
        cmd_size: 0,
        cmd: 0,
        blob_id: 0,
    };
    let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB, &mut blob as *mut _);
    let blob_ok = rc == 0 && blob.bo_handle != 0;
    if blob_ok {
        out(b"  blob bo_handle = ");
        out_u64(blob.bo_handle as u64);
        out(b" res_handle = ");
        out_u64(blob.res_handle as u64);
        out(b"\n");
    }
    if !report(b"resource_create_blob", blob_ok) { failures += 1; }

    let mut mapped_ok = false;
    if blob_ok {
        let mut m = DrmVirtgpuMap {
            offset: 0,
            handle: blob.bo_handle,
            pad: 0,
        };
        let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_MAP, &mut m as *mut _);
        let map_ok = rc == 0 && m.offset != 0;
        if map_ok {
            out(b"  blob mmap offset = ");
            out_hex(m.offset);
            out(b"\n");
        }
        if !report(b"virtgpu_map", map_ok) { failures += 1; }

        if map_ok {
            let p = mmap(
                core::ptr::null_mut(),
                BLOB_SIZE,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                fd,
                m.offset as i64,
            );
            if p as isize <= 0 {
                if !report(b"blob_mmap", false) { failures += 1; }
            } else {
                report(b"blob_mmap", true);
                // Write a pattern and read it back through the same mapping.
                let b = p as *mut u8;
                for i in 0..BLOB_SIZE {
                    *b.add(i) = (i as u8) ^ 0x5A;
                }
                let mut good = true;
                for i in 0..BLOB_SIZE {
                    if *b.add(i) != (i as u8) ^ 0x5A {
                        good = false;
                        break;
                    }
                }
                mapped_ok = good;
                if !report(b"blob_write_readback", good) { failures += 1; }
            }
        }
    }
    let _ = mapped_ok;

    // ── 4b. RESOURCE_INFO ────────────────────────────────────────────────────
    // The last virtgpu ioctl Mesa's Venus ICD needs. Its two callers in
    // vn_renderer_virtgpu.c reject an import whose blob_mem is not the type they
    // allocate with, and take `size` as the mmap size for the blob — so as with
    // get_caps, "the ioctl returned 0" proves nothing and the values are checked
    // against what RESOURCE_CREATE_BLOB was actually asked for.
    if blob_ok {
        let mut info = DrmVirtgpuResourceInfo {
            bo_handle: blob.bo_handle,
            ..Default::default()
        };
        let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_INFO, &mut info as *mut _);
        if !report(b"resource_info", rc == 0) { failures += 1; }
        if rc == 0 {
            out(b"  info res_handle = ");
            out_u64(info.res_handle as u64);
            out(b" size = ");
            out_u64(info.size as u64);
            out(b" blob_mem = ");
            out_u64(info.blob_mem as u64);
            out(b"\n");
            // bo_handle is an input: the kernel must leave it alone.
            // size is the page-aligned allocation, so >= what we asked for.
            let fields_ok = info.bo_handle == blob.bo_handle
                && info.res_handle == blob.res_handle
                && info.size as usize >= BLOB_SIZE
                && info.blob_mem == VIRTGPU_BLOB_MEM_GUEST;
            if !fields_ok {
                out(b"  expected res_handle = ");
                out_u64(blob.res_handle as u64);
                out(b", size >= ");
                out_u64(BLOB_SIZE as u64);
                out(b", blob_mem = ");
                out_u64(VIRTGPU_BLOB_MEM_GUEST as u64);
                out(b", bo_handle = ");
                out_u64(blob.bo_handle as u64);
                out(b"\n");
            }
            if !report(b"resource_info_fields_match", fields_ok) { failures += 1; }
        }
    }

    // A handle that was never allocated must be refused outright. This runs
    // unconditionally — including on a host with no 3D support, where blob
    // creation above was refused and nothing else in this section executes — so
    // that the "resource does not exist" path is always covered and is seen to
    // return an error rather than hang or fault.
    //
    // CAVEAT: this check alone cannot prove the ioctl is IMPLEMENTED. The DRM
    // server collapses every DriverError to a single -1, so "refused: no such
    // handle" and "refused: unknown ioctl" are indistinguishable from userspace;
    // a kernel missing the dispatch arm entirely would also PASS here. What
    // separates them is the kernel's serial line
    // `[DRM] RESOURCE_INFO: unknown bo_handle=…`, and `resource_info_fields_match`
    // above — which only runs where a blob can actually be created.
    {
        let mut info = DrmVirtgpuResourceInfo {
            bo_handle: NO_SUCH_HANDLE,
            ..Default::default()
        };
        let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_INFO, &mut info as *mut _);
        // Refused AND nothing written back: a kernel that "failed" but still
        // filled the struct would feed Mesa a bogus resource id.
        let refused = rc != 0 && info.res_handle == 0 && info.size == 0 && info.blob_mem == 0;
        if !refused {
            out(b"  expected refusal for bo_handle ");
            out_u64(NO_SUCH_HANDLE as u64);
            out(b", got rc = ");
            out_hex(rc as u32 as u64);
            out(b"\n");
        }
        if !report(b"resource_info_bad_handle_refused", refused) { failures += 1; }
    }

    // ── 4c. Host-side (HOST3D) blob: RESOURCE_MAP_BLOB into the shmem window ─
    //
    // WHAT THIS COVERS. A VIRTIO_GPU_BLOB_MEM_HOST3D blob has no guest pages at
    // all; the guest reaches it by asking the host (RESOURCE_MAP_BLOB) to place
    // the resource at a guest-chosen offset inside the virtio-gpu shared-memory
    // BAR window, then mmap()ing the guest-physical range that offset resolves
    // to. Mesa's Venus ICD allocates its command ring exactly this way — 132 KiB,
    // HOST3D, USE_MAPPABLE — and every vkCreateInstance returns
    // VK_ERROR_OUT_OF_HOST_MEMORY if the map is refused. That refusal is what
    // this section exists to keep from coming back.
    //
    // WHAT IT DELIBERATELY DOES NOT CLAIM. venustest cannot manufacture a
    // *valid* HOST3D blob: the `blob_id` naming host storage is minted by the
    // host renderer in response to Venus protocol traffic, which encoding is
    // Mesa's job, not this test's. So on a real 3D host the create below may
    // legitimately be refused for an unknown blob_id, and on a host with no 3D
    // at all it is refused at the feature gate. Neither outcome is a failure.
    // What IS asserted, on every host:
    //   * every outcome is an explicit, self-consistent rc — a refusal never
    //     leaves a handle or an offset written back, and a success never yields
    //     an unusable token (this is what turns a hang or a silent zero into a
    //     FAIL);
    //   * the shared-memory window space is fully returned across a burst of
    //     create/map/close cycles, which is otherwise invisible from userspace
    //     and is the one thing a ~20-blob Vulkan app would expose;
    //   * with no window advertised, nothing is ever reported as mapped.
    // Where a host DOES accept the blob, the mmap + write/readback and the
    // offset-reuse check below become real coverage of the whole path.
    out(b"--- 4c: host-visible (HOST3D) blob map path ---\n");
    {
        /// The size Mesa's Venus ring asks for.
        const RING_SIZE: usize = 132 * 1024;
        /// Enough create/destroy cycles that a per-blob window leak shows up as
        /// a rising span count, and the same order of magnitude a real Vulkan
        /// app does.
        const CYCLES: usize = 20;

        let win_mib_raw = getparam_quiet(fd, VIRTGPU_PARAM_LEANDROS_HOSTVIS_MIB);
        let win_mib = if win_mib_raw == u64::MAX { 0 } else { win_mib_raw };
        let spans_before = getparam_quiet(fd, VIRTGPU_PARAM_LEANDROS_HOSTVIS_SPANS);
        out(b"  host-visible window = ");
        out_u64(win_mib);
        out(b" MiB, live window spans before = ");
        out_param(spans_before);
        out(b"\n");

        // VIRTGPU_PARAM_HOST_VISIBLE is upstream's "host-visible blob memory
        // works here" claim, and Mesa refuses the device outright without it
        // (`one of required kernel params (4 or 9) is missing`). It is only
        // honest if a window really is advertised, so the two must agree.
        let hv_agrees = (p_hv == 1) == (win_mib > 0);
        if !hv_agrees {
            out(b"  HOST_VISIBLE param = ");
            out_param(p_hv);
            out(b" but window = ");
            out_u64(win_mib);
            out(b" MiB\n");
        }
        if !report(b"host_visible_param_matches_window", hv_agrees) { failures += 1; }

        // Every outcome was definite and self-consistent (see the list above).
        let mut explicit = true;
        let mut created = 0usize;
        let mut mapped = 0usize;
        let mut readback_ok = 0usize;
        // First-fit means a released window slot is handed straight back out, so
        // every cycle of an otherwise idle system must land on the same offset.
        let mut first_offset = 0u64;
        let mut offsets_reused = true;

        for _ in 0..CYCLES {
            let mut hb = DrmVirtgpuResourceCreateBlob {
                blob_mem: VIRTGPU_BLOB_MEM_HOST3D,
                blob_flags: VIRTGPU_BLOB_FLAG_USE_MAPPABLE,
                bo_handle: 0,
                res_handle: 0,
                size: RING_SIZE as u64,
                pad: 0,
                cmd_size: 0,
                cmd: 0,
                blob_id: 0,
            };
            let crc = ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB, &mut hb as *mut _);
            if crc != 0 {
                // A refusal must be clean: no handle written back, nothing to
                // close, nothing reserved.
                if hb.bo_handle != 0 { explicit = false; }
                continue;
            }
            created += 1;

            let mut m = DrmVirtgpuMap { offset: 0, handle: hb.bo_handle, pad: 0 };
            let mrc = ioctl(fd, DRM_IOCTL_VIRTGPU_MAP, &mut m as *mut _);
            if mrc != 0 {
                // Refused (e.g. no window): the offset field must be untouched,
                // otherwise a caller would mmap() a token nobody vouches for.
                if m.offset != 0 { explicit = false; }
            } else if m.offset == 0 || (m.offset & 0xFFF) != 0 {
                // "Succeeded" with an unusable token is the failure mode this
                // whole section is watching for.
                explicit = false;
            } else {
                mapped += 1;
                if first_offset == 0 {
                    first_offset = m.offset;
                    out(b"  first host-visible mmap token = ");
                    out_hex(m.offset);
                    out(b"\n");
                } else if m.offset != first_offset {
                    offsets_reused = false;
                }
                let p = mmap(
                    core::ptr::null_mut(),
                    RING_SIZE,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED,
                    fd,
                    m.offset as i64,
                );
                if p as isize <= 0 {
                    explicit = false;
                } else {
                    // Touch the first and last 64 bytes only. This is host memory
                    // reached across a PCI BAR; a full 132 KiB byte loop buys
                    // nothing over covering both ends of the range and is
                    // painfully slow under TCG.
                    let b = p as *mut u8;
                    let mut good = true;
                    for k in 0..2usize {
                        let base = if k == 0 { 0 } else { RING_SIZE - 64 };
                        for j in 0..64usize {
                            *b.add(base + j) = ((j as u8) ^ 0xC3).wrapping_add(k as u8);
                        }
                    }
                    for k in 0..2usize {
                        let base = if k == 0 { 0 } else { RING_SIZE - 64 };
                        for j in 0..64usize {
                            if *b.add(base + j) != ((j as u8) ^ 0xC3).wrapping_add(k as u8) {
                                good = false;
                            }
                        }
                    }
                    if good { readback_ok += 1; } else { explicit = false; }
                }
            }

            // Closing must release the host resource, the window reservation and
            // the handle — that is what `spans_after` below measures.
            if gem_close(fd, hb.bo_handle) != 0 { explicit = false; }
        }

        let spans_after = getparam_quiet(fd, VIRTGPU_PARAM_LEANDROS_HOSTVIS_SPANS);
        out(b"  cycles = ");
        out_u64(CYCLES as u64);
        out(b", created = ");
        out_u64(created as u64);
        out(b", mapped = ");
        out_u64(mapped as u64);
        out(b", readback ok = ");
        out_u64(readback_ok as u64);
        out(b"\n  live window spans after = ");
        out_param(spans_after);
        out(b"\n");

        if !report(b"host3d_blob_outcomes_explicit", explicit) { failures += 1; }

        // THE leak assertion. Also asserts the readback param exists at all: a
        // failed readback is u64::MAX on both sides and must not "match".
        let released = spans_before != u64::MAX && spans_after == spans_before;
        if !released {
            out(b"  window spans must return to ");
            out_param(spans_before);
            out(b", got ");
            out_param(spans_after);
            out(b"\n");
        }
        if !report(b"host3d_window_spans_released", released) { failures += 1; }

        if win_mib == 0 {
            // No window advertised (this Mac's plain virtio-gpu-pci, or any host
            // without blob support): nothing may be reported as mapped, and the
            // refusal must have been explicit — which `explicit` already covers.
            if !report(b"host3d_map_refused_without_window", mapped == 0) { failures += 1; }
        } else {
            out(b"  host-visible window present; map path exercised for real\n");
            if mapped >= 2 {
                // Only meaningful once two cycles have actually mapped: with the
                // window otherwise idle, first-fit must hand the same offset back
                // every time. A bump allocator passes every other check here and
                // fails this one.
                if !report(b"host3d_window_offset_reused", offsets_reused) { failures += 1; }
            }
        }
    }

    // ── 4d. fork() with a device mapping live ────────────────────────────────
    //
    // Two halves, and they run on different machines ON PURPOSE:
    //
    //   * GUEST-BACKED half — runs everywhere, including a plain virtio-gpu-pci
    //     host with no 3D at all. The mapping is a device VMA whose physical
    //     range happens to be guest RAM, so the child inherits it and the
    //     SHARED assertion is what carries the weight: a fork that copies the
    //     VMA passes "child survived" and fails "same memory".
    //   * HOST-VISIBLE half — needs a host that grants RESOURCE_MAP_BLOB, so it
    //     is skipped (loudly) on any host without a shared-memory window. There
    //     the physical range is a PCI BAR, outside RAM entirely, and it is the
    //     SURVIVED assertion that carries the weight: the old copying fork
    //     never returned from this at all, it panicked the kernel in memcpy.
    //
    // A skip prints a line rather than a PASS. Nothing here is asserted to have
    // been exercised that was not.
    out(b"--- 4d: fork() with a device mapping live ---\n");
    {
        // Guest-backed: its own blob, closed at the end, so this section does
        // not depend on what section 4 left behind.
        const FORK_BLOB_SIZE: usize = 64 * 1024;
        let mut fb = DrmVirtgpuResourceCreateBlob {
            blob_mem: VIRTGPU_BLOB_MEM_GUEST,
            blob_flags: VIRTGPU_BLOB_FLAG_USE_MAPPABLE,
            bo_handle: 0,
            res_handle: 0,
            size: FORK_BLOB_SIZE as u64,
            pad: 0,
            cmd_size: 0,
            cmd: 0,
            blob_id: 0,
        };
        let crc = ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB, &mut fb as *mut _);
        let mut mapped: *mut u8 = core::ptr::null_mut();
        if crc == 0 && fb.bo_handle != 0 {
            let mut m = DrmVirtgpuMap { offset: 0, handle: fb.bo_handle, pad: 0 };
            if ioctl(fd, DRM_IOCTL_VIRTGPU_MAP, &mut m as *mut _) == 0 && m.offset != 0 {
                let p = mmap(
                    core::ptr::null_mut(),
                    FORK_BLOB_SIZE,
                    PROT_READ | PROT_WRITE,
                    MAP_SHARED,
                    fd,
                    m.offset as i64,
                );
                if p as isize > 0 { mapped = p as *mut u8; }
            }
        }
        if mapped.is_null() {
            // A guest-BACKED blob still needs the host to offer
            // VIRTIO_GPU_F_RESOURCE_BLOB — the guest supplying the pages does not
            // make the command work on a host that does not implement it. On a
            // plain virtio-gpu-pci host every blob path is refused, so this is a
            // skip, not a failure. The device-VMA fork path is covered there by
            // drmsmoke's FORK_DEVMAP_* version of this same test, which needs no
            // 3D and no blob support at all.
            out(b"  SKIPPED guest half: host refuses RESOURCE_CREATE_BLOB (no blob support)\n");
            out(b"  (drmsmoke's FORK_DEVMAP_* covers the device-VMA fork path here)\n");
        } else {
            failures += check_fork_with_device_mapping(
                mapped,
                FORK_BLOB_SIZE,
                b"fork_with_device_map_guest_survives",
                b"fork_with_device_map_guest_shared",
            );
        }
        if fb.bo_handle != 0 { gem_close(fd, fb.bo_handle); }

        // Host-visible: only where the device advertises a window.
        let win_mib_raw = getparam_quiet(fd, VIRTGPU_PARAM_LEANDROS_HOSTVIS_MIB);
        let win_mib = if win_mib_raw == u64::MAX { 0 } else { win_mib_raw };
        let mut hv_mapped: *mut u8 = core::ptr::null_mut();
        let mut hv_handle: u32 = 0;
        if win_mib > 0 {
            const RING_SIZE: usize = 132 * 1024;
            let mut hb = DrmVirtgpuResourceCreateBlob {
                blob_mem: VIRTGPU_BLOB_MEM_HOST3D,
                blob_flags: VIRTGPU_BLOB_FLAG_USE_MAPPABLE,
                bo_handle: 0,
                res_handle: 0,
                size: RING_SIZE as u64,
                pad: 0,
                cmd_size: 0,
                cmd: 0,
                blob_id: 0,
            };
            if ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB, &mut hb as *mut _) == 0
                && hb.bo_handle != 0
            {
                hv_handle = hb.bo_handle;
                let mut m = DrmVirtgpuMap { offset: 0, handle: hb.bo_handle, pad: 0 };
                if ioctl(fd, DRM_IOCTL_VIRTGPU_MAP, &mut m as *mut _) == 0 && m.offset != 0 {
                    let p = mmap(
                        core::ptr::null_mut(),
                        RING_SIZE,
                        PROT_READ | PROT_WRITE,
                        MAP_SHARED,
                        fd,
                        m.offset as i64,
                    );
                    if p as isize > 0 { hv_mapped = p as *mut u8; }
                }
            }
            if !hv_mapped.is_null() {
                failures += check_fork_with_device_mapping(
                    hv_mapped,
                    RING_SIZE,
                    b"fork_with_device_map_hostvis_survives",
                    b"fork_with_device_map_hostvis_shared",
                );
            } else {
                // A window exists but this host would not give us a mappable
                // HOST3D blob (see 4c: only Mesa can mint a valid blob_id).
                // Still not a failure — but say so, so the skip is never read
                // as coverage.
                out(b"  SKIPPED hostvis half: window present but no mappable HOST3D blob\n");
            }
            if hv_handle != 0 { gem_close(fd, hv_handle); }
        } else {
            out(b"  SKIPPED hostvis half: no host-visible window on this host\n");
            out(b"  (the outside-RAM device VMA that panicked the kernel needs a 3D host)\n");
        }
    }

    // ── 5. EXECBUFFER + WAIT ─────────────────────────────────────────────────
    // The payload is NOT a valid Venus command stream (encoding those is M2's
    // job) — this only proves the bytes reach the host and a fence retires.
    // The host may well reject the stream; that is reported, not hidden.
    if ctx_ok {
        let cmd_stream: [u32; 8] = [0, 0, 0, 0, 0, 0, 0, 0];
        let mut exec = DrmVirtgpuExecbuffer {
            flags: 0,
            size: (cmd_stream.len() * 4) as u32,
            command: cmd_stream.as_ptr() as u64,
            bo_handles: 0,
            num_bo_handles: 0,
            fence_fd: -1,
            ring_idx: 0,
            syncobj_stride: 0,
            num_in_syncobjs: 0,
            num_out_syncobjs: 0,
            in_syncobjs: 0,
            out_syncobjs: 0,
        };
        let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &mut exec as *mut _);
        if !report(b"execbuffer_submit", rc == 0) { failures += 1; }

        let mut w = DrmVirtgpu3dWait {
            handle: if blob.bo_handle != 0 { blob.bo_handle } else { 1 },
            flags: 0,
        };
        let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_WAIT, &mut w as *mut _);
        if !report(b"virtgpu_wait_fence", rc == 0) { failures += 1; }

        // ── bo_handles + per-BO fences ───────────────────────────────────────
        // WAIT takes a BO handle, and until the per-BO fence landed it threw
        // that handle away and answered from one process-global fence. These
        // assert the upstream contract that replaced it. Note what they can and
        // cannot prove: submission is a synchronous busy-spin, so every fence is
        // already retired and every *successful* wait would also have succeeded
        // under the old code. The refusals are the load-bearing half — they are
        // only possible once handles are resolved rather than ignored.
        if blob.bo_handle != 0 {
            // A submission that names a real BO is accepted, and the BO can
            // then be waited on.
            let rc = exec_with_bos(fd, &[blob.bo_handle]);
            if !report(b"execbuffer_with_bo_handles", rc == 0) { failures += 1; }
            if !report(b"virtgpu_wait_after_bo_exec", wait_bo(fd, blob.bo_handle) == 0) {
                failures += 1;
            }
        }

        // A submission naming a handle that was never allocated must be refused
        // outright (upstream -ENOENT from virtio_gpu_array_from_handles), not
        // executed with the bad handle quietly skipped. NO_SUCH_HANDLE is the
        // same never-allocated value the RESOURCE_INFO refusal test uses.
        let rc = exec_with_bos(fd, &[NO_SUCH_HANDLE]);
        if !report(b"execbuffer_bad_bo_handle_refused", rc != 0) { failures += 1; }

        // …and a real handle alongside a bad one fails the whole submission,
        // rather than partially honouring it.
        if blob.bo_handle != 0 {
            let rc = exec_with_bos(fd, &[blob.bo_handle, NO_SUCH_HANDLE]);
            if !report(b"execbuffer_mixed_bo_handles_refused", rc != 0) { failures += 1; }
        }

        // WAIT on a handle that names nothing is likewise refused. This is the
        // single clearest signal that the handle is being looked up at all: the
        // old code returned 0 here, because it never consulted the handle.
        if !report(b"virtgpu_wait_bad_handle_refused", wait_bo(fd, NO_SUCH_HANDLE) != 0) {
            failures += 1;
        }

        // SIMULATE_SYNCOBJ's out-fence fd. Same `ctx_ok` gate as the rest of
        // phase 5: EXECBUFFER is refused before CONTEXT_INIT, so without a
        // context none of this is reachable.
        failures += phase7_simulate_syncobj(fd);
    }

    // ── 6. Release the blob ──────────────────────────────────────────────────
    // Exercised deliberately: repeated runs per boot are the project's way of
    // catching leaks, and a blob BO holds both a buddy allocation and a host
    // resource id until it is closed.
    if blob.bo_handle != 0 {
        let rc = gem_close(fd, blob.bo_handle);
        if !report(b"gem_close_blob", rc == 0) { failures += 1; }
    }

    close(fd);

    // ── Phase 2: per-open-file context isolation ─────────────────────────────
    // Each open file description must own its own 3D context. This regressed
    // when VIRTGPU_CTX_ID was a single process-global slot: a second
    // CONTEXT_INIT (on a second fd) tore down the first client's context and
    // installed its own, so fdA's submissions silently began executing in
    // fdB's context.
    //
    // Note what that means for the assertions below: with the global slot the
    // ioctls all keep returning 0 — fdB's context is live and valid, it just
    // isn't fdA's — so a liveness check proves nothing. Every check that can
    // actually catch the regression compares the *id* the kernel reports for a
    // given open against the id that open was given at CONTEXT_INIT time.
    out(b"--- phase 2: per-open-file context isolation ---\n");

    let fd_a = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
    if fd_a < 0 {
        report(b"phase2_open_fdA", false);
        out(b"--- venustest done, failures = ");
        out_u64((failures + 1) as u64);
        out(b" ---\n");
        return failures + 1;
    }

    // 1. CONTEXT_INIT(capset=Venus) on fdA, and remember the id it was given.
    // Everything after this is measured against `ctx_a`.
    if !report(b"phase2_context_init_fdA", ctx_init_venus(fd_a)) { failures += 1; }
    let ctx_a = ctx_id(fd_a);
    out(b"  fdA ctx_id = ");
    out_ctx(ctx_a);
    out(b"\n");
    if !report_ctx_real(b"phase2_ctxid_fdA_nonzero", ctx_a) { failures += 1; }

    let fd_b = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
    if fd_b < 0 {
        report(b"phase2_open_fdB", false);
        close(fd_a);
        out(b"--- venustest done, failures = ");
        out_u64((failures + 1) as u64);
        out(b" ---\n");
        return failures + 1;
    }

    // 2. CONTEXT_INIT(capset=Venus) on fdB — a second, independent open file.
    if !report(b"phase2_context_init_fdB", ctx_init_venus(fd_b)) { failures += 1; }
    let ctx_b = ctx_id(fd_b);
    out(b"  fdB ctx_id = ");
    out_ctx(ctx_b);
    out(b"\n");
    if !report_ctx_real(b"phase2_ctxid_fdB_nonzero", ctx_b) { failures += 1; }

    // 3. Two independent opens must not share a context. With one global slot
    // there is only one id in existence, so fdB reports what fdA reports.
    if !report_ctx_ne(b"phase2_ctxid_fdB_differs_from_fdA", ctx_a, ctx_b) { failures += 1; }

    // 4. ★ THE regression check. fdA's context id must be untouched by fdB's
    // CONTEXT_INIT. A global slot now holds fdB's id, so asking fdA reports
    // fdB's context — which is exactly the defect, and exactly what an
    // "ioctl returned 0" check cannot see, because fdB's context is live.
    if !report_ctx_eq(b"phase2_ctxid_fdA_survives_fdB_init", ctx_a, ctx_id(fd_a)) {
        failures += 1;
    }

    // 5. Liveness alongside identity: submitting on fdA must still work.
    if !report(b"phase2_execbuffer_fdA_after_fdB_init", exec_noop(fd_a)) { failures += 1; }

    // 6. fdB's own context must independently still work too.
    if !report(b"phase2_execbuffer_fdB", exec_noop(fd_b)) { failures += 1; }

    // 7. Re-running CONTEXT_INIT on fdA with the same capset must be
    // idempotent (re-init of one's own context, not an error).
    if !report(b"phase2_context_init_fdA_idempotent", ctx_init_venus(fd_a)) { failures += 1; }

    // 8. ★ And idempotent means the SAME context, not a fresh one. The global
    // implementation tore down whatever context was current and created a new
    // one on every CONTEXT_INIT, so fdA's id changes here.
    if !report_ctx_eq(b"phase2_ctxid_fdA_unchanged_by_reinit", ctx_a, ctx_id(fd_a)) {
        failures += 1;
    }

    // 9. Closing fdB must destroy only fdB's context — fdA keeps its own id…
    close(fd_b);
    if !report_ctx_eq(b"phase2_ctxid_fdA_after_close_fdB", ctx_a, ctx_id(fd_a)) {
        failures += 1;
    }

    // 10. …and keeps working.
    if !report(b"phase2_execbuffer_fdA_after_close_fdB", exec_noop(fd_a)) { failures += 1; }

    // 11. A dup() shares the open file description, and therefore the context:
    // fdC must report fdA's id, not a new one.
    let fd_c = dup(fd_a);
    let dup_ok = fd_c >= 0;
    let ctx_c = if dup_ok { ctx_id(fd_c) } else { CTX_UNKNOWN };
    if !report_ctx_eq(b"phase2_ctxid_fdC_dup_shares_fdA", ctx_a, ctx_c) { failures += 1; }

    // 12. Closing the original fd number must not tear down the context the
    // dup still refers to — the open outlives the fd.
    close(fd_a);
    let ctx_c_after = if dup_ok { ctx_id(fd_c) } else { CTX_UNKNOWN };
    if !report_ctx_eq(b"phase2_ctxid_fdC_after_close_fdA", ctx_a, ctx_c_after) {
        failures += 1;
    }

    // 13. Liveness on the surviving dup.
    let exec_c_ok = dup_ok && exec_noop(fd_c);
    if !report(b"phase2_execbuffer_fdC_dup_after_close_fdA", exec_c_ok) {
        failures += 1;
    }

    // 14-16. fork() shares the open file description across processes too: the
    // child must see the SAME context on the inherited fd, and the child's exit
    // must not retire the context the parent still uses on that very fd.
    {
        let n_ctx_child = b"phase2_ctxid_fork_child_inherits";
        let n_ctx_after = b"phase2_ctxid_fdC_survives_child_exit";
        let n_exec = b"phase2_execbuffer_fork_child_and_parent";
        // A shared page is the only channel: the child reads the id in its own
        // address space, and the parent must see the value, not a COW copy.
        let sh = if dup_ok {
            mmap(
                core::ptr::null_mut(),
                4096,
                PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_ANONYMOUS,
                -1,
                0,
            )
        } else {
            -1isize as *mut c_void
        };
        if !dup_ok || sh as isize == -1 {
            if !report(n_ctx_child, false) { failures += 1; }
            if !report(n_ctx_after, false) { failures += 1; }
            if !report(n_exec, false) { failures += 1; }
        } else {
            let ctx_slot = sh as *mut u64;
            let flag = (sh as *mut u8).add(8);
            *ctx_slot = 0;
            *flag = 0;

            let r = fork();
            if r == 0 {
                *ctx_slot = ctx_id(fd_c);
                *flag = if exec_noop(fd_c) { 1 } else { 0 };
                _exit(0);
            }
            if r < 0 {
                if !report(n_ctx_child, false) { failures += 1; }
                if !report(n_ctx_after, false) { failures += 1; }
                if !report(n_exec, false) { failures += 1; }
            } else {
                let mut status: c_int = 0;
                waitpid(r, &mut status, 0);
                let reaped = wifexited(status) && wexitstatus(status) == 0;

                // The inherited fd is the same open, so the same context id.
                if !report_ctx_eq(n_ctx_child, ctx_a, if reaped { *ctx_slot } else { CTX_UNKNOWN }) {
                    failures += 1;
                }
                // The child exiting closed its copy of the fd; the parent's
                // open must be untouched by that.
                if !report_ctx_eq(n_ctx_after, ctx_a, ctx_id(fd_c)) { failures += 1; }

                let child_ok = reaped && *flag == 1;
                let parent_exec_ok = exec_noop(fd_c);
                if !report(n_exec, child_ok && parent_exec_ok) { failures += 1; }
            }
        }
    }

    close(fd_c);

    // ── Phase 3: per-open context slot exhaustion ────────────────────────────
    // MAX_GPU_CTXS in drivers/src/drm_device_interface.rs is 16, and running
    // out of slots is new, otherwise-untested code. Requirements: 16 opens get
    // 16 DISTINCT contexts (not one aliased one), the 17th is refused cleanly
    // rather than panicking or silently handing back somebody else's context,
    // and closing them all returns the slots.
    //
    // Assumes venustest is the only client holding 3D contexts, which is true
    // when it is run from the shell; a live compositor would hold one and shift
    // the boundary by one.
    out(b"--- phase 3: per-open context slot exhaustion ---\n");
    {
        const N_CTX: usize = 16; // MAX_GPU_CTXS
        const N_TRY: usize = N_CTX + 1;
        let mut fds = [-1i32; N_TRY];
        let mut ids = [0u64; N_TRY];
        let mut init_ok = [false; N_TRY];

        let mut opened = 0usize;
        for i in 0..N_TRY {
            fds[i] = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
            if fds[i] < 0 { break; }
            opened += 1;
        }
        if !report(b"phase3_open_17_fds", opened == N_TRY) { failures += 1; }

        for i in 0..opened {
            init_ok[i] = ctx_init_venus(fds[i]);
        }
        // Read the ids only once EVERY init has run, never right after each
        // one. Read eagerly, each fd would report the context that had just
        // been created — which a single global slot also satisfies. Read at the
        // end, 16 distinct ids can only come from 16 separate bindings.
        for i in 0..opened {
            ids[i] = ctx_id(fds[i]);
        }

        // The first 16 must each have succeeded with a real, unique id.
        let mut distinct_ok = opened >= N_CTX;
        for i in 0..N_CTX.min(opened) {
            if !init_ok[i] || ids[i] == 0 || ids[i] == CTX_UNKNOWN {
                distinct_ok = false;
            }
            for j in 0..i {
                if ids[j] == ids[i] { distinct_ok = false; }
            }
        }
        out(b"  ctx ids:");
        for i in 0..opened {
            out(b" ");
            out_ctx(ids[i]);
            if !init_ok[i] { out(b"(init-failed)"); }
        }
        out(b"\n");
        if !report(b"phase3_16_contexts_distinct", distinct_ok) { failures += 1; }

        // The 17th must fail the ioctl outright…
        let refused = opened == N_TRY && !init_ok[N_CTX];
        if !report(b"phase3_17th_context_init_refused", refused) { failures += 1; }

        // …and must not have been left holding anyone's context.
        let orphan_ok = opened == N_TRY && ids[N_CTX] == 0;
        if !orphan_ok && opened == N_TRY {
            out(b"  17th open ctx_id = ");
            out_ctx(ids[N_CTX]);
            out(b" (expected 0)\n");
        }
        if !report(b"phase3_17th_has_no_context", orphan_ok) { failures += 1; }

        for i in 0..opened {
            close(fds[i]);
        }

        // Slots must have come back: a fresh open still gets a context. (If the
        // kernel leaked them, this fails while everything above still passes.)
        let fd_fresh = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
        let mut reclaimed = false;
        if fd_fresh >= 0 {
            if ctx_init_venus(fd_fresh) {
                let id = ctx_id(fd_fresh);
                reclaimed = id != 0 && id != CTX_UNKNOWN;
                if !reclaimed {
                    out(b"  fresh open ctx_id = ");
                    out_ctx(id);
                    out(b"\n");
                }
            }
            close(fd_fresh);
        }
        if !report(b"phase3_slots_reclaimed_after_close", reclaimed) { failures += 1; }
    }

    // ── Phase 4: per-open BO ownership, and the per-open fence ───────────────
    // The BO handle space is process-global here (upstream gives each drm_file
    // its own handle table), so isolation is enforced by an owner tag instead:
    // a handle belonging to another open must read as a handle that does not
    // exist. Four ioctls consume a handle and all four must agree — MAP,
    // RESOURCE_INFO, WAIT and GEM_CLOSE — because scoping only some of them
    // would leave a BO that one open can close but not describe.
    //
    // GEM_CLOSE is the one that matters most and is the hardest to observe: it
    // reports success either way (a handle you do not own names nothing, and
    // closing a nonexistent handle has always been success). It is tested
    // indirectly — fdB closes fdA's handle, then fdA must still be able to use
    // it, which is only true if fdB's close was correctly a no-op.
    out(b"--- phase 4: per-open BO ownership + per-open fence ---\n");
    {
        let fd_a = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
        let fd_b = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
        if fd_a < 0 || fd_b < 0 {
            if !report(b"phase4_open_two_fds", false) { failures += 1; }
        } else {
            report(b"phase4_open_two_fds", true);
            let a_ctx = ctx_init_venus(fd_a);
            let b_ctx = ctx_init_venus(fd_b);
            if !report(b"phase4_context_init_both", a_ctx && b_ctx) { failures += 1; }

            // fdA creates a blob. fdB never learns the handle through any legal
            // channel — the test hands it over precisely to prove the kernel
            // refuses it anyway.
            const P4_BLOB_SIZE: u64 = 64 * 1024;
            let mut blob = DrmVirtgpuResourceCreateBlob {
                blob_mem: VIRTGPU_BLOB_MEM_GUEST,
                blob_flags: VIRTGPU_BLOB_FLAG_USE_MAPPABLE,
                size: P4_BLOB_SIZE,
                ..Default::default()
            };
            let rc = ioctl(fd_a, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB, &mut blob as *mut _);
            let have = rc == 0 && blob.bo_handle != 0;
            if !report(b"phase4_blob_created_on_fdA", have) { failures += 1; }

            if have {
                let h = blob.bo_handle;

                // The owner can reach its own BO through every one of them.
                let own_ok = map_bo_rc(fd_a, h) == 0
                    && resource_info_rc(fd_a, h) == 0
                    && wait_bo(fd_a, h) == 0
                    && exec_with_bos(fd_a, &[h]) == 0;
                if !report(b"phase4_owner_can_use_own_bo", own_ok) { failures += 1; }

                // …and the other open cannot reach it through any of them.
                if !report(b"phase4_other_open_map_refused", map_bo_rc(fd_b, h) != 0) {
                    failures += 1;
                }
                if !report(b"phase4_other_open_info_refused", resource_info_rc(fd_b, h) != 0) {
                    failures += 1;
                }
                if !report(b"phase4_other_open_wait_refused", wait_bo(fd_b, h) != 0) {
                    failures += 1;
                }
                if !report(b"phase4_other_open_exec_refused", exec_with_bos(fd_b, &[h]) != 0) {
                    failures += 1;
                }

                // fdB closing fdA's handle must be a no-op, not a teardown of
                // fdA's resource. Reported success either way, so the proof is
                // that fdA still works afterwards.
                let _ = gem_close(fd_b, h);
                let survived = resource_info_rc(fd_a, h) == 0 && wait_bo(fd_a, h) == 0;
                if !report(b"phase4_bo_survives_close_by_other_open", survived) {
                    failures += 1;
                }

                // MODE_DESTROY_DUMB retires handles through the same path as
                // GEM_CLOSE (upstream's drm_gem_handle_delete is literally
                // shared between the two ioctls), so it inherits the per-open
                // ownership test commit 49399f9 landed. fdB destroying fdA's
                // handle must therefore be the same no-op the GEM_CLOSE above
                // is, and the proof is again that fdA still works afterwards.
                // Mesa reaches this: its kms-dri winsys releases every handle
                // it owns with DESTROY_DUMB and never with GEM_CLOSE
                // (kms_dri_sw_winsys.c:295, return value discarded).
                let mut dd_b = h;
                ioctl(fd_b, DRM_IOCTL_MODE_DESTROY_DUMB, &mut dd_b as *mut _);
                let survived_dd = resource_info_rc(fd_a, h) == 0 && wait_bo(fd_a, h) == 0;
                if !report(b"phase4_other_open_destroy_dumb_refused", survived_dd) {
                    failures += 1;
                }

                // ── The per-open fence ───────────────────────────────────────
                // While the submitting fence was one process-global atomic, an
                // open that had never submitted still reported whichever open
                // submitted last. fdB has submitted nothing that succeeded, so
                // its fence must be 0 while fdA's is not.
                let fence_a = getparam_quiet(fd_a, VIRTGPU_PARAM_LEANDROS_LAST_FENCE);
                let fence_b = getparam_quiet(fd_b, VIRTGPU_PARAM_LEANDROS_LAST_FENCE);
                out(b"  fdA last_fence = ");
                out_u64(fence_a);
                out(b", fdB last_fence = ");
                out_u64(fence_b);
                out(b"\n");
                if !report(b"phase4_fence_fdA_nonzero", fence_a != 0 && fence_a != u64::MAX) {
                    failures += 1;
                }
                if !report(b"phase4_fence_fdB_zero_until_it_submits", fence_b == 0) {
                    failures += 1;
                }

                // Once fdB does submit, its fence advances and — the actual
                // regression check — fdA's does NOT move with it.
                let submitted = exec_with_bos(fd_b, &[]) == 0;
                let fence_b2 = getparam_quiet(fd_b, VIRTGPU_PARAM_LEANDROS_LAST_FENCE);
                let fence_a2 = getparam_quiet(fd_a, VIRTGPU_PARAM_LEANDROS_LAST_FENCE);
                if !report(b"phase4_fence_fdB_advances_on_submit", submitted && fence_b2 != 0) {
                    failures += 1;
                }
                if !report(b"phase4_fence_fdA_unmoved_by_fdB_submit", fence_a2 == fence_a) {
                    failures += 1;
                }

                if !report(b"phase4_owner_gem_close", gem_close(fd_a, h) == 0) { failures += 1; }
            }
        }
        if fd_a >= 0 { close(fd_a); }
        if fd_b >= 0 { close(fd_b); }
    }

    // ── phase 5: PRIME/dmabuf export of blob BOs ─────────────────────────────
    //
    // WHY THIS EXISTS. `PRIME_HANDLE_TO_FD` used to resolve handles only through
    // the dumb-buffer registry, so it answered EINVAL for every blob. That is
    // the single gate on `vkGetMemoryFdKHR`, which Mesa's WSI calls for every
    // swapchain image on every DRM-image path — headless, display and Wayland
    // dmabuf alike — which is exactly why offscreen rendering worked while no
    // WSI surface could be created at all.
    //
    // The two shapes are asserted separately because they are backed
    // differently and only one of them is mappable:
    //   * a GUEST blob owns contiguous guest pages, so its exported fd must
    //     alias them, coherently, the way a dumb buffer's does;
    //   * a HOST3D blob owns NO guest pages. Its export must still SUCCEED —
    //     Mesa exports device-local memory during
    //     `wsi_drm_check_dma_buf_sync_file_import_export` — but mmap of that fd
    //     must FAIL. Handing out zeroed anonymous frames instead would be a
    //     silent coherence bug presenting as a Vulkan bug, and that is what a
    //     growable page list would have produced.
    //
    // The size assertion uses a deliberately NON-power-of-two blob: the buddy
    // allocation rounds 0x3000 up to 0x4000, so an fd reporting 0x4000 would
    // prove the export is describing the allocator rather than the resource.
    // Mesa's kms_swrast PRIME importer takes `lseek(fd, 0, SEEK_END)` verbatim
    // as the buffer size.
    out(b"--- phase 5: PRIME export of blob BOs ---\n");
    {
        let fd_a = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
        let fd_b = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
        if fd_a < 0 || fd_b < 0 {
            if !report(b"phase5_open_two_fds", false) { failures += 1; }
        } else {
            let a_ctx = ctx_init_venus(fd_a);
            let b_ctx = ctx_init_venus(fd_b);
            if !report(b"phase5_context_init_both", a_ctx && b_ctx) { failures += 1; }

            const P5_BLOB_SIZE: u64 = 0x3000;
            let mut blob = DrmVirtgpuResourceCreateBlob {
                blob_mem: VIRTGPU_BLOB_MEM_GUEST,
                blob_flags: VIRTGPU_BLOB_FLAG_USE_MAPPABLE,
                size: P5_BLOB_SIZE,
                ..Default::default()
            };
            let rc = ioctl(fd_a, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB, &mut blob as *mut _);
            let have = rc == 0 && blob.bo_handle != 0;
            if !report(b"phase5_guest_blob_created", have) { failures += 1; }

            if have {
                let h = blob.bo_handle;

                let mut ph = DrmPrimeHandle { handle: h, flags: 0, fd: -1 };
                let exported =
                    ioctl(fd_a, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut ph as *mut _) == 0
                        && ph.fd >= 0;
                if !report(b"phase5_prime_export_guest_blob", exported) { failures += 1; }

                if exported {
                    let sz = lseek(ph.fd, 0, SEEK_END);
                    if sz != P5_BLOB_SIZE as i64 {
                        out(b"  expected size ");
                        out_u64(P5_BLOB_SIZE);
                        out(b", got ");
                        out_u64(sz as u64);
                        out(b"\n");
                    }
                    if !report(b"phase5_prime_export_reports_resource_size",
                               sz == P5_BLOB_SIZE as i64) { failures += 1; }

                    let mut ph2 = DrmPrimeHandle { handle: 0, flags: 0, fd: ph.fd };
                    let rt = ioctl(fd_a, DRM_IOCTL_PRIME_FD_TO_HANDLE, &mut ph2 as *mut _) == 0
                        && ph2.handle == h;
                    if !report(b"phase5_prime_roundtrip_guest_blob", rt) { failures += 1; }

                    // The exported fd aliases the very pages VIRTGPU_MAP hands
                    // out — the same coherence drmsmoke asserts for a dumb
                    // buffer, and the only assertion here that can tell a real
                    // export from an fd over unrelated memory.
                    let mut map = DrmVirtgpuMap { offset: 0, handle: h, pad: 0 };
                    let mut alias_ok = false;
                    if ioctl(fd_a, DRM_IOCTL_VIRTGPU_MAP, &mut map as *mut _) == 0 {
                        let dp = mmap(core::ptr::null_mut(), P5_BLOB_SIZE as size_t,
                                      PROT_READ | PROT_WRITE, MAP_SHARED, ph.fd, 0);
                        let cp = mmap(core::ptr::null_mut(), P5_BLOB_SIZE as size_t,
                                      PROT_READ | PROT_WRITE, MAP_SHARED,
                                      fd_a, map.offset as i64);
                        if dp as isize > 0 && cp as isize > 0 {
                            *(dp as *mut u32) = 0x5EED_1234;
                            alias_ok = *(cp as *const u32) == 0x5EED_1234;
                        }
                    }
                    if !report(b"phase5_prime_mmap_alias_guest_blob", alias_ok) { failures += 1; }

                    // A borrowed dmabuf export is not resizable, and SHRINK is
                    // the dangerous direction: the frame list is the DRM
                    // layer's order-N buddy block, so dropping entries off the
                    // end calls unref_or_free(frame, 0) on the tail of it —
                    // order-0 frees out of an order-2 allocation, which is
                    // allocator corruption rather than a leak. This blob is
                    // 0x3000 backed by 4 frames, so truncating to 0x1000 would
                    // free 3 of them individually. Must be refused outright.
                    let shrink_refused = ftruncate(ph.fd, 0x1000) != 0;
                    if !report(b"phase5_dmabuf_export_not_truncatable", shrink_refused) {
                        failures += 1;
                    }

                    close(ph.fd);
                }

                // Scoping survives the new resolver: an open that cannot MAP,
                // describe or wait on a BO must not be able to export it either.
                let mut phb = DrmPrimeHandle { handle: h, flags: 0, fd: -1 };
                let refused =
                    ioctl(fd_b, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut phb as *mut _) != 0;
                if !report(b"phase5_other_open_export_refused", refused) { failures += 1; }

                let _ = gem_close(fd_a, h);
            }

            // Host-side blob: export must succeed, mmap must not.
            let mut hblob = DrmVirtgpuResourceCreateBlob {
                blob_mem: VIRTGPU_BLOB_MEM_HOST3D,
                blob_flags: VIRTGPU_BLOB_FLAG_USE_MAPPABLE,
                size: 0x1000,
                ..Default::default()
            };
            let hrc = ioctl(fd_a, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB,
                            &mut hblob as *mut _);
            if hrc == 0 && hblob.bo_handle != 0 {
                let hh = hblob.bo_handle;
                let mut ph = DrmPrimeHandle { handle: hh, flags: 0, fd: -1 };
                let ok = ioctl(fd_a, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut ph as *mut _) == 0
                    && ph.fd >= 0;
                if !report(b"phase5_prime_export_host3d_blob", ok) { failures += 1; }
                if ok {
                    let p = mmap(core::ptr::null_mut(), 0x1000, PROT_READ | PROT_WRITE,
                                 MAP_SHARED, ph.fd, 0);
                    if !report(b"phase5_host3d_export_is_not_mappable", p as isize <= 0) {
                        failures += 1;
                    }

                    // mmap is not the only way into the frame list. A token fd
                    // reports len = the resource size but owns NO frames, so
                    // read() and write() must both stop at the frames that
                    // actually exist rather than at EOF.
                    //
                    // READ: the clamp this asserts is not politeness. Without
                    // it, `n` is bounded only by `len`, and vmo_copy_out
                    // indexes pages[0] of an EMPTY Vec — an out-of-bounds panic
                    // in kernel context. So the failing form of this assertion
                    // is a KERNEL PANIC, not a wrong return value; see the
                    // report. A short read of 0 is the correct answer: there
                    // are no bytes here, only a handle.
                    let mut rbuf = [0u8; 8];
                    let nread = read(ph.fd, rbuf.as_mut_ptr() as *mut c_void, 8);
                    if !report(b"phase5_host3d_export_reads_short", nread == 0) {
                        failures += 1;
                    }

                    // WRITE: growing here would append a zeroed anonymous frame
                    // that vmo_free_slot never frees (it returns early for a
                    // borrowed VMO), so the write would both leak and pretend
                    // to have stored bytes into a host resource it never
                    // touched. ENOSPC is the honest answer.
                    let wbuf = [0xA5u8; 8];
                    let nwrote = write(ph.fd, wbuf.as_ptr() as *const c_void, 8);
                    if !report(b"phase5_host3d_export_refuses_write", nwrote < 0) {
                        failures += 1;
                    }

                    close(ph.fd);
                }
                let _ = gem_close(fd_a, hh);
            } else {
                out(b"  (no host3d blob on this host - skipping host-side export checks)\n");
            }
        }
        if fd_a >= 0 { close(fd_a); }
        if fd_b >= 0 { close(fd_b); }
    }

    // ── phase 6: an exported dmabuf fd keeps its buffer alive ────────────────
    //
    // WHY THIS EXISTS. `release_blob` / `free_dumb` used to call
    // `mm::buddy::free(phys, order)` the instant the gem handle went away, and
    // `vmo_free_slot` returns early for a borrowed VMO without freeing on the
    // stated grounds that the DRM layer frees the block exactly once. Nothing
    // made the DRM object outlive the exported fd, so this sequence —
    //
    //     h  = RESOURCE_CREATE_BLOB(GUEST, N)
    //     fd = PRIME_HANDLE_TO_FD(h)
    //     GEM_CLOSE(h)
    //     read(fd, buf, N)
    //
    // — read out of frames the buddy allocator had already handed to someone
    // else, and `mmap(fd, MAP_SHARED)` WROTE to them. One unprivileged process,
    // no cross-open work at all. Pre-existing on the dumb path; widened to
    // blobs by the PRIME export.
    //
    // WHY THE CHURN LOOP IS NOT PADDING. `mm::buddy::free` does not scrub, so a
    // `read()` straight after GEM_CLOSE would very often return the pattern
    // anyway and this test would pass against the very bug it exists to catch —
    // exactly the "failed for the wrong reason" trap. The churn allocates
    // same-order blobs, and `virtgpu_handle_resource_create_blob` ZEROES the
    // whole buddy block it is handed, so the moment the freed block comes back
    // round the pattern is destroyed. The allocator's own free-list links are a
    // second, independent destroyer: `push_front` writes next/prev into the
    // first 16 bytes of the block it pushes.
    //
    // WHAT FAILURE LOOKS LIKE, so a red line can be triaged:
    //   * `..._objs_survive_close` FAILs as a WRONG VALUE (0 where 1 was
    //     expected). No error, no crash — the object is simply gone.
    //   * `..._payload_survives_close` FAILs as a WRONG VALUE: `read()` returns
    //     the full count and the bytes are wrong (zeros, or free-list
    //     pointers). It cannot fail as an error code, and it must not panic:
    //     the frames are still mapped in the HHDM, they merely belong to
    //     someone else now.
    //   * `..._objs_zero_after_fd_close` FAILing means the OPPOSITE bug — the
    //     reference is never dropped and every export leaks a buffer.
    //   * `..._alloc_after_release` FAILing means a DOUBLE free reached the
    //     buddy allocator, which is corruption rather than a leak; the next
    //     allocation is the cheapest detector we have. Check the serial log for
    //     `[DRM] bo refcount underflow` at the same time.
    out(b"--- phase 6: exported dmabuf keeps the buffer alive ---\n");
    {
        let fd = open(b"/dev/dri/card0\0".as_ptr(), O_RDWR);
        if fd < 0 {
            if !report(b"phase6_open_card0", false) { failures += 1; }
        } else {
            const P6_SIZE: usize = 0x3000; // deliberately not a power of two
            // The counter is a LeandrOS-private param. A kernel that does not
            // have it is exactly the kernel this test is a regression for, so
            // its absence must NOT skip the payload assertions — those are the
            // ones that read the recycled memory, and they are what makes this
            // fail on an unfixed kernel by construction rather than by
            // agreement. Only the counter assertions are gated.
            let objs0 = getparam_quiet(fd, VIRTGPU_PARAM_LEANDROS_BLOB_OBJS);
            let have_objs = objs0 != u64::MAX;
            if !have_objs {
                out(b"  (no BLOB_OBJS getparam - counter assertions skipped,\n");
                out(b"   payload assertions still run and are the real gate)\n");
            }
            {
                let mut blob = DrmVirtgpuResourceCreateBlob {
                    blob_mem: VIRTGPU_BLOB_MEM_GUEST,
                    blob_flags: VIRTGPU_BLOB_FLAG_USE_MAPPABLE,
                    size: P6_SIZE as u64,
                    ..Default::default()
                };
                let made = ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB,
                                 &mut blob as *mut _) == 0
                    && blob.bo_handle != 0;
                if !report(b"phase6_guest_blob_created", made) { failures += 1; }

                if made {
                    let h = blob.bo_handle;
                    let objs1 = getparam_quiet(fd, VIRTGPU_PARAM_LEANDROS_BLOB_OBJS);
                    if have_objs
                        && !report(b"phase6_create_adds_one_object", objs1 == objs0 + 1)
                    {
                        failures += 1;
                    }

                    // Stamp the pattern through the device mapping — the same
                    // memory the export will alias.
                    let mut m = DrmVirtgpuMap { offset: 0, handle: h, pad: 0 };
                    let mut stamped = false;
                    if ioctl(fd, DRM_IOCTL_VIRTGPU_MAP, &mut m as *mut _) == 0 {
                        let p = mmap(core::ptr::null_mut(), P6_SIZE,
                                     PROT_READ | PROT_WRITE, MAP_SHARED, fd,
                                     m.offset as i64);
                        if p as isize > 0 {
                            let b = p as *mut u8;
                            for i in 0..P6_SIZE { *b.add(i) = pat_byte(i); }
                            stamped = true;
                        }
                    }
                    if !report(b"phase6_pattern_stamped", stamped) { failures += 1; }

                    let mut ph = DrmPrimeHandle { handle: h, flags: 0, fd: -1 };
                    let exported =
                        ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut ph as *mut _) == 0
                            && ph.fd >= 0;
                    if !report(b"phase6_exported", exported) { failures += 1; }

                    // Exporting must not mint a second OBJECT. It takes a
                    // reference on the one that exists; a fresh object here
                    // would mean the fd aliases memory nothing else knows about.
                    let objs2 = getparam_quiet(fd, VIRTGPU_PARAM_LEANDROS_BLOB_OBJS);
                    if have_objs && !report(b"phase6_export_adds_no_object", objs2 == objs1) {
                        failures += 1;
                    }

                    // MODE_DESTROY_DUMB must release a BLOB handle, not only a
                    // dumb one. Mesa's kms-dri winsys frees every handle it
                    // owns — the ones it minted with CREATE_DUMB and the ones
                    // it minted with drmPrimeFDToHandle alike — through
                    // DESTROY_DUMB, and GEM_CLOSE appears nowhere in that file
                    // (kms_dri_sw_winsys.c:295, return value discarded). A
                    // DESTROY_DUMB that consults only the dumb registry
                    // therefore leaks one object per import, once imports mint
                    // handles at all. Uses a blob of its own so the export
                    // sequence under test above is left untouched. Gated on the
                    // object counter like the other counter assertions, and
                    // BLOB_BUFFERS entries only exist on a host with blob
                    // support, so this is unrunnable rather than green on a
                    // host that refuses blob=on.
                    if have_objs {
                        let base = getparam_quiet(fd, VIRTGPU_PARAM_LEANDROS_BLOB_OBJS);
                        let mut g = DrmVirtgpuResourceCreateBlob {
                            blob_mem: VIRTGPU_BLOB_MEM_GUEST,
                            blob_flags: VIRTGPU_BLOB_FLAG_USE_MAPPABLE,
                            size: P6_SIZE as u64,
                            ..Default::default()
                        };
                        let g_made =
                            ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB,
                                  &mut g as *mut _) == 0
                                && g.bo_handle != 0;
                        let after_create =
                            getparam_quiet(fd, VIRTGPU_PARAM_LEANDROS_BLOB_OBJS);
                        if g_made {
                            let mut dd = g.bo_handle;
                            ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut dd as *mut _);
                        }
                        let after_destroy =
                            getparam_quiet(fd, VIRTGPU_PARAM_LEANDROS_BLOB_OBJS);

                        let created_one = g_made && after_create == base + 1;
                        let released = created_one && after_destroy == base;
                        if !released {
                            out(b"  DESTROY_DUMB on a blob handle: live objects ");
                            out_u64(base);
                            out(b" -> ");
                            out_param(after_create);
                            out(b" -> ");
                            out_param(after_destroy);
                            out(b" (want back to ");
                            out_u64(base);
                            out(b")\n");
                        }
                        if !report(b"phase6_destroy_dumb_releases_blob_handle", released) {
                            failures += 1;
                        }
                    }

                    if exported && stamped {
                        // ── the sequence ────────────────────────────────────
                        let _ = gem_close(fd, h);

                        let objs3 = getparam_quiet(fd, VIRTGPU_PARAM_LEANDROS_BLOB_OBJS);
                        if have_objs && objs3 != objs1 {
                            out(b"  after GEM_CLOSE: expected ");
                            out_u64(objs1);
                            out(b" live objects, got ");
                            out_param(objs3);
                            out(b"\n");
                        }
                        if have_objs && !report(b"phase6_objs_survive_close", objs3 == objs1) {
                            failures += 1;
                        }

                        // Force the freed frames back into circulation. Each
                        // create zeroes its whole buddy block, so if GEM_CLOSE
                        // really returned ours, this destroys the pattern.
                        const CHURN: usize = 8;
                        let mut churn = [0u32; CHURN];
                        for slot in churn.iter_mut() {
                            let mut z = DrmVirtgpuResourceCreateBlob {
                                blob_mem: VIRTGPU_BLOB_MEM_GUEST,
                                blob_flags: VIRTGPU_BLOB_FLAG_USE_MAPPABLE,
                                size: P6_SIZE as u64,
                                ..Default::default()
                            };
                            if ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB,
                                     &mut z as *mut _) == 0
                            {
                                *slot = z.bo_handle;
                            }
                        }

                        // THE HAZARD ASSERTION. Read the exported fd and
                        // require every byte of the pattern back.
                        let mut intact = lseek(ph.fd, 0, SEEK_SET) == 0;
                        let mut rbuf = [0u8; 256];
                        let mut off = 0usize;
                        let mut bad_at = usize::MAX;
                        while intact && off < P6_SIZE {
                            let want = rbuf.len().min(P6_SIZE - off);
                            let got = read(ph.fd, rbuf.as_mut_ptr() as *mut c_void, want);
                            if got <= 0 { intact = false; break; }
                            for (i, &b) in rbuf.iter().take(got as usize).enumerate() {
                                if b != pat_byte(off + i) {
                                    bad_at = off + i;
                                    intact = false;
                                    break;
                                }
                            }
                            off += got as usize;
                        }
                        if !intact {
                            out(b"  payload lost at offset ");
                            if bad_at == usize::MAX {
                                out(b"<short read at ");
                                out_u64(off as u64);
                                out(b">");
                            } else {
                                out_u64(bad_at as u64);
                            }
                            out(b" - the fd read RECYCLED memory\n");
                        }
                        if !report(b"phase6_payload_survives_close", intact) {
                            failures += 1;
                        }

                        // The same memory through a MAP_SHARED mapping of the
                        // fd, which is the write-capable half of the hazard.
                        let mp = mmap(core::ptr::null_mut(), P6_SIZE,
                                      PROT_READ | PROT_WRITE, MAP_SHARED, ph.fd, 0);
                        let mut mapped_ok = mp as isize > 0;
                        if mapped_ok {
                            let b = mp as *const u8;
                            for i in 0..P6_SIZE {
                                if *b.add(i) != pat_byte(i) { mapped_ok = false; break; }
                            }
                        }
                        if !report(b"phase6_mmap_of_fd_still_coherent", mapped_ok) {
                            failures += 1;
                        }
                        // Drop the view BEFORE the fd, so nothing in this
                        // process still points at frames the next assertion
                        // requires to be back in the allocator. (`buddy::free`
                        // does not consult `mm::pageref` — a recorded, separate
                        // hazard — so a surviving mapping would be exactly the
                        // dangling one this whole change exists to prevent.)
                        if mp as isize > 0 { munmap(mp, P6_SIZE); }

                        for &z in churn.iter() {
                            if z != 0 { let _ = gem_close(fd, z); }
                        }

                        // Closing the fd is what finally releases it. If this
                        // FAILs the fix leaks instead of corrupting, which is
                        // the opposite error and is just as much a bug.
                        close(ph.fd);
                        let objs4 = getparam_quiet(fd, VIRTGPU_PARAM_LEANDROS_BLOB_OBJS);
                        if have_objs
                            && !report(b"phase6_objs_zero_after_fd_close", objs4 == objs0)
                        {
                            failures += 1;
                        }

                        // A double `mm::buddy::free` of an order-N block is
                        // allocator corruption, not a leak, and the next
                        // allocation is the cheapest detector available.
                        let mut after = DrmVirtgpuResourceCreateBlob {
                            blob_mem: VIRTGPU_BLOB_MEM_GUEST,
                            blob_flags: VIRTGPU_BLOB_FLAG_USE_MAPPABLE,
                            size: P6_SIZE as u64,
                            ..Default::default()
                        };
                        let ok_after = ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB,
                                             &mut after as *mut _) == 0
                            && after.bo_handle != 0;
                        if !report(b"phase6_alloc_after_release", ok_after) { failures += 1; }
                        if ok_after { let _ = gem_close(fd, after.bo_handle); }
                    } else if exported {
                        close(ph.fd);
                        let _ = gem_close(fd, h);
                    } else {
                        let _ = gem_close(fd, h);
                    }
                }
            }

            // ── the dumb half, which is the PRE-EXISTING one ────────────────
            // Same sequence with CREATE_DUMB / DESTROY_DUMB. There is no object
            // counter for dumb buffers, so this asserts on the payload only —
            // which is the assertion that matters anyway, since it is the one
            // that reads the recycled memory.
            {
                let mut cd = DrmModeCreateDumb {
                    width: 64, height: 16, bpp: 32, ..Default::default()
                };
                let made = ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut cd as *mut _) == 0
                    && cd.handle != 0
                    && cd.size >= 4096;
                if !report(b"phase6_dumb_created", made) { failures += 1; }
                if made {
                    let dsize = cd.size as usize;
                    let mut md = DrmModeMapDumb { handle: cd.handle, pad: 0, offset: 0 };
                    let mut stamped = false;
                    if ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mut md as *mut _) == 0 {
                        let p = mmap(core::ptr::null_mut(), dsize,
                                     PROT_READ | PROT_WRITE, MAP_SHARED, fd,
                                     md.offset as i64);
                        if p as isize > 0 {
                            let b = p as *mut u8;
                            for i in 0..dsize { *b.add(i) = pat_byte(i); }
                            stamped = true;
                        }
                    }
                    if !report(b"phase6_dumb_pattern_stamped", stamped) { failures += 1; }

                    let mut ph = DrmPrimeHandle { handle: cd.handle, flags: 0, fd: -1 };
                    let exported =
                        ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut ph as *mut _) == 0
                            && ph.fd >= 0;
                    if !report(b"phase6_dumb_exported", exported) { failures += 1; }

                    if exported && stamped {
                        let mut dd = cd.handle;
                        ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut dd as *mut _);

                        // Churn, same reasoning as the blob half: a dumb create
                        // zeroes its whole allocation (`DrmDumbBuffer::create`).
                        let mut churn = [0u32; 8];
                        for slot in churn.iter_mut() {
                            let mut z = DrmModeCreateDumb {
                                width: 64, height: 16, bpp: 32, ..Default::default()
                            };
                            if ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut z as *mut _) == 0 {
                                *slot = z.handle;
                            }
                        }

                        let mut intact = lseek(ph.fd, 0, SEEK_SET) == 0;
                        let mut rbuf = [0u8; 256];
                        let mut off = 0usize;
                        let mut bad_at = usize::MAX;
                        while intact && off < dsize {
                            let want = rbuf.len().min(dsize - off);
                            let got = read(ph.fd, rbuf.as_mut_ptr() as *mut c_void, want);
                            if got <= 0 { intact = false; break; }
                            for (i, &b) in rbuf.iter().take(got as usize).enumerate() {
                                if b != pat_byte(off + i) {
                                    bad_at = off + i;
                                    intact = false;
                                    break;
                                }
                            }
                            off += got as usize;
                        }
                        if !intact {
                            out(b"  dumb payload lost at offset ");
                            if bad_at == usize::MAX { out(b"<short read>"); }
                            else { out_u64(bad_at as u64); }
                            out(b"\n");
                        }
                        if !report(b"phase6_dumb_payload_survives_destroy", intact) {
                            failures += 1;
                        }

                        for &z in churn.iter() {
                            if z != 0 {
                                let mut zz = z;
                                ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut zz as *mut _);
                            }
                        }
                        close(ph.fd);

                        let mut cd2 = DrmModeCreateDumb {
                            width: 64, height: 16, bpp: 32, ..Default::default()
                        };
                        let ok_after =
                            ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut cd2 as *mut _) == 0
                                && cd2.handle != 0;
                        if !report(b"phase6_dumb_alloc_after_release", ok_after) {
                            failures += 1;
                        }
                        if ok_after {
                            let mut h2 = cd2.handle;
                            ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut h2 as *mut _);
                        }
                    } else {
                        if exported { close(ph.fd); }
                        let mut dd = cd.handle;
                        ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &mut dd as *mut _);
                    }
                }
            }
            close(fd);
        }
    }

    out(b"--- venustest done, failures = ");
    out_u64(failures as u64);
    out(b" ---\n");
    failures
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    unsafe {
        puts(b"venustest: PANIC\n\0".as_ptr());
        loop {}
    }
}
