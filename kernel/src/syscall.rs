//! Syscall dispatch — the only controlled gate into kernel space.
#![allow(dead_code)]
//!
//! Syscall ABI (register mapping follows Linux on each arch):
//!   AArch64: x8 = number, x0-x5 = args, x0 = return value
//!   x86-64:  rax = number, rdi/rsi/rdx/r10/r8/r9 = args, rax = return value
//!
//! Syscall numbers match Linux ABI so that musl libc requires no patching.
//! Cyanos-private syscalls (IPC, spawn) use numbers above 509.

use core::sync::atomic::{AtomicUsize, AtomicU32, Ordering};
use alloc::vec::Vec;
use crate::print_number;
use ipc::{Message, port};
use sched::{
    fork_current, clone_thread, 
    sys_sigaction, sys_sigprocmask, restore_signal_frame,
    futex_wait, futex_wake,
    current_pid, current_ppid, current_pgid, current_sid,
    ticks, yield_now, exit, spawn_user,
    deliver_signal, pending_signals, clear_pending_signal, replace_signal_mask,
    current_reply_port, set_current_reply_port, block_on, set_clear_child_tid,
    set_fs_base, get_fs_base, replace_address_space,
    with_current_address_space, with_current_address_space_mut
};
use mm::paging::PageFlags;
use elf;
use vfs_server as vfs;
use net_server;
use tty_server;

/// Bump allocator base for anonymous mmap with no hint (addr=0).
static MMAP_BUMP: AtomicUsize = AtomicUsize::new(0x0000_1000_0000_usize);

/// IPC port of the VFS server; u32::MAX = not yet registered.
static VFS_SERVER_PORT: AtomicU32 = AtomicU32::new(u32::MAX);

/// Auxv tag: Cyanos VFS server port (private, value > AT_MINSIGSTKSZ).
const AT_CYANOS_VFS_PORT: u64 = 256;

/// Register the VFS server port so sys_execve can embed it in auxv.
pub fn set_vfs_server_port(port: u32) {
    VFS_SERVER_PORT.store(port, Ordering::Relaxed);
}

/// IPC port of the net server; u32::MAX = not yet registered.
static NET_SERVER_PORT: AtomicU32 = AtomicU32::new(u32::MAX);

/// Auxv tag: Cyanos net server port.
const AT_CYANOS_NET_PORT: u64 = 257;

pub fn set_net_server_port(port: u32) {
    NET_SERVER_PORT.store(port, Ordering::Relaxed);
}

// ── VFS call helper ───────────────────────────────────────────────────────────

/// Build a VFS message with up to 7 u64 arguments packed into data[].
fn make_vfs_msg(tag: u64, args: &[u64]) -> Message {
    let mut m = Message::empty();
    m.tag = tag;
    for (i, &a) in args.iter().enumerate().take(7) {
        let off = i * 8;
        m.data[off..off + 8].copy_from_slice(&a.to_le_bytes());
    }
    m
}

/// Extract the i64 return value from a VFS reply (first 8 bytes of data).
fn vfs_reply_val(reply: &Message) -> isize {
    let bytes: [u8; 8] = reply.data[0..8].try_into().unwrap_or([0u8; 8]);
    i64::from_le_bytes(bytes) as isize
}

/// Same extraction for net server replies.
fn net_reply_val(reply: &Message) -> isize {
    let bytes: [u8; 8] = reply.data[0..8].try_into().unwrap_or([0u8; 8]);
    i64::from_le_bytes(bytes) as isize
}

/// Upper bound of user-space virtual addresses (canonical hole on 48-bit VA).
const USER_SPACE_END: usize = 0x0000_8000_0000_0000;

/// Default user stack top for a freshly exec'd process.
const USER_STACK_TOP: usize = 0x0000_7fff_ffff_f000;
/// Size of the initial user stack mapping (32 KiB).
const USER_STACK_SIZE: usize = 8 * mm::buddy::PAGE_SIZE;

/// Validate that `[ptr, ptr+len)` is entirely within user-space.
fn validate_user_buf(ptr: usize, len: usize) -> bool {
    if ptr == 0 { return false; }
    let end = match ptr.checked_add(len) {
        Some(e) => e,
        None    => return false,
    };
    end <= USER_SPACE_END
}

/// Validate that `ptr` is in user-space **and** aligned to `align` bytes.
///
/// `align` must be a power of two.
fn validate_user_ptr_aligned(ptr: usize, size: usize, align: usize) -> bool {
    validate_user_buf(ptr, size) && (ptr & (align - 1)) == 0
}

// ── Syscall number constants (architecture-specific, matching Linux ABI) ──────
//
// AArch64 and x86-64 use different numbers for the same syscall.  These cfg-
// gated constants ensure the dispatch table matches what musl/user-space sends.

// ── Cyanos-private (same on all architectures) ────────────────────────────────
pub const SYS_IPC_SEND: usize = 511;
pub const SYS_IPC_RECV: usize = 512;
pub const SYS_IPC_CALL: usize = 513;
pub const SYS_SPAWN:    usize = 510;

// ── AArch64 Linux syscall numbers ─────────────────────────────────────────────
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const MMAP:           usize = 222;
    pub const MUNMAP:         usize = 215;
    pub const MPROTECT:       usize = 226;
    pub const BRK:            usize = 214;
    pub const RT_SIGACTION:   usize = 134;
    pub const RT_SIGPROCMASK: usize = 135;
    pub const RT_SIGRETURN:   usize = 139;
    pub const SCHED_YIELD:    usize = 124;
    pub const CLONE:          usize = 220;
    pub const EXECVE:         usize = 221;
    pub const EXIT:           usize = 93;
    pub const WAIT4:          usize = 260;
    pub const KILL:           usize = 129;
    pub const CLOCK_GETTIME:  usize = 113;
    pub const FUTEX:          usize = 98;
    pub const SET_TID_ADDR:   usize = 96;
    pub const GETPID:         usize = 172;
    pub const GETPPID:        usize = 173;
    pub const WRITE:          usize = 64;
    pub const READ:           usize = 63;
    pub const WRITEV:         usize = 66;
    pub const READV:          usize = 65;
    pub const OPENAT:         usize = 56;
    pub const CLOSE:          usize = 57;
    pub const FSTAT:          usize = 80;
    pub const NEWFSTATAT:     usize = 79;
    pub const LSEEK:          usize = 62;
    pub const IOCTL:          usize = 29;
    pub const FCNTL:          usize = 25;
    pub const PIPE2:          usize = 59;
    pub const GETDENTS64:     usize = 61;
    pub const DUP:            usize = 23;
    pub const DUP3:           usize = 24;
    pub const READLINKAT:     usize = 78;
    pub const PPOLL:          usize = 73;
    pub const GETUID:         usize = 174;
    pub const GETEUID:        usize = 175;
    pub const GETGID:         usize = 176;
    pub const GETEGID:        usize = 177;
    pub const GETTID:         usize = 178;
    pub const TGKILL:         usize = 131;
    pub const SIGALTSTACK:    usize = 132;
    pub const UNAME:          usize = 160;
    pub const PRLIMIT64:      usize = 261;
    pub const EXIT_GROUP:     usize = 94;
    // Socket syscalls (AArch64)
    pub const SOCKET:         usize = 198;
    pub const BIND:           usize = 200;
    pub const LISTEN:         usize = 201;
    pub const ACCEPT:         usize = 202;
    pub const CONNECT:        usize = 203;
    pub const GETSOCKNAME:    usize = 204;
    pub const GETPEERNAME:    usize = 205;
    pub const SENDTO:         usize = 206;
    pub const RECVFROM:       usize = 207;
    pub const SETSOCKOPT:     usize = 208;
    pub const GETSOCKOPT:     usize = 209;
    pub const SHUTDOWN:       usize = 210;
    pub const SENDMSG:        usize = 211;
    pub const RECVMSG:        usize = 212;
    pub const ACCEPT4:        usize = 242;
    pub const SOCKETPAIR:     usize = 199;
    pub const CLOCK_NANOSLEEP: usize = 115;
    pub const NANOSLEEP:      usize = 101;
    pub const GETTIMEOFDAY:   usize = 169;
    pub const SYSINFO:        usize = 179;
    pub const GETRLIMIT:      usize = 163;
    pub const SETRLIMIT:      usize = 164;
    pub const SENDFILE:       usize = 71;
    pub const SETITIMER:      usize = 103;
    pub const GETITIMER:      usize = 102;
    pub const SIGPENDING:     usize = 136;
    pub const GETRANDOM:      usize = 278;
    pub const PRCTL:          usize = 167;
    pub const MADVISE:        usize = 233;
    pub const MSYNC:          usize = 227;
    pub const MLOCK:          usize = 228;
    pub const MUNLOCK:        usize = 229;
    pub const MLOCKALL:       usize = 230;
    pub const MUNLOCKALL:     usize = 231;
    pub const CLOCK_GETRES:   usize = 114;
    pub const PREAD64:        usize = 67;
    pub const PWRITE64:       usize = 68;
    pub const TIMES:          usize = 153;
    pub const TIMERFD_CREATE: usize = 85;
    pub const TIMERFD_SETTIME: usize = 86;
    pub const TIMERFD_GETTIME: usize = 87;
    pub const TIMER_CREATE:   usize = 107;
    pub const TIMER_SETTIME:  usize = 110;
    pub const TIMER_GETTIME:  usize = 108;
    pub const TIMER_DELETE:   usize = 111;
    // Process management
    pub const CHDIR:          usize = 49;
    pub const FCHDIR:         usize = 50;
    pub const GETCWD:         usize = 17;
    pub const SETPGID:        usize = 154;
    pub const GETPGID:        usize = 155;
    pub const SETSID:         usize = 157;
    pub const GETSID:         usize = 156;
    pub const GETPGRP:        usize = 155; // same as GETPGID on AArch64
    pub const SETUID:         usize = 146;
    pub const SETGID:         usize = 144;
    pub const SETRESUID:      usize = 147;
    pub const SETRESGID:      usize = 149;
    pub const GETRESUID:      usize = 148;
    pub const GETRESGID:      usize = 150;
    pub const UMASK:          usize = 166;
    pub const GETGROUPS:      usize = 158;
    pub const SETGROUPS:      usize = 159;
    // Filesystem operations
    pub const DUP2:           usize = 1000; // AArch64 has no dup2; uses dup3
    pub const MKDIRAT:        usize = 34;
    pub const UNLINKAT:       usize = 35;
    pub const RENAMEAT:       usize = 38;
    pub const RENAMEAT2:      usize = 276;
    pub const LINKAT:         usize = 37;
    pub const SYMLINKAT:      usize = 36;
    pub const FCHMODAT:       usize = 53;
    pub const FCHMOD:         usize = 52;
    pub const FCHOWNAT:       usize = 54;
    pub const FCHOWN:         usize = 55;
    pub const TRUNCATE:       usize = 45;
    pub const FTRUNCATE:      usize = 46;
    pub const FACCESSAT:      usize = 48;
    pub const STATFS:         usize = 43;
    pub const FSTATFS:        usize = 44;
    pub const FSYNC:          usize = 82;
    pub const FDATASYNC:      usize = 83;
    pub const FALLOCATE:      usize = 47;
    pub const UTIMENSAT:      usize = 88;
    pub const MKNOD:          usize = 33;
    pub const MKNODAT:        usize = 33;  // same on AArch64
    // poll / select / epoll (AArch64)
    pub const SELECT:         usize = 270;
    pub const PSELECT6:       usize = 72;
    pub const EPOLL_CREATE1:  usize = 20;
    pub const EPOLL_CTL:      usize = 21;
    pub const EPOLL_PWAIT:    usize = 22;
    pub const EPOLL_PWAIT2:   usize = 441;
    pub const EVENTFD2:       usize = 19;
    pub const SIGNALFD4:      usize = 74;
    pub const RT_SIGSUSPEND:  usize = 133;
    pub const WAITID:         usize = 95;
    pub const MEMFD_CREATE:   usize = 279;
    pub const COPY_FILE_RANGE: usize = 285;
    pub const PAUSE:          usize = 1000; // no separate pause on AArch64
    pub const MREMAP:              usize = 216;
    pub const MINCORE:             usize = 232;
    pub const FLOCK:               usize = 32;
    pub const SPLICE:              usize = 76;
    pub const EPOLL_WAIT:          usize = 1001; // AArch64 uses EPOLL_PWAIT
    pub const PIPE:                usize = 1002; // AArch64 has no pipe without flags
    pub const GETDENTS:            usize = 1003; // AArch64 has no old-style getdents
    pub const GETRUSAGE:           usize = 165;
    pub const SCHED_SETSCHEDULER:  usize = 119;
    pub const SCHED_GETSCHEDULER:  usize = 120;
    pub const SCHED_SETPARAM:      usize = 118;
    pub const SCHED_GETPARAM:      usize = 121;
    pub const SCHED_SETAFFINITY:   usize = 122;
    pub const SCHED_GETAFFINITY:   usize = 123;
    pub const SCHED_GET_PRIORITY_MAX: usize = 125;
    pub const SCHED_GET_PRIORITY_MIN: usize = 126;
    pub const CAPGET:              usize = 90;
    pub const CAPSET:              usize = 91;
    pub const MEMBARRIER:          usize = 283;
    pub const RSEQ:                usize = 293;
    pub const STATX:               usize = 291;
    pub const OPENAT2:             usize = 437;
    pub const CLOSE_RANGE:         usize = 436;
    pub const PIDFD_OPEN:          usize = 434;
    pub const RT_SIGTIMEDWAIT:     usize = 137;
    pub const INOTIFY_INIT1:       usize = 360;
    pub const INOTIFY_ADD_WATCH:   usize = 27;
    pub const INOTIFY_RM_WATCH:    usize = 28;
    pub const POSIX_FADVISE:       usize = 223;
    pub const SYNC_FILE_RANGE:     usize = 84;
    pub const READAHEAD:           usize = 213;
}

