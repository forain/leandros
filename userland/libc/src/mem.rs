//! Memory allocation — bump allocator using sys_brk (Stage 1).
//!
//! Stage 2 will replace this with a proper free-list allocator.
//! `free` and `realloc`-shrink are no-ops; memory is never returned to the OS.

use core::sync::atomic::{AtomicUsize, Ordering};
use crate::syscall::{nr, syscall1};

use crate::io::size_t;

// Current program break. 0 = not yet initialised.
static HEAP_PTR: AtomicUsize = AtomicUsize::new(0);

/// Set (or query) the program break.
unsafe fn brk(addr: usize) -> usize {
    syscall1(nr::BRK, addr) as usize
}

/// Initialise the heap by querying the current break.
unsafe fn heap_init() -> usize {
    let start = brk(0);
    HEAP_PTR.store(start, Ordering::Relaxed);
    start
}

/// Allocate `size` bytes. Returns a 16-byte-aligned pointer, or NULL on OOM.
#[no_mangle]
pub unsafe extern "C" fn malloc(size: size_t) -> *mut u8 {
    if size == 0 { return core::ptr::null_mut(); }
    // Round up to 16-byte alignment; prepend an 8-byte size header.
    let alloc_size = (size + 8 + 15) & !15;
    let mut ptr = HEAP_PTR.load(Ordering::Relaxed);
    if ptr == 0 { ptr = heap_init(); }
    let new_ptr = ptr + alloc_size;
    let actual  = brk(new_ptr);
    if actual < new_ptr {
        crate::errno::set_errno(crate::errno::ENOMEM);
        return core::ptr::null_mut();
    }
    // Store size before the returned pointer for realloc/free.
    *(ptr as *mut usize) = size;
    HEAP_PTR.store(actual, Ordering::Relaxed);
    (ptr + 8) as *mut u8
}

/// Allocate `size` zero-initialised bytes.
#[no_mangle]
pub unsafe extern "C" fn calloc(nmemb: size_t, size: size_t) -> *mut u8 {
    let total = nmemb.checked_mul(size).unwrap_or(0);
    let p = malloc(total);
    if !p.is_null() { memset(p, 0, total); }
    p
}

/// Resize an allocation.  For Stage 1, grow-only: allocates fresh and copies.
#[no_mangle]
pub unsafe extern "C" fn realloc(ptr: *mut u8, new_size: size_t) -> *mut u8 {
    if ptr.is_null() { return malloc(new_size); }
    let old_size = *((ptr as usize - 8) as *const usize);
    if new_size <= old_size { return ptr; } // shrink: no-op
    let new_ptr = malloc(new_size);
    if !new_ptr.is_null() {
        memcpy(new_ptr, ptr, old_size);
        // free(ptr) is a no-op in Stage 1, so we don't bother.
    }
    new_ptr
}

/// No-op for Stage 1 bump allocator.
#[no_mangle]
pub unsafe extern "C" fn free(_ptr: *mut u8) {}

// ── Global Allocator ─────────────────────────────────────────────────────────

struct LibcAllocator;

unsafe impl core::alloc::GlobalAlloc for LibcAllocator {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        malloc(layout.size())
    }
    unsafe fn dealloc(&self, ptr: *mut u8, _layout: core::alloc::Layout) {
        free(ptr)
    }
}

#[global_allocator]
static ALLOCATOR: LibcAllocator = LibcAllocator;

// ── Core memory operations (also used by string.rs) ──────────────────────────

/// Copy `n` bytes from `src` to `dst` (non-overlapping).
#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: size_t) -> *mut u8 {
    for i in 0..n { *dst.add(i) = *src.add(i); }
    dst
}

/// Copy `n` bytes from `src` to `dst` (may overlap).
#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, n: size_t) -> *mut u8 {
    if (dst as usize) <= (src as usize) || (dst as usize) >= (src as usize + n) {
        memcpy(dst, src, n)
    } else {
        for i in (0..n).rev() { *dst.add(i) = *src.add(i); }
        dst
    }
}

/// Fill `n` bytes at `s` with byte value `c`.
#[no_mangle]
pub unsafe extern "C" fn memset(s: *mut u8, c: i32, n: size_t) -> *mut u8 {
    for i in 0..n { *s.add(i) = c as u8; }
    s
}

/// Map memory into the process address space.
#[no_mangle]
pub unsafe extern "C" fn mmap(
    addr: *mut u8, len: size_t, prot: i32, flags: i32, fd: i32, offset: i64,
) -> *mut u8 {
    let r = crate::syscall::syscall6(
        nr::MMAP,
        addr as usize,
        len,
        prot as usize,
        flags as usize,
        fd as usize,
        offset as usize,
    );
    if r < 0 {
        crate::errno::set_errno(-r as i32);
        return (-1isize) as *mut u8;
    }
    r as *mut u8
}

/// Unmap memory from the process address space.
#[no_mangle]
pub unsafe extern "C" fn munmap(addr: *mut u8, len: size_t) -> i32 {
    let r = crate::syscall::syscall2(nr::MUNMAP, addr as usize, len);
    if r < 0 {
        crate::errno::set_errno(-r as i32);
        -1
    } else {
        0
    }
}

/// Compare `n` bytes of `a` and `b`.
#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: size_t) -> i32 {
    for i in 0..n {
        let d = *a.add(i) as i32 - *b.add(i) as i32;
        if d != 0 { return d; }
    }
    0
}

/// Find byte `c` in the first `n` bytes of `s`.
#[no_mangle]
pub unsafe extern "C" fn memchr(s: *const u8, c: i32, n: size_t) -> *mut u8 {
    for i in 0..n {
        if *s.add(i) == c as u8 { return s.add(i) as *mut u8; }
    }
    core::ptr::null_mut()
}
