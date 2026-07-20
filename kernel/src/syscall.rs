//! Syscall dispatch — the only controlled gate into kernel space.
#![allow(dead_code)]
//!
//! Syscall ABI (register mapping follows Linux on each arch):
//!   AArch64: x8 = number, x0-x5 = args, x0 = return value
//!   x86-64:  rax = number, rdi/rsi/rdx/r10/r8/r9 = args, rax = return value
//!
//! Syscall numbers match Linux ABI so that musl libc requires no patching.
//! Leandros-private syscalls (IPC, spawn) use numbers above 509.

use core::sync::atomic::{AtomicUsize, AtomicU32, Ordering};
use alloc::vec::Vec;
use crate::{serial_print_str, serial_write_raw, BOOT_INFO_PTR, init};
use ipc::{Message, port};
use sched::{
    fork_current, clone_thread,
    sys_sigaction, sys_sigprocmask, sys_sigaltstack, restore_signal_frame,
    current_pid, current_ppid,
    ticks, yield_now, irq_window, exit, spawn_user,
    pending_signals, clear_pending_signal, replace_signal_mask,
    current_reply_port, set_current_reply_port, set_clear_child_tid,
    block_on_port_prepare, block_on_port_cancel, block_on_port_commit,
    replace_address_space,
    with_current_address_space, with_current_address_space_mut
};
#[cfg(target_arch = "x86_64")]
use sched::{set_fs_base, get_fs_base};
use mm::paging::PageFlags;
use elf;
use vfs_server as vfs;
use net_server;
use tty_server;
use evdev_server;

/// Bump allocator base for anonymous mmap with no hint (addr=0).
static MMAP_BUMP: AtomicUsize = AtomicUsize::new(0x0000_4000_0000_usize);

/// IPC port of the VFS server; u32::MAX = not yet registered.
static VFS_SERVER_PORT: AtomicU32 = AtomicU32::new(u32::MAX);

/// Auxv tag: Leandros VFS server port (private, value > AT_MINSIGSTKSZ).
const AT_LEANDROS_VFS_PORT: u64 = 256;

/// Register the VFS server port so sys_execve can embed it in auxv.
pub fn set_vfs_server_port(port: u32) {
    VFS_SERVER_PORT.store(port, Ordering::Relaxed);
    sched::set_vfs_port(port);
}

/// IPC port of the net server; u32::MAX = not yet registered.
static NET_SERVER_PORT: AtomicU32 = AtomicU32::new(u32::MAX);

/// Auxv tag: Leandros net server port.
const AT_LEANDROS_NET_PORT: u64 = 257;

pub fn set_net_server_port(port: u32) {
    NET_SERVER_PORT.store(port, Ordering::Relaxed);
    sched::set_net_port(port);
}

/// IPC port of the audio server; u32::MAX = not yet registered.
static AUDIO_SERVER_PORT: AtomicU32 = AtomicU32::new(u32::MAX);

/// Auxv tag: Leandros audio server port.
const AT_LEANDROS_AUDIO_PORT: u64 = 258;

pub fn set_audio_server_port(port: u32) {
    AUDIO_SERVER_PORT.store(port, Ordering::Relaxed);
    sched::set_audio_port(port);
}

// ── Demand-paged exec: backing-file registry ──────────────────────────────────
//
// A demand-paged exec image keeps its ELF file open on the mounted
// filesystem for the lifetime of the process image; the page-fault handler
// reads pages from it on first touch.  Each entry is identified by a
// capability token (`index + 1`, so 0 stays "anonymous" and usize::MAX stays
// the device-mapping sentinel) stored in the VMAs' `file_cap`.  `refs`
// counts live VMAs (across fork clones) plus a transient creation reference
// held during sys_execve setup; the file is closed on the mount when it
// drops to zero.

#[derive(Clone, Copy)]
struct ExecFileEntry {
    port:    u32, // IPC port of the owning mount (used for direct f2fs calls)
    file_id: u32, // open-file slot on that mount
    refs:    u32,
}

const MAX_EXEC_FILES: usize = 64;
static EXEC_FILES: spin::Mutex<[Option<ExecFileEntry>; MAX_EXEC_FILES]> =
    spin::Mutex::new([None; MAX_EXEC_FILES]);

fn exec_file_register(port: u32, file_id: u32) -> Option<usize> {
    let mut tbl = EXEC_FILES.lock();
    for (i, slot) in tbl.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(ExecFileEntry { port, file_id, refs: 1 });
            return Some(i + 1);
        }
    }
    None
}

/// mm file-read hook.  Runs in page-fault context: everything below is a
/// synchronous direct call into the f2fs server (registered port handlers
/// execute in the caller's context) ending in polled virtio I/O — no
/// blocking, no rescheduling, no IPC reply ports.
fn exec_file_read(cap: usize, offset: u64, dst: *mut u8, len: usize) -> bool {
    if cap == 0 || cap > MAX_EXEC_FILES { return false; }
    let entry = match EXEC_FILES.lock()[cap - 1] {
        Some(e) => e,
        None    => return false,
    };
    f2fs_server::pread_by_port(entry.port, entry.file_id as u64, dst, len, offset)
        == len as isize
}

/// mm file-retain hook (one reference per live VMA; fork clones retain).
fn exec_file_retain(cap: usize) {
    if cap == 0 || cap > MAX_EXEC_FILES { return; }
    if let Some(ref mut e) = EXEC_FILES.lock()[cap - 1] {
        e.refs += 1;
    }
}

/// mm file-release hook; closes the mount-side file when the last VMA goes.
fn exec_file_release(cap: usize) {
    if cap == 0 || cap > MAX_EXEC_FILES { return; }
    let closed = {
        let mut tbl = EXEC_FILES.lock();
        match tbl[cap - 1] {
            Some(ref mut e) => {
                e.refs -= 1;
                if e.refs == 0 {
                    let ent = *e;
                    tbl[cap - 1] = None;
                    Some(ent)
                } else {
                    None
                }
            }
            None => None,
        }
    };
    if let Some(e) = closed {
        f2fs_server::close_by_port(e.port, e.file_id as u64);
    }
}

/// Wire the mm crate's file-backed-VMA hooks to the registry above.
/// Called once from kernel init, before userspace starts.
pub fn init_exec_file_backing() {
    mm::vmm::set_file_backing_hooks(exec_file_read, exec_file_retain, exec_file_release);
}

/// Fault in `[ptr, ptr+len)` of the current address space.
///
/// Kernel and server code dereferences user buffer pointers directly (the
/// servers run synchronously in the caller's context).  With demand-paged
/// exec images a user pointer can now name a page that was never touched —
/// e.g. a string literal in a .rodata page — and taking that fault *inside*
/// f2fs would re-enter the filesystem from the fault handler and deadlock on
/// F2FS_MOUNTS.  Every user pointer that flows into vfs::handle must
/// therefore be faulted in first, while no filesystem lock is held.
fn prefault_user(ptr: usize, len: usize) {
    if ptr == 0 || len == 0 { return; }
    let _ = with_current_address_space_mut(|as_| as_.prefault_range(ptr, len));
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
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&reply.data[0..8]);
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
/// Size of the initial user stack mapping (256 KiB).
const USER_STACK_SIZE: usize = 64 * mm::buddy::PAGE_SIZE;

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

// ── Leandros-private (same on all architectures) ────────────────────────────────
pub const SYS_IPC_SEND: usize = 511;
pub const SYS_IPC_RECV: usize = 512;
pub const SYS_IPC_CALL: usize = 513;
pub const SYS_SPAWN:    usize = 510;
/// Create a queue-based port owned by the calling task and return its id.
/// Exists so userspace can exercise the raw send/recv blocking path
/// directly (see userland/racetest); servers create their ports kernel-side.
pub const SYS_PORT_CREATE: usize = 514;
/// Device-enumeration syscalls backing lsblk/lspci/lsusb — no Linux
/// equivalent (Linux does this via ioctls on device nodes or sysfs).
pub const SYS_BLKDEV_COUNT: usize = 515;
pub const SYS_BLKDEV_INFO:  usize = 516;
pub const SYS_PCIDEV_COUNT: usize = 517;
pub const SYS_PCIDEV_INFO:  usize = 518;
pub const SYS_USBDEV_COUNT: usize = 519;
pub const SYS_USBDEV_INFO:  usize = 520;
/// Live mount table (servers/vfs::list_mounts), backing `mount`/`lsblk`.
/// Deliberately a syscall rather than a `/proc/mounts` read: the VFS's
/// RAMFS-served files are fixed &'static byte slices baked in at compile
/// time (see servers/vfs/src/lib.rs RAMFS), not a place designed for
/// per-open dynamic content, so this sidesteps that rather than fighting it.
pub const SYS_MOUNTS_COUNT: usize = 521;
pub const SYS_MOUNTS_INFO:  usize = 522;

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
    pub const TKILL:          usize = 130;
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
    pub const TIMER_GETOVERRUN: usize = 109;
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
    pub const GETCPU:              usize = 168;
    pub const UMOUNT2:             usize = 39;
    pub const MOUNT:               usize = 40;
    pub const PIVOT_ROOT:          usize = 41;
}

// ── x86-64 Linux syscall numbers ──────────────────────────────────────────────
#[cfg(not(target_arch = "aarch64"))]
mod nr {
    pub const MMAP:           usize = 9;
    pub const POLL:           usize = 7;
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
    pub const TKILL:          usize = 200;
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
    pub const TIMER_GETOVERRUN: usize = 225;
    pub const TIMER_DELETE:   usize = 226;
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
    pub const GETCPU:              usize = 309;
    pub const MOUNT:               usize = 165;
    pub const UMOUNT2:             usize = 166;
    pub const PIVOT_ROOT:          usize = 155;
}

use nr::*;

// ── Arch-only extern ──────────────────────────────────────────────────────────
extern "C" { fn arch_alloc_page_table_root() -> usize; }

/// Top-level syscall handler, invoked from the arch-specific trap stub.
///
/// The `frame_ptr` argument carries the address of the `UserFrame` saved on
/// the kernel stack by the trap entry path, on both AArch64 (EL0 exception
/// handler) and x86-64 (SYSCALL entry trampoline).
#[no_mangle]
pub extern "C" fn syscall_dispatch(
    number: usize,
    a0: usize, a1: usize, a2: usize,
    a3: usize, a4: usize,
    a5: usize, frame_ptr: usize, _padding: usize,
) -> isize {
    dispatch(number, a0, a1, a2, a3, a4, a5, frame_ptr)
}

pub fn dispatch(
    number: usize,
    a0: usize, a1: usize, a2: usize,
    a3: usize, a4: usize, a5: usize,
    frame_ptr: usize,
) -> isize {
    let ret = dispatch_inner(number, a0, a1, a2, a3, a4, a5, frame_ptr);
    if SYSCALL_TRACE_EINVAL && ret == -22 && current_pid() >= 3 {
        let _g = TRACE_LOCK.lock();
        #[cfg(target_arch = "aarch64")]
        if frame_ptr != 0 {
            let uf = unsafe { &*(frame_ptr as *const sched::context::UserFrame) };
            crate::serial_print_str("[SC-EINVAL] caller-pc=");
            crate::serial_print_hex(uf.elr_el1 as usize);
            crate::serial_print_str(" lr=");
            crate::serial_print_hex(uf.x[30] as usize);
            // Walk the user frame-pointer chain for a short backtrace:
            // AArch64 frame record is [x29] = previous x29, [x29+8] = LR.
            let mut fp = uf.x[29] as usize;
            for _ in 0..6 {
                if fp == 0 || fp % 8 != 0 { break; }
                let mut rec = [0u8; 16];
                let ok = with_current_address_space(|as_| as_.read_user_buf(fp, &mut rec))
                    .unwrap_or(false);
                if !ok { break; }
                let prev_fp = usize::from_ne_bytes(rec[0..8].try_into().unwrap());
                let lr = usize::from_ne_bytes(rec[8..16].try_into().unwrap());
                crate::serial_print_str(" <- ");
                crate::serial_print_hex(lr);
                if prev_fp <= fp { break; }
                fp = prev_fp;
            }
            crate::serial_print_str("\n");
        }
        crate::serial_print_str("[SC-EINVAL] nr=");
        crate::serial_print_hex(number);
        crate::serial_print_str(" a0=");
        crate::serial_print_hex(a0);
        crate::serial_print_str(" a1=");
        crate::serial_print_hex(a1);
        crate::serial_print_str(" a2=");
        crate::serial_print_hex(a2);
        crate::serial_print_str(" a3=");
        crate::serial_print_hex(a3);
        crate::serial_print_str(" pid=");
        crate::serial_print_hex(current_pid() as usize);
        crate::serial_print_str("\n");
    }
    if SYSCALL_TRACE && current_pid() >= 3 && number != 0x16 && number != 0x65 {
        let _g = TRACE_LOCK.lock();
        crate::serial_print_str("[SC] p=");
        crate::serial_print_hex(current_pid() as usize);
        crate::serial_print_str(" nr=");
        crate::serial_print_hex(number);
        crate::serial_print_str(" a0=");
        crate::serial_print_hex(a0);
        crate::serial_print_str(" a1=");
        crate::serial_print_hex(a1);
        crate::serial_print_str(" ret=");
        crate::serial_print_hex(ret as usize);
        crate::serial_print_str("\n");
    }
    // Fire any expired POSIX timers before returning to user-space.
    tty_server::check_timers(current_pid());
    ret
}




/// Log every syscall entry (number + pid) over serial. Extremely verbose —
/// enable only while bisecting a userland bring-up failure.
const SYSCALL_TRACE: bool = false;

/// Log every syscall that fails with EINVAL (nr + args + pid). Cheap and
/// high-signal while bringing up a new ported binary.
const SYSCALL_TRACE_EINVAL: bool = false;

/// Serializes multi-part trace prints — concurrent syscalls on other CPUs
/// otherwise interleave their serial output into an unreadable shuffle.
static TRACE_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// Unsupported syscalls are logged unconditionally: they are rare, and a
/// silent ENOSYS is the single most common cause of a ported binary dying
/// with no output.
fn log_enosys(number: usize) -> isize {
    let _g = TRACE_LOCK.lock();
    crate::serial_print_str("[SYSCALL] ENOSYS nr=");
    crate::serial_print_hex(number);
    crate::serial_print_str(" pid=");
    crate::serial_print_hex(current_pid() as usize);
    crate::serial_print_str("\n");
    -38
}