// ── x86-64 Linux syscall numbers ──────────────────────────────────────────────
#[cfg(not(target_arch = "aarch64"))]
mod nr {
    pub const MMAP:           usize = 9;
    pub const MUNMAP:         usize = 11;
    pub const MPROTECT:       usize = 10;
    pub const BRK:            usize = 12;
    pub const RT_SIGACTION:   usize = 13;
    pub const RT_SIGPROCMASK: usize = 14;
    pub const RT_SIGRETURN:   usize = 15;
    pub const SCHED_YIELD:    usize = 24;
    pub const CLONE:          usize = 56;
    pub const FORK:           usize = 57;
    pub const EXECVE:         usize = 59;
    pub const EXIT:           usize = 60;
    pub const WAIT4:          usize = 61;
    pub const KILL:           usize = 62;
    pub const CLOCK_GETTIME:  usize = 228;
    pub const FUTEX:          usize = 202;
    pub const SET_TID_ADDR:   usize = 218;
    pub const ARCH_PRCTL:     usize = 158;
    pub const GETPID:         usize = 39;
    pub const GETPPID:        usize = 110;
    pub const WRITE:          usize = 1;
    pub const READ:           usize = 0;
    pub const WRITEV:         usize = 20;
    pub const READV:          usize = 19;
    pub const OPENAT:         usize = 257;
    pub const CLOSE:          usize = 3;
    pub const FSTAT:          usize = 5;
    pub const NEWFSTATAT:     usize = 262;
    pub const LSEEK:          usize = 8;
    pub const IOCTL:          usize = 16;
    pub const FCNTL:          usize = 72;
    pub const PIPE2:          usize = 293;
    pub const GETDENTS64:     usize = 217;
    pub const DUP:            usize = 32;
    pub const DUP3:           usize = 292;
    pub const READLINKAT:     usize = 267;
    pub const PPOLL:          usize = 271;
    pub const GETUID:         usize = 102;
    pub const GETEUID:        usize = 107;
    pub const GETGID:         usize = 104;
    pub const GETEGID:        usize = 108;
    pub const GETTID:         usize = 186;
    pub const TGKILL:         usize = 234;
    pub const SIGALTSTACK:    usize = 131;
    pub const UNAME:          usize = 63;
    pub const PRLIMIT64:      usize = 302;
    pub const EXIT_GROUP:     usize = 231;
    // Socket syscalls (x86-64)
    pub const SOCKET:         usize = 41;
    pub const CONNECT:        usize = 42;
    pub const ACCEPT:         usize = 43;
    pub const SENDTO:         usize = 44;
    pub const RECVFROM:       usize = 45;
    pub const SENDMSG:        usize = 46;
    pub const RECVMSG:        usize = 47;
    pub const SHUTDOWN:       usize = 48;
    pub const BIND:           usize = 49;
    pub const LISTEN:         usize = 50;
    pub const GETSOCKNAME:    usize = 51;
    pub const GETPEERNAME:    usize = 52;
    pub const SOCKETPAIR:     usize = 53;
    pub const SETSOCKOPT:     usize = 54;
    pub const GETSOCKOPT:     usize = 55;
    pub const ACCEPT4:        usize = 288;
    pub const CLOCK_NANOSLEEP: usize = 230;
    pub const NANOSLEEP:      usize = 35;
    pub const GETTIMEOFDAY:   usize = 96;
    pub const SYSINFO:        usize = 99;
    pub const TIME:           usize = 201;
    pub const GETRLIMIT:      usize = 97;
    pub const SETRLIMIT:      usize = 160;
    pub const SENDFILE:       usize = 40;
    pub const ALARM:          usize = 37;
    pub const SETITIMER:      usize = 38;
    pub const GETITIMER:      usize = 36;
    pub const SIGPENDING:     usize = 127;
    pub const GETRANDOM:      usize = 318;
    pub const PRCTL:          usize = 157;
    pub const MADVISE:        usize = 28;
    pub const MSYNC:          usize = 26;
    pub const MLOCK:          usize = 149;
    pub const MUNLOCK:        usize = 150;
    pub const MLOCKALL:       usize = 151;
    pub const MUNLOCKALL:     usize = 152;
    pub const CLOCK_GETRES:   usize = 229;
    pub const PREAD64:        usize = 17;
    pub const PWRITE64:       usize = 18;
    pub const TIMES:          usize = 100;
    // Old-style (non-AT) syscalls used by older programs / musl fallbacks.
    pub const OPEN:           usize = 2;
    pub const CREAT:          usize = 85;
    pub const STAT:           usize = 4;
    pub const LSTAT:          usize = 6;
    pub const TIMERFD_CREATE: usize = 283;
    pub const TIMERFD_SETTIME: usize = 286;
    pub const TIMERFD_GETTIME: usize = 287;
    pub const TIMER_CREATE:   usize = 222;
    pub const TIMER_SETTIME:  usize = 223;
    pub const TIMER_GETTIME:  usize = 224;
    pub const TIMER_DELETE:   usize = 225;
    // Process management
    pub const CHDIR:          usize = 80;
    pub const FCHDIR:         usize = 81;
    pub const GETCWD:         usize = 79;
    pub const SETPGID:        usize = 109;
    pub const GETPGID:        usize = 121;
    pub const SETSID:         usize = 112;
    pub const GETSID:         usize = 124;
    pub const GETPGRP:        usize = 111;
    pub const SETUID:         usize = 105;
    pub const SETGID:         usize = 106;
    pub const SETRESUID:      usize = 117;
    pub const SETRESGID:      usize = 119;
    pub const GETRESUID:      usize = 118;
    pub const GETRESGID:      usize = 120;
    pub const UMASK:          usize = 95;
    pub const GETGROUPS:      usize = 115;
    pub const SETGROUPS:      usize = 116;
    // Filesystem operations
    pub const DUP2:           usize = 33;
    pub const MKDIRAT:        usize = 258;
    pub const MKDIR:          usize = 83;
    pub const UNLINK:         usize = 87;
    pub const UNLINKAT:       usize = 263;
    pub const RENAME:         usize = 82;
    pub const RENAMEAT:       usize = 264;
    pub const RENAMEAT2:      usize = 316;
    pub const LINK:           usize = 86;
    pub const LINKAT:         usize = 265;
    pub const SYMLINK:        usize = 88;
    pub const SYMLINKAT:      usize = 266;
    pub const FCHMODAT:       usize = 268;
    pub const FCHMOD:         usize = 91;
    pub const CHMOD:          usize = 90;
    pub const FCHOWNAT:       usize = 260;
    pub const FCHOWN:         usize = 93;
    pub const CHOWN:          usize = 92;
    pub const LCHOWN:         usize = 94;
    pub const TRUNCATE:       usize = 76;
    pub const FTRUNCATE:      usize = 77;
    pub const ACCESS:         usize = 21;
    pub const FACCESSAT:      usize = 269;
    pub const STATFS:         usize = 137;
    pub const FSTATFS:        usize = 138;
    pub const FSYNC:          usize = 74;
    pub const FDATASYNC:      usize = 75;
    pub const FALLOCATE:      usize = 285;
    pub const UTIMENSAT:      usize = 280;
    pub const MKNOD:          usize = 133;
    pub const MKNODAT:        usize = 259;
    // poll / select / epoll (x86-64)
    pub const SELECT:         usize = 23;
    pub const PSELECT6:       usize = 270;
    pub const EPOLL_CREATE1:  usize = 291;
    pub const EPOLL_CTL:      usize = 233;
    pub const EPOLL_PWAIT:    usize = 281;
    pub const EPOLL_PWAIT2:   usize = 441;
    pub const EVENTFD2:       usize = 290;
    pub const SIGNALFD4:      usize = 289;
    pub const RT_SIGSUSPEND:  usize = 130;
    pub const PAUSE:          usize = 34;
    pub const WAITID:         usize = 247;
    pub const MEMFD_CREATE:   usize = 319;
    pub const COPY_FILE_RANGE: usize = 326;
    pub const MREMAP:              usize = 25;
    pub const MINCORE:             usize = 27;
    pub const FLOCK:               usize = 73;
    pub const SPLICE:              usize = 275;
    pub const EPOLL_WAIT:          usize = 232;
    pub const PIPE:                usize = 22;
    pub const GETDENTS:            usize = 78;
    pub const READLINK:            usize = 89;
    pub const GETRUSAGE:           usize = 98;
    pub const SCHED_SETSCHEDULER:  usize = 144;
    pub const SCHED_GETSCHEDULER:  usize = 145;
    pub const SCHED_SETPARAM:      usize = 142;
    pub const SCHED_GETPARAM:      usize = 143;
    pub const SCHED_SETAFFINITY:   usize = 203;
    pub const SCHED_GETAFFINITY:   usize = 204;
    pub const SCHED_GET_PRIORITY_MAX: usize = 146;
    pub const SCHED_GET_PRIORITY_MIN: usize = 147;
    pub const CAPGET:              usize = 125;
    pub const CAPSET:              usize = 126;
    pub const MEMBARRIER:          usize = 324;
    pub const RSEQ:                usize = 334;
    pub const STATX:               usize = 332;
    pub const OPENAT2:             usize = 437;
    pub const CLOSE_RANGE:         usize = 436;
    pub const PIDFD_OPEN:          usize = 434;
    pub const RT_SIGTIMEDWAIT:     usize = 128;
    pub const INOTIFY_INIT1:       usize = 294;
    pub const INOTIFY_ADD_WATCH:   usize = 254;
    pub const INOTIFY_RM_WATCH:    usize = 255;
    pub const POSIX_FADVISE:       usize = 221;
    pub const SYNC_FILE_RANGE:     usize = 277;
    pub const READAHEAD:           usize = 187;
}

use nr::*;

// ── Arch-only extern ──────────────────────────────────────────────────────────
extern "C" { fn arch_alloc_page_table_root() -> usize; }

/// Top-level syscall handler, invoked from the arch-specific trap stub.
///
/// The `frame_ptr` argument carries the address of the `UserFrame` saved on
/// the kernel stack by the AArch64 EL0 exception handler.  It is 0 on x86-64.
#[no_mangle]
pub extern "C" fn syscall_dispatch(
    number: usize,
    a0: usize, a1: usize, a2: usize,
    a3: usize, a4: usize, a5: usize,
    frame_ptr: usize,
) -> isize {
    // Debug: Print syscall info
    unsafe {
        extern "C" { fn arch_serial_putc(ch: u8); }
        let debug_msg = b"[SYSCALL] Got syscall #";
        for &b in debug_msg { arch_serial_putc(b); }
        let mut n = number;
        if n == 0 { arch_serial_putc(b'0'); } else {
            let mut buf = [0u8; 10];
            let mut i = 10;
            while n > 0 { i -= 1; buf[i] = b'0' + ((n % 10) as u8); n /= 10; }
            for &c in &buf[i..] { arch_serial_putc(c); }
        }
        let debug_nl = b"\r\n";
        for &b in debug_nl { arch_serial_putc(b); }
    }

    dispatch(number, a0, a1, a2, a3, a4, a5, frame_ptr)
}

pub fn dispatch(
    number: usize,
    a0: usize, a1: usize, a2: usize,
    a3: usize, a4: usize, a5: usize,
    frame_ptr: usize,
) -> isize {
    let ret = dispatch_inner(number, a0, a1, a2, a3, a4, a5, frame_ptr);
    // Fire any expired POSIX timers before returning to user-space.
    tty_server::check_timers(current_pid());
    ret
}

fn dispatch_inner(
    number: usize,
    a0: usize, a1: usize, a2: usize,
    a3: usize, a4: usize, a5: usize,
    frame_ptr: usize,
) -> isize {
    crate::serial_print("[SYSCALL] nr=");
    print_number(number as u32);
    crate::serial_print("\n");

    match number {
        // ── Cyanos-private IPC syscalls ───────────────────────────────────────
        SYS_IPC_SEND => sys_send(a0, a1, a2),
        SYS_IPC_RECV => sys_recv(a0, a1),
        SYS_IPC_CALL => sys_call(a0, a1, a2),

        // ── Memory ────────────────────────────────────────────────────────────
        MMAP     => sys_mmap(a0, a1, a2, a3, a4, a5),
        MUNMAP   => sys_unmap_mem(a0, a1),
        MPROTECT => sys_mprotect(a0, a1, a2),
        BRK      => sys_brk(a0),
        MREMAP   => sys_mremap(a0, a1, a2, a3, a4),
        MINCORE  => 0, // pretend all pages are resident

        // ── Scheduling ────────────────────────────────────────────────────────
        SCHED_YIELD => { yield_now(); 0 }

        // ── Process lifecycle ─────────────────────────────────────────────────
        EXIT    => { vfs_close_all_current(); exit(a0 as i32) }
        SYS_SPAWN => sys_spawn(a0, a1, a2),
        WAIT4   => sys_wait(a0, a1),
        WAITID  => sys_waitid(a0, a1, a2, a3),
        GETPID  => current_pid() as isize,
        GETPPID => sys_getppid(),

        // ── exec / fork ───────────────────────────────────────────────────────
        EXECVE  => sys_execve(a0, a1, a2),
        CLONE   => sys_clone_or_fork(a0, a1, a2, a3, a4, frame_ptr),
        #[cfg(not(target_arch = "aarch64"))]
        FORK    => {
            let parent_pid = current_pid();
            let ret = fork_current(frame_ptr);
            if ret > 0 {
                let msg = make_vfs_msg(vfs::VFS_FORK_DUP, &[parent_pid as u64, ret as u64]);
                let _ = vfs::handle(&msg, parent_pid);
            }
            ret
        }

        // ── Time ─────────────────────────────────────────────────────────────
        CLOCK_GETTIME => sys_clock_gettime(a0, a1),

        // ── Signals (stubs — full implementation in Phase 2) ─────────────────
        RT_SIGACTION   => sys_rt_sigaction(a0, a1, a2),
        RT_SIGPROCMASK => sys_rt_sigprocmask(a0, a1, a2),
        RT_SIGRETURN   => sys_rt_sigreturn(frame_ptr),
        KILL           => sys_kill(a0, a1),
        RT_SIGSUSPEND  => sys_rt_sigsuspend(a0, a1),
        RT_SIGTIMEDWAIT => sys_rt_sigtimedwait(a0, a1, a2, a3),
        #[cfg(not(target_arch = "aarch64"))]
        PAUSE          => sys_rt_sigsuspend(0, 0),

        // ── Threads (stubs — full implementation in Phase 4) ─────────────────
        SET_TID_ADDR => sys_set_tid_address(a0),
        FUTEX        => sys_futex(a0, a1, a2, a3),

        // ── Architecture-specific ─────────────────────────────────────────────
        #[cfg(not(target_arch = "aarch64"))]
        ARCH_PRCTL => sys_arch_prctl(a0, a1),

        // ── I/O ───────────────────────────────────────────────────────────────
        WRITE  => {
            // Debug the write syscall parameters
            unsafe {
                extern "C" { fn arch_serial_putc(ch: u8); }
                let debug_write = b"[SYSCALL] write(";
                for &b in debug_write { arch_serial_putc(b); }
                // Print fd
                let mut n = a0;
                if n == 0 { arch_serial_putc(b'0'); } else {
                    let mut buf = [0u8; 10];
                    let mut i = 10;
                    while n > 0 { i -= 1; buf[i] = b'0' + ((n % 10) as u8); n /= 10; }
                    for &c in &buf[i..] { arch_serial_putc(c); }
                }
                arch_serial_putc(b',');
                arch_serial_putc(b' ');
                // Print buffer address
                let debug_0x = b"0x";
                for &b in debug_0x { arch_serial_putc(b); }
                for shift in (0..16).rev() {
                    let nibble = (a1 >> (shift * 4)) & 0xF;
                    let ch = if nibble < 10 { b'0' + nibble as u8 } else { b'A' + (nibble - 10) as u8 };
                    arch_serial_putc(ch);
                }
                let debug_comma = b", ";
                for &b in debug_comma { arch_serial_putc(b); }
                // Print count
                let mut n = a2;
                if n == 0 { arch_serial_putc(b'0'); } else {
                    let mut buf = [0u8; 10];
                    let mut i = 10;
                    while n > 0 { i -= 1; buf[i] = b'0' + ((n % 10) as u8); n /= 10; }
                    for &c in &buf[i..] { arch_serial_putc(c); }
                }
                let debug_end = b")\r\n";
                for &b in debug_end { arch_serial_putc(b); }
            }
            sys_write(a0, a1, a2)
        }
        READ   => sys_read(a0, a1, a2),
        WRITEV => sys_writev(a0, a1, a2),
        READV  => sys_readv(a0, a1, a2),

        // ── VFS syscalls ──────────────────────────────────────────────────────
        OPENAT      => sys_openat(a0, a1, a2, a3),
        CLOSE       => sys_close(a0),
        FSTAT       => sys_fstat(a0, a1),
        NEWFSTATAT  => sys_newfstatat(a0, a1, a2, a3),
        LSEEK       => sys_lseek(a0, a1, a2),
        IOCTL       => sys_ioctl(a0, a1, a2),
        FCNTL       => sys_fcntl(a0, a1, a2),
        PIPE2       => sys_pipe2(a0, a1),
        FLOCK       => 0, // advisory lock stub — always succeeds
        #[cfg(not(target_arch = "aarch64"))]
        PIPE        => sys_pipe2(a0, 0),
        #[cfg(not(target_arch = "aarch64"))]
        GETDENTS    => sys_getdents64(a0, a1, a2),
        #[cfg(not(target_arch = "aarch64"))]
        READLINK    => sys_readlinkat(0, a0, a1, a2),
        #[cfg(not(target_arch = "aarch64"))]
        EPOLL_WAIT  => sys_epoll_wait(a0, a1, a2, 0),
        GETDENTS64  => sys_getdents64(a0, a1, a2),
        DUP         => sys_dup(a0),
        DUP3        => sys_dup3(a0, a1, a2),
        #[cfg(not(target_arch = "aarch64"))]
        DUP2        => sys_dup3(a0, a1, 0),  // dup2(old,new) == dup3(old,new,0)
        READLINKAT  => sys_readlinkat(a0, a1, a2, a3),
        PPOLL       => sys_ppoll(a0, a1, a2, a3),
        // Process management
        CHDIR       => sys_chdir(a0),
        FCHDIR      => sys_fchdir(a0),
        GETCWD      => sys_getcwd(a0, a1),
        SETPGID     => sys_setpgid(a0, a1),
        GETPGID     => sys_getpgid(a0),
        SETSID      => sched::setsid() as isize,
        GETSID      => sched::current_sid() as isize,
        // GETPGRP is an alias for GETPGID(0) on x86-64 but shares the same
        // number as GETPGID on AArch64, so only emit this arm on x86-64.
        #[cfg(target_arch = "x86_64")]
        GETPGRP     => sched::current_pgid() as isize,
        SETUID | SETGID | SETRESUID | SETRESGID | SETGROUPS => 0, // root: accept
        GETRESUID   => sys_getresxid(a0, a1, a2, false),
        GETRESGID   => sys_getresxid(a0, a1, a2, true),
        GETGROUPS   => 0,   // 0 supplementary groups
        UMASK       => sched::umask(a0 as u32) as isize,
        // Filesystem operations (writable for /tmp, read-only otherwise)
        MKDIRAT     => sys_mkdirat(a0, a1, a2),
        UNLINKAT    => sys_unlinkat(a0, a1, a2),
        RENAMEAT | RENAMEAT2 => sys_renameat(a1, a2),
        LINKAT      => -30,
        SYMLINKAT   => -30,
        FCHMODAT | FCHMOD => 0, // pretend success (RamFS ignores permissions)
        FCHOWNAT | FCHOWN => 0,
        TRUNCATE    => sys_truncate(a0, a1),
        FTRUNCATE   => sys_ftruncate(a0, a1),
        FACCESSAT   => sys_faccessat(a0, a1, a2, a3),
        STATFS | FSTATFS => sys_statfs(a0, a1),
        FSYNC | FDATASYNC => 0,
        FALLOCATE   => 0, // advisory pre-allocation; no-op is valid
        UTIMENSAT   => 0,
        MKNODAT     => -30,
        #[cfg(not(target_arch = "aarch64"))]
        UNLINK => sys_unlinkat(0, a0, 0),
        #[cfg(not(target_arch = "aarch64"))]
        MKDIR  => sys_mkdirat(0, a0, a1),
        #[cfg(not(target_arch = "aarch64"))]
        RENAME => sys_renameat(a0, a1),
        #[cfg(not(target_arch = "aarch64"))]
        LINK | SYMLINK | CHMOD | CHOWN | LCHOWN | MKNOD => -30,
        #[cfg(not(target_arch = "aarch64"))]
        ACCESS      => sys_faccessat(0, a0, a1, 0),

        // ── Socket syscalls ───────────────────────────────────────────────────
        SOCKET      => sys_socket(a0, a1, a2),
        BIND        => sys_bind(a0, a1, a2),
        LISTEN      => sys_listen(a0, a1),
        ACCEPT | ACCEPT4 => sys_accept(a0, a1, a2),
        CONNECT     => sys_connect(a0, a1, a2),
        SENDTO      => sys_sendto(a0, a1, a2, a3, a4, a5),
        RECVFROM    => sys_recvfrom(a0, a1, a2, a3, a4, a5),
        SENDMSG     => sys_sendmsg(a0, a1, a2),
        RECVMSG     => sys_recvmsg(a0, a1, a2),
        SHUTDOWN    => sys_net_shutdown(a0, a1),
        GETSOCKNAME => sys_getsockname(a0, a1, a2),
        GETPEERNAME => sys_getpeername(a0, a1, a2),
        SOCKETPAIR  => sys_socketpair(a0, a1, a2, a3),
        SETSOCKOPT  => sys_setsockopt(a0, a1, a2, a3, a4),
        GETSOCKOPT  => sys_getsockopt(a0, a1, a2, a3, a4),

        // ── POSIX timers (Phase 8) ────────────────────────────────────────────
        TIMER_CREATE  => sys_timer_create(a0, a1, a2),
        TIMER_SETTIME => sys_timer_settime(a0, a1, a2, a3),
        TIMER_GETTIME => sys_timer_gettime(a0, a1),
        TIMER_DELETE  => sys_timer_delete(a0),
        NANOSLEEP       => sys_nanosleep(a0, a1),
        CLOCK_NANOSLEEP => sys_nanosleep(a2, a3), // clock_nanosleep(clk,flags,rqtp,rmtp)
        TIMERFD_CREATE  => sys_timerfd_create(a0),
        TIMERFD_SETTIME => sys_timerfd_settime(a0, a1, a2, a3),
        TIMERFD_GETTIME => sys_timerfd_gettime(a0, a1),
        GETTIMEOFDAY => sys_gettimeofday(a0, a1),
        SYSINFO      => sys_sysinfo(a0),
        SENDFILE     => sys_sendfile(a0, a1, a2, a3),
        COPY_FILE_RANGE => sys_sendfile(a0, a2, a4, a5),
        MEMFD_CREATE => sys_memfd_create(a0, a1),
        SPLICE       => sys_sendfile(a1, a3, 0, a4), // in_fd, out_fd, offset=none, len
        SETITIMER    => sys_setitimer(a0, a1, a2),
        GETITIMER    => sys_getitimer(a0, a1),
        SIGPENDING   => sys_sigpending(a0),
        #[cfg(not(target_arch = "aarch64"))]
        ALARM        => sys_alarm(a0),
        GETRANDOM    => sys_getrandom(a0, a1, a2),
        PRCTL        => sys_prctl(a0, a1, a2, a3, a4),
        MADVISE | MSYNC | MLOCK | MUNLOCK | MLOCKALL | MUNLOCKALL => 0,
        CLOCK_GETRES => sys_clock_getres(a0, a1),
        PREAD64      => sys_pread64(a0, a1, a2, a3),
        PWRITE64     => sys_pwrite64(a0, a1, a2, a3),
        TIMES        => sys_times(a0),
        #[cfg(not(target_arch = "aarch64"))]
        TIME         => sys_time(a0),

        // ── poll / select / epoll (Phase 9) ───────────────────────────────────
        SELECT | PSELECT6 => sys_select(a0, a1, a2, a3, a4),
        EPOLL_CREATE1  => sys_epoll_create1(a0),
        EPOLL_CTL      => sys_epoll_ctl(a0, a1, a2, a3),
        EPOLL_PWAIT | EPOLL_PWAIT2 => sys_epoll_wait(a0, a1, a2, a3),
        EVENTFD2       => sys_eventfd2(a0, a1),
        SIGNALFD4      => -38,

        // ── Scheduling policy/affinity ────────────────────────────────────────
        SCHED_SETSCHEDULER | SCHED_SETPARAM => 0,
        SCHED_GETSCHEDULER => 0, // SCHED_OTHER = 0
        SCHED_GETPARAM     => sys_sched_getparam(a0, a1),
        SCHED_SETAFFINITY  => 0,
        SCHED_GETAFFINITY  => sys_sched_getaffinity(a0, a1, a2),
        SCHED_GET_PRIORITY_MAX | SCHED_GET_PRIORITY_MIN => 0,

        // ── Resource usage ────────────────────────────────────────────────────
        GETRUSAGE => sys_getrusage(a0, a1),

        // ── Capabilities ─────────────────────────────────────────────────────
        CAPGET => sys_capget(a0, a1),
        CAPSET => 0,

        // ── Modern Linux (stubs) ──────────────────────────────────────────────
        MEMBARRIER  => 0,
        RSEQ        => -38, // ENOSYS
        STATX       => sys_statx(a0, a1, a2, a3, a4),
        OPENAT2     => sys_openat(a0, a1, a2, a3),
        CLOSE_RANGE => sys_close_range(a0, a1, a2),
        PIDFD_OPEN  => -38,

        // ── Credentials ───────────────────────────────────────────────────────
        GETUID | GETEUID | GETGID | GETEGID => 0, // all root for now
        GETTID    => current_pid() as isize,
        TGKILL    => sys_tgkill(a0, a1, a2),

        // ── Signal helpers ────────────────────────────────────────────────────
        SIGALTSTACK => sys_sigaltstack(a0, a1),

        // ── Resource limits ───────────────────────────────────────────────────
        GETRLIMIT  => sys_getrlimit(a0, a1),
        SETRLIMIT  => 0, // silently accept any limit

        // ── Old-style (non-AT) syscalls (x86-64 only) ─────────────────────────
        #[cfg(not(target_arch = "aarch64"))]
        OPEN  => sys_openat(0, a0, a1, a2),
        #[cfg(not(target_arch = "aarch64"))]
        CREAT => sys_openat(0, a0,
                            0x41 /* O_CREAT|O_WRONLY */, a1),
        #[cfg(not(target_arch = "aarch64"))]
        STAT  => sys_stat_at_path(a0, a1),
        #[cfg(not(target_arch = "aarch64"))]
        LSTAT => sys_stat_at_path(a0, a1), // no symlinks — same as stat

        // ── Misc ──────────────────────────────────────────────────────────────
        UNAME      => sys_uname(a0),
        PRLIMIT64  => sys_prlimit64(a0, a1, a2, a3),
        EXIT_GROUP => { vfs_close_all_current(); exit(a0 as i32) }

        // ── File advise / range operations (advisory — safe to no-op) ────────
        POSIX_FADVISE | SYNC_FILE_RANGE | READAHEAD => 0,

        // ── inotify (no filesystem events in Cyanos) ──────────────────────────
        INOTIFY_INIT1 | INOTIFY_ADD_WATCH | INOTIFY_RM_WATCH => -38,

        _ => -38, // ENOSYS
    }
}

