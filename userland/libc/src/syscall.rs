//! Raw Linux-ABI syscall wrappers.
//!
//! AArch64: x8 = syscall number, x0–x5 = arguments, svc #0
//! x86_64:  rax = syscall number, rdi,rsi,rdx,r10,r8,r9 = args 0-5, syscall
//! Return value in x0/rax (negative errno on error).

// ── AArch64 implementations ──────────────────────────────────────────────────

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn syscall0(nr: usize) -> isize {
    let ret: isize;
    core::arch::asm!("svc #0", in("x8") nr, lateout("x0") ret, options(nostack));
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn syscall1(nr: usize, a0: usize) -> isize {
    let ret: isize;
    core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret, options(nostack));
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn syscall2(nr: usize, a0: usize, a1: usize) -> isize {
    let ret: isize;
    core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
         in("x1") a1, options(nostack));
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn syscall3(nr: usize, a0: usize, a1: usize, a2: usize) -> isize {
    let ret: isize;
    core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
         in("x1") a1, in("x2") a2, options(nostack));
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn syscall4(nr: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let ret: isize;
    core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
         in("x1") a1, in("x2") a2, in("x3") a3, options(nostack));
    ret
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn syscall6(nr: usize, a0: usize, a1: usize, a2: usize,
                       a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
         in("x1") a1, in("x2") a2, in("x3") a3,
         in("x4") a4, in("x5") a5, options(nostack));
    ret
}

// ── x86_64 implementations ───────────────────────────────────────────────────
// x86_64 ABI: rax=nr, rdi=a0, rsi=a1, rdx=a2, r10=a3, r8=a4, r9=a5
// syscall clobbers rcx (← user RIP) and r11 (← user RFLAGS).

#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub unsafe fn syscall0(nr: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        out("rcx") _,
        out("r11") _,
        options(nostack),
    );
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub unsafe fn syscall1(nr: usize, a0: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a0,
        out("rcx") _,
        out("r11") _,
        options(nostack),
    );
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub unsafe fn syscall2(nr: usize, a0: usize, a1: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a0,
        in("rsi") a1,
        out("rcx") _,
        out("r11") _,
        options(nostack),
    );
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub unsafe fn syscall3(nr: usize, a0: usize, a1: usize, a2: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a0,
        in("rsi") a1,
        in("rdx") a2,
        out("rcx") _,
        out("r11") _,
        options(nostack),
    );
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub unsafe fn syscall4(nr: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a0,
        in("rsi") a1,
        in("rdx") a2,
        in("r10") a3,   // NOTE: r10, not rcx (rcx is clobbered by syscall)
        out("rcx") _,
        out("r11") _,
        options(nostack),
    );
    ret
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub unsafe fn syscall6(nr: usize, a0: usize, a1: usize, a2: usize,
                       a3: usize, a4: usize, a5: usize) -> isize {
    let ret: isize;
    core::arch::asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") a0,
        in("rsi") a1,
        in("rdx") a2,
        in("r10") a3,
        in("r8")  a4,
        in("r9")  a5,
        out("rcx") _,
        out("r11") _,
        options(nostack),
    );
    ret
}

// ── Fallback stubs for other architectures ───────────────────────────────────

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub unsafe fn syscall0(_nr: usize) -> isize { 0 }
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub unsafe fn syscall1(_nr: usize, _a0: usize) -> isize { 0 }
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub unsafe fn syscall2(_nr: usize, _a0: usize, _a1: usize) -> isize { 0 }
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub unsafe fn syscall3(_nr: usize, _a0: usize, _a1: usize, _a2: usize) -> isize { 0 }
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub unsafe fn syscall4(_nr: usize, _a0: usize, _a1: usize, _a2: usize, _a3: usize) -> isize { 0 }
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
pub unsafe fn syscall6(_nr: usize, _a0: usize, _a1: usize, _a2: usize,
                       _a3: usize, _a4: usize, _a5: usize) -> isize { 0 }

// ── Syscall number constants ──────────────────────────────────────────────────

pub mod nr {
    #[cfg(target_arch = "aarch64")] pub const READ:           usize = 63;
    #[cfg(target_arch = "x86_64")]  pub const READ:           usize = 0;

    #[cfg(target_arch = "aarch64")] pub const WRITE:          usize = 64;
    #[cfg(target_arch = "x86_64")]  pub const WRITE:          usize = 1;

    #[cfg(target_arch = "aarch64")] pub const OPENAT:         usize = 56;
    #[cfg(target_arch = "x86_64")]  pub const OPENAT:         usize = 257;

    #[cfg(target_arch = "aarch64")] pub const CLOSE:          usize = 57;
    #[cfg(target_arch = "x86_64")]  pub const CLOSE:          usize = 3;

