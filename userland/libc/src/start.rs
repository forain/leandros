//! Process entry point.
//!
//! `_start` is the raw ELF entry point. The kernel places the initial stack
//! frame at SP: [ argc | argv[0]..argv[argc] | NULL | envp.. | NULL | auxv.. ]
//!
//! `_start` passes SP to `__libc_start_main`, which parses the frame and calls
//! the user's `main(argc, argv, envp)`.

use core::arch::global_asm;

// AArch64 entry stub — runs before any C code.
#[cfg(target_arch = "aarch64")]
global_asm!(
    ".section .text._start, \"ax\", %progbits",
    ".global _start",
    ".type   _start, %function",
    "_start:",
    // Call libc_start_main properly to enable normal init process
    "   mov  x29, #0",           // clear frame pointer (no parent frame)
    "   mov  x30, #0",           // clear link register  (no return address)
    "   mov  x0,  sp",           // argument: initial stack pointer → argc
    "   bl   __libc_start_main", // tail-call to libc (never returns)
    "1: wfe",
    "   b    1b",

    "debug_start_msg:",
    ".ascii \"[USERSPACE] _start reached! Assembly entry point working correctly!\\n\""
);

// x86_64 entry stub — runs before any C code.
#[cfg(target_arch = "x86_64")]
global_asm!(
    ".section .text._start, \"ax\", @progbits",
    ".global _start",
    ".type   _start, @function",
    "_start:",
    // Call libc_start_main properly to enable normal init process
    "   xor  rbp, rbp",          // clear frame pointer (no parent frame)
    "   mov  rdi, rsp",          // argument: initial stack pointer → argc
    "   call __libc_start_main", // tail-call to libc (never returns)
    "1: hlt",
    "   jmp 1b",

    "debug_start_msg:",
    ".ascii \"[USERSPACE] _start reached! Assembly entry point working correctly!\\n\""
);

/// Called by `_start` with the raw initial stack pointer.
///
/// Layout at `sp`:
/// ```text
/// sp+0             argc (usize)
/// sp+8             argv[0]  (pointer to NUL-terminated string)
/// …
/// sp+8*(argc+1)    NULL
/// sp+8*(argc+2)    envp[0]
/// …                NULL
///                  auxv pairs  (type: usize, value: usize), terminated by AT_NULL
/// ```
#[no_mangle]
pub unsafe extern "C" fn __libc_start_main(sp: *const usize) -> ! {
    // IMMEDIATE DEBUG: If we reach here, userspace execution is working!
    debug_print_userspace_entry();

    extern "C" {
        fn main(argc: i32, argv: *const *const u8, envp: *const *const u8) -> i32;
    }

    let argc = *sp as i32;
    let argv = sp.add(1) as *const *const u8;
    let envp = argv.add(argc as usize + 1);

    // Walk past envp to find auxv start — cache server ports (Stage 2 hook).
    let mut ep = envp;
    while !(*ep).is_null() {
        ep = ep.add(1);
    }
    let auxv = ep.add(1) as *const usize;
    parse_auxv(auxv);

    let code = main(argc, argv, envp);
    crate::process::exit(code);
}

/// Emergency debug function to confirm userspace execution
unsafe fn debug_print_userspace_entry() {
    // Use direct write syscall to print immediately
    let msg = b"[USERSPACE] SUCCESS! Userspace execution confirmed - libc_start_main reached!\n";
    crate::syscall::syscall3(crate::syscall::nr::WRITE, 1, msg.as_ptr() as usize, msg.len());
}

/// Parse auxv and cache Cyanos-private entries for Stage 2 IPC routing.
/// Currently a no-op placeholder; Stage 2 will read AT_CYANOS_VFS_PORT etc.
unsafe fn parse_auxv(mut av: *const usize) {
    const AT_NULL: usize = 0;
    loop {
        let tag = *av;
        av = av.add(1);
        let _val = *av;
        av = av.add(1);
        if tag == AT_NULL { break; }
    }
}
