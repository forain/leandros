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

// x86_64 with SMAP: the kernel runs with RFLAGS.AC clear, so supervisor
// loads and stores to user pages fault. Each helper saves the caller's
// RFLAGS in r8, opens the window with `stac` and, on every exit (including
// the fault landings), closes it with `clac` unless the caller already had
// it open. r8 survives a fault (the fault frame restores every GPR).
// stac/clac are #UD without SMAP, so both are gated on PAN_MODE, which arch
// init forces to 0 on a CPU without the feature.
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
    "    pushfq",
    "    pop r8",
    "    cmp byte ptr [rip + {pan}], 0",
    "    je 2f",
    "    stac",
    "2:",
    "__uaccess_start:",
    "    rep movsb",
    "__uaccess_end:",
    "    mov rax, rcx",
    "    jmp __uaccess_close",
    // Shared epilogue: close the window unless the caller had it open.
    ".global __uaccess_close",
    "__uaccess_close:",
    "    test r8d, 0x40000",
    "    jnz 3f",
    "    cmp byte ptr [rip + {pan}], 0",
    "    je 3f",
    "    clac",
    "3:",
    "    ret",
    pan = sym PAN_MODE,
);

// AArch64 with FEAT_PAN: the kernel runs with PSTATE.PAN set (see
// [`PAN_MODE`]), so EL1 loads and stores to EL0-accessible pages fault. The
// copy clears PAN for its own duration and puts the caller's value back.
// x4 (saved PAN) and w5 (PAN_MODE) are live across the copy loop; a fault
// inside the loop restores every GPR from the exception frame, so the
// landing pad at __uaccess_end still sees them.
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
    "    adrp x5, {pan}",
    "    ldrb w5, [x5, :lo12:{pan}]",
    "    cbz  w5, 0f",
    "    mrs  x4, S3_0_C4_C2_3",   // PAN
    "    .inst 0xd500409f",        // msr pan, #0
    "0:",
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
    "    cbz  w5, 4f",
    "    msr  S3_0_C4_C2_3, x4",   // restore the caller's PAN
    "4:  mov x0, x2",
    "    ret",
    pan = sym PAN_MODE,
);

// Fault-tolerant 32-bit compare-and-swap on a user word (futex PI, robust
// list): `__uaccess_cmpxchg32(addr, old, new, *observed) -> 0 | 1 (fault)`.
// Every user access lies in [__uaccess_cx_start, __uaccess_cx_end); an
// unresolvable fault resumes at __uaccess_cx_fault.
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text.uaccess, \"ax\"",
    ".global __uaccess_cmpxchg32",
    ".global __uaccess_cx_start",
    ".global __uaccess_cx_end",
    ".global __uaccess_cx_fault",
    // rdi = addr, esi = old, edx = new, rcx = observed (kernel pointer).
    "__uaccess_cmpxchg32:",
    "    pushfq",
    "    pop r8",
    "    cmp byte ptr [rip + {pan}], 0",
    "    je 2f",
    "    stac",
    "2:",
    "    mov eax, esi",
    "__uaccess_cx_start:",
    "    lock cmpxchg dword ptr [rdi], edx",
    "__uaccess_cx_end:",
    "    mov dword ptr [rcx], eax",
    "    xor eax, eax",
    "    jmp __uaccess_close",
    "__uaccess_cx_fault:",
    "    mov eax, 1",
    "    jmp __uaccess_close",
    // __uaccess_load32(addr, *out) -> 0 | 1 (fault): one single-copy-atomic
    // aligned 32-bit load.
    ".global __uaccess_load32",
    ".global __uaccess_ld_start",
    ".global __uaccess_ld_end",
    ".global __uaccess_ld_fault",
    "__uaccess_load32:",
    "    pushfq",
    "    pop r8",
    "    cmp byte ptr [rip + {pan}], 0",
    "    je 2f",
    "    stac",
    "2:",
    "__uaccess_ld_start:",
    "    mov eax, dword ptr [rdi]",
    "__uaccess_ld_end:",
    "    mov dword ptr [rsi], eax",
    "    xor eax, eax",
    "    jmp __uaccess_close",
    "__uaccess_ld_fault:",
    "    mov eax, 1",
    "    jmp __uaccess_close",
    pan = sym PAN_MODE,
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text.uaccess, \"ax\"",
    ".global __uaccess_cmpxchg32",
    ".global __uaccess_cx_start",
    ".global __uaccess_cx_end",
    ".global __uaccess_cx_fault",
    // x0 = addr, w1 = old, w2 = new, x3 = observed (kernel pointer).
    // x4 (saved PAN) and w5 (PAN_MODE) survive a fault (GPRs come back
    // from the exception frame).
    "__uaccess_cmpxchg32:",
    "    adrp x5, {pan}",
    "    ldrb w5, [x5, :lo12:{pan}]",
    "    cbz  w5, 0f",
    "    mrs  x4, S3_0_C4_C2_3",   // PAN
    "    .inst 0xd500409f",        // msr pan, #0
    "0:",
    "__uaccess_cx_start:",
    "1:  ldaxr w6, [x0]",
    "    cmp   w6, w1",
    "    b.ne  2f",
    "    stlxr w7, w2, [x0]",
    "    cbnz  w7, 1b",
    "    b     3f",
    "2:  clrex",
    "3:",
    "__uaccess_cx_end:",
    "    str  w6, [x3]",
    "    mov  x8, #0",
    "    b    5f",
    "__uaccess_cx_fault:",
    "    mov  x8, #1",
    "5:  cbz  w5, 6f",
    "    msr  S3_0_C4_C2_3, x4",   // restore the caller's PAN
    "6:  mov  x0, x8",
    "    ret",
    // __uaccess_load32(x0 = addr, x1 = *out) -> 0 | 1 (fault).
    ".global __uaccess_load32",
    ".global __uaccess_ld_start",
    ".global __uaccess_ld_end",
    ".global __uaccess_ld_fault",
    "__uaccess_load32:",
    "    adrp x5, {pan}",
    "    ldrb w5, [x5, :lo12:{pan}]",
    "    cbz  w5, 0f",
    "    mrs  x4, S3_0_C4_C2_3",
    "    .inst 0xd500409f",
    "0:",
    "__uaccess_ld_start:",
    "    ldar w6, [x0]",
    "__uaccess_ld_end:",
    "    str  w6, [x1]",
    "    mov  x8, #0",
    "    b    5f",
    "__uaccess_ld_fault:",
    "    mov  x8, #1",
    "5:  cbz  w5, 6f",
    "    msr  S3_0_C4_C2_3, x4",
    "6:  mov  x0, x8",
    "    ret",
    pan = sym PAN_MODE,
);

