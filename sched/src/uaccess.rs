//! Fault-tolerant copies between kernel and user memory.
//!
//! Kernel and server code runs in the calling task's context and has always
//! dereferenced user pointers directly. A kernel-mode fault on a user address
//! is serviced like a user fault (`handle_page_fault`: demand-zero, CoW,
//! file-backed). That covers every *valid* pointer. It does not cover a
//! pointer no access could make valid (PROT_NONE, read-only for a store,
//! unmapped, raced by a sibling's munmap): the fault handler then had
//! nothing to return to but a kill (aarch64) or a halted CPU (x86_64) — both
//! with whatever lock the faulting code held still held, which wedged every
//! later user of that lock (the unix-socket table: a `send()` from such a
//! page left `UNIX_CONNS` locked forever).
//!
//! [`copy_raw`] is the one copy loop whose faults are *expected*: when a fault
//! inside it cannot be resolved, the arch fault handler asks [`fixup`] for a
//! landing address and resumes there instead of killing, and the copy returns
//! the number of bytes it did not copy (Linux's `copy_{from,to}_user`
//! contract). Callers turn a non-zero result into EFAULT.
//!
//! Faults that *can* be resolved still are — including file-backed ones,
//! which read the filesystem. Code that holds a filesystem lock must still
//! prefault its user buffers first ([`prefault`]); the copy only removes the
//! kill/halt, not the need to keep file I/O out from under such locks.

/// First non-canonical user address on both architectures (48-bit VA).
pub const USER_END: usize = 0x0000_8000_0000_0000;

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text.uaccess, \"ax\"",
    ".global __uaccess_copy",
    ".global __uaccess_start",
    ".global __uaccess_end",
    // rdi = dst, rsi = src, rdx = len  ->  rax = bytes not copied.
    // `rep movsb` keeps rsi/rdi/rcx exact at a fault: a resolved fault
    // resumes the same instruction where it stopped, and an unresolved one
    // lands on __uaccess_end with rcx = bytes left.
    "__uaccess_copy:",
    "    mov rcx, rdx",
    "__uaccess_start:",
    "    rep movsb",
    "__uaccess_end:",
    "    mov rax, rcx",
    "    ret",
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text.uaccess, \"ax\"",
    ".global __uaccess_copy",
    ".global __uaccess_start",
    ".global __uaccess_end",
    // x0 = dst, x1 = src, x2 = len  ->  x0 = bytes not copied. Every
    // faulting instruction lies in [__uaccess_start, __uaccess_end); x2 is
    // only decremented after a chunk was stored, so at a fault it is the
    // count not yet copied (the 8-byte loop may over-report by up to 7).
    "__uaccess_copy:",
    "__uaccess_start:",
    "1:  cmp x2, #8",
    "    b.lo 2f",
    "    ldr x3, [x1], #8",
    "    str x3, [x0], #8",
    "    sub x2, x2, #8",
    "    b 1b",
    "2:  cbz x2, 3f",
    "    ldrb w3, [x1], #1",
    "    strb w3, [x0], #1",
    "    sub x2, x2, #1",
    "    b 2b",
    "3:",
    "__uaccess_end:",
    "    mov x0, x2",
    "    ret",
);

extern "C" {
    fn __uaccess_copy(dst: *mut u8, src: *const u8, len: usize) -> usize;
    static __uaccess_start: u8;
    static __uaccess_end: u8;
}

/// Where to resume after a fault at `pc` that the page-fault handler could
/// not resolve: `Some(landing)` when `pc` is inside [`copy_raw`], else None.
/// The landing code returns the count still in the copy's length register.
pub fn fixup(pc: usize) -> Option<usize> {
    let start = core::ptr::addr_of!(__uaccess_start) as usize;
    let end = core::ptr::addr_of!(__uaccess_end) as usize;
    if pc >= start && pc < end { Some(end) } else { None }
}

/// True when `[ptr, ptr + len)` lies entirely in user space.
#[inline]
pub fn user_range_ok(ptr: usize, len: usize) -> bool {
    match ptr.checked_add(len) {
        Some(end) => end <= USER_END,
        None => false,
    }
}

/// Copy `len` bytes from `src` to `dst`, either or both of which may be user
/// addresses of the current task. Returns the number of bytes NOT copied
/// (0 = success). A fault on a user page is resolved as usual; one that
/// cannot be ends the copy early instead of killing the task. No range
/// check: callers that got the pointer from userspace check it (or use
/// [`copy_from_user`] / [`copy_to_user`]).
///
/// # Safety
/// The kernel-side pointer must be valid for `len` bytes.
#[inline]
pub unsafe fn copy_raw(dst: *mut u8, src: *const u8, len: usize) -> usize {
    if len == 0 { return 0; }
    __uaccess_copy(dst, src, len)
}

/// Copy `len` bytes from user address `src` into kernel memory at `dst`.
/// Returns the number of bytes not copied (`len` for a bad range).
///
/// # Safety
/// `dst` must be valid for `len` bytes.
pub unsafe fn copy_from_user(dst: *mut u8, src: usize, len: usize) -> usize {
    if len == 0 { return 0; }
    if src == 0 || !user_range_ok(src, len) { return len; }
    copy_raw(dst, src as *const u8, len)
}

/// Copy `len` bytes from kernel memory at `src` to user address `dst`.
/// Returns the number of bytes not copied (`len` for a bad range).
///
/// # Safety
/// `src` must be valid for `len` bytes.
pub unsafe fn copy_to_user(dst: usize, src: *const u8, len: usize) -> usize {
    if len == 0 { return 0; }
    if dst == 0 || !user_range_ok(dst, len) { return len; }
    copy_raw(dst as *mut u8, src, len)
}

/// Read one `T` from user address `src`; None on a bad pointer.
pub fn read_user<T: Copy>(src: usize) -> Option<T> {
    let mut v = core::mem::MaybeUninit::<T>::uninit();
    let n = core::mem::size_of::<T>();
    if unsafe { copy_from_user(v.as_mut_ptr() as *mut u8, src, n) } != 0 { return None; }
    Some(unsafe { v.assume_init() })
}

/// Write one `T` to user address `dst`; false on a bad pointer.
pub fn write_user<T: Copy>(dst: usize, v: T) -> bool {
    let n = core::mem::size_of::<T>();
    unsafe { copy_to_user(dst, &v as *const T as *const u8, n) == 0 }
}

/// Fault `[ptr, ptr + len)` of the current task in before a copy that will
/// run with a lock held: absent pages are populated (file pages read with
/// no lock held), and unless `read_only` no page is left shared
/// copy-on-write. Pages that cannot be populated are skipped; the copy
/// itself then reports them.
pub fn prefault(ptr: usize, len: usize, read_only: bool) {
    if len == 0 || !user_range_ok(ptr, len) { return; }
    crate::prefault_current_range(ptr, len, read_only);
}
