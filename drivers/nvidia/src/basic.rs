//! The first real OS callbacks: heap, libc-like memory/string helpers,
//! debug hooks and IRQ-saving spinlocks. These are the ones RM cannot survive
//! a "fail" stub for -- it ignores a failed os_alloc_spinlock and spins on
//! NULL (seen in the LEANDROS_NVRM_SELFTEST boot) -- and they need nothing the
//! kernel lacks.
//!
//! IRQ safety: the kernel slab allocator takes a blocking spin::Mutex, so
//! os_alloc_mem/os_free_mem must not be reached from rm_isr on the same CPU
//! as a holder. Haiku solves this with an interrupt-safe pool
//! (IntrSafePool.cpp); that is lane G2 work, gated on the ISR design.

use core::alloc::Layout;
use core::ffi::{c_char, c_void, CStr};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::{nvlog, NV_OK};

const NV_ERR_NO_MEMORY: u32 = 0x0000_0051;
const NV_ERR_INVALID_ARGUMENT: u32 = 0x0000_001F;

/// Allocation header: the slab allocator needs the size back on free.
const HDR: usize = 16;

extern "C" {
    fn arch_interrupt_save() -> usize;
    fn arch_interrupt_restore(flags: usize);
}

// ------------------------------------------------------------------ heap --

/// C: `NV_STATUS os_alloc_mem(void **, NvU64)`. Zeroed, 16-byte aligned
/// (kmalloc on Linux is not zeroed; Haiku uses calloc -- zeroing is safer).
#[no_mangle]
pub unsafe extern "C" fn os_alloc_mem(address: *mut *mut c_void, size: u64) -> u32 {
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
    // memmove semantics: Linux implements os_mem_copy with memcpy, but RM
    // callers are not audited for overlap and memmove costs nothing extra.
    core::ptr::copy(src as *const u8, dst as *mut u8, len as usize);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn os_mem_set(dst: *mut c_void, c: u8, len: u32) -> *mut c_void {
    core::ptr::write_bytes(dst as *mut u8, c, len as usize);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn os_mem_cmp(a: *const u8, b: *const u8, len: u32) -> i32 {
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
    let n = CStr::from_ptr(src).to_bytes_with_nul().len();
    core::ptr::copy_nonoverlapping(src, dst, n);
    dst
}

#[no_mangle]
pub unsafe extern "C" fn os_string_length(s: *const c_char) -> u32 {
    CStr::from_ptr(s).to_bytes().len() as u32
}

#[no_mangle]
pub unsafe extern "C" fn os_string_compare(a: *const c_char, b: *const c_char) -> i32 {
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
    if !s.is_null() {
        crate::lnv_log_str(u32::MAX, s);
    }
}

#[no_mangle]
pub unsafe extern "C" fn os_dbg_breakpoint() {
    // Linux: a no-op unless NVreg_EnableDbgBreakpoint. RM has already printed
    // the assertion through out_string/nv_printf.
}

#[no_mangle]
pub unsafe extern "C" fn os_dbg_init() {}

#[no_mangle]
pub unsafe extern "C" fn os_dbg_set_level(_level: u32) {}

#[no_mangle]
pub unsafe extern "C" fn os_bug_check(code: u32, msg: *const c_char) {
    let m = if msg.is_null() { "" } else { CStr::from_ptr(msg).to_str().unwrap_or("") };
    panic!("NVRM bug check {:#x}: {}", code, m);
}

// ------------------------------------------------------------- spinlocks --

/// C: `NV_STATUS os_alloc_spinlock(void **)`.
#[no_mangle]
pub unsafe extern "C" fn os_alloc_spinlock(lock: *mut *mut c_void) -> u32 {
    let st = os_alloc_mem(lock, core::mem::size_of::<AtomicBool>() as u64);
    if st == NV_OK {
        (*lock as *mut AtomicBool).write(AtomicBool::new(false));
    } else {
        nvlog!("[nvrm] os_alloc_spinlock: out of memory\n");
    }
    st
}

#[no_mangle]
pub unsafe extern "C" fn os_free_spinlock(lock: *mut c_void) {
    os_free_mem(lock);
}

/// C: `NvU64 os_acquire_spinlock(void *)` -- returns the saved IRQ state,
/// which RM passes back to os_release_spinlock (spin_lock_irqsave).
#[no_mangle]
pub unsafe extern "C" fn os_acquire_spinlock(lock: *mut c_void) -> u64 {
    let flags = arch_interrupt_save();
    let l = &*(lock as *const AtomicBool);
    while l
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    flags as u64
}

#[no_mangle]
pub unsafe extern "C" fn os_release_spinlock(lock: *mut c_void, old_irql: u64) {
    (*(lock as *const AtomicBool)).store(false, Ordering::Release);
    arch_interrupt_restore(old_irql as usize);
}