/// PAN state, read by the copy above and by `cpu_switch_to`:
/// 0 = PAN not in use (no FEAT_PAN / no SMAP on x86_64, or `pan=off`),
/// [`PAN_AUDIT`] = PAN on, a raw kernel access to user memory is logged once
/// per PC and then allowed, [`PAN_STRICT`] = PAN on, such an access is an
/// unresolvable fault (the task is killed with SIGSEGV).
#[no_mangle]
pub static PAN_MODE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
pub const PAN_AUDIT: u8 = 1;
pub const PAN_STRICT: u8 = 2;

/// Raw kernel accesses to user memory caught by PAN (both modes).
pub static PAN_FAULTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// `secprobe` (kernel/src/syscall.rs) bookkeeping: the user address it is
/// about to touch raw, and whether the PAN fault handler saw that access.
pub static PAN_PROBE_ADDR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(usize::MAX);
pub static PAN_PROBE_HIT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

#[inline]
pub fn pan_mode() -> u8 { PAN_MODE.load(core::sync::atomic::Ordering::Relaxed) }

/// An open window in which the kernel may dereference user pointers
/// directly (AArch64: PSTATE.PAN cleared; x86_64: RFLAGS.AC set, SMAP).
/// Dropping it restores the value it found, so windows nest. A window may
/// block or be preempted: `cpu_switch_to` saves and restores PAN / AC per task.
///
/// Prefer [`copy_from_user`] / [`copy_to_user`] / [`read_user`] /
/// [`write_user`], which also survive a bad pointer; a window is for code
/// that must work on user memory in place (atomics, large legacy paths).
pub struct UserAccess {
    #[cfg(target_arch = "aarch64")]
    saved: u64,
    #[cfg(target_arch = "aarch64")]
    active: bool,
    /// x86_64: the window was closed when opened (close it again on drop).
    #[cfg(target_arch = "x86_64")]
    close: bool,
}

impl UserAccess {
    #[inline(always)]
    pub fn open() -> UserAccess {
        #[cfg(target_arch = "aarch64")]
        {
            if pan_mode() == 0 { return UserAccess { saved: 0, active: false }; }
            let saved: u64;
            unsafe {
                core::arch::asm!("mrs {}, S3_0_C4_C2_3", ".inst 0xd500409f",
                                 out(reg) saved, options(nostack, preserves_flags));
            }
            UserAccess { saved, active: true }
        }
        #[cfg(target_arch = "x86_64")]
        {
            if pan_mode() == 0 { return UserAccess { close: false }; }
            let flags: u64;
            unsafe {
                core::arch::asm!("pushfq", "pop {}", "stac", out(reg) flags, options(nomem));
            }
            UserAccess { close: flags & (1 << 18) == 0 }
        }
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        { UserAccess {} }
    }
}