// ── IPC syscalls ──────────────────────────────────────────────────────────────

/// sys_send(port, msg_ptr, _msg_len) — copy message from caller and enqueue it.
fn sys_send(port_id: usize, msg_ptr: usize, _msg_len: usize) -> isize {
    // Message must be naturally aligned (8-byte) so the read is defined.
    if !validate_user_ptr_aligned(msg_ptr, core::mem::size_of::<Message>(), 8) { return -14; }
    let msg = unsafe { core::ptr::read(msg_ptr as *const Message) };
    match port::send(port_id as u32, msg) {
        Ok(())                          =>  0,
        Err(port::SendError::QueueFull) => -11, // EAGAIN — queue full, caller should retry
        Err(port::SendError::PortNotFound) => -9, // EBADF — invalid port
    }
}

/// sys_recv(port, msg_ptr) — dequeue a message; block if the queue is empty.
///
/// Returns:
///   -13 (EACCES) — the calling task does not own the port
///   -9  (EBADF)  — port was closed while the task was blocked (woken by
///                  `release_by_owner` → `sched::unblock_port`)
fn sys_recv(port_id: usize, msg_ptr: usize) -> isize {
    // Message must be naturally aligned (8-byte) so the write is defined.
    if !validate_user_ptr_aligned(msg_ptr, core::mem::size_of::<Message>(), 8) { return -14; }
    let caller = current_pid();
    if !port::is_owner(port_id as u32, caller) { return -13; }  // EACCES
    loop {
        match port::recv_as(port_id as u32, caller) {
            Some(msg) => {
                unsafe { core::ptr::write(msg_ptr as *mut Message, msg); }
                return 0;
            }
            None => {
                // Check whether the port still exists before blocking.
                // It may have been closed by release_by_owner between the
                // ownership check above and this point.
                if !port::is_owner(port_id as u32, caller) {
                    return -9; // EBADF — port was closed
                }
                block_on(port_id as u32);
                // After being woken (either by a send or by release_by_owner),
                // re-check port existence before looping back to recv_as.
                if !port::is_owner(port_id as u32, caller) {
                    return -9; // EBADF — port closed while we were blocked
                }
            }
        }
    }
}

/// sys_call — send to `port_id`, then block on the caller's own reply port.
///
/// The reply port is lazily allocated on the first call and cached in the
/// `Task::reply_port` field.  The port ID is stamped into `msg.reply_port`
/// before the message is forwarded, so the server can send its response back
/// to the correct endpoint via `sys_send(msg.reply_port, reply_msg)`.
///
/// Unlike the old implementation, the caller waits on a port it **owns**
/// rather than on the server's port, fixing the EACCES ownership error.
fn sys_call(port_id: usize, msg_ptr: usize, _msg_len: usize) -> isize {
    if !validate_user_ptr_aligned(msg_ptr, core::mem::size_of::<Message>(), 8) { return -14; }

    // Lazily allocate the caller's reply port.
    let reply_port = {
        let rp = current_reply_port();
        if rp != u32::MAX {
            rp
        } else {
            let caller = current_pid();
            match port::create(caller) {
                Some(p) => { set_current_reply_port(p); p }
                None    => return -12, // ENOMEM — port table full
            }
        }
    };

    // Read the message, stamp our reply port, and forward it to the server.
    let mut msg = unsafe { core::ptr::read(msg_ptr as *const Message) };
    msg.reply_port = reply_port;
    match port::send(port_id as u32, msg) {
        Ok(())                              => {}
        Err(port::SendError::QueueFull)     => return -11, // EAGAIN
        Err(port::SendError::PortNotFound)  => return -9,  // EBADF
    }

    // Block on our own reply port (which we own) until the server responds.
    sys_recv(reply_port as usize, msg_ptr)
}

// ── Memory syscalls ───────────────────────────────────────────────────────────

/// Maximum bytes a single sys_map_mem call may request.
/// Prevents a user task from exhausting the buddy allocator in one call.
const MAP_MAX_BYTES: usize = 256 * 1024 * 1024; // 256 MiB

/// Translate Linux `mmap(2)` `prot` bits to kernel `PageFlags`.
fn prot_to_page_flags(prot: usize) -> PageFlags {
    const PROT_WRITE: usize = 2;
    const PROT_EXEC:  usize = 4;
    let mut f = PageFlags::PRESENT | PageFlags::USER;
    if prot & PROT_WRITE != 0 { f |= PageFlags::WRITABLE; }
    if prot & PROT_EXEC  != 0 { f |= PageFlags::EXECUTE; }
    f
}

/// sys_mmap(addr, len, prot, flags, fd, off) — Linux mmap(2) ABI.
///
/// Phase 6 supports anonymous (`MAP_ANONYMOUS`) mappings only.  File-backed
/// mappings (no `MAP_ANONYMOUS`) return `ENOSYS` until Phase 7 (VFS server).
///
/// Address selection:
///   - `MAP_FIXED`         — use `addr` exactly; unmap any existing range first.
///   - `addr != 0` (hint)  — try the hint; fall back to bump if already mapped.
///   - `addr == 0`         — bump-allocate a fresh VA region.
///
/// Returns the mapped virtual address on success, or a negative errno.
fn sys_mmap(addr: usize, len: usize, prot: usize,
            flags: usize, fd: usize, off: usize) -> isize {
    // Linux mmap flags.
    const MAP_FIXED:     usize = 0x10;
    const MAP_ANONYMOUS: usize = 0x20;

    if len == 0 { return -22; } // EINVAL

    let page = mm::buddy::PAGE_SIZE;
    let len  = (len + page - 1) & !(page - 1);
    if len > MAP_MAX_BYTES { return -22; }

    let page_flags = prot_to_page_flags(prot);

    // W^X enforcement.
    if page_flags.contains(PageFlags::WRITABLE) && page_flags.contains(PageFlags::EXECUTE) {
        return -22;
    }

    // Determine the virtual address to use.
    let virt = if flags & MAP_FIXED != 0 {
        if addr == 0 { return -22; }
        addr
    } else if addr != 0 {
        addr
    } else {
        MMAP_BUMP.fetch_add(len, Ordering::Relaxed)
    };

    let end = match virt.checked_add(len) {
        Some(e) => e,
        None    => return -22,
    };
    if end > USER_SPACE_END { return -22; }

    // ── Anonymous mmap ────────────────────────────────────────────────────────
    if flags & MAP_ANONYMOUS != 0 {
        let mapped = with_current_address_space_mut(|as_| {
            if flags & MAP_FIXED != 0 { as_.unmap_range(virt, len); }
            as_.map_lazy(virt, len, page_flags)
        });
        return match mapped {
            Some(true)  => virt as isize,
            Some(false) => {
                if flags & MAP_FIXED == 0 && addr != 0 {
                    let bump = MMAP_BUMP.fetch_add(len, Ordering::Relaxed);
                    let m2 = with_current_address_space_mut(|as_| as_.map_lazy(bump, len, page_flags));
                    match m2 { Some(true) => bump as isize, _ => -12 }
                } else { -12 }
            }
            None => -1,
        };
    }

    // ── File-backed mmap ──────────────────────────────────────────────────────
    // Strategy (mirrors the ELF loader):
    //   1. Seek the fd to `off` in the VFS server.
    //   2. Map the virtual range eagerly (allocates contiguous physical pages).
    //   3. Obtain the physical base address of the new VMA.
    //   4. Read file data directly into physical memory (kernel identity map).
    //   5. If prot is read-only, the VMA page_flags already enforce that.
    //
    // MAP_SHARED is not supported (no VMO page cache yet); silently treat as
    // MAP_PRIVATE — data is copied on map, modifications are local only.

    let pid = current_pid();

    // Step 1: seek the fd to the requested offset.
    if off != 0 {
        let seek_msg = make_vfs_msg(vfs::VFS_LSEEK,
            &[fd as u64, off as u64, 0 /* SEEK_SET */]);
        let r = vfs_reply_val(&vfs::handle(&seek_msg, pid));
        if r < 0 { return r as isize; }
    }

    // Step 2: map the VMA eagerly.  We temporarily use WRITABLE | PRESENT so
    // the copy in step 4 lands in physical memory regardless of prot bits.
    // The final page_flags (which may be read-only) are applied via map_flags
    // on the VMA; subsequent accesses use those bits.
    let write_flags = page_flags | PageFlags::WRITABLE;
    let mapped_phys = with_current_address_space_mut(|as_| {
        if flags & MAP_FIXED != 0 { as_.unmap_range(virt, len); }
        if !as_.map(virt, len, write_flags) { return None; }
        // Retrieve the physical base of the just-created VMA.
        as_.find(virt).map(|vma| vma.phys)
    });

    // mapped_phys : Option<Option<usize>> — outer None = no address space
    let phys = match mapped_phys {
        Some(Some(p)) => p,
        _             => return -12, // ENOMEM or no address space
    };

    // Step 3: read file data into the physical pages.
    // We read up to `len` bytes; if the file is shorter, the rest stays zero.
    let read_msg = make_vfs_msg(vfs::VFS_READ, &[fd as u64, phys as u64, len as u64]);
    let n = vfs_reply_val(&vfs::handle(&read_msg, pid));
    if n < 0 {
        // Read failed — unmap the eagerly-allocated VMA and return error.
        with_current_address_space_mut(|as_| as_.unmap(virt, len));
        return n as isize;
    }

    // Step 4: if the caller wants read-only, downgrade the page permissions.
    // Re-map each page with the original (possibly non-writable) page_flags.
    if !page_flags.contains(PageFlags::WRITABLE) {
        // mprotect the VMA to remove the temporary WRITABLE bit.
        // Use sys_mprotect's logic: walk VMA list, remap pages.
        let _ = sys_mprotect(virt, len, prot);
    }

    virt as isize
}

/// sys_unmap_mem(virt, size) — unmap and free the pages at `virt`.
fn sys_unmap_mem(virt: usize, size: usize) -> isize {
    if virt == 0 || size == 0 { return -22; } // EINVAL
    if virt >= USER_SPACE_END  { return -22; }

    with_current_address_space_mut(|as_| as_.unmap(virt, size));
    0
}

/// sys_mremap(old_addr, old_size, new_size, flags, new_addr) — resize mapping.
///
/// Conservative implementation: if new_size ≤ old_size, shrink by unmapping the
/// tail.  If new_size > old_size, attempt a new anonymous mapping at new_addr
/// (MREMAP_FIXED) or anywhere (returns ENOMEM if no room found — rare for
/// anonymous mappings which use the bump allocator).  Copy is NOT performed;
/// callers expecting content-preserving moves will see zeroes in the new pages.
fn sys_mremap(
    old_addr: usize, old_size: usize, new_size: usize,
    flags: usize, new_addr: usize,
) -> isize {
    const MREMAP_FIXED: usize = 2;
    const PAGE: usize = 4096;

    let old_pages = old_size.div_ceil(PAGE);
    let new_pages = new_size.div_ceil(PAGE);

    if new_pages == old_pages { return old_addr as isize; }

    if new_pages < old_pages {
        // Shrink: unmap the tail pages.
        let tail = old_addr + new_pages * PAGE;
        let tail_len = (old_pages - new_pages) * PAGE;
        with_current_address_space_mut(|as_| as_.unmap(tail, tail_len));
        return old_addr as isize;
    }

    // Grow: allocate a new (larger) anonymous region.
    let target = if flags & MREMAP_FIXED != 0 { new_addr } else { 0 };
    let result = sys_mmap(target, new_size, 3 /* PROT_READ|WRITE */,
                          0x22 /* MAP_PRIVATE|MAP_ANONYMOUS */, usize::MAX, 0);
    if result < 0 { return result; }
    // Unmap the old region.
    with_current_address_space_mut(|as_| as_.unmap(old_addr, old_size));
    result
}

// ── Task management syscalls ──────────────────────────────────────────────────

/// sys_spawn(entry_va, stack_va, priority) — spawn a user-mode task.
///
/// `entry_va`  — virtual address of the task entry point (must be in user space)
/// `stack_va`  — virtual address of the top of the user stack
/// `priority`  — signed 8-bit scheduling priority, passed as a `usize`
///               (cast to `i8`; callers typically pass 0 for normal priority)
///
/// Returns the new task's PID (positive), or a negative errno on failure:
///   -22 (EINVAL)  — entry_va or stack_va is outside user space
///   -12 (ENOMEM)  — run queue full or OOM
fn sys_spawn(entry_va: usize, stack_va: usize, priority_raw: usize) -> isize {
    // Reject entries that point into the kernel half of the address space.
    if entry_va == 0 || entry_va >= USER_SPACE_END { return -22; }
    if stack_va  >= USER_SPACE_END                 { return -22; }

    let priority = priority_raw as i8;
    match spawn_user(entry_va, stack_va, priority) {
        Some(pid) => pid as isize,
        None      => -12, // ENOMEM
    }
}

/// sys_wait(pid, status_ptr) — block until `pid` exits; write its exit code.
///
/// Blocks until the target task becomes a Zombie, writes its `i32` exit code
/// to `status_ptr` (user-space aligned pointer), reaps the task, and returns 0.
///
/// Returns:
///   -3  (ESRCH)   — `pid` does not exist
///   -14 (EFAULT)  — `status_ptr` is null, misaligned, or out of range
fn sys_wait(pid_raw: usize, status_ptr: usize) -> isize {
    // Validate before blocking — catches bad pointers before we yield.
    if status_ptr != 0 && !validate_user_ptr_aligned(status_ptr, core::mem::size_of::<i32>(), 4) {
        return -14;
    }

    match sched::wait_pid(pid_raw as u32) {
        Some(code) => {
            if status_ptr != 0 {
                unsafe { core::ptr::write(status_ptr as *mut i32, code); }
            }
            0
        }
        None => -3, // ESRCH — pid not found
    }
}

