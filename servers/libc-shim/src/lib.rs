//! Leandros libc shim — user-space runtime helper.
//!
//! This library is linked into every user-space binary (before `libc.a`) to:
//!
//!  1. Parse the kernel's auxv on startup and cache the Leandros-private entries
//!     (e.g. `AT_LEANDROS_VFS_PORT = 256`) in `static` variables accessible from
//!     C and Rust user-space code.
//!
//!  2. Provide `getauxval(type)` — musl has its own internal implementation,
//!     but our version is available to bare user-space code that doesn't link
//!     musl (e.g. in Phase 5 smoke tests).
//!
//!  3. Provide [`ShimMsg`] and [`raw_syscall`] for code that calls the VFS
//!     server directly via IPC (Phase 6+).  Until the VFS server is registered
//!     the port is `u32::MAX` and all calls return `ENOSYS (-38)`.
//!
//! # Initialisation
//!
//! Call `leandros_shim_init_from_sp(sp)` from `_start` with the initial stack
//! pointer (which points at `argc`).  The function walks past argc/argv/envp to
//! find the auxv array and caches all known entries.
//!
//! For bare binaries the entry stub (`start.s`) must call it explicitly.
#![no_std]

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

// ── Panic handler ─────────────────────────────────────────────────────────────

/// Terminate the process via `sys_exit(1)` on panic.
#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!(
            "mov x8, #93",
            "mov x0, #1",
            "svc #0",
            options(noreturn)
        );
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::asm!(
            "mov eax, 60",
            "mov edi, 1",
            "syscall",
            options(noreturn)
        );
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    loop { core::hint::spin_loop(); }
}

// ── Linux auxv type constants ─────────────────────────────────────────────────

const AT_NULL:            usize = 0;
const AT_PAGESZ:          usize = 6;
const AT_RANDOM:          usize = 25;
const AT_LEANDROS_VFS_PORT: usize = 256; // Leandros-private

// ── Cached auxv values ────────────────────────────────────────────────────────

static AUX_PAGESZ:   AtomicUsize = AtomicUsize::new(4096);
static AUX_RANDOM:   AtomicUsize = AtomicUsize::new(0);
/// IPC port of the VFS server, or `u32::MAX` if not yet registered.
static AUX_VFS_PORT: AtomicU32   = AtomicU32::new(u32::MAX);

// ── Public accessors ──────────────────────────────────────────────────────────

/// Return the kernel page size (`AT_PAGESZ`).
#[no_mangle]
pub extern "C" fn leandros_page_size() -> usize {
    AUX_PAGESZ.load(Ordering::Relaxed)
}

/// Return the pointer to 16 bytes of random seed data (`AT_RANDOM`).
#[no_mangle]
pub extern "C" fn leandros_random_ptr() -> usize {
    AUX_RANDOM.load(Ordering::Relaxed)
}

/// Return the Leandros VFS server IPC port, or `u32::MAX` if not registered.
#[no_mangle]
pub extern "C" fn leandros_vfs_port() -> u32 {
    AUX_VFS_PORT.load(Ordering::Relaxed)
}

// ── getauxval ─────────────────────────────────────────────────────────────────

/// Return the auxv value for `type`, or 0 if not present.
///
/// Compatible with the glibc/musl `getauxval(3)` signature.
#[no_mangle]
pub extern "C" fn getauxval(r#type: usize) -> usize {
    match r#type {
        AT_PAGESZ             => AUX_PAGESZ.load(Ordering::Relaxed),
        AT_RANDOM             => AUX_RANDOM.load(Ordering::Relaxed),
        AT_LEANDROS_VFS_PORT    => AUX_VFS_PORT.load(Ordering::Relaxed) as usize,
        _                     => 0,
    }
}

// ── Initialisation ────────────────────────────────────────────────────────────

/// Parse the auxv vector starting at `auxv_ptr` and cache known entries.
///
/// `auxv_ptr` must point at the first `AT_*` key (immediately after the
/// NULL terminator of the `envp` array on the initial stack).
///
/// # Safety
///
/// `auxv_ptr` must be a valid, properly terminated auxv from the kernel.
#[no_mangle]
pub unsafe extern "C" fn leandros_shim_init(auxv_ptr: *const usize) {
    if auxv_ptr.is_null() { return; }
    let mut p = auxv_ptr;
    loop {
        let key = unsafe { p.read() };
        let val = unsafe { p.add(1).read() };
        match key {
            AT_NULL            => break,
            AT_PAGESZ          => { AUX_PAGESZ.store(val, Ordering::Relaxed); }
            AT_RANDOM          => { AUX_RANDOM.store(val, Ordering::Relaxed); }
            AT_LEANDROS_VFS_PORT => { AUX_VFS_PORT.store(val as u32, Ordering::Relaxed); }
            _                  => {}
        }
        p = unsafe { p.add(2) };
    }
}

/// Initialise the shim from an `_start`-style initial stack pointer.
///
/// Walks past argc/argv/envp on the stack to locate the auxv array, then
/// calls [`leandros_shim_init`].
///
/// # Safety
///
/// `sp` must be the initial stack pointer provided by the kernel, pointing
/// at `argc`.
#[no_mangle]
pub unsafe extern "C" fn leandros_shim_init_from_sp(sp: *const usize) {
    if sp.is_null() { return; }
    let argc = unsafe { sp.read() };
    // argv starts at sp+1; the null terminator is at sp+1+argc.
    // envp starts at sp+1+argc+1; scan for its null terminator.
    let mut p = unsafe { sp.add(1 + argc + 1) }; // start of envp
    while unsafe { p.read() } != 0 {
        p = unsafe { p.add(1) };
    }
    // p is now the envp null terminator; auxv immediately follows.
    let auxv = unsafe { p.add(1) };
    leandros_shim_init(auxv);
}

// ── IPC message layout (Phase 6+) ────────────────────────────────────────────

/// Compact IPC message for VFS server calls (mirrors `ipc::Message`).
///
/// In Phase 6 user-space VFS calls will send this structure to the VFS server
/// port via `SYS_IPC_CALL`.  For now it is provided as a type definition only.
#[repr(C)]
pub struct ShimMsg {
    pub tag:        u64,
    pub reply_port: u32,
    pub _pad:       u32,
    pub data:       [u8; 56],
}

impl ShimMsg {
    pub const fn new(tag: u64) -> Self {
        Self { tag, reply_port: 0, _pad: 0, data: [0u8; 56] }
    }
}

// ── Raw syscall helper ────────────────────────────────────────────────────────

/// Issue a raw kernel syscall.  Returns the kernel return value (negative ⇒ errno).
#[cfg(target_arch = "aarch64")]
pub fn raw_syscall(nr: usize,
                   a0: usize, a1: usize, a2: usize,
                   a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    unsafe {
        core::arch::asm!(
            "svc #0",
            inout("x8") nr => _,
            inout("x0") a0 => ret,
            in("x1") a1, in("x2") a2, in("x3") a3,
            in("x4") a4, in("x5") a5,
            options(nostack),
        );
    }
    ret
}

#[cfg(target_arch = "x86_64")]
pub fn raw_syscall(nr: usize,
                   a0: usize, a1: usize, a2: usize,
                   a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    unsafe {
        core::arch::asm!(
            "syscall",
            inout("rax") nr => ret,
            in("rdi") a0, in("rsi") a1, in("rdx") a2,
            in("r10") a3, in("r8")  a4, in("r9")  a5,
            out("rcx") _, out("r11") _,
            options(nostack),
        );
    }
    ret
}
