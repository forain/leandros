//! Process entry point.
//!
//! `_start` is the raw ELF entry point. The kernel places the initial stack
//! frame at SP: [ argc | argv[0]..argv[argc] | NULL | envp.. | NULL | auxv.. ]
//!
//! `_start` passes SP to `__libc_start_main` (this file), which parses the
//! stack frame and calls `main`.

use core::sync::atomic::{AtomicU32, Ordering};

extern "C" {
    fn main(argc: i32, argv: *const *const u8, envp: *const *const u8) -> i32;
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",      // Clear frame pointer
    "   mov rdi, rsp",      // Argument 1: stack pointer
    "   and rsp, -16",      // Align stack
    "   call __libc_start_main",
    "   ud2"                // Should never return
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   mov x29, #0",       // Clear frame pointer
    "   mov x30, #0",       // Clear link register
    "   mov x0, sp",        // Argument 1: stack pointer
    "   and sp, x0, #-16",  // Align stack
    "   bl __libc_start_main",
    "   brk #0"             // Should never return
);

static AUX_VFS_PORT: AtomicU32 = AtomicU32::new(u32::MAX);
static AUX_NET_PORT: AtomicU32 = AtomicU32::new(u32::MAX);
static AUX_AUDIO_PORT: AtomicU32 = AtomicU32::new(u32::MAX);

/// Returns the VFS server port resolved from auxv.
#[no_mangle]
pub extern "C" fn get_vfs_port() -> u32 {
    AUX_VFS_PORT.load(Ordering::Relaxed)
}

/// Returns the net server port resolved from auxv.
#[no_mangle]
pub extern "C" fn get_net_port() -> u32 {
    AUX_NET_PORT.load(Ordering::Relaxed)
}

/// Returns the audio server port resolved from auxv.
#[no_mangle]
pub extern "C" fn get_audio_port() -> u32 {
    AUX_AUDIO_PORT.load(Ordering::Relaxed)
}

/// Entry point from crt0.s
/// 
/// Safety: `sp` must point to a valid stack frame as described above.
#[no_mangle]
pub unsafe extern "C" fn __libc_start_main(sp: *const usize) -> ! {
    let argc = *sp as i32;
    let argv = sp.add(1) as *const *const u8;
    
    // Find envp (after NULL terminator of argv)
    let mut envp_ptr = sp.add(2 + argc as usize);
    let envp = envp_ptr as *const *const u8;
    
    // Find auxv (after NULL terminator of envp)
    while !(*envp_ptr == 0) {
        envp_ptr = envp_ptr.add(1);
    }
    let auxv = envp_ptr.add(1);
    
    parse_auxv(auxv);

    // Initialise memory allocation (libc/mem.rs)
    // No explicit call needed, malloc handles lazy init.

    // Call user main
    let status = main(argc, argv, envp);
    
    crate::process::exit(status);
}

/// Parse auxiliary vector entries.
unsafe fn parse_auxv(mut av: *const usize) {
    const AT_NULL: usize = 0;
    const AT_LEANDROS_VFS_PORT: usize = 256;
    const AT_LEANDROS_NET_PORT: usize = 257;
    const AT_LEANDROS_AUDIO_PORT: usize = 258;

    loop {
        let tag = *av;
        if tag == AT_NULL { break; }
        av = av.add(1);
        let val = *av;
        av = av.add(1);

        match tag {
            AT_LEANDROS_VFS_PORT => { AUX_VFS_PORT.store(val as u32, Ordering::Relaxed); }
            AT_LEANDROS_NET_PORT => { AUX_NET_PORT.store(val as u32, Ordering::Relaxed); }
            AT_LEANDROS_AUDIO_PORT => { AUX_AUDIO_PORT.store(val as u32, Ordering::Relaxed); }
            _ => {}
        }
    }
}