fn dispatch_inner(
    number: usize,
    a0: usize, a1: usize, a2: usize,
    a3: usize, a4: usize, a5: usize,
    frame_ptr: usize,
) -> isize {
    match number {
        // ── Leandros-private IPC syscalls ───────────────────────────────────────
        SYS_IPC_SEND => sys_send(a0, a1, a2),
        SYS_IPC_RECV => sys_recv(a0, a1),
        SYS_IPC_CALL => sys_call(a0, a1, a2),
        SYS_PORT_CREATE => match port::create(current_pid()) {
            Some(p) => p as isize,
            None    => -12, // ENOMEM — port table full
        },

        // ── Device enumeration (lsblk/lspci/lsusb) ──────────────────────────────
        SYS_BLKDEV_COUNT => drivers::blkdev::device_count() as isize,
        SYS_BLKDEV_INFO  => sys_blkdev_info(a0, a1),
        SYS_PCIDEV_COUNT => drivers::pci::scan().len() as isize,
        SYS_PCIDEV_INFO  => sys_pcidev_info(a0, a1),
        SYS_USBDEV_COUNT => drivers::usb_hcd::device_count() as isize,
        SYS_USBDEV_INFO  => sys_usbdev_info(a0, a1),
        SYS_MOUNTS_COUNT => vfs::list_mounts().iter().filter(|e| e.in_use).count() as isize,
        SYS_MOUNTS_INFO  => sys_mounts_info(a0, a1),

        // ── Memory ────────────────────────────────────────────────────────────
        MMAP     => sys_mmap(a0, a1, a2, a3, a4, a5),
        MUNMAP   => sys_unmap_mem(a0, a1),
        MPROTECT => sys_mprotect(a0, a1, a2),
        BRK      => sys_brk(a0),
        MREMAP   => sys_mremap(a0, a1, a2, a3, a4),
        MINCORE  => 0, // pretend all pages are resident

        // ── Scheduling ────────────────────────────────────────────────────────
        SCHED_YIELD => { yield_now("syscall_yield"); 0 }

        // ── Process lifecycle ─────────────────────────────────────────────────
        EXIT    => { vfs_close_all_current(); exit(a0 as i32) }
        SYS_SPAWN => sys_spawn(a0, a1, a2),
        WAIT4   => sys_wait4(a0, a1, a2, a3),
        WAITID  => sys_waitid(a0, a1, a2, a3),
        GETPID  => current_pid() as isize,
        GETPPID => sys_getppid(),

        // ── exec / fork ───────────────────────────────────────────────────────
        EXECVE  => {
            let res = sys_execve(a0, a1, a2);
            if res < 0 {
                crate::serial_print_str("  [SYSCALL] sys_execve failed with error: ");
                crate::serial_print_hex(res as usize);
                crate::serial_print_str("\n");
            }
            res
        }
        CLONE   => sys_clone_or_fork(a0, a1, a2, a3, a4, frame_ptr),
        #[cfg(not(target_arch = "aarch64"))]
        FORK    => {
            // fd tables are keyed by tgid — see the identical note in
            // sys_clone_or_fork's fork arm.
            let parent_pid = sched::tgid_of(current_pid());
            // The fd table must be duplicated BEFORE the child is enqueued:
            // on SMP another CPU can run the child immediately, and its
            // first fd-allocating syscall would otherwise see an empty table.
            fork_current(frame_ptr, |child_pid| {
                let msg = make_vfs_msg(vfs::VFS_FORK_DUP,
                                       &[parent_pid as u64, child_pid as u64]);
                let _ = vfs::handle(&msg, parent_pid);
                let nmsg = make_vfs_msg(net_server::NET_FORK_DUP,
                                        &[parent_pid as u64, child_pid as u64]);
                let _ = net_server::handle(&nmsg, parent_pid);
            })
        }

        // ── Time ─────────────────────────────────────────────────────────────
        CLOCK_GETTIME => sys_clock_gettime(a0, a1),

        // ── Signals ────────────────────────────────────────────────────────
        RT_SIGACTION   => sys_rt_sigaction(a0, a1, a2),
        RT_SIGPROCMASK => sys_rt_sigprocmask(a0, a1, a2),
        RT_SIGRETURN   => sys_rt_sigreturn(frame_ptr),
        KILL           => sys_kill(a0, a1),
        RT_SIGSUSPEND  => sys_rt_sigsuspend(a0, a1),
        RT_SIGTIMEDWAIT => sys_rt_sigtimedwait(a0, a1, a2, a3),
        #[cfg(not(target_arch = "aarch64"))]
        PAUSE          => sys_rt_sigsuspend(0, 0),

        // ── Threads ────────────────────────────────────────────────────────────
        SET_TID_ADDR => sys_set_tid_address(a0),
        FUTEX        => sys_futex(a0, a1, a2, a3, a4, a5),

        // ── Architecture-specific ─────────────────────────────────────────────
        #[cfg(not(target_arch = "aarch64"))]
        ARCH_PRCTL => sys_arch_prctl(a0, a1),

        // ── I/O ───────────────────────────────────────────────────────────────
        WRITE  => sys_write(a0, a1, a2),
        READ   => sys_read(a0, a1, a2),
        WRITEV => sys_writev(a0, a1, a2),
        READV  => sys_readv(a0, a1, a2),

        // ── VFS syscalls ──────────────────────────────────────────────────────
        #[cfg(not(target_arch = "aarch64"))]
        OPEN        => sys_open(a0, a1, a2),
        OPENAT      => sys_openat(a0, a1, a2, a3),
        CLOSE       => sys_close(a0),
        FSTAT       => sys_fstat(a0, a1),
        NEWFSTATAT  => sys_newfstatat(a0, a1, a2, a3),
        LSEEK       => sys_lseek(a0, a1, a2),
        IOCTL       => sys_ioctl(a0, a1, a2),
        FCNTL       => sys_fcntl(a0, a1, a2),
        PIPE2       => sys_pipe2(a0, a1),
        FLOCK       => sys_flock(a0, a1),
        #[cfg(not(target_arch = "aarch64"))]
        PIPE        => sys_pipe2(a0, 0),
        #[cfg(not(target_arch = "aarch64"))]
        GETDENTS    => sys_getdents64(a0, a1, a2),
        #[cfg(not(target_arch = "aarch64"))]
        READLINK    => sys_readlinkat(AT_FDCWD, a0, a1, a2),
        // x86_64's legacy epoll_wait(2) syscall (232) takes a real 4th
        // arg (timeout_ms) — unlike PIPE/DUP2 just above, which hardcode a
        // 0 because those *real* legacy syscalls genuinely have no such
        // argument. Forcing 0 here made every epoll_wait() with a caller
        // timeout return instantly with "no events", so callers expecting
        // to block (e.g. mio's epoll backend, which musl's libc routes
        // through this exact syscall number on x86_64) busy-spun calling
        // it in a tight loop instead of sleeping — see
        // project_tty_isatty_and_vfork_tls.md.
        #[cfg(not(target_arch = "aarch64"))]
        EPOLL_WAIT  => sys_epoll_wait(a0, a1, a2, a3),
        GETDENTS64  => sys_getdents64(a0, a1, a2),
        DUP         => sys_dup(a0),
        DUP3        => sys_dup3(a0, a1, a2),
        #[cfg(not(target_arch = "aarch64"))]
        DUP2        => sys_dup3(a0, a1, 0),  // dup2(old,new) == dup3(old,new,0)
        READLINKAT  => sys_readlinkat(a0, a1, a2, a3),
        PPOLL       => sys_ppoll(a0, a1, a2, a3),
        #[cfg(not(target_arch = "aarch64"))]
        POLL        => sys_poll(a0, a1, a2 as isize),
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
        SETUID => if sched::set_current_uid(a0 as u32) { 0 } else { -1 }, // EPERM
        SETGID => if sched::set_current_gid(a0 as u32) { 0 } else { -1 }, // EPERM
        SETRESUID | SETRESGID | SETGROUPS => 0, // root: accept
        GETRESUID   => sys_getresxid(a0, a1, a2, false),
        GETRESGID   => sys_getresxid(a0, a1, a2, true),
        GETGROUPS   => 0,   // 0 supplementary groups
        UMASK       => sched::umask(a0 as u32) as isize,
        // Filesystem operations (writable for /tmp, read-only otherwise)
        MKDIRAT     => sys_mkdirat(a0, a1, a2),
        UNLINKAT    => sys_unlinkat(a0, a1, a2),
        RENAMEAT | RENAMEAT2 => sys_renameat(a1, a3),
        LINKAT      => sys_linkat(a0, a1, a2, a3, a4),
        SYMLINKAT   => sys_symlinkat(a0, a1, a2),
        FCHMOD      => sys_fchmod(a0, a1),
        FCHMODAT    => sys_fchmodat(a0, a1, a2, a3),
        FCHOWN      => sys_fchown(a0, a1, a2),
        FCHOWNAT    => sys_fchownat(a0, a1, a2, a3, a4),
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

        MOUNT       => sys_mount(a0, a1, a2, a3, a4),
        UMOUNT2     => sys_umount2(a0, a1),
        PIVOT_ROOT  => sys_pivot_root(a0, a1),

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
        TIMER_GETOVERRUN => sys_timer_getoverrun(a0),
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
        SIGNALFD4      => log_enosys(number),

        // ── Scheduling policy/affinity ────────────────────────────────────────
        SCHED_SETSCHEDULER | SCHED_SETPARAM => 0,
        SCHED_GETSCHEDULER => 0, // SCHED_OTHER = 0
        SCHED_GETPARAM     => sys_sched_getparam(a0, a1),
        SCHED_SETAFFINITY  => 0,
        SCHED_GETAFFINITY  => sys_sched_getaffinity(a0, a1, a2),
        SCHED_GET_PRIORITY_MAX | SCHED_GET_PRIORITY_MIN => 0,
        GETCPU             => sys_getcpu(a0, a1, a2),

        // ── Resource usage ────────────────────────────────────────────────────
        GETRUSAGE => sys_getrusage(a0, a1),

        // ── Capabilities ─────────────────────────────────────────────────────
        CAPGET => sys_capget(a0, a1),
        CAPSET => 0,

        // ── Modern Linux (stubs) ──────────────────────────────────────────────
        MEMBARRIER  => 0,
        RSEQ        => -38, // ENOSYS (musl probes and falls back silently)
        STATX       => sys_statx(a0, a1, a2, a3, a4),
        OPENAT2     => sys_openat(a0, a1, a2, a3),
        CLOSE_RANGE => sys_close_range(a0, a1, a2),
        PIDFD_OPEN  => log_enosys(number),

        // ── Credentials ───────────────────────────────────────────────────────
        GETUID  => sched::current_uid()  as isize,
        GETEUID => sched::current_euid() as isize,
        GETGID  => sched::current_gid()  as isize,
        GETEGID => sched::current_egid() as isize,
        GETTID    => current_pid() as isize,
        TGKILL    => sys_tgkill(a0, a1, a2),
        TKILL     => sys_tkill(a0, a1),

        // ── Signal helpers ────────────────────────────────────────────────────
        SIGALTSTACK => sys_sigaltstack(a0, a1, frame_ptr),

        // ── Resource limits ───────────────────────────────────────────────────
        GETRLIMIT  => sys_getrlimit(a0, a1),
        SETRLIMIT  => 0, // silently accept any limit

        // ── Old-style (non-AT) syscalls (x86-64 only) ─────────────────────────
        #[cfg(not(target_arch = "aarch64"))]
        STAT  => sys_stat_at_path(a0, a1),
        #[cfg(not(target_arch = "aarch64"))]
        // lstat(2) is stat-without-following. 0x100 is AT_SYMLINK_NOFOLLOW,
        // which routes fstatat_into at VFS_LSTAT instead of VFS_STAT. This
        // used to be a plain alias for stat, so a symlink reported its
        // target's type and `ls -l` never printed an 'l'.
        LSTAT => sys_newfstatat(AT_FDCWD, a0, a1, 0x100),

        // ── Misc ──────────────────────────────────────────────────────────────
        UNAME      => sys_uname(a0),
        PRLIMIT64  => sys_prlimit64(a0, a1, a2, a3),
        EXIT_GROUP => {
            // Linux exit_group(2) kills every thread in the calling process,
            // not just the caller. Do that for real before tearing down the
            // shared address space (owned by the thread-group leader's
            // Task) — otherwise a sibling still mid-flight on another CPU
            // (e.g. a std::thread worker that outlives main()) faults into
            // page tables that vanished under it. See sched::kill_next_group_member.
            loop {
                match sched::kill_next_group_member(a0 as i32) {
                    sched::GroupKillStep::Done => break,
                    sched::GroupKillStep::Reaped(pid) => vfs_close_all_for(pid),
                    sched::GroupKillStep::Kicking => core::hint::spin_loop(),
                }
            }
            vfs_close_all_current();
            exit(a0 as i32)
        }

        // ── File advise / range operations (advisory — safe to no-op) ────────
        POSIX_FADVISE | SYNC_FILE_RANGE | READAHEAD => 0,

        // ── inotify (no filesystem events in Leandros) ──────────────────────────
        INOTIFY_INIT1 | INOTIFY_ADD_WATCH | INOTIFY_RM_WATCH => log_enosys(number),

        _ => log_enosys(number),
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
        // Publish Blocked BEFORE looking at the queue: a sender that
        // enqueues after recv_as reports empty must already see this task
        // Blocked so its unblock_port() wake is never lost.  The old
        // check-then-block order lost the wake when the send+unblock
        // landed between the empty recv_as and block_on marking us
        // Blocked — the message sat queued while we slept forever.
        block_on_port_prepare(port_id as u32);
        match port::recv_as(port_id as u32, caller) {
            Some(msg) => {
                block_on_port_cancel();
                unsafe { core::ptr::write(msg_ptr as *mut Message, msg); }
                return 0;
            }
            None => {
                // Check whether the port still exists before blocking.
                // It may have been closed by release_by_owner between the
                // ownership check above and this point.
                if !port::is_owner(port_id as u32, caller) {
                    block_on_port_cancel();
                    return -9; // EBADF — port was closed
                }
                block_on_port_commit();
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
    const MAP_SHARED:    usize = 0x01;
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
        MMAP_BUMP.fetch_add((len + 4095) & !4095, Ordering::Relaxed)
    };

    let end = match virt.checked_add(len) {
        Some(e) => e,
        None    => return -22,
    };
    if end > USER_SPACE_END { return -22; }

    // ── Anonymous mmap ────────────────────────────────────────────────────────
    if flags & MAP_ANONYMOUS != 0 {
        let is_shared = flags & MAP_SHARED != 0;
        let mapped = with_current_address_space_mut(|as_| {
            if flags & MAP_FIXED != 0 { as_.unmap_range(virt, len); }
            as_.map_lazy(virt, len, page_flags, is_shared)
        });
        return match mapped {
            Some(true)  => virt as isize,
            Some(false) => {
                if flags & MAP_FIXED == 0 && addr != 0 {
                    let bump = MMAP_BUMP.fetch_add((len + 4095) & !4095, Ordering::Relaxed);
                    let m2 = with_current_address_space_mut(|as_| as_.map_lazy(bump, len, page_flags, is_shared));
                    match m2 { Some(true) => bump as isize, _ => -12 }
                } else { -12 }
            }
            None => -1,
        };
    }

    // ── File-backed mmap ──────────────────────────────────────────────────────
    // Strategy (mirrors the ELF loader):
    //   1. Check if the fd is a device supporting direct mmap via ioctl.
    //   2. Seek the fd to `off` in the VFS server.
    //   3. Map the virtual range eagerly (allocates contiguous physical pages).
    //   4. Obtain the physical base address of the new VMA.
    //   5. Read file data directly into physical memory (kernel identity map).
    //   6. If prot is read-only, the VMA page_flags already enforce that.
    //
    // MAP_SHARED is not supported (no VMO page cache yet); silently treat as
    // MAP_PRIVATE — data is copied on map, modifications are local only.

    let pid = current_pid();

    // Step 1: Check if the fd is a device supporting direct mmap via ioctl 0x1007
    let kind = vfs::vfs_get_node_kind(pid, fd);
    let mut phys_addr: usize = 0;
    
    if let Some(vfs::VnodeKind::DynamicDevice { port, dev_id }) = kind {
        crate::serial_print_str("[MMAP] DynamicDevice fd, off=");
        crate::serial_print_hex(off);
        crate::serial_print_str("\n");
        // This is a dynamic device, call its ioctl to get the physical address
        let mut proxy_msg = Message::empty();
        proxy_msg.tag = vfs::VFS_IOCTL as u64;
        proxy_msg.data[0..8].copy_from_slice(&(dev_id as u64).to_le_bytes());
        proxy_msg.data[8..16].copy_from_slice(&(0x1007u64).to_le_bytes()); // DRM_IOCTL_MMAP
        proxy_msg.data[16..24].copy_from_slice(&(off as u64).to_le_bytes()); // Pass requested physical address
        proxy_msg.data[24..32].copy_from_slice(&(pid as u64).to_le_bytes()); // PID

        let reply = vfs::call_port(port, proxy_msg);
        if reply.tag == 0 {
            let res = u64::from_le_bytes(reply.data[0..8].try_into().unwrap_or([0u8; 8])) as usize;
            crate::serial_print_str("[MMAP] device ioctl returned phys=");
            crate::serial_print_hex(res);
            crate::serial_print_str("\n");
            if res != 0 {
                phys_addr = res;
            }
        } else {
            crate::serial_print_str("[MMAP] device ioctl reply tag non-zero\n");
        }
    }

    if phys_addr != 0 {
        // This is a device mapping — map the physical address directly.
        let mapped = with_current_address_space_mut(|as_| {
            if flags & MAP_FIXED != 0 { as_.unmap_range(virt, len); }
            as_.map_device(virt, phys_addr, len, page_flags)
        });
        let ret = match mapped {
            Some(true)  => virt as isize,
            _           => -12, // ENOMEM
        };
        crate::serial_print_str("[MMAP] map_device virt=");
        crate::serial_print_hex(virt);
        crate::serial_print_str(" result=");
        crate::serial_print_hex(ret as usize);
        crate::serial_print_str("\n");
        return ret;
    }

    // Normal file-backed mmap follows...
    // Step 1: seek the fd to the requested offset.
    //
    // This must happen even when `off == 0`: mmap(2) reads from the file
    // *offset argument*, never from the descriptor's current position, and the
    // two routinely differ. The classic case is write-then-map — tempfile(3)
    // copies stdin into a temp file (leaving the fd at EOF) and then maps it at
    // offset 0; skipping the seek here read from EOF and produced a mapping of
    // zeroes. Nor may mmap disturb the position it found, so it is restored
    // below once the data has been copied in.
    let saved_pos = {
        let cur_msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, 0, 1 /* SEEK_CUR */]);
        vfs_reply_val(&vfs::handle(&cur_msg, pid))
    };
    {
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
    // Loop rather than issuing one big read: several VFS read paths (tmpfs and
    // the pipe ring among them) cap a single reply at 4 KiB, so a one-shot read
    // silently left everything past the first page zeroed in any mapping larger
    // than that. Short reads are normal here, not EOF.
    let hhdm_ptr = mm::phys_to_virt(phys) as *mut u8;
    let mut filled: usize = 0;
    let mut n: isize = 0;
    while filled < len {
        let read_msg = make_vfs_msg(vfs::VFS_READ,
            &[fd as u64, (hhdm_ptr as usize + filled) as u64, (len - filled) as u64]);
        let r = vfs_reply_val(&vfs::handle(&read_msg, pid));
        if r < 0 { n = r; break; }
        if r == 0 { break; } // genuine EOF; the rest of the mapping stays zero
        filled += r as usize;
    }
    if n >= 0 { n = filled as isize; }

    // Restore the descriptor's original file position — mmap(2) leaves it alone.
    if saved_pos >= 0 {
        let restore = make_vfs_msg(vfs::VFS_LSEEK,
            &[fd as u64, saved_pos as u64, 0 /* SEEK_SET */]);
        let _ = vfs_reply_val(&vfs::handle(&restore, pid));
    }

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
/// anonymous mappings which use the bump allocator), then copies the
/// overlapping `old_size` bytes into the new mapping before releasing the old
/// one, matching Linux's content-preserving semantics.
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
    let new_va = result as usize;

    // Preserve the overlapping `old_size` bytes.  Old and new mappings are
    // always in the caller's own address space, so both can be reached
    // through a single lock acquisition.
    let copied = with_current_address_space_mut(|as_| -> bool {
        as_.prefault_range(new_va, old_size);
        let mut off = 0usize;
        while off < old_size {
            let chunk = core::cmp::min(PAGE, old_size - off);
            let mut tmp = [0u8; PAGE];
            if !as_.read_user_buf(old_addr + off, &mut tmp[..chunk]) { return false; }
            if !as_.write_user_buf(new_va + off, &tmp[..chunk]) { return false; }
            off += chunk;
        }
        true
    });
    if copied != Some(true) {
        with_current_address_space_mut(|as_| as_.unmap(new_va, new_size));
        return -12; // ENOMEM
    }

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

/// Encode an internal exit code as a POSIX wait status.
///
/// `<sys/wait.h>`'s `WIFEXITED`/`WEXITSTATUS` macros expect the musl/Linux
/// layout, where a normal termination is `(code & 0xff) << 8` and the low 7
/// bits are a terminating signal (0 ⇒ exited normally). Writing the raw code
/// straight through — as this used to — makes `WIFEXITED(status)` read false
/// and `WEXITSTATUS(status)` read 0 for any exit code ≥ 128, so every caller
/// that inspects the status misreads it.
///
/// This kernel tracks only a single `i32` exit code per task with no separate
/// "terminated by signal" flag (the signal path stores the shell-convention
/// `128 + signo`), so every wait is reported as a normal exit. That is the
/// honest encoding for what is tracked; distinguishing `WIFSIGNALED` would
/// require threading a terminating-signal field through the exit path.
#[inline]
fn encode_wait_status(code: i32) -> i32 {
    (code & 0xff) << 8
}

/// True when the calling task has a deliverable signal — the condition under
/// which a blocking syscall must bail out with EINTR so the signal gets
/// delivered on the return-to-user path. Disposition-aware: ignored signals
/// never interrupt (see sched::has_deliverable_signal).
fn interrupted() -> bool {
    sched::has_deliverable_signal()
}

/// sys_wait4(pid, status_ptr, options, rusage) — full wait4(2) semantics.
///
/// pid > 0: that child; pid == -1: any child; pid == 0: caller's process
/// group; pid < -1: process group -pid. WNOHANG returns 0 when matching
/// children exist but none has terminated. rusage is accepted and ignored.
///
/// Children forked from any thread of the caller are waitable (parentage is
/// matched through the caller's tgid — see sched::wait_try).
///
/// Returns:
///   > 0           — the reaped child's pid
///   0             — WNOHANG and no terminated child yet
///   -4  (EINTR)   — interrupted by a deliverable signal (e.g. SIGCHLD)
///   -10 (ECHILD)  — no matching waitable children
///   -14 (EFAULT)  — `status_ptr` is null, misaligned, or out of range
fn sys_wait4(pid_raw: usize, status_ptr: usize, options: usize, _rusage: usize) -> isize {
    // Validate before blocking — catches bad pointers before we yield.
    if status_ptr != 0 && !validate_user_ptr_aligned(status_ptr, core::mem::size_of::<i32>(), 4) {
        return -14;
    }
    const WNOHANG: usize = 1;
    // pid_t travels as a sign-extended long; truncating through u32 maps
    // both 0xFFFF_FFFF and 0xFFFF_FFFF_FFFF_FFFF to -1.
    let pid_i = pid_raw as u32 as i32;
    let sel = if pid_i == -1 {
        sched::WaitSel::Any
    } else if pid_i == 0 {
        sched::WaitSel::Pgid(sched::current_pgid())
    } else if pid_i < -1 {
        sched::WaitSel::Pgid((-(pid_i as i64)) as u32)
    } else {
        sched::WaitSel::Pid(pid_i as u32)
    };
    let caller_tgid = sched::current_tgid();

    loop {
        match sched::wait_try(sel, caller_tgid) {
            sched::WaitTry::Reaped(pid, code) => {
                if status_ptr != 0 {
                    // Fault the destination in first: a demand-paged .bss
                    // status variable would otherwise make write_user_buf
                    // fail silently.
                    prefault_user(status_ptr, 4);
                    let status = encode_wait_status(code);
                    // Write through the address space's own virt->phys/HHDM
                    // path rather than dereferencing the raw user pointer:
                    // this syscall runs in kernel context (ring 0 / EL1) but
                    // the target page may still be a CoW-shared, PTE-read-only
                    // page belonging to the current process (e.g. its own
                    // stack right after a fork()) — a raw write would fault
                    // in a context this kernel's page-fault handlers don't
                    // attempt to recover from.
                    with_current_address_space_mut(|as_| {
                        as_.write_user_buf(status_ptr, &status.to_ne_bytes())
                    });
                }
                return pid as isize;
            }
            sched::WaitTry::NoChildren => return -10, // ECHILD
            sched::WaitTry::StillRunning => {
                if options & WNOHANG != 0 { return 0; }
                if interrupted() { return -4; } // EINTR
                irq_window();
                yield_now("wait4");
            }
        }
    }
}

/// sys_waitid(idtype, id, infop, options) — wait for a child state change.
///
/// Shares sched::wait_try with sys_wait4. With WNOHANG and no terminated
/// child, returns 0 with a zeroed siginfo (si_pid == 0), per waitid(2) —
/// brush polls exactly this shape after every SIGCHLD.
fn sys_waitid(idtype: usize, id: usize, infop: usize, options: usize) -> isize {
    const WNOHANG:    usize = 1;
    const WSTOPPED:   usize = 2;
    const WEXITED:    usize = 4;
    const WCONTINUED: usize = 8;
    // Linux requires at least one wait-state flag.
    if options & (WEXITED | WSTOPPED | WCONTINUED) == 0 { return -22; } // EINVAL
    // idtype: 0=P_ALL, 1=P_PID, 2=P_PGID
    let sel = match idtype {
        0 => sched::WaitSel::Any,
        1 => sched::WaitSel::Pid(id as u32),
        2 => sched::WaitSel::Pgid(id as u32),
        _ => return -22, // EINVAL
    };
    let caller_tgid = sched::current_tgid();
    // Without WEXITED the caller only wants stop/continue reports (e.g.
    // brush's poll_for_stopped_children uses WSTOPPED|WNOHANG) — exit
    // statuses must be left for a later wait4/WEXITED wait to collect.
    // This kernel has no stopped-task states yet, so such a wait can only
    // ever report "no state change" (or ECHILD).
    let reap_exits = options & WEXITED != 0;

    // Fill siginfo_t (si_signo=SIGCHLD at +0, si_code at +8, si_pid at +16,
    // si_status at +24). Built as a kernel-local buffer and copied out via
    // write_user_buf (virt->phys/HHDM), not a raw dereference of `infop`:
    // this syscall runs in kernel context, and the target page may be a
    // CoW-shared, PTE-read-only page the current process's own fault handler
    // never gets a chance to promote in that context.
    let write_info = |pid: u32, code: i32| {
        if infop != 0 && validate_user_buf(infop, 128) {
            let mut buf = [0u8; 128];
            if pid != 0 {
                buf[0..4].copy_from_slice(&17i32.to_ne_bytes());   // si_signo = SIGCHLD
                buf[8..12].copy_from_slice(&1i32.to_ne_bytes());   // si_code = CLD_EXITED
                buf[16..20].copy_from_slice(&pid.to_ne_bytes());   // si_pid
                buf[24..28].copy_from_slice(&code.to_ne_bytes());  // si_status
            }
            with_current_address_space_mut(|as_| as_.write_user_buf(infop, &buf));
        }
    };

    loop {
        let attempt = if reap_exits {
            sched::wait_try(sel, caller_tgid)
        } else {
            sched::wait_peek(sel, caller_tgid)
        };
        match attempt {
            sched::WaitTry::Reaped(pid, code) if reap_exits => {
                write_info(pid, code);
                return 0;
            }
            sched::WaitTry::NoChildren => return -10, // ECHILD
            _ => {
                // StillRunning, or a terminated child we must not consume.
                if options & WNOHANG != 0 {
                    write_info(0, 0); // "no state change" — zeroed si_pid
                    return 0;
                }
                if interrupted() { return -4; } // EINTR
                irq_window();
                yield_now("waitid");
            }
        }
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
        irq_window();
        yield_now("sigsuspend");
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
        irq_window();
        yield_now("sigtimedwait");
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
/// PR_GET_NAME (16): write "leandros\0" to arg2.
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
                let name = b"leandros\0\0\0\0\0\0\0\0\0\0";
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

/// sys_poll(fds_ptr, nfds, timeout_ms) — old-style poll syscall.
#[cfg(not(target_arch = "aarch64"))]
fn sys_poll(fds_ptr: usize, nfds: usize, timeout_ms: isize) -> isize {
    if nfds == 0 { return 0; }
    let sz = nfds.saturating_mul(8);
    if !validate_user_buf(fds_ptr, sz) { return -14; }

    let pid = current_pid();

    let (infinite, deadline) = if timeout_ms < 0 {
        (true, 0)
    } else {
        let ticks_needed = (timeout_ms as u64) / 10;
        (false, ticks().wrapping_add(ticks_needed))
    };

    loop {
        let mut nready = 0isize;
        for i in 0..nfds {
            let pfd = fds_ptr + i * 8;
            let fd     = unsafe { core::ptr::read(pfd       as *const i32) };
            let events = unsafe { core::ptr::read((pfd + 4) as *const i16) };

            if fd < 0 {
                unsafe { core::ptr::write((pfd + 6) as *mut i16, 0); }
                continue;
            }
            let revents = probe_fd_events(pid, fd as usize, events as u16 as u32) as i16;
            unsafe { core::ptr::write((pfd + 6) as *mut i16, revents); }
            const POLLNVAL: i16 = 0x0020;
            if revents != 0 && revents != POLLNVAL { nready += 1; }
        }
        if nready > 0 { return nready; }
        if !infinite && ticks() >= deadline { return 0; }
        if interrupted() { return -4; } // EINTR

        irq_window();

        yield_now("poll");
    }
}

/// sys_ppoll(fds_ptr, nfds, timeout_ptr, sigmask_ptr) — wait for events on fd set.
///
/// Rechecks every struct pollfd against real per-fd readiness (see
/// `probe_fd_events`) in a cooperative retry loop until at least one is
/// ready or `timeout_ptr`'s `struct timespec` elapses (NULL = block
/// indefinitely, `{0,0}` = check once and return immediately). If all fds
/// report POLLNVAL (bad fd) or no events by the deadline, returns 0.
fn sys_ppoll(fds_ptr: usize, nfds: usize, timeout_ptr: usize, _sigmask: usize) -> isize {
    // struct pollfd { fd: i32, events: i16, revents: i16 } = 8 bytes.
    const POLLNVAL: i16 = 0x0020;

    if nfds == 0 { return 0; }
    let sz = nfds.saturating_mul(8);
    if !validate_user_buf(fds_ptr, sz) { return -14; }

    let pid = current_pid();

    let (infinite, deadline) = if timeout_ptr == 0 {
        (true, 0)
    } else {
        if !validate_user_buf(timeout_ptr, 16) { return -14; }
        let tv_sec  = unsafe { core::ptr::read(timeout_ptr       as *const i64) };
        let tv_nsec = unsafe { core::ptr::read((timeout_ptr + 8) as *const i64) };
        if tv_sec < 0 || tv_nsec < 0 { return -22; } // EINVAL
        let ticks_needed = (tv_sec as u64) * 100 + (tv_nsec as u64) / 10_000_000;
        (false, ticks().wrapping_add(ticks_needed))
    };

    loop {
        let mut nready = 0isize;
        for i in 0..nfds {
            let pfd = fds_ptr + i * 8;
            let fd     = unsafe { core::ptr::read(pfd       as *const i32) };
            let events = unsafe { core::ptr::read((pfd + 4) as *const i16) };

            if fd < 0 {
                unsafe { core::ptr::write((pfd + 6) as *mut i16, 0); }
                continue;
            }
            let revents = probe_fd_events(pid, fd as usize, events as u16 as u32) as i16;
            unsafe { core::ptr::write((pfd + 6) as *mut i16, revents); }
            if revents != 0 && revents != POLLNVAL { nready += 1; }
        }
        if nready > 0 { return nready; }
        if !infinite && ticks() >= deadline { return 0; }
        if interrupted() { return -4; } // EINTR

        irq_window();

        yield_now("ppoll");
    }
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
        if interrupted() { return -4; } // EINTR (rmtp not filled — callers retry)
        irq_window();

        yield_now("nanosleep");
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

// ── Signal delivery syscalls ───────────────────────────────────────────────────

fn sys_rt_sigaction(signum: usize, act_ptr: usize, oldact_ptr: usize) -> isize {
    if signum == 0 || signum >= 64 { return -22; } // EINVAL
    sys_sigaction(signum as u32, act_ptr, oldact_ptr)
}

fn sys_rt_sigprocmask(how: usize, set_ptr: usize, oldset_ptr: usize) -> isize {
    sys_sigprocmask(how, set_ptr, oldset_ptr)
}

fn sys_rt_sigreturn(frame_ptr: usize) -> isize {
    // Restore the pre-signal user register context from the rt_sigframe on
    // the user stack, including the signal mask.
    restore_signal_frame(frame_ptr);
    // The trap-return asm stores THIS function's return value into the
    // frame's x0/rax slot AFTER dispatch returns — i.e. after the restore
    // above already rewrote the frame. Return the just-restored value so
    // that store is a no-op. Returning a literal 0 here clobbered the
    // interrupted syscall's result: an EINTR'd read() appeared to return 0
    // (EOF) to userspace instead of -EINTR, losing the whole EINTR contract
    // for any process with a real signal handler.
    if frame_ptr == 0 { return 0; }
    let uf = unsafe { &*(frame_ptr as *const sched::context::UserFrame) };
    #[cfg(target_arch = "aarch64")]
    { uf.x[0] as isize }
    #[cfg(target_arch = "x86_64")]
    { uf.rax as isize }
}

/// kill(2) with full pid-argument semantics: pid > 0 signals that process;
/// pid == 0 the caller's process group; pid < -1 the process group -pid;
/// pid == -1 every process the caller may signal (not supported → EPERM).
/// sig == 0 is the existence probe (no signal sent).
fn sys_kill(pid_raw: usize, sig_raw: usize) -> isize {
    let sig = sig_raw as u32;
    if sig >= 64 { return -22; } // EINVAL
    let pid_i = pid_raw as u32 as i32; // pid_t travels sign-extended
    if pid_i > 0 {
        if sig == 0 { return sched::exists_probe(pid_i as u32); }
        // kill(2) is process-directed: route to a thread in the target group
        // that hasn't masked `sig`, not blindly its leader.
        return sched::deliver_signal_process(sched::tgid_of(pid_i as u32), sig);
    }
    if pid_i == -1 { return -1; } // EPERM — kill-everything unsupported
    let pgid = if pid_i == 0 { sched::current_pgid() } else { (-(pid_i as i64)) as u32 };
    sched::kill_pgrp(pgid, sig)
}

fn sys_getppid() -> isize {
    current_ppid() as isize
}

// ── Thread primitives (futex, TID address, TLS base) ──────────────────────────

fn sys_set_tid_address(tidptr: usize) -> isize {
    set_clear_child_tid(tidptr);
    current_pid() as isize
}

fn sys_futex(uaddr: usize, op: usize, val: usize, timeout_ptr: usize, uaddr2: usize, val3: usize) -> isize {
    // Strip FUTEX_PRIVATE_FLAG (128) and FUTEX_CLOCK_REALTIME (256).
    const FUTEX_PRIVATE_FLAG: usize = 128;
    match op & !FUTEX_PRIVATE_FLAG {
        // FUTEX_WAIT and FUTEX_WAIT_BITSET (9): relibc's RlctMutex/condvar
        // always call the bitset form with FUTEX_BITSET_MATCH_ANY (no actual
        // bitmask filtering), so it's semantically identical to plain WAIT
        // here — the extra uaddr2/val3 args (a4/a5, unused for *_WAIT) aren't
        // even forwarded to this function.
        0 | 9 => {
            // FUTEX_WAIT: if *uaddr == val, block until woken.  The value
            // check happens inside futex_wait under the FUTEX_TABLE lock —
            // checking it out here would reopen the SMP lost-wake-up window
            // (another CPU could change the value and issue FUTEX_WAKE
            // between an early check and the waiter registration).
            if !validate_user_ptr_aligned(uaddr, 4, 4) { return -14; }
            // timeout_ptr is a `struct timespec` (relative — real FUTEX_WAIT
            // semantics; treated the same for the WAIT_BITSET/9 case, which
            // is technically absolute on real Linux, but no caller in this
            // tree relies on that distinction and treating it as relative is
            // never worse than this kernel's prior behavior of ignoring it
            // outright). Converted to a `ticks()` deadline exactly like
            // sys_nanosleep. NULL means no timeout (block indefinitely).
            let deadline = if timeout_ptr == 0 {
                None
            } else {
                if !validate_user_buf(timeout_ptr, 16) { return -14; }
                let tv_sec  = unsafe { core::ptr::read(timeout_ptr as *const i64) };
                let tv_nsec = unsafe { core::ptr::read((timeout_ptr + 8) as *const i64) };
                if tv_sec < 0 || tv_nsec < 0 || tv_nsec >= 1_000_000_000 { return -22; } // EINVAL
                let ticks_needed = (tv_sec as u64) * 100 + (tv_nsec as u64) / 10_000_000;
                Some(ticks().wrapping_add(ticks_needed))
            };
            sched::futex_wait(uaddr, val as u32, deadline)
        }
        1 => {
            // FUTEX_WAKE: wake up to `val` tasks sleeping on `uaddr`.
            sched::futex_wake(uaddr, val as u32) as isize
        }
        3 | 4 => {
            // FUTEX_REQUEUE = 3, FUTEX_CMP_REQUEUE = 4
            if !validate_user_ptr_aligned(uaddr, 4, 4) { return -14; }
            if !validate_user_ptr_aligned(uaddr2, 4, 4) { return -14; }
            if op & !FUTEX_PRIVATE_FLAG == 4 {
                let current = unsafe { core::ptr::read_volatile(uaddr as *const u32) };
                if current != val3 as u32 {
                    return -11; // EAGAIN
                }
            }
            sched::futex_requeue(uaddr, uaddr2, val as u32, timeout_ptr as u32)
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
/// Read exactly `buf.len()` bytes from `fd` (kernel destination).  Returns
/// bytes actually read (may be short at EOF); negative errno on failure.
fn read_fd_upto(fd: usize, buf: &mut [u8]) -> isize {
    let mut got = 0usize;
    while got < buf.len() {
        let r = sys_read_impl(
            fd,
            buf.as_mut_ptr().wrapping_add(got) as usize,
            buf.len() - got,
            true,
        );
        if r < 0 { return r; }
        if r == 0 { break; }
        got += r as usize;
    }
    got as isize
}

/// Cap on how far into a binary the program-header table may sit for the
/// demand-paged exec path; anything stranger falls back to the eager loader.
const EXEC_HEADER_MAX: usize = 512 * 1024;

/// Open `path` and read its ELF header + full program-header table.
///
/// Returns the still-open fd and the header bytes if `path` is a mounted
/// (disk) file containing ELF magic — the preconditions for demand-paged
/// exec.  On any failure the fd is closed and None is returned so the caller
/// can fall back to the eager whole-file loader.
fn open_exec_header(path: &str, pid: u32) -> Option<(usize, alloc::vec::Vec<u8>)> {
    // `path` is already absolute (sys_execve resolved it) and lives in kernel
    // memory, so it must bypass sys_open's user-pointer validation.
    let fd = open_kernel_path(path, 0 /* O_RDONLY */, 0);
    if fd < 0 { return None; }
    let fd = fd as usize;

    // Demand paging needs a filesystem-backed file (positional reads via the
    // mount port); anything else (ramfs, devices) uses the eager path.
    if !matches!(
        vfs::vfs_get_node_kind(pid, fd),
        Some(vfs::VnodeKind::MountedFile { .. })
    ) {
        let _ = sys_close(fd);
        return None;
    }

    let mut hdr = alloc::vec![0u8; 4096];
    let _ = sys_lseek(fd, 0, 0 /* SEEK_SET */);
    let got = read_fd_upto(fd, &mut hdr);
    if got < 64 || hdr[0..4] != [0x7F, b'E', b'L', b'F'] {
        let _ = sys_close(fd);
        return None;
    }
    hdr.truncate(got as usize);

    // Make sure the whole program-header table is in the buffer.
    let phoff     = u64::from_le_bytes(hdr[32..40].try_into().unwrap()) as usize;
    let phentsize = u16::from_le_bytes(hdr[54..56].try_into().unwrap()) as usize;
    let phnum     = u16::from_le_bytes(hdr[56..58].try_into().unwrap()) as usize;
    let needed = match phentsize.checked_mul(phnum).and_then(|n| n.checked_add(phoff)) {
        Some(n) => n,
        None    => { let _ = sys_close(fd); return None; }
    };
    if needed > EXEC_HEADER_MAX {
        let _ = sys_close(fd);
        return None;
    }
    if needed > hdr.len() {
        hdr = alloc::vec![0u8; needed];
        let _ = sys_lseek(fd, 0, 0);
        if read_fd_upto(fd, &mut hdr) != needed as isize {
            let _ = sys_close(fd);
            return None;
        }
    }
    Some((fd, hdr))
}

fn read_file_from_vfs(path: &str) -> Option<alloc::vec::Vec<u8>> {
    // Kernel-resident path — see open_kernel_path(); sys_open would reject it
    // as a bad user pointer.
    let fd = open_kernel_path(path, 0 /* O_RDONLY */, 0);
    if fd < 0 {
        return None;
    }
    let fd_usize = fd as usize;
    
    let size = sys_lseek(fd_usize, 0, 2 /* SEEK_END */);
    if size < 0 {
        let _ = sys_close(fd_usize);
        return None;
    }
    let size_usize = size as usize;
    
    let _ = sys_lseek(fd_usize, 0, 0 /* SEEK_SET */);
    
    let mut buf = alloc::vec![0u8; size_usize];
    let mut total_read = 0;
    while total_read < size_usize {
        let r = sys_read_impl(fd_usize, buf.as_mut_ptr().wrapping_add(total_read) as usize, size_usize - total_read, true);
        if r <= 0 {
            let _ = sys_close(fd_usize);
            return None;
        }
        total_read += r as usize;
    }
    
    let _ = sys_close(fd_usize);
    Some(buf)
}

fn sys_execve(path_ptr: usize, argv_ptr: usize, envp_ptr: usize) -> isize {
    let pid = current_pid();

    // Resolve against the cwd through the shared helper, so `./prog` and
    // `prog` name the same file execve sees as every other path syscall.
    let kpath = match resolve_user_path(path_ptr) { Ok(p) => p, Err(e) => return e };
    let path = match core::str::from_utf8(kpath.bytes()) {
        Ok(s) => s,
        Err(_) => return -2, // ENOENT — non-UTF-8 paths do not exist here
    };

    // ── Resolve ELF source ────────────────────────────────────────────────────
    //
    // Preferred: the demand-paged path for filesystem-backed binaries — read
    // only the ELF/program headers now, register the open file in
    // EXEC_FILES, and map every PT_LOAD as a file-backed lazy VMA whose
    // pages the fault handler reads in on first touch.  This replaces
    // reading + copying the whole image at exec time (397 MB for MAME).
    //
    // Fallback: the eager whole-file loader, for ramfs/initrd binaries and
    // for anything the header probe rejects.
    let mut exec_cap: usize = 0;
    let mut header_data: Option<alloc::vec::Vec<u8>> = None;

    if let Some((fd, hdr)) = open_exec_header(path, pid) {
        match vfs::steal_mounted_file(pid, fd) {
            Some((port, file_id)) => match exec_file_register(port, file_id) {
                Some(cap) => {
                    exec_cap = cap;
                    header_data = Some(hdr);
                }
                None => {
                    // Registry full — close the stolen mount file, fall back.
                    f2fs_server::close_by_port(port, file_id as u64);
                }
            },
            None => {
                let _ = sys_close(fd);
            }
        }
    }

    let vfs_elf_data = if exec_cap == 0 { read_file_from_vfs(path) } else { None };

    let (elf_ptr, elf_len) = if exec_cap != 0 {
        (0usize, 0usize) // unused on the demand-paged path
    } else if let Some(ref data) = vfs_elf_data {
        (data.as_ptr() as usize, data.len())
    } else if let Some((ptr, len)) = vfs::get_file_data_by_path(path) {
        (ptr as usize, len)
    } else {
        let mut initrd_data = None;
        unsafe {
            let bi_ptr = BOOT_INFO_PTR.load(Ordering::SeqCst);
            if bi_ptr != 0 {
                let boot_info = &*(bi_ptr as *const boot::BootInfo);
                if let Some(data) = init::extract_binary_from_initrd(path, boot_info) {
                    initrd_data = Some((data.as_ptr() as usize, data.len()));
                }
            }
        }
        if let Some((ptr, len)) = initrd_data {
            (ptr, len)
        } else {
            serial_print_str("[EXEC] Failed to find binary in VFS, RamFS, or initrd: ");
            serial_print_str(path);
            serial_print_str("\n");
            return -2; // ENOENT
        }
    };

    if exec_cap == 0 && elf_len == 0 { return -22; }

    // ── Collect argv / envp strings ───────────────────────────────────────────
    let mut argv = EXEC_ARGV.lock();
    let mut envp = EXEC_ENVP.lock();
    argv.reset();
    envp.reset();

    // Fault in the pointer arrays themselves (they can live in .data/.rodata
    // of a demand-paged image, not just on the stack).
    prefault_user(argv_ptr, MAX_EXEC_ARGS * core::mem::size_of::<usize>());
    prefault_user(envp_ptr, MAX_EXEC_ARGS * core::mem::size_of::<usize>());

    // Read argv[] from user-space (array of pointers, null-terminated).
    if argv_ptr != 0 {
        let mut i = 0usize;
        loop {
            if i >= MAX_EXEC_ARGS { break; }
            let ptr_addr = argv_ptr + i * core::mem::size_of::<usize>();
            if !validate_user_buf(ptr_addr, core::mem::size_of::<usize>()) { break; }

            let mut str_ptr: usize = 0;
            let ok = with_current_address_space(|as_| {
                as_.read_user_buf(ptr_addr, unsafe {
                    core::slice::from_raw_parts_mut(&mut str_ptr as *mut usize as *mut u8, core::mem::size_of::<usize>())
                })
            }).unwrap_or(false);
            if !ok || str_ptr == 0 { break; }

            // The string may live in a not-yet-faulted page of a
            // demand-paged image (e.g. argv literals in .rodata); push_cstr
            // dereferences it raw, so fault it in first.
            prefault_user(str_ptr, 512);
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

            let mut str_ptr: usize = 0;
            let ok = with_current_address_space(|as_| {
                as_.read_user_buf(ptr_addr, unsafe {
                    core::slice::from_raw_parts_mut(&mut str_ptr as *mut usize as *mut u8, core::mem::size_of::<usize>())
                })
            }).unwrap_or(false);
            if !ok || str_ptr == 0 { break; }

            prefault_user(str_ptr, 512);
            envp.push_cstr(str_ptr);
            i += 1;
        }
    }
    let argc = argv.count;
    let envc = envp.count;

    // ── Load ELF into fresh address space ─────────────────────────────────────
    let pt_root = unsafe { arch_alloc_page_table_root() };
    if pt_root == 0 {
        if exec_cap != 0 { exec_file_release(exec_cap); }
        return -12;
    }
    let mut new_as = alloc::boxed::Box::new(mm::vmm::AddressSpace::new(pt_root));

    let elf_info = if exec_cap != 0 {
        // Demand-paged: map PT_LOADs as file-backed lazy VMAs (each takes a
        // reference on exec_cap), no segment data read here.
        let r = elf::load_lazy(header_data.as_deref().unwrap_or(&[]), &mut new_as, exec_cap);
        // Drop the creation reference: the image's lifetime is now carried
        // by the VMAs (dropping new_as on the error paths below releases
        // theirs, letting the refcount reach zero and close the file).
        exec_file_release(exec_cap);
        match r {
            Ok(e)  => e,
            Err(_) => { drop(new_as); return -8; }
        }
    } else {
        let elf_bytes = unsafe { core::slice::from_raw_parts(elf_ptr as *const u8, elf_len) };
        match elf::load(elf_bytes, &mut new_as) {
            Ok(e)  => e,
            Err(_) => { drop(new_as); return -8; }
        }
    };

    // The loaders are done with the file bytes (eager: copied into the new
    // address space; lazy: only the header was ever read).  Free the buffers
    // explicitly: replace_address_space below never returns, so destructors
    // of locals still alive at that call never run, and the eager buffer is
    // the size of the whole ELF (hundreds of MB for large binaries).
    drop(vfs_elf_data);
    drop(header_data);

    // Map user stack (read+write, eager so virt_to_phys works immediately).
    let stack_flags = PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE;
    if !new_as.map(USER_STACK_TOP - USER_STACK_SIZE, USER_STACK_SIZE, stack_flags) {
        drop(new_as); return -12;
    }

    // Map the sigreturn trampoline page (read+exec) and fill in the
    // rt_sigreturn stub. Signal delivery points a handler's return address
    // here when the sigaction carries no SA_RESTORER — the Linux-aarch64
    // convention musl relies on (see sched::signal::SIGRET_TRAMPOLINE_VA).
    #[cfg(target_arch = "aarch64")]
    {
        let tramp_flags = PageFlags::PRESENT | PageFlags::USER | PageFlags::EXECUTE;
        if new_as.map(sched::signal::SIGRET_TRAMPOLINE_VA, 4096, tramp_flags) {
            // movz x8, #139 (rt_sigreturn) ; svc #0
            let stub: [u8; 8] = {
                let mut b = [0u8; 8];
                b[0..4].copy_from_slice(&0xD280_1168u32.to_le_bytes());
                b[4..8].copy_from_slice(&0xD400_0001u32.to_le_bytes());
                b
            };
            new_as.write_user_buf(sched::signal::SIGRET_TRAMPOLINE_VA, &stub);
        }
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
    //   [AT_LEANDROS_AUDIO_PORT pair]
    //   [AT_LEANDROS_NET_PORT pair]
    //   [AT_LEANDROS_VFS_PORT pair]
    //   [AT_EGID pair]
    //   [AT_GID pair]
    //   [AT_EUID pair]
    //   [AT_UID pair]
    //   [AT_PAGESZ pair]
    //   [AT_RANDOM pair]
    //   [AT_PHNUM pair]
    //   [AT_PHENT pair]
    //   [AT_PHDR pair]
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
    // auxv: PHDR + PHENT + PHNUM + RANDOM + PAGESZ + UID + EUID + GID + EGID + VFS + NET + AUDIO + NULL = 13 pairs
    let auxv_words = 13 * 2;
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
    let virt_base = mm::phys_to_virt(phys_base);

    // Write the stack frame to kernel-accessible virtual memory (HHDM).
    // Helper: write a u64 at byte offset `off` into the physical frame.
    let write64 = |off: usize, val: u64| unsafe {
        core::ptr::write((virt_base + off) as *mut u64, val);
    };
    let write8 = |off: usize, src: *const u8, len: usize| unsafe {
        core::ptr::copy_nonoverlapping(src, (virt_base + off) as *mut u8, len);
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
        (3,  elf_info.phdr_va   as u64),           // AT_PHDR
        (4,  elf_info.phentsize as u64),            // AT_PHENT
        (5,  elf_info.phnum     as u64),            // AT_PHNUM
        (25, rand_va as u64),                       // AT_RANDOM
        (6,  mm::buddy::PAGE_SIZE as u64),          // AT_PAGESZ
        (11, 0),                                    // AT_UID
        (12, 0),                                    // AT_EUID
        (13, 0),                                    // AT_GID
        (14, 0),                                    // AT_EGID
        (AT_LEANDROS_VFS_PORT, VFS_SERVER_PORT.load(Ordering::Relaxed) as u64),
        (AT_LEANDROS_NET_PORT, NET_SERVER_PORT.load(Ordering::Relaxed) as u64),
        (AT_LEANDROS_AUDIO_PORT, AUDIO_SERVER_PORT.load(Ordering::Relaxed) as u64),
        (0,  0),                                    // AT_NULL
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
    let cloexec_msg = make_vfs_msg(vfs::VFS_EXEC_CLOEXEC, &[pid as u64]);
    let _ = vfs::handle(&cloexec_msg, pid);
    let net_cloexec = make_vfs_msg(net_server::NET_EXEC_CLOEXEC, &[pid as u64]);
    let _ = net_server::handle(&net_cloexec, pid);

    // A CLONE_VFORK child stops borrowing the parent's address space here —
    // release the parent from its vfork suspension (POSIX: parent resumes on
    // the child's successful exec or exit).
    sched::vfork_complete(pid);

    replace_address_space(*new_as, pt_root, heap_start, elf_info.entry, user_sp);
}

// ── I/O syscalls ──────────────────────────────────────────────────────────────

/// A small ring of synthetic console input bytes waiting to be delivered.
///
/// Only the cursor-position report uses this today, so 32 bytes is ample; a
/// full ring drops the newest byte rather than blocking a writer.
struct ByteRing { buf: [u8; 32], head: usize, tail: usize }

impl ByteRing {
    const fn new() -> Self { Self { buf: [0; 32], head: 0, tail: 0 } }

    fn push(&mut self, b: u8) {
        let next = (self.tail + 1) % self.buf.len();
        if next != self.head {
            self.buf[self.tail] = b;
            self.tail = next;
        }
    }

    fn pop(&mut self) -> Option<u8> {
        if self.head == self.tail { return None; }
        let b = self.buf[self.head];
        self.head = (self.head + 1) % self.buf.len();
        Some(b)
    }
}

static PENDING_INPUT: spin::Mutex<ByteRing> = spin::Mutex::new(ByteRing::new());

/// True when synthetic console input is waiting to be read.
///
/// `poll`/`epoll` readiness must account for this: crossterm registers its
/// /dev/tty handle in epoll and blocks there for the cursor-position reply, so
/// a reply invisible to the readiness check leaves that wait hanging until
/// reedline's CPR timeout fires and brush drops out of interactive mode.
fn console_input_pending() -> bool {
    let q = PENDING_INPUT.lock();
    q.head != q.tail
}

/// Queue the answer to a `CSI 6 n` cursor-position report.
///
/// The framebuffer is the primary console, so the reply must describe *its*
/// cursor.  Letting the query reach the UART instead means whatever terminal
/// is attached there answers on behalf of a screen with different geometry,
/// and a line editor then repaints at a row that only makes sense on that
/// other terminal.
fn answer_cursor_position_report() {
    // Read the cursor before taking the input lock: fb_cursor_cell takes the
    // framebuffer lock internally and the two must never be held together.
    let (row, col) = drivers::framebuffer::fb_cursor_cell();

    fn push_dec(q: &mut ByteRing, mut n: usize) {
        let mut digits = [0u8; 5];
        let mut i = 0;
        if n == 0 { q.push(b'0'); return; }
        while n > 0 && i < digits.len() { digits[i] = b'0' + (n % 10) as u8; n /= 10; i += 1; }
        while i > 0 { i -= 1; q.push(digits[i]); }
    }

    let mut q = PENDING_INPUT.lock();
    q.push(0x1b);
    q.push(b'[');
    push_dec(&mut q, row);
    q.push(b';');
    push_dec(&mut q, col);
    q.push(b'R');
}

/// Console output from userspace, with `CSI 6 n` intercepted and answered
/// locally instead of forwarded to the serial line.
///
/// Everything else passes through untouched, so the serial log still shows the
/// full byte stream.  A query split across two `write` calls is not recognised
/// and falls back to the old behaviour (an attached terminal answers it).
fn console_write_user(bytes: &[u8]) {
    const CPR_QUERY: &[u8] = b"\x1b[6n";

    if bytes.len() < CPR_QUERY.len()
        || !bytes.windows(CPR_QUERY.len()).any(|w| w == CPR_QUERY)
    {
        serial_write_raw(bytes);
        return;
    }

    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(CPR_QUERY) {
            if i > start { serial_write_raw(&bytes[start..i]); }
            answer_cursor_position_report();
            i += CPR_QUERY.len();
            start = i;
        } else {
            i += 1;
        }
    }
    if start < bytes.len() { serial_write_raw(&bytes[start..]); }
}

/// Helper to read a single ASCII byte from evdev0 (unifying UART and keyboard).
fn read_input_byte() -> Option<u8> {
    static mut SHIFT_PRESSED: bool = false;
    // Synthetic input (the cursor-position reply) is delivered ahead of the
    // hardware queue so it reaches the reader in the order it was generated.
    if let Some(b) = PENDING_INPUT.lock().pop() { return Some(b); }
    loop {
        if let Some(ev) = evdev_server::pop_event(0) {
            // EV_KEY
            if ev.type_ == 1 {
                // value == 2 means this event carries a literal ASCII byte from serial
                // input (see arch/x86_64/timer.rs's on_tick), not a real keyboard
                // scancode — ASCII '6' and '*' are also 54/42, the evdev codes for
                // Right/Left Shift, so without this guard those two characters get
                // silently swallowed as shift-key state changes instead of reaching
                // the console (found via ping's destination IP getting mangled:
                // "192.168.105.1" -> "192.18.105.1").
                if (ev.code == 42 || ev.code == 54) && ev.value != 2 { // Left Shift or Right Shift
                    unsafe { SHIFT_PRESSED = ev.value != 0; }
                    continue;
                }
                
                // EV_KEY down (1) or serial typematic (2)
                if ev.value == 1 || ev.value == 2 {
                    if ev.value == 2 {
                        // Serial input: code is already a raw byte. Pass
                        // everything through — dropping control bytes here
                        // swallowed the ESC of every ANSI escape sequence, so
                        // a terminal's cursor-position report ("\x1b[n;mR")
                        // arrived as "[n;mR" and crossterm's CPR parser never
                        // matched (brush bailed out of interactive mode).
                        // Line-discipline signal bytes (^C/^Z/^\) were
                        // already intercepted at the UART drain; UTF-8 lead/
                        // continuation bytes (>= 0x80) must survive too.
                        let c = ev.code;
                        if c > 0 && c <= 255 {
                            return Some(c as u8);
                        }
                        continue;
                    }
                    let shifted = unsafe { SHIFT_PRESSED };
                    // Map standard Linux evdev scan codes back to ASCII for the kernel console
                    let ascii = match ev.code {
                        1 => 27, // ESC
                        2 => if shifted { b'!' } else { b'1' },
                        3 => if shifted { b'@' } else { b'2' },
                        4 => if shifted { b'#' } else { b'3' },
                        5 => if shifted { b'$' } else { b'4' },
                        6 => if shifted { b'%' } else { b'5' },
                        7 => if shifted { b'^' } else { b'6' },
                        8 => if shifted { b'&' } else { b'7' },
                        9 => if shifted { b'*' } else { b'8' },
                        10 => if shifted { b'(' } else { b'9' },
                        11 => if shifted { b')' } else { b'0' },
                        12 => if shifted { b'_' } else { b'-' },
                        13 => if shifted { b'+' } else { b'=' },
                        14 => 127, // Backspace
                        15 => 9,   // Tab
                        16 => if shifted { b'Q' } else { b'q' }, 
                        17 => if shifted { b'W' } else { b'w' }, 
                        18 => if shifted { b'E' } else { b'e' }, 
                        19 => if shifted { b'R' } else { b'r' }, 
                        20 => if shifted { b'T' } else { b't' },
                        21 => if shifted { b'Y' } else { b'y' }, 
                        22 => if shifted { b'U' } else { b'u' }, 
                        23 => if shifted { b'I' } else { b'i' }, 
                        24 => if shifted { b'O' } else { b'o' }, 
                        25 => if shifted { b'P' } else { b'p' },
                        26 => if shifted { b'{' } else { b'[' }, 
                        27 => if shifted { b'}' } else { b']' },
                        28 => b'\n', // Enter
                        30 => if shifted { b'A' } else { b'a' }, 
                        31 => if shifted { b'S' } else { b's' }, 
                        32 => if shifted { b'D' } else { b'd' }, 
                        33 => if shifted { b'F' } else { b'f' }, 
                        34 => if shifted { b'G' } else { b'g' },
                        35 => if shifted { b'H' } else { b'h' }, 
                        36 => if shifted { b'J' } else { b'j' }, 
                        37 => if shifted { b'K' } else { b'k' }, 
                        38 => if shifted { b'L' } else { b'l' }, 
                        39 => if shifted { b':' } else { b';' },
                        40 => if shifted { b'\"' } else { b'\'' }, 
                        41 => if shifted { b'~' } else { b'`' },
                        43 => if shifted { b'|' } else { b'\\' }, 
                        44 => if shifted { b'Z' } else { b'z' }, 
                        45 => if shifted { b'X' } else { b'x' }, 
                        46 => if shifted { b'C' } else { b'c' }, 
                        47 => if shifted { b'V' } else { b'v' },
                        48 => if shifted { b'B' } else { b'b' }, 
                        49 => if shifted { b'N' } else { b'n' }, 
                        50 => if shifted { b'M' } else { b'm' }, 
                        51 => if shifted { b'<' } else { b',' }, 
                        52 => if shifted { b'>' } else { b'.' },
                        53 => if shifted { b'?' } else { b'/' },
                        57 => b' ', // Space
                        96 => b'\n', // KPEnter
                        _ => {
                            // If it's already ASCII-range (e.g. from UART), use it as-is.
                            // Allow common control characters: Tab(9), LF(10), CR(13), BS(8/127).
                            let c = ev.code;
                            if c < 128 && (c > 31 || c == 10 || c == 13 || c == 9 || c == 127 || c == 8) {
                                c as u8
                            } else { 0 }
                        }
                    };
                    
                    if ascii != 0 {
                        return Some(ascii);
                    }
                }
            }
            // Continue loop to skip EV_SYN or other events.
        } else {
            // No events pending — report "no data yet" and let the caller
            // yield-loop.  Do NOT read the UART FIFO directly here: on SMP
            // this task may run on any CPU while CPU 0's timer tick drains
            // the same FIFO into evdev, and two concurrent consumers reorder
            // the byte stream (typing "ls" arrives as "sl").  CPU 0 polls the
            // UART every 10 ms tick, so all input flows through evdev in
            // order with at most one tick of latency.
            return None;
        }
    }
}

/// sys_write(fd, buf, count) — write bytes to a file descriptor.
///
/// fd 1/2 write directly to serial.  All other fds route through VFS.
fn sys_write(fd: usize, buf_ptr: usize, count: usize) -> isize {
    if count == 0 { return 0; }

    if !validate_user_buf(buf_ptr, count) { return -14; }
    // Fault the source in before it reaches VFS/f2fs: taking a demand-page
    // fault inside the filesystem (its lock held) would deadlock, and the
    // fd 1/2 fast path's read_user_buf would fail with EFAULT on a
    // never-touched page (e.g. a .rodata string of a demand-paged binary).
    prefault_user(buf_ptr, count);
    let pid = current_pid();
    match fd {
        // Only take the serial fast path if this fd hasn't been dup2'd to a
        // real VFS target (e.g. Command::output()'s pipe capture) — otherwise
        // the redirection is silently shadowed and the writer's data never
        // reaches the pipe. See fd_redirected's doc comment. A /dev/std*
        // proxy whose target is the raw console (a dup'd stdio fd, possibly
        // dup2'd back onto 1/2) is console output too.
        f if (matches!(f, 1 | 2) && !vfs::fd_redirected(pid, f))
            || (f < net_server::SOCK_FD_BASE && vfs::fd_is_console_stdio(pid, f)) => {
            let mut kbuf = Vec::with_capacity(count);
            unsafe { kbuf.set_len(count); }

            let ok = with_current_address_space(|as_| {
                as_.read_user_buf(buf_ptr, &mut kbuf)
            }).unwrap_or(false);

            if !ok { return -14; }

            trace_fd_route(fd, "console");
            console_write_user(kbuf.as_slice());
            count as isize
        }

        // write(2) on a socket ≡ send(fd, buf, len, 0). Without this route
        // the VFS answers EBADF for the socket fd range — tokio's signal
        // driver writes its self-pipe (a socketpair end) with plain write().
        // Blocking sockets loop on a full ring; O_NONBLOCK ones see EAGAIN.
        f if f >= net_server::SOCK_FD_BASE && f < EPOLL_FD_BASE => {
            let msg = make_vfs_msg(net_server::NET_SEND,
                &[fd as u64, buf_ptr as u64, count as u64, 0, 0, 0]);
            let nonblock = net_fd_nonblock(pid, fd);
            loop {
                let n = net_reply_val(&net_server::handle(&msg, pid));
                if n != -11 || nonblock { return n; }
                if interrupted() { return -4; } // EINTR
                irq_window();
                yield_now("sys_write_sock");
            }
        }

        _ => {
            let msg = make_vfs_msg(vfs::VFS_WRITE, &[fd as u64, buf_ptr as u64, count as u64]);
            let reply = vfs::handle(&msg, pid);
            vfs_reply_val(&reply)
        }
    }
}

/// True when `buf` ends inside an unterminated ANSI escape sequence, so the
/// console read should briefly wait for the rest before returning (see the
/// coalescing loop in `sys_read_impl`'s console branch). Recognized shapes:
/// a bare trailing ESC; CSI (`ESC [ params… final`), complete at the first
/// byte in 0x40..=0x7E after the bracket; SS3 (`ESC O`), complete after one
/// more byte. Anything else after ESC counts as complete. Sequences that
/// grow past 16 bytes are treated as malformed and released as-is.
fn console_ends_mid_escape(buf: &[u8]) -> bool {
    // Only the tail matters; a sequence longer than 16 bytes is not one we
    // should keep stalling a read for.
    let tail = &buf[buf.len().saturating_sub(16)..];
    let esc_pos = match tail.iter().rposition(|&b| b == 0x1b) {
        Some(p) => p,
        None => return false,
    };
    let after = &tail[esc_pos + 1..];
    match after.first() {
        None => true,                       // bare ESC — could be Esc key or sequence start
        Some(b'[') => {
            // CSI: terminated by the first byte in 0x40..=0x7E ('R' of a
            // cursor-position report, '~' of a keypad key, letters, …).
            !after[1..].iter().any(|&b| (0x40..=0x7e).contains(&b))
        }
        Some(b'O') => after.len() < 2,      // SS3: exactly one byte follows
        Some(_) => false,                   // two-byte sequence (ESC 7, ESC 8, …)
    }
}

/// sys_read(fd, buf, count) — read bytes from a file descriptor.
///
/// fd 0 (stdin) blocks on serial UART until at least one byte arrives,
/// unless O_NONBLOCK is set via fcntl (see `stdio_nonblocking`), in which
/// case an immediately-empty queue returns EAGAIN instead. All other fds
/// route through VFS.
fn sys_read_impl(fd: usize, buf_ptr: usize, count: usize, is_kernel: bool) -> isize {
    match fd {
        // Only take the serial fast path if fd 0 hasn't been dup2'd to a real
        // VFS target (e.g. a pipe feeding a child's stdin) — see fd_redirected's
        // doc comment and the identical guard in sys_write. Console /dev/std*
        // proxies (dup'd stdio fds) read the console too.
        f if (f == 0 && !vfs::fd_redirected(current_pid(), 0))
            || (f < net_server::SOCK_FD_BASE
                && vfs::fd_is_console_stdio(current_pid(), f)) => {
            if count == 0 { return 0; }
            if !is_kernel && !validate_user_buf(buf_ptr, count) { return -14; }
            // Edge-triggered epoll consumers (crossterm/mio's TTY reader is
            // exactly this) set O_NONBLOCK and expect a "readable" epoll
            // notification to be followed by read-until-EAGAIN, not a
            // second indefinite block — without this check, that second
            // read() call hangs forever the instant input arrives one byte
            // at a time (e.g. over a slow serial link) instead of in a
            // single burst. See project_tty_isatty_and_vfork_tls.md.
            //
            // The retry budget below (not an immediate EAGAIN) exists
            // because readiness and consumption look at different queues:
            // poll_fd_state's `serial_has_data() || evdev_server::has_events`
            // can see a raw byte sitting in the UART that the BSP-only
            // timer tick (on_tick's `serial_read_byte` drain, arch/*/timer.rs)
            // hasn't yet turned into an evdev event for `read_input_byte`
            // to pop. An immediate single check loses that race almost
            // every time right after a poll() wakeup; a few yield_now()
            // spins give the next tick (~10ms) a chance to catch up while
            // still bounding how long a "nonblocking" read can take.
            let nonblocking = !is_kernel && stdio_nonblocking(current_pid());
            const NONBLOCK_RETRY_SPINS: u32 = 32;
            let mut spins = 0u32;
            // Yield-loop until evdev has at least one key event.
            let first = loop {
                match read_input_byte() {
                    Some(b) => break b,
                    None    => {
                        if nonblocking {
                            spins += 1;
                            if spins >= NONBLOCK_RETRY_SPINS { return -11; } // EAGAIN
                        }
                        if interrupted() { return -4; } // EINTR
                        irq_window();

                        yield_now("sys_read_stdin");
                    }
                }
            };

            let mut kbuf = Vec::with_capacity(count);
            unsafe { kbuf.set_len(count); }

            kbuf[0] = first;
            let mut n = 1usize;
            // Drain any additional bytes that arrived without blocking.
            while n < count {
                match read_input_byte() {
                    Some(b) => { kbuf[n] = b; n += 1; }
                    None    => break,
                }
            }
            // ANSI escape sequences must be returned whole. Serial input
            // reaches evdev one byte per tick-drain, so a burst like a
            // terminal's cursor-position report (ESC[row;colR) can straddle
            // reads — and crossterm's parser commits a read that ends in a
            // bare ESC as the Esc KEY, after which the sequence's remaining
            // printable bytes land in the line editor as typed text. If the
            // buffer ends mid-sequence, wait (bounded) for the continuation;
            // each arriving byte refreshes the deadline, so a byte-per-tick
            // trickle still assembles. A real lone Esc keypress costs at
            // most ESC_COALESCE_TICKS (~30ms) — the same disambiguation
            // delay readline/vim use.
            const ESC_COALESCE_TICKS: u64 = 3;
            let mut deadline = ticks().wrapping_add(ESC_COALESCE_TICKS);
            while n < count && console_ends_mid_escape(&kbuf[..n]) {
                match read_input_byte() {
                    Some(b) => {
                        kbuf[n] = b; n += 1;
                        deadline = ticks().wrapping_add(ESC_COALESCE_TICKS);
                    }
                    None => {
                        if ticks() >= deadline { break; }
                        irq_window();
                        yield_now("sys_read_esc_seq");
                    }
                }
            }
            if is_kernel {
                unsafe { core::ptr::copy_nonoverlapping(kbuf.as_ptr(), buf_ptr as *mut u8, n); }
            } else {
                let ok = with_current_address_space(|as_| {
                    as_.write_user_buf(buf_ptr, &kbuf[..n])
                }).unwrap_or(false);
                if !ok { return -14; }
            }
            
            n as isize
        }
        // read(2) on a socket ≡ recv(fd, buf, len, 0) — see the matching
        // socket route in sys_write. Blocking sockets loop on an empty ring
        // (std's exec-error socketpair is CLOEXEC but NOT nonblocking — the
        // parent's read must block until the child's exec/exit closes the
        // peer); O_NONBLOCK ones (mio/tokio) see EAGAIN immediately.
        f if f >= net_server::SOCK_FD_BASE && f < EPOLL_FD_BASE => {
            if !is_kernel {
                if count != 0 && !validate_user_buf(buf_ptr, count) { return -14; }
                if count != 0 {
                    with_current_address_space_mut(|as_| as_.prefault_range(buf_ptr, count));
                }
            }
            let pid = current_pid();
            let msg = make_vfs_msg(net_server::NET_RECV,
                &[fd as u64, buf_ptr as u64, count as u64, 0, 0, 0]);
            let nonblock = net_fd_nonblock(pid, fd);
            loop {
                let n = net_reply_val(&net_server::handle(&msg, pid));
                if n != -11 || nonblock { return n; }
                if interrupted() { return -4; } // EINTR
                irq_window();
                yield_now("sys_read_sock");
            }
        }
        _ => {
            if !is_kernel {
                if count != 0 && !validate_user_buf(buf_ptr, count) { return -14; }
                // Demand-page any not-yet-faulted pages in the destination buffer
                // so the VFS can copy directly without taking a kernel-mode fault.
                if count != 0 {
                    with_current_address_space_mut(|as_| as_.prefault_range(buf_ptr, count));
                }
            }
            let pid = current_pid();
            let msg = make_vfs_msg(vfs::VFS_READ, &[fd as u64, buf_ptr as u64, count as u64]);
            // Pipe read: VFS returns -EAGAIN when write end is open but empty.
            // Block (yield-loop) until data arrives or the write end closes —
            // but only for blocking fds. O_NONBLOCK readers (e.g. an evdev
            // poll loop) must see EAGAIN, or a single read() call spins here
            // forever while the device stays empty.
            let nonblock = vfs::fd_nonblock(pid, fd);
            loop {
                let n = vfs_reply_val(&vfs::handle(&msg, pid));
                if n != -11 || nonblock { return n; }
                if interrupted() { return -4; } // EINTR
                irq_window();

                yield_now("sys_read_vfs");
            }
        }
    }
}

fn sys_read(fd: usize, buf_ptr: usize, count: usize) -> isize {
    sys_read_impl(fd, buf_ptr, count, false)
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
                prefault_user(base, len);
                let bytes = unsafe { core::slice::from_raw_parts(base as *const u8, len) };
                serial_write_raw(bytes);
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
                prefault_user(base, len);
                let msg = make_vfs_msg(vfs::VFS_READ, &[fd as u64, base as u64, len as u64]);
                // Blocking pipe: yield-loop on EAGAIN.
                let n = loop {
                    let v = vfs_reply_val(&vfs::handle(&msg, pid));
                    if v != -11 { break v; }
                    if interrupted() { break -4; } // EINTR (short read if partial)
                    irq_window();
                    yield_now("sys_readv_vfs");
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

/// sys_tkill(tid, sig) — send a signal to a specific thread (legacy form of
/// tgkill without the thread-group-id argument). Used by raise()/pthread_kill.
fn sys_tkill(tid: usize, sig: usize) -> isize {
    if sig >= 64 { return -22; } // EINVAL
    sched::deliver_signal(tid as u32, sig as u32)
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
        (b"Leandros\0",  0),    // sysname
        (b"leandros\0",  65),   // nodename
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
    bytes as isize
}

/// sys_getcpu(cpu_ptr, node_ptr, _tcache) — report CPU 0, NUMA node 0.
fn sys_getcpu(cpu_ptr: usize, node_ptr: usize, _tcache: usize) -> isize {
    if cpu_ptr != 0 {
        if !validate_user_buf(cpu_ptr, 4) { return -14; }
        unsafe { core::ptr::write_unaligned(cpu_ptr as *mut u32, 0); }
    }
    if node_ptr != 0 {
        if !validate_user_buf(node_ptr, 4) { return -14; }
        unsafe { core::ptr::write_unaligned(node_ptr as *mut u32, 0); }
    }
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
fn sys_statx(dirfd: usize, path_ptr: usize, flags: usize, _mask: usize, statxbuf: usize) -> isize {
    if !validate_user_buf(statxbuf, 256) { return -14; }
    // Zero the entire statx buffer first.
    unsafe { core::ptr::write_bytes(statxbuf as *mut u8, 0, 256); }
    // Reuse a native-sized stat buffer on the stack, fill it, then copy fields.
    let mut stat_buf = [0u8; STAT_SIZE];
    let stat_ptr = stat_buf.as_mut_ptr() as usize;
    // Forward `flags` rather than 0 so AT_EMPTY_PATH (statx's spelling of
    // fstat) reaches sys_newfstatat's descriptor branch. Path resolution
    // against the cwd happens there too.
    // stat_ptr is a kernel stack buffer — see fstatat_into's `user_dest`.
    let r = fstatat_into(dirfd, path_ptr, stat_ptr, flags, false);
    if r < 0 { return r; }
    // Map struct stat → struct statx (fields differ in layout).
    // statx: stx_mask(u32@0), stx_blksize(u32@4), stx_attributes(u64@8),
    //        stx_nlink(u32@16), stx_uid(u32@20), stx_gid(u32@24),
    //        stx_mode(u16@28), stx_ino(u64@32), stx_size(u64@40),
    //        stx_blocks(u64@48), stx_atime(i64 pair@56), stx_btime(56+16),
    //        stx_ctime(56+32), stx_mtime(56+48), stx_rdev_major(u32@104),
    //        stx_rdev_minor(u32@108), stx_dev_major(u32@112), stx_dev_minor(u32@116).
    // Source fields via the ABI-correct offsets rather than the x86-64 ones:
    // st_mode and st_nlink sit in different slots (and different widths) on
    // AArch64, and st_blksize is an `int` there, not a `long`.
    #[cfg(target_arch = "x86_64")]
    let (mode, nlink, blksize) = unsafe {
        (
            ((stat_ptr + 24) as *const u32).read_unaligned(),
            ((stat_ptr + 16) as *const u64).read_unaligned() as u32,
            ((stat_ptr + 56) as *const i64).read_unaligned() as u32,
        )
    };
    #[cfg(target_arch = "aarch64")]
    let (mode, nlink, blksize) = unsafe {
        (
            ((stat_ptr + 16) as *const u32).read_unaligned(),
            ((stat_ptr + 20) as *const u32).read_unaligned(),
            ((stat_ptr + 56) as *const i32).read_unaligned() as u32,
        )
    };
    unsafe {
        let ino    = ((stat_ptr +  8) as *const u64).read_unaligned();
        let size   = ((stat_ptr + 48) as *const i64).read_unaligned();
        let blocks = ((stat_ptr + 64) as *const i64).read_unaligned();
        // stx_mask — report all fields valid (0x7ff)
        core::ptr::write(statxbuf          as *mut u32, 0x7ff);
        // stx_blksize
        core::ptr::write((statxbuf +  4)   as *mut u32, blksize);
        // stx_nlink — the real link count, not a hardcoded 1
        core::ptr::write((statxbuf + 16)   as *mut u32, nlink);
        // stx_mode
        core::ptr::write((statxbuf + 28)   as *mut u16, mode as u16);
        // stx_ino
        core::ptr::write((statxbuf + 32)   as *mut u64, ino);
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
/// Delegates to sys_newfstatat so both entry points share one metadata
/// path. It previously did its own open()+sys_fstat(), which meant the
/// x86-64 `stat`/`lstat` syscalls fabricated S_IFREG|0644 with a zeroed
/// st_ino/st_nlink instead of consulting the owning filesystem.
#[cfg(not(target_arch = "aarch64"))]
fn sys_stat_at_path(path_ptr: usize, statbuf_ptr: usize) -> isize {
    // Legacy stat/lstat are cwd-relative by definition — AT_FDCWD is exact.
    sys_newfstatat(AT_FDCWD, path_ptr, statbuf_ptr, 0)
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

/// Issue `VFS_OPEN` for an already-resolved, NUL-terminated path.
///
/// `path_ptr` must point at memory the VFS can read by plain dereference and
/// must NOT be validated as user memory — kernel-internal callers (exec's
/// loaders) legitimately pass a kernel address, which is exactly what
/// `resolve_user_path` rejects.  Every user-facing entry point resolves first
/// and then lands here.
fn vfs_open_resolved(path_ptr: usize, flags: usize, mode: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_OPEN, &[path_ptr as u64, flags as u64, mode as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// Open a kernel-resident path string (no user-pointer validation, no cwd
/// resolution — callers inside the kernel already hold an absolute path).
///
/// This exists because `sys_open` now funnels through `resolve_user_path`,
/// whose `validate_user_buf` check rejects any address at or above the
/// user/kernel split.  Kernel callers that used to hand `sys_open` a heap
/// pointer silently started getting `-EFAULT`.
fn open_kernel_path(path: &str, flags: usize, mode: usize) -> isize {
    if path.len() >= KPATH_MAX { return -36; } // ENAMETOOLONG
    let mut buf = [0u8; KPATH_MAX + 1];
    buf[..path.len()].copy_from_slice(path.as_bytes());
    buf[path.len()] = 0;
    vfs_open_resolved(buf.as_ptr() as usize, flags, mode)
}

fn sys_open(path_ptr: usize, flags: usize, mode: usize) -> isize {
    let path = match resolve_user_path(path_ptr) { Ok(p) => p, Err(e) => return e };
    vfs_open_resolved(path.ptr(), flags, mode)
}

fn sys_openat(dirfd: usize, path_ptr: usize, flags: usize, mode: usize) -> isize {
    // dirfd is honoured only as AT_FDCWD — see resolve_at_path().
    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    vfs_open_resolved(path.ptr(), flags, mode)
}

fn sys_close(fd: usize) -> isize {
    let pid = current_pid();
    // Epoll fds sit above the socket range, so this check must come before
    // the `>= SOCK_FD_BASE` net-server routing or they never get freed.
    if fd >= EPOLL_FD_BASE && fd < EPOLL_FD_BASE + MAX_EPOLL_FDS {
        return sys_epoll_close(fd);
    }
    // Route socket fds (≥ SOCK_FD_BASE) to the net server.
    if fd >= net_server::SOCK_FD_BASE {
        let msg = make_vfs_msg(net_server::NET_CLOSE, &[fd as u64]);
        return net_reply_val(&net_server::handle(&msg, pid));
    }
    let msg = make_vfs_msg(vfs::VFS_CLOSE, &[fd as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// Real Linux `struct stat` size: 144 bytes on x86-64, but only 128 on
/// AArch64 (the generic `asm-generic/stat.h` layout musl/glibc use there
/// packs `st_mode`/`st_nlink`/`st_uid`/`st_gid` into half the space and
/// drops a padding word). Getting this wrong isn't just an ABI nit — the
/// caller's buffer (typically a `struct stat` local) is only ever as big
/// as its *own* platform's definition, so zero-filling/validating 144
/// bytes on aarch64 overruns it by 16 bytes and corrupts whatever the
/// compiler placed right after it on the stack (often the caller's saved
/// FP/LR — see the crash this fixes: a `bottom` worker thread executing a
/// stat-heavy /proc scan took an instruction-fetch fault at PC 0 because
/// `ret` popped a zeroed LR that this overrun had stomped).
/// Single source of truth lives in the VFS server alongside the writer that
/// fills the struct, so the size and the field offsets can never drift apart.
const STAT_SIZE: usize = vfs::STAT_SIZE;

/// sys_fstat(fd, statbuf_ptr) — fill struct stat for an open fd.
///
/// Populates `st_size` (offset 48) by seeking to EOF and back.
/// `st_mode` is set to S_IFREG|0644 (0x81A4) for regular files, or
/// S_IFCHR|0666 (0x21B6) for character devices (fd 0/1/2 / /dev/*).
fn sys_fstat(fd: usize, statbuf_ptr: usize) -> isize {
    fstat_into(fd, statbuf_ptr, true)
}

/// Body of `fstat`. See `fstatat_into` for why `user_dest` exists: the
/// `AT_EMPTY_PATH` branch below is reached from `sys_statx`, whose destination
/// is a kernel stack buffer.
fn fstat_into(fd: usize, statbuf_ptr: usize, user_dest: bool) -> isize {
    if user_dest {
        if !validate_user_buf(statbuf_ptr, STAT_SIZE) { return -14; }
    } else if statbuf_ptr == 0 {
        return -14;
    }
    unsafe { core::ptr::write_bytes(statbuf_ptr as *mut u8, 0, STAT_SIZE); }

    let pid = current_pid();

    // An *unredirected* fd 0/1/2 is the serial console: a character device.
    // A redirected one is whatever it was dup2'd onto, so it must fall through
    // to the VFS — a shell that redirects a child's stdout into a pipe and then
    // fstat's fd 1 has to see S_IFIFO, not S_IFCHR.
    if fd <= 2 && !vfs::fd_redirected(pid, fd) {
        // st_mode: S_IFCHR | 0666, with the console's own inode — this used to
        // report st_ino 0, which no path on the system stats to. ttyname()
        // cross-checks (st_dev, st_ino) here against stat("/dev/console") and
        // rejected the fd on the mismatch, so `tty` said "not a tty".
        vfs::write_stat_full(statbuf_ptr, 0o020666, 1, 0, vfs::CONSOLE_INO, 0, 0);
        return 0;
    }

    // The VFS owns the fd table, so it is the only thing that knows what kind
    // of object an fd names. This used to fabricate a blanket S_IFREG|0644 for
    // every fd above 2, which made a pipe end look like a regular file —
    // tokio's `pipe::Receiver::from_file` checks S_ISFIFO and refused brush's
    // command-substitution pipe with "not a pipe", so `$(...)` never ran.
    // VFS_FSTAT reports the real file type (S_IFIFO / S_IFCHR / S_IFDIR /
    // S_IFREG) and fills st_size and st_ino itself.
    let msg = make_vfs_msg(vfs::VFS_FSTAT, &[fd as u64, statbuf_ptr as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_mount(
    source_ptr: usize,
    target_ptr: usize,
    fstype_ptr: usize,
    _flags: usize,
    _data_ptr: usize,
) -> isize {
    let (source_raw, source_len) = match read_cstr_for_vfs(unsafe { core::slice::from_raw_parts(source_ptr as *const u8, 256) }) {
        Some(p) => p,
        None => return -14, // EFAULT
    };
    let source_str = match core::str::from_utf8(&source_raw[..source_len]) {
        Ok(s) => s,
        Err(_) => return -22, // EINVAL
    };

    // The mount point is a real path — resolve it against the cwd so
    // `mount /dev/vda mnt` and `mount /dev/vda /mnt` agree.
    let target_path = match resolve_user_path(target_ptr) { Ok(p) => p, Err(e) => return e };
    let target_str = match core::str::from_utf8(target_path.bytes()) {
        Ok(s) => s,
        Err(_) => return -22, // EINVAL
    };

    let (fstype_raw, fstype_len) = match read_cstr_for_vfs(unsafe { core::slice::from_raw_parts(fstype_ptr as *const u8, 256) }) {
        Some(p) => p,
        None => return -14, // EFAULT
    };
    let fstype_str = match core::str::from_utf8(&fstype_raw[..fstype_len]) {
        Ok(s) => s,
        Err(_) => return -22, // EINVAL
    };

    if fstype_str != "f2fs" {
        return -22; // EINVAL
    }

    let dev_idx = if source_str.starts_with("/dev/vd") && source_str.len() >= 8 {
        let drive_char = source_str.as_bytes()[7];
        if drive_char >= b'a' && drive_char <= b'z' {
            (drive_char - b'a') as usize
        } else {
            return -6; // ENXIO
        }
    } else if let Ok(idx) = source_str.parse::<usize>() {
        idx
    } else {
        return -22; // EINVAL
    };

    if dev_idx >= drivers::blkdev::device_count() {
        return -6; // ENXIO
    }

    if !drivers::blkdev::has_f2fs(dev_idx) {
        return -22; // EINVAL
    }

    let mount_point: &'static str = match target_str {
        "/mnt" => "/mnt",
        "/mnt/" => "/mnt",
        "/data" => "/data",
        "/home" => "/home",
        "/var" => "/var",
        _ => {
            let s = alloc::string::String::from(target_str);
            alloc::boxed::Box::leak(s.into_boxed_str())
        }
    };

    if let Some(_port) = f2fs_server::mount(dev_idx, mount_point, current_pid()) {
        0
    } else {
        -1
    }
}

fn sys_umount2(target_ptr: usize, _flags: usize) -> isize {
    let target_path = match resolve_user_path(target_ptr) { Ok(p) => p, Err(e) => return e };
    let target_str = match core::str::from_utf8(target_path.bytes()) {
        Ok(s) => s,
        Err(_) => return -22, // EINVAL
    };

    if f2fs_server::unmount(target_str) {
        0
    } else {
        -22 // EINVAL — nothing mounted there
    }
}

fn sys_pivot_root(new_root_ptr: usize, put_old_ptr: usize) -> isize {
    let pid = current_pid();
    let new_path = match resolve_user_path(new_root_ptr) { Ok(p) => p, Err(e) => return e };
    let old_path = match resolve_user_path(put_old_ptr)  { Ok(p) => p, Err(e) => return e };

    let msg = make_vfs_msg(vfs::VFS_PIVOT_ROOT, &[new_path.ptr() as u64, old_path.ptr() as u64]);
    let reply = vfs::handle(&msg, pid);
    let val = i64::from_le_bytes(reply.data[0..8].try_into().unwrap_or([0u8; 8]));
    val as isize
}

// ── Device enumeration syscalls (lsblk/lspci/lsusb) ───────────────────────────
//
// No Linux equivalent (Linux does this via ioctls on device nodes or sysfs);
// see SYS_BLKDEV_COUNT et al. Each *_info syscall fills a fixed-layout struct
// at `out_ptr` via raw offset writes, matching the sys_prlimit64 style above.

/// out layout: total_blocks:u64@0, block_size:u32@8, has_fstype:u8@12, fstype:[u8;8]@13 (25 bytes)
const BLKDEV_INFO_SIZE: usize = 24;

fn sys_blkdev_info(index: usize, out_ptr: usize) -> isize {
    if !validate_user_buf(out_ptr, BLKDEV_INFO_SIZE) { return -14; } // EFAULT
    let info = match drivers::blkdev::info(index) {
        Some(i) => i,
        None => return -19, // ENODEV
    };
    unsafe {
        core::ptr::write(out_ptr as *mut u64, info.total_blocks);
        core::ptr::write((out_ptr + 8) as *mut u32, info.block_size);
        let fstype = info.fstype.unwrap_or("");
        core::ptr::write((out_ptr + 12) as *mut u8, if fstype.is_empty() { 0 } else { 1 });
        let mut name_buf = [0u8; 8];
        let n = fstype.len().min(8);
        name_buf[..n].copy_from_slice(&fstype.as_bytes()[..n]);
        core::ptr::copy_nonoverlapping(name_buf.as_ptr(), (out_ptr + 13) as *mut u8, 8);
    }
    0
}

/// out layout: bus:u8@0, dev:u8@1, func:u8@2, vendor_id:u16@4, device_id:u16@6,
/// class:u8@8, subclass:u8@9, prog_if:u8@10 (12 bytes)
const PCIDEV_INFO_SIZE: usize = 12;

fn sys_pcidev_info(index: usize, out_ptr: usize) -> isize {
    if !validate_user_buf(out_ptr, PCIDEV_INFO_SIZE) { return -14; } // EFAULT
    let devices = drivers::pci::scan();
    let dev = match devices.get(index) {
        Some(d) => d,
        None => return -19, // ENODEV
    };
    unsafe {
        core::ptr::write(out_ptr as *mut u8, dev.bus);
        core::ptr::write((out_ptr + 1) as *mut u8, dev.dev);
        core::ptr::write((out_ptr + 2) as *mut u8, dev.func);
        core::ptr::write((out_ptr + 4) as *mut u16, dev.vendor_id);
        core::ptr::write((out_ptr + 6) as *mut u16, dev.device_id);
        core::ptr::write((out_ptr + 8) as *mut u8, dev.class);
        core::ptr::write((out_ptr + 9) as *mut u8, dev.subclass);
        core::ptr::write((out_ptr + 10) as *mut u8, dev.prog_if);
    }
    0
}

/// out layout: bus:u8@0, address:u8@1, vendor_id:u16@4, product_id:u16@6, class:u8@8 (12 bytes)
const USBDEV_INFO_SIZE: usize = 12;

fn sys_usbdev_info(index: usize, out_ptr: usize) -> isize {
    if !validate_user_buf(out_ptr, USBDEV_INFO_SIZE) { return -14; } // EFAULT
    let info = match drivers::usb_hcd::device_info(index) {
        Some(i) => i,
        None => return -19, // ENODEV
    };
    unsafe {
        core::ptr::write(out_ptr as *mut u8, info.bus);
        core::ptr::write((out_ptr + 1) as *mut u8, info.address);
        core::ptr::write((out_ptr + 4) as *mut u16, info.vendor_id);
        core::ptr::write((out_ptr + 6) as *mut u16, info.product_id);
        core::ptr::write((out_ptr + 8) as *mut u8, info.class);
    }
    0
}

/// out layout: mountpoint:[u8;32]@0, device:[u8;16]@32, fstype:[u8;8]@48 (56 bytes)
const MOUNTS_INFO_SIZE: usize = 56;

fn sys_mounts_info(index: usize, out_ptr: usize) -> isize {
    if !validate_user_buf(out_ptr, MOUNTS_INFO_SIZE) { return -14; } // EFAULT
    let mounts = vfs::list_mounts();
    let entry = match mounts.iter().filter(|e| e.in_use).nth(index) {
        Some(e) => e,
        None => return -19, // ENODEV
    };
    unsafe {
        let mut mp = [0u8; 32];
        let n = entry.prefix.len().min(32);
        mp[..n].copy_from_slice(&entry.prefix.as_bytes()[..n]);
        core::ptr::copy_nonoverlapping(mp.as_ptr(), out_ptr as *mut u8, 32);

        let mut dev = [0u8; 16];
        let n = entry.device.len().min(16);
        dev[..n].copy_from_slice(&entry.device.as_bytes()[..n]);
        core::ptr::copy_nonoverlapping(dev.as_ptr(), (out_ptr + 32) as *mut u8, 16);

        let mut fst = [0u8; 8];
        let n = entry.fstype.len().min(8);
        fst[..n].copy_from_slice(&entry.fstype.as_bytes()[..n]);
        core::ptr::copy_nonoverlapping(fst.as_ptr(), (out_ptr + 48) as *mut u8, 8);
    }
    0
}

fn sys_newfstatat(dirfd: usize, path_ptr: usize, statbuf_ptr: usize, flags: usize) -> isize {
    fstatat_into(dirfd, path_ptr, statbuf_ptr, flags, true)
}

/// Body of `fstatat`, with the destination buffer's provenance made explicit.
///
/// `user_dest == false` means `statbuf_ptr` is kernel memory, so the
/// user-range check and the prefault must be skipped. `sys_statx` fills a
/// stack `struct stat` this way; validating it as user memory rejected every
/// kernel address (kernel stacks live above the user/kernel split), so statx
/// returned `-EFAULT` unconditionally.
fn fstatat_into(
    dirfd: usize,
    path_ptr: usize,
    statbuf_ptr: usize,
    flags: usize,
    user_dest: bool,
) -> isize {
    if user_dest {
        if !validate_user_buf(statbuf_ptr, STAT_SIZE) { return -14; }
        prefault_user(statbuf_ptr, STAT_SIZE);
    } else if statbuf_ptr == 0 {
        return -14;
    }

    // fstatat(fd, "", ..., AT_EMPTY_PATH) is how several libcs spell fstat().
    // It must consult the descriptor, not the (empty, hence ENOENT) path.
    if flags & AT_EMPTY_PATH != 0 && dirfd != AT_FDCWD {
        let empty = validate_user_buf(path_ptr, 1)
            && unsafe { *(path_ptr as *const u8) } == 0;
        if path_ptr == 0 || empty { return fstat_into(dirfd, statbuf_ptr, user_dest); }
    }

    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    let path_ptr = path.ptr();
    let pid = current_pid();

    // Ask the VFS for real metadata first. VFS_STAT resolves RamFS, tmpfs,
    // device nodes and — crucially — forwards to the mounted filesystem's
    // own handler, so f2fs reports the true st_mode (0o100755 for the /bin
    // binaries), st_ino and st_nlink. The open()+sys_fstat() path below
    // cannot: sys_fstat has only an fd, so it *fabricates* S_IFREG|0644 and
    // leaves st_ino/st_nlink zero, which is precisely the 0644/0/0 that
    // `stat /bin/cat` reported on both architectures.
    // AT_SYMLINK_NOFOLLOW selects lstat semantics: the final component is left
    // unresolved, so a symlink reports S_IFLNK rather than whatever it points
    // at. `ls -l` gets its 'l' type character from exactly this, and `rm -r`
    // uses it to avoid descending through a link into a directory.
    const AT_SYMLINK_NOFOLLOW: usize = 0x100;
    let stat_tag = if flags & AT_SYMLINK_NOFOLLOW != 0 { vfs::VFS_LSTAT } else { vfs::VFS_STAT };
    let smsg = make_vfs_msg(stat_tag, &[path_ptr as u64, statbuf_ptr as u64]);
    if vfs_reply_val(&vfs::handle(&smsg, pid)) >= 0 {
        return 0;
    }

    // Fallbacks below only run when VFS_STAT could not describe the path.
    if vfs::is_directory(path_ptr) {
        // st_mode: S_IFDIR | 0755
        vfs::write_stat_full(statbuf_ptr, 0o040755, 2, 0, 0, 0, 0);
        return 0;
    }
    // Open path, use sys_fstat, then close.
    let omsg = make_vfs_msg(vfs::VFS_OPEN, &[path_ptr as u64, 0u64, 0]);
    let fd = vfs_reply_val(&vfs::handle(&omsg, pid));
    if fd < 0 { return fd; }
    let r = fstat_into(fd as usize, statbuf_ptr, user_dest);
    let cmsg = make_vfs_msg(vfs::VFS_CLOSE, &[fd as u64]);
    let _ = vfs::handle(&cmsg, pid);
    r
}

fn sys_lseek(fd: usize, offset: usize, whence: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_LSEEK, &[fd as u64, offset as u64, whence as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

// ── Hardwired console fds (0/1/2): per-pid fcntl flags ────────────────────────
// Just like isatty()/ioctl() (see servers/tty's ConsoleTermios), fcntl()
// on stdin/stdout/stderr can't route through VFS_FCNTL — VFS's alloc_fd
// deliberately never hands out 0-2 (see its own doc comment), so
// `handle_fcntl` always sees "not in_use" and returns EBADF for these
// fds. That silently breaks any nonblocking-I/O consumer that calls
// fcntl(0, F_SETFL, O_NONBLOCK) before doing edge-triggered
// epoll-driven reads (exactly what crossterm/mio does) — found while
// chasing why `bottom`'s interactive TUI never responded to input; see
// project_tty_isatty_and_vfork_tls.md. Only O_NONBLOCK is tracked: it's
// the only flag `sys_read_impl`'s fd-0 branch actually consults.
const MAX_STDIO_FLAGS_PROCS: usize = 64;
const O_NONBLOCK: u32 = 0x800;

struct StdioFlags { pid: u32, in_use: bool, flags: u32 }

static STDIO_FLAGS: spin::Mutex<[StdioFlags; MAX_STDIO_FLAGS_PROCS]> =
    spin::Mutex::new([const { StdioFlags { pid: 0, in_use: false, flags: 0 } }; MAX_STDIO_FLAGS_PROCS]);

fn stdio_nonblocking(pid: u32) -> bool {
    // Keyed by tgid — stdio flags are process-wide state, and a worker
    // thread must observe O_NONBLOCK set by the main thread (and vice versa).
    let pid = sched::tgid_of(pid);
    STDIO_FLAGS.lock().iter().any(|s| s.in_use && s.pid == pid && s.flags & O_NONBLOCK != 0)
}

/// Release `pid`'s STDIO_FLAGS slot on exit — otherwise, since slots are
/// never reused except by a matching pid, a long-running system would
/// eventually exhaust MAX_STDIO_FLAGS_PROCS after that many distinct
/// processes had ever touched fcntl() on fd 0/1/2.
fn stdio_flags_close_all(pid: u32) {
    if let Some(s) = STDIO_FLAGS.lock().iter_mut().find(|s| s.in_use && s.pid == pid) {
        *s = StdioFlags { pid: 0, in_use: false, flags: 0 };
    }
}

fn set_stdio_flags(pid: u32, flags: u32) {
    let pid = sched::tgid_of(pid); // process-wide — see stdio_nonblocking
    let mut tbl = STDIO_FLAGS.lock();
    if let Some(s) = tbl.iter_mut().find(|s| s.in_use && s.pid == pid) {
        s.flags = flags;
        return;
    }
    if let Some(s) = tbl.iter_mut().find(|s| !s.in_use) {
        *s = StdioFlags { pid, in_use: true, flags };
    }
}

/// True when a socket fd has O_NONBLOCK set (SOCK_NONBLOCK at creation or
/// fcntl F_SETFL later) — decides whether the kernel read/write loops block.
fn net_fd_nonblock(pid: u32, fd: usize) -> bool {
    let msg = make_vfs_msg(net_server::NET_GETFL, &[fd as u64]);
    let v = net_reply_val(&net_server::handle(&msg, pid));
    v >= 0 && v & 0x800 != 0
}

fn sys_fcntl(fd: usize, cmd: usize, arg: usize) -> isize {
    let pid = current_pid();
    if fd >= EPOLL_FD_BASE && fd < EPOLL_FD_BASE + MAX_EPOLL_FDS {
        return epoll_fcntl(fd, cmd);
    }
    if fd >= net_server::SOCK_FD_BASE && fd < EPOLL_FD_BASE {
        const F_DUPFD: usize = 0;
        const F_GETFL: usize = 3;
        const F_SETFL: usize = 4;
        const F_DUPFD_CLOEXEC: usize = 1030;
        return match cmd {
            F_DUPFD | F_DUPFD_CLOEXEC => {
                let msg = make_vfs_msg(net_server::NET_DUP,
                    &[fd as u64, (cmd == F_DUPFD_CLOEXEC) as u64]);
                net_reply_val(&net_server::handle(&msg, pid))
            }
            F_SETFL => {
                let msg = make_vfs_msg(net_server::NET_SETFL, &[fd as u64, arg as u64]);
                net_reply_val(&net_server::handle(&msg, pid))
            }
            F_GETFL => {
                let msg = make_vfs_msg(net_server::NET_GETFL, &[fd as u64]);
                net_reply_val(&net_server::handle(&msg, pid))
            }
            _ => 0, // F_GETFD/F_SETFD: cloexec is tracked at creation/dup
        };
    }
    if fd <= 2 {
        const F_GETFL: usize = 3;
        const F_SETFL: usize = 4;
        return match cmd {
            F_GETFL => {
                let flags = STDIO_FLAGS.lock().iter()
                    .find(|s| s.in_use && s.pid == pid)
                    .map(|s| s.flags).unwrap_or(0);
                flags as isize
            }
            F_SETFL => { set_stdio_flags(pid, arg as u32); 0 }
            _ => 0, // F_GETFD/F_SETFD/etc: no real close-on-exec semantics needed for stdio
        };
    }
    let msg = make_vfs_msg(vfs::VFS_FCNTL, &[fd as u64, cmd as u64, arg as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_fchmod(fd: usize, mode: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_FCHMOD, &[fd as u64, mode as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_fchmodat(dirfd: usize, path_ptr: usize, mode: usize, _flags: usize) -> isize {
    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_CHMOD, &[path.ptr() as u64, mode as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_fchown(fd: usize, uid: usize, gid: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_FCHOWN, &[fd as u64, uid as u64, gid as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_fchownat(dirfd: usize, path_ptr: usize, uid: usize, gid: usize, _flags: usize) -> isize {
    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_CHOWN, &[path.ptr() as u64, uid as u64, gid as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_flock(fd: usize, op: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_FLOCK, &[fd as u64, op as u64]);
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
    // dup() picks the lowest free fd — that's VFS_ALLOC_FD. (VFS_DUP2 targets a
    // specific newfd and rejects the u64::MAX "any" sentinel as out of range.)
    let msg = make_vfs_msg(vfs::VFS_ALLOC_FD, &[oldfd as u64]);
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
    prefault_user(buf_ptr, count);
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_GETDENTS64, &[fd as u64, buf_ptr as u64, count as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_mkdirat(dirfd: usize, path_ptr: usize, mode: usize) -> isize {
    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_MKDIR, &[path.ptr() as u64, mode as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_mknodat(dirfd: usize, path_ptr: usize, mode: usize, _dev: usize) -> isize {
    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };

    // Decode the requested node type from the S_IFMT bits of `mode`, exactly
    // as mknod(2)/mknodat(2) do. tmpfs can only back a plain file (S_IFREG,
    // or the type left unset — 0 — which mknod also treats as "regular") or
    // a FIFO (S_IFIFO); see the scope note on vfs::handle_mknod for what
    // "FIFO" means here. Character/block devices (S_IFCHR/S_IFBLK) and
    // sockets (S_IFSOCK) require CAP_MKNOD on Linux, which an unprivileged
    // caller never has, so return EPERM exactly like Linux does rather than
    // pretending to create a device tmpfs can't back.
    const S_IFMT:  usize = 0o170000;
    const S_IFREG: usize = 0o100000;
    const S_IFIFO: usize = 0o010000;
    let ftype = mode & S_IFMT;
    if ftype != 0 && ftype != S_IFREG && ftype != S_IFIFO {
        return -1; // EPERM
    }

    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_MKNOD, &[path.ptr() as u64, mode as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// symlinkat(target, newdirfd, linkpath) — note the argument order: the target
/// comes *first* and the dirfd applies to the link name, not to the target.
///
/// The target is copied verbatim (see `read_user_cstr_kpath`); only the link
/// name is cwd-resolved.
fn sys_symlinkat(target_ptr: usize, newdirfd: usize, linkpath_ptr: usize) -> isize {
    let target = match read_user_cstr_kpath(target_ptr) { Ok(p) => p, Err(e) => return e };
    let link   = match resolve_at_path(newdirfd, linkpath_ptr) { Ok(p) => p, Err(e) => return e };
    let msg = make_vfs_msg(vfs::VFS_SYMLINK, &[target.ptr() as u64, link.ptr() as u64]);
    vfs_reply_val(&vfs::handle(&msg, current_pid()))
}

/// linkat(olddirfd, oldpath, newdirfd, newpath, flags) — create a hard link.
///
/// `AT_SYMLINK_FOLLOW` is accepted and ignored: without it (the default, and
/// what `ln` passes) `link(2)` links the symlink itself, which is what the VFS
/// does unconditionally. Honouring the flag would need a "resolve then link"
/// round trip that nothing in the coreutils suite asks for.
fn sys_linkat(
    olddirfd: usize,
    oldpath_ptr: usize,
    newdirfd: usize,
    newpath_ptr: usize,
    _flags: usize,
) -> isize {
    let old = match resolve_at_path(olddirfd, oldpath_ptr) { Ok(p) => p, Err(e) => return e };
    let new = match resolve_at_path(newdirfd, newpath_ptr) { Ok(p) => p, Err(e) => return e };
    let msg = make_vfs_msg(vfs::VFS_LINK, &[old.ptr() as u64, new.ptr() as u64]);
    vfs_reply_val(&vfs::handle(&msg, current_pid()))
}

fn sys_unlinkat(dirfd: usize, path_ptr: usize, flags: usize) -> isize {
    const AT_REMOVEDIR: usize = 0x200;
    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    let pid = current_pid();
    let tag = if flags & AT_REMOVEDIR != 0 { vfs::VFS_RMDIR } else { vfs::VFS_UNLINK };
    let msg = make_vfs_msg(tag, &[path.ptr() as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

fn sys_chdir(path_ptr: usize) -> isize {
    let path = match resolve_user_path(path_ptr) { Ok(p) => p, Err(e) => return e };

    // Check the target exists by opening it O_RDONLY. Probe the *resolved*
    // path: re-resolving the user pointer here would repeat the work, and
    // would drift if the user buffer changed under us in between.
    let pid = current_pid();
    let omsg = make_vfs_msg(vfs::VFS_OPEN, &[path.ptr() as u64, 0u64, 0]);
    let fd = vfs_reply_val(&vfs::handle(&omsg, pid));
    if fd < 0 { return fd; }
    sys_close(fd as usize);

    sched::set_cwd(path.bytes());
    0
}

/// fchdir(fd) — make the directory `fd` names the new cwd.
///
/// This used to be a hardcoded ENOTDIR ("no directory fds yet"). Directory
/// descriptors do work — `resolve_at_path` resolves against them — so recover
/// the fd's absolute path the same way and adopt it, after confirming it
/// really is a directory (fchdir on a regular file is ENOTDIR, as on Linux).
fn sys_fchdir(fd: usize) -> isize {
    let mut base = [0u8; KPATH_MAX];
    let n = match fd_abs_path(fd, &mut base) { Some(n) => n, None => return -9 }; // EBADF

    let mut kp = KPath { buf: [0u8; KPATH_MAX + 1], len: n };
    kp.buf[..n].copy_from_slice(&base[..n]);
    kp.buf[n] = 0;

    const S_IFMT: u32 = 0o170000;
    const S_IFDIR: u32 = 0o040000;
    let mut stat_buf = [0u8; STAT_SIZE];
    let msg = make_vfs_msg(vfs::VFS_STAT, &[kp.ptr() as u64, stat_buf.as_mut_ptr() as u64]);
    if vfs_reply_val(&vfs::handle(&msg, current_pid())) < 0 { return -9; } // EBADF
    if vfs::read_stat_mode(stat_buf.as_ptr() as usize) & S_IFMT != S_IFDIR { return -20; } // ENOTDIR

    sched::set_cwd(kp.bytes());
    0
}

fn sys_getcwd(buf_ptr: usize, size: usize) -> isize {
    if !validate_user_buf(buf_ptr, size.min(1)) { return -14; }
    let mut tmp = [0u8; 256];
    let res = sched::current_cwd(tmp.as_mut_ptr(), 256);
    if res <= 0 { return -34; } // ERANGE or error
    let len = res as usize;

    // len is the number of bytes in CWD. If len >= size, it won't fit (+ NUL).
    if len >= size { return -34; } // ERANGE

    unsafe {
        core::ptr::copy_nonoverlapping(tmp.as_ptr(), buf_ptr as *mut u8, len);
        *(buf_ptr as *mut u8).add(len) = 0; // NUL terminate
    }
    (len + 1) as isize
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

/// sys_faccessat(dirfd, path, mode, flags) — permission probe.
///
/// This used to answer purely from `vfs::get_file_data` / `vfs::is_directory`,
/// both of which only ever scan the compiled-in RamFS table and tmpfs — they
/// know nothing about mounted filesystems. So every path on the f2fs root
/// (i.e. all of /bin) reported ENOENT, and any shell that resolves PATH
/// entries with `access(X_OK)` — brush does exactly that, via
/// nix::unistd::access in brush-core/src/sys/unix/fs.rs — concluded the
/// binary did not exist. That is the real "command not found" for `cat` and
/// `hello` alike, independent of the st_mode bug below.
///
/// Route through VFS_STAT instead, which resolves RamFS, tmpfs, devices and
/// mounted filesystems alike, then answer the R_OK/W_OK/X_OK question from
/// the real mode bits.
fn sys_faccessat(dirfd: usize, path_ptr: usize, mode: usize, _flags: usize) -> isize {
    let path = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    let path_ptr = path.ptr();

    const F_OK: usize = 0;
    const X_OK: usize = 1;
    const W_OK: usize = 2;
    const R_OK: usize = 4;

    let pid = current_pid();
    let mut stat_buf = [0u8; STAT_SIZE];
    let msg = make_vfs_msg(vfs::VFS_STAT, &[path_ptr as u64, stat_buf.as_mut_ptr() as u64]);
    if vfs_reply_val(&vfs::handle(&msg, pid)) < 0 {
        // Fall back to the legacy RamFS/tmpfs probe so anything VFS_STAT
        // cannot describe yet still behaves as it did before.
        return if vfs::get_file_data(path_ptr).is_some() || vfs::is_directory(path_ptr) {
            0
        } else {
            -2 // ENOENT
        };
    }

    // Existence-only probe: getting here already proves the path resolves.
    if mode == F_OK { return 0; }

    let st_mode = vfs::read_stat_mode(stat_buf.as_ptr() as usize);
    // Everything runs as root today, but X_OK must still require that *some*
    // execute bit is set — POSIX carves that case out of root's blanket
    // access, and brush relies on it to reject 0644 data files in PATH.
    let perm = st_mode & 0o777;
    if mode & X_OK != 0 && perm & 0o111 == 0 { return -13; } // EACCES
    if mode & W_OK != 0 && perm & 0o222 == 0 { return -13; }
    if mode & R_OK != 0 && perm & 0o444 == 0 { return -13; }
    0
}

fn sys_readlinkat(dirfd: usize, path_ptr: usize, buf_ptr: usize, size: usize) -> isize {
    if size == 0 || !validate_user_buf(buf_ptr, size) { return -14; }
    prefault_user(buf_ptr, size);

    // Resolve against the cwd first, so readlink("self/exe") from /proc — and
    // any other relative link probe — names the same file the rest of the
    // syscall surface would.
    let kpath = match resolve_at_path(dirfd, path_ptr) { Ok(p) => p, Err(e) => return e };
    let pb = &kpath.buf;
    let pl = kpath.len;
    let path = kpath.bytes();

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
    // "/proc/self/fd/" is 14 bytes, not 15 — the old bounds compared a
    // 15-byte slice against the 14-byte literal (never equal) and then sliced
    // the digits from index 15, dropping the first one. This branch was dead.
    if pl > 14 && &pb[..14] == b"/proc/self/fd/" {
        let num_str = &pb[14..pl];
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

    // Real symlinks. VFS_READLINK does not follow the final component (that
    // would make readlink() answer about the *target*), and returns -EINVAL
    // for a path that exists but is not a link — the distinction `ls` and
    // `readlink` both rely on. This used to be an unconditional -ENOENT, so
    // every symlink on the system read as "not there".
    let msg = make_vfs_msg(vfs::VFS_READLINK, &[kpath.ptr() as u64, buf_ptr as u64, size as u64]);
    vfs_reply_val(&vfs::handle(&msg, current_pid()))
}

/// statfs(path, buf) — per-mount figures, answered by whichever filesystem
/// owns `path`.
///
/// This used to be a stub that wrote a fixed synthetic superblock: an
/// `EXT2_SUPER_MAGIC` f_type and a 4096 f_bsize written as 32-bit words (both
/// fields are 64-bit in the asm-generic layout, so even those two landed
/// wrong), and zeros everywhere else. Zeros are what broke `df`: uutils drops
/// every filesystem reporting `f_blocks == 0` unless `-a` is given, so with a
/// correct mount table it parsed both mounts, discarded both, and printed
/// "df: no file systems processed".
///
/// The path is now resolved against the cwd like every other path syscall and
/// handed to the VFS, which forwards it to the owning mount server.
fn sys_statfs(path_ptr: usize, buf_ptr: usize) -> isize {
    if !validate_user_buf(buf_ptr, vfs::STATFS_SIZE) { return -14; }
    let path = match resolve_user_path(path_ptr) {
        Ok(p)  => p,
        Err(e) => return e,
    };
    prefault_user(buf_ptr, vfs::STATFS_SIZE);
    let msg = make_vfs_msg(vfs::VFS_STATFS, &[path.ptr() as u64, buf_ptr as u64]);
    vfs_reply_val(&vfs::handle(&msg, current_pid()))
}

/// fstatfs(fd, buf) — same answer as `statfs`, selected by open descriptor.
///
/// Previously aliased onto `sys_statfs`, which treated its first argument as a
/// path pointer — so an fd number was dereferenced as a `const char *`. It
/// only ever "worked" because the stub ignored the argument entirely.
fn sys_fstatfs(fd: usize, buf_ptr: usize) -> isize {
    if !validate_user_buf(buf_ptr, vfs::STATFS_SIZE) { return -14; }
    prefault_user(buf_ptr, vfs::STATFS_SIZE);
    let msg = make_vfs_msg(vfs::VFS_FSTATFS, &[fd as u64, buf_ptr as u64]);
    vfs_reply_val(&vfs::handle(&msg, current_pid()))
}

/// PATH_MAX for the kernel's path plumbing. Every kernel-side path buffer in
/// this file — `read_cstr_for_vfs`, `resolve_path`, `KPath` — is sized to
/// this, as is `read_cstr_raw` in the VFS server. Keep them in lockstep.
const KPATH_MAX: usize = 256;

/// `AT_FDCWD` — "interpret a relative path against the process cwd".
const AT_FDCWD: usize = -100isize as usize;
/// `AT_EMPTY_PATH` — operate on `dirfd` itself when the path is "".
const AT_EMPTY_PATH: usize = 0x1000;

/// A NUL-terminated, cwd-resolved absolute path held in kernel memory.
///
/// Handing the VFS a *kernel* pointer rather than the raw user pointer is
/// what makes cwd resolution possible at all. The VFS server and the mount
/// servers read path arguments by plain dereference of the pointer they are
/// given (`read_cstr_raw` in servers/vfs/src/lib.rs, the `ptr as *const u8`
/// loops in servers/f2fs/src/lib.rs) rather than through an address-space
/// accessor. Kernel memory is mapped in every address space, so such a
/// pointer is valid everywhere the old user pointer was — and it can carry a
/// path we rewrote. The buffer lives in the calling syscall's frame, and the
/// caller stays blocked in `call_port` for the whole round trip, so it
/// outlives every server that reads it.
struct KPath {
    buf: [u8; KPATH_MAX + 1],
    len: usize,
}

impl KPath {
    /// Pointer to pass to the VFS in place of the user `path_ptr`.
    #[inline]
    fn ptr(&self) -> usize { self.buf.as_ptr() as usize }
    #[inline]
    fn bytes(&self) -> &[u8] { &self.buf[..self.len] }
}

/// Copy the user path at `path_ptr`, resolve it against the calling task's
/// cwd when it is relative, normalise "." / ".." components, and return a
/// kernel-resident absolute path.
///
/// This is the single choke point every path-taking syscall must funnel
/// through. Before it existed, only `sys_open`, `sys_chdir`, `sys_execve` and
/// `sys_pivot_root` resolved anything — every other path syscall forwarded the
/// raw user pointer straight into a VFS message, so the VFS saw the bare
/// relative string ("a.txt"), matched neither a RamFS entry nor a mount
/// prefix, and answered ENOENT. That is why `cd /tmp; ls a.txt` failed while
/// `ls /tmp/a.txt` worked.
///
/// Errors are already errnos: `-EFAULT` for an unreadable pointer, `-ENOENT`
/// for the empty path (which is what Linux returns for `open("")`, `stat("")`
/// and friends — callers that mean "operate on the dirfd" must pass
/// `AT_EMPTY_PATH`, handled separately by `sys_newfstatat`).
fn resolve_user_path(path_ptr: usize) -> Result<KPath, isize> {
    if path_ptr == 0 || !validate_user_buf(path_ptr, 1) { return Err(-14); }
    prefault_user(path_ptr, KPATH_MAX);

    let (raw, raw_len) = match read_cstr_for_vfs(unsafe {
        core::slice::from_raw_parts(path_ptr as *const u8, KPATH_MAX)
    }) {
        Some(p) => p,
        None => return Err(-14),
    };
    // An empty path is ENOENT, never "the cwd" — resolve_path() would happily
    // hand back the cwd for it, silently turning stat("") into stat(".").
    if raw_len == 0 { return Err(-2); }

    let mut abs = [0u8; KPATH_MAX];
    let abs_len = resolve_path(&raw[..raw_len], &mut abs);
    if abs_len == 0 { return Err(-2); }

    let mut kp = KPath { buf: [0u8; KPATH_MAX + 1], len: abs_len };
    kp.buf[..abs_len].copy_from_slice(&abs[..abs_len]);
    kp.buf[abs_len] = 0; // VFS string readers scan for the NUL
    Ok(kp)
}

/// Copy a user string into kernel memory **verbatim** — no cwd resolution and
/// no "." / ".." normalisation.
///
/// `symlink(2)`'s first argument is not a path to look up; it is the link's
/// body, and it is stored byte for byte. Running it through
/// `resolve_user_path` would rewrite `ln -s ../x l` into an absolute link and
/// destroy the property that a relative target resolves against the link's own
/// directory rather than the creating process's cwd.
fn read_user_cstr_kpath(ptr: usize) -> Result<KPath, isize> {
    if ptr == 0 || !validate_user_buf(ptr, 1) { return Err(-14); }
    prefault_user(ptr, KPATH_MAX);
    let (raw, raw_len) = match read_cstr_for_vfs(unsafe {
        core::slice::from_raw_parts(ptr as *const u8, KPATH_MAX)
    }) {
        Some(p) => p,
        None    => return Err(-14),
    };
    if raw_len == 0 { return Err(-2); } // ENOENT — empty target
    let mut kp = KPath { buf: [0u8; KPATH_MAX + 1], len: raw_len };
    kp.buf[..raw_len].copy_from_slice(&raw[..raw_len]);
    kp.buf[raw_len] = 0;
    Ok(kp)
}

/// Ask the VFS for the absolute path an open descriptor was opened by, into
/// `out`. Returns the length, or `None` when the fd names nothing with a
/// filesystem path (a pipe, an eventfd, a socket, a closed fd, …).
///
/// This is the same `VFS_FD_PATH` query that answers
/// `readlink("/proc/self/fd/N")`: the VFS resolves RamFS and tmpfs entries
/// itself and forwards mounted files to the owning mount server, which
/// remembers the absolute path each `OpenFile` slot was opened by. Directory
/// descriptors are covered because a directory opens as an ordinary vnode on
/// both backends (a `TmpFile` slot with `is_dir`, or a `MountedFile`).
///
/// `out` is kernel memory, which is mapped in every address space, so the
/// pointer stays valid for the mount server that writes through it — the same
/// property `KPath` documents above.
fn fd_abs_path(fd: usize, out: &mut [u8; KPATH_MAX]) -> Option<usize> {
    // Descriptors the VFS does not own at all (sockets, epoll) would be
    // misinterpreted as VFS fd numbers — reject them up front.
    if fd >= net_server::SOCK_FD_BASE { return None; }
    let msg = make_vfs_msg(vfs::VFS_FD_PATH,
                           &[fd as u64, out.as_mut_ptr() as u64, KPATH_MAX as u64]);
    let n = vfs_reply_val(&vfs::handle(&msg, current_pid()));
    if n <= 0 { return None; }
    let n = n as usize;
    // Only a real absolute path can serve as a resolution base; the synthetic
    // names ("pipe:[7]", "eventfd", …) must not.
    if n > KPATH_MAX || out[0] != b'/' { return None; }
    Some(n)
}

/// Resolve an `*at()`-family path argument against its `dirfd`.
///
/// `AT_FDCWD` and absolute paths resolve against the process cwd exactly as
/// before. A relative path with a real `dirfd` is now joined onto the
/// directory that descriptor names, which is what POSIX requires and what the
/// openat-relative traversal idiom depends on.
///
/// This used to ignore `dirfd` outright and always resolve against the cwd.
/// That silently broke every caller that opens a directory and then operates
/// through it — the TOCTOU-safe pattern GNU fts and `uucore::safe_traversal`
/// both use. `rm /tmp/a/f` from a different cwd, for instance, opens
/// `/tmp/a`, calls `unlinkat(dirfd, "f", 0)`, and got "f" resolved against the
/// *caller's* cwd instead: ENOENT on a file that plainly exists. `du`'s
/// `fstatat(dirfd, entry_name, …)` walk failed the same way, one directory
/// level down. `ls` and `stat` were unaffected only because they pass whole
/// paths with `AT_FDCWD`.
///
/// When the descriptor has no path the VFS can name, this falls back to the
/// old cwd-relative behaviour rather than failing, so nothing that worked
/// before can start returning EBADF.
fn resolve_at_path(dirfd: usize, path_ptr: usize) -> Result<KPath, isize> {
    if dirfd == AT_FDCWD { return resolve_user_path(path_ptr); }

    if path_ptr == 0 || !validate_user_buf(path_ptr, 1) { return Err(-14); }
    prefault_user(path_ptr, KPATH_MAX);
    let (raw, raw_len) = match read_cstr_for_vfs(unsafe {
        core::slice::from_raw_parts(path_ptr as *const u8, KPATH_MAX)
    }) {
        Some(p) => p,
        None    => return Err(-14),
    };
    if raw_len == 0 { return Err(-2); }
    // An absolute path ignores the dirfd entirely, per POSIX.
    if raw[0] == b'/' { return resolve_user_path(path_ptr); }

    let mut base = [0u8; KPATH_MAX];
    let base_len = match fd_abs_path(dirfd, &mut base) {
        Some(n) => n,
        None    => return resolve_user_path(path_ptr),
    };

    // base + '/' + relative, then normalise "." / ".." the usual way.
    let mut joined = [0u8; KPATH_MAX * 2];
    let mut jl = base_len;
    joined[..base_len].copy_from_slice(&base[..base_len]);
    if joined[jl - 1] != b'/' { joined[jl] = b'/'; jl += 1; }
    if jl + raw_len >= KPATH_MAX { return Err(-36); } // ENAMETOOLONG
    joined[jl..jl + raw_len].copy_from_slice(&raw[..raw_len]);
    jl += raw_len;

    let mut abs = [0u8; KPATH_MAX];
    let abs_len = resolve_path(&joined[..jl], &mut abs);
    if abs_len == 0 { return Err(-2); }
    let mut kp = KPath { buf: [0u8; KPATH_MAX + 1], len: abs_len };
    kp.buf[..abs_len].copy_from_slice(&abs[..abs_len]);
    kp.buf[abs_len] = 0;
    Ok(kp)
}

/// Resolve a path to absolute form, handling ".." and "." components.
/// `path` — the input path (not null-terminated, just bytes).
/// `out`  — output buffer (256 bytes), written without null terminator.
/// Returns the length of the resolved path written to `out`.
fn resolve_path(path: &[u8], out: &mut [u8; 256]) -> usize {
    // path may contain a NUL terminator if it came from read_cstr_for_vfs's raw slice;
    // ensure we only process up to the first NUL.
    let path_to_process = if let Some(nul_pos) = path.iter().position(|&b| b == 0) {
        &path[..nul_pos]
    } else {
        path
    };

    let mut resolved = [0u8; 256];
    let mut res_len;

    // 1. Initialise base path (absolute vs relative).
    if !path_to_process.is_empty() && path_to_process[0] == b'/' {
        resolved[0] = b'/';
        res_len = 1;
    } else {
        // Use a local buffer to get CWD
        let mut cwd_buf = [0u8; 256];
        let cwd_len = sched::current_cwd(cwd_buf.as_mut_ptr(), 256);
        if cwd_len > 0 {
            let n = (cwd_len as usize).min(255);
            resolved[..n].copy_from_slice(&cwd_buf[..n]);
            res_len = n;
        } else {
            // Default to root if task has no CWD
            resolved[0] = b'/';
            res_len = 1;
        }
    }

    // 2. Iterate components.
    for component in path_to_process.split(|&b| b == b'/') {
        if component.is_empty() || component == b"." {
            continue;
        } else if component == b".." {
            if res_len > 1 {
                let mut last = res_len - 1;
                while last > 0 && resolved[last] != b'/' {
                    last -= 1;
                }
                res_len = if last == 0 { 1 } else { last };
            }
        } else {
            // Append with separator if not at root.
            if res_len > 1 && resolved[res_len - 1] != b'/' {
                if res_len < 255 {
                    resolved[res_len] = b'/';
                    res_len += 1;
                }
            } else if res_len == 0 {
                resolved[0] = b'/';
                res_len = 1;
            }

            let copy = component.len().min(256 - res_len);
            resolved[res_len..res_len + copy].copy_from_slice(&component[..copy]);
            res_len += copy;
        }
    }

    // 3. Finalise: default to root if empty, and strip trailing slash unless root.
    if res_len == 0 {
        resolved[0] = b'/';
        res_len = 1;
    }
    if res_len > 1 && resolved[res_len - 1] == b'/' {
        res_len -= 1;
    }

    let final_len = res_len.min(256);
    out[..final_len].copy_from_slice(&resolved[..final_len]);
    final_len
}

/// Read a cstr from user-space into a fixed buffer for VFS path lookup.
/// Returns Some((buf256, len)) or None on fault.
fn read_cstr_for_vfs(path: &[u8]) -> Option<([u8; 256], usize)> {
    if path.is_empty() { return None; }
    let mut buf = [0u8; 256];
    let mut len = 0;
    while len < 255 && len < path.len() && path[len] != 0 {
        buf[len] = path[len];
        len += 1;
    }
    Some((buf, len))
}

/// Close all FDs for the current process in VFS (called on exit).
fn vfs_close_all_current() {
    vfs_close_all_for(current_pid());
}

/// Close all FDs, sockets, TTY handles and epoll instances owned by `pid`.
///
/// Split out from `vfs_close_all_current` so `exit_group`'s forced-kill loop
/// (`EXIT_GROUP` below) can release a *sibling* thread's resources too — that
/// thread never gets to run its own `EXIT` syscall, since it's being killed
/// out from under it.
fn vfs_close_all_for(pid: u32) {
    let msg = make_vfs_msg(vfs::VFS_CLOSE_ALL, &[pid as u64]);
    let _ = vfs::handle(&msg, pid);
    // Net sockets and epoll instances are per-process (tgid-keyed), so only
    // a thread-group leader's exit tears them down — a plain pthread exiting
    // must not close the sockets its siblings still use.
    if sched::tgid_of(pid) == pid {
        let nmsg = make_vfs_msg(net_server::NET_CLOSE_ALL, &[pid as u64]);
        let _ = net_server::handle(&nmsg, pid);
        epoll_close_all(pid);
    }
    tty_server::close_all(pid);
    stdio_flags_close_all(pid);
}

/// sys_ioctl — try VFS first (FIONREAD on pipes/files), then TTY server.
fn sys_ioctl(fd: usize, cmd: usize, arg: usize) -> isize {
    let pid = current_pid();
    const FIONREAD: usize = 0x541B;
    const FBIOGET_VSCREENINFO: usize = 0x4600;
    const ENOTTY: isize = -25;

    // Console proxies (/dev/tty, dup'd stdio fds) answer terminal ioctls
    // exactly like the console fd they alias — crossterm probes TIOCGWINSZ
    // and termios on its /dev/tty handle.
    let fd = if fd > 2 && fd < net_server::SOCK_FD_BASE
        && vfs::fd_is_console_stdio(pid, fd) { 0 } else { fd };

    if cmd == FIONREAD && fd == 0 {
        if arg == 0 || !validate_user_buf(arg, 4) { return -14; }
        let has_data = crate::serial_has_data();
        unsafe { (arg as *mut i32).write(if has_data { 1 } else { 0 }) };
        return 0;
    }

    // FIONBIO — std's `Pipe::set_nonblocking` (used by `Command::output()`'s
    // internal read loop, which crossterm's `tput` terminal-size fallback
    // goes through) issues this instead of fcntl(F_SETFL). Previously any
    // fd outside the TTY server's own ranges fell straight through to
    // tty_server::handle_ioctl and got ENOTTY, which std unconditionally
    // unwraps deep in read_output() — a libstd invariant violation that
    // panicked the whole process. Route it through the same F_GETFL/F_SETFL
    // path plain fcntl() uses, scoped to ordinary VFS fds (pipes/files);
    // net/tty fds keep their existing (or future) FIONBIO handling.
    const FIONBIO: usize = 0x5421;
    if cmd == FIONBIO && fd < net_server::SOCK_FD_BASE {
        if arg == 0 || !validate_user_buf(arg, 4) { return -14; }
        const F_GETFL: usize = 3;
        const F_SETFL: usize = 4;
        const O_NONBLOCK: usize = 0x800;
        let nonblocking = unsafe { (arg as *const i32).read() } != 0;
        let get_msg = make_vfs_msg(vfs::VFS_FCNTL, &[fd as u64, F_GETFL as u64, 0]);
        let cur = vfs_reply_val(&vfs::handle(&get_msg, pid));
        if cur < 0 { return cur; }
        let flags = if nonblocking { cur as usize | O_NONBLOCK } else { cur as usize & !O_NONBLOCK };
        let set_msg = make_vfs_msg(vfs::VFS_FCNTL, &[fd as u64, F_SETFL as u64, flags as u64]);
        return vfs_reply_val(&vfs::handle(&set_msg, pid));
    }

    // DRM ioctl commands
    const DRM_IOCTL_GET_MODE: usize = 0x1003;
    const DRM_IOCTL_SET_MODE: usize = 0x1001;
    const DRM_IOCTL_CREATE_FB: usize = 0x1002;
    const DRM_IOCTL_FLIP_PAGE: usize = 0x1004;
    const DRM_IOCTL_SET_PLANE: usize = 0x1005;
    const DRM_IOCTL_GET_CAPS: usize = 0x1006;

    // Check if it's a standard Linux EVDEV (type 'E' = 0x45) or DRM (type 'd' = 0x64) ioctl
    let ioctl_type = (cmd >> 8) & 0xFF;
    let is_evdev = ioctl_type == 0x45;
    let is_drm = ioctl_type == 0x64;

    if cmd == FIONREAD || cmd == FBIOGET_VSCREENINFO ||
       cmd == DRM_IOCTL_GET_MODE || cmd == DRM_IOCTL_SET_MODE ||
       cmd == DRM_IOCTL_CREATE_FB || cmd == DRM_IOCTL_FLIP_PAGE ||
       cmd == DRM_IOCTL_SET_PLANE || cmd == DRM_IOCTL_GET_CAPS ||
       is_evdev || is_drm {
        
        let msg = make_vfs_msg(vfs::VFS_IOCTL, &[fd as u64, cmd as u64, arg as u64]);
        let reply = vfs::handle(&msg, pid);
        return u64::from_le_bytes(reply.data[0..8].try_into().unwrap_or([0u8; 8])) as isize;
    }
    let msg = make_vfs_msg(tty_server::TTY_IOCTL, &[fd as u64, cmd as u64, arg as u64]);
    net_reply_val(&tty_server::handle(&msg, pid))
}

// ── POSIX timer syscalls ──────────────────────────────────────────────────────

/// sys_timer_create(clockid, sigevent_ptr, timerid_ptr)
fn sys_timer_create(_clockid: usize, sigevent_ptr: usize, timerid_ptr: usize) -> isize {
    // struct sigevent: sigev_value(8) + sigev_signo(4) + sigev_notify(4) + ...
    // We only care about sigev_signo at offset 8 (SIGEV_SIGNAL = 0).
    if timerid_ptr != 0 && !validate_user_buf(timerid_ptr, core::mem::size_of::<usize>()) { return -14; }
    let signo = if sigevent_ptr != 0 && validate_user_buf(sigevent_ptr, 12) {
        let mut buf = [0u8; 12];
        let ok = with_current_address_space(|as_| as_.read_user_buf(sigevent_ptr, &mut buf))
            .unwrap_or(false);
        if ok { u32::from_ne_bytes(buf[8..12].try_into().unwrap()) } else { 14 }
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
    if ospec_ptr != 0 && !validate_user_buf(ospec_ptr, 32) { return -14; }
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

/// sys_timer_getoverrun(timerid) — number of extra expirations since the
/// timer's signal was last checked (reset to 0 on each call).
fn sys_timer_getoverrun(timerid: usize) -> isize {
    let pid = current_pid();
    let msg = make_vfs_msg(tty_server::TIMER_GETOVERRUN, &[timerid as u64]);
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

/// Shared blocking wrapper for the four send/recv syscalls: a blocking
/// socket loops on EAGAIN (EINTR-aware), a nonblocking one — O_NONBLOCK on
/// the fd or MSG_DONTWAIT in `flags` — returns it straight through.
fn net_blocking_op(pid: u32, sockfd: usize, flags: usize, msg: &Message) -> isize {
    const MSG_DONTWAIT: usize = 0x40;
    let nonblock = flags & MSG_DONTWAIT != 0 || net_fd_nonblock(pid, sockfd);
    loop {
        let n = net_reply_val(&net_server::handle(msg, pid));
        if n != -11 || nonblock { return n; }
        if interrupted() { return -4; } // EINTR
        irq_window();
        yield_now("net_blocking_op");
    }
}

fn sys_sendto(sockfd: usize, buf_ptr: usize, len: usize,
              flags: usize, addr_ptr: usize, addrlen: usize) -> isize {
    if len != 0 && !validate_user_buf(buf_ptr, len) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_SEND,
        &[sockfd as u64, buf_ptr as u64, len as u64,
          flags as u64, addr_ptr as u64, addrlen as u64]);
    net_blocking_op(pid, sockfd, flags, &msg)
}

fn sys_recvfrom(sockfd: usize, buf_ptr: usize, len: usize,
                flags: usize, addr_ptr: usize, addrlen_ptr: usize) -> isize {
    if len != 0 && !validate_user_buf(buf_ptr, len) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_RECV,
        &[sockfd as u64, buf_ptr as u64, len as u64,
          flags as u64, addr_ptr as u64, addrlen_ptr as u64]);
    net_blocking_op(pid, sockfd, flags, &msg)
}

fn sys_sendmsg(sockfd: usize, msghdr_ptr: usize, flags: usize) -> isize {
    if !validate_user_buf(msghdr_ptr, 48) { return -14; } // sizeof(msghdr)≥48
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_SENDMSG,
        &[sockfd as u64, msghdr_ptr as u64, flags as u64]);
    net_blocking_op(pid, sockfd, flags, &msg)
}

fn sys_recvmsg(sockfd: usize, msghdr_ptr: usize, flags: usize) -> isize {
    if !validate_user_buf(msghdr_ptr, 48) { return -14; }
    let pid = current_pid();
    let msg = make_vfs_msg(net_server::NET_RECVMSG,
        &[sockfd as u64, msghdr_ptr as u64, flags as u64]);
    net_blocking_op(pid, sockfd, flags, &msg)
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
// Readiness for every fd is queried for real (see `probe_fd_events` /
// `poll_fd_state`) by asking the owning server (VFS or net) about its actual
// ring/counter state — never assumed. `epoll_wait`/`ppoll`/`select` each run
// a cooperative check-then-yield loop (mirroring `sys_read`'s pipe-EAGAIN
// loop and `sys_nanosleep`'s deadline loop elsewhere in this file) until
// something is ready or their timeout elapses, so a real blocking multi-fd
// wait no longer busy-returns "not ready" on the very first check.

const MAX_EPOLL_INSTANCES: usize = 16;
const MAX_EPOLL_INTERESTS: usize = 32;

#[derive(Clone, Copy)]
struct EpollInterest {
    fd:     i32,
    events: u32,
    data:   u64,
    in_use: bool,
    /// Edge-trigger bookkeeping: the VFS object event-seq (see PipeRing::seq /
    /// EVENTFD_SEQ) last delivered for this interest. An EPOLLET fd re-fires
    /// only when its seq advances past this, so a permanently-level-ready fd
    /// (pipe at EOF, mio's never-drained eventfd waker) can't pin tokio's
    /// reactor in a 0-timeout epoll spin, yet no genuine edge is ever dropped.
    /// u64::MAX = never delivered, so the first readiness always fires.
    last_seq: u64,
}

impl EpollInterest {
    const fn empty() -> Self { Self { fd: -1, events: 0, data: 0, in_use: false, last_seq: u64::MAX } }
}

#[derive(Clone, Copy)]
struct EpollInstance {
    owner_pid: u32,
    interests: [EpollInterest; MAX_EPOLL_INTERESTS],
    in_use:    bool,
    /// Number of epoll fds (EPOLL_FDS entries) referencing this instance.
    /// fcntl(F_DUPFD*) on an epoll fd creates a second fd aliasing the same
    /// instance (mio/tokio clone their registry handle this way); the
    /// instance is only torn down when the last alias closes.
    refs:      u32,
}

impl EpollInstance {
    const fn empty() -> Self {
        Self { owner_pid: 0, interests: [const { EpollInterest::empty() }; MAX_EPOLL_INTERESTS],
               in_use: false, refs: 0 }
    }
}

/// Epoll fd numbers are an indirection over instance slots so that two fds
/// can alias one instance (dup semantics). fd = EPOLL_FD_BASE + entry index.
const MAX_EPOLL_FDS: usize = 32;

#[derive(Clone, Copy)]
struct EpollFdEntry { in_use: bool, slot: u8 }

static EPOLL_FDS: spin::Mutex<[EpollFdEntry; MAX_EPOLL_FDS]> =
    spin::Mutex::new([EpollFdEntry { in_use: false, slot: 0 }; MAX_EPOLL_FDS]);

/// Resolve an epoll fd to its instance slot, or None if out of range/closed.
fn epoll_slot_of(epfd: usize) -> Option<usize> {
    if !(EPOLL_FD_BASE..EPOLL_FD_BASE + MAX_EPOLL_FDS).contains(&epfd) { return None; }
    let t = EPOLL_FDS.lock();
    let e = t[epfd - EPOLL_FD_BASE];
    if e.in_use { Some(e.slot as usize) } else { None }
}

/// fcntl on an epoll fd. Supports the dup commands mio/tokio actually use;
/// flag commands are accepted as no-ops (epoll fds carry no meaningful
/// status flags here).
fn epoll_fcntl(epfd: usize, cmd: usize) -> isize {
    const F_DUPFD: usize = 0;
    const F_DUPFD_CLOEXEC: usize = 1030;
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let slot = match epoll_slot_of(epfd) { Some(s) => s, None => return -9 };
            let mut ep = EPOLL_INSTANCES.lock();
            if !ep[slot].in_use || sched::tgid_of(ep[slot].owner_pid) != sched::current_tgid() {
                return -9;
            }
            let mut t = EPOLL_FDS.lock();
            match t.iter().position(|e| !e.in_use) {
                Some(i) => {
                    t[i] = EpollFdEntry { in_use: true, slot: slot as u8 };
                    ep[slot].refs += 1;
                    (EPOLL_FD_BASE + i) as isize
                }
                None => -24, // EMFILE
            }
        }
        _ => 0, // F_GETFD/F_SETFD/F_GETFL/F_SETFL
    }
}

/// FD base for epoll instances — must not overlap VFS/TTY/net ranges.
const EPOLL_FD_BASE: usize = 0x400;

/// `struct epoll_event` on-the-wire layout: real Linux packs this to 12
/// bytes (`data` at offset 4) on x86_64 only (`EPOLL_PACKED` in glibc's
/// `bits/epoll.h`); every other architecture, aarch64 included, uses the
/// natural 16-byte layout (`data` at offset 8). Must match
/// `userland/relibc/src/header/sys_epoll/mod.rs`'s `epoll_event` exactly —
/// see that struct's doc comment.
#[cfg(target_arch = "x86_64")]
const EPOLL_EVENT_SIZE: usize = 12;
#[cfg(target_arch = "x86_64")]
const EPOLL_EVENT_DATA_OFF: usize = 4;
#[cfg(not(target_arch = "x86_64"))]
const EPOLL_EVENT_SIZE: usize = 16;
#[cfg(not(target_arch = "x86_64"))]
const EPOLL_EVENT_DATA_OFF: usize = 8;

static EPOLL_INSTANCES: spin::Mutex<[EpollInstance; MAX_EPOLL_INSTANCES]> =
    spin::Mutex::new([const { EpollInstance::empty() }; MAX_EPOLL_INSTANCES]);

/// Close an epoll fd alias: drop its fd entry; release the instance slot
/// (and all interests with it) when the last alias goes away.
fn sys_epoll_close(epfd: usize) -> isize {
    let slot = match epoll_slot_of(epfd) { Some(s) => s, None => return -9 }; // EBADF
    let tgid = sched::current_tgid();
    let mut ep = EPOLL_INSTANCES.lock();
    if !ep[slot].in_use || sched::tgid_of(ep[slot].owner_pid) != tgid { return -9; } // EBADF
    EPOLL_FDS.lock()[epfd - EPOLL_FD_BASE].in_use = false;
    ep[slot].refs = ep[slot].refs.saturating_sub(1);
    if ep[slot].refs == 0 { ep[slot] = EpollInstance::empty(); }
    0
}

/// Free every epoll instance owned by `pid` (and all fd aliases onto them).
/// Called on process exit so a process that dies without closing its epoll
/// fds can't leak instance slots (there are only MAX_EPOLL_INSTANCES of
/// them for the whole system).
fn epoll_close_all(pid: u32) {
    let mut ep = EPOLL_INSTANCES.lock();
    let mut t = EPOLL_FDS.lock();
    for (i, inst) in ep.iter_mut().enumerate() {
        if inst.in_use && inst.owner_pid == pid {
            for e in t.iter_mut() {
                if e.in_use && e.slot as usize == i { e.in_use = false; }
            }
            *inst = EpollInstance::empty();
        }
    }
}

fn sys_epoll_create1(_flags: usize) -> isize {
    // Owner is the thread group, not the creating thread: the instance must
    // survive its creator thread's exit and be cleaned up with the process.
    let pid = sched::current_tgid();
    let mut ep = EPOLL_INSTANCES.lock();
    let mut t = EPOLL_FDS.lock();
    let fd_idx = match t.iter().position(|e| !e.in_use) {
        Some(i) => i,
        None => return -24, // EMFILE
    };
    match ep.iter().position(|e| !e.in_use) {
        Some(i) => {
            ep[i] = EpollInstance::empty();
            ep[i].in_use    = true;
            ep[i].owner_pid = pid;
            ep[i].refs      = 1;
            t[fd_idx] = EpollFdEntry { in_use: true, slot: i as u8 };
            (fd_idx + EPOLL_FD_BASE) as isize
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

    let slot = match epoll_slot_of(epfd) {
        Some(s) => s,
        None => return -9, // EBADF
    };

    // Ownership is thread-group-scoped, not task-scoped: an epoll instance is
    // an ordinary fd, shared by every CLONE_THREAD sibling of the creating
    // process (real POSIX fd-table semantics) — see the identical comment on
    // sys_epoll_wait's check for the bug this fixes.
    let tgid = sched::current_tgid();
    let mut ep = EPOLL_INSTANCES.lock();
    if !ep[slot].in_use || sched::tgid_of(ep[slot].owner_pid) != tgid { return -9; }

    match op {
        CTL_ADD | CTL_MOD => {
            if event_ptr == 0 || !validate_user_buf(event_ptr, EPOLL_EVENT_SIZE) { return -14; }
            let events = unsafe { core::ptr::read(event_ptr as *const u32) };
            // Not necessarily 8-byte aligned on x86_64 (offset 4 in the
            // packed layout) — read_unaligned is required, not read.
            let data = unsafe {
                core::ptr::read_unaligned((event_ptr + EPOLL_EVENT_DATA_OFF) as *const u64)
            };
            // Find existing entry or allocate new one.
            let inst = &mut ep[slot];
            let idx = inst.interests.iter().position(|i| i.in_use && i.fd == fd as i32)
                          .or_else(|| inst.interests.iter().position(|i| !i.in_use));
            match idx {
                Some(i) => {
                    inst.interests[i] = EpollInterest { fd: fd as i32, events, data, in_use: true, last_seq: u64::MAX };
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
/// `timeout_ms` follows `epoll_wait(2)`: `usize::MAX` (the bit pattern of a
/// raw `-1` `c_int` sign-extended by relibc's `syscall!` macro) blocks
/// indefinitely, `0` checks once and returns immediately, otherwise it's a
/// millisecond budget rounded to this kernel's ~10ms tick granularity.
/// Returns the number of ready events, or 0 on timeout.
fn sys_epoll_wait(epfd: usize, events_ptr: usize, maxevents: usize, timeout: usize) -> isize {
    if maxevents == 0 { return -22; }
    if !validate_user_buf(events_ptr, maxevents * EPOLL_EVENT_SIZE) { return -14; }

    let slot = match epoll_slot_of(epfd) {
        Some(s) => s,
        None => return -9, // EBADF
    };

    let pid = current_pid();
    {
        // Ownership is thread-group-scoped, not task-scoped: real epoll fds
        // are ordinary file descriptors, shared by every CLONE_THREAD
        // sibling of the creating process. A raw owner_pid == pid check
        // rejects any thread other than the exact one that called
        // epoll_create1 — e.g. crossterm's shared, lazily-created event
        // reader (used by both bottom's background input-reader thread and
        // its main thread's cursor-position query) is created by whichever
        // thread wins that race, then unusable from every other thread of
        // the same process, which manifests as every subsequent epoll_wait
        // failing with EBADF instead of blocking/timing out — and crossterm
        // treats that as a retry-forever condition (see
        // crossterm's `read_position_raw`'s `Err(_) => {}` loop arm), an
        // effectively permanent hang from the caller's perspective.
        let ep = EPOLL_INSTANCES.lock();
        if !ep[slot].in_use || sched::tgid_of(ep[slot].owner_pid) != sched::current_tgid() {
            return -9;
        }
    }

    let infinite = timeout == usize::MAX;
    let deadline = ticks().wrapping_add((timeout as u64) / 10);

    // EPOLLET (edge-triggered) bit and POLLIN. This kernel emulates epoll
    // level for fds without an edge source (net sockets, fd 0-2, whose
    // readiness only asserts when they actually have data/space and which
    // tokio drains itself), edge-triggered for VFS fds via their event-seq.
    // The seq (Some(_)) lets us re-fire an EPOLLET interest only when the
    // object signalled a new event, so a permanently-level-ready fd — a pipe
    // at EOF (POLLIN|POLLHUP forever), or mio's never-drained eventfd waker —
    // fires once per real edge instead of pinning tokio's reactor in a
    // 0-timeout spin, while a self-pipe byte written between two epoll_waits
    // is never dropped (its seq advanced).
    loop {
        let nready = {
            let mut ep = EPOLL_INSTANCES.lock();
            let mut n = 0usize;
            let base = events_ptr;
            for i in 0..MAX_EPOLL_INTERESTS {
                if n >= maxevents { break; }
                let interest = ep[slot].interests[i];
                if !interest.in_use { continue; }
                let (cur, seq) = probe_fd_events_seq(pid, interest.fd as usize, interest.events);
                // Edge-triggered fds (seq present) fire only when the seq has
                // advanced since we last delivered; level fds fire on any ready
                // bit. Non-VFS fds report no seq and stay level-triggered.
                let fire = cur != 0 && match seq {
                    Some(s) => s != interest.last_seq,
                    None    => true,
                };
                if fire {
                    if let Some(s) = seq { ep[slot].interests[i].last_seq = s; }
                    let off = n * EPOLL_EVENT_SIZE;
                    unsafe {
                        core::ptr::write((base + off) as *mut u32, cur);
                        // See the read_unaligned note in sys_epoll_ctl.
                        core::ptr::write_unaligned(
                            (base + off + EPOLL_EVENT_DATA_OFF) as *mut u64, interest.data);
                    }
                    n += 1;
                }
            }
            n
        };
        if nready > 0 { return nready as isize; }
        if timeout == 0 || (!infinite && ticks() >= deadline) { return 0; }
        if interrupted() { return -4; } // EINTR — lets e.g. tokio's SIGCHLD handler run

        irq_window();
        yield_now("epoll_wait");
    }
}

/// Query real, current readiness for `fd` — routes to the owning server
/// (VFS for fd < `net_server::SOCK_FD_BASE`, net otherwise) or, for fd 0-2,
/// the same evdev/serial/always-writable checks the console already used.
/// Returns the fd's true POLLIN/POLLOUT/POLLERR/POLLHUP/POLLNVAL state.
fn poll_fd_state(pid: u32, fd: usize) -> u32 {
    const POLLIN:   u32 = 0x0001;
    const POLLOUT:  u32 = 0x0004;
    const POLLNVAL: u32 = 0x0020;

    if fd == 0 {
        return if console_input_pending()
            || evdev_server::has_key_event(0) || crate::serial_has_data() { POLLIN } else { 0 };
    }
    if fd == 1 || fd == 2 {
        return POLLOUT;
    }
    // Console proxy fds (/dev/tty, a dup'd stdin) mirror fd 0/1/2 readiness —
    // crossterm registers its /dev/tty handle in epoll and waits on it for
    // the cursor-position reply; VFS_POLL reports DevStdio not-ready, which
    // hung that wait.
    if fd < net_server::SOCK_FD_BASE && vfs::fd_is_console_stdio(pid, fd) {
        return if console_input_pending()
            || evdev_server::has_key_event(0) || crate::serial_has_data() {
            POLLIN | POLLOUT
        } else {
            POLLOUT
        };
    }
    if fd >= net_server::SOCK_FD_BASE {
        let msg = make_vfs_msg(net_server::NET_POLL, &[fd as u64]);
        let r = net_reply_val(&net_server::handle(&msg, pid));
        return if r < 0 { POLLNVAL } else { r as u32 };
    }
    let msg = make_vfs_msg(vfs::VFS_POLL, &[fd as u64]);
    let r = vfs_reply_val(&vfs::handle(&msg, pid));
    if r < 0 { POLLNVAL } else { r as u32 }
}

/// Mask real fd readiness (`poll_fd_state`) down to what the caller actually
/// asked about, except POLLERR/POLLHUP/POLLNVAL — real `poll(2)`/
/// `epoll_wait(2)` report those unconditionally regardless of the requested
/// event mask.
fn probe_fd_events(pid: u32, fd: usize, requested: u32) -> u32 {
    const POLLERR:  u32 = 0x0008;
    const POLLHUP:  u32 = 0x0010;
    const POLLNVAL: u32 = 0x0020;
    let state = poll_fd_state(pid, fd);
    (state & requested) | (state & (POLLERR | POLLHUP | POLLNVAL))
}

/// Like `probe_fd_events`, but also returns the fd's edge-trigger event-seq
/// so `sys_epoll_wait` can emulate EPOLLET without dropping edges. `Some(seq)`
/// is a monotonic per-object counter for VFS fds (pipes/eventfd/timerfd — see
/// VFS handle_poll); `None` means the fd has no edge source (net sockets, fd
/// 0-2), so the caller must treat it level-triggered. The revents masking
/// matches `probe_fd_events` exactly.
fn probe_fd_events_seq(pid: u32, fd: usize, requested: u32) -> (u32, Option<u64>) {
    const POLLERR:  u32 = 0x0008;
    const POLLHUP:  u32 = 0x0010;
    const POLLNVAL: u32 = 0x0020;

    // Only real VFS fds carry a seq; fd 0-2 and net sockets stay level.
    // Console stdio proxies (/dev/tty, dup'd stdin — VFS DevStdio vnodes)
    // must take the level path too: VFS handle_poll reports DevStdio as
    // never-ready, so routing them to VFS_POLL below leaves an epoll
    // interest that can never fire. poll(2) already probes these via
    // poll_fd_state's console-proxy branch; without the same carve-out
    // here, crossterm's mio-registered /dev/tty handle never wakes for
    // the ESC[6n cursor-position reply and reedline bails out of
    // interactive mode after its 2s CPR timeout.
    if fd <= 2 || fd >= net_server::SOCK_FD_BASE
        || vfs::fd_is_console_stdio(pid, fd) {
        return (probe_fd_events(pid, fd, requested), None);
    }
    let msg = make_vfs_msg(vfs::VFS_POLL, &[fd as u64]);
    let reply = vfs::handle(&msg, pid);
    let r = vfs_reply_val(&reply);
    let state = if r < 0 { POLLNVAL } else { r as u32 };
    let seq = u64::from_le_bytes(reply.data[8..16].try_into().unwrap_or([0u8; 8]));
    let masked = (state & requested) | (state & (POLLERR | POLLHUP | POLLNVAL));
    (masked, Some(seq))
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

/// sys_select(nfds, readfds, writefds, exceptfds, timeout_ptr).
///
/// Only bits the caller actually set in `readfds`/`writefds` are ever
/// considered — unlike the fake implementation this replaces, an fd is
/// reported ready only once `probe_fd_events` confirms it for real. Retries
/// in a cooperative loop until something is ready or `timeout_ptr`'s
/// `struct timeval` elapses (NULL = block indefinitely). `exceptfds`, if
/// given, is always cleared: no fd type in this kernel produces an
/// "exceptional condition" distinct from POLLERR/POLLHUP. Bad fds (bits set
/// past `nfds`'s real descriptor table) are simply never marked ready rather
/// than failing the call with EBADF, unlike real Linux `select(2)`.
fn sys_select(nfds: usize, rfds: usize, wfds: usize, efds: usize, tv_ptr: usize) -> isize {
    const POLLIN:  u32 = 0x0001;
    const POLLOUT: u32 = 0x0004;

    if nfds > 1024 { return -22; } // EINVAL — matches relibc's FD_SETSIZE
    let bytes = (nfds + 7) / 8;
    let pid = current_pid();

    let has_r = rfds != 0 && validate_user_buf(rfds, bytes);
    let has_w = wfds != 0 && validate_user_buf(wfds, bytes);
    let has_e = efds != 0 && validate_user_buf(efds, bytes);

    let (infinite, deadline) = if tv_ptr == 0 {
        (true, 0)
    } else {
        if !validate_user_buf(tv_ptr, 16) { return -14; }
        let tv_sec  = unsafe { core::ptr::read(tv_ptr       as *const i64) };
        let tv_usec = unsafe { core::ptr::read((tv_ptr + 8) as *const i64) };
        if tv_sec < 0 || tv_usec < 0 { return -22; } // EINVAL
        let ticks_needed = (tv_sec as u64) * 100 + (tv_usec as u64) / 10_000;
        (false, ticks().wrapping_add(ticks_needed))
    };

    loop {
        // fd_set is capped at 1024 bits (FD_SETSIZE) above, so 128 bytes
        // always covers `bytes`.
        let mut out_r = [0u8; 128];
        let mut out_w = [0u8; 128];
        let mut nready = 0isize;

        for fd in 0..nfds {
            let want_r = has_r && unsafe { (*(rfds as *const u8).add(fd / 8) >> (fd % 8)) & 1 != 0 };
            let want_w = has_w && unsafe { (*(wfds as *const u8).add(fd / 8) >> (fd % 8)) & 1 != 0 };
            if !want_r && !want_w { continue; }
            let requested = (if want_r { POLLIN } else { 0 }) | (if want_w { POLLOUT } else { 0 });
            let ev = probe_fd_events(pid, fd, requested);
            if want_r && ev & POLLIN  != 0 { out_r[fd / 8] |= 1 << (fd % 8); nready += 1; }
            if want_w && ev & POLLOUT != 0 { out_w[fd / 8] |= 1 << (fd % 8); nready += 1; }
        }

        if nready > 0 || (!infinite && ticks() >= deadline) {
            if has_r { unsafe { core::ptr::copy_nonoverlapping(out_r.as_ptr(), rfds as *mut u8, bytes); } }
            if has_w { unsafe { core::ptr::copy_nonoverlapping(out_w.as_ptr(), wfds as *mut u8, bytes); } }
            if has_e { unsafe { core::ptr::write_bytes(efds as *mut u8, 0, bytes); } }
            return nready;
        }
        if interrupted() { return -4; } // EINTR

        irq_window();

        yield_now("select");
    }
}

// ── rename / truncate / sendfile / itimer / sigpending / alarm ───────────────

/// sys_renameat(old_path_ptr, new_path_ptr) — rename a /tmp file.
fn sys_renameat(old_path_ptr: usize, new_path_ptr: usize) -> isize {
    let old = match resolve_user_path(old_path_ptr) { Ok(p) => p, Err(e) => return e };
    let new = match resolve_user_path(new_path_ptr) { Ok(p) => p, Err(e) => return e };
    let pid = current_pid();
    let msg = make_vfs_msg(vfs::VFS_RENAME, &[old.ptr() as u64, new.ptr() as u64]);
    vfs_reply_val(&vfs::handle(&msg, pid))
}

/// sys_truncate(path_ptr, length) — set a file's size by path.
fn sys_truncate(path_ptr: usize, length: usize) -> isize {
    let path = match resolve_user_path(path_ptr) { Ok(p) => p, Err(e) => return e };
    let pid = current_pid();
    let omsg = make_vfs_msg(vfs::VFS_OPEN, &[path.ptr() as u64, 0x0002u64 /* O_RDWR */, 0]);
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
const ITIMER_TICK_HZ: u64 = 100;
const ITIMER_USEC_PER_TICK: u64 = 1_000_000 / ITIMER_TICK_HZ;

/// Parse a 32-byte `struct itimerval` (`{ it_interval, it_value }`, each a
/// `{ tv_sec: i64, tv_usec: i64 }` pair) into `(interval_ticks, value_ticks)`.
fn parse_itimerval(buf: &[u8; 32]) -> (u64, u64) {
    let iv_sec  = i64::from_ne_bytes(buf[0..8].try_into().unwrap());
    let iv_usec = i64::from_ne_bytes(buf[8..16].try_into().unwrap());
    let va_sec  = i64::from_ne_bytes(buf[16..24].try_into().unwrap());
    let va_usec = i64::from_ne_bytes(buf[24..32].try_into().unwrap());
    let itv = (iv_sec as u64 * ITIMER_TICK_HZ) + (iv_usec as u64 / ITIMER_USEC_PER_TICK);
    let vtv = (va_sec as u64 * ITIMER_TICK_HZ) + (va_usec as u64 / ITIMER_USEC_PER_TICK);
    (itv, vtv)
}

/// Encode `(interval_ticks, value_ticks)` as a 32-byte `struct itimerval`.
fn itimerval_bytes(interval_ticks: u64, value_ticks: u64) -> [u8; 32] {
    let mut buf = [0u8; 32];
    buf[0..8].copy_from_slice(&((interval_ticks / ITIMER_TICK_HZ) as i64).to_ne_bytes());
    buf[8..16].copy_from_slice(&(((interval_ticks % ITIMER_TICK_HZ) * ITIMER_USEC_PER_TICK) as i64).to_ne_bytes());
    buf[16..24].copy_from_slice(&((value_ticks / ITIMER_TICK_HZ) as i64).to_ne_bytes());
    buf[24..32].copy_from_slice(&(((value_ticks % ITIMER_TICK_HZ) * ITIMER_USEC_PER_TICK) as i64).to_ne_bytes());
    buf
}

fn sys_setitimer(which: usize, new_ptr: usize, old_ptr: usize) -> isize {
    // struct itimerval: { it_interval: timeval(16), it_value: timeval(16) } = 32 bytes
    if new_ptr != 0 && !validate_user_buf(new_ptr, 32) { return -14; }
    if old_ptr != 0 && !validate_user_buf(old_ptr, 32) { return -14; }

    // We only implement ITIMER_REAL (0).
    if which != 0 { return 0; } // silently succeed for VIRTUAL/PROF

    let pid = current_pid();

    let (interval_ticks, value_ticks) = if new_ptr != 0 {
        let mut buf = [0u8; 32];
        if !with_current_address_space(|as_| as_.read_user_buf(new_ptr, &mut buf)).unwrap_or(false) {
            return -14;
        }
        parse_itimerval(&buf)
    } else {
        (0, 0)
    };

    // Arm the reserved ITIMER_REAL slot directly (tick units — no synthetic
    // user-space pointer round-trip; see set_real_itimer's doc comment).
    let (old_interval_ticks, old_value_ticks) = tty_server::set_real_itimer(pid, interval_ticks, value_ticks);

    if old_ptr != 0 {
        let obuf = itimerval_bytes(old_interval_ticks, old_value_ticks);
        if !with_current_address_space(|as_| as_.write_user_buf(old_ptr, &obuf)).unwrap_or(false) {
            return -14;
        }
    }
    0
}

/// sys_getitimer(which, cur_ptr) — get current interval timer state.
fn sys_getitimer(which: usize, cur_ptr: usize) -> isize {
    if which != 0 { return 0; }
    if !validate_user_buf(cur_ptr, 32) { return -14; }
    let pid = current_pid();
    let (interval_ticks, value_ticks) = tty_server::get_real_itimer(pid);
    let buf = itimerval_bytes(interval_ticks, value_ticks);
    if with_current_address_space(|as_| as_.write_user_buf(cur_ptr, &buf)).unwrap_or(false) { 0 } else { -14 }
}

/// sys_sigpending(set_ptr) — return the set of pending signals.
fn sys_sigpending(set_ptr: usize) -> isize {
    if !validate_user_buf(set_ptr, 8) { return -14; }
    // Thread-pending plus process-level pending: a process-directed signal
    // that every thread currently masks is parked on the leader
    // (shared_signal_pending) but is still "pending" to sigpending(2).
    let pending = pending_signals() | sched::shared_pending_signals();
    if with_current_address_space(|as_| as_.write_user_buf(set_ptr, &pending.to_ne_bytes())).unwrap_or(false) {
        0
    } else {
        -14
    }
}

/// sys_alarm(seconds) — schedule SIGALRM after `seconds` seconds (x86-64 only).
#[cfg(not(target_arch = "aarch64"))]
fn sys_alarm(seconds: usize) -> isize {
    let pid = current_pid();
    const TICK_HZ: u64 = 100;
    let value_ticks = seconds as u64 * TICK_HZ;

    // One-shot (no interval) — arm the reserved ITIMER_REAL slot directly.
    let (_, old_value_ticks) = tty_server::set_real_itimer(pid, 0, value_ticks);

    // Real alarm() returns the number of seconds remaining on any previous
    // alarm, rounded up so a caller never sees "0 seconds left" for an
    // alarm that's about to fire (matches glibc/Linux behavior).
    ((old_value_ticks + TICK_HZ - 1) / TICK_HZ) as isize
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
    a0:          usize,
    a1:          usize,
    a2:          usize,
    a3:          usize,
    a4:          usize,
    frame_ptr:   usize,
) -> isize {
    const CLONE_VM: usize = 0x0000_0100;

    #[cfg(target_arch = "x86_64")]
    let (flags, child_stack, _ptid, ctid, tls) = (a0, a1, a2, a3, a4);

    #[cfg(target_arch = "aarch64")]
    let (flags, child_stack, _ptid, tls, ctid) = (a0, a1, a2, a3, a4);

    if flags & CLONE_VM != 0 {
        const CLONE_THREAD: usize = 0x0001_0000;
        // Identify the parent by tgid: its fd table is keyed there, not by the
        // (possibly non-leader) forking thread's pid.
        let parent_pid = sched::tgid_of(current_pid());
        clone_thread(flags, child_stack, tls, ctid, frame_ptr, |child_pid| {
            // Real CLONE_THREAD siblings (pthread_create) share the leader's
            // tgid and, today, have no fd table of their own at all — every
            // VFS call from such a thread already resolves fds by its own
            // pid, unrelated to this dup. Only vfork-style children
            // (CLONE_VM without CLONE_THREAD — e.g. musl/std's
            // Command::spawn posix_spawn-fast-path clone(CLONE_VM|
            // CLONE_VFORK|SIGCHLD)) need this: per POSIX they get their own
            // *copy* of the fd table (same as fork), not a share, and
            // without it every fd the child inherited (its stdio
            // redirections, the exec-failure error-reporting pipe) is
            // invisible to the VFS server the instant the child runs its
            // first fd syscall — VFS_FORK_DUP is exactly fork_current's own
            // fix for the identical fork() case (see the FORK arm above).
            if flags & CLONE_THREAD == 0 {
                let msg = make_vfs_msg(vfs::VFS_FORK_DUP,
                                       &[parent_pid as u64, child_pid as u64]);
                let _ = vfs::handle(&msg, parent_pid);
                let nmsg = make_vfs_msg(net_server::NET_FORK_DUP,
                                        &[parent_pid as u64, child_pid as u64]);
                let _ = net_server::handle(&nmsg, parent_pid);
            }
        })
    } else {
        let _ = (child_stack, _ptid, tls, ctid);
        // fd tables are keyed by tgid, so the parent must be identified by its
        // thread-group id — a fork issued by a non-leader thread (e.g. a tokio
        // worker calling std's pre_exec fork path) otherwise names a pid the
        // fd-table search never matches, and the child inherits an empty table.
        let parent_pid = sched::tgid_of(current_pid());
        // Duplicate the fd table before the child becomes runnable (see the
        // FORK arm of syscall_dispatch for the SMP race this prevents).
        fork_current(frame_ptr, |child_pid| {
            let msg = make_vfs_msg(vfs::VFS_FORK_DUP,
                                   &[parent_pid as u64, child_pid as u64]);
            let _ = vfs::handle(&msg, parent_pid);
            let nmsg = make_vfs_msg(net_server::NET_FORK_DUP,
                                    &[parent_pid as u64, child_pid as u64]);
            let _ = net_server::handle(&nmsg, parent_pid);
        })
    }
}