    #[cfg(target_arch = "aarch64")] pub const LSEEK:          usize = 62;
    #[cfg(target_arch = "x86_64")]  pub const LSEEK:          usize = 8;

    #[cfg(target_arch = "aarch64")] pub const MMAP:           usize = 222;
    #[cfg(target_arch = "x86_64")]  pub const MMAP:           usize = 9;

    #[cfg(target_arch = "aarch64")] pub const MUNMAP:         usize = 215;
    #[cfg(target_arch = "x86_64")]  pub const MUNMAP:         usize = 11;

    #[cfg(target_arch = "aarch64")] pub const BRK:            usize = 214;
    #[cfg(target_arch = "x86_64")]  pub const BRK:            usize = 12;

    #[cfg(target_arch = "aarch64")] pub const CLONE:          usize = 220;
    #[cfg(target_arch = "x86_64")]  pub const CLONE:          usize = 56;

    #[cfg(target_arch = "aarch64")] pub const EXECVE:         usize = 221;
    #[cfg(target_arch = "x86_64")]  pub const EXECVE:         usize = 59;

    #[cfg(target_arch = "aarch64")] pub const EXIT:           usize = 93;
    #[cfg(target_arch = "x86_64")]  pub const EXIT:           usize = 60;

    #[cfg(target_arch = "aarch64")] pub const EXIT_GROUP:     usize = 94;
    #[cfg(target_arch = "x86_64")]  pub const EXIT_GROUP:     usize = 231;

    #[cfg(target_arch = "aarch64")] pub const WAIT4:          usize = 260;
    #[cfg(target_arch = "x86_64")]  pub const WAIT4:          usize = 61;

    #[cfg(target_arch = "aarch64")] pub const GETPID:         usize = 172;
    #[cfg(target_arch = "x86_64")]  pub const GETPID:         usize = 39;

    #[cfg(target_arch = "aarch64")] pub const GETPPID:        usize = 173;
    #[cfg(target_arch = "x86_64")]  pub const GETPPID:        usize = 110;

    #[cfg(target_arch = "aarch64")] pub const CLOCK_GETTIME:  usize = 113;
    #[cfg(target_arch = "x86_64")]  pub const CLOCK_GETTIME:  usize = 228;

    #[cfg(target_arch = "aarch64")] pub const NANOSLEEP:      usize = 101;
    #[cfg(target_arch = "x86_64")]  pub const NANOSLEEP:      usize = 35;

    #[cfg(target_arch = "aarch64")] pub const KILL:           usize = 129;
    #[cfg(target_arch = "x86_64")]  pub const KILL:           usize = 62;

    #[cfg(target_arch = "aarch64")] pub const GETDENTS64:     usize = 61;
    #[cfg(target_arch = "x86_64")]  pub const GETDENTS64:     usize = 217;

    #[cfg(target_arch = "aarch64")] pub const FSTAT:          usize = 80;
    #[cfg(target_arch = "x86_64")]  pub const FSTAT:          usize = 5;

    #[cfg(target_arch = "aarch64")] pub const NEWFSTATAT:     usize = 79;
    #[cfg(target_arch = "x86_64")]  pub const NEWFSTATAT:     usize = 262;

    #[cfg(target_arch = "aarch64")] pub const FCNTL:          usize = 25;
    #[cfg(target_arch = "x86_64")]  pub const FCNTL:          usize = 72;

    #[cfg(target_arch = "aarch64")] pub const DUP:            usize = 23;
    #[cfg(target_arch = "x86_64")]  pub const DUP:            usize = 32;

    #[cfg(target_arch = "aarch64")] pub const DUP3:           usize = 24;
    #[cfg(target_arch = "x86_64")]  pub const DUP3:           usize = 292;

    #[cfg(target_arch = "aarch64")] pub const PIPE2:          usize = 59;
    #[cfg(target_arch = "x86_64")]  pub const PIPE2:          usize = 293;

    #[cfg(target_arch = "aarch64")] pub const GETCWD:         usize = 17;
    #[cfg(target_arch = "x86_64")]  pub const GETCWD:         usize = 79;

    #[cfg(target_arch = "aarch64")] pub const CHDIR:          usize = 49;
    #[cfg(target_arch = "x86_64")]  pub const CHDIR:          usize = 80;

    #[cfg(target_arch = "aarch64")] pub const MKDIRAT:        usize = 34;
    #[cfg(target_arch = "x86_64")]  pub const MKDIRAT:        usize = 258;

    #[cfg(target_arch = "aarch64")] pub const UNLINKAT:       usize = 35;
    #[cfg(target_arch = "x86_64")]  pub const UNLINKAT:       usize = 263;

    #[cfg(target_arch = "aarch64")] pub const RENAMEAT:       usize = 38;
    #[cfg(target_arch = "x86_64")]  pub const RENAMEAT:       usize = 264;