impl Drop for UserAccess {
    #[inline(always)]
    fn drop(&mut self) {
        #[cfg(target_arch = "aarch64")]
        if self.active {
            unsafe {
                core::arch::asm!("msr S3_0_C4_C2_3, {}", in(reg) self.saved,
                                 options(nostack, preserves_flags));
            }
        }
        #[cfg(target_arch = "x86_64")]
        if self.close {
            unsafe { core::arch::asm!("clac", options(nomem, nostack)); }
        }
    }
}

/// Run `f` inside a [`UserAccess`] window.
#[inline(always)]
pub fn with_user_access<R>(f: impl FnOnce() -> R) -> R {
    let _w = UserAccess::open();
    f()
}

extern "C" {
    fn __uaccess_copy(dst: *mut u8, src: *const u8, len: usize) -> usize;
    fn __uaccess_cmpxchg32(addr: usize, old: u32, new: u32, observed: *mut u32) -> u32;
    static __uaccess_start: u8;
    static __uaccess_end: u8;
    static __uaccess_cx_start: u8;
    static __uaccess_cx_end: u8;
    static __uaccess_cx_fault: u8;
    fn __uaccess_load32(addr: usize, out: *mut u32) -> u32;
    static __uaccess_ld_start: u8;
    static __uaccess_ld_end: u8;
    static __uaccess_ld_fault: u8;
}

/// Where to resume after a fault at `pc` that the page-fault handler could
/// not resolve: `Some(landing)` when `pc` is inside [`copy_raw`] (the landing
/// returns the count still in the copy's length register) or inside
/// [`cmpxchg_user_u32`] (the landing reports the fault), else None.
pub fn fixup(pc: usize) -> Option<usize> {
    let start = core::ptr::addr_of!(__uaccess_start) as usize;
    let end = core::ptr::addr_of!(__uaccess_end) as usize;
    if pc >= start && pc < end { return Some(end); }
    let start = core::ptr::addr_of!(__uaccess_cx_start) as usize;
    let end = core::ptr::addr_of!(__uaccess_cx_end) as usize;
    if pc >= start && pc < end { return Some(core::ptr::addr_of!(__uaccess_cx_fault) as usize); }
    let start = core::ptr::addr_of!(__uaccess_ld_start) as usize;
    let end = core::ptr::addr_of!(__uaccess_ld_end) as usize;
    if pc >= start && pc < end { return Some(core::ptr::addr_of!(__uaccess_ld_fault) as usize); }
    None
}

/// Single-copy-atomic (acquire) load of the aligned user word at `addr`;
/// None for a misaligned, non-user or unmappable address (EFAULT).
pub fn load_user_u32(addr: usize) -> Option<u32> {
    if addr & 3 != 0 || addr == 0 || !user_range_ok(addr, 4) { return None; }
    let mut v = 0u32;
    if unsafe { __uaccess_load32(addr, &mut v) } != 0 { return None; }
    Some(v)
}

/// Atomically replace the user word at `addr` with `new` if it holds `old`
/// (sequentially consistent). Returns the value found (== `old` on success),
/// or None for a misaligned, non-user or unmappable address (EFAULT).
pub fn cmpxchg_user_u32(addr: usize, old: u32, new: u32) -> Option<u32> {
    if addr & 3 != 0 || addr == 0 || !user_range_ok(addr, 4) { return None; }
    let mut seen = 0u32;
    if unsafe { __uaccess_cmpxchg32(addr, old, new, &mut seen) } != 0 { return None; }
    Some(seen)
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
    mm::paging::tlb_service_pending();
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
    mm::paging::tlb_service_pending();
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
///
/// All or nothing, like Linux's `put_user`: a `T` that straddles a page
/// boundary is written tail page first, so when that page is unwritable the
/// head page is left untouched. A byte-wise `rep movsb` (x86_64) otherwise
/// stores the head bytes before it faults — pipe2 on a straddling pointer
/// left the low bytes of its fds in the caller's memory.
pub fn write_user<T: Copy>(dst: usize, v: T) -> bool {
    let n = core::mem::size_of::<T>();
    let src = &v as *const T as *const u8;
    if n == 0 || dst == 0 || !user_range_ok(dst, n) { return n == 0; }
    let head = (4096 - (dst & 4095)).min(n);
    unsafe {
        if head < n && copy_raw((dst + head) as *mut u8, src.add(head), n - head) != 0 {
            return false;
        }
        copy_raw(dst as *mut u8, src, head) == 0
    }
}

/// Fault `[ptr, ptr + len)` of the current task in before a copy that will
/// run with a lock held: absent pages are populated (file pages read with
/// no lock held), and unless `read_only` no page is left shared
/// copy-on-write. Pages that cannot be populated are skipped; the copy
/// itself then reports them.
pub fn prefault(ptr: usize, len: usize, read_only: bool) {
    if len == 0 || !user_range_ok(ptr, len) { return; }
    mm::paging::tlb_service_pending();
    crate::prefault_current_range(ptr, len, read_only);
}
