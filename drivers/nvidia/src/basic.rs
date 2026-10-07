//! Heap, libc-like memory/string helpers and debug hooks.
//!
//! IRQ safety: in hard-IRQ context the kernel's global allocator serves
//! requests up to 4 KiB from `mm::atomic_pool` first (lane kfacil), which is
//! what nvidia-haiku's IntrSafePool does for `os_alloc_mem` from `rm_isr`;
//! frees return pool blocks to the pool from any context.

use core::alloc::Layout;
use core::ffi::{c_char, c_void, CStr};

use crate::NV_OK;

const NV_ERR_NO_MEMORY: u32 = 0x0000_0051;
const NV_ERR_INVALID_ARGUMENT: u32 = 0x0000_001F;

/// Allocation header: the slab allocator needs the size back on free.
const HDR: usize = 16;

// ------------------------------------------------------------------ heap --

/// C: `NV_STATUS os_alloc_mem(void **, NvU64)`. Zeroed, 16-byte aligned
/// (kmalloc on Linux is not zeroed; Haiku uses calloc -- zeroing is safer).
#[no_mangle]
pub unsafe extern "C" fn os_alloc_mem(address: *mut *mut c_void, size: u64) -> u32 {
    cb!("os_alloc_mem");
    if address.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    let total = match (size as usize).checked_add(HDR) {
        Some(t) => t,
        None => return NV_ERR_NO_MEMORY,
    };
    let layout = match Layout::from_size_align(total, HDR) {
        Ok(l) => l,
        Err(_) => return NV_ERR_NO_MEMORY,
    };
    let p = alloc::alloc::alloc_zeroed(layout);
    if p.is_null() {
        *address = core::ptr::null_mut();
        return NV_ERR_NO_MEMORY;
    }
    (p as *mut usize).write(total);
    *address = p.add(HDR) as *mut c_void;
    NV_OK
}

/// C: `void os_free_mem(void *)`.
#[no_mangle]
pub unsafe extern "C" fn os_free_mem(address: *mut c_void) {
    cb!("os_free_mem");
    if address.is_null() {
        return;
    }
    let p = (address as *mut u8).sub(HDR);
    let total = (p as *const usize).read();
    alloc::alloc::dealloc(p, Layout::from_size_align_unchecked(total, HDR));
}

// --------------------------------------------------------- memory/string --

#[no_mangle]
pub unsafe extern "C" fn os_mem_copy(dst: *mut c_void, src: *const c_void, len: u32) -> *mut c_void {
    cb!("os_mem_copy");
    // memmove semantics: Linux implements os_mem_copy with memcpy, but RM
    // callers are not audited for overlap and memmove costs nothing extra.
    core::ptr::copy(src as *const u8, dst as *mut u8, len as usize);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn os_mem_set(dst: *mut c_void, c: u8, len: u32) -> *mut c_void {
    cb!("os_mem_set");
    core::ptr::write_bytes(dst as *mut u8, c, len as usize);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn os_mem_cmp(a: *const u8, b: *const u8, len: u32) -> i32 {
    cb!("os_mem_cmp");
    for i in 0..len as usize {
        let (x, y) = (*a.add(i), *b.add(i));
        if x != y {
            return x as i32 - y as i32;
        }
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn os_string_copy(dst: *mut c_char, src: *const c_char) -> *mut c_char {
    cb!("os_string_copy");
    let n = CStr::from_ptr(src).to_bytes_with_nul().len();
    core::ptr::copy_nonoverlapping(src, dst, n);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn os_string_length(s: *const c_char) -> u32 {
    cb!("os_string_length");
    CStr::from_ptr(s).to_bytes().len() as u32
}

#[no_mangle]
pub unsafe extern "C" fn os_string_compare(a: *const c_char, b: *const c_char) -> i32 {
    cb!("os_string_compare");
    let (a, b) = (CStr::from_ptr(a).to_bytes(), CStr::from_ptr(b).to_bytes());
    match a.cmp(b) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    }
}

/// C: `NvU32 os_strtoul(const char *, char **, NvU32 base)` (base 0 = auto).
#[no_mangle]
pub unsafe extern "C" fn os_strtoul(s: *const c_char, end: *mut *mut c_char, base: u32) -> u32 {
    cb!("os_strtoul");
    let mut p = s as *const u8;
    while matches!(*p, b' ' | b'\t' | b'\n') {
        p = p.add(1);
    }
    let mut base = base;
    if (base == 0 || base == 16) && *p == b'0' && matches!(*p.add(1), b'x' | b'X') {
        p = p.add(2);
        base = 16;
    } else if base == 0 && *p == b'0' {
        base = 8;
    } else if base == 0 {
        base = 10;
    }
    let mut v: u32 = 0;
    loop {
        let d = match *p {
            c @ b'0'..=b'9' => (c - b'0') as u32,
            c @ b'a'..=b'z' => (c - b'a') as u32 + 10,
            c @ b'A'..=b'Z' => (c - b'A') as u32 + 10,
            _ => break,
        };
        if d >= base {
            break;
        }
        v = v.wrapping_mul(base).wrapping_add(d);
        p = p.add(1);
    }
    if !end.is_null() {
        *end = p as *mut c_char;
    }
    v
}

// ----------------------------------------------------------------- debug --

#[no_mangle]
pub unsafe extern "C" fn out_string(s: *const c_char) {
    cb!("out_string");
    if !s.is_null() {
        crate::lnv_log_str(u32::MAX, s);
    }
}

#[no_mangle]
pub unsafe extern "C" fn os_dbg_breakpoint() {
    cb!("os_dbg_breakpoint");
    crate::nvlog!("[nvrm] +{}ms os_dbg_breakpoint (RM assertion; continuing)\n", crate::ms());
    // Linux: a no-op unless NVreg_EnableDbgBreakpoint. RM has already printed
    // the assertion through out_string/nv_printf.
}

#[no_mangle]
pub unsafe extern "C" fn os_dbg_init() {
    cb!("os_dbg_init");
}

#[no_mangle]
pub unsafe extern "C" fn os_dbg_set_level(_level: u32) {
    cb!("os_dbg_set_level");
}

#[no_mangle]
pub unsafe extern "C" fn os_bug_check(code: u32, msg: *const c_char) {
    cb!("os_bug_check");
    let m = if msg.is_null() { "" } else { CStr::from_ptr(msg).to_str().unwrap_or("") };
    crate::nvlog!("[nvrm] +{}ms NVRM BUG CHECK {:#x}: {}\n", crate::ms(), code, m);
    // Linux panics here; on the bring-up box a parked RM thread keeps the
    // machine (and the persisted log) alive instead.
    crate::device::fatal("os_bug_check");
}
