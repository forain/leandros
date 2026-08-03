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
//! write/readback; EXECBUFFER + WAIT.
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

// ── virtgpu_drm.h ioctl codes ────────────────────────────────────────────────
// DRM_IOWR(DRM_COMMAND_BASE + nr, struct):
//   (3 << 30) | (size << 16) | ('d' << 8) | (0x40 + nr)
const DRM_IOCTL_VIRTGPU_MAP: c_ulong = 0xC0106441;
const DRM_IOCTL_VIRTGPU_EXECBUFFER: c_ulong = 0xC0406442;
const DRM_IOCTL_VIRTGPU_GETPARAM: c_ulong = 0xC0106443;
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
}

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
