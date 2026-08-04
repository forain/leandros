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

const VIRTGPU_DRM_CAPSET_VENUS: u32 = 4;

const VIRTGPU_CONTEXT_PARAM_CAPSET_ID: u64 = 0x0001;
const VIRTGPU_CONTEXT_PARAM_NUM_RINGS: u64 = 0x0002;

const VIRTGPU_BLOB_MEM_GUEST: u32 = 0x0001;
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

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: c_int, buf: *const c_void, count: size_t) -> isize;
    pub fn open(path: *const u8, oflag: c_int, ...) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    pub fn mmap(addr: *mut c_void, len: size_t, prot: c_int, flags: c_int,
                fd: c_int, offset: i64) -> *mut c_void;

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

/// A GETPARAM probe: prints the value and returns it (u64::MAX on ioctl error).
unsafe fn getparam(fd: c_int, param: u64, name: &[u8]) -> u64 {
    let mut gp = DrmVirtgpuGetparam { param, value: 0 };
    let rc = ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &mut gp as *mut _);
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

/// Returned by `ctx_id` when the readback ioctl itself failed. Distinct from a
/// legitimate 0 ("this open has no context"), and never equal to a real id.
const CTX_UNKNOWN: u64 = u64::MAX;

/// The 3D context id the kernel has bound to *this fd's open*. This is the one
/// observation that distinguishes per-open contexts from a global one: on a
/// kernel with a single global slot every fd reports the same (most recently
/// created) id, no matter which fd asks.
unsafe fn ctx_id(fd: c_int) -> u64 {
    let mut gp = DrmVirtgpuGetparam {
        param: VIRTGPU_PARAM_LEANDROS_CTX_ID,
        value: 0,
    };
    if ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &mut gp as *mut _) != 0 {
        return CTX_UNKNOWN;
    }
    gp.value
}

fn out_ctx(v: u64) {
    if v == CTX_UNKNOWN {
        out(b"<readback ioctl failed>");
    } else {
        out_u64(v);
    }
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
    let _ = p_hv;

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
        const NO_SUCH_HANDLE: u32 = 0x7FFF_FFFF;
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
    }

    // ── 6. Release the blob ──────────────────────────────────────────────────
    // Exercised deliberately: repeated runs per boot are the project's way of
    // catching leaks, and a blob BO holds both a buddy allocation and a host
    // resource id until it is closed.
    if blob.bo_handle != 0 {
        #[repr(C)]
        #[derive(Default, Clone, Copy)]
        struct DrmGemClose {
            handle: u32,
            pad: u32,
        }
        const DRM_IOCTL_GEM_CLOSE: c_ulong = 0x40086409;
        let mut gc = DrmGemClose { handle: blob.bo_handle, pad: 0 };
        let rc = ioctl(fd, DRM_IOCTL_GEM_CLOSE, &mut gc as *mut _);
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