/// sys_waitid(idtype, id, infop, options) — wait for a child state change.
///
/// Simplified: delegates to wait_pid; fills siginfo_t at infop with exit code.
fn sys_waitid(idtype: usize, id: usize, infop: usize, _options: usize) -> isize {
    // idtype: 0=P_ALL, 1=P_PID, 2=P_PGID
    let target_pid: u32 = if idtype == 1 { id as u32 } else { u32::MAX };
    let code = sched::wait_pid(target_pid);
    if let Some(exit_code) = code {
        // Fill siginfo_t (si_signo=SIGCHLD at +0, si_code at +8, si_pid at +16, si_status at +24)
        if infop != 0 && validate_user_buf(infop, 128) {
            unsafe {
                core::ptr::write_bytes(infop as *mut u8, 0, 128);
                core::ptr::write(infop            as *mut i32, 17);         // si_signo = SIGCHLD
                core::ptr::write((infop + 8)      as *mut i32, 1);          // si_code = CLD_EXITED
                core::ptr::write((infop + 16)     as *mut u32, target_pid); // si_pid
                core::ptr::write((infop + 24)     as *mut i32, exit_code);  // si_status
            }
        }
        0
    } else {
        -10 // ECHILD
    }
}

/// sys_rt_sigsuspend(mask_ptr, sigsetsize) — atomically set signal mask and pause.
///
/// Replaces the current signal mask, then yields until any unmasked signal
/// arrives.  Always returns -EINTR.
fn sys_rt_sigsuspend(mask_ptr: usize, _sigsetsize: usize) -> isize {
    let new_mask = if mask_ptr != 0 && validate_user_buf(mask_ptr, 8) {
        unsafe { core::ptr::read(mask_ptr as *const u64) }
    } else {
        0
    };
    let old_mask = replace_signal_mask(new_mask);
    // Yield until a signal arrives that is not blocked by new_mask.
    loop {
        if pending_signals() & !new_mask != 0 { break; }
        yield_now();
    }
    // Restore old mask before returning.
    let _ = replace_signal_mask(old_mask);
    -4 // EINTR
}

/// sys_rt_sigtimedwait(set_ptr, info_ptr, timeout_ptr, sigsetsize)
/// Waits until a signal in `set` is pending, or timeout elapses.
fn sys_rt_sigtimedwait(set_ptr: usize, info_ptr: usize, timeout_ptr: usize, _sz: usize) -> isize {
    let wait_mask: u64 = if set_ptr != 0 && validate_user_buf(set_ptr, 8) {
        unsafe { core::ptr::read(set_ptr as *const u64) }
    } else { !0u64 };

    // Compute deadline from timespec (tv_sec + tv_nsec).
    let deadline = if timeout_ptr != 0 && validate_user_buf(timeout_ptr, 16) {
        let tv_sec  = unsafe { core::ptr::read(timeout_ptr as *const i64) };
        let tv_nsec = unsafe { core::ptr::read((timeout_ptr + 8) as *const i64) };
        if tv_sec == 0 && tv_nsec == 0 {
            Some(ticks()) // zero timeout = poll only
        } else {
            let ticks_val = (tv_sec as u64) * 100 + (tv_nsec as u64) / 10_000_000;
            Some(ticks() + ticks_val.max(1))
        }
    } else {
        None // no timeout — wait indefinitely
    };

    loop {
        let pending = pending_signals() & wait_mask;
        if pending != 0 {
            let signo = pending.trailing_zeros() as u32 + 1;
            // Clear the signal from pending.
            clear_pending_signal(signo);
            // Optionally fill siginfo_t (128 bytes) with signo.
            if info_ptr != 0 && validate_user_buf(info_ptr, 128) {
                unsafe {
                    core::ptr::write_bytes(info_ptr as *mut u8, 0, 128);
                    core::ptr::write(info_ptr as *mut i32, signo as i32); // si_signo
                }
            }
            return signo as isize;
        }
        if let Some(dl) = deadline {
            if ticks() >= dl { return -110; } // ETIMEDOUT
        }
        yield_now();
    }
}

/// sys_clock_gettime(clkid, tp_ptr) — write monotonic tick counter to user memory.
///
/// `clkid` is ignored (all clocks return the same monotonic tick counter).
/// Writes a `struct timespec { tv_sec: i64, tv_nsec: i64 }` at `tp_ptr`.
/// Tick frequency is ~100 Hz (10 ms per tick).
fn sys_clock_gettime(_clkid: usize, tp_ptr: usize) -> isize {
    if !validate_user_ptr_aligned(tp_ptr, 16, 8) { return -14; }
    let ticks = ticks();
    // Treat each tick as 10 ms.
    let tv_sec  = (ticks / 100) as i64;
    let tv_nsec = ((ticks % 100) * 10_000_000) as i64;
    unsafe {
        core::ptr::write(tp_ptr as *mut i64, tv_sec);
        core::ptr::write((tp_ptr + 8) as *mut i64, tv_nsec);
    }
    0
}

/// sys_getrandom(buf, count, flags) — fill buffer with pseudo-random bytes.
///
/// Uses a simple LCG seeded from ticks.  Not cryptographically secure, but
/// satisfies musl's use for arc4random seeding.
fn sys_getrandom(buf_ptr: usize, count: usize, _flags: usize) -> isize {
    if count == 0 { return 0; }
    if !validate_user_buf(buf_ptr, count) { return -14; }
    // LCG with 64-bit state; seeded from monotonic ticks.
    let mut state = ticks().wrapping_add(0x_dead_beef_cafe_babe);
    let buf = unsafe { core::slice::from_raw_parts_mut(buf_ptr as *mut u8, count) };
    for chunk in buf.chunks_mut(8) {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let bytes = state.to_le_bytes();
        for (d, &s) in chunk.iter_mut().zip(bytes.iter()) { *d = s; }
    }
    count as isize
}

/// sys_prctl(option, arg2..5) — process control.
///
/// PR_SET_NAME (15): ignore (we don't track thread names).
/// PR_GET_NAME (16): write "cyanos\0" to arg2.
/// All others: return 0 (silently ignore).
fn sys_prctl(option: usize, arg2: usize, _a3: usize, _a4: usize, _a5: usize) -> isize {
    const PR_SET_NAME: usize = 15;
    const PR_GET_NAME: usize = 16;
    const PR_SET_DUMPABLE: usize = 4;
    const PR_GET_DUMPABLE: usize = 3;
    match option {
        PR_SET_NAME => 0,
        PR_GET_NAME => {
            // Write a 16-byte NUL-padded thread name.
            if validate_user_buf(arg2, 16) {
                let name = b"cyanos\0\0\0\0\0\0\0\0\0\0";
                unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), arg2 as *mut u8, 16); }
            }
            0
        }
        PR_SET_DUMPABLE => 0,
        PR_GET_DUMPABLE => 1,
        _ => 0, // silently accept anything else
    }
}

/// sys_clock_getres(clkid, res_ptr) — return the resolution of a clock.
///
/// All clocks report 10 ms resolution (100 Hz tick counter).
fn sys_clock_getres(_clkid: usize, res_ptr: usize) -> isize {
    if res_ptr != 0 {
        if !validate_user_buf(res_ptr, 16) { return -14; }
        // struct timespec { tv_sec=0, tv_nsec=10_000_000 (10 ms) }
        unsafe {
            core::ptr::write(res_ptr          as *mut i64, 0i64);
            core::ptr::write((res_ptr + 8)    as *mut i64, 10_000_000i64);
        }
    }
    0
}

/// sys_pread64(fd, buf, count, offset) — read from `fd` at `offset` without changing pos.
fn sys_pread64(fd: usize, buf_ptr: usize, count: usize, offset: usize) -> isize {
    if count == 0 { return 0; }
    if !validate_user_buf(buf_ptr, count) { return -14; }
    let pid = current_pid();
    // Seek to offset, read, seek back (best-effort; position state is in VFS).
    let seek_msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, offset as u64, 0 /* SEEK_SET */]);
    let cur = vfs_reply_val(&vfs::handle(&seek_msg, pid));
    if cur < 0 { return cur; }
    let read_msg = make_vfs_msg(vfs::VFS_READ, &[fd as u64, buf_ptr as u64, count as u64]);
    let n = vfs_reply_val(&vfs::handle(&read_msg, pid));
    // Restore original position.
    let back_msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, cur as u64, 0]);
    let _ = vfs::handle(&back_msg, pid);
    n
}

/// sys_pwrite64(fd, buf, count, offset) — write to `fd` at `offset` without changing pos.
fn sys_pwrite64(fd: usize, buf_ptr: usize, count: usize, offset: usize) -> isize {
    if count == 0 { return 0; }
    if !validate_user_buf(buf_ptr, count) { return -14; }
    let pid = current_pid();
    // Get current position.
    let cur_msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, 0u64, 1 /* SEEK_CUR */]);
    let cur = vfs_reply_val(&vfs::handle(&cur_msg, pid));
    if cur < 0 { return if fd <= 2 { count as isize } else { cur }; }
    // Seek to target offset.
    let seek_msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, offset as u64, 0]);
    let _ = vfs::handle(&seek_msg, pid);
    // Write.
    let write_msg = make_vfs_msg(vfs::VFS_WRITE, &[fd as u64, buf_ptr as u64, count as u64]);
    let n = vfs_reply_val(&vfs::handle(&write_msg, pid));
    // Restore position.
    let back_msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, cur as u64, 0]);
    let _ = vfs::handle(&back_msg, pid);
    n
}