    #[cfg(target_arch = "aarch64")] pub const SOCKET:         usize = 198;
    #[cfg(target_arch = "x86_64")]  pub const SOCKET:         usize = 41;

    #[cfg(target_arch = "aarch64")] pub const BIND:           usize = 200;
    #[cfg(target_arch = "x86_64")]  pub const BIND:           usize = 49;

    #[cfg(target_arch = "aarch64")] pub const CONNECT:        usize = 203;
    #[cfg(target_arch = "x86_64")]  pub const CONNECT:        usize = 42;

    #[cfg(target_arch = "aarch64")] pub const LISTEN:         usize = 201;
    #[cfg(target_arch = "x86_64")]  pub const LISTEN:         usize = 50;

    #[cfg(target_arch = "aarch64")] pub const ACCEPT4:        usize = 242;
    #[cfg(target_arch = "x86_64")]  pub const ACCEPT4:        usize = 288;

    #[cfg(target_arch = "aarch64")] pub const SENDTO:         usize = 206;
    #[cfg(target_arch = "x86_64")]  pub const SENDTO:         usize = 44;

    #[cfg(target_arch = "aarch64")] pub const RECVFROM:       usize = 207;
    #[cfg(target_arch = "x86_64")]  pub const RECVFROM:       usize = 45;

    #[cfg(target_arch = "aarch64")] pub const SCHED_YIELD:    usize = 124;
    #[cfg(target_arch = "x86_64")]  pub const SCHED_YIELD:    usize = 24;

    #[cfg(target_arch = "aarch64")] pub const FUTEX:          usize = 98;
    #[cfg(target_arch = "x86_64")]  pub const FUTEX:          usize = 202;

    #[cfg(target_arch = "aarch64")] pub const SET_TID_ADDR:   usize = 96;
    #[cfg(target_arch = "x86_64")]  pub const SET_TID_ADDR:   usize = 218;

    #[cfg(target_arch = "aarch64")] pub const RT_SIGACTION:   usize = 134;
    #[cfg(target_arch = "x86_64")]  pub const RT_SIGACTION:   usize = 13;

    #[cfg(target_arch = "aarch64")] pub const RT_SIGPROCMASK: usize = 135;
    #[cfg(target_arch = "x86_64")]  pub const RT_SIGPROCMASK: usize = 14;

    #[cfg(target_arch = "aarch64")] pub const RT_SIGRETURN:   usize = 139;
    #[cfg(target_arch = "x86_64")]  pub const RT_SIGRETURN:   usize = 15;

    #[cfg(target_arch = "aarch64")] pub const MPROTECT:       usize = 226;
    #[cfg(target_arch = "x86_64")]  pub const MPROTECT:       usize = 10;

    #[cfg(target_arch = "aarch64")] pub const UMASK:          usize = 166;
    #[cfg(target_arch = "x86_64")]  pub const UMASK:          usize = 95;

    #[cfg(target_arch = "aarch64")] pub const GETUID:         usize = 174;
    #[cfg(target_arch = "x86_64")]  pub const GETUID:         usize = 102;

    #[cfg(target_arch = "aarch64")] pub const GETGID:         usize = 176;
    #[cfg(target_arch = "x86_64")]  pub const GETGID:         usize = 104;

    #[cfg(target_arch = "aarch64")] pub const GETEUID:        usize = 175;
    #[cfg(target_arch = "x86_64")]  pub const GETEUID:        usize = 107;

    #[cfg(target_arch = "aarch64")] pub const GETEGID:        usize = 177;
    #[cfg(target_arch = "x86_64")]  pub const GETEGID:        usize = 108;

    // Fallback for other architectures (cargo check host)
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const READ: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const WRITE: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const OPENAT: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const CLOSE: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const LSEEK: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const MMAP: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const MUNMAP: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const BRK: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const CLONE: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const EXECVE: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const EXIT: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const EXIT_GROUP: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const WAIT4: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const GETPID: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const GETPPID: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const CLOCK_GETTIME: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const NANOSLEEP: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const KILL: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const GETDENTS64: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const FSTAT: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const NEWFSTATAT: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const FCNTL: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const DUP: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const DUP3: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const PIPE2: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const GETCWD: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const CHDIR: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const MKDIRAT: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const UNLINKAT: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const RENAMEAT: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const SOCKET: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const BIND: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const CONNECT: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const LISTEN: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const ACCEPT4: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const SENDTO: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const RECVFROM: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const SCHED_YIELD: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const FUTEX: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const SET_TID_ADDR: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const RT_SIGACTION: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const RT_SIGPROCMASK: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const RT_SIGRETURN: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const MPROTECT: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const UMASK: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const GETUID: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const GETGID: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const GETEUID: usize = 0;
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    pub const GETEGID: usize = 0;
}