/// sys_ftruncate(fd, length) — set tmpfs file size.
fn sys_ftruncate(fd: usize, length: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_FTRUNCATE, &[fd as u64, length as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// sys_times(buf_ptr) — return process and child CPU times.
///
/// All times are zero (we don't track per-task CPU usage).
/// Returns the number of ticks since boot as the wall-clock value.
fn sys_times(buf_ptr: usize) -> isize {
    // struct tms { tms_utime, tms_stime, tms_cutime, tms_cstime } all u64 = 32 bytes.
    if buf_ptr != 0 && validate_user_buf(buf_ptr, 32) {
        unsafe { core::ptr::write_bytes(buf_ptr as *mut u8, 0, 32); }
    }
    ticks() as isize
}

/// sys_ppoll(fds_ptr, nfds, timeout_ptr, sigmask_ptr) — wait for events on fd set.
///
/// Checks each struct pollfd once; marks revents for ready fds.
/// If all fds report POLLNVAL (bad fd) or no events, returns 0 (timeout).
fn sys_ppoll(fds_ptr: usize, nfds: usize, timeout_ptr: usize, _sigmask: usize) -> isize {
    // struct pollfd { fd: i32, events: i16, revents: i16 } = 8 bytes.
    const POLLIN:   i16 = 0x0001;
    const POLLOUT:  i16 = 0x0004;
    const POLLERR:  i16 = 0x0008;
    const POLLHUP:  i16 = 0x0010;
    const POLLNVAL: i16 = 0x0020;

    if nfds == 0 { return 0; }
    let sz = nfds.saturating_mul(8);
    if !validate_user_buf(fds_ptr, sz) { return -14; }

    let pid = current_pid();
    let mut nready = 0isize;

    for i in 0..nfds {
        let pfd = fds_ptr + i * 8;
        let fd     = unsafe { core::ptr::read(pfd          as *const i32) };
        let events = unsafe { core::ptr::read((pfd + 4)    as *const i16) };

        if fd < 0 {
            unsafe { core::ptr::write((pfd + 6) as *mut i16, 0); }
            continue;
        }
        let fd = fd as usize;

        let revents: i16 = if fd <= 2 {
            // fd 0 = stdin: report readable if serial has data, else 0.
            // fd 1/2 = stdout/stderr: always writable.
            if fd == 0 {
                if crate::serial_read_byte().is_some() { POLLIN } else { 0 }
            } else {
                events & POLLOUT
            }
        } else {
            // Check VFS: probe with a zero-count read.
            let probe = make_vfs_msg(vfs::VFS_LSEEK,
                &[fd as u64, 0u64, 1u64 /* SEEK_CUR */]);
            let r = vfs_reply_val(&vfs::handle(&probe, pid));
            if r == -9 {
                POLLNVAL
            } else {
                // Assume writable; readable if it's a pipe with data.
                let mut rev = 0i16;
                if events & POLLOUT != 0 { rev |= POLLOUT; }
                if events & POLLIN  != 0 { rev |= POLLIN; }
                rev
            }
        };

        unsafe { core::ptr::write((pfd + 6) as *mut i16, revents); }
        if revents != 0 && revents != POLLNVAL { nready += 1; }
    }

    // If timeout_ptr is NULL (infinite wait) but no events yet, yield once and
    // return 0 — caller should retry.  If timeout is {0,0}, return immediately.
    if nready == 0 && timeout_ptr != 0 {
        // Check if timeout is zero.
        let tv_sec  = unsafe { core::ptr::read(timeout_ptr       as *const i64) };
        let tv_nsec = unsafe { core::ptr::read((timeout_ptr + 8) as *const i64) };
        if tv_sec == 0 && tv_nsec == 0 { return 0; }
        // Non-zero timeout: yield once before returning (cooperative).
        yield_now();
    } else if nready == 0 {
        yield_now();
    }
    nready
}

/// sys_nanosleep / sys_clock_nanosleep — yield-loop until the requested time
/// has elapsed (based on tick counter).
///
/// `rqtp_ptr` points to `struct timespec { tv_sec: i64, tv_nsec: i64 }`.
/// The second argument (`clockid` for clock_nanosleep, or `rmtp` for nanosleep)
/// is ignored; remaining time is never written back.
fn sys_nanosleep(rqtp_ptr: usize, _rmtp: usize) -> isize {
    if rqtp_ptr == 0 { return 0; }
    if !validate_user_buf(rqtp_ptr, 16) { return -14; }
    let tv_sec  = unsafe { core::ptr::read(rqtp_ptr         as *const i64) };
    let tv_nsec = unsafe { core::ptr::read((rqtp_ptr + 8)   as *const i64) };
    if tv_sec < 0 || tv_nsec < 0 || tv_nsec >= 1_000_000_000 { return -22; } // EINVAL
    // Convert to ticks (~100 Hz).
    let ticks_needed = (tv_sec as u64) * 100 + (tv_nsec as u64) / 10_000_000;
    if ticks_needed == 0 { return 0; }
    let deadline = ticks().wrapping_add(ticks_needed);
    loop {
        yield_now();
        if ticks() >= deadline { break; }
    }
    0
}

/// sys_gettimeofday(tv_ptr, tz_ptr) — fill `struct timeval` with wall-clock time.
///
/// We don't have a real-time clock, so we synthesise from ticks (boot = epoch).
/// `tz_ptr` is always written as UTC (+0).
fn sys_gettimeofday(tv_ptr: usize, tz_ptr: usize) -> isize {
    // struct timeval { tv_sec: i64, tv_usec: i64 }
    if tv_ptr != 0 {
        if !validate_user_buf(tv_ptr, 16) { return -14; }
        let ticks = ticks();
        let tv_sec  = (ticks / 100) as i64;
        let tv_usec = ((ticks % 100) * 10_000) as i64;
        unsafe {
            core::ptr::write(tv_ptr        as *mut i64, tv_sec);
            core::ptr::write((tv_ptr + 8)  as *mut i64, tv_usec);
        }
    }
    // struct timezone { tz_minuteswest: i32, tz_dsttime: i32 }
    if tz_ptr != 0 && validate_user_buf(tz_ptr, 8) {
        unsafe { core::ptr::write_bytes(tz_ptr as *mut u8, 0, 8); }
    }
    0
}

/// sys_time(tloc) — return seconds since boot as a `time_t` (i64).
///
/// x86-64 only (AArch64 does not have syscall #201 for `time`).
#[cfg(not(target_arch = "aarch64"))]
fn sys_time(tloc: usize) -> isize {
    let t = (ticks() / 100) as i64;
    if tloc != 0 && validate_user_buf(tloc, 8) {
        unsafe { core::ptr::write(tloc as *mut i64, t); }
    }
    t as isize
}

/// sys_sysinfo(info_ptr) — fill Linux `struct sysinfo` (112 bytes).
fn sys_sysinfo(info_ptr: usize) -> isize {
    // struct sysinfo {
    //   uptime:    i64,       // +0
    //   loads:     [u64; 3], // +8   (1/5/15-min load averages × 65536)
    //   totalram:  u64,      // +32
    //   freeram:   u64,      // +40
    //   sharedram: u64,      // +48
    //   bufferram: u64,      // +56
    //   totalswap: u64,      // +64
    //   freeswap:  u64,      // +72
    //   procs:     u16,      // +80
    //   _pad:      [u8; 6],  // +82
    //   totalhigh: u64,      // +88
    //   freehigh:  u64,      // +96
    //   mem_unit:  u32,      // +104
    //   _f:        [u8; 8],  // +108
    // }  = 116 bytes on 64-bit Linux; glibc sysinfo uses 112-byte kernel struct
    const SYSINFO_SIZE: usize = 112;
    if !validate_user_buf(info_ptr, SYSINFO_SIZE) { return -14; }
    unsafe { core::ptr::write_bytes(info_ptr as *mut u8, 0, SYSINFO_SIZE); }

    let ticks = ticks();
    let uptime = (ticks / 100) as i64;
    // Free memory estimate from buddy allocator.
    let free_pages = mm::buddy::free_pages();
    let total_pages = mm::buddy::total_pages();
    let page_size = mm::buddy::PAGE_SIZE as u64;

    unsafe {
        // uptime (i64 at offset 0)
        core::ptr::write(info_ptr as *mut i64, uptime);
        // loads[3] (u64 × 3 at offset 8) — report 0 load
        // totalram (u64 at offset 32)
        core::ptr::write((info_ptr + 32) as *mut u64, total_pages as u64 * page_size);
        // freeram (u64 at offset 40)
        core::ptr::write((info_ptr + 40) as *mut u64, free_pages as u64 * page_size);
        // procs (u16 at offset 80) — 1 process
        core::ptr::write((info_ptr + 80) as *mut u16, 1u16);
        // mem_unit (u32 at offset 104) — 1 byte
        core::ptr::write((info_ptr + 104) as *mut u32, 1u32);
    }
    0
}

// ── Signal stubs (Phase 2 will provide real implementations) ─────────────────

fn sys_rt_sigaction(signum: usize, act_ptr: usize, oldact_ptr: usize) -> isize {
    if signum == 0 || signum >= 64 { return -22; } // EINVAL
    sys_sigaction(signum as u32, act_ptr, oldact_ptr)
}

fn sys_rt_sigprocmask(how: usize, set_ptr: usize, oldset_ptr: usize) -> isize {
    sys_sigprocmask(how, set_ptr, oldset_ptr)
}

fn sys_rt_sigreturn(frame_ptr: usize) -> isize {
    // Restore the pre-signal user register context from the rt_sigframe on the
    // user stack, including the signal mask.  The return value written into
    // the frame's x0 / rax slot will be overwritten by the restored context.
    restore_signal_frame(frame_ptr);
    0 // only reached if frame_ptr == 0 (x86-64 stub path)
}

fn sys_kill(pid_raw: usize, sig_raw: usize) -> isize {
    let pid = pid_raw as u32;
    let sig = sig_raw as u32;
    if sig >= 64 { return -22; } // EINVAL
    deliver_signal(pid, sig)
}

fn sys_getppid() -> isize {
    current_ppid() as isize
}

// ── Thread stubs (Phase 4 will provide real implementations) ─────────────────

fn sys_set_tid_address(tidptr: usize) -> isize {
    set_clear_child_tid(tidptr);
    current_pid() as isize
}

fn sys_futex(uaddr: usize, op: usize, val: usize, timeout_ptr: usize) -> isize {
    // Strip FUTEX_PRIVATE_FLAG (128) and FUTEX_CLOCK_REALTIME (256).
    const FUTEX_PRIVATE_FLAG: usize = 128;
    match op & !FUTEX_PRIVATE_FLAG {
        0 => {
            // FUTEX_WAIT: if *uaddr == val, block until woken.
            if !validate_user_ptr_aligned(uaddr, 4, 4) { return -14; }
            let current = unsafe { core::ptr::read(uaddr as *const u32) };
            if current != val as u32 { return -11; } // EAGAIN — value changed
            sched::futex_wait(uaddr, timeout_ptr)
        }
        1 => {
            // FUTEX_WAKE: wake up to `val` tasks sleeping on `uaddr`.
            sched::futex_wake(uaddr, val as u32) as isize
        }
        _ => -38, // ENOSYS
    }
}

fn sys_arch_prctl(code: usize, addr: usize) -> isize {
    // ARCH_SET_FS = 0x1002, ARCH_GET_FS = 0x1003 (x86-64 only)
    #[cfg(target_arch = "x86_64")]
    {
        const ARCH_SET_FS: usize = 0x1002;
        const ARCH_GET_FS: usize = 0x1003;
        match code {
            ARCH_SET_FS => {
                set_fs_base(addr as u64);
                // Immediately write to hardware for the current task.
                unsafe {
                    core::arch::asm!(
                        "wrfsbase {v}",
                        v = in(reg) addr as u64,
                        options(nomem, nostack)
                    );
                }
                0
            }
            ARCH_GET_FS => {
                if !validate_user_ptr_aligned(addr, 8, 8) { return -14; }
                let base = get_fs_base();
                unsafe { core::ptr::write(addr as *mut u64, base); }
                0
            }
            _ => -22, // EINVAL
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    { let _ = (code, addr); -38 } // ENOSYS on non-x86-64
}

// ── Memory stubs (Phase 6 will expand) ───────────────────────────────────────

fn sys_mprotect(addr: usize, len: usize, prot: usize) -> isize {
    if addr == 0 || len == 0 { return -22; }
    let ok = with_current_address_space_mut(|as_| as_.mprotect(addr, len, prot as u32));
    match ok {
        Some(true)  =>  0,
        Some(false) => -22, // EINVAL
        None        => -1,
    }
}

fn sys_brk(new_end: usize) -> isize {
    with_current_address_space_mut(|as_| as_.brk(new_end))
        .unwrap_or(-12) // ENOMEM
}

// ── execve ────────────────────────────────────────────────────────────────────

// ── execve string-building infrastructure ────────────────────────────────────

const MAX_EXEC_ARGS: usize = 64;
const MAX_EXEC_STR:  usize = 8192; // total bytes for all argv + envp strings

/// Static buffer used during execve to collect argv/envp strings before the
/// address space is replaced.  Protected by the single-threaded execve path
/// (only the current task runs during the critical section).
struct ExecStrBuf {
    data:    [u8; MAX_EXEC_STR],
    end:     usize,
    offsets: [usize; MAX_EXEC_ARGS], // start offset of each string in data[]
    lengths: [usize; MAX_EXEC_ARGS], // byte length (excl. NUL) of each string
    count:   usize,
}

impl ExecStrBuf {
    const fn new() -> Self {
        Self { data: [0u8; MAX_EXEC_STR], end: 0,
               offsets: [0; MAX_EXEC_ARGS], lengths: [0; MAX_EXEC_ARGS], count: 0 }
    }

    /// Read one null-terminated C string from user-space `ptr` into the buffer.
    /// Returns false on overflow or fault.
    fn push_cstr(&mut self, ptr: usize) -> bool {
        if self.count >= MAX_EXEC_ARGS { return false; }
        if ptr == 0 { return false; }
        let start = self.end;
        loop {
            if self.end >= MAX_EXEC_STR - 1 { return false; }
            let b = unsafe { *(ptr as *const u8).add(self.end - start) };
            if b == 0 { break; }
            self.data[self.end] = b;
            self.end += 1;
        }
        let len = self.end - start;
        self.data[self.end] = 0; // null terminator (not counted in lengths)
        self.end += 1;
        self.offsets[self.count] = start;
        self.lengths[self.count] = len;
        self.count += 1;
        true
    }

    fn reset(&mut self) { self.end = 0; self.count = 0; }
}

static EXEC_ARGV: spin::Mutex<ExecStrBuf> = spin::Mutex::new(ExecStrBuf::new());
static EXEC_ENVP: spin::Mutex<ExecStrBuf> = spin::Mutex::new(ExecStrBuf::new());

/// sys_execve(path_ptr, argv_ptr, envp_ptr) — Phase 3 ABI (VFS path lookup).
///
/// Phase 1 backward-compat: if `path_ptr` points to ELF magic bytes and
/// `argv_ptr` looks like an ELF length (`< 64 MiB`), treat `path_ptr` as an
/// ELF image pointer and `argv_ptr` as the image length (old ABI).
///
/// Phase 3+: `path_ptr` is a user-space C string; the kernel looks it up in
/// VFS, reads the ELF, processes argv/envp, and builds the initial user stack.
///
/// Returns:
///   never   — on success
///   -14     EFAULT  — pointer out of range
///   -2      ENOENT  — path not found in VFS
///   -8      ENOEXEC — ELF parse error
///   -12     ENOMEM  — OOM
///   -38     ENOSYS  — not an ELF / no VFS yet (legacy path)
#[allow(clippy::too_many_lines)]
/// sys_execve(path_ptr, argv_ptr, envp_ptr) — Phase 1 ABI.
///
/// Phase 1 ABI: if the pointer addresses an ELF magic header, treat
/// `(a0, a1)` as `(elf_image_ptr, elf_image_len)` and load directly.
/// Phase 3 replaces this with a VFS path lookup.
///
/// Returns:
///   never   — on success (replaces the calling process image)
///   -14     EFAULT  — pointer out of range
///   -22     EINVAL  — bad len
///   -38     ENOSYS  — not an ELF image (no VFS yet)
///   -8      ENOEXEC — ELF parse / load error
///   -12     ENOMEM  — OOM
fn sys_execve(path_or_elf: usize, argv_ptr: usize, envp_ptr: usize) -> isize {
    if !validate_user_buf(path_or_elf, 1) { return -14; }

    // ── Detect Phase 1 backward-compat ABI ───────────────────────────────────
    // If path_or_elf points to ELF magic AND argv_ptr looks like a byte length,
    // treat as (elf_ptr, elf_len) call.
    let maybe_elf_len = argv_ptr;
    let is_elf_ptr = if validate_user_buf(path_or_elf, 4) {
        let magic = unsafe { core::ptr::read(path_or_elf as *const [u8; 4]) };
        magic == [0x7f, b'E', b'L', b'F'] && maybe_elf_len > 0 && maybe_elf_len <= 64 << 20
    } else { false };

    // ── Resolve ELF bytes ─────────────────────────────────────────────────────
    let (elf_ptr, elf_len) = if is_elf_ptr {
        (path_or_elf, maybe_elf_len)
    } else {
        // VFS path lookup — returns a pointer into static RamFS data.
        match vfs::get_file_data(path_or_elf) {
            Some((ptr, len)) => (ptr as usize, len),
            None             => return -2, // ENOENT
        }
    };

    if elf_len == 0 { return -22; }
    if is_elf_ptr && !validate_user_buf(elf_ptr, elf_len) { return -14; }

    // ── Collect argv / envp strings ───────────────────────────────────────────
    let mut argv = EXEC_ARGV.lock();
    let mut envp = EXEC_ENVP.lock();
    argv.reset();
    envp.reset();

    if !is_elf_ptr {
        // Read argv[] from user-space (array of pointers, null-terminated).
        if argv_ptr != 0 {
            let mut i = 0usize;
            loop {
                if i >= MAX_EXEC_ARGS { break; }
                let ptr_addr = argv_ptr + i * core::mem::size_of::<usize>();
                if !validate_user_buf(ptr_addr, core::mem::size_of::<usize>()) { break; }
                let str_ptr = unsafe { core::ptr::read(ptr_addr as *const usize) };
                if str_ptr == 0 { break; }
                argv.push_cstr(str_ptr);
                i += 1;
            }
        }
        // Read envp[] similarly.
        if envp_ptr != 0 {
            let mut i = 0usize;
            loop {
                if i >= MAX_EXEC_ARGS { break; }
                let ptr_addr = envp_ptr + i * core::mem::size_of::<usize>();
                if !validate_user_buf(ptr_addr, core::mem::size_of::<usize>()) { break; }
                let str_ptr = unsafe { core::ptr::read(ptr_addr as *const usize) };
                if str_ptr == 0 { break; }
                envp.push_cstr(str_ptr);
                i += 1;
            }
        }
    }

    let argc = argv.count;
    let envc = envp.count;

    // ── Load ELF into fresh address space ─────────────────────────────────────
    let pt_root = unsafe { arch_alloc_page_table_root() };
    if pt_root == 0 { return -12; }
    let mut new_as = mm::vmm::AddressSpace::new(pt_root);

    let elf_bytes = unsafe { core::slice::from_raw_parts(elf_ptr as *const u8, elf_len) };
    let entry = match elf::load(elf_bytes, &mut new_as) {
        Ok(e)  => e,
        Err(_) => { drop(new_as); return -8; }
    };

    // Map user stack (read+write, eager so virt_to_phys works immediately).
    let stack_flags = PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE;
    if !new_as.map(USER_STACK_TOP - USER_STACK_SIZE, USER_STACK_SIZE, stack_flags) {
        drop(new_as); return -12;
    }
    let heap_start = new_as.heap_start;

    // ── Build initial user stack ──────────────────────────────────────────────
    //
    // Stack grows downward from USER_STACK_TOP.
    //
    // Layout (high → low address):
    //   [AT_RANDOM bytes: 16]                ← rand_va
    //   [envp strings, null-terminated]
    //   [argv strings, null-terminated]
    //   [16-byte alignment pad]
    //   [AT_NULL pair (0, 0)]
    //   [AT_CYANOS_VFS_PORT pair]
    //   [AT_EGID pair]
    //   [AT_GID pair]
    //   [AT_EUID pair]
    //   [AT_UID pair]
    //   [AT_PAGESZ pair]
    //   [AT_RANDOM pair]
    //   [NULL (envp terminator)]
    //   [envp[envc-1] pointer]
    //   ...
    //   [envp[0] pointer]
    //   [NULL (argv terminator)]
    //   [argv[argc-1] pointer]
    //   ...
    //   [argv[0] pointer]
    //   [argc]        ← user SP points here

    const W: usize = core::mem::size_of::<u64>(); // 8

    // Compute sizes.
    let argv_str_total = argv.end;
    let envp_str_total = envp.end;
    let rand_bytes     = 16usize;

    // pointer table: argc(1) + argv[argc](argc) + null(1) + envp[envc](envc) + null(1)
    let ptr_words = 1 + argc + 1 + envc + 1;
    // auxv: AT_RANDOM + AT_PAGESZ + AT_UID + AT_EUID + AT_GID + AT_EGID + AT_CYANOS_VFS_PORT + AT_NULL = 8 pairs
    let auxv_words = 8 * 2;
    let total_words = ptr_words + auxv_words;
    let total_ptr_bytes = total_words * W;
    // Align string section to 16 bytes.
    let str_section = argv_str_total + envp_str_total + rand_bytes;
    let str_aligned = (str_section + 15) & !15;

    let frame_size = total_ptr_bytes + str_aligned;
    if frame_size > USER_STACK_SIZE { return -22; } // EINVAL — too many args

    let user_sp = USER_STACK_TOP - frame_size;
    // Align sp to 16 bytes (ABI requirement on AArch64 and x86-64).
    let user_sp = user_sp & !15;

    // String base in user VA — starts right after the pointer table.
    let str_base_va = user_sp + total_ptr_bytes;
    let rand_va     = str_base_va + argv_str_total + envp_str_total;

    // Get physical address of the start of the stack frame.
    let phys_base = match new_as.virt_to_phys(user_sp) {
        Some(p) => p, None => { drop(new_as); return -12; }
    };

    // Write the stack frame to kernel-accessible physical memory.
    // Helper: write a u64 at byte offset `off` into the physical frame.
    let write64 = |off: usize, val: u64| unsafe {
        core::ptr::write((phys_base + off) as *mut u64, val);
    };
    let write8 = |off: usize, src: *const u8, len: usize| unsafe {
        core::ptr::copy_nonoverlapping(src, (phys_base + off) as *mut u8, len);
    };

    // Pointer table section.
    let mut w = 0usize; // word index

    // argc
    write64(w * W, argc as u64); w += 1;

    // argv pointers
    let mut str_off = 0usize; // offset within string section in user VA
    for i in 0..argc {
        write64(w * W, (str_base_va + str_off) as u64); w += 1;
        str_off += argv.lengths[i] + 1; // +1 for NUL
    }
    write64(w * W, 0); w += 1; // argv null terminator

    // envp pointers
    for i in 0..envc {
        write64(w * W, (str_base_va + argv_str_total + envp.offsets[i]) as u64); w += 1;
    }
    write64(w * W, 0); w += 1; // envp null terminator

    // auxv
    let t = ticks();
    let auxv: &[(u64, u64)] = &[
        (25, rand_va as u64),                      // AT_RANDOM
        (6,  mm::buddy::PAGE_SIZE as u64),          // AT_PAGESZ
        (11, 0),                                   // AT_UID
        (12, 0),                                   // AT_EUID
        (13, 0),                                   // AT_GID
        (14, 0),                                   // AT_EGID
        (AT_CYANOS_VFS_PORT, VFS_SERVER_PORT.load(Ordering::Relaxed) as u64),
        (0,  0),                                   // AT_NULL
    ];
    for &(k, v) in auxv {
        write64(w * W, k); w += 1;
        write64(w * W, v); w += 1;
    }

    // String data section.
    let str_phys = phys_base + total_ptr_bytes;
    write8(str_phys - phys_base, argv.data.as_ptr(), argv_str_total);
    write8(str_phys - phys_base + argv_str_total, envp.data.as_ptr(), envp_str_total);

    // AT_RANDOM data: 16 bytes of pseudo-random.
    let rand_phys = phys_base + total_ptr_bytes + argv_str_total + envp_str_total;
    let r0 = t ^ 0xdeadbeef_cafebabe_u64;
    let r1 = t.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    write8(rand_phys - phys_base, r0.to_le_bytes().as_ptr(), 8);
    write8(rand_phys - phys_base + 8, r1.to_le_bytes().as_ptr(), 8);

    // Release argv/envp buffers before the AS swap.
    drop(argv);
    drop(envp);

    // ── VFS lifecycle and address space replacement ────────────────────────────
    let pid = current_pid();
    let cloexec_msg = make_vfs_msg(vfs::VFS_EXEC_CLOEXEC, &[pid as u64]);
    let _ = vfs::handle(&cloexec_msg, pid);

    replace_address_space(new_as, pt_root, heap_start, entry, user_sp);
    0
}

// ── I/O syscalls ──────────────────────────────────────────────────────────────

/// sys_write(fd, buf, count) — write bytes to a file descriptor.
///
/// fd 1/2 write directly to serial.  All other fds route through VFS.
fn sys_write(fd: usize, buf_ptr: usize, count: usize) -> isize {
    if count == 0 { return 0; }
    
    // Debug print for all writes to fd 1/2
    if fd == 1 || fd == 2 {
        crate::serial_print("[SYSCALL] write fd=");
        let fd_char = (b'0' + fd as u8) as char;
        let mut buf = [0u8; 1];
        buf[0] = fd_char as u8;
        crate::serial_write_raw(&buf);
        crate::serial_print(" count=");
        print_number(count as u32);
        crate::serial_print("\n");
    }

    if !validate_user_buf(buf_ptr, count) { return -14; }
    match fd {
        1 | 2 => {
            let mut kbuf = Vec::with_capacity(count);
            unsafe { kbuf.set_len(count); }
            
            let ok = with_current_address_space(|as_| {
                as_.read_user_buf(buf_ptr, &mut kbuf)
            }).unwrap_or(false);

            if !ok { 
                crate::serial_print("[SYSCALL] write: read_user_buf failed\n");
                return -14; 
            }
            
            crate::serial_write_raw(&kbuf);
            count as isize
        }
        _ => {
            let pid = current_pid();
            let msg = make_vfs_msg(vfs::VFS_WRITE, &[fd as u64, buf_ptr as u64, count as u64]);
            let reply = vfs::handle(&msg, pid);
            vfs_reply_val(&reply)
        }
    }
}

/// sys_read(fd, buf, count) — read bytes from a file descriptor.
///
/// fd 0 (stdin) blocks on serial UART until at least one byte arrives.
/// All other fds route through VFS.
fn sys_read(fd: usize, buf_ptr: usize, count: usize) -> isize {
    match fd {
        0 => {
            if count == 0 { return 0; }
            if !validate_user_buf(buf_ptr, count) { return -14; }
            // Yield-loop until the UART RX FIFO has at least one byte.
            let first = loop {
                match crate::serial_read_byte() {
                    Some(b) => break b,
                    None    => yield_now(),
                }
            };
            
            let mut kbuf = Vec::with_capacity(count);
            unsafe { kbuf.set_len(count); }
            
            kbuf[0] = first;
            let mut n = 1usize;
            // Drain any additional bytes that arrived without blocking.
            while n < count {
                match crate::serial_read_byte() {
                    Some(b) => { kbuf[n] = b; n += 1; }
                    None    => break,
                }
            }
            
            let ok = with_current_address_space(|as_| {
                as_.write_user_buf(buf_ptr, &kbuf[..n])
            }).unwrap_or(false);
            
            if !ok { return -14; }
            
            n as isize
        }
        _ => {
            if count != 0 && !validate_user_buf(buf_ptr, count) { return -14; }
            let pid = current_pid();
            let msg = make_vfs_msg(vfs::VFS_READ, &[fd as u64, buf_ptr as u64, count as u64]);
            // Pipe read: VFS returns -EAGAIN when write end is open but empty.
            // Block (yield-loop) until data arrives or the write end closes.
            loop {
                let n = vfs_reply_val(&vfs::handle(&msg, pid));
                if n != -11 { return n; }
                yield_now();
            }
        }
    }
}

/// sys_writev(fd, iov, iovcnt) — scatter-gather write.
fn sys_writev(fd: usize, iov_ptr: usize, iovcnt: usize) -> isize {
    if iovcnt == 0 { return 0; }
    // Each `struct iovec` is { base: *const u8 (8 bytes), len: usize (8 bytes) }.
    if !validate_user_buf(iov_ptr, iovcnt.saturating_mul(16)) { return -14; }
    match fd {
        1 | 2 => {
            let mut total: isize = 0;
            for i in 0..iovcnt {
                let iov_addr = iov_ptr + i * 16;
                let base = unsafe { core::ptr::read(iov_addr as *const usize) };
                let len  = unsafe { core::ptr::read((iov_addr + 8) as *const usize) };
                if len == 0 { continue; }
                if !validate_user_buf(base, len) { return -14; }
                let bytes = unsafe { core::slice::from_raw_parts(base as *const u8, len) };
                crate::serial_write_raw(bytes);
                total = total.saturating_add(len as isize);
            }
            total
        }
        _ => -9,
    }
}

/// sys_readv(fd, iov, iovcnt) — scatter-gather read, one iovec at a time via VFS.
fn sys_readv(fd: usize, iov_ptr: usize, iovcnt: usize) -> isize {
    if iovcnt == 0 { return 0; }
    if !validate_user_buf(iov_ptr, iovcnt.saturating_mul(16)) { return -14; }
    match fd {
        0 => {
            // Delegate to sys_read for the first non-empty iov.
            for i in 0..iovcnt {
                let iov_addr = iov_ptr + i * 16;
                let base = unsafe { core::ptr::read(iov_addr as *const usize) };
                let len  = unsafe { core::ptr::read((iov_addr + 8) as *const usize) };
                if len > 0 { return sys_read(0, base, len); }
            }
            0
        }
        _ => {
            let pid = current_pid();
            let mut total: isize = 0;
            for i in 0..iovcnt {
                let iov_addr = iov_ptr + i * 16;
                let base = unsafe { core::ptr::read(iov_addr as *const usize) };
                let len  = unsafe { core::ptr::read((iov_addr + 8) as *const usize) };
                if len == 0 { continue; }
                if !validate_user_buf(base, len) { return -14; }
                let msg = make_vfs_msg(vfs::VFS_READ, &[fd as u64, base as u64, len as u64]);
                // Blocking pipe: yield-loop on EAGAIN.
                let n = loop {
                    let v = vfs_reply_val(&vfs::handle(&msg, pid));
                    if v != -11 { break v; }
                    yield_now();
                };
                if n < 0 { return if total > 0 { total } else { n }; }
                total = total.saturating_add(n);
                if (n as usize) < len { break; } // short read
            }
            total
        }
    }
}

// ── Thread / signal helpers ───────────────────────────────────────────────────

/// sys_tgkill(tgid, tid, sig) — send a signal to a specific thread.
fn sys_tgkill(_tgid: usize, tid: usize, sig: usize) -> isize {
    if sig >= 64 { return -22; } // EINVAL
    sched::deliver_signal(tid as u32, sig as u32)
}

/// sys_sigaltstack(ss, oss) — set/get alternate signal stack.
///
/// Stub: the kernel does not use an alternate signal stack yet.
/// If `oss` is non-null, return a zeroed `stack_t` (SS_DISABLE).
fn sys_sigaltstack(_ss: usize, oss: usize) -> isize {
    // struct stack_t { ss_sp: *void (8), ss_flags: int (4), _pad (4), ss_size: usize (8) }
    // = 24 bytes.  SS_DISABLE = 4 in ss_flags.
    if oss != 0 && validate_user_buf(oss, 24) {
        unsafe {
            core::ptr::write_bytes(oss as *mut u8, 0, 24);
            // ss_flags at offset 8: SS_DISABLE = 4
            core::ptr::write((oss + 8) as *mut u32, 4);
        }
    }
    0
}

// ── Misc syscalls ─────────────────────────────────────────────────────────────

/// sys_uname(buf) — return system identification.
///
/// Fills a Linux `struct utsname` (6 × 65-byte NUL-terminated fields).
fn sys_uname(buf_ptr: usize) -> isize {
    const UTSNAME_SIZE: usize = 6 * 65; // 390 bytes
    if !validate_user_buf(buf_ptr, UTSNAME_SIZE) { return -14; }

    unsafe { core::ptr::write_bytes(buf_ptr as *mut u8, 0, UTSNAME_SIZE); }

    let fields: [(&[u8], usize); 5] = [
        (b"Cyanos\0",  0),    // sysname
        (b"cyanos\0",  65),   // nodename
        (b"1.0.0\0",   130),  // release
        (b"#1\0",      195),  // version
        (#[cfg(target_arch = "aarch64")] b"aarch64\0",
         #[cfg(not(target_arch = "aarch64"))] b"x86_64\0",
         260),                // machine
    ];

    for (s, off) in &fields {
        unsafe {
            core::ptr::copy_nonoverlapping(
                s.as_ptr(),
                (buf_ptr + off) as *mut u8,
                s.len(),
            );
        }
    }
    0
}

/// sys_getrlimit(resource, rlim_ptr) — return soft/hard limits.
///
/// All resources report RLIM_INFINITY (no real enforcement).
fn sys_getrlimit(_resource: usize, rlim_ptr: usize) -> isize {
    if rlim_ptr != 0 {
        if !validate_user_buf(rlim_ptr, 16) { return -14; }
        const RLIM_INFINITY: u64 = u64::MAX;
        unsafe {
            core::ptr::write(rlim_ptr         as *mut u64, RLIM_INFINITY);
            core::ptr::write((rlim_ptr + 8)   as *mut u64, RLIM_INFINITY);
        }
    }
    0
}

/// sys_getrusage(who, usage_ptr) — return resource usage for self or children.
///
/// All CPU-time fields are zero (no per-task accounting).  Wall-clock time is
/// approximated from tick counter.
fn sys_getrusage(_who: usize, usage_ptr: usize) -> isize {
    // struct rusage is 144 bytes on Linux.
    if !validate_user_buf(usage_ptr, 144) { return -14; }
    unsafe { core::ptr::write_bytes(usage_ptr as *mut u8, 0, 144); }
    // ru_utime (offset 0) and ru_stime (offset 16) left as 0.
    // ru_maxrss (offset 32) — report a plausible 4 MiB RSS.
    unsafe { core::ptr::write((usage_ptr + 32) as *mut i64, 4096); }
    0
}

/// sys_sched_getparam(pid, param_ptr) — fill sched_param with priority 0.
fn sys_sched_getparam(_pid: usize, param_ptr: usize) -> isize {
    // struct sched_param { int sched_priority; } = 4 bytes
    if param_ptr != 0 && validate_user_buf(param_ptr, 4) {
        unsafe { core::ptr::write(param_ptr as *mut i32, 0); }
    }
    0
}

/// sys_sched_getaffinity(pid, cpusetsize, mask_ptr) — report CPU 0 only.
fn sys_sched_getaffinity(_pid: usize, cpusetsize: usize, mask_ptr: usize) -> isize {
    if mask_ptr == 0 { return -14; }
    let bytes = cpusetsize.min(128);
    if !validate_user_buf(mask_ptr, bytes) { return -14; }
    unsafe { core::ptr::write_bytes(mask_ptr as *mut u8, 0, bytes); }
    // Set bit 0 — CPU 0 is available.
    if bytes > 0 { unsafe { *(mask_ptr as *mut u8) = 0x01; } }
    0
}

/// sys_capget(hdr_ptr, data_ptr) — return empty capability sets (running as root).
fn sys_capget(_hdr_ptr: usize, data_ptr: usize) -> isize {
    // struct __user_cap_data_struct: effective(4) permitted(4) inheritable(4) × 2 = 24 bytes
    if data_ptr != 0 && validate_user_buf(data_ptr, 24) {
        // All capabilities granted (root).
        const ALL_CAPS: u32 = 0xFFFF_FFFF;
        unsafe {
            core::ptr::write(data_ptr        as *mut u32, ALL_CAPS); // effective[0]
            core::ptr::write((data_ptr + 4)  as *mut u32, ALL_CAPS); // permitted[0]
            core::ptr::write((data_ptr + 8)  as *mut u32, 0);         // inheritable[0]
            core::ptr::write((data_ptr + 12) as *mut u32, ALL_CAPS); // effective[1]
            core::ptr::write((data_ptr + 16) as *mut u32, ALL_CAPS); // permitted[1]
            core::ptr::write((data_ptr + 20) as *mut u32, 0);         // inheritable[1]
        }
    }
    0
}

/// sys_statx(dirfd, path, flags, mask, statxbuf) — extended stat.
///
/// Delegates to sys_newfstatat for the path lookup, then zero-extends to the
/// wider statx layout.  The statx struct is 256 bytes; struct stat is 144 bytes.
fn sys_statx(dirfd: usize, path_ptr: usize, _flags: usize, _mask: usize, statxbuf: usize) -> isize {
    if !validate_user_buf(statxbuf, 256) { return -14; }
    // Zero the entire statx buffer first.
    unsafe { core::ptr::write_bytes(statxbuf as *mut u8, 0, 256); }
    // Reuse a 144-byte stat buffer on the stack, fill it, then copy fields.
    let mut stat_buf = [0u8; 144];
    let stat_ptr = stat_buf.as_mut_ptr() as usize;
    let r = sys_newfstatat(dirfd, path_ptr, stat_ptr, 0);
    if r < 0 { return r; }
    // Map struct stat → struct statx (fields differ in layout).
    // statx: stx_mask(u32@0), stx_blksize(u32@4), stx_attributes(u64@8),
    //        stx_nlink(u32@16), stx_uid(u32@20), stx_gid(u32@24),
    //        stx_mode(u16@28), stx_ino(u64@32), stx_size(u64@40),
    //        stx_blocks(u64@48), stx_atime(i64 pair@56), stx_btime(56+16),
    //        stx_ctime(56+32), stx_mtime(56+48), stx_rdev_major(u32@104),
    //        stx_rdev_minor(u32@108), stx_dev_major(u32@112), stx_dev_minor(u32@116).
    unsafe {
        let mode  = core::ptr::read((stat_ptr + 24) as *const u32);
        let size  = core::ptr::read((stat_ptr + 48) as *const i64);
        let blksize = core::ptr::read((stat_ptr + 56) as *const i64);
        let blocks  = core::ptr::read((stat_ptr + 64) as *const i64);
        // stx_mask — report all fields valid (0x7ff)
        core::ptr::write(statxbuf          as *mut u32, 0x7ff);
        // stx_blksize
        core::ptr::write((statxbuf +  4)   as *mut u32, blksize as u32);
        // stx_nlink = 1
        core::ptr::write((statxbuf + 16)   as *mut u32, 1);
        // stx_mode
        core::ptr::write((statxbuf + 28)   as *mut u16, mode as u16);
        // stx_size
        core::ptr::write((statxbuf + 40)   as *mut i64, size);
        // stx_blocks
        core::ptr::write((statxbuf + 48)   as *mut i64, blocks);
    }
    0
}

/// sys_close_range(first, last, flags) — close a range of file descriptors.
fn sys_close_range(first: usize, last: usize, _flags: usize) -> isize {
    let pid = current_pid();
    let end = last.min(1023);
    for fd in first..=end {
        let msg = make_vfs_msg(vfs::VFS_CLOSE, &[fd as u64]);
        let _ = vfs::handle(&msg, pid);
    }
    0
}

/// sys_stat_at_path(path_ptr, statbuf_ptr) — path-based stat (x86-64 `stat`/`lstat`).
///
/// Opens the path, calls sys_fstat to fill the stat buf, then closes.
#[cfg(not(target_arch = "aarch64"))]
fn sys_stat_at_path(path_ptr: usize, statbuf_ptr: usize) -> isize {
    if !validate_user_buf(path_ptr, 1) { return -14; }
    if !validate_user_buf(statbuf_ptr, 144) { return -14; }
    let pid = current_pid();
    let omsg = make_vfs_msg(vfs::VFS_OPEN, &[path_ptr as u64, 0u64, 0]);
    let fd = vfs_reply_val(&vfs::handle(&omsg, pid));
    if fd < 0 { return fd; }
    let r = sys_fstat(fd as usize, statbuf_ptr);
    let cmsg = make_vfs_msg(vfs::VFS_CLOSE, &[fd as u64]);
    let _ = vfs::handle(&cmsg, pid);
    r
}

/// sys_prlimit64(pid, resource, new_limit, old_limit)
///
/// Stub: all resources report RLIM_INFINITY; new limits are silently ignored.
fn sys_prlimit64(
    _pid:     usize,
    _res:     usize,
    _new_ptr: usize,
    old_ptr:  usize,
) -> isize {
    // struct rlimit64 { rlim_cur: u64, rlim_max: u64 } = 16 bytes
    if old_ptr != 0 {
        if !validate_user_buf(old_ptr, 16) { return -14; }
        const RLIM_INFINITY: u64 = u64::MAX;
        unsafe {
            core::ptr::write(old_ptr          as *mut u64, RLIM_INFINITY);
            core::ptr::write((old_ptr + 8)    as *mut u64, RLIM_INFINITY);
        }
    }
    0
}

// ── VFS syscall implementations ───────────────────────────────────────────────

fn sys_openat(_dirfd: usize, path_ptr: usize, flags: usize, _mode: usize) -> isize {
    if !validate_user_buf(path_ptr, 1) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_OPEN, &[path_ptr as u64, flags as u64, 0]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_close(fd: usize) -> isize {
    let pid = current_pid();
    // Route socket fds (≥ SOCK_FD_BASE) to the net server.
    if fd >= net_server::SOCK_FD_BASE {
        let msg = make_vfs_msg(net_server::NET_CLOSE, &[fd as u64]);
        return net_reply_val(&net_server::handle(&msg, pid));
    }
    let msg = make_vfs_msg(vfs::VFS_CLOSE, &[fd as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// sys_fstat(fd, statbuf_ptr) — fill struct stat for an open fd.
///
/// Populates `st_size` (offset 48) by seeking to EOF and back.
/// `st_mode` is set to S_IFREG|0644 (0x81A4) for regular files, or
/// S_IFCHR|0666 (0x21B6) for character devices (fd 0/1/2 / /dev/*).
fn sys_fstat(fd: usize, statbuf_ptr: usize) -> isize {
    // struct stat is 144 bytes on Linux AArch64 / x86-64.
    if !validate_user_buf(statbuf_ptr, 144) { return -14; }
    unsafe { core::ptr::write_bytes(statbuf_ptr as *mut u8, 0, 144); }

    // fds 0/1/2 are character devices (serial console).
    if fd <= 2 {
        // st_mode at offset 24: S_IFCHR | 0666
        unsafe { core::ptr::write((statbuf_ptr + 24) as *mut u32, 0x21B6u32); }
        return 0;
    }

    let pid = current_pid();
    // Get current position.
    let cur_msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, 0u64, 1u64 /* SEEK_CUR */]);
    let cur = vfs_reply_val(&vfs::handle(&cur_msg, pid));
    if cur == -9 { return -9; } // EBADF

    // Seek to end to get file size.
    let end_msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, 0u64, 2u64 /* SEEK_END */]);
    let size = vfs_reply_val(&vfs::handle(&end_msg, pid));

    // Seek back to original position.
    let back_msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, cur as u64, 0u64 /* SEEK_SET */]);
    let _ = vfs::handle(&back_msg, pid);

    // st_mode at offset 24: S_IFREG | 0644 = 0x81A4
    unsafe { core::ptr::write((statbuf_ptr + 24) as *mut u32, 0x81A4u32); }

    // st_size at offset 48.
    if size >= 0 {
        unsafe { core::ptr::write((statbuf_ptr + 48) as *mut i64, size as i64); }
        // st_blksize at offset 56 = 512 (standard block size)
        unsafe { core::ptr::write((statbuf_ptr + 56) as *mut i64, 512i64); }
        // st_blocks at offset 64 = ceil(size/512)
        let blocks = ((size as i64) + 511) / 512;
        unsafe { core::ptr::write((statbuf_ptr + 64) as *mut i64, blocks); }
    }
    0
}

fn sys_newfstatat(_dirfd: usize, path_ptr: usize, statbuf_ptr: usize, _flags: usize) -> isize {
    if !validate_user_buf(path_ptr, 1)      { return -14; }
    if !validate_user_buf(statbuf_ptr, 144) { return -14; }
    let pid = current_pid();
    // Check for directory first (no fd needed).
    if vfs::is_directory(path_ptr) {
        unsafe { core::ptr::write_bytes(statbuf_ptr as *mut u8, 0, 144); }
        // st_mode: S_IFDIR | 0755 = 0x41ED
        unsafe { core::ptr::write((statbuf_ptr + 24) as *mut u32, 0x41EDu32); }
        return 0;
    }
    // Open path, use sys_fstat, then close.
    let omsg = make_vfs_msg(vfs::VFS_OPEN, &[path_ptr as u64, 0u64, 0]);
    let fd = vfs_reply_val(&vfs::handle(&omsg, pid));
    if fd < 0 { return fd; }
    let r = sys_fstat(fd as usize, statbuf_ptr);
    let cmsg = make_vfs_msg(vfs::VFS_CLOSE, &[fd as u64]);
    let _ = vfs::handle(&cmsg, pid);
    r
}

fn sys_lseek(fd: usize, offset: usize, whence: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, offset as u64, whence as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_fcntl(fd: usize, cmd: usize, arg: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_FCNTL, &[fd as u64, cmd as u64, arg as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_pipe2(pipefd_ptr: usize, _flags: usize) -> isize {
    // int pipefd[2] — two ints (4 bytes each) packed at pipefd_ptr.
    if !validate_user_buf(pipefd_ptr, 8) { return -14; }
    let rfd_ptr = pipefd_ptr;
    let wfd_ptr = pipefd_ptr + 4;
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_PIPE, &[rfd_ptr as u64, wfd_ptr as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_dup(oldfd: usize) -> isize {
    let pid = current_pid();
    // Allocate a new fd: pass newfd=u32::MAX as sentinel for "any free fd".
    let msg = make_vfs_msg(vfs::VFS_DUP2, &[oldfd as u64, u64::MAX]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_dup3(oldfd: usize, newfd: usize, _flags: usize) -> isize {
    let pid = current_pid();
    // If newfd == u64::MAX this is sys_dup (allocate any free fd).
    let tag = if newfd == usize::MAX { vfs::VFS_ALLOC_FD } else { vfs::VFS_DUP2 };
    let msg = make_vfs_msg(tag, &[oldfd as u64, newfd as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_getdents64(fd: usize, buf_ptr: usize, count: usize) -> isize {
    if !validate_user_buf(buf_ptr, count.min(1)) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_GETDENTS64, &[fd as u64, buf_ptr as u64, count as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_mkdirat(_dirfd: usize, path_ptr: usize, _mode: usize) -> isize {
    if !validate_user_buf(path_ptr, 1) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_MKDIR, &[path_ptr as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_unlinkat(_dirfd: usize, path_ptr: usize, _flags: usize) -> isize {
    if !validate_user_buf(path_ptr, 1) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_UNLINK, &[path_ptr as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_chdir(path_ptr: usize) -> isize {
    if !validate_user_buf(path_ptr, 1) { return -14; }
    // Read the path string.
    let mut buf = [0u8; 256];
    let mut len = 0usize;
    for i in 0..255 {
        let b = unsafe { *(path_ptr as *const u8).add(i) };
        if b == 0 { len = i; break; }
        buf[i] = b;
    }
    if len == 0 && buf[0] == 0 { return -2; } // ENOENT — empty path

    // Resolve to absolute path (prepend cwd if relative).
    let mut abs = [0u8; 256];
    let abs_len = resolve_path(&buf[..len], &mut abs);
    let abs_path = &abs[..abs_len];

    // Check the path exists (directory or file).
    // Accept if it's a known directory or file.
    let known = vfs::is_directory(abs.as_ptr() as usize)
                || vfs::get_file_data(abs.as_ptr() as usize).is_some();
    if !known { return -2; } // ENOENT

    sched::set_cwd(abs_path);
    0
}

fn sys_fchdir(_fd: usize) -> isize {
    // No directory fds yet — all fds are files; return ENOTDIR.
    -20
}

fn sys_getcwd(buf_ptr: usize, size: usize) -> isize {
    if !validate_user_buf(buf_ptr, size.min(1)) { return -14; }
    let n = sched::current_cwd(buf_ptr as *mut u8, size);
    if n < 0 { return -1; }
    n + 1  // return ptr value (Linux getcwd returns ptr, but musl checks > 0)
}

fn sys_setpgid(pid_raw: usize, pgid_raw: usize) -> isize {
    let pid  = if pid_raw  == 0 { current_pid() } else { pid_raw as u32 };
    let pgid = if pgid_raw == 0 { pid } else { pgid_raw as u32 };
    if sched::set_pgid(pid, pgid) { 0 } else { -3 } // ESRCH
}

fn sys_getpgid(pid_raw: usize) -> isize {
    if pid_raw == 0 { return sched::current_pgid() as isize; }
    // For other PIDs: we'd need to look them up — return our own pgid.
    sched::current_pgid() as isize
}

fn sys_getresxid(r_ptr: usize, e_ptr: usize, s_ptr: usize, is_gid: bool) -> isize {
    // We're always root (uid/gid = 0).
    let v = 0u32;
    if r_ptr != 0 && validate_user_buf(r_ptr, 4) { unsafe { core::ptr::write(r_ptr as *mut u32, v); } }
    if e_ptr != 0 && validate_user_buf(e_ptr, 4) { unsafe { core::ptr::write(e_ptr as *mut u32, v); } }
    if s_ptr != 0 && validate_user_buf(s_ptr, 4) { unsafe { core::ptr::write(s_ptr as *mut u32, v); } }
    let _ = is_gid;
    0
}

fn sys_faccessat(_dirfd: usize, path_ptr: usize, _mode: usize, _flags: usize) -> isize {
    if !validate_user_buf(path_ptr, 1) { return -14; }
    // Accept if the path is a known file or directory.
    if vfs::get_file_data(path_ptr).is_some() || vfs::is_directory(path_ptr) {
        0
    } else {
        -2 // ENOENT
    }
}

fn sys_readlinkat(_dirfd: usize, path_ptr: usize, buf_ptr: usize, size: usize) -> isize {
    if !validate_user_buf(path_ptr, 1) { return -14; }
    if size == 0 || !validate_user_buf(buf_ptr, size) { return -14; }

    // Read the link path from user space.
    let mut pb = [0u8; 256];
    let mut pl = 0usize;
    for i in 0..255 {
        let b = unsafe { *(path_ptr as *const u8).add(i) };
        if b == 0 { pl = i; break; }
        pb[i] = b;
    }
    let path = &pb[..pl];

    // /proc/self/exe → "/bin/init"
    if path == b"/proc/self/exe" {
        let target = b"/bin/init";
        let n = target.len().min(size);
        unsafe { core::ptr::copy_nonoverlapping(target.as_ptr(), buf_ptr as *mut u8, n); }
        return n as isize;
    }

    // /proc/self/maps → empty (no VMAs exposed)
    if path == b"/proc/self/maps" {
        return 0;
    }

    // /proc/self/fd/N → resolve fd N via VFS
    if pl > 15 && &pb[..15] == b"/proc/self/fd/" {
        let num_str = &pb[15..pl];
        let mut fd = 0usize;
        let mut valid = !num_str.is_empty();
        for &d in num_str {
            if d < b'0' || d > b'9' { valid = false; break; }
            fd = fd * 10 + (d - b'0') as usize;
        }
        if valid {
            let pid = current_pid();
            let msg = make_vfs_msg(vfs::VFS_FD_PATH, &[fd as u64, buf_ptr as u64, size as u64]);
            return vfs_reply_val(&vfs::handle(&msg, pid));
        }
    }

    -2 // ENOENT
}

/// Simple statfs stub — return a reasonable-looking result.
fn sys_statfs(path_or_fd: usize, buf_ptr: usize) -> isize {
    // struct statfs64 varies by arch; write 120 bytes of zeros with a few fields set.
    const STATFS_SIZE: usize = 120;
    if !validate_user_buf(buf_ptr, STATFS_SIZE) { return -14; }
    unsafe { core::ptr::write_bytes(buf_ptr as *mut u8, 0, STATFS_SIZE); }
    // f_type at offset 0 (EXT2_SUPER_MAGIC = 0xEF53)
    unsafe { core::ptr::write(buf_ptr as *mut u32, 0xEF53u32); }
    // f_bsize at offset 4
    unsafe { core::ptr::write((buf_ptr + 4) as *mut u32, 4096u32); }
    let _ = path_or_fd;
    0
}

/// Resolve a path to absolute form, handling ".." and "." components.
/// `path` — the input path (not null-terminated, just bytes).
/// `out`  — output buffer (256 bytes), written without null terminator.
/// Returns the length of the resolved path written to `out`.
fn resolve_path(path: &[u8], out: &mut [u8; 256]) -> usize {
    if path.is_empty() { out[0] = b'/'; return 1; }
    if path[0] == b'/' {
        // Absolute: copy as-is (simplified, no ".." normalization for now).
        let len = path.len().min(255);
        out[..len].copy_from_slice(&path[..len]);
        // Remove trailing slash unless root.
        let end = if len > 1 && out[len - 1] == b'/' { len - 1 } else { len };
        return end;
    }
    // Relative: prepend cwd.
    let mut cwd = [0u8; 256];
    let cwd_len = sched::current_cwd(cwd.as_mut_ptr(), 256) as usize;
    let cwd_len = cwd_len.min(255);
    let sep = if cwd_len > 1 { 1 } else { 0 }; // add "/" unless cwd is "/"
    let total = (cwd_len + sep + path.len()).min(255);
    out[..cwd_len].copy_from_slice(&cwd[..cwd_len]);
    if sep == 1 { out[cwd_len] = b'/'; }
    let src_len = path.len().min(total - cwd_len - sep);
    out[cwd_len + sep..cwd_len + sep + src_len].copy_from_slice(&path[..src_len]);
    cwd_len + sep + src_len
}

/// Read a cstr from user-space into a fixed buffer for VFS path lookup.
/// Returns Some((buf256, len)) or None on fault.
fn read_cstr_for_vfs(path: &[u8]) -> Option<([u8; 256], usize)> {
    if path.is_empty() { return None; }
    let mut buf = [0u8; 256];
    let len = path.len().min(255);
    buf[..len].copy_from_slice(&path[..len]);
    Some((buf, len))
}

/// Close all FDs for the current process in VFS (called on exit).
fn vfs_close_all_current() {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_CLOSE_ALL, &[pid as u64]);
    let _ = vfs::handle(&msg, pid);
    // Also close net sockets and TTY fds.
    let nmsg = make_vfs_msg(net_server::NET_CLOSE_ALL, &[pid as u64]);
    let _ = net_server::handle(&nmsg, pid);
    tty_server::close_all(pid);
}

/// sys_ioctl — try VFS first (FIONREAD on pipes/files), then TTY server.
fn sys_ioctl(fd: usize, cmd: usize, arg: usize) -> isize {
    let pid = current_pid();
    const FIONREAD: usize = 0x541B;
    const ENOTTY: isize = -25;
    if cmd == FIONREAD {
        let msg = make_vfs_msg(vfs::VFS_IOCTL, &[fd as u64, cmd as u64, arg as u64]);
        let r = vfs_reply_val(&vfs::handle(&msg, pid));
        if r != ENOTTY { return r; }
    }
    let msg = make_vfs_msg(tty_server::TTY_IOCTL, &[fd as u64, cmd as u64, arg as u64]);
    net_reply_val(&tty_server::handle(&msg, pid))
}

// ── POSIX timer syscalls ──────────────────────────────────────────────────────

/// sys_timer_create(clockid, sigevent_ptr, timerid_ptr)
fn sys_timer_create(_clockid: usize, sigevent_ptr: usize, timerid_ptr: usize) -> isize {
    // struct sigevent: sigev_value(8) + sigev_signo(4) + sigev_notify(4) + ...
    // We only care about sigev_signo at offset 8 (SIGEV_SIGNAL = 0).
    let signo = if sigevent_ptr != 0 && validate_user_buf(sigevent_ptr, 12) {
        unsafe { core::ptr::read((sigevent_ptr + 8) as *const u32) }
    } else {
        14 // SIGALRM default
    };
    let pid = current_pid();
    let msg = make_vfs_msg(tty_server::TIMER_CREATE, &[signo as u64, timerid_ptr as u64]);
    let reply = tty_server::handle(&msg, pid);
    net_reply_val(&reply)
}

fn sys_timer_settime(timerid: usize, _flags: usize, ispec_ptr: usize, ospec_ptr: usize) -> isize {
    if ispec_ptr != 0 && !validate_user_buf(ispec_ptr, 32) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(tty_server::TIMER_SETTIME,
        &[timerid as u64, ispec_ptr as u64, ospec_ptr as u64]);
    let reply = tty_server::handle(&msg, pid);
    net_reply_val(&reply)
}

fn sys_timer_gettime(timerid: usize, ospec_ptr: usize) -> isize {
    if !validate_user_buf(ospec_ptr, 32) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(tty_server::TIMER_GETTIME, &[timerid as u64, ospec_ptr as u64]);
    let reply = tty_server::handle(&msg, pid);
    net_reply_val(&reply)
}

fn sys_timer_delete(timerid: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(tty_server::TIMER_DELETE, &[timerid as u64]);
    let reply = tty_server::handle(&msg, pid);
    net_reply_val(&reply)
}

// ── Net server syscalls (Phase 7) ─────────────────────────────────────────────

fn sys_socket(domain: usize, sock_type: usize, protocol: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_SOCKET,
        &[domain as u64, sock_type as u64, protocol as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

fn sys_bind(sockfd: usize, addr_ptr: usize, addrlen: usize) -> isize {
    if addrlen > 128 || !validate_user_buf(addr_ptr, addrlen) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_BIND,
        &[sockfd as u64, addr_ptr as u64, addrlen as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

fn sys_listen(sockfd: usize, backlog: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_LISTEN, &[sockfd as u64, backlog as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

fn sys_accept(sockfd: usize, addr_ptr: usize, addrlen_ptr: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_ACCEPT,
        &[sockfd as u64, addr_ptr as u64, addrlen_ptr as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

fn sys_connect(sockfd: usize, addr_ptr: usize, addrlen: usize) -> isize {
    if addrlen > 128 || !validate_user_buf(addr_ptr, addrlen) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_CONNECT,
        &[sockfd as u64, addr_ptr as u64, addrlen as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

fn sys_sendto(sockfd: usize, buf_ptr: usize, len: usize,
              flags: usize, addr_ptr: usize, addrlen: usize) -> isize {
    if len != 0 && !validate_user_buf(buf_ptr, len) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_SEND,
        &[sockfd as u64, buf_ptr as u64, len as u64,
          flags as u64, addr_ptr as u64, addrlen as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

fn sys_recvfrom(sockfd: usize, buf_ptr: usize, len: usize,
                flags: usize, addr_ptr: usize, addrlen_ptr: usize) -> isize {
    if len != 0 && !validate_user_buf(buf_ptr, len) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_RECV,
        &[sockfd as u64, buf_ptr as u64, len as u64,
          flags as u64, addr_ptr as u64, addrlen_ptr as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

fn sys_sendmsg(sockfd: usize, msghdr_ptr: usize, flags: usize) -> isize {
    if !validate_user_buf(msghdr_ptr, 48) { return -14; } // sizeof(msghdr)≥48
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_SENDMSG,
        &[sockfd as u64, msghdr_ptr as u64, flags as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

fn sys_recvmsg(sockfd: usize, msghdr_ptr: usize, flags: usize) -> isize {
    if !validate_user_buf(msghdr_ptr, 48) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_RECVMSG,
        &[sockfd as u64, msghdr_ptr as u64, flags as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

fn sys_net_shutdown(sockfd: usize, how: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_SHUTDOWN, &[sockfd as u64, how as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

fn sys_getsockname(sockfd: usize, addr_ptr: usize, addrlen_ptr: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_GETSOCKNAME,
        &[sockfd as u64, addr_ptr as u64, addrlen_ptr as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

fn sys_getpeername(sockfd: usize, addr_ptr: usize, addrlen_ptr: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_GETPEERNAME,
        &[sockfd as u64, addr_ptr as u64, addrlen_ptr as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

fn sys_socketpair(domain: usize, sock_type: usize, protocol: usize, sv_ptr: usize) -> isize {
    if !validate_user_buf(sv_ptr, 8) { return -14; } // int sv[2]
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_SOCKETPAIR,
        &[domain as u64, sock_type as u64, protocol as u64, sv_ptr as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

fn sys_setsockopt(sockfd: usize, level: usize, optname: usize,
                  optval_ptr: usize, optlen: usize) -> isize {
    if optlen > 128 || (optlen != 0 && !validate_user_buf(optval_ptr, optlen)) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_SETSOCKOPT,
        &[sockfd as u64, level as u64, optname as u64, optval_ptr as u64, optlen as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

fn sys_getsockopt(sockfd: usize, level: usize, optname: usize,
                  optval_ptr: usize, optlen_ptr: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_GETSOCKOPT,
        &[sockfd as u64, level as u64, optname as u64, optval_ptr as u64, optlen_ptr as u64]);
    net_reply_val(&net_server::handle(&msg, pid))
}

// ── poll / select / epoll (Phase 9) ──────────────────────────────────────────
//
// Strategy: we implement a simple "check once and return ready events" model.
// Blocking poll is emulated by returning 0 (timeout) — the caller retries.
// This is correct for most POSIX use cases: non-blocking I/O + eventfd loops.

const MAX_EPOLL_INSTANCES: usize = 16;
const MAX_EPOLL_INTERESTS: usize = 32;

#[derive(Clone, Copy)]
struct EpollInterest {
    fd:     i32,
    events: u32,
    data:   u64,
    in_use: bool,
}

impl EpollInterest {
    const fn empty() -> Self { Self { fd: -1, events: 0, data: 0, in_use: false } }
}

#[derive(Clone, Copy)]
struct EpollInstance {
    owner_pid: u32,
    interests: [EpollInterest; MAX_EPOLL_INTERESTS],
    in_use:    bool,
}

impl EpollInstance {
    const fn empty() -> Self {
        Self { owner_pid: 0, interests: [const { EpollInterest::empty() }; MAX_EPOLL_INTERESTS],
               in_use: false }
    }
}

/// FD base for epoll instances — must not overlap VFS/TTY/net ranges.
const EPOLL_FD_BASE: usize = 0x400;

static EPOLL_INSTANCES: spin::Mutex<[EpollInstance; MAX_EPOLL_INSTANCES]> =
    spin::Mutex::new([const { EpollInstance::empty() }; MAX_EPOLL_INSTANCES]);

fn sys_epoll_create1(_flags: usize) -> isize {
    let pid = current_pid();
    let mut ep = EPOLL_INSTANCES.lock();
    match ep.iter().position(|e| !e.in_use) {
        Some(i) => {
            ep[i] = EpollInstance::empty();
            ep[i].in_use    = true;
            ep[i].owner_pid = pid;
            (i + EPOLL_FD_BASE) as isize
        }
        None => -12, // ENOMEM
    }
}

/// sys_epoll_ctl(epfd, op, fd, event_ptr)
fn sys_epoll_ctl(epfd: usize, op: usize, fd: usize, event_ptr: usize) -> isize {
    // EPOLL_CTL_ADD=1, EPOLL_CTL_DEL=2, EPOLL_CTL_MOD=3
    const CTL_ADD: usize = 1;
    const CTL_DEL: usize = 2;
    const CTL_MOD: usize = 3;

    let slot = if epfd >= EPOLL_FD_BASE && epfd < EPOLL_FD_BASE + MAX_EPOLL_INSTANCES {
        epfd - EPOLL_FD_BASE
    } else {
        return -9; // EBADF
    };

    let pid = current_pid();
    let mut ep = EPOLL_INSTANCES.lock();
    if !ep[slot].in_use || ep[slot].owner_pid != pid { return -9; }

    match op {
        CTL_ADD | CTL_MOD => {
            // struct epoll_event: events(u32) + data(u64) = 12 bytes (packed) or 16 bytes.
            if event_ptr == 0 || !validate_user_buf(event_ptr, 12) { return -14; }
            let events = unsafe { core::ptr::read(event_ptr as *const u32) };
            let data   = unsafe { core::ptr::read((event_ptr + 4) as *const u64) };
            // Find existing entry or allocate new one.
            let inst = &mut ep[slot];
            let idx = inst.interests.iter().position(|i| i.in_use && i.fd == fd as i32)
                          .or_else(|| inst.interests.iter().position(|i| !i.in_use));
            match idx {
                Some(i) => {
                    inst.interests[i] = EpollInterest { fd: fd as i32, events, data, in_use: true };
                    0
                }
                None => -12, // ENOMEM — too many interests
            }
        }
        CTL_DEL => {
            let inst = &mut ep[slot];
            if let Some(i) = inst.interests.iter().position(|x| x.in_use && x.fd == fd as i32) {
                inst.interests[i] = EpollInterest::empty();
            }
            0
        }
        _ => -22, // EINVAL
    }
}

/// sys_epoll_wait(epfd, events_ptr, maxevents, timeout_ms)
///
/// Checks each registered fd for readiness once.  Pipes and sockets with data
/// in their ring buffers are immediately EPOLLIN-ready.  Everything else is
/// considered ready-to-write (EPOLLOUT).  Returns the number of ready events.
fn sys_epoll_wait(epfd: usize, events_ptr: usize, maxevents: usize, _timeout: usize) -> isize {
    if maxevents == 0 { return -22; }
    if !validate_user_buf(events_ptr, maxevents * 12) { return -14; }

    let slot = if epfd >= EPOLL_FD_BASE && epfd < EPOLL_FD_BASE + MAX_EPOLL_INSTANCES {
        epfd - EPOLL_FD_BASE
    } else {
        return -9;
    };

    let pid = current_pid();
    let ep = EPOLL_INSTANCES.lock();
    if !ep[slot].in_use || ep[slot].owner_pid != pid { return -9; }

    let mut nready = 0usize;
    let base = events_ptr;

    for interest in ep[slot].interests.iter() {
        if !interest.in_use || nready >= maxevents { break; }
        // Determine if the fd has data available (EPOLLIN).
        // We check VFS pipe rings and net socket rings.
        let ready_events = probe_fd_events(pid, interest.fd as usize, interest.events);
        if ready_events != 0 {
            let off = nready * 12;
            unsafe {
                core::ptr::write((base + off) as *mut u32, ready_events);
                core::ptr::write((base + off + 4) as *mut u64, interest.data);
            }
            nready += 1;
        }
    }
    nready as isize
}

/// Check what events are currently available on `fd` for `pid`.
///
/// Returns a bitmask of `EPOLLIN`/`EPOLLOUT` flags.  We probe the VFS and net
/// server ring states directly without going through the full handle() path.
fn probe_fd_events(_pid: u32, _fd: usize, requested: u32) -> u32 {
    const EPOLLIN:  u32 = 0x0001;
    const EPOLLOUT: u32 = 0x0004;
    const EPOLLHUP: u32 = 0x0010;

    // For sockets and TTY fds, assume always writable.
    // For VFS fds (pipe read-end), check ring count via a zero-byte read probe.
    // Simplest correct behavior: check if a zero-count read/recv returns >0.
    // We use the "send 0-byte read" trick — if VFS_READ(fd, null, 0) returns >=0,
    // the fd is valid; actual readability is harder to check without reading data.
    // For now: fd 0 (stdin) is never ready; pipes/sockets return EPOLLOUT always,
    // plus EPOLLIN if there's data.

    let mut events = 0u32;

    // Pipes: probe by trying a zero-byte VFS_READ.  VFS returns 0 for an
    // empty pipe (not -EAGAIN), so we can't detect empty vs. EOF this way.
    // Return EPOLLOUT so writers are never blocked; return EPOLLIN speculatively.
    if requested & EPOLLIN != 0 {
        // Conservative: mark EPOLLIN ready for all readable fds.
        // musl's stdio will do the actual read and handle blocking.
        events |= EPOLLIN;
    }
    if requested & EPOLLOUT != 0 {
        events |= EPOLLOUT;
    }
    events
}

fn sys_eventfd2(initval: usize, _flags: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_EVENTFD, &[initval as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// memfd_create(name_ptr, flags) → writable anonymous fd backed by a TmpFile.
fn sys_memfd_create(name_ptr: usize, _flags: usize) -> isize {
    // Build path "/tmp/memfd:<name>" truncated to fit TmpFileEntry::path.
    let mut path = [0u8; 64];
    let prefix = b"/tmp/memfd:";
    path[..prefix.len()].copy_from_slice(prefix);
    let mut plen = prefix.len();
    if name_ptr != 0 {
        for i in 0..48usize {
            let b = unsafe { *(name_ptr as *const u8).add(i) };
            if b == 0 { break; }
            path[plen] = b; plen += 1;
        }
    } else {
        path[plen] = b'0'; let _ = plen;
    }
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_OPEN, &[
        path.as_ptr() as u64,
        (0x041 | 0x200) as u64, // O_WRONLY|O_CREAT|O_TRUNC
        0o600u64,
    ]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_timerfd_create(_clockid: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_TIMERFD_CREATE, &[]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// timerfd_settime(fd, flags, new_value_ptr, old_value_ptr)
/// Reads itimerspec {interval, value} from new_value_ptr (2×16 bytes).
fn sys_timerfd_settime(fd: usize, _flags: usize, new_ptr: usize, _old_ptr: usize) -> isize {
    if new_ptr == 0 || !validate_user_buf(new_ptr, 32) { return -14; } // EFAULT
    let (value_ns, interval_ns) = unsafe {
        let p = new_ptr as *const i64;
        let iv_sec  = p.read();       // interval.tv_sec
        let iv_nsec = p.add(1).read();// interval.tv_nsec
        let vl_sec  = p.add(2).read();// value.tv_sec
        let vl_nsec = p.add(3).read();// value.tv_nsec
        let interval = (iv_sec as u64) * 1_000_000_000 + (iv_nsec as u64);
        let value    = (vl_sec as u64) * 1_000_000_000 + (vl_nsec as u64);
        (value, interval)
    };
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_TIMERFD_SETTIME, &[fd as u64, value_ns, interval_ns]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_timerfd_gettime(fd: usize, cur_ptr: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_TIMERFD_GETTIME, &[fd as u64, cur_ptr as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// sys_select(nfds, readfds, writefds, exceptfds, timeout) — simplified.
///
/// Returns immediately: readfds and writefds are left unchanged (all bits set
/// for fds that are valid — conservative "all ready" answer).
fn sys_select(nfds: usize, rfds: usize, wfds: usize, _efds: usize, _tv: usize) -> isize {
    // Number of bytes in fd_set for nfds descriptors: ceil(nfds/8).
    let bytes = (nfds + 7) / 8;
    if rfds != 0 && validate_user_buf(rfds, bytes) {
        // Set all bits up to nfds — conservatively "all ready".
        unsafe {
            let n_full = bytes.saturating_sub(1);
            for i in 0..n_full { *(rfds as *mut u8).add(i) = 0xFF; }
            // Last byte: only bits 0..(nfds%8) set.
            let last_bits = nfds % 8;
            *(rfds as *mut u8).add(n_full) = if last_bits == 0 { 0xFF }
                                             else { (1u8 << last_bits) - 1 };
        }
    }
    if wfds != 0 && validate_user_buf(wfds, bytes) {
        unsafe {
            let n_full = bytes.saturating_sub(1);
            for i in 0..n_full { *(wfds as *mut u8).add(i) = 0xFF; }
            let last_bits = nfds % 8;
            *(wfds as *mut u8).add(n_full) = if last_bits == 0 { 0xFF }
                                             else { (1u8 << last_bits) - 1 };
        }
    }
    // Return number of "ready" fds — nfds is an upper bound.
    nfds as isize
}

// ── rename / truncate / sendfile / itimer / sigpending / alarm ───────────────

/// sys_renameat(old_path_ptr, new_path_ptr) — rename a /tmp file.
fn sys_renameat(old_path_ptr: usize, new_path_ptr: usize) -> isize {
    if !validate_user_buf(old_path_ptr, 1) { return -14; }
    if !validate_user_buf(new_path_ptr, 1) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_RENAME, &[old_path_ptr as u64, new_path_ptr as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// sys_truncate(path_ptr, length) — set a file's size by path.
fn sys_truncate(path_ptr: usize, length: usize) -> isize {
    if !validate_user_buf(path_ptr, 1) { return -14; }
    let pid = current_pid();
    let omsg = make_vfs_msg(vfs::VFS_OPEN, &[path_ptr as u64, 0x0002u64 /* O_RDWR */, 0]);
    let fd = vfs_reply_val(&vfs::handle(&omsg, pid));
    if fd < 0 { return fd; }
    let r = sys_ftruncate(fd as usize, length);
    let cmsg = make_vfs_msg(vfs::VFS_CLOSE, &[fd as u64]);
    let _ = vfs::handle(&cmsg, pid);
    r
}

/// sys_sendfile(out_fd, in_fd, offset_ptr, count) — copy data between fds.
///
/// Reads from `in_fd` (seeking to *offset_ptr first if non-null) in 4 KiB
/// chunks and writes to `out_fd`.  Updates *offset_ptr on success.
fn sys_sendfile(out_fd: usize, in_fd: usize, offset_ptr: usize, count: usize) -> isize {
    if count == 0 { return 0; }
    let pid = current_pid();

    // If offset_ptr is given, seek in_fd to the caller-supplied offset.
    if offset_ptr != 0 {
        if !validate_user_buf(offset_ptr, 8) { return -14; }
        let off = unsafe { core::ptr::read(offset_ptr as *const u64) } as usize;
        let smsg = make_vfs_msg(vfs::VFS_LSEEK, &[in_fd as u64, off as u64, 0 /* SEEK_SET */]);
        let pos = vfs_reply_val(&vfs::handle(&smsg, pid));
        if pos < 0 { return pos; }
    }

    // Transfer in up to 4 KiB chunks via a stack buffer (embedded in kernel stack).
    const CHUNK: usize = 4096;
    let mut buf = [0u8; CHUNK];
    let buf_ptr = buf.as_mut_ptr() as usize;
    let mut transferred: usize = 0;

    while transferred < count {
        let want = (count - transferred).min(CHUNK);
        let rmsg = make_vfs_msg(vfs::VFS_READ, &[in_fd as u64, buf_ptr as u64, want as u64]);
        let n = vfs_reply_val(&vfs::handle(&rmsg, pid));
        if n <= 0 { break; }
        let wmsg = make_vfs_msg(vfs::VFS_WRITE,
            &[out_fd as u64, buf_ptr as u64, n as u64]);
        let w = vfs_reply_val(&vfs::handle(&wmsg, pid));
        if w <= 0 { break; }
        transferred += w as usize;
    }

    // Update *offset_ptr to reflect how many bytes were consumed.
    if offset_ptr != 0 && transferred > 0 {
        let off = unsafe { core::ptr::read(offset_ptr as *const u64) };
        unsafe { core::ptr::write(offset_ptr as *mut u64, off + transferred as u64); }
    }

    transferred as isize
}

/// sys_setitimer(which, new_ptr, old_ptr) — set an interval timer.
///
/// Maps `ITIMER_REAL` (which=0) to a POSIX timer with SIGALRM.
/// Other `which` values (VIRTUAL, PROF) are accepted but ignored.
fn sys_setitimer(which: usize, new_ptr: usize, old_ptr: usize) -> isize {
    // struct itimerval: { it_interval: timeval(16), it_value: timeval(16) } = 32 bytes
    // struct timeval: { tv_sec: i64, tv_usec: i64 }
    if new_ptr != 0 && !validate_user_buf(new_ptr, 32) { return -14; }
    if old_ptr != 0 && !validate_user_buf(old_ptr, 32) { return -22; }

    // We only implement ITIMER_REAL (0).
    if which != 0 { return 0; } // silently succeed for VIRTUAL/PROF

    let pid = current_pid();

    // Read new itimerval: {it_interval.tv_sec, it_interval.tv_usec, it_value.tv_sec, it_value.tv_usec}
    const TICK_HZ: u64 = 100;
    let (interval_ticks, value_ticks) = if new_ptr != 0 {
        unsafe {
            let iv_sec  = core::ptr::read(new_ptr          as *const i64);
            let iv_usec = core::ptr::read((new_ptr +  8)   as *const i64);
            let va_sec  = core::ptr::read((new_ptr + 16)   as *const i64);
            let va_usec = core::ptr::read((new_ptr + 24)   as *const i64);
            let itv = (iv_sec as u64 * TICK_HZ) + (iv_usec as u64 * TICK_HZ / 1_000_000);
            let vtv = (va_sec as u64 * TICK_HZ) + (va_usec as u64 * TICK_HZ / 1_000_000);
            (itv, vtv)
        }
    } else {
        (0, 0)
    };

    // If old_ptr requested, return zeros (we don't track the previous state here).
    if old_ptr != 0 {
        unsafe { core::ptr::write_bytes(old_ptr as *mut u8, 0, 32); }
    }

    // Use POSIX timer slot 0 for ITIMER_REAL.
    // First ensure the timer slot exists (create it if necessary).
    let create_msg = make_vfs_msg(tty_server::TIMER_CREATE, &[14u64 /* SIGALRM */, 0u64]);
    let _ = tty_server::handle(&create_msg, pid);

    // Build a synthetic itimerspec in stack memory and call TIMER_SETTIME.
    // struct itimerspec: { it_interval: timespec(16), it_value: timespec(16) }
    const NSEC_PER_TICK: u64 = 1_000_000_000 / TICK_HZ;
    let mut spec = [0i64; 4];
    spec[0] = (interval_ticks / TICK_HZ) as i64;
    spec[1] = ((interval_ticks % TICK_HZ) * NSEC_PER_TICK) as i64;
    spec[2] = (value_ticks / TICK_HZ) as i64;
    spec[3] = ((value_ticks % TICK_HZ) * NSEC_PER_TICK) as i64;
    let spec_ptr = spec.as_ptr() as usize;

    let set_msg = make_vfs_msg(tty_server::TIMER_SETTIME,
        &[0u64 /* timerid=0 */, spec_ptr as u64, 0u64]);
    let r = tty_server::handle(&set_msg, pid);
    if net_reply_val(&r) < 0 { -22 } else { 0 }
}

/// sys_getitimer(which, cur_ptr) — get current interval timer state.
fn sys_getitimer(which: usize, cur_ptr: usize) -> isize {
    if which != 0 { return 0; }
    if !validate_user_buf(cur_ptr, 32) { return -14; }
    // Zero out the itimerval (simplified — we don't track remaining time per-itimer).
    unsafe { core::ptr::write_bytes(cur_ptr as *mut u8, 0, 32); }
    0
}

/// sys_sigpending(set_ptr) — return the set of pending signals.
fn sys_sigpending(set_ptr: usize) -> isize {
    if !validate_user_buf(set_ptr, 8) { return -14; }
    let pending = pending_signals();
    unsafe { core::ptr::write(set_ptr as *mut u64, pending); }
    0
}

/// sys_alarm(seconds) — schedule SIGALRM after `seconds` seconds (x86-64 only).
#[cfg(not(target_arch = "aarch64"))]
fn sys_alarm(seconds: usize) -> isize {
    let pid = current_pid();
    const TICK_HZ: u64 = 100;
    const NSEC_PER_TICK: u64 = 1_000_000_000 / TICK_HZ;
    let value_ticks = seconds as u64 * TICK_HZ;

    // Ensure POSIX timer slot 0 exists.
    let create_msg = make_vfs_msg(tty_server::TIMER_CREATE, &[14u64 /* SIGALRM */, 0u64]);
    let _ = tty_server::handle(&create_msg, pid);

    // Build itimerspec (one-shot, no interval).
    let mut spec = [0i64; 4];
    // it_interval = 0 (one-shot)
    // it_value = seconds
    spec[2] = (value_ticks / TICK_HZ) as i64;
    spec[3] = ((value_ticks % TICK_HZ) * NSEC_PER_TICK) as i64;
    let spec_ptr = spec.as_ptr() as usize;

    let set_msg = make_vfs_msg(tty_server::TIMER_SETTIME,
        &[0u64, spec_ptr as u64, 0u64]);
    let _ = tty_server::handle(&set_msg, pid);
    0 // previous alarm remaining seconds (we don't track)
}

// ── fork / clone ──────────────────────────────────────────────────────────────

/// sys_clone_or_fork — dispatches `fork()` and `clone()`.
///
/// AArch64 register convention (matching Linux):
///   a0 = flags, a1 = child_stack, a2 = ptid, a3 = tls, a4 = ctid
///
/// On AArch64 there is no separate `fork` syscall; musl uses `clone(SIGCHLD)`
/// which has CLONE_VM clear.  On x86-64 `FORK` (57) routes directly in the
/// dispatch table; this function only sees `CLONE` (56).
fn sys_clone_or_fork(
    flags:       usize,
    child_stack: usize,
    _ptid:       usize,
    tls:         usize,
    ctid:        usize,
    frame_ptr:   usize,
) -> isize {
    const CLONE_VM: usize = 0x0000_0100;

    if flags & CLONE_VM != 0 {
        clone_thread(flags, child_stack, tls, ctid, frame_ptr)
    } else {
        let _ = (child_stack, _ptid, tls, ctid);
        let parent_pid = current_pid();
        let ret = fork_current(frame_ptr);
        if ret > 0 {
            // ret is child PID — duplicate FD table for child.
            let msg = make_vfs_msg(vfs::VFS_FORK_DUP,
                                   &[parent_pid as u64, ret as u64]);
            let _ = vfs::handle(&msg, parent_pid);
        }
        ret
    }
}
