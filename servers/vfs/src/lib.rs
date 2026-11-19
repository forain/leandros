//! VFS server — per-process FD tables, RamFS, pipes, and devfs.
//!
//! # Message encoding
//!
//! Arguments are packed into `Message.data` as little-endian `u64` words:
//!   data[0..8] = arg0, data[8..16] = arg1, data[16..24] = arg2
//!
//! | Tag             | arg0       | arg1      | arg2    | Reply arg0          |
//! |-----------------|------------|-----------|---------|---------------------|
//! | VFS_OPEN        | path_ptr   | flags     | mode    | fd or -errno        |
//! | VFS_READ        | fd         | buf_ptr   | count   | bytes or -errno     |
//! | VFS_WRITE       | fd         | buf_ptr   | count   | bytes written        |
//! | VFS_CLOSE       | fd         | 0         | 0       | 0 or -errno         |
//! | VFS_STAT        | path_ptr   | stat_ptr  | 0       | 0 or -errno         |
//! | VFS_LSEEK       | fd         | offset    | whence  | new offset or -errno|
//! | VFS_PIPE        | rfd_ptr    | wfd_ptr   | 0       | 0 or -errno         |
//! | VFS_DUP2        | oldfd      | newfd     | 0       | newfd or -errno     |
//! | VFS_FCNTL       | fd         | cmd       | arg     | result or -errno    |
//! | VFS_FORK_DUP    | parent_pid | child_pid | 0       | 0                   |
//! | VFS_EXEC_CLOEXEC| pid        | 0         | 0       | 0                   |
//! | VFS_CLOSE_ALL   | pid        | 0         | 0       | 0                   |
//! | VFS_RMDIR       | path_ptr   | 0         | 0       | 0 or -errno         |
//! | VFS_FLOCK       | fd         | op        | 0       | 0 or -errno         |
//! | VFS_CHMOD       | path_ptr   | mode      | 0       | 0 or -errno         |
//! | VFS_FCHMOD      | fd         | mode      | 0       | 0 or -errno         |
//! | VFS_CHOWN       | path_ptr   | uid       | gid     | 0 or -errno         |
//! | VFS_FCHOWN      | fd         | uid       | gid     | 0 or -errno         |
//! | VFS_STATFS      | path_ptr   | statfs_ptr| 0       | 0 or -errno         |
//! | VFS_FSTATFS     | fd         | statfs_ptr| 0       | 0 or -errno         |
//!
//! When the VFS *forwards* one of these to a mount server, VFS_WRITE carries a
//! fourth argument the kernel never sends: arg3 != 0 means "this descriptor is
//! O_APPEND, write at end-of-file". It has to be per write rather than per
//! open because the mount server owns the file position and O_APPEND positions
//! at EOF at write time (and because `fcntl(F_SETFL)` can set the bit after
//! the open was forwarded). Unset — a zeroed payload — means an ordinary write
//! at the slot's current position, so senders that predate it are unaffected.

#![no_std]

use ipc::{Message, port};
use spin::Mutex;
use alloc::vec::Vec;

extern crate alloc;
extern crate mm;

/// Block devices (`/dev/vd*`, partitions, loop devices) and the synthesized
/// `/sys/class/block` tree. See the module docs for why it lives here and why
/// sysfs is generated on demand rather than stored.
pub mod block;
pub mod sysclass;
pub mod sysinput;
pub mod sysdrm;
pub mod cgroupfs;

/// Open file descriptions (see the module docs): what epoll keys by.
pub mod ofd;
pub mod inotify;
pub mod mqueue;

/// Alias mounts (proc/devtmpfs/sysfs/bind) — how a chroot gets its /proc.
pub mod alias;
pub mod devpts;
pub mod mnt;
pub mod cmdline_redact;
/// Overlay plumbing: real-layer helpers, per-mount pseudo-pid creds.
pub mod ovl;
mod tmppool;

/// PATH_MAX: the longest path string a caller may name, NUL included.
pub const PATH_MAX: usize = 4096;
/// NAME_MAX: the longest single path component.
pub const NAME_MAX: usize = 255;
/// Longest host-absolute path the kernel hands to the VFS and the mount
/// servers, NUL included. A PATH_MAX path under a chroot root (or relative to
/// a deep cwd) is longer than PATH_MAX once made host-absolute; this is the
/// bound every path buffer on the server side honours.
pub const HOST_PATH_MAX: usize = 2 * PATH_MAX;

// ── Protocol tag constants ────────────────────────────────────────────────────

pub const VFS_OPEN:        u64 = 0x10;
pub const VFS_READ:        u64 = 0x11;
pub const VFS_WRITE:       u64 = 0x12;
pub const VFS_CLOSE:       u64 = 0x13;
pub const VFS_STAT:        u64 = 0x14;
pub const VFS_LSEEK:       u64 = 0x15;
pub const VFS_PIPE:        u64 = 0x17;
pub const VFS_DUP2:        u64 = 0x18;
pub const VFS_FCNTL:       u64 = 0x19;
pub const VFS_FORK_DUP:    u64 = 0x1A;
pub const VFS_EXEC_CLOEXEC: u64 = 0x1B;
pub const VFS_CLOSE_ALL:   u64 = 0x1C;
pub const VFS_GETDENTS64:  u64 = 0x1D;
pub const VFS_ALLOC_FD:    u64 = 0x1E; // dup() — alloc new fd pointing at same vnode
pub const VFS_UNLINK:      u64 = 0x1F; // unlink(path_ptr) — remove a /tmp file
pub const VFS_MKDIR:       u64 = 0x20; // mkdir(path_ptr, mode) — create a /tmp subdir
pub const VFS_FTRUNCATE:   u64 = 0x21; // ftruncate(fd, length) — set file size
pub const VFS_RENAME:      u64 = 0x22; // rename(old_ptr, new_ptr) — rename /tmp file
pub const VFS_FD_PATH:     u64 = 0x23; // fd_path(fd, buf_ptr, buf_len) → len or -errno
pub const VFS_EVENTFD:     u64 = 0x24; // eventfd2(initval, flags) → fd or -errno
pub const VFS_TIMERFD_CREATE:  u64 = 0x25; // timerfd_create(flags, clockid) → fd
pub const VFS_TIMERFD_SETTIME: u64 = 0x26; // timerfd_settime(fd, value_ns (0 = disarm), interval_ns, TFD_TIMER_* flags)
pub const VFS_TIMERFD_GETTIME: u64 = 0x27; // timerfd_gettime(fd, out_ptr)
pub const VFS_IOCTL:           u64 = 0x28; // ioctl(fd, cmd, arg) → result or -errno
pub const VFS_RMDIR:           u64 = 0x29; // rmdir(path_ptr) → 0 or -errno
pub const VFS_FLOCK:           u64 = 0x2A; // flock(fd, op) → 0 or -errno
pub const VFS_CHMOD:           u64 = 0x2B; // chmod(path_ptr, mode) → 0 or -errno
pub const VFS_FCHMOD:          u64 = 0x2C; // fchmod(fd, mode) → 0 or -errno
pub const VFS_CHOWN:           u64 = 0x2D; // chown(path_ptr, uid, gid) → 0 or -errno
pub const VFS_FCHOWN:          u64 = 0x2E; // fchown(fd, uid, gid) → 0 or -errno
pub const VFS_POLL:            u64 = 0x2F; // poll(fd) → revents bitmask (POLLIN/OUT/ERR/HUP)
pub const VFS_PIVOT_ROOT:      u64 = 0x30;
/// fstat(fd, stat_ptr) → 0 or -errno. Reports the *kind* of the open file
/// behind an fd (S_IFIFO for a pipe end, S_IFCHR for a console/dev proxy,
/// S_IFDIR/S_IFREG for tmpfs and mounted files) instead of the blanket
/// S_IFREG the kernel used to fabricate. Load-bearing: tokio's
/// `net::unix::pipe::Receiver::from_file` gates on `S_ISFIFO(st_mode)` and
/// rejects anything else with "not a pipe", which is what broke every
/// `$(...)` command substitution in brush.
pub const VFS_FSTAT:           u64 = 0x31;
/// mknod(path_ptr, mode) → 0 or -errno. Creates a /tmp entry as a plain file
/// or a FIFO depending on the S_IFMT bits of `mode` — see `handle_mknod`.
pub const VFS_MKNOD:           u64 = 0x32;
/// statfs(path_ptr, buf_ptr) → 0 or -errno. Fills a `struct statfs` for the
/// filesystem that owns `path`. Forwarded to the mount server when `path`
/// falls under a registered mount (the mount is the only thing that knows its
/// real geometry); answered from the tmpfs pool otherwise. Mount servers
/// receive this same tag and may ignore `path_ptr` — one port is one volume.
pub const VFS_STATFS:          u64 = 0x33;
/// fstatfs(fd, buf_ptr) → 0 or -errno. Same answer as VFS_STATFS, selected by
/// an open descriptor rather than a path.
pub const VFS_FSTATFS:         u64 = 0x34;
/// symlink(target_ptr, linkpath_ptr) → 0 or -errno. `target_ptr` is the raw
/// link body, stored verbatim and never resolved; `linkpath_ptr` is the name
/// to create. The final component of `linkpath` is NOT followed.
pub const VFS_SYMLINK:         u64 = 0x35;
/// readlink(path_ptr, buf_ptr, buf_len) → len or -errno. -EINVAL when `path`
/// exists but is not a symlink, which is exactly how callers distinguish
/// "not a link" from "not there". Does NOT follow the final component.
pub const VFS_READLINK:        u64 = 0x36;
/// link(oldpath_ptr, newpath_ptr) → 0 or -errno. Neither path's final
/// component is followed (Linux `link(2)` semantics). -EXDEV when the two
/// paths live on different filesystems, -EPERM for a directory source.
pub const VFS_LINK:            u64 = 0x37;
/// lstat(path_ptr, stat_ptr) → 0 or -errno. Identical to VFS_STAT except that
/// the final component is not followed, so a symlink reports S_IFLNK and the
/// length of its target as st_size.
pub const VFS_LSTAT:           u64 = 0x38;
/// fsync(fd) → 0 or -errno. Flush the filesystem backing `fd` to stable
/// storage. Filesystems with no write-back state (tmpfs, procfs, devices)
/// answer 0 without doing anything, which is honest rather than a stub: there
/// is genuinely nothing of theirs that can outlive a reset.
pub const VFS_FSYNC:           u64 = 0x39;
/// VFS_FSYNC data[1] (lane fsyncbatch): what to commit. 0 = everything on
/// the mount (sync(2), and any caller that sends no kind).
pub const FSYNC_KIND_SYNC:     u64 = 0;
pub const FSYNC_KIND_FILE:     u64 = 1;
pub const FSYNC_KIND_DATA:     u64 = 2;
pub const FSYNC_KIND_SYNCFS:   u64 = 3;
/// sync() → 0. Flush *every* mounted filesystem. Takes no argument and cannot
/// fail, matching `sync(2)`.
pub const VFS_SYNC:            u64 = 0x3A;
/// chmod/chown that act on a symlink itself rather than its target — the
/// `AT_SYMLINK_NOFOLLOW` forms of fchmodat/fchownat, and `lchown(2)`.
/// Separate tags rather than a flag argument, matching how VFS_LSTAT already
/// distinguishes itself from VFS_STAT: the follow decision has to be made in
/// `path_args`, above the handlers, so it must be visible in the opcode.
pub const VFS_LCHMOD:          u64 = 0x3B;
pub const VFS_LCHOWN:          u64 = 0x3C;

/// Extended attributes. All pointers are caller-space and forwarded verbatim
/// to mount servers, like every other op. l-forms don't follow a final
/// symlink; f-forms take an fd (rewritten to the mount-local file_id when
/// proxied). Shared wire format, size caps, permission gates, and the POSIX
/// ACL evaluator live in the `xattr` crate (servers/xattr) — the kernel,
/// this server, and f2fs all use that one implementation.
///
/// setxattr(path_ptr, name_ptr, value_ptr, size, flags) → 0 or -errno
pub const VFS_SETXATTR:        u64 = 0x3D;
pub const VFS_LSETXATTR:       u64 = 0x3E;
/// fsetxattr(fd, name_ptr, value_ptr, size, flags) → 0 or -errno
pub const VFS_FSETXATTR:       u64 = 0x3F;
/// getxattr(path_ptr, name_ptr, value_ptr, size) → value length or -errno.
/// size==0 is a length query; size too small for the value → -ERANGE.
pub const VFS_GETXATTR:        u64 = 0x40;
pub const VFS_LGETXATTR:       u64 = 0x41;
pub const VFS_FGETXATTR:       u64 = 0x42;
/// listxattr(path_ptr, list_ptr, size) → total bytes of NUL-joined names or
/// -errno. size==0 is a length query.
pub const VFS_LISTXATTR:       u64 = 0x43;
pub const VFS_LLISTXATTR:      u64 = 0x44;
pub const VFS_FLISTXATTR:      u64 = 0x45;
/// removexattr(path_ptr, name_ptr) → 0 or -errno
pub const VFS_REMOVEXATTR:     u64 = 0x46;
pub const VFS_LREMOVEXATTR:    u64 = 0x47;
pub const VFS_FREMOVEXATTR:    u64 = 0x48;
/// access(path_ptr, mode) → 0 or -errno. mode = R_OK|W_OK|X_OK bits (F_OK=0
/// is just existence). The owning filesystem answers via xattr::access_check
/// so stored POSIX ACLs are honored; the kernel's faccessat routes here.
pub const VFS_ACCESS:          u64 = 0x49;
/// signalfd4(fd, mask, flags) → fd or -errno. fd == u64::MAX creates a new
/// SignalFd vnode with `mask`; an existing signalfd fd updates its mask.
pub const VFS_SIGNALFD_CREATE: u64 = 0x4A;
/// inotify_init1(flags) → fd or -errno. Allocates an Inotify vnode (a valid fd
/// that accepts watches but never delivers events — see the K2 design).
pub const VFS_INOTIFY_CREATE:  u64 = 0x4B;
/// inotify_add_watch(fd, path_ptr, mask) → watch descriptor (≥1) or -errno.
pub const VFS_INOTIFY_ADD:     u64 = 0x4C;
/// inotify_rm_watch(fd, wd) → 0 or -errno (only sent unless `vfs.inotify=0`).
pub const VFS_INOTIFY_RM:      u64 = 0x53;
/// fanotify_init(flags, event_f_flags) → fd or -errno.
pub const VFS_FANOTIFY_INIT:   u64 = 0x54;
/// fanotify_mark(fd, path_ptr (0 = the fd named by dirfd), flags, mask, dirfd) → 0 or -errno.
pub const VFS_FANOTIFY_MARK:   u64 = 0x55;
/// utimensat: `VFS_UTIMENS(path_ptr, atime_sec, atime_nsec, mtime_sec,
/// mtime_nsec, flags)` / `VFS_LUTIMENS` (final symlink not followed) /
/// `VFS_FUTIMENS(fd, ...)` → 0 or -errno. The kernel has already turned
/// `UTIME_NOW` into the current time; an `nsec` of [`UTIME_OMIT`] leaves that
/// timestamp alone. `flags & UTIMENS_EXPLICIT` says the caller supplied at
/// least one explicit time, which needs ownership (or root) rather than
/// write permission — the utimensat(2) rule.
pub const VFS_UTIMENS:         u64 = 0x4D;
pub const VFS_LUTIMENS:        u64 = 0x4E;
pub const VFS_FUTIMENS:        u64 = 0x4F;
/// fallocate(fd, mode, offset, len). Forwarded to a mount's server for a
/// `MountedFile` with the same argument layout (`file_id` in place of fd).
pub const VFS_FALLOCATE:       u64 = 0x50;
/// pread/pwrite(fd, buf, count, offset) on a block-device node: a positional
/// op that never touches the description's offset. Any other fd kind answers
/// `ERR_NOT_POSITIONAL` and the kernel falls back to seek emulation.
pub const VFS_PREAD:           u64 = 0x51;
pub const VFS_PWRITE:          u64 = 0x52;
pub const ERR_NOT_POSITIONAL:  i64 = -524;
pub const UTIME_NOW:  i64 = (1 << 30) - 1;
pub const UTIME_OMIT: i64 = (1 << 30) - 2;
pub const UTIMENS_EXPLICIT: u64 = 1;


/// Readiness bitmask, numerically identical to Linux's POLLIN/POLLOUT/POLLERR/
/// POLLHUP (and thus also EPOLLIN/EPOLLOUT/EPOLLERR/EPOLLHUP) so one value
/// answers a `VFS_POLL` query regardless of which multiplexing syscall the
/// kernel is servicing it for.
pub const POLLIN:  u32 = 0x0001;
pub const POLLOUT: u32 = 0x0004;
pub const POLLERR: u32 = 0x0008;
pub const POLLHUP: u32 = 0x0010;

// ── Message helpers ───────────────────────────────────────────────────────────

#[inline]
fn arg(msg: &Message, n: usize) -> u64 {
    let off = n * 8;
    u64::from_le_bytes(msg.data[off..off + 8].try_into().unwrap_or([0u8; 8]))
}

fn make_reply(v: i64) -> Message {
    let mut m = Message::empty();
    m.data[0..8].copy_from_slice(&(v as u64).to_le_bytes());
    m
}

/// Inverse of `make_reply`: read back the i64 an internal handler returned, for
/// the cases where one handler is built on another (e.g. handle_fstat sizing a
/// mounted file via handle_lseek).
fn reply_val(m: &Message) -> i64 {
    i64::from_le_bytes(m.data[0..8].try_into().unwrap_or([0; 8]))
}

fn ok_reply()        -> Message { make_reply(0) }
fn err_reply(e: i32) -> Message { make_reply(e as i64) }
fn val_reply(v: u64) -> Message { make_reply(v as i64) }

/// VFS_POLL reply carrying both the revents bitmask (data[0..8]) and the
/// object's edge-trigger sequence (data[8..16]). See handle_poll / PipeRing::seq.
fn poll_reply(revents: u32, seq: u64, tag: u64) -> Message {
    let mut m = make_reply(revents as i64);
    m.data[8..16].copy_from_slice(&seq.to_le_bytes());
    // Targeted-wake tag (see sched::poll_tag): data[24..32] carries it and
    // data[32] == 1 marks it valid. data[32] == 0 (a server that does not fill
    // this in) means the kernel prober must treat the interest as broadcast
    // (POLL_TAG_ALL) — never as tag 0, which would contribute nothing and could
    // lose a wake.
    m.data[24..32].copy_from_slice(&tag.to_le_bytes());
    m.data[32] = 1;
    m
}

// ── IPC Call helper ──────────────────────────────────────────────────────────

// ── ENOMEM attribution ───────────────────────────────────────────────────────
//
// Three of this server's error paths return -12, and userspace sees errno 12
// and nothing else from all three. "Out of memory" from LeandrOS has already
// sent two investigations looking for a leak that was not there, so each site
// names the table it actually ran out of. Once per boot per site: these tables
// stay full once they fill, and the console costs ~0.19 s per line. A dead
// mount port is not one of them: it answers ENOTCONN (`report_send_failed`).

extern "C" { fn arch_serial_putc(c: u8); }

fn dbg_str(msg: &str) {
    for &b in msg.as_bytes() { unsafe { arch_serial_putc(b) }; }
}

fn dbg_dec(mut v: usize) {
    if v == 0 { unsafe { arch_serial_putc(b'0') }; return; }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v > 0 { i -= 1; buf[i] = b'0' + (v % 10) as u8; v /= 10; }
    for &b in &buf[i..] { unsafe { arch_serial_putc(b) }; }
}

/// The caller has no reply port and the IPC table could not give it one, so
/// every operation on a mounted filesystem now fails. `ipc::port` prints the
/// occupancy; this line names the consequence, which is the half that explains
/// an errno 12 out of `open`, `stat` or `getdents64`.
fn report_no_reply_port() {
    static REPORTED: atomic::AtomicBool = atomic::AtomicBool::new(false);
    if REPORTED.swap(true, atomic::Ordering::Relaxed) { return; }
    dbg_str("\n[VFS] ENOMEM: no reply port for this task -- every call to a mounted\n");
    dbg_str("[VFS] filesystem (open/stat/getdents64/exec) now returns errno 12. Not RAM.\n");
}

/// A send to a mounted filesystem's port failed. A port that is gone means
/// the server behind a still-registered mount went away: ENOTCONN, which is
/// what Linux answers for a FUSE mount whose daemon died ("Transport
/// endpoint is not connected") — it is not a memory shortage, and reporting
/// it as ENOMEM once sent an investigation after a bad f2fs image when the
/// mount's port had died with the `mount` process. A full queue stays
/// ENOMEM (a resource limit, not a broken mount). Returns the errno.
fn report_send_failed(e: ipc::port::SendError) -> i32 {
    static REPORTED: atomic::AtomicU32 = atomic::AtomicU32::new(0);
    let (bit, errno, msg) = match e {
        ipc::port::SendError::PortNotFound => (1, -107,
            "\n[VFS] ENOTCONN: the port of a mounted filesystem is gone (its server or owner exited).\n"),
        ipc::port::SendError::QueueFull => (2, -12,
            "\n[VFS] ENOMEM: port::send failed (the server port's queue is full).\n"),
    };
    if REPORTED.fetch_or(bit, atomic::Ordering::Relaxed) & bit == 0 { dbg_str(msg); }
    errno
}

/// One serial line per pool, the first time a GLOBAL fixed-size pool runs
/// dry. Such a pool fails an allocation in a process that holds almost no
/// fds, which is otherwise indistinguishable from a per-process leak.
fn report_pool_full(name: &str, cap: usize) {
    static REPORTED: atomic::AtomicU32 = atomic::AtomicU32::new(0);
    let bit = match name { "eventfd" => 1, "timerfd" => 2, "pipe" => 4, "tmpfs" => 16, "pty" => 32, _ => 8 };
    if REPORTED.fetch_or(bit, atomic::Ordering::Relaxed) & bit != 0 { return; }
    dbg_str("\n[VFS] global ");
    dbg_str(name);
    dbg_str(" pool FULL (ENFILE; tmpfs/pty: ENOSPC) at ");
    dbg_dec(cap);
    dbg_str(" -- a vfs fixed table, not this process's fd limit\n");
}

/// Synchronously call another server via its IPC port.
/// Blocks the current task until a reply is received on its reply port.
pub fn call_port(port_id: u32, mut msg: Message) -> Message {
    // Lazily allocate the caller's reply port.
    let reply_port = {
        let rp = sched::current_reply_port();
        if rp != u32::MAX {
            rp
        } else {
            let caller = sched::current_pid();
            match port::create_reply(caller) {
                Some(p) => {
                    sched::set_current_reply_port(p);
                    p
                }
                // A zeroed Message::empty() decodes as val_reply(0) — a
                // phantom success (e.g. an empty file) rather than a visible
                // error. Callers like handle_open's MountedFile path only
                // check `< 0`, so this must be a real negative errno.
                None    => { report_no_reply_port(); return err_reply(-12); } // ENOMEM
            }
        }
    };

    msg.reply_port = reply_port;
    if let Err(e) = port::send(port_id, msg) {
        return err_reply(report_send_failed(e));
    }

    let caller = sched::current_pid();
    // A port with a direct handler answers inside `port::send`, so the reply
    // is normally already queued here: take it without the prepare/cancel
    // park protocol below, which costs two RUN_QUEUE holds per call for a
    // sleep that never happens.
    if let Some(reply) = port::recv_as(reply_port, caller) {
        return reply;
    }
    loop {
        // Publish Blocked before the queue check so a reply enqueued after
        // an empty recv_as still finds us Blocked and its unblock_port()
        // wake is never lost (same check-then-block race as sys_recv).
        sched::block_on_port_prepare(reply_port);
        match port::recv_as(reply_port, caller) {
            Some(reply) => {
                sched::block_on_port_cancel();
                return reply;
            }
            None => {
                sched::block_on_port_commit();
            }
        }
    }
}

// ── Writable tmpfs pool ───────────────────────────────────────────────────────

// The pool holds *metadata* only: a regular file's bytes live in page frames
// owned by its `TmpVmo` (see "Shared VMO store" below), allocated one 4 KiB
// page at a time as they are written, so a file is bounded by the tmpfs page
// budget (`tmpfs_budget_pages`, half of RAM as on Linux) rather than by a
// fixed in-struct array. Until 2026-10-01 every entry carried a 32 KiB inline
// `data` array — 16 MiB of BSS for 512 entries and a hard 32 KiB file cap
// (`cp` of a 371 KB file into /tmp was ENOSPC, captured stderr was silently
// cut at 32 KiB).
// 128 -> 512 (lane multiterm, 2026-09-27): every Wayland client holds several
// memfds here (wl_shm pools, keymaps), and five cosmic-terms took the pool to
// 120/128. The entry is all-zero when empty (`link_to` is biased by one), so
// the table lands in .bss instead of the kernel image.
// 512 -> 4096 (lane ffoom, 2026-10-14): Firefox playing YouTube on the
// laptop (iris: Firefox turns on its dma-buf paths for Mesa/Intel) ran the
// pool dry — `[VFS] global tmpfs pool FULL ... at 512` — with 14 GB free, and
// every later memfd_create(), i.e. every Firefox IPC shared-memory segment,
// failed with ENOSPC, which Firefox reports as NS_ERROR_OUT_OF_MEMORY. Every
// memfd and every exported dma-buf in the system is one slot here (Linux has
// no such pool; its tmpfs inode limit is half of RAM in pages). ~2.5 KiB of
// BSS per entry (the xattr arena dominates), so 4096 is ~10 MiB.
// Pool size and entry layout live in `tmppool` (lane tmpfast): the pool owns a
// name index, a parent-hash array, an in-use bitmap and per-inode alias counts,
// and keeps the 2 KiB xattr arenas out of the hot entry.
use tmppool::{TmpFileEntry, TmpPool, MAX_TMP_FILES, MAX_TMP_PATH, TMP_LINK_MAX};
/// Largest offset a tmpfs file may reach (EFBIG beyond it). The page index is
/// a dense `Vec` grown only as far as the highest *populated* page — holes
/// after it are implicit — so this bounds the worst-case index (a single byte
/// written at the very end) at 32 MiB of kernel heap, allocated fallibly.
const MAX_TMP_FILE_SIZE: usize = 16 << 30;

static TMP_FILES: Mutex<TmpPool> = Mutex::new(TmpPool::new());

/// The `/proc` path each `/tmp/.psys_<idx>` snapshot of a generated system file
/// was opened from, so the open descriptor names that path (readlink of
/// `/proc/self/fd/N`, and the mount(2) targets crun passes that way) and not
/// the synthetic one. Entries die with `gen_proc_system_file` reusing the slot.
/// `/proc/sys/kernel/random/boot_id`, drawn on first read.
static BOOT_ID_UUID: Mutex<Option<[u8; 16]>> = Mutex::new(None);
static PSYS_ORIGIN: Mutex<alloc::vec::Vec<(usize, alloc::vec::Vec<u8>)>> = Mutex::new(alloc::vec::Vec::new());

// ── Shared VMO store (K1 shared file-backed mmap) ────────────────────────────
//
// A tmpfs/memfd inode is "promoted" into a `TmpVmo` — a list of real 4 KiB
// buddy frames that become the single source of truth for the file's bytes.
// read/write/ftruncate operate on these frames, and `MAP_SHARED` mmap installs
// the *same* frames into user page tables (see `vmo_acquire_frames` +
// `AddressSpace::map_shared_frames`), so two mappings — or a mapping and a
// read()/write() — of the same file alias the same physical pages. This is
// what makes cross-process shared memory (wl_shm pools, memfd handoff over
// SCM_RIGHTS) genuinely coherent.
//
// Keyed by the data-owning slot (`tmp_owner`), so hard links and fds passed
// over SCM_RIGHTS share one VMO. Frames are **untracked** in `pageref` on
// allocation (implicit refcount 1 = "the VMO owns it"); each shared mapping
// takes one `pageref::inc` (in `vmo_acquire_frames`, under this lock), released
// by munmap/exit via the existing lazy-VMA teardown. VMO teardown drops the
// implicit ref with `unref_or_free`, freeing a frame only once the last mapping
// is also gone.
//
// Every regular tmpfs file stores its bytes here (2026-10-01; before that a
// file that was never memfd'd or MAP_SHARED-mapped lived in a 32 KiB inline
// array). The VMO is created lazily on the first page a file needs, so an
// empty or never-written file costs nothing beyond its pool entry.
//
// Pages are allocated on demand and may be absent: `pages[i] == 0`, and every
// index at or past `pages.len()`, is a *hole* that reads as zeros and costs no
// memory — so `lseek` past EOF + write, and ftruncate-grow, are sparse exactly
// as on Linux tmpfs. Bytes past EOF inside an allocated page are don't-care:
// every operation that moves EOF forward zeroes the gap it exposes first
// (`vmo_zero_range`), and ftruncate-shrink zeroes the tail of the new last page.
//
// Accounting: every frame a non-borrowed VMO owns is counted in
// `TMP_PAGES_USED` from allocation until the VMO drops it (truncate, unlink of
// the last name, last close of an unlinked file). Page allocation driven by
// userspace (write, MAP_SHARED over a hole; ftruncate allocates nothing) is
// refused with ENOSPC once the count reaches `tmpfs_budget_pages()`.
// Kernel-generated /proc snapshots are counted but never refused, so a full
// /tmp cannot make /proc unreadable. The budget is shared by /tmp, /dev/shm,
// /run/user and memfds, which all live in this one pool.
struct TmpVmo {
    /// Physical frame for each 4 KiB page index; `0` (and any index past the
    /// end of the list) is a hole. Never sparse for a borrowed VMO. The list
    /// is decoupled from `len` both ways: a mapping larger than the file grows
    /// it without moving EOF, and a sparse file's trailing hole is simply not
    /// listed.
    pages: alloc::vec::Vec<usize>,
    /// Logical file size in bytes (EOF). Mirrored into `TmpFileEntry.len` so
    /// fstat/lseek/poll — which read `entry.len` — stay correct unchanged.
    len:  usize,
    /// `F_SEAL_*` bits. Only `F_SEAL_SHRINK` is enforced (in ftruncate).
    seals: u32,
    /// Seals are only permitted on memfd inodes; also gates `F_GET_SEALS`.
    is_memfd: bool,
    /// memfd_create's name (debug label; /proc/self/exe of a memfd exec).
    mname: [u8; 32],
    mnlen: u8,
    /// A **borrowed** VMO aliases physical frames it does not own — currently a
    /// PRIME/dmabuf export whose pages ARE a DRM dumb buffer's contiguous buddy
    /// block. Teardown (`vmo_free_slot`) must NOT free these frames: the DRM
    /// layer owns them and frees the whole order-N block via `buddy::free` on
    /// GEM_CLOSE/DESTROY_DUMB. Freeing them here would double-free and corrupt
    /// the buddy allocator (order-0 frees of an order-N block).
    borrowed: bool,
    /// For a dmabuf export: the GEM handle this VMO aliases, so PRIME_FD_TO_HANDLE
    /// resolves fd -> handle without a separate registry (auto-cleans on close).
    /// 0 = not a dmabuf.
    ///
    /// This is a *round-trip convenience only* and emphatically **not** what
    /// keeps the buffer alive — see `dmabuf_obj`. It will be replaced the day
    /// PRIME_FD_TO_HANDLE mints a handle for the *importing* open instead of
    /// echoing the exporter's, at which point a gem handle stops being
    /// meaningful across an fd at all.
    dmabuf_handle: u32,
    /// For a dmabuf export: the **BO object id** this VMO holds a reference on,
    /// as returned by `drivers::drm_device_interface::prime_export_acquire`.
    /// 0 = not a dmabuf export.
    ///
    /// The reference is held **per slot, not per fd**, which is why it lives
    /// here: `TMP_VMOS` is keyed by the data-owning slot, so dup, fork and
    /// SCM_RIGHTS copies of one dmabuf fd already share one slot, and that slot
    /// is destroyed exactly once, by `vmo_free_slot`. One reference per slot is
    /// therefore both sufficient and impossible to double-drop.
    ///
    /// An object id and never a gem handle, on purpose: a gem handle is retired
    /// by GEM_CLOSE/DESTROY_DUMB while this fd is still open — that retirement
    /// is the exact sequence the reference exists to survive — and for a blob
    /// it is per-open besides.
    dmabuf_obj: u32,
    /// Identity handed to the address spaces that map this VMO (`VmaRegion::vmo`):
    /// `serial << VMO_ID_IDX_BITS | slot`, assigned at the first `MAP_SHARED`
    /// mapping (0 = never mapped). Unique for the boot, so a mapping that
    /// outlives the slot never names its next tenant. See [`vmo_id_of`].
    id: u64,
    /// Address-space VMAs naming this object (`mm::vmm::vmo_retain`/`vmo_release`).
    /// A VMO destroyed while this is non-zero lives on in `TMP_ZOMBIES`, so the
    /// pages a lazy mapping has not faulted in yet can still be found.
    maps: u32,
}

/// Objects whose last descriptor/name went away while VMAs still map them
/// (the fd is closed right after mmap by most shm users). Faults of those
/// mappings are served from here; the entry dies with its last VMA.
static TMP_ZOMBIES: Mutex<alloc::vec::Vec<TmpVmo>> = Mutex::new(alloc::vec::Vec::new());

/// Low bits of a VMO identity: the TMP_VMOS slot.
const VMO_ID_IDX_BITS: u32 = 16;
static VMO_ID_SERIAL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// The identity of the VMO in slot `idx`, assigning one if it has none.
fn vmo_assign_id(vmo: &mut TmpVmo, idx: usize) -> u64 {
    if vmo.id == 0 {
        let serial = VMO_ID_SERIAL.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        vmo.id = (serial << VMO_ID_IDX_BITS) | idx as u64;
    }
    vmo.id
}

/// Slot of a VMO identity (the VMO there must still carry `id`).
fn vmo_id_slot(id: u64) -> usize { (id & ((1u64 << VMO_ID_IDX_BITS) - 1)) as usize }

/// Keyed by data-owning pool slot. Grows ahead of the pool, so it always
/// holds at least `TMP_FILES.len()` entries (see `tmp_free_slot`).
static TMP_VMOS: Mutex<alloc::vec::Vec<Option<TmpVmo>>> = Mutex::new(alloc::vec::Vec::new());
const _: () = assert!(MAX_TMP_FILES <= 1usize << VMO_ID_IDX_BITS);

/// A free tmpfs slot, growing the pool when every slot is in use: TMP_VMOS
/// first (so it always covers every pool slot), then the pool. `None` once
/// `MAX_TMP_FILES` slots exist and all are in use, or the heap is out of
/// memory; callers answer ENOSPC / `tmpfs_pool_full` as before. Caller holds
/// TMP_FILES and must NOT hold TMP_VMOS (order TMP_FILES -> TMP_VMOS).
fn tmp_free_slot(tmp: &mut TmpPool) -> Option<usize> {
    if let Some(i) = tmp.free_slot() { return Some(i); }
    let cap = tmp.next_cap()?;
    {
        let mut vmos = TMP_VMOS.lock();
        if vmos.len() < cap {
            let add = cap - vmos.len();
            if vmos.try_reserve_exact(add).is_err() { return None; }
            vmos.resize_with(cap, || None);
        }
    }
    if !tmp.grow_to(cap) { return None; }
    tmp.free_slot()
}

// Linux memfd/fcntl seal bits (values fixed by the ABI).
const F_ADD_SEALS:   usize = 1033;
const F_GET_SEALS:   usize = 1034;
const F_SEAL_SEAL:   u32   = 0x0001;
const F_SEAL_SHRINK: u32   = 0x0002;
const F_SEAL_GROW:   u32   = 0x0004;
const F_SEAL_WRITE:  u32   = 0x0008;
const F_SEAL_FUTURE_WRITE: u32 = 0x0010;
const F_SEAL_EXEC:   u32   = 0x0020;
const F_SEAL_KNOWN:  u32   = 0x003f;

/// Allocate one zeroed 4 KiB buddy frame for a VMO. Zeroing touches HHDM
/// (kernel) memory only, never user memory.
fn vmo_alloc_zeroed_frame() -> Option<usize> {
    let phys = mm::buddy::alloc(0)?;
    unsafe { (mm::phys_to_virt(phys) as *mut u8).write_bytes(0, 4096); }
    Some(phys)
}

/// Frames currently owned by non-borrowed tmpfs VMOs (see the accounting note
/// above `TmpVmo`).
static TMP_PAGES_USED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The tmpfs size limit in pages: half of RAM, Linux's default `size=`.
fn tmpfs_budget_pages() -> usize {
    (mm::buddy::total_pages() / 2).max(1024)
}

/// Pages currently charged to tmpfs (for statfs and the tests).
pub fn tmpfs_used_pages() -> usize {
    TMP_PAGES_USED.load(core::sync::atomic::Ordering::Relaxed)
}

/// Allocate one charged tmpfs page. `check_budget` is false only for
/// kernel-generated snapshots. Err is a negative errno: ENOSPC for the
/// budget, ENOMEM when the buddy allocator itself is empty.
fn tmp_page_alloc(check_budget: bool) -> Result<usize, i64> {
    use core::sync::atomic::Ordering;
    if check_budget && TMP_PAGES_USED.load(Ordering::Relaxed) >= tmpfs_budget_pages() {
        return Err(-28); // ENOSPC
    }
    let phys = vmo_alloc_zeroed_frame().ok_or(-12i64)?; // ENOMEM
    TMP_PAGES_USED.fetch_add(1, Ordering::Relaxed);
    Ok(phys)
}

/// Drop a VMO's own reference on one of its pages and uncharge it. A frame a
/// live mapping still holds survives until that mapping goes (`pageref`).
fn tmp_page_put(phys: usize) {
    if phys == 0 { return; }
    mm::pageref::unref_or_free(phys, 0);
    TMP_PAGES_USED.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
}

/// What a hole reads as.
static TMP_ZERO_PAGE: [u8; 4096] = [0u8; 4096];

impl TmpVmo {
    /// An empty, owning VMO (no pages: the whole file is a hole).
    const fn new_owned(is_memfd: bool) -> Self {
        TmpVmo { pages: alloc::vec::Vec::new(), len: 0, seals: 0, is_memfd, mname: [0; 32], mnlen: 0,
                 borrowed: false, dmabuf_handle: 0, dmabuf_obj: 0, id: 0, maps: 0 }
    }

    /// The frame backing page `i`, or 0 for a hole.
    fn page(&self, i: usize) -> usize {
        self.pages.get(i).copied().unwrap_or(0)
    }

    /// Frames this VMO actually holds (holes excluded) — `st_blocks / 8`.
    fn resident_pages(&self) -> usize {
        self.pages.iter().filter(|&&p| p != 0).count()
    }

    /// Make page `i` real, allocating it (zeroed) if it is a hole. A borrowed
    /// VMO's page list is immutable (see `vmo_acquire_frames`): any hole in it
    /// is ENOSPC. The index grows fallibly — a write at a huge offset gets
    /// ENOMEM, never a kernel panic on a failed `Vec` reallocation.
    fn ensure_page(&mut self, i: usize, check_budget: bool) -> Result<usize, i64> {
        if i < self.pages.len() && self.pages[i] != 0 { return Ok(self.pages[i]); }
        if self.borrowed { return Err(-28); }
        if i >= self.pages.len() {
            if self.pages.try_reserve(i + 1 - self.pages.len()).is_err() { return Err(-12); }
            self.pages.resize(i + 1, 0);
        }
        let phys = tmp_page_alloc(check_budget)?;
        self.pages[i] = phys;
        Ok(phys)
    }

    /// Free every page wholly at or past byte `from` (rounded up to a page)
    /// and drop them from the index. Never called on a borrowed VMO.
    fn free_from(&mut self, from: usize) {
        let keep = (from + 4095) / 4096;
        if keep >= self.pages.len() { return; }
        for &phys in &self.pages[keep..] { tmp_page_put(phys); }
        self.pages.truncate(keep);
        // Drop trailing holes too, so the index never outlives the data.
        while self.pages.last() == Some(&0) { self.pages.pop(); }
    }
}

/// Copy `n` bytes of file content starting at logical offset `off` into
/// kernel/user `dst`, page by page; holes (no VMO at all, a 0 entry, or past
/// the page list) read as zeros.
///
/// `dst` is the caller's buffer and TMP_FILES/TMP_VMOS are held, so the copy
/// is fault-tolerant (see `sched::uaccess`): false = a fault stopped it.
unsafe fn vmo_copy_out(vmo: Option<&TmpVmo>, off: usize, dst: *mut u8, n: usize) -> bool {
    let mut done = 0usize;
    while done < n {
        let pos  = off + done;
        let page = pos / 4096;
        let poff = pos % 4096;
        let cnt  = (4096 - poff).min(n - done);
        let phys = vmo.map_or(0, |v| v.page(page));
        let src  = if phys == 0 { TMP_ZERO_PAGE.as_ptr().add(poff) }
                   else { (mm::phys_to_virt(phys) + poff) as *const u8 };
        if sched::uaccess::copy_raw(dst.add(done), src, cnt) != 0 { return false; }
        done += cnt;
    }
    true
}

/// Copy `n` bytes from `src` into the file at logical offset `off`,
/// allocating hole pages on the way. Returns the bytes copied — fewer than
/// `n` only when a page could not be allocated after some progress — or a
/// negative errno when nothing was copied (ENOSPC/ENOMEM from the allocator,
/// EFAULT from a faulting `src`).
unsafe fn vmo_copy_in(vmo: &mut TmpVmo, off: usize, src: *const u8, n: usize,
                      check_budget: bool) -> Result<usize, i64> {
    let mut done = 0usize;
    while done < n {
        let pos  = off + done;
        let page = pos / 4096;
        let poff = pos % 4096;
        let cnt  = (4096 - poff).min(n - done);
        let phys = match vmo.ensure_page(page, check_budget) {
            Ok(p) => p,
            Err(e) => return if done > 0 { Ok(done) } else { Err(e) },
        };
        let dst = (mm::phys_to_virt(phys) + poff) as *mut u8;
        if sched::uaccess::copy_raw(dst, src.add(done), cnt) != 0 { return Err(-14); }
        done += cnt;
    }
    Ok(done)
}

/// Zero bytes `[from, to)` of whatever frames back that range (HHDM only);
/// holes are already zero. Used whenever EOF moves forward, so stale bytes
/// past the old EOF inside its page never become file content. Bounded by
/// the page list, so a hole gigabytes long costs nothing.
fn vmo_zero_range(vmo: &mut TmpVmo, from: usize, to: usize) {
    if vmo.borrowed { return; }
    let to = to.min(vmo.pages.len() * 4096);
    let mut pos = from;
    while pos < to {
        let page = pos / 4096;
        let poff = pos % 4096;
        let cnt  = (4096 - poff).min(to - pos);
        let phys = vmo.pages[page];
        if phys != 0 {
            unsafe { ((mm::phys_to_virt(phys) + poff) as *mut u8).write_bytes(0, cnt); }
        }
        pos += cnt;
    }
}

/// Set a regular file's size, Linux `truncate` semantics: growing exposes
/// zeros (sparse — no page is allocated), shrinking frees every page past the
/// new EOF and zeroes the tail of the new last page. Enforces F_SEAL_SHRINK
/// and refuses a borrowed (dmabuf) VMO. Returns 0 or a negative errno.
/// Caller holds TMP_FILES → TMP_VMOS and passes `owner`.
fn tmp_set_size(tmp: &mut TmpPool, vmos: &mut [Option<TmpVmo>], owner: usize,
                new_len: usize) -> i64 {
    if new_len > MAX_TMP_FILE_SIZE { return -27; } // EFBIG
    let old_len = tmp[owner].len;
    if let Some(vmo) = vmos[owner].as_mut() {
        // A borrowed dmabuf export is not resizable, in either direction.
        // Growing would append frames `vmo_free_slot` never frees; shrinking
        // would `unref_or_free` frames the DRM layer owns — order-0 frees out
        // of an order-N buddy block, i.e. allocator corruption. Linux does not
        // let you ftruncate a dmabuf either.
        if vmo.borrowed { return -1; } // EPERM
        if new_len < old_len && vmo.seals & F_SEAL_SHRINK != 0 { return -1; } // EPERM
        if new_len > old_len && vmo.seals & F_SEAL_GROW != 0 { return -1; } // EPERM
        if vmo.seals & F_SEAL_WRITE != 0 && new_len != old_len { return -1; } // EPERM
        if new_len > old_len {
            vmo_zero_range(vmo, old_len, new_len);
        } else if new_len < old_len {
            // Frames a live mapping still holds survive (unref_or_free), so
            // there is no use-after-free (Linux would SIGBUS — out of scope).
            vmo.free_from(new_len);
            let page_end = ((new_len + 4095) / 4096) * 4096;
            vmo_zero_range(vmo, new_len, page_end);
        }
        vmo.len = new_len;
    }
    tmp[owner].len = new_len;
    0
}

/// Replace a fresh slot's content with kernel bytes (a /proc, /sys or mtab
/// snapshot). Uncapped and never budget-refused; false only if the buddy
/// allocator is empty. Caller holds TMP_FILES; takes TMP_VMOS.
fn tmp_fill_kernel(tmp: &mut TmpPool, idx: usize, data: &[u8]) -> bool {
    let mut vmos = TMP_VMOS.lock();
    // A fresh slot has no VMO (every free site clears it); be defensive anyway.
    // (A slot is only reused after its VMO is torn down, so this never holds
    // a dmabuf reference to leak.)
    if let Some(old) = vmos[idx].take() { let _ = vmo_free_owned(old); }
    let mut vmo = TmpVmo::new_owned(false);
    let ok = data.is_empty()
        || unsafe { vmo_copy_in(&mut vmo, 0, data.as_ptr(), data.len(), false) } == Ok(data.len());
    if !ok { let _ = vmo_free_owned(vmo); return false; }
    vmo.len = data.len();
    tmp[idx].len = data.len();
    if !vmo.pages.is_empty() { vmos[idx] = Some(vmo); }
    true
}

/// Kernel-buffer read of a tmpfs file's content at `off` (loop devices, the
/// serial-dump diagnostic). Caller holds TMP_FILES. Returns bytes copied,
/// clamped to EOF.
fn tmp_read_kernel(tmp: &TmpPool, owner: usize, off: usize, out: &mut [u8]) -> usize {
    let len = tmp[owner].len;
    if off >= len { return 0; }
    let n = (len - off).min(out.len());
    let vmos = TMP_VMOS.lock();
    if !unsafe { vmo_copy_out(vmos[owner].as_ref(), off, out.as_mut_ptr(), n) } { return 0; }
    n
}

/// Kernel-buffer write into a tmpfs file at `off`, extending EOF (loop
/// devices). Caller holds TMP_FILES. Returns false on EFBIG/ENOSPC/ENOMEM or
/// a short write.
fn tmp_write_kernel(tmp: &mut TmpPool, owner: usize, off: usize, data: &[u8]) -> bool {
    let end = match off.checked_add(data.len()) { Some(e) if e <= MAX_TMP_FILE_SIZE => e, _ => return false };
    let mut vmos = TMP_VMOS.lock();
    let len = tmp[owner].len;
    let vmo = vmos[owner].get_or_insert_with(|| TmpVmo::new_owned(false));
    if off > len { vmo_zero_range(vmo, len, off); }
    let ok = unsafe { vmo_copy_in(vmo, off, data.as_ptr(), data.len(), true) } == Ok(data.len());
    if ok && end > len { vmo.len = end; tmp[owner].len = end; }
    ok
}

// ── The DRM dmabuf release hook ───────────────────────────────────────────────
//
// WHY A FUNCTION POINTER. `vfs-server` depends on `ipc`, `sched`, `mm`, `xattr`
// and `spin` — **not** on `drivers`, and it must not start to: the VFS is below
// the device layer and an edge the other way would be a cycle in spirit if not
// in cargo. So the DRM layer registers itself, once, at boot
// (`servers/drm/src/lib.rs::init`, which already depends on both). A null
// registration means "no DRM device on this build", and doing nothing is then
// exactly right, because nothing can have exported a dmabuf.
//
// WHY IT IS CALLED WITH NO LOCK HELD. `vmo_free_slot` runs from inode-teardown
// sites that hold `TMP_FILES`, and the hook lands in `blob_unref`, which takes
// `VIRTIO_GPU` and busy-spins on a device round-trip. Calling straight through
// would (a) hold a tmpfs lock across a device round-trip and (b) introduce a
// second lock order, TMP_FILES → VIRTIO_GPU, into a codebase that has one
// already. This project froze all four vCPUs once by taking a device/fault path
// under a held lock (82d0cc3) and has a standing invariant against the shape.
//
// So the plumbing is: `vmo_free_slot` RETURNS the object id it just dropped and
// calls nothing; each caller lets its `TMP_FILES` guard die and only then
// invokes `dmabuf_release`. The hook itself touches no user memory and takes no
// VFS lock, so the reverse order cannot arise either.
static DMABUF_RELEASE: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Register the DRM layer's BO-release entry point. Called once, from
/// `drm::init`, with `drivers::drm_device_interface::bo_release_exported`.
///
/// The callee MUST be safe to call with no VFS lock held and MUST NOT re-enter
/// the VFS — see the note above. Idempotent; the last registration wins.
pub fn set_drm_sysfs_override(f: fn(&[u8], &mut [u8]) -> Option<usize>) {
    DRM_SYSFS_OVERRIDE.store(f as usize, core::sync::atomic::Ordering::Release);
}

/// The DRM nodes' PCI identity files, `/sys/dev/char/226:{0,128}/device/<attr>`,
/// as answered by a registered GPU backend (see `drivers::gpu_uapi`). These
/// paths are staged ON DISK with the virtio-gpu identity
/// (scripts/mkfs-f2fs-populated.py) and libdrm's drmGetDevices2 classifies
/// the device from them; a backend on real hardware (the NVIDIA nouveau-UAPI
/// layer) must answer its own vendor/device instead, or NVK never sees an
/// NVIDIA device.
///
/// THE OVERLAY IS NARROW ON PURPOSE (see the 2026-09-15 /sys shadowing
/// regression that blacked out the desktop): only those exact attribute
/// files, only while the hook answers `Some`, and never a directory — the
/// `device/drm` and `subsystem` entries drm-rs and libdrm stat stay on disk.
/// With no backend registered the hook answers `None` for everything and
/// every path resolves exactly as before.
static DRM_SYSFS_OVERRIDE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

fn drm_sysfs_attr(path: &[u8], out: &mut [u8]) -> Option<usize> {
    if !path.starts_with(b"/sys/dev/char/226:") {
        return None;
    }
    let f = DRM_SYSFS_OVERRIDE.load(core::sync::atomic::Ordering::Acquire);
    if f == 0 {
        return None;
    }
    let f: fn(&[u8], &mut [u8]) -> Option<usize> = unsafe { core::mem::transmute(f) };
    f(path, out)
}

fn is_drm_sysfs_override(path: &[u8]) -> bool {
    if !path.starts_with(b"/sys/dev/char/226:") {
        return false;
    }
    let mut buf = [0u8; 512];
    drm_sysfs_attr(path, &mut buf).is_some()
}

/// The kernel command line, for `/proc/cmdline` (set once at boot).
static KERNEL_CMDLINE: Mutex<Option<&'static str>> = Mutex::new(None);

pub fn set_kernel_cmdline(s: &'static str) {
    *KERNEL_CMDLINE.lock() = Some(s);
}

pub fn set_dmabuf_release(f: fn(u32)) {
    DMABUF_RELEASE.store(f as usize, core::sync::atomic::Ordering::Release);
}

/// Drop the DRM-side reference an exporting VMO slot held. **Call only after
/// every VFS guard has been dropped.** `obj == 0` (not a dmabuf) is a no-op, as
/// is an unregistered hook.
fn dmabuf_release(obj: u32) {
    if obj == 0 { return; }
    let f = DMABUF_RELEASE.load(core::sync::atomic::Ordering::Acquire);
    if f == 0 { return; }
    let f: fn(u32) = unsafe { core::mem::transmute::<usize, fn(u32)>(f) };
    f(obj);
}

/// Release a VMO slot's implicit references and clear it. Called from every
/// inode-free site (`tmp_drop_name`, `tmp_release_ephemeral`) with `TMP_FILES`
/// already held — the lock order is TMP_FILES → TMP_VMOS. A frame still held by
/// a live mapping survives (its `pageref` > 1); the last holder frees it.
///
/// **Returns the dmabuf object id this slot held a reference on**, or `None`
/// for every ordinary slot. It does not drop that reference itself, because it
/// cannot: it runs under `TMP_FILES` and the drop reaches the device. Every
/// caller must pass the value to `dmabuf_release` *after* releasing its guard.
/// Returning it — rather than taking a new lock here — is what keeps this
/// function lock-order-neutral.
#[must_use = "the dmabuf reference must be dropped, with TMP_FILES released"]
fn vmo_free_slot(owner: usize) -> Option<u32> {
    NSMAP_OPENERS.lock()[owner] = None; // drops the captured namespace reference
    let vmo = TMP_VMOS.lock()[owner].take()?;
    vmo_free_owned(vmo)
}

/// Tear down a VMO already taken out of `TMP_VMOS` (see `vmo_free_slot`).
#[must_use = "the dmabuf reference must be dropped, with TMP_FILES released"]
fn vmo_free_owned(vmo: TmpVmo) -> Option<u32> {
    // Borrowed (dmabuf) VMOs alias frames the DRM layer owns — dropping the
    // slot must forget the page list WITHOUT freeing; the buddy block is
    // freed by the DRM layer once the LAST reference goes, and this slot
    // was one of them. Any live mmap of the fd already balances its own
    // pageref inc/dec (the frames start untracked so a mapping's unref
    // never reaches the free branch).
    if vmo.borrowed {
        return if vmo.dmabuf_obj != 0 { Some(vmo.dmabuf_obj) } else { None };
    }
    // VMAs still name this object (lazy shared mappings with unfaulted pages):
    // keep it findable by id until the last of them goes.
    if vmo.maps > 0 && vmo.id != 0 {
        TMP_ZOMBIES.lock().push(vmo);
        return None;
    }
    for phys in vmo.pages { tmp_page_put(phys); }
    None
}

/// One more VMA names VMO `id` (`mm::vmm::VmoRefFn`). Takes only the leaf
/// VMO locks.
pub fn vmo_retain(id: u64) {
    if id == 0 { return; }
    let idx = vmo_id_slot(id);
    {
        let mut vmos = TMP_VMOS.lock();
        if let Some(v) = vmos.get_mut(idx).and_then(|v| v.as_mut()).filter(|v| v.id == id) {
            v.maps += 1;
            return;
        }
    }
    if let Some(v) = TMP_ZOMBIES.lock().iter_mut().find(|v| v.id == id) { v.maps += 1; }
}

/// A VMA naming VMO `id` went away; the last one frees a zombie object.
pub fn vmo_release(id: u64) {
    if id == 0 { return; }
    let idx = vmo_id_slot(id);
    {
        let mut vmos = TMP_VMOS.lock();
        if let Some(v) = vmos.get_mut(idx).and_then(|v| v.as_mut()).filter(|v| v.id == id) {
            v.maps = v.maps.saturating_sub(1);
            return;
        }
    }
    let dead = {
        let mut z = TMP_ZOMBIES.lock();
        match z.iter().position(|v| v.id == id) {
            Some(i) => {
                z[i].maps = z[i].maps.saturating_sub(1);
                if z[i].maps == 0 { Some(z.swap_remove(i)) } else { None }
            }
            None => None,
        }
    };
    if let Some(v) = dead { for phys in v.pages { tmp_page_put(phys); } }
}

/// Prepare a lazy `MAP_SHARED` mapping of `[off, off+len)` of the tmpfs/memfd
/// behind `fd`: promote the file to a VMO if it has none and return the
/// object id, WITHOUT allocating, zeroing or pinning any frame. The pages
/// are found at fault time (`vmo_fault_frame`). None (caller maps eagerly)
/// for a dmabuf/borrowed object, or a range reaching past EOF (a fault
/// there would be SIGBUS, which the eager path never raised).
pub fn vmo_lazy_prepare(pid: u32, fd: usize, off: usize, len: usize) -> Option<u64> {
    if len == 0 || off % 4096 != 0 { return None; }
    let idx = tmpfile_owner_of(pid, fd)?;
    let end = off.checked_add(len)?.checked_add(4095)? & !4095;
    let tmp = TMP_FILES.lock();
    let mut vmos = TMP_VMOS.lock();
    if vmos[idx].is_none() {
        let mut v = TmpVmo::new_owned(false);
        v.len = tmp[idx].len;
        vmos[idx] = Some(v);
    }
    let vmo = vmos[idx].as_mut().unwrap();
    if vmo.borrowed || ((vmo.len + 4095) & !4095) < end { return None; }
    Some(vmo_assign_id(vmo, idx))
}

/// Resolve `(pid, fd)` to the owning tmpfs slot index, or `None` if the fd is
/// not an open `TmpFile`.
///
/// `pid` is canonicalised to the TGID for the same reason `vfs_get_node_kind`
/// does it: fd tables are per-process, not per-thread. Without this, every
/// caller that hands us a bare `current_pid()` — `mark_memfd` and, fatally,
/// `vmo_acquire_frames` — found no table at all on a non-leader thread.
/// `sys_mmap` turns that `None` into ENOMEM, which is exactly the
/// `Failed to create memory pool.: Create(Os { code: 12, kind: OutOfMemory })`
/// that kills the `smithay-clipboard` thread in every COSMIC session: a spawned
/// thread's `MultiPool::new` reaches `MmapMut::map_mut` on a memfd it created
/// itself. `install_dmabuf_vmo` and `dmabuf_handle_of` already pass a TGID
/// explicitly (the same lesson, learnt once in 36f62d0); `tgid_of` is
/// idempotent, so they are unaffected.
fn tmpfile_owner_of(pid: u32, fd: usize) -> Option<usize> {
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    let tbl = find_tbl(pid, &mut tbls)?;
    if !tbl.fds.used(fd) { return None; }
    match tbl.fds[fd].kind {
        VnodeKind::TmpFile { idx, .. } => Some(idx),
        _ => None,
    }
}

/// [GAP2] Public wrapper over `tmpfile_owner_of` for the instrumentation.
pub fn gap2_tmpfile_idx(pid: u32, fd: usize) -> Option<usize> {
    tmpfile_owner_of(pid, fd)
}

/// [GAP2] Stable small tag per VnodeKind, for one-line traces.
pub fn gap2_kind_tag(k: &Option<VnodeKind>) -> usize {
    match k {
        None => 0,
        Some(VnodeKind::None) => 1,
        Some(VnodeKind::RamFile { .. }) => 2,
        Some(VnodeKind::TmpFile { .. }) => 3,
        Some(VnodeKind::MountedFile { .. }) => 4,
        Some(VnodeKind::Pipe { .. }) => 5,
        Some(VnodeKind::DynamicDevice { .. }) => 6,
        Some(_) => 7,
    }
}

/// [GAP2] Read-only content probe of a tmpfs/memfd VMO.
/// Returns `(npages, first_phys, checksum, vmo_len)`.
///
/// SAFE FROM IRQ CONTEXT, and only because of two properties:
///   * `try_lock` ONLY. `TMP_VMOS` is taken from syscall context with interrupts
///     enabled, so a blocking `lock()` from the timer tick would deadlock
///     against a holder preempted on this same CPU.
///   * It reads the frames through `mm::phys_to_virt` — the HHDM identity map,
///     i.e. kernel memory that is always present. It never touches a user
///     virtual address, so it cannot demand-page and cannot re-enter the fault
///     handler (project rule: never touch user memory from IRQ/lock context).
pub fn vmo_debug_probe(idx: usize) -> Option<(usize, usize, u64, usize)> {
    let vmos = TMP_VMOS.try_lock()?;
    let vmo = vmos.get(idx)?.as_ref()?;
    let np = vmo.pages.len();
    if np == 0 { return Some((0, 0, 0, vmo.len)); }
    // Checksum the WHOLE buffer. Do not "optimise" this to the first page: the
    // applet's clock glyphs are vertically centred (rows 5..26 of 32 at an
    // 880-byte stride), so page 0 is uniform background and a page-0-only
    // checksum would be constant even while the clock ticks.
    let total = vmo.len.min(np * 4096);
    let mut sum: u64 = 0;
    let mut off = 0usize;
    while off + 4 <= total {
        let phys = vmo.pages[off / 4096];
        if phys != 0 {
            let p = (mm::phys_to_virt(phys) + (off % 4096)) as *const u32;
            sum = sum.wrapping_mul(31)
                     .wrapping_add(unsafe { core::ptr::read_volatile(p) } as u64);
        }
        off += 4;
    }
    Some((np, vmo.pages[0], sum, vmo.len))
}

/// Mark the tmpfs inode behind `fd` as a memfd: create an empty seal-capable
/// VMO on its owner slot. Called by `sys_memfd_create` right after open.
pub fn mark_memfd(pid: u32, fd: usize, name: &[u8]) {
    let idx = match tmpfile_owner_of(pid, fd) { Some(i) => i, None => return };
    // Lock order TMP_FILES -> TMP_VMOS, even though only `len` is read.
    let tmp = TMP_FILES.lock();
    let mut vmos = TMP_VMOS.lock();
    let n = name.len().min(32);
    match vmos[idx].as_mut() {
        Some(vmo) => { vmo.is_memfd = true; vmo.mname[..n].copy_from_slice(&name[..n]); vmo.mnlen = n as u8; }
        None => {
            let mut v = TmpVmo::new_owned(true);
            v.len = tmp[idx].len;
            v.mname[..n].copy_from_slice(&name[..n]); v.mnlen = n as u8;
            vmos[idx] = Some(v);
        }
    }
}

// ── memfd as the running executable ─────────────────────────────────────────
// fexecve(memfd) / execve("/proc/self/fd/N"): the nameless tmpfs inode keeps
// living as the task's /proc/self/exe ("/memfd:<name> (deleted)"), so crun's
// "am I already the sealed clone" check (open /proc/self/exe, F_GET_SEALS)
// sees the memfd after the close-on-exec sweep. The pin is one TMP_FDREFS
// count; it is dropped when the tgid's exe path stops being a memfd one
// (re-exec, exit), lazily, by `exe_pin_gc`.

/// A pending pin: holds the slot alive until committed or dropped.
pub struct ExeRef { idx: usize, name: [u8; 32], nlen: usize, live: bool }

impl ExeRef {
    /// The exe path string, "/memfd:<name> (deleted)".
    pub fn path(&self) -> alloc::vec::Vec<u8> {
        let mut v = alloc::vec::Vec::from(&b"/memfd:"[..]);
        v.extend_from_slice(&self.name[..self.nlen]);
        v.extend_from_slice(b" (deleted)");
        v
    }
}
impl Drop for ExeRef {
    fn drop(&mut self) {
        if self.live {
            TMP_FDREFS[self.idx].fetch_update(atomic::Ordering::AcqRel, atomic::Ordering::Acquire,
                |n| Some(n.saturating_sub(1))).ok();
            tmp_release_ephemeral(self.idx);
        }
    }
}

static EXE_PINS: spin::Mutex<[Option<(u32, ExeRef)>; 16]> =
    spin::Mutex::new([const { None }; 16]);

/// Pin the memfd behind `fd` of `pid` (None if it is not a memfd).
pub fn memfd_exe_take(pid: u32, fd: usize) -> Option<ExeRef> {
    let idx = tmpfile_owner_of(pid, fd)?;
    let tmp = TMP_FILES.lock();
    let vmos = TMP_VMOS.lock();
    let vmo = vmos[idx].as_ref()?;
    if !vmo.is_memfd || !tmp[idx].in_use() { return None; }
    let mut name = [0u8; 32];
    name[..vmo.mnlen as usize].copy_from_slice(&vmo.mname[..vmo.mnlen as usize]);
    let nlen = vmo.mnlen as usize;
    TMP_FDREFS[idx].fetch_add(1, atomic::Ordering::AcqRel);
    Some(ExeRef { idx, name, nlen, live: true })
}

/// Record `r` as `tgid`'s exe memfd, releasing what it pinned before.
pub fn exe_pin_commit(tgid: u32, r: ExeRef) {
    exe_pin_gc(tgid);
    let mut old = None;
    {
        let mut t = EXE_PINS.lock();
        let mut slot = t.iter().position(|e| matches!(e, Some((g, _)) if *g == tgid));
        if slot.is_none() { slot = t.iter().position(|e| e.is_none()); }
        if let Some(i) = slot { old = t[i].replace((tgid, r)); }
    }
    drop(old);
}

/// fork: the child's exe path is inherited, so it inherits the pin too (own
/// TMP_FDREFS count, dropped by `exe_pin_gc` once it execs something else).
pub fn exe_pin_inherit(parent: u32, child: u32) {
    let mut t = EXE_PINS.lock();
    let (idx, name, nlen) = match t.iter().find_map(|e| match e {
        Some((g, r)) if *g == parent => Some((r.idx, r.name, r.nlen)), _ => None }) {
        Some(x) => x,
        None => return,
    };
    let Some(slot) = t.iter().position(|e| e.is_none()) else { return };
    TMP_FDREFS[idx].fetch_add(1, atomic::Ordering::AcqRel);
    t[slot] = Some((child, ExeRef { idx, name, nlen, live: true }));
}

/// Release pins whose tgid no longer runs a memfd image (`keep` is spared).
pub fn exe_pin_gc(keep: u32) {
    let mut dead: alloc::vec::Vec<ExeRef> = alloc::vec::Vec::new();
    {
        let mut t = EXE_PINS.lock();
        for e in t.iter_mut() {
            let stale = match e {
                Some((g, _)) if *g != keep => {
                    let mut b = [0u8; 16];
                    !matches!(sched::exe_path(*g, &mut b), Some(n) if n >= 7 && &b[..7] == b"/memfd:")
                }
                _ => false,
            };
            if stale { if let Some((_, r)) = e.take() { dead.push(r); } }
        }
    }
    drop(dead);
}

/// A fresh open of `tgid`'s pinned exe memfd (`flags` as for open).
fn exe_memfd_kind(pid: u32, tgid: u32, flags: u32) -> Result<VnodeKind, i32> {
    let idx = {
        let t = EXE_PINS.lock();
        match t.iter().find_map(|e| match e { Some((g, r)) if *g == tgid => Some(r.idx), _ => None }) {
            Some(i) => i,
            None => return Err(-2),
        }
    };
    reopen_tmp_idx(pid, idx, flags)
}

/// Promote the tmpfs inode behind `fd` into a **borrowed dmabuf VMO** for the
/// GEM object `handle`. Called by the PRIME_HANDLE_TO_FD syscall intercept
/// right after opening an ephemeral `/tmp/dmabuf:<n>` node. Returns false on a
/// bad/non-tmpfs fd. The gem `handle` is stored so PRIME_FD_TO_HANDLE can
/// resolve fd -> handle. No user memory is touched.
///
/// TWO SHAPES, one entry point:
///
///   * `phys != 0` — the export aliases the BO's contiguous buddy block
///     (`phys .. phys + (1<<order)*4096`), so a MAP_SHARED mmap of this fd hits
///     the very same physical pages as the MAP_DUMB / VIRTGPU_MAP device
///     mapping (coherent). This is the dumb-buffer and guest-backed-blob case.
///   * `phys == 0` — the BO has NO guest pages: a pure host-side Venus blob.
///     The export still has to succeed, because Mesa calls `vkGetMemoryFdKHR`
///     for every swapchain image and fails swapchain creation outright if it
///     errors, and the fd's real job is to be a token that PRIME_FD_TO_HANDLE
///     turns back into a GEM handle. The page list is left EMPTY, and what
///     makes that safe rather than silently wrong is that a borrowed VMO's page
///     list is immutable: `vmo_acquire_frames`, the write path and
///     `handle_ftruncate` all refuse to grow it. Without those guards an mmap
///     would have been satisfied with freshly allocated zeroed anonymous
///     frames that have nothing to do with the host resource — a coherence bug
///     that reads as a Vulkan bug.
///
/// `len` is what fstat/lseek report. It is the resource's own size for a blob
/// and the whole buddy block for a dumb buffer (see `prime_export_acquire`);
/// it never exceeds the frames listed, so the clamp in the read path and the
/// bound in `vmo_acquire_frames` are both satisfiable.
///
/// The frames are NOT owned here (see `TmpVmo.borrowed`); the DRM layer frees
/// the whole order-N block once the last reference goes.
///
/// **`obj` transfers ownership of one DRM reference into this slot.** The
/// caller took it in `prime_export_acquire`; on success it is the slot's, and
/// `vmo_free_slot` hands it back for release. **On failure (`false`) the caller
/// still owns it** and must release it with `bo_release_exported`, or the
/// buffer leaks for the rest of the boot.
pub fn install_dmabuf_vmo(pid: u32, fd: usize, phys: usize, n_pages: usize, len: usize,
                          obj: u32, handle: u32) -> bool {
    // `n_pages` contiguous frames from `phys` (a buddy block's 2^order, or a
    // GPU backend's exact page count); `phys == 0` is a token-only export.
    let pages = if phys == 0 {
        alloc::vec::Vec::new()
    } else {
        let mut v = alloc::vec::Vec::with_capacity(n_pages);
        for i in 0..n_pages { v.push(phys + i * 4096); }
        v
    };
    install_dmabuf_vmo_pages(pid, fd, pages, len, obj, handle)
}

/// [`install_dmabuf_vmo`] over an explicit page list, for a buffer whose
/// backing is not one contiguous range (a scattered virgl resource, L1.10).
/// Same ownership rules: the pages stay the exporter's (`borrowed`).
pub fn install_dmabuf_vmo_pages(pid: u32, fd: usize, pages: alloc::vec::Vec<usize>, len: usize,
                                obj: u32, handle: u32) -> bool {
    let idx = match tmpfile_owner_of(pid, fd) { Some(i) => i, None => return false };
    // Lock order FD_TABLES -> TMP_FILES -> TMP_VMOS (FD_TABLES already released
    // by tmpfile_owner_of). Set the inode length so fstat/lseek report the
    // buffer size (GBM/EGL fstat the dmabuf to validate it; Mesa's kms_swrast
    // PRIME importer lseeks to SEEK_END and gives up if that fails).
    let mut tmp = TMP_FILES.lock();
    let mut vmos = TMP_VMOS.lock();
    // Never overwrite a live VMO. The slot belongs to an ephemeral node the
    // caller opened one instruction ago, so this cannot fire today — but if it
    // ever did, the displaced slot's DRM reference would be dropped on the
    // floor and its buffer pinned for the rest of the boot. Refusing hands the
    // caller back a failure it already knows how to unwind.
    if vmos[idx].is_some() {
        return false;
    }
    tmp[idx].len = len;
    vmos[idx] = Some(TmpVmo {
        pages, len, seals: 0, is_memfd: true, mname: [0; 32], mnlen: 0,
        borrowed: true, dmabuf_handle: handle, dmabuf_obj: obj, id: 0, maps: 0,
    });
    true
}

/// Resolve a dmabuf fd back to the GEM handle it was exported from
/// (PRIME_FD_TO_HANDLE). `None` if the fd is not a borrowed dmabuf VMO.
pub fn dmabuf_handle_of(pid: u32, fd: usize) -> Option<u32> {
    let idx = tmpfile_owner_of(pid, fd)?;
    let vmos = TMP_VMOS.lock();
    match vmos[idx].as_ref() {
        Some(vmo) if vmo.borrowed && vmo.dmabuf_handle != 0 => Some(vmo.dmabuf_handle),
        _ => None,
    }
}

/// The BO **object** id a dmabuf fd holds its reference on — the lifetime
/// identity `prime_export_acquire` returned, not the exporter's gem handle.
/// This is what PRIME_FD_TO_HANDLE hands to `prime_import_blob` so the
/// importing open gets a handle of its own; the exporter's handle number is
/// meaningless to any other open (see `BlobHandle::owner`). `None` if the fd is
/// not a borrowed dmabuf VMO.
pub fn dmabuf_obj_of(pid: u32, fd: usize) -> Option<u32> {
    let idx = tmpfile_owner_of(pid, fd)?;
    let vmos = TMP_VMOS.lock();
    match vmos[idx].as_ref() {
        Some(vmo) if vmo.borrowed && vmo.dmabuf_obj != 0 => Some(vmo.dmabuf_obj),
        _ => None,
    }
}

/// PRIME_HANDLE_TO_FD of an object that already has a live dma-buf: hand
/// `pid` a new fd on THAT dma-buf instead of minting a second one.
///
/// Linux keeps one `struct dma_buf` per GEM object (`obj->dma_buf`), so a
/// second export returns a new fd on the same file: same `st_ino`, and no
/// new kernel object. Here every export used to cost one of the global tmpfs
/// slots, so a client that exports a multi-plane buffer once per plane, or
/// re-exports a buffer it still holds (GBM/EGL do both), spent slots Linux
/// never spends — and that pool is shared by every memfd in the system.
///
/// Implemented as an SCM_RIGHTS-style hand-off from any fd that names the
/// slot (`export_fd` + `import_fd`), so the new fd shares the existing open
/// file description exactly as Linux's `get_file(dmabuf->file)` does, and
/// every reference count is taken by the code that already balances it.
/// The holder fd is re-validated after the export: if it was closed and its
/// number reused in between, the transfer is dropped and `None` returned, and
/// the caller falls back to a fresh export.
///
/// Returns the new fd, or `None` when no live dma-buf of `obj` is reachable
/// (or `pid`'s table is full — the fresh path then reports that properly).
pub fn dmabuf_share_existing(pid: u32, obj: u32, cloexec: bool) -> Option<isize> {
    if obj == 0 { return None; }
    let pid = sched::tgid_of(pid);
    // Find the slot, then one fd naming it. TMP_VMOS is a leaf, released
    // before any table is locked; each table is locked on its own (at most
    // one table lock at a time, see "Locking the fd tables"). A slot freed or
    // reused in between is caught by the re-validation after `export_fd`.
    let idx = TMP_VMOS.lock().iter().position(|v| v.as_ref()
        .map_or(false, |v| v.borrowed && v.dmabuf_obj == obj))?;
    let (holder, hfd) = {
        let mut found = None;
        for tr in all_tables() {
            let t = tr.lock();
            if t.dead { continue; }
            let hit = t.fds.iter_used()
                .find(|(_, f)| matches!(f.kind, VnodeKind::TmpFile { idx: i, .. } if i == idx))
                .map(|(fd, _)| fd);
            if let Some(fd) = hit {
                found = Some((t.pid, fd));
                break;
            }
        }
        found?
    };
    let tf = export_fd(holder, hfd)?;
    let same = matches!(tf.kind, VnodeKind::TmpFile { idx: i, .. } if i == idx)
        && TMP_VMOS.lock()[idx].as_ref().map_or(false, |v| v.borrowed && v.dmabuf_obj == obj);
    if !same {
        drop_transfer(tf);
        return None;
    }
    let r = import_fd(pid, tf, cloexec);
    if r < 0 {
        drop_transfer(tf);
        return None;
    }
    DMABUF_SHARED.fetch_add(1, atomic::Ordering::Relaxed);
    Some(r)
}

/// Exports answered by `dmabuf_share_existing` (for the periodic census).
static DMABUF_SHARED: atomic::AtomicU64 = atomic::AtomicU64::new(0);

/// The tmpfs pool at a glance: `MAX_TMP_FILES` slots shared by every /tmp,
/// /dev/shm and /run file, every memfd and every exported dma-buf in the
/// system. `None` when a lock is busy (IRQ-safe: `try_lock` only).
#[derive(Clone, Copy, Default)]
pub struct TmpfsCensus {
    pub used: usize,
    pub cap: usize,
    /// High-water mark of `used` over every census and every pool-full event.
    pub hwm: usize,
    /// Nameless inodes kept alive by an fd (unlinked memfds and dma-bufs).
    pub anon: usize,
    pub memfd: usize,
    pub dmabuf: usize,
    /// Exports that reused a live dma-buf instead of taking a slot.
    pub shared: u64,
    /// Times the pool has been found full (ENOSPC).
    pub full: u64,
    /// Nameless inodes that no fd and no queued (SCM_RIGHTS) fd refers to:
    /// nothing can ever free them. Always 0 unless a close path leaked one
    /// (a microsecond window between an fd's removal and its release aside).
    pub orphan: usize,
}

static TMP_HWM: atomic::AtomicUsize = atomic::AtomicUsize::new(0);
static TMP_FULL: atomic::AtomicU64 = atomic::AtomicU64::new(0);

pub fn tmpfs_census() -> Option<TmpfsCensus> {
    let mut c = TmpfsCensus { cap: MAX_TMP_FILES, ..Default::default() };
    // Referenced slots first: every table at one instant (the scan gate,
    // exclusive, as `tmp_open_fd_mask` takes it), then TMP_INFLIGHT, then
    // TMP_FILES — the established order, all try_lock (IRQ-safe).
    let refd = {
        let mut mask = TmpMask::new();
        {
            let tbls = FD_TABLES.try_lock()?;
            for tr in tbls.map.values() {
                let t = tr.try_lock()?;
                if t.dead { continue; }
                for (_, f) in t.fds.iter_used() {
                    if let VnodeKind::TmpFile { idx, .. } = f.kind { mask.set(idx); }
                }
            }
        }
        let fl = TMP_INFLIGHT.try_lock()?;
        for (i, &n) in fl.iter().enumerate() { if n > 0 { mask.set(i); } }
        mask
    };
    {
        let t = TMP_FILES.try_lock()?;
        for i in t.used_from(0) {
            c.used += 1;
            if t[i].ephemeral() {
                c.anon += 1;
                if !refd.has(i) && !tmp_fd_open(i) && tmp_alias_count(&t, i) == 0 { c.orphan += 1; }
            }
        }
    }
    {
        let v = TMP_VMOS.try_lock()?;
        for e in v.iter().flatten() {
            if e.borrowed && e.dmabuf_obj != 0 { c.dmabuf += 1; } else if e.is_memfd { c.memfd += 1; }
        }
    }
    c.hwm = TMP_HWM.fetch_max(c.used, atomic::Ordering::Relaxed).max(c.used);
    c.shared = DMABUF_SHARED.load(atomic::Ordering::Relaxed);
    c.full = TMP_FULL.load(atomic::Ordering::Relaxed);
    Some(c)
}

/// The tmpfs pool just ran dry: count it, and the first time, say WHO holds
/// it — one line of totals and one line of the heaviest holders (tmpfs fds
/// per process, dma-bufs split out), so the next boot log answers "leak or
/// legitimate load" without a rebuild. Callers hold no VFS lock.
fn tmpfs_pool_full() {
    TMP_HWM.store(MAX_TMP_FILES, atomic::Ordering::Relaxed);
    if TMP_FULL.fetch_add(1, atomic::Ordering::Relaxed) != 0 { return; }
    report_pool_full("tmpfs", MAX_TMP_FILES);
    if let Some(c) = tmpfs_census() {
        dbg_str("[VFS] tmpfs census: used ");
        dbg_dec(c.used); dbg_str("/"); dbg_dec(c.cap);
        dbg_str(" anon "); dbg_dec(c.anon);
        dbg_str(" memfd "); dbg_dec(c.memfd);
        dbg_str(" dmabuf "); dbg_dec(c.dmabuf);
        dbg_str(" named "); dbg_dec(c.used - c.anon);
        dbg_str("\n");
    }
    // Heaviest holders: (pid, tmpfs fds, of which dma-buf).
    let mut top = [(0u32, 0usize, 0usize); 8];
    for tr in all_tables() {
        let Some(t) = tr.try_lock() else { continue };
        if t.dead { continue; }
        let vmos = TMP_VMOS.try_lock();
        {
            let (mut n, mut d) = (0usize, 0usize);
            for (_, f) in t.fds.iter_used() {
                if let VnodeKind::TmpFile { idx, .. } = f.kind {
                    n += 1;
                    if vmos.as_ref().map_or(false, |v| v[idx].as_ref().map_or(false, |o| o.borrowed && o.dmabuf_obj != 0)) {
                        d += 1;
                    }
                }
            }
            if n == 0 { continue; }
            let min = (0..top.len()).min_by_key(|&i| top[i].1).unwrap_or(0);
            if n > top[min].1 { top[min] = (t.pid, n, d); }
        }
    }
    top.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    dbg_str("[VFS] tmpfs holders (pid=fds/dmabuf):");
    for (pid, n, d) in top.iter().filter(|e| e.1 > 0) {
        dbg_str(" "); dbg_dec(*pid as usize); dbg_str("="); dbg_dec(*n); dbg_str("/"); dbg_dec(*d);
    }
    dbg_str("\n");
}

/// Ensure the tmpfs/memfd file behind `fd` has a VMO whose frames cover the
/// page range `[off, off+len)`, pin **one** `pageref` reference per mapped
/// frame, and return those frames in order for
/// `AddressSpace::map_shared_frames`.
///
/// First `MAP_SHARED` of a plain tmpfs file **promotes** it: the inline
/// `data[..len]` bytes are migrated into freshly allocated frames. `off`/`len`
/// are page-aligned by the caller (`sys_mmap`). The `pageref::inc` happens
/// inside the VMO lock (pin-before-publish), so a concurrent ftruncate-shrink
/// cannot free a frame between listing and pinning. Returns `None` on bad fd /
/// non-tmpfs fd / OOM.
///
/// Lock order: FD_TABLES → TMP_FILES → TMP_VMOS, all leaf `spin::Mutex`es,
/// never nested under AS-`busy` or RUN_QUEUE. The caller maps the returned
/// frames *after* this returns (AS-`busy` taken second).
pub fn vmo_acquire_frames(pid: u32, fd: usize, off: usize, len: usize)
    -> Option<(alloc::vec::Vec<usize>, u64)>
{
    if len == 0 || off % 4096 != 0 { return None; }
    let idx = tmpfile_owner_of(pid, fd)?;
    let first = off / 4096;
    let n = (len + 4095) / 4096;
    let need_pages = first.checked_add(n)?;

    let tmp = TMP_FILES.lock();
    let mut vmos = TMP_VMOS.lock();

    // Every regular file's bytes already live in its VMO; one that has never
    // been written simply has none yet.
    let g2_promoted = vmos[idx].is_none();
    if vmos[idx].is_none() {
        let mut v = TmpVmo::new_owned(false);
        v.len = tmp[idx].len;
        vmos[idx] = Some(v);
    }

    let vmo = vmos[idx].as_mut().unwrap();
    let id = vmo_assign_id(vmo, idx);
    // A borrowed (dmabuf) VMO's page list is IMMUTABLE. Two reasons, and the
    // first one is a pre-existing leak this closes:
    //   * its frames belong to the DRM layer, and `vmo_free_slot` returns early
    //     for a borrowed VMO without freeing anything — so any frame appended
    //     here would be leaked for the lifetime of the system;
    //   * for a host-backed export (no guest pages at all) growing would
    //     satisfy the mmap with zeroed anonymous memory in place of the host
    //     resource, which is a coherence bug that presents as a Vulkan bug.
    // Refuse instead: sys_mmap turns None into ENOMEM, which is what "this fd
    // is a shareable token, not a mapping" looks like from userspace.
    if vmo.borrowed && need_pages > vmo.pages.len() { return None; }
    // Fill every hole in the mapped range — sparse pages, and pages past EOF
    // for a mapping larger than the file. This does NOT move EOF (`vmo.len`):
    // those frames are zero-filled. Partial growth is harmless (the VMO owns
    // and accounts every frame it got). The mapping aliases these frames, so
    // they must exist now; a full tmpfs refuses the mmap (ENOMEM).
    for p in first..need_pages {
        if vmo.ensure_page(p, true).is_err() { return None; }
    }

    // Pin and collect the mapped range's frames (inc under the lock).
    let mut out = alloc::vec::Vec::with_capacity(n);
    for p in first..need_pages {
        let phys = vmo.pages[p];
        mm::pageref::inc(phys);
        out.push(phys);
    }
    let g2_np = vmo.pages.len();
    let g2_vlen = vmo.len;
    let g2_memfd = vmo.is_memfd;
    let g2_borrow = vmo.borrowed;
    let g2_p0 = out.first().copied().unwrap_or(0);
    // Release TMP_VMOS/TMP_FILES *before* the serial write: a ~100-char line on
    // a polled UART is milliseconds of busy-wait, and these two mutexes gate
    // every tmpfs operation in the system.
    drop(vmos);
    drop(tmp);
    if mm::gap2::budget_ok() {
        mm::gap2::s("[G2ACQ] pid="); mm::gap2::h(pid as usize);
        mm::gap2::kv(" fd=",     fd);
        mm::gap2::kv(" idx=",    idx);
        mm::gap2::kv(" off=",    off);
        mm::gap2::kv(" len=",    len);
        mm::gap2::kv(" prom=",   g2_promoted as usize);
        mm::gap2::kv(" memfd=",  g2_memfd as usize);
        mm::gap2::kv(" borrow=", g2_borrow as usize);
        mm::gap2::kv(" np=",     g2_np);
        mm::gap2::kv(" vlen=",   g2_vlen);
        mm::gap2::kv(" p0=",     g2_p0);
        mm::gap2::nl();
    }
    // [GAP2] Arm the sampler on the panel's OWN bar pools so they are checksummed
    // on the same cadence as the applet's. 0x28000 = 1280*32*4 is exactly the
    // full-width 32px panel bar at the aarch64 mode. Deliberately OUTSIDE the
    // `budget_ok()` block above: arming must not depend on log budget remaining.
    if mm::gap2::ON && g2_memfd && g2_vlen == 0x28000 {
        mm::gap2::set_watch(idx);
    }
    Some((out, id))
}

/// `vmo_acquire_frames` by VMO identity (`mremap` growing a tmpfs/memfd
/// mapping, which has no descriptor at hand): the frames of pages
/// `[off, off+len)` of the object `id`, one `pageref` reference each.
/// None if the object is gone, is a dmabuf export that does not cover the
/// range, or tmpfs is full.
pub fn vmo_acquire_frames_by_id(id: u64, off: usize, len: usize) -> Option<alloc::vec::Vec<usize>> {
    if id == 0 || len == 0 || off % 4096 != 0 { return None; }
    let idx = vmo_id_slot(id);
    let first = off / 4096;
    let n = (len + 4095) / 4096;
    let need_pages = first.checked_add(n)?;
    let mut vmos = TMP_VMOS.lock();
    let vmo = vmos.get_mut(idx)?.as_mut().filter(|v| v.id == id)?;
    if vmo.borrowed && need_pages > vmo.pages.len() { return None; }
    for p in first..need_pages {
        if vmo.ensure_page(p, true).is_err() { return None; }
    }
    let mut out = alloc::vec::Vec::with_capacity(n);
    for p in first..need_pages {
        let phys = vmo.pages[p];
        mm::pageref::inc(phys);
        out.push(phys);
    }
    Some(out)
}

/// The page fault of a MAP_SHARED tmpfs/memfd mapping whose page a truncate
/// or hole punch took away, or that was never touched (`mm::vmm::VmoPageFn`):
/// the frames of object `id` holding bytes `off ..` into `out` (one page
/// each, one `pageref` reference each, holes allocated zeroed), all under one
/// lock round trip. Returns how many consecutive pages were acquired; the
/// first one failing (past the object's current end: SIGBUS; gone; out of
/// memory) gives 0, a later one just ends the batch (fault-around is best
/// effort). So a file that grows back is seen again by its old mappings, as
/// on Linux. Called with the address space unlocked.
pub fn vmo_fault_frames(id: u64, off: u64, out: &mut [usize]) -> usize {
    if id == 0 { return 0; }
    let idx = vmo_id_slot(id);
    let p0 = (off / 4096) as usize;
    fn fill(vmo: &mut TmpVmo, p0: usize, off: u64, out: &mut [usize]) -> usize {
        let end = ((vmo.len as u64) + 4095) & !4095;
        let mut n = 0;
        while n < out.len() {
            if off + (n as u64) * 4096 >= end { break; }
            match vmo.ensure_page(p0 + n, true) {
                Ok(phys) => { mm::pageref::inc(phys); out[n] = phys; n += 1; }
                Err(_) => break,
            }
        }
        n
    }
    {
        let mut vmos = TMP_VMOS.lock();
        if let Some(vmo) = vmos.get_mut(idx).and_then(|v| v.as_mut()).filter(|v| v.id == id) {
            return fill(vmo, p0, off, out);
        }
    }
    let mut z = TMP_ZOMBIES.lock();
    match z.iter_mut().find(|v| v.id == id) {
        Some(vmo) => fill(vmo, p0, off, out),
        None => 0,
    }
}

/// `/proc/<pid>/maps` name of the VMO `id` into `out` (its tmpfs path; a
/// memfd as Linux shows one, `/memfd:<name> (deleted)`). 0 if it is gone.
pub fn vmo_name(id: u64, out: &mut [u8]) -> usize {
    let idx = vmo_id_slot(id);
    if id == 0 { return 0; }
    let tmp = TMP_FILES.lock();
    let vmos = TMP_VMOS.lock();
    let memfd = match vmos.get(idx).and_then(|v| v.as_ref()) { Some(v) if v.id == id => v.is_memfd, _ => return 0 };
    let path = tmp[idx].path();
    let (name, suffix): (&[u8], &[u8]) = match path.strip_prefix(b"/tmp/memfd:") {
        Some(rest) if memfd => (rest, b" (deleted)"),
        _ => (path, b""),
    };
    let mut n = 0;
    let prefix: &[u8] = if memfd && path.starts_with(b"/tmp/memfd:") { b"/memfd:" } else { b"" };
    for &b in prefix.iter().chain(name.iter()).chain(suffix.iter()) {
        if n == out.len() { break; }
        out[n] = b; n += 1;
    }
    n
}

/// Identity and size of the tmpfs object behind `fd` if it has ever been
/// mapped shared (`ftruncate` must then drop the mapped pages it frees).
fn vmo_id_of(idx: usize) -> Option<(u64, usize)> {
    let vmos = TMP_VMOS.lock();
    vmos[idx].as_ref().filter(|v| v.id != 0).map(|v| (v.id, v.len))
}

/// Release frames pinned by `vmo_acquire_frames` when the mapping could not be
/// installed (drops the caller's `pageref::inc`). Only used for the
/// no-address-space edge; `map_shared_frames` releases them itself on the
/// mapping-failure path.
pub fn vmo_release_frames(frames: &[usize]) {
    for &phys in frames {
        if phys != 0 { mm::pageref::unref_or_free(phys, 0); }
    }
}

// ── Shared open file descriptions (tmpfs offsets) ──────────────────────────────
//
// POSIX: dup(2), fork(2) and SCM_RIGHTS copy a *descriptor*, and every copy
// shares one open file description — one offset. A tmpfs fd used to carry its
// offset inside its own `VnodeKind::TmpFile`, i.e. per descriptor, so
// `(seq 1 10000; seq 1 10) > /tmp/x` wrote the second child's bytes over the
// first child's from offset 0 (48894 bytes instead of 48915), and a session
// whose many children inherit one redirected stderr ended up with a log as
// long as its single largest writer — "50 KB of logs, wc -c says 4089".
// f2fs never had the bug: its offset lives in the mount server's open-file slot.
//
// One entry per open(2) of a tmpfs regular file, refcounted by every fd copy
// (`pipe_ref_inc` / `pipe_ref_dec` / `release_vnode` / `handle_close`). Leaf
// lock: may be taken under FD_TABLES, never held while taking another lock.
const MAX_TMP_OFDS: usize = 4096;

#[derive(Clone, Copy)]
struct TmpOfd { pos: usize, refs: u32 }

static TMP_OFDS: Mutex<[TmpOfd; MAX_TMP_OFDS]> =
    Mutex::new([TmpOfd { pos: 0, refs: 0 }; MAX_TMP_OFDS]);

/// Claim a description with one reference and offset `pos`; 0 when the table
/// is full (the fd then keeps a private offset, the pre-2026-10-01 behaviour).
fn tmp_ofd_alloc(pos: usize) -> u16 {
    let mut t = TMP_OFDS.lock();
    for i in 1..MAX_TMP_OFDS {
        if t[i].refs == 0 { t[i] = TmpOfd { pos, refs: 1 }; return i as u16; }
    }
    0
}

fn tmp_ofd_get(ofd: u16) {
    if ofd != 0 { let mut t = TMP_OFDS.lock(); t[ofd as usize].refs += 1; }
}

fn tmp_ofd_put(ofd: u16) {
    if ofd != 0 {
        let mut t = TMP_OFDS.lock();
        let e = &mut t[ofd as usize];
        e.refs = e.refs.saturating_sub(1);
    }
}

fn tmp_idx_of(kind: &VnodeKind) -> usize {
    match *kind { VnodeKind::TmpFile { idx, .. } => idx, _ => 0 }
}

/// The offset of a tmpfs fd: its shared description's, or its own.
fn tmp_pos_get(kind: &VnodeKind) -> usize {
    match *kind {
        VnodeKind::TmpFile { ofd, .. } if ofd != 0 => TMP_OFDS.lock()[ofd as usize].pos,
        VnodeKind::TmpFile { pos, .. } => pos,
        _ => 0,
    }
}

fn tmp_pos_set(kind: &mut VnodeKind, new_pos: usize) {
    if let VnodeKind::TmpFile { pos, ofd, .. } = kind {
        *pos = new_pos;
        if *ofd != 0 { TMP_OFDS.lock()[*ofd as usize].pos = new_pos; }
    }
}

// ── Vnode kinds ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
pub enum VnodeKind {
    None,
    /// Static read-only RamFS file.
    ///
    /// `is_dir` distinguishes a real file (whose `data` is its *contents*) from
    /// a RAMFS_DIRS pseudo-directory (whose `data` is its own **path**, used
    /// solely as the enumeration root by `handle_getdents64`). Conflating the
    /// two handed that path string to userspace as file data: `open("/tmp")`
    /// returned a readable `S_IFREG` fd of size 4 whose bytes were `/tmp`, so
    /// `cat /bin` printed "/bin" and — via tempfile(3)'s `O_TMPFILE` probe,
    /// which we wrongly *succeeded* — `tac` fed from a pipe printed "/tmp"
    /// instead of the reversed input. A directory must never serve content.
    RamFile { data: &'static [u8], pos: usize, is_dir: bool },
    /// /dev/null — reads return 0; writes discarded.
    DevNull,
    /// /dev/zero — reads return zero bytes; writes discarded.
    DevZero,
    /// /dev/full (1:7) — reads return zero bytes; every write fails ENOSPC.
    DevFull,
    /// One end of a pipe.
    Pipe { ring: usize, is_write: bool },
    /// One end of a pseudo-terminal pair (`/dev/ptmx` or `/dev/pts/N`).
    ///
    /// Deliberately shaped like `Pipe`: the pool, the rings and the whole
    /// termios line discipline live in `tty_server::pty`, and this variant
    /// exists only so a pty end is an ordinary fd-table entry. Every fd
    /// behaviour a pipe already has — dup2, fork inheritance, O_NONBLOCK,
    /// epoll edges, SCM_RIGHTS — therefore applies unchanged, which is why
    /// each `VnodeKind::Pipe` arm in this file has a `Pty` neighbour.
    Pty { pair: u16, is_master: bool },
    /// Writable entry in the TmpFiles pool (idx into TMP_FILES).
    ///
    /// `ofd` names the shared open file description (`TMP_OFDS`) whose offset
    /// every dup/fork/SCM_RIGHTS copy of this fd shares, as POSIX requires;
    /// 0 = none (directories, which keep their getdents cursor in `pos`, and
    /// the rare open that found the description table full). Always go through
    /// `tmp_pos_get`/`tmp_pos_set` for the offset of a regular file.
    TmpFile { idx: usize, pos: usize, writable: bool, ofd: u16 },
    /// eventfd: counter value; read returns counter as u64, write adds to it.
    EventFd { slot: usize },
    /// POSIX message queue descriptor (mq_open, or open of /dev/mqueue/NAME).
    Mq { slot: usize },
    /// timerfd: index into TIMERFD_POOL.
    TimerFd { slot: usize },
    /// /dev/urandom — reads return LFSR pseudo-random bytes.
    DevUrandom,
    /// The machine console, as reached through a descriptor: a dup of a raw
    /// (untracked) fd 0-2, `/dev/console`, `/dev/tty` with no controlling
    /// pty, or `/dev/std*` opened while that fd was the raw console.
    /// `target_fd` only records which stdio name it was made from (for
    /// /proc/self/fd); it is NOT a live link to that slot. A descriptor names
    /// an object, fixed when it is created: following the slot made
    /// `cmd 2>&1 >FILE` send stderr into FILE once fd 1 was redirected, and
    /// `/dev/console` write into whatever file stdin had been redirected from.
    DevStdio { target_fd: usize },
    /// /dev/fb0 — linear framebuffer.
    DevFb { pos: usize },
    /// Dynamically registered device proxy.
    ///
    /// `open_id` is an opaque per-*open* cookie (see `DEVICE_OPEN_REFS`), the
    /// VFS-side stand-in for Linux's `struct file`. Two `open("/dev/dri/card0")`
    /// calls get two different `open_id`s; dup/fork/SCM_RIGHTS copies of one fd
    /// all carry the same one. Device servers use it to key per-open state
    /// (the virtgpu 3D context) instead of keeping one global.
    DynamicDevice { port: u32, dev_id: u32, open_id: u32 },
    /// File or directory on a mounted filesystem (F2FS, etc.).
    MountedFile { port: u32, file_id: u32 },
    /// signalfd: POLLIN iff a signal in its mask is pending for the caller;
    /// read yields one `signalfd_siginfo` per pending signal in the mask.
    /// `mask` is the mask given at creation; `ofd` names the open file
    /// description, and a later signalfd4(fd, mask) re-mask is stored per
    /// description in `SIGFD_MASKS` (see `signalfd_mask`), so dup'd, forked
    /// and SCM_RIGHTS copies all see it, as on Linux.
    SignalFd { mask: u64, ofd: u32 },
    /// inotify: a valid fd that accepts watches (init/add_watch) but never
    /// delivers events — keeps a config-watch source quiet in an event loop.
    /// `next_wd` hands out monotonic watch descriptors (≥1).
    /// `inst` is the id of the description the instance lives on (`inotify.rs`).
    Inotify { inst: u32 },
    /// A socket descriptor. `sock` is the socket's handle in the net server
    /// (`SOCK_HANDLE_BASE + slot`); every socket a process holds is one of
    /// these entries (see "Socket and epoll handles in the fd table"). The
    /// kernel translates every syscall on this fd to the handle before
    /// routing it, so nothing in the VFS ever reads or writes through one.
    /// (Until lane fdmax only `dup2(sock, low_fd)` made one; sockets
    /// otherwise had numbers of their own above the VFS range.)
    SockAlias { sock: usize },
    /// An epoll descriptor: `h` is the epoll handle (`EPOLL_HANDLE_BASE` +
    /// index into the kernel's epoll fd table). Translated like a socket.
    EpollFd { h: usize },
    /// `/dev/tty0` .. `/dev/tty6` — a virtual console (`tty_server::vt`).
    ///
    /// `vt` is 0 for `/dev/tty0`, which names *the active VT* rather than a
    /// fixed one, and 1..=6 for `/dev/tty1`../dev/tty6`.
    ///
    /// `seen` is the active VT number this **open** last observed, and it is
    /// the whole of the `/dev/tty0` change-notification state (see
    /// [`fd_vt_number`]). It lives in the fd entry rather than in a table
    /// keyed by (pid, fd) because that is what makes two openers independent
    /// for free — which is the property the notification exists to provide.
    DevVt { vt: u8, seen: u8 },
    /// A block device node: `/dev/vda`, a partition of one, `/dev/loopN`, or
    /// the `/dev/loop-control` character node (`dev == block::LOOP_CONTROL`).
    ///
    /// `pos` is the fd's byte offset, kept here rather than in the block
    /// registry because it is a property of the *descriptor*: two opens of
    /// `/dev/loop0` seek independently, and the `gpt` crate sizes a device by
    /// `lseek(fd, 0, SEEK_END)` on its own handle while the partitioner holds
    /// another.
    BlockDev { dev: u16, pos: u64 },
    /// A directory in the synthesized `/sys/class/block` tree, named by the
    /// registry slot it describes plus which level of the tree it is (see
    /// `block::LVL_*`). Container directories (`/sys`, `/sys/class`,
    /// `/sys/class/block`) use `block::NO_DEV`. `pos` is the getdents cursor.
    ///
    /// No path string is stored: the pair *is* the identity, which is what
    /// keeps this variant inside the 32-byte `VnodeKind` budget below.
    SysBlock { dev: u16, level: u8, pos: u32 },
    /// pidfd_open(2) / CLONE_PIDFD: names process `pid` (a thread-group id).
    /// No pool behind it — pids are never reused, so the number alone is a
    /// stable identity and close needs no teardown. POLLIN once the process
    /// has exited; see `sched::process_exit_state`.
    PidFd { pid: u32 },
    /// bpf(2) program fd (lane bpfsys); one `sched::bpfcg` reference per fd.
    BpfProg { id: u32 },
    /// An opened /proc/<pid>/ns/<name> (nsfs-like): `kind` indexes
    /// `sched::ns::NS_NAMES`, `id` is the namespace, on which every copy of
    /// the descriptor holds one reference (`pipe_ref_inc` / `pipe_ref_dec`).
    NsFd { kind: u8, id: u16 },
}

// `VnodeKind` is embedded in `FdEntry`, one per open descriptor of every
// process, so its size is per-descriptor kernel memory.
// 32 is the size before `DynamicDevice` grew its `open_id` (the `RamFile`
// variant, a 16-byte slice plus a usize, sets the floor), and it must stay
// that size: adding the u32 has to land in padding, not widen the enum.
const _: () = assert!(::core::mem::size_of::<VnodeKind>() <= 32);

/// True if `fd` was opened with (or fcntl'd to) O_NONBLOCK. The kernel's
/// sys_read EAGAIN retry loop consults this: blocking fds yield-and-retry,
/// non-blocking fds must surface EAGAIN to the caller (POSIX). Without this
/// check a non-blocking read of an empty device (e.g. MAME polling
/// /dev/input/event0) never returns and the caller spins in-kernel forever.
pub fn fd_nonblock(pid: u32, fd: usize) -> bool {
    const O_NONBLOCK: u32 = 0o4000;
    let pid = sched::tgid_of(pid); // fd tables are per-process
    let mut tbls = TblSlot::new();
    if let Some(tbl) = find_tbl(pid, &mut tbls) {
        if tbl.fds.used(fd) {
            return tbl.fds[fd].flags & O_NONBLOCK != 0;
        }
    }
    false
}

/// Whether `fd` can honour preadv2/pwritev2's RWF_NOWAIT (Linux: the file has
/// FMODE_NOWAIT), `None` if it is not open. Files, pipes and the fd-backed
/// event objects can: their reads and writes either complete now or answer
/// EAGAIN. Terminals (ptys, VTs, the console and its /dev/std* proxies) and
/// driver devices cannot, as on Linux, where tty files never set
/// FMODE_NOWAIT; the caller answers EOPNOTSUPP and the program falls back to
/// poll + read. An untracked fd 0-2 is the raw console.
pub fn fd_rwf_nowait_capable(pid: u32, fd: usize) -> Option<bool> {
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    let console = if fd <= 2 { Some(false) } else { None };
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return console };
    if fd >= FD_NUMBER_CEIL { return None; }
    if !tbl.fds.used(fd) { return if tbl.fds.implicit_console(fd) { console } else { None }; }
    Some(matches!(tbl.fds[fd].kind,
        VnodeKind::RamFile { .. } | VnodeKind::DevNull | VnodeKind::DevZero | VnodeKind::DevFull
        | VnodeKind::Pipe { .. } | VnodeKind::TmpFile { .. } | VnodeKind::EventFd { .. }
        | VnodeKind::Mq { .. } | VnodeKind::TimerFd { .. } | VnodeKind::DevUrandom | VnodeKind::MountedFile { .. }
        | VnodeKind::SignalFd { .. } | VnodeKind::Inotify { .. }))
}

/// The open flags of `fd` (O_ACCMODE, O_APPEND, ...), if it is open.
pub fn fd_flags(pid: u32, fd: usize) -> Option<u32> {
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    let tbl = find_tbl(pid, &mut tbls)?;
    if tbl.fds.used(fd) { Some(tbl.fds[fd].flags) } else { None }
}

/// If `fd` is an O_APPEND regular file (tmpfs or a mounted volume), a key
/// naming what its appends must be serialised against: the tmpfs inode, or
/// the whole mount (a mount's open-file slot is per open, not per inode).
/// The kernel's writev holds the matching append lock across all its iovecs
/// so the vector lands as one contiguous append, as POSIX requires.
pub fn fd_append_lock_key(pid: u32, fd: usize) -> Option<u32> {
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    let tbl = find_tbl(pid, &mut tbls)?;
    if !tbl.fds.used(fd) || tbl.fds[fd].flags & O_APPEND == 0 { return None; }
    match tbl.fds[fd].kind {
        VnodeKind::TmpFile { idx, .. } => Some(idx as u32),
        VnodeKind::MountedFile { port, .. } => Some(port ^ 0x8000_0000),
        _ => None,
    }
}

/// `(mount port, open-file slot, open flags)` of `fd` if it is a file on a
/// mounted volume (pread/pwrite go to the mount directly, positionally).
pub fn mounted_file_of(pid: u32, fd: usize) -> Option<(u32, u32, u32)> {
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    let tbl = find_tbl(pid, &mut tbls)?;
    if !tbl.fds.used(fd) { return None; }
    match tbl.fds[fd].kind {
        VnodeKind::MountedFile { port, file_id } => Some((port, file_id, tbl.fds[fd].flags)),
        _ => None,
    }
}

/// The targeted-wake tag (`sched::poll_tag`) a blocking read() (`write` =
/// false) or write() (`write` = true) on `fd` may park under, or `None` when
/// the fd's kind has no producer that is trusted to wake its level changes
/// with a tag — the caller then parks on the broadcast mask with a tick
/// deadline instead. Every `Some` is the tag the fd's `handle_poll` answer
/// carries, computed by the same helper, so a parked read()er has exactly the
/// wake guarantee a poll(POLLIN) waiter on that fd has:
///
/// * pipes, ptys, eventfds, timerfds, /dev/ttyN: their producers (pipe
///   write/read/close, pty writes, eventfd write, timerfd expiry, VT switch)
///   wake that tag or its key;
/// * a signalfd: `signalfd_wake_tag`, what every signal post to the caller's
///   thread group wakes (`sched::signal_wake_tag`);
/// * inotify: `inotify_wake_tag`, which nothing wakes because nothing queues
///   inotify events; a signal makes the reader Ready directly;
/// * a dynamic device (DRM card/render, evdev, /dev/netctl, ...): the tag the
///   device server's own VFS_POLL answer names, asked on this EAGAIN slow
///   path. It is used only when that answer is valid (data[32] == 1, one
///   key at most) AND agrees the fd is not ready in the blocked direction:
///   the server's poll contract is that it wakes that tag when the bit sets.
///   An answer that says "ready" while the op said EAGAIN is a contract this
///   crate cannot vouch for, so it keeps the broadcast fallback.
///
/// The answer also carries the refinement `handle_poll` gives the same fd
/// (`fd_poll_tag`): the exact key of a pipe / eventfd / timerfd (or a device
/// server's key), or "foreign" for a kind no keyed wake targets. Without it
/// the park was bit-only, so EVERY keyed wake whose key shared the bit woke
/// the reader: a blocked signalfd read whose group tag happened to share a
/// bit with some AF_UNIX connection's or pipe's key was woken by each of its
/// round trips (vfstag `signalfd_read`, ~60 000 wakes a second).
pub fn fd_wake_tag(pid: u32, fd: usize, write: bool) -> Option<sched::PollTag> {
    use sched::{poll_class, poll_tag};
    let caller = pid;
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    let tbl = find_tbl(pid, &mut tbls)?;
    if !tbl.fds.used(fd) { return None; }
    let kind = tbl.fds[fd].kind;
    let local = |tag: u64| Some(fd_poll_tag(&kind, tag, None));
    let (port, dev_id, open_id) = match &kind {
        VnodeKind::Pipe { ring, is_write } => return local(sched::poll_key_bit(pipe_end_key(*ring, *is_write))),
        VnodeKind::Pty { pair, .. }   => return local(poll_tag(poll_class::PTY, *pair as u32)),
        VnodeKind::EventFd { slot }   => return local(poll_tag(poll_class::EVENTFD, *slot as u32)),
        VnodeKind::Mq { slot }        => return local(poll_tag(poll_class::MQ, *slot as u32)),
        VnodeKind::TimerFd { slot }   => return local(poll_tag(poll_class::TIMERFD, *slot as u32)),
        VnodeKind::DevVt { .. }       => return local(sched::POLL_TAG_VT_SWITCH),
        VnodeKind::SignalFd { .. } if !write => return local(signalfd_wake_tag()),
        VnodeKind::Inotify { inst } if !write => return local(sched::poll_key_bit(inotify::key(*inst))),
        VnodeKind::DynamicDevice { port, dev_id, open_id } if *open_id != 0 =>
            (*port, *dev_id, *open_id),
        _ => return None,
    };
    // DYNAMIC_DEVICES is not taken under an FD table elsewhere, and the poll
    // proxy below is an IPC round-trip: release the table first.
    tbls.release();
    // /dev/netctl: the net server wakes an open's readers with
    // `poll_tag(NETCTL, open_id)` (servers/net/src/netctl `tag_of`), the same
    // tag its VFS_POLL answer carries. Answered here without the round-trip.
    if dynamic_device_number(port, dev_id) == Some((NETCTL_MAJOR, NETCTL_MINOR)) {
        return Some(fd_poll_tag(&kind, netctl_wake_tag(open_id), None));
    }
    let (revents, _, tag, dev_key) = poll_vnode_k(&kind, caller);
    dev_block_tag(revents, tag, write).map(|t| fd_poll_tag(&kind, t, dev_key))
}

/// The `sched::PollTag` `handle_poll` answers for a vnode of `kind` whose
/// probe gave `tag` (and a device server's `dev_key`): keyed when the kind's
/// producers wake with that key, foreign when no keyed wake targets the kind,
/// bits only otherwise. The one rule both the poll answer and the blocked
/// read/write park (`fd_wake_tag`) follow.
fn fd_poll_tag(kind: &VnodeKind, tag: u64, dev_key: Option<u64>) -> sched::PollTag {
    if let Some(k) = poll_vnode_key(kind).or(dev_key) {
        if sched::poll_key_bit(k) == tag { return sched::PollTag::keyed(&[k], 0); }
    } else if tag != sched::POLL_TAG_ALL && poll_vnode_foreign(kind) {
        return sched::PollTag::foreign(tag);
    }
    sched::PollTag::bits(tag)
}

/// The park tag of a blocked op on a dynamic device whose server answered
/// VFS_POLL with (`revents`, `tag`): see `fd_wake_tag`. Broadcast answers
/// (an error, no tag, an unforwardable key set) and answers that already
/// report the blocked direction ready give `None`.
fn dev_block_tag(revents: u32, tag: u64, write: bool) -> Option<u64> {
    if tag == sched::POLL_TAG_ALL || tag == 0 { return None; }
    let ready = if write { POLLOUT | POLLERR | POLLHUP } else { POLLIN | POLLERR | POLLHUP };
    if revents & ready != 0 { None } else { Some(tag) }
}

/// The tag a signalfd's poll answer and a blocked signalfd read name: the
/// CALLER's thread group's signal tag, whose pending set the answer reads,
/// and what every signal post to that group wakes (`sched::signal_wake_tag`).
fn signalfd_wake_tag() -> u64 { sched::signal_wake_tag(sched::current_tgid()) }

/// The tag of an inotify instance's poll answer: its exact key's bit.
fn inotify_wake_tag(inst: u32) -> u64 { sched::poll_key_bit(inotify::key(inst)) }

/// True unless `vfs.inotify=0` is on the kernel command line. The kernel's
/// inotify syscalls take the real path then; with the opt-out the old silent
/// stub (valid fd, fake wds, no events) stays byte for byte.
pub fn inotify_enabled() -> bool {
    let c = *KERNEL_CMDLINE.lock();
    inotify::enabled(c)
}

/// The tag of every vnode whose readiness never changes (regular files,
/// /dev/null|zero|urandom, framebuffers, block devices, /dev/pipewire): a
/// poller parks on it only under EPOLLET after the first edge or for bits
/// the fd never reports, and nothing ever needs to wake it. Reported foreign.
/// Device servers with a constant answer use it too.
pub const fn static_poll_tag() -> u64 { sched::poll_tag(sched::poll_class::STATIC, 0) }

/// The tag a reader of the /dev/netctl open `open_id` parks on. Must equal
/// the producer's `tag_of` in servers/net/src/netctl (which calls this).
pub const fn netctl_wake_tag(open_id: u32) -> u64 {
    sched::poll_tag(sched::poll_class::NETCTL, open_id)
}

/// True when `fd`'s reads and writes are the machine console's: a
/// `/dev/stdin|stdout|stderr` proxy whose target is the raw console (untracked
/// fd 0-2, or itself another console proxy), or a virtual console node. The
/// kernel's console fast paths treat such fds exactly like bare 0/1/2 —
/// without this, a dup'd stdio fd dup2'd back onto 0/1/2 (command_fds'
/// identity mappings) would recurse inside the VFS instead of reaching the
/// serial console.
///
/// `/dev/ttyN` is included because that is the only thing that makes the
/// `DevVt` read/write arms below reachable in the way `DevStdio { target_fd:
/// 0 }` is: fd 0 is untracked for a console session, so a VFS-side delegation
/// to it answers EBADF, and only the kernel's fast path actually reaches the
/// console. Callers that must distinguish a VT from a plain console proxy —
/// the ioctl router, and `/dev/tty0`'s read/poll, which are the VT-change
/// notification rather than console input — pair this with [`fd_vt_number`];
/// that costs a second FD_TABLES pass only on fds for which this already
/// answered true, which is never a hot path.
pub fn fd_is_console_stdio(pid: u32, fd: usize) -> bool {
    let pid = sched::tgid_of(pid);
    if fd >= FD_NUMBER_CEIL { return false; }
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return false };
    if !tbl.fds[fd].in_use { return tbl.fds.implicit_console(fd); }
    match tbl.fds[fd].kind {
        // A console proxy is the console itself — never a link to whatever
        // its `target_fd` slot holds now (see `VnodeKind::DevStdio`).
        VnodeKind::DevStdio { .. } => true,
        // A virtual console IS the console: writes land on the active VT
        // (the mirror `vt::console_out` feeds), reads take console input.
        VnodeKind::DevVt { .. } => true,
        // Everything else — including a `Pty` on fd 0/1/2, which is a
        // terminal but emphatically NOT *this* terminal. Answering true
        // for one would hand every read and write on a shell's pty stdio
        // to the kernel's serial fast path, and the pty would never see a
        // byte.
        _ => false,
    }
}

/// Transfer ownership of a mounted-file fd out of `pid`'s FD table.
///
/// Frees the fd slot WITHOUT proxying VFS_CLOSE to the mount, so the
/// filesystem-side open file stays live; the caller (the kernel's
/// demand-paged exec path) becomes responsible for closing it via the
/// mount's port when the last reference goes away.  Returns the mount port
/// and file id, or None if `fd` is not a MountedFile.
pub fn steal_mounted_file(pid: u32, fd: usize) -> Option<(u32, u32)> {
    let mut tbls = TblSlot::new();
    let tbl = find_tbl(pid, &mut tbls)?;
    if !tbl.fds.used(fd) { return None; }
    if let VnodeKind::MountedFile { port, file_id } = tbl.fds[fd].kind {
        let gone = tbl.fds[fd].ofd;
        tbl.fds.clear(fd);
        drop(tbls);
        ofd::put(gone);
        Some((port, file_id))
    } else {
        None
    }
}

/// The open object behind an fd, as (kind tag, object id), for the kinds whose
/// object is shared by `dup`/fork: two fds with the same identity name the
/// same open file. None for kinds with no shared object id.
pub fn fd_object_identity(k: &VnodeKind) -> Option<(u8, u64)> {
    match *k {
        VnodeKind::Pipe { ring, is_write } => Some((1, (ring as u64) << 1 | is_write as u64)),
        VnodeKind::Pty { pair, is_master } => Some((2, (pair as u64) << 1 | is_master as u64)),
        VnodeKind::TmpFile { idx, .. } => Some((3, idx as u64)),
        VnodeKind::EventFd { slot } => Some((4, slot as u64)),
        VnodeKind::Mq { slot } => Some((8, slot as u64)),
        VnodeKind::TimerFd { slot } => Some((5, slot as u64)),
        VnodeKind::DynamicDevice { port, open_id, .. } => Some((6, (port as u64) << 32 | open_id as u64)),
        VnodeKind::MountedFile { port, file_id } => Some((7, (port as u64) << 32 | file_id as u64)),
        _ => None,
    }
}

// ── Socket and epoll handles in the fd table ─────────────────────────────────
//
// One descriptor namespace per process (lane fdmax). Sockets live in the net
// server's tables and epoll instances in the kernel's epoll layer, each
// under its own internal *handle* number; a process reaches them only through
// an entry of its own fd table here (`VnodeKind::SockAlias` / `EpollFd`) that
// names the handle. So every kind of descriptor takes the lowest free number,
// shares RLIMIT_NOFILE, and is dup2-able, fork-inherited and close_range'd
// like any other. The kernel translates a descriptor to what it names
// (`resolve_fd`) once per syscall and routes by the handle's range.
//
// Handle numbers sit above every possible descriptor (`FD_NUMBER_CEIL`) and
// are never valid descriptors themselves: `resolve_user_fd` maps one to
// `BAD_FD`, so a user process can neither guess nor forge one.
//
// A socket's FD_CLOEXEC and O_NONBLOCK live on its net-side entry (fcntl on
// the descriptor is translated to the socket), an epoll fd's FD_CLOEXEC on
// its epoll-side entry; the table entry carries no flags and no description
// of its own. Both sides are 1:1: each entry names its own handle, and the
// handle is closed when the entry is.
//
// A pidfd (or any further kernel object with its own table) slots in the same
// way: a handle range here, a `VnodeKind` naming it, an arm in `handle_of`
// and in the kernel's routing.

/// First socket handle (`net_server::SOCK_FD_BASE`).
pub const SOCK_HANDLE_BASE: usize = 0x0100_0000;
/// First epoll handle (the kernel's `EPOLL_FD_BASE`); one past the last
/// socket handle.
pub const EPOLL_HANDLE_BASE: usize = 0x0200_0000;
/// One past the last handle of any kind.
pub const HANDLE_SPACE_END: usize = 0x0400_0000;
const _: () = assert!(FD_NUMBER_CEIL <= SOCK_HANDLE_BASE);

/// What a descriptor number that names nothing resolves to: never open, so
/// every path answers EBADF for it (and, being below the handle space, it is
/// routed to the VFS, which says so).
pub const BAD_FD: usize = FD_NUMBER_CEIL;

/// Is `n` in the internal handle space?
#[inline]
pub fn is_handle(n: usize) -> bool { (SOCK_HANDLE_BASE..HANDLE_SPACE_END).contains(&n) }

/// The handle an fd-table entry names, if it is a handle entry.
#[inline]
fn handle_of(kind: &VnodeKind) -> Option<usize> {
    match *kind {
        VnodeKind::SockAlias { sock } => Some(sock),
        VnodeKind::EpollFd { h } => Some(h),
        _ => None,
    }
}

/// The table entry that names `h`.
fn handle_entry(h: usize) -> FdEntry {
    let kind = if h < EPOLL_HANDLE_BASE { VnodeKind::SockAlias { sock: h } } else { VnodeKind::EpollFd { h } };
    FdEntry { kind, flags: 0, in_use: true, ofd: 0 }
}

/// What `fd` of `pid`'s process names, for routing: the socket or epoll
/// handle behind a handle entry, else `fd` itself. A number already in the
/// handle space passes through — the kernel's own probes (epoll interests,
/// description holders) carry handles. Values past the handle space
/// (AT_FDCWD, -1) pass through too.
pub fn resolve_fd(pid: u32, fd: usize) -> usize {
    if fd >= FD_NUMBER_CEIL { return fd; }
    let pid = sched::tgid_of(pid);
    with_tbl(pid, |t| t.fds.get(fd).and_then(|e| handle_of(&e.kind))).flatten().unwrap_or(fd)
}

/// `resolve_fd` for a number that came from userspace: a raw handle number is
/// not a descriptor (`BAD_FD`).
#[inline]
pub fn resolve_user_fd(pid: u32, fd: usize) -> usize {
    if fd >= FD_NUMBER_CEIL {
        return if fd < HANDLE_SPACE_END { BAD_FD } else { fd };
    }
    resolve_fd(pid, fd)
}

/// The socket handle a descriptor names, if `fd` is a socket's.
pub fn sock_alias_of(pid: u32, fd: usize) -> Option<usize> {
    match vfs_get_node_kind(pid, fd) {
        Some(VnodeKind::SockAlias { sock }) => Some(sock),
        _ => None,
    }
}

/// Another open fd of `pid`'s process, other than `except`, naming the same
/// object as `kind` (see `fd_object_identity`), if any. The epoll layer uses
/// it to keep a registration alive across close() of one of two dups.
pub fn find_alias_fd(pid: u32, kind: &VnodeKind, except: usize) -> Option<usize> {
    let id = fd_object_identity(kind)?;
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    let tbl = find_tbl(pid, &mut tbls)?;
    let found = tbl.fds.iter_used()
        .find(|&(i, f)| i != except && fd_object_identity(&f.kind) == Some(id)).map(|(i, _)| i);
    found
}

/// Every open fd of `pid`'s process (close_range), every kind.
pub fn open_fds(pid: u32) -> alloc::vec::Vec<usize> {
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    match find_tbl(pid, &mut tbls) {
        Some(t) => t.fds.used_fds(),
        None => alloc::vec::Vec::new(),
    }
}

/// The open file description `fd` names in `pid`'s process: `Some(0)` for an
/// fd with none (description table full, or a handle entry, whose
/// description lives with its socket), `None` when `fd` is not open.
pub fn fd_ofd(pid: u32, fd: usize) -> Option<u32> {
    let pid = sched::tgid_of(pid);
    if fd >= FD_NUMBER_CEIL { return None; }
    let mut tbls = TblSlot::new();
    let tbl = find_tbl(pid, &mut tbls)?;
    if tbl.fds[fd].in_use { Some(tbl.fds[fd].ofd) } else { None }
}

/// Would Linux refuse `epoll_ctl` on `fd` with EPERM? It does for an object
/// with no poll method (`file_can_poll`): a regular file or directory on a
/// disk filesystem or tmpfs, a block device, /dev/null, /dev/zero, /dev/fb0.
///
/// Deliberately conservative, because this VFS serves several pollable Linux
/// objects through file kinds: procfs/sysfs entries (static RamFile, the
/// `/tmp/.<name>` TmpFile snapshots, f2fs-backed `/sys` files) and dma-bufs
/// (unlinked `/tmp/dmabuf:<n>` TmpFiles — smithay polls them for implicit
/// sync) all stay allowed, as do memfds (EPERM on Linux, harmless here).
pub fn fd_epoll_eperm(pid: u32, fd: usize) -> bool {
    if fd >= FD_NUMBER_CEIL { return false; }
    let pid = sched::tgid_of(pid);
    let kind = {
        let mut tbls = TblSlot::new();
        match find_tbl(pid, &mut tbls) {
            Some(t) if t.fds[fd].in_use => t.fds[fd].kind,
            _ => return false,
        }
    };
    match kind {
        VnodeKind::DevNull | VnodeKind::DevZero | VnodeKind::DevFull | VnodeKind::DevFb { .. }
        | VnodeKind::BlockDev { .. } | VnodeKind::SysBlock { .. } => true,
        VnodeKind::RamFile { is_dir, .. } => is_dir,
        VnodeKind::TmpFile { idx, .. } => {
            let tmp = TMP_FILES.lock();
            let e = &tmp[idx];
            let path = e.path();
            e.in_use() && !e.ephemeral() && !path.starts_with(b"/tmp/.") && !path.starts_with(b"/.leandros-sb/") && !path.starts_with(b"/tmp/dmabuf")
        }
        VnodeKind::MountedFile { .. } => {
            let mut buf = [0u8; 128];
            let r = reply_val(&handle_fd_path(pid, fd, buf.as_mut_ptr() as usize, buf.len()));
            if r <= 0 { return false; }
            let path = &buf[..(r as usize).min(buf.len())];
            !(path.starts_with(b"/sys/") || path.starts_with(b"/proc/") || path.starts_with(b"/dev/"))
        }
        _ => false,
    }
}

/// Some process's fd naming description `id`, as (process id, fd): what the
/// epoll layer probes a registration through once the fd it was made on no
/// longer names it (closed while a fork child or a dup keeps the description
/// alive). Prefers `pid`'s own process. A full-table scan: only reached for
/// such orphaned registrations, and the caller caches the answer.
pub fn ofd_holder(pid: u32, id: u32) -> Option<(u32, usize)> {
    if id == 0 { return None; }
    let pid = sched::tgid_of(pid);
    let find = |t: &mut ProcFdTable| t.fds.iter_used().find(|(_, f)| f.ofd == id).map(|(fd, _)| (t.pid, fd));
    if let Some(r) = with_tbl(pid, find).flatten() { return Some(r); }
    // One table at a time: a best-effort search (the caller re-checks).
    all_tables().iter().find_map(|t| {
        let mut g = t.lock();
        if g.pid == pid || g.dead { None } else { find(&mut g) }
    })
}

/// Give handle `h` (a socket or an epoll fd the caller just created or
/// duplicated) the lowest free descriptor number `>= min` in `pid`'s
/// process. Returns the descriptor, -EMFILE when the table is at its limit,
/// or -EINVAL when `min` is at or past it (F_DUPFD). On failure nothing was
/// installed: the caller still owns `h` and must close it.
pub fn install_handle(pid: u32, h: usize, min: usize) -> isize {
    if !is_handle(h) { return -9; }
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    let tbl = match get_or_create(pid, &mut tbls) { Some(t) => t, None => return -23 };
    if min >= tbl.cap() { return -22; }
    let fd = match tbl.alloc_fd_from(min) { Some(f) => f, None => return -24 };
    tbl.fds.set(fd, handle_entry(h));
    fd as isize
}

/// Make descriptor `fd` name handle `h` (dup2/dup3 of a socket or an epoll fd
/// onto `fd`). Whatever `fd` held is released, as dup2 does; a handle entry it
/// held has already been closed by the caller. Returns `fd`, or -EBADF when
/// `fd` is at or past the limit (the caller still owns `h` then).
pub fn install_handle_at(pid: u32, fd: usize, h: usize) -> isize {
    if fd >= FD_NUMBER_CEIL || !is_handle(h) { return -9; } // EBADF
    let pid = sched::tgid_of(pid);
    let replaced = {
        let mut tbls = TblSlot::new();
        let tbl = match get_or_create(pid, &mut tbls) { Some(t) => t, None => return -23 };
        if fd >= tbl.cap() { return -9; }
        let replaced = tbl.fds.get(fd).map(|e| (e.kind, e.ofd));
        tbl.fds.set(fd, handle_entry(h));
        replaced
    };
    if let Some((old, old_ofd)) = replaced { release_vnode(old, pid); ofd::put(old_ofd); }
    fd as isize
}

/// Back-compat name for `install_handle_at` on a socket handle.
pub fn install_sock_alias(pid: u32, fd: usize, sock: usize) -> isize {
    install_handle_at(pid, fd, sock)
}

/// Drop every handle entry in `pid`'s table whose handle is gone
/// (`alive(h)` is false): after execve's close-on-exec sweeps closed the
/// sockets/epoll fds behind them, and after fork for the sockets the net
/// server does not copy (inet, listening).
pub fn prune_handles(pid: u32, alive: impl Fn(usize) -> bool) {
    let pid = sched::tgid_of(pid);
    let found: alloc::vec::Vec<(usize, usize)> = {
        let mut tbls = TblSlot::new();
        let Some(tbl) = find_tbl(pid, &mut tbls) else { return };
        tbl.fds.iter_used().filter_map(|(fd, e)| handle_of(&e.kind).map(|h| (fd, h))).collect()
    };
    for &(fd, h) in found.iter() {
        if alive(h) { continue; }
        let mut tbls = TblSlot::new();
        if let Some(tbl) = find_tbl(pid, &mut tbls) {
            if tbl.fds.get(fd).and_then(|e| handle_of(&e.kind)) == Some(h) {
                tbl.fds.clear(fd);
            }
        }
    }
}

/// Every socket / epoll handle the table `pid` resolves to still names
/// (fork: the child's copy of one thread's table).
pub fn table_handles(pid: u32) -> alloc::vec::Vec<usize> {
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    match find_tbl(pid, &mut tbls) {
        Some(t) => t.fds.iter_used().filter_map(|(_, e)| handle_of(&e.kind)).collect(),
        None => alloc::vec::Vec::new(),
    }
}

/// Back-compat name for `prune_handles`.
pub fn prune_sock_aliases(pid: u32, alive: impl Fn(usize) -> bool) {
    prune_handles(pid, alive)
}

/// Every handle entry of `pid`'s process, as (fd, handle).
pub fn handle_fds(pid: u32) -> alloc::vec::Vec<(usize, usize)> {
    let pid = sched::tgid_of(pid);
    with_tbl(pid, |t| t.fds.iter_used().filter_map(|(fd, e)| handle_of(&e.kind).map(|h| (fd, h))).collect())
        .unwrap_or_default()
}

/// The directory listing the caller's descriptors.
const PROC_SELF_FD_DIR: &[u8] = b"/proc/self/fd";

/// `N` of a "/proc/self/fd/N" path.
fn proc_self_fd_number(path: &[u8]) -> Option<usize> {
    let digits = path.strip_prefix(b"/proc/self/fd/")?;
    if digits.is_empty() || digits.len() > 7 { return None; }
    let mut n = 0usize;
    for &d in digits {
        if !d.is_ascii_digit() { return None; }
        n = n * 10 + (d - b'0') as usize;
    }
    Some(n)
}

/// `(pid, N)` of a "/proc/<pid>/fd/N" path.
fn proc_pid_fd_number(path: &[u8]) -> Option<(u32, usize)> {
    let rest = path.strip_prefix(b"/proc/")?;
    let slash = rest.iter().position(|&c| c == b'/')?;
    let pid = proc_parse_num(&rest[..slash])?;
    let n = proc_self_fd_number(&[b"/proc/self/fd/", rest[slash..].strip_prefix(b"/fd/")?].concat())?;
    Some((pid, n))
}

/// The vnode a fresh open of "/proc/self/fd/`fd`" gets: the same object with
/// its own offset and the access mode `flags` asks for. A tmpfs file or memfd
/// is reopened in place; the permission check is the file's own, as for any
/// open. Kinds with no such identity to reopen (a file on a mounted
/// filesystem, a pipe, a device) stay ENOENT as before; a socket alias is
/// ENXIO, as on Linux.
fn reopen_fd_kind(pid: u32, fd: usize, flags: u32) -> Result<VnodeKind, i32> {
    reopen_fd_kind_of(pid, pid, fd, flags)
}

/// As `reopen_fd_kind`, but fd `fd` is looked up in `target`'s table (the
/// owner's fd table, per process) while `pid` is the opener whose
/// credentials are checked.
fn reopen_fd_kind_of(pid: u32, target: u32, fd: usize, flags: u32) -> Result<VnodeKind, i32> {
    let kind = match vfs_get_node_kind(target, fd) { Some(k) => k, None => return Err(-2) };
    match kind {
        VnodeKind::TmpFile { idx, .. } => reopen_tmp_idx(pid, idx, flags),
        VnodeKind::DevNull | VnodeKind::DevZero | VnodeKind::DevFull | VnodeKind::DevUrandom => Ok(kind),
        // A pipe: a new description on the same pipe, whose end follows the
        // access mode asked for (O_RDONLY reads, O_WRONLY writes) whichever
        // end the original was. O_RDWR keeps the original's end. The caller
        // takes the endpoint reference once the fd is installed.
        VnodeKind::Pipe { ring, is_write } => {
            let accmode = flags & 0x3;
            let w = match accmode { O_WRONLY => true, O_RDWR => is_write, _ => false };
            Ok(VnodeKind::Pipe { ring, is_write: w })
        }
        // A file on a mounted filesystem: reopen by the path the owning
        // server reports for it, through the ordinary forwarded open, so the
        // new description has a fresh offset and the new flags.
        VnodeKind::MountedFile { port, .. } => {
            let mut buf = [0u8; 512];
            let r = reply_val(&handle_fd_path(target, fd, buf.as_mut_ptr() as usize, buf.len() - 1));
            if r <= 0 { return Err(-2); }
            let n = (r as usize).min(buf.len() - 1);
            buf[n] = 0;
            let mut proxy = Message::empty();
            proxy.tag = VFS_OPEN;
            proxy.data[0..8].copy_from_slice(&(buf.as_ptr() as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&((flags & !(O_CREAT | O_EXCL)) as u64).to_le_bytes());
            let reply = call_port(port, proxy);
            let id = i64::from_le_bytes(reply.data[0..8].try_into().unwrap_or([0u8; 8]));
            if id < 0 { return Err(id as i32); }
            Ok(VnodeKind::MountedFile { port, file_id: id as u32 })
        }
        VnodeKind::SockAlias { .. } | VnodeKind::EpollFd { .. } => Err(-6), // ENXIO
        _ => Err(-2),
    }
}

fn reopen_tmp_idx(pid: u32, idx: usize, flags: u32) -> Result<VnodeKind, i32> {
    let accmode = flags & 0x3;
    let want_read = accmode != O_WRONLY;
    let want_write = accmode == O_WRONLY || accmode == O_RDWR;
    let cred = cred_of(pid);
    let tmp = TMP_FILES.lock();
    let (e, xa) = (&tmp[idx], tmp.xattr(idx));
    if !e.in_use() || e.is_dir { return Err(-2); }
    let meta = tmp_meta(e);
    let acl = xattr::find(&xa[..], xattr::IDX_ACL_ACCESS, b"");
    if !xattr::access_check(&meta, &cred, acl, want_read, want_write, false) {
        return Err(-13); // EACCES
    }
    Ok(VnodeKind::TmpFile { idx, pos: 0, writable: want_write, ofd: 0 })
}

/// Identify the kind of a vnode from a process's FD table.
pub fn vfs_get_node_kind(pid: u32, fd: usize) -> Option<VnodeKind> {
    let pid = sched::tgid_of(pid); // fd tables are per-process, not per-thread:
    // a multithreaded client (cosmic-comp's render thread) may mmap/ioctl a card0
    // fd its main thread opened, so resolve the fd in the *process* table. Without
    // this, sys_mmap's DynamicDevice detection misses the DRM fd on a non-main
    // thread and falls through to the file-backed path, whose lseek on the
    // non-seekable card0 returns ESPIPE — Mesa's kms_sw scanout mmap then fails
    // and softpipe faults on the NULL displaytarget map (the W2 crash).
    with_tbl(pid, |t| t.fds.get(fd).map(|e| e.kind)).flatten()
}

// ── Dynamic Device Registry ───────────────────────────────────────────────────

const MAX_DYNAMIC_DEVICES: usize = 32;

#[derive(Clone, Copy)]
pub struct DynamicDeviceEntry {
    pub path: &'static str,
    pub port: u32,
    pub dev_id: u32,
    /// Character-device major/minor. libdrm derives /sys/dev/char/226:0 from
    /// major(st_rdev)=226; libudev likewise keys on the node's rdev. 0/0 for
    /// nodes with no meaningful device number (e.g. /dev/pipewire).
    pub major: u32,
    pub minor: u32,
    pub in_use: bool,
    /// The server wants a VFS_OPEN for every new open (see
    /// `register_device_notify_open`) and may refuse it.
    pub notify_open: bool,
}

impl DynamicDeviceEntry {
    const fn empty() -> Self {
        Self { path: "", port: 0, dev_id: 0, major: 0, minor: 0, in_use: false, notify_open: false }
    }
}

/// Character-device major of the dynamic device `(port, dev_id)`, if registered.
/// `sys_mmap` uses it to tell a DRM node (major 226) apart from other device
/// servers when the device refuses a mapping.
pub fn dynamic_device_major(port: u32, dev_id: u32) -> Option<u32> {
    DYNAMIC_DEVICES.lock().iter().find(|d| d.in_use && d.port == port && d.dev_id == dev_id).map(|d| d.major)
}

/// `(major, minor)` of the dynamic device `(port, dev_id)`, if registered.
pub fn dynamic_device_number(port: u32, dev_id: u32) -> Option<(u32, u32)> {
    DYNAMIC_DEVICES.lock().iter()
        .find(|d| d.in_use && d.port == port && d.dev_id == dev_id)
        .map(|d| (d.major, d.minor))
}

use netctl_abi::{NETCTL_MAJOR, NETCTL_MINOR};

static DYNAMIC_DEVICES: Mutex<[DynamicDeviceEntry; MAX_DYNAMIC_DEVICES]> =
    Mutex::new([const { DynamicDeviceEntry::empty() }; MAX_DYNAMIC_DEVICES]);

/// Linux dev_t encoding: major in bits 8..20 + 32.., minor split around bit 8.
pub fn makedev(major: u32, minor: u32) -> u64 {
    let (major, minor) = (major as u64, minor as u64);
    ((major & 0xfff) << 8) | (minor & 0xff) | ((minor & !0xff) << 12)
}

/// Inverse of [`makedev`] (glibc `major()`/`minor()` for the low 32 bits).
pub fn dev_major_minor(dev: u64) -> (u32, u32) {
    (((dev >> 8) & 0xfff) as u32, ((dev & 0xff) | ((dev >> 12) & !0xff)) as u32)
}

/// Look up the rdev (as dev_t) for a registered dynamic device by (port, dev_id).
/// Returns 0 if the node has no device number registered.
fn lookup_device_rdev(port: u32, dev_id: u32) -> u64 {
    let devices = DYNAMIC_DEVICES.lock();
    for d in devices.iter() {
        if d.in_use && d.port == port && d.dev_id == dev_id {
            return makedev(d.major, d.minor);
        }
    }
    0
}

/// Register a dynamic device path to be proxied to a specific IPC port,
/// carrying its char-device major/minor (used for st_rdev and synthetic sysfs).
pub fn register_device(path: &'static str, port: u32, dev_id: u32, major: u32, minor: u32) {
    register_device_ex(path, port, dev_id, major, minor, false);
}

/// [`register_device`] for a server that must see every open, like a Linux
/// driver's `->open`: each open(2) that passed the permission check is sent
/// to `port` as VFS_OPEN {dev_id, flags, _, pid, open_id} before an fd
/// exists, and a negative reply fails the open with that errno (an exclusive
/// device answers EBUSY). The open_id is the same cookie later reads, writes,
/// ioctls and the final VFS_CLOSE carry; a refused open gets no VFS_CLOSE.
pub fn register_device_notify_open(path: &'static str, port: u32, dev_id: u32, major: u32, minor: u32) {
    register_device_ex(path, port, dev_id, major, minor, true);
}

fn register_device_ex(path: &'static str, port: u32, dev_id: u32, major: u32, minor: u32, notify_open: bool) {
    let mut devices = DYNAMIC_DEVICES.lock();
    if let Some(slot) = devices.iter_mut().find(|d| !d.in_use) {
        *slot = DynamicDeviceEntry { path, port, dev_id, major, minor, in_use: true, notify_open };
    }
}

/// Remove the dynamic device `(port, dev_id)` (a hot-unplugged input device):
/// later opens of its path fail with ENOENT and it leaves getdents/stat. Fds
/// already open keep routing to `port`, which answers them itself (evdev:
/// ENODEV, POLLHUP|POLLERR). Returns whether an entry was removed.
pub fn unregister_device(port: u32, dev_id: u32) -> bool {
    let mut devices = DYNAMIC_DEVICES.lock();
    match devices.iter_mut().find(|d| d.in_use && d.port == port && d.dev_id == dev_id) {
        Some(slot) => { *slot = DynamicDeviceEntry::empty(); true }
        None => false,
    }
}

// ── Filesystem mount registry ─────────────────────────────────────────────────

const MAX_MOUNTS: usize = 8;

#[derive(Clone, Copy)]
pub struct MountEntry {
    pub prefix: &'static str,
    pub port:   u32,
    pub in_use: bool,
    pub device: &'static str,
    pub fstype: &'static str,
    /// MS_NOSUID: execve ignores set-user-ID / set-group-ID bits of files
    /// on this mount; /proc/mounts lists `nosuid`.
    pub nosuid: bool,
    /// MS_RDONLY: mounted (or remounted) read-only; /proc/mounts lists `ro`
    /// and statfs reports ST_RDONLY. The filesystem server keeps it current
    /// (`set_mount_rdonly_by_port`).
    pub rdonly: bool,
    /// `st_dev` of every inode on this mount, in [`makedev`]'s encoding: the
    /// backing block device's dev_t (as Linux reports it), or an anonymous
    /// `0:<n>` when the device is not in the block registry. Unique among
    /// mounts so (st_dev, st_ino) identifies a file across volumes.
    pub dev: u64,
}

impl MountEntry {
    const fn empty() -> Self {
        Self { prefix: "", port: 0, in_use: false, device: "", fstype: "", nosuid: false, rdonly: false, dev: 0 }
    }
}

/// `st_dev` of everything served by the VFS itself that is not a tmpfs
/// (initrd, ramfs pseudo-directories, /proc, /dev nodes): anonymous 0:1.
pub const VFS_ANON_DEV: u64 = 1;

/// VFS-private: a filesystem met a symlink while the call was marked strict (mount
/// namespace task, paths already resolved in ns space). `handle_as` retries the
/// whole resolution at most twice, then answers ELOOP. Never reaches userspace.
pub const VFS_ELINKRACE: i32 = -4100;

/// Anonymous `st_dev` of the `i`-th built-in tmpfs root (TMPFS_ROOTS order),
/// matching the minor /proc/self/mountinfo prints for it.
fn tmpfs_root_dev(i: usize) -> u64 { makedev(0, 10 + i as u32) }

/// `st_dev` of the tmpfs root owning `path`, if any.
fn tmpfs_dev_of(path: &[u8]) -> Option<u64> {
    let r = tmpfs_root_of(path)?;
    TMPFS_ROOTS.iter().position(|&x| x == r).map(tmpfs_root_dev)
}

/// `st_dev` of the mount served on `port` (`VFS_ANON_DEV` if none).
pub fn mount_dev_by_port(port: u32) -> u64 {
    MOUNTS.lock().iter().find(|e| e.in_use && e.port == port).map_or(VFS_ANON_DEV, |e| e.dev)
}

/// Overwrite `st_dev` (offset 0 on both arches) in a filled user stat buffer.
fn set_stat_dev(stat_ptr: usize, dev: u64) {
    unsafe { sched::uaccess::copy_raw(stat_ptr as *mut u8, &dev as *const u64 as *const u8, 8); }
}

static MOUNTS: Mutex<[MountEntry; MAX_MOUNTS]> =
    Mutex::new([const { MountEntry::empty() }; MAX_MOUNTS]);

/// Register a mounted filesystem at `prefix` (e.g. "/mnt") to an IPC `port`.
pub fn register_mount(prefix: &'static str, port: u32, device: &'static str, fstype: &'static str) {
    // Linux reports the backing device's major:minor as st_dev. Two volumes
    // cloned from one image share every inode number, so a shared st_dev made
    // `cp -a /a/f /b/f` refuse "are the same file" and broke -xdev walks.
    let bdev = block::lookup_dev_node(device.as_bytes())
        .map(block::dev_rdev).filter(|&d| d != 0);
    let mut m = MOUNTS.lock();
    if let Some(i) = m.iter().position(|e| !e.in_use) {
        let dev = bdev.filter(|&d| !m.iter().any(|e| e.in_use && e.dev == d))
            .unwrap_or(makedev(0, 32 + i as u32));
        m[i] = MountEntry { prefix, port, in_use: true, device, fstype, nosuid: false, rdonly: false, dev };
        recount_tmp_nested(&*m);
    }
}

/// Devices a filesystem is mounting right now (its first I/O precedes `register_mount`):
/// a module that owns the disk must not unload meanwhile.
static MOUNT_PINS: Mutex<alloc::vec::Vec<&'static str>> = Mutex::new(alloc::vec::Vec::new());

pub fn pin_mounting(device: &'static str) { MOUNT_PINS.lock().push(device); }
pub fn unpin_mounting(device: &'static str) {
    let mut p = MOUNT_PINS.lock();
    if let Some(i) = p.iter().position(|d| *d == device) { p.remove(i); }
}
/// True while a mount of `device` ("/dev/x" or "x") is in progress.
pub fn mounting_device(device: &str) -> bool {
    let w = device.strip_prefix("/dev/").unwrap_or(device);
    MOUNT_PINS.lock().iter().any(|d| d.strip_prefix("/dev/").unwrap_or(d) == w)
}

/// Why the block-device family rooted at `paths` ("/dev/x" or "x": a disk and its partitions)
/// cannot lose its driver, or None. A busy pin for module unload (D6/D9). Matches by BDEVS slot
/// (the disk, every `Backing::Part` whose parent is one of them, so BLKPG partitions count) and
/// not by name. Checks, in order: a filesystem mounting, a mount on any family member, an open fd
/// (any process table, or queued in SCM_RIGHTS), a loop bound directly to a family member.
/// Scans run with the owner's Upper gate closed; an open that races it fails EIO (ENXIO once the
/// slot is retired) rather than reaching freed module code, even if the unload is then refused.
pub fn block_family_busy(paths: &[&str]) -> Option<&'static str> {
    let mut roots: alloc::vec::Vec<u16> = alloc::vec::Vec::new();
    for p in paths {
        if mounting_device(p) { return Some("mounting"); }
        let mut path = alloc::string::String::from("/dev/");
        path.push_str(p.strip_prefix("/dev/").unwrap_or(p));
        if let Some(d) = block::lookup_dev_node(path.as_bytes()) { roots.push(d); }
    }
    let fam = block::family_slots(&roots);
    let mounts = list_mounts();
    for m in mounts.iter().filter(|m| m.in_use) {
        let w = m.device.strip_prefix("/dev/").unwrap_or(m.device);
        if fam.iter().any(|(_, n)| n.as_str() == w) { return Some("mounted"); }
    }
    if fam.iter().any(|(d, _)| BLK_FDREFS.get(*d as usize).map_or(false, |c| c.load(atomic::Ordering::Acquire) != 0)) {
        return Some("block device open");
    }
    if block::loop_bound_on(&fam) { return Some("loop device bound"); }
    None
}

/// Snapshot of every currently registered mount (for `/proc/mounts`, `mount`, `lsblk`).
pub fn list_mounts() -> [MountEntry; MAX_MOUNTS] {
    *MOUNTS.lock()
}

/// Unregister a mounted filesystem at `prefix` (e.g. "/mnt").
pub fn unregister_mount(prefix: &str) -> bool {
    let mut m = MOUNTS.lock();
    if let Some(slot) = m.iter_mut().find(|e| e.in_use && e.prefix == prefix) {
        *slot = MountEntry::empty();
        recount_tmp_nested(&*m);
        true
    } else {
        false
    }
}

/// Longest-prefix match: if `path` falls under any registered mount, return its port.
/// True when a mount whose prefix lies inside a tmpfs namespace (something
/// mounted on /tmp/x) covers `path`.
fn mounted_inside_tmpfs(path: &[u8]) -> bool {
    MOUNTS.lock().iter().any(|e| {
        let pb = e.prefix.as_bytes();
        e.in_use && tmpfs_root_of(pb).is_some() && path.starts_with(pb)
            && (path.len() == pb.len() || path.get(pb.len()) == Some(&b'/'))
    })
}

/// Set or clear MS_NOSUID on the mount at exactly `prefix` (mount(2) and
/// MS_REMOUNT). False when nothing is mounted there.
pub fn set_mount_nosuid(prefix: &str, nosuid: bool) -> bool {
    let mut m = MOUNTS.lock();
    match m.iter_mut().find(|e| e.in_use && e.prefix == prefix) {
        Some(e) => { e.nosuid = nosuid; true }
        None => false,
    }
}

/// Record whether the mount served by `port` is read-only (mount -o ro, a
/// remount, or the filesystem going read-only at shutdown). False when no
/// mount uses that port.
pub fn set_mount_rdonly_by_port(port: u32, rdonly: bool) -> bool {
    let mut m = MOUNTS.lock();
    let mut hit = false;
    for e in m.iter_mut().filter(|e| e.in_use && e.port == port) { e.rdonly = rdonly; hit = true; }
    hit
}

/// The ST_* flags statfs reports for the mount served by `port`.
fn mount_st_flags(port: u32) -> u64 {
    const ST_RDONLY: u64 = 1;
    const ST_NOSUID: u64 = 2;
    let m = MOUNTS.lock();
    m.iter().find(|e| e.in_use && e.port == port).map_or(0, |e| {
        (if e.rdonly { ST_RDONLY } else { 0 }) | (if e.nosuid { ST_NOSUID } else { 0 })
    })
}

/// The comma-separated option list /proc/mounts and mountinfo print first:
/// `rw`/`ro`, then `nosuid`.
fn mount_opts(m: &MountEntry) -> &'static [u8] {
    match (m.rdonly, m.nosuid) {
        (false, false) => b"rw",
        (false, true)  => b"rw,nosuid",
        (true, false)  => b"ro",
        (true, true)   => b"ro,nosuid",
    }
}

/// Does backing path `path` live on a mount with MS_NOSUID? The mount is the
/// one `find_mount_port` routes the path to; a tmpfs root carries its own
/// flags (`set_tmpfs_root_flags`).
pub fn path_on_nosuid_mount(path: &[u8]) -> bool {
    if tmpfs_path(path).is_some() { return tmpfs_flags_of(path) & TMPFS_MS_NOSUID != 0; }
    let m = MOUNTS.lock();
    let mut best: Option<(usize, bool)> = None;
    for e in m.iter() {
        if !e.in_use { continue; }
        let pb = e.prefix.as_bytes();
        if path.starts_with(pb) && (pb == b"/" || path.len() == pb.len() || path.get(pb.len()) == Some(&b'/')) {
            if best.map_or(true, |(l, _)| pb.len() >= l) { best = Some((pb.len(), e.nosuid)); }
        }
    }
    best.map_or(false, |(_, n)| n)
}

// Mount flags of the built-in tmpfs roots (TMPFS_ROOTS order), as the
// mount(2) MS_* bits. The roots exist from boot, so `mount -t tmpfs -o ...`
// onto one, or a remount of it, only sets these (the contents stay).
const TMPFS_MS_NOSUID: u8 = 2;
const TMPFS_MS_NODEV:  u8 = 4;
const TMPFS_MS_NOEXEC: u8 = 8;
static TMPFS_ROOT_FLAGS: [atomic::AtomicU8; 8] = [const { atomic::AtomicU8::new(0) }; 8];

fn tmpfs_root_index(path: &[u8]) -> Option<usize> {
    let r = tmpfs_root_of(path)?;
    TMPFS_ROOTS.iter().position(|&x| x == r).filter(|&i| i < TMPFS_ROOT_FLAGS.len())
}

/// mount(2) / remount on a tmpfs root: replace its nosuid/nodev/noexec
/// flags. False when `path` is not exactly a tmpfs root.
pub fn set_tmpfs_root_flags(path: &[u8], ms_flags: usize) -> bool {
    if !is_tmpfs_root(path) { return false; }
    let Some(i) = tmpfs_root_index(path) else { return false };
    let f = (ms_flags & (TMPFS_MS_NOSUID | TMPFS_MS_NODEV | TMPFS_MS_NOEXEC) as usize) as u8;
    TMPFS_ROOT_FLAGS[i].store(f, atomic::Ordering::Relaxed);
    true
}

fn tmpfs_flags_of(path: &[u8]) -> u8 {
    if let Some(n) = mnt::ns0::tmpfs_inst_of(path) { return tmpfs_inst_attr(n).0; }
    tmpfs_root_index(path).map_or(0, |i| TMPFS_ROOT_FLAGS[i].load(atomic::Ordering::Relaxed))
}

/// MS_NOEXEC: execve of a file on this mount fails with EACCES. Only the
/// tmpfs roots take the flag. (tmpfs holds no device nodes, so nodev is
/// recorded and reported but has nothing more to refuse.)
pub fn path_on_noexec_mount(path: &[u8]) -> bool {
    tmpfs_path(path).is_some() && tmpfs_flags_of(path) & TMPFS_MS_NOEXEC != 0
}

/// ST_* flags statfs reports for a tmpfs path (ST_NOSUID=2, ST_NODEV=4,
/// ST_NOEXEC=8 coincide with the MS_* bits).
fn tmpfs_st_flags(path: &[u8]) -> u64 { tmpfs_flags_of(path) as u64 }

/// mmap(PROT_EXEC) / mprotect(PROT_EXEC) of the file open as `fd` is refused
/// (EPERM) when it lives on a noexec tmpfs mount. Nameless/ephemeral inodes
/// (memfd, dmabuf, unlinked) are not on a mount and are never refused.
pub fn fd_on_noexec_mount(pid: u32, fd: usize) -> bool {
    let Some(idx) = tmpfile_owner_of(pid, fd) else { return false };
    let tmp = TMP_FILES.lock();
    let e = &tmp[idx];
    if e.ephemeral() || e.unlinked() { return false; }
    tmpfs_st_flags(e.path()) & TMPFS_MS_NOEXEC as u64 != 0
}

/// The option list /proc/mounts and mountinfo print for a tmpfs root.
fn tmpfs_opts(root: &[u8]) -> &'static [u8] {
    match tmpfs_flags_of(root) {
        0 => b"rw", 2 => b"rw,nosuid", 4 => b"rw,nodev", 6 => b"rw,nosuid,nodev",
        8 => b"rw,noexec", 10 => b"rw,nosuid,noexec", 12 => b"rw,nodev,noexec",
        _ => b"rw,nosuid,nodev,noexec",
    }
}

fn find_mount_port(path: &[u8]) -> Option<u32> {
    let m = MOUNTS.lock();
    let mut best_len = 0usize;
    let mut best_port = 0u32;
    let mut found = false;
    for e in m.iter() {
        if !e.in_use { continue; }
        let pb = e.prefix.as_bytes();
        if path.starts_with(pb) && (pb == b"/" || path.len() == pb.len() || path.get(pb.len()) == Some(&b'/')) {
            if pb.len() >= best_len {
                best_len = pb.len();
                best_port = e.port;
                found = true;
            }
        }
    }
    if found { Some(best_port) } else { None }
}

// ── Pipe ring buffers ─────────────────────────────────────────────────────────

/// Capacity of one pipe's ring. Linux's default is 65536; 16K is the
/// compromise that keeps the static pool affordable (MAX_PIPES * this) while
/// being large enough for the buffers real programs assume. It matters more
/// now that a full ring blocks the writer instead of failing the write: brush
/// stages here-documents by writing the whole body into a pipe *before*
/// anything reads it, so a body larger than one ring would deadlock rather
/// than merely error. See the F_SETPIPE_SZ arm in handle_fcntl, which refuses
/// requests above this so that case fails cleanly instead of wedging.
pub const PIPE_RING_SIZE: usize = 16384;
/// Most bytes one pipe read() delivers; callers (kernel prefault) clamp to it.
pub const PIPE_READ_MAX: usize = 4096;
/// POSIX PIPE_BUF: a write of at most this many bytes is atomic — it is
/// either written whole or (blocking fd) waits / (O_NONBLOCK) fails EAGAIN,
/// never interleaved with another writer's bytes. Also the POLLOUT threshold:
/// the write end is writable when at least PIPE_BUF bytes are free (Linux).
pub const PIPE_BUF: usize = 4096;
// The full COSMIC desktop session spawns ~14 long-lived components via
// cosmic-session/launch_pad, each holding 3 stdio pipes (stdin/stdout/stderr)
// for its whole lifetime so launch_pad can stream on_stdout/on_stderr — plus
// dbus-run-session/busd and transient restart overlap. At MAX_PIPES=16 the pool
// exhausted after ~5 spawns and every later component (cosmic-bg, -osd,
// -workspaces, -launcher, and the panel's own children) failed with ENFILE
// (-23), triggering a restart storm that corrupted already-connected clients.
// 128 pipes (2 MiB static, trivial on the 2 GiB guest) covers the whole session
// simultaneously with generous headroom. PIPE_RING_SIZE stays 16 KiB — here-doc
// staging and the F_SETPIPE_SZ cap depend on it (see the note above).
//
// 128 -> grown on demand (lane fdmax): a build (make -j, cargo) or a process
// holding hundreds of pipes ran the static 128 dry with ENFILE. The table now
// grows one ring at a time, and a ring's 16 KiB buffer exists only while the
// pipe has an end open, so the cost is the pipes in use. The cap bounds
// what a runaway pipe() loop can pin: 16384 live pipes is 256 MiB of buffers.
const MAX_PIPES:      usize = 16384;

struct PipeRing {
    /// PIPE_RING_SIZE bytes while the pipe has an end open; empty otherwise.
    buf:         alloc::vec::Vec<u8>,
    read_pos:    usize,
    write_pos:   usize,
    count:       usize,
    // Reference counts of open read / write endpoints, not booleans: a pipe end
    // can be held by more than one fd (via dup/dup2 or inherited across fork), so
    // EOF/EPIPE/POLLHUP must only fire once the LAST fd on that end is closed.
    readers:     u32,
    writers:     u32,
    /// Monotonic event counter, bumped on every state change that can newly
    /// assert readiness (data written, data read → write-end space freed,
    /// last writer closed → EOF/HUP, last reader closed → EPIPE/ERR). The
    /// epoll layer emulates edge-triggered (EPOLLET) delivery by remembering
    /// the seq it last reported for an interest and re-firing only when the
    /// seq advances — so a pipe stuck permanently readable at EOF (POLLIN|
    /// POLLHUP) can't pin tokio's reactor in a level-triggered epoll spin, and
    /// a self-pipe byte written between two epoll_waits is never lost.
    seq:         u64,
    /// Bytes promised to splice(2)s that are reading their source right now
    /// ([`pipe_reserve`]): free space no other writer may take, so every
    /// byte such a splice consumed from a socket or console has room here.
    reserved:    usize,
}

impl PipeRing {
    const fn new() -> Self {
        Self {
            buf: alloc::vec::Vec::new(),
            read_pos: 0, write_pos: 0, count: 0,
            readers: 0, writers: 0,
            seq: 0,
            reserved: 0,
        }
    }

    /// Space a writer may fill now: the ring minus its bytes and minus the
    /// space splices have reserved.
    fn free(&self) -> usize {
        PIPE_RING_SIZE.saturating_sub(self.count + self.reserved)
    }

    /// Move up to `max` bytes out to the caller's buffer (user or kernel
    /// memory). PIPE_RINGS is held, so the copy is the fault-tolerant one: a
    /// fault it cannot resolve (a read-only or PROT_NONE buffer) stops it
    /// instead of killing the caller with the lock held — which left every
    /// later pipe user spinning on PIPE_RINGS. Only delivered bytes are
    /// consumed; `Err(EFAULT)` means none were.
    fn copy_out(&mut self, dst: *mut u8, max: usize) -> Result<usize, i32> {
        let n = max.min(self.count);
        if n != 0 && self.buf.len() != PIPE_RING_SIZE { return Ok(0); }
        let mut done = 0usize;
        while done < n {
            let chunk = (n - done).min(PIPE_RING_SIZE - self.read_pos);
            let left = unsafe {
                sched::uaccess::copy_raw(dst.add(done), self.buf.as_ptr().add(self.read_pos), chunk)
            };
            let copied = chunk - left;
            self.read_pos = (self.read_pos + copied) % PIPE_RING_SIZE;
            self.count -= copied;
            done += copied;
            if left != 0 { return if done == 0 { Err(-14) } else { Ok(done) }; }
        }
        Ok(done)
    }

    /// Append up to `max` bytes from the caller's buffer; same fault rules
    /// as [`copy_out`](Self::copy_out).
    fn copy_in(&mut self, src: *const u8, max: usize) -> Result<usize, i32> {
        let n = max.min(self.free());
        if n != 0 && self.buf.len() != PIPE_RING_SIZE { return Ok(0); }
        let mut done = 0usize;
        while done < n {
            let chunk = (n - done).min(PIPE_RING_SIZE - self.write_pos);
            let left = unsafe {
                sched::uaccess::copy_raw(self.buf.as_mut_ptr().add(self.write_pos), src.add(done), chunk)
            };
            let copied = chunk - left;
            self.write_pos = (self.write_pos + copied) % PIPE_RING_SIZE;
            self.count += copied;
            done += copied;
            if left != 0 { return if done == 0 { Err(-14) } else { Ok(done) }; }
        }
        Ok(done)
    }
}

static PIPE_RINGS: sched::lockwatch::TrackedMutex<alloc::vec::Vec<PipeRing>> =
    sched::lockwatch::TrackedMutex::new(sched::lockwatch::L_PIPE_RINGS, alloc::vec::Vec::new());

/// Bump the reader/writer refcount for a pipe endpoint when an fd referring to
/// it is duplicated (dup, dup2, fork inheritance). No-op for non-pipe fds.
/// Caller must NOT hold the FD_TABLES lock (we take PIPE_RINGS here).
fn pipe_ref_inc(kind: &VnodeKind) {
    match kind {
        VnodeKind::Pipe { ring, is_write } => {
            let mut rings = PIPE_RINGS.lock();
            if *is_write { rings[*ring].writers += 1; } else { rings[*ring].readers += 1; }
        }
        // A pty end is refcounted for exactly the reason a pipe end is: the
        // last master fd closing is the hangup that SIGHUPs the slave's job,
        // and the last slave fd closing is the EOF a terminal emulator reads
        // as "my child exited". A fork that did not bump this would fire both
        // the moment the parent closed its copy.
        VnodeKind::Pty { pair, is_master } => {
            tty_server::pty::add_ref(*pair as usize, *is_master);
        }
        // eventfd/timerfd slots are pooled and shared across dup'd fds; bump the
        // slot refcount so the pool entry survives until the last fd closes.
        VnodeKind::EventFd { slot } => { EVENTFD_REFS.lock()[*slot] += 1; }
        VnodeKind::NsFd { id, .. } => sched::ns::hold_raw(*id),
        VnodeKind::BpfProg { id } => { sched::bpfcg::prog_get(*id); }
        VnodeKind::Mq { slot } => { mqueue::ref_inc(*slot); }
        VnodeKind::TimerFd { slot } => { TIMERFD_REFS.lock()[*slot] += 1; }
        // A dynamic-device open is shared by every duplicate of its fd, so the
        // device server must not be told "closed" until the last one goes. This
        // one arm covers dup/dup2/dup3, fcntl(F_DUPFD{,_CLOEXEC}), fork
        // inheritance and SCM_RIGHTS export — they all funnel through here.
        VnodeKind::DynamicDevice { open_id, .. } => device_open_inc(*open_id),
        // The copy shares the open file description, and with it the offset.
        VnodeKind::TmpFile { ofd, .. } => tmp_ofd_get(*ofd),
        _ => {}
    }
}

/// Drop one reference to a pipe endpoint (a close, or a dup2 that overwrites an
/// already-open fd). Saturating so a stray double-close can never underflow.
/// When the last endpoint on BOTH sides is gone, reset the ring: the allocator
/// in handle_pipe only reuses slots with `count == 0`, so a pipe abandoned with
/// unread data would otherwise leak its slot for the lifetime of the system.
fn pipe_drop_ref(ring: usize, is_write: bool) {
    let edge = {
        let mut rings = PIPE_RINGS.lock();
        if is_write { rings[ring].writers = rings[ring].writers.saturating_sub(1); }
        else        { rings[ring].readers = rings[ring].readers.saturating_sub(1); }
        // The last writer or reader going away is a new pollable edge (EOF/HUP on
        // the read end, EPIPE/ERR on the write end) — advance the seq so epoll
        // re-fires it once, edge-triggered.
        let edge = (is_write && rings[ring].writers == 0) || (!is_write && rings[ring].readers == 0);
        if edge { rings[ring].seq = rings[ring].seq.wrapping_add(1); }
        if rings[ring].readers == 0 && rings[ring].writers == 0 {
            rings[ring].read_pos  = 0;
            rings[ring].write_pos = 0;
            rings[ring].count     = 0;
            // The buffer goes with the last end; a new pipe allocates its own.
            rings[ring].buf       = alloc::vec::Vec::new();
        }
        edge
    };
    // A level change too: the read end becomes readable-at-EOF, the write end
    // POLLERR. Until 2026-09-16 nothing woke a poller (or, now, a parked
    // blocking read()er/write()r) for a close(2) — only a process exit did, via
    // the SIGCHLD broadcast — so a live process closing its end left the peer
    // asleep until some unrelated wake. PIPE_RINGS is released first (lock
    // order: never take RUN_QUEUE under a server lock).
    // The last writer leaving is the READ end's edge (EOF), the last reader
    // leaving the WRITE end's (EPIPE/POLLERR): wake only that end.
    if edge { sched::wake_poll_key(pipe_end_key(ring, !is_write)); }
}

fn pipe_ref_dec(kind: &VnodeKind) {
    match kind {
        VnodeKind::Pipe { ring, is_write } => {
            pipe_drop_ref(*ring, *is_write);
        }
        VnodeKind::Pty { pair, is_master } => {
            tty_server::pty::drop_ref(*pair as usize, *is_master);
        }
        // Symmetric with pipe_ref_inc: a dup2/dup3 that overwrites an open
        // eventfd/timerfd fd releases its slot reference; free the slot only
        // when the last sharer is gone (see EVENTFD_REFS).
        VnodeKind::EventFd { slot } => {
            let mut refs = EVENTFD_REFS.lock();
            refs[*slot] = refs[*slot].saturating_sub(1);
            if refs[*slot] == 0 {
                EVENTFD_COUNTERS.lock()[*slot] = u64::MAX;
                EVENTFD_SEQ.lock()[*slot] = 0;
            }
        }
        VnodeKind::NsFd { id, .. } => sched::ns::put_raw(*id),
        VnodeKind::BpfProg { id } => sched::bpfcg::prog_put(*id),
        VnodeKind::TimerFd { slot } => {
            let mut refs = TIMERFD_REFS.lock();
            refs[*slot] = refs[*slot].saturating_sub(1);
            if refs[*slot] == 0 { TIMERFD_POOL.lock()[*slot] = TimerFdEntry::free(); }
        }
        VnodeKind::Mq { slot } => { mqueue::ref_dec(*slot, sched::current_pid()); }
        // Symmetric with pipe_ref_inc. This also closes a pre-existing leak:
        // the dup2/dup3 overwrite path retired a dynamic-device fd without ever
        // sending VFS_CLOSE, so the server kept the open alive forever.
        VnodeKind::DynamicDevice { port, dev_id, open_id } => {
            device_close(*port, *dev_id, *open_id);
        }
        // A dup2/dup3 overwriting a tmpfs fd drops its description reference.
        VnodeKind::TmpFile { ofd, .. } => tmp_ofd_put(*ofd),
        _ => {}
    }
}

// ── splice(2) ring access ─────────────────────────────────────────────────────
//
// splice moves bytes between a pipe and another descriptor without a user
// buffer. The kernel drives it (kernel/src/syscall.rs `sys_splice`) through
// these, which are the read and write arms of `handle_read`/`handle_write`
// cut at the point where the other end's I/O has to happen: the pipe side
// can be peeked and consumed separately, so bytes a file or socket did not
// accept stay in the pipe instead of being lost, and the writer side is
// sized to the space the ring has, so nothing ever waits on a full ring
// with the bytes held hostage in a kernel buffer. Every function takes
// PIPE_RINGS itself, never with another VFS lock held, and wakes pollers
// exactly as the read/write arms do (after the lock is released).

/// The pipe end `fd` names: `(ring, is_write)`.
pub fn pipe_end_of(pid: u32, fd: usize) -> Option<(usize, bool)> {
    match vfs_get_node_kind(pid, fd)? {
        VnodeKind::Pipe { ring, is_write } => Some((ring, is_write)),
        _ => None,
    }
}

/// What a splice parked on this pipe end waits for: data or end of input on
/// the read end, space or a vanished reader on the write end.
pub fn pipe_ready(ring: usize, is_write: bool) -> bool {
    let rings = PIPE_RINGS.lock();
    let r = &rings[ring];
    if is_write { r.readers == 0 || r.free() > 0 } else { r.count > 0 || r.writers == 0 }
}

/// The wake a drain of `n` bytes owes the write end (see handle_read).
fn pipe_drained(ring: usize, was_full: bool, now_writable: bool, n: usize) {
    if n == 0 { return; }
    if was_full && now_writable { sched::wake_poll_key(pipe_wkey(ring)); }
    else                        { sched::request_poll_wake_key(pipe_wkey(ring)); }
}

/// The wake an append of `n` bytes owes the read end (see handle_write).
fn pipe_filled(ring: usize, was_empty: bool, n: usize) {
    if n == 0 { return; }
    if was_empty { sched::wake_poll_key(pipe_rkey(ring)); }
    else         { sched::request_poll_wake_key(pipe_rkey(ring)); }
}

/// Copy up to `max` bytes from the front of `ring` into kernel memory at
/// `dst` WITHOUT consuming them. `Ok(0)` is end of input (empty, no writer
/// left), `Err(-11)` empty with a writer still open.
pub fn pipe_peek(ring: usize, dst: *mut u8, max: usize) -> Result<usize, i32> {
    let rings = PIPE_RINGS.lock();
    let r = &rings[ring];
    if r.count == 0 { return if r.writers > 0 { Err(-11) } else { Ok(0) }; }
    if r.buf.len() != PIPE_RING_SIZE { return Ok(0); } // no end open: nothing buffered
    let n = max.min(r.count);
    let first = n.min(PIPE_RING_SIZE - r.read_pos);
    unsafe {
        core::ptr::copy_nonoverlapping(r.buf.as_ptr().add(r.read_pos), dst, first);
        core::ptr::copy_nonoverlapping(r.buf.as_ptr(), dst.add(first), n - first);
    }
    Ok(n)
}

/// Drop `n` bytes a [`pipe_peek`] returned, now that they were delivered.
pub fn pipe_consume(ring: usize, n: usize) {
    if n == 0 { return; }
    let (was_full, now_writable, n) = {
        let mut rings = PIPE_RINGS.lock();
        let r = &mut rings[ring];
        let n = n.min(r.count);
        let was_full = r.free() < PIPE_BUF;
        r.read_pos = (r.read_pos + n) % PIPE_RING_SIZE;
        r.count -= n;
        if n > 0 { r.seq = r.seq.wrapping_add(1); }
        (was_full, r.free() >= PIPE_BUF, n)
    };
    pipe_drained(ring, was_full, now_writable, n);
}

/// Reserve up to `max` bytes of free space in `ring` for a splice that is
/// about to read its source; returns how many were reserved (0 when full)
/// or `Err(-32)` (EPIPE) once no reader is left. Every reservation is ended
/// by exactly one [`pipe_commit`]. Until then no other writer can take that
/// space, so a writer racing into the same pipe can no longer leave the
/// splice holding bytes it consumed from a socket or console and has
/// nowhere to put.
pub fn pipe_reserve(ring: usize, max: usize) -> Result<usize, i32> {
    let mut rings = PIPE_RINGS.lock();
    let r = &mut rings[ring];
    if r.readers == 0 { return Err(-32); }
    if r.buf.len() != PIPE_RING_SIZE { return Ok(0); }
    let n = max.min(r.free());
    r.reserved += n;
    Ok(n)
}

/// End a [`pipe_reserve`] of `reserved` bytes: append the `n <= reserved`
/// bytes of kernel memory at `src` the splice read, and give the rest of the
/// reservation back. Returns `n`, or `Err(-32)` (EPIPE) when the last reader
/// left meanwhile (the bytes are dropped, as Linux drops a pipe's contents
/// with its last reader).
pub fn pipe_commit(ring: usize, src: *const u8, n: usize, reserved: usize) -> Result<usize, i32> {
    let n = n.min(reserved);
    let (was_empty, was_full, now_writable, released, res) = {
        let mut rings = PIPE_RINGS.lock();
        let r = &mut rings[ring];
        let was_full = r.free() < PIPE_BUF;
        r.reserved = r.reserved.saturating_sub(reserved);
        let was_empty = r.count == 0;
        let res = if r.readers == 0 {
            Err(-32)
        } else if n > 0 && r.buf.len() == PIPE_RING_SIZE {
            // The reservation kept this space free: it all fits.
            let done = n.min(PIPE_RING_SIZE - r.count);
            let first = done.min(PIPE_RING_SIZE - r.write_pos);
            unsafe {
                core::ptr::copy_nonoverlapping(src, r.buf.as_mut_ptr().add(r.write_pos), first);
                core::ptr::copy_nonoverlapping(src.add(first), r.buf.as_mut_ptr(), done - first);
            }
            r.write_pos = (r.write_pos + done) % PIPE_RING_SIZE;
            r.count += done;
            r.seq = r.seq.wrapping_add(1);
            Ok(done)
        } else {
            Ok(0)
        };
        let done = *res.as_ref().unwrap_or(&0);
        if done < reserved { r.seq = r.seq.wrapping_add(1); }
        (was_empty, was_full, r.free() >= PIPE_BUF, reserved - done, res)
    };
    if let Ok(done) = res { pipe_filled(ring, was_empty, done); }
    // Space handed back is space a parked writer may be waiting for.
    pipe_drained(ring, was_full, now_writable, released);
    res
}

/// Move up to `max` bytes from ring `src` to ring `dst` in one PIPE_RINGS
/// hold (both rings live in the same table). `Ok(0)` = `src` is at end of
/// input; `Err(-11)` = nothing can move now (src empty with a writer, or dst
/// full); `Err(-32)` = `dst` has no reader. With `peek` (tee(2)) the bytes
/// are copied but stay in `src`.
pub fn pipe_to_pipe(src: usize, dst: usize, max: usize, peek: bool) -> Result<usize, i32> {
    if src == dst { return Err(-22); }
    let (n, src_full, src_writable, dst_empty) = {
        let mut rings = PIPE_RINGS.lock();
        if rings[dst].readers == 0 { return Err(-32); }
        if rings[src].count == 0 { return if rings[src].writers > 0 { Err(-11) } else { Ok(0) }; }
        let n = max.min(rings[src].count).min(rings[dst].free());
        if n == 0 { return Err(-11); }
        let dst_empty = rings[dst].count == 0;
        let src_full = rings[src].free() < PIPE_BUF;
        // Both rings have an end open (src a reader, dst a reader), so both
        // buffers exist (see `PipeRing::buf`).
        if rings[src].buf.len() != PIPE_RING_SIZE || rings[dst].buf.len() != PIPE_RING_SIZE { return Err(-11); }
        let (sp, mut wp) = (rings[src].read_pos, rings[dst].write_pos);
        for i in 0..n {
            let b = rings[src].buf[(sp + i) % PIPE_RING_SIZE];
            rings[dst].buf[wp] = b;
            wp = (wp + 1) % PIPE_RING_SIZE;
        }
        rings[dst].write_pos = wp;
        rings[dst].count += n;
        rings[dst].seq = rings[dst].seq.wrapping_add(1);
        if !peek {
            let s = &mut rings[src];
            s.read_pos = (s.read_pos + n) % PIPE_RING_SIZE;
            s.count -= n;
            s.seq = s.seq.wrapping_add(1);
        }
        (n, src_full, rings[src].free() >= PIPE_BUF, dst_empty)
    };
    if !peek { pipe_drained(src, src_full, src_writable, n); }
    pipe_filled(dst, dst_empty, n);
    Ok(n)
}

// ── eventfd counters ──────────────────────────────────────────────────────────

// A real calloop-based Wayland compositor (cosmic-comp) holds many live
// eventfds at once — calloop's Ping/Channel wakers, smithay worker-thread
// notifiers, zbus — comfortably more than 16 across the compositor + D-Bus
// broker + init. With slots now correctly refcounted (freed only when the last
// dup closes) the pool no longer masks exhaustion by early-freeing, so 16 filled
// and eventfd()/inotify creation started returning EMFILE mid-init. 64 is a pure
// sizing bump (small fixed arrays), matching the MAX_FDS 64→128 headroom rationale.
//
// 64→256 (m7z2): this is a GLOBAL, session-wide pool. A full COSMIC session runs
// ~20 processes (comp, busd, panel, dock, bg, greeter, idle, workspaces,
// notifications, launcher, osd, settings-daemon, app-library, …); each event loop
// (polling/mio/calloop) allocates an eventfd waker, and every process runs several
// (tokio runtime + calloop + one per notify::Watcher). At 64 the pool exhausted
// session-wide — a process holding only 15 fds still got EMFILE on eventfd() —
// which crash-looped cosmic-panel/cosmic-greeter (calloop Ping / winit loop
// creation). ~20 procs × ~6-8 eventfds ⇒ ~150; 256 covers that plus restart
// overlap. Pool arrays are u64/u32 (256 × 20 B ≈ 5 KB), a cheap bump.
//
// 256 -> 1024 (lane multiterm, 2026-09-27): measured on a GPU session, the
// pool held 235 slots with an empty desktop and 254 after the first
// cosmic-term (~17 per instance: tokio + calloop + wgpu), so the second
// cosmic-term died at startup on calloop's Ping ("Failed to create a Ping:
// TooManyOpenFiles") and no window ever appeared. There was no leak: slots
// tracked live fds. 1024 x 20 B is ~20 KB.
//
// 1024 -> grown on demand (lane fdmax): the four per-slot arrays below start
// empty and grow together (`eventfd_grow`, EVENTFD_GROW slots at a time) up
// to this ceiling, so a process with thousands of eventfds is bounded by its
// RLIMIT_NOFILE, not by a session-wide table sized for a desktop.
const MAX_EVENTFDS: usize = 1 << 18;
const EVENTFD_GROW: usize = 256;
// u64::MAX = free slot sentinel.
static EVENTFD_COUNTERS: Mutex<alloc::vec::Vec<u64>> = Mutex::new(alloc::vec::Vec::new());
/// Per-eventfd monotonic event counter, bumped on every write. mio registers
/// its waker eventfd edge-triggered (EPOLLET) and never drains it, so its
/// POLLIN level stays high forever; the epoll layer compares this seq to
/// re-fire once per wake() instead of spinning on the stuck level.
static EVENTFD_SEQ: Mutex<alloc::vec::Vec<u64>> = Mutex::new(alloc::vec::Vec::new());
/// Per-slot EFD_SEMAPHORE: a read returns 1 and subtracts 1 instead of
/// returning and zeroing the whole counter. Never held with another lock.
static EVENTFD_SEM: Mutex<alloc::vec::Vec<bool>> = Mutex::new(alloc::vec::Vec::new());
/// Per-slot fd refcount. An eventfd can be dup'd (dup/dup2/fork/exec-keep), so
/// several fds share one counter slot. The slot must survive until the LAST of
/// them closes — otherwise close(one dup) frees the slot (counter → u64::MAX),
/// a later eventfd() reuses it, and the two live eventfds alias one counter: a
/// drain of one reads EAGAIN on the other. That aliasing killed cosmic-comp's
/// calloop (a ping-eventfd dup shared a slot with a second source's eventfd; the
/// ping drain emptied it, the source read EAGAIN and calloop treated it fatal).
/// Mirrors the pipe reader/writer refcount; bumped by pipe_ref_inc on every dup.
static EVENTFD_REFS: Mutex<alloc::vec::Vec<u32>> = Mutex::new(alloc::vec::Vec::new());

/// Add EVENTFD_GROW free slots to every eventfd array. False at the ceiling
/// or when the heap refuses. Locks in the established order REFS → COUNTERS →
/// SEQ → META; the tick-side signaller only try-locks, so it simply retries.
fn eventfd_grow() -> bool {
    let mut refs = EVENTFD_REFS.lock();
    let mut counters = EVENTFD_COUNTERS.lock();
    let mut seqs = EVENTFD_SEQ.lock();
    let mut meta = EVENTFD_META.lock();
    let mut sem = EVENTFD_SEM.lock();
    let n = counters.len();
    if counters.iter().any(|&v| v == u64::MAX) { return true; } // a racing grow (or free) beat us
    if n >= MAX_EVENTFDS { return false; }
    let m = (n + EVENTFD_GROW).min(MAX_EVENTFDS) - n;
    if refs.try_reserve_exact(m).is_err() || counters.try_reserve_exact(m).is_err()
        || seqs.try_reserve_exact(m).is_err() || meta.try_reserve_exact(m).is_err()
        || sem.try_reserve_exact(m).is_err() { return false; }
    sem.resize(n + m, false);
    refs.resize(n + m, 0);
    counters.resize(n + m, u64::MAX);
    seqs.resize(n + m, 0);
    meta.resize(n + m, EvMeta::EMPTY);
    true
}

// ── eventfd provenance (diagnostic, Ctrl-T) ─────────────────────────────────
//
// Who minted each live slot: the creating tgid, a boot-wide creation serial,
// the origin (EVFD_ORIGIN_*), the GPU fence id for an out-fence, and a user
// backtrace when the kernel's FD_PROVENANCE_BT is on. All-zero when empty, so
// it stays in .bss. Ctrl-T prints `[FDK] … ev_fence=N` for every heavy table,
// and one `[EVFD]` line per eventfd with EVFD_DUMP_DETAIL. A virgl client's
// GL sync objects each hold one out-fence eventfd, so a compositor that keeps
// GL textures (smithay's per-texture read_sync) shows them here — which is how
// cosmic-comp's retained closed-window textures were found (lane compfdleak).
pub const EVFD_ORIGIN_EVENTFD2: u8 = 1;
pub const EVFD_ORIGIN_FENCE: u8 = 2;
/// A sync file (drivers::sync_file): `fence` holds its token.
pub const EVFD_ORIGIN_SYNC_FILE: u8 = 3;
const EVFD_DUMP_DETAIL: bool = false;
#[derive(Clone, Copy)]
pub struct EvMeta { pub origin: u8, pub tgid: u32, pub serial: u32, pub fence: u64, pub bt: [u64; 16] }
impl EvMeta {
    const EMPTY: Self = EvMeta { origin: 0, tgid: 0, serial: 0, fence: 0, bt: [0; 16] };
}
static EVENTFD_META: Mutex<alloc::vec::Vec<EvMeta>> = Mutex::new(alloc::vec::Vec::new());
static EVENTFD_SERIAL: atomic::AtomicU32 = atomic::AtomicU32::new(0);

/// Record provenance for the eventfd `fd` of `pid` (just created).
pub fn eventfd_note(pid: u32, fd: usize, origin: u8, fence: u64, bt: [u64; 16]) {
    if let Some(VnodeKind::EventFd { slot }) = vfs_get_node_kind(pid, fd) {
        let serial = EVENTFD_SERIAL.fetch_add(1, atomic::Ordering::Relaxed) + 1;
        EVENTFD_META.lock()[slot] = EvMeta { origin, tgid: sched::tgid_of(pid), serial, fence, bt };
    }
}

/// The sync-file token of `pid`'s fd, if that fd is a sync file minted by
/// the DRM layer (an eventfd noted with origin SYNC_FILE).
pub fn eventfd_sync_token(pid: u32, fd: usize) -> Option<u64> {
    match vfs_get_node_kind(pid, fd) {
        Some(VnodeKind::EventFd { slot }) if slot < MAX_EVENTFDS => {
            let m = EVENTFD_META.lock().get(slot).copied()?;
            if m.origin == EVFD_ORIGIN_SYNC_FILE { Some(m.fence) } else { None }
        }
        _ => None,
    }
}

/// `(origin, fence)` of `pid`'s fd if it is an eventfd: the GPU fence id of
/// a FENCE_FD_OUT out-fence (origin FENCE) or the token of a sync file
/// (origin SYNC_FILE). EXECBUFFER's FENCE_FD_IN resolves its fd with this.
pub fn eventfd_fence_meta(pid: u32, fd: usize) -> Option<(u8, u64)> {
    match vfs_get_node_kind(pid, fd) {
        Some(VnodeKind::EventFd { slot }) if slot < MAX_EVENTFDS => {
            let m = EVENTFD_META.lock().get(slot).copied()?;
            Some((m.origin, m.fence))
        }
        _ => None,
    }
}

// ── Kernel-side eventfd signalling (GPU out-fences) ──────────────────────────
//
// A `FENCE_FD_OUT` eventfd is signalled by the DRM layer when the GPU fence it
// was armed on retires — from the 100 Hz tick, not from a syscall. Two seams
// make that safe: the arming side takes an extra slot reference so a caller
// closing the fd before its fence retires cannot have the slot reallocated to
// an unrelated eventfd that then receives our signal, and the signalling side
// only ever try-locks, because a tick must not spin on a pool lock a
// preempted task may hold.

/// Take one more reference on eventfd `slot`. Task context (blocking locks).
pub fn eventfd_slot_ref(slot: usize) {
    if slot >= MAX_EVENTFDS { return; }
    let mut refs = EVENTFD_REFS.lock();
    if refs[slot] > 0 { refs[slot] += 1; }
}

/// Add one to eventfd `slot`, wake its pollers, and drop the reference
/// `eventfd_slot_ref` took. Tick/IRQ safe: try-locks only, and the wake is
/// deferred to the poll-deadline tick if RUN_QUEUE is busy. Returns false — and
/// changes nothing — if a pool lock was contended; the caller retries later.
pub fn eventfd_slot_signal_unref(slot: usize) -> bool {
    if slot >= MAX_EVENTFDS { return true; }
    let mut refs = match EVENTFD_REFS.try_lock() { Some(g) => g, None => return false };
    let mut counters = match EVENTFD_COUNTERS.try_lock() { Some(g) => g, None => return false };
    let mut seqs = match EVENTFD_SEQ.try_lock() { Some(g) => g, None => return false };
    let live = refs[slot] > 0 && counters[slot] != u64::MAX;
    refs[slot] = refs[slot].saturating_sub(1);
    if refs[slot] == 0 {
        // Ours was the last reference: the fd is gone, nobody can read it.
        counters[slot] = u64::MAX;
        seqs[slot] = 0;
        return true;
    }
    if live {
        counters[slot] = counters[slot].saturating_add(1);
        seqs[slot] = seqs[slot].wrapping_add(1);
    }
    drop(seqs); drop(counters); drop(refs);
    let key = eventfd_key(slot);
    if !sched::try_wake_poll_key(key) { sched::request_poll_wake_key(key); }
    true
}

/// Drop the reference `eventfd_slot_ref` took, from task context. Frees the
/// slot if it was the last one, exactly as `release_vnode` does.
pub fn eventfd_slot_unref(slot: usize) {
    if slot >= MAX_EVENTFDS { return; }
    let mut refs = EVENTFD_REFS.lock();
    refs[slot] = refs[slot].saturating_sub(1);
    if refs[slot] == 0 {
        EVENTFD_COUNTERS.lock()[slot] = u64::MAX;
        EVENTFD_SEQ.lock()[slot] = 0;
    }
}

/// Add one to eventfd `slot` right now, from task context (the fence had
/// already retired when the fd was minted). Blocking locks, like a write(2).
pub fn eventfd_slot_signal(slot: usize) {
    if slot >= MAX_EVENTFDS { return; }
    {
        let mut counters = EVENTFD_COUNTERS.lock();
        if counters[slot] == u64::MAX { return; }
        counters[slot] = counters[slot].saturating_add(1);
    }
    {
        let mut seqs = EVENTFD_SEQ.lock();
        seqs[slot] = seqs[slot].wrapping_add(1);
    }
    sched::wake_poll_key(eventfd_key(slot));
}


// ── timerfd pool ──────────────────────────────────────────────────────────────

// See MAX_EVENTFDS: calloop arms a timerfd per timer source; 16 is tight for a
// compositor. 64 gives headroom now that slots are refcounted (freed at last close).
// 64→256 (m7z2): GLOBAL pool, same session-wide pressure as MAX_EVENTFDS — the
// `polling` backend (calloop 0.14) creates one timerfd PER Poll/epoll instance for
// its wait timeout, so #timerfds tracks #event-loops (~150 in a full session).
// TimerFdEntry is small (256 × ~36 B ≈ 9 KB).
// 256 -> grown on demand up to this ceiling (lane fdmax), like the eventfds.
const MAX_TIMERFDS: usize = 1 << 16;
const TIMERFD_GROW: usize = 64;

#[derive(Clone, Copy)]
struct TimerFdEntry {
    armed:        bool,
    /// Absolute `sched::monotonic_ns()` instant of the next expiration. The
    /// deadline lives on the monotonic clock whatever `clockid` says: a
    /// CLOCK_REALTIME absolute value is translated at settime.
    deadline_ns:  u64,
    interval_ns:  u64,   // 0 = one-shot
    expirations:  u64,   // accumulated unread expiration count
    /// CLOCK_REALTIME (0) or CLOCK_MONOTONIC (1): decides how a
    /// TFD_TIMER_ABSTIME `it_value` is read at settime.
    clockid:      u32,
    /// Armed with TFD_TIMER_ABSTIME on CLOCK_REALTIME: the deadline is a wall
    /// instant, so a clock step moves the monotonic `deadline_ns` with it
    /// (`timerfd_clock_was_set`), as a Linux CLOCK_REALTIME hrtimer does.
    abs_realtime: bool,
    /// TFD_TIMER_CANCEL_ON_SET was given with an absolute CLOCK_REALTIME
    /// setting (Linux `might_cancel`): a discontinuous clock step cancels.
    might_cancel: bool,
    /// The wall clock was stepped since the last settime/read: the fd polls
    /// readable and the next read() answers ECANCELED (Linux `moffs ==
    /// KTIME_MAX`).
    canceled:     bool,
    /// Number of cancellations, folded into the poll edge seq so an
    /// edge-triggered waiter sees each one.
    cancels:      u64,
}

impl TimerFdEntry {
    const fn free() -> Self {
        Self { armed: false, deadline_ns: 0, interval_ns: 0, expirations: 0, clockid: 1,
               abs_realtime: false, might_cancel: false, canceled: false, cancels: 0 }
    }
    const fn is_free(&self) -> bool { !self.armed && self.deadline_ns == 0 && self.expirations == 0 && !self.canceled }
}

static TIMERFD_POOL: Mutex<alloc::vec::Vec<TimerFdEntry>> = Mutex::new(alloc::vec::Vec::new());
/// Per-slot fd refcount for timerfds — same dup-aliasing hazard as EVENTFD_REFS.
static TIMERFD_REFS: Mutex<alloc::vec::Vec<u32>> = Mutex::new(alloc::vec::Vec::new());

/// Add TIMERFD_GROW free slots to both timerfd arrays (lock order REFS →
/// POOL, as in release). False at the ceiling or when the heap refuses.
fn timerfd_grow() -> bool {
    let mut refs = TIMERFD_REFS.lock();
    let mut pool = TIMERFD_POOL.lock();
    if pool.iter().any(|e| e.is_free()) { return true; }
    let n = pool.len();
    if n >= MAX_TIMERFDS { return false; }
    let m = (n + TIMERFD_GROW).min(MAX_TIMERFDS) - n;
    if refs.try_reserve_exact(m).is_err() || pool.try_reserve_exact(m).is_err() { return false; }
    refs.resize(n + m, 0);
    pool.resize(n + m, TimerFdEntry::free());
    true
}

/// Recompute a timerfd's pending-expiration count against the monotonic clock,
/// folding any missed periods into `expirations` and rearming the deadline —
/// the same catch-up logic `handle_read`'s `TimerFd` arm used to run inline.
/// Non-consuming: callers that read the count (rather than just probing
/// readiness) are responsible for resetting `expirations` to 0 afterward.
/// Shared with `handle_ioctl`'s `FIONREAD` and `handle_poll` so neither can
/// under-report an expiry that hasn't been read yet.
fn timerfd_poll_expirations(slot: usize) -> u64 {
    let now = sched::monotonic_ns();
    let mut pool = TIMERFD_POOL.lock();
    let e = &mut pool[slot];
    if e.armed && now >= e.deadline_ns {
        let elapsed = now - e.deadline_ns;
        let extra = if e.interval_ns > 0 { elapsed / e.interval_ns + 1 } else { 1 };
        e.expirations += extra;
        if e.interval_ns > 0 {
            e.deadline_ns += extra * e.interval_ns;
        } else {
            e.armed = false;
        }
    }
    e.expirations
}

// ── FD table ─────────────────────────────────────────────────────────────────
//
// One fd table per live process (tgid), created on first use and kept in a
// map keyed by tgid. Each table is a growable, chunked array: memory follows
// the number of descriptors a process actually holds (and the highest number
// it uses), not its limit. Until lane fdmax this was a static
// `[[FdEntry; 512]; MAX_PROCESSES]` — 10 MiB of .bss, a fixed 512-fd cap per
// process, a linear pid search on every fd operation and a 20 KiB copy of the
// parent's table onto the kernel stack at every fork.
//
// The limit a process can reach is its RLIMIT_NOFILE soft limit (`limit`,
// set by setrlimit/prlimit through `set_fd_limit`), never more than
// `FD_NUMBER_CEIL`.

/// Highest descriptor number + 1 any process may ever hold: Linux's default
/// `/proc/sys/fs/nr_open` (1 << 20). RLIMIT_NOFILE's hard limit cannot be
/// raised above it.
pub const NR_OPEN: usize = 1 << 20;
const _: () = assert!(NR_OPEN as u64 == sched::task::NR_OPEN);

/// Every descriptor number is below this. Equal to `NR_OPEN`: the table is
/// the process's one descriptor namespace (sockets and epoll fds are entries
/// in it), and the kernel's internal handles start above it.
pub const FD_NUMBER_CEIL: usize = NR_OPEN;

/// RLIMIT_NOFILE soft limit a process starts with (Linux: 1024, so that
/// select()'s FD_SETSIZE covers every fd an unaware program gets). The
/// scheduler's `RLIMIT_DEFAULTS` holds the same value.
pub const DEFAULT_NOFILE_CUR: usize = sched::task::NOFILE_DEFAULT_CUR as usize;

/// One serial line when a process's own fd table is full (EMFILE), once per
/// pid in a row. Without it a compositor that runs out of fds fails the
/// client's buffer import in silence and the client just never draws.
fn report_fd_table_full(pid: u32, limit: usize) {
    static LAST: atomic::AtomicU32 = atomic::AtomicU32::new(0);
    if LAST.swap(pid, atomic::Ordering::Relaxed) == pid { return; }
    dbg_str("\n[VFS] pid ");
    dbg_dec(pid as usize);
    dbg_str(": fd table FULL at ");
    dbg_dec(limit);
    dbg_str(" (EMFILE) -- this process's RLIMIT_NOFILE\n");
}
const O_CLOEXEC: u32   = 0x8_0000;
/// O_NONBLOCK (== EFD_NONBLOCK/TFD_NONBLOCK/SFD_NONBLOCK). Module-level so the
/// eventfd/timerfd/signalfd creators can record it on the fd (fd_nonblock reads
/// FdEntry.flags); a dropped O_NONBLOCK makes the read path yield-spin.
const O_NONBLOCK_FL: u32 = 0o4000;
/// The status flags fcntl(F_SETFL) changes (Linux `SETFL_MASK`), in the VFS's
/// x86_64 layout: O_APPEND, O_NONBLOCK, O_ASYNC, O_DIRECT, O_NOATIME.
const SETFL_MASK: u32 = 0o2000 | 0o4000 | 0o20000 | 0o40000 | 0o1000000;

#[derive(Clone, Copy)]
struct FdEntry {
    kind:   VnodeKind,
    flags:  u32,
    in_use: bool,
    /// The open file description this fd names (`ofd` module): new for an
    /// fd created from nothing, shared by dup/fork/SCM_RIGHTS copies, one
    /// reference per entry. 0 = none (description table full).
    ofd:    u32,
}

impl FdEntry {
    const fn empty() -> Self {
        Self { kind: VnodeKind::None, flags: 0, in_use: false, ofd: 0 }
    }
}

/// What `FdVec`'s `Index` answers for a number with no chunk behind it.
static EMPTY_FD: FdEntry = FdEntry::empty();

/// Entries per chunk: one `u64` of the in-use bitmap per chunk. 64 entries
/// of 40 bytes is 2.5 KiB, so a typical process (a few dozen fds) holds one
/// chunk, and even `NR_OPEN` descriptors stay far below the allocator's
/// largest single allocation per piece.
const FD_CHUNK: usize = 64;

type FdChunk = alloc::boxed::Box<[FdEntry; FD_CHUNK]>;

/// A sparse, growable `[FdEntry]` indexed by fd number.
///
/// Reading an absent number yields an empty entry. Entries are installed and
/// removed with `set`/`clear`, which keep the in-use bitmap (`bits`, one word
/// per chunk) in step; `IndexMut` is only for changing a field of an entry
/// that is already in use (flags, kind, ofd) and never flips `in_use`.
struct FdVec {
    chunks: alloc::vec::Vec<Option<FdChunk>>,
    bits:   alloc::vec::Vec<u64>,
    count:  usize,
    /// Bit n (n < 3): fd n, which has no entry, was closed. An fd 0-2 with no
    /// entry is otherwise the implicit console (the kernel's serial fast
    /// paths); once closed it is EBADF like any other free number until
    /// something is installed there again. Inherited by fork (it is part of
    /// the table), kept across exec.
    con_closed: u8,
}

impl Clone for FdVec {
    fn clone(&self) -> Self {
        // Every tmpfs entry the copy holds is one more reference to its slot.
        for (_, e) in self.iter_used() { tmp_fdref_inc(&e.kind); }
        Self { chunks: self.chunks.clone(), bits: self.bits.clone(), count: self.count,
               con_closed: self.con_closed }
    }
}

impl Drop for FdVec {
    fn drop(&mut self) {
        for (_, e) in self.iter_used() { tmp_fdref_dec(&e.kind); }
    }
}

impl FdVec {
    const fn new() -> Self {
        Self { chunks: alloc::vec::Vec::new(), bits: alloc::vec::Vec::new(), count: 0, con_closed: 0 }
    }

    /// fd 0-2 with no entry that has not been closed: the raw console.
    #[inline]
    fn implicit_console(&self, fd: usize) -> bool {
        fd <= 2 && self.con_closed & (1 << fd) == 0 && !self.used(fd)
    }

    /// fd 0-2 with no entry, closed since (EBADF, and free for reuse).
    #[inline]
    fn console_closed(&self, fd: usize) -> bool {
        fd <= 2 && self.con_closed & (1 << fd) != 0 && !self.used(fd)
    }

    #[inline]
    fn used(&self, fd: usize) -> bool { self.get(fd).is_some() }

    #[inline]
    fn get(&self, fd: usize) -> Option<&FdEntry> {
        let c = self.chunks.get(fd / FD_CHUNK)?.as_ref()?;
        let e = &c[fd % FD_CHUNK];
        if e.in_use { Some(e) } else { None }
    }

    fn chunk_mut(&mut self, fd: usize) -> &mut [FdEntry; FD_CHUNK] {
        let ci = fd / FD_CHUNK;
        if ci >= self.chunks.len() {
            self.chunks.resize_with(ci + 1, || None);
            self.bits.resize(ci + 1, 0);
        }
        self.chunks[ci].get_or_insert_with(|| alloc::boxed::Box::new([FdEntry::empty(); FD_CHUNK]))
    }

    /// Install `e` at `fd` (or, for an `!in_use` entry, clear it). Returns the
    /// entry that was there.
    fn set(&mut self, fd: usize, e: FdEntry) -> FdEntry {
        if !e.in_use { return self.clear(fd); }
        tmp_fdref_inc(&e.kind);
        if fd <= 2 { self.con_closed &= !(1u8 << fd); }
        let slot = &mut self.chunk_mut(fd)[fd % FD_CHUNK];
        let old = core::mem::replace(slot, e);
        let bit = 1u64 << (fd % FD_CHUNK);
        let w = &mut self.bits[fd / FD_CHUNK];
        if *w & bit == 0 { *w |= bit; self.count += 1; }
        else { tmp_fdref_dec(&old.kind); }
        old
    }

    /// Remove whatever is at `fd`, returning it (an empty entry if none).
    /// The chunk itself is freed once its last entry goes, so a burst of
    /// descriptors (or one dup2 to a high number) does not pin memory.
    fn clear(&mut self, fd: usize) -> FdEntry {
        let ci = fd / FD_CHUNK;
        let Some(Some(c)) = self.chunks.get_mut(ci) else { return FdEntry::empty() };
        let old = core::mem::replace(&mut c[fd % FD_CHUNK], FdEntry::empty());
        let bit = 1u64 << (fd % FD_CHUNK);
        if self.bits[ci] & bit != 0 {
            self.bits[ci] &= !bit;
            self.count -= 1;
            tmp_fdref_dec(&old.kind);
            // A closed fd 0-2 stays closed: it does not fall back to being
            // the implicit console.
            if fd <= 2 { self.con_closed |= 1u8 << fd; }
        }
        if self.bits[ci] == 0 {
            self.chunks[ci] = None;
            while matches!(self.chunks.last(), Some(None)) { self.chunks.pop(); self.bits.pop(); }
        }
        old
    }

    /// The lowest number `>= from` with no entry (it may be past the end).
    fn lowest_free(&self, from: usize) -> usize {
        let mut ci = from / FD_CHUNK;
        let mut mask = !0u64 << (from % FD_CHUNK);
        while ci < self.bits.len() {
            let free = !self.bits[ci] & mask;
            if free != 0 { return ci * FD_CHUNK + free.trailing_zeros() as usize; }
            ci += 1;
            mask = !0;
        }
        // Past the last chunk: everything is free.
        from.max(self.bits.len() * FD_CHUNK)
    }

    /// Number of descriptors in the table.
    fn len(&self) -> usize { self.count }

    /// Every in-use entry, in ascending fd order.
    fn iter_used(&self) -> impl Iterator<Item = (usize, &FdEntry)> + '_ {
        self.chunks.iter().zip(self.bits.iter()).enumerate().flat_map(|(ci, (c, &w))| {
            let w = if c.is_some() { w } else { 0 };
            BitIter(w).map(move |b| (ci * FD_CHUNK + b, &c.as_ref().unwrap()[b]))
        })
    }

    /// Every in-use fd number, in ascending order (a snapshot: callers that
    /// mutate the table while walking it use this).
    fn used_fds(&self) -> alloc::vec::Vec<usize> {
        let mut v = alloc::vec::Vec::with_capacity(self.count);
        for (ci, &w) in self.bits.iter().enumerate() {
            for b in BitIter(w) { v.push(ci * FD_CHUNK + b); }
        }
        v
    }

    /// Remove every entry, returning them in ascending fd order.
    fn drain(&mut self) -> alloc::vec::Vec<(usize, FdEntry)> {
        let v: alloc::vec::Vec<(usize, FdEntry)> = self.iter_used().map(|(fd, e)| (fd, *e)).collect();
        *self = FdVec::new(); // Drop of the old table drops the tmpfs references
        v
    }
}

/// Set bits of a word, lowest first.
struct BitIter(u64);
impl Iterator for BitIter {
    type Item = usize;
    #[inline]
    fn next(&mut self) -> Option<usize> {
        if self.0 == 0 { return None; }
        let b = self.0.trailing_zeros() as usize;
        self.0 &= self.0 - 1;
        Some(b)
    }
}

impl core::ops::Index<usize> for FdVec {
    type Output = FdEntry;
    #[inline]
    fn index(&self, fd: usize) -> &FdEntry {
        match self.chunks.get(fd / FD_CHUNK) {
            Some(Some(c)) => &c[fd % FD_CHUNK],
            _ => &EMPTY_FD,
        }
    }
}

impl core::ops::IndexMut<usize> for FdVec {
    /// Field updates of an entry that is in use. Never use this to install or
    /// remove an entry (`set`/`clear` keep the bitmap right).
    #[inline]
    fn index_mut(&mut self, fd: usize) -> &mut FdEntry {
        &mut self.chunk_mut(fd)[fd % FD_CHUNK]
    }
}

struct ProcFdTable {
    /// The table's id: the tgid, or a private id (`sched::fdkey`).
    pid:   u32,
    /// The owning process (tgid); POSIX locks are held under it.
    owner: u32,
    fds:   FdVec,
    /// RLIMIT_NOFILE soft limit: no descriptor at or above it can be created.
    /// Mirrors the task's rlimit (`set_fd_limit`), so the fd-creating paths
    /// need not call into the scheduler with FD_TABLES held.
    limit: usize,
    /// The process exited: `handle_close_all` drained this table and took it
    /// out of the map. A caller that looked it up before that and locked it
    /// after must not install anything here (`TblSlot::holds`).
    dead:  bool,
}

impl ProcFdTable {
    fn new(pid: u32) -> Self {
        Self { pid, owner: pid, fds: FdVec::new(), limit: DEFAULT_NOFILE_CUR, dead: false }
    }

    /// The exclusive upper bound for new descriptor numbers.
    #[inline]
    fn cap(&self) -> usize { self.limit.min(FD_NUMBER_CEIL) }

    fn alloc_fd(&mut self) -> Option<usize> {
        self.alloc_fd_from(0)
    }

    /// The lowest free number `>= min` below the limit (F_DUPFD), or None:
    /// EMFILE.
    ///
    /// An fd 0-2 with no entry is the implicit console (the kernel's serial
    /// fast paths), so it is free only once closed (`close_console`): after
    /// `close(0)` the next open() is fd 0, as POSIX requires, and a table
    /// created fresh never hands its first pipe() a number the console holds.
    fn alloc_fd_from(&mut self, min: usize) -> Option<usize> {
        for c in min..3 {
            if self.fds.console_closed(c) && c < self.cap() { return Some(c); }
        }
        let fd = self.fds.lowest_free(min.max(3));
        if fd < self.cap() { return Some(fd); }
        report_fd_table_full(self.pid, self.cap());
        None
    }
}

// ── Locking the fd tables ────────────────────────────────────────────────────
//
// Each process's table has its own lock (`FdTableLock`, lockwatch class
// FD_TABLE); `FD_TABLES` is only the tgid → table map, held just long enough
// to look a table up, insert one or remove one. Until lane fdgaps one lock
// guarded every table, so every fd syscall of every process serialised on it.
//
// Lock order:
// - A table lock → `FD_TABLES` (the map) → the leaf locks
//   (PIPE_RINGS, TMP_*, ofd, ...) exactly as they nested under the old global
//   lock. The map is a leaf with respect to tables: no table lock is ever
//   taken while it is held (the IRQ-context census only try-locks).
// - At most one table lock at a time. Fork does not need two: the child's
//   table is built from a copy of the parent's and inserted whole.
//
// "Is this object still open?" (last close of a mounted file, a tmpfs
// inode's last fd, a block device's unload check) is answered from per-object
// reference counts kept by `FdVec` itself (`TMP_FDREFS`, `MOUNT_FDREFS`,
// `BLK_FDREFS`: one per fd-table entry plus one per queued SCM_RIGHTS copy),
// not by walking every table. The counts are bumped before a copy becomes
// visible and dropped after the source goes, so they never read 0 for a live
// object; there is no global copy gate.

type FdTableLock = sched::lockwatch::TrackedMutex<ProcFdTable, { sched::lockwatch::L_FD_TABLE }>;
type FdTableRef = alloc::sync::Arc<FdTableLock>;
type FdTableGuard = sched::lockwatch::TrackedGuard<'static, ProcFdTable>;

/// Every process's fd table, keyed by tgid.
struct FdTables {
    map: alloc::collections::BTreeMap<u32, FdTableRef>,
}

impl FdTables {
    const fn new() -> Self { Self { map: alloc::collections::BTreeMap::new() } }
}

static FD_TABLES: sched::lockwatch::TrackedMutex<FdTables, { sched::lockwatch::L_FD_TABLES }> =
    sched::lockwatch::TrackedMutex::new_typed(FdTables::new());

/// The table each CPU looked up last. Nearly every fd syscall resolves its
/// descriptor and then operates on it, two lookups of the same table; with
/// one map lock for every process those lookups were where all the
/// contention moved once the tables had their own locks (the 4-process
/// `fdmaxtest bench` measured the map lock contended on 23 % of 3.9 M
/// acquisitions). A hit costs this CPU's own slot and the table's refcount.
/// Entries are only ever hints: a stale one names a table its process has
/// left (`dead`), which `find_tbl` sees once it holds the lock and then asks
/// the map. Pids are never reused, so a hit is the right process's table.
/// try_lock only: a slot busy on this CPU (it cannot be, syscalls run with
/// IRQs masked, but a migrated caller may share it) just falls back to the map.
static TBL_CACHE: [spin::Mutex<Option<(u32, FdTableRef)>>; sched::MAX_CPUS] =
    [const { spin::Mutex::new(None) }; sched::MAX_CPUS];

#[inline]
fn tbl_cache_slot() -> &'static spin::Mutex<Option<(u32, FdTableRef)>> {
    &TBL_CACHE[unsafe { sched::cpu_id() }.min(sched::MAX_CPUS - 1)]
}

/// The fd-table id `pid`'s descriptors live in. `pid` is a tgid (callers
/// canonicalise) or a private id. The calling thread's own process means the
/// calling thread's table; another process means its leader's.
#[inline]
pub fn tbl_key(pid: u32) -> u32 {
    if pid & sched::fdkey::FD_KEY_PRIVATE != 0 || !sched::fdkey::any() { return pid; }
    let tid = if pid == sched::current_tgid() { sched::current_pid() } else { pid };
    sched::fdkey::override_of(tid).unwrap_or(pid)
}

/// The process-wide (leader's) table id of tgid `pid`, ignoring the caller.
#[inline]
fn leader_key(pid: u32) -> u32 {
    if !sched::fdkey::any() { return pid; }
    sched::fdkey::override_of(pid).unwrap_or(pid)
}

/// `pid`'s table, if it has one, through this CPU's cache.
fn table_ref(pid: u32) -> Option<FdTableRef> {
    if let Some(c) = tbl_cache_slot().try_lock() {
        if let Some((p, t)) = c.as_ref() { if *p == pid { return Some(t.clone()); } }
    }
    table_ref_uncached(pid)
}

/// `pid`'s table from the map, remembered in this CPU's cache.
fn table_ref_uncached(pid: u32) -> Option<FdTableRef> {
    let t = FD_TABLES.lock().map.get(&pid).cloned()?;
    if let Some(mut c) = tbl_cache_slot().try_lock() {
        // The table this replaces may be the last reference to a dead one;
        // dropping it frees memory, under no lock but this leaf.
        *c = Some((pid, t.clone()));
    }
    Some(t)
}

/// `pid`'s table, created empty if it has none.
fn table_ref_or_create(pid: u32) -> FdTableRef {
    let mut m = FD_TABLES.lock();
    if let Some(t) = m.map.get(&pid) { return t.clone(); }
    let t: FdTableRef = alloc::sync::Arc::new(FdTableLock::new_typed(ProcFdTable::new(pid)));
    m.map.insert(pid, t.clone());
    t
}

/// Every table, for a scan: references taken under the map lock, each table
/// locked by the caller in turn.
fn all_tables() -> alloc::vec::Vec<FdTableRef> {
    FD_TABLES.lock().map.values().cloned().collect()
}

/// A table lock held on behalf of a caller: what `find_tbl` / `get_or_create`
/// lock a table into, and what dropping (or reusing for another pid) releases.
/// Holds the table's reference so the lock outlives a concurrent exit's
/// removal of the table from the map.
struct TblSlot {
    // Field order is drop order: the guard goes before the table it locks.
    guard: Option<FdTableGuard>,
    table: Option<FdTableRef>,
}

impl TblSlot {
    const fn new() -> Self { Self { guard: None, table: None } }

    fn release(&mut self) {
        self.guard = None;
        self.table = None;
    }

    /// Is the table locked here `pid`'s, and still live?
    fn holds(&self, pid: u32) -> bool {
        self.guard.as_ref().map_or(false, |g| g.pid == pid && !g.dead)
    }

    /// Lock `t` into this slot (after releasing whatever it held).
    fn lock(&mut self, t: FdTableRef) {
        self.release();
        let g = t.lock();
        // SAFETY: the guard borrows the mutex inside the Arc this slot keeps
        // in `table`, which is dropped strictly after the guard (field order,
        // and `release`). The Arc's heap allocation never moves.
        let g: FdTableGuard = unsafe { core::mem::transmute(g) };
        self.table = Some(t);
        self.guard = Some(g);
    }

    fn get(&mut self) -> &mut ProcFdTable {
        &mut **self.guard.as_mut().expect("TblSlot::get on an empty slot")
    }
}

/// Lock `pid`'s table into `slot` (reusing it if it already holds that
/// table). None: the process has no table (or it exited).
fn find_tbl<'a>(pid: u32, slot: &'a mut TblSlot) -> Option<&'a mut ProcFdTable> {
    let pid = tbl_key(pid);
    if !slot.holds(pid) {
        let t = table_ref(pid)?;
        slot.lock(t);
        if !slot.holds(pid) {
            // A dead table: from a stale cache entry, or the process exited
            // after the lookup. Ask the map once more.
            slot.release();
            let t = table_ref_uncached(pid)?;
            slot.lock(t);
            if !slot.holds(pid) { slot.release(); return None; }
        }
    }
    Some(slot.get())
}

/// `find_tbl`, creating an empty table for a process that has none. A table
/// whose process exited while this waited for its lock is dead; the retry
/// then finds no table in the map and creates one, as the old global lock
/// would have.
fn get_or_create<'a>(pid: u32, slot: &'a mut TblSlot) -> Option<&'a mut ProcFdTable> {
    let pid = tbl_key(pid);
    if find_tbl(pid, slot).is_some() { return Some(slot.get()); }
    for _ in 0..4 {
        if slot.holds(pid) { break; }
        let t = table_ref_or_create(pid);
        slot.lock(t);
        if slot.holds(pid) { break; }
        slot.release();
    }
    if slot.holds(pid) { Some(slot.get()) } else { None }
}

/// [`with_tbl`] for an exact table id, with no thread translation.
fn with_tbl_raw<R>(id: u32, f: impl FnOnce(&mut ProcFdTable) -> R) -> Option<R> {
    let mut slot = TblSlot::new();
    if !slot.holds(id) {
        let t = table_ref(id)?;
        slot.lock(t);
        if !slot.holds(id) { return None; }
    }
    Some(f(slot.get()))
}

/// Run `f` on `pid`'s table under its lock. None: no table.
fn with_tbl<R>(pid: u32, f: impl FnOnce(&mut ProcFdTable) -> R) -> Option<R> {
    let mut slot = TblSlot::new();
    find_tbl(pid, &mut slot).map(f)
}

/// Fork never fails for lack of an fd table any more (tables are allocated
/// per process on demand); kept for the kernel's fork paths.
pub fn fd_table_slot_free() -> bool { true }

/// Set `pid`'s RLIMIT_NOFILE soft limit as the fd tables see it (setrlimit,
/// prlimit). Existing descriptors at or above it stay open, as on Linux.
pub fn set_fd_limit(pid: u32, limit: usize) {
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    if let Some(t) = get_or_create(pid, &mut tbls) { t.limit = limit.min(NR_OPEN).max(3); }
    drop(tbls);
    // RLIMIT_NOFILE is per process: every table of the process (a thread's
    // private one included) follows it.
    let lim = limit.min(NR_OPEN).max(3);
    for t in all_tables() {
        let mut g = t.lock();
        if g.owner == pid { g.limit = lim; }
    }
}

/// `pid`'s fd limit as the fd tables enforce it.
pub fn fd_limit(pid: u32) -> usize {
    let pid = sched::tgid_of(pid);
    with_tbl(pid, |t| t.limit).unwrap_or(DEFAULT_NOFILE_CUR)
}

/// Number of descriptors `pid`'s process holds.
pub fn fd_count(pid: u32) -> usize {
    let pid = sched::tgid_of(pid);
    with_tbl(pid, |t| t.fds.len()).unwrap_or(0)
}

/// Descriptors held across every process (/proc/sys/fs/file-nr).
pub fn fd_count_all() -> usize {
    all_tables().iter().map(|t| t.lock().fds.len()).sum()
}
// ── Dynamic-device open identities ───────────────────────────────────────────
//
// A device server needs to distinguish "a second fd on the same open" (dup,
// fork, SCM_RIGHTS) from "a second, independent open of the device". The fd
// tables cannot answer that on their own, and a refcount recovered by scanning
// FD_TABLES is wrong besides: `export_fd` lifts an entry OUT of every table
// into an in-flight `TransferFd`, so a scan sees zero references during the
// window an SCM_RIGHTS descriptor is queued.
//
// So each open of a dynamic device takes a slot here, and the slot id travels
// inside the `VnodeKind` — copied for free by every path that copies an fd
// entry, refcounted by the same `pipe_ref_inc`/`pipe_ref_dec` pair that already
// refcounts pipe endpoints and eventfd/timerfd pool slots.
// Sized for a full desktop session, not just the GPU: this pool is shared by
// every dynamic device, so a COSMIC session's evdev/console/pipewire opens draw
// from it too, and exhaustion surfaces as ENFILE on open() rather than anything
// diagnosable. 4 KiB of BSS is the cheaper side of that trade.
// 1024 -> grown on demand up to this ceiling (lane fdmax).
const MAX_DEVICE_OPENS: usize = 1 << 16;

/// Parked value for a slot whose VFS_CLOSE is still in flight to the server.
/// The allocator only takes slots reading 0, so this keeps a concurrent open
/// from reusing (and thereby aliasing) an identity the server is still tearing
/// down.
const DEVICE_OPEN_CLOSING: u32 = u32::MAX;

static DEVICE_OPEN_REFS: Mutex<alloc::vec::Vec<u32>> = Mutex::new(alloc::vec::Vec::new());

/// Claim a fresh open identity, returning `slot + 1` so that 0 is an
/// unambiguous "this vnode carries no open id". 0 is also returned when the
/// table is full (caller answers ENFILE).
fn device_open_alloc() -> u32 {
    let mut refs = DEVICE_OPEN_REFS.lock();
    match refs.iter().position(|&r| r == 0) {
        Some(i) => { refs[i] = 1; (i + 1) as u32 }
        None if refs.len() < MAX_DEVICE_OPENS && refs.try_reserve(1).is_ok() => {
            refs.push(1);
            refs.len() as u32
        }
        None    => 0,
    }
}

/// Index of a live open id, or None for the untracked id 0 / an out-of-range id.
fn device_open_idx(open_id: u32) -> Option<usize> {
    if open_id == 0 { return None; }
    let i = (open_id - 1) as usize;
    if i < DEVICE_OPEN_REFS.lock().len() { Some(i) } else { None }
}

/// One more fd now refers to this open (dup/dup2/dup3/F_DUPFD, fork
/// inheritance, or an SCM_RIGHTS descriptor in flight).
fn device_open_inc(open_id: u32) {
    if let Some(i) = device_open_idx(open_id) {
        let mut refs = DEVICE_OPEN_REFS.lock();
        if refs[i] != DEVICE_OPEN_CLOSING { refs[i] = refs[i].saturating_add(1); }
    }
}

/// Drop one fd's reference to a dynamic-device open. The server is told
/// (VFS_CLOSE) only when the LAST fd goes away — previously every close of a
/// duplicate sent one, and the dup2-overwrite path sent none at all.
///
/// Caller must NOT hold FD_TABLES: this makes an IPC round-trip to the device
/// server, which re-enters the VFS.
fn device_close(port: u32, dev_id: u32, open_id: u32) {
    let idx = device_open_idx(open_id);
    if let Some(i) = idx {
        let mut refs = DEVICE_OPEN_REFS.lock();
        if refs[i] == DEVICE_OPEN_CLOSING { return; }
        refs[i] = refs[i].saturating_sub(1);
        if refs[i] != 0 { return; }
        refs[i] = DEVICE_OPEN_CLOSING;
    }
    FDCLOSE[7].fetch_add(1, atomic::Ordering::Relaxed);
    let mut close_msg = Message::empty();
    close_msg.tag = VFS_CLOSE;
    close_msg.data[0..8].copy_from_slice(&(dev_id as u64).to_le_bytes());
    close_msg.data[8..16].copy_from_slice(&(open_id as u64).to_le_bytes());
    let _ = call_port(port, close_msg);
    // Free the slot only after the close has been delivered — the server drops
    // state keyed by this open_id inside that call.
    if let Some(i) = idx { DEVICE_OPEN_REFS.lock()[i] = 0; }
}

static INITRD_BASE: atomic::AtomicUsize = atomic::AtomicUsize::new(0);
static INITRD_SIZE: atomic::AtomicUsize = atomic::AtomicUsize::new(0);

static FB_BASE:   atomic::AtomicU64 = atomic::AtomicU64::new(0);
static FB_WIDTH:  atomic::AtomicU32 = atomic::AtomicU32::new(0);
static FB_HEIGHT: atomic::AtomicU32 = atomic::AtomicU32::new(0);
static FB_PITCH:  atomic::AtomicU32 = atomic::AtomicU32::new(0);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
}

pub fn set_initrd(base: usize, size: usize) {
    INITRD_BASE.store(base, atomic::Ordering::SeqCst);
    INITRD_SIZE.store(size, atomic::Ordering::SeqCst);
}

pub fn set_framebuffer(base: u64, width: u32, height: u32, pitch: u32) {
    // Ensure pitch is in bytes
    let p_bytes = if pitch < width * 4 { width * 4 } else { pitch };
    
    FB_BASE.store(base, atomic::Ordering::SeqCst);
    FB_WIDTH.store(width, atomic::Ordering::SeqCst);
    FB_HEIGHT.store(height, atomic::Ordering::SeqCst);
    FB_PITCH.store(p_bytes, atomic::Ordering::SeqCst);
}

/// Get current framebuffer information for DRM
#[no_mangle]
pub extern "C" fn vfs_get_framebuffer_info(info: &mut FramebufferInfo) {
    let width = FB_WIDTH.load(atomic::Ordering::SeqCst);
    let pitch = FB_PITCH.load(atomic::Ordering::SeqCst);
    
    info.width = width;
    info.height = FB_HEIGHT.load(atomic::Ordering::SeqCst);
    // Ensure pitch is in bytes and at least width * 4
    info.pitch = if pitch < width * 4 { width * 4 } else { pitch };
}

/// Re-point /dev/fb0 at a new console surface.
///
/// KMS moves the console off the bootloader's framebuffer when that memory is
/// not DMA-able — with virtio-vga, OVMF's GOP lives in the VGA VRAM BAR and
/// virtio-gpu cannot back a resource with it, so `kms::detect_and_configure`
/// allocates a RAM surface, copies the boot console into it and redirects
/// BOOT_FB/KERNEL_FB there. It did not redirect this, so on x86_64 /dev/fb0
/// went on reporting the VRAM address: reads returned a frozen copy of the
/// screen as it looked at handoff, while both the console and every DRM present
/// wrote somewhere else entirely.
#[no_mangle]
pub extern "C" fn vfs_set_framebuffer(base: u64, width: u32, height: u32, pitch: u32) {
    set_framebuffer(base, width, height, pitch);
}

/// Get framebuffer base address for DRM mmap
#[no_mangle]
pub extern "C" fn vfs_get_framebuffer_base() -> u64 {
    FB_BASE.load(atomic::Ordering::SeqCst)
}

/// Write data to framebuffer - called by DRM driver
#[no_mangle]
pub extern "C" fn vfs_write_framebuffer(buffer_ptr: *const u8, count: usize) -> i64 {
    let base = FB_BASE.load(atomic::Ordering::SeqCst);
    if base == 0 {
        return -19; // ENODEV - no framebuffer available
    }

    if count == 0 || buffer_ptr.is_null() {
        return -14; // EFAULT - invalid parameters
    }

    // For DRM hardware scaling, we accept writes directly to framebuffer
    // The DRM hardware will handle scaling from source to display resolution
    let height = FB_HEIGHT.load(atomic::Ordering::SeqCst) as usize;
    let pitch = FB_PITCH.load(atomic::Ordering::SeqCst) as usize;

    if height == 0 || pitch == 0 {
        return -19; // ENODEV - invalid framebuffer configuration
    }

    let display_fb_size = height * pitch;
    let n = count.min(display_fb_size);

    // Map the physical framebuffer to a kernel virtual address
    let fb_virt = mm::phys_to_virt(base as usize) as *mut u8;

    unsafe {
        core::ptr::copy_nonoverlapping(buffer_ptr, fb_virt, n);
    }

    n as i64
}

use core::sync::atomic;

// ── Static RamFS ──────────────────────────────────────────────────────────────

/// Inode number of the console character device.
///
/// One identity shared by every route that can name the console: `fstat` on an
/// unredirected fd 0/1/2, `fstat` on a console proxy (a dup'd stdio fd or an
/// fd opened on /dev/tty), and `stat("/dev/console")` / `stat("/dev/tty")`.
///
/// They have to agree because `ttyname()` — which is how `tty(1)` and every
/// other "what terminal am I on" query is actually implemented — fstats the
/// fd, readlinks /proc/self/fd/N, then stats the path it read back and demands
/// that (st_dev, st_ino) match. Any disagreement reads as "not a tty".
/// Distinct from the pipe (0x1000_0000) and tmpfs (0x2000_0000) ranges.
pub const CONSOLE_INO: u64 = 0x3000_0000;

/// Inode number of one end of pty `pair`, shared by `fstat` on the fd and
/// `stat` on `/dev/ptmx` / `/dev/pts/N` — `ttyname()` compares the two and
/// treats any disagreement as "not a tty".
fn pty_ino(pair: u16, is_master: bool) -> u64 {
    0x4000_0000 + (pair as u64) * 2 + if is_master { 1 } else { 0 }
}

struct RamEntry { path: &'static [u8], data: &'static [u8] }

static RAMFS: &[RamEntry] = &[
    // /dev virtual devices (zero-length placeholder; open is intercepted above)
    RamEntry { path: b"/dev/null",    data: b"" },
    RamEntry { path: b"/dev/zero",    data: b"" },
    RamEntry { path: b"/dev/full",    data: b"" },
    RamEntry { path: b"/dev/urandom", data: b"" },
    RamEntry { path: b"/dev/random",  data: b"" },
    RamEntry { path: b"/dev/stdin",   data: b"" },
    RamEntry { path: b"/dev/stdout",  data: b"" },
    RamEntry { path: b"/dev/stderr",  data: b"" },
    RamEntry { path: b"/dev/tty",     data: b"" },
    // The controlling terminal under its other conventional name. `open` has
    // always accepted it (it maps to the same console proxy as /dev/tty), but
    // without a table entry it did not exist for `access`, `ls /dev` or the
    // RamFS half of `stat` — and it is the path `ttyname()` now reports.
    RamEntry { path: b"/dev/console", data: b"" },
    // The virtual consoles. `open` is intercepted below (each maps to a
    // `DevVt` vnode); the entries are what make `ls /dev`, `access` and
    // `stat` agree they exist — logind/libseat shims stat /dev/tty0 before
    // they will admit the system has VTs at all, and a missing node reads as
    // "seatd is the only option".
    RamEntry { path: b"/dev/tty0",    data: b"" },
    RamEntry { path: b"/dev/tty1",    data: b"" },
    RamEntry { path: b"/dev/tty2",    data: b"" },
    RamEntry { path: b"/dev/tty3",    data: b"" },
    RamEntry { path: b"/dev/tty4",    data: b"" },
    RamEntry { path: b"/dev/tty5",    data: b"" },
    RamEntry { path: b"/dev/tty6",    data: b"" },
    RamEntry { path: b"/dev/fb0",     data: b"" },
    // The pty multiplexor. `open` is intercepted (it allocates a pair); the
    // entry is what makes `access("/dev/ptmx", W_OK)`, `stat` and `ls /dev`
    // agree that it exists — openpty(3) and posix_openpt(3) both stat it
    // first, and a missing node is how a terminal emulator concludes the
    // system has no ptys at all.
    RamEntry { path: b"/dev/ptmx",    data: b"" },
    // /etc
    RamEntry { path: b"/etc/motd",
               data: b"Welcome to Leandros!\nType 'help' for available commands.\n" },
    // /etc/passwd, /etc/group and /etc/shadow deliberately have no entries
    // here: the real files live on the F2FS root (seeded by
    // mkfs-f2fs-populated.py) and a static copy would shadow them on every
    // lookup — login(1) would authenticate against the disk shadow file but
    // read uid/shell from a stale built-in.
    RamEntry { path: b"/etc/hostname", data: b"leandros\n" },
    RamEntry { path: b"/etc/hosts",
               data: b"127.0.0.1\tlocalhost\n::1\t\tlocalhost\n127.0.0.1\tleandros\n" },
    // Opening /etc/resolv.conf reads the DHCP lease's servers (see
    // gen_proc_system_content); this entry is what stat and directory
    // listings see, and its contents match the pre-lease fallback.
    RamEntry { path: b"/etc/resolv.conf",
               data: b"nameserver 8.8.8.8\nnameserver 8.8.4.4\n" },
    RamEntry { path: b"/etc/services",
               data: b"http\t80/tcp\nhttps\t443/tcp\nssh\t22/tcp\nftp\t21/tcp\n" },
    RamEntry { path: b"/etc/protocols",
               data: b"ip\t0\tIP\ntcp\t6\tTCP\nudp\t17\tUDP\nicmp\t1\tICMP\n" },
    RamEntry { path: b"/etc/nsswitch.conf",
               data: b"hosts: files dns\npasswd: files\ngroup: files\n" },
    RamEntry { path: b"/etc/os-release",
               data: b"NAME=\"Leandros\"\nVERSION=\"1.0\"\nID=leandros\nPRETTY_NAME=\"Leandros 1.0\"\n" },
    RamEntry { path: b"/proc/version",
               data: b"Linux version 6.0.0-leandros (Leandros Project) (gcc 13.0)\n" },
    // /proc/cpuinfo is generated dynamically (see gen_proc_system_content) —
    // it needs one "processor" block per online CPU, which this static
    // table can't express. This entry is unreachable (the dynamic path is
    // checked first) but is kept so the RAMFS table still lists the path
    // for any code that enumerates it statically.
    RamEntry { path: b"/proc/cpuinfo",
               data: b"processor\t: 0\nmodel name\t: Leandros Virtual CPU\ncpu MHz\t\t: 1000.000\n\
                       cache size\t: 4096 KB\nflags\t\t: fpu vme de pse tsc msr pae mce\n" },
    RamEntry { path: b"/proc/filesystems",
               data: b"nodev\ttmpfs\nnodev\tramfs\nnodev\tprocfs\nnodev\tcgroup\nnodev\tcgroup2\n\text2\n" },
    // /proc/mounts is generated from list_mounts() by gen_proc_system_content
    // (see the "/proc/mounts" arm there) — a static entry here would shadow
    // the generated one and go stale the moment a real filesystem is
    // mounted, which is exactly what broke `df`.
    RamEntry { path: b"/proc/net/dev",
               data: b"Inter-|   Receive                                       |  Transmit\n\
                       face |bytes packets errs drop fifo frame compressed multicast\
                       |bytes packets errs drop fifo colls carrier compressed\n\
                   lo:      0       0    0    0    0     0          0         0       0       0    0    0    0     0       0          0\n" },
    // The live /proc/net files come from the net server (`proc_net_open`);
    // these entries are what stat, access and `ls /proc/net` see (size 0,
    // as procfs reports).
    RamEntry { path: b"/proc/net/route", data: b"" },
    RamEntry { path: b"/proc/net/nic",   data: b"" },
    RamEntry { path: b"/proc/net/listen", data: b"" },
    RamEntry { path: b"/proc/net/wifi",  data: b"" },
    RamEntry { path: b"/proc/net/tcp",   data: b"" },
    RamEntry { path: b"/proc/net/udp",   data: b"" },
    RamEntry { path: b"/proc/net/unix",  data: b"" },
    RamEntry { path: b"/proc/net/tcp6",  data: b"" },
    RamEntry { path: b"/proc/net/if_inet6", data: b"" },
    RamEntry { path: b"/proc/net/ipv6_route", data: b"" },
    RamEntry { path: b"/proc/net/udp6",  data: b"" },
    RamEntry { path: b"/proc/net/raw",   data: b"" },
    RamEntry { path: b"/proc/net/raw6",  data: b"" },
    RamEntry { path: b"/proc/net/if_inet6",  data: b"" },
    RamEntry { path: b"/proc/net/fib_trie",  data: b"Main:\n  +-- 0.0.0.0/0\n" },
    RamEntry { path: b"/proc/sys/kernel/hostname",   data: b"leandros\n" },
    RamEntry { path: b"/proc/sys/kernel/domainname", data: b"(none)\n" },
    RamEntry { path: b"/proc/sys/kernel/ostype",     data: b"Linux\n" },
    RamEntry { path: b"/proc/sys/kernel/osrelease",  data: b"6.0.0-leandros\n" },
    // pids are never reused here (a monotonic u32), so pid_max reports
    // Linux's ceiling; threads-max is the task table (`runqueue::MAX_TASKS`,
    // asserted below).
    RamEntry { path: b"/proc/sys/kernel/pid_max",    data: b"4194304\n" },
    RamEntry { path: b"/proc/sys/fs/mqueue/msg_max", data: b"10\n" },
    RamEntry { path: b"/proc/sys/fs/mqueue/msgsize_max", data: b"8192\n" },
    RamEntry { path: b"/proc/sys/fs/mqueue/queues_max", data: b"256\n" },
    RamEntry { path: b"/proc/sys/fs/mqueue/msg_default", data: b"10\n" },
    RamEntry { path: b"/proc/sys/fs/mqueue/msgsize_default", data: b"8192\n" },
    RamEntry { path: b"/proc/sys/kernel/threads-max", data: b"4096\n" },
    RamEntry { path: b"/proc/sys/vm/overcommit_memory", data: b"0\n" },
    // Id the kernel shows for an unmapped uid/gid (ns::OVERFLOW_ID), and the
    // user-namespace limit (ns::NS_MAX - 1 usable). Read-only.
    RamEntry { path: b"/proc/sys/kernel/overflowuid", data: b"65534\n" },
    RamEntry { path: b"/proc/sys/kernel/overflowgid", data: b"65534\n" },
    // Highest capability number the caps implementation knows (CAP_LAST,
    // asserted below): podman and crun read it to size capability sets.
    RamEntry { path: b"/proc/sys/kernel/cap_last_cap", data: b"40\n" },
    // Supplementary-group limit (sched::task::NGROUPS_MAX, asserted below).
    RamEntry { path: b"/proc/sys/kernel/ngroups_max", data: b"32\n" },
    RamEntry { path: b"/proc/sys/net/core/somaxconn", data: b"4096\n" },
    RamEntry { path: b"/proc/sys/net/core/rmem_max", data: b"212992\n" },
    RamEntry { path: b"/proc/sys/net/core/wmem_max", data: b"212992\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/ip_forward", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/ip_unprivileged_port_start", data: b"1024\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/ip_local_port_range", data: b"32768\t60999\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/ping_group_range", data: b"1\t0\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/ip_nonlocal_bind", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/ip_default_ttl", data: b"64\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/tcp_syncookies", data: b"1\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/conf/all/forwarding", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/conf/all/route_localnet", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/conf/all/rp_filter", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/conf/all/arp_notify", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/conf/default/forwarding", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/conf/default/route_localnet", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/conf/default/rp_filter", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/conf/default/arp_notify", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/conf/lo/forwarding", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/conf/lo/route_localnet", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/conf/lo/rp_filter", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/conf/lo/arp_notify", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/all/disable_ipv6", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/all/forwarding", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/all/accept_ra", data: b"1\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/all/accept_dad", data: b"1\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/default/disable_ipv6", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/default/forwarding", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/default/accept_ra", data: b"1\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/default/accept_dad", data: b"1\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/lo/disable_ipv6", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/lo/forwarding", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/lo/accept_ra", data: b"1\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/lo/accept_dad", data: b"1\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/all/autoconf", data: b"1\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/default/autoconf", data: b"1\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/lo/autoconf", data: b"1\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/all/dad_transmits", data: b"1\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/default/dad_transmits", data: b"1\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/conf/lo/dad_transmits", data: b"1\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/bindv6only", data: b"0\n" },
    RamEntry { path: b"/proc/sys/net/ipv4/route/mtu_expires", data: b"600\n" },
    RamEntry { path: b"/proc/sys/net/ipv6/route/mtu_expires", data: b"600\n" },
];
const _: () = assert!(sched::caps::CAP_LAST == 40, "update /proc/sys/kernel/cap_last_cap");
const _: () = assert!(sched::task::NGROUPS_MAX == 32, "update /proc/sys/kernel/ngroups_max");
const _: () = assert!(sched::runqueue::MAX_TASKS == 4096, "update /proc/sys/kernel/threads-max");

/// Known directories for getdents64.
static RAMFS_DIRS: &[&[u8]] = &[
    b"/",
    VOID_ROOT,
    b"/etc",
    b"/dev",
    b"/proc",
    b"/bin",
    b"/tmp",
    b"/dev/shm",
    // The DRM node directory. Its *contents* have always been synthesised from
    // the dynamic-device registry (see the getdents pass below), but the
    // directory itself was not openable, and libdrm's drmGetDevices2() opens
    // with `opendir(DRM_DIR_NAME)` and bails out with -errno the moment that
    // fails (xf86drm.c:4855) — so every DRM node was invisible to it no matter
    // what /sys said. Listing it here is what makes open/stat/getdents agree on
    // it.
    b"/dev/dri",
    // The evdev node directory, for the same reason: its contents come from
    // the dynamic-device registry, and without an entry `ls /dev/input` (and
    // any opendir scan) failed with ENOENT although every event node opened.
    // libinput never scans it here: the libudev shim enumerates input devices
    // from its own static table (ports/input-stack/shims/libudev).
    b"/dev/input",
    // The pts directory. Its contents are synthesised from the live pty pool
    // (see the getdents pass below); it is listed here so that `open`, `stat`
    // and `getdents64` agree it is a directory — `/dev/pts/N` resolution in
    // `handle_open` does not depend on it, but ttyname()'s readlink+stat
    // cross-check and `ls /dev/pts` do.
    b"/dev/pts",
    // Backing directories of devpts mounts (see devpts.rs).
    b"/dev/.devpts",
    b"/dev/mqueue",
    b"/run",
    b"/run/user",
    b"/mnt",
    b"/home",
    b"/root",
    // `/sys` itself is staged on disk (see mkfs-f2fs-populated.py) and is not
    // listed here; only `/sys/class/block` and below are synthesized by
    // `block::sysfs_*` from the live block registry and open as a `SysBlock`
    // vnode.
    b"/proc/net",
    b"/proc/sys",
    b"/proc/sys/kernel",
    b"/proc/sys/kernel/random",
    b"/proc/sys/vm",
    b"/proc/sys/user",
    b"/proc/sys/net",
    b"/proc/sys/net/core",
    b"/proc/sys/net/ipv4",
    b"/proc/sys/net/ipv4/conf",
    b"/proc/sys/net/ipv4/conf/all",
    b"/proc/sys/net/ipv4/conf/default",
    b"/proc/sys/net/ipv4/conf/lo",
    b"/proc/sys/net/ipv6",
    b"/proc/sys/net/ipv4/route",
    b"/proc/sys/net/ipv6/route",
    b"/proc/sys/net/ipv6/conf",
    b"/proc/sys/net/ipv6/conf/all",
    b"/proc/sys/net/ipv6/conf/default",
    b"/proc/sys/net/ipv6/conf/lo",
];

// ── Server port ───────────────────────────────────────────────────────────────

static SERVER_PORT: Mutex<u32> = Mutex::new(u32::MAX);

/// Initialise the VFS server and return its IPC port ID.
/// Ctrl-T dump hook: the VFS's pooled kernel memory, so a leak that lives in a
/// tmpfs/memfd VMO, a pipe ring or a stale fd table is visible next to the
/// buddy census rather than only as a lower `free_pages`. IRQ context —
/// `try_lock` only, nothing user-visible touched.
pub fn dump_vfs_census() {
    extern "C" { fn arch_serial_putc(c: u8); }
    fn ps(s: &str) { for &b in s.as_bytes() { unsafe { arch_serial_putc(b); } } }
    // Raw UART only: the kernel's print_number mirrors into the VT buffer
    // under the VT lock, which an interrupted CPU may hold (see `dump_raw`).
    fn pn(mut v: usize) {
        let mut buf = [0u8; 20]; let mut i = buf.len();
        loop { i -= 1; buf[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
        for &b in &buf[i..] { unsafe { arch_serial_putc(b); } }
    }
    fn phx(v: u64) {
        const H: &[u8; 16] = b"0123456789abcdef";
        let mut i = 60i32; let mut started = false;
        while i >= 0 { let d = ((v >> i) & 0xF) as usize; if d != 0 || started || i == 0 { started = true; unsafe { arch_serial_putc(H[d]); } } i -= 4; }
    }
    ps("[VFS]");
    match TMP_FILES.try_lock() {
        Some(t) => {
            let used = t.count();
            let eph = t.used_from(0).filter(|&i| t[i].ephemeral()).count();
            ps(" tmpfiles="); pn(used); ps(" ephemeral="); pn(eph);
            // Pool index self-check (lane tmpfast): 0 = consistent.
            ps(" tmpfsck="); pn(t.verify());
        }
        None => ps(" tmpfiles=busy"),
    }
    match TMP_VMOS.try_lock() {
        Some(v) => {
            let (mut slots, mut pages, mut borrowed, mut memfd) = (0usize, 0usize, 0usize, 0usize);
            for e in v.iter().flatten() {
                slots += 1;
                if e.borrowed { borrowed += 1; } else { pages += e.resident_pages(); }
                if e.is_memfd { memfd += 1; }
            }
            ps(" vmos="); pn(slots); ps(" vmo_pages="); pn(pages);
            ps(" memfd="); pn(memfd); ps(" dmabuf="); pn(borrowed);
        }
        None => ps(" vmos=busy"),
    }
    match PIPE_RINGS.try_lock() {
        Some(r) => {
            let live = r.iter().filter(|p| p.readers > 0 || p.writers > 0).count();
            ps(" pipes="); pn(live);
        }
        None => ps(" pipes=busy"),
    }
    match FD_TABLES.try_lock() {
        Some(t) => {
            let (mut tables, mut fds) = (0usize, 0usize);
            for tr in t.map.values() { tables += 1; if let Some(tb) = tr.try_lock() { fds += tb.fds.len(); } }
            ps(" fdtables="); pn(tables); ps(" fds="); pn(fds);
            // Per-process detail for the heavy tables: a table near its
            // RLIMIT_NOFILE is the next EMFILE.
            ps(" heavy:");
            for tr in t.map.values() { let Some(tb) = tr.try_lock() else { continue };
                let n = tb.fds.len();
                if n < 24 { continue; }
                let ev = tb.fds.iter_used().filter(|(_, f)| matches!(f.kind, VnodeKind::EventFd { .. })).count();
                ps(" "); pn(tb.pid as usize); ps("="); pn(n); ps("/ev"); pn(ev);
            }
        }
        None => ps(" fdtables=busy"),
    }
    match EVENTFD_COUNTERS.try_lock() {
        Some(c) => { ps(" eventfds="); pn(c.iter().filter(|&&v| v != u64::MAX).count()); ps("/"); pn(MAX_EVENTFDS); }
        None => ps(" eventfds=busy"),
    }
    match TIMERFD_REFS.try_lock() {
        Some(r) => { ps(" timerfds="); pn(r.iter().filter(|&&v| v > 0).count()); ps("/"); pn(MAX_TIMERFDS); }
        None => ps(" timerfds=busy"),
    }
    ps("\n");
    // Heavy tables (>= 20 eventfds, i.e. compositor-class processes): fds by
    // kind, eventfds split by origin, and with EVFD_DUMP_DETAIL one line per
    // eventfd with its provenance (see EVENTFD_META).
    if let Some(t) = FD_TABLES.try_lock() {
        let (meta, cnt) = match (EVENTFD_META.try_lock(), EVENTFD_COUNTERS.try_lock()) {
            (Some(m), Some(c)) => (m, c),
            _ => return,
        };
        for tr in t.map.values() { let Some(tb) = tr.try_lock() else { continue };
            let ev = tb.fds.iter_used().filter(|(_, f)| matches!(f.kind, VnodeKind::EventFd { .. })).count();
            if ev < 20 { continue; }
            // A dmabuf fd is a TmpFile over a borrowed VMO.
            let (mut fence, mut tmp, mut dmabuf, mut dev, mut pipe, mut other) = (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
            let vmos = TMP_VMOS.try_lock();
            for (_, f) in tb.fds.iter_used() {
                match f.kind {
                    VnodeKind::EventFd { slot } => if meta[slot].origin == EVFD_ORIGIN_FENCE { fence += 1 },
                    VnodeKind::TmpFile { idx, .. } => {
                        let db = vmos.as_ref().map_or(false, |v| v.get(idx).and_then(|o| o.as_ref()).map_or(false, |o| o.borrowed && o.dmabuf_obj != 0));
                        if db { dmabuf += 1 } else { tmp += 1 }
                    }
                    VnodeKind::DynamicDevice { .. } => dev += 1,
                    VnodeKind::Pipe { .. } => pipe += 1,
                    _ => other += 1,
                }
            }
            drop(vmos);
            ps("[FDK] pid="); pn(tb.pid as usize); ps(" ev="); pn(ev); ps(" ev_fence="); pn(fence);
            ps(" tmp="); pn(tmp); ps(" dmabuf="); pn(dmabuf); ps(" dev="); pn(dev);
            ps(" pipe="); pn(pipe); ps(" other="); pn(other); ps("\n");
            if !EVFD_DUMP_DETAIL { continue; }
            for (fd, f) in tb.fds.iter_used() {
                if let VnodeKind::EventFd { slot } = f.kind {
                    let m = meta[slot];
                    ps("[EVFD] pid="); pn(tb.pid as usize); ps(" fd="); pn(fd);
                    ps(" s="); pn(slot); ps(" o="); pn(m.origin as usize);
                    ps(" t="); pn(m.tgid as usize); ps(" q="); pn(m.serial as usize);
                    ps(" f="); phx(m.fence);
                    ps(" c="); pn(if cnt[slot] == u64::MAX { 99999 } else { cnt[slot].min(99998) as usize });
                    ps(" bt="); for b in m.bt { if b == 0 { break; } phx(b); ps(","); }
                    ps("\n");
                }
            }
        }
    }
}

pub fn init(owner_pid: u32) -> Option<u32> {
    let port_id = port::create(owner_pid)?;
    *SERVER_PORT.lock() = port_id;
    sched::register_dump_hook(dump_vfs_census);

    // Seed root's XDG runtime dir. /run/user/<uid> dirs are ordinary tmpfs
    // pool directories under the /run/user mount root, normally mkdir'd by
    // the login session that owns them — but early boot (and scmtest's
    // tmpfs_mounts_exist) relies on /run/user/0 existing before any login
    // step has run, so it is created here: uid 0, gid 0, mode 0700.
    {
        let mut tmp = TMP_FILES.lock();
        // The pool starts empty; grow it so slot 0 exists (and is free).
        let first = tmp_free_slot(&mut *tmp);
        debug_assert_eq!(first, Some(0));
        tmp.claim(0, b"/run/user/0", false);
        let e = &mut tmp[0];
        e.is_dir = true;
        e.mode   = 0o700;
        // The hidden superblock space for namespace tmpfs instances: created
        // here, at boot, root-owned and 0711, before any user can run.
        let _ = tmpfs_inst_parents(&mut tmp);
    }
    
    // Register IPC handler to respond to PINGs (prevents deadlocks during discovery scans)
    port::register_kernel_handler(port_id, |msg, pid, _target| {
        if msg.tag == 0x1000 {
            let mut reply = Message::empty();
            reply.tag = 0x1001;
            reply
        } else {
            handle(msg, pid)
        }
    });

    // Test: manually register a test device that should route to DRM server for testing
    {
        let mut devices = DYNAMIC_DEVICES.lock();
        if let Some(slot) = devices.iter_mut().find(|d| !d.in_use) {
            *slot = DynamicDeviceEntry {
                path: "/dev/input/testdrm",
                port: 999, // Invalid port - should fail but help us debug
                dev_id: 888,
                major: 0,
                minor: 0,
                in_use: true,
                notify_open: false,
            };
        }
    }

    // Register PID 1 with stdin/stdout/stderr → /dev/null.
    let mut tbls = TblSlot::new();
    let slot = get_or_create(1, &mut tbls)?;
    for fd in 0..3 {
        slot.fds.set(fd, FdEntry { kind: VnodeKind::DevNull, flags: 0, in_use: true, ofd: ofd::alloc() });
    }
    Some(port_id)
}

pub fn server_port() -> u32 { *SERVER_PORT.lock() }

/// Look up a path in RamFS and return a pointer + length to its static data.
/// Returns `None` if the path is not found.
pub fn get_file_data(path_ptr: usize) -> Option<(*const u8, usize)> {
    let (pbuf, plen) = read_cstr_raw(path_ptr)?;
    for entry in RAMFS {
        if path_eq(&pbuf, plen, entry.path) {
            return Some((entry.data.as_ptr(), entry.data.len()));
        }
    }
    None
}

/// Debug-only kernel-side read of a tmpfs regular file by absolute normalised
/// path (e.g. "/tmp/panel.log") into `out`, returning bytes copied. Kernel
/// buffers only — no user pointers — so it is safe to call from an arbitrary
/// syscall context (it takes only `TMP_FILES.lock()`, a plain spin mutex).
///
/// Backs the gated serial-dump diagnostic (`dbg_serial_dump` in the syscall
/// crate), which streams guest logs directly out the PL011 TX to bypass the
/// console/tty entirely — under HVF a Wayland compositor starves the UART RX
/// FIFO, so driver keystrokes drop and `cat`-of-a-log garbles. Returns `None`
/// for a missing path, a directory, a symlink, or an ephemeral /proc snapshot.
pub fn tmpfs_read_all(path: &str, out: &mut [u8]) -> Option<usize> {
    let p = path.as_bytes();
    let tmp = TMP_FILES.lock();
    let idx = tmp_find(&tmp, p)?;
    if tmp[idx].is_dir || tmp[idx].is_link || tmp[idx].is_fifo || tmp[idx].is_sock || tmp[idx].devtype != 0 {
        return None;
    }
    let owner = tmp_owner(&tmp, idx);
    Some(tmp_read_kernel(&tmp, owner, 0, out))
}

/// Look up a path string in RamFS and return a pointer + length to its data.
pub fn get_file_data_by_path(path: &str) -> Option<(*const u8, usize)> {
    let bytes = path.as_bytes();
    for entry in RAMFS {
        if entry.path == bytes {
            return Some((entry.data.as_ptr(), entry.data.len()));
        }
    }
    None
}

/// Check whether `path_ptr` points to a known directory (static or tmpfs).
pub fn is_directory(path_ptr: usize) -> bool {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return false };
    for &dir in RAMFS_DIRS {
        if path_eq(&pbuf, plen, dir) { return true; }
    }
    if devpts::inst_of_backing(&pbuf[..plen]).is_some() { return true; }
    // Check tmpfs dirs.
    let path = match tmpfs_path(&pbuf[..plen]) { Some(p) => p, None => return false };
    let tmp = TMP_FILES.lock();
    tmp_find(&tmp, path).map_or(false, |i| tmp[i].is_dir)
}

// ── Message dispatch ──────────────────────────────────────────────────────────

/// Which message arguments of `tag` are paths, and whether the *final*
/// component of each must be followed through a symlink.
///
/// Intermediate components are always followed (POSIX has no flavour of
/// lookup that doesn't); only the last one varies. The `false` group is the
/// set of operations that act on the link itself rather than its target —
/// getting `VFS_UNLINK` into the wrong group is what makes `rm symlink`
/// delete the *target*, so this table is the load-bearing part.
///
/// Takes the whole message because for `VFS_OPEN` the answer is not a property
/// of the opcode at all: `O_NOFOLLOW` moves it from "follow" to "don't", and it
/// lives in the flags argument.
fn path_args(msg: &Message) -> (Option<(usize, bool)>, Option<(usize, bool)>) {
    let tag = msg.tag;
    match tag {
        // arg1 is the open flags word.
        VFS_OPEN => (Some((0, arg(msg, 1) as u32 & O_NOFOLLOW == 0)), None),
        // The l-prefixed variants exist for the same reason VFS_LSTAT does:
        // AT_SYMLINK_NOFOLLOW makes the caller mean the link, not its target.
        VFS_LCHMOD | VFS_LCHOWN | VFS_LUTIMENS                  => (Some((0, false)), None),
        VFS_STAT | VFS_STATFS | VFS_CHMOD | VFS_CHOWN
        | VFS_UTIMENS                                           => (Some((0, true)), None),
        // xattr/access path forms: the plain and l-prefixed variants differ
        // only in whether the *final* component is followed, exactly like
        // stat/lstat above. The f-forms take an fd (arg0) and are absent here.
        VFS_SETXATTR | VFS_GETXATTR | VFS_LISTXATTR
        | VFS_REMOVEXATTR | VFS_ACCESS                         => (Some((0, true)), None),
        VFS_LSETXATTR | VFS_LGETXATTR | VFS_LLISTXATTR
        | VFS_LREMOVEXATTR                                     => (Some((0, false)), None),
        VFS_UNLINK | VFS_RMDIR | VFS_MKDIR | VFS_MKNOD
        | VFS_LSTAT | VFS_READLINK                              => (Some((0, false)), None),
        VFS_RENAME | VFS_LINK                                   => (Some((0, false)), Some((1, false))),
        // inotify_add_watch(fd, path, mask): the path is arg1; IN_DONT_FOLLOW keeps a final symlink.
        VFS_INOTIFY_ADD                                         => (Some((1, arg(msg, 2) as u32 & inotify::IN_DONT_FOLLOW == 0)), None),
        VFS_FANOTIFY_MARK if arg(msg, 1) != 0                   => (Some((1, arg(msg, 2) as u32 & 4 == 0)), None),
        // arg0 of VFS_SYMLINK is the link *body*, stored verbatim — resolving
        // it here would turn `ln -s ../x l` into an absolute link.
        VFS_SYMLINK                                             => (None, Some((1, false))),
        _                                                       => (None, None),
    }
}

/// Replace one path argument with its symlink-resolved form, parked in `buf`.
///
/// Only `/tmp` paths are touched. Everything else — `/bin/shell`, `/bin/brush`,
/// every RamFS and mount path — is handed to the handlers byte-identical to
/// how it arrived, so no non-tmpfs lookup (the exec path above all) can change
/// behaviour because of this.
/// Readlink of a staged `/sys` symlink; `None` when it is not a link.
fn sys_readlink(path: &[u8]) -> Option<Vec<u8>> {
    let mut p = path.to_vec();
    p.push(0);
    let mut out = alloc::vec![0u8; 256];
    let r = handle_readlink(p.as_ptr() as usize, out.as_mut_ptr() as usize, out.len());
    let n = reply_val(&r);
    if n <= 0 { return None; }
    out.truncate(n as usize);
    Some(out)
}

/// Readlink of one backing path on any filesystem; `None` when it is not a link.
fn backing_readlink(backing: &[u8]) -> Option<Vec<u8>> {
    if backing.starts_with(b"/proc") || backing.starts_with(b"/sys") || backing.starts_with(b"/dev") { return None; }
    let mut p = backing.to_vec();
    p.push(0);
    let mut out = alloc::vec![0u8; 512];
    let r = handle_readlink(p.as_ptr() as usize, out.as_mut_ptr() as usize, out.len());
    let n = reply_val(&r);
    if n <= 0 { return None; }
    out.truncate(n as usize);
    Some(out)
}

/// Resolve every symlink of namespace path `ns_path` in mount namespace `ns`
/// (each hop is translated through the namespace tree; an absolute target is
/// anchored at the namespace root). Returns the resolved namespace path, its
/// backing path and the final mount id; `None` when the namespace has no
/// tree. Namespace-0 tasks never come here.
pub fn ns_resolve_links(ns: sched::ns::NsId, ns_path: &[u8], follow: bool, floor: &[u8])
    -> Result<Option<(Vec<u8>, Vec<u8>, u32)>, i32>
{
    // `floor` is the caller's chroot prefix in namespace space ("/" or "" when
    // not chrooted): an absolute link body re-anchors there and `..` never
    // climbs above it (Linux path_walk root semantics).
    let floor: &[u8] = if floor.len() <= 1 { &[] } else { floor.strip_suffix(b"/").unwrap_or(floor) };
    let mut todo: Vec<Vec<u8>> = ns_path.split(|&c| c == b'/').filter(|c| !c.is_empty())
        .map(|c| c.to_vec()).collect();
    todo.reverse();
    let mut cur: Vec<u8> = Vec::new(); // "" == "/"
    let mut hops = 0u32;
    while let Some(c) = todo.pop() {
        if c == b"." { continue; }
        if c == b".." {
            if cur.len() > floor.len() {
                if let Some(i) = cur.iter().rposition(|&b| b == b'/') { cur.truncate(i); }
            }
            continue;
        }
        let last = todo.is_empty();
        let save = cur.len();
        cur.push(b'/');
        cur.extend_from_slice(&c);
        if last && !follow { continue; }
        let x = match mnt::translate(ns, &cur) { Some(x) => x, None => return Ok(None) };
        // A bind root swapped for a link must fail before that link is followed.
        bindpin_verify(ns, x.mnt_id)?;
        if let Some(t) = backing_readlink(&x.backing) {
            hops += 1;
            if hops > SYMLINK_MAX_HOPS { return Err(-40); }
            cur.truncate(save);
            if t.first() == Some(&b'/') { cur.clear(); cur.extend_from_slice(floor); }
            for comp in t.split(|&b| b == b'/').filter(|c| !c.is_empty()).rev() { todo.push(comp.to_vec()); }
        }
    }
    if cur.is_empty() { cur.push(b'/'); }
    let x = match mnt::translate(ns, &cur) { Some(x) => x, None => return Ok(None) };
    bindpin_verify(ns, x.mnt_id)?;
    Ok(Some((cur, x.backing, x.mnt_id)))
}

/// `(st_dev, st_ino)` of backing path `root`, `None` when it cannot be stat'ed.
fn stat_id_of(root: &[u8]) -> Option<(u64, u64)> {
    let mut p = root.to_vec();
    p.push(0);
    let mut buf = [0u8; 256];
    let r = handle_stat(p.as_ptr() as usize, buf.as_mut_ptr() as usize);
    if reply_val(&r) < 0 { None } else { Some(read_stat_id(buf.as_ptr() as usize)) }
}

/// mntiso-bindpin (D5): a pinned bind root must still be the object that was
/// bound. Only checked after a name-changing op somewhere (`NAME_GEN` moved
/// since the last verification): one stat. Mismatch = ESTALE (fail closed;
/// Linux would keep following the old dentry).
fn bindpin_verify(ns: sched::ns::NsId, mid: u32) -> Result<(), i32> {
    let Some((root, pin, vgen)) = mnt::pin_of(ns, mid) else { return Ok(()) };
    let g = mnt::name_gen();
    if vgen >= g { return Ok(()); }
    BINDPIN_STATS_CHECKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    match stat_id_of(&root) {
        Some(id) if id == pin => { mnt::pin_verified(ns, mid, g); Ok(()) }
        _ => { BINDPIN_STATS_STALE.fetch_add(1, core::sync::atomic::Ordering::Relaxed); Err(-116) }
    }
}
static BINDPIN_STATS_CHECKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static BINDPIN_STATS_STALE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Pin the root of bind mount `mid` of namespace `ns` to the object it names now.
/// Only host-backed trees are pinned (not /proc, /sys, /dev or the hidden sb space).
pub fn bindpin_set(ns: sched::ns::NsId, mid: u32) {
    let Some(root) = mnt::root_of(ns, mid) else { return };
    if root.starts_with(b"/proc") || root.starts_with(b"/sys") || root.starts_with(b"/dev") || mnt::is_sb_path(&root) { return; }
    let g = mnt::name_gen();
    if let Some(id) = stat_id_of(&root) { mnt::pin_set(ns, mid, id, g); }
}

/// Per-mount MS_RDONLY / MS_NODEV enforcement for a path resolved into a
/// mount of a non-host mount namespace (`flags` are that mount's flags).
/// EROFS for anything that modifies the tree or opens for write (special
/// files stay writable, as on Linux); EACCES for opening a device node on a
/// nodev mount. fds opened read-write before a remount-ro keep working.
fn mnt_flag_check(tag: u64, msg: &Message, backing: &[u8], flags: u8) -> Result<(), i32> {
    const S_IFMT: u32 = 0o170000;
    let st = |b: &[u8]| -> Option<u32> {
        let mut p = b.to_vec();
        p.push(0);
        let mut buf = [0u8; 256];
        let r = handle_stat(p.as_ptr() as usize, buf.as_mut_ptr() as usize);
        if reply_val(&r) < 0 { None } else { Some(read_stat_mode(buf.as_ptr() as usize)) }
    };
    if tag == VFS_OPEN {
        let fl = arg(msg, 1) as u32;
        if fl & O_PATH != 0 { return Ok(()); }
        let mode = st(backing);
        let special = matches!(mode.map(|m| m & S_IFMT), Some(0o020000) | Some(0o060000) | Some(0o010000) | Some(0o140000));
        if flags & mnt::MNT_NODEV != 0 && matches!(mode.map(|m| m & S_IFMT), Some(0o020000) | Some(0o060000)) {
            return Err(-13);
        }
        if flags & mnt::MNT_RDONLY != 0 && !special {
            let write = fl & 3 != 0 || fl & O_TRUNC != 0;
            if write { return Err(-30); }
            if fl & O_CREAT != 0 && mode.is_none() { return Err(-30); }
        }
        return Ok(());
    }
    if flags & mnt::MNT_RDONLY != 0 {
        match tag {
            VFS_UNLINK | VFS_RMDIR | VFS_MKDIR | VFS_MKNOD | VFS_RENAME | VFS_LINK | VFS_SYMLINK
            | VFS_CHMOD | VFS_LCHMOD | VFS_CHOWN | VFS_LCHOWN | VFS_UTIMENS | VFS_LUTIMENS
            | VFS_SETXATTR | VFS_LSETXATTR | VFS_REMOVEXATTR | VFS_LREMOVEXATTR => return Err(-30),
            _ => {}
        }
    }
    Ok(())
}

fn rewrite_one(msg: &mut Message, spec: Option<(usize, bool)>, buf: &mut Vec<u8>, mid_out: &mut Option<u32>)
    -> Result<(), i32>
{
    let (idx, follow) = match spec { Some(s) => s, None => return Ok(()) };
    let ptr = arg(msg, idx) as usize;
    let (raw, rlen) = match read_cstr_raw(ptr) { Some(r) => r, None => return Ok(()) };
    let path = strip_trailing_slash(&raw[..rlen]);
    // devfs: a renamed/linked/removed static /dev node is addressed by its
    // canonical name from here on (rename/unlink/link see the raw names).
    if !matches!(msg.tag, VFS_RENAME | VFS_UNLINK | VFS_LINK) && path.starts_with(b"/dev/") {
        if let Some(c) = devfs_canon(path) {
            *buf = c;
            buf.push(0);
            msg.data[idx * 8..idx * 8 + 8].copy_from_slice(&(buf.as_ptr() as u64).to_le_bytes());
            return Ok(());
        }
    }
    // Mount-namespace tasks: symlinks resolve in namespace space (an absolute
    // target is anchored at the pivoted root, not at the backing volume root).
    let pid = sched::current_pid();
    if let Some(cx) = mnt::callctx_get(pid, ptr) {
        if let Some(hf) = cx.hidden_flags {
            // dirfd with no name in this namespace: the flags of the mount it
            // was opened on still apply (Linux: the fd keeps its vfsmount).
            if hf & (mnt::MNT_RDONLY | mnt::MNT_NODEV) != 0 { mnt_flag_check(msg.tag, msg, path, hf)?; }
            // Unflagged ns-0 mount (carried only for identity): resolve as usual.
            if hf != 0 { return Ok(()); }
        } else {
        let ns = sched::current_mntns();
        if ns != 0 { sched::set_strict_links(true); }
        let jail = sched::current_root_arc();
        let floor: &[u8] = jail.as_deref().unwrap_or(b"/");
        let rr = ns_resolve_links(ns, &cx.ns_path, follow, floor);
        if let Some((nsp, backing, mid)) = rr? {
            *mid_out = Some(mid);
            // A mount root is busy for unlink/rmdir/rename (Linux: EBUSY); the
            // backing source must never be modified through the mount point.
            if matches!(msg.tag, VFS_UNLINK | VFS_RMDIR | VFS_RENAME) && mnt::is_mount_root(ns, &nsp) {
                return Err(-16);
            }
            let fl = mnt::flags_of(ns, mid);
            if fl & (mnt::MNT_RDONLY | mnt::MNT_NODEV) != 0 { mnt_flag_check(msg.tag, msg, &backing, fl)?; }
            if nsp != cx.ns_path {
                *buf = backing;
                buf.push(0);
                let nptr = buf.as_ptr() as usize;
                mnt::callctx_set(pid, mnt::CallCtx { ptr: nptr, ns_path: nsp, mnt_id: mid, hidden_flags: None });
                msg.data[idx * 8..idx * 8 + 8].copy_from_slice(&(nptr as u64).to_le_bytes());
                return Ok(());
            }
        }
        }
    }
    if path.starts_with(b"/sys/") {
        if let Some(r) = sysclass::expand(path, follow)? {
            *buf = r;
            buf.push(0);
            msg.data[idx * 8..idx * 8 + 8].copy_from_slice(&(buf.as_ptr() as u64).to_le_bytes());
            return Ok(());
        }
    }
    if !is_tmp_path(path) { return Ok(()); }

    let resolved = tmp_resolve_links(path, follow)?;
    if resolved[..] == raw[..rlen] { return Ok(()); }

    *buf = resolved;
    buf.push(0); // handlers scan for the NUL
    msg.data[idx * 8..idx * 8 + 8]
        .copy_from_slice(&(buf.as_ptr() as u64).to_le_bytes());
    Ok(())
}

pub fn handle(msg: &Message, caller_pid: u32) -> Message {
    // Fd tables are per-*process*: canonicalize the caller to its thread-
    // group id so CLONE_THREAD siblings share one table (a pipe opened on a
    // process's main thread must be readable/pollable from its worker
    // threads). Ops that operate on explicit pids (VFS_FORK_DUP,
    // VFS_CLOSE_ALL, VFS_EXEC_CLOEXEC) take them as message args and are
    // unaffected by this canonicalization.
    let caller_pid = sched::tgid_of(caller_pid);
    handle_as(msg, caller_pid)
}

/// Close every descriptor of `pid` (used to tear down an overlay pseudo-pid).
pub(crate) fn handle_close_all_pub(pid: u32) -> Message { handle_close_all(pid) }

/// `handle` for a caller that is already canonical (no tgid mapping): the
/// overlay's pseudo-pids are not scheduler tasks, so `tgid_of` must not see them.
pub(crate) fn handle_as(msg: &Message, caller_pid: u32) -> Message {
    // Symlink resolution happens here, once, for every path-taking operation —
    // rather than in each of the fifteen handlers that would otherwise each
    // need their own copy of the walk. The rewritten paths live in this
    // frame, and the caller stays blocked in `call_port` for the whole round
    // trip, so they outlive every server that reads them (the same lifetime
    // argument `KPath` makes on the kernel side).
    let (p0, p1) = path_args(&msg);
    if p0.is_some() || p1.is_some() {
        // mntiso-strict: a filesystem that meets a link in a ns-resolved call
        // answers VFS_ELINKRACE; redo the resolution at most twice, then ELOOP.
        let was_strict = sched::strict_links();
        let mut attempt = 0;
        let out = loop {
            let mut b0: Vec<u8> = Vec::new();
            let mut b1: Vec<u8> = Vec::new();
            let mut m = *msg;
            let (mut i0, mut i1): (Option<u32>, Option<u32>) = (None, None);
            let mut r = match rewrite_one(&mut m, p0, &mut b0, &mut i0) {
                Err(e) => err_reply(e),
                Ok(()) => match rewrite_one(&mut m, p1, &mut b1, &mut i1) {
                    Err(e) => err_reply(e),
                    Ok(()) => {
                        // rename/link across mounts is EXDEV even when both sit on one volume.
                        let xdev = matches!(m.tag, VFS_RENAME | VFS_LINK)
                            && matches!((i0, i1), (Some(a), Some(b)) if a != b);
                        if xdev { err_reply(-18) } else { dispatch(&m, caller_pid) }
                    }
                },
            };
            // Contexts registered for the rewritten buffers die with them (a stale
            // entry would be found again when the allocator reuses the address).
            let me = sched::current_pid();
            if !b0.is_empty() { mnt::callctx_clear(me, b0.as_ptr() as usize); }
            if !b1.is_empty() { mnt::callctx_clear(me, b1.as_ptr() as usize); }
            if reply_val(&r) == VFS_ELINKRACE as i64 {
                attempt += 1;
                if attempt > 2 { r = err_reply(-40); } else { continue; }
            }
            break r;
        };
        sched::set_strict_links(was_strict);
        return out;
    }
    dispatch(msg, caller_pid)
}

/// (parent, name) of a normalised path.
fn inotify_split(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.len() < 2 { return None; }
    let i = path.iter().rposition(|&b| b == b'/')?;
    Some((if i == 0 { &path[..1] } else { &path[..i] }, &path[i + 1..]))
}

/// The normalised backing path named by message argument `n`, owned (the
/// tmpfs-pool spelling for a tmpfs path, the plain path for every other
/// filesystem; the same normalisation `handle_inotify_add` stores).
fn inotify_arg(msg: &Message, n: usize) -> Option<Vec<u8>> {
    let (b, l) = read_cstr_raw(arg(msg, n) as usize)?;
    let raw = &b[..l];
    Some(tmpfs_path(raw).unwrap_or_else(|| strip_trailing_slash(raw)).to_vec())
}

/// Inode identity of an existing path: `(st_dev, st_ino)`, link count, kind.
struct InoIdent { id: inotify::Id, nlink: u64, isdir: bool }

#[cfg(target_arch = "x86_64")]
const ST_NLINK_OFF: usize = 16;
#[cfg(target_arch = "aarch64")]
const ST_NLINK_OFF: usize = 20;

/// Identity of `path` (tmpfs: the pool; other filesystems: an lstat, so
/// callers gate it on `inotify::may_watch`).
fn inotify_ident(path: &[u8]) -> Option<InoIdent> {
    {
        let tmp = TMP_FILES.lock();
        if let Some(i) = tmp_find(&tmp, path) {
            let o = tmp_owner(&tmp, i);
            let dev = tmpfs_dev_of(path).unwrap_or(0);
            return Some(InoIdent { id: (dev, tmp[o].ino as u64), nlink: if tmp[i].is_dir { 1 } else { tmp_nlink(&tmp, i) }, isdir: tmp[i].is_dir });
        }
    }
    if tmpfs_path(path).is_some() { return None; }
    let mut z = path.to_vec();
    z.push(0);
    let mut st = [0u8; STAT_SIZE];
    if reply_val(&stat_common(z.as_ptr() as usize, st.as_mut_ptr() as usize, false)) < 0 { return None; }
    let rd64 = |o: usize| unsafe { ((st.as_ptr() as usize + o) as *const u64).read_unaligned() };
    let mode = unsafe { ((st.as_ptr() as usize + ST_MODE_OFF) as *const u32).read_unaligned() };
    #[cfg(target_arch = "x86_64")]
    let nlink = rd64(ST_NLINK_OFF);
    #[cfg(target_arch = "aarch64")]
    let nlink = unsafe { ((st.as_ptr() as usize + ST_NLINK_OFF) as *const u32).read_unaligned() } as u64;
    Some(InoIdent { id: (rd64(0), rd64(8)), nlink, isdir: mode & 0o170000 == 0o040000 })
}

fn inotify_kind(path: &[u8]) -> Option<bool> { inotify_ident(path).map(|i| i.isdir) }

/// Identity for `inotify.rs` (its hooks resolve names to inodes with this).
pub fn inotify_id(path: &[u8]) -> Option<inotify::Id> { inotify_ident(path).map(|i| i.id) }

fn inotify_id_if(path: &[u8]) -> inotify::Id {
    if inotify::multi_watched() { inotify_id(path).unwrap_or((0, 0)) } else { (0, 0) }
}

/// `Some(kind)` when a watch could match `path` (or its parent) and the path
/// exists; the lookup is skipped (None) when no watch could care.
fn inotify_kind_if_watched(path: &[u8]) -> Option<bool> {
    if inotify::may_watch(path) { inotify_kind(path) } else { None }
}

fn inotify_dflag(isdir: Option<bool>) -> u32 { if isdir == Some(true) { inotify::IN_ISDIR } else { 0 } }

/// The description id and file kind behind `fd`: (ofd, writable) for tmpfs and
/// mounted regular files and directories, None for everything else.
fn inotify_fd_ofd(pid: u32, fd: usize) -> Option<(u32, bool)> {
    let mut tbls = TblSlot::new();
    let tbl = find_tbl(pid, &mut tbls)?;
    if !tbl.fds.used(fd) { return None; }
    let e = &tbl.fds[fd];
    let w = match &e.kind {
        VnodeKind::TmpFile { writable, .. } => *writable,
        VnodeKind::MountedFile { .. } => e.flags & 3 != 0,
        _ => return None,
    };
    if e.ofd == 0 { None } else { Some((e.ofd, w)) }
}

/// Start remembering the path behind `ofd` (and have its last put report back).
fn inotify_note(ofd: u32, path: &[u8], writable: bool, isdir: bool) {
    if ofd::mark_watched(ofd) { inotify::note_set(ofd, path, inotify_id_if(path), writable, isdir); }
}

/// An inode event on the file behind `fd` (writes, reads, fchmod ...).
fn inotify_fd_event(pid: u32, fd: usize, mask: u32) {
    let Some((o, w)) = inotify_fd_ofd(pid, fd) else { return };
    if inotify::note_event(o, mask) { return; }
    // Opened before any watch asked for inode events: learn its path now.
    let mut buf = alloc::vec![0u8; 4096];
    let r = reply_val(&handle_fd_path(pid, fd, buf.as_mut_ptr() as usize, buf.len()));
    if r <= 0 { return; }
    buf.truncate(r as usize);
    if buf.ends_with(b" (deleted)") { return; }
    let p = tmpfs_path(&buf).unwrap_or_else(|| strip_trailing_slash(&buf)).to_vec();
    let isdir = inotify_kind_if_watched(&p) == Some(true);
    inotify_note(o, &p, w, isdir);
    inotify::note_event(o, mask);
}

/// The mount `path` lies on: (its backing root prefix, its `st_dev`). A
/// tmpfs space is its own mount; otherwise the longest registered mount
/// prefix, else the root.
pub fn fan_mount_of(path: &[u8]) -> (Vec<u8>, u64) {
    let p = tmpfs_path(path).unwrap_or(path);
    if let Some(r) = tmpfs_root_of(p) { return (r.to_vec(), tmpfs_dev_of(p).unwrap_or(VFS_ANON_DEV)); }
    let m = MOUNTS.lock();
    let mut best: Option<(&[u8], u64)> = None;
    for e in m.iter() {
        if !e.in_use { continue; }
        let pb = e.prefix.as_bytes();
        if p.starts_with(pb) && (pb == b"/" || p.len() == pb.len() || p.get(pb.len()) == Some(&b'/')) && best.map_or(true, |(b, _)| pb.len() >= b.len()) { best = Some((pb, e.dev)); }
    }
    match best { Some((b, d)) => (b.to_vec(), d), None => (b"/".to_vec(), VFS_ANON_DEV) }
}

/// FAN_OPEN_EXEC_PERM gate of execve(`path`): 0 to go ahead, else -errno.
pub fn fan_exec_perm(path: &[u8]) -> i32 {
    if !inotify::armed() { return 0; }
    let p = tmpfs_path(path).unwrap_or_else(|| strip_trailing_slash(path)).to_vec();
    if inotify_kind(&p) == Some(false) { inotify::fan_open_exec(&p); inotify::inode(&p, inotify::IN_OPEN); }
    if !inotify::perm_watched() { return 0; }
    match inotify_kind(&p) { Some(false) => inotify::fan_perm(&p, inotify::FAN_OPEN_EXEC_PERM, false), _ => 0 }
}

/// FAN_ACCESS_PERM gate of a read on `fd` (regular files and directories).
fn fan_access_gate(pid: u32, fd: usize) -> i32 {
    if inotify_fd_ofd(pid, fd).is_none() { return 0; }
    let mut buf = alloc::vec![0u8; 4096];
    let r = reply_val(&handle_fd_path(pid, fd, buf.as_mut_ptr() as usize, buf.len()));
    if r <= 0 { return 0; }
    buf.truncate(r as usize);
    if buf.ends_with(b" (deleted)") { return 0; }
    let p = tmpfs_path(&buf).unwrap_or_else(|| strip_trailing_slash(&buf)).to_vec();
    let isdir = inotify_kind(&p) == Some(true);
    inotify::fan_perm(&p, inotify::FAN_ACCESS_PERM, isdir)
}

/// Run `dispatch_inner` and, when a watch exists anywhere, emit the events a
/// successful operation implies (design §2). Only reached when
/// `inotify::armed()`. Events come from the VFS's own routing points, so one
/// path covers tmpfs and every mounted filesystem.
fn inotify_hooks(msg: &Message, pid: u32) -> Message {
    use inotify::*;
    match msg.tag {
        VFS_OPEN => {
            let flags = arg(msg, 1) as u32;
            let path = inotify_arg(msg, 0);
            let pre = match &path {
                Some(p) if flags & (O_CREAT | O_TRUNC) != 0 => inotify_kind_if_watched(p),
                _ => None,
            };
            // Linux raises FAN_OPEN_PERM for an O_CREAT that creates the file too,
            // after the creation: a deny leaves the new file and fails the open.
            let mut perm_after = false;
            if perm_watched() && flags & O_PATH == 0 {
                if let Some(p) = &path {
                    if let Some(isdir) = inotify_kind(p) {
                        let pr = fan_perm(p, FAN_OPEN_PERM, isdir); if pr != 0 { return err_reply(pr); }
                    } else if flags & O_CREAT != 0 { perm_after = true; }
                }
            }
            let r = dispatch_inner(msg, pid);
            let v = reply_val(&r);
            if perm_after && v >= 0 {
                if let Some(p) = &path {
                    let pr = fan_perm(p, FAN_OPEN_PERM, false);
                    if pr != 0 { let _ = handle_close(pid, v as usize); return err_reply(pr); }
                }
            }
            if let Some(p) = path {
                if v >= 0 {
                    let created = flags & O_CREAT != 0 && pre.is_none() && may_watch(&p);
                    if created {
                        if let Some((par, nm)) = inotify_split(&p) { dirent(par, nm, IN_CREATE, 0); }
                    }
                    if flags & O_PATH == 0 {
                        let isdir = if created { Some(false) } else if pre.is_some() { pre } else { inotify_kind_if_watched(&p) };
                        let d = inotify_dflag(isdir);
                        if may_watch(&p) { inode(&p, IN_OPEN | d); }
                        if flags & O_TRUNC != 0 && pre.is_some() && !created { inode(&p, IN_MODIFY | d); }
                        if files_watched() {
                            if let Some((o, w)) = inotify_fd_ofd(pid, v as usize) {
                                inotify_note(o, &p, w, isdir == Some(true));
                            }
                        }
                    }
                }
            }
            r
        }
        VFS_MKDIR | VFS_MKNOD | VFS_SYMLINK | VFS_LINK => {
            let (idx, mask) = match msg.tag {
                VFS_MKDIR => (0, IN_CREATE | IN_ISDIR),
                VFS_MKNOD => (0, IN_CREATE),
                _ => (1, IN_CREATE),
            };
            let path = inotify_arg(msg, idx);
            let target = if msg.tag == VFS_LINK { inotify_arg(msg, 0) } else { None };
            let r = dispatch_inner(msg, pid);
            if reply_val(&r) >= 0 {
                if let Some(p) = path {
                    if let Some((par, nm)) = inotify_split(&p) { dirent(par, nm, mask, 0); }
                }
                if let Some(t) = target {
                    if may_watch(&t) {
                        if let Some(i) = inotify_ident(&t) { promote(i.id, &t); inode_id(&t, Some(i.id), IN_ATTRIB); }
                    }
                }
            }
            r
        }
        VFS_UNLINK | VFS_RMDIR => {
            let path = inotify_arg(msg, 0);
            let pre = match &path { Some(p) if msg.tag == VFS_UNLINK && may_watch(p) => inotify_ident(p), _ => None };
            let r = dispatch_inner(msg, pid);
            if let Some(p) = path {
                if reply_val(&r) >= 0 {
                    let isdir = msg.tag == VFS_RMDIR;
                    if let Some((par, nm)) = inotify_split(&p) {
                        dirent(par, nm, IN_DELETE | if isdir { IN_ISDIR } else { 0 }, 0);
                    }
                    if isdir {
                        removed(&p, true);
                    } else if may_watch(&p) {
                        let id = pre.as_ref().map(|i| i.id);
                        inode_id(&p, id, IN_ATTRIB);
                        // Other names remain: the inode lives on (link count change only).
                        if pre.as_ref().map_or(false, |i| i.nlink > 1) {
                        } else if !note_unlink(&p, id) { removed_id(&p, false, id); }
                    } else {
                        note_unlink(&p, None);
                    }
                }
            }
            r
        }
        VFS_RENAME => {
            let (old, new) = (inotify_arg(msg, 0), inotify_arg(msg, 1));
            let looked = old.as_deref().map_or(false, |o| may_watch(o)) || new.as_deref().map_or(false, |n| may_watch(n));
            let src = if looked { old.as_deref().and_then(inotify_ident) } else { None };
            let dst = if looked { new.as_deref().and_then(inotify_ident) } else { None };
            let r = dispatch_inner(msg, pid);
            if let (Some(o), Some(n)) = (old, new) {
                if o != n && reply_val(&r) >= 0 {
                    if let Some(si) = src {
                        let isdir = si.isdir;
                        let cookie = new_cookie();
                        let dir = if isdir { IN_ISDIR } else { 0 };
                        if let (Some((op, on)), Some((np, nn))) = (inotify_split(&o), inotify_split(&n)) {
                            dirent(op, on, IN_MOVED_FROM | dir, cookie);
                            dirent(np, nn, IN_MOVED_TO | dir, cookie);
                            fan_rename(&o, &n, isdir);
                        }
                        if let Some(di) = dst {
                            if di.nlink > 1 && !di.isdir { inode_id(&n, Some(di.id), IN_ATTRIB); } else { removed_id(&n, di.isdir, Some(di.id)); }
                        }
                        dirent_id(&o, b"", IN_MOVE_SELF | dir, 0, Some(si.id));
                    }
                    renamed(&o, &n);
                }
            }
            r
        }
        VFS_WRITE | VFS_PWRITE | VFS_FTRUNCATE | VFS_FALLOCATE if files_watched() => {
            let r = dispatch_inner(msg, pid);
            let v = reply_val(&r);
            if v > 0 || (v == 0 && msg.tag != VFS_WRITE && msg.tag != VFS_PWRITE) {
                inotify_fd_event(pid, arg(msg, 0) as usize, IN_MODIFY);
            }
            r
        }
        VFS_READ | VFS_PREAD if access_watched() || perm_watched() => {
            if perm_watched() { let gr = fan_access_gate(pid, arg(msg, 0) as usize); if gr != 0 { return err_reply(gr); } }
            let r = dispatch_inner(msg, pid);
            if access_watched() && reply_val(&r) > 0 { inotify_fd_event(pid, arg(msg, 0) as usize, IN_ACCESS); }
            r
        }
        VFS_CHMOD | VFS_LCHMOD | VFS_CHOWN | VFS_LCHOWN | VFS_UTIMENS | VFS_LUTIMENS
        | VFS_SETXATTR | VFS_LSETXATTR | VFS_REMOVEXATTR | VFS_LREMOVEXATTR if files_watched() => {
            let path = inotify_arg(msg, 0);
            let r = dispatch_inner(msg, pid);
            if reply_val(&r) >= 0 {
                if let Some(p) = path {
                    if may_watch(&p) { inode(&p, IN_ATTRIB | inotify_dflag(inotify_kind(&p))); }
                }
            }
            r
        }
        VFS_FCHMOD | VFS_FCHOWN | VFS_FUTIMENS | VFS_FSETXATTR | VFS_FREMOVEXATTR if files_watched() => {
            let r = dispatch_inner(msg, pid);
            if reply_val(&r) >= 0 { inotify_fd_event(pid, arg(msg, 0) as usize, IN_ATTRIB); }
            r
        }
        _ => dispatch_inner(msg, pid),
    }
}

/// A mount whose backing root is `prefix` went away (umount, superblock death).
pub fn inotify_unmounted(prefix: &[u8]) { inotify::unmounted(prefix); }

fn dispatch(msg: &Message, caller_pid: u32) -> Message {
    if inotify::armed() { return inotify_hooks(msg, caller_pid); }
    dispatch_inner(msg, caller_pid)
}

fn dispatch_inner(msg: &Message, caller_pid: u32) -> Message {
    match msg.tag {
        VFS_OPEN         => {
            // Remember the mount a mount-namespace task opened through, so a
            // later fchmod/fchown/futimens can answer EROFS on a read-only one.
            let cx = mnt::callctx_get(caller_pid, arg(msg,0) as usize);
            let r = handle_open(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32, arg(msg,2) as u32);
            if let Some(cx) = cx {
                let fd = reply_val(&r);
                if fd >= 0 {
                    match cx.hidden_flags {
                        // Inherit the originating mount's flags (ns MAX: no live table).
                        Some(hf) => fd_note_mnt(caller_pid, fd as usize, u16::MAX, cx.mnt_id.max(1), hf),
                        None => fd_note_mnt(caller_pid, fd as usize, sched::current_mntns(), cx.mnt_id, mnt::flags_of(sched::current_mntns(), cx.mnt_id)),
                    }
                }
            }
            r
        }
        VFS_READ         => handle_read(caller_pid, arg(msg,0) as usize,
                                         arg(msg,1) as usize, arg(msg,2) as usize),
        VFS_WRITE        => handle_write(caller_pid, arg(msg,0) as usize,
                                          arg(msg,1) as usize, arg(msg,2) as usize, arg(msg,3)),
        VFS_PREAD        => handle_pio(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize,
                                       arg(msg,2) as usize, arg(msg,3), false),
        VFS_PWRITE       => handle_pio(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize,
                                        arg(msg,2) as usize, arg(msg,3), true),
        VFS_CLOSE        => handle_close(caller_pid, arg(msg,0) as usize),
        VFS_STAT         => handle_stat(arg(msg,0) as usize, arg(msg,1) as usize),
        VFS_STATFS       => handle_statfs(arg(msg,0) as usize, arg(msg,1) as usize),
        VFS_FSTATFS      => handle_fstatfs(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize),
        VFS_LSEEK        => handle_lseek(caller_pid, arg(msg,0) as usize,
                                          arg(msg,1) as i64, arg(msg,2) as u32),
        VFS_PIPE         => handle_pipe(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize,
                                        arg(msg,2) as u32),
        VFS_FSTAT        => handle_fstat(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize),
        VFS_DUP2         => handle_dup2(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize,
                                        arg(msg,2) as u32 & O_CLOEXEC != 0),
        VFS_FCNTL        => handle_fcntl(caller_pid, arg(msg,0) as usize,
                                         arg(msg,1) as usize, arg(msg,2) as usize),
        VFS_FORK_DUP     => handle_fork_dup(arg(msg,0) as u32, arg(msg,1) as u32),
        VFS_EXEC_CLOEXEC => handle_exec_cloexec(arg(msg,0) as u32),
        VFS_CLOSE_ALL    => handle_close_all(arg(msg,0) as u32),
        VFS_GETDENTS64   => handle_getdents64(caller_pid, arg(msg,0) as usize,
                                               arg(msg,1) as usize, arg(msg,2) as usize),
        VFS_ALLOC_FD     => handle_alloc_fd(caller_pid, arg(msg,0) as usize),
        VFS_UNLINK       => handle_unlink(caller_pid, arg(msg,0) as usize),
        VFS_MKDIR        => handle_mkdir(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32),
        VFS_MKNOD        => handle_mknod(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32, arg(msg,2)),
        VFS_FTRUNCATE    => handle_ftruncate(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize),
        VFS_FALLOCATE    => handle_fallocate(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32,
                                             arg(msg,2) as i64, arg(msg,3) as i64),
        VFS_FSYNC        => handle_fsync(caller_pid, arg(msg,0) as usize, arg(msg,1)),
        VFS_SYNC         => handle_sync(),
        VFS_RENAME       => handle_rename(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize,
                                          arg(msg,2) as usize),
        VFS_FD_PATH      => handle_fd_path(caller_pid, arg(msg,0) as usize,
                                            arg(msg,1) as usize, arg(msg,2) as usize),
        VFS_EVENTFD          => handle_eventfd(caller_pid, arg(msg,0) as u64, arg(msg,1) as u32),
        VFS_SIGNALFD_CREATE  => handle_signalfd_create(caller_pid, arg(msg,0) as usize, arg(msg,1) as u64, arg(msg,2) as u32),
        VFS_INOTIFY_CREATE   => handle_inotify_create(caller_pid, arg(msg,0) as u32),
        VFS_INOTIFY_ADD      => handle_inotify_add(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize, arg(msg,2) as u32),
        VFS_FANOTIFY_INIT    => handle_fanotify_init(caller_pid, arg(msg,0) as u32, arg(msg,1) as u32),
        VFS_FANOTIFY_MARK    => handle_fanotify_mark(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize, arg(msg,2) as u32, arg(msg,3), arg(msg,4) as usize),
        VFS_INOTIFY_RM       => handle_inotify_rm(caller_pid, arg(msg,0) as usize, arg(msg,1) as i32),
        VFS_TIMERFD_CREATE   => handle_timerfd_create(caller_pid, arg(msg,0) as u32, arg(msg,1) as u32),
        VFS_TIMERFD_SETTIME  => handle_timerfd_settime(caller_pid, arg(msg,0) as usize,
                                                        arg(msg,1) as u64, arg(msg,2) as u64,
                                                        arg(msg,3) as u32),
        VFS_TIMERFD_GETTIME  => handle_timerfd_gettime(caller_pid, arg(msg,0) as usize,
                                                        arg(msg,1) as usize),
        VFS_IOCTL            => handle_ioctl(caller_pid, arg(msg,0) as usize,
                                              arg(msg,1) as usize, arg(msg,2) as usize),
        VFS_RMDIR            => handle_rmdir(caller_pid, arg(msg,0) as usize),
        VFS_FLOCK            => handle_flock(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32),
        VFS_CHMOD            => handle_chmod(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32, true),
        VFS_LCHMOD           => handle_chmod(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32, false),
        VFS_FCHMOD           => handle_fchmod(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32),
        VFS_CHOWN            => handle_chown(caller_pid, arg(msg,0) as usize,
                                              arg(msg,1) as u32, arg(msg,2) as u32, true),
        VFS_LCHOWN           => handle_chown(caller_pid, arg(msg,0) as usize,
                                              arg(msg,1) as u32, arg(msg,2) as u32, false),
        VFS_UTIMENS          => handle_utimens(caller_pid, msg, true),
        VFS_LUTIMENS         => handle_utimens(caller_pid, msg, false),
        VFS_FUTIMENS         => handle_futimens(caller_pid, msg),
        VFS_FCHOWN           => handle_fchown(caller_pid, arg(msg,0) as usize,
                                               arg(msg,1) as u32, arg(msg,2) as u32),
        VFS_POLL             => handle_poll(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32),
        VFS_PIVOT_ROOT       => handle_pivot_root(arg(msg,0) as usize, arg(msg,1) as usize),
        VFS_SYMLINK          => handle_symlink(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize),
        VFS_READLINK         => handle_readlink(arg(msg,0) as usize, arg(msg,1) as usize,
                                                arg(msg,2) as usize),
        VFS_LINK             => handle_link(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize),
        VFS_LSTAT            => stat_common(arg(msg,0) as usize, arg(msg,1) as usize, false),
        VFS_SETXATTR | VFS_LSETXATTR
                             => handle_setxattr(caller_pid, msg.tag, arg(msg,0) as usize,
                                                arg(msg,1) as usize, arg(msg,2) as usize,
                                                arg(msg,3) as usize, arg(msg,4) as u32),
        VFS_FSETXATTR        => handle_fsetxattr(caller_pid, arg(msg,0) as usize,
                                                 arg(msg,1) as usize, arg(msg,2) as usize,
                                                 arg(msg,3) as usize, arg(msg,4) as u32),
        VFS_GETXATTR | VFS_LGETXATTR
                             => handle_getxattr(caller_pid, msg.tag, arg(msg,0) as usize,
                                                arg(msg,1) as usize, arg(msg,2) as usize,
                                                arg(msg,3) as usize),
        VFS_FGETXATTR        => handle_fgetxattr(caller_pid, arg(msg,0) as usize,
                                                 arg(msg,1) as usize, arg(msg,2) as usize,
                                                 arg(msg,3) as usize),
        VFS_LISTXATTR | VFS_LLISTXATTR
                             => handle_listxattr(caller_pid, msg.tag, arg(msg,0) as usize,
                                                 arg(msg,1) as usize, arg(msg,2) as usize),
        VFS_FLISTXATTR       => handle_flistxattr(caller_pid, arg(msg,0) as usize,
                                                  arg(msg,1) as usize, arg(msg,2) as usize),
        VFS_REMOVEXATTR | VFS_LREMOVEXATTR
                             => handle_removexattr(caller_pid, msg.tag, arg(msg,0) as usize,
                                                   arg(msg,1) as usize),
        VFS_FREMOVEXATTR     => handle_fremovexattr(caller_pid, arg(msg,0) as usize,
                                                    arg(msg,1) as usize),
        VFS_ACCESS           => handle_access(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32, arg(msg,2) != 0),
        _                    => err_reply(-38), // ENOSYS
    }
}

fn handle_pivot_root(new_root_ptr: usize, put_old_ptr: usize) -> Message {
    let (new_buf, new_len) = match read_cstr_raw(new_root_ptr) {
        Some(r) => r,
        None    => return err_reply(-14), // EFAULT
    };
    let new_root = &new_buf[..new_len];

    let (old_buf, old_len) = match read_cstr_raw(put_old_ptr) {
        Some(r) => r,
        None    => return err_reply(-14), // EFAULT
    };
    let put_old = &old_buf[..old_len];

    let mut norm_new = new_root;
    if norm_new.ends_with(b"/") && norm_new.len() > 1 {
        norm_new = &norm_new[..norm_new.len()-1];
    }
    let norm_new_str = match core::str::from_utf8(norm_new) {
        Ok(s) => s,
        Err(_) => return err_reply(-22), // EINVAL
    };

    let mut mounts = MOUNTS.lock();
    let mount_idx = match mounts.iter().position(|m| m.in_use && m.prefix == norm_new_str) {
        Some(idx) => idx,
        None => return err_reply(-2), // ENOENT (new_root is not a mount point)
    };

    let rel_old = if put_old.starts_with(norm_new) {
        let r = &put_old[norm_new.len()..];
        if r.is_empty() {
            b"/"
        } else {
            r
        }
    } else {
        put_old
    };

    let put_old_str = match core::str::from_utf8(rel_old) {
        Ok(s) => {
            let s_obj = alloc::string::String::from(s);
            alloc::boxed::Box::leak(s_obj.into_boxed_str())
        }
        Err(_) => return err_reply(-22), // EINVAL
    };

    mounts[mount_idx].prefix = "/";
    *OLD_ROOT_PREFIX.lock() = Some(put_old_str);

    ok_reply()
}


// ── Handlers ─────────────────────────────────────────────────────────────────

// O_CREAT, O_TRUNC, O_WRONLY, O_RDWR flags
const O_WRONLY:    u32 = 0x01;
const O_RDWR:      u32 = 0x02;
const O_CREAT:     u32 = 0x40;
const O_EXCL:      u32 = 0x80;
const O_TRUNC:     u32 = 0x200;
const O_APPEND:    u32 = 0x400;
#[allow(dead_code)]
const O_DIRECTORY: u32 = 0x10000;
/// Refuse to open a symlink through its target (`ELOOP`) — the flag a
/// privileged writer uses so a symlink planted in a shared directory cannot
/// redirect it.
const O_NOFOLLOW:  u32 = 0x20000;
/// `O_PATH` — open the file only as a *reference* to a location in the tree.
/// The descriptor is legal to `fstat`, `dup`, `close` and pass as a `dirfd`,
/// but carries no read/write access at all, so those must fail EBADF rather
/// than succeed. It also suppresses the data-mutating flags: `O_PATH` ignores
/// the access mode and everything that would create or truncate, so an
/// `O_PATH | O_TRUNC` open must not destroy the file it is merely naming.
const O_PATH:      u32 = 0x200000;

static OLD_ROOT_PREFIX: Mutex<Option<&'static str>> = Mutex::new(None);

/// Is `path` /etc (or below it) on a mounted root filesystem? The disk's /etc
/// is the real one: the static RamFS entries (hostname, hosts, ...) only
/// stand in before the root is mounted, and must not shadow — or hide from a
/// listing — what the disk holds, nor stop an administrator editing it. Two
/// names stay synthesized: /etc/mtab (the live mount table) and
/// /etc/resolv.conf (the DHCP lease's servers).
fn etc_on_disk(path: &[u8]) -> bool {
    (path == b"/etc" || path.starts_with(b"/etc/"))
        && path != b"/etc/mtab" && path != b"/etc/resolv.conf"
        && find_mount_port(path).is_some()
}

fn should_lookup_ramfs<'a>(path: &'a [u8]) -> Option<&'a [u8]> {
    if etc_on_disk(path) { return None; }
    if path.starts_with(b"/dev/") || path.starts_with(b"/proc/")
       || path.starts_with(b"/tmp/") || path.starts_with(b"/etc/")
       || path == b"/dev" || path == b"/proc" || path == b"/tmp" || path == b"/etc"
       || path == VOID_ROOT
       // The /run and /run/user tmpfs mounts (pid/lock files, Wayland, D-Bus
       // and service sockets). /dev/shm rides the /dev/ prefix above; /tmp
       // rides /tmp/. Intercept /run before the mount table so it lands on
       // tmpfs, not the pivoted F2FS root.
       || path == b"/run" || path.starts_with(b"/run/")
       // Namespace tmpfs instances (hidden pool).
       || path == TMPFS_INST_PARENT || path == TMPFS_INST_DIR || path.starts_with(b"/.leandros-sb/tmpfs/")
       // Bind-mounted nsfs files (`alias::NS_SRC`).
       || path.starts_with(alias::NS_SRC)
       // Only the synthesized `/sys/class/block` subtree. Like /proc it is not
       // a mount and must be intercepted before `find_mount_port()`, which
       // matches every absolute path once init has pivot_root'ed the F2FS
       // volume onto "/". The rest of /sys is staged on disk and falls
       // through to the normal mount lookup below.
       || block::is_sysfs_path(path)
       // Hook-backed power_supply/backlight classes, only while they have devices.
       || sysclass::is_path(path)
       || sysclass::is_tables_path(path)
       // The cgroup2 tree: exactly /sys/fs/cgroup, and only once mounted.
       || cgroupfs::is_path(path)
       // A GPU backend's PCI identity for the DRM nodes — only the attribute
       // files it answers, only while it is registered (`drm_sysfs_attr`).
       || is_drm_sysfs_override(path)
    {
        return Some(path);
    }
    let old_root_prefix_lock = OLD_ROOT_PREFIX.lock();
    if let Some(prefix) = *old_root_prefix_lock {
        let pb = prefix.as_bytes();
        if path.starts_with(pb) && (path.len() == pb.len() || path.get(pb.len()) == Some(&b'/')) {
            let mut rest = &path[pb.len()..];
            if rest.is_empty() {
                rest = b"/";
            }
            return Some(rest);
        }
        None
    } else {
        Some(path)
    }
}

/// The tmpfs mount roots. Each is an absolute, normalised path with no trailing
/// slash; the TMP_FILES pool holds strict descendants of these (files, dirs,
/// symlinks, and AF_UNIX socket nodes), and each root itself is a pseudo-dir
/// listed in RAMFS_DIRS. `/tmp` is the original; `/dev/shm` (POSIX shm, wl_shm)
/// and `/run/user` (Wayland + D-Bus sockets) are the K1 additions.
///
/// `/run/user` is the root — NOT `/run/user/0` — so every per-uid XDG runtime
/// dir (`/run/user/<uid>`) is an ordinary pool directory a login session can
/// `mkdir` with its own uid/gid and 0700 mode. Root's `/run/user/0` is seeded
/// into the pool by `init()` so it exists from boot.
// `/run` is a tmpfs like on Linux (runtime state: pid files, lock files,
// service sockets such as /run/bluetooth/btd.sock); nothing under it survives
// a reboot, and the F2FS root's own /run is shadowed. `/run/user` stays its
// own root nested inside it -- it must precede `/run` here, because
// `tmpfs_root_of` takes the first match and the per-root index is the
// anonymous st_dev (appending keeps the older roots' device numbers).
static TMPFS_ROOTS: &[&[u8]] = &[b"/tmp", b"/dev/shm", b"/run/user", b"/run"];

/// Owner, group and permission bits of each `/dev/pts/N`, as devpts keeps
/// them: a new pair's slave belongs to whoever opened `/dev/ptmx`, group tty,
/// mode 0620 (Linux's `gid=5,mode=620` mount, what every distribution uses
/// and what glibc's grantpt would otherwise set up), and chown/chmod on the
/// node or on a slave fd change them — sshd hands the tty to the session's
/// user that way (OpenSSH's pty_setowner). Opening the slave by path is
/// checked against them like any device node.
static PTS_META: Mutex<[(u32, u32, u16); tty_server::pty::MAX_PTYS]> =
    Mutex::new([(0, GID_TTY, 0o620); tty_server::pty::MAX_PTYS]);

fn pts_meta(n: usize) -> xattr::FileMeta {
    let (uid, gid, mode) = PTS_META.lock().get(n).copied().unwrap_or((0, GID_TTY, 0o620));
    xattr::FileMeta { mode: 0o020000 | mode, uid, gid }
}

/// A fresh pair `n` from `/dev/ptmx`, opened by `pid`.
fn pts_meta_init(n: usize, pid: u32) {
    let (inst, _) = tty_server::pty::ident(n);
    let (uid, gid, mode) = match devpts::opts(inst) {
        Some(o) => (if o.set_uid { o.uid } else { sched::fsuid_of(pid) },
                    if o.set_gid { o.gid } else { sched::fsgid_of(pid) }, o.mode),
        None => (sched::fsuid_of(pid), GID_TTY, 0o620),
    };
    if let Some(e) = PTS_META.lock().get_mut(n) { *e = (uid, gid, mode); }
}

/// chmod(2) of `/dev/pts/N`: the owner or root.
fn pts_chmod(pid: u32, n: usize, mode: u32) -> Message {
    let cred = cred_of(pid);
    let mut t = PTS_META.lock();
    let Some(e) = t.get_mut(n) else { return err_reply(-2) };
    // Owner, or CAP_FOWNER over the inode in the caller's user namespace.
    if cred.euid != e.0 && !cred.cap_inode(xattr::CAP_FOWNER, e.0, e.1) { return err_reply(-1); } // EPERM
    e.2 = (mode & 0o7777) as u16;
    ok_reply()
}

/// chown(2) of `/dev/pts/N`, with the same rules as every other inode
/// (`apply_chown`): root changes anything, the owner only the group, to one
/// of its own.
fn pts_chown(pid: u32, n: usize, uid: u32, gid: u32) -> Message {
    let cred = cred_of(pid);
    let mut t = PTS_META.lock();
    let Some(e) = t.get_mut(n) else { return err_reply(-2) };
    // CAP_CHOWN over the inode in the caller's user namespace (host root: all).
    if !cred.cap_inode(xattr::CAP_CHOWN, e.0, e.1) {
        if cred.euid != e.0 { return err_reply(-1); }
        if uid != u32::MAX && uid != e.0 { return err_reply(-1); }
        if gid != u32::MAX && !cred.in_group(gid) { return err_reply(-1); }
    }
    if uid != u32::MAX { e.0 = uid; }
    if gid != u32::MAX { e.1 = gid; }
    ok_reply()
}

/// The `N` in `/dev/pts/N`, if `path` names an allocated pty slave.
///
/// Returns `None` for `/dev/pts` itself (the directory), for a non-numeric or
/// multi-component tail, and for a number no live master owns — so the caller
/// falls through to the ordinary ENOENT rather than inventing a device.
fn pts_number(path: &[u8]) -> Option<usize> {
    match devpts::parse(path)? {
        (inst, devpts::Tail::Slave(n)) => tty_server::pty::find(inst, n),
        _ => None,
    }
}

/// Instance whose `ptmx` `path` names (`/dev/ptmx` is the default's).
fn ptmx_inst(path: &[u8]) -> Option<u32> {
    if path == b"/dev/ptmx" { return Some(0); }
    match devpts::parse(path)? { (inst, devpts::Tail::Ptmx) => Some(inst), _ => None }
}

/// Owner/mode of an instance's `ptmx` node.
fn ptmx_meta(inst: u32) -> xattr::FileMeta {
    let mode = devpts::opts(inst).map_or(0o666, |o| o.ptmxmode);
    xattr::FileMeta { mode: 0o020000 | mode, uid: 0, gid: 0 }
}

/// The `N` in `/dev/ttyN`, if `path` names a virtual console.
///
/// `/dev/tty` (no digit) is deliberately NOT matched: it is the controlling
/// terminal, resolved through `pty::ctty_for_sid` by the caller, and folding it
/// in here would put every shell running under `cosmic-term` back on the
/// framebuffer console.
fn vt_number(path: &[u8]) -> Option<u8> {
    let rest = path.strip_prefix(b"/dev/tty".as_slice())?;
    match rest {
        [d @ b'0'..=b'6'] => Some(d - b'0'),
        _ => None,
    }
}

/// Inode numbers of the virtual consoles: `VT_INO_BASE + N` for `/dev/ttyN`.
///
/// Distinct from the pipe (0x1000_0000), tmpfs (0x2000_0000), console
/// (0x3000_0000) and pty (0x4000_0000) ranges, and distinct from `CONSOLE_INO`
/// in particular: `/dev/tty2` and `/dev/console` are different devices, and
/// `ttyname()` on a VT fd must read back `/dev/tty2`, not the console.
const VT_INO_BASE: u64 = 0x5000_0000;

/// VT number an fd names: 0 for /dev/tty0 ("the active VT"), 1..=6 for
/// /dev/tty1../dev/tty6. None for anything that is not a VT node.
///
/// Used by `sys_ioctl` to route the VT/KD ioctl set to `tty_server::vt`, and by
/// the kernel's console fast paths to tell `/dev/tty0` (whose read and poll are
/// the change notification below) from `/dev/tty1..6` (which are the console).
///
/// # The `/dev/tty0` VT-change notification
///
/// An open `/dev/tty0` is a **pollable edge source for "the active VT
/// changed"**, which is the one thing a libseat/logind shim's `get_fd()` +
/// `dispatch()` contract cannot be built without. The contract, exactly:
///
/// * `poll`/`epoll` report `POLLIN` while `vt::active()` differs from the VT
///   this open last saw. `POLLOUT` is always set (writes go to the console and
///   never block).
/// * `read()` with `count >= 1` consumes the edge and returns **one byte**
///   holding the new active VT number (1..=6). It writes exactly one byte
///   regardless of `count`.
/// * `read()` with no edge pending returns `EAGAIN`. A blocking fd therefore
///   blocks until the next switch; an `O_NONBLOCK` fd (what an event loop
///   wants) sees `EAGAIN` immediately, so drain-until-EAGAIN is correct.
/// * The state is **per open**: two `open("/dev/tty0")` calls each get their
///   own notification, and neither consumes the other's. `dup`/`fork` copies
///   carry a copy of the state rather than sharing one — a deliberate
///   divergence from Linux's shared `struct file`, and the safe direction (an
///   extra notification, never a missing one).
/// * The epoll edge-seq is the **active VT number itself**, not a monotonic
///   counter. `sys_epoll_wait` compares the seq for *inequality*, and the
///   readable condition is exactly "active differs from what this open last
///   saw", so the two agree by construction: a switch away and back to the same
///   VT between two polls is genuinely no event for this fd, and both the seq
///   and the level condition say so.
///
/// This is a LeandrOS-specific channel — Linux delivers VT changes by signal
/// through `VT_SETMODE(VT_PROCESS)`, which we also implement (`vt::ioctl`), but
/// which cannot be waited on with a file descriptor.
pub fn fd_vt_number(pid: u32, fd: usize) -> Option<usize> {
    let pid = sched::tgid_of(pid); // fd tables are per-process
    if fd >= FD_NUMBER_CEIL { return None; }
    let mut tbls = TblSlot::new();
    let tbl = find_tbl(pid, &mut tbls)?;
    if !tbl.fds[fd].in_use { return None; }
    match tbl.fds[fd].kind {
        VnodeKind::DevVt { vt, .. } => Some(vt as usize),
        _ => None,
    }
}

/// True when `path` names a tmpfs mount root exactly.
fn is_tmpfs_root(path: &[u8]) -> bool {
    TMPFS_ROOTS.iter().any(|&r| r == path)
}

/// The tmpfs root that owns `path` (the root itself, or a descendant under it),
/// or `None` when no tmpfs mount owns it.
fn tmpfs_root_of(path: &[u8]) -> Option<&'static [u8]> {
    let under = |r: &[u8]| path == r || (path.len() > r.len() && path.starts_with(r) && path[r.len()] == b'/');
    // The hidden namespace-instance pool counts as a tmpfs space, but only
    // strictly below its kind directory (the directory itself is a plain
    // pool entry created at boot).
    if path.len() > TMPFS_INST_DIR.len() && under(TMPFS_INST_DIR) { return Some(TMPFS_INST_DIR); }
    TMPFS_ROOTS.iter().copied().find(|&r| under(r))
}

/// Return true if `path` lives under any tmpfs mount root (root or descendant).
fn is_tmp_path(path: &[u8]) -> bool {
    tmpfs_root_of(path).is_some() && !(TMP_NESTED_MOUNTS.load(atomic::Ordering::Relaxed) != 0 && mounted_inside_tmpfs(path))
}

/// Mounts whose mount point lies inside a tmpfs root (a FUSE filesystem on
/// /tmp/x): paths under them belong to the mount, not to the tmpfs.
static TMP_NESTED_MOUNTS: atomic::AtomicUsize = atomic::AtomicUsize::new(0);

fn recount_tmp_nested(m: &[MountEntry]) {
    let n = m.iter().filter(|e| e.in_use && tmpfs_root_of(e.prefix.as_bytes()).is_some()).count();
    TMP_NESTED_MOUNTS.store(n, atomic::Ordering::Relaxed);
}

/// S_IFDIR mode (type | permission bits) for a RAMFS_DIRS pseudo-directory.
/// The tmpfs mount roots carry the conventional Linux perms, and since the
/// VFS now enforces them on creation these are load-bearing: `/tmp` and
/// `/dev/shm` are world-writable + sticky (1777) so any uid can create there
/// but only remove its own entries; `/run/user` stays 0755 root — the per-uid
/// dirs under it are created by root (`userland/init` seeds one per account)
/// and are pool entries carrying their own uid + 0700. Everything else keeps
/// the historical 0755.
fn ramfs_dir_mode(dir: &[u8]) -> u32 {
    match dir {
        b"/tmp" | b"/dev/shm" => 0o041777,
        VOID_ROOT             => 0o040555,
        _                     => 0o040755,
    }
}

// ── tmpfs permission gates ───────────────────────────────────────────────────
//
// One evaluator for the whole pool: `xattr::may_access` on the entry's meta
// (`tmp_meta`) and its stored access ACL. The gates below only decide WHICH
// inode is the subject — the parent for create/remove, each prefix component
// for traversal (`tmp_resolve_links`), the socket node for connect.

/// Meta of a tmpfs mount root ("/tmp", "/dev/shm", "/run/user"): root-owned,
/// mode from `ramfs_dir_mode`, no ACL.
fn tmp_root_meta(root: &[u8]) -> xattr::FileMeta {
    xattr::FileMeta { mode: ramfs_dir_mode(root) as u16, uid: 0, gid: 0 }
}

/// `xattr::may_access` on pool entry `idx` (resolved through `tmp_owner`, so a
/// hard-link alias is judged by the inode it names).
fn tmp_may(tmp: &TmpPool, idx: usize, cred: &xattr::Cred, mask: u8) -> bool {
    let owner = tmp_owner(tmp, idx);
    let (e, xa) = (&tmp[owner], tmp.xattr(owner));
    let meta = tmp_meta(e);
    let acl = xattr::find(&xa[..], xattr::IDX_ACL_ACCESS, b"");
    xattr::may_access(&meta, cred, acl, mask)
}

/// Check `mask` against directory `dir` (a mount root or a pool directory).
/// `Err(-ENOENT)` if it does not exist, `Err(-ENOTDIR)` if it is not a
/// directory, `Err(-EACCES)` if the caller lacks the permission. The
/// creation/removal gates call this with `MAY_WRITE | MAY_EXEC`.
fn tmp_dir_may(tmp: &TmpPool, dir: &[u8], cred: &xattr::Cred, mask: u8) -> Result<(), i32> {
    if is_tmpfs_root(dir) {
        return if xattr::may_access(&tmp_root_meta(dir), cred, None, mask) { Ok(()) } else { Err(-13) };
    }
    match tmp_find(tmp, dir) {
        None => Err(-2),
        Some(i) if !tmp[i].is_dir => Err(-20),
        Some(i) => if tmp_may(tmp, i, cred, mask) { Ok(()) } else { Err(-13) },
    }
}

/// Gate for creating a NEW entry `path` in the tmpfs pool, in Linux's order
/// (`filename_create` then `may_create`): search on the parent (EACCES),
/// then the existence probe (EEXIST), then write on the parent (EACCES).
/// The order is observable: `mkdir /tmp` as a user is "File exists", not
/// "Permission denied", and Rust's `create_dir_all` relies on it — it treats
/// any error other than NotFound as fatal unless the path already is a
/// directory... which it never gets to ask when the answer was EACCES.
/// Existence is not a secret to anyone with search permission anyway
/// (`stat` tells).
fn tmp_create_gate(tmp: &TmpPool, path: &[u8], cred: &xattr::Cred) -> Result<(), i32> {
    let parent = match tmp_parent(path) { Some(p) => p, None => return Err(-2) }; // ENOENT
    tmp_dir_may(tmp, parent, cred, xattr::MAY_EXEC)?;
    if tmp_find(tmp, path).is_some() { return Err(-17); }                        // EEXIST
    tmp_dir_may(tmp, parent, cred, xattr::MAY_WRITE | xattr::MAY_EXEC)?;
    // Linux `fsuidgid_has_mapping`: a caller whose fsuid/fsgid is unmapped in
    // its own user namespace (between unshare and the map write) cannot
    // create files whose owner would not exist. EOVERFLOW.
    if !cred.owner_mapped() { return Err(-75); }
    Ok(())
}

/// The sticky rule for removing/replacing pool entry `victim` inside `parent`
/// (`xattr::sticky_denies`): true means EPERM.
fn tmp_sticky_denies(tmp: &TmpPool, parent: &[u8], victim: usize, cred: &xattr::Cred) -> bool {
    let dmeta = if is_tmpfs_root(parent) {
        tmp_root_meta(parent)
    } else {
        match tmp_find(tmp, parent) { Some(i) => tmp_meta(&tmp[i]), None => return false }
    };
    let vmeta = tmp_meta(&tmp[tmp_owner(tmp, victim)]);
    xattr::sticky_denies_c(&dmeta, &vmeta, cred)
}

/// Removal gate: write + search on `parent`, then the sticky rule for `victim`.
fn tmp_may_delete(tmp: &TmpPool, parent: &[u8], victim: usize, cred: &xattr::Cred) -> Result<(), i32> {
    tmp_dir_may(tmp, parent, cred, xattr::MAY_WRITE | xattr::MAY_EXEC)?;
    if tmp_sticky_denies(tmp, parent, victim, cred) { Err(-1) } else { Ok(()) }
}

/// Search permission on every directory component of `path` strictly between
/// its tmpfs mount root and byte offset `upto` (the '/' that starts the
/// component being looked up). This is POSIX path traversal for the pool;
/// the mount roots themselves are world-searchable and skipped. Root bypasses
/// the permission part (`MAY_EXEC` on a directory is always granted to euid
/// 0) but not the walk: a file used as a directory ("/tmp/f/x") is ENOTDIR
/// for everyone, as on Linux (root used to get ENOENT). The check stops at
/// the first component that fails, so a caller learns nothing about what lies
/// below an unsearchable directory: EACCES, never ENOENT.
fn tmp_check_traversal(tmp: &TmpPool, path: &[u8], upto: usize, cred: &xattr::Cred) -> Result<(), i32> {
    let root_len = match tmpfs_root_of(path) { Some(r) => r.len(), None => return Ok(()) };
    let mut end = root_len;
    while end < upto {
        // `end` is the '/' preceding a component; find its end.
        let mut ce = end + 1;
        while ce < upto && path[ce] != b'/' { ce += 1; }
        if ce > upto { break; } // the component starting at `upto` is the leaf — not traversed
        match tmp_find(tmp, &path[..ce]) {
            None => return Err(-2),
            Some(i) if !tmp[i].is_dir => return Err(-20),
            Some(i) => if !cred.host_root && !tmp_may(tmp, i, cred, xattr::MAY_EXEC) { return Err(-13); },
        }
        end = ce;
    }
    Ok(())
}

// ── tmpfs path convention ────────────────────────────────────────────────────
//
// Every TMP_FILES entry stores an **absolute, normalised path with no trailing
// slash**: "/tmp/f", "/tmp/d", "/tmp/d/f". Directories are stored in exactly
// the same shape as files — the `is_dir` flag is the *only* thing that marks
// one, never a trailing '/'. "/tmp" itself is never a pool entry (it is a
// RAMFS_DIRS pseudo-directory), so the pool holds strict descendants of "/tmp"
// and nothing else.
//
// Parent/child is decided purely by byte prefix: `P` is a direct child of
// directory `D` iff `P.starts_with(D) && P[D.len()] == b'/'` and
// `P[D.len()+1..]` contains no further '/'. `handle_getdents64`, `handle_rmdir`
// and the directory rename below all use exactly that rule, so anything that
// creates an entry must normalise first or it becomes invisible to all three
// (an entry stored as "/tmp/d/" yields the child name "d/", which contains a
// '/' and is therefore skipped by enumeration).

/// Drop redundant trailing slashes ("/tmp/d/" → "/tmp/d"). "/" is left alone.
fn strip_trailing_slash(p: &[u8]) -> &[u8] {
    let mut p = p;
    while p.len() > 1 && p[p.len() - 1] == b'/' { p = &p[..p.len() - 1]; }
    p
}

/// Resolve `path` to the normalised tmpfs-pool path it names, or `None` when
/// the tmpfs pool does not own it.
///
/// This is the single choke point for "is this a tmpfs path?", and every
/// path-taking handler must consult it *before* `find_mount_port()`.
///
/// Why the ordering matters: `userland/init` pivot_roots onto F2FS, and
/// `handle_pivot_root` rewrites that mount's prefix to "/". From that moment
/// `find_mount_port()` matches *every* absolute path — `/tmp/...` included.
/// `handle_open` and `handle_stat` were immune because they funnel through
/// `should_lookup_ramfs()` first, but mkdir/rmdir/unlink/rename/chmod/chown
/// asked the mount table first and shipped tmpfs paths off to F2FS. That is
/// why `mv`/`rm` inside /tmp answered ENOENT while `cp` and `ls` worked, and
/// why `mkdir /tmp/d` "succeeded" silently: it created the directory on the
/// F2FS volume, where nothing enumerating /tmp could ever see it.
fn tmpfs_path(path: &[u8]) -> Option<&[u8]> {
    let lookup = strip_trailing_slash(should_lookup_ramfs(path)?);
    if is_tmp_path(lookup) { Some(lookup) } else { None }
}

/// Parent directory of a normalised tmpfs path ("/tmp/d/f" → "/tmp/d").
/// `None` for "/tmp" itself, which has no tmpfs parent.
fn tmp_parent(path: &[u8]) -> Option<&[u8]> {
    if is_tmpfs_root(path) { return None; }
    let i = path.iter().rposition(|&b| b == b'/')?;
    Some(if i == 0 { &path[..1] } else { &path[..i] })
}

/// Find a *named* pool entry. Ephemeral /proc snapshots are excluded: they
/// squat on synthetic "/tmp/.proc_N" paths that no user path can ever name.
/// O(1): the pool keeps a name index (lane tmpfast).
fn tmp_find(tmp: &TmpPool, path: &[u8]) -> Option<usize> {
    tmp.find(path)
}

/// True when any pool entry (other than `skip`) lives under directory `dir`.
///
/// `skip` is the directory's own slot: a named slot exists below `dir` only
/// as a child of its parent's slot (every intermediate directory is a pool
/// slot), so the pool's child index answers in O(1).
fn tmp_has_descendants(tmp: &TmpPool, dir: &[u8], skip: usize) -> bool {
    debug_assert!(tmp[skip].path() == dir);
    let _ = dir;
    tmp.has_children(skip)
}

// ── Hard links ───────────────────────────────────────────────────────────────
//
// A name and an inode are the same pool slot for an unlinked file. A hard link
// splits them: the *second* name gets its own slot whose `link_to` points at
// the first, and every operation that touches file content resolves through
// `tmp_owner()` first. That keeps the change surgical — `VnodeKind::TmpFile`
// still carries a bare pool index, and read/write/lseek/ftruncate/fstat are
// untouched — at the cost of one rule that must be honoured everywhere:
//
//   *Never* index the pool with a raw `tmp_find()` result when you are about
//   to touch the file's VMO, `.len`, `.mode`, `.uid` or `.gid`. Map it through
//   `tmp_owner()` first. Only path-shaped operations (rename, getdents,
//   lookup) legitimately use the un-mapped index.

/// Map a pool index to the slot that actually owns the bytes.
fn tmp_owner(tmp: &[TmpFileEntry], idx: usize) -> usize {
    let to = tmp[idx].link_to().wrapping_sub(1);
    if tmp[idx].link_to() == 0 || to >= tmp.len() || !tmp[to].in_use() { idx } else { to }
}

/// Number of aliases pointing at data-owning slot `owner`.
/// O(1): the pool counts aliases as `link_to` is set and slots are freed.
fn tmp_alias_count(tmp: &TmpPool, owner: usize) -> usize {
    tmp.alias_count(owner)
}

/// `st_nlink` for the file owned by slot `idx`.
///
/// An `ephemeral` owner is a *nameless* inode (see `tmp_drop_name`), so it
/// contributes nothing — which is how `ln a b; rm a; stat b` correctly reports
/// 1 rather than 2.
fn tmp_nlink(tmp: &TmpPool, idx: usize) -> u64 {
    let owner = tmp_owner(tmp, idx);
    let named = if tmp[owner].ephemeral() { 0 } else { 1 };
    named + tmp_alias_count(tmp, owner) as u64
}

/// Drop one *name*. The inode (the data-owning slot) survives while any other
/// name still refers to it — that is the whole point of `st_nlink`.
///
/// When the name being dropped is the data owner's own and aliases remain, the
/// slot is marked `ephemeral` rather than freed: that flag already means
/// "in use, but invisible to lookup and to getdents64", which is exactly a
/// nameless inode. The alternative — promoting an alias and memcpy'ing the
/// bytes into it — would invalidate every `VnodeKind::TmpFile { idx }` already
/// held by an open descriptor, so `ln a b; exec 3<a; rm a` would break fd 3.
/// An open descriptor keeps a nameless inode alive just as an alias does.
/// `open_fds` answers whether a descriptor still names a slot (`TMP_FDREFS`);
/// without it, `fd = open(p); unlink(p)` freed the pool slot out from
/// under `fd`, and the next creat() handed the same slot to an unrelated file.
/// That is precisely the create-then-unlink idiom tempfile(3) uses for
/// anonymous temporaries, so it is on the hot path for `tac`, `sort -o` and
/// every other tool that buffers through a temp file.
///
/// **Returns any dmabuf object reference the collected inode held**, because it
/// runs with the caller's `TMP_FILES` guard alive and must not drop a DRM
/// reference from under it (see `DMABUF_RELEASE`). Every caller passes the
/// value to `dmabuf_release` after its guard has died.
#[must_use = "the dmabuf reference must be dropped, with TMP_FILES released"]
fn tmp_drop_name(tmp: &mut TmpPool, idx: usize, open_fds: TmpOpen) -> Option<u32> {
    let referenced = |i: usize| open_fds.has(i);
    let owner = tmp_owner(tmp, idx);
    if owner != idx {
        // An alias. Free it (an alias never owns a VMO — the VMO is keyed on
        // `owner`), and collect the inode if that was the last name and the
        // owner had already lost its own.
        tmp.release(idx);
        if tmp[owner].ephemeral() && tmp_alias_count(tmp, owner) == 0 && !referenced(owner) {
            let obj = vmo_free_slot(owner); // release the inode's VMO frames (K1)
            tmp.release(owner);
            return obj;
        }
        return None;
    }
    if tmp_alias_count(tmp, idx) > 0 || referenced(idx) {
        tmp.make_nameless(idx);
        None
    } else {
        let obj = vmo_free_slot(idx); // release the inode's VMO frames (K1)
        tmp.release(idx);
        obj
    }
}

// ── In-flight SCM_RIGHTS references on tmpfs slots ─────────────────────────────
//
// A tmpfs/memfd inode's lifetime is *table-scan driven*: it dies once no fd
// table entry names its slot (`tmp_release_ephemeral`). A descriptor queued on
// an AF_UNIX stream is in neither table — `export_fd` lifted it out of the
// sender's and `import_fd` has not yet put it in the receiver's — so for the
// length of that queue it is invisible to the scan.
//
// That was harmless while every memfd kept its name: a named inode is not
// `ephemeral`, and `tmp_release_ephemeral` refuses to collect it. Now that
// `sys_memfd_create` unlinks, the window is a genuine use-after-free — Mesa's
// swrast path does `wl_shm_create_pool(fd)` then `close(fd)` immediately, so
// the sender's last descriptor is gone well before cosmic-comp's recvmsg, and
// the slot would be freed and handed to an unrelated file before the receiver's
// `TmpFile { idx }` ever resolved.
//
// One counter per slot, incremented by `export_fd`, decremented by
// `import_fd`/`drop_transfer`. Leaf lock: taken and released on its own, or
// nested only under FD_TABLES — never while TMP_FILES is held.
static TMP_INFLIGHT: Mutex<[u16; MAX_TMP_FILES]> = Mutex::new([0u16; MAX_TMP_FILES]);

/// Fd-table entries naming each tmpfs pool slot, across every process.
/// Maintained by `FdVec` itself (`set`, `clear`, `drain`, clone, drop), the
/// only ways an entry enters or leaves a table, so it cannot drift from the
/// tables. Replaces the walk of every fd table that each tmpfs close, unlink
/// and rename used to make to learn whether a slot was still open.
static TMP_FDREFS: [atomic::AtomicU32; MAX_TMP_FILES] =
    [const { atomic::AtomicU32::new(0) }; MAX_TMP_FILES];

/// Open references (fd-table entries plus queued SCM_RIGHTS copies) to each
/// block-device registry slot. Same discipline as `TMP_FDREFS`.
static BLK_FDREFS: [atomic::AtomicU32; block::MAX_BDEV] =
    [const { atomic::AtomicU32::new(0) }; block::MAX_BDEV];

/// Open references to each mounted-file open, keyed (port, file_id): every
/// fd-table entry and every queued SCM_RIGHTS copy. Leaf lock. Replaces the
/// walk of every fd table that the last
/// close of a mounted file used to make, which made `close` O(all open fds).
static MOUNT_FDREFS: Mutex<alloc::vec::Vec<(u32, u32, u32)>> = Mutex::new(alloc::vec::Vec::new());

fn mount_fdref_add(port: u32, file_id: u32, inc: bool) {
    let mut v = MOUNT_FDREFS.lock();
    if let Some(i) = v.iter().position(|e| e.0 == port && e.1 == file_id) {
        if inc { v[i].2 += 1; } else { v[i].2 = v[i].2.saturating_sub(1); if v[i].2 == 0 { v.swap_remove(i); } }
    } else if inc {
        v.push((port, file_id, 1));
    }
}

/// Is a mounted-file open still named by any descriptor or queued copy?
fn mount_referenced(port: u32, file_id: u32) -> bool {
    MOUNT_FDREFS.lock().iter().any(|e| e.0 == port && e.1 == file_id)
}

#[inline]
fn tmp_fdref_inc(kind: &VnodeKind) {
    match *kind {
        VnodeKind::TmpFile { idx, .. } => {
            if idx < MAX_TMP_FILES { TMP_FDREFS[idx].fetch_add(1, atomic::Ordering::AcqRel); }
        }
        VnodeKind::MountedFile { port, file_id } => mount_fdref_add(port, file_id, true),
        VnodeKind::BlockDev { dev, .. } => {
            if let Some(c) = BLK_FDREFS.get(dev as usize) { c.fetch_add(1, atomic::Ordering::AcqRel); }
        }
        _ => {}
    }
}

#[inline]
fn tmp_fdref_dec(kind: &VnodeKind) {
    match *kind {
        VnodeKind::TmpFile { idx, .. } => {
            if idx < MAX_TMP_FILES {
                let _ = TMP_FDREFS[idx].fetch_update(atomic::Ordering::AcqRel, atomic::Ordering::Acquire,
                                                     |n| Some(n.saturating_sub(1)));
            }
        }
        VnodeKind::MountedFile { port, file_id } => mount_fdref_add(port, file_id, false),
        VnodeKind::BlockDev { dev, .. } => {
            if let Some(c) = BLK_FDREFS.get(dev as usize) {
                let _ = c.fetch_update(atomic::Ordering::AcqRel, atomic::Ordering::Acquire,
                                       |n| Some(n.saturating_sub(1)));
            }
        }
        _ => {}
    }
}

/// Is tmpfs slot `idx` named by any fd-table entry?
#[inline]
fn tmp_fd_open(idx: usize) -> bool {
    idx < MAX_TMP_FILES && TMP_FDREFS[idx].load(atomic::Ordering::Acquire) != 0
}

// A mounted-filesystem open (`MountedFile { port, file_id }`) is likewise
// closed in its mount server once no fd-table entry names it (`handle_close`,
// `release_vnode`). A descriptor queued on an AF_UNIX socket is in no table,
// so a sender that does `dup; sendmsg(SCM_RIGHTS); close` — Firefox handing a
// Cache Storage body file to a content process — closed the server's open
// slot before the receiver's recvmsg installed its copy, which then read
// nothing (or whatever file reused the slot). `MOUNT_INFLIGHT` counts the
// queued copies per (port, file_id) so the last close waits for them.

/// Take an in-flight reference if `kind` is a tmpfs fd. No-op otherwise.
fn tmp_inflight_inc(kind: &VnodeKind) {
    // A queued copy is one more reference (same counters as fd-table entries).
    if matches!(*kind, VnodeKind::MountedFile { .. } | VnodeKind::BlockDev { .. }) { tmp_fdref_inc(kind); }
    if let VnodeKind::TmpFile { idx, .. } = *kind {
        if idx < MAX_TMP_FILES {
            let mut f = TMP_INFLIGHT.lock();
            f[idx] = f[idx].saturating_add(1);
            drop(f);
            tmp_fdref_inc(kind);
        }
    }
}

/// Drop an in-flight reference taken by `tmp_inflight_inc`. In `import_fd` this
/// must run *after* the descriptor is installed in the receiver's table, so the
/// slot is never unreferenced for even an instant.
fn tmp_inflight_dec(kind: &VnodeKind) {
    if matches!(*kind, VnodeKind::MountedFile { .. } | VnodeKind::BlockDev { .. }) { tmp_fdref_dec(kind); }
    if let VnodeKind::TmpFile { idx, .. } = *kind {
        if idx < MAX_TMP_FILES {
            let mut f = TMP_INFLIGHT.lock();
            f[idx] = f[idx].saturating_sub(1);
            drop(f);
            tmp_fdref_dec(kind);
        }
    }
}

/// One bit per tmpfs pool slot (the census's independent scan of the tables).
#[derive(Clone, Copy)]
struct TmpMask([u64; MAX_TMP_FILES / 64]);
const _: () = assert!(MAX_TMP_FILES % 64 == 0);

impl TmpMask {
    const fn new() -> Self { Self([0; MAX_TMP_FILES / 64]) }
    fn set(&mut self, i: usize) { if i < MAX_TMP_FILES { self.0[i / 64] |= 1u64 << (i % 64); } }
    fn has(&self, i: usize) -> bool { i < MAX_TMP_FILES && self.0[i / 64] & (1u64 << (i % 64)) != 0 }
}

/// "Is this slot still referenced by a descriptor?", for `tmp_drop_name`:
/// answered from `TMP_FDREFS` (fd-table entries plus queued SCM_RIGHTS
/// copies), which callers may consult with `TMP_FILES` held.
#[derive(Clone, Copy)]
struct TmpOpen;
impl TmpOpen {
    #[inline] fn has(&self, i: usize) -> bool { tmp_fd_open(i) }
}

fn tmp_open_fd_mask() -> TmpOpen { TmpOpen }

// ── Symlinks ─────────────────────────────────────────────────────────────────

/// Maximum number of symlink traversals in one path resolution before ELOOP.
/// Linux uses 40 (`MAXSYMLINKS`); matching it means a path that resolves on
/// Linux resolves here, and a cycle costs at most 40 bounded, *iterative*
/// passes — there is no recursion anywhere in the resolver, so a hostile
/// symlink graph cannot grow the kernel stack.
const SYMLINK_MAX_HOPS: u32 = 40;

/// Rewrite an absolute path in place, dropping empty / "." components and
/// resolving ".." lexically. Needed because splicing a symlink target back
/// into a path reintroduces both (the kernel normalised the *original* path,
/// but it never saw the link body).
/// Lexically normalise an absolute path. `floor` is the byte offset below which
/// `..` may not climb — 1 (the real root) normally, or the length of a chroot
/// jail's root when confining a jailed tmpfs symlink, so that an absolute link
/// target cannot use `..` to escape the jail. For `floor == 1` this is
/// byte-for-byte the old `normalize_abs`.
fn normalize_abs_floor(src: &[u8], floor: usize) -> Vec<u8> {
    let floor = floor.max(1);
    let mut out: Vec<u8> = Vec::with_capacity(src.len() + 1);
    out.push(b'/');
    for comp in src.split(|&b| b == b'/') {
        if comp.is_empty() || comp == b"." { continue; }
        if comp == b".." {
            let len = out.len();
            if len > floor {
                let mut last = len - 1;
                while last > 0 && out[last] != b'/' { last -= 1; }
                let clamped = if last == 0 { 1 } else { last };
                out.truncate(if clamped < floor { floor } else { clamped });
            }
            continue;
        }
        if out.len() > 1 { out.push(b'/'); }
        out.extend_from_slice(comp);
    }
    out
}

/// The calling task's chroot root, but only when it lies on tmpfs — the one
/// namespace this resolver owns. Returns its length, or 0 when the task is not
/// chrooted or its jail is rooted on another filesystem (a tmpfs symlink is
/// then unreachable by construction and needs no re-anchoring here).
///
/// tmpfs paths are host-absolute, so — unlike f2fs, which resolves in
/// volume-relative space — the jail root needs no coordinate translation: it is
/// already in the same space as the paths this resolver walks. Runs in the
/// caller's context (synchronous IPC), so `sched::current_root` names the right
/// task without any protocol change.
fn caller_jail_tmp() -> Vec<u8> {
    match sched::current_root_arc() {
        Some(h) if h.len() > 1 && is_tmp_path(&h) => h.to_vec(),
        _ => Vec::new(),
    }
}

/// Resolve every symlink in a **tmpfs** path, iteratively.
///
/// `follow_final` selects the two POSIX flavours of lookup: `false` stops one
/// component short, which is what unlink/rmdir/rename/lstat/readlink/symlink
/// need (they operate on the link itself); `true` is what open/stat/chmod/…
/// need. Intermediate components are always followed regardless.
///
/// A relative target is resolved against the directory holding the *symlink*,
/// never against the caller's cwd — that distinction is the classic bug here,
/// and it is why the splice below reuses `path[..comp_start]` rather than
/// anything derived from the process.
///
/// Returns the resolved path, which may legitimately leave the tmpfs pool
/// (`/tmp/l -> /bin/ls`): the caller re-dispatches it through normal routing,
/// so a tmpfs symlink into f2fs works. Returns `Err(-ELOOP)` on a cycle and
/// `Err(-ENAMETOOLONG)` if a splice overflows.
fn tmp_resolve_links(input: &[u8], follow_final: bool) -> Result<Vec<u8>, i32> {
    // Jail root on tmpfs (empty = not confined here). An absolute symlink target
    // must re-anchor here, not at the tmpfs root, or a link inside a jail rooted
    // on tmpfs (`chroot /tmp/jail`) can name a path above the jail. Unjailed,
    // `jlen == 0` and `floor == 1`, so everything below is the old behaviour.
    let jail = caller_jail_tmp();
    let jlen = jail.len();
    let floor = if jlen > 1 { jlen } else { 1 };

    let mut cur = normalize_abs_floor(input, floor);
    let mut hops = 0u32;
    let start_in_sb = mnt::is_sb_path(&cur);
    // Instance root `/.leandros-sb/tmpfs/<n>` of a walk that starts in the pool:
    // hops may not leave it for another instance (relative `..` or absolute).
    let sb_inst: Option<Vec<u8>> = if start_in_sb {
        let mut cnt = 0usize;
        let mut end = cur.len();
        for (i, &c) in cur.iter().enumerate() {
            if c == b'/' { cnt += 1; if cnt == 4 { end = i; break; } }
        }
        Some(cur[..end].to_vec())
    } else { None };

    // The caller's credentials, for the traversal check on each directory the
    // walk passes through. Taken before TMP_FILES (RUN_QUEUE is never taken
    // under it here).
    let cred = cred_of(sched::current_pid());
    let strict = sched::strict_links();

    loop {
        let path = &cur[..];
        // Left the tmpfs namespace (an absolute target pointing elsewhere) —
        // stop resolving and let the caller route the result.
        if !is_tmp_path(path) { break; }

        // Locate the first component that is a symlink. `comp_start` is the
        // index of the '/' preceding it, `comp_end` one past its last byte.
        let hit = {
            let tmp = TMP_FILES.lock();
            // Skip the mount-root prefix ("/tmp", "/dev/shm", "/run/user") so
            // component scanning starts at the first path element under it.
            let mut comp_start = tmpfs_root_of(path).map(|r| r.len()).unwrap_or(4);
            let mut found = None;
            while comp_start < path.len() {
                let mut comp_end = comp_start + 1;
                while comp_end < path.len() && path[comp_end] != b'/' { comp_end += 1; }
                let is_last = comp_end == path.len();
                if !(is_last && !follow_final) {
                    if let Some(idx) = tmp_find(&tmp, &path[..comp_end]) {
                        if tmp[idx].is_link {
                            let tlen = tmp[idx].len.min(TMP_LINK_MAX);
                            let target: Vec<u8> = tmp[idx].link[..tlen].to_vec();
                            found = Some((comp_start, comp_end, target, tlen));
                            break;
                        }
                    }
                }
                comp_start = comp_end;
            }
            // Traversal: every directory walked to reach the symlink (or, when
            // fully resolved, every directory above the final component) must
            // be searchable by the caller. Checked on each hop so a link
            // reached through an unsearchable directory is refused even when
            // its body leads somewhere the caller could otherwise reach.
            let upto = match &found {
                Some((cs, _, _, _)) => *cs,
                None => path.iter().rposition(|&b| b == b'/').unwrap_or(0),
            };
            tmp_check_traversal(&tmp, path, upto, &cred)?;
            found
        };

        let (comp_start, comp_end, target, tlen) = match hit {
            Some(h) => h,
            None    => break, // fully resolved
        };

        // Mount-namespace task whose path was already resolved in ns space: a
        // link met here appeared after that scan (a race) — never follow it.
        if strict { return Err(VFS_ELINKRACE); }

        hops += 1;
        if hops > SYMLINK_MAX_HOPS { return Err(-40); } // ELOOP

        // Splice: [prefix] + target + [remainder]. An absolute target replaces
        // the prefix outright; a relative one hangs off the symlink's own
        // parent directory (`path[..comp_start]`).
        let mut next: Vec<u8> = Vec::new();
        let mut n = 0usize;
        let mut push = |bytes: &[u8], n: &mut usize| -> bool {
            if *n + bytes.len() >= HOST_PATH_MAX { return false; }
            next.extend_from_slice(bytes);
            *n += bytes.len();
            true
        };
        if tlen > 0 && target[0] == b'/' {
            // Absolute target: re-anchor at the jail root so it cannot reach
            // tmpfs paths above the jail. Unjailed, `jlen == 0` and this is the
            // old verbatim behaviour.
            if jlen > 1 { if !push(&jail[..jlen], &mut n) { return Err(-36); } }
            if !push(&target[..tlen], &mut n) { return Err(-36); }
        } else {
            if !push(&path[..comp_start], &mut n) { return Err(-36); }
            if !push(b"/", &mut n) { return Err(-36); }
            if !push(&target[..tlen], &mut n) { return Err(-36); }
        }
        if !push(&path[comp_end..], &mut n) { return Err(-36); }

        drop(push);
        cur = normalize_abs_floor(&next[..n], floor);
        // A link may not lead into the hidden superblock space.
        if mnt::is_sb_path(&cur) {
            match &sb_inst {
                None => return Err(-2),
                Some(inst) => {
                    let ok = cur.starts_with(&inst[..])
                        && (cur.len() == inst.len() || cur[inst.len()] == b'/');
                    if !ok { return Err(-2); }
                }
            }
        }
    }

    Ok(cur)
}

/// Release a pool slot backing an ephemeral /proc snapshot once no fd refers
/// to it any more. Without this, every open of /proc/self/* burned one of the
/// 32 pool slots forever, and the pool exhaustion surfaced much later as an
/// unexplained ENOSPC from creat()/mkdir().
///
/// Lock order is the established FD_TABLES → TMP_FILES.
///
/// A dmabuf export's slot dies HERE, on the last close of the exported fd: the
/// PRIME intercept unlinks `/tmp/dmabuf:<n>` immediately, so the node is
/// nameless and this is the only site that can collect it. The DRM reference is
/// therefore dropped after the `TMP_FILES` guard, at the bottom of this
/// function — never inside it (see `DMABUF_RELEASE`).
fn tmp_release_ephemeral(idx: usize) {
    let mut tmp = TMP_FILES.lock();
    // `ephemeral` also marks a hard-linked inode whose own name was unlinked
    // while other names survive. Those are still reachable through an alias,
    // so an fd going away must not collect them.
    // `TMP_FDREFS` counts every fd-table entry and every queued SCM_RIGHTS
    // copy naming the slot, so one load answers what used to take a walk of
    // every table under the exclusive scan gate.
    let mut freed_obj = None;
    if !tmp_fd_open(idx) && tmp[idx].in_use() && tmp[idx].ephemeral()
        && tmp_alias_count(&tmp, idx) == 0
    {
        // A memfd whose name was unlinked while an fd stayed open lands here on
        // the final close — release its VMO frames before freeing the slot (K1).
        freed_obj = vmo_free_slot(idx);
        tmp.release(idx);
    }
    // Both guards die on these two lines, BEFORE the device is touched. The
    // order matters: `dmabuf_release` reaches `blob_unref`, which takes
    // VIRTIO_GPU and busy-spins on a round-trip.
    drop(tmp);
    dmabuf_release(freed_obj.unwrap_or(0));
}


/// Write a u32 decimal to `buf` starting at `pos`.  Returns new pos.
fn write_u32(buf: &mut [u8; TMP_BUF_SIZE], pos: usize, mut v: u32) -> usize {
    let start = pos;
    let mut tmp = [0u8; 10];
    let mut ti = 0usize;
    if v == 0 { tmp[ti] = b'0'; ti += 1; }
    while v > 0 { tmp[ti] = b'0' + (v % 10) as u8; ti += 1; v /= 10; }
    let mut out = pos;
    for i in (0..ti).rev() {
        if out < buf.len() { buf[out] = tmp[i]; out += 1; }
    }
    let _ = start;
    out
}

/// Write a literal byte slice into buf at pos.
fn write_lit(buf: &mut [u8; TMP_BUF_SIZE], pos: usize, s: &[u8]) -> usize {
    let copy = s.len().min(buf.len().saturating_sub(pos));
    buf[pos..pos+copy].copy_from_slice(&s[..copy]);
    pos + copy
}

/// Room for one generated /proc or /sys file. A process status with
/// NGROUPS_MAX supplementary groups of 10 digits runs past 700 bytes (it was
/// cut at 512 with about 20 groups: no VmRSS, no Threads line).
const TMP_BUF_SIZE: usize = 2048;

fn write_u64(buf: &mut [u8; TMP_BUF_SIZE], pos: usize, mut v: u64) -> usize {
    let mut tmp = [0u8; 20];
    let mut ti = 0usize;
    if v == 0 { tmp[ti] = b'0'; ti += 1; }
    while v > 0 { tmp[ti] = b'0' + (v % 10) as u8; ti += 1; v /= 10; }
    let mut out = pos;
    for i in (0..ti).rev() {
        if out < buf.len() { buf[out] = tmp[i]; out += 1; }
    }
    out
}

/// Append one `/etc/mtab`-format line per in-use mount:
/// `<device> <mountpoint> <fstype> <options> 0 0`.
///
/// This is the layout uucore's `MountInfo::new()` expects for both
/// `/etc/mtab` and the legacy `/proc/mounts` (fsext.rs: `LINUX_MTAB` arm —
/// `raw[0]`=dev_name, `raw[1]`=mount_dir, `raw[2]`=fs_type,
/// `raw[3]`=mount_option, split on the raw space-separated line). Bounded by
/// `TMP_BUF_SIZE` via `write_lit`/`write_u32`'s own clamping; if a line
/// would overflow the buffer it — and everything after it — is dropped
/// rather than emitted truncated (which would otherwise corrupt the last
/// field of the previous line for a caller that assumes one mount per
/// line).
fn write_mtab_lines(buf: &mut [u8; TMP_BUF_SIZE], mut p: usize) -> usize {
    for m in list_mounts().iter() {
        if !m.in_use { continue; }
        let start = p;
        p = write_lit(buf, p, m.device.as_bytes());
        p = write_lit(buf, p, b" ");
        p = write_lit(buf, p, m.prefix.as_bytes());
        p = write_lit(buf, p, b" ");
        p = write_lit(buf, p, m.fstype.as_bytes());
        p = write_lit(buf, p, b" ");
        p = write_lit(buf, p, mount_opts(m));
        p = write_lit(buf, p, b" 0 0\n");
        if p >= buf.len() { return start; }
    }
    // The in-memory tmpfs roots are served by this server rather than a
    // registered mount, but they are mounts all the same (statfs answers
    // TMPFS_MAGIC for them). Without these lines `df /tmp` matched "/" and
    // reported the root f2fs volume.
    for root in TMPFS_ROOTS.iter() {
        let start = p;
        p = write_lit(buf, p, b"tmpfs ");
        p = write_lit(buf, p, root);
        p = write_lit(buf, p, b" tmpfs ");
        p = write_lit(buf, p, tmpfs_opts(root));
        p = write_lit(buf, p, b" 0 0\n");
        if p >= buf.len() { return start; }
    }
    // Alias mounts (proc/devtmpfs/sysfs/bind), where the reader sees them.
    for (dev, mp, fstype, _sbi, fl) in alias_mounts_for_reader() {
        let start = p;
        p = write_lit(buf, p, &dev);
        p = write_lit(buf, p, b" ");
        p = write_lit(buf, p, &mp);
        p = write_lit(buf, p, b" ");
        p = write_lit(buf, p, fstype.as_bytes());
        // Alias mounts are always nosuid (see alias.rs).
        p = write_lit(buf, p, b" ");
        p = write_alias_opts(buf, p, fl);
        p = write_lit(buf, p, b" 0 0\n");
        if p >= buf.len() { return start; }
    }
    // The built-in mqueue tree (served at /dev/mqueue like /dev/pts) is a mount
    // too; an explicit `mount -t mqueue` at the same place is a no-op alias
    // and would otherwise leave /proc/mounts without it.
    let jailed = matches!(sched::current_root_arc().as_deref(), Some(r) if r.len() > 1);
    if !jailed {
        let start = p;
        p = write_lit(buf, p, b"mqueue /dev/mqueue mqueue rw,nosuid,nodev,noexec 0 0\n");
        if p >= buf.len() { return start; }
        // cgroup2, once mounted (the initial-namespace line only).
        if cgroupfs::mounted() {
            let start = p;
            p = write_lit(buf, p, b"cgroup2 /sys/fs/cgroup cgroup2 rw,nosuid,nodev,noexec,relatime 0 0\n");
            if p >= buf.len() { return start; }
        }
    }
    p
}

/// The alias mounts as the calling task sees them: mount points inside its
/// chroot with the root stripped; the rest omitted (as Linux hides mounts
/// outside a reader's root). Unjailed readers see them all.
fn alias_mounts_for_reader() -> Vec<(Vec<u8>, Vec<u8>, &'static str, Option<u32>, u8)> {
    let root = sched::current_root_arc();
    alias::list().into_iter().filter_map(|(dev, mp, fstype, sbi)| {
        let fl = alias::flags_at(&mp);
        match root.as_deref() {
            Some(r) if r.len() > 1 => {
                if !alias::under(&mp, r) { return None; }
                let v = if mp.len() == r.len() { b"/".to_vec() } else { mp[r.len()..].to_vec() };
                Some((dev, v, fstype, sbi, fl))
            }
            _ => Some((dev, mp, fstype, sbi, fl)),
        }
    }).collect()
}

/// Per-mount options of an ns-0 alias/bind mount: `ro`/`rw`, `nosuid`, then
/// `nodev`/`noexec` when set (Linux order).
fn write_alias_opts(buf: &mut [u8; TMP_BUF_SIZE], mut p: usize, fl: u8) -> usize {
    p = write_lit(buf, p, if fl & mnt::MNT_RDONLY != 0 { b"ro" } else { b"rw" });
    p = write_lit(buf, p, b",nosuid");
    if fl & mnt::MNT_NODEV != 0 { p = write_lit(buf, p, b",nodev"); }
    if fl & mnt::MNT_NOEXEC != 0 { p = write_lit(buf, p, b",noexec"); }
    p
}

/// ` shared:N`, ` master:N`, ` unbindable` for the ns-0 mount at `path`.
fn write_prop_fields(buf: &mut [u8; TMP_BUF_SIZE], mut p: usize, path: &[u8]) -> usize {
    let (g, m, u) = mnt::ns0_prop(path);
    if g != 0 { p = write_lit(buf, p, b" shared:"); p = write_u32(buf, p, g); }
    if m != 0 { p = write_lit(buf, p, b" master:"); p = write_u32(buf, p, m); }
    if u { p = write_lit(buf, p, b" unbindable"); }
    p
}

/// mountinfo (`info`) or mounts of mount namespace `ns`: its own tree when it
/// has one, else the initial namespace's tables.
fn gen_mount_view(ns: sched::ns::NsId, info: bool, buf: &mut [u8; TMP_BUF_SIZE]) -> usize {
    if ns != 0 {
        if let Some(v) = mnt::render_tree(ns, info) {
            // Drop a trailing partial line rather than truncating mid-field.
            let mut n = v.len().min(TMP_BUF_SIZE);
            if n < v.len() { while n > 0 && v[n - 1] != b'\n' { n -= 1; } }
            buf[..n].copy_from_slice(&v[..n]);
            return n;
        }
    }
    if info { write_mountinfo_lines(buf, 0) } else { write_mtab_lines(buf, 0) }
}

/// Namespace mountpoints of the initial namespace (see `ns0_snapshot`).
pub(crate) fn ns0_mountpoints() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = alloc::vec![b"/".to_vec()];
    for m in list_mounts().iter().filter(|m| m.in_use) { v.push(m.prefix.as_bytes().to_vec()); }
    for r in TMPFS_ROOTS.iter() { v.push(r.to_vec()); }
    for p in [&b"/proc"[..], b"/dev", b"/dev/pts", b"/sys"] { v.push(p.to_vec()); }
    for (mp, ..) in alias::entries() { v.push(mp); }
    for mp in alias::self_binds() { v.push(mp); }
    v.sort(); v.dedup();
    v
}

/// Append one `/proc/self/mountinfo`-format line per in-use mount:
/// `<id> <parent-id> <major>:<minor> <root> <mountpoint> <options> - <fstype> <source> <superoptions>`.
///
/// Field layout is dictated by uucore's `MountInfo::new()` `LINUX_MOUNTINFO`
/// arm (fsext.rs): it splits the line on spaces, scans fields[6..] for a
/// literal "-" separator, and reads `fs_type`/`dev_name` from the two fields
/// immediately after it, while `raw[3]`/`raw[4]`/`raw[5]` are root/mountpoint/
/// options. Emitting exactly zero optional fields (the "-" lands at index 6)
/// keeps that scan trivial. Bounded the same way as `write_mtab_lines`.
fn write_mountinfo_lines(buf: &mut [u8; TMP_BUF_SIZE], mut p: usize) -> usize {
    const ROOT_ID: u32 = 20;
    let mut mount_id: u32 = ROOT_ID;
    for m in list_mounts().iter() {
        if !m.in_use { continue; }
        let start = p;
        p = write_u32(buf, p, mount_id);
        p = write_lit(buf, p, b" ");
        p = write_u32(buf, p, ROOT_ID);
        p = write_lit(buf, p, b" ");
        let (maj, min) = dev_major_minor(m.dev);
        p = write_u32(buf, p, maj);
        p = write_lit(buf, p, b":");
        p = write_u32(buf, p, min);
        p = write_lit(buf, p, b" / ");
        p = write_lit(buf, p, m.prefix.as_bytes());
        p = write_lit(buf, p, b" ");
        p = write_lit(buf, p, mount_opts(m));
        p = write_lit(buf, p, b",relatime");
        p = write_prop_fields(buf, p, m.prefix.as_bytes());
        p = write_lit(buf, p, b" - ");
        p = write_lit(buf, p, m.fstype.as_bytes());
        p = write_lit(buf, p, b" ");
        p = write_lit(buf, p, m.device.as_bytes());
        p = write_lit(buf, p, b" rw\n");
        if p >= buf.len() { return start; }
        mount_id += 1;
    }
    // tmpfs roots — see write_mtab_lines. Each gets its own minor so `df`
    // (which de-duplicates mounts by device number) lists all of them.
    for (ri, root) in TMPFS_ROOTS.iter().enumerate() {
        let start = p;
        p = write_u32(buf, p, mount_id);
        p = write_lit(buf, p, b" 20 0:");
        p = write_u32(buf, p, dev_major_minor(tmpfs_root_dev(ri)).1);
        p = write_lit(buf, p, b" / ");
        p = write_lit(buf, p, root);
        p = write_lit(buf, p, b" ");
        p = write_lit(buf, p, tmpfs_opts(root));
        p = write_lit(buf, p, b",relatime");
        p = write_prop_fields(buf, p, root);
        p = write_lit(buf, p, b" - tmpfs tmpfs rw\n");
        if p >= buf.len() { return start; }
        mount_id += 1;
    }
    for (dev, mp, fstype, sbi, fl) in alias_mounts_for_reader() {
        let start = p;
        p = write_u32(buf, p, mount_id);
        p = write_lit(buf, p, b" 20 ");
        // A tmpfs instance shows its superblock's device number (as Linux
        // does); other aliases keep the synthetic `0:<mount id>`.
        match sbi.and_then(mnt::sb_dev) {
            Some(d) => { let (ma, mi) = dev_major_minor(d as u64); p = write_u32(buf, p, ma); p = write_lit(buf, p, b":"); p = write_u32(buf, p, mi); }
            None => { p = write_lit(buf, p, b"0:"); p = write_u32(buf, p, mount_id); }
        }
        p = write_lit(buf, p, b" / ");
        p = write_lit(buf, p, &mp);
        p = write_lit(buf, p, b" ");
        p = write_alias_opts(buf, p, fl);
        p = write_lit(buf, p, b",relatime");
        p = write_prop_fields(buf, p, &mp);
        p = write_lit(buf, p, b" - ");
        p = write_lit(buf, p, fstype.as_bytes());
        p = write_lit(buf, p, b" ");
        p = write_lit(buf, p, &dev);
        p = write_lit(buf, p, b" rw\n");
        if p >= buf.len() { return start; }
        mount_id += 1;
    }
    // cgroup2, once mounted (the initial-namespace line only).
    if cgroupfs::mounted() && !matches!(sched::current_root_arc().as_deref(), Some(r) if r.len() > 1) {
        let start = p;
        p = write_u32(buf, p, mount_id);
        p = write_lit(buf, p, b" 1 0:");
        p = write_u32(buf, p, mount_id);
        p = write_lit(buf, p, b" / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime - cgroup2 cgroup2 rw\n");
        if p >= buf.len() { return start; }
    }
    p
}

/// Generate `/etc/mtab` content from `list_mounts()`. Allocates an ephemeral
/// tmpfs slot the same way `gen_proc_system`/`gen_proc_self` do, since
/// `/etc/mtab` is not under `/proc` and so isn't routed through either of
/// those.
fn gen_etc_mtab() -> Option<VnodeKind> {
    let mut buf = [0u8; TMP_BUF_SIZE];
    let len = write_mtab_lines(&mut buf, 0);

    let mut tmp = TMP_FILES.lock();
    let idx = tmp_free_slot(&mut *tmp)?;
    tmp.claim(idx, b"", true);
    // Unique synthetic path "/tmp/.mtab_<idx>" — never conflicts with user files.
    let mut fake_path = [0u8; 20];
    let base = b"/tmp/.mtab_";
    fake_path[..base.len()].copy_from_slice(base);
    let mut fpl = base.len();
    let mut n = idx;
    if n == 0 { fake_path[fpl] = b'0'; fpl += 1; }
    else {
        let mut digits = [0u8; 5]; let mut di = 0;
        while n > 0 { digits[di] = b'0' + (n % 10) as u8; di += 1; n /= 10; }
        for i in (0..di).rev() { fake_path[fpl] = digits[i]; fpl += 1; }
    }
    let fp_len = fpl.min(MAX_TMP_PATH - 1);
    tmp.set_path(idx, &fake_path[..fp_len]);
    let copy = len.min(TMP_BUF_SIZE);
    if !tmp_fill_kernel(&mut tmp, idx, &buf[..copy]) { tmp.release(idx); return None; }
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: false, ofd: 0 })
}

/// IPv4 name servers from the DHCP lease (servers/net), in the server's
/// order; none until a lease arrives.
static DHCP_DNS: Mutex<([[u8; 4]; 3], usize)> = Mutex::new(([[0; 4]; 3], 0));

/// Record the name servers a DHCP lease named; `/etc/resolv.conf` lists
/// them from then on. Called by the net server.
pub fn set_dhcp_dns(servers: &[[u8; 4]]) {
    let mut d = DHCP_DNS.lock();
    let n = servers.len().min(3);
    d.0[..n].copy_from_slice(&servers[..n]);
    d.1 = n;
}

/// The net server's `/proc/net/<name>` generator (`net_server::procnet`),
/// registered at its init; 0 until then. A fn pointer, not a call, because
/// the net server depends on this crate. It takes only net-server locks and
/// is called with no VFS lock held.
static PROC_NET_GEN: atomic::AtomicUsize = atomic::AtomicUsize::new(0);

/// Register the `/proc/net/*` generator: `f(name, out)` appends the file's
/// bytes for `name` (the part after "/proc/net/") and returns false for a
/// name it does not know.
pub fn set_proc_net_gen(f: fn(&[u8], &mut alloc::vec::Vec<u8>) -> bool) {
    PROC_NET_GEN.store(f as usize, atomic::Ordering::Release);
}

/// A snapshot of a live `/proc/net/*` file, or None (not under /proc/net,
/// no generator yet, or a name it does not produce — the static table then
/// answers, e.g. if_inet6).
fn proc_net_open(path: &[u8]) -> Option<VnodeKind> {
    let name = path.strip_prefix(b"/proc/net/")?;
    let f = PROC_NET_GEN.load(atomic::Ordering::Acquire);
    if f == 0 { return None; }
    let f: fn(&[u8], &mut alloc::vec::Vec<u8>) -> bool = unsafe { core::mem::transmute(f) };
    let mut out = alloc::vec::Vec::new();
    if !f(name, &mut out) { return None; }
    proc_snapshot(b"/tmp/.procnet", &out, false)
}

/// Generate dynamic /proc/ system-wide entries (meminfo, uptime, loadavg, stat).
fn gen_proc_system(path: &[u8], root: bool) -> Option<VnodeKind> {
    let mut buf = [0u8; TMP_BUF_SIZE];
    let len = gen_proc_system_content(path, root, &mut buf)?;
    gen_proc_system_file(&buf, len)
}

/// `/proc/sys/kernel/{hostname,domainname}` as seen from `pid`'s UTS namespace.
fn gen_proc_uts(pid: u32, domain: bool) -> Option<VnodeKind> {
    let (h, d) = sched::ns::uts_names(sched::ns::utsns_of(pid));
    let (b, l) = if domain { d } else { h };
    let mut buf = [0u8; TMP_BUF_SIZE];
    buf[..l].copy_from_slice(&b[..l]);
    buf[l] = b'\n';
    gen_proc_system_file(&buf, l + 1)
}

fn gen_proc_system_file(buf: &[u8; TMP_BUF_SIZE], len: usize) -> Option<VnodeKind> {
    let mut tmp = TMP_FILES.lock();
    let idx = tmp_free_slot(&mut *tmp)?;
    tmp.claim(idx, b"", true);
    // Unique synthetic path "/tmp/.psys_<idx>".
    let mut fake_path = [0u8; 20];
    let base = b"/tmp/.psys_";
    fake_path[..base.len()].copy_from_slice(base);
    let mut fpl = base.len();
    let mut n2 = idx;
    if n2 == 0 { fake_path[fpl] = b'0'; fpl += 1; }
    else {
        let mut digits = [0u8; 5]; let mut di = 0;
        while n2 > 0 { digits[di] = b'0' + (n2 % 10) as u8; di += 1; n2 /= 10; }
        for i in (0..di).rev() { fake_path[fpl] = digits[i]; fpl += 1; }
    }
    let fl = fpl.min(MAX_TMP_PATH - 1);
    PSYS_ORIGIN.lock().retain(|e| e.0 != idx);
    tmp.set_path(idx, &fake_path[..fl]);
    let copy = len.min(TMP_BUF_SIZE);
    if !tmp_fill_kernel(&mut tmp, idx, &buf[..copy]) { tmp.release(idx); return None; }
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: false, ofd: 0 })
}

/// `/proc/kmemstat`: where the kernel's pages are, for leak hunting.
///
/// Three tables, all read without locks: buddy pages live per allocation
/// site (`mm::buddy` charges every block to its caller's file:line), slab
/// pages and live objects per size class, and live heap objects per exact
/// size. Too large for the 512-byte `gen_proc_system` buffer, so it is
/// formatted into a pre-sized heap buffer and parked in a tmpfs slot.
fn gen_kmemstat() -> Option<VnodeKind> {
    // The census callbacks run with allocator-internal state held, so nothing
    // may allocate inside them: `W` writes into a buffer sized up front and
    // silently stops at its end, exactly as the old fixed 32 KiB slot did.
    const KMEMSTAT_MAX: usize = 128 * 1024;
    struct W { buf: alloc::vec::Vec<u8>, p: usize }
    impl W {
        fn s(&mut self, t: &str) {
            for &b in t.as_bytes() { if self.p < self.buf.len() { self.buf[self.p] = b; self.p += 1; } }
        }
        fn i(&mut self, v: isize) {
            if v < 0 { self.s("-"); }
            let mut v = v.unsigned_abs();
            let mut d = [0u8; 20]; let mut n = d.len();
            loop { n -= 1; d[n] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
            for k in n..d.len() { if self.p < self.buf.len() { self.buf[self.p] = d[k]; self.p += 1; } }
        }
    }
    let mut w = W { buf: alloc::vec![0u8; KMEMSTAT_MAX], p: 0 };
    w.s("total_pages "); w.i(mm::buddy::total_pages() as isize);
    w.s("\nfree_pages "); w.i(mm::buddy::free_pages() as isize);
    w.s("\nrefused_frees "); w.i(mm::buddy::bad_frees() as isize);
    w.s("\ntmpfs_pages "); w.i(tmpfs_used_pages() as isize);
    w.s("\ntmpfs_budget_pages "); w.i(tmpfs_budget_pages() as isize);
    let (hits, fills, reclaimed) = mm::pagecache::legacy_stats();
    w.s("\npagecache_pages "); w.i(mm::pagecache::cached_pages() as isize);
    w.s("\npagecache_hits_fills_reclaimed "); w.i(hits as isize);
    w.s(" "); w.i(fills as isize); w.s(" "); w.i(reclaimed as isize);
    w.s("\nfields site file:line live_pages peak_pages\n");
    let mut sum = 0isize;
    mm::buddy::site_census(&mut |f, l, live, peak| {
        sum += live;
        w.s("site "); w.s(f); w.s(":"); w.i(l as isize);
        w.s(" "); w.i(live); w.s(" "); w.i(peak); w.s("\n");
    });
    w.s("site_sum "); w.i(sum);
    w.s("\nslab_reclaimed_pages "); w.i(mm::slab::reclaimed_pages() as isize);
    w.s("\nfields slab class_bytes pages live_objs\n");
    mm::slab::class_census(&mut |c, pages, live| {
        w.s("slab "); w.i(c as isize); w.s(" "); w.i(pages as isize); w.s(" "); w.i(live); w.s("\n");
    });
    w.s("fields heap size_bytes live_objs (small: 8-byte bucket upper bound; large: pages*4096)\n");
    mm::slab::size_census(&mut |sz, n| {
        w.s("heap "); w.i(sz as isize); w.s(" "); w.i(n); w.s("\n");
    });
    // Per-process resident pages by bucket, after the allocator tables (no
    // allocator state is held here). File pages are private copies of file
    // data: the same library page mapped by two processes is two frames.
    w.s("fields proc pid anon_pages file_pages shmem_pages exe\n");
    let mut pids = [0u32; 1024];
    let n = sched::process_pids_from(1, &mut pids);
    let (mut ta, mut tf, mut ts) = (0isize, 0isize, 0isize);
    for &pid in &pids[..n] {
        let c = match sched::proc_mem_of(pid) { Some((c, _)) => c, None => continue };
        ta += c.rss_anon as isize; tf += c.rss_file as isize; ts += c.rss_shmem as isize;
        w.s("proc "); w.i(pid as isize);
        w.s(" "); w.i(c.rss_anon as isize);
        w.s(" "); w.i(c.rss_file as isize);
        w.s(" "); w.i(c.rss_shmem as isize);
        let mut base = [0u8; NAME_MAX];
        let bl = sched::exe_basename(pid, &mut base).unwrap_or(0);
        w.s(" "); w.s(core::str::from_utf8(&base[..bl]).unwrap_or("?")); w.s("\n");
    }
    w.s("proc_sum "); w.i(ta); w.s(" "); w.i(tf); w.s(" "); w.i(ts); w.s("\n");
    let len = w.p;
    proc_snapshot(b"/tmp/.kmemstat", &w.buf[..len], false)
}

/// `/proc/lockstat`: per tracked kernel lock (`sched::lockwatch`), one line
/// `name acquisitions contended spin_us` of counts since boot. Diff two reads
/// around a workload to see what it contends on.
fn gen_lockstat() -> Option<VnodeKind> {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    let _ = writeln!(s, "lock acquisitions contended spin_us");
    for id in 1..sched::lockwatch::N_LOCKS as u8 {
        let (a, c, ns) = sched::lockwatch::lock_stats(id);
        let _ = writeln!(s, "{} {} {} {}", sched::lockwatch::name(id), a, c, ns / 1000);
    }
    // Held spin::Mutex count summed over CPUs, then the voluntary switches
    // and sleeps made with one held (lane ipcregress: a leaked count made a
    // CPU look atomic for good, so every wait on it spun).
    let _ = writeln!(s, "spin_held {} {} {}", sched::spinheld::held_total(),
                     sched::spinheld::switch_violations(), sched::irqctx::sleep_violations());
    proc_snapshot(b"/tmp/.lockstat", s.as_bytes(), false)
}

/// The epoll wait syscalls (`epoll_wait`, `epoll_pwait`, `epoll_pwait2`),
/// summed into `/proc/sysstat`'s `e` lines.
#[cfg(target_arch = "x86_64")]
const EPOLL_WAIT_NRS: [u32; 3] = [232, 281, 441];
#[cfg(target_arch = "aarch64")]
const EPOLL_WAIT_NRS: [u32; 2] = [22, 441];

/// `/proc/sysstat` (lane deskbusy2): where processes spend CPU time.
/// `t pid tgid cpu_ns sys_ns` per live thread (total on-CPU time and the part
/// inside syscalls), then `w pid tgid state sc port listen poll_mask in_sc_ns`
/// per live thread (where it waits now: state R/C/S/T/Z, the syscall it was
/// switched out in, the IPC port it is blocked on, parked on the poll
/// listener, its poll mask, how long it has been inside that syscall, poll
/// wakes; a task parked in a futex wait also gets ` fut=0x.. fval=0x..|?
/// rip=0x.. rsp=0x..` appended), then
/// `s tgid nr calls ns zero wall` per (thread group, syscall number): calls,
/// on-CPU kernel ns, returns of 0 and entry-to-exit wall ns of the completed
/// calls (`sched::sysacct`), then `e tgid calls oncpu_ns wall_ns blocked_ns
/// zero` per thread group: the epoll wait syscalls summed. All cumulative
/// except `w`; diff two reads.
fn gen_sysstat() -> Option<VnodeKind> {
    use core::fmt::Write;
    let mut tasks = alloc::vec::Vec::with_capacity(4096);
    sched::task_times(&mut tasks);
    let mut s = alloc::string::String::new();
    // Lane mmlock: longest IRQ-masked stretch on a busy CPU since the last
    // read (reset by reading), teardown reschedule points, /proc observers
    // that parked or gave up on a busy address space, cross-process copies.
    {
        use core::sync::atomic::Ordering::Relaxed;
        let _ = writeln!(s, "# mm late_tick_max_ms {} teardown_points {} observer_parked {} observer_gaveup {} foreign_copies {} as_hold_max_us {}",
                         sched::take_late_tick_max_ms(),
                         mm::vmm::TEARDOWN_RESCHED_POINTS.load(Relaxed),
                         sched::FOREIGN_AS_PARKED.load(Relaxed),
                         sched::FOREIGN_AS_GAVE_UP.load(Relaxed),
                         sched::FOREIGN_TASK_COPIES.load(Relaxed),
                         sched::take_as_hold_max_us());
    }
    // Console UART RX health (openbugs #101): longest RX-unserviced stretch
    // of a console write, longest cpu0 tick lateness, bytes pulled out of the
    // RX FIFO during IRQ-masked sections, hardware overruns. aarch64 only.
    {
        extern "C" { fn kernel_uart_rx_stats(out: *mut u64); }
        let mut u = [0u64; 4];
        unsafe { kernel_uart_rx_stats(u.as_mut_ptr()); }
        let _ = writeln!(s, "# uart txgap_us {} irqlate_us {} rescued {} overruns {}", u[0], u[1], u[2], u[3]);
    }
    let _ = writeln!(s, "# t pid tgid cpu_ns sys_ns");
    for (pid, tgid, c, y) in tasks {
        let _ = writeln!(s, "t {} {} {} {}", pid, tgid, c, y);
    }
    let mut waits = alloc::vec::Vec::with_capacity(4096);
    sched::task_waits(&mut waits);
    let _ = writeln!(s, "# w pid tgid state sc port listen poll_mask in_sc_ns pwakes");
    for w in waits {
        let sc = if w.sc == sched::NO_SYSCALL { -1 } else { w.sc as i64 };
        let _ = writeln!(s, "w {} {} {} {} {:#x} {} {:#x} {} {}", w.pid, w.tgid, w.state as char, sc,
                         w.port, w.listen as u8, w.poll_mask, w.in_sc_ns, w.poll_wakes);
        if w.fut != 0 {
            // Appended for futex waiters only (lane futwhy), after `pwakes`.
            s.pop(); // the newline
            let _ = write!(s, " fut={:#x}", w.fut);
            if w.fval == u64::MAX { s.push_str(" fval=?"); } else { let _ = write!(s, " fval={:#x}", w.fval); }
            let _ = writeln!(s, " rip={:#x} rsp={:#x}", w.urip, w.ursp);
        }
    }
    let _ = writeln!(s, "# s tgid nr calls ns zero wall");
    // (tgid, calls, ns, wall, zero) of the epoll waits, for the `e` lines.
    let mut ep: alloc::vec::Vec<(u32, u64, u64, u64, u64)> = alloc::vec::Vec::new();
    sched::sysacct::for_each(|tg, nr, n, ns, z, wall| {
        let _ = writeln!(s, "s {} {} {} {} {} {}", tg, nr, n, ns, z, wall);
        if EPOLL_WAIT_NRS.contains(&nr) {
            match ep.iter_mut().find(|e| e.0 == tg) {
                Some(e) => { e.1 += n; e.2 += ns; e.3 += wall; e.4 += z; }
                None => ep.push((tg, n, ns, wall, z)),
            }
        }
    });
    let _ = writeln!(s, "# e tgid calls oncpu_ns wall_ns blocked_ns zero");
    for (tg, n, ns, wall, z) in ep {
        let _ = writeln!(s, "e {} {} {} {} {} {}", tg, n, ns, wall, wall.saturating_sub(ns), z);
    }
    {
        use sched::sysacct as sa;
        let l = |a: &core::sync::atomic::AtomicU64| a.load(core::sync::atomic::Ordering::Relaxed);
        let _ = writeln!(s, "# f fence_waits spin_hits spin_ns sleep_ns spin_max_ns spin_us");
        let us = l(&sa::FENCE_SPIN_US);
        let _ = writeln!(s, "f {} {} {} {} {} {}", l(&sa::FENCE_WAITS), l(&sa::FENCE_SPIN_HITS), l(&sa::FENCE_SPIN_NS),
                         l(&sa::FENCE_SLEEP_NS), l(&sa::FENCE_SPIN_MAX_NS), if us == u64::MAX { -1 } else { us as i64 });
    }
    let _ = writeln!(s, "# overflow {}", sched::sysacct::OVERFLOW.load(core::sync::atomic::Ordering::Relaxed));
    proc_snapshot(b"/tmp/.sysstat", s.as_bytes(), false)
}

/// `/proc/mntselftest`: `case: PASS|FAIL` rows from the mount-tree selftest
/// (walk/reverse/copy/registry on a synthetic tree), plus the live counters
/// `mnt_ns_live=`, `sb_count=`.
fn gen_mntselftest() -> Option<VnodeKind> {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    for (name, ok) in mnt::selftest() { let _ = writeln!(s, "{}: {}", name, if ok { "PASS" } else { "FAIL" }); }
    // mntiso-strict (D1): inside a mount namespace a tmpfs walk that meets a link in
    // a ns-resolved call must fail closed. The fixture link `/tmp/mntiso-strict-l`
    // (-> /tmp) is created by mnttest; absent fixture or ns 0 = no row.
    if sched::current_mntns() != 0 {
        let fixture = { let t = TMP_FILES.lock(); tmp_find(&t, b"/tmp/mntiso-strict-l").map_or(false, |i| t[i].is_link) };
        if fixture {
            let was = sched::set_strict_links(true);
            let r = tmp_resolve_links(b"/tmp/mntiso-strict-l/x", true);
            let rn = tmp_resolve_links(b"/tmp/mntiso-strict-l", false);
            sched::set_strict_links(was);
            let ok = matches!(r, Err(e) if e == VFS_ELINKRACE) && rn.is_ok();
            let _ = writeln!(s, "strict_tmp_link_refused: {}", if ok { "PASS" } else { "FAIL" });
        }
    }
    let _ = writeln!(s, "bindpin_checks={} bindpin_stale={}", BINDPIN_STATS_CHECKS.load(core::sync::atomic::Ordering::Relaxed), BINDPIN_STATS_STALE.load(core::sync::atomic::Ordering::Relaxed));
    let _ = writeln!(s, "ns0_mrefs={}", mnt::ns0::mrefs_len());
    let _ = writeln!(s, "ns_live={} ns_dying={} sb_count={}", sched::ns::live_count(), sched::ns::dying_count(), mnt::sb_count());
    let _ = write!(s, "sb_devs=");
    for d in mnt::sb_devs() { let _ = write!(s, "{},", d); }
    let _ = writeln!(s);
    proc_snapshot(b"/tmp/.mntselftest", s.as_bytes(), false)
}

/// Interim tmpfs instances for mount namespaces (lane M1; lane M5 replaces
/// this with real per-superblock roots). An instance is a directory of the
/// tmpfs pool under the hidden, root-owned backing prefix
/// `/.leandros-sb/tmpfs/<n>`: no namespace path reaches the prefix (resolve_ns
/// returns ENOENT, getdents hides it, a link hop into it from outside is
/// ENOENT, and the mount-tree walk refuses it unless the mount root is itself
/// there). The parents are created at boot, root-owned 0711; each instance
/// directory is created exclusively (EEXIST if anything is there), 0700,
/// owned by the creating uid.
const TMPFS_INST_PARENT: &[u8] = b"/.leandros-sb";
const TMPFS_INST_DIR: &[u8] = b"/.leandros-sb/tmpfs";

/// Create the directory `path` exclusively: fails with EEXIST if any entry is
/// there, whatever it is (an entry we did not create is never adopted).
fn tmpfs_inst_mkdir(tmp: &mut TmpPool, path: &[u8], uid: u32, gid: u32, mode: u16) -> Result<(), i32> {
    if tmp_find(tmp, path).is_some() { return Err(-17); }
    let idx = tmp_free_slot(tmp).ok_or(-28)?;
    tmp.claim(idx, path, false);
    tmp[idx].is_dir = true;
    tmp[idx].uid = uid;
    tmp[idx].gid = gid;
    tmp_init_created(tmp, idx, path, mode, 0, true);
    Ok(())
}

/// Ensure the root-owned parents exist. An existing entry is accepted only
/// if it is a directory owned by root:root with mode 0711 (our own).
fn tmpfs_inst_parents(tmp: &mut TmpPool) -> Result<(), i32> {
    for p in [TMPFS_INST_PARENT, TMPFS_INST_DIR] {
        match tmp_find(tmp, p) {
            None => tmpfs_inst_mkdir(tmp, p, 0, 0, 0o711)?,
            Some(i) => {
                let e = &tmp[i];
                if !e.is_dir || e.uid != 0 || e.gid != 0 || e.is_link { return Err(-17); }
            }
        }
    }
    Ok(())
}

fn tmpfs_inst_root(n: u32) -> Vec<u8> {
    let mut root = TMPFS_INST_DIR.to_vec();
    root.push(b'/');
    let mut d = [0u8; 10]; let mut k = 0; let mut v = n;
    loop { d[k] = b'0' + (v % 10) as u8; k += 1; v /= 10; if v == 0 { break; } }
    for i in (0..k).rev() { root.push(d[i]); }
    root
}

/// Per-instance attributes of the tmpfs mounts made by mount(2):
/// (superblock id, MS_NOSUID|NODEV|NOEXEC bits, size limit in pages, 0 = none).
static TMPFS_INST_ATTR: Mutex<Vec<(u32, u8, usize)>> = Mutex::new(Vec::new());

fn tmpfs_inst_attr(n: u32) -> (u8, usize) {
    TMPFS_INST_ATTR.lock().iter().find(|e| e.0 == n).map_or((0, 0), |e| (e.1, e.2))
}

/// Record the mount flags (MS_* bits) and size limit of instance `n`.
pub fn tmpfs_instance_set_attr(n: u32, ms_flags: usize, size_pages: usize) {
    let f = (ms_flags & (TMPFS_MS_NOSUID | TMPFS_MS_NODEV | TMPFS_MS_NOEXEC) as usize) as u8;
    let mut t = TMPFS_INST_ATTR.lock();
    t.retain(|e| e.0 != n);
    if f != 0 || size_pages != 0 { t.push((n, f, size_pages)); }
}

/// Options of a tmpfs mount(2) data string.
#[derive(Default, Clone, Copy)]
pub struct TmpfsOpts {
    pub mode: Option<u16>,
    /// uid/gid as the caller names them (the kernel translates).
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// Size limit in pages, 0 = default (the global tmpfs budget).
    pub size_pages: usize,
}

/// Parse `mode=,uid=,gid=,size=` (size: bytes, k/m/g suffix, or N% of RAM).
/// `nr_inodes=` and unknown options are accepted and ignored. EINVAL on a
/// malformed value.
pub fn parse_tmpfs_opts(data: &[u8]) -> Result<TmpfsOpts, i32> {
    fn num(v: &[u8], radix: u32) -> Option<u64> {
        if v.is_empty() { return None; }
        u64::from_str_radix(core::str::from_utf8(v).ok()?, radix).ok()
    }
    let mut o = TmpfsOpts::default();
    for opt in data.split(|&c| c == b',') {
        if opt.is_empty() { continue; }
        let (k, v) = match opt.iter().position(|&c| c == b'=') {
            Some(i) => (&opt[..i], &opt[i + 1..]),
            None => continue,
        };
        match k {
            b"mode" => o.mode = Some((num(v, 8).ok_or(-22)? & 0o7777) as u16),
            b"uid" => o.uid = Some(num(v, 10).ok_or(-22)? as u32),
            b"gid" => o.gid = Some(num(v, 10).ok_or(-22)? as u32),
            b"size" => {
                let pages = if let Some(pc) = v.strip_suffix(b"%".as_slice()) {
                    let p = num(pc, 10).ok_or(-22)?.min(100) as usize;
                    mm::buddy::total_pages() * p / 100
                } else {
                    let (digits, mult) = match v.last() {
                        Some(b'k') | Some(b'K') => (&v[..v.len() - 1], 1u64 << 10),
                        Some(b'm') | Some(b'M') => (&v[..v.len() - 1], 1u64 << 20),
                        Some(b'g') | Some(b'G') => (&v[..v.len() - 1], 1u64 << 30),
                        _ => (v, 1u64),
                    };
                    let bytes = num(digits, 10).ok_or(-22)?.saturating_mul(mult);
                    ((bytes + 4095) / 4096) as usize
                };
                // size=0 means unlimited on Linux.
                o.size_pages = pages;
            }
            _ => {}
        }
    }
    Ok(o)
}

/// Create tmpfs instance `n` (the superblock id) owned by `uid`:`gid` with
/// permission bits `mode`; returns its backing root.
pub fn tmpfs_instance_create(n: u32, uid: u32, gid: u32, mode: u16) -> Result<Vec<u8>, i32> {
    let root = tmpfs_inst_root(n);
    let mut tmp = TMP_FILES.lock();
    tmpfs_inst_parents(&mut tmp)?;
    tmpfs_inst_mkdir(&mut tmp, &root, uid, gid, mode)?;
    Ok(root)
}

/// Pages resident in instance `n` (every file below its root).
fn tmpfs_inst_used_pages(tmp: &TmpPool, vmos: &[Option<TmpVmo>], n: u32) -> usize {
    let root = tmpfs_inst_root(n);
    let Some(top) = tmp_find(tmp, &root) else { return 0 };
    let mut used = 0;
    for idx in tmp.subtree(top) {
        if let Some(v) = vmos[idx].as_ref() { if !v.borrowed { used += v.resident_pages(); } }
    }
    used
}

/// Superblock destroy hook: free the tmpfs instance of the superblock whose
/// registry root is `sb_root` (`/.leandros-sb/tmpfs/<n>`) and everything
/// below it (its last mount is gone). A file an fd still holds survives
/// nameless.
pub fn tmpfs_instance_destroy(sb_root: &[u8]) {
    let name = match sb_root.iter().rposition(|&c| c == b'/') { Some(i) => &sb_root[i + 1..], None => return };
    let n: u32 = match core::str::from_utf8(name).ok().and_then(|x| x.parse().ok()) { Some(n) => n, None => return };
    TMPFS_INST_ATTR.lock().retain(|e| e.0 != n);
    let root_buf = tmpfs_inst_root(n);
    let root = &root_buf[..];
    let mut objs: Vec<u32> = Vec::new();
    {
        let mut tmp = TMP_FILES.lock();
        let Some(top) = tmp_find(&tmp, root) else { return };
        let mut all = tmp.subtree(top);
        all.reverse(); // children before parents
        all.push(top);
        for idx in all {
            if !tmp[idx].in_use() { continue; }
            if let Some(o) = tmp_drop_name(&mut tmp, idx, TmpOpen) { objs.push(o); }
        }
    }
    for o in objs { dmabuf_release(o); }
}

/// The initial namespace's mount table as a tree, for copying into a new
/// mount namespace: MOUNTS, the built-in tmpfs roots, the synthesized
/// `/proc` `/dev` `/dev/pts` `/sys`, and the ns-0 alias/bind mounts, nested
/// by namespace path. Every mount is private; superblock 0 (not registered).
pub(crate) fn ns0_snapshot() -> Vec<mnt::Mnt> {
    use alloc::boxed::Box;
    // (ns mountpoint, backing root, fstype, device, flags)
    let mut raw: Vec<(Vec<u8>, Vec<u8>, &'static str, Vec<u8>, u8)> = Vec::new();
    let mut have_root = false;
    for m in list_mounts().iter().filter(|m| m.in_use) {
        if m.prefix == "/" { have_root = true; }
        let mut fl = mnt::MNT_RELATIME;
        if m.nosuid { fl |= mnt::MNT_NOSUID; }
        if m.rdonly { fl |= mnt::MNT_RDONLY; }
        raw.push((m.prefix.as_bytes().to_vec(), m.prefix.as_bytes().to_vec(), m.fstype, m.device.as_bytes().to_vec(), fl));
    }
    if !have_root { raw.insert(0, (b"/".to_vec(), b"/".to_vec(), "rootfs", b"rootfs".to_vec(), mnt::MNT_RELATIME)); }
    for r in TMPFS_ROOTS.iter() { raw.push((r.to_vec(), r.to_vec(), "tmpfs", b"tmpfs".to_vec(), mnt::MNT_RELATIME)); }
    for (p, fs) in [(&b"/proc"[..], "proc"), (b"/dev", "devtmpfs"), (b"/dev/pts", "devpts"), (b"/sys", "sysfs")] {
        raw.push((p.to_vec(), p.to_vec(), fs, fs.as_bytes().to_vec(), mnt::MNT_RELATIME));
    }
    let mut nssb: Vec<(Vec<u8>, mnt::SbId)> = Vec::new();
    for (mp, src, fs, dev) in alias::entries() {
        // A bound nsfs file: the copy takes its own namespace reference,
        // owned by a superblock of its own (dropped with the last copy).
        let sb = match alias::ns_src(&src) {
            Some((_, id)) => { sched::ns::hold_raw(id); mnt::nsfs_sb_new(id) }
            None => 0,
        };
        if sb != 0 { nssb.push((mp.clone(), sb)); }
        let xf = alias::flags_at(&mp);
        raw.push((mp, src, fs, dev, mnt::MNT_NOSUID | mnt::MNT_RELATIME | xf));
    }
    // Self-binds (`mount --bind /run/netns /run/netns`) are mount points too.
    for mp in alias::self_binds() {
        let xf = alias::flags_at(&mp);
        raw.push((mp.clone(), mp, "none", b"none".to_vec(), mnt::MNT_NOSUID | mnt::MNT_RELATIME | xf));
    }
    // Parents: the deepest earlier-or-equal mountpoint covering ours (the
    // root first, so "/" is its own tree root and later duplicates overmount).
    raw.sort_by_key(|r| r.0.len());
    let ids: Vec<u32> = raw.iter().map(|_| mnt::alloc_mnt_id()).collect();
    let mut out: Vec<mnt::Mnt> = Vec::new();
    for (i, r) in raw.iter().enumerate() {
        let parent = (0..i).filter(|&j| alias::under(&r.0, &raw[j].0)).max_by_key(|&j| (raw[j].0.len(), j));
        let (pid, rel): (u32, Vec<u8>) = match parent {
            None => (0, b"/".to_vec()),
            Some(j) => {
                let pm = &raw[j].0;
                let rest = if pm == b"/" { &r.0[..] } else { &r.0[pm.len()..] };
                (ids[j], if rest.is_empty() { b"/".to_vec() } else { rest.to_vec() })
            }
        };
        let pg = mnt::ns0_prop(&r.0);
        let mut m = mnt::Mnt { id: ids[i], parent: pid, mp: rel.into_boxed_slice(), root: r.1.clone().into_boxed_slice(),
            sb: mnt::ns0::tmpfs_inst_of(&r.1).or_else(|| devpts::inst_of_backing(&r.1))
                .or_else(|| if alias::ns_src(&r.1).is_some() { nssb.iter().find(|e| e.0 == r.0).map(|e| e.1) } else { None }).unwrap_or(0), flags: r.4, locked: 0, group: pg.0, master: pg.1, unbindable: pg.2, fstype: r.2,
            device: r.3.clone().into_boxed_slice(), opts: Box::from(&b""[..]), pin: None, vgen: 0 };
        if pid == 0 && i != 0 { m.parent = ids[0]; m.mp = r.0.clone().into_boxed_slice(); }
        out.push(m);
    }
    out
}

/// `/proc/tryboot`: `state=off|armed|held|confirmed deadline_s= left_s= holds=`.
fn gen_tryboot() -> Option<VnodeKind> {
    use core::fmt::Write;
    use core::sync::atomic::Ordering::Acquire;
    use sched::tryboot_state as t;
    let now = sched::monotonic_ns();
    let (state, dl) = if !t::ACTIVE.load(Acquire) { ("off", 0) } else {
        let dl = t::DEADLINE_S.load(Acquire);
        (if t::CONFIRMED.load(Acquire) { "confirmed" }
         else if t::HOLD_UNTIL.load(Acquire) > now { "held" } else { "armed" }, dl)
    };
    let left = if state == "off" || state == "confirmed" { 0 } else { dl.saturating_sub(now / 1_000_000_000) };
    let mut s = alloc::string::String::new();
    let _ = writeln!(s, "state={} deadline_s={} left_s={} holds={}", state, dl, left, t::HOLDS.load(Acquire));
    proc_snapshot(b"/tmp/.tryboot", s.as_bytes(), false)
}

/// `/proc/rqhold` (lane deskbusy2): RUN_QUEUE holds per acquire site,
/// `file:line holds ns_held max_ns ns_spun` (time spent spinning for the lock
/// before that acquire), cumulative since boot — diff two reads.
fn gen_rqhold() -> Option<VnodeKind> {
    use core::fmt::Write;
    let mut rows: alloc::vec::Vec<(&'static str, u32, u64, u64, u64, u64)> = alloc::vec::Vec::with_capacity(512);
    sched::lockwatch::for_each_hold_site(|f, l, n, ns, mx, sp| if rows.len() < rows.capacity() { rows.push((f, l, n, ns, mx, sp)) });
    rows.sort_by(|a, b| b.3.cmp(&a.3));
    let mut s = alloc::string::String::new();
    let _ = writeln!(s, "# site holds ns_held max_ns ns_spun");
    for (f, l, n, ns, mx, sp) in rows { let _ = writeln!(s, "{}:{} {} {} {} {}", f, l, n, ns, mx, sp); }
    let _ = writeln!(s, "# overflow {}", sched::lockwatch::HS_OVERFLOW.load(core::sync::atomic::Ordering::Relaxed));
    // Lane sigfast: signal checks that skipped RUN_QUEUE vs took it (syscall
    // returns, has_deliverable_signal), and sampled hint misses (must be 0).
    let (rf, rs, i_f, is, miss) = sched::signal::sigfast_stats();
    let _ = writeln!(s, "# sigfast ret_fast {} ret_slow {} intr_fast {} intr_slow {} miss {} miss_pid {}",
        rf, rs, i_f, is, miss, sched::signal::SIGFAST_MISS_PID.load(core::sync::atomic::Ordering::Relaxed));
    // Lane percpu: one row per scheduler lock (TASKS = RUN_QUEUE; RQ<c> once
    // the per-CPU queues have their own locks), then the scheduler's
    // always-on consistency counters (all must stay 0 but the *_fast/_skip).
    sched::lockwatch::for_each_lock_row(|name, idx, n, ns, cont, spin| {
        if name == "TASKS" {
            let _ = writeln!(s, "# lock {} holds {} ns_held {} cont {} ns_spun {}", name, n, ns, cont, spin);
        } else {
            let _ = writeln!(s, "# lock {}{} holds {} ns_held {} cont {} ns_spun {}", name, idx, n, ns, cont, spin);
        }
    });
    {
        use sched::playstat as ps; let r = |a: &atomic::AtomicU64| a.load(atomic::Ordering::Relaxed);
        let _ = writeln!(s, "# playcost widen_skip {} widen_deref {} fwake_empty {} fwake_locked {} fwait_tidy {}",
            r(&ps::WIDEN_SKIP), r(&ps::WIDEN_DEREF), r(&ps::FWAKE_EMPTY), r(&ps::FWAKE_LOCKED), r(&ps::FWAIT_TIDY));
        let _ = writeln!(s, "# w4scan calls {} slots {} fallback {}",
            r(&ps::W4_CALLS), r(&ps::W4_SLOTS), r(&ps::W4_FALLBACK));
        let _ = writeln!(s, "# dlscan pass {} slots {} full {}",
            r(&ps::DL_PASS), r(&ps::DL_SLOTS), r(&ps::DL_FULL));
        let _ = writeln!(s, "# dlsplit due {} stale {} repush {} tfdpass {} tfdslots {} drop {} miss {}",
            r(&ps::DL_DUE), r(&ps::DL_STALE), r(&ps::DL_REPUSH), r(&ps::DL_TFDPASS), r(&ps::DL_TFDSLOTS), r(&ps::DL_DROP), r(&ps::DL_TFDMISS));
        let _ = writeln!(s, "# kwake calls {} slots {} deref {} full {}",
            r(&ps::KW_CALLS), r(&ps::KW_SLOTS), r(&ps::KW_DEREF), r(&ps::KW_FULL));
        let _ = writeln!(s, "# widen calls {} slots {} full {}",
            r(&ps::WIDEN_CALLS), r(&ps::WIDEN_SLOTS), r(&ps::WIDEN_FULL));
        let c: [u64; 8] = core::array::from_fn(|i| r(&FDCLOSE[i]));
        let d: [u64; 8] = core::array::from_fn(|i| r(&FDDUP[i]));
        let _ = writeln!(s, "# fdclose pipe {} eventfd {} timerfd {} dyndev {} mounted {} tmp {} other {} dyndev_last {}",
            c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]);
        let _ = writeln!(s, "# fddup pipe {} eventfd {} timerfd {} dyndev {} mounted {} tmp {} other {}",
            d[0], d[1], d[2], d[3], d[4], d[5], d[6]);
    }
    let _ = writeln!(s, "{}", sched::SchedStatsLine);
    proc_snapshot(b"/tmp/.rqhold", s.as_bytes(), false)
}

/// `/proc/pcsample`: the timer-tick PC samples taken since the previous read
/// (`sched::pcsample`), one line `cpu tgid pid sc mode pc` (decimal ids, sc
/// 4095 = not in a syscall, mode u/k/i, pc hex). Kernel time is
/// under-sampled (syscalls run with IRQs masked) — see the module docs.
fn gen_pcsample() -> Option<VnodeKind> {
    use core::fmt::Write;
    let mut v: alloc::vec::Vec<(u64, u64)> = alloc::vec::Vec::with_capacity(32768);
    sched::pcsample::snapshot(|m, pc| if v.len() < v.capacity() { v.push((m, pc)) });
    let mut s = alloc::string::String::with_capacity(v.len() * 40 + 64);
    let _ = writeln!(s, "# cpu tgid pid sc mode pc");
    for (m, pc) in v {
        let mode = if m & (1 << 53) != 0 { 'u' } else if m & (1 << 52) != 0 { 'i' } else { 'k' };
        let _ = writeln!(s, "{} {} {} {} {} {:x}", (m >> 54) & 0xf, m & 0xfffff,
            (m >> 20) & 0xfffff, (m >> 40) & 0xfff, mode, pc);
    }
    proc_snapshot(b"/tmp/.pcsample", s.as_bytes(), false)
}

/// `/proc/wakestat`: who issues poll-channel wakes and how many tasks each
/// one makes Ready (`sched::wakestat`, always on, cumulative since boot —
/// diff two reads), then, with `sched.wakestat=1` on the command line, which
/// of those wakes the woken epoll/poll loop found useful vs spurious
/// (`sched::gdwake`, per waker site and thread group).
fn gen_wakestat() -> Option<VnodeKind> {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    let mut rows: alloc::vec::Vec<sched::wakestat::Site> = alloc::vec::Vec::new();
    sched::wakestat::for_each(|r| rows.push(r));
    rows.sort_by(|a, b| b.4.cmp(&a.4));
    let _ = writeln!(s, "# site calls tagged woken allmask");
    for (f, l, calls, tagged, woken, allmask) in rows {
        let _ = writeln!(s, "{}:{} {} {} {} {}", f, l, calls, tagged, woken, allmask);
    }
    let _ = writeln!(s, "# overflow {}", sched::wakestat::overflow());
    {
        use core::sync::atomic::Ordering::Relaxed;
        let _ = writeln!(s, "# keyed skipped {} fp_hit {} unrefined {}",
            sched::wakestat::KEY_SKIPPED.load(Relaxed),
            sched::wakestat::KEY_FP_HIT.load(Relaxed),
            sched::wakestat::KEY_UNREFINED.load(Relaxed));
        let _ = writeln!(s, "# epoll calls {} miss {} probes {} unknown {} parks {}",
            sched::wakestat::EP_CALLS.load(Relaxed),
            sched::wakestat::EP_MISS.load(Relaxed),
            sched::wakestat::EP_PROBES.load(Relaxed),
            sched::wakestat::EP_UNKNOWN.load(Relaxed),
            sched::wakestat::EP_PARKS.load(Relaxed));
        let _ = writeln!(s, "# epollx inner {} passes {} recheck {} gencancel {} useless {} probe_ns {} filt_ns {}",
            sched::wakestat::EP_INNER.load(Relaxed),
            sched::wakestat::EP_PASSES.load(Relaxed),
            sched::wakestat::EP_RECHECK.load(Relaxed),
            sched::wakestat::EP_GENCANCEL.load(Relaxed),
            sched::wakestat::EP_USELESS.load(Relaxed),
            sched::wakestat::EP_PROBE_NS.load(Relaxed),
            sched::wakestat::EP_FILT_NS.load(Relaxed));
        {
            let n = &sched::wakestat::EP_NAMELESS;
            let _ = writeln!(s, "# epnameless named {} all {} deadline {} others {} self {} keys {} bits {} empty {} disarmed {}",
                n[0].load(Relaxed), n[1].load(Relaxed), n[2].load(Relaxed), n[3].load(Relaxed),
                n[4].load(Relaxed), n[5].load(Relaxed), n[6].load(Relaxed), n[7].load(Relaxed), n[8].load(Relaxed));
        }
        let _ = writeln!(s, "# pollx calls {} probes {} parks {} useless {} exclskip {} preskip {}",
            sched::wakestat::PO_CALLS.load(Relaxed),
            sched::wakestat::PO_PROBES.load(Relaxed),
            sched::wakestat::PO_PARKS.load(Relaxed),
            sched::wakestat::PO_USELESS.load(Relaxed),
            sched::wakestat::EXCL_SKIPPED.load(Relaxed),
            sched::wakestat::PO_PRESKIP.load(Relaxed));
        let _ = writeln!(s, "# wakeidx mode {} scans {} indexed {} visited {} derefs {} woken {} mskip {} miss {}",
            sched::runqueue::WAKE_IDX.load(Relaxed),
            sched::wakestat::WX_SCANS.load(Relaxed),
            sched::wakestat::WX_INDEXED.load(Relaxed),
            sched::wakestat::WX_VISITED.load(Relaxed),
            sched::wakestat::WX_DEREFS.load(Relaxed),
            sched::wakestat::WX_WOKEN.load(Relaxed),
            sched::wakestat::WX_MSKIP.load(Relaxed),
            sched::wakestat::WX_MISS.load(Relaxed));
        let _ = writeln!(s, "# nproc fast {} walked {} miss {}",
            sched::uidcount::FAST.load(Relaxed),
            sched::uidcount::WALKED.load(Relaxed),
            sched::uidcount::MISS.load(Relaxed));
    }
    if sched::gdwake::enabled() {
        let mut out: alloc::vec::Vec<(u64, u32, u64, u64)> = alloc::vec::Vec::new();
        sched::gdwake::for_each_outcome(|k, t, u, sp| out.push((k, t, u, sp)));
        out.sort_by(|a, b| b.3.cmp(&a.3));
        let _ = writeln!(s, "# outcome site tgid useful spurious");
        for (k, t, u, sp) in out {
            let mut name = alloc::string::String::new();
            sched::gdwake::site_name(k, &mut |f, l| {
                let _ = if l == 0 { write!(name, "{}", f) } else { write!(name, "{}:{}", f, l) };
            });
            let _ = writeln!(s, "outcome {} {} {} {}", name, t, u, sp);
        }
        let _ = writeln!(s, "# outcome_overflow {}", sched::gdwake::outcome_overflow());
        let _ = writeln!(s, "# park pid tgid mask_bits kbits keyed unkeyed");
        sched::gdwake::for_each_park(|p, t, m, k, ky, uk| {
            let _ = writeln!(s, "park {} {} {} {} {} {}", p, t, m, k, ky, uk);
        });
    } else {
        let _ = writeln!(s, "# outcome: off (boot with sched.wakestat=1)");
    }
    proc_snapshot(b"/tmp/.wakestat", s.as_bytes(), false)
}

/// `/proc/epollstat` (lane epattr): one row per epoll instance that has
/// waited since boot: owner thread group, the fd it was last waited on by,
/// parks, waits that returned events after a sleep (`useful`), sleeps after
/// which the wait found nothing (`useless`), and the registered fds those
/// useless wakes were pinned on (`fd:count:kind`; fd 4294967294 = the wake
/// named no registration, a full re-probe). Cumulative; diff two reads.
fn gen_epollstat() -> Option<VnodeKind> {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    let _ = writeln!(s, "# tgid epfd handle parks useful useless overflow | fd:useless:kind ...");
    sched::wakestat::ep_stats_each(|r| {
        use core::sync::atomic::Ordering::Relaxed;
        let tgid = r.tgid.load(Relaxed);
        // The wait names the epoll handle; show the fd the process holds it as.
        let h = r.epfd.load(Relaxed) as usize;
        let ufd = (0..4096usize).find(|&f| matches!(vfs_get_node_kind(tgid, f),
            Some(VnodeKind::EpollFd { h: x }) if x == h)).map_or(-1, |f| f as i64);
        let _ = write!(s, "{} {} {} {} {} {} {} |", tgid, ufd, h, r.parks.load(Relaxed),
            r.useful.load(Relaxed), r.useless.load(Relaxed), r.overflow.load(Relaxed));
        let mut top = r.top();
        top.sort_by(|a, b| b.1.cmp(&a.1));
        for (fd, n) in top {
            if n == 0 { continue; }
            let kind = if fd == sched::wakestat::EpStat::ALL_FD { "all" } else {
                match vfs_get_node_kind(tgid, fd as usize) {
                    Some(VnodeKind::Pipe { .. }) => "pipe",
                    Some(VnodeKind::Pty { .. }) => "pty",
                    Some(VnodeKind::EventFd { .. }) => "eventfd",
                    Some(VnodeKind::TimerFd { .. }) => "timerfd",
                    Some(VnodeKind::SignalFd { .. }) => "signalfd",
                    Some(VnodeKind::Inotify { .. }) => "inotify",
                    Some(VnodeKind::DynamicDevice { .. }) => "device",
                    Some(VnodeKind::EpollFd { .. }) => "epoll",
                    Some(VnodeKind::SockAlias { .. }) => "socket",
                    Some(_) => "file",
                    None => "?",
                }
            };
            let _ = write!(s, " {}:{}:{}", fd as i32, n, kind);
        }
        let _ = writeln!(s);
    });
    proc_snapshot(b"/tmp/.epollstat", s.as_bytes(), false)
}

/// `/proc/i915`: the i915 driver's GT frequency and activity counters,
/// answered by `drivers::i915::proc_text` (registered by servers/drm: this
/// crate must not depend on `drivers`). Unregistered, or before the i915
/// exec bring-up, the file does not exist.
static PROC_I915: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

pub fn set_proc_i915(f: fn() -> Option<alloc::string::String>) {
    PROC_I915.store(f as usize, core::sync::atomic::Ordering::Release);
}

/// `/proc/klog` source: the newest part of the kernel's RAM log ring.
static PROC_KLOG: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

pub fn set_proc_klog(f: fn() -> alloc::vec::Vec<u8>) {
    PROC_KLOG.store(f as usize, core::sync::atomic::Ordering::Release);
}

fn gen_klog() -> Option<VnodeKind> {
    let f = PROC_KLOG.load(core::sync::atomic::Ordering::Acquire);
    if f == 0 { return None; }
    let f: fn() -> alloc::vec::Vec<u8> = unsafe { core::mem::transmute(f) };
    proc_snapshot(b"/tmp/.klog", &f(), false)
}

/// `/proc/pcipm` text source (drivers::pci::pm_report), set by the kernel.
static PROC_PCIPM: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

pub fn set_proc_pcipm(f: fn() -> Option<alloc::string::String>) {
    PROC_PCIPM.store(f as usize, core::sync::atomic::Ordering::Release);
}

fn gen_pcipm() -> Option<VnodeKind> {
    let f = PROC_PCIPM.load(core::sync::atomic::Ordering::Acquire);
    if f == 0 { return None; }
    let f: fn() -> Option<alloc::string::String> = unsafe { core::mem::transmute(f) };
    let s = f()?;
    proc_snapshot(b"/tmp/.pcipm", s.as_bytes(), false)
}

fn gen_i915() -> Option<VnodeKind> {
    let f = PROC_I915.load(core::sync::atomic::Ordering::Acquire);
    if f == 0 { return None; }
    let f: fn() -> Option<alloc::string::String> = unsafe { core::mem::transmute(f) };
    let s = f()?;
    proc_snapshot(b"/tmp/.i915", s.as_bytes(), false)
}

/// `/proc/cpufreq`: per-CPU P-state mode, HWP/EIST caps and the effective
/// frequency over the last APERF/MPERF window (`arch_x86_64::cpufreq`).
fn gen_cpufreq() -> Option<VnodeKind> {
    let mut out = alloc::vec![0u8; 8192];
    // x86_64: arch_x86_64::cpufreq; aarch64: drivers::thermal (Pi 5 firmware clock).
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    let n = {
        extern "C" { fn arch_cpufreq_report(buf: *mut u8, cap: usize) -> usize; }
        unsafe { arch_cpufreq_report(out.as_mut_ptr(), out.len()) }
    };
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let n = {
        let m = b"cpufreq: not implemented on this architecture\n";
        out[..m.len()].copy_from_slice(m);
        m.len()
    };
    out.truncate(n);
    proc_snapshot(b"/tmp/.cpufreq", &out, false)
}

/// `/proc/thermal`: SoC temperature in millidegrees (`drivers::thermal`).
fn gen_thermal() -> Option<VnodeKind> {
    let mut out = alloc::vec![0u8; 256];
    extern "C" { fn arch_thermal_report(buf: *mut u8, cap: usize) -> usize; }
    let n = unsafe { arch_thermal_report(out.as_mut_ptr(), out.len()) };
    out.truncate(n);
    proc_snapshot(b"/tmp/.thermal", &out, false)
}

/// `/proc/modules`: loaded kernel modules (`kernel::kmod::sys`).
fn gen_modules() -> Option<VnodeKind> {
    let mut out = alloc::vec![0u8; 4096];
    extern "C" { fn arch_kmod_report(buf: *mut u8, cap: usize) -> usize; }
    let n = unsafe { arch_kmod_report(out.as_mut_ptr(), out.len()) };
    out.truncate(n);
    proc_snapshot(b"/tmp/.modules", &out, false)
}

/// `/proc/kmod_buildid`: this kernel's build-id in hex (the `lib/modules/<id>/` directory
/// name). The initrd's copy of that directory is openable by path but not listable, and an
/// installed disk root has no /lib/modules at all, so `kmodctl load <name>` asks here.
fn gen_kmod_buildid() -> Option<VnodeKind> {
    let mut out = alloc::vec![0u8; 128];
    extern "C" { fn arch_kmod_buildid(buf: *mut u8, cap: usize) -> usize; }
    let n = unsafe { arch_kmod_buildid(out.as_mut_ptr(), out.len()) };
    out.truncate(n);
    proc_snapshot(b"/tmp/.kmod_buildid", &out, false)
}

/// `/proc/kmod_initrd`: the regular files of the initrd's `lib/modules/<id>/`, one name per line
/// (cpio directories cannot be listed by getdents). init copies them to /run/modules/<id> before
/// the pivot when the disk root has no such directory.
fn gen_kmod_initrd() -> Option<VnodeKind> {
    extern "C" { fn arch_kmod_buildid(buf: *mut u8, cap: usize) -> usize; }
    let mut idb = [0u8; 128];
    let il = unsafe { arch_kmod_buildid(idb.as_mut_ptr(), idb.len()) };
    let id = &idb[..il];
    let id = if id.last() == Some(&b'\n') { &id[..id.len() - 1] } else { id };
    let mut prefix: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    prefix.extend_from_slice(b"lib/modules/");
    prefix.extend_from_slice(id);
    prefix.push(b'/');
    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let base = INITRD_BASE.load(atomic::Ordering::SeqCst);
    let size = INITRD_SIZE.load(atomic::Ordering::SeqCst);
    if base != 0 && size != 0 {
        let data: &'static [u8] = unsafe { core::slice::from_raw_parts(mm::phys_to_virt(base) as *const u8, size) };
        let hex = |b: &[u8]| -> usize { core::str::from_utf8(b).ok().and_then(|t| usize::from_str_radix(t, 16).ok()).unwrap_or(0) };
        let mut off = 0usize;
        while off + 110 <= data.len() && &data[off..off + 6] == b"070701" {
            let mode = hex(&data[off + 14..off + 22]);
            let fsz = hex(&data[off + 54..off + 62]);
            let nsz = hex(&data[off + 94..off + 102]);
            if nsz == 0 || off + 110 + nsz > data.len() { break; }
            let name = &data[off + 110..off + 110 + nsz - 1];
            let name = name.strip_prefix(b"./").unwrap_or(name);
            let name = name.strip_prefix(b"/").unwrap_or(name);
            if mode & 0o170000 == 0o100000 && name.starts_with(&prefix) {
                let rest = &name[prefix.len()..];
                if !rest.is_empty() && !rest.contains(&b'/') && out.len() + rest.len() < 60000 {
                    out.extend_from_slice(rest);
                    out.push(b'\n');
                }
            }
            off = ((off + 110 + nsz + 3) & !3) + fsz;
            off = (off + 3) & !3;
        }
    }
    proc_snapshot(b"/tmp/.kmod_initrd", &out, false)
}

/// `/proc/pcache`: the unified page cache (`mm::pagecache`), one
/// `name value` per line; counters are cumulative pages since boot.
/// f2fs no-I/O request reruns / requests that gave up (cumulative), and the
/// request to drop the clean block cache (`/proc/f2fsdropcache`, test knob).
/// /proc/f2fsstat counters of the f2fs server's checkpoint and fsync paths (lane f2fsnlog1).
pub static F2FS_CP_TOTAL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static F2FS_RF_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static F2FS_LX_CHAINED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static F2FS_LX_FALLBACK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static F2FS_LX_NODES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static F2FS_FSYNC_DELAYED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static F2FS_FSYNC_UNDELAYED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Checkpoint device-write time average (us) that sizes the group-commit window (lane f2fsckpt).
pub static F2FS_CP_EWMA_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static F2FS_NOIO_ROUNDS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static F2FS_NOIO_GAVE_UP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static F2FS_DROP_CLEAN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// `/proc/f2fsstat`: `noio_rounds N` and `noio_gave_up N`. Reading
/// `/proc/f2fsdropcache` asks the f2fs server to drop its clean cache on its
/// next request.
fn gen_f2fsstat(drop: bool) -> Option<VnodeKind> {
    use core::sync::atomic::Ordering::Relaxed;
    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let mut put = |name: &str, v: u64| {
        out.extend_from_slice(name.as_bytes());
        out.push(b' ');
        let mut d = [0u8; 20]; let mut n = d.len(); let mut x = v;
        loop { n -= 1; d[n] = b'0' + (x % 10) as u8; x /= 10; if x == 0 { break; } }
        out.extend_from_slice(&d[n..]);
        out.push(b'\n');
    };
    if drop { F2FS_DROP_CLEAN.store(true, Relaxed); }
    put("noio_rounds", F2FS_NOIO_ROUNDS.load(Relaxed));
    put("noio_gave_up", F2FS_NOIO_GAVE_UP.load(Relaxed));
    put("cp_total", F2FS_CP_TOTAL.load(Relaxed));
    put("rf_count", F2FS_RF_COUNT.load(Relaxed));
    put("lx_chained", F2FS_LX_CHAINED.load(Relaxed));
    put("lx_fallback", F2FS_LX_FALLBACK.load(Relaxed));
    put("lx_nodes", F2FS_LX_NODES.load(Relaxed));
    put("fsync_delayed", F2FS_FSYNC_DELAYED.load(Relaxed));
    put("fsync_undelayed", F2FS_FSYNC_UNDELAYED.load(Relaxed));
    put("cp_ewma_us", F2FS_CP_EWMA_US.load(Relaxed));
    proc_snapshot(b"/tmp/.f2fsstat", &out, false)
}

fn gen_pcache() -> Option<VnodeKind> {
    let st = mm::pagecache::stats();
    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    let mut put = |name: &str, v: usize| {
        out.extend_from_slice(name.as_bytes());
        out.push(b' ');
        let mut d = [0u8; 20]; let mut n = d.len(); let mut x = v;
        loop { n -= 1; d[n] = b'0' + (x % 10) as u8; x /= 10; if x == 0 { break; } }
        out.extend_from_slice(&d[n..]);
        out.push(b'\n');
    };
    put("enabled", mm::pagecache::read_cache_enabled() as usize);
    put("cached_pages", st.cached);
    put("cached_mib", st.cached * 4 / 1024);
    put("cap_pages", st.cap);
    put("dirty_pages", st.dirty);
    put("filling_pages", st.filling);
    put("mapped_hint_pages", st.mapped_hint);
    put("files", st.files);
    put("hits", st.hits);
    put("misses", st.misses);
    put("readahead", st.readahead);
    put("fills", st.fills);
    put("evicted", st.evicted);
    put("emergency_evicted", st.emergency);
    put("fill_waits", st.waits);
    put("stale_discards", st.stale);
    put("write_through", st.wrote_through);
    put("written_back", st.written_back);
    put("mkclean_ptes", mm::pagecache::MKCLEAN_PTES.load(core::sync::atomic::Ordering::Relaxed));
    put("cache_lock_acq", mm::pagecache::LOCK_ACQ.load(core::sync::atomic::Ordering::Relaxed));
    put("cache_lock_contended", mm::pagecache::LOCK_CONTENDED.load(core::sync::atomic::Ordering::Relaxed));
    put("uncached_reads", st.bypass);
    put("free_pages", mm::buddy::free_pages());
    let (low, high) = mm::pagecache::watermarks();
    put("low_watermark_pages", low);
    put("high_watermark_pages", high);
    proc_snapshot(b"/tmp/.pcache", &out, false)
}

/// Generate a `/sys/class/block/...` attribute file.
///
/// Same shape as `gen_proc_system`: the bytes are parked in an ephemeral
/// tmpfs slot under a synthetic "/tmp/.sysfs_<idx>" path no user path can
/// name, and the fd is an ordinary read-only `TmpFile`. The slot is released
/// by `handle_close` like every other ephemeral snapshot.
fn gen_drm_sysfs(path: &[u8]) -> Option<VnodeKind> {
    let mut buf = [0u8; TMP_BUF_SIZE];
    let len = drm_sysfs_attr(path, &mut buf)?;
    gen_sysfs_bytes(&buf[..len.min(TMP_BUF_SIZE)])
}

/// Writable snapshot of a hook-backed sysfs attribute; the fake path carries
/// the real one so `handle_write` can route the bytes to the hook.
const SYSCLASS_W: &[u8] = b"/tmp/.syscw";

/// May `pid` open the sysclass attribute at `path` for writing? Root for any
/// writable attribute; group `video` (gid 44) only for a backlight `brightness`.
fn sysclass_write_ok(pid: u32, path: &[u8]) -> bool {
    let mut g = [0u32; xattr::NGROUPS_MAX];
    let n = sched::groups_of(pid, &mut g).min(g.len());
    sysclass::write_allowed(sched::caps::host_capable_of(pid, sched::caps::CAP_DAC_OVERRIDE), sched::egid_of(pid), &g[..n],
                            sysclass::is_writable(path), sysclass::is_video_writable(path))
}

fn gen_sysclass_writable(path: &[u8]) -> Option<VnodeKind> {
    let mut data = [0u8; 64];
    let n = sysclass::attr(path, &mut data).unwrap_or(0).min(64);
    let mut fake = [0u8; 96];
    if SYSCLASS_W.len() + path.len() > fake.len() { return None; }
    fake[..SYSCLASS_W.len()].copy_from_slice(SYSCLASS_W);
    fake[SYSCLASS_W.len()..SYSCLASS_W.len() + path.len()].copy_from_slice(path);
    let fl = SYSCLASS_W.len() + path.len();
    let mut tmp = TMP_FILES.lock();
    let idx = tmp_free_slot(&mut *tmp)?;
    tmp.claim(idx, b"", true);
    tmp.set_path(idx, &fake[..fl]);
    if !tmp_fill_kernel(&mut tmp, idx, &data[..n]) { tmp.release(idx); return None; }
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: true, ofd: 0 })
}

/// A cgroupfs control file opened for writing: an ephemeral tmpfs slot named
/// `cgroupfs::SNAP_W` + `<cg>/<file>`, which handle_write routes to cgroupfs.
fn gen_cgroup_writable(cg: sched::cgroup::CgId, f: cgroupfs::F, data: &[u8]) -> Option<VnodeKind> {
    let fake = cgroupfs::snap_w_path(cg, f);
    let mut tmp = TMP_FILES.lock();
    let idx = tmp_free_slot(&mut *tmp)?;
    tmp.claim(idx, b"", true);
    tmp.set_path(idx, fake.as_bytes());
    if !tmp_fill_kernel(&mut tmp, idx, data) { tmp.release(idx); return None; }
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: true, ofd: 0 })
}

fn gen_sysfs(path: &[u8]) -> Option<VnodeKind> {
    let mut buf = [0u8; TMP_BUF_SIZE];
    let len = block::sysfs_attr(path, &mut buf)?;
    gen_sysfs_bytes(&buf[..len.min(TMP_BUF_SIZE)])
}

/// Park generated attribute bytes in an ephemeral read-only pool slot.
fn gen_sysfs_bytes(data: &[u8]) -> Option<VnodeKind> {
    let len = data.len();
    let buf = data;
    let mut tmp = TMP_FILES.lock();
    let idx = tmp_free_slot(&mut *tmp)?;
    tmp.claim(idx, b"", true);
    let mut fake = [0u8; 24];
    let base = b"/tmp/.sysfs_";
    fake[..base.len()].copy_from_slice(base);
    let mut fl = base.len();
    let mut n = idx;
    if n == 0 { fake[fl] = b'0'; fl += 1; }
    else {
        let mut d = [0u8; 5]; let mut di = 0;
        while n > 0 { d[di] = b'0' + (n % 10) as u8; di += 1; n /= 10; }
        while di > 0 { di -= 1; fake[fl] = d[di]; fl += 1; }
    }
    let fl = fl.min(MAX_TMP_PATH - 1);
    tmp.set_path(idx, &fake[..fl]);
    let copy = len.min(TMP_BUF_SIZE);
    if !tmp_fill_kernel(&mut tmp, idx, &buf[..copy]) { tmp.release(idx); return None; }
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: false, ofd: 0 })
}

fn gen_proc_system_content(path: &[u8], root: bool, buf: &mut [u8; TMP_BUF_SIZE]) -> Option<usize> {
    let ticks = sched::ticks();
    let uptime_sec  = ticks / 100;
    let uptime_frac = (ticks % 100) / 10; // tenths of a second

    // The resolver configuration follows the DHCP lease: the network's own
    // name server (QEMU's 10.0.2.3, vmnet's gateway, a LAN's DNS) answers
    // where a hard-coded public resolver may be blocked or slow. Before a
    // lease, the public resolvers are the fallback.
    if path == b"/etc/resolv.conf" {
        let (list, n) = *DHCP_DNS.lock();
        let mut p = 0;
        if n == 0 {
            p = write_lit(buf, p, b"nameserver 8.8.8.8\nnameserver 8.8.4.4\n");
        } else {
            p = write_lit(buf, p, b"# from DHCP\n");
            for a in list[..n].iter() {
                p = write_lit(buf, p, b"nameserver ");
                for (i, o) in a.iter().enumerate() {
                    if i > 0 { p = write_lit(buf, p, b"."); }
                    p = write_u32(buf, p, *o as u32);
                }
                p = write_lit(buf, p, b"\n");
            }
        }
        return Some(p);
    }

    // The fs sysctls a program sizes its descriptor use by (read-only here).
    // nr_open is the ceiling RLIMIT_NOFILE's hard limit can be raised to;
    // file-max the system-wide count of open file descriptions (`ofd`), and
    // file-nr those in use, Linux's "<allocated> 0 <max>".
    if let Some(c) = nslimit_sysctl(path) {
        let p = write_u32(buf, 0, sched::ns::limit_get(c));
        return Some(write_lit(buf, p, b"\n"));
    }
    if let Some(k) = inotify_sysctl(path) {
        let p = write_u32(buf, 0, inotify::limit_get(k) as u32);
        return Some(write_lit(buf, p, b"\n"));
    }
    if path == b"/proc/sys/fs/nr_open" {
        let p = write_u32(buf, 0, NR_OPEN as u32);
        return Some(write_lit(buf, p, b"\n"));
    }
    if path == b"/proc/sys/fs/file-max" {
        let p = write_u32(buf, 0, ofd::MAX_OFDS as u32);
        return Some(write_lit(buf, p, b"\n"));
    }
    if path == b"/proc/sys/fs/ovl_selftest" {
        // Runs the overlay real-op selftest at read time (root only).
        if !root { return None; }
        let mut o = Vec::new();
        ovl::selftest(&mut o);
        let n = o.len().min(buf.len());
        buf[..n].copy_from_slice(&o[..n]);
        return Some(n);
    }
    // `/proc/sys/kernel/random/{boot_id,uuid}`: a RFC 4122 v4 UUID, lowercase
    // 8-4-4-4-12 and a newline (Linux's format). boot_id is drawn once per
    // boot (ostree, systemd tools and flatpak key caches on it); uuid is new
    // on every read.
    if path == b"/proc/sys/kernel/random/boot_id" || path == b"/proc/sys/kernel/random/uuid" {
        let mut u = [0u8; 16];
        if path.ends_with(b"/boot_id") {
            let mut g = BOOT_ID_UUID.lock();
            if g.is_none() { let mut r = [0u8; 16]; sched::random::fill(&mut r); *g = Some(r); }
            u = g.unwrap();
        } else {
            sched::random::fill(&mut u);
        }
        u[6] = (u[6] & 0x0f) | 0x40;
        u[8] = (u[8] & 0x3f) | 0x80;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut p = 0;
        for (i, b) in u.iter().enumerate() {
            if i == 4 || i == 6 || i == 8 || i == 10 { buf[p] = b'-'; p += 1; }
            buf[p] = HEX[(b >> 4) as usize]; buf[p + 1] = HEX[(b & 15) as usize]; p += 2;
        }
        buf[p] = b'\n';
        return Some(p + 1);
    }
    if path == b"/proc/sys/fs/file-nr" {
        let live = ofd::census().map_or(0, |(l, _)| l);
        let mut p = write_u32(buf, 0, live as u32);
        p = write_lit(buf, p, b"\t0\t");
        p = write_u32(buf, p, ofd::MAX_OFDS as u32);
        return Some(write_lit(buf, p, b"\n"));
    }

    if path == b"/proc/cmdline" {
        let mut p = 0;
        // Secret knobs (wifi.psk=...) are redacted for EVERY reader. The
        // kernel already lifts them off its stored line at boot (see
        // drivers::cmdline_secrets), so this is the second fence. Root sees
        // the raw line only when the boot opted in with `cmdline.raw=1`.
        if let Some(c) = *KERNEL_CMDLINE.lock() {
            if root && c.split_ascii_whitespace().any(|t| t == "cmdline.raw=1") {
                p = write_lit(buf, p, c.as_bytes());
            } else {
                cmdline_redact::redact(c, |piece| p = write_lit(buf, p, piece.as_bytes()));
            }
        }
        p = write_lit(buf, p, b"\n");
        return Some(p);
    }

    // NVIDIA bring-up state (drivers/nvidia): absent | probing | ready |
    // failed. `nvrm.fakestate=<secs>:<ready|failed>` on the kernel command
    // line fakes "probing for <secs> of uptime, then ready/failed" so the
    // userspace wait paths can be exercised in QEMU.
    if path == b"/proc/nvrm_state" {
        let mut st = sched::nvrm_state::get();
        if let Some(c) = *KERNEL_CMDLINE.lock() {
            if let Some(v) = c.split_ascii_whitespace().find_map(|w| w.strip_prefix("nvrm.fakestate=")) {
                let (secs, to) = v.split_once(':').unwrap_or((v, "ready"));
                let secs: u64 = secs.parse().unwrap_or(0);
                st = if uptime_sec < secs { sched::nvrm_state::PROBING }
                     else if to == "failed" { sched::nvrm_state::FAILED }
                     else { sched::nvrm_state::READY };
            }
        }
        let p = write_lit(buf, 0, sched::nvrm_state::name(st).as_bytes());
        return Some(write_lit(buf, p, b"\n"));
    }

    if path == b"/proc/uptime" {
        let mut p = 0;
        p = write_u32(buf, p, uptime_sec as u32);
        p = write_lit(buf, p, b".");
        p = write_u32(buf, p, uptime_frac as u32);
        p = write_lit(buf, p, b" ");
        p = write_u32(buf, p, uptime_sec as u32); // idle ≈ uptime (no SMP idle accounting)
        p = write_lit(buf, p, b".0\n");
        return Some(p);
    }

    if path == b"/proc/loadavg" {
        // Last field is the most recently allocated pid, as on Linux: it is
        // the upper bound a `/proc/<pid>/stat` scan (init's stray sweep,
        // a `ps`) needs, since /proc has no per-pid directory listing.
        // "1-min 5-min 15-min running/threads last_pid", as Linux.
        let mut p = 0;
        for v in sched::loadavg() {
            let (i, f) = sched::loadavg_parts(v);
            p = write_u32(buf, p, i as u32);
            p = write_lit(buf, p, if f < 10 { b".0" } else { b"." });
            p = write_u32(buf, p, f as u32);
            p = write_lit(buf, p, b" ");
        }
        p = write_u32(buf, p, sched::nr_running().max(1)); // the reader itself runs
        p = write_lit(buf, p, b"/");
        p = write_u32(buf, p, sched::task_count() as u32);
        p = write_lit(buf, p, b" ");
        p = write_u32(buf, p, sched::last_pid());
        p = write_lit(buf, p, b"\n");
        return Some(p);
    }

    if path == b"/proc/meminfo" {
        // init's memory-pressure guard reads this every 0.25-2 s: give back
        // page-cache frames no mapping holds first, so what it judges is
        // memory that is really in use (kswapd's role on Linux).
        mm::pagecache::maybe_reclaim();
        let total = mm::buddy::total_pages() * 4; // pages → KiB
        let free  = mm::buddy::free_pages()  * 4;
        let used  = total.saturating_sub(free);
        let cached = mm::pagecache::cached_pages() * 4;
        // Clean cached pages no mapping is known to hold come back on
        // demand, as on Linux; MemAvailable counts them (init's guard keeps
        // judging free memory the cache could not give back: the cache
        // keeps MemFree above its low watermark, over the guard's floor).
        let avail = free + (mm::pagecache::reclaimable_pages() * 4).min(used);
        let mut p = 0;
        p = write_lit(buf, p, b"MemTotal:       ");
        p = write_u32(buf, p, total as u32);
        p = write_lit(buf, p, b" kB\nMemFree:        ");
        p = write_u32(buf, p, free as u32);
        p = write_lit(buf, p, b" kB\nMemAvailable:   ");
        p = write_u32(buf, p, avail as u32);
        p = write_lit(buf, p, b" kB\nBuffers:        0 kB\nCached:         ");
        p = write_u32(buf, p, cached.min(used) as u32);
        p = write_lit(buf, p, b" kB\nDirty:          ");
        p = write_u32(buf, p, (mm::pagecache::dirty_pages() * 4) as u32);
        // Shared anonymous memory (MAP_SHARED|MAP_ANONYMOUS, /dev/zero):
        // frames held by live `mm::shmem` objects.
        p = write_lit(buf, p, b" kB\nShmem:          ");
        p = write_u32(buf, p, (mm::shmem::pages() * 4) as u32);
        p = write_lit(buf, p, b" kB\nSwapTotal:      0 kB\nSwapFree:       0 kB\n");
        return Some(p);
    }

    if path == b"/proc/stat" {
        let mut p = 0;
        // Real per-CPU accounting: idle = time parked in hlt/wfi, busy (shown as
        // user time: no user/system split is kept) = uptime - idle, in jiffies.
        let ncpu = sched::active_cpu_count();
        let up_j = uptime_sec * 100 + (ticks % 100);
        let mut idle_tot = 0u64;
        let mut busy_tot = 0u64;
        for c in 0..ncpu {
            let idle = (sched::cpu_idle_ns(c) / 10_000_000).min(up_j);
            idle_tot += idle;
            busy_tot += up_j - idle;
        }
        p = write_lit(buf, p, b"cpu  ");
        p = write_u64(buf, p, busy_tot);
        p = write_lit(buf, p, b" 0 0 ");
        p = write_u64(buf, p, idle_tot);
        for c in 0..ncpu {
            let idle = (sched::cpu_idle_ns(c) / 10_000_000).min(up_j);
            p = write_lit(buf, p, b" 0 0 0 0 0 0\ncpu");
            p = write_u32(buf, p, c as u32);
            p = write_lit(buf, p, b" ");
            p = write_u64(buf, p, up_j - idle);
            p = write_lit(buf, p, b" 0 0 ");
            p = write_u64(buf, p, idle);
        }
        p = write_lit(buf, p, b" 0 0 0 0 0 0\nbtime ");
        // Boot time = now − uptime (fake: use 0)
        p = write_lit(buf, p, b"0\nctxt ");
        p = write_u64(buf, p, sched::slot::DISPATCHES.load(core::sync::atomic::Ordering::Relaxed));
        // `processes` is forks since boot (the last pid is the best count
        // kept); procs_running the runnable tasks now, as /proc/loadavg.
        p = write_lit(buf, p, b"\nprocesses ");
        p = write_u32(buf, p, sched::last_pid());
        p = write_lit(buf, p, b"\nprocs_running ");
        p = write_u32(buf, p, sched::nr_running().max(1));
        p = write_lit(buf, p, b"\nprocs_blocked 0\n");
        // Diagnostic (usbstalltest): `timerstat <ticks> <timer irqs>
        // <deadline-only irqs that returned early> <uart rx drains>
        // <uart rx bytes>`.
        {
            extern "C" { fn arch_timer_stats(out: *mut u64); }
            let mut st = [0u64; 5];
            unsafe { arch_timer_stats(st.as_mut_ptr()); }
            p = write_lit(buf, p, b"timerstat");
            for v in st { p = write_lit(buf, p, b" "); p = write_u64(buf, p, v); }
            p = write_lit(buf, p, b"\n");
        }
        return Some(p);
    }

    if path == b"/proc/self" {
        // Symlink target: just return pid as a string (used by some programs as a dir)
        let mut p = 0;
        p = write_u32(buf, p, pv(sched::tgid_of(sched::current_pid())));
        return Some(p);
    }

    if path == b"/proc/interrupts" {
        // One line per registered interrupt source (GIC SPI, ITS MSI LPI,
        // x86_64 IDT MSI vector): `INTID: count kind`.
        extern "C" { fn drivers_irq_report(buf: *mut u8, cap: usize) -> usize; }
        return Some(unsafe { drivers_irq_report(buf.as_mut_ptr(), buf.len()) });
    }

    if path == b"/proc/cpuinfo" {
        // One block per online CPU — Linux tools (nproc, lscpu, and
        // anything that counts "processor :" lines) derive the CPU count
        // from this file, same as sched_getaffinity's mask.
        //
        // `cpu MHz` is the TSC frequency the x86_64 timer resolved at boot
        // (CPUID-stated or PIT-measured; the `[TSC]` boot line), which is the
        // scale behind CLOCK_MONOTONIC and every kernel `*_us` diagnostic —
        // timertest reads it back to check `clock_gettime` against a raw
        // `rdtsc`. Other arches have no such single number and keep the
        // placeholder.
        #[cfg(target_arch = "x86_64")]
        let khz = {
            extern "C" { fn arch_tsc_khz() -> u64; }
            unsafe { arch_tsc_khz() }
        };
        #[cfg(not(target_arch = "x86_64"))]
        let khz: u64 = 1_000_000;
        let n = sched::active_cpu_count();
        let mut p = 0;
        for cpu in 0..n {
            p = write_lit(buf, p, b"processor\t: ");
            p = write_u32(buf, p, cpu as u32);
            // Linux arm64 layout: podman (getCPUInfo) and lscpu key off
            // "CPU architecture".
            #[cfg(target_arch = "aarch64")]
            {
                let _ = khz;
                p = write_lit(buf, p, b"\nBogoMIPS\t: 50.00\nFeatures\t: fp asimd evtstrm crc32 cpuid\nCPU implementer\t: 0x41\nCPU architecture: 8\nCPU variant\t: 0x0\nCPU part\t: 0xd08\nCPU revision\t: 3\n\n");
            }
            #[cfg(not(target_arch = "aarch64"))]
            {
            p = write_lit(buf, p, b"\nmodel name\t: Leandros Virtual CPU\ncpu MHz\t\t: ");
            p = write_u32(buf, p, (khz / 1000) as u32);
            let frac = (khz % 1000) as u32;
            p = write_lit(buf, p, if frac < 10 { b".00" } else if frac < 100 { b".0" } else { b"." });
            p = write_u32(buf, p, frac);
            p = write_lit(buf, p, b"\n\n");
            }
        }
        return Some(p);
    }

    if path == b"/proc/mounts" {
        // Legacy mtab-format mount table, generated from the live mount
        // registry — see write_mtab_lines for the field layout and why.
        return Some(gen_mount_view(sched::current_mntns(), false, buf));
    }

    None
}

/// Park generated bytes in an ephemeral pool slot (fake path `fake`), as
/// `gen_proc_self` does for the small files.
fn proc_snapshot(fake: &[u8], data: &[u8], is_dir: bool) -> Option<VnodeKind> {
    let mut tmp = TMP_FILES.lock();
    let idx = tmp_free_slot(&mut *tmp)?;
    tmp.claim(idx, b"", true);
    tmp[idx].is_dir    = is_dir;
    if is_dir { tmp[idx].mode = 0o555; }
    let fl = fake.len().min(MAX_TMP_PATH - 1);
    tmp.set_path(idx, &fake[..fl]);
    if !tmp_fill_kernel(&mut tmp, idx, data) { tmp.release(idx); return None; }
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: false, ofd: 0 })
}

/// `/proc/pollfds`: every task parked in a poll-family syscall with its fd
/// table (tag, hit against the park mask, cheap revents), then the virtio-gpu
/// fence / DRM state from the kernel. try_lock only; never blocks or probes a
/// device server.
fn gen_pollfds() -> Option<VnodeKind> {
    use core::fmt::Write;
    let mut s = alloc::string::String::with_capacity(16384);
    let mut waits = alloc::vec::Vec::with_capacity(1024);
    sched::task_waits(&mut waits);
    const POLL_SCS: [u32; 6] = [7, 23, 271, 270, 73, 72];
    for w in waits.iter() {
        if w.state != b'S' || w.sc == sched::NO_SYSCALL || !POLL_SCS.contains(&w.sc) { continue; }
        let _ = writeln!(s, "task {} {} sc={} in_sc_ms={} mask={:#x} listen={} wakes={}",
                         w.pid, w.tgid, w.sc, w.in_sc_ns / 1_000_000, w.poll_mask, w.listen as u8, w.poll_wakes);
        let Some(t) = FD_TABLES.try_lock() else { let _ = writeln!(s, "  fd_tables busy"); continue };
        let Some(tr) = t.map.get(&w.tgid) else { continue };
        let Some(tb) = tr.try_lock() else { let _ = writeln!(s, "  fd_table busy"); continue };
        let mut n = 0;
        for (fd, f) in tb.fds.iter_used() {
            if n >= 64 { let _ = writeln!(s, "  ..."); break; }
            n += 1;
            let kind = f.kind;
            let name = match kind {
                VnodeKind::Pipe { .. } => "pipe", VnodeKind::Pty { .. } => "pty",
                VnodeKind::EventFd { .. } => "eventfd", VnodeKind::TimerFd { .. } => "timerfd",
                VnodeKind::Mq { .. } => "mq", VnodeKind::PidFd { .. } => "pidfd",
                VnodeKind::SignalFd { .. } => "signalfd", VnodeKind::Inotify { .. } => "inotify",
                VnodeKind::DynamicDevice { .. } => "dev", VnodeKind::TmpFile { .. } => "tmp",
                VnodeKind::RamFile { .. } => "ram", VnodeKind::MountedFile { .. } => "file",
                _ => "other",
            };
            let tag = poll_vnode_key(&kind).map(sched::poll_key_bit);
            let _ = write!(s, "  fd {} kind={}", fd, name);
            match tag {
                Some(g) => { let _ = write!(s, " tag={:#x} hit={}", g, (g & w.poll_mask != 0) as u8); }
                None => s.push_str(" tag=? hit=?"),
            }
            match kind {
                VnodeKind::EventFd { slot } => {
                    let cnt = EVENTFD_COUNTERS.try_lock().map(|c| c[slot]);
                    let m = EVENTFD_META.try_lock().and_then(|m| m.get(slot).copied());
                    match cnt {
                        Some(c) => { let _ = write!(s, " rev={:#x}", POLLOUT | if c > 0 { POLLIN } else { 0 }); }
                        None => s.push_str(" rev=?"),
                    }
                    let _ = write!(s, " slot={}", slot);
                    if let Some(c) = cnt { let _ = write!(s, " cnt={}", c); }
                    if let Some(m) = m { let _ = write!(s, " origin={} fence={}", m.origin, m.fence); }
                }
                VnodeKind::Pipe { ring, is_write } => {
                    match PIPE_RINGS.try_lock() {
                        Some(r) => {
                            let r = &r[ring];
                            let ev = if is_write { if r.readers == 0 { POLLERR } else if r.free() >= PIPE_BUF { POLLOUT } else { 0 } }
                                     else { (if r.count > 0 { POLLIN } else { 0 }) | if r.writers == 0 { POLLIN | POLLHUP } else { 0 } };
                            let _ = write!(s, " rev={:#x}", ev);
                        }
                        None => s.push_str(" rev=?"),
                    }
                }
                VnodeKind::DynamicDevice { port, open_id, .. } => { let _ = write!(s, " rev=? port={} open={}", port, open_id); }
                _ => s.push_str(" rev=?"),
            }
            s.push('\n');
        }
    }
    let (dn, dpend) = sched::deferred_wake_state();
    let _ = writeln!(s, "sched deferred_keys={} poll_wake_pending={:#x}", dn, dpend);
    extern "C" { fn kernel_pollfds_gpu(buf: *mut u8, cap: usize) -> usize; }
    let mut g = alloc::vec![0u8; 8192];
    let n = unsafe { kernel_pollfds_gpu(g.as_mut_ptr(), g.len()) };
    s.push_str(core::str::from_utf8(&g[..n]).unwrap_or(""));
    proc_snapshot(b"/tmp/.pollfds", s.as_bytes(), false)
}

/// Which `/proc/<pid>` memory map file [`gen_proc_maps`] renders.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MapsKind { Maps, Smaps, Rollup }

/// `/proc/<pid>/maps` (one line per VMA), `smaps` (that line plus the
/// per-VMA figures) or `smaps_rollup` (the totals).
///
/// The census runs under the target's address-space `busy` flag and only
/// copies the per-VMA figures out; formatting and naming happen after it is
/// released (naming takes registry and tmpfs locks, which are never taken
/// under `busy`). Mappings are named as on Linux: the file's path, a memfd
/// as `/memfd:<name> (deleted)`, `[heap]` and `[stack]`. There is no vDSO.
/// Until 2026-10-19 `maps` was a stub that always read as 0 bytes and
/// `smaps` named nothing but the heap and stack.
fn gen_proc_maps(pid: u32, kind: MapsKind) -> Option<VnodeKind> {
    // Far above any real process (Firefox: a few thousand VMAs); a bound so a
    // pathological one cannot take the heap with it.
    const MAPS_MAX: usize = 16 << 20;
    struct W { buf: alloc::vec::Vec<u8>, full: bool }
    impl W {
        fn s(&mut self, t: &[u8]) {
            if self.buf.len() + t.len() > MAPS_MAX { self.full = true; return; }
            self.buf.extend_from_slice(t);
        }
        fn hex(&mut self, v: usize, width: usize) {
            let mut d = [b'0'; 16]; let mut n = 0; let mut v = v;
            while v > 0 || n < width { d[15 - n] = b"0123456789abcdef"[v & 15]; v >>= 4; n += 1; if n == 16 { break; } }
            let t = d; self.s(&t[16 - n..]);
        }
        fn kb(&mut self, key: &[u8], bytes: usize) {
            self.s(key);
            let mut d = [0u8; 20]; let mut n = d.len(); let mut v = bytes / 1024;
            loop { n -= 1; d[n] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
            let pad = 8usize.saturating_sub(d.len() - n);
            for _ in 0..pad { self.s(b" "); }
            let t = d; self.s(&t[n..]);
            self.s(b" kB\n");
        }
    }
    let mut stats: alloc::vec::Vec<mm::vmm::VmaStat> = alloc::vec::Vec::new();
    let ok = sched::proc_vma_census(pid, !matches!(kind, MapsKind::Maps), &mut |v: &mm::vmm::VmaStat| stats.push(*v));
    if !ok { return None; }

    let mut w = W { buf: alloc::vec::Vec::new(), full: false };
    let mut tot = mm::vmm::VmaStat::default();
    let mut lo = usize::MAX; let mut hi = 0usize;
    let mut name = alloc::vec![0u8; HOST_PATH_MAX];
    for v in &stats {
        lo = lo.min(v.start); hi = hi.max(v.end);
        tot.rss += v.rss; tot.anon += v.anon; tot.shared_pages += v.shared_pages;
        tot.pss_bytes += v.pss_bytes;
        tot.end += v.end - v.start;
        if kind == MapsKind::Rollup || w.full { continue; }
        let mark = w.buf.len();
        w.hex(v.start, 8); w.s(b"-"); w.hex(v.end, 8);
        w.s(if v.prot & 1 != 0 { b" r" } else { b" -" });
        w.s(if v.prot & 2 != 0 { b"w" } else { b"-" });
        w.s(if v.prot & 4 != 0 { b"x" } else { b"-" });
        w.s(if v.shared { b"s " } else { b"p " });
        w.hex(if v.file_backed || v.vmo != 0 { v.file_off as usize } else { 0 }, 8);
        w.s(b" 00:00 0");
        let n = if v.file_cap != 0 { mm::vmm::file_name(v.file_cap, &mut name) }
            else if v.vmo != 0 { vmo_name(v.vmo, &mut name) }
            else { 0 };
        let label: &[u8] = if n > 0 { &name[..n] }
            else if v.is_stack { b"[stack]" }
            else if v.is_heap { b"[heap]" }
            else { b"" };
        if !label.is_empty() {
            // Linux pads the fixed part to 72 columns, then one space.
            let line = w.buf.len() - mark;
            for _ in line..72 { w.s(b" "); }
            w.s(b" ");
            w.s(label);
        }
        w.s(b"\n");
        if kind == MapsKind::Smaps { smaps_rows(&mut w, v); }
        if w.full { w.buf.truncate(mark); }
    }
    if kind == MapsKind::Rollup {
        if lo == usize::MAX { lo = 0; }
        w.hex(lo, 8); w.s(b"-"); w.hex(hi, 8);
        w.s(b" ---p 00000000 00:00 0                          [rollup]\n");
        tot.start = 0;
        smaps_rows(&mut w, &tot);
    }
    fn smaps_rows(w: &mut W, v: &mm::vmm::VmaStat) {
        let pg = 4096usize;
        w.kb(b"Size:           ", v.end - v.start);
        w.kb(b"KernelPageSize: ", pg);
        w.kb(b"MMUPageSize:    ", pg);
        w.kb(b"Rss:            ", v.rss * pg);
        w.kb(b"Pss:            ", v.pss_bytes);
        w.kb(b"Shared_Clean:   ", 0);
        w.kb(b"Shared_Dirty:   ", v.shared_pages * pg);
        w.kb(b"Private_Clean:  ", 0);
        w.kb(b"Private_Dirty:  ", (v.rss - v.shared_pages.min(v.rss)) * pg);
        w.kb(b"Referenced:     ", v.rss * pg);
        w.kb(b"Anonymous:      ", v.anon * pg);
        w.kb(b"Swap:           ", 0);
        w.kb(b"SwapPss:        ", 0);
        w.kb(b"Locked:         ", 0);
    }
    proc_snapshot(match kind {
        MapsKind::Maps => b"/tmp/.proc_maps",
        MapsKind::Smaps => b"/tmp/.proc_smaps",
        MapsKind::Rollup => b"/tmp/.proc_smaps_rollup",
    }, &w.buf, false)
}

/// Open of the directory `/proc/<pid>`: an ephemeral pool directory whose
/// path *is* `/proc/<pid>`, so `openat(dirfd, "stat")` (bottom, procps)
/// resolves through the fd's path, fstat says S_IFDIR, and getdents lists
/// `PROC_PID_FILES`.
/// `/proc/<pid|self|thread-self>/ns` (the directory itself) -> the process
/// it belongs to. Linux lists the namespace links here; bwrap, nsenter and
/// runc open the directory and then openat() each link.
fn proc_ns_dir(path: &[u8], caller: u32) -> Option<u32> {
    let rest = strip_trailing_slash(path).strip_prefix(b"/proc/")?;
    let who = rest.strip_suffix(b"/ns")?;
    if who == b"thread-self" { return Some(caller); }
    if who == b"self" { return Some(sched::tgid_of(caller)); }
    proc_parse_num(who)
}

/// Open of the directory `/proc/<pid>/ns`: an ephemeral pool directory whose
/// path is `/proc/<pid>/ns`, listed by `handle_getdents64` from `NS_NAMES`.
fn gen_proc_ns_dir(pid: u32) -> Option<VnodeKind> {
    let mut path = [0u8; 24];
    path[..6].copy_from_slice(b"/proc/");
    let mut n = 6;
    let mut d = [0u8; 10]; let mut k = 0; let mut v = pv(pid);
    loop { d[k] = b'0' + (v % 10) as u8; k += 1; v /= 10; if v == 0 { break; } }
    while k > 0 { k -= 1; path[n] = d[k]; n += 1; }
    path[n..n + 3].copy_from_slice(b"/ns");
    n += 3;
    proc_snapshot(&path[..n], b"", true)
}

fn gen_proc_pid_dir(pid: u32) -> Option<VnodeKind> {
    let mut path = [0u8; 16];
    path[..6].copy_from_slice(b"/proc/");
    let mut n = 6;
    let mut d = [0u8; 10]; let mut k = 0; let mut v = pv(pid);
    loop { d[k] = b'0' + (v % 10) as u8; k += 1; v /= 10; if v == 0 { break; } }
    while k > 0 { k -= 1; path[n] = d[k]; n += 1; }
    proc_snapshot(&path[..n], b"", true)
}

/// `/proc/self/comm`, `/proc/<tid>/comm`, `/proc/<pid>/task/<tid>/comm` or
/// `/proc/thread-self/comm` -> the thread it names.
fn proc_comm_target(path: &[u8], caller: u32) -> Option<u32> {
    if let Some(r) = proc_task_parse(path, caller) {
        return match r {
            Some(ProcTask::File { tid, name, .. }) if name == b"comm" => Some(tid),
            _ => None,
        };
    }
    let rest = path.strip_prefix(b"/proc/")?;
    let who = rest.strip_suffix(b"/comm")?;
    if who == b"self" { return Some(caller); }
    proc_parse_num(who)
}

fn proc_parse_num(s: &[u8]) -> Option<u32> {
    if s.is_empty() || s.len() > 9 || !s.iter().all(|b| b.is_ascii_digit()) { return None; }
    let mut v = 0u32;
    for &d in s { v = v * 10 + (d - b'0') as u32; }
    pv_in(v)
}

/// A pid number from a /proc path, as the reader's pid namespace sees it, to
/// the kernel pid (None = not visible, so ENOENT).
fn pv_in(v: u32) -> Option<u32> {
    sched::pidns::from_cur(v).ok()
}

/// Kernel pid `k` as the reader's pid namespace numbers it (0 = invisible).
fn pv(k: u32) -> u32 {
    let n = sched::pidns::cur();
    if n == 0 { k } else { sched::pidns::to_v(n, k) }
}

/// A path below `/proc/<pid>/task` or `/proc/thread-self`, resolved to the
/// live thread group / thread it names (Linux semantics: `task` lists every
/// thread of the group; `<tid>` must belong to it).
enum ProcTask<'a> {
    /// `/proc/<pid>/task`
    Dir { tgid: u32 },
    /// `/proc/<pid>/task/<tid>`
    Tid { tgid: u32, tid: u32 },
    /// `/proc/<pid>/task/<tid>/<name>`, `name` in PROC_PID_FILES
    File { tgid: u32, tid: u32, name: &'a [u8] },
}

/// `Some(None)`: a task path naming nothing (ENOENT). `None`: not a task path.
fn proc_task_parse<'a>(path: &'a [u8], caller: u32) -> Option<Option<ProcTask<'a>>> {
    let rest = path.strip_prefix(b"/proc/")?;
    let alive = |t: u32| sched::proc_stat_of(t).is_some();
    // /proc/thread-self[/<name>] is /proc/<tgid>/task/<tid>[/<name>].
    if let Some(t) = rest.strip_prefix(b"thread-self") {
        if !(t.is_empty() || t[0] == b'/') { return None; }
        let tid = sched::current_pid();
        let tgid = sched::tgid_of(tid);
        let name = t.strip_prefix(b"/").unwrap_or(t);
        let name = name.strip_suffix(b"/").unwrap_or(name);
        if name == b"fd" { return None; } // the fd directory: see proc_pid_fd_dir
        if name.is_empty() { return Some(Some(ProcTask::Tid { tgid, tid })); }
        if !PROC_PID_FILES.iter().any(|f| *f == name) { return Some(None); }
        return Some(Some(ProcTask::File { tgid, tid, name }));
    }
    let slash = rest.iter().position(|&b| b == b'/')?;
    let (who, tail) = (&rest[..slash], &rest[slash + 1..]);
    let tail = tail.strip_prefix(b"task")?;
    if !(tail.is_empty() || tail[0] == b'/') { return None; }
    let pid = if who == b"self" { sched::tgid_of(caller) } else { proc_parse_num(who)? };
    if !alive(pid) { return Some(None); }
    let tgid = sched::tgid_of(pid);
    let tail = tail.strip_prefix(b"/").unwrap_or(tail);
    let tail = tail.strip_suffix(b"/").unwrap_or(tail);
    if tail.is_empty() { return Some(Some(ProcTask::Dir { tgid })); }
    let (t, name) = match tail.iter().position(|&b| b == b'/') {
        Some(i) => (&tail[..i], &tail[i + 1..]),
        None => (tail, &tail[tail.len()..]),
    };
    let Some(tid) = proc_parse_num(t) else { return Some(None) };
    if !alive(tid) || sched::tgid_of(tid) != tgid { return Some(None); }
    if name.is_empty() { return Some(Some(ProcTask::Tid { tgid, tid })); }
    if !PROC_PID_FILES.iter().any(|f| *f == name) { return Some(None); }
    Some(Some(ProcTask::File { tgid, tid, name }))
}

/// "/proc/<tgid>/task[/<tid>[/<name>]]" into `out`; returns the length.
fn proc_task_path(out: &mut [u8; 96], tgid: u32, tid: Option<u32>, name: &[u8]) -> usize {
    let mut b = Buf96 { b: out, n: 0 };
    b.push(b"/proc/"); b.num(pv(tgid)); b.push(b"/task");
    if let Some(t) = tid {
        b.push(b"/"); b.num(pv(t));
        if !name.is_empty() { b.push(b"/"); b.push(name); }
    }
    b.n
}

struct Buf96<'a> { b: &'a mut [u8; 96], n: usize }
impl Buf96<'_> {
    fn push(&mut self, s: &[u8]) {
        let k = s.len().min(96 - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s[..k]);
        self.n += k;
    }
    fn num(&mut self, mut v: u32) {
        let mut d = [0u8; 10]; let mut k = 0;
        loop { d[k] = b'0' + (v % 10) as u8; k += 1; v /= 10; if v == 0 { break; } }
        while k > 0 { k -= 1; self.push(&[d[k]]); }
    }
}

/// Open of a `/proc/<pid>/task...` or `/proc/thread-self...` path.
fn open_proc_task(caller: u32, t: ProcTask, flags: u32) -> Result<VnodeKind, i32> {
    let mut p = [0u8; 96];
    match t {
        ProcTask::Dir { tgid } => {
            if flags & 0o3 != 0 { return Err(-21); } // EISDIR
            let n = proc_task_path(&mut p, tgid, None, b"");
            proc_snapshot(&p[..n], b"", true).ok_or(-23)
        }
        ProcTask::Tid { tgid, tid } => {
            if flags & 0o3 != 0 { return Err(-21); }
            let n = proc_task_path(&mut p, tgid, Some(tid), b"");
            proc_snapshot(&p[..n], b"", true).ok_or(-23)
        }
        ProcTask::File { tgid, tid, name } => {
            if name == b"environ" && !proc_environ_allowed(caller, tid) { return Err(-13); }
            let n = proc_task_path(&mut p, tgid, Some(tid), name);
            gen_proc_self(caller, tid, &p[..n]).ok_or(-2)
        }
    }
}

/// `/proc/<pid>/environ` is readable by root and by the process's owner
/// (Linux: PTRACE_MODE_READ_FSCREDS, so non-dumpable targets need CAP_SYS_PTRACE).
fn proc_environ_allowed(caller: u32, target: u32) -> bool {
    sched::caps::host_capable_of(caller, sched::caps::CAP_SYS_PTRACE) || sched::cred::ptrace_read_check(caller, target) == 0
}

/// stat of a per-process procfs path: `None` not one; `Some(None)` it names
/// nothing (ENOENT); else `(is_dir, owner pid, file name)`.
fn proc_stat_kind(path: &[u8]) -> Option<Option<(bool, u32, &[u8])>> {
    let path = strip_trailing_slash(path);
    if let Some(r) = proc_task_parse(path, sched::current_tgid()) {
        return Some(r.map(|t| match t {
            ProcTask::Dir { tgid } => (true, tgid, &b""[..]),
            ProcTask::Tid { tid, .. } => (true, tid, &b""[..]),
            ProcTask::File { tid, name, .. } => (false, tid, name),
        }));
    }
    let alive = |p: u32| sched::proc_stat_of(p).is_some();
    if let Some(p) = proc_ns_dir(path, sched::current_pid()) {
        return Some(if alive(p) { Some((true, p, &b""[..])) } else { None });
    }
    if path == PROC_SELF_FDINFO_DIR { return Some(Some((true, sched::current_tgid(), &b""[..]))); }
    if let Some((p, n)) = proc_fdinfo_file(path, sched::current_pid()) {
        let ok = alive(p) && vfs_get_node_kind(p, n).is_some();
        return Some(if ok { Some((false, p, &b"fdinfo"[..])) } else { None });
    }
    if let Some(p) = proc_pid_dir(path).or_else(|| proc_pid_fd_dir(path)) {
        return Some(if alive(p) { Some((true, p, &b""[..])) } else { None });
    }
    if let Some(p) = proc_pid_path(path) {
        let name = &path[path.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1)..];
        return Some(if alive(p) { Some((false, p, name)) } else { None });
    }
    None
}

/// What `/proc/<pid>` (and `/proc/<pid>/task/<tid>`) directories list.
fn proc_dir_entry(i: usize, with_task: bool) -> Option<(&'static [u8], u8)> {
    if i < PROC_PID_FILES.len() { return Some((PROC_PID_FILES[i], 8)); }
    if i == PROC_PID_FILES.len() { return Some((b"ns", 4)); }
    if with_task && i == PROC_PID_FILES.len() + 1 { return Some((b"task", 4)); }
    None
}

/// `/proc/<pid>/cmdline` / `environ`: the strings the process was exec'd
/// with, read back from its stack (`sched::read_proc_args`), NUL-separated.
/// Kernel tasks have an empty cmdline (ps then shows `[comm]`); a process
/// that never exec'd shows its executable path.
const PROC_ARGS_MAX: usize = 64 * 1024;
fn gen_proc_args(pid: u32, environ: bool) -> Option<VnodeKind> {
    let mut v: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    match sched::read_proc_args(pid, environ, PROC_ARGS_MAX, &mut v) {
        Some(true) | None => {}
        Some(false) => if !environ {
            let mut exe = alloc::vec![0u8; PATH_MAX];
            let n = sched::exe_path(sched::tgid_of(pid), &mut exe).unwrap_or(0);
            let p: &[u8] = if n > 0 { &exe[..n] } else { b"/bin/init" };
            v.extend_from_slice(p);
            v.push(0);
        },
    }
    proc_snapshot(if environ { b"/tmp/.proc_environ" } else { b"/tmp/.proc_cmdline" }, &v, false)
}

const PROC_COMM_W: &[u8] = b"/tmp/.proc_comm_";
const PROC_NSMAP_W: &[u8] = b"/tmp/.proc_nsmap_";
const PROC_OOM_W: &[u8] = b"/tmp/.proc_oom_";

/// `/proc/<pid|self>/oom_score_adj` -> the process it names.
fn proc_oom_target(path: &[u8], caller: u32) -> Option<u32> {
    let rest = path.strip_prefix(b"/proc/")?;
    let who = rest.strip_suffix(b"/oom_score_adj")?;
    if who == b"self" { return Some(caller); }
    proc_parse_num(who)
}

fn oom_text(v: i32, out: &mut [u8; 12]) -> usize {
    let mut d = [0u8; 10]; let mut k = 0; let mut a = v.unsigned_abs();
    loop { d[k] = b'0' + (a % 10) as u8; k += 1; a /= 10; if a == 0 { break; } }
    let mut n = 0;
    if v < 0 { out[0] = b'-'; n = 1; }
    while k > 0 { k -= 1; out[n] = d[k]; n += 1; }
    out[n] = b'\n';
    n + 1
}

/// Fake path of a writable `/proc/sys/net/...` snapshot: carries netns and slot.
const PROC_NETCTL_W: &[u8] = b"/tmp/.proc_netctl_";

/// `/proc/sys/net/<rel>` file → slot in sched::ns::NETCTL.
fn netctl_slot(ns: u16, path: &[u8]) -> Option<usize> {
    sched::ns::netctl_find_ns(ns, path.strip_prefix(b"/proc/sys/net/")?)
}

fn gen_proc_netctl(pid: u32, k: usize, write: bool) -> Option<VnodeKind> {
    let ns = sched::ns::netns_of(pid);
    let mut data = [0u8; 32];
    let l = sched::ns::netctl_text(ns, k, &mut data);
    if !write {
        let mut buf = [0u8; TMP_BUF_SIZE];
        buf[..l].copy_from_slice(&data[..l]);
        return gen_proc_system_file(&buf, l);
    }
    let mut fake = [0u8; 48];
    fake[..PROC_NETCTL_W.len()].copy_from_slice(PROC_NETCTL_W);
    let mut n = PROC_NETCTL_W.len();
    for v in [ns as usize, k] {
        let mut d = [0u8; 8]; let mut i = 0; let mut x = v;
        loop { d[i] = b'0' + (x % 10) as u8; i += 1; x /= 10; if x == 0 { break; } }
        while i > 0 { i -= 1; fake[n] = d[i]; n += 1; }
        fake[n] = b'_'; n += 1;
    }
    let mut tmp = TMP_FILES.lock();
    let idx = tmp_free_slot(&mut *tmp)?;
    tmp.claim(idx, b"", true);
    tmp.set_path(idx, &fake[..n]);
    if !tmp_fill_kernel(&mut tmp, idx, &data[..l]) { tmp.release(idx); return None; }
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: true, ofd: 0 })
}

/// A writable snapshot of `/proc/<pid>/oom_score_adj`; the fake path carries
/// the pid so `handle_write` can route the digits to `sched::set_oom_score_adj`.
fn gen_proc_oom_writable(tid: u32) -> Option<VnodeKind> {
    let mut fake = [0u8; 40];
    fake[..PROC_OOM_W.len()].copy_from_slice(PROC_OOM_W);
    let mut n = PROC_OOM_W.len();
    let mut d = [0u8; 10]; let mut k = 0; let mut v = tid;
    loop { d[k] = b'0' + (v % 10) as u8; k += 1; v /= 10; if v == 0 { break; } }
    while k > 0 { k -= 1; fake[n] = d[k]; n += 1; }
    let mut data = [0u8; 12];
    let l = oom_text(sched::oom_score_adj_of(tid).unwrap_or(0), &mut data);
    let mut tmp = TMP_FILES.lock();
    let idx = tmp_free_slot(&mut *tmp)?;
    tmp.claim(idx, b"", true);
    tmp.set_path(idx, &fake[..n]);
    if !tmp_fill_kernel(&mut tmp, idx, &data[..l]) { tmp.release(idx); return None; }
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: true, ofd: 0 })
}
/// Opener credentials of each writable map file, by tmp-pool slot.
static NSMAP_OPENERS: Mutex<[Option<sched::ns::Opener>; MAX_TMP_FILES]> = Mutex::new([const { None }; MAX_TMP_FILES]);

/// `/proc/<pid|self>/{uid_map,gid_map,setgroups}` → (thread group, 0|1|2).
fn proc_nsmap_target(path: &[u8], caller: u32) -> Option<(u32, usize)> {
    let rest = path.strip_prefix(b"/proc/")?;
    let slash = rest.iter().position(|&b| b == b'/')?;
    let (who, name) = (&rest[..slash], &rest[slash + 1..]);
    let kind = match name { b"uid_map" => 0, b"gid_map" => 1, b"setgroups" => 2, _ => return None };
    let pid = if who == b"self" { sched::tgid_of(caller) } else { sched::tgid_of(proc_parse_num(who)?) };
    Some((pid, kind))
}

/// A writable snapshot of a namespace map file; the fake path carries
/// `<kind><tgid>` so `handle_write` can route the bytes to `sched::ns`.
fn gen_proc_nsmap_writable(tgid: u32, kind: usize, opener: sched::ns::Opener) -> Option<VnodeKind> {
    let mut fake = [0u8; 40];
    fake[..PROC_NSMAP_W.len()].copy_from_slice(PROC_NSMAP_W);
    let mut n = PROC_NSMAP_W.len();
    fake[n] = b'0' + kind as u8; n += 1;
    let mut d = [0u8; 10]; let mut k = 0; let mut v = tgid;
    loop { d[k] = b'0' + (v % 10) as u8; k += 1; v /= 10; if v == 0 { break; } }
    while k > 0 { k -= 1; fake[n] = d[k]; n += 1; }
    let mut tmp = TMP_FILES.lock();
    let idx = tmp_free_slot(&mut *tmp)?;
    tmp.claim(idx, b"", true);
    tmp.set_path(idx, &fake[..n]);
    if !tmp_fill_kernel(&mut tmp, idx, b"") { tmp.release(idx); return None; }
    NSMAP_OPENERS.lock()[idx] = Some(opener);
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: true, ofd: 0 })
}

/// `/proc/<pid|self>/ns/<name>`: `None` not such a path; `Some(None)` no such
/// namespace link; `Some(Some((pid, kind)))` the link's process and kind.
fn proc_ns_file(path: &[u8], caller: u32) -> Option<Option<(u32, usize)>> {
    let rest = path.strip_prefix(b"/proc/")?;
    // /proc/thread-self/ns/<name>: the calling thread's own namespaces.
    if let Some(t) = rest.strip_prefix(b"thread-self/ns/") {
        return Some(sched::ns::NS_NAMES.iter().position(|n| *n == t).map(|k| (caller, k)));
    }
    let slash = rest.iter().position(|&b| b == b'/')?;
    let (who, tail) = (&rest[..slash], &rest[slash + 1..]);
    // /proc/<pid|self>/task/<tid>/ns/<name>: that thread's namespaces (net
    // can differ per thread after unshare/setns in one thread).
    if let Some(t) = tail.strip_prefix(b"task/") {
        let pid = if who == b"self" { sched::tgid_of(caller) } else { proc_parse_num(who)? };
        let s2 = t.iter().position(|&b| b == b'/')?;
        let name = t[s2 + 1..].strip_prefix(b"ns/")?;
        let Some(tid) = proc_parse_num(&t[..s2]) else { return Some(None) };
        if sched::proc_stat_of(pid).is_none() || sched::proc_stat_of(tid).is_none()
            || sched::tgid_of(tid) != sched::tgid_of(pid) { return Some(None); }
        return Some(sched::ns::NS_NAMES.iter().position(|n| *n == name).map(|k| (tid, k)));
    }
    let name = tail.strip_prefix(b"ns/")?;
    let pid = if who == b"self" { sched::tgid_of(caller) } else { proc_parse_num(who)? };
    Some(sched::ns::NS_NAMES.iter().position(|n| *n == name).map(|k| (pid, k)))
}

/// A writable snapshot of `/proc/<tid>/comm`; the fake path carries the tid so
/// `handle_write` can route the bytes to `sched::set_comm`.
fn gen_proc_comm_writable(tid: u32) -> Option<VnodeKind> {
    let mut fake = [0u8; 32];
    fake[..PROC_COMM_W.len()].copy_from_slice(PROC_COMM_W);
    let mut n = PROC_COMM_W.len();
    let mut d = [0u8; 10]; let mut k = 0; let mut v = tid;
    loop { d[k] = b'0' + (v % 10) as u8; k += 1; v /= 10; if v == 0 { break; } }
    while k > 0 { k -= 1; fake[n] = d[k]; n += 1; }
    let mut nm = [0u8; 15];
    let l = sched::comm_of(tid, &mut nm);
    let mut data = [0u8; 16];
    data[..l].copy_from_slice(&nm[..l]);
    data[l] = b'\n';
    let mut tmp = TMP_FILES.lock();
    let idx = tmp_free_slot(&mut *tmp)?;
    tmp.claim(idx, b"", true);
    tmp.set_path(idx, &fake[..n]);
    if !tmp_fill_kernel(&mut tmp, idx, &data[..l + 1]) { tmp.release(idx); return None; }
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: true, ofd: 0 })
}

/// Generate dynamic content for a /proc/self/<name> path.
/// Allocates a TmpFile slot, writes the content, and returns the vnode.
fn gen_proc_self(reader: u32, pid: u32, path: &[u8]) -> Option<VnodeKind> {
    if path.ends_with(b"/uid_map") || path.ends_with(b"/gid_map") || path.ends_with(b"/setgroups") {
        let ns = sched::ns::userns_of(pid);
        let mut b = [0u8; 128];
        let n = if path.ends_with(b"/setgroups") {
            let t: &[u8] = if sched::ns::setgroups_denied(ns) { b"deny\n" } else { b"allow\n" };
            b[..t.len()].copy_from_slice(t); t.len()
        } else { sched::ns::map_text_for(path.ends_with(b"/gid_map") as usize, ns, sched::ns::userns_of(reader), &mut b) };
        return proc_snapshot(b"/tmp/.proc_nsmap", &b[..n], false);
    }
    if path.ends_with(b"/smaps") || path.ends_with(b"/smaps_rollup") || path.ends_with(b"/maps") {
        let kind = if path.ends_with(b"/smaps_rollup") { MapsKind::Rollup }
            else if path.ends_with(b"/smaps") { MapsKind::Smaps } else { MapsKind::Maps };
        return gen_proc_maps(pid, kind);
    }
    if path.ends_with(b"/cgroup") {
        return proc_snapshot(b"/tmp/.proc_cgroup", &cgroupfs::proc_cgroup(pid, reader), false);
    }
    if path.ends_with(b"/oom_score_adj") {
        let mut b = [0u8; 12];
        let n = oom_text(sched::oom_score_adj_of(pid)?, &mut b);
        return proc_snapshot(b"/tmp/.proc_oom", &b[..n], false);
    }
    if path.ends_with(b"/cmdline") || path.ends_with(b"/environ") {
        return gen_proc_args(pid, path.ends_with(b"/environ"));
    }
    // A mount namespace's table is not bounded by the 2 KiB scratch buffer: a
    // truncated mountinfo hides the newest mounts (bwrap --dev/--proc looks the
    // mount it just made up in it, "Unable to find ... in mount table").
    if path.ends_with(b"/mountinfo") || path.ends_with(b"/mounts") {
        let ns = sched::ns::mntns_of(pid);
        if ns != 0 {
            if let Some(v) = mnt::render_tree(ns, path.ends_with(b"/mountinfo")) {
                return proc_snapshot(b"/tmp/.proc_mnt", &v, false);
            }
        }
    }
    let mut buf = [0u8; TMP_BUF_SIZE];
    let len = gen_proc_self_content(pid, path, &mut buf)?;

    let mut tmp = TMP_FILES.lock();
    let idx = tmp_free_slot(&mut *tmp)?;
    tmp.claim(idx, b"", true);
    // Use a unique synthetic path: "/tmp/.proc_<idx>" — never conflicts with user files.
    let mut fake_path = [0u8; 20];
    let base = b"/tmp/.proc_";
    fake_path[..base.len()].copy_from_slice(base);
    let mut fpl = base.len();
    let mut n = idx;
    if n == 0 { fake_path[fpl] = b'0'; fpl += 1; }
    else {
        let mut digits = [0u8; 5]; let mut di = 0;
        while n > 0 { digits[di] = b'0' + (n % 10) as u8; di += 1; n /= 10; }
        for i in (0..di).rev() { fake_path[fpl] = digits[i]; fpl += 1; }
    }
    let fp_len = fpl.min(MAX_TMP_PATH - 1);
    tmp.set_path(idx, &fake_path[..fp_len]);
    // Copy the generated content into the data buffer.
    let copy = len.min(TMP_BUF_SIZE);
    if !tmp_fill_kernel(&mut tmp, idx, &buf[..copy]) { tmp.release(idx); return None; }
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: false, ofd: 0 })
}

/// Files served under `/proc/<pid>/` for any live process — what `ps`/`top`
/// (bottom), jobtest's state probe and init's memory-pressure guard read —
/// and what a `/proc/<pid>` directory lists. `/proc/<pid>/fd` is served too
/// (`proc_pid_fd_dir`); the rest of `/proc/<pid>/` is still `/proc/self/`-only.
/// Synthesized at open (see handle_open) and not in RAMFS: stat must know them.
const PROC_DIAG_FILES: &[&[u8]] = &[b"/proc/sysstat", b"/proc/kmemstat", b"/proc/wakestat",
                                     b"/proc/vtstat", b"/proc/pollfds", b"/proc/rqhold", b"/proc/pcsample",
                                     b"/proc/lockstat", b"/proc/i915", b"/proc/cpufreq", b"/proc/thermal",
                                     b"/proc/pcache", b"/proc/f2fsstat", b"/proc/f2fsdropcache", b"/proc/epollstat", b"/proc/pcipm", b"/proc/klog", b"/proc/modules", b"/proc/kmod_buildid", b"/proc/kmod_initrd", b"/proc/tryboot", b"/proc/mntselftest", b"/proc/cgroups", b"/proc/drm/fence_stats", b"/proc/drm/fence_hold"];
/// The regular files `handle_open` generates through `gen_proc_system` (and
/// the `/proc/sys/fs` ones), none of which is a RAMFS entry: stat must answer
/// for them or they cannot be a bind-mount target.
const PROC_SYSTEM_FILES: &[&[u8]] = &[b"/proc/meminfo", b"/proc/uptime", b"/proc/cmdline", b"/proc/nvrm_state",
                                      b"/proc/loadavg", b"/proc/stat", b"/proc/mounts", b"/proc/interrupts",
                                      b"/proc/sys/fs/nr_open", b"/proc/sys/fs/file-max", b"/proc/sys/fs/file-nr",
                                      b"/proc/sys/fs/ovl_selftest", b"/proc/sys/kernel/random/boot_id", b"/proc/sys/kernel/random/uuid", b"/proc/kmod_initrd", b"/proc/kmod_buildid"];
const PROC_PID_FILES: &[&[u8]] = &[b"stat", b"status", b"statm", b"cmdline", b"comm", b"maps", b"smaps",
                                   b"smaps_rollup", b"environ", b"uid_map", b"gid_map", b"setgroups", b"oom_score_adj", b"cgroup",
                                   b"mountinfo", b"mounts"];

/// `/proc/<pid|self>/root` or `/proc/<pid|self>/root/<rest>` → `rest`
/// (empty for the root itself) when the process is alive. See handle_open.
fn proc_pid_root_rest(path: &[u8]) -> Option<&[u8]> {
    let (alive, tail) = proc_pid_root_split(path)?;
    if alive { Some(tail) } else { None }
}

/// The same path shape, but naming a process that does not exist.
fn proc_pid_root_dead(path: &[u8]) -> bool {
    matches!(proc_pid_root_split(path), Some((false, _)))
}

fn proc_pid_root_split(path: &[u8]) -> Option<(bool, &[u8])> {
    let rest = path.strip_prefix(b"/proc/")?;
    let slash = rest.iter().position(|&b| b == b'/')?;
    let (who, tail) = (&rest[..slash], &rest[slash..]);
    let tail = tail.strip_prefix(b"/root")?;
    if !(tail.is_empty() || tail[0] == b'/') {
        return None;
    }
    if who == b"self" {
        return Some((true, tail));
    }
    if who.is_empty() || who.len() > 9 || !who.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut pid = 0u32;
    for &d in who { pid = pid * 10 + (d - b'0') as u32; }
    let pid = pv_in(pid)?;
    Some((sched::proc_stat_of(pid).is_some(), tail))
}

/// `/proc/<digits>` or `/proc/<digits>/` → that pid (the directory itself).
fn proc_pid_dir(path: &[u8]) -> Option<u32> {
    let rest = path.strip_prefix(b"/proc/")?;
    let digits = rest.strip_suffix(b"/").unwrap_or(rest);
    if digits.is_empty() || digits.len() > 9 || !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut pid = 0u32;
    for &d in digits { pid = pid * 10 + (d - b'0') as u32; }
    pv_in(pid)
}

/// The path the kernel substitutes for the root of a task that chroots into
/// procfs (Firefox's `chroot("/proc/self/fdinfo")`): an empty read-only
/// directory under which nothing resolves. It is not listed in "/" and the
/// kernel refuses it to a task whose root is not this directory.
pub const VOID_ROOT: &[u8] = b"/.leandros-void";

const PROC_SELF_FDINFO_DIR: &[u8] = b"/proc/self/fdinfo";

/// `/proc/<digits>/fd` or `/proc/<digits>/fdinfo` (the directory itself) → that pid.
fn proc_pid_fd_dir(path: &[u8]) -> Option<u32> {
    let rest = path.strip_prefix(b"/proc/")?;
    let digits = rest.strip_suffix(b"/fd").or_else(|| rest.strip_suffix(b"/fdinfo"))?;
    proc_parse_num(digits)
}

/// `(pid, N)` of `/proc/<pid|self|thread-self>/fdinfo/N`; `me` stands for self.
fn proc_fdinfo_file(path: &[u8], me: u32) -> Option<(u32, usize)> {
    let rest = path.strip_prefix(b"/proc/")?;
    let slash = rest.iter().position(|&c| c == b'/')?;
    let who = &rest[..slash];
    let digits = rest[slash..].strip_prefix(b"/fdinfo/")?;
    if digits.is_empty() || digits.len() > 7 || !digits.iter().all(|d| d.is_ascii_digit()) { return None; }
    let n = digits.iter().fold(0usize, |a, &d| a * 10 + (d - b'0') as usize);
    let pid = if who == b"self" || who == b"thread-self" { me } else { proc_parse_num(who)? };
    Some((pid, n))
}

/// The text of `/proc/<pid>/fdinfo/N`: `pos`, `flags` (octal), `mnt_id`.
fn gen_proc_fdinfo(path: &[u8], target: u32, fd: usize) -> Result<VnodeKind, i32> {
    let kind = vfs_get_node_kind(target, fd).ok_or(-2)?;
    let flags = fd_flags(target, fd).unwrap_or(0);
    let pos: u64 = match kind {
        VnodeKind::TmpFile { pos, .. } => pos as u64,
        VnodeKind::RamFile { pos, .. } => pos as u64,
        _ => 0,
    };
    let mut buf = [0u8; TMP_BUF_SIZE];
    let mut p = write_lit(&mut buf, 0, b"pos:\t");
    p = write_u64(&mut buf, p, pos);
    p = write_lit(&mut buf, p, b"\nflags:\t0");
    let mut o = [0u8; 12]; let mut k = o.len(); let mut v = flags;
    loop { k -= 1; o[k] = b'0' + (v & 7) as u8; v >>= 3; if v == 0 { break; } }
    p = write_lit(&mut buf, p, &o[k..]);
    p = write_lit(&mut buf, p, b"\nmnt_id:\t0\n");
    proc_snapshot(path, &buf[..p], false).ok_or(-23)
}

/// Open of the directory `/proc/<pid>/fd`: an ephemeral pool directory whose
/// path is `/proc/<pid>/fd`, listed by `handle_getdents64` from the target's
/// descriptor table.
fn gen_proc_fd_dir(pid: u32, info: bool) -> Option<VnodeKind> {
    let mut path = [0u8; 28];
    path[..6].copy_from_slice(b"/proc/");
    let mut n = 6;
    let mut d = [0u8; 10]; let mut k = 0; let mut v = pv(pid);
    loop { d[k] = b'0' + (v % 10) as u8; k += 1; v /= 10; if v == 0 { break; } }
    while k > 0 { k -= 1; path[n] = d[k]; n += 1; }
    let sfx: &[u8] = if info { b"/fdinfo" } else { b"/fd" };
    path[n..n + sfx.len()].copy_from_slice(sfx);
    n += sfx.len();
    proc_snapshot(&path[..n], b"", true)
}

/// `/proc/<digits>/<file>` for a file in `PROC_PID_FILES` → that pid.
fn proc_pid_path(path: &[u8]) -> Option<u32> {
    let rest = path.strip_prefix(b"/proc/")?;
    let slash = rest.iter().position(|&b| b == b'/')?;
    let (digits, tail) = (&rest[..slash], &rest[slash..]);
    if digits.is_empty() || digits.len() > 9 || !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !PROC_PID_FILES.iter().any(|f| &tail[1..] == *f) {
        return None;
    }
    let mut pid = 0u32;
    for &d in digits { pid = pid * 10 + (d - b'0') as u32; }
    pv_in(pid)
}

fn gen_proc_self_content(pid: u32, path: &[u8], buf: &mut [u8; TMP_BUF_SIZE]) -> Option<usize> {
    // Real values for the process named — `/proc/self/` (the caller) or
    // `/proc/<pid>/` — including the procps state letter, so a stopped job
    // reads `T` the way `ps` and `jobs` expect. The caller can only ever see
    // itself as `R`.
    let (ppid, pgid, sid, state) = match sched::proc_stat_of(pid) {
        Some((ppid, pgid, sid, state)) => (ppid, pgid, sid, state),
        None => (sched::current_ppid(), sched::current_pgid(), sched::current_sid(), b'R'),
    };
    let state_word: &[u8] = match state {
        b'S' => b"S (sleeping)",
        b'T' => b"T (stopped)",
        b'Z' => b"Z (zombie)",
        _    => b"R (running)",
    };
    let ticks = sched::ticks();

    // Memory figures (see `mm::vmm::MemCounters`) and the executable's name
    // for `Name:` / the stat comm — what `ps`, `top`/bottom and init's
    // memory-pressure guard key on.
    let wants_mem = path.ends_with(b"/status") || path.ends_with(b"/statm")
        || path.ends_with(b"/stat");
    let mem = if wants_mem { sched::proc_mem_of(pid) } else { None };
    let threads = mem.map(|(_, t)| t.max(1)).unwrap_or(1);
    let mc = mem.map(|(c, _)| c).unwrap_or_default();
    // `/proc/<pid>/task/<tid>/...`: the thread's own CPU times (the
    // process-level files sum the group).
    let per_thread = path.windows(6).any(|w| w == b"/task/");
    // Linux's comm is at most 15 bytes (TASK_COMM_LEN - 1); an explicit
    // prctl(PR_SET_NAME) / write to /proc/<tid>/comm overrides the basename.
    let mut comm_buf = [0u8; 15];
    let comm_len = sched::comm_of(pid, &mut comm_buf);
    let comm = &comm_buf[..comm_len];

    if path == b"/proc/self/comm" || path.ends_with(b"/comm") {
        let mut p = write_lit(buf, 0, comm);
        p = write_lit(buf, p, b"\n");
        return Some(p);
    }
    const KB_PER_PAGE: u64 = 4;

    if path == b"/proc/self/status" || path.ends_with(b"/status") {
        let mut p = 0;
        p = write_lit(buf, p, b"Name:\t");
        p = write_lit(buf, p, comm);
        p = write_lit(buf, p, b"\nState:\t");
        p = write_lit(buf, p, state_word);
        p = write_lit(buf, p, b"\nTgid:\t");
        p = write_u32(buf, p, pv(sched::tgid_of(pid)));
        p = write_lit(buf, p, b"\nPid:\t");
        p = write_u32(buf, p, pv(pid));
        p = write_lit(buf, p, b"\nPPid:\t");
        p = write_u32(buf, p, pv(ppid));
        p = write_lit(buf, p, b"\nTracerPid:\t0");
        p = write_lit(buf, p, b"\nPGid:\t");
        p = write_u32(buf, p, pv(pgid));
        // Real, effective, saved, filesystem ids. These were hardcoded 0,
        // so every process — the uid-1000 session included — looked like
        // root to anything reading status.
        let (u, g) = sched::cred::ids_of(pid).unwrap_or(([0; 4], [0; 4]));
        for (label, ids, kind) in [(&b"\nUid:"[..], u, 0usize), (&b"\nGid:"[..], g, 1usize)] {
            p = write_lit(buf, p, label);
            for v in ids {
                p = write_lit(buf, p, b"\t");
                p = write_u32(buf, p, sched::ns::view_id(kind, v));
            }
        }
        // Supplementary groups, as Linux prints them (each followed by a
        // space): how a session service's group memberships are checked.
        p = write_lit(buf, p, b"\nGroups:\t");
        {
            let mut gl = [0u32; xattr::NGROUPS_MAX];
            let n = sched::groups_of(pid, &mut gl);
            for g in &gl[..n] {
                p = write_u32(buf, p, sched::ns::view_id(1, *g));
                p = write_lit(buf, p, b" ");
            }
        }
        // NSpid / NSpgid / NSsid / NStgid: the id at every pid-namespace level
        // from the reader's namespace down to the task's own, outermost first.
        {
            let rn = sched::pidns::cur();
            let tn = sched::pidns::pidns_of(pid);
            let mut chain = [0u16; 34];
            let mut cl = 0usize;
            let mut x = tn;
            loop {
                chain[cl] = x; cl += 1;
                if x == rn || x == 0 || cl >= chain.len() { break; }
                x = sched::ns::parent(x);
            }
            let tg = sched::tgid_of(pid);
            for (label, k) in [(&b"\nNStgid:"[..], tg), (&b"\nNSpid:"[..], pid),
                               (&b"\nNSpgid:"[..], pgid), (&b"\nNSsid:"[..], sid)] {
                p = write_lit(buf, p, label);
                for i in (0..cl).rev() {
                    p = write_lit(buf, p, b"\t");
                    let v = if k == pid || k == tg { sched::pidns::view(chain[i], k, tn) }
                            else { sched::pidns::to_v(chain[i], k) };
                    p = write_u32(buf, p, v);
                }
            }
        }
        if let Some(c) = sched::caps::caps_of(pid) {
            for (label, v) in [(&b"\nCapInh:\t"[..], c[2]), (&b"\nCapPrm:\t"[..], c[1]),
                               (&b"\nCapEff:\t"[..], c[0]), (&b"\nCapBnd:\t"[..], c[3]), (&b"\nCapAmb:\t"[..], c[4])] {
                p = write_lit(buf, p, label);
                let mut hex = [b'0'; 16];
                for i in 0..16 { hex[i] = b"0123456789abcdef"[(v >> (60 - 4 * i) & 15) as usize]; }
                p = write_lit(buf, p, &hex);
            }
        }
        p = write_lit(buf, p, b"\n");
        // Linux order: ... CapBnd, CapAmb, NoNewPrivs, Seccomp, Seccomp_filters.
        if let Some((nnp, mode, nf)) = sched::seccomp::status_of(pid) {
            p = write_lit(buf, p, b"NoNewPrivs:\t");
            p = write_u32(buf, p, nnp as u32);
            p = write_lit(buf, p, b"\nSeccomp:\t");
            p = write_u32(buf, p, mode);
            p = write_lit(buf, p, b"\nSeccomp_filters:\t");
            p = write_u32(buf, p, nf);
            p = write_lit(buf, p, b"\nSeccomp_irq_windows:\t");
            p = write_u64(buf, p, sched::seccomp::IRQ_WINDOWS.load(core::sync::atomic::Ordering::Relaxed));
            p = write_lit(buf, p, b"\n");
        }
        let kb = |pages: usize| pages as u64 * KB_PER_PAGE;
        let rows: [(&[u8], u64); 11] = [
            (b"VmPeak:\t", kb(mc.peak)),
            (b"VmSize:\t", kb(mc.size)),
            (b"VmHWM:\t", kb(mc.hwm)),
            (b"VmRSS:\t", kb(mc.rss())),
            (b"RssAnon:\t", kb(mc.rss_anon)),
            (b"RssFile:\t", kb(mc.rss_file)),
            (b"RssShmem:\t", kb(mc.rss_shmem)),
            (b"VmData:\t", kb(mc.data)),
            (b"VmStk:\t", kb(mc.stk)),
            (b"VmExe:\t", kb(mc.exe)),
            (b"VmSwap:\t", 0),
        ];
        for (k, v) in rows.iter() {
            p = write_lit(buf, p, k);
            p = write_u64(buf, p, *v);
            p = write_lit(buf, p, b" kB\n");
        }
        p = write_lit(buf, p, b"Threads:\t");
        p = write_u32(buf, p, threads);
        p = write_lit(buf, p, b"\n");
        return Some(p);
    }

    if path == b"/proc/self/statm" || path.ends_with(b"/statm") {
        // size resident shared text lib data dt, in pages (proc(5)).
        let mut p = 0;
        for (i, v) in [mc.size, mc.rss(), mc.rss_file + mc.rss_shmem, mc.exe, 0,
                       mc.data + mc.stk, 0].iter().enumerate() {
            if i > 0 { p = write_lit(buf, p, b" "); }
            p = write_u64(buf, p, *v as u64);
        }
        p = write_lit(buf, p, b"\n");
        return Some(p);
    }

    if path == b"/proc/self/stat" || path.ends_with(b"/stat") {
        // proc(5) field order: pid (comm) state ppid pgrp session tty_nr
        // tpgid flags minflt cminflt majflt cmajflt utime stime cutime
        // cstime priority nice num_threads itrealvalue starttime vsize rss
        // rsslim, then 27..52. Until 2026-09-26 two fields were missing
        // before `priority`, which shifted num_threads/vsize/rss by two.
        let mut p = 0;
        p = write_u32(buf, p, pv(pid));
        p = write_lit(buf, p, b" (");
        p = write_lit(buf, p, comm);
        p = write_lit(buf, p, b") ");
        p = write_lit(buf, p, &[state]);
        p = write_lit(buf, p, b" ");
        p = write_u32(buf, p, pv(ppid));
        p = write_lit(buf, p, b" ");
        p = write_u32(buf, p, pv(pgid));
        p = write_lit(buf, p, b" ");
        p = write_u32(buf, p, pv(sid)); // field 6: session id
        // 7 tty_nr .. 13 cmajflt, 14 utime, 15 stime, 16 cutime, 17 cstime,
        // 18 priority, 19 nice. utime / stime are the thread group's CPU
        // time outside / inside system calls (`Task::cpu_ns` minus / and
        // `Task::sys_ns`, `sched::sysacct`; page faults and IRQs count as
        // user) in clock ticks of 1/100 s (`sysconf(_SC_CLK_TCK)`).
        let (cpu, sys) = if per_thread { sched::thread_cpu_sys_ns(pid) }
                         else { sched::process_cpu_sys_ns(sched::tgid_of(pid)) };
        // 7 tty_nr (Linux dev_t: major in bits 8..15, minor in 0..7 and
        // 20..31) and 8 tpgid (-1: no controlling terminal), from the
        // session's controlling terminal (`tty_server::ctty_dev_of_sid`).
        match tty_server::ctty_dev_of_sid(sid) {
            Some((maj, min, fg)) => {
                p = write_lit(buf, p, b" ");
                p = write_u64(buf, p, makedev(maj, min));
                p = write_lit(buf, p, b" ");
                p = write_u32(buf, p, fg);
            }
            None => p = write_lit(buf, p, b" 0 -1"),
        }
        // 9 flags, 10 minflt, 11 cminflt, 12 majflt, 13 cmajflt: the page
        // faults of the process (its address space), or of the thread itself
        // for task/<tid>/stat, and of the children this process waited for.
        let ch = sched::process_child_acct(sched::tgid_of(pid));
        let (minflt, majflt) = if per_thread {
            sched::thread_rusage(pid).map_or((0, 0), |r| (r.2, r.3))
        } else { (mc.minflt, mc.majflt) };
        p = write_lit(buf, p, b" 0 ");
        for (i, v) in [minflt, ch.minflt, majflt, ch.majflt].iter().enumerate() {
            if i > 0 { p = write_lit(buf, p, b" "); }
            p = write_u64(buf, p, *v);
        }
        p = write_lit(buf, p, b" ");
        p = write_u64(buf, p, cpu.saturating_sub(sys) / 10_000_000);
        p = write_lit(buf, p, b" ");
        p = write_u64(buf, p, sys / 10_000_000);
        // 16 cutime, 17 cstime: waited-for children (`sched::ChildAcct`).
        p = write_lit(buf, p, b" ");
        p = write_u64(buf, p, ch.cpu_ns.saturating_sub(ch.sys_ns) / 10_000_000);
        p = write_lit(buf, p, b" ");
        p = write_u64(buf, p, ch.sys_ns / 10_000_000);
        p = write_lit(buf, p, b" 20 0 ");
        p = write_u32(buf, p, threads); // 20
        p = write_lit(buf, p, b" 0 "); // 21 itrealvalue
        // 22 starttime: when the task named was created, in clock ticks
        // (1/100 s) since boot -- the leader's for /proc/<pid>/stat, the
        // thread's own for task/<tid>/stat and /proc/<tid>/stat (Linux reads
        // the named task's start_boottime either way). It was the *current*
        // uptime in seconds until 2026-10-17, which made it useless as half of
        // a process identity: sudo's credential cache keys on (pid,
        // starttime) of its parent or session leader, so a recycled pid could
        // inherit a cached credential.
        let start_ticks = sched::task_start_ns(pid)
            .map(|ns| ns / 10_000_000)
            .unwrap_or(ticks);
        p = write_u64(buf, p, start_ticks);
        p = write_lit(buf, p, b" ");
        p = write_u64(buf, p, mc.size as u64 * 4096); // 23 vsize, bytes
        p = write_lit(buf, p, b" ");
        p = write_u64(buf, p, mc.rss() as u64); // 24 rss, pages
        // 25 rsslim, 26..52
        p = write_lit(buf, p, b" 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n");
        return Some(p);
    }

    if path == b"/proc/self/fd" {
        // Return placeholder empty content for the directory.
        return Some(0);
    }

    if path == b"/proc/self/auxv" || path.ends_with(b"/auxv") {
        // Mirror the auxv written to the user stack at execve time.
        // MAME's leandros_sound driver reads this to discover AT_LEANDROS_AUDIO_PORT.
        let entries: &[(u64, u64)] = &[
            (6,   4096),                                           // AT_PAGESZ
            (11,  0), (12, 0), (13, 0), (14, 0),                  // AT_UID/EUID/GID/EGID
            (256, sched::get_vfs_port()   as u64),                 // AT_LEANDROS_VFS_PORT
            (257, sched::get_net_port()   as u64),                 // AT_LEANDROS_NET_PORT
            (258, sched::get_audio_port() as u64),                 // AT_LEANDROS_AUDIO_PORT
            (0,   0),                                              // AT_NULL
        ];
        let bytes = entries.len() * 16;
        if bytes <= TMP_BUF_SIZE {
            for (i, &(k, v)) in entries.iter().enumerate() {
                buf[i * 16..i * 16 + 8].copy_from_slice(&k.to_le_bytes());
                buf[i * 16 + 8..i * 16 + 16].copy_from_slice(&v.to_le_bytes());
            }
            return Some(bytes);
        }
    }

    if path == b"/proc/self/mountinfo" || path.ends_with(b"/mountinfo") {
        // This is the file `df`/`read_fs_list()` prefers over /etc/mtab —
        // see write_mountinfo_lines for the field layout and why. Per
        // namespace: the target process's mount namespace.
        return Some(gen_mount_view(sched::ns::mntns_of(pid), true, buf));
    }
    if path == b"/proc/self/mounts" || path.ends_with(b"/mounts") {
        return Some(gen_mount_view(sched::ns::mntns_of(pid), false, buf));
    }

    None
}

fn handle_open(pid: u32, path_ptr: usize, flags: u32, mode: u32) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) {
        Some(r) => r,
        None    => return err_reply(-14),
    };
    // O_PATH names a location without opening it for data access. Strip the
    // access mode and the destructive flags before anything downstream reads
    // them, so the open resolves and stats but neither creates nor truncates;
    // handle_read/handle_write refuse the resulting fd by inspecting O_PATH in
    // the stored flags.
    // O_TMPFILE (__O_TMPFILE | O_DIRECTORY, x86_64 layout): no filesystem here
    // can create an unnamed inode that linkat(2) later names. Linux answers
    // EOPNOTSUPP for such a filesystem and every caller (ostree/glnx, glibc
    // tmpfile, Python tempfile) then falls back to a named temp file plus
    // rename. Before 2026-11-19 f2fs opened the directory read-write instead,
    // so ostree's linkat(/proc/self/fd/N) failed ENOENT ("Creating repo: linkat").
    if flags & 0o20200000 == 0o20200000 { return err_reply(-95); }
    let flags = if flags & O_PATH != 0 {
        flags & !(O_WRONLY | O_RDWR | O_CREAT | O_TRUNC | O_APPEND | O_EXCL)
    } else {
        flags
    };
    let mut path = &pbuf[..plen];

    // Basic normalization: . to /, strip trailing slash
    if path == b"." {
        path = b"/";
    } else {
        path = strip_trailing_slash(path);
    }

    // /dev/mqueue/<name>: an open of a POSIX message queue (mqueue mount).
    if let Some(name) = mqueue::path_name(path) {
        let r = mqueue::open_path(pid, name, flags);
        return if r < 0 { err_reply(r as i32) } else { val_reply(r as u64) };
    }

    // `/proc/<pid>/root[/...]` (and `/proc/self/root`) of a live process is
    // that process's root directory. LeandrOS has one filesystem namespace,
    // so it is "/" itself. xdg-desktop-portal opens it for every caller to
    // look for `<root>/.flatpak-info` (absent => a host app); with ENOENT it
    // refused every portal call ("Unable to open /proc/<pid>/root").
    // Mount servers re-read the path from the pointer they are handed, so a
    // rewritten path is forwarded from `root_buf` (NUL-terminated).
    let mut root_buf = alloc::vec![0u8; HOST_PATH_MAX];
    let mut fwd_ptr = path_ptr;
    if let Some(rest) = proc_pid_root_rest(path) {
        fwd_ptr = root_buf.as_ptr() as usize;
        root_buf[0] = b'/';
        let rest = rest.strip_prefix(b"/").unwrap_or(rest);
        let n = rest.len().min(root_buf.len() - 2);
        root_buf[1..1 + n].copy_from_slice(&rest[..n]);
        path = strip_trailing_slash(&root_buf[..1 + n]);
        if path.is_empty() { path = b"/"; }
    } else if proc_pid_root_dead(path) {
        return err_reply(-2); // ENOENT: no such process
    }

    // Device cgroup (BPF_CGROUP_DEVICE), before any driver open runs. On-disk
    // and tmpfs device nodes re-enter here through `devnode_open` with the
    // canonical /dev name, so they are checked on that pass. O_PATH is skipped.
    if flags & O_PATH == 0 && sched::bpfcg::ATTACHED.load(core::sync::atomic::Ordering::Acquire) != 0 {
        if let Some((is_blk, maj, min)) = open_dev_identity(path) {
            let acc = match flags & 3 { 0 => 2, 1 => 4, _ => 6 }; // READ / WRITE / both
            if sched::bpfcg::dev_check(pid, is_blk, maj, min, acc).is_err() {
                return err_reply(-1); // EPERM
            }
        }
    }

    // A mapped tmpfs file that O_TRUNC emptied (its VMO id): every mapping
    // drops its pages after the open, with no VFS lock held.
    let mut trunc_zap: Option<u64> = None;
    let kind = if let Some(lookup_path) = should_lookup_ramfs(path) {
        if lookup_path == b"/dev/null" {
            VnodeKind::DevNull
        } else if lookup_path == b"/dev/zero" {
            VnodeKind::DevZero
        } else if lookup_path == b"/dev/full" {
            VnodeKind::DevFull
        } else if lookup_path == b"/dev/urandom" || lookup_path == b"/dev/random" {
            VnodeKind::DevUrandom
        } else if lookup_path == b"/dev/stdin" {
            VnodeKind::DevStdio { target_fd: 0 }
        } else if lookup_path == b"/dev/stdout" {
            VnodeKind::DevStdio { target_fd: 1 }
        } else if lookup_path == b"/dev/stderr" {
            VnodeKind::DevStdio { target_fd: 2 }
        } else if lookup_path == b"/dev/tty" {
            // The caller's *controlling* terminal — which for anything running
            // under a terminal emulator is its pty slave, not the machine
            // console. Resolving it to the console unconditionally is what a
            // shell on a pty cannot survive: crossterm (reedline, brush) opens
            // /dev/tty for its raw-mode handle and writes the ESC[6n cursor
            // query there, so the prompt was painted on the framebuffer while
            // the emulator holding the master saw nothing.
            //
            // A session whose controlling terminal is the console (the serial
            // login's, see `tty_server::console_is_ctty_of`) gets the console
            // object: the same one a dup'd stdin gives, and the reason a
            // plain empty RamFile here used to return instant EOF and starve
            // crossterm's input reader.
            //
            // Anything else has no controlling terminal — a daemon, a
            // `setsid` child, a session whose pty hung up — and gets Linux's
            // ENXIO. Until lane procfix (2026-10-18) that case got the
            // console proxy too, which answered termios from whatever fd 0
            // was: with stdin a pipe, sudo-rs "had a terminal" whose every
            // termios call was ENOTTY (it carried a patch for it).
            let sid = sched::current_sid();
            match tty_server::pty::ctty_for_sid(sid) {
                Some(n) => {
                    if let Err(e) = tty_server::pty::slave_open(n) { return err_reply(e); }
                    VnodeKind::Pty { pair: n as u16, is_master: false }
                }
                None if tty_server::console_is_ctty_of(sid) => VnodeKind::DevStdio { target_fd: 0 },
                None => return err_reply(-6), // ENXIO
            }
        } else if lookup_path == b"/dev/console" {
            // Always the machine console, never a pty: /dev/console is the
            // system's console device, not "whatever terminal I happen to be
            // on". init and the getty write here on purpose.
            VnodeKind::DevStdio { target_fd: 0 }
        } else if let Some(vt) = vt_number(lookup_path).filter(|&v| v >= 2) {
            // VT 2..6 carries a text session of its own — a pty pair whose
            // master is the kernel (see "Text sessions" in
            // servers/tty/src/vt.rs). The fd is that pair's slave, so termios,
            // job control, TIOCSCTTY and `/dev/tty` resolution are the pty
            // pool's, and the VT/KD ioctls are routed to this VT by sys_ioctl.
            match tty_server::vt::session_open(vt as usize) {
                Ok(pair) => VnodeKind::Pty { pair: pair as u16, is_master: false },
                Err(e) => return err_reply(e),
            }
        } else if let Some(vt) = vt_number(lookup_path) {
            // A virtual console. `seen` is snapshotted at open so a fresh
            // /dev/tty0 is NOT immediately notification-readable: the contract
            // is "the active VT changed since *this* open last looked", and an
            // open that has never looked has to start from the truth on screen,
            // or every consumer would process one phantom switch at start-up.
            VnodeKind::DevVt { vt, seen: tty_server::vt::active() as u8 }
        } else if lookup_path == b"/dev/fb0" {
            if !xattr::may_access(&fb_meta(), &cred_of(pid), None, open_mask(flags)) {
                return err_reply(-13); // EACCES
            }
            VnodeKind::DevFb { pos: 0 }
        } else if cgroupfs::is_path(lookup_path) {
            match cgroupfs::open(pid, lookup_path, flags) {
                cgroupfs::Open::Err(e) => return err_reply(e),
                cgroupfs::Open::Dir(cg) => VnodeKind::SysBlock { dev: cgroupfs::DEV_BASE | cg, level: cgroupfs::LVL, pos: 0 },
                cgroupfs::Open::Read(d) => match proc_snapshot(cgroupfs::SNAP_R, &d, false) {
                    Some(v) => v,
                    None => return err_reply(-23),
                },
                cgroupfs::Open::Write(cg, f, d) => match gen_cgroup_writable(cg, f, &d) {
                    Some(v) => v,
                    None => return err_reply(-23),
                },
            }
        } else if is_drm_sysfs_override(lookup_path) {
            if flags & (O_WRONLY | O_RDWR) != 0 { return err_reply(-13); } // EACCES
            match gen_drm_sysfs(lookup_path) {
                Some(v) => v,
                None => return err_reply(-2),
            }
        } else if sysclass::is_tables_dir(lookup_path) {
            if flags & (O_WRONLY | O_RDWR) != 0 { return err_reply(-21); } // EISDIR
            VnodeKind::SysBlock { dev: sysclass::SC_TABLES, level: sysclass::LVL_SC_CLASS, pos: 0 }
        } else if let Some(t) = sysclass::acpi_table(lookup_path) {
            // Raw firmware tables are root-only, as on Linux (mode 0400).
            if flags & (O_WRONLY | O_RDWR) != 0 || !sched::caps::host_capable_of(pid, sched::caps::CAP_DAC_OVERRIDE) { return err_reply(-13); }
            let mut tmp = TMP_FILES.lock();
            let Some(idx) = tmp_free_slot(&mut *tmp) else { return err_reply(-23) };
            tmp.claim(idx, b"", true);
            let fake = alloc::format!("/tmp/.acpitbl_{}", idx);
            tmp.set_path(idx, fake.as_bytes());
            if !tmp_fill_kernel(&mut tmp, idx, t) { tmp.release(idx); return err_reply(-12); }
            VnodeKind::TmpFile { idx, pos: 0, writable: false, ofd: 0 }
        } else if sysclass::is_path(lookup_path) {
            if let Some((dev, level)) = sysclass::dir(lookup_path) {
                if flags & (O_WRONLY | O_RDWR) != 0 { return err_reply(-21); } // EISDIR
                VnodeKind::SysBlock { dev, level, pos: 0 }
            } else if flags & (O_WRONLY | O_RDWR) != 0 {
                if !sysclass_write_ok(pid, lookup_path) { return err_reply(-13); }
                match gen_sysclass_writable(lookup_path) {
                    Some(v) => v,
                    None => return err_reply(-2),
                }
            } else {
                let mut buf = [0u8; TMP_BUF_SIZE];
                match sysclass::attr(lookup_path, &mut buf).and_then(|n| gen_sysfs_bytes(&buf[..n.min(TMP_BUF_SIZE)])) {
                    Some(v) => v,
                    None => return err_reply(-2),
                }
            }
        } else if block::is_sysfs_path(lookup_path) {
            // A directory in the synthesized `/sys/class/block` subtree, or
            // one of its attribute files. Checked before the RAMFS sweep
            // below, which would otherwise answer ENOENT for it (nothing
            // there is static). The rest of /sys is staged on disk and is
            // handled by the ordinary mount lookup, not here.
            if let Some((dev, level)) = block::sysfs_dir(lookup_path) {
                if flags & (O_WRONLY | O_RDWR) != 0 { return err_reply(-21); } // EISDIR
                VnodeKind::SysBlock { dev, level, pos: 0 }
            } else {
                match gen_sysfs(lookup_path) {
                    Some(v) => v,
                    None => return err_reply(-2),
                }
            }
        } else if let Some(dev) = block::lookup_dev_node(lookup_path) {
            // /dev/vda, /dev/vda1, /dev/loopN, /dev/loop-control. Ahead of the
            // RAMFS sweep for the same reason /dev/ptmx is: a placeholder entry
            // there would report a zero-byte regular file.
            //
            // These are root:disk 0660 (what stat reports): raw access to a
            // disk is every file on it, so it is root and group `disk` only —
            // it used to open for anyone, and `head /dev/vdb` as uid 1000 read
            // the root filesystem.
            if !xattr::may_access(&blockdev_meta(dev), &cred_of(pid), None, open_mask(flags)) {
                return err_reply(-13); // EACCES
            }
            let pos = if flags & O_APPEND != 0 { block::dev_size(dev) } else { 0 };
            VnodeKind::BlockDev { dev, pos }
        } else if let Some(inst) = ptmx_inst(lookup_path) {
            // Every open of the multiplexor is a *new* pair, not a new handle
            // on a shared object — that is the whole ptmx contract, and what
            // TIOCGPTN then reports. A devpts instance's ptmx allocates in
            // that instance's number space.
            if inst != 0 && !xattr::may_access(&ptmx_meta(inst), &cred_of(pid), None, open_mask(flags)) {
                return err_reply(-13); // EACCES
            }
            match tty_server::pty::alloc_in(inst) {
                Some(pair) => { pts_meta_init(pair, pid); VnodeKind::Pty { pair: pair as u16, is_master: true } }
                // ENOSPC, as Linux's devpts answers at its pty limit.
                None => { report_pool_full("pty", tty_server::pty::MAX_PTYS); return err_reply(-28) }
            }
        } else if let Some(n) = pts_number(lookup_path) {
            // The slave half. `slave_open` enforces the TIOCSPTLCK lock and the
            // master's existence, so a mis-sequenced open (no `unlockpt`) fails
            // loudly instead of quietly working. The node's owner and mode
            // (`PTS_META`) decide who may open it, as for any device node.
            if !xattr::may_access(&pts_meta(n), &cred_of(pid), None, open_mask(flags)) {
                return err_reply(-13); // EACCES
            }
            if let Err(e) = tty_server::pty::slave_open(n) { return err_reply(e); }
            VnodeKind::Pty { pair: n as u16, is_master: false }
        } else if is_tmp_path(path) && !is_tmpfs_root(path) {
            // ── Writable tmpfs file (/tmp, /dev/shm, /run/user) ──────────────────
            let writable = flags & (O_WRONLY | O_RDWR) != 0;
            let create   = flags & O_CREAT  != 0;
            let trunc    = flags & O_TRUNC  != 0;
            // O_EXCL only has meaning together with O_CREAT, and then it means
            // "fail if the target already exists". It was ignored outright, so
            // the atomic-create-a-lockfile idiom — the whole reason the flag
            // exists — silently succeeded on an existing file and every caller
            // believed it had won the race. mktemp-style flows and several
            // coreutils safety checks are built on it.
            let excl     = create && flags & O_EXCL != 0;
            let accmode  = flags & 0x3;
            let want_read  = accmode != O_WRONLY;
            // O_TRUNC destroys content, so it needs write permission even on
            // an O_RDONLY open (Linux may_open: O_TRUNC implies MAY_WRITE).
            let want_write = accmode == O_WRONLY || accmode == O_RDWR || trunc;
            let cred = cred_of(pid);

            let mut tmp = TMP_FILES.lock();
            // A mkdir'd tmpfs directory opens as a directory vnode — the
            // TmpFile slot it already has, with `pos` doubling as the
            // getdents64 cursor. Before this, the file lookup below skipped
            // is_dir entries entirely, so opendir("/tmp/sub") was ENOENT and
            // O_CREAT on it would have shadowed the directory with a file.
            if let Some(idx) = tmp_find(&tmp, path).filter(|&i| tmp[i].is_dir) {
                if excl { return err_reply(-17); } // EEXIST — beats EISDIR, as on Linux
                if flags & (O_WRONLY | O_RDWR) != 0 { return err_reply(-21); } // EISDIR
                {
                    let (e, xa) = (&tmp[idx], tmp.xattr(idx));
                    let meta = tmp_meta(e);
                    let acl = xattr::find(&xa[..], xattr::IDX_ACL_ACCESS, b"");
                    if !xattr::access_check(&meta, &cred, acl, true, false, false) {
                        return err_reply(-13); // EACCES
                    }
                }
                VnodeKind::TmpFile { idx, pos: 0, writable: false, ofd: 0 }
            } else {
            // Look for an existing entry. A hard-link alias carries no bytes,
            // so the fd must be bound to the slot that owns them — do that
            // once, here, and every downstream read/write/lseek/ftruncate/
            // fstat keeps working on a bare pool index unchanged.
            let existing = tmp_find(&tmp, path).map(|i| tmp_owner(&tmp, i));
            match existing {
                Some(idx) => {
                    // O_CREAT|O_EXCL on an existing file: EEXIST, checked
                    // before access permissions so an unreadable file still
                    // reports "already there" rather than leaking EACCES.
                    if excl { return err_reply(-17); } // EEXIST
                    {
                        let (e, xa) = (&tmp[idx], tmp.xattr(idx));
                        let meta = tmp_meta(e);
                        let acl = xattr::find(&xa[..], xattr::IDX_ACL_ACCESS, b"");
                        if !xattr::access_check(&meta, &cred, acl, want_read, want_write, false) {
                            return err_reply(-13); // EACCES
                        }
                    }
                    if tmp[idx].devtype != 0 {
                        let (blk, rdev) = (tmp[idx].devtype == 2, tmp[idx].rdev);
                        drop(tmp);
                        return devnode_open(pid, blk, rdev, flags, mode);
                    }
                    // Linux `handle_truncate`: any O_TRUNC open strips set-ID bits.
                    if trunc && !tmp[idx].is_link { tmp_kill_setid(&mut tmp, idx, &cred); }
                    if trunc && tmp[idx].len != 0 && !tmp[idx].is_link {
                        let mut vmos = TMP_VMOS.lock();
                        // Mapped: its mappings lose every page once the locks
                        // are dropped (as ftruncate does; see `trunc_zap`).
                        trunc_zap = vmos[idx].as_ref().filter(|v| v.id != 0).map(|v| v.id);
                        let r = tmp_set_size(&mut tmp, &mut vmos[..], idx, 0);
                        drop(vmos);
                        if r != 0 { return make_reply(r); }
                        tmp_touch_mtime(&mut tmp[idx]);
                    }
                    let pos = if writable && trunc { 0 }
                              else if flags & O_APPEND != 0 { tmp[idx].len }
                              else { 0 };
                    VnodeKind::TmpFile { idx, pos, writable: writable || create, ofd: 0 }
                }
                None if create => {
                    // The parent directory must already exist, exactly as on
                    // any real fs: `touch /tmp/nodir/f` is ENOENT, not a file
                    // named "/tmp/nodir/f" that no directory can enumerate —
                    // and the caller needs write + search on it to create.
                    match tmp_parent(path) {
                        Some(p) => if let Err(e) = tmp_dir_may(&tmp, p, &cred,
                                                               xattr::MAY_WRITE | xattr::MAY_EXEC) {
                            return err_reply(e);
                        } else if !cred.owner_mapped() {
                            return err_reply(-75); // EOVERFLOW (fsuidgid_has_mapping)
                        },
                        None => return err_reply(-2), // ENOENT
                    }
                    if path.len() > MAX_TMP_PATH - 1 { return err_reply(-36); }
                    // Allocate a new slot.
                    match tmp_free_slot(&mut *tmp) {
                        Some(idx) => {
                            tmp.claim(idx, path, false);
                            tmp[idx].is_dir   = false;
                            tmp[idx].uid      = cred.euid;
                            tmp[idx].gid      = cred.egid;
                            let (m, um) = xattr::unpack_create_mode(mode as u64);
                            tmp_init_created(&mut tmp, idx, path, m, um, false);
                            VnodeKind::TmpFile { idx, pos: 0, writable: true, ofd: 0 }
                        }
                        None => { drop(tmp); tmpfs_pool_full(); return err_reply(-28) } // ENOSPC: tmpfs full
                    }
                }
                None => return err_reply(-2), // ENOENT
            }
            }
        } else if let Some((tpid, n)) = proc_fdinfo_file(lookup_path, pid) {
            // /proc/<pid>/fdinfo/N: pos, flags, mnt_id of one descriptor.
            if sched::proc_stat_of(tpid).is_none() || sched::leaderless(tpid) { return err_reply(-2); }
            if flags & 0o3 != 0 { return err_reply(-13); }
            if sched::tgid_of(tpid) != sched::tgid_of(pid) && !proc_environ_allowed(pid, tpid) {
                return err_reply(-13);
            }
            match gen_proc_fdinfo(lookup_path, tpid, n) {
                Ok(k) => k,
                Err(e) => return err_reply(e),
            }
        } else if lookup_path == PROC_SELF_FDINFO_DIR {
            if flags & 0o3 != 0 { return err_reply(-21); }
            VnodeKind::RamFile { data: PROC_SELF_FDINFO_DIR, pos: 0, is_dir: true }
        } else if let Some(t) = lookup_path.strip_prefix(b"/proc/.memfd-exe/").and_then(proc_parse_num) {
            // open("/proc/self/exe") of a task running a memfd image.
            match exe_memfd_kind(pid, t, flags) {
                Ok(k) => k,
                Err(e) => return err_reply(e),
            }
        } else if let Some(n) = proc_self_fd_number(lookup_path) {
            // open("/proc/self/fd/N") opens what fd N names afresh, with the
            // new flags — not a dup: its own offset and its own access mode.
            // Firefox makes the read-only half of every shared-memory region
            // this way (a memfd reopened O_RDONLY); with ENOENT it fell back
            // to /dev/shm files whose handles its IPC then lost.
            match reopen_fd_kind(pid, n, flags) {
                Ok(k) => k,
                Err(e) => return err_reply(e),
            }
        } else if let Some((tpid, n)) = proc_pid_fd_number(lookup_path) {
            // open("/proc/<pid>/fd/N") of another process: reopen against the
            // target's table (ptrace-read-like: same euid or root).
            if sched::proc_stat_of(tpid).is_none() || sched::leaderless(tpid) { return err_reply(-2); }
            if sched::tgid_of(tpid) != sched::tgid_of(pid) && !proc_environ_allowed(pid, tpid) {
                return err_reply(-13);
            }
            match reopen_fd_kind_of(pid, tpid, n, flags) {
                Ok(k) => k,
                Err(e) => return err_reply(e),
            }
        } else if lookup_path == PROC_SELF_FD_DIR {
            // The caller's descriptors, one entry each (see handle_getdents64).
            if flags & 0o3 != 0 { return err_reply(-21); } // EISDIR for write opens
            VnodeKind::RamFile { data: PROC_SELF_FD_DIR, pos: 0, is_dir: true }
        } else if flags & 0o3 != 0 && proc_nsmap_target(lookup_path, pid).is_some() {
            // Open /proc/<pid>/{uid_map,gid_map,setgroups} for writing.
            let (tpid, kind) = proc_nsmap_target(lookup_path, pid).unwrap();
            if sched::proc_stat_of(tpid).is_none() { return err_reply(-2); }
            let Some(op) = sched::ns::snapshot(pid, tpid) else { return err_reply(-1) };
            if !sched::ns::open_allowed(&op, kind) { return err_reply(-1); } // EPERM
            match gen_proc_nsmap_writable(tpid, kind, op) {
                Some(v) => v,
                None    => return err_reply(-23),
            }
        } else if let Some((k, id)) = alias::ns_src(lookup_path) {
            // A bind-mounted nsfs file (`ip netns add`, netavark): the mount
            // holds the namespace, each open takes its own reference.
            if flags & 0o3 != 0 { return err_reply(-13); }
            if !sched::ns::hold_checked(k, id) { return err_reply(-2); }
            VnodeKind::NsFd { kind: k, id }
        } else if let Some(r) = proc_ns_file(lookup_path, pid) {
            // /proc/<pid>/ns/<name> followed: an empty nsfs-like file.
            let Some((tpid, k)) = r else { return err_reply(-2) };
            if sched::proc_stat_of(tpid).is_none() { return err_reply(-2); }
            if flags & 0o3 != 0 { return err_reply(-13); }
            // PTRACE_MODE_READ_FSCREDS on the target (Linux proc_ns_get_link).
            let pr = sched::cred::ptrace_read_check(pid, tpid);
            if pr != 0 { return err_reply(pr as i32); }
            // The fd holds the namespace alive (taken atomically with the read).
            match sched::ns::grab(tpid, k) {
                Some(id) => VnodeKind::NsFd { kind: k as u8, id },
                None     => return err_reply(-2),
            }
        } else if let Some(tpid) = proc_ns_dir(lookup_path, pid) {
            // /proc/<pid>/ns: the directory of namespace links.
            if sched::proc_stat_of(tpid).is_none() { return err_reply(-2); }
            if flags & 0o3 != 0 { return err_reply(-21); } // EISDIR for write opens
            match gen_proc_ns_dir(tpid) {
                Some(v) => v,
                None    => return err_reply(-23), // ENFILE: pool full
            }
        } else if flags & 0o3 != 0 && proc_oom_target(lookup_path, pid).is_some() {
            let tpid = proc_oom_target(lookup_path, pid).unwrap();
            if sched::proc_stat_of(tpid).is_none() { return err_reply(-2); }
            if flags & 0o3 == 0o0 { return err_reply(-13); }
            let me = sched::euid_of(pid);
            if !sched::caps::host_capable_of(pid, sched::caps::CAP_DAC_OVERRIDE) && me != sched::euid_of(tpid) { return err_reply(-13); }
            match gen_proc_oom_writable(tpid) {
                Some(v) => v,
                None    => return err_reply(-23),
            }
        } else if flags & 0o3 != 0 && proc_comm_target(lookup_path, pid).is_some() {
            // Open /proc/<tid>/comm for writing: owner (or root) only.
            let tpid = proc_comm_target(lookup_path, pid).unwrap();
            if sched::proc_stat_of(tpid).is_none() { return err_reply(-2); }
            if flags & 0o3 == 0o0 { return err_reply(-13); }
            let me = sched::euid_of(pid);
            if !sched::caps::host_capable_of(pid, sched::caps::CAP_DAC_OVERRIDE) && me != sched::euid_of(tpid) { return err_reply(-13); } // EACCES
            match gen_proc_comm_writable(tpid) {
                Some(v) => v,
                None    => return err_reply(-23),
            }
        } else if let Some(t) = proc_task_parse(lookup_path, pid) {
            // /proc/<pid>/task[/<tid>[/<file>]], /proc/thread-self[/<file>]
            let Some(t) = t else { return err_reply(-2) };
            match open_proc_task(pid, t, flags) {
                Ok(v) => v,
                Err(e) => return err_reply(e),
            }
        } else if lookup_path == b"/proc/self/environ"
               || (lookup_path.ends_with(b"/environ") && proc_pid_path(lookup_path).is_some()) {
            let tpid = if lookup_path.starts_with(b"/proc/self/") { pid }
                       else { proc_pid_path(lookup_path).unwrap() };
            if sched::proc_stat_of(tpid).is_none() { return err_reply(-2); }
            if !proc_environ_allowed(pid, tpid) { return err_reply(-13); }
            match gen_proc_args(tpid, true) {
                Some(v) => v,
                None    => return err_reply(-23),
            }
        } else if lookup_path.starts_with(b"/proc/self/") && lookup_path != b"/proc/self/" {
            let kind = gen_proc_self(pid, pid, lookup_path);
            match kind {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if let Some(tpid) = proc_pid_path(lookup_path) {
            // /proc/<pid>/<file> of another live process (PROC_PID_FILES).
            if sched::proc_stat_of(tpid).is_none() { return err_reply(-2); }
            match gen_proc_self(pid, tpid, lookup_path) {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if let Some(tpid) = if lookup_path == b"/proc/thread-self/fd" || lookup_path == b"/proc/thread-self/fdinfo" { Some(sched::current_pid()) }
                                   else { proc_pid_fd_dir(lookup_path) } {
            // /proc/<pid>/fd of any live process the caller may inspect
            // (own uid or root, like /proc/<pid>/environ).
            if sched::proc_stat_of(tpid).is_none() { return err_reply(-2); }
            if flags & 0o3 != 0 { return err_reply(-21); }
            if !proc_environ_allowed(pid, tpid) { return err_reply(-13); }
            match gen_proc_fd_dir(tpid, lookup_path.ends_with(b"/fdinfo")) {
                Some(v) => v,
                None    => return err_reply(-23),
            }
        } else if let Some(tpid) = proc_pid_dir(lookup_path) {
            if sched::proc_stat_of(tpid).is_none() { return err_reply(-2); }
            if flags & 0o3 != 0 { return err_reply(-21); } // EISDIR for write opens
            match gen_proc_pid_dir(tpid) {
                Some(v) => v,
                None    => return err_reply(-23), // ENFILE: pool full
            }
        } else if lookup_path == b"/proc/vtstat" {
            // VT / DRM-master / evdev-arbitration counters (`vt::stats_text`).
            let mut buf = alloc::vec![0u8; 16384];
            let n = tty_server::vt::stats_text(&mut buf);
            match proc_snapshot(b"/tmp/.vtstat", &buf[..n], false) {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if let Some(v) = proc_net_open(lookup_path) {
            v
        } else if lookup_path == b"/proc/kmemstat" {
            match gen_kmemstat() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/wakestat" {
            match gen_wakestat() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/epollstat" {
            match gen_epollstat() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/pollfds" {
            match gen_pollfds() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/sysstat" {
            match gen_sysstat() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/mntselftest" {
            match gen_mntselftest() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/tryboot" {
            match gen_tryboot() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/rqhold" {
            match gen_rqhold() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/pcsample" {
            match gen_pcsample() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/lockstat" {
            match gen_lockstat() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/i915" {
            match gen_i915() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/klog" {
            match gen_klog() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/pcipm" {
            match gen_pcipm() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/cpufreq" {
            match gen_cpufreq() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/thermal" {
            match gen_thermal() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/drm/fence_stats" {
            extern "C" { fn kernel_drm_fence_stats(buf: *mut u8, cap: usize) -> usize; }
            let mut g = alloc::vec![0u8; 2048];
            let n = unsafe { kernel_drm_fence_stats(g.as_mut_ptr(), g.len()) };
            match proc_snapshot(b"/tmp/.drm_fence_stats", &g[..n], false) {
                Some(v) => v,
                None    => return err_reply(-23),
            }
        } else if lookup_path == b"/proc/drm/fence_hold" {
            // Test knob: root only (CAP_SYS_ADMIN in the initial userns).
            let write = flags & 0o3 != 0;
            if write && !sched::caps::pid_capable_in(pid, 0, 21) { return err_reply(-13); }
            match gen_proc_drm_hold(write) {
                Some(v) => v,
                None    => return err_reply(-23),
            }
        } else if let Some(c) = nslimit_sysctl(lookup_path) {
            // Linux: 0644 root; a write needs CAP_SYS_ADMIN in the initial userns.
            let write = flags & 0o3 != 0;
            if write && !sched::caps::pid_capable_in(pid, 0, 21) { return err_reply(-13); }
            match gen_proc_nslimit(c, write) {
                Some(v) => v,
                None    => return err_reply(-23),
            }
        } else if let Some(k) = inotify_sysctl(lookup_path) {
            // Linux: 0644 root; a write needs CAP_SYS_ADMIN in the initial userns.
            let write = flags & 0o3 != 0;
            if write && !sched::caps::pid_capable_in(pid, 0, 21) { return err_reply(-13); }
            match gen_proc_inotify(k, write) {
                Some(v) => v,
                None    => return err_reply(-23),
            }
        } else if let Some(k) = netctl_slot(sched::ns::netns_of(pid), lookup_path) {
            // Per-netns sysctl; writing needs CAP_NET_ADMIN over the netns owner.
            let write = flags & 0o3 != 0;
            if write && !sched::caps::pid_capable_in(pid, sched::ns::owner_userns(2, sched::ns::netns_of(pid)), 12) {
                return err_reply(-13);
            }
            match gen_proc_netctl(pid, k, write) {
                Some(v) => v,
                None    => return err_reply(-23),
            }
        } else if lookup_path == b"/proc/sys/kernel/hostname" || lookup_path == b"/proc/sys/kernel/domainname" {
            match gen_proc_uts(pid, lookup_path == b"/proc/sys/kernel/domainname") {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/cgroups" {
            match proc_snapshot(b"/tmp/.proc_cgroups", &cgroupfs::proc_cgroups(), false) {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/modules" {
            match gen_modules() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/kmod_initrd" {
            match gen_kmod_initrd() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/kmod_buildid" {
            match gen_kmod_buildid() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/f2fsstat" || lookup_path == b"/proc/f2fsdropcache" {
            match gen_f2fsstat(lookup_path == b"/proc/f2fsdropcache") {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/pcache" {
            match gen_pcache() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/proc/meminfo" || lookup_path == b"/proc/uptime"
               || lookup_path == b"/proc/cmdline" || lookup_path == b"/proc/nvrm_state"
               || lookup_path == b"/proc/loadavg" || lookup_path == b"/proc/stat"
               || lookup_path == b"/proc/self" || lookup_path == b"/proc/mounts"
               || lookup_path == b"/proc/cpuinfo" || lookup_path == b"/proc/interrupts" || lookup_path == b"/etc/resolv.conf"
               || lookup_path == b"/proc/sys/fs/nr_open" || lookup_path == b"/proc/sys/fs/file-max"
               || lookup_path == b"/proc/sys/fs/file-nr"
               || lookup_path == b"/proc/sys/kernel/random/boot_id" || lookup_path == b"/proc/sys/kernel/random/uuid"
               || lookup_path == b"/proc/sys/fs/ovl_selftest" {
            match gen_proc_system(lookup_path, sched::fsuid_of(pid) == 0) {
                Some(v) => {
                    if let VnodeKind::TmpFile { idx, .. } = v {
                        // Same lock order as everywhere: TMP_FILES, then PSYS_ORIGIN. Drop the
                        // entries of snapshots that were closed since (their slot is free or reused).
                        let tmp = TMP_FILES.lock();
                        let mut o = PSYS_ORIGIN.lock();
                        o.retain(|e| e.0 != idx && e.0 < tmp.len() && tmp[e.0].in_use() && tmp[e.0].path().starts_with(b"/tmp/.psys_"));
                        o.push((idx, lookup_path.to_vec()));
                    }
                    v
                }
                None    => return err_reply(-2),
            }
        } else if lookup_path == b"/etc/mtab" {
            // Not under /proc, so it can't go through gen_proc_system/
            // gen_proc_self — handled directly here, ahead of the general
            // RAMFS/initrd/mount-proxy lookup below so a stale static entry
            // (there isn't one, but a future RAMFS addition could shadow it)
            // never wins over the live mount table.
            match gen_etc_mtab() {
                Some(v) => v,
                None    => return err_reply(-2),
            }
        } else {
            // General lookup for RAMFS, initrd, and mounts
            let mut dev_num = None;
            let mut notify_open = false;
            let mut found = {
                let devices = DYNAMIC_DEVICES.lock();
                devices.iter()
                    .find(|d| d.in_use && d.path.as_bytes() == lookup_path)
                    .map(|d| {
                        dev_num = Some((d.major, d.minor));
                        notify_open = d.notify_open;
                        VnodeKind::DynamicDevice { port: d.port, dev_id: d.dev_id, open_id: 0 }
                    })
            };
            // Device node permissions (see dyn_dev_meta): evdev is root:input
            // and the DRM primary node root:video, both 0660, as on Linux.
            if let Some((maj, min)) = dev_num {
                if !xattr::may_access(&dyn_dev_meta(maj, min), &cred_of(pid), None, open_mask(flags)) {
                    return err_reply(-13); // EACCES
                }
            }
            // Claim the open identity out here, NOT inside the `.map()` above:
            // that closure runs while the DYNAMIC_DEVICES guard is still held,
            // and device_open_alloc takes another lock.
            if let Some(VnodeKind::DynamicDevice { port, dev_id, open_id }) = &mut found {
                *open_id = device_open_alloc();
                if *open_id == 0 { return err_reply(-23); } // ENFILE
                // The driver's ->open (register_device_notify_open): it may
                // refuse, e.g. EBUSY from an exclusive device. A refused open
                // never existed, so its identity is released without the
                // VFS_CLOSE a real last close sends.
                if notify_open {
                    let mut m = Message::empty();
                    m.tag = VFS_OPEN;
                    m.data[0..8].copy_from_slice(&(*dev_id as u64).to_le_bytes());
                    m.data[8..16].copy_from_slice(&(flags as u64).to_le_bytes());
                    m.data[24..32].copy_from_slice(&(pid as u64).to_le_bytes());
                    m.data[32..40].copy_from_slice(&(*open_id as u64).to_le_bytes());
                    let r = call_port(*port, m);
                    let rv = i64::from_le_bytes(r.data[0..8].try_into().unwrap_or([0u8; 8]));
                    if rv < 0 {
                        if let Some(i) = device_open_idx(*open_id) { DEVICE_OPEN_REFS.lock()[i] = 0; }
                        return err_reply(rv as i32);
                    }
                }
            }
            if found.is_none() {
                for entry in RAMFS {
                    if lookup_path == entry.path {
                        found = Some(VnodeKind::RamFile { data: entry.data, pos: 0, is_dir: false });
                        break;
                    }
                }
            }
            if found.is_none() {
                for &dir in RAMFS_DIRS {
                    if lookup_path == dir {
                        found = Some(VnodeKind::RamFile { data: dir, pos: 0, is_dir: true });
                        break;
                    }
                }
            }
            if found.is_none() {
                if let Some(sp) = devpts::backing_static(lookup_path) {
                    found = Some(VnodeKind::RamFile { data: sp, pos: 0, is_dir: true });
                }
            }
            if found.is_none() && is_tmp_path(path) {
                found = Some(VnodeKind::RamFile { data: b"/tmp", pos: 0, is_dir: true });
            }
            // Check tmpfs dirs
            if found.is_none() && is_tmp_path(path) {
                let tmp = TMP_FILES.lock();
                if tmp.find(path).is_some_and(|i| tmp[i].is_dir) {
                    found = Some(VnodeKind::DevNull);
                }
            }
            if found.is_none() {
                if let Some(data) = find_in_initrd(lookup_path) {
                    found = Some(VnodeKind::RamFile { data, pos: 0, is_dir: false });
                }
            }
            if found.is_none() {
                if let Some(port) = find_mount_port(path) {
                    let mut proxy = Message::empty();
                    proxy.tag = VFS_OPEN;
                    proxy.data[0..8].copy_from_slice(&(fwd_ptr as u64).to_le_bytes());
                    proxy.data[8..16].copy_from_slice(&(flags as u64).to_le_bytes());
                    // Forward the real creation mode. This used to be a
                    // hardcoded 0, which was invisible only because the f2fs
                    // server ignored the field and created everything 0644.
                    proxy.data[16..24].copy_from_slice(&(mode as u64).to_le_bytes());
                    let reply = call_port(port, proxy);
                    let file_id_raw = i64::from_le_bytes(reply.data[0..8].try_into().unwrap_or([0u8; 8]));
                    if file_id_raw <= -DEVNODE_BASE {
                        let v = (-file_id_raw - DEVNODE_BASE) as u64;
                        return devnode_open(pid, v >> 32 & 1 != 0, v as u32, flags, mode);
                    }
                    if file_id_raw < 0 {
                        return make_reply(file_id_raw);
                    }
                    found = Some(VnodeKind::MountedFile { port, file_id: file_id_raw as u32 });
                }
            }
            match found { Some(v) => v, None => return err_reply(-2) }
        }
    } else {
        // Only mounts should be checked if we are NOT looking up RAMFS
        if let Some(port) = find_mount_port(path) {
            let mut proxy = Message::empty();
            proxy.tag = VFS_OPEN;
            proxy.data[0..8].copy_from_slice(&(fwd_ptr as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&(flags as u64).to_le_bytes());
            // See above: forward the real mode, not 0.
            proxy.data[16..24].copy_from_slice(&(mode as u64).to_le_bytes());
            let reply = call_port(port, proxy);
            let file_id_raw = i64::from_le_bytes(reply.data[0..8].try_into().unwrap_or([0u8; 8]));
            if file_id_raw <= -DEVNODE_BASE {
                let v = (-file_id_raw - DEVNODE_BASE) as u64;
                return devnode_open(pid, v >> 32 & 1 != 0, v as u32, flags, mode);
            }
            if file_id_raw < 0 {
                return make_reply(file_id_raw);
            }
            VnodeKind::MountedFile { port, file_id: file_id_raw as u32 }
        } else {
            return err_reply(-2);
        }
    };

    // A RAMFS pseudo-directory opens read-only, exactly as on Linux: it may be
    // enumerated (opendir/getdents64) but never written. This is also the
    // documented failure mode `O_TMPFILE` callers probe for — tempfile(3) opens
    // its target *directory* with O_RDWR|O_TMPFILE and falls back to a named
    // temp file on EISDIR. We used to succeed that open and hand back the
    // directory's own path string as a readable regular file, which is how
    // `printf '1\n2\n3\n' | tac` came to print "/tmp".
    if let VnodeKind::RamFile { is_dir: true, .. } = kind {
        if flags & (O_WRONLY | O_RDWR) != 0 { return err_reply(-21); } // EISDIR
    }

    if let Some(id) = trunc_zap { sched::zap_backing_all(0, id, 0, u64::MAX); }
    // A /proc/<pid>[/<file>] snapshot is owned as path stat reports it
    // (`task_dump_owner`), so fstat on the fd agrees: newuidmap fstats the
    // /proc/<pid> dirfd it opened and wants the caller's uid.
    if let VnodeKind::TmpFile { idx, .. } = kind {
        if path.starts_with(b"/proc/") {
            if let Some(Some((_, owner, _))) = proc_stat_kind(path) {
                let (u, g) = sched::cred::proc_dump_owner(owner);
                let mut t = TMP_FILES.lock();
                t[idx].uid = u;
                t[idx].gid = g;
            }
        }
    }
    // A regular tmpfs file gets its own open file description, so every
    // dup/fork copy of this fd shares one offset (see `TMP_OFDS`).
    let mut kind = kind;
    if let VnodeKind::TmpFile { idx, pos, ofd, .. } = &mut kind {
        if !TMP_FILES.lock()[*idx].is_dir { *ofd = tmp_ofd_alloc(*pos); }
    }
    // `/dev/stdin|stdout|stderr` name whatever object fd 0/1/2 holds at the
    // moment of the open (Linux reopens it through /proc/self/fd/N; the BSDs
    // dup it, which is what this does — the copy shares the description).
    // Only a raw-console fd 0-2 yields a console proxy. The proxy used to
    // follow the slot forever, so `exec 3>/dev/stdout; exec >FILE` sent fd 3
    // into FILE as well.
    let std_alias = match should_lookup_ramfs(path) {
        Some(b"/dev/stdin")  => Some(0usize),
        Some(b"/dev/stdout") => Some(1usize),
        Some(b"/dev/stderr") => Some(2usize),
        _ => None,
    };
    // A pipe reopened through /proc/<pid>/fd/N is a second endpoint on the
    // same ring: it takes the reference a dup would.
    let mut aliased = matches!(kind, VnodeKind::Pipe { .. })
        && (proc_self_fd_number(path).is_some() || proc_pid_fd_number(path).is_some());
    let installed = {
        let mut tbls = TblSlot::new();
        match get_or_create(pid, &mut tbls) {
            None => Err(-23), // ENFILE: fd-table pool full
            Some(tbl) => {
                if let Some(t) = std_alias {
                    if tbl.fds[t].in_use && !matches!(tbl.fds[t].kind,
                        VnodeKind::DevStdio { .. } | VnodeKind::DevVt { .. } | VnodeKind::None) {
                        kind = tbl.fds[t].kind;
                        aliased = true;
                    }
                }
                match tbl.alloc_fd() {
                    None     => Err(-24),
                    Some(fd) => { tbl.fds.set(fd, FdEntry { kind, flags, in_use: true, ofd: ofd::alloc() }); Ok(fd) }
                }
            }
        }
    };
    if aliased {
        // A second descriptor on fd N's object: take the reference dup takes.
        // On failure nothing was claimed, so there is nothing to release.
        return match installed {
            Ok(fd) => { pipe_ref_inc(&kind); val_reply(fd as u64) }
            Err(e) => err_reply(e),
        };
    }
    match installed {
        Ok(fd) => val_reply(fd as u64),
        Err(e) => {
            // The open identity claimed above has no fd to own it; release it
            // (and tell the server) rather than leaking the slot.
            match kind {
                VnodeKind::DynamicDevice { port, dev_id, open_id } => {
                    device_close(port, dev_id, open_id);
                }
                // Same shape as the DynamicDevice arm: `pty::alloc`/`slave_open`
                // above already took the reference this fd was going to own, and
                // nothing else will ever release it. For a master that leaks the
                // whole pair permanently (alloc sets master_refs = 1).
                VnodeKind::Pty { pair, is_master } => {
                    tty_server::pty::drop_ref(pair as usize, is_master);
                }
                // A mount server allocated a real open-file slot for this
                // open, and that slot is now unreachable: no fd names it, so
                // no close will ever come. The arm was missing, and the leak
                // is a death spiral rather than a slow drift — the pool is
                // per-mount and SYSTEM-WIDE (f2fs::MAX_OPEN_FILES), while the
                // `Err(-24)` above is the *caller's* own fd table filling up.
                // A single process that keeps hitting its own limit therefore
                // burns global slots permanently, one per attempt, until every
                // other process on the volume can no longer open anything.
                //
                // No dup-refcount scan is needed here (unlike handle_close's
                // MountedFile arm): the server hands out a free slot, so this
                // `file_id` is brand new and no other fd can be sharing it —
                // nothing was installed for it to be shared with.
                VnodeKind::MountedFile { port, file_id } => {
                    let mut proxy = Message::empty();
                    proxy.tag = VFS_CLOSE;
                    proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
                    let _ = call_port(port, proxy);
                }
                // No fd owns the description (or, for a /proc snapshot, the
                // ephemeral slot) this open claimed.
                VnodeKind::TmpFile { idx, ofd, .. } => {
                    tmp_ofd_put(ofd);
                    tmp_release_ephemeral(idx);
                }
                _ => {}
            }
            err_reply(e)
        }
    }
}

fn handle_read(pid: u32, fd: usize, buf_ptr: usize, count: usize) -> Message {
    if count == 0 { return val_reply(0); }
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }
    // An O_PATH descriptor carries no access rights — POSIX/Linux answer EBADF.
    if tbl.fds[fd].flags & O_PATH != 0 { return err_reply(-9); }
    let buf = buf_ptr as *mut u8;
    if let VnodeKind::RamFile { data, is_dir, .. } = tbl.fds[fd].kind {
        // read(2) on a directory is EISDIR. For a RAMFS_DIRS entry `data`
        // is the directory's own path, not file content — returning it
        // here leaked that static buffer to userspace as file data.
        if is_dir { return err_reply(-21); } // EISDIR
        // The offset is the description's (shared by dup/fork copies).
        let pos = desc_pos(&tbl.fds[fd]);
        let remaining = data.len().saturating_sub(pos);
        let n = count.min(remaining);
        if n == 0 { return val_reply(0); }
        if unsafe { sched::uaccess::copy_raw(buf, data.as_ptr().add(pos), n) } != 0 { return err_reply(-14); }
        set_dir_pos(&mut tbl.fds[fd], pos + n);
        return val_reply(n as u64);
    }
    match &mut tbl.fds[fd].kind {
        VnodeKind::DevNull =>
            val_reply(0),
        VnodeKind::DevZero | VnodeKind::DevFull => {
            // Whole request in one read, as on Linux (bounded by its
            // MAX_RW_COUNT). The kernel prefaulted the full buffer; the table
            // lock is dropped before the user memory is touched. This used to
            // stop at 4 KiB, so `dd bs=1M` saw only short reads.
            const MAX_RW_COUNT: usize = 0x7fff_f000;
            drop(tbls);
            let n = count.min(MAX_RW_COUNT);
            // Fault-tolerant page-sized copies from the shared zero page: a
            // fault ends the read short (EFAULT if nothing was written).
            let mut done = 0usize;
            while done < n {
                let cnt = (n - done).min(4096);
                if unsafe { sched::uaccess::copy_raw(buf.add(done), TMP_ZERO_PAGE.as_ptr(), cnt) } != 0 {
                    return if done > 0 { val_reply(done as u64) } else { err_reply(-14) };
                }
                done += cnt;
            }
            val_reply(n as u64)
        }
        VnodeKind::DevUrandom => {
            // /dev/urandom and /dev/random: the kernel CSPRNG, the same one
            // getrandom(2) uses (sched::random). Never blocks — it is seeded
            // before userspace starts.
            drop(tbls);
            let n = count.min(4096);
            let mut chunk = [0u8; sched::random::CHUNK];
            let mut done = 0;
            while done < n {
                let k = (n - done).min(chunk.len());
                sched::random::fill(&mut chunk[..k]);
                if unsafe { sched::uaccess::copy_raw(buf.add(done), chunk.as_ptr(), k) } != 0 {
                    return if done == 0 { err_reply(-14) } else { val_reply(done as u64) };
                }
                done += k;
            }
            val_reply(n as u64)
        }
        VnodeKind::DevStdio { .. } => {
            // The console is served by the kernel's fast path (sys_read
            // consults fd_is_console_stdio before routing here). Never
            // re-enter on the `target_fd` slot: it may name a different
            // object now (see `VnodeKind::DevStdio`).
            drop(tbls);
            err_reply(-9)
        }
        VnodeKind::DevVt { vt, seen } => {
            if *vt == 0 {
                // /dev/tty0 is the VT-change notification, not console input —
                // the full contract is on `fd_vt_number`. EAGAIN when nothing
                // changed makes a blocking fd block (the kernel's VFS_READ
                // retry loop) and a non-blocking one drain to EAGAIN, which is
                // what an event loop registered on this fd needs.
                let cur = tty_server::vt::active() as u8;
                if cur == *seen { drop(tbls); return err_reply(-11); } // EAGAIN
                *seen = cur;
                drop(tbls);
                // One byte, whatever `count` was, and only after the fd table
                // lock is released: a demand-paging fault on the user buffer
                // under it re-enters the scheduler (the 82d0cc3 hazard).
                if unsafe { sched::uaccess::copy_raw(buf, &cur, 1) } != 0 { return err_reply(-14); }
                return val_reply(1);
            }
            // /dev/tty1../dev/tty6 read the console, exactly as
            // `DevStdio { target_fd: 0 }` does. Normally unreachable — the
            // kernel's console fast path claims these fds first
            // (`fd_is_console_stdio`) — so this is the same EBADF fallback a
            // /dev/console fd gets when fd 0 is untracked. The proxy exclusion
            // is what stops `dup2(vtfd, 0)` from recursing forever.
            let target_ok = tbl.fds[0].in_use && !matches!(tbl.fds[0].kind,
                VnodeKind::DevStdio { .. } | VnodeKind::DevVt { .. });
            drop(tbls);
            if !target_ok { return err_reply(-9); }
            handle_read(pid, 0, buf_ptr, count)
        }
        VnodeKind::DevFb { pos } => {
            let base = FB_BASE.load(atomic::Ordering::SeqCst);
            if base == 0 { return err_reply(-19); } // ENODEV
            let height = FB_HEIGHT.load(atomic::Ordering::SeqCst) as usize;
            let pitch  = FB_PITCH.load(atomic::Ordering::SeqCst) as usize;
            let total_size = height * pitch;

            let cur = *pos;
            if cur >= total_size { return val_reply(0); }
            let n = count.min(total_size - cur);

            let fb_virt = if base >= 0xFFFF_0000_0000_0000 {
                base as usize + cur
            } else {
                mm::phys_to_virt(base as usize + cur)
            };
            if unsafe { sched::uaccess::copy_raw(buf, fb_virt as *const u8, n) } != 0 {
                return err_reply(-14);
            }
            *pos = cur + n;
            val_reply(n as u64)
        }
        // A block device. The FD table lock is released before any I/O:
        // a loop device's read re-enters the VFS (`call_port` to the mount
        // holding its backing file), and the user buffer is touched with
        // nothing held so a demand-paging fault there resolves normally.
        VnodeKind::BlockDev { dev, pos } => {
            let (d, p) = (*dev, *pos);
            drop(tbls);
            if block::dev_is_char(d) { return val_reply(0); } // /dev/loop-control
            let n = block::read_at(d, p, buf_ptr, count);
            if n < 0 { return make_reply(n as i64); }
            let mut tbls2 = TblSlot::new();
            if let Some(t) = find_tbl(pid, &mut tbls2) {
                if let VnodeKind::BlockDev { pos, .. } = &mut t.fds[fd].kind {
                    *pos = p + n as u64;
                }
            }
            val_reply(n as u64)
        }
        VnodeKind::DynamicDevice { port, dev_id, open_id } => {
            let port = *port;
            let dev_id = *dev_id;
            let open_id = *open_id;
            drop(tbls);
            let mut proxy_msg = Message::empty();
            proxy_msg.tag = VFS_READ;
            proxy_msg.data[0..8].copy_from_slice(&(dev_id as u64).to_le_bytes());
            proxy_msg.data[8..16].copy_from_slice(&(buf_ptr as u64).to_le_bytes());
            proxy_msg.data[16..24].copy_from_slice(&(count as u64).to_le_bytes());
            proxy_msg.data[24..32].copy_from_slice(&(pid as u64).to_le_bytes());
            // Slot 4: which *open* this read is on — the same cookie handle_ioctl
            // already forwards. evdev keys its per-open event queue off it.
            proxy_msg.data[32..40].copy_from_slice(&(open_id as u64).to_le_bytes());
            match call_port(port, proxy_msg) {
                reply => reply,
            }
        }
        VnodeKind::RamFile { .. } => err_reply(-9), // answered above
        VnodeKind::Pipe { ring, is_write: false } => {
            let ring_idx = *ring;
            drop(tbls); // release FD table lock before acquiring pipe lock
            let mut rings = PIPE_RINGS.lock();
            let r = &mut rings[ring_idx];
            if r.count == 0 {
                // No data yet.  Signal the kernel whether to retry:
                //   -11 (EAGAIN) = write end still open → caller should yield and retry
                //    0 (EOF)     = write end closed → caller returns 0
                return if r.writers > 0 { err_reply(-11) } else { val_reply(0) };
            }
            // The write end withholds POLLOUT while fewer than PIPE_BUF bytes
            // are free (see handle_poll's write-end arm and the atomic-write
            // rule in the write arm), so only a drain that crosses that line
            // is a level change.
            let was_full = r.free() < PIPE_BUF;
            let n = match r.copy_out(buf, count.min(PIPE_READ_MAX)) { Ok(n) => n, Err(e) => return err_reply(e) };
            let now_writable = r.free() >= PIPE_BUF;
            // Draining bytes frees ring space → a new POLLOUT edge for the
            // write end. Advance the seq so an epoll writer blocked on a full
            // pipe is re-woken edge-triggered.
            if n > 0 { r.seq = r.seq.wrapping_add(1); }
            drop(rings); // release PIPE_RINGS before waking pollers (K2 lock order)
            // Mirror of the write arm below, and for the same reason: an
            // unconditional `wake_poll` here is a system-wide herd, and a
            // reader draining a pipe in 4 KiB bites pays it on every single
            // read. Only the full → not-full transition actually flips the
            // write end's POLLOUT level, so only that transition can release a
            // poller that parked *because* POLLOUT was false. Every other
            // drain merely advanced `seq`, which matters solely to an EPOLLET
            // interest that is parked with `last_seq == seq`; that is real but
            // rare, so it is deferred and coalesced by the tick rather than
            // charged to every read.
            if n > 0 {
                // Write end only: a drain never makes the read end ready, and
                // the reader, re-parked by the time the deferred wake is paid,
                // would take it as a useless wake (lane panelpipe).
                if was_full && now_writable { sched::wake_poll_key(pipe_wkey(ring_idx)); }
                else { sched::request_poll_wake_key(pipe_wkey(ring_idx)); }
            }
            val_reply(n as u64)
        }
        VnodeKind::Pty { pair, is_master } => {
            let (p, m) = (*pair as usize, *is_master);
            drop(tbls); // the pty pool lock is taken inside; never nest it under FD_TABLES
            // Both directions answer -EAGAIN for "nothing yet" and 0 for a real
            // end-of-input, which is exactly the contract the pipe arm above
            // has with sys_read's retry loop.
            let n = unsafe {
                if m { tty_server::pty::master_read(p, buf, count) }
                else { tty_server::pty::slave_read(p, buf, count) }
            };
            make_reply(n as i64)
        }
        VnodeKind::TmpFile { .. } => {
            let kind = tbl.fds[fd].kind;
            let idx = tmp_idx_of(&kind);
            let cur = tmp_pos_get(&kind);
            drop(tbls);
            let mut tmp = TMP_FILES.lock();
            // `entry.len` is the EOF for every regular file. A read is served
            // whole (up to TMP_READ_MAX per call, which bounds how long the
            // tmpfs locks are held): a short read from a regular file before
            // EOF is legal but breaks callers that size one read() by st_size.
            const TMP_READ_MAX: usize = 1 << 20;
            let remaining = tmp[idx].len.saturating_sub(cur);
            let mut n = count.min(remaining).min(TMP_READ_MAX);
            if n == 0 { return val_reply(0); }
            // The VMO frames ARE the file, so read()↔mmap coherence is free;
            // holes (and a file with no VMO yet) read as zeros.
            let vmos = TMP_VMOS.lock();
            if let Some(vmo) = vmos[idx].as_ref().filter(|v| v.borrowed) {
                // Never read past the frames a borrowed dmabuf export actually
                // has: a host-side BO's `len` is the resource size while its
                // `pages` is empty, and it has no bytes to give — EOF, not zeros.
                n = n.min((vmo.pages.len() * 4096).saturating_sub(cur));
                if n == 0 { return val_reply(0); }
            }
            if !unsafe { vmo_copy_out(vmos[idx].as_ref(), cur, buf, n) } { return err_reply(-14); }
            drop(vmos);
            // relatime: a real read happened, so consider bumping atime.
            let now = sched::clock_ts();
            if relatime_needs_update(tmp[idx].atime, tmp[idx].mtime, tmp[idx].ctime, now) {
                tmp[idx].atime = now;
            }
            drop(tmp);
            let mut tbls2 = TblSlot::new();
            if let Some(tbl2) = find_tbl(pid, &mut tbls2) {
                if tbl2.fds.used(fd) {
                    tmp_pos_set(&mut tbl2.fds[fd].kind, cur + n);
                }
            }
            val_reply(n as u64)
        }
        VnodeKind::EventFd { slot } => {
            let slot = *slot;
            drop(tbls);
            if count < 8 { return err_reply(-22); } // EINVAL
            let sem = EVENTFD_SEM.lock()[slot];
            let mut counters = EVENTFD_COUNTERS.lock();
            let mut val = counters[slot];
            if val == 0 { return err_reply(-11); } // EAGAIN
            if sem { val = 1; counters[slot] -= 1; } else { counters[slot] = 0; }
            drop(counters);
            // The counter dropped: wake writers/pollers keyed on this slot.
            sched::wake_poll_key(eventfd_key(slot));
            if unsafe { sched::uaccess::copy_raw(buf, &val as *const u64 as *const u8, 8) } != 0 { return err_reply(-14); }
            val_reply(8)
        }
        VnodeKind::Mq { slot } => {
            let slot = *slot;
            let o = tbl.fds[fd].ofd;
            drop(tbls);
            let txt = mqueue::qsize_text(slot);
            let pos = ofd::pos(o).unwrap_or(0) as usize;
            if pos >= txt.len() { return val_reply(0); }
            let n = (txt.len() - pos).min(count);
            if unsafe { sched::uaccess::copy_raw(buf, txt[pos..].as_ptr(), n) } != 0 { return err_reply(-14); }
            ofd::set_pos(o, (pos + n) as u64);
            val_reply(n as u64)
        }
        VnodeKind::TimerFd { slot } => {
            let slot = *slot;
            drop(tbls);
            if count < 8 { return err_reply(-22); } // EINVAL
            let exp = timerfd_poll_expirations(slot);
            {
                // A wall-clock step cancelled a CANCEL_ON_SET timer: this
                // read reports it (and consumes any expirations with it),
                // as Linux's timerfd_read does (`ticks = 0`, -ECANCELED).
                let mut pool = TIMERFD_POOL.lock();
                if pool[slot].canceled {
                    pool[slot].canceled = false;
                    pool[slot].expirations = 0;
                    return err_reply(-125); // ECANCELED
                }
            }
            if exp == 0 { return err_reply(-11); } // EAGAIN
            TIMERFD_POOL.lock()[slot].expirations = 0;
            if unsafe { sched::uaccess::copy_raw(buf, &exp as *const u64 as *const u8, 8) } != 0 { return err_reply(-14); }
            val_reply(8)
        }
        VnodeKind::MountedFile { port, file_id } => {
            let port = *port; let file_id = *file_id;
            drop(tbls);
            let mut proxy = Message::empty();
            proxy.tag = VFS_READ;
            proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&(buf_ptr as u64).to_le_bytes());
            proxy.data[16..24].copy_from_slice(&(count as u64).to_le_bytes());
            call_port(port, proxy)
        }
        VnodeKind::SignalFd { mask, ofd } => {
            let mask = signalfd_mask(*ofd, *mask);
            drop(tbls);
            if count < 128 { return err_reply(-22); } // EINVAL — buffer too small
            // Accept (dequeue) each pending signal in the mask, emitting a
            // 128-byte signalfd_siginfo per signal. Reading consumes the
            // pending bit (POSIX: the signal is accepted, not run through a
            // handler), relying on the caller having blocked it in all threads.
            //
            // The payload fields come from the same per-signal `SigInfo` a
            // handler would have been given, via `accept_pending_signal`, so a
            // process that reaps children through a signalfd and one that
            // installs a SIGCHLD handler read the same `si_code`/`si_pid`/
            // `si_status` for the same death. `ssi_signo` alone (which is all
            // calloop reads) was the previous extent of this.
            //
            // struct signalfd_siginfo, Linux <sys/signalfd.h>:
            //   +0 ssi_signo  +4 ssi_errno   +8 ssi_code  +12 ssi_pid
            //   +16 ssi_uid   +20 ssi_fd     +24 ssi_tid  +28 ssi_band
            //   +32 ssi_overrun +36 ssi_trapno +40 ssi_status ... 128 total
            const SSI_CODE:    usize = 8;
            const SSI_PID:     usize = 12;
            const SSI_UID:     usize = 16;
            const SSI_TID:     usize = 24;
            const SSI_OVERRUN: usize = 32;
            const SSI_STATUS:  usize = 40;
            const SSI_INT:     usize = 44;
            const SSI_PTR:     usize = 48;
            const SSI_ADDR:    usize = 72;
            const SI_QUEUE: i32 = -1;
            const SI_TIMER: i32 = -2;
            const SYNC_FAULTS: u64 = (1 << 3) | (1 << 4) | (1 << 6) | (1 << 7) | (1 << 10);
            if (sched::pending_signals() | sched::shared_pending_signals()) & mask == 0 {
                return err_reply(-11); // EAGAIN
            }
            // One record per accepted INSTANCE, lowest signal first, as
            // Linux's signalfd_read dequeues: a real-time signal queued
            // several times yields several records in one read.
            let mut written = 0usize;
            while written + 128 <= count {
                let pending = (sched::pending_signals() | sched::shared_pending_signals()) & mask;
                if pending == 0 { break; }
                let sig = pending.trailing_zeros() + 1;
                // Consume first: this returns the payload that was attached
                // to the very instance being dequeued.
                let info = sched::accept_pending_signal(sig);
                let mut recbuf = [0u8; 128];
                unsafe {
                    let rec = recbuf.as_mut_ptr();
                    // `buf` is a raw user pointer with no alignment
                    // guarantee beyond the caller's; write unaligned.
                    core::ptr::write_unaligned(rec as *mut u32, sig); // ssi_signo
                    core::ptr::write_unaligned(rec.add(SSI_CODE) as *mut i32, info.si_code);
                    // The siginfo member the code selects (Linux
                    // `signalfd_copyinfo` by `siginfo_layout`).
                    let fault = SYNC_FAULTS & (1u64 << (sig - 1)) != 0 && info.si_code > 0;
                    if fault {
                        core::ptr::write_unaligned(rec.add(SSI_ADDR) as *mut u64, info.si_addr as u64);
                    } else if info.si_code == SI_TIMER {
                        core::ptr::write_unaligned(rec.add(SSI_TID)     as *mut u32, info.si_pid as u32);
                        core::ptr::write_unaligned(rec.add(SSI_OVERRUN) as *mut u32, info.si_uid);
                        core::ptr::write_unaligned(rec.add(SSI_INT)     as *mut i32, info.si_value as i32);
                        core::ptr::write_unaligned(rec.add(SSI_PTR)     as *mut u64, info.si_value);
                    } else {
                        core::ptr::write_unaligned(rec.add(SSI_PID) as *mut u32, info.si_pid as u32);
                        core::ptr::write_unaligned(rec.add(SSI_UID) as *mut u32, sched::ns::view_id(0, info.si_uid));
                        if info.si_code == SI_QUEUE {
                            core::ptr::write_unaligned(rec.add(SSI_INT) as *mut i32, info.si_value as i32);
                            core::ptr::write_unaligned(rec.add(SSI_PTR) as *mut u64, info.si_value);
                        } else {
                            core::ptr::write_unaligned(rec.add(SSI_STATUS) as *mut i32, info.si_status);
                        }
                    }
                }
                if unsafe { sched::uaccess::copy_raw(buf.add(written), recbuf.as_ptr(), 128) } != 0 {
                    return if written == 0 { err_reply(-14) } else { val_reply(written as u64) };
                }
                written += 128;
            }
            val_reply(written as u64)
        }
        VnodeKind::Inotify { inst } => {
            let inst = *inst;
            drop(tbls);
            if inst == 0 { return err_reply(-11); } // silent stub: never any events
            if inotify::is_fan(inst) { return handle_fan_read(pid, inst, buf, count); }
            let mut out: Vec<u8> = Vec::new();
            match inotify::read(inst, count, &mut out) {
                Err(e) => err_reply(e),
                Ok(()) => {
                    if unsafe { sched::uaccess::copy_raw(buf, out.as_ptr(), out.len()) } != 0 { return err_reply(-14); }
                    val_reply(out.len() as u64)
                }
            }
        }
        _ => err_reply(-9),
    }
}

/// Positional read/write on a block-device descriptor: explicit offset, the
/// description's `pos` never read or written, so threads sharing one fd cannot
/// interleave a seek between another's seek and I/O. The access checks are
/// those of the plain read/write arms (`block::read_at`/`write_at` enforce
/// read-only, BLKWRWIN and the loop's rw state).
fn handle_pio(pid: u32, fd: usize, buf_ptr: usize, count: usize, off: u64, write: bool) -> Message {
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }
    if tbl.fds[fd].flags & O_PATH != 0 { return err_reply(-9); }
    let d = match tbl.fds[fd].kind {
        VnodeKind::BlockDev { dev, .. } => dev,
        _ => return make_reply(ERR_NOT_POSITIONAL),
    };
    drop(tbls);
    if count == 0 { return val_reply(0); }
    if block::dev_is_char(d) { return if write { err_reply(-9) } else { val_reply(0) }; }
    let n = if write { block::write_at(d, off, buf_ptr, count) } else { block::read_at(d, off, buf_ptr, count) };
    if n < 0 { return make_reply(n as i64); }
    val_reply(n as u64)
}

/// `ap` is the per-call append override of pwritev2 (arg 3): 1 = RWF_NOAPPEND,
/// 2 = RWF_APPEND, 0 = follow the descriptor's O_APPEND.
fn handle_write(pid: u32, fd: usize, buf_ptr: usize, count: usize, ap: u64) -> Message {
    if count == 0 { return val_reply(0); }
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }
    // See handle_read: O_PATH grants no access rights.
    if tbl.fds[fd].flags & O_PATH != 0 { return err_reply(-9); }
    let buf = buf_ptr as *const u8;
    match &mut tbl.fds[fd].kind {
        VnodeKind::DevFull => err_reply(-28), // ENOSPC
        VnodeKind::DevUrandom | VnodeKind::DevNull | VnodeKind::DevZero =>
            val_reply(count as u64),
        // fanotify permission verdict: struct fanotify_response { s32 fd; u32 response }.
        VnodeKind::Inotify { inst } if inotify::is_perm_group(*inst) => {
            let inst = *inst;
            drop(tbls);
            if count < 8 { return err_reply(-22); }
            let mut b = [0u8; 8];
            if !sched::with_current_address_space(|as_| as_.read_user_buf(buf_ptr, &mut b)).unwrap_or(false) { return err_reply(-14); }
            let fdv = i32::from_le_bytes(b[0..4].try_into().unwrap());
            let resp = u32::from_le_bytes(b[4..8].try_into().unwrap());
            match inotify::fan_verdict(inst, fdv, resp) { Ok(()) => val_reply(8), Err(e) => err_reply(e) }
        }
        // See the read arm: locks dropped before I/O and before the user
        // buffer is read.
        VnodeKind::BlockDev { dev, pos } => {
            let (d, p) = (*dev, *pos);
            drop(tbls);
            if block::dev_is_char(d) { return err_reply(-9); } // EBADF on loop-control
            let n = block::write_at(d, p, buf_ptr, count);
            if n < 0 { return make_reply(n as i64); }
            let mut tbls2 = TblSlot::new();
            if let Some(t) = find_tbl(pid, &mut tbls2) {
                if let VnodeKind::BlockDev { pos, .. } = &mut t.fds[fd].kind {
                    *pos = p + n as u64;
                }
            }
            val_reply(n as u64)
        }
        VnodeKind::Pty { pair, is_master } => {
            let (p, m) = (*pair as usize, *is_master);
            drop(tbls);
            // Which direction a write takes is the whole difference between the
            // two ends: a master write runs the *input* discipline (ISIG,
            // canonical editing, echo), a slave write only OPOST.
            let n = unsafe {
                if m { tty_server::pty::master_write(p, buf, count) }
                else { tty_server::pty::slave_write(p, buf, count) }
            };
            make_reply(n as i64)
        }
        VnodeKind::Pipe { ring, is_write: true } => {
            let ring_idx = *ring;
            drop(tbls);
            let mut rings = PIPE_RINGS.lock();
            let r = &mut rings[ring_idx];
            if r.readers == 0 { return err_reply(-32); } // EPIPE
            // Captured before the put loop: an EMPTY ring is the only state in
            // which the read end withholds POLLIN, so empty → non-empty is the
            // only write that changes the read end's readiness *level*. See the
            // wake block below.
            let was_empty = r.count == 0;
            // PIPE_BUF atomicity: a write of at most PIPE_BUF bytes goes in
            // whole or not at all (EAGAIN; the kernel's blocking retry parks
            // the writer until the reader has freed enough). Larger writes
            // fill whatever is free and return the partial count; the kernel's sys_write
            // loops on blocking fds until all bytes are in (POSIX).
            let free = r.free();
            let mut n = 0usize;
            if count > PIPE_BUF || free >= count {
                n = match r.copy_in(buf, count) { Ok(n) => n, Err(e) => return err_reply(e) };
            }
            if n > 0 { r.seq = r.seq.wrapping_add(1); } // new readable edge for the read end
            let no_space = n == 0 && count > 0;
            drop(rings); // release PIPE_RINGS before waking pollers (K2 lock order)
            // A full ring must report EAGAIN, never a zero-length write: Rust's
            // `write_all` maps Ok(0) to ErrorKind::WriteZero and gives up, so a
            // pipeline moving more than PIPE_RING_SIZE bytes failed outright
            // instead of applying backpressure. The kernel's sys_write blocks
            // and retries on EAGAIN for blocking fds (mirroring sys_read).
            if no_space { return err_reply(-11); } // EAGAIN
            // A reader parked in poll/epoll_wait on the read end has a new
            // POLLIN edge (K2 real blocking — the seq bump above alone no
            // longer suffices once the reader actually sleeps).
            //
            // THE HERD. `sched::wake_poll` is not a targeted wake: there is one
            // global POLL_WAIT_CHANNEL, so it wakes EVERY parked poller in the
            // system, each of which then re-probes its entire interest set
            // through the global FD_TABLES / PIPE_RINGS locks. Calling it
            // unconditionally charges that to every write(2), however small.
            // Rust's `std::io::Stderr` is unbuffered, so tracing's fmt layer
            // emits one write per format fragment: cosmic-comp's ~4.4 KB of
            // startup logging is thousands of writes down a launch-pad pipe,
            // thousands of herds, and a cosmic-session handshake that took
            // ~40 s on 4 vCPUs against ~2.4 s with the same output redirected
            // to a file (the file path calls wake_poll never).
            //
            // WHY THE EDGE IS ENOUGH. A poller only ever parks after its
            // re-probe said "not ready": epoll_wait's three-phase block
            // (kernel/src/syscall.rs ~:7156) publishes Blocked, RE-probes, and
            // cancels if anything is ready, so a reader cannot be asleep on a
            // ring it has been told holds data. A blocking read()er likewise
            // only returns to its retry loop on EAGAIN, which the arm above
            // answers only when `count == 0`. So a parked POLLIN waiter implies
            // an empty ring, and `was_empty` is exactly the transition that can
            // release one.
            //
            // WHY THE OTHER CASE IS DEFERRED, NOT DROPPED. One poller CAN park
            // with data queued: an EPOLLET interest that was already delivered
            // for the current `seq` does not re-fire until `seq` moves again
            // (`fire = cur != 0 && seq != last_seq`), so it can re-enter
            // epoll_wait while the ring is non-empty. A write that leaves the
            // ring non-empty still moves `seq` and so still owes that interest
            // a wake. `request_poll_wake` hands it to the 100 Hz tick, which
            // collapses any number of such writes into one herd and bounds the
            // extra latency at ~10 ms. Dropping it instead would be a real lost
            // wake: POLL_SAFETY_WAKE is off and an epoll_wait(-1) waiter has no
            // deadline, so nothing else would ever wake it.
            if n > 0 {
                if was_empty { sched::wake_poll_key(pipe_rkey(ring_idx)); }
                else         { sched::request_poll_wake_key(pipe_rkey(ring_idx)); }
            }
            val_reply(n as u64)
        }
        VnodeKind::TmpFile { writable, .. } => {
            if !*writable { return err_reply(-9); } // not open for writing
            let kind = tbl.fds[fd].kind;
            let idx = tmp_idx_of(&kind);
            let append = match ap { 1 => false, 2 => true, _ => tbl.fds[fd].flags & O_APPEND != 0 };
            drop(tbls);
            let ofd = match kind { VnodeKind::TmpFile { ofd, .. } => ofd, _ => 0 };
            // A writable /proc/<tid>/comm snapshot: the bytes are the new name.
            let mut setid = false;
            let comm_tid = {
                let tmp = TMP_FILES.lock();
                setid = tmp_has_setid(&tmp, idx);
                let e = &tmp[idx];
                if e.in_use() && e.ephemeral() && e.path().starts_with(PROC_COMM_W) {
                    let mut t = 0u32;
                    for &c in &e.path()[PROC_COMM_W.len()..] { t = t * 10 + (c.wrapping_sub(b'0')) as u32; }
                    Some(t)
                } else { None }
            };
            let nsmap = {
                let tmp = TMP_FILES.lock();
                let e = &tmp[idx];
                if e.in_use() && e.ephemeral() && e.path().starts_with(PROC_NSMAP_W) {
                    let p = &e.path()[PROC_NSMAP_W.len()..];
                    let mut t = 0u32;
                    for &c in &p[1..] { t = t * 10 + (c.wrapping_sub(b'0')) as u32; }
                    Some(((p[0] - b'0') as usize, t))
                } else { None }
            };
            if let Some((k, tgid)) = nsmap {
                let n = count.min(512);
                let mut data = [0u8; 512];
                if unsafe { sched::uaccess::copy_raw(data.as_mut_ptr(), buf, n) } != 0 { return err_reply(-14); }
                let g = NSMAP_OPENERS.lock();
                let Some(op) = g[idx].as_ref() else { return err_reply(-1) };
                let r = if k == 2 { sched::ns::write_setgroups(op, pid, &data[..n]) }
                        else { sched::ns::write_map(op, pid, k, &data[..n]) };
                drop(g); let _ = tgid;
                return if r < 0 { err_reply(r as i32) } else { val_reply(count as u64) };
            }
            let cg_w = {
                let tmp = TMP_FILES.lock();
                let e = &tmp[idx];
                if e.in_use() && e.ephemeral() && e.path().starts_with(cgroupfs::SNAP_W) {
                    cgroupfs::parse_snap_w(e.path())
                } else { None }
            };
            if let Some((cg, f)) = cg_w {
                let n = count.min(256);
                let mut data = [0u8; 256];
                if unsafe { sched::uaccess::copy_raw(data.as_mut_ptr(), buf, n) } != 0 { return err_reply(-14); }
                let r = cgroupfs::write(pid, cg, f, &data[..n]);
                return if r < 0 { err_reply(r as i32) } else { val_reply(count as u64) };
            }
            let sc_path = {
                let tmp = TMP_FILES.lock();
                let e = &tmp[idx];
                if e.in_use() && e.ephemeral() && e.path().starts_with(SYSCLASS_W) {
                    let mut p = [0u8; 96]; let rp = &e.path()[SYSCLASS_W.len()..];
                    let l = rp.len().min(96); p[..l].copy_from_slice(&rp[..l]);
                    Some((p, l))
                } else { None }
            };
            if let Some((p, l)) = sc_path {
                let n = count.min(64);
                let mut data = [0u8; 64];
                if unsafe { sched::uaccess::copy_raw(data.as_mut_ptr(), buf, n) } != 0 { return err_reply(-14); }
                let r = sysclass::write(&p[..l], &data[..n]);
                return if r < 0 { err_reply(r) } else { val_reply(count as u64) };
            }
            let netctl = {
                let tmp = TMP_FILES.lock();
                let e = &tmp[idx];
                if e.in_use() && e.ephemeral() && e.path().starts_with(PROC_NETCTL_W) {
                    let mut it = e.path()[PROC_NETCTL_W.len()..].split(|&c| c == b'_').map(|d| d.iter().fold(0usize, |a, &c| a * 10 + c.wrapping_sub(b'0') as usize));
                    let ns = it.next().unwrap_or(0); let k = it.next().unwrap_or(0);
                    Some((ns as u16, k))
                } else { None }
            };
            if let Some((ns, k)) = netctl {
                let n = count.min(63);
                let mut b = [0u8; 63];
                if unsafe { sched::uaccess::copy_raw(b.as_mut_ptr(), buf, n) } != 0 { return err_reply(-14); }
                let r = sched::ns::netctl_set(ns, k, &b[..n]);
                return if r < 0 { err_reply(r as i32) } else { val_reply(count as u64) };
            }
            let nsl_k = {
                let tmp = TMP_FILES.lock();
                let e = &tmp[idx];
                if e.in_use() && e.ephemeral() && e.path().starts_with(PROC_NSLIM_W) {
                    Some((e.path()[PROC_NSLIM_W.len()] - b'0') as usize)
                } else { None }
            };
            if let Some(c) = nsl_k {
                let n = count.min(15);
                let mut b = [0u8; 15];
                if unsafe { sched::uaccess::copy_raw(b.as_mut_ptr(), buf, n) } != 0 { return err_reply(-14); }
                let mut s = &b[..n];
                while let Some((&l, r)) = s.split_last() { if l == b'\n' || l == b' ' { s = r } else { break } }
                if s.is_empty() || !s.iter().all(|c| c.is_ascii_digit()) { return err_reply(-22); }
                let mut v = 0u32;
                for &d in s { v = v.saturating_mul(10).saturating_add((d - b'0') as u32); }
                let r = sched::ns::limit_set(c, v);
                return if r < 0 { err_reply(r as i32) } else { val_reply(count as u64) };
            }
            let drm_hold = {
                let tmp = TMP_FILES.lock();
                let e = &tmp[idx];
                e.in_use() && e.ephemeral() && e.path() == PROC_DRMH_W
            };
            if drm_hold {
                if !sched::caps::pid_capable_in(pid, 0, 21) { return err_reply(-13); }
                let n = count.min(15);
                let mut b = [0u8; 15];
                if unsafe { sched::uaccess::copy_raw(b.as_mut_ptr(), buf, n) } != 0 { return err_reply(-14); }
                let mut s = &b[..n];
                while let Some((&l, r)) = s.split_last() { if l == b'\n' || l == b' ' { s = r } else { break } }
                if s != b"0" && s != b"1" { return err_reply(-22); }
                extern "C" { fn kernel_drm_fence_hold_set(on: u32); }
                unsafe { kernel_drm_fence_hold_set((s == b"1") as u32) };
                return val_reply(count as u64);
            }
            let inot_k = {
                let tmp = TMP_FILES.lock();
                let e = &tmp[idx];
                if e.in_use() && e.ephemeral() && e.path().starts_with(PROC_INOT_W) {
                    Some((e.path()[PROC_INOT_W.len()] - b'0') as usize)
                } else { None }
            };
            if let Some(k) = inot_k {
                let n = count.min(15);
                let mut b = [0u8; 15];
                if unsafe { sched::uaccess::copy_raw(b.as_mut_ptr(), buf, n) } != 0 { return err_reply(-14); }
                let mut s = &b[..n];
                while let Some((&l, r)) = s.split_last() { if l == b'\n' || l == b' ' { s = r } else { break } }
                if s.is_empty() || !s.iter().all(|c| c.is_ascii_digit()) { return err_reply(-22); }
                let mut v = 0usize;
                for &c in s { v = v.saturating_mul(10).saturating_add((c - b'0') as usize); }
                return match inotify::limit_set(k, v) { Ok(()) => val_reply(count as u64), Err(e) => err_reply(e) };
            }
            let oom_tid = {
                let tmp = TMP_FILES.lock();
                let e = &tmp[idx];
                if e.in_use() && e.ephemeral() && e.path().starts_with(PROC_OOM_W) {
                    let mut t = 0u32;
                    for &c in &e.path()[PROC_OOM_W.len()..] { t = t * 10 + (c.wrapping_sub(b'0')) as u32; }
                    Some(t)
                } else { None }
            };
            if let Some(tid) = oom_tid {
                let n = count.min(15);
                let mut b = [0u8; 15];
                if unsafe { sched::uaccess::copy_raw(b.as_mut_ptr(), buf, n) } != 0 { return err_reply(-14); }
                let mut s = &b[..n];
                while let Some((&l, r)) = s.split_last() { if l == b'\n' || l == b' ' { s = r } else { break } }
                let (neg, digs) = match s.first() { Some(b'-') => (true, &s[1..]), _ => (false, s) };
                if digs.is_empty() || !digs.iter().all(|c| c.is_ascii_digit()) { return err_reply(-22); }
                let mut v = 0i32;
                for &c in digs { v = v.saturating_mul(10).saturating_add((c - b'0') as i32); }
                let r = sched::set_oom_score_adj(pid, tid, if neg { -v } else { v });
                return if r < 0 { err_reply(r as i32) } else { val_reply(count as u64) };
            }
            if let Some(tid) = comm_tid {
                let me = sched::euid_of(pid);
                if !sched::caps::host_capable_of(pid, sched::caps::CAP_DAC_OVERRIDE) && me != sched::euid_of(tid) { return err_reply(-13); }
                if sched::proc_stat_of(tid).is_none() { return err_reply(-3); } // ESRCH
                let n = count.min(15);
                let mut name = [0u8; 15];
                if unsafe { sched::uaccess::copy_raw(name.as_mut_ptr(), buf, n) } != 0 { return err_reply(-14); }
                let mut l = n;
                if l > 0 && name[l - 1] == b'\n' { l -= 1; }
                sched::set_comm(tid, &name[..l]);
                return val_reply(count as u64);
            }
            // Looked up before TMP_FILES is retaken, and only for a file
            // that has a set-ID bit to lose.
            let kcred = if setid { cred_of(pid) } else { xattr::Cred::ROOT };
            // RLIMIT_FSIZE (RUN_QUEUE: before the tmpfs locks).
            let fsize = sched::rlimit_of(pid, sched::task::RLIMIT_FSIZE);
            let mut tmp = TMP_FILES.lock();
            if setid { tmp_kill_setid(&mut tmp, idx, &kcred); }
            let mut vmos = TMP_VMOS.lock();
            // The starting offset is read under TMP_FILES and the shared one
            // advanced before it is released, so writers sharing a description
            // (or appending) never land on the same bytes.
            let cur = if append { tmp[idx].len }
                      else if ofd != 0 { TMP_OFDS.lock()[ofd as usize].pos }
                      else { tmp_pos_get(&kind) };
            // EFBIG at the file-size limit, and a write straddling it is cut
            // short there (Linux generic_write_checks).
            if cur >= MAX_TMP_FILE_SIZE { return err_reply(-27); } // EFBIG
            let mut want = count.min(MAX_TMP_FILE_SIZE - cur);
            if fsize != sched::task::RLIM_INFINITY {
                if cur as u64 >= fsize {
                    drop(vmos); drop(tmp);
                    let _ = sched::fsize_check(pid, fsize, cur as u64, want); // SIGXFSZ
                    return err_reply(-27);
                }
                want = want.min((fsize - cur as u64) as usize);
            }
            let len = tmp[idx].len;
            // size= of a tmpfs mount: cap the write at the instance's limit.
            if let Some(n) = mnt::ns0::tmpfs_inst_of(tmp[idx].path()) {
                let limit = tmpfs_inst_attr(n).1;
                if limit != 0 {
                    let used = tmpfs_inst_used_pages(&*tmp, &vmos[..], n);
                    let first = cur / 4096;
                    let last = (cur + want.max(1) - 1) / 4096;
                    let have = |p: usize| vmos[idx].as_ref().map_or(false, |v| v.page(p) != 0);
                    let mut fresh = 0;
                    for p in first..=last { if !have(p) { fresh += 1; } }
                    if used + fresh > limit {
                        // Fit what the remaining room allows, else ENOSPC.
                        let room = limit.saturating_sub(used);
                        let mut fit_end = cur;
                        let mut p = first;
                        let mut r = room;
                        while p <= last {
                            if !have(p) { if r == 0 { break; } r -= 1; }
                            fit_end = (p + 1) * 4096;
                            p += 1;
                        }
                        let allowed = fit_end.saturating_sub(cur).min(want);
                        if allowed == 0 { return err_reply(-28); }
                        want = allowed;
                    }
                }
            }
            let vmo = vmos[idx].get_or_insert_with(|| { let mut v = TmpVmo::new_owned(false); v.len = len; v });
            // Pages are allocated as the bytes land (F_SEAL_WRITE/GROW are out
            // of scope — not enforced here). A borrowed (dmabuf) VMO never
            // grows: a write past the frames the DRM layer lent us is ENOSPC.
            // Writing past EOF leaves a hole; whatever stale bytes sit between
            // the old EOF and `cur` inside an existing page are zeroed first.
            if vmo.is_memfd {
                // memfd seals: WRITE / FUTURE_WRITE refuse every write, GROW
                // refuses one that would extend the file (Linux: EPERM).
                if vmo.seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 && want > 0 {
                    return err_reply(-1);
                }
                if vmo.seals & F_SEAL_GROW != 0 && cur + want > len && want > 0 {
                    return err_reply(-1);
                }
            }
            if cur > len { vmo_zero_range(vmo, len, cur); }
            let n = match unsafe { vmo_copy_in(vmo, cur, buf, want, true) } {
                Ok(n) => n,
                Err(e) => return make_reply(e),
            };
            let new_pos = cur + n;
            if new_pos > len { vmo.len = new_pos; tmp[idx].len = new_pos; }
            tmp_touch_mtime(&mut tmp[idx]);
            if ofd != 0 { TMP_OFDS.lock()[ofd as usize].pos = new_pos; }
            drop(vmos);
            drop(tmp);
            let mut tbls2 = TblSlot::new();
            if let Some(tbl2) = find_tbl(pid, &mut tbls2) {
                if tbl2.fds.used(fd) {
                    tmp_pos_set(&mut tbl2.fds[fd].kind, new_pos);
                }
            }
            val_reply(n as u64)
        }
        VnodeKind::EventFd { slot } => {
            let slot = *slot;
            drop(tbls);
            if count < 8 { return err_reply(-22); } // EINVAL
            let mut addval = 0u64;
            if unsafe { sched::uaccess::copy_raw(&mut addval as *mut u64 as *mut u8, buf, 8) } != 0 { return err_reply(-14); }
            if addval == u64::MAX { return err_reply(-22); } // EINVAL
            let mut counters = EVENTFD_COUNTERS.lock();
            counters[slot] = counters[slot].saturating_add(addval);
            drop(counters);
            let mut seqs = EVENTFD_SEQ.lock();
            seqs[slot] = seqs[slot].wrapping_add(1);
            drop(seqs);
            // eventfd is mio/tokio's runtime waker: a POLLIN edge for whoever
            // is parked in epoll_wait (K2 real blocking).
            sched::wake_poll_key(eventfd_key(slot));
            val_reply(8)
        }
        VnodeKind::DevStdio { .. } => {
            // Console output is served by the kernel's serial fast path
            // (sys_write consults fd_is_console_stdio before routing here);
            // see the read arm for why the `target_fd` slot is not followed.
            drop(tbls);
            err_reply(-9)
        }
        VnodeKind::DevVt { .. } => {
            // Every /dev/ttyN write is a console write, including one to a VT
            // that is not on screen — a deliberate, documented divergence.
            // `vt::console_out` mirrors the console byte stream to the ACTIVE
            // screen and there is no per-VT text sink beneath it, so the honest
            // behaviour is "this goes wherever console output goes" rather than
            // a silent discard that would look like it had been remembered.
            // Same delegation as `DevStdio { target_fd: 0 }`; the kernel's fast
            // path normally claims these fds before the VFS sees them.
            let target_ok = tbl.fds[0].in_use && !matches!(tbl.fds[0].kind,
                VnodeKind::DevStdio { .. } | VnodeKind::DevVt { .. });
            drop(tbls);
            if !target_ok { return err_reply(-9); }
            handle_write(pid, 0, buf_ptr, count, 0)
        }
        VnodeKind::DevFb { pos } => {
            let base = FB_BASE.load(atomic::Ordering::SeqCst);
            if base == 0 { return err_reply(-19); } // ENODEV
            let height = FB_HEIGHT.load(atomic::Ordering::SeqCst) as usize;
            let pitch  = FB_PITCH.load(atomic::Ordering::SeqCst) as usize;
            let total_size = height * pitch;

            let cur = *pos;
            if cur >= total_size { return val_reply(0); }
            let n = count.min(total_size - cur);

            let fb_virt = if base >= 0xFFFF_0000_0000_0000 {
                base as usize + cur
            } else {
                mm::phys_to_virt(base as usize + cur)
            } as *mut u8;

            let ok = sched::with_current_address_space(|as_| {
                unsafe {
                    as_.read_user_buf(buf_ptr, core::slice::from_raw_parts_mut(fb_virt, n))
                }
            }).unwrap_or(false);

            if !ok { return err_reply(-14); } // EFAULT

            *pos += n;

            // Flush any console text still pending, then put the rows this
            // write touched on screen. `fb_flush` alone transfers only the
            // console's own dirty rectangle, so fbdev pixels never reached the
            // host (see `fb_dev_written`).
            extern "C" { fn fb_flush(); fn fb_dev_written(offset: usize, len: usize); }
            unsafe { fb_flush(); fb_dev_written(cur, n); }
            
            val_reply(n as u64)
        }
        VnodeKind::DynamicDevice { port, dev_id, open_id } => {
            let port = *port;
            let dev_id = *dev_id;
            let open_id = *open_id;
            drop(tbls);
            let mut proxy_msg = Message::empty();
            proxy_msg.tag = VFS_WRITE;
            proxy_msg.data[0..8].copy_from_slice(&(dev_id as u64).to_le_bytes());
            proxy_msg.data[8..16].copy_from_slice(&(buf_ptr as u64).to_le_bytes());
            proxy_msg.data[16..24].copy_from_slice(&(count as u64).to_le_bytes());
            proxy_msg.data[24..32].copy_from_slice(&(pid as u64).to_le_bytes());
            // Slot 4: which *open* this write is on, as for read/ioctl/poll.
            proxy_msg.data[32..40].copy_from_slice(&(open_id as u64).to_le_bytes());
            match call_port(port, proxy_msg) {
                reply => reply,
            }
        }
        VnodeKind::MountedFile { port, file_id } => {
            let port = *port; let file_id = *file_id;
            // O_APPEND is a property of *this descriptor*, and the file
            // position of a mounted file lives in the mount server's open-file
            // slot — the VFS has no `pos` of its own to bump here. So the bit
            // travels with every write (arg3) rather than being latched at
            // open: the mount server then positions at EOF at write time,
            // which is what O_APPEND actually means (Linux appends at the
            // moment of the write, not at the moment of the open).
            //
            // Carrying it per write rather than per open is also what keeps
            // `fcntl(fd, F_SETFL, O_APPEND)` working — F_SETFL rewrites these
            // flags long after VFS_OPEN was forwarded, and nothing re-opens
            // the mount-side slot. Before this, every append to an f2fs file
            // wrote from offset 0: `printf x >> f` three times left only the
            // last line, because each open started the server slot at pos 0
            // and nothing ever moved it.
            //
            // Message::empty() zeroes the payload, so any other sender of
            // VFS_WRITE to a mount port defaults to "not appending".
            let append = match ap { 1 => false, 2 => true, _ => tbl.fds[fd].flags & O_APPEND != 0 };
            drop(tbls);
            let mut proxy = Message::empty();
            proxy.tag = VFS_WRITE;
            proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&(buf_ptr as u64).to_le_bytes());
            proxy.data[16..24].copy_from_slice(&(count as u64).to_le_bytes());
            proxy.data[24..32].copy_from_slice(&(append as u64).to_le_bytes());
            call_port(port, proxy)
        }
        _ => err_reply(-9),
    }
}

/// Lane playcost: close/dup counts by descriptor kind (`# fdclose`/`# fddup`
/// rows of `/proc/rqhold`): pipe eventfd timerfd dyndev mounted tmp other,
/// then dyndev closes that reached the driver (last reference).
pub static FDCLOSE: [atomic::AtomicU64; 8] = [const { atomic::AtomicU64::new(0) }; 8];
pub static FDDUP:   [atomic::AtomicU64; 8] = [const { atomic::AtomicU64::new(0) }; 8];
fn fdkind_idx(k: &VnodeKind) -> usize {
    match k {
        VnodeKind::Pipe { .. } => 0, VnodeKind::EventFd { .. } => 1, VnodeKind::TimerFd { .. } => 2,
        VnodeKind::DynamicDevice { .. } => 3, VnodeKind::MountedFile { .. } => 4,
        VnodeKind::TmpFile { .. } => 5, _ => 6,
    }
}

fn handle_close(pid: u32, fd: usize) -> Message {
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }
    
    let kind = tbl.fds[fd].kind;
    FDCLOSE[fdkind_idx(&kind)].fetch_add(1, atomic::Ordering::Relaxed);
    let closed_ofd = tbl.fds[fd].ofd;
    tbl.fds.clear(fd);
    drop(tbls);
    // Last reference anywhere → the epoll layer drops its registrations on it.
    ofd::put(closed_ofd);
    if let Some(key) = lock_key_of(&kind) { release_locks(key, pid); }

    match kind {
        VnodeKind::Pipe { ring, is_write } => {
            pipe_drop_ref(ring, is_write);
        }
        VnodeKind::Pty { pair, is_master } => {
            tty_server::pty::drop_ref(pair as usize, is_master);
        }
        VnodeKind::Mq { slot } => { mqueue::ref_dec(slot, pid); }
        VnodeKind::BpfProg { id } => sched::bpfcg::prog_put(id),
        VnodeKind::NsFd { id, .. } => sched::ns::put_raw(id),
        VnodeKind::EventFd { slot } => {
            // Refcounted: an eventfd is dup-able, so free the pool slot only when
            // the last fd sharing it closes. Freeing early let a later eventfd()
            // reuse the slot while a surviving dup still read/wrote it — two live
            // eventfds aliasing one counter, which deadlocked cosmic-comp's loop.
            let mut refs = EVENTFD_REFS.lock();
            refs[slot] = refs[slot].saturating_sub(1);
            if refs[slot] == 0 {
                EVENTFD_COUNTERS.lock()[slot] = u64::MAX;
                EVENTFD_SEQ.lock()[slot] = 0;
            }
        }
        VnodeKind::TimerFd { slot } => {
            let mut refs = TIMERFD_REFS.lock();
            refs[slot] = refs[slot].saturating_sub(1);
            if refs[slot] == 0 { TIMERFD_POOL.lock()[slot] = TimerFdEntry::free(); }
        }
        VnodeKind::DynamicDevice { port, dev_id, open_id } => {
            device_close(port, dev_id, open_id);
        }
        VnodeKind::MountedFile { port, file_id } => {
            // A dup'd fd shares this (port, file_id), so the mount server's
            // open-file slot must survive until the *last* of them is closed.
            // The just-closed fd was already cleared above, so this scan sees
            // only the survivors. Without it, closing a dup — e.g. the throwaway
            // fd `fdopendir` makes to run one `readdir` — freed the slot out from
            // under the original, and the next fstat/unlink on it came back
            // EBADF. That is what broke `rm -r`, `du` and every fts-style walk.
            let still_referenced = mount_referenced(port, file_id);
            if !still_referenced {
                let mut proxy = Message::empty();
                proxy.tag = VFS_CLOSE;
                proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
                let _ = call_port(port, proxy);
            }
        }
        VnodeKind::TmpFile { idx, ofd, .. } => { tmp_ofd_put(ofd); tmp_release_ephemeral(idx) }
        _ => {}
    }
    ok_reply()
}

fn handle_lseek(pid: u32, fd: usize, offset: i64, whence: u32) -> Message {
    const SEEK_SET: u32 = 0;
    const SEEK_CUR: u32 = 1;
    const SEEK_END: u32 = 2;
    const SEEK_DATA: u32 = 3;
    const SEEK_HOLE: u32 = 4;
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }
    // A directory's cursor and a static file's offset are the description's
    // (`desc_pos`), so the seek moves every dup/fork copy along with it.
    let dpos = desc_pos(&tbl.fds[fd]) as i64;
    match &mut tbl.fds[fd].kind {
        VnodeKind::RamFile { data, .. } => {
            // For a pseudo-directory `pos` is the getdents64 cursor, so
            // lseek(fd, 0, SEEK_SET) still works as rewinddir().
            let len = data.len() as i64;
            let new_pos = match whence {
                SEEK_SET => offset,
                SEEK_CUR => dpos + offset,
                SEEK_END => len + offset,
                _        => return err_reply(-22),
            };
            if new_pos < 0 { return err_reply(-22); }
            set_dir_pos(&mut tbl.fds[fd], new_pos as usize);
            val_reply(new_pos as u64)
        }
        VnodeKind::DevFb { pos } => {
            let height = FB_HEIGHT.load(atomic::Ordering::SeqCst) as usize;
            let pitch  = FB_PITCH.load(atomic::Ordering::SeqCst) as usize;
            let len = (height * pitch) as i64;
            let new_pos = match whence {
                SEEK_SET => offset,
                SEEK_CUR => *pos as i64 + offset,
                SEEK_END => len + offset,
                _        => return err_reply(-22),
            };
            if new_pos < 0 { return err_reply(-22); }
            *pos = new_pos as usize;
            val_reply(new_pos as u64)
        }
        VnodeKind::TmpFile { idx, ofd: tofd, .. } => {
            let idx = *idx;
            // A directory has no tmpfs description: its cursor is the
            // generic description's (`desc_pos`).
            let generic = *tofd == 0 && TMP_FILES.lock()[idx].is_dir;
            let cur = if generic { dpos } else { tmp_pos_get(&tbl.fds[fd].kind) as i64 };
            let tmp = TMP_FILES.lock();
            let file_len = tmp[idx].len as i64;
            // SEEK_DATA / SEEK_HOLE at page granularity, as Linux tmpfs
            // reports them: a hole is an unallocated page, and EOF is the
            // implicit hole every file ends with. Both answer ENXIO for an
            // offset at or past EOF.
            let sparse = if whence == SEEK_DATA || whence == SEEK_HOLE {
                if offset < 0 || offset >= file_len { return err_reply(-6); } // ENXIO
                let vmos = TMP_VMOS.lock();
                let vmo = vmos[idx].as_ref();
                let mut page = offset as usize / 4096;
                let last = (file_len as usize + 4095) / 4096;
                let want_data = whence == SEEK_DATA;
                // Past the page list everything is hole, so the scan never
                // walks a long trailing hole page by page.
                let scan_end = last.min(vmo.map_or(0, |v| v.pages.len()));
                while page < scan_end && (vmo.map_or(0, |v| v.page(page)) != 0) != want_data { page += 1; }
                if page >= scan_end && want_data {
                    return err_reply(-6); // ENXIO: only hole remains
                } else if page >= last {
                    Some(file_len)
                } else {
                    Some(((page * 4096) as i64).max(offset).min(file_len))
                }
            } else { None };
            drop(tmp);
            let new_pos = match whence {
                SEEK_SET => offset,
                SEEK_CUR => cur + offset,
                SEEK_END => file_len + offset,
                SEEK_DATA | SEEK_HOLE => sparse.unwrap_or(file_len),
                _        => return err_reply(-22),
            };
            if new_pos < 0 { return err_reply(-22); }
            if generic { set_dir_pos(&mut tbl.fds[fd], new_pos as usize); }
            else { tmp_pos_set(&mut tbl.fds[fd].kind, new_pos as usize); }
            val_reply(new_pos as u64)
        }
        // A block device is seekable and `SEEK_END` must report its real
        // capacity: the `gpt` crate finds the backup GPT header with
        // `seek(SeekFrom::End(0))` (gpt-4.1.0 header/mod.rs:384) and gives up
        // with "too small for backup" if that answers 0.
        VnodeKind::BlockDev { dev, pos } => {
            let d = *dev;
            let len = block::dev_size(d) as i64;
            let new_pos = match whence {
                SEEK_SET => offset,
                SEEK_CUR => *pos as i64 + offset,
                SEEK_END => len + offset,
                _        => return err_reply(-22),
            };
            if new_pos < 0 { return err_reply(-22); }
            *pos = new_pos as u64;
            val_reply(new_pos as u64)
        }
        // A sysfs directory's `pos` is the getdents cursor, so
        // lseek(fd, 0, SEEK_SET) works as rewinddir().
        VnodeKind::SysBlock { .. } => {
            let new_pos = match whence {
                SEEK_SET => offset,
                SEEK_CUR => dpos + offset,
                SEEK_END => offset,
                _        => return err_reply(-22),
            };
            if new_pos < 0 { return err_reply(-22); }
            set_dir_pos(&mut tbl.fds[fd], new_pos as usize);
            val_reply(new_pos as u64)
        }
        VnodeKind::MountedFile { port, file_id } => {
            let port = *port; let file_id = *file_id;
            drop(tbls);
            let mut proxy = Message::empty();
            proxy.tag = VFS_LSEEK;
            proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&(offset as u64).to_le_bytes());
            proxy.data[16..24].copy_from_slice(&(whence as u64).to_le_bytes());
            call_port(port, proxy)
        }
        // Linux's null/zero/random char devices accept any seek and report
        // offset 0 (`noop_llseek` / `null_lseek`). uutils dd seeks its output
        // even for `of=/dev/null` and aborted with "Invalid seek" on ESPIPE.
        VnodeKind::DevNull | VnodeKind::DevZero | VnodeKind::DevFull | VnodeKind::DevUrandom => val_reply(0),
        _ => err_reply(-29), // ESPIPE — not seekable (pipes, ttys, etc.)
    }
}

/// pipe2(pipefd, flags).
///
/// `flags` carries the caller's O_CLOEXEC/O_NONBLOCK (std's `io::pipe()` always
/// passes O_CLOEXEC). Dropping them used to have two visible consequences:
/// every pipe end leaked across execve, so a reader waiting on a child's stdout
/// never saw EOF once the child inherited a stray copy of the write end; and
/// `fd_nonblock` reported false for a pipe the caller had asked to be
/// non-blocking.
///
/// The two ends are also given real access modes — O_RDONLY on the read end,
/// O_WRONLY on the write end — because `fcntl(F_GETFL)` is how tokio decides
/// whether a pipe fd it was handed may be read or written; with both ends
/// reporting a flat 0 (== O_RDONLY) a `pipe::Sender` was rejected outright with
/// "not in O_WRONLY or O_RDWR access mode".
fn handle_pipe(pid: u32, rfd_ptr: usize, wfd_ptr: usize, flags: u32) -> Message {
    const O_NONBLOCK: u32 = 0o4000;
    const O_WRONLY:   u32 = 0o1;
    // Only the two flags pipe2 defines are meaningful here; anything else the
    // caller passed is not ours to record.
    let inherited = flags & (O_CLOEXEC | O_NONBLOCK);
    let ring_idx = {
        // The buffer is allocated before the lock: a 16 KiB allocation is
        // not something to do with every pipe user spinning behind us.
        let mut buf = alloc::vec::Vec::new();
        if buf.try_reserve_exact(PIPE_RING_SIZE).is_err() { return err_reply(-23); }
        buf.resize(PIPE_RING_SIZE, 0u8);
        let mut rings = PIPE_RINGS.lock();
        let found = rings.iter().position(|r| r.readers == 0 && r.writers == 0 && r.count == 0 && r.reserved == 0);
        let i = match found {
            Some(i) => i,
            None if rings.len() < MAX_PIPES && rings.try_reserve(1).is_ok() => {
                rings.push(PipeRing::new());
                rings.len() - 1
            }
            None => { drop(rings); report_pool_full("pipe", MAX_PIPES); return err_reply(-23) }
        };
        rings[i].buf = buf;
        rings[i].readers = 1;
        rings[i].writers = 1;
        i
    };
    let mut tbls = TblSlot::new();
    let tbl = match get_or_create(pid, &mut tbls) { Some(t) => t, None => return err_reply(-23) };
    let rfd = match tbl.alloc_fd() { Some(f) => f, None => return err_reply(-24) };
    tbl.fds.set(rfd, FdEntry { kind: VnodeKind::Pipe { ring: ring_idx, is_write: false },
                             flags: inherited, in_use: true, ofd: 0 });
    let wfd = match tbl.alloc_fd() { Some(f) => f, None => {
        tbl.fds.clear(rfd); return err_reply(-24);
    }};
    tbl.fds.set(wfd, FdEntry { kind: VnodeKind::Pipe { ring: ring_idx, is_write: true },
                             flags: inherited | O_WRONLY, in_use: true, ofd: ofd::alloc() });
    tbl.fds[rfd].ofd = ofd::alloc();
    unsafe {
        core::ptr::write(rfd_ptr as *mut u32, rfd as u32);
        core::ptr::write(wfd_ptr as *mut u32, wfd as u32);
    }
    ok_reply()
}

/// True if `fd` (0-2, the range the kernel's sys_read/sys_write fast paths
/// hardwire straight to the serial console) has been explicitly redirected
/// via dup2/dup3 to a real VFS target, e.g. `dup2(pipefd[1], STDOUT_FILENO)`.
/// The kernel consults this before applying the hardwire so a legitimate
/// redirection (Command::output()'s stdout capture, used by crossterm's
/// tput fallback) actually takes effect instead of being silently shadowed.
///
/// A closed fd 0-2 (`close_console`) also answers true: it is no longer the
/// console, and the VFS, which has no entry for it, answers EBADF.
pub fn fd_redirected(pid: u32, fd: usize) -> bool {
    let pid = sched::tgid_of(pid); // fd tables are per-process
    if fd >= FD_NUMBER_CEIL { return false; }
    let mut tbls = TblSlot::new();
    match find_tbl(pid, &mut tbls) {
        Some(t) => t.fds[fd].in_use || t.fds.console_closed(fd),
        None    => false,
    }
}

/// close(2) of an fd 0-2 that is still the implicit console (no table entry):
/// from now on the number is closed — reads, writes, fstat and a second close
/// answer EBADF, and the next open/dup/socket may take it, as on Linux.
/// False when `fd` is not the implicit console (the caller then closes it
/// through the VFS as usual).
pub fn close_console(pid: u32, fd: usize) -> bool {
    if fd > 2 { return false; }
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    let tbl = match get_or_create(pid, &mut tbls) { Some(t) => t, None => return false };
    if !tbl.fds.implicit_console(fd) { return false; }
    tbl.fds.con_closed |= 1 << fd;
    true
}

/// dup2(oldfd, newfd) / dup3(oldfd, newfd, flags).
///
/// `cloexec` comes from dup3's flags argument and is the ONLY thing that
/// decides whether the new descriptor is close-on-exec. POSIX is explicit that
/// the duplicate does not inherit FD_CLOEXEC from `oldfd`: plain dup2 always
/// clears it.
///
/// Copying the whole FdEntry (flags included) used to violate that, which is
/// precisely how a working pipeline still ended up on the console. std's
/// `try_clone` hands the shell an O_CLOEXEC descriptor, posix_spawn's file
/// actions dup2 it onto the child's fd 1, the flag rode along, and execve's
/// close-on-exec sweep then closed the very descriptor the redirection had just
/// installed — so the child wrote to the console, and a child whose stdin was
/// closed the same way blocked forever on console input and wedged the shell.
fn handle_dup2(pid: u32, oldfd: usize, newfd: usize, cloexec: bool) -> Message {
    if oldfd >= FD_NUMBER_CEIL || newfd >= FD_NUMBER_CEIL { return err_reply(-9); }
    let mut tbls = TblSlot::new();
    let tbl = match get_or_create(pid, &mut tbls) { Some(t) => t, None => return err_reply(-23) };
    // A target at or past RLIMIT_NOFILE is EBADF (Linux), even for an open
    // oldfd.
    if newfd >= tbl.cap() { return err_reply(-9); }
    if !tbl.fds[oldfd].in_use {
        // Untracked fd 0-2 = raw console — same implicit /dev/stdio proxy
        // rule as handle_alloc_fd (e.g. dup2(dup-of-stdout, 1) round trips).
        // A closed one is EBADF like any other free number.
        if !tbl.fds.implicit_console(oldfd) { return err_reply(-9); }
        if oldfd != newfd {
            let replaced = tbl.fds.get(newfd).map(|e| (e.kind, e.ofd));
            tbl.fds.set(newfd, FdEntry {
                kind:   VnodeKind::DevStdio { target_fd: oldfd },
                flags:  if cloexec { O_CLOEXEC } else { 0 },
                in_use: true,
                ofd:    ofd::alloc(),
            });
            drop(tbls);
            if let Some((old, old_ofd)) = replaced { pipe_ref_dec(&old); ofd::put(old_ofd); }
            return val_reply(newfd as u64);
        }
        return val_reply(newfd as u64); // dup2(console, itself)
    }
    if oldfd == newfd { return val_reply(newfd as u64); } // dup2(fd, fd) is a no-op
    // A socket/epoll entry is duplicated by the kernel (a new handle per
    // entry); copying it here would make two entries share one handle.
    if handle_of(&tbl.fds[oldfd].kind).is_some() { return err_reply(-9); }
    // dup2 silently closes newfd first if it was open; drop its pipe ref.
    let replaced = tbl.fds.get(newfd).map(|e| (e.kind, e.ofd));
    let dupled = tbl.fds[oldfd].kind;
    let shared = tbl.fds[oldfd].ofd;
    { let e = tbl.fds[oldfd]; tbl.fds.set(newfd, e); }
    // The duplicate never inherits close-on-exec — only dup3's own flag sets it.
    if cloexec { tbl.fds[newfd].flags |= O_CLOEXEC; }
    else       { tbl.fds[newfd].flags &= !O_CLOEXEC; }
    drop(tbls);
    // Reference the new copy before dropping the replaced one: dup2(a, b) where
    // b already names a's description must not see it die in between.
    ofd::get(shared);
    if let Some((old, old_ofd)) = replaced { pipe_ref_dec(&old); ofd::put(old_ofd); }
    pipe_ref_inc(&dupled); // newfd is a second fd on the same pipe endpoint
    val_reply(newfd as u64)
}

/// A file description in flight over an AF_UNIX SCM_RIGHTS control message.
///
/// The net server (which owns the byte stream and the message boundaries the
/// fds ride with) cannot touch the per-process fd tables — those live here.
/// So SCM_RIGHTS fd passing is split: `export_fd` lifts an fd out of the
/// sender's table into one of these (taking an in-flight reference so the
/// underlying object survives the sender closing its fd while the descriptor
/// is still queued), the net server stows the `TransferFd` alongside the
/// stream, and `import_fd` installs it into the receiver's table on the recv
/// that consumes the carrying byte. `drop_transfer` releases one that was
/// never delivered (control buffer too small → Linux closes it; or the socket
/// was torn down with fds still queued).
///
/// `VnodeKind` is `Copy`, so this is just the sender's fd entry lifted out.
/// This mirrors what `handle_fork_dup` already does across fork (a raw entry
/// copy plus a pipe-endpoint refcount bump) — the same lifetime model, only
/// the destination table belongs to a different, unrelated process.
#[derive(Clone, Copy)]
pub struct TransferFd {
    kind:  VnodeKind,
    flags: u32,
    /// The description, with the in-flight reference `export_fd` took on it.
    ofd:   u32,
}

impl TransferFd {
    /// The open file description this descriptor carries.
    pub fn ofd(&self) -> u32 { self.ofd }
}

/// Readiness of a descriptor queued in an SCM_RIGHTS message, as
/// (revents, edge seq, wake tag) — see `net_server::inflight_vfs`.
pub fn poll_transfer(tf: &TransferFd, pid: u32) -> (u32, u64, u64) {
    poll_vnode(&tf.kind, pid)
}

/// Lift `fd` out of `pid`'s table into a `TransferFd`, taking an in-flight
/// reference on the underlying object. Returns None (→ EBADF) for a closed or
/// out-of-range fd, or for the untracked console fds 0-2 (passing stdio over
/// SCM_RIGHTS is not needed by Wayland/D-Bus; see the K1 report).
pub fn export_fd(pid: u32, fd: usize) -> Option<TransferFd> {
    // The copy leaves the table here and is counted in flight below: a scan
    // that decides an object is unreferenced must not run in between.
    let (kind, flags, desc) = {
        let mut tbls = TblSlot::new();
        let e = find_tbl(pid, &mut tbls)?.fds.get(fd)?;
        // A socket is exported by the net server from its own table, and an
        // epoll fd cannot be passed (its holders are tracked per process).
        if handle_of(&e.kind).is_some() { return None; }
        (e.kind, e.flags, e.ofd)
    };
    // SCM_RIGHTS passes the description itself, not a new open.
    ofd::get(desc);
    // Second reference held by the queued descriptor: a pipe endpoint must not
    // reach EOF/EPIPE just because the sender closed its fd before the peer
    // recv'd.
    pipe_ref_inc(&kind);
    // Same reason for a tmpfs/memfd fd, and the reason this is no longer a
    // no-op: its lifetime IS table-scan driven (`tmp_release_ephemeral`) and a
    // queued descriptor is in no table, so without this the sender closing a
    // now-anonymous memfd right after `wl_shm_create_pool` frees the slot before
    // the receiver ever imports it.
    tmp_inflight_inc(&kind);
    Some(TransferFd { kind, flags, ofd: desc })
}

/// A second in-flight reference to what `tf` names, as `export_fd` would take
/// for the same fd. MSG_PEEK on an AF_UNIX stream uses it: the peeked fds are
/// installed while the originals stay queued (Linux: unix_peek_fds). Balanced
/// like any `TransferFd`, by one successful `import_fd` or one `drop_transfer`.
/// Takes only leaf locks, so the net server may call it under UNIX_CONNS.
pub fn clone_transfer(tf: &TransferFd) -> TransferFd {
    ofd::get(tf.ofd);
    pipe_ref_inc(&tf.kind);
    tmp_inflight_inc(&tf.kind);
    *tf
}

/// Install a queued `TransferFd` as a fresh fd in `pid`'s table, consuming the
/// in-flight reference `export_fd` took (the installed fd now owns it — so no
/// extra ref bump). `cloexec` sets FD_CLOEXEC per MSG_CMSG_CLOEXEC. Returns the
/// new fd, or -EMFILE if the table is full.
///
/// **Ownership rule: a `TransferFd` is consumed only on success.** Exactly one
/// of `import_fd`-returning-a-fd or `drop_transfer` balances each `export_fd`.
/// A negative return leaves the descriptor owned by the caller, which must
/// retire it with `drop_transfer` like any other one that did not fit.
///
/// This used to release the reference here as well, which made the EMFILE path
/// a double release: `handle_recvmsg` sets `fit = i` on the failure, and its
/// `for j in fit..nfds` overflow loop then `drop_transfer`s the very descriptor
/// this call had already released. The second `release_vnode` comes out of some
/// *other* holder's reference — for a pipe it drives `writers`/`readers` to
/// zero under a live fd (spurious EOF/EPIPE, and a ring the allocator will
/// re-hand out), for an eventfd/timerfd it frees the pool slot under a live fd,
/// and for a `DynamicDevice` it sends VFS_CLOSE and recycles the open id.
///
/// Releasing here was wrong on its own terms too: it passed the *receiver's*
/// pid to `release_vnode`, so `release_locks` dropped whatever advisory locks
/// the receiver happened to hold on that vnode through an unrelated fd of its
/// own — for a descriptor the receiver never even took delivery of.
pub fn import_fd(pid: u32, tf: TransferFd, cloexec: bool) -> isize {
    let mut tbls = TblSlot::new();
    let tbl = match get_or_create(pid, &mut tbls) { Some(t) => t, None => {
        drop(tbls); return -23; // ENFILE (fd-table pool full); `tf` is still the caller's to drop
    }};
    let slot = match tbl.alloc_fd() { Some(s) => s, None => {
        drop(tbls); return -24; // EMFILE; `tf` is still the caller's to drop
    }};
    let mut flags = tf.flags;
    if cloexec { flags |= O_CLOEXEC; } else { flags &= !O_CLOEXEC; }
    tbl.fds.set(slot, FdEntry { kind: tf.kind, flags, in_use: true, ofd: tf.ofd });
    drop(tbls);
    // Strictly after the install: the table entry is now the reference, so the
    // slot is never momentarily unreferenced. Over-counting is safe here,
    // under-counting frees the inode out from under the receiver.
    tmp_inflight_dec(&tf.kind);
    // [GAP2] SCM_RIGHTS hand-off of a tmpfs/memfd fd — this is exactly the
    // Wayland wl_shm pool arriving in the compositor. If `idx` here does not
    // match the sender's, or the kind is not TmpFile, the receiver's mmap will
    // miss the K1 aliasing branch (hypothesis (b)).
    if let VnodeKind::TmpFile { idx, writable, .. } = tf.kind {
        if mm::gap2::budget_ok() {
            mm::gap2::s("[G2IMP] rxpid="); mm::gap2::h(pid as usize);
            mm::gap2::kv(" fd=",    slot);
            mm::gap2::kv(" idx=",   idx);
            mm::gap2::kv(" w=",     writable as usize);
            mm::gap2::kv(" flags=", flags as usize);
            mm::gap2::nl();
        }
    }
    slot as isize
}

/// Release a `TransferFd` that never reached a receiver — the in-flight
/// reference `export_fd` took is dropped, and any last-reference teardown runs
/// exactly as a close would. Linux closes SCM_RIGHTS fds that don't fit the
/// receiver's control buffer, and drops queued fds when the socket dies.
pub fn drop_transfer(tf: TransferFd) {
    tmp_inflight_dec(&tf.kind);
    release_vnode(tf.kind, 0);
    ofd::put(tf.ofd);
}

fn handle_fork_dup(parent_pid: u32, child_pid: u32) -> Message {
    copy_fd_table(parent_pid, child_pid, child_pid);
    ok_reply()
}

/// Copy `src_pid`'s table (the calling thread's, if it is that process's)
/// into a new table `dst_key` owned by tgid `owner`. false: no source table.
pub fn copy_fd_table(src_pid: u32, dst_key: u32, owner: u32) -> bool {
    // The new table is a copy of the source, built on the heap under the
    // source's lock (chunk by chunk: no table-sized temporary anywhere) and
    // published whole, so no second table lock is ever held.
    let copied = {
        let mut tbls = TblSlot::new();
        let parent = match find_tbl(src_pid, &mut tbls) { Some(t) => t, None => return false };
        let child = ProcFdTable { pid: dst_key, owner, fds: parent.fds.clone(), limit: parent.limit, dead: false };
        let entries: alloc::vec::Vec<FdEntry> = child.fds.iter_used().map(|(_, e)| *e).collect();
        let child: FdTableRef = alloc::sync::Arc::new(FdTableLock::new_typed(child));
        let stale = FD_TABLES.lock().map.insert(dst_key, child);
        drop(tbls);
        if let Some(stale) = stale {
            // A table left behind by an earlier process with this pid would
            // leak its references; it cannot happen (close_all runs at exit),
            // but if it did, keep the new one and say so.
            let mut s = stale.lock();
            s.dead = true;
            if s.fds.len() != 0 { dbg_str("\n[VFS] fork: replaced a stale fd table\n"); }
        }
        entries
    };
    // The copy holds a second fd on every inherited pipe endpoint, so its
    // reader/writer refcount must go up: otherwise the parent closing its copy
    // would falsely signal EOF/EPIPE/POLLHUP to the still-open child (and vice
    // versa) -- the exact defect that broke poll/select/epoll across fork.
    for f in copied.iter() {
        pipe_ref_inc(&f.kind); ofd::get(f.ofd);
    }
    true
}

/// Move the table `from` to id `to` (exec: a private table becomes the
/// process table). The map lock is never held with a table lock.
pub fn rekey_table(from: u32, to: u32) {
    let t = FD_TABLES.lock().map.remove(&from);
    let Some(t) = t else { return };
    t.lock().pid = to;
    let stale = FD_TABLES.lock().map.insert(to, t);
    if let Some(stale) = stale { stale.lock().dead = true; }
}

/// Release whatever the vnode behind a closed fd was holding: pipe endpoint
/// refcounts, eventfd/timerfd pool slots, dynamic-device handles, ephemeral
/// tmpfs entries, and advisory locks. Shared by every path that retires an fd
/// (close_all on exit, the O_CLOEXEC sweep on exec) so they can't drift apart.
/// Caller must NOT hold the FD_TABLES lock.
fn release_vnode(kind: VnodeKind, pid: u32) {
    if let Some(key) = lock_key_of(&kind) { release_locks(key, pid); }
    match kind {
        VnodeKind::Pipe { ring, is_write } => {
            pipe_drop_ref(ring, is_write);
        }
        VnodeKind::Pty { pair, is_master } => {
            tty_server::pty::drop_ref(pair as usize, is_master);
        }
        VnodeKind::Mq { slot } => { mqueue::ref_dec(slot, pid); }
        VnodeKind::NsFd { id, .. } => sched::ns::put_raw(id),
        VnodeKind::EventFd { slot } => {
            // Only free the slot when the LAST dup of this eventfd closes;
            // otherwise a surviving dup keeps using a counter a new eventfd
            // has since been allocated over (the calloop aliasing bug).
            let mut refs = EVENTFD_REFS.lock();
            refs[slot] = refs[slot].saturating_sub(1);
            if refs[slot] == 0 {
                EVENTFD_COUNTERS.lock()[slot] = u64::MAX;
                EVENTFD_SEQ.lock()[slot] = 0; // start clean for the next reuse
            }
        }
        VnodeKind::TimerFd { slot } => {
            let mut refs = TIMERFD_REFS.lock();
            refs[slot] = refs[slot].saturating_sub(1);
            if refs[slot] == 0 { TIMERFD_POOL.lock()[slot] = TimerFdEntry::free(); }
        }
        VnodeKind::DynamicDevice { port, dev_id, open_id } => {
            device_close(port, dev_id, open_id);
        }
        // Lane loginhang: the exit / close-on-exec paths never told the mount
        // server, so every process that died (or exec'd) with an f2fs file
        // open leaked one of the mount's 256 open-file slots for good —
        // killmt fs_io ran the pool dry (EMFILE for every later open, logins
        // included) after ~100 kills. Same last-reference rule as `handle_close`.
        VnodeKind::MountedFile { port, file_id } => {
            let still_referenced = mount_referenced(port, file_id);
            if !still_referenced {
                let mut proxy = Message::empty();
                proxy.tag = VFS_CLOSE;
                proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
                let _ = call_port(port, proxy);
            }
        }
        VnodeKind::TmpFile { idx, ofd, .. } => { tmp_ofd_put(ofd); tmp_release_ephemeral(idx) }
        _ => {}
    }
}

fn handle_exec_cloexec(pid: u32) -> Message {
    // Collect first, release after dropping the table lock: release_vnode takes
    // PIPE_RINGS and may call out to a device port.
    let mut closed: alloc::vec::Vec<FdEntry> = alloc::vec::Vec::new();
    {
        let mut tbls = TblSlot::new();
        if let Some(t) = find_tbl(pid, &mut tbls) {
            let doomed: alloc::vec::Vec<usize> = t.fds.iter_used()
                .filter(|(_, f)| f.flags & O_CLOEXEC != 0).map(|(i, _)| i).collect();
            for i in doomed { closed.push(t.fds.clear(i)); }
        }
    }
    for e in closed.iter() { ofd::put(e.ofd); }
    // Without this the close-on-exec sweep silently leaked a reference on every
    // pipe end it retired, so the peer's reader never reached EOF: brush's
    // `$(...)` read_to_string and every `a | b` reader blocked forever waiting
    // for a writer count that could no longer reach zero.
    for e in closed {
        if !matches!(e.kind, VnodeKind::None) { release_vnode(e.kind, pid); }
    }
    ok_reply()
}

fn handle_close_all(pid: u32) -> Message {
    let gone = FD_TABLES.lock().map.remove(&pid);
    if let Some(t) = gone {
        // Out of the map; a caller that looked the table up before this and
        // locks it after sees it dead and installs nothing (`TblSlot::holds`).
        let (entries, owner) = {
            let mut g = t.lock();
            g.dead = true;
            (g.fds.drain(), g.owner)
        };
        drop(t);
        for (_, e) in entries.iter() { ofd::put(e.ofd); }
        // Close them all properly
        for (_, e) in entries {
            release_vnode(e.kind, owner);
        }
    }
    ok_reply()
}

// ── Advisory file locking (flock + fcntl F_GETLK/F_SETLK/F_SETLKW) ──────────────
//
// Locks are keyed by vnode identity (tmpfs slot, or mount port + remote file_id)
// rather than by fd, so dup()'d fds and separate opens of the same path share
// the same lock domain. flock() and fcntl() locks share one table: this is
// stricter than POSIX (real Linux keeps the two lock classes independent) but
// never grants access that either model would have denied on its own.

// 64 -> grown on demand up to this ceiling (lane fdmax): a database or a build
// holding one lock per open file hit ENOLCK at 64 locks system-wide.
const MAX_LOCKS: usize = 1 << 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LockKey { Tmp(usize), Mount(u32, u32), Ino(u32, u64) }

#[derive(Clone, Copy)]
struct LockRecord {
    key:       LockKey, // lock domain: the inode (tmpfs index, or mount port + inode)
    slot:      LockKey, // the open-file slot the lock was taken through (released by its close)
    pid:       u32,
    start:     u64,
    end:       u64, // exclusive upper bound; u64::MAX == to EOF
    exclusive: bool,
    in_use:    bool,
}

impl LockRecord {
    const fn empty() -> Self {
        Self { key: LockKey::Tmp(0), slot: LockKey::Tmp(0), pid: 0, start: 0, end: 0, exclusive: false, in_use: false }
    }
}

static LOCKS: Mutex<alloc::vec::Vec<LockRecord>> = Mutex::new(alloc::vec::Vec::new());

/// Store `rec` in a free record, growing the table if none is free. False:
/// ENOLCK.
fn lock_insert(locks: &mut alloc::vec::Vec<LockRecord>, rec: LockRecord) -> bool {
    if let Some(slot) = locks.iter_mut().find(|l| !l.in_use) { *slot = rec; return true; }
    if locks.len() >= MAX_LOCKS || locks.try_reserve(1).is_err() { return false; }
    locks.push(rec);
    true
}

/// Identify the lock domain of a vnode. Only regular tmpfs/mounted files are lockable.
fn lock_key_of(kind: &VnodeKind) -> Option<LockKey> {
    match kind {
        VnodeKind::TmpFile { idx, .. }           => Some(LockKey::Tmp(*idx)),
        VnodeKind::MountedFile { port, file_id }  => Some(LockKey::Mount(*port, *file_id)),
        _ => None,
    }
}

/// st_ino of an open mounted file, asked of its mount server. A mount server's
/// `file_id` names an open-file SLOT (one per open(2)), never the file, so it
/// cannot be the lock domain: two processes opening the same path got two
/// unrelated keys and their fcntl locks never conflicted (sqlite on f2fs: both
/// podman processes held EXCLUSIVE at once and deleted each other's journal).
fn mounted_ino(port: u32, file_id: u32) -> Option<u64> {
    let mut st = [0u8; 160];
    let mut proxy = Message::empty();
    proxy.tag = VFS_FSTAT;
    proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
    proxy.data[8..16].copy_from_slice(&(st.as_mut_ptr() as u64).to_le_bytes());
    let r = call_port(port, proxy);
    if reply_val(&r) != 0 { return None; }
    let ino = u64::from_le_bytes(st[8..16].try_into().unwrap());
    if ino == 0 { None } else { Some(ino) }
}

/// (lock domain, open-file slot) of a vnode. The domain is what conflicts are
/// decided on; the slot is what a close releases.
fn lock_domain_of(kind: &VnodeKind) -> Option<(LockKey, LockKey)> {
    let slot = lock_key_of(kind)?;
    let dom = match kind {
        VnodeKind::MountedFile { port, file_id } =>
            mounted_ino(*port, *file_id).map(|i| LockKey::Ino(*port, i)).unwrap_or(slot),
        _ => slot,
    };
    Some((dom, slot))
}

fn ranges_overlap(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start < b_end && b_start < a_end
}

/// Find another pid's lock on `key` that conflicts with a request for `[start,end)`.
/// A conflict requires either side to be exclusive (two shared locks never conflict).
fn find_conflict(locks: &[LockRecord], key: LockKey, pid: u32, start: u64, end: u64, exclusive: bool) -> Option<LockRecord> {
    locks.iter().find(|l| {
        l.in_use && l.key == key && l.pid != pid
            && ranges_overlap(l.start, l.end, start, end)
            && (exclusive || l.exclusive)
    }).copied()
}

/// The key a waiter for a lock on `key` (F_SETLKW, blocking flock) parks on and
/// every release of a record on `key` wakes. A collision between two domains
/// only costs a re-probe. Waiters park on it and releases wake it, so an unrelated keyed wake sharing only the bloom bit never reaches a
/// lock waiter (a bits-only park woke on every such collision).
fn lock_wake_key(key: LockKey) -> u64 {
    let index = match key {
        LockKey::Tmp(idx) => idx as u32,
        LockKey::Mount(port, file_id) =>
            0x8000_0000 | (port.wrapping_mul(0x9E37_79B1) ^ file_id) & 0x7fff_ffff,
        LockKey::Ino(port, ino) =>
            0x8000_0000 | (port.wrapping_mul(0x9E37_79B1) ^ (ino as u32) ^ ((ino >> 32) as u32).wrapping_mul(0x85EB_CA6B)) & 0x7fff_ffff,
    };
    sched::poll_key(sched::poll_class::FLOCK, index)
}

/// Lock waiters re-probe at least this often even without a wake. Every
/// path that clears a record wakes `lock_wake_key`, so this is only a
/// backstop for a release path added later without one.
const LOCK_WAIT_BACKSTOP_NS: u64 = 1_000_000_000;

/// Clear every record on `key` held by `pid` that `pick` selects, and wake
/// the domain's waiters if any was cleared. Every release of a lock record
/// goes through here (LOCK_UN, F_UNLCK, close, exit, exec, and a grant that
/// supersedes the holder's own older range), so a waiter cannot miss one.
/// Called with `locks` held; the wake happens after the caller drops it.
#[must_use]
fn locks_clear(locks: &mut [LockRecord], key: LockKey, pid: u32,
               mut pick: impl FnMut(&LockRecord) -> bool) -> bool {
    let mut cleared = false;
    for l in locks.iter_mut() {
        if l.in_use && l.key == key && l.pid == pid && pick(l) {
            *l = LockRecord::empty();
            cleared = true;
        }
    }
    cleared
}

/// Wake the waiters for a lock on `key` (after some record on it was cleared).
fn locks_released(key: LockKey) { sched::wake_poll_key(lock_wake_key(key)); }

/// Release every lock `pid` holds on `key`.
fn release_locks(slot: LockKey, pid: u32) {
    let pid = sched::tgid_of(pid);
    let mut woke: Option<LockKey> = None;
    {
        let mut locks = LOCKS.lock();
        for l in locks.iter_mut() {
            if l.in_use && l.slot == slot && l.pid == pid {
                woke = Some(l.key);
                *l = LockRecord::empty();
            }
        }
    }
    if let Some(k) = woke { locks_released(k); }
}

/// Park a lock waiter: `try_grant` runs once before parking and once after
/// the park is published, so a release between the two cannot be missed
/// (the release wakes the published park). `Some` is the grant's reply.
fn lock_wait(key: LockKey, mut try_grant: impl FnMut() -> Option<Message>) -> Message {
    loop {
        if let Some(m) = try_grant() { return m; }
        sched::block_on_poll_prepare_tag(sched::monotonic_ns() + LOCK_WAIT_BACKSTOP_NS,
                                         &sched::PollTag::keyed(&[lock_wake_key(key)], 0));
        if let Some(m) = try_grant() { sched::block_on_poll_cancel(); return m; }
        sched::block_on_poll_commit();
    }
}

/// Resolve an `l_whence`/`l_start`/`l_len` triple (from `struct flock`) into an
/// absolute `[start, end)` byte range. SEEK_CUR/SEEK_END are only resolvable for
/// tmpfs vnodes, whose position/size VFS tracks directly.
fn resolve_lock_range(kind: &VnodeKind, whence: i16, l_start: i64, l_len: i64) -> Option<(u64, u64)> {
    const SEEK_SET: i16 = 0;
    const SEEK_CUR: i16 = 1;
    const SEEK_END: i16 = 2;
    let base: i64 = match whence {
        SEEK_SET => 0,
        SEEK_CUR => match kind { VnodeKind::TmpFile { .. } => tmp_pos_get(kind) as i64, _ => return None },
        SEEK_END => match kind {
            VnodeKind::TmpFile { idx, .. } => TMP_FILES.lock()[*idx].len as i64,
            _ => return None,
        },
        _ => return None,
    };
    let mut start = base.checked_add(l_start)?;
    let mut len = l_len;
    if len < 0 { start = start.checked_add(len)?; len = -len; }
    if start < 0 { return None; }
    let start = start as u64;
    let end = if len == 0 { u64::MAX } else { start.checked_add(len as u64)? };
    Some((start, end))
}

/// Read a `struct flock` (Linux x86_64/aarch64 layout, 32 bytes) from user memory.
/// Returns (l_type, l_whence, l_start, l_len, l_pid).
fn read_flock(ptr: usize) -> Option<(i16, i16, i64, i64, u32)> {
    let mut buf = [0u8; 32];
    let ok = sched::with_current_address_space(|as_| as_.read_user_buf(ptr, &mut buf))
        .unwrap_or(false);
    if !ok { return None; }
    Some((
        i16::from_le_bytes(buf[0..2].try_into().unwrap()),
        i16::from_le_bytes(buf[2..4].try_into().unwrap()),
        i64::from_le_bytes(buf[8..16].try_into().unwrap()),
        i64::from_le_bytes(buf[16..24].try_into().unwrap()),
        u32::from_le_bytes(buf[24..28].try_into().unwrap()),
    ))
}

/// Write a `struct flock` back to user memory via the safe user-buffer accessor
/// (never dereference the raw pointer directly — it may sit on a CoW page that
/// a supervisor-mode fault can't recover from).
fn write_flock(ptr: usize, l_type: i16, l_whence: i16, l_start: i64, l_len: i64, l_pid: u32) -> bool {
    let mut buf = [0u8; 32];
    buf[0..2].copy_from_slice(&l_type.to_le_bytes());
    buf[2..4].copy_from_slice(&l_whence.to_le_bytes());
    buf[8..16].copy_from_slice(&l_start.to_le_bytes());
    buf[16..24].copy_from_slice(&l_len.to_le_bytes());
    buf[24..28].copy_from_slice(&l_pid.to_le_bytes());
    sched::with_current_address_space_mut(|as_| as_.write_user_buf(ptr, &buf)).unwrap_or(false)
}

const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;

fn handle_fcntl_lock(pid: u32, kind: VnodeKind, cmd: usize, arg: usize) -> Message {
    const F_GETLK:  usize = 5;
    const F_SETLK:  usize = 6;

    let pid = sched::tgid_of(pid);
    let (key, slot) = match lock_domain_of(&kind) { Some(k) => k, None => return err_reply(-22) }; // EINVAL
    let (l_type, l_whence, l_start, l_len, _) = match read_flock(arg) {
        Some(v) => v, None => return err_reply(-14), // EFAULT
    };
    let (start, end) = match resolve_lock_range(&kind, l_whence, l_start, l_len) {
        Some(r) => r, None => return err_reply(-22),
    };

    if l_type == F_UNLCK {
        let cleared = locks_clear(&mut LOCKS.lock(), key, pid,
                                  |l| ranges_overlap(l.start, l.end, start, end));
        if cleared { locks_released(key); }
        return ok_reply();
    }

    if cmd == F_GETLK {
        let locks = LOCKS.lock();
        match find_conflict(&*locks, key, pid, start, end, l_type == F_WRLCK) {
            Some(c) => {
                drop(locks);
                let len = if c.end == u64::MAX { 0 } else { (c.end - c.start) as i64 };
                write_flock(arg, if c.exclusive { F_WRLCK } else { F_RDLCK }, 0, c.start as i64, len, c.pid);
            }
            None => { drop(locks); write_flock(arg, F_UNLCK, 0, 0, 0, 0); }
        }
        return ok_reply();
    }

    // F_SETLK / F_SETLKW
    let exclusive = l_type == F_WRLCK;
    let try_grant = || -> Option<Message> {
        let mut locks = LOCKS.lock();
        if find_conflict(&*locks, key, pid, start, end, exclusive).is_some() { return None; }
        // No sub-range splitting: a new grant simply supersedes any range
        // this same pid already held that overlaps it — which can release
        // bytes or downgrade exclusive to shared, so it wakes like a release.
        let cleared = locks_clear(&mut locks, key, pid,
                                  |l| ranges_overlap(l.start, l.end, start, end));
        let ok = lock_insert(&mut locks, LockRecord { key, slot, pid, start, end, exclusive, in_use: true });
        drop(locks);
        if cleared { locks_released(key); }
        Some(if ok { ok_reply() } else { err_reply(-37) }) // ENOLCK
    };
    if cmd == F_SETLK { return try_grant().unwrap_or_else(|| err_reply(-11)); } // EAGAIN
    // F_SETLKW: parked on the domain's lock tag, woken by every release.
    lock_wait(key, try_grant)
}

fn handle_flock(pid: u32, fd: usize, op: u32) -> Message {
    const LOCK_SH: u32 = 1;
    const LOCK_EX: u32 = 2;
    const LOCK_NB: u32 = 4;
    const LOCK_UN: u32 = 8;

    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }
    let kind = tbl.fds[fd].kind;
    drop(tbls);
    let pid = sched::tgid_of(pid);
    let (key, slot) = match lock_domain_of(&kind) { Some(k) => k, None => return err_reply(-22) };

    if op & LOCK_UN != 0 {
        release_locks(slot, pid);
        return ok_reply();
    }

    let exclusive = op & LOCK_EX != 0;
    if !exclusive && op & LOCK_SH == 0 { return err_reply(-22); } // EINVAL
    let nonblock = op & LOCK_NB != 0;

    let try_grant = || -> Option<Message> {
        let mut locks = LOCKS.lock();
        if find_conflict(&*locks, key, pid, 0, u64::MAX, exclusive).is_some() { return None; }
        // A conversion replaces the holder's own lock (EX -> SH releases).
        let cleared = locks_clear(&mut locks, key, pid, |_| true);
        let ok = lock_insert(&mut locks, LockRecord { key, slot, pid, start: 0, end: u64::MAX, exclusive, in_use: true });
        drop(locks);
        if cleared { locks_released(key); }
        Some(if ok { ok_reply() } else { err_reply(-37) }) // ENOLCK
    };
    if nonblock { return try_grant().unwrap_or_else(|| err_reply(-11)); } // EWOULDBLOCK
    // Parked on the domain's lock tag, woken by every release (see F_SETLKW).
    lock_wait(key, try_grant)
}

fn handle_fcntl(pid: u32, fd: usize, cmd: usize, arg: usize) -> Message {
    // F_GETFD=1, F_SETFD=2, F_GETFL=3, F_SETFL=4, F_GETLK=5, F_SETLK=6, F_SETLKW=7
    const F_GETFD:  usize = 1;
    const F_SETFD:  usize = 2;
    const F_GETFL:  usize = 3;
    const F_SETFL:  usize = 4;
    const F_GETLK:  usize = 5;
    const F_SETLK:  usize = 6;
    const F_SETLKW: usize = 7;
    // Duplication commands. These MUST be handled: they return a *file
    // descriptor*, so the old catch-all `_ => ok_reply()` answered them with 0
    // — a plausible-looking success that silently aliased every duplicated
    // handle onto fd 0 (the console). std's `File::try_clone` /
    // `PipeReader::try_clone` / `BorrowedFd::try_clone_to_owned` are all
    // F_DUPFD_CLOEXEC, and brush uses exactly those to hand a redirection or a
    // pipe end to a child process — so `a | b` sent a's stdout to the console
    // and `cmd < file` gave cmd the console as stdin and hung the shell
    // forever waiting on a keystroke.
    const F_DUPFD:         usize = 0;
    const F_DUPFD_CLOEXEC: usize = 1030;
    const F_SETPIPE_SZ:    usize = 1031;
    const F_GETPIPE_SZ:    usize = 1032;
    // fcntl's FD_CLOEXEC is bit 0 of the *descriptor* flags, a different
    // namespace from the O_CLOEXEC bit we store in `flags`.
    const FD_CLOEXEC: u32 = 1;

    if cmd == F_DUPFD || cmd == F_DUPFD_CLOEXEC {
        return dup_fd_min(pid, fd, arg, cmd == F_DUPFD_CLOEXEC);
    }

    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }
    match cmd {
        F_GETFD => val_reply((tbl.fds[fd].flags & O_CLOEXEC != 0) as u64),
        // F_SETFD carries only FD_CLOEXEC. Assigning `arg` wholesale both
        // failed to set our O_CLOEXEC bit (FD_CLOEXEC is 1, O_CLOEXEC is
        // 0x80000, so fcntl-requested close-on-exec never actually fired) and
        // wiped the access mode plus O_APPEND/O_NONBLOCK that handle_write and
        // fd_nonblock read back.
        F_SETFD => {
            let cloexec = arg as u32 & FD_CLOEXEC != 0;
            if cloexec { tbl.fds[fd].flags |= O_CLOEXEC; }
            else       { tbl.fds[fd].flags &= !O_CLOEXEC; }
            ok_reply()
        }
        F_GETFL => val_reply(tbl.fds[fd].flags as u64),
        // Only the status flags F_SETFL may change (Linux SETFL_MASK); the
        // access mode, O_PATH and close-on-exec stay. They belong to the
        // open file description, so every fd sharing it — a dup, a forked
        // child's copy — sees the change too (a shell's `O_NONBLOCK` on an
        // inherited pipe is the parent's as well).
        F_SETFL => {
            let set = arg as u32 & SETFL_MASK;
            tbl.fds[fd].flags = (tbl.fds[fd].flags & !SETFL_MASK) | set;
            let id = tbl.fds[fd].ofd;
            drop(tbls);
            if id != 0 && ofd::shared(id) {
                for t in all_tables() {
                    let mut t = t.lock();
                    let fds: alloc::vec::Vec<usize> = t.fds.iter_used()
                        .filter(|(_, e)| e.ofd == id).map(|(n, _)| n).collect();
                    for n in fds { t.fds[n].flags = (t.fds[n].flags & !SETFL_MASK) | set; }
                }
            }
            ok_reply()
        }
        F_GETLK | F_SETLK | F_SETLKW => {
            let kind = tbl.fds[fd].kind;
            drop(tbls);
            handle_fcntl_lock(pid, kind, cmd, arg)
        }
        // Pipe capacity is fixed at PIPE_RING_SIZE. Report it honestly rather
        // than accepting a larger request we cannot satisfy: brush sizes a pipe
        // to a here-document's length and then writes the whole body before any
        // reader exists, so silently "succeeding" here would leave the write
        // blocked forever on a full ring. EINVAL surfaces as a clean shell error.
        F_GETPIPE_SZ => val_reply(PIPE_RING_SIZE as u64),
        F_SETPIPE_SZ => {
            if arg > PIPE_RING_SIZE { err_reply(-22) } // EINVAL
            else { val_reply(PIPE_RING_SIZE as u64) }
        }
        // memfd seals (K1). Only permitted on a memfd inode; F_SEAL_SHRINK is
        // the only bit enforced (in handle_ftruncate). Other bits are accepted
        // and stored but not acted on (F_SEAL_WRITE/GROW/SEAL out of scope).
        F_ADD_SEALS => {
            let kind = tbl.fds[fd].kind;
            drop(tbls);
            if let VnodeKind::TmpFile { idx, .. } = kind {
                let mut vmos = TMP_VMOS.lock();
                match vmos[idx].as_mut() {
                    Some(vmo) if vmo.is_memfd => {
                        // Linux memfd_add_seals: no new seals once F_SEAL_SEAL is
                        // set; unknown bits are EINVAL.
                        if arg as u32 & !F_SEAL_KNOWN != 0 { return err_reply(-22); }
                        if vmo.seals & F_SEAL_SEAL != 0 { return err_reply(-1); } // EPERM
                        vmo.seals |= arg as u32; ok_reply()
                    }
                    _ => err_reply(-22), // EINVAL — not a memfd
                }
            } else { err_reply(-22) }
        }
        F_GET_SEALS => {
            let kind = tbl.fds[fd].kind;
            drop(tbls);
            if let VnodeKind::TmpFile { idx, .. } = kind {
                let vmos = TMP_VMOS.lock();
                match vmos[idx].as_ref() {
                    Some(vmo) if vmo.is_memfd => val_reply(vmo.seals as u64),
                    _ => err_reply(-22), // EINVAL — not a memfd
                }
            } else { err_reply(-22) }
        }
        _ => ok_reply(), // silently ignore unknown fcntl
    }
}

/// Allocate a new fd number pointing at the same vnode as `oldfd`, choosing the
/// lowest free number that is >= `minfd`. Backs both `dup()`/VFS_ALLOC_FD
/// (`minfd` = 3, O_CLOEXEC cleared) and `fcntl(F_DUPFD{,_CLOEXEC})` (`minfd`
/// from the caller's arg).
///
/// `cloexec` is what the *new* descriptor gets, independent of `oldfd`: plain
/// dup() and F_DUPFD always clear it, F_DUPFD_CLOEXEC always sets it.
fn dup_fd_min(pid: u32, oldfd: usize, minfd: usize, cloexec: bool) -> Message {
    if oldfd >= FD_NUMBER_CEIL { return err_reply(-9); }
    // fds 0-2 are handed out only once closed (see ProcFdTable::alloc_fd).
    let floor = minfd;
    let mut tbls = TblSlot::new();
    let tbl = match get_or_create(pid, &mut tbls) { Some(t) => t, None => return err_reply(-23) };
    // F_DUPFD with a minimum at or past RLIMIT_NOFILE is EINVAL (Linux).
    if minfd >= tbl.cap() { return err_reply(-22); }
    if !tbl.fds.used(oldfd) && !tbl.fds.implicit_console(oldfd) { return err_reply(-9); }
    // See handle_dup2: the kernel duplicates socket/epoll entries itself.
    if handle_of(&tbl.fds[oldfd].kind).is_some() { return err_reply(-9); }
    FDDUP[fdkind_idx(&tbl.fds[oldfd].kind)].fetch_add(1, atomic::Ordering::Relaxed);

    let newfd = match tbl.alloc_fd_from(floor) {
        Some(f) => f, None => return err_reply(-24), // EMFILE
    };

    if !tbl.fds[oldfd].in_use {
        // An untracked fd 0-2 is the raw console: dup() of it yields a
        // /dev/stdin|stdout|stderr proxy, exactly what opening those paths
        // creates. (std's try_clone_to_owned on stdio — fcntl F_DUPFD — and
        // command_fds' stdio mappings depend on this producing a real fd.)
        if tbl.fds.implicit_console(oldfd) {
            tbl.fds.set(newfd, FdEntry {
                kind:   VnodeKind::DevStdio { target_fd: oldfd },
                flags:  if cloexec { O_CLOEXEC } else { 0 },
                in_use: true,
                ofd:    ofd::alloc(),
            });
            return val_reply(newfd as u64);
        }
        return err_reply(-9);
    }

    { let e = tbl.fds[oldfd]; tbl.fds.set(newfd, e); }
    tbl.fds[newfd].flags = if cloexec {
        tbl.fds[oldfd].flags | O_CLOEXEC
    } else {
        tbl.fds[oldfd].flags & !O_CLOEXEC
    };
    let dupled = tbl.fds[newfd].kind;
    let shared = tbl.fds[newfd].ofd;
    drop(tbls);
    pipe_ref_inc(&dupled); // newfd is a second fd on the same pipe endpoint
    ofd::get(shared);      // ... and on the same open file description
    val_reply(newfd as u64)
}

/// Allocate a new fd number pointing at the same vnode as `oldfd`.
/// Used by sys_dup() which doesn't know the new fd number in advance.
fn handle_alloc_fd(pid: u32, oldfd: usize) -> Message {
    dup_fd_min(pid, oldfd, 3, false)
}

/// `TIOCGPTPEER(master_fd, open_flags)` — open the slave of the pair `master_fd`
/// names, without going through `/dev/pts/N`.
///
/// It lives here rather than in `pty::ioctl` because it is not really an ioctl:
/// it *allocates a descriptor*, and descriptors are this crate's business. The
/// pty pool cannot hand one out.
///
/// This is not an optional nicety. `rustix::pty::openpt` tries `TIOCGPTPEER`
/// **first** and only falls back to `TIOCGPTN` + `open("/dev/pts/N")` on ENOSYS
/// or EPERM — any other errno (our generic ENOTTY for an unknown command, for
/// instance) propagates and the pty open fails outright. cosmic-term reaches
/// the kernel through rustix's raw-syscall backend rather than through musl, so
/// this path, not the libc one, is what opening a terminal tab actually runs.
///
/// Racing the master closed between the ownership check and `slave_open` is
/// handled by `slave_open` itself, which refuses a pair with no live master.
pub fn pty_get_peer(pid: u32, master_fd: usize, open_flags: u32) -> isize {
    let pid = sched::tgid_of(pid);
    let pair = {
        let mut tbls = TblSlot::new();
        let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return -9 };
        if !tbl.fds.used(master_fd) { return -9; }
        match tbl.fds[master_fd].kind {
            VnodeKind::Pty { pair, is_master: true } => pair as usize,
            // ENOTTY on anything else, including the slave end: Linux only
            // answers this on a ptmx fd.
            _ => return -25,
        }
    };
    if let Err(e) = tty_server::pty::slave_open(pair) { return e as isize; }
    let newfd = {
        let mut tbls = TblSlot::new();
        let slot = get_or_create(pid, &mut tbls).and_then(|tbl| {
            tbl.alloc_fd().map(|f| {
                tbl.fds.set(f, FdEntry {
                    kind: VnodeKind::Pty { pair: pair as u16, is_master: false },
                    // Only the two flags the caller can meaningfully ask for
                    // here; the access mode is implicit (a pty end is always
                    // read-write) and the rest are creation flags.
                    flags: open_flags & (O_CLOEXEC | O_NONBLOCK_FL),
                    in_use: true,
                    ofd: ofd::alloc(),
                });
                f
            })
        });
        drop(tbls);
        match slot {
            Some(f) => f,
            None => {
                // The reference `slave_open` just took has no fd to own it.
                tty_server::pty::drop_ref(pair, false);
                return -24; // EMFILE
            }
        }
    };
    newfd as isize
}

/// Store the getdents64 cursor back into a directory fd without changing what
/// kind of directory it is. A tmpfs directory is a `TmpFile` vnode whose pool
/// slot carries `is_dir`; overwriting it with a `RamFile` (as the enumeration
/// used to do unconditionally) would detach it from its pool entry.
///
/// The cursor belongs to the open file description, not the descriptor: a
/// dup'd or inherited directory fd continues where the other left off (and a
/// rewinddir through one rewinds both), as on Linux. The description keeps it
/// (`ofd::set_pos`); the copy in the vnode is what an fd without a
/// description (the table was full) uses.
fn set_dir_pos(e: &mut FdEntry, new_pos: usize) {
    match &mut e.kind {
        VnodeKind::RamFile { pos, .. } => *pos = new_pos,
        VnodeKind::TmpFile { pos, .. }  => *pos = new_pos,
        VnodeKind::SysBlock { pos, .. } => *pos = new_pos as u32,
        _ => return,
    }
    ofd::set_pos(e.ofd, new_pos as u64);
}

/// The description offset of a directory fd (`set_dir_pos`), or of a static
/// RamFS file: the open file description's when the fd has one, else the
/// copy in the vnode.
fn desc_pos(e: &FdEntry) -> usize {
    let local = match e.kind {
        VnodeKind::RamFile { pos, .. } => pos,
        VnodeKind::TmpFile { pos, .. } => pos,
        VnodeKind::SysBlock { pos, .. } => pos as usize,
        _ => return 0,
    };
    if e.ofd == 0 { return local; }
    ofd::pos(e.ofd).map_or(local, |p| p as usize)
}

/// getdents64 — fill `buf` with `struct linux_dirent64` entries for `fd`.
fn handle_getdents64(pid: u32, fd: usize, buf_ptr: usize, count: usize) -> Message {
    if count < 64 { return err_reply(-22); }

    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }

    // A tmpfs directory's path is not `'static` (it lives in the TMP_FILES
    // pool and can be rmdir'd), so copy it into a frame-local buffer and
    // borrow *that* for the whole enumeration.
    let mut tmp_dir_buf: Vec<u8> = Vec::new();
    let mut tmp_dir_len = 0usize;
    let mut dir_is_tmp  = false;
    // A synthesized sysfs directory has no path to walk and no static table to
    // sweep — the block registry is enumerated directly. Handled here rather
    // than in the sweeps below because nothing in them applies.
    if let VnodeKind::SysBlock { dev, .. } = tbl.fds[fd].kind {
      if cgroupfs::is_dev(dev) {
        let list = cgroupfs::entries(cgroupfs::dev_cg(dev));
        let buf = buf_ptr as *mut u8;
        let mut off = 0usize;
        let mut cur = desc_pos(&tbl.fds[fd]);
        loop {
            let (name, is_dir, ino): (&[u8], bool, u64) = if cur == 0 { (b".", true, 1) }
                else if cur == 1 { (b"..", true, 1) }
                else { match list.get(cur - 2) {
                    Some((n, d)) => (n.as_bytes(), *d, 0x6100_0000 + cur as u64),
                    None => break,
                } };
            let nlen = name.len();
            let reclen = ((8 + 8 + 2 + 1 + nlen + 1) + 7) & !7;
            if off + reclen > count { break; }
            unsafe {
                let p = buf.add(off);
                core::ptr::write(p as *mut u64, ino);
                core::ptr::write(p.add(8) as *mut u64, 0u64);
                core::ptr::write(p.add(16) as *mut u16, reclen as u16);
                *p.add(18) = if is_dir { 4 } else { 8 };
                core::ptr::copy_nonoverlapping(name.as_ptr(), p.add(19), nlen);
                *p.add(19 + nlen) = 0;
            }
            off += reclen;
            cur += 1;
        }
        set_dir_pos(&mut tbl.fds[fd], cur);
        return val_reply(off as u64);
      }
    }
    if let VnodeKind::SysBlock { dev, level, .. } = tbl.fds[fd].kind {
        let (dev, level) = (dev, level);
        let buf = buf_ptr as *mut u8;
        let mut off = 0usize;
        let mut cur = desc_pos(&tbl.fds[fd]);
        loop {
            let mut name = [0u8; sysclass::NAME_LEN];
            let (nlen, is_dir, ino) = if cur == 0 {
                name[0] = b'.'; (1, true, 1)
            } else if cur == 1 {
                name[0] = b'.'; name[1] = b'.'; (2, true, 1)
            } else {
                let r = if dev >= sysclass::SC_BASE {
                    sysclass::dirent(dev, level, cur - 2, &mut name)
                } else {
                    let mut n16 = [0u8; block::NAME_MAX];
                    let r = block::sysfs_dirent(dev, level, cur - 2, &mut n16);
                    name[..block::NAME_MAX].copy_from_slice(&n16);
                    r
                };
                match r {
                    Some((n, d)) => (n, d, 0x5000_0000 + cur as u64),
                    None => break,
                }
            };
            let reclen = ((8 + 8 + 2 + 1 + nlen + 1) + 7) & !7;
            if off + reclen > count { break; }
            unsafe {
                let p = buf.add(off);
                core::ptr::write(p as *mut u64, ino);
                core::ptr::write(p.add(8) as *mut u64, 0u64);
                core::ptr::write(p.add(16) as *mut u16, reclen as u16);
                *p.add(18) = if is_dir { 4 } else if dev >= sysclass::SC_BASE && sysclass::dirent_link(dev, level, cur - 2) { 10 } else { 8 }; // DT_DIR / DT_LNK / DT_REG
                core::ptr::copy_nonoverlapping(name.as_ptr(), p.add(19), nlen);
                *p.add(19 + nlen) = 0;
            }
            off += reclen;
            cur += 1;
        }
        set_dir_pos(&mut tbl.fds[fd], cur);
        return val_reply(off as u64);
    }

    let dpos = desc_pos(&tbl.fds[fd]);
    let (static_path, start_pos): (&'static [u8], usize) = match &tbl.fds[fd].kind {
        VnodeKind::RamFile { data, .. } => (*data, dpos),
        VnodeKind::TmpFile { idx, .. } => {
            let i = *idx; let p = dpos;
            let t = TMP_FILES.lock();
            if !t[i].in_use() || !t[i].is_dir { return err_reply(-20); } // ENOTDIR
            tmp_dir_len = t[i].path().len();
            tmp_dir_buf.extend_from_slice(t[i].path());
            drop(t);
            dir_is_tmp = true;
            (b"", p)
        }
        VnodeKind::MountedFile { port, file_id } => {
            let port = *port; let file_id = *file_id;
            drop(tbls);
            let mut proxy = Message::empty();
            proxy.tag = VFS_GETDENTS64;
            proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&(buf_ptr as u64).to_le_bytes());
            proxy.data[16..24].copy_from_slice(&(count as u64).to_le_bytes());
            return call_port(port, proxy);
        }
        _ => return err_reply(-20), // ENOTDIR
    };
    let dir_path: &[u8] = if dir_is_tmp { &tmp_dir_buf[..tmp_dir_len] } else { static_path };
    let dir_len = dir_path.len();
    let buf = buf_ptr as *mut u8;
    let mut off = 0usize;
    let mut pos = start_pos;

    let write_dirent = |buf: *mut u8, off: usize, count: usize,
                        ino: u64, name: &[u8], d_type: u8| -> Option<usize> {
        let name_len = name.len();
        let reclen_raw = 8 + 8 + 2 + 1 + name_len + 1;
        let reclen = (reclen_raw + 7) & !7;
        if off + reclen > count { return None; }
        unsafe {
            let p = buf.add(off);
            core::ptr::write(p           as *mut u64, ino);
            core::ptr::write(p.add(8)    as *mut u64, 0u64);
            core::ptr::write(p.add(16)   as *mut u16, reclen as u16);
            *p.add(18) = d_type;
            core::ptr::copy_nonoverlapping(name.as_ptr(), p.add(19), name_len);
            *p.add(19 + name_len) = 0;
        }
        Some(reclen)
    };

    if pos == 0 {
        if let Some(r) = write_dirent(buf, off, count, 1, b".", 4) { off += r; pos += 1; }
        else { return val_reply(0); }
    }
    if pos == 1 {
        if let Some(r) = write_dirent(buf, off, count, 1, b"..", 4) { off += r; pos += 1; }
        else { set_dir_pos(&mut tbl.fds[fd], pos); return val_reply(off as u64); }
    }

    // /proc/self/fd: one entry per descriptor the caller holds, every kind,
    // in ascending order. Past "." and ".." the cursor is the next fd number
    // + 2, so a descriptor closed or opened between two calls neither
    // repeats nor shifts the listing. 0-2 are always listed: an untracked
    // one is the console, which is open.
    let fd_tgt: Option<u32> = if !dir_is_tmp && (dir_path == PROC_SELF_FD_DIR || dir_path == PROC_SELF_FDINFO_DIR) { Some(sched::tgid_of(pid)) }
                              else if dir_is_tmp { proc_pid_fd_dir(dir_path) } else { None };
    if let Some(tp) = fd_tgt {
        let from = pos - 2;
        let collect = |t: &mut ProcFdTable| -> Vec<usize> {
            let mut fds = t.fds.used_fds();
            for c in (0..3usize).rev() { if t.fds.implicit_console(c) { fds.insert(0, c); } }
            fds.sort_unstable();
            fds
        };
        let tgt = sched::tgid_of(tp);
        // A group whose leader has exited has no table (Linux: `files` is
        // NULL), so its leader's directory lists nothing.
        let lless = sched::leaderless(tp);
        let own = tgt == sched::tgid_of(pid);
        // thread-self (`/proc/<tid>/fd` of the calling non-leader thread)
        // lists the thread's own, possibly private, table (`tbl`); every
        // other directory lists the process's (leader's) table.
        let thread_view = own && tp == sched::current_pid() && tp != tgt;
        // Another table is read with ours released (no lock ordering
        // between two tables).
        let mine: Vec<usize> = if !lless && thread_view { collect(tbl) } else { Vec::new() };
        drop(tbls);
        let mut fds: Vec<usize> = if !lless && !thread_view {
            with_tbl_raw(leader_key(tgt), collect).unwrap_or_default()
        } else { mine };
        fds.retain(|&f| f >= from);
        let mut tbls2 = TblSlot::new();
        let Some(tbl) = find_tbl(pid, &mut tbls2) else { return err_reply(-9) };
        let mut next = from;
        for &f in fds.iter() {
            let mut d = [0u8; 20]; let mut k = d.len(); let mut v = f;
            loop { k -= 1; d[k] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
            let dt = if dir_path.ends_with(b"/fdinfo") { 8 } else { 10 }; // DT_REG / DT_LNK
            match write_dirent(buf, off, count, 0x6800_0000 + f as u64, &d[k..], dt) {
                Some(r) => { off += r; next = f + 1; }
                None => break,
            }
        }
        if off == 0 && fds.iter().any(|&f| f >= next) { return err_reply(-22); } // EINVAL: buffer too small
        if !fds.iter().any(|&f| f >= next) { next = fds.last().map_or(from, |&l| l + 1).max(from); }
        set_dir_pos(&mut tbl.fds[fd], next + 2);
        return val_reply(off as u64);
    }

    // `/proc/<pid>/ns` (see `gen_proc_ns_dir`): one symlink per namespace kind.
    if dir_is_tmp && proc_ns_dir(dir_path, pid).is_some() {
        let mut i = pos - 2;
        while i < sched::ns::NS_NAMES.len() {
            match write_dirent(buf, off, count, 0x6400_0000 + i as u64, sched::ns::NS_NAMES[i], 10) {
                Some(r) => { off += r; i += 1; }
                None => break,
            }
        }
        if off == 0 && i < sched::ns::NS_NAMES.len() { return err_reply(-22); }
        set_dir_pos(&mut tbl.fds[fd], i + 2);
        return val_reply(off as u64);
    }

    // A `/proc/<pid>` directory (see `gen_proc_pid_dir`) holds exactly
    // PROC_PID_FILES; nothing below applies to it.
    // `/proc/<pid>` also lists `task`; `/proc/<pid>/task/<tid>` the files.
    let task_dir = if dir_is_tmp { proc_task_parse(dir_path, pid) } else { None };
    let pid_like = dir_is_tmp && proc_pid_dir(dir_path).is_some();
    if pid_like || matches!(task_dir, Some(Some(ProcTask::Tid { .. }))) {
        let mut i = pos - 2;
        while let Some((name, dt)) = proc_dir_entry(i, pid_like) {
            match write_dirent(buf, off, count, 0x6000_0000 + i as u64, name, dt) {
                Some(r) => { off += r; i += 1; }
                None => break,
            }
        }
        set_dir_pos(&mut tbl.fds[fd], i + 2);
        return val_reply(off as u64);
    }
    // `/proc/<pid>/task`: one directory per thread of the group. The cursor
    // is tid-based like /proc's own (see below).
    if let Some(Some(ProcTask::Dir { tgid })) = task_dir {
        const TID_POS: usize = 1 << 24;
        let mut next = if pos >= TID_POS { (pos - TID_POS) as u32 } else { 0 };
        let mut tids = [0u32; 256];
        let n = sched::thread_ids_from(tgid, next, &mut tids);
        let mut stopped = false;
        for &t in &tids[..n] {
            let mut d = [0u8; 10]; let mut k = d.len(); let mut v = pv(t);
            loop { k -= 1; d[k] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
            match write_dirent(buf, off, count, 0x7800_0000 + t as u64, &d[k..], 4) {
                Some(r) => { off += r; next = t + 1; }
                None => { next = t; stopped = true; break; }
            }
        }
        if off == 0 && stopped { return err_reply(-22); } // EINVAL: buffer too small
        set_dir_pos(&mut tbl.fds[fd], TID_POS + next as usize);
        return val_reply(off as u64);
    }

    // A tmpfs directory lists only TMP_FILES children (no static table or
    // device registry entry lives below /tmp). Past "." and ".." the cursor
    // is the next TMP_FILES index + 2: a slot index does not move when an
    // earlier entry is unlinked, where the ordinal cursor below skipped as
    // many survivors as had just been removed (rm -rf's read-unlink-read
    // loop then failed with ENOTEMPTY; same bug as f2fs had).
    if dir_is_tmp {
        let tmp = TMP_FILES.lock();
        // Only slots whose parent-path hash matches are read (lane tmpfast).
        let mut i = MAX_TMP_FILES;
        for c in tmp.children_from(dir_path, pos - 2) {
            let e = &tmp[c];
            let d_type = if e.is_dir { 4 } else if e.is_fifo { 1 } else if e.is_link { 10 }
                         else if e.is_sock { 12 } else if e.devtype == 1 { 2 }
                         else if e.devtype == 2 { 6 } else { 8 };
            match write_dirent(buf, off, count, 400 + c as u64, &e.path()[dir_len + 1..], d_type) {
                Some(r) => off += r,
                None => { i = c; break; }
            }
        }
        drop(tmp);
        if off == 0 && i < MAX_TMP_FILES { return err_reply(-22); } // EINVAL: buffer too small
        set_dir_pos(&mut tbl.fds[fd], i + 2);
        return val_reply(off as u64);
    }

    let mut virtual_idx = 2usize;

    // RAMFS directories
    for &child_dir in RAMFS_DIRS {
        if child_dir == dir_path || child_dir == VOID_ROOT || child_dir == mnt::SB_ROOT { continue; }
        let is_root = dir_path == b"/";
        let is_child = if is_root {
            child_dir.len() > 1 && child_dir[0] == b'/' && !child_dir[1..].contains(&b'/')
        } else {
            child_dir.len() > dir_len + 1 && child_dir.starts_with(dir_path) && child_dir[dir_len] == b'/' && !child_dir[dir_len+1..].contains(&b'/')
        };
        if is_child {
            if virtual_idx >= pos {
                let name = if is_root { &child_dir[1..] } else { &child_dir[dir_len+1..] };
                if let Some(r) = write_dirent(buf, off, count, virtual_idx as u64 + 100, name, 4) {
                    off += r; pos += 1;
                } else {
                    set_dir_pos(&mut tbl.fds[fd], pos);
                    return val_reply(off as u64);
                }
            }
            virtual_idx += 1;
        }
    }

    // RAMFS files
    for entry in RAMFS {
        let is_root = dir_path == b"/";
        let is_child = if is_root {
            entry.path.len() > 1 && entry.path[0] == b'/' && !entry.path[1..].contains(&b'/')
        } else {
            entry.path.len() > dir_len + 1 && entry.path.starts_with(dir_path) && entry.path[dir_len] == b'/' && !entry.path[dir_len+1..].contains(&b'/')
        };
        if is_child {
            if virtual_idx >= pos {
                let name = if is_root { &entry.path[1..] } else { &entry.path[dir_len+1..] };
                if let Some(r) = write_dirent(buf, off, count, virtual_idx as u64 + 200, name, 8) {
                    off += r; pos += 1;
                } else {
                    set_dir_pos(&mut tbl.fds[fd], pos);
                    return val_reply(off as u64);
                }
            }
            virtual_idx += 1;
        }
    }

    // Block device nodes (/dev). Synthesized from the live registry for the
    // same reason /dev/pts is: there is no static table, and `ls /dev` plus
    // any tool that scans the directory for `vd*`/`loop*` has to see them.
    if dir_path == b"/dev" {
        let mut i = 0usize;
        loop {
            let mut name = [0u8; block::NAME_MAX];
            let nlen = match block::dev_node_name(i, &mut name) { Some(n) => n, None => break };
            if virtual_idx >= pos {
                if let Some(r) = write_dirent(buf, off, count, 0x4000_0000 + i as u64,
                                              &name[..nlen], 6 /* DT_BLK */) {
                    off += r; pos += 1;
                } else {
                    set_dir_pos(&mut tbl.fds[fd], pos);
                    return val_reply(off as u64);
                }
            }
            virtual_idx += 1;
            i += 1;
        }
        if virtual_idx >= pos {
            if let Some(r) = write_dirent(buf, off, count, 0x4000_0000 + 0xFFFE,
                                          b"loop-control", 2 /* DT_CHR */) {
                off += r; pos += 1;
            } else {
                set_dir_pos(&mut tbl.fds[fd], pos);
                return val_reply(off as u64);
            }
        }
        virtual_idx += 1;
    }

    // Live pty slaves (/dev/pts). The pool is the only source of truth here —
    // there is no static table to walk — and `ls /dev/pts` is how a user (and
    // ttyname's fallback scan) finds which pairs exist.
    if let Some(dinst) = devpts::dir_inst(dir_path) {
        // Snapshot under the pool lock, then emit: write_dirent touches user
        // memory, which can demand-page, and a fault taken while holding a
        // server spinlock re-enters the scheduler (see the 82d0cc3 rule).
        let mut live = [false; tty_server::pty::MAX_PTYS];
        tty_server::pty::each_in(dinst, |n| { if n < live.len() { live[n] = true; } });
        for (n, &alive) in live.iter().enumerate() {
            if !alive { continue; }
            if virtual_idx >= pos {
                // Two decimal digits cover MAX_PTYS with room to spare.
                let mut name = [0u8; 3];
                let nlen = if n < 10 {
                    name[0] = b'0' + n as u8; 1
                } else {
                    name[0] = b'0' + (n / 10) as u8; name[1] = b'0' + (n % 10) as u8; 2
                };
                if let Some(r) = write_dirent(buf, off, count, pty_ino(n as u16, false),
                                              &name[..nlen], 2 /* DT_CHR */) {
                    off += r; pos += 1;
                } else {
                    set_dir_pos(&mut tbl.fds[fd], pos);
                    return val_reply(off as u64);
                }
            }
            virtual_idx += 1;
        }
    }

    // POSIX message queues of the caller's IPC namespace.
    if dir_path == b"/dev/mqueue" {
        for (name, ino) in mqueue::list(pid) {
            if virtual_idx >= pos {
                if let Some(r) = write_dirent(buf, off, count, ino, &name, 8 /* DT_REG */) {
                    off += r; pos += 1;
                } else {
                    set_dir_pos(&mut tbl.fds[fd], pos);
                    return val_reply(off as u64);
                }
            }
            virtual_idx += 1;
        }
    }

    // Writable tmpfs pool (/tmp and anything mkdir'd underneath it).
    //
    // Without this pass every tmpfs directory enumerated as empty: `/tmp`
    // opens as RamFile{b"/tmp"} (it is a RAMFS_DIRS entry) and the loops
    // above only ever walk the *static* tables, so `ls /tmp` exited 0 with
    // no output even though the files were readable by name.
    // Named direct children only (ephemeral /proc snapshots park under fake
    // "/tmp/.proc_N" paths; they are fd-owned scratch, not directory
    // contents), found through the pool's parent-hash array. A pool path is
    // never a direct child of "/" (they live under /tmp, /dev/shm, /run/user).
    if dir_path != b"/" {
        let tmp = TMP_FILES.lock();
        let mut full = false;
        for i in tmp.children_from(dir_path, 0) {
            let e = &tmp[i];
            let name = &e.path()[dir_len + 1..];
            if virtual_idx >= pos {
                // DT_DIR / DT_FIFO / DT_REG
                // DT_DIR / DT_FIFO / DT_LNK / DT_REG. `ls -F` prints the '@'
                // suffix and `ls -l` the 'l' type char straight off this.
                let d_type = if e.is_dir { 4 }
                             else if e.is_fifo { 1 }
                             else if e.is_link { 10 }
                             else if e.is_sock { 12 } // DT_SOCK
                             else if e.devtype == 1 { 2 } // DT_CHR
                             else if e.devtype == 2 { 6 } // DT_BLK
                             else { 8 };
                if let Some(r) = write_dirent(buf, off, count, 400 + i as u64, name, d_type) {
                    off += r; pos += 1;
                } else {
                    full = true;
                    break;
                }
            }
            virtual_idx += 1;
        }
        drop(tmp);
        if full {
            set_dir_pos(&mut tbl.fds[fd], pos);
            return val_reply(off as u64);
        }
    }

    // Dynamic devices
    {
        let devices = DYNAMIC_DEVICES.lock();
        let dir_len = dir_path.len();
        let mut seen_dirs: [Option<&'static str>; 4] = [None; 4]; // Avoid duplicate directory entries

        for device in devices.iter() {
            if device.in_use && device.path.as_bytes().starts_with(dir_path) {
                let rel_path = &device.path[dir_len..];
                if rel_path.starts_with('/') {
                    let name = &rel_path[1..];
                    if let Some(slash_pos) = name.find('/') {
                        // This is a directory (e.g., "dri" in "/dev/dri/card0" when listing "/dev")
                        let dir_name = &name[..slash_pos];

                        // ...unless the RAMFS_DIRS pass above already emitted
                        // it. /dev/dri is listed there now (so that opendir and
                        // stat work on it), and without this guard `ls /dev`
                        // would show it twice — once from each pass. The index
                        // bookkeeping stays right because that pass already
                        // consumed a virtual_idx for it.
                        let full = &device.path.as_bytes()[..dir_len + 1 + slash_pos];
                        if RAMFS_DIRS.iter().any(|&d| d == full) { continue; }

                        // Check if we already added this directory
                        if !seen_dirs.iter().any(|&d| d == Some(dir_name)) {
                            if virtual_idx >= pos {
                                if let Some(r) = write_dirent(buf, off, count, virtual_idx as u64 + 300, dir_name.as_bytes(), 4) { // 4 = DT_DIR
                                    off += r; pos += 1;
                                } else {
                                    drop(devices);
                                    set_dir_pos(&mut tbl.fds[fd], pos);
                                    return val_reply(off as u64);
                                }
                            }
                            virtual_idx += 1;
                            // Add to seen dirs
                            if let Some(empty_slot) = seen_dirs.iter_mut().find(|s| s.is_none()) {
                                *empty_slot = Some(dir_name);
                            }
                        }
                    } else if !name.is_empty() {
                        // This is the device itself (e.g., "card0" when listing "/dev/dri")
                        if virtual_idx >= pos {
                            // Every registered device is a character device
                            // (stat reports S_IFCHR), so DT_CHR.
                            if let Some(r) = write_dirent(buf, off, count, virtual_idx as u64 + 300, name.as_bytes(), 2) { // 2 = DT_CHR
                                off += r; pos += 1;
                            } else {
                                drop(devices);
                                set_dir_pos(&mut tbl.fds[fd], pos);
                                    return val_reply(off as u64);
                            }
                        }
                        virtual_idx += 1;
                    }
                }
            }
        }
    }

    // Initrd files (Deduplicated)
    let initrd_base = INITRD_BASE.load(atomic::Ordering::SeqCst);
    let initrd_size = INITRD_SIZE.load(atomic::Ordering::SeqCst);
    if initrd_base != 0 && initrd_size != 0 {
        let initrd_ptr = mm::phys_to_virt(initrd_base) as *const u8;
        let data = unsafe { core::slice::from_raw_parts(initrd_ptr, initrd_size) };
        if data.len() > 6 && &data[0..6] == b"070701" {
            let mut offset = 0;
            loop {
                if offset + 110 > data.len() { break; }
                let header = &data[offset..offset+110];
                if &header[0..6] != b"070701" { break; }
                let namesize = parse_cpio_hex(&header[94..102]);
                let filesize = parse_cpio_hex(&header[54..62]);
                let mode = parse_cpio_hex(&header[14..22]);
                let name_offset = offset + 110;
                if name_offset + namesize > data.len() { break; }
                let name_bytes = &data[name_offset..name_offset + namesize - 1];
                if name_bytes == b"TRAILER!!!" { break; }

                // match_name is the CPIO path without ./ prefix
                let mut match_name = if name_bytes.starts_with(b"./") { &name_bytes[2..] } else { name_bytes };
                if match_name.starts_with(b"/") { match_name = &match_name[1..]; }

                let is_root = dir_path == b"/";
                // match_dir is dir_path without leading /
                let mut match_dir = if dir_path.starts_with(b"/") { &dir_path[1..] } else { dir_path };
                if match_dir.ends_with(b"/") { match_dir = &match_dir[..match_dir.len()-1]; }

                let is_match = if is_root {
                    !match_name.is_empty() && !match_name.contains(&b'/') && match_name != b"."
                } else if !match_dir.is_empty() && match_name.starts_with(match_dir) && match_name.len() > match_dir.len() && match_name[match_dir.len()] == b'/' {
                    let r = &match_name[match_dir.len()+1..];
                    !r.is_empty() && !r.contains(&b'/')
                } else {
                    false
                };

                if is_match && !is_duplicated(name_bytes) {
                    if virtual_idx >= pos {
                        let d_type = if (mode & 0o170000) == 0o040000 { 4 } else { 8 };
                        let child_name = if is_root { match_name } else { &match_name[match_dir.len()+1..] };
                        if let Some(r) = write_dirent(buf, off, count, 1000 + offset as u64, child_name, d_type) {
                            off += r; pos += 1;
                        } else {
                            set_dir_pos(&mut tbl.fds[fd], pos);
                            return val_reply(off as u64);
                        }
                    }
                    virtual_idx += 1;
                }
                let file_offset = (name_offset + namesize + 3) & !3;
                let next_offset = (file_offset + filesize + 3) & !3;
                if next_offset <= offset { break; }
                offset = next_offset;
            }
        }
    }

    // One directory per process under /proc. The cursor past the static
    // entries is pid-based (PROC_PID_POS + next pid), not an index, so a
    // process exiting between two getdents calls neither repeats nor skips
    // an entry.
    if dir_path == b"/proc" {
        const PROC_PID_POS: usize = 1 << 24;
        let mut next = if pos >= PROC_PID_POS { (pos - PROC_PID_POS) as u32 } else { 0 };
        // Snapshot first: write_dirent touches user memory (may fault).
        let mut pids = [0u32; 256];
        let n = sched::process_pids_from(next, &mut pids);
        let rn = sched::pidns::cur();
        for &p in &pids[..n] {
            let pvv = if rn == 0 { p } else { sched::pidns::to_v(rn, p) };
            if pvv == 0 { next = p + 1; continue; }
            let mut name = [0u8; 10]; let mut k = 0; let mut v = pvv;
            let mut d = [0u8; 10];
            loop { d[k] = b'0' + (v % 10) as u8; k += 1; v /= 10; if v == 0 { break; } }
            let len = k;
            for j in 0..len { name[j] = d[len - 1 - j]; }
            match write_dirent(buf, off, count, 0x7000_0000 + p as u64, &name[..len], 4) {
                Some(r) => { off += r; next = p + 1; }
                None => { next = p; break; }
            }
        }
        if n > 0 || pos >= PROC_PID_POS { pos = PROC_PID_POS + next as usize; }
        else { pos = PROC_PID_POS; }
    }

    set_dir_pos(&mut tbl.fds[fd], pos);
    val_reply(off as u64)
}

/// Look up a file by absolute path in the initrd CPIO archive.
/// Returns a `'static` slice to the raw file bytes, or `None` if not found.
fn find_in_initrd(path: &[u8]) -> Option<&'static [u8]> {
    let initrd_base = INITRD_BASE.load(atomic::Ordering::SeqCst);
    let initrd_size = INITRD_SIZE.load(atomic::Ordering::SeqCst);
    if initrd_base == 0 || initrd_size == 0 { return None; }

    let ptr = mm::phys_to_virt(initrd_base) as *const u8;
    let data: &'static [u8] = unsafe { core::slice::from_raw_parts(ptr, initrd_size) };

    if data.len() < 6 || &data[0..6] != b"070701" { return None; }

    // Strip leading slash from the query path for comparison.
    let query = if path.starts_with(b"/") { &path[1..] } else { path };

    let mut offset = 0usize;
    loop {
        if offset + 110 > data.len() { break; }
        let header = &data[offset..offset + 110];
        if &header[0..6] != b"070701" { break; }
        let namesize = parse_cpio_hex(&header[94..102]);
        let filesize = parse_cpio_hex(&header[54..62]);
        let name_off = offset + 110;
        if name_off + namesize > data.len() { break; }
        let name = &data[name_off..name_off + namesize.saturating_sub(1)];
        if name == b"TRAILER!!!" { break; }

        // Normalise CPIO name: strip "./" or leading "/"
        let mut cpio_name = if name.starts_with(b"./") { &name[2..] } else { name };
        if cpio_name.starts_with(b"/") { cpio_name = &cpio_name[1..]; }

        let file_off = (name_off + namesize + 3) & !3;
        if cpio_name == query {
            let end = file_off + filesize;
            if end <= data.len() {
                return Some(&data[file_off..end]);
            }
        }
        let next_off = (file_off + filesize + 3) & !3;
        if next_off <= offset { break; }
        offset = next_off;
    }
    None
}

fn parse_cpio_hex(s: &[u8]) -> usize {
    let mut val = 0usize;
    for &b in s {
        let digit = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return 0,
        };
        val = (val << 4) | (digit as usize);
    }
    val
}

fn is_duplicated(path: &[u8]) -> bool {
    let mut abs_path = [0u8; 256];
    
    let mut src = if path.starts_with(b"./") { &path[2..] } else { path };
    if src.starts_with(b"/") { src = &src[1..]; }

    // Convert to absolute for comparison with RAMFS
    abs_path[0] = b'/';
    let mut len = 1;
    let copy_len = src.len().min(254);
    abs_path[len..len + copy_len].copy_from_slice(&src[..copy_len]);
    len += copy_len;
    
    let p = &abs_path[..len];

    for entry in RAMFS {
        if entry.path == p { return true; }
    }
    for &dir in RAMFS_DIRS {
        if dir == p { return true; }
    }
    false
}

/// Copy a NUL-terminated path argument (a kernel buffer the kernel built, see
/// `KPath`) of at most HOST_PATH_MAX bytes, NUL included. `None` for a null
/// pointer or no NUL in range. Heap-held: handlers keep two of these, and
/// several frames of them can be live on one kernel stack.
fn read_cstr_raw(ptr: usize) -> Option<(Vec<u8>, usize)> {
    if ptr == 0 { return None; }
    let p = ptr as *const u8;
    let mut len = 0usize;
    while len < HOST_PATH_MAX {
        if unsafe { *p.add(len) } == 0 {
            let v = unsafe { core::slice::from_raw_parts(p, len) }.to_vec();
            return Some((v, len));
        }
        len += 1;
    }
    None
}

fn path_eq(buf: &[u8], len: usize, path: &[u8]) -> bool {
    len == path.len() && buf[..len] == *path
}

static _SERVER_PORT_ID: atomic::AtomicU32 = atomic::AtomicU32::new(u32::MAX);

fn handle_eventfd(pid: u32, initval: u64, flags: u32) -> Message {
    // Preserve EFD_NONBLOCK/EFD_CLOEXEC (== O_NONBLOCK/O_CLOEXEC): a reader that
    // asked for a non-blocking eventfd must see EAGAIN, not a kernel yield-spin.
    let stored = flags & (O_NONBLOCK_FL | O_CLOEXEC);
    let mut counters = EVENTFD_COUNTERS.lock();
    let slot = loop {
        match counters.iter().position(|&v| v == u64::MAX) {
            Some(s) => break s,
            None => {
                drop(counters);
                // The pool is system-wide, so its exhaustion is ENFILE ("file
                // table overflow"), not the caller's own EMFILE -- a fresh
                // process holding a handful of fds must not be told *it* has
                // too many.
                if !eventfd_grow() { report_pool_full("eventfd", MAX_EVENTFDS); return err_reply(-23); }
                counters = EVENTFD_COUNTERS.lock();
            }
        }
    };
    counters[slot] = if initval == u64::MAX { u64::MAX - 1 } else { initval };
    drop(counters);
    EVENTFD_SEM.lock()[slot] = flags & 1 != 0; // EFD_SEMAPHORE
    EVENTFD_SEQ.lock()[slot] = 0;   // fresh slot: no stale edge from a prior tenant
    EVENTFD_REFS.lock()[slot] = 1;  // one fd references it until dup'd/closed
    let mut tbls = TblSlot::new();
    let tbl = match get_or_create(pid, &mut tbls) {
        Some(t) => t, None => { EVENTFD_COUNTERS.lock()[slot] = u64::MAX; EVENTFD_REFS.lock()[slot] = 0; return err_reply(-23); }
    };
    let fd = match tbl.alloc_fd() {
        Some(f) => f, None => { EVENTFD_COUNTERS.lock()[slot] = u64::MAX; EVENTFD_REFS.lock()[slot] = 0; return err_reply(-24); }
    };
    tbl.fds.set(fd, FdEntry { kind: VnodeKind::EventFd { slot }, flags: stored, in_use: true, ofd: ofd::alloc() });
    val_reply(fd as u64)
}

/// Install a pidfd naming process `target` in `pid`'s table. `flags` may carry
/// O_NONBLOCK (PIDFD_NONBLOCK); pidfds are always close-on-exec, as on Linux.
/// The kernel has already checked that `target` is a live process.
pub fn install_pidfd(pid: u32, target: u32, flags: u32) -> isize {
    let pid = sched::tgid_of(pid);
    let stored = (flags & O_NONBLOCK_FL) | O_CLOEXEC;
    let mut tbls = TblSlot::new();
    let tbl = match get_or_create(pid, &mut tbls) { Some(t) => t, None => return -23 };
    let fd = match tbl.alloc_fd() { Some(f) => f, None => return -24 };
    tbl.fds.set(fd, FdEntry { kind: VnodeKind::PidFd { pid: target }, flags: stored, in_use: true, ofd: ofd::alloc() });
    fd as isize
}

/// Install a bpf program fd for program `id` (the caller's reference moves to
/// the fd). Always close-on-exec, as on Linux. On failure the reference stays
/// with the caller.
pub fn install_bpf_prog(pid: u32, id: u32) -> isize {
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    let tbl = match get_or_create(pid, &mut tbls) { Some(t) => t, None => return -23 };
    let fd = match tbl.alloc_fd() { Some(f) => f, None => return -24 };
    tbl.fds.set(fd, FdEntry { kind: VnodeKind::BpfProg { id }, flags: O_CLOEXEC, in_use: true, ofd: ofd::alloc() });
    fd as isize
}

/// The program a bpf fd names: `Ok(id)`, `Err(-9)` for a closed fd, `Err(-22)`
/// for an open fd that is not a bpf program.
pub fn bpf_prog_of_fd(pid: u32, fd: usize) -> Result<u32, i32> {
    if fd >= FD_NUMBER_CEIL { return Err(-9); }
    let pid = sched::tgid_of(pid);
    let kind = with_tbl(pid, |t| t.fds.get(fd).map(|e| e.kind)).flatten().ok_or(-9)?;
    match kind { VnodeKind::BpfProg { id } => Ok(id), _ => Err(-22) }
}

/// The live cgroup a directory fd names (bpf attach target): `Ok(cg)`, or
/// `Err(-9)` for a closed fd / anything that is not a cgroupfs directory.
pub fn cgroup_of_fd(pid: u32, fd: usize) -> Result<sched::cgroup::CgId, i32> {
    if fd >= FD_NUMBER_CEIL { return Err(-9); }
    let pid = sched::tgid_of(pid);
    let kind = with_tbl(pid, |t| t.fds.get(fd).map(|e| e.kind)).flatten().ok_or(-9)?;
    match kind {
        VnodeKind::SysBlock { dev, .. } if cgroupfs::is_dev(dev) => {
            let cg = cgroupfs::dev_cg(dev);
            if sched::cgroup::alive(cg) { Ok(cg) } else { Err(-9) }
        }
        _ => Err(-9),
    }
}

/// Point pidfd `fd` of process `pid` at `target`. clone(CLONE_PIDFD) reserves
/// the descriptor (naming pid 0) before the child exists, so a full table
/// fails the clone instead of orphaning a child, and names the child here.
pub fn pidfd_retarget(pid: u32, fd: usize, target: u32) {
    if fd >= FD_NUMBER_CEIL { return; }
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    if let Some(tbl) = find_tbl(pid, &mut tbls) {
        if tbl.fds.used(fd) {
            if let VnodeKind::PidFd { .. } = tbl.fds[fd].kind {
                tbl.fds[fd].kind = VnodeKind::PidFd { pid: target };
            }
        }
    }
}

/// The process a pidfd names: `Ok(pid)`, `Err(-9)` for a closed fd, `Err(-22)`
/// for an open fd that is not a pidfd (Linux's EBADF/EINVAL split).
pub fn pidfd_target(pid: u32, fd: usize) -> Result<u32, i32> {
    if fd >= FD_NUMBER_CEIL { return Err(-9); }
    let pid = sched::tgid_of(pid);
    let kind = with_tbl(pid, |t| t.fds.get(fd).map(|e| e.kind)).flatten().ok_or(-9)?;
    match kind {
        VnodeKind::PidFd { pid } => Ok(pid),
        _ => Err(-22),
    }
}

pub(crate) fn sched_ns_put(id: u16) { sched::ns::put_raw(id); }

/// bind(2)-style mount of /proc/<pid>/ns/<name>: take a reference on that
/// namespace and return the synthetic source the alias mount stands for
/// (see `mnt::ns0::NS_SRC`). `Ok(None)` when `path` is not an nsfs file.
pub fn nsfs_bind_source(path: &[u8], caller: u32) -> Result<Option<Vec<u8>>, isize> {
    let Some(r) = proc_ns_file(path, caller) else { return Ok(None) };
    let Some((tpid, k)) = r else { return Err(-2) };
    let pr = sched::cred::ptrace_read_check(caller, tpid);
    if pr != 0 { return Err(pr as isize); }
    let Some(id) = sched::ns::grab(tpid, k) else { return Err(-2) };
    let mut v = alias::NS_SRC.to_vec();
    push_dec(&mut v, k as u32); v.push(b'/'); push_dec(&mut v, id as u32);
    Ok(Some(v))
}
fn push_dec(v: &mut Vec<u8>, mut n: u32) {
    let mut d = [0u8; 10]; let mut i = 0;
    loop { d[i] = b'0' + (n % 10) as u8; i += 1; n /= 10; if n == 0 { break; } }
    while i > 0 { i -= 1; v.push(d[i]); }
}

/// The namespace an nsfs-like fd names: `Ok((kind, id))`, `Err(-9)` closed,
/// `Err(-22)` an open fd of another kind.
pub fn nsfd_target(pid: u32, fd: usize) -> Result<(u8, u16), i32> {
    if fd >= FD_NUMBER_CEIL { return Err(-9); }
    let pid = sched::tgid_of(pid);
    let kind = with_tbl(pid, |t| t.fds.get(fd).map(|e| e.kind)).flatten().ok_or(-9)?;
    match kind {
        VnodeKind::NsFd { kind, id } => Ok((kind, id)),
        _ => Err(-22),
    }
}

/// Install an nsfs-like fd for namespace `id` of link `kind`, taking a reference.
pub fn install_nsfd(pid: u32, kind: u8, id: u16) -> isize {
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    let tbl = match get_or_create(pid, &mut tbls) { Some(t) => t, None => return -23 };
    let fd = match tbl.alloc_fd() { Some(f) => f, None => return -24 };
    sched::ns::hold_raw(id);
    tbl.fds.set(fd, FdEntry { kind: VnodeKind::NsFd { kind, id }, flags: O_CLOEXEC, in_use: true, ofd: ofd::alloc() });
    fd as isize
}

/// ioctl on an nsfs-like fd: NS_GET_USERNS / NS_GET_PARENT / NS_GET_NSTYPE /
/// NS_GET_OWNER_UID.
fn ns_ioctl(pid: u32, kind: u8, id: u16, cmd: usize, arg: usize) -> isize {
    let k = kind as usize;
    match cmd {
        0xb701 => { // NS_GET_USERNS: the owning user namespace (the parent of a user namespace)
            if k == 0 && id == 0 { return -1; }
            install_nsfd(pid, 0, sched::ns::owner_userns(k, id))
        }
        0xb702 => { // NS_GET_PARENT: user and pid namespaces only
            if !matches!(k, 0 | 1 | 7) { return -22; }
            if id == 0 { return -1; }
            install_nsfd(pid, if k == 0 { 0 } else { 1 }, sched::ns::parent(id))
        }
        0xb703 => sched::ns::clone_flag(k) as isize,
        0xb704 => { // NS_GET_OWNER_UID
            if k != 0 { return -22; }
            let uid = sched::ns::view_id(0, sched::ns::owner(id).0);
            if arg == 0 { return -14; }
            unsafe { sched::uaccess::copy_raw(arg as *mut u8, &uid as *const u32 as *const u8, 4); }
            0
        }
        _ => -25,
    }
}

/// Re-masked signalfd descriptions: (ofd id, mask). A signalfd's mask belongs
/// to its open file description, not to one fd entry: signalfd4(fd, newmask)
/// used to rewrite only the caller's entry, so a dup, a fork child's copy or
/// an SCM_RIGHTS copy went on reading with the old mask. Only re-masked
/// descriptions appear here (creation stores the mask in the vnode), so the
/// table stays tiny; dead ids are swept on every insert. Leaf lock.
static SIGFD_MASKS: spin::Mutex<alloc::vec::Vec<(u32, u64)>> = spin::Mutex::new(alloc::vec::Vec::new());

/// The live mask of the signalfd description `ofd` (`initial` = its creation
/// mask, used until the first re-mask, or when the description has no id).
fn signalfd_mask(ofd: u32, initial: u64) -> u64 {
    if ofd == 0 { return initial; }
    let t = SIGFD_MASKS.lock();
    if t.is_empty() { return initial; }
    t.iter().find(|e| e.0 == ofd).map_or(initial, |e| e.1)
}

/// signalfd4(existing_fd, mask, flags). `existing_fd == usize::MAX` (the kernel
/// maps the -1 sentinel to this) allocates a new SignalFd vnode; otherwise the
/// named signalfd's mask is replaced — on its open file description, so every
/// copy of it sees the new mask. No backing pool, so close needs no teardown.
fn handle_signalfd_create(pid: u32, existing_fd: usize, mask: u64, flags: u32) -> Message {
    let mut tbls = TblSlot::new();
    if existing_fd != usize::MAX {
        let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
        if !tbl.fds.used(existing_fd) { return err_reply(-9); }
        match tbl.fds[existing_fd].kind {
            VnodeKind::SignalFd { ofd, .. } => {
                // The entry keeps the new mask too: it is the fallback when the
                // description has no id (ofd 0, table full).
                tbl.fds[existing_fd].kind = VnodeKind::SignalFd { mask, ofd };
                drop(tbls);
                if ofd != 0 {
                    let mut t = SIGFD_MASKS.lock();
                    t.retain(|e| e.0 != ofd && ofd::live(e.0));
                    let _ = t.try_reserve(1);
                    if t.len() < t.capacity() { t.push((ofd, mask)); }
                }
                // A wider mask can make an already-pending signal readable:
                // wake the signalfd's pollers on the tag its probe reports
                // (invariant 16).
                sched::wake_poll_tagged(signalfd_wake_tag());
                val_reply(existing_fd as u64)
            }
            _ => err_reply(-22), // EINVAL — fd is not a signalfd
        }
    } else {
        // Preserve SFD_NONBLOCK/SFD_CLOEXEC (== O_NONBLOCK/O_CLOEXEC).
        let stored = flags & (O_NONBLOCK_FL | O_CLOEXEC);
        let tbl = match get_or_create(pid, &mut tbls) { Some(t) => t, None => return err_reply(-23) };
        let fd = match tbl.alloc_fd() { Some(f) => f, None => return err_reply(-24) };
        let id = ofd::alloc();
        tbl.fds.set(fd, FdEntry { kind: VnodeKind::SignalFd { mask, ofd: id }, flags: stored, in_use: true, ofd: id });
        drop(tbls);
        // A recycled id must not inherit a dead description's re-mask.
        if id != 0 {
            let mut t = SIGFD_MASKS.lock();
            if !t.is_empty() { t.retain(|e| e.0 != id); }
        }
        val_reply(fd as u64)
    }
}

/// inotify_init1(flags) → an Inotify fd. `flags` carries IN_CLOEXEC (==
/// O_CLOEXEC, 0x80000) and IN_NONBLOCK (== O_NONBLOCK, 0o4000). These MUST be
/// persisted on the fd: dropping IN_CLOEXEC made every inotify instance
/// survive the execve close-on-exec sweep (cosmic-config's per-key watchers
/// leaked into every child and filled late children's fd tables: EMFILE
/// crash-loop of cosmic-greeter).
///
/// With `vfs.inotify=1` the instance lives on the fd's description
/// (`inotify.rs`); otherwise it is the old silent stub (`inst == 0`: accepted
/// watches, no events).
fn handle_inotify_create(pid: u32, flags: u32) -> Message {
    let real = inotify_enabled();
    let mut tbls = TblSlot::new();
    let tbl = match get_or_create(pid, &mut tbls) { Some(t) => t, None => return err_reply(-23) };
    let fd = match tbl.alloc_fd() { Some(f) => f, None => return err_reply(-24) };
    let stored = flags & (O_CLOEXEC | O_NONBLOCK_FL);
    let o = ofd::alloc();
    let mut inst = 0;
    if real {
        if o == 0 { return err_reply(-23); }
        if !inotify::create(o, &inotify_chain(pid)) { ofd::put(o); return err_reply(-24); }
        // The last put of this description must free the instance.
        ofd::mark_watched(o);
        inst = o;
    }
    tbl.fds.set(fd, FdEntry { kind: VnodeKind::Inotify { inst }, flags: stored, in_use: true, ofd: o });
    val_reply(fd as u64)
}

/// ucounts chain of `pid`: its (userns, kuid), then each ancestor namespace
/// under the kuid that created the child (Linux `inc_ucount`).
fn inotify_chain(pid: u32) -> Vec<inotify::UKey> {
    let mut ns = sched::ns::userns_of(pid);
    let mut k = sched::ruid_of(pid);
    let mut v = Vec::new();
    loop {
        v.push((ns as u16, k));
        if ns == 0 || v.len() > 33 { break; }
        k = sched::ns::owner(ns).0;
        ns = sched::ns::parent(ns);
    }
    v
}

/// `/proc/sys/user/max_<kind>_namespaces` -> ns slot class (sched::ns::limit_*).
fn nslimit_sysctl(path: &[u8]) -> Option<usize> {
    let n = path.strip_prefix(b"/proc/sys/user/")?;
    (1..sched::ns::LIMIT_N).find(|&c| sched::ns::LIMIT_NAMES[c] == n)
}
const PROC_NSLIM_W: &[u8] = b"/tmp/.proc_nslim_";

fn gen_proc_nslimit(c: usize, write: bool) -> Option<VnodeKind> {
    let mut data = [0u8; 12];
    let l = oom_text(sched::ns::limit_get(c) as i32, &mut data);
    let mut buf = [0u8; TMP_BUF_SIZE];
    if !write {
        buf[..l].copy_from_slice(&data[..l]);
        return gen_proc_system_file(&buf, l);
    }
    let mut fake = [0u8; 32];
    fake[..PROC_NSLIM_W.len()].copy_from_slice(PROC_NSLIM_W);
    fake[PROC_NSLIM_W.len()] = b'0' + c as u8;
    let n = PROC_NSLIM_W.len() + 1;
    let mut tmp = TMP_FILES.lock();
    let idx = tmp_free_slot(&mut *tmp)?;
    tmp.claim(idx, b"", true);
    tmp.set_path(idx, &fake[..n]);
    if !tmp_fill_kernel(&mut tmp, idx, &data[..l]) { tmp.release(idx); return None; }
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: true, ofd: 0 })
}

const PROC_DRMH_W: &[u8] = b"/tmp/.proc_drmhold";

/// `/proc/drm/fence_hold`: "1\n" while the fence-retirement hold is active.
/// A write of 0 or 1 routes through `handle_write` (test knob, root only).
fn gen_proc_drm_hold(write: bool) -> Option<VnodeKind> {
    extern "C" { fn kernel_drm_fence_hold_get() -> u32; }
    let data: &[u8] = if unsafe { kernel_drm_fence_hold_get() } != 0 { b"1\n" } else { b"0\n" };
    if !write {
        let mut buf = [0u8; TMP_BUF_SIZE];
        buf[..data.len()].copy_from_slice(data);
        return gen_proc_system_file(&buf, data.len());
    }
    let mut tmp = TMP_FILES.lock();
    let idx = tmp_free_slot(&mut *tmp)?;
    tmp.claim(idx, b"", true);
    tmp.set_path(idx, PROC_DRMH_W);
    if !tmp_fill_kernel(&mut tmp, idx, data) { tmp.release(idx); return None; }
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: true, ofd: 0 })
}

/// `/proc/sys/fs/inotify/<name>` -> limit index.
fn inotify_sysctl(path: &[u8]) -> Option<usize> {
    match path.strip_prefix(b"/proc/sys/fs/inotify/")? {
        b"max_queued_events" => Some(0), b"max_user_instances" => Some(1), b"max_user_watches" => Some(2),
        b"events_queued" => Some(3), b"events_coalesced" => Some(4), b"events_overflow" => Some(5),
        b"instances" => Some(6), b"watches" => Some(7), _ => None,
    }
}
const PROC_INOT_W: &[u8] = b"/tmp/.proc_inotlim_";

fn gen_proc_inotify(k: usize, write: bool) -> Option<VnodeKind> {
    let mut data = [0u8; 12];
    let l = oom_text(inotify::limit_get(k) as i32, &mut data);
    let mut buf = [0u8; TMP_BUF_SIZE];
    if !write {
        buf[..l].copy_from_slice(&data[..l]);
        return gen_proc_system_file(&buf, l);
    }
    let mut fake = [0u8; 32];
    fake[..PROC_INOT_W.len()].copy_from_slice(PROC_INOT_W);
    fake[PROC_INOT_W.len()] = b'0' + k as u8;
    let n = PROC_INOT_W.len() + 1;
    let mut tmp = TMP_FILES.lock();
    let idx = tmp_free_slot(&mut *tmp)?;
    tmp.claim(idx, b"", true);
    tmp.set_path(idx, &fake[..n]);
    if !tmp_fill_kernel(&mut tmp, idx, &data[..l]) { tmp.release(idx); return None; }
    Some(VnodeKind::TmpFile { idx, pos: 0, writable: true, ofd: 0 })
}

/// Fake watch descriptors of the silent stub (`vfs.inotify=0`).
static STUB_WD: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

/// The instance behind `fd`, or the errno (EBADF / EINVAL).
fn inotify_inst_of(pid: u32, fd: usize) -> Result<u32, i32> {
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return Err(-9) };
    if !tbl.fds.used(fd) { return Err(-9); }
    match &tbl.fds[fd].kind { VnodeKind::Inotify { inst } => Ok(*inst), _ => Err(-22) }
}

/// inotify_add_watch(fd, path, mask) → watch descriptor. `path_ptr` is already
/// the backing path (path_args). Search permission on the components comes from
/// the lookup; read permission on the target is checked here (MAY_READ).
fn handle_inotify_add(pid: u32, fd: usize, path_ptr: usize, mask: u32) -> Message {
    let inst = match inotify_inst_of(pid, fd) { Ok(i) => i, Err(e) => return err_reply(e) };
    if inst == 0 {
        // Silent stub: the kernel only sends the fd.
        return val_reply(STUB_WD.fetch_add(1, core::sync::atomic::Ordering::Relaxed) as u64);
    }
    if mask & inotify::IN_ALL_EVENTS == 0 { return err_reply(-22); }
    if mask & inotify::IN_MASK_ADD != 0 && mask & inotify::IN_MASK_CREATE != 0 { return err_reply(-22); }
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];
    let path: Vec<u8>;
    if let Some(tp) = tmpfs_path(raw) {
        let cred = cred_of(pid);
        let tmp = TMP_FILES.lock();
        if !is_tmpfs_root(tp) {
            let parent = match tmp_parent(tp) { Some(p) => p, None => return err_reply(-2) };
            if let Err(e) = tmp_dir_may(&tmp, parent, &cred, xattr::MAY_EXEC) { return err_reply(e); }
            let idx = match tmp_find(&tmp, tp) { Some(i) => i, None => return err_reply(-2) };
            if mask & inotify::IN_ONLYDIR != 0 && !tmp[idx].is_dir { return err_reply(-20); }
            if !tmp_may(&tmp, idx, &cred, xattr::MAY_READ) { return err_reply(-13); }
        }
        path = tp.to_vec();
    } else {
        // Other filesystems: accepted (existence and read permission checked
        // by their own access path); no events until a later lane.
        let r = reply_val(&handle_access(pid, path_ptr, 4, true));
        if r == -38 {
            let mut st = [0u8; STAT_SIZE];
            let sr = reply_val(&handle_stat(path_ptr, st.as_mut_ptr() as usize));
            if sr < 0 { return err_reply(sr as i32); }
        } else if r < 0 {
            return err_reply(r as i32);
        }
        path = strip_trailing_slash(raw).to_vec();
    }
    let ident = inotify_ident(&path);
    let (id, nlink, isdir) = ident.map_or(((0, 0), 1, false), |i| (i.id, i.nlink, i.isdir));
    match inotify::add(inst, &path, mask, id, nlink, isdir) {
        Ok(wd) => val_reply(wd as u64),
        Err(e) => err_reply(e),
    }
}

/// fanotify_init: a notification-class group is an inotify instance whose
/// marks are watches and whose read format is `struct fanotify_event_metadata`.
fn handle_fanotify_init(pid: u32, flags: u32, evflags: u32) -> Message {
    if !inotify_enabled() { return err_reply(-38); }
    // CLOEXEC | NONBLOCK, class NOTIF (0), UNLIMITED_QUEUE | UNLIMITED_MARKS.
    // FAN_REPORT_FID 0x200, DIR_FID 0x400, NAME 0x800 (NAME needs DIR_FID; FID groups are notification-class only).
    let cls = flags & 0xc; // FAN_CLASS_NOTIF 0, CONTENT 4, PRE_CONTENT 8
    let fid = flags & 0x1e00;
    if flags & !0x1e3f != 0 || cls == 0xc || evflags & 3 == 3 { return err_reply(-22); }
    if fid != 0 && cls != 0 { return err_reply(-22); }
    if fid & inotify::FAN_REPORT_NAME != 0 && fid & inotify::FAN_REPORT_DIR_FID == 0 { return err_reply(-22); }
    // FAN_REPORT_TARGET_FID needs FAN_REPORT_FID and FAN_REPORT_DFID_NAME.
    if fid & inotify::FAN_REPORT_TARGET_FID != 0 && fid & 0xe00 != 0xe00 { return err_reply(-22); }
    if !sched::caps::host_capable_of(pid, sched::caps::CAP_SYS_ADMIN) { return err_reply(-1); }
    let f = (if flags & 1 != 0 { O_CLOEXEC } else { 0 }) | (if flags & 2 != 0 { O_NONBLOCK_FL } else { 0 });
    let r = handle_inotify_create(pid, f);
    let fd = reply_val(&r);
    if fd < 0 { return r; }
    if let Ok(inst) = inotify_inst_of(pid, fd as usize) { inotify::fan_set(inst, evflags & (3 | O_CLOEXEC | O_NONBLOCK_FL), cls != 0, sched::tgid_of(pid) as u32, fid); }
    r
}

/// fanotify_mark(fd, flags, mask, dirfd, path): ADD / REMOVE / FLUSH on an
/// inode, or on the mount rooted at the path with FAN_MARK_MOUNT.
fn handle_fanotify_mark(pid: u32, fd: usize, path_ptr: usize, flags: u32, mask: u64, dirfd: usize) -> Message {
    let inst = match inotify_inst_of(pid, fd) { Ok(i) => i, Err(e) => return err_reply(e) };
    if inst == 0 || !inotify::is_fan(inst) { return err_reply(-22); }
    let op = flags & (inotify::FAN_MARK_ADD | inotify::FAN_MARK_REMOVE | inotify::FAN_MARK_FLUSH);
    // DONT_FOLLOW 4, ONLYDIR 8, MOUNT 0x10, FILESYSTEM 0x100 (treated as MOUNT).
    if op.count_ones() != 1 || flags & !(0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x80 | 0x100) != 0 { return err_reply(-22); }
    let ignore = flags & inotify::FAN_MARK_IGNORED_MASK != 0;
    let surv = flags & inotify::FAN_MARK_IGNORED_SURV_MODIFY != 0;
    if surv && !ignore { return err_reply(-22); }
    let mount = flags & (inotify::FAN_MARK_MOUNT | 0x100) != 0;
    if op == inotify::FAN_MARK_FLUSH { inotify::fan_flush(inst, mount); return ok_reply(); }
    let perm = inotify::is_perm_group(inst);
    let gfid = inotify::fan_fid(inst);
    // FID groups add ATTRIB / DELETE_SELF / MOVE_SELF; DIR_FID groups also the
    // directory-entry events. Neither is valid on a mount mark.
    let fs = flags & 0x100 != 0;
    let fid_ev = if mount && !fs { 0 } else if gfid & inotify::FAN_REPORT_DIR_FID != 0 { inotify::FAN_FID_EVENTS | if gfid & inotify::FAN_REPORT_NAME != 0 { inotify::FAN_RENAME } else { 0 } }
        else if gfid != 0 { inotify::IN_ATTRIB | inotify::IN_DELETE_SELF | inotify::IN_MOVE_SELF } else { 0 };
    let ok_mask = (inotify::FAN_EVENTS | inotify::FAN_OPEN_EXEC | fid_ev | inotify::FAN_ONDIR | inotify::FAN_ON_CHILD | if perm { inotify::FAN_PERM } else { 0 }) as u64;
    if mask & !ok_mask != 0 || mask & (inotify::FAN_EVENTS | inotify::FAN_OPEN_EXEC | fid_ev | inotify::FAN_PERM) as u64 == 0 { return err_reply(-22); }
    let path: Vec<u8> = if path_ptr == 0 {
        let mut buf = alloc::vec![0u8; 4096];
        let r = reply_val(&handle_fd_path(pid, dirfd, buf.as_mut_ptr() as usize, buf.len()));
        if r <= 0 { return err_reply(if r < 0 { r as i32 } else { -9 }); }
        buf.truncate(r as usize);
        tmpfs_path(&buf).unwrap_or_else(|| strip_trailing_slash(&buf)).to_vec()
    } else {
        let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
        let raw = &pbuf[..plen];
        tmpfs_path(raw).unwrap_or_else(|| strip_trailing_slash(raw)).to_vec()
    };
    let (id, nlink, isdir) = match inotify_ident(&path) {
        Some(i) => (i.id, i.nlink, i.isdir),
        None if tmpfs_path(&path).map_or(false, |t| is_tmpfs_root(t)) || is_tmpfs_root(&path) => (inotify::path_id(&path), 1, true),
        None => return err_reply(-2),
    };
    if flags & 8 != 0 && !isdir { return err_reply(-20); }
    let remove = op == inotify::FAN_MARK_REMOVE;
    // A MOUNT / FILESYSTEM mark is on the mount (superblock) the path is on, not on the path.
    let (mpath, mdev) = if mount { fan_mount_of(&path) } else { (path.clone(), 0) };
    match inotify::fan_mark(inst, remove, &mpath, mask as u32, id, nlink, isdir, mount, fs, mdev, ignore, surv) {
        Ok(()) => ok_reply(),
        Err(e) => err_reply(e),
    }
}


/// Handles issued so far (name_to_handle_at, fanotify FID records): (dev, ino)
/// -> the path it was issued for. open_by_handle_at reopens through it and
/// answers ESTALE when the path no longer names the same inode. Supported on
/// every filesystem `inotify::may_watch` accepts (tmpfs, f2fs, ...).
static FH_TABLE: Mutex<alloc::vec::Vec<((u64, u64), alloc::vec::Vec<u8>)>> = Mutex::new(alloc::vec::Vec::new());

pub fn fh_remember(id: (u64, u64), path: &[u8]) {
    let mut t = FH_TABLE.lock();
    if let Some(e) = t.iter_mut().find(|e| e.0 == id) { e.1 = path.to_vec(); return; }
    if t.len() >= 4096 { t.remove(0); }
    t.push((id, path.to_vec()));
}

/// (dev, ino) of `path` and the path recorded for the handle; `None` when the
/// filesystem cannot be watched (EOPNOTSUPP).
pub fn fh_ident(path: &[u8]) -> Option<(u64, u64)> {
    if !inotify::may_watch(path) && tmpfs_path(path).is_none() && !is_f2fs_path(path) { return None; }
    let p = tmpfs_path(path).unwrap_or(path);
    inotify_ident(p).map(|i| i.id)
}

/// Does `path` lie on an f2fs mount (handles decode by inode number there)?
fn is_f2fs_path(path: &[u8]) -> bool {
    if is_tmp_path(path) { return false; }
    let port = match find_mount_port(path) { Some(p) => p, None => return false };
    MOUNTS.lock().iter().any(|e| e.in_use && e.port == port && e.fstype == "f2fs")
}

/// f2fs-only VFS tag: inode number -> mount-relative path + i_generation.
const VFS_INO_PATH_F2FS: u64 = 0x7B;

/// Ask the f2fs mount holding `host` for inode `ino`: (mount prefix, relative
/// path, i_generation), `Err(true)` = the inode is gone (ESTALE), `Err(false)`
/// = no answer (other filesystem, or no live name is reachable).
fn f2fs_ino_path(host: &[u8], ino: u64) -> Result<(&'static str, Vec<u8>, u32), bool> {
    let port = find_mount_port(host).ok_or(false)?;
    let prefix = {
        let m = MOUNTS.lock();
        match m.iter().find(|e| e.in_use && e.port == port && e.fstype == "f2fs") { Some(e) => e.prefix, None => return Err(false) }
    };
    let mut buf = alloc::vec![0u8; 4096];
    let mut msg = Message::empty();
    msg.tag = VFS_INO_PATH_F2FS;
    msg.data[0..8].copy_from_slice(&ino.to_le_bytes());
    msg.data[8..16].copy_from_slice(&(buf.as_mut_ptr() as u64).to_le_bytes());
    msg.data[16..24].copy_from_slice(&(buf.len() as u64).to_le_bytes());
    let r = call_port(port, msg);
    let v = i64::from_le_bytes(r.data[0..8].try_into().unwrap_or([0xff; 8]));
    if v == -116 { return Err(true); }
    if v < 0 { return Err(false); }
    let n = (v as u64 & 0xffff_ffff) as usize;
    let g = ((v as u64) >> 32) as u32;
    buf.truncate(n.min(4096));
    Ok((prefix, buf, g))
}

/// i_generation to put in a handle for `host` (0 where the filesystem has none).
pub fn fh_gen(host: &[u8], id: (u64, u64)) -> u32 {
    if is_tmp_path(host) { return 0; }
    f2fs_ino_path(host, id.1).map(|r| r.2).unwrap_or(0)
}

/// Path of the file a handle (`ino`, `gen`) names on the filesystem `mp` lies
/// on: asked of the owning filesystem by inode number (f2fs: rebuilt from
/// i_pino/i_name; tmpfs: the pool slot owning the inode), so renames follow
/// and handles survive remounts and reboots. Anything else (and an f2fs hard
/// link whose recorded name is gone) falls back to the issued-handle table.
/// `None` = ESTALE.
pub fn fh_lookup(mp: &[u8], dev: Option<u64>, ino: u64, gen: u32) -> Option<alloc::vec::Vec<u8>> {
    if is_tmp_path(mp) {
        let found = {
            let tmp = TMP_FILES.lock();
            tmp.iter().find(|e| e.in_use() && !e.path().is_empty() && e.ino == ino && !e.is_link && dev.map_or(true, |d| tmpfs_dev_of(e.path()) == Some(d))).map(|e| e.path().to_vec())
        };
        if let Some(p) = found {
            if fh_ident(&p).map_or(false, |i| i.1 == ino && dev.map_or(true, |d| d == i.0)) { return Some(p); }
        }
    } else {
        match f2fs_ino_path(mp, ino) {
            Ok((prefix, rel, g)) => {
                if gen != 0 && g != gen { return None; }
                let mut p = prefix.as_bytes().to_vec();
                while p.len() > 1 && p.last() == Some(&b'/') { p.pop(); }
                if p == b"/" { p.clear(); }
                p.extend_from_slice(&rel);
                if p.is_empty() { p.push(b'/'); }
                return Some(p);
            }
            Err(true) => return None,
            Err(false) => {}
        }
    }
    let (id, p) = FH_TABLE.lock().iter().find(|e| e.0 .1 == ino && dev.map_or(true, |d| d == e.0 .0)).map(|e| (e.0, e.1.clone()))?;
    if fh_ident(&p) == Some(id) { Some(p) } else { None }
}

/// st_dev of the object `path` names (no watchability gate).
pub fn fh_dev(path: &[u8]) -> Option<u64> {
    inotify_ident(tmpfs_path(path).unwrap_or(path)).map(|i| i.id.0)
}

pub fn fh_known(id: (u64, u64)) -> bool { FH_TABLE.lock().iter().any(|e| e.0 == id) }

/// One `struct fanotify_event_info_fid` record: header, fsid, `struct
/// file_handle`. The handle is the LeandrOS object identity that
/// name_to_handle_at also returns (reopened through open_by_handle_at): handle_type
/// FILEID_LEANDROS (0x4c), 12 bytes = st_ino (u64) + generation (u32, 0);
/// fsid = the st_dev halves. Pushes nothing when `id` is unknown.
fn fan_fid_record(out: &mut Vec<u8>, ty: u8, id: (u64, u64), name: Option<&[u8]>) {
    let nlen = name.map_or(0, |n| n.len() + 1);
    let len = (4 + 8 + 8 + 12 + nlen + 3) & !3;
    out.push(ty);
    out.push(0);
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&(id.0 as u32).to_le_bytes());
    out.extend_from_slice(&((id.0 >> 32) as u32).to_le_bytes());
    out.extend_from_slice(&12u32.to_le_bytes());
    out.extend_from_slice(&0x4ci32.to_le_bytes());
    out.extend_from_slice(&id.1.to_le_bytes());
    let hp = FH_TABLE.lock().iter().find(|e| e.0 == id).map(|e| e.1.clone());
    let g = hp.map_or(0, |p| fh_gen(&p, id));
    out.extend_from_slice(&g.to_le_bytes());
    if let Some(n) = name { out.extend_from_slice(n); out.push(0); }
    while (out.len()) % 4 != 0 { out.push(0); }
}

/// Info records of a FID group event on `path`: DFID_NAME (2) / DFID (3) of the
/// directory, then FID (1) of the object. `eino` is the watched inode of a
/// self event (or the watched directory of a named one).
fn fan_fid_records(out: &mut Vec<u8>, gfid: u32, path: &[u8], eino: (u64, u64), named: bool, isdir: bool) {
    let (dir, base) = match path.iter().rposition(|&b| b == b'/') {
        Some(0) => (&path[..1], &path[1..]),
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (&path[..0], path),
    };
    // Mount / filesystem marks carry no watched inode: resolve it from the path.
    let eino = if eino == (0, 0) && !named { inotify_ident(path).map_or((0, 0), |i| i.id) } else { eino };
    let eino = if eino == (0, 0) && named { inotify_ident(dir).map_or((0, 0), |i| i.id) } else { eino };
    let obj: Option<(u64, u64)> = if named { inotify_ident(path).map(|i| i.id) } else if eino == (0, 0) { None } else { Some(eino) };
    if let Some(o) = obj { fh_remember(o, path); }
    if gfid & inotify::FAN_REPORT_DIR_FID != 0 {
        let (did, nm): (Option<(u64, u64)>, &[u8]) = if named { (if eino == (0, 0) { None } else { Some(eino) }, base) }
            else if isdir { (Some(eino), b".") } else { (inotify_ident(dir).map(|i| i.id), base) };
        if let Some(d) = did {
            fh_remember(d, if named || !isdir { dir } else { path });
            if gfid & inotify::FAN_REPORT_NAME != 0 { fan_fid_record(out, 2, d, Some(nm)); } else { fan_fid_record(out, 3, d, None); }
        }
    }
    if gfid & inotify::FAN_REPORT_FID != 0 { if let Some(o) = obj { fan_fid_record(out, 1, o, None); } }
}

/// FAN_RENAME records: OLD_DFID_NAME (10), NEW_DFID_NAME (12) and, with
/// FAN_REPORT_TARGET_FID, the FID (1) of the renamed object. `blob` is `old NUL new`.
fn fan_rename_records(out: &mut Vec<u8>, gfid: u32, blob: &[u8]) {
    let z = blob.iter().position(|&b| b == 0).unwrap_or(blob.len());
    let (old, new) = (&blob[..z], if z < blob.len() { &blob[z + 1..] } else { &blob[..0] });
    for (ty, p) in [(10u8, old), (12u8, new)] {
        let (dir, base) = match p.iter().rposition(|&b| b == b'/') {
            Some(0) => (&p[..1], &p[1..]),
            Some(i) => (&p[..i], &p[i + 1..]),
            None => (&p[..0], p),
        };
        if let Some(d) = inotify_ident(dir).map(|i| i.id) { fh_remember(d, dir); fan_fid_record(out, ty, d, Some(base)); }
    }
    if gfid & inotify::FAN_REPORT_TARGET_FID != 0 {
        if let Some(o) = inotify_ident(new).map(|i| i.id) { fh_remember(o, new); fan_fid_record(out, 1, o, None); }
    }
}

/// read() on a fanotify group: `struct fanotify_event_metadata` records, each
/// with an fd opened on the object in the reader's table.
fn handle_fan_read(pid: u32, inst: u32, buf: *mut u8, count: usize) -> Message {
    const MD: usize = 24;
    if count < MD { return err_reply(-22); }
    let evs = match inotify::fan_read(inst, if inotify::fan_fid(inst) != 0 { (count / 512).max(1) } else { count / MD }) { Ok(v) => v, Err(e) => return err_reply(e) };
    let evflags = inotify::fan_evflags(inst);
    let gfid = inotify::fan_fid(inst);
    let mut out: Vec<u8> = Vec::with_capacity(evs.len() * MD);
    let mut fds: Vec<i32> = Vec::new();
    for (mask, path, epid, permid, eino, named) in evs {
        let mut efd: i32 = -1;
        let mut info: Vec<u8> = Vec::new();
        if gfid != 0 && mask & inotify::FAN_RENAME != 0 {
            fan_rename_records(&mut info, gfid, &path);
        } else if gfid != 0 {
            fan_fid_records(&mut info, gfid, &path, eino, named, mask & inotify::IN_ISDIR != 0);
        } else if !path.is_empty() {
            let mut z = path.clone();
            z.push(0);
            let r = reply_val(&handle_open(pid, z.as_ptr() as usize, evflags, 0));
            if r >= 0 { efd = r as i32; fds.push(efd); }
        }
        if permid != 0 { inotify::fan_bind(inst, permid, efd); }
        out.extend_from_slice(&((MD + info.len()) as u32).to_le_bytes());
        out.push(3); // FANOTIFY_METADATA_VERSION
        out.push(0);
        out.extend_from_slice(&(MD as u16).to_le_bytes());
        out.extend_from_slice(&(mask as u64).to_le_bytes());
        out.extend_from_slice(&efd.to_le_bytes());
        out.extend_from_slice(&(epid as i32).to_le_bytes());
        out.extend_from_slice(&info);
    }
    if unsafe { sched::uaccess::copy_raw(buf, out.as_ptr(), out.len()) } != 0 {
        for f in fds { let _ = handle_close(pid, f as usize); }
        return err_reply(-14);
    }
    val_reply(out.len() as u64)
}

/// inotify_rm_watch(fd, wd).
fn handle_inotify_rm(pid: u32, fd: usize, wd: i32) -> Message {
    let inst = match inotify_inst_of(pid, fd) { Ok(i) => i, Err(e) => return err_reply(e) };
    if inst == 0 { return ok_reply(); }
    match inotify::rm(inst, wd) { Ok(()) => ok_reply(), Err(e) => err_reply(e) }
}

fn handle_timerfd_create(pid: u32, flags: u32, clockid: u32) -> Message {
    // Preserve TFD_NONBLOCK/TFD_CLOEXEC (== O_NONBLOCK/O_CLOEXEC). The polling
    // crate (calloop's poller) reads its timeout timerfd expecting EAGAIN before
    // it fires; without the recorded O_NONBLOCK the kernel read yield-spins,
    // pinning a single-threaded compositor's event loop.
    let stored = flags & (O_NONBLOCK_FL | O_CLOEXEC);
    let mut pool = TIMERFD_POOL.lock();
    let slot = loop {
        match pool.iter().position(|e| e.is_free()) {
            Some(s) => break s,
            None => {
                drop(pool);
                if !timerfd_grow() { report_pool_full("timerfd", MAX_TIMERFDS); return err_reply(-23); }
                pool = TIMERFD_POOL.lock();
            }
        }
    };
    pool[slot] = TimerFdEntry::free();
    pool[slot].deadline_ns = 1;
    pool[slot].clockid = clockid;
    drop(pool);
    TIMERFD_REFS.lock()[slot] = 1;
    let mut tbls = TblSlot::new();
    let tbl = match get_or_create(pid, &mut tbls) {
        Some(t) => t, None => { TIMERFD_POOL.lock()[slot] = TimerFdEntry::free(); TIMERFD_REFS.lock()[slot] = 0; return err_reply(-23); }
    };
    let fd = match tbl.alloc_fd() {
        Some(f) => f, None => { TIMERFD_POOL.lock()[slot] = TimerFdEntry::free(); TIMERFD_REFS.lock()[slot] = 0; return err_reply(-24); }
    };
    tbl.fds.set(fd, FdEntry { kind: VnodeKind::TimerFd { slot }, flags: stored, in_use: true, ofd: ofd::alloc() });
    val_reply(fd as u64)
}

/// Arm (or disarm, `value_ns == 0`) a timerfd.
///
/// `value_ns` is `it_value`: with `absolute` it is an instant on the fd's own
/// clock (CLOCK_MONOTONIC as `clock_gettime` reports it, or CLOCK_REALTIME,
/// translated through `sched::realtime_to_monotonic_ns`); otherwise it is an
/// interval added to `sched::monotonic_ns()` NOW — not to the tick counter,
/// which ignores the fraction of the tick already elapsed and fired up to
/// 10 ms early by clock_gettime's reckoning. Linux converts relative to
/// absolute at settime for the same reason. The stored deadline is always a
/// monotonic instant, compared exactly (nanoseconds) by the poll-deadline
/// tick, so a timerfd is never observed expired before the instant it was
/// armed for; the tick only bounds how late (≤ 10 ms) the wake can be.
///
/// A deadline already in the past is not an error and not a disarm: the
/// timer fires immediately (one expiration for a one-shot; for a periodic
/// timer the missed periods are counted as overruns by
/// `timerfd_poll_expirations`, as Linux's `hrtimer_forward_now` does).
///
/// `flags` are timerfd_settime's: TFD_TIMER_ABSTIME (1) and
/// TFD_TIMER_CANCEL_ON_SET (2). As in Linux's `timerfd_setup_cancel`, the
/// cancel request takes effect only together with TFD_TIMER_ABSTIME on a
/// CLOCK_REALTIME fd — whether or not `value_ns` arms the timer (systemd's
/// clock-change watch is a disarmed/far-future CANCEL_ON_SET timerfd) — and
/// every settime clears a pending cancellation.
fn handle_timerfd_settime(pid: u32, fd: usize, value_ns: u64, interval_ns: u64, flags: u32) -> Message {
    let absolute = flags & TFD_TIMER_ABSTIME != 0;
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }
    let slot = match tbl.fds[fd].kind { VnodeKind::TimerFd { slot } => slot, _ => return err_reply(-22) };
    drop(tbls);
    let mut pool = TIMERFD_POOL.lock();
    let e = &mut pool[slot];
    let realtime_abs = absolute && e.clockid == TFD_CLOCK_REALTIME;
    e.might_cancel = realtime_abs && flags & TFD_TIMER_CANCEL_ON_SET != 0;
    e.canceled = false;
    e.abs_realtime = false;
    if value_ns == 0 { e.armed = false; e.expirations = 0; }
    else {
        e.abs_realtime = realtime_abs;
        e.armed = true;
        let deadline = if !absolute {
            sched::monotonic_ns().saturating_add(value_ns)
        } else if e.clockid == TFD_CLOCK_REALTIME {
            sched::realtime_to_monotonic_ns(value_ns)
        } else {
            value_ns
        };
        // `.max(1)`: deadline_ns == 0 is the free-slot sentinel (`is_free`),
        // and an already-due deadline would otherwise make a live fd's slot
        // look reclaimable once its expiry has been read.
        e.deadline_ns = deadline.max(1);
        e.interval_ns = interval_ns;
        e.expirations = 0;
    }
    let armed_deadline = if e.armed { Some(e.deadline_ns) } else { None };
    drop(pool);
    // Publish the expiry so the poll-deadline tick wakes an epoll_wait(-1)
    // waiter parked on this timerfd (K2). If it is already due, wake now.
    if let Some(d) = armed_deadline {
        sched::register_poll_deadline(d);
        if sched::monotonic_ns() >= d {
            sched::wake_poll_tagged(sched::poll_tag(sched::poll_class::TIMERFD, slot as u32));
        }
    }
    ok_reply()
}

/// Earliest absolute `sched::monotonic_ns()` instant at which any armed timerfd
/// next expires (u64::MAX = none). Folded into the K2 poll-deadline tick so a periodic timerfd nobody
/// re-arms still fires, and an epoll_wait(-1) waiter on a timerfd wakes on
/// time with no per-tick wake_poll. try_lock only (tick contract): a contended
/// scan just defers this tick's decision ≤10 ms.
pub fn earliest_timerfd_deadline() -> u64 {
    match TIMERFD_POOL.try_lock() {
        Some(pool) => {
            let mut m = u64::MAX;
            for e in pool.iter() {
                if e.armed && e.deadline_ns < m { m = e.deadline_ns; }
            }
            m
        }
        None => u64::MAX,
    }
}

/// Fold every armed timerfd whose deadline has passed, exactly as
/// `timerfd_poll_expirations` would when its owner polls it — one-shots are
/// disarmed, periodics are re-armed onto their next future deadline, and the
/// missed-expiration count is accumulated (never cleared, so a later poll/read
/// still observes the expiry). Returns the OR of the poll tags of every
/// timerfd that expired on this pass (0 = none), so the caller can wake or
/// defer exactly those interests. Called from the 100 Hz `poll_deadline_tick`.
///
/// Why this exists: an armed one-shot timerfd whose owner does not promptly
/// consume it (e.g. a compositor briefly descheduled) used to sit `armed` with
/// an already-past deadline forever. `earliest_timerfd_deadline` then reported
/// it every tick, so the tick fired `timerfd_due` and mass-woke every poll
/// waiter 100×/s — a self-reinforcing wake STORM that saturated the CPU and
/// starved the very process that had to run to consume/re-arm the timerfd (it
/// never could), so `ext-idle-notify`'s `Idled` never fired on an idle desktop.
/// Folding here retires each expiry once, turning the perpetual `timerfd_due`
/// into a single edge per expiration. IRQ-context safe: try_lock only
/// (82d0cc3 invariant — no user memory touched, no cross-server call).
pub fn fold_expired_timerfds(now: u64) -> u64 {
    fold_expired_timerfds_keyed(now, &mut sched::WakeKeys::new())
}

/// `fold_expired_timerfds` that also collects the expired timerfds' exact keys
/// (`timerfd_key`) into `keys` (which marks itself overflowed past its
/// capacity), so the deadline tick can wake only their pollers.
pub fn fold_expired_timerfds_keyed(now: u64, keys: &mut sched::WakeKeys) -> u64 {
    let mut pool = match TIMERFD_POOL.try_lock() { Some(p) => p, None => return 0 };
    let mut tags = 0u64;
    for (slot, e) in pool.iter_mut().enumerate() {
        if e.armed && now >= e.deadline_ns {
            let elapsed = now - e.deadline_ns;
            if e.interval_ns > 0 {
                let extra = elapsed / e.interval_ns + 1;
                e.expirations += extra;
                e.deadline_ns += extra * e.interval_ns; // stays armed, next future instant
            } else {
                e.expirations += 1;
                e.armed = false; // one-shot: retire (expiry preserved in `expirations`)
            }
            tags |= sched::poll_tag(sched::poll_class::TIMERFD, slot as u32);
            keys.push(timerfd_key(slot));
        }
    }
    tags
}

/// timerfd_settime flag: `it_value` is an absolute instant on the fd's clock.
const TFD_TIMER_ABSTIME: u32 = 1;
/// timerfd_settime flag: cancel on a discontinuous CLOCK_REALTIME change.
const TFD_TIMER_CANCEL_ON_SET: u32 = 2;
/// The `clockid` a CLOCK_REALTIME timerfd records.
const TFD_CLOCK_REALTIME: u32 = 0;

/// The wall clock was stepped (clock_settime / settimeofday): the realtime
/// offset went from `old_off` to `new_off` (CLOCK_REALTIME = monotonic + off).
///
/// Linux keeps an absolute CLOCK_REALTIME timer on the realtime hrtimer base,
/// so its expiry stays the same WALL instant across a step; here the deadline
/// is stored on the monotonic clock and is moved by the difference instead.
/// Every such timer armed with TFD_TIMER_CANCEL_ON_SET is also cancelled
/// (`timerfd_clock_was_set`): it polls readable and its next read answers
/// ECANCELED. Both kinds get their exact poll key woken — the cancelled one
/// because it became readable, a moved one because it may now be due (the
/// deadline tick is told about the new instant too).
///
/// Called by the kernel after the offset is stored, with no lock held.
pub fn timerfd_clock_was_set(old_off: i64, new_off: i64) {
    if old_off == new_off { return; }
    let delta = new_off.wrapping_sub(old_off);
    let now = sched::monotonic_ns();
    // (slot, moved deadline, wake now)
    let mut todo: alloc::vec::Vec<(usize, Option<u64>, bool)> = alloc::vec::Vec::new();
    {
        let mut pool = TIMERFD_POOL.lock();
        for (slot, e) in pool.iter_mut().enumerate() {
            if e.is_free() { continue; }
            let mut moved = None;
            let mut wake = false;
            if e.armed && e.abs_realtime {
                // wall = mono_old + old_off = mono_new + new_off
                let d = (e.deadline_ns as i64).saturating_sub(delta).max(1) as u64;
                e.deadline_ns = d;
                moved = Some(d);
                wake = d <= now;
            }
            if e.might_cancel {
                e.canceled = true;
                e.cancels = e.cancels.wrapping_add(1);
                wake = true;
            }
            if (moved.is_some() || wake) && todo.try_reserve(1).is_ok() {
                todo.push((slot, moved, wake));
            }
        }
    }
    for (slot, moved, wake) in todo {
        if let Some(d) = moved { sched::register_poll_deadline(d); }
        if wake { sched::wake_poll_key(timerfd_key(slot)); }
    }
}

fn handle_timerfd_gettime(pid: u32, fd: usize, out_ptr: usize) -> Message {
    if out_ptr == 0 { return err_reply(-14); }
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }
    let slot = match tbl.fds[fd].kind { VnodeKind::TimerFd { slot } => slot, _ => return err_reply(-22) };
    drop(tbls);
    let pool = TIMERFD_POOL.lock();
    let e = &pool[slot];
    let now = sched::monotonic_ns();
    let remaining_ns = if e.armed && e.deadline_ns > now { e.deadline_ns - now } else { 0 };
    let interval_ns = e.interval_ns;
    drop(pool);
    let its: [i64; 4] = [(interval_ns / 1_000_000_000) as i64, (interval_ns % 1_000_000_000) as i64,
                         (remaining_ns / 1_000_000_000) as i64, (remaining_ns % 1_000_000_000) as i64];
    if !sched::uaccess::write_user(out_ptr, its) { return err_reply(-14); }
    ok_reply()
}

const FIONREAD: usize = 0x541B;
const FBIOGET_VSCREENINFO: usize = 0x4600;

fn handle_ioctl(pid: u32, fd: usize, cmd: usize, arg: usize) -> Message {
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }


    // Block-device ioctls (BLKGETSIZE64, BLKSSZGET, BLKPG, LOOP_*). The FD
    // table lock is dropped first: LOOP_SET_FD re-enters this server to
    // resolve and open the backing file, and every arm touches user memory
    // through `arg`, which must be able to demand-page.
    if let VnodeKind::NsFd { kind, id } = tbl.fds[fd].kind {
        drop(tbls);
        return make_reply(ns_ioctl(pid, kind, id, cmd, arg) as i64);
    }
    if let VnodeKind::BlockDev { dev, .. } = tbl.fds[fd].kind {
        let ofd_id = tbl.fds[fd].ofd;
        drop(tbls);
        return make_reply(block::ioctl(pid, ofd_id, dev, cmd, arg) as i64);
    }

    if let VnodeKind::DynamicDevice { port, dev_id, open_id } = &tbl.fds[fd].kind {
        let port = *port;
        let dev_id = *dev_id;
        let open_id = *open_id;
        drop(tbls);

        let mut proxy_msg = Message::empty();
        proxy_msg.tag = VFS_IOCTL;
        proxy_msg.data[0..8].copy_from_slice(&(dev_id as u64).to_le_bytes());
        proxy_msg.data[8..16].copy_from_slice(&(cmd as u64).to_le_bytes());
        proxy_msg.data[16..24].copy_from_slice(&(arg as u64).to_le_bytes());
        proxy_msg.data[24..32].copy_from_slice(&(pid as u64).to_le_bytes());
        // Slot 4: which *open* of the device this ioctl is on. Device servers
        // key per-open state off it (virtgpu's 3D context); servers that don't
        // care simply never read it.
        proxy_msg.data[32..40].copy_from_slice(&(open_id as u64).to_le_bytes());

        let reply = call_port(port, proxy_msg);
        return reply;
    }

    if cmd == FBIOGET_VSCREENINFO {
        if let VnodeKind::DevFb { .. } = &tbl.fds[fd].kind {
            let width  = FB_WIDTH.load(atomic::Ordering::SeqCst);
            let height = FB_HEIGHT.load(atomic::Ordering::SeqCst);
            let pitch  = FB_PITCH.load(atomic::Ordering::SeqCst);
            drop(tbls);

            let mut info = [0u32; 8];
            info[0] = width;
            info[1] = height;
            info[2] = width;
            info[3] = height;
            info[4] = 0;
            info[5] = 0;
            info[6] = 32;
            info[7] = pitch;

            if !sched::uaccess::write_user(arg, info) { return err_reply(-14); } // EFAULT
            return ok_reply();
        }
    }

    if cmd != FIONREAD { return err_reply(-25); }
    if arg == 0 { return err_reply(-14); }

    let bytes_avail: i32 = match &tbl.fds[fd].kind {
        // Either end, as on Linux: the write end's count is how far ahead a
        // producer is of its reader (DOOM paces its PCM pipe to pw-cat by it).
        VnodeKind::Pipe { ring, .. } => { let r = *ring; drop(tbls); PIPE_RINGS.lock()[r].count as i32 }
        VnodeKind::Pty { pair, is_master } => {
            let (p, m) = (*pair as usize, *is_master); drop(tbls);
            tty_server::pty::readable_bytes(p, m) as i32
        }
        VnodeKind::RamFile { data, is_dir, .. } => {
            if *is_dir { return err_reply(-25); } // ENOTTY — no readable byte stream
            let n = (data.len().saturating_sub(desc_pos(&tbl.fds[fd]))) as i32;
            drop(tbls); // the count is stored below, with no lock held
            n
        }
        VnodeKind::TmpFile { idx, .. } => { let i = *idx; let c = tmp_pos_get(&tbl.fds[fd].kind); drop(tbls); TMP_FILES.lock()[i].len.saturating_sub(c) as i32 }
        VnodeKind::Inotify { inst } => { let i = *inst; drop(tbls); inotify::nbytes(i) as i32 }
        VnodeKind::EventFd { slot } => { let s = *slot; drop(tbls); if EVENTFD_COUNTERS.lock()[s] > 0 { 8 } else { 0 } }
        VnodeKind::TimerFd { slot } => { let s = *slot; drop(tbls); if timerfd_poll_expirations(s) > 0 || TIMERFD_POOL.lock()[s].canceled { 8 } else { 0 } }
        _ => return err_reply(-25),
    };
    // Every arm has released the fd table: the store is a fault-tolerant
    // copy with nothing held, so a bad pointer is EFAULT, not a kill.
    if !sched::uaccess::write_user(arg, bytes_avail) { return err_reply(-14); }
    val_reply(0)
}

/// VFS_POLL(fd) → revents bitmask reflecting the vnode's *actual* current
/// state — never a guess.  Callers (poll/select/epoll in `kernel/src/syscall.rs`)
/// AND this against their requested-events mask, except for POLLERR/POLLHUP
/// which real `poll(2)`/`epoll_wait(2)` report unconditionally.
///
/// DevStdio and DynamicDevice fds beyond the kernel's own fd-0 fast path
/// (evdev/serial, handled directly in `kernel/src/syscall.rs` before this is
/// ever reached) have no readiness source wired through this crate yet — they
/// conservatively report not-ready rather than risk a false POLLIN/POLLOUT.
/// `want_ofd` != 0: the caller (an epoll registration) only wants `fd` if it
/// still names that open file description; anything else answers ESTALE so
/// it can look for the description elsewhere (see `ofd_holder`).
fn handle_poll(pid: u32, fd: usize, want_ofd: u32) -> Message {
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }
    if want_ofd != 0 && tbl.fds[fd].ofd != want_ofd { return err_reply(-116); } // ESTALE

    // Returns (revents, seq). `seq` is a monotonic per-object event counter the
    // epoll layer uses to emulate edge-triggered delivery (see PipeRing::seq /
    // EVENTFD_SEQ). fd kinds with no re-arming edge source report seq 0, which
    // makes an EPOLLET interest fire exactly once for their (constant)
    // readiness — correct edge behaviour for an always-ready fd.
    let kind = tbl.fds[fd].kind;
    drop(tbls);
    let (revents, seq, tag, dev_key) = poll_vnode_k(&kind, pid);
    let mut m = poll_reply(revents, seq, tag);
    // Pipes and eventfds also carry their exact key (see sched::PollSet): their
    // producers wake with `sched::wake_poll_key` of the same key. So do device
    // nodes whose server answered with one (evdev: `evdev_poll_key`).
    let t = fd_poll_tag(&kind, tag, dev_key);
    if t.n > 0 {
        sched::poll_reply_put_keys(&mut m.data, &t.keys[..t.n as usize]);
    } else if t.foreign {
        m.data[33] = sched::POLL_REPLY_FOREIGN;
    }
    m
}

/// `VFS_POLL` without the message (lane epollabi): the kernel's poll/epoll
/// probe of an ordinary descriptor in one fd-table pass. The probe used to
/// look the descriptor up three times — `resolve_fd`, `fd_is_console_stdio`,
/// then `handle_poll` behind `handle`'s own `tgid_of` — for a one-fd
/// zero-timeout poll that is most of its cost (Firefox's VA-API frame pool
/// polls an eventfd per surface per frame, ~18k calls/s). `None` for what
/// the kernel routes itself or reports specially: a closed number, the
/// console (an untracked fd 0-2, a `DevStdio` or `DevVt` proxy), a socket or
/// epoll alias, a description other than `want_ofd` (when non-zero).
/// Otherwise (masked-by-nothing revents, edge seq, tag) exactly as
/// `handle_poll`'s reply reads.
pub fn poll_fd_direct(pid: u32, fd: usize, want_ofd: u32) -> Option<(u32, u64, sched::PollTag)> {
    if fd >= FD_NUMBER_CEIL { return None; }
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    let tbl = find_tbl(pid, &mut tbls)?;
    if !tbl.fds.used(fd) { return None; }
    if want_ofd != 0 && tbl.fds[fd].ofd != want_ofd { return None; }
    let kind = tbl.fds[fd].kind;
    drop(tbls);
    if handle_of(&kind).is_some() || matches!(kind, VnodeKind::DevStdio { .. } | VnodeKind::DevVt { .. }) {
        return None;
    }
    let (revents, seq, tag, dev_key) = poll_vnode_k(&kind, pid);
    Some((revents, seq, fd_poll_tag(&kind, tag, dev_key)))
}

/// Vnodes whose readiness no producer ever publishes with a keyed wake
/// (`sched::wake_poll_key` is used only for pipes and eventfds here, the
/// deadline tick's timerfd expiry wake, and connected AF_UNIX data edges in
/// the net server): their tag bits need no key
/// to be refined (`sched::PollTag::foreign`). Listed explicitly — a kind added
/// later stays unrefined (the safe side) until someone checks its producers.
fn poll_vnode_foreign(kind: &VnodeKind) -> bool {
    matches!(kind,
        VnodeKind::Pty { .. }
        | VnodeKind::Inotify { .. } | VnodeKind::DevVt { .. }
        | VnodeKind::DynamicDevice { .. }
        // The static kinds (`static_poll_tag`): no producer at all.
        | VnodeKind::RamFile { .. } | VnodeKind::TmpFile { .. } | VnodeKind::MountedFile { .. }
        | VnodeKind::DevNull | VnodeKind::DevZero | VnodeKind::DevFull | VnodeKind::DevUrandom | VnodeKind::DevFb { .. }
        | VnodeKind::BlockDev { .. } | VnodeKind::SysBlock { .. })
}

/// The exact key of a vnode whose producers wake with `sched::wake_poll_key`
/// (`pipe_key` / `eventfd_key`) or the tick's keyed timerfd expiry
/// (`timerfd_key`), `None` for everything else.
fn poll_vnode_key(kind: &VnodeKind) -> Option<u64> {
    match kind {
        VnodeKind::Pipe { ring, is_write } => Some(pipe_end_key(*ring, *is_write)),
        VnodeKind::EventFd { slot } => Some(eventfd_key(*slot)),
        VnodeKind::Mq { slot } => Some(mqueue::key(*slot)),
        VnodeKind::TimerFd { slot } => Some(timerfd_key(*slot)),
        // Exit / reap wake it keyed (sched exit paths, pidfd_reaped_wake).
        VnodeKind::PidFd { pid } => Some(sched::poll_key(sched::poll_class::PIDFD, *pid)),
        // Signal posts wake the reader's group keyed (sched::signal_wake_key).
        VnodeKind::SignalFd { .. } => Some(sched::signal_wake_key(sched::current_tgid())),
        VnodeKind::Inotify { inst } => Some(inotify::key(*inst)),
        _ => None,
    }
}

/// Exact poll key of pipe ring `ring`'s READ end: what data and EOF wake and
/// what a read-end probe reports. The write end has its own key
/// (`pipe_wkey`): until lane panelpipe both ends shared one, so every drain's
/// write-space wake also woke the (re-parked, still empty) reader.
fn pipe_rkey(ring: usize) -> u64 { sched::poll_key(sched::poll_class::PIPE, ring as u32) }
/// Exact poll key of pipe ring `ring`'s WRITE end: space and reader-gone.
fn pipe_wkey(ring: usize) -> u64 { sched::poll_key(sched::poll_class::PIPE_WR, ring as u32) }
/// The key of one end (`is_write` selects the write end).
fn pipe_end_key(ring: usize, is_write: bool) -> u64 {
    if is_write { pipe_wkey(ring) } else { pipe_rkey(ring) }
}
/// Exact poll key of eventfd `slot` (see `pipe_key`).
fn eventfd_key(slot: usize) -> u64 { sched::poll_key(sched::poll_class::EVENTFD, slot as u32) }
/// Exact poll key of timerfd `slot`: what the deadline tick's keyed expiry
/// wake (`fold_expired_timerfds_keyed`) and `handle_poll` report.
fn timerfd_key(slot: usize) -> u64 { sched::poll_key(sched::poll_class::TIMERFD, slot as u32) }

/// The readiness of one vnode: (revents, edge seq, wake tag) — the probe of a
/// description that no fd names (`poll_transfer`), which reports bit-only.
fn poll_vnode(kind: &VnodeKind, pid: u32) -> (u32, u64, u64) {
    let (ev, seq, tag, _) = poll_vnode_k(kind, pid);
    (ev, seq, tag)
}

/// `poll_vnode` plus the exact key a device server (`DynamicDevice`) answered
/// with (data[33] == `POLL_REPLY_KEYS`, one key): `handle_poll`, once the fd
/// is resolved. Called with no VFS lock held.
fn poll_vnode_k(kind: &VnodeKind, pid: u32) -> (u32, u64, u64, Option<u64>) {
    let mut dev_key: Option<u64> = None;
    let r: (u32, u64, u64) = match kind {
        VnodeKind::Pipe { ring, is_write: false } => {
            let r = *ring;
            let ring = &PIPE_RINGS.lock()[r];
            let mut ev = 0;
            if ring.count > 0 { ev |= POLLIN; }
            if ring.writers == 0 { ev |= POLLIN | POLLHUP; } // EOF: read() returns 0 without blocking
            (ev, ring.seq, sched::poll_key_bit(pipe_rkey(r)))
        }
        VnodeKind::Pipe { ring, is_write: true } => {
            let r = *ring;
            let ring = &PIPE_RINGS.lock()[r];
            let ev = if ring.readers == 0 {
                POLLERR // reader gone: next write() gets EPIPE
            } else if ring.free() >= PIPE_BUF {
                POLLOUT // a PIPE_BUF-sized write will not block (Linux rule)
            } else {
                0
            };
            (ev, ring.seq, sched::poll_key_bit(pipe_wkey(r)))
        }
        // The pty pool owns the only honest answer here: `poll_mask` mirrors
        // `master_read`/`slave_read` case for case, including canonical mode's
        // "a complete line is queued" rule, so an epoll consumer is never woken
        // for a readability the following read denies.
        VnodeKind::Pty { pair, is_master } => {
            let (p, m) = (*pair as usize, *is_master);
            (tty_server::pty::poll_mask(p, m), tty_server::pty::seq(p),
             sched::poll_tag(sched::poll_class::PTY, p as u32))
        }
        VnodeKind::RamFile { .. } | VnodeKind::TmpFile { .. } | VnodeKind::MountedFile { .. }
        | VnodeKind::DevNull | VnodeKind::DevZero | VnodeKind::DevFull | VnodeKind::DevUrandom | VnodeKind::DevFb { .. }
        | VnodeKind::BlockDev { .. } | VnodeKind::SysBlock { .. } => {
            // Always ready and never changing: a poller parks on these only
            // under EPOLLET after the first edge, or for bits they never
            // report. The broadcast tag made such a poller (and its whole
            // interest set) wake on every wake in the system; the static tag
            // has no producer because there is nothing to announce.
            (POLLIN | POLLOUT, 0, static_poll_tag())
        }
        VnodeKind::Mq { slot } => {
            let s = *slot;
            let (r, w, seq) = mqueue::readiness(s);
            (if r { POLLIN } else { 0 } | if w { POLLOUT } else { 0 }, seq, sched::poll_tag(sched::poll_class::MQ, s as u32))
        }
        VnodeKind::EventFd { slot } => {
            let s = *slot;
            let mut ev = POLLOUT; // only EINVAL's on overflow, never actually blocks
            if EVENTFD_COUNTERS.lock()[s] > 0 { ev |= POLLIN; }
            (ev, EVENTFD_SEQ.lock()[s], sched::poll_tag(sched::poll_class::EVENTFD, s as u32))
        }
        VnodeKind::TimerFd { slot } => {
            let s = *slot;
            // Cumulative expiration count doubles as the edge seq: each new
            // expiration advances it; a read that resets expirations drops
            // revents to 0 so no spurious fire results.
            // A pending CANCEL_ON_SET cancellation is readable too (the
            // read answers ECANCELED); `cancels` moves the seq for ET.
            let exp = timerfd_poll_expirations(s);
            let (canceled, cancels) = { let p = TIMERFD_POOL.lock(); (p[s].canceled, p[s].cancels) };
            (if exp > 0 || canceled { POLLIN } else { 0 }, exp.wrapping_add(cancels << 40),
             sched::poll_tag(sched::poll_class::TIMERFD, s as u32))
        }
        VnodeKind::SignalFd { mask, ofd } => {
            let mask = signalfd_mask(*ofd, *mask);
            // POLLIN once a signal in the mask is pending for the caller. seq 0
            // (level); calloop's Signals source registers this level-triggered.
            let pending = (sched::pending_signals() | sched::shared_pending_signals()) & mask;
            // Every signal post wakes the target group's signal tag
            // (sched::deliver_signal*, `signal_wake_tag`), which is this tag,
            // from the one helper a blocked signalfd read parks on too
            // (`fd_wake_tag`). Reporting POLL_TAG_ALL here made every
            // signalfd holder (PipeWire, pipewire-pulse, the sound sink: SPA
            // loops watch SIGINT/SIGTERM) a broadcast-mask parker, woken by
            // every socket/pipe/eventfd wake in the system (lane mediabus).
            // The tag names the CALLER's thread group, whose pending set the
            // answer reads — not the fd-table owner `pid`, which differs when
            // epoll probes a registration through another holder of the
            // description (a forked child's copy).
            (if pending != 0 { POLLIN } else { 0 }, 0, signalfd_wake_tag())
        }
        VnodeKind::PidFd { pid } => {
            // Readable once the whole thread group is gone (Linux pidfd_poll:
            // EPOLLIN; plus EPOLLHUP once it has been reaped). The exit is a
            // one-time edge, so the seq is simply "exited" — an EPOLLET
            // interest (tokio's reaper registers this way) fires exactly once.
            let pid = *pid;
            let (exited, reaped) = sched::process_exit_state(pid);
            let ev = if reaped { POLLIN | POLLHUP } else if exited { POLLIN } else { 0 };
            (ev, exited as u64, sched::poll_tag(sched::poll_class::PIDFD, pid))
        }
        VnodeKind::NsFd { .. } | VnodeKind::BpfProg { .. } => (0, 0, sched::poll_tag(sched::poll_class::PIDFD, 0)),
        VnodeKind::Inotify { inst } => {
            // Never fires: accepted watches silently produce no events. Its
            // own tag, not the broadcast mask — with POLL_TAG_ALL every event
            // loop holding a config watch (every COSMIC client) parked on a
            // broadcast mask and was woken by every timerfd expiry and every
            // untagged wake in the system: ~180 of the idle desktop's ~800
            // wakeups a second.
            let (rdy, seq) = inotify::ready(*inst);
            (if rdy { POLLIN } else { 0 }, seq, inotify_wake_tag(*inst))
        }
        VnodeKind::DynamicDevice { port, dev_id, open_id } => {
            let port = *port;
            let dev_id = *dev_id;
            let open_id = *open_id;
            // Proxy readiness to the owning device server (DRM card0 events,
            // evdev eventN). Holds no VFS lock across the IPC round-trip, and the
            // epoll wait loop already drops EPOLL_INSTANCES before probing, so
            // this stays clear of the 82d0cc3 lock-under-fault hazard. A server
            // that does not implement VFS_POLL replies with a negative errno —
            // treat that as "not ready" (the pre-K4 behaviour).
            let mut proxy = Message::empty();
            proxy.tag = VFS_POLL;
            proxy.data[0..8].copy_from_slice(&(dev_id as u64).to_le_bytes());
            proxy.data[24..32].copy_from_slice(&(pid as u64).to_le_bytes());
            // Slot 4: which *open* this poll is on — the same cookie handle_ioctl
            // already forwards. Readiness must be answered from the queue the
            // matching read() will drain, or one reader's drain silently
            // un-readies the other.
            proxy.data[32..40].copy_from_slice(&(open_id as u64).to_le_bytes());
            let reply = call_port(port, proxy);
            let raw = i64::from_le_bytes(reply.data[0..8].try_into().unwrap_or([0u8; 8]));
            if raw < 0 {
                (0, 0, sched::POLL_TAG_ALL)
            } else {
                let seq = u64::from_le_bytes(reply.data[8..16].try_into().unwrap_or([0u8; 8]));
                // A device server that fills data[32]=1 gives us its object tag
                // (evdev does — see servers/evdev VFS_POLL). One that does not
                // (DRM today) leaves data[32]==0, and we fall back to broadcast
                // so its wake_poll is never missed.
                let tag = if reply.data[32] == 1 {
                    u64::from_le_bytes(reply.data[24..32].try_into().unwrap_or([0u8; 8]))
                } else {
                    sched::POLL_TAG_ALL
                };
                // Its exact key, when it sends one (evdev does). A device
                // whose producers wake keyed must NOT be reported foreign, so
                // an answer claiming keys that cannot be forwarded (not
                // exactly one) degrades to broadcast instead.
                if reply.data[32] == 1 && reply.data[33] == sched::POLL_REPLY_KEYS {
                    if reply.data[34] != 1 {
                        return (raw as u32, seq, sched::POLL_TAG_ALL, None);
                    }
                    dev_key = Some(u64::from_le_bytes(
                        reply.data[40..48].try_into().unwrap_or([0u8; 8])));
                }
                (raw as u32, seq, tag)
            }
        }
        VnodeKind::DevVt { vt, seen } => {
            let (vt, seen) = (*vt, *seen);
            if vt != 0 {
                // A console; the kernel answers its readiness from the serial
                // port and evdev before reaching here (`fd_is_console_stdio`).
                // POLLOUT only, never a POLLIN this crate cannot substantiate.
                (POLLOUT, 0, sched::POLL_TAG_ALL)
            } else {
                // /dev/tty0's VT-change edge. The seq IS the active VT number:
                // sys_epoll_wait compares seqs for inequality, and the readable
                // condition below is "active differs from what this open last
                // saw", so a switch away and back between two polls is one
                // non-event by both measures. See `fd_vt_number`.
                let cur = tty_server::vt::active();
                let ev = if cur as u8 != seen { POLLIN | POLLOUT } else { POLLOUT };
                (ev, cur as u64, sched::POLL_TAG_VT_SWITCH)
            }
        }
        // A socket alias is translated by the kernel before it polls; reaching
        // here would mean a caller skipped that, so report it never-ready.
        VnodeKind::DevStdio { .. } | VnodeKind::None | VnodeKind::SockAlias { .. } | VnodeKind::EpollFd { .. } => {
            (0, 0, sched::POLL_TAG_ALL)
        }
    };
    (r.0, r.1, r.2, dev_key)
}

fn handle_ftruncate(pid: u32, fd: usize, new_len: usize) -> Message {
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }
    match tbl.fds[fd].kind {
        VnodeKind::TmpFile { idx, .. } => {
            drop(tbls);
            let kcred = cred_of(pid);
            let mapped = vmo_id_of(idx);
            let mut tmp = TMP_FILES.lock();
            let mut vmos = TMP_VMOS.lock();
            let r = tmp_set_size(&mut tmp, &mut vmos[..], idx, new_len);
            if r != 0 { return make_reply(r); }
            tmp_kill_setid(&mut tmp, idx, &kcred);
            tmp_touch_mtime(&mut tmp[idx]);
            drop(vmos);
            drop(tmp);
            // Shrinking freed the object's pages from the new end on: take
            // them from every mapping too, so the next touch is SIGBUS (as on
            // Linux) instead of a frame no longer part of the file. Done with
            // no VFS lock held (address-space `busy` is taken inside).
            if let Some((id, old_len)) = mapped {
                if new_len < old_len {
                    sched::zap_backing_all(0, id, ((new_len + 4095) & !4095) as u64, u64::MAX);
                }
            }
            ok_reply()
        }
        VnodeKind::MountedFile { port, file_id } => {
            drop(tbls);
            let mut proxy = Message::empty();
            proxy.tag = VFS_FTRUNCATE;
            proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&(new_len as u64).to_le_bytes());
            call_port(port, proxy)
        }
        _ => err_reply(-22),
    }
}

/// fallocate(fd, mode, offset, len).
///
/// Supported modes are 0, `FALLOC_FL_KEEP_SIZE` and `FALLOC_FL_PUNCH_HOLE |
/// FALLOC_FL_KEEP_SIZE` (the last on tmpfs only); anything else is
/// EOPNOTSUPP, exactly the answer Linux gives for a mode a filesystem lacks.
/// This used to be a kernel-side `0`, so posix_fallocate "succeeded" while
/// leaving the file at its old size.
///
/// tmpfs really allocates: every page of the range becomes a charged frame
/// (ENOSPC against the tmpfs budget), and mode 0 moves EOF to the end of the
/// range. A punched range reads back as zeros and its whole pages are freed.
fn handle_fallocate(pid: u32, fd: usize, mode: u32, offset: i64, len: i64) -> Message {
    const FALLOC_FL_KEEP_SIZE:  u32 = 0x01;
    const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;
    if offset < 0 || len <= 0 { return err_reply(-22); } // EINVAL
    let punch = mode == FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE;
    if mode != 0 && mode != FALLOC_FL_KEEP_SIZE && !punch { return err_reply(-95); } // EOPNOTSUPP
    let end = match (offset as u64).checked_add(len as u64) {
        Some(e) if e <= i64::MAX as u64 => e as usize,
        _ => return err_reply(-27), // EFBIG
    };
    let offset = offset as usize;
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }
    if tbl.fds[fd].flags & O_PATH != 0 { return err_reply(-9); }
    let kind = tbl.fds[fd].kind;
    drop(tbls);
    match kind {
        VnodeKind::TmpFile { writable, .. } => {
            let idx = tmp_idx_of(&kind);
            let kcred = cred_of(pid);
            let mut tmp = TMP_FILES.lock();
            if tmp[idx].is_dir { return err_reply(-21); } // EISDIR
            if !writable { return err_reply(-9); }
            if end > MAX_TMP_FILE_SIZE { return err_reply(-27); } // EFBIG
            tmp_kill_setid(&mut tmp, idx, &kcred); // Linux file_modified
            let mut vmos = TMP_VMOS.lock();
            let len = tmp[idx].len;
            let vmo = vmos[idx].get_or_insert_with(|| { let mut v = TmpVmo::new_owned(false); v.len = len; v });
            // A dmabuf export's frames belong to the DRM layer.
            if vmo.borrowed { return err_reply(-19); } // ENODEV
            if punch {
                // Zero what the range covers in existing frames, then drop the
                // pages it covers whole. Bounded by the page list, so punching
                // far past the data costs nothing.
                vmo_zero_range(vmo, offset, end);
                let first = (offset + 4095) / 4096;
                let last = (end / 4096).min(vmo.pages.len()); // exclusive
                for i in first..last.max(first) {
                    let phys = core::mem::replace(&mut vmo.pages[i], 0);
                    tmp_page_put(phys);
                }
                while vmo.pages.last() == Some(&0) { vmo.pages.pop(); }
                // Mappings lose the punched pages too (see handle_ftruncate).
                let id = vmo.id;
                if id != 0 && last > first {
                    drop(vmos);
                    drop(tmp);
                    sched::zap_backing_all(0, id, (first * 4096) as u64, (last * 4096) as u64);
                    let mut tmp = TMP_FILES.lock();
                    tmp_touch_mtime(&mut tmp[idx]);
                    return ok_reply();
                }
            } else {
                // Bytes between the old EOF and the range in an existing
                // frame must read as zeros once EOF moves past them.
                if mode == 0 && end > len { vmo_zero_range(vmo, len, end); }
                for i in offset / 4096..(end + 4095) / 4096 {
                    if let Err(e) = vmo.ensure_page(i, true) { return make_reply(e); }
                }
                if mode == 0 && end > len { vmo.len = end; tmp[idx].len = end; }
            }
            tmp_touch_mtime(&mut tmp[idx]);
            ok_reply()
        }
        VnodeKind::MountedFile { port, file_id } => {
            if punch { return err_reply(-95); } // EOPNOTSUPP
            let mut proxy = Message::empty();
            proxy.tag = VFS_FALLOCATE;
            proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&(mode as u64).to_le_bytes());
            proxy.data[16..24].copy_from_slice(&(offset as u64).to_le_bytes());
            proxy.data[24..32].copy_from_slice(&((end - offset) as u64).to_le_bytes());
            call_port(port, proxy)
        }
        VnodeKind::Pipe { .. } => err_reply(-29), // ESPIPE
        _ => err_reply(-19), // ENODEV — not a regular file
    }
}

/// fsync(fd) — flush the filesystem backing `fd`.
///
/// Only a mounted filesystem has anything to flush. tmpfs, procfs, pipes and
/// device nodes hold no write-back state that could survive a reset, so
/// reporting success for them is accurate, not a shortcut.
///
/// `kind` (`FSYNC_KIND_*`) tells the filesystem whether this is fsync /
/// fdatasync of the file (which f2fs can skip or batch per file) or syncfs
/// of the whole mount. The proxy carries (file_id, kind); a server that does
/// not look at them flushes everything, which is what sync(2)'s empty
/// message (kind 0) asks for.
fn handle_fsync(pid: u32, fd: usize, kind: u64) -> Message {
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); } // EBADF
    match tbl.fds[fd].kind {
        VnodeKind::MountedFile { port, file_id } => {
            drop(tbls);
            let mut proxy = Message::empty();
            proxy.tag = VFS_FSYNC;
            proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&kind.to_le_bytes());
            call_port(port, proxy)
        }
        // `gpt::GptDisk::write()` hands its caller the device file and
        // disks-rs immediately calls `sync_all()` on it, so this must both
        // succeed and actually commit the device's write cache.
        VnodeKind::BlockDev { dev, .. } => {
            drop(tbls);
            make_reply(block::flush_dev(dev) as i64)
        }
        _ => ok_reply(),
    }
}

/// sync() — flush every mounted filesystem.
///
/// `sync(2)` returns void and cannot fail, so a port that answers with an
/// error is logged by omission rather than propagated: there is no way to
/// report it, and giving up on the remaining mounts would be worse.
fn handle_sync() -> Message {
    let ports = {
        let m = MOUNTS.lock();
        let mut ports = [0u32; MAX_MOUNTS];
        let mut n = 0;
        for e in m.iter() {
            if e.in_use { ports[n] = e.port; n += 1; }
        }
        (ports, n)
    };
    // Collect the ports first, then release MOUNTS: call_port re-enters the
    // server, and holding the mount table across that invites a deadlock.
    let (ports, n) = ports;
    for &port in &ports[..n] {
        let mut proxy = Message::empty();
        proxy.tag = VFS_FSYNC;
        let _ = call_port(port, proxy);
    }
    ok_reply()
}

// NOTE — every handler below asks `tmpfs_path()` *before* `find_mount_port()`.
// See the comment on `tmpfs_path` for why the reverse order silently routed
// all of /tmp's mutating operations at the pivoted-root F2FS mount.

// ── devfs name table ─────────────────────────────────────────────────────────
//
// /dev's built-in nodes (RAMFS entries directly under /dev) are static. Linux
// devtmpfs lets root rename, hard-link and unlink them, so a small mutable
// name table sits in front: `alias` maps a name to the static node's
// canonical path, `gone` lists static names that were renamed away or removed.
// `rewrite_one` swaps an aliased name for its canonical path (a removed name
// for a path that does not exist) before any handler runs. Only host root may
// change it (Linux: /dev is root:root 0755, no write for anyone else).

struct DevfsNames { alias: Vec<(Vec<u8>, Vec<u8>)>, gone: Vec<Vec<u8>> }
static DEVFS_NAMES: Mutex<DevfsNames> = Mutex::new(DevfsNames { alias: Vec::new(), gone: Vec::new() });
static DEVFS_MUTATED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// `/dev/<name>` with a single component.
fn devfs_leaf(path: &[u8]) -> bool {
    match path.strip_prefix(b"/dev/".as_slice()) {
        Some(r) => !r.is_empty() && !r.contains(&b'/'),
        None => false,
    }
}

fn devfs_static(path: &[u8]) -> bool {
    devfs_leaf(path) && RAMFS.iter().any(|e| e.path == path)
}

/// Canonical path for `path` when the table changes its meaning: the static
/// node an alias points at, or a never-existing path for a removed name.
fn devfs_canon(path: &[u8]) -> Option<Vec<u8>> {
    if !DEVFS_MUTATED.load(core::sync::atomic::Ordering::Acquire) || !devfs_leaf(path) { return None; }
    let t = DEVFS_NAMES.lock();
    if let Some((_, c)) = t.alias.iter().find(|(n, _)| n.as_slice() == path) { return Some(c.clone()); }
    if t.gone.iter().any(|n| n.as_slice() == path) { return Some(b"/dev/.gone".to_vec()); }
    None
}

/// The static node `path` currently names (None if it names nothing).
fn devfs_target(t: &DevfsNames, path: &[u8]) -> Option<Vec<u8>> {
    if let Some((_, c)) = t.alias.iter().find(|(n, _)| n.as_slice() == path) { return Some(c.clone()); }
    if devfs_static(path) && !t.gone.iter().any(|n| n.as_slice() == path) { return Some(path.to_vec()); }
    None
}

/// Drop the name `path` (alias entry, and a tombstone if it is a static name).
fn devfs_drop(t: &mut DevfsNames, path: &[u8]) {
    t.alias.retain(|(n, _)| n.as_slice() != path);
    if devfs_static(path) && !t.gone.iter().any(|n| n.as_slice() == path) { t.gone.push(path.to_vec()); }
    DEVFS_MUTATED.store(true, core::sync::atomic::Ordering::Release);
}

/// Names a devfs rename/link/unlink concerns: a single-component /dev name
/// that is a built-in node or an alias. `None` when the path is not devfs's.
fn devfs_owns(path: &[u8]) -> bool {
    if !devfs_leaf(path) { return false; }
    let t = DEVFS_NAMES.lock();
    devfs_target(&t, path).is_some() || t.gone.iter().any(|n| n.as_slice() == path)
}

fn devfs_dest_ok(path: &[u8]) -> Option<i32> {
    if !devfs_leaf(path) { return Some(-18); }                    // EXDEV: not a plain /dev name
    if path.len() - 5 > 255 { return Some(-36); }
    if DEVFS_DIRS.iter().any(|d| *d == path) || is_tmpfs_root(path) { return Some(-21); } // EISDIR
    None
}

const DEVFS_DIRS: &[&[u8]] = &[b"/dev/shm", b"/dev/dri", b"/dev/input", b"/dev/pts", b"/dev/.devpts", b"/dev/mqueue"];

fn devfs_rename(pid: u32, old: &[u8], new: &[u8], flags: usize) -> Message {
    const RENAME_NOREPLACE: usize = 1;
    if flags & !RENAME_NOREPLACE != 0 { return err_reply(-22); }
    if !host_root_of(pid) { return err_reply(-13); } // EACCES: /dev is root-writable only
    if let Some(e) = devfs_dest_ok(new) { return err_reply(e); }
    let mut t = DEVFS_NAMES.lock();
    let target = match devfs_target(&t, old) { Some(c) => c, None => return err_reply(-2) };
    if old == new { return ok_reply(); }
    if flags & RENAME_NOREPLACE != 0 && devfs_target(&t, new).is_some() { return err_reply(-17); }
    devfs_drop(&mut t, old);
    t.alias.retain(|(n, _)| n.as_slice() != new);
    t.alias.push((new.to_vec(), target));
    ok_reply()
}

fn devfs_link(pid: u32, old: &[u8], new: &[u8]) -> Message {
    if !host_root_of(pid) { return err_reply(-13); }
    if let Some(e) = devfs_dest_ok(new) { return err_reply(e); }
    let mut t = DEVFS_NAMES.lock();
    let target = match devfs_target(&t, old) { Some(c) => c, None => return err_reply(-2) };
    if devfs_target(&t, new).is_some() { return err_reply(-17); }
    t.alias.push((new.to_vec(), target));
    DEVFS_MUTATED.store(true, core::sync::atomic::Ordering::Release);
    ok_reply()
}

fn devfs_unlink(pid: u32, path: &[u8]) -> Message {
    if !host_root_of(pid) { return err_reply(-13); }
    let mut t = DEVFS_NAMES.lock();
    if devfs_target(&t, path).is_none() { return err_reply(-2); }
    devfs_drop(&mut t, path);
    ok_reply()
}

fn handle_rename(pid: u32, old_ptr: usize, new_ptr: usize, flags: usize) -> Message {
    mnt::name_gen_bump(); // mntiso-bindpin: bind pins re-verify after this
    let (obuf, olen) = match read_cstr_raw(old_ptr) { Some(r) => r, None => return err_reply(-14) };
    let (nbuf, nlen) = match read_cstr_raw(new_ptr) { Some(r) => r, None => return err_reply(-14) };

    {
        let (o, n) = (strip_trailing_slash(&obuf[..olen]), strip_trailing_slash(&nbuf[..nlen]));
        if o.starts_with(b"/dev/") && n.starts_with(b"/dev/") && devfs_owns(o) {
            return devfs_rename(pid, o, n, flags);
        }
    }
    match (tmpfs_path(&obuf[..olen]), tmpfs_path(&nbuf[..nlen])) {
        (Some(old), Some(new)) => tmpfs_rename(pid, old, new, flags),
        // One side in tmpfs, the other not: a real cross-filesystem move,
        // which is EXDEV. Coreutils' `mv` falls back to copy+unlink on this.
        (Some(_), None) | (None, Some(_)) => err_reply(-18),
        (None, None) => {
            let old_port = find_mount_port(&obuf[..olen]);
            let new_port = find_mount_port(&nbuf[..nlen]);
            match (old_port, new_port) {
                (Some(op), Some(np)) if op == np => {
                    let mut proxy = Message::empty();
                    proxy.tag = VFS_RENAME;
                    proxy.data[0..8].copy_from_slice(&(old_ptr as u64).to_le_bytes());
                    proxy.data[8..16].copy_from_slice(&(new_ptr as u64).to_le_bytes());
                    proxy.data[16..24].copy_from_slice(&(flags as u64).to_le_bytes());
                    call_port(op, proxy)
                }
                (None, None) => err_reply(-30), // EROFS — RAMFS and friends
                _ => err_reply(-18),            // EXDEV
            }
        }
    }
}

/// rename(2) within the tmpfs pool. Both paths are already normalised.
///
/// Renaming a directory rewrites every descendant's stored path, since the
/// pool is flat and parentage is encoded in the path bytes alone.
fn tmpfs_rename(pid: u32, old: &[u8], new: &[u8], flags: usize) -> Message {
    const RENAME_NOREPLACE: usize = 1;
    if old == new { return ok_reply(); }
    if is_tmpfs_root(old) || is_tmpfs_root(new) { return err_reply(-16); } // EBUSY — mount root
    if new.len() > MAX_TMP_PATH - 1 { return err_reply(-36); }     // ENAMETOOLONG
    let cred = cred_of(pid);

    let open_fds = tmp_open_fd_mask(); // before TMP_FILES: FD_TABLES → TMP_FILES
    // A DRM reference released from a clobbered destination inode, held until
    // the TMP_FILES guard is gone (see `DMABUF_RELEASE`).
    let mut clobbered_obj: Option<u32> = None;
    let mut tmp = TMP_FILES.lock();
    // Search on the source's parent legitimises the lookup that follows.
    let old_parent = match tmp_parent(old) { Some(p) => p, None => return err_reply(-16) };
    if let Err(e) = tmp_dir_may(&tmp, old_parent, &cred, xattr::MAY_EXEC) { return err_reply(e); }
    let idx = match tmp_find(&tmp, old) { Some(i) => i, None => return err_reply(-2) };
    let src_is_dir = tmp[idx].is_dir;

    // "mv d d/sub" would detach the subtree from the namespace.
    if new.len() > old.len() && new.starts_with(old) && new[old.len()] == b'/' {
        return err_reply(-22); // EINVAL
    }
    let parent = match tmp_parent(new) { Some(p) => p, None => return err_reply(-16) };
    // Permissions, Linux may_delete/may_create order: removing the source
    // name (write+search on its parent, sticky rule), creating the destination
    // name (write+search on the new parent), and write on a directory that
    // changes parents.
    if let Err(e) = tmp_may_delete(&tmp, old_parent, idx, &cred) { return err_reply(e); }
    if let Err(e) = tmp_dir_may(&tmp, parent, &cred, xattr::MAY_WRITE | xattr::MAY_EXEC) {
        return err_reply(e);
    }
    if src_is_dir && parent != old_parent && !tmp_may(&tmp, idx, &cred, xattr::MAY_WRITE) {
        return err_reply(-13);
    }

    // Destination handling, POSIX order: type mismatches first, then the
    // implicit removal of an existing target.
    if let Some(didx) = tmp_find(&tmp, new) {
        if flags & RENAME_NOREPLACE != 0 { return err_reply(-17); } // EEXIST
        if tmp_sticky_denies(&tmp, parent, didx, &cred) { return err_reply(-1); } // EPERM
        let dst_is_dir = tmp[didx].is_dir;
        if src_is_dir && !dst_is_dir { return err_reply(-20); }  // ENOTDIR
        if !src_is_dir && dst_is_dir { return err_reply(-21); }  // EISDIR
        if dst_is_dir && tmp_has_descendants(&tmp, new, didx) {
            return err_reply(-39); // ENOTEMPTY
        }
        // Clobbering the destination drops one name, not necessarily the file:
        // if the victim was hard-linked elsewhere its bytes must survive.
        // A dmabuf export cannot land here — the PRIME intercept unlinks its
        // node immediately, so it is nameless and `tmp_find` never sees it —
        // but the reference is threaded out regardless rather than relying on
        // that, and it is released only once TMP_FILES is gone.
        clobbered_obj = tmp_drop_name(&mut tmp, didx, open_fds);
    }

    if !src_is_dir {
        tmp.set_path(idx, new);
        drop(tmp);
        dmabuf_release(clobbered_obj.unwrap_or(0));
        return ok_reply();
    }

    // Directory: check every descendant fits under the new prefix *before*
    // mutating anything, so a failure leaves the pool untouched.
    let grow = new.len() as isize - old.len() as isize;
    // Phrased as `any` rather than an early `return` inside `for ... in
    // tmp.iter()`: the guard has to be dropped before `dmabuf_release`, and it
    // cannot be while an iterator is borrowing it.
    let under_old = |e: &TmpFileEntry| {
        let p = e.path();
        !e.ephemeral() && p.len() > old.len() && &p[..old.len()] == old && p[old.len()] == b'/'
    };
    // The pool's child index names exactly the subtree, parents first.
    let subtree = tmp.subtree(idx);
    let too_long = subtree.iter().any(|&i| {
        under_old(&tmp[i]) && (tmp[i].path().len() as isize + grow) as usize > MAX_TMP_PATH - 1
    });
    if too_long {
        drop(tmp);
        dmabuf_release(clobbered_obj.unwrap_or(0));
        return err_reply(-36); // ENAMETOOLONG
    }
    // The directory first, then its contents top-down: each re-pathed slot
    // is re-linked under its (already re-pathed) parent.
    tmp.set_path(idx, new);
    let mut buf: Vec<u8> = Vec::with_capacity(MAX_TMP_PATH);
    for i in subtree {
        if !tmp[i].in_use() || !under_old(&tmp[i]) { continue; }
        buf.clear();
        buf.extend_from_slice(new);
        buf.extend_from_slice(&tmp[i].path()[old.len()..]);
        tmp.set_path(i, &buf);
    }
    drop(tmp);
    dmabuf_release(clobbered_obj.unwrap_or(0));
    ok_reply()
}

fn handle_unlink(pid: u32, path_ptr: usize) -> Message {
    mnt::name_gen_bump(); // mntiso-bindpin: bind pins re-verify after this
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];

    if cgroupfs::is_path(strip_trailing_slash(raw)) { return err_reply(-1); } // EPERM
    if devfs_owns(strip_trailing_slash(raw)) { return devfs_unlink(pid, strip_trailing_slash(raw)); }
    if let Some(name) = mqueue::path_name(strip_trailing_slash(raw)) {
        let r = mqueue::unlink(pid, name);
        return if r < 0 { err_reply(r as i32) } else { ok_reply() };
    }
    if let Some(path) = tmpfs_path(raw) {
        if is_tmpfs_root(path) { return err_reply(-21); } // EISDIR — mount root
        let cred = cred_of(pid);
        let open_fds = tmp_open_fd_mask(); // before TMP_FILES: FD_TABLES → TMP_FILES
        let mut tmp = TMP_FILES.lock();
        let parent = match tmp_parent(path) { Some(p) => p, None => return err_reply(-21) };
        if let Err(e) = tmp_dir_may(&tmp, parent, &cred, xattr::MAY_EXEC) { return err_reply(e); }
        let (reply, freed_obj) = match tmp_find(&tmp, path) {
            Some(idx) if tmp[idx].is_dir => (err_reply(-21), None), // EISDIR — use rmdir()
            // Drops the *name* — once write on the parent and the sticky rule
            // allow it (a /tmp file belonging to someone else stays put). The
            // bytes go only when the last name does —
            // see tmp_drop_name. A symlink lands here too (the choke point
            // deliberately did not follow the final component), so `rm l`
            // removes the link and never the file it points at.
            //
            // This is the path an `unlink("/tmp/dmabuf:<n>")` takes while the
            // exported fd is still open: the inode is only marked ephemeral,
            // so nothing is released here and `freed_obj` is None. It becomes
            // Some only if the name outlived every fd, which the PRIME
            // intercept's immediate unlink makes impossible in practice.
            Some(idx) => match tmp_may_delete(&tmp, parent, idx, &cred) {
                Ok(())  => (ok_reply(), tmp_drop_name(&mut tmp, idx, open_fds)),
                Err(e)  => (err_reply(e), None),
            },
            None      => (err_reply(-2), None),
        };
        // TMP_FILES gone before the device is touched (see `DMABUF_RELEASE`).
        drop(tmp);
        dmabuf_release(freed_obj.unwrap_or(0));
        return reply;
    }
    if let Some(port) = find_mount_port(raw) {
        let mut proxy = Message::empty();
        proxy.tag = VFS_UNLINK;
        proxy.data[0..8].copy_from_slice(&(path_ptr as u64).to_le_bytes());
        return call_port(port, proxy);
    }
    err_reply(-30) // EROFS
}

fn handle_mkdir(pid: u32, path_ptr: usize, mode: u32) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];

    if cgroupfs::is_path(strip_trailing_slash(raw)) {
        let r = cgroupfs::mkdir(pid, strip_trailing_slash(raw));
        return if r < 0 { err_reply(r as i32) } else { ok_reply() };
    }

    if let Some(path) = tmpfs_path(raw) {
        if is_tmpfs_root(path) { return err_reply(-17); }              // EEXIST — mount root
        if path.len() > MAX_TMP_PATH - 1 { return err_reply(-36); }    // ENAMETOOLONG
        let cred = cred_of(pid);
        let mut tmp = TMP_FILES.lock();
        // Intermediate components must already exist — `mkdir -p` creates
        // them outermost-first, so this is the check that makes it correct
        // rather than silently producing an orphaned "/tmp/a/b" — and the
        // caller needs write + search on the parent, checked in Linux's
        // order (see `tmp_create_gate`: EEXIST before the write check).
        if let Err(e) = tmp_create_gate(&tmp, path, &cred) { return err_reply(e); }
        let idx = match tmp_free_slot(&mut *tmp) {
            Some(i) => i,
            None    => return err_reply(-28), // ENOSPC
        };
        tmp.claim(idx, path, false);
        tmp[idx].is_dir = true;
        tmp[idx].uid  = cred.euid;
        tmp[idx].gid  = cred.egid;
        let (m, um) = xattr::unpack_create_mode(mode as u64);
        tmp_init_created(&mut tmp, idx, path, m, um, true);
        return ok_reply();
    }
    if let Some(port) = find_mount_port(raw) {
        let mut proxy = Message::empty();
        proxy.tag = VFS_MKDIR;
        proxy.data[0..8].copy_from_slice(&(path_ptr as u64).to_le_bytes());
        proxy.data[8..16].copy_from_slice(&(mode as u64).to_le_bytes());
        return call_port(port, proxy);
    }
    for &dir in RAMFS_DIRS { if raw == dir { return err_reply(-17); } }
    err_reply(-30) // EROFS
}

/// mknod(path_ptr, mode) — create a /tmp entry as either a plain file (no
/// S_IFMT bits, or S_IFREG) or a FIFO (S_IFIFO). Mirrors handle_mkdir's
/// duplicate/parent-exists/free-slot checks; the kernel-side caller
/// (sys_mknodat) has already rejected device/socket type bits with EPERM,
/// so by the time a message reaches here `mode` only ever names a file or
/// a FIFO.
///
/// SCOPE LIMIT: this only makes the tmpfs entry exist and reports the right
/// type from fstat/stat/getdents64 (S_IFIFO / DT_FIFO). It does NOT
/// implement FIFO read/write semantics — VFS_OPEN on an is_fifo entry still
/// behaves like an empty regular file (no blocking open, no rendezvous
/// between reader and writer). Real FIFO semantics would need their own
/// vnode kind, the way `Pipe` already has one.
fn handle_mknod(pid: u32, path_ptr: usize, mode: u32, dev: u64) -> Message {
    const S_IFMT:  u32 = 0o170000;
    const S_IFCHR: u32 = 0o020000;
    const S_IFBLK: u32 = 0o060000;
    const S_IFIFO: u32 = 0o010000;
    const S_IFSOCK: u32 = 0o140000;

    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];
    let ftype = mode & S_IFMT;

    // Device cgroup (BPF_CGROUP_DEVICE): creating a device node needs MKNOD
    // access. The whiteout (char 0:0) is exempt. Every mknod route (syscall,
    // overlay copy-up, mount proxy) funnels through here.
    if (ftype == S_IFCHR || ftype == S_IFBLK) && !(ftype == S_IFCHR && dev as u32 == 0) {
        let (maj, min) = dev_major_minor(dev);
        if sched::bpfcg::dev_check(pid, ftype == S_IFBLK, maj, min, 1).is_err() {
            return err_reply(-1); // EPERM
        }
    }

    if let Some(path) = tmpfs_path(raw) {
        if is_tmpfs_root(path) { return err_reply(-17); }              // EEXIST — mount root
        if path.len() > MAX_TMP_PATH - 1 { return err_reply(-36); }    // ENAMETOOLONG
        let cred = cred_of(pid);
        let mut tmp = TMP_FILES.lock();
        if let Err(e) = tmp_create_gate(&tmp, path, &cred) { return err_reply(e); }
        let idx = match tmp_free_slot(&mut *tmp) {
            Some(i) => i,
            None    => return err_reply(-28), // ENOSPC
        };
        tmp.claim(idx, path, false);
        tmp[idx].is_dir  = false;
        tmp[idx].is_fifo = ftype == S_IFIFO;
        if ftype == S_IFSOCK {
            // A bare socket node (no listener): connect() finds it but
            // there is nothing behind it -> ECONNREFUSED.
            tmp[idx].is_sock = true;
            tmp[idx].sock_id = u64::MAX;
        }
        if ftype == S_IFCHR || ftype == S_IFBLK {
            tmp[idx].devtype = if ftype == S_IFCHR { 1 } else { 2 };
            tmp[idx].rdev    = dev as u32;
        }
        tmp[idx].uid  = cred.euid;
        tmp[idx].gid  = cred.egid;
        // mknod's mode arrives unpacked (it carries the S_IFMT type bits).
        tmp_init_created(&mut tmp, idx, path, (mode & 0o7777) as u16,
                         sched::umask(u32::MAX) as u16, false);
        return ok_reply();
    }
    if let Some(port) = find_mount_port(raw) {
        let mut proxy = Message::empty();
        proxy.tag = VFS_MKNOD;
        proxy.data[0..8].copy_from_slice(&(path_ptr as u64).to_le_bytes());
        // The server gets the S_IFMT bits plus the caller's umask packed
        // above bit 16 (see `xattr::pack_create_mode`); the type sits at
        // bits 12..16 of `mode`, which the packed word's 0o7777 mask would
        // drop, so it travels in its own word.
        let packed = xattr::pack_create_mode(mode & 0o7777, sched::umask(u32::MAX));
        proxy.data[8..16].copy_from_slice(&packed.to_le_bytes());
        proxy.data[16..24].copy_from_slice(&((ftype >> 12) as u64).to_le_bytes());
        proxy.data[24..32].copy_from_slice(&dev.to_le_bytes());
        return call_port(port, proxy);
    }
    for &dir in RAMFS_DIRS { if raw == dir { return err_reply(-17); } }
    err_reply(-30) // EROFS
}


/// f2fs (and any mount server) answers an open of a character/block device
/// node with `-(DEVNODE_BASE + (is_blk << 32 | rdev))`: the node's driver
/// lives in the VFS, not in the filesystem.
const DEVNODE_BASE: i64 = 1 << 40;

/// Finish an open of an on-disk device node: find the driver that owns
/// `rdev` and re-enter `handle_open` on its /dev name. Unknown numbers (and
/// the whiteout 0:0) are ENXIO, as on Linux.
/// Device identity `(is_block, major, minor)` of a canonical open path, for
/// the device-cgroup check: fixed char nodes, VTs, pts, dynamic devices
/// (drm, input, ...) and block devices.
fn open_dev_identity(path: &[u8]) -> Option<(bool, u32, u32)> {
    let r = dev_node_rdev(path);
    if r != 0 {
        let (ma, mi) = dev_major_minor(r);
        return Some((false, ma, mi));
    }
    if path.starts_with(b"/dev/") {
        if path == b"/dev/ptmx" { return Some((false, 5, 2)); }
        if let Some(dev) = block::lookup_dev_node(path) {
            let (ma, mi) = dev_major_minor(block::dev_rdev(dev));
            return Some((true, ma, mi));
        }
        let devices = DYNAMIC_DEVICES.lock();
        if let Some(d) = devices.iter().find(|d| d.in_use && d.path.as_bytes() == path) {
            return Some((false, d.major, d.minor));
        }
    }
    None
}

fn devnode_open(pid: u32, is_blk: bool, rdev: u32, flags: u32, mode: u32) -> Message {
    let (major, minor) = dev_major_minor(rdev as u64);
    let mut buf: Vec<u8> = Vec::new();
    if is_blk {
        if !block::path_for_rdev(major, minor, &mut buf) { return err_reply(-6); }
    } else {
        let fixed: Option<&[u8]> = match (major, minor) {
            (1, 3) => Some(b"/dev/null"), (1, 5) => Some(b"/dev/zero"), (1, 7) => Some(b"/dev/full"),
            (1, 8) => Some(b"/dev/random"), (1, 9) => Some(b"/dev/urandom"),
            (5, 0) => Some(b"/dev/tty"), (5, 1) => Some(b"/dev/console"),
            (5, 2) => Some(b"/dev/ptmx"), (29, 0) => Some(b"/dev/fb0"),
            _ => None,
        };
        if let Some(p) = fixed {
            buf.extend_from_slice(p);
        } else if major == 4 && (1..=63).contains(&minor) {
            buf.extend_from_slice(b"/dev/tty");
            push_decimal(&mut buf, minor);
        } else if major >= 136 && major <= 143 {
            buf.extend_from_slice(b"/dev/pts/");
            push_decimal(&mut buf, (major - 136) * 256 + minor);
        } else {
            let devices = DYNAMIC_DEVICES.lock();
            match devices.iter().find(|d| d.in_use && d.major == major && d.minor == minor && (major, minor) != (0, 0)) {
                Some(d) => buf.extend_from_slice(d.path.as_bytes()),
                None => return err_reply(-6), // ENXIO
            }
        }
    }
    buf.push(0);
    handle_open(pid, buf.as_ptr() as usize, flags, mode)
}

fn push_decimal(buf: &mut Vec<u8>, mut n: u32) {
    let mut tmp = [0u8; 10];
    let mut i = tmp.len();
    loop { i -= 1; tmp[i] = b'0' + (n % 10) as u8; n /= 10; if n == 0 { break; } }
    buf.extend_from_slice(&tmp[i..]);
}

// ── AF_UNIX socket nodes (called from the net server) ────────────────────────
//
// A pathname AF_UNIX bind creates a real S_IFSOCK node on tmpfs; connect
// resolves the path through the same lookup machinery (symlinks, the
// /tmp/-/dev/shm/-/run/user mounts) back to the net server's listener.
// Abstract-namespace sockets never reach here (net matches those by bytes).

/// bind(): create an S_IFSOCK node at `path`, tagged with the net server's
/// `sock_id`. Intermediate components are followed through symlinks; the final
/// component is created literally (bind does not follow a trailing symlink).
///
/// 0 on success, else a negative errno: -17 EEXIST (net → EADDRINUSE), -2
/// ENOENT (missing parent), -36 ENAMETOOLONG, -28 ENOSPC, -95 EOPNOTSUPP (the
/// path is on no mount server). Sockets on mounted filesystems (f2fs) are a
/// real S_IFSOCK inode plus an in-memory (path, inode) -> listener table
/// (`mounted_sock_bind`).
pub fn unix_bind_node(pid: u32, path: &[u8], sock_id: u64) -> i32 {
    let resolved = match tmp_resolve_links(path, false) {
        Ok(v)  => v,
        Err(e) => return e,
    };
    let rpath = &resolved[..];
    let tpath = match tmpfs_path(rpath) {
        Some(p) => p,
        None    => return mounted_sock_bind(rpath, sock_id),
    };
    if is_tmpfs_root(tpath) { return -17; }             // EEXIST — the mount root
    if tpath.len() > MAX_TMP_PATH - 1 { return -36; }   // ENAMETOOLONG
    let cred = cred_of(pid);
    let mut tmp = TMP_FILES.lock();
    // A socket node is an entry like any other: write + search on the parent.
    if let Err(e) = tmp_create_gate(&tmp, tpath, &cred) { return e; }
    let idx = match tmp_free_slot(&mut *tmp) {
        Some(i) => i, None => return -28, // ENOSPC
    };
    tmp.claim(idx, tpath, false);
    tmp[idx].is_sock = true;
    tmp[idx].sock_id = sock_id;
    tmp[idx].uid  = cred.euid;
    tmp[idx].gid  = cred.egid;
    tmp_init_created(&mut tmp, idx, tpath, 0o777, sched::umask(u32::MAX) as u16, false);
    0
}

// ── AF_UNIX sockets on mounted filesystems ───────────────────────────────────
//
// bind() makes a real S_IFSOCK inode through the mount server (permission and
// EEXIST checks are the server's own mknod gate), then records which listener
// it belongs to. The listener identity lives only in memory, so after a reboot
// the node is a stale socket file: connect finds the inode but no listener
// (ECONNREFUSED), as on Linux. unlink needs no hook: connect re-stats the path
// and only honours an entry whose inode number still matches.

/// (mount-server path, st_ino, net sock_id)
static MOUNTED_SOCKS: Mutex<Vec<(Vec<u8>, u64, u64)>> = Mutex::new(Vec::new());
const MOUNTED_SOCKS_MAX: usize = 1024;

/// lstat `path` on its mount server: (mode, uid, gid, ino) or -errno.
fn mounted_lstat(port: u32, path: &[u8]) -> Result<(u32, u32, u32, u64), i32> {
    let mut pb: Vec<u8> = Vec::new();
    if pb.try_reserve_exact(path.len() + 1).is_err() { return Err(-12); }
    pb.extend_from_slice(path);
    pb.push(0);
    let mut st = [0u64; 24];
    let mut m = Message::empty();
    m.tag = VFS_LSTAT;
    m.data[0..8].copy_from_slice(&(pb.as_ptr() as u64).to_le_bytes());
    m.data[8..16].copy_from_slice(&(st.as_mut_ptr() as u64).to_le_bytes());
    let r = reply_val(&call_port(port, m));
    if r < 0 { return Err(r as i32); }
    let p = st.as_ptr() as *const u8;
    unsafe {
        let ino = (p.add(8) as *const u64).read_unaligned();
        #[cfg(target_arch = "x86_64")]
        let (mode, uid, gid) = ((p.add(24) as *const u32).read_unaligned(),
            (p.add(28) as *const u32).read_unaligned(), (p.add(32) as *const u32).read_unaligned());
        #[cfg(not(target_arch = "x86_64"))]
        let (mode, uid, gid) = ((p.add(16) as *const u32).read_unaligned(),
            (p.add(24) as *const u32).read_unaligned(), (p.add(28) as *const u32).read_unaligned());
        Ok((mode, uid, gid, ino))
    }
}

fn mounted_sock_bind(rpath: &[u8], sock_id: u64) -> i32 {
    let port = match find_mount_port(rpath) { Some(p) => p, None => return -95 };
    if rpath.len() >= PATH_MAX { return -36; }
    let mut pb: Vec<u8> = Vec::new();
    if pb.try_reserve_exact(rpath.len() + 1).is_err() { return -12; }
    pb.extend_from_slice(rpath);
    pb.push(0);
    let packed = xattr::pack_create_mode(0o777, sched::umask(u32::MAX));
    let mut m = Message::empty();
    m.tag = VFS_MKNOD;
    m.data[0..8].copy_from_slice(&(pb.as_ptr() as u64).to_le_bytes());
    m.data[8..16].copy_from_slice(&packed.to_le_bytes());
    m.data[16..24].copy_from_slice(&((0o140000u32 >> 12) as u64).to_le_bytes());
    m.data[24..32].copy_from_slice(&0u64.to_le_bytes());
    let r = reply_val(&call_port(port, m));
    if r < 0 { return r as i32; } // -17 EEXIST, -2 ENOENT, -13 EACCES, -28 ENOSPC...
    let ino = match mounted_lstat(port, rpath) { Ok((_, _, _, i)) => i, Err(e) => return e };
    let mut t = MOUNTED_SOCKS.lock();
    t.retain(|e| e.0 != rpath);
    if t.len() >= MOUNTED_SOCKS_MAX {
        // Drop entries whose node is gone or was replaced.
        let snap: Vec<(Vec<u8>, u64, u64)> = core::mem::take(&mut *t);
        drop(t);
        let keep: Vec<_> = snap.into_iter().filter(|e| {
            find_mount_port(&e.0).map_or(false, |p| matches!(mounted_lstat(p, &e.0), Ok((_, _, _, i)) if i == e.1))
        }).collect();
        t = MOUNTED_SOCKS.lock();
        t.extend(keep);
        if t.len() >= MOUNTED_SOCKS_MAX { return -28; }
    }
    t.push((rpath.to_vec(), ino, sock_id));
    0
}

fn mounted_sock_resolve(pid: u32, rpath: &[u8]) -> i64 {
    let port = match find_mount_port(rpath) { Some(p) => p, None => return -2 };
    let (mode, uid, gid, ino) = match mounted_lstat(port, rpath) { Ok(v) => v, Err(e) => return e as i64 };
    if mode & 0o170000 != 0o140000 { return -111; } // exists, not a socket
    let cred = cred_of(pid);
    if cred.euid != 0 {
        let bits = if cred.euid == uid { (mode >> 6) & 7 }
                   else if cred.egid == gid { (mode >> 3) & 7 } else { mode & 7 };
        if bits & 2 == 0 { return -13; } // connect needs write on the socket inode
    }
    let mut t = MOUNTED_SOCKS.lock();
    match t.iter().position(|e| e.0 == rpath) {
        Some(i) if t[i].1 == ino => t[i].2 as i64,
        Some(i) => { t.swap_remove(i); -111 } // replaced node
        None => -111,                         // stale node: no listener
    }
}

/// connect(): resolve `path` (following symlinks on every component) to the
/// `sock_id` of the S_IFSOCK node bound there. Returns the sock_id (>= 0) or a
/// negative errno: -2 ENOENT (nothing there), -111 ECONNREFUSED (exists but is
/// not a socket), -13 EACCES (a prefix directory the caller may not search,
/// or a socket node the caller may not write — connect needs write on the
/// socket inode, exactly as on Linux; root bypasses).
///
/// A non-tmpfs path is ENOENT, not EOPNOTSUPP: sockets can only ever be bound
/// on tmpfs here, so no socket exists at such a path — and that is exactly
/// what Linux reports when a client connects to a socket file that isn't
/// there (e.g. zbus probing /run/dbus/system_bus_socket on a system with no
/// system bus). EOPNOTSUPP stays reserved for bind, where it correctly says
/// "this filesystem cannot host a socket node".
pub fn unix_resolve_node(pid: u32, path: &[u8]) -> i64 {
    let resolved = match tmp_resolve_links(path, true) {
        Ok(v)  => v,
        Err(e) => return e as i64,
    };
    let rpath = &resolved[..];
    let tpath = match tmpfs_path(rpath) {
        Some(p) => p,
        None    => return mounted_sock_resolve(pid, rpath),
    };
    let cred = cred_of(pid);
    let tmp = TMP_FILES.lock();
    match tmp_find(&tmp, tpath) {
        Some(idx) if tmp[idx].is_sock => {
            if !tmp_may(&tmp, idx, &cred, xattr::MAY_WRITE) { return -13; } // EACCES
            let sid = tmp[tmp_owner(&tmp, idx)].sock_id;
            if sid == u64::MAX { -111 } else { sid as i64 } // mknod'ed node: no listener
        }
        Some(_) => -111, // exists, not a socket → ECONNREFUSED
        None    => -2,   // ENOENT
    }
}

/// symlink(target, linkpath) — create a symlink.
///
/// The target is stored verbatim in the entry's data bytes: no normalisation,
/// no existence check, no resolution. A dangling link is a perfectly legal
/// object (`ln -s /nonexistent l` succeeds on every Unix), and rewriting a
/// relative target at creation time would break the "resolve against the
/// link's own directory" rule that `tmp_resolve_links` implements.
fn handle_symlink(pid: u32, target_ptr: usize, link_ptr: usize) -> Message {
    mnt::name_gen_bump(); // mntiso-bindpin: bind pins re-verify after this
    let (tbuf, tlen) = match read_cstr_raw(target_ptr) { Some(r) => r, None => return err_reply(-14) };
    let (lbuf, llen) = match read_cstr_raw(link_ptr)   { Some(r) => r, None => return err_reply(-14) };
    if tlen == 0 { return err_reply(-2); } // ENOENT — empty target
    let raw = &lbuf[..llen];

    if let Some(path) = tmpfs_path(raw) {
        if is_tmpfs_root(path) { return err_reply(-17); }           // EEXIST — mount root
        if path.len() > MAX_TMP_PATH - 1 { return err_reply(-36); } // ENAMETOOLONG
        // A link body is at most PATH_MAX - 1 bytes, as on Linux.
        if tlen > TMP_LINK_MAX { return err_reply(-36); }             // ENAMETOOLONG
        let cred = cred_of(pid);
        let mut tmp = TMP_FILES.lock();
        if let Err(e) = tmp_create_gate(&tmp, path, &cred) { return err_reply(e); }
        let idx = match tmp_free_slot(&mut *tmp) {
            Some(i) => i,
            None    => return err_reply(-28), // ENOSPC
        };
        tmp.claim(idx, path, false);
        tmp[idx].is_link = true;
        // Symlink permission bits are 0777 everywhere and are never consulted;
        // the target's bits are what govern access.
        tmp[idx].mode = 0o777;
        tmp[idx].uid  = cred.euid;
        tmp[idx].gid  = cred.egid;
        tmp[idx].len  = tlen;
        tmp[idx].link = tbuf[..tlen].to_vec();
        tmp_stamp_new(&mut tmp[idx]);
        return ok_reply();
    }
    if let Some(port) = find_mount_port(raw) {
        let mut proxy = Message::empty();
        proxy.tag = VFS_SYMLINK;
        proxy.data[0..8].copy_from_slice(&(target_ptr as u64).to_le_bytes());
        proxy.data[8..16].copy_from_slice(&(link_ptr as u64).to_le_bytes());
        return call_port(port, proxy);
    }
    err_reply(-30) // EROFS
}

/// readlink(path, buf, len) — copy a symlink's body out, untruncated-length
/// semantics included: the return is `min(target_len, buf_len)` and the buffer
/// is never NUL-terminated, exactly as readlink(2) specifies.
///
/// EINVAL (not ENOENT) for a path that exists but is not a symlink — callers
/// including coreutils' `ls` use precisely that to tell the two apart.
fn handle_readlink(path_ptr: usize, buf_ptr: usize, buf_len: usize) -> Message {
    if buf_ptr == 0 || buf_len == 0 { return err_reply(-14); }
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];

    // /proc/self -> "<tgid>", /proc/thread-self -> "<tgid>/task/<tid>", both
    // numbered in the reader's pid namespace.
    if raw == b"/proc/self" || raw == b"/proc/thread-self" {
        let me = sched::current_pid();
        let tgv = pv(sched::tgid_of(me));
        let mut tmp = [0u8; 48];
        let mut n = 0;
        let mut put = |v: u32, tmp: &mut [u8; 48], n: &mut usize| {
            let mut d = [0u8; 10]; let mut k = 0; let mut x = v;
            loop { d[k] = b'0' + (x % 10) as u8; k += 1; x /= 10; if x == 0 { break; } }
            while k > 0 { k -= 1; tmp[*n] = d[k]; *n += 1; }
        };
        put(tgv, &mut tmp, &mut n);
        if raw == b"/proc/thread-self" {
            for &c in b"/task/" { tmp[n] = c; n += 1; }
            put(pv(me), &mut tmp, &mut n);
        }
        let n = n.min(buf_len);
        unsafe { let _ = sched::uaccess::copy_raw(buf_ptr as *mut u8, tmp.as_ptr(), n); }
        return val_reply(n as u64);
    }

    if raw.starts_with(b"/sys/") {
        let mut lb = [0u8; 96];
        if let Some(n) = sysclass::link(raw, &mut lb) {
            let n = n.min(buf_len);
            unsafe { let _ = sched::uaccess::copy_raw(buf_ptr as *mut u8, lb.as_ptr(), n); }
            return val_reply(n as u64);
        }
        if sysclass::is_path(raw) { return err_reply(-22); } // generated, not a link
    }
    if let Some(path) = tmpfs_path(raw) {
        let tmp = TMP_FILES.lock();
        return match tmp_find(&tmp, path) {
            Some(idx) if tmp[idx].is_link => {
                let n = tmp[idx].len.min(buf_len);
                unsafe { let _ = sched::uaccess::copy_raw(buf_ptr as *mut u8, tmp[idx].link.as_ptr(), n); }
                val_reply(n as u64)
            }
            Some(_) => err_reply(-22), // EINVAL — exists, not a link
            // "/tmp" itself is never a TMP_FILES entry — it is a RAMFS_DIRS
            // pseudo-directory, and the pool holds only strict descendants of
            // it. Falling through to a blanket ENOENT here made
            // readlink("/tmp") claim /tmp does not exist, which is what broke
            // every *relative* realpath(1) under /tmp: musl's realpath(3)
            // readlink()s each path component in turn and aborts the whole
            // call unless a failure is exactly EINVAL ("not a symlink"). Any
            // other errno propagates, so ENOENT on the "/tmp" component made
            // canonicalizing the cwd fail before the operand was ever looked
            // at. Answer for the pseudo-directory the same way the RAMFS_DIRS
            // arm below would.
            None if is_tmpfs_root(path) => err_reply(-22),
            None    => err_reply(-2),  // ENOENT
        };
    }
    if let Some(port) = find_mount_port(raw) {
        let mut proxy = Message::empty();
        proxy.tag = VFS_READLINK;
        proxy.data[0..8].copy_from_slice(&(path_ptr as u64).to_le_bytes());
        proxy.data[8..16].copy_from_slice(&(buf_ptr as u64).to_le_bytes());
        proxy.data[16..24].copy_from_slice(&(buf_len as u64).to_le_bytes());
        return call_port(port, proxy);
    }
    // RamFS, devfs and /proc hold no symlinks. Distinguish "exists but isn't a
    // link" from "isn't there" so callers get the same two errnos they would
    // on Linux.
    for &dir in RAMFS_DIRS { if raw == dir { return err_reply(-22); } }
    for entry in RAMFS     { if raw == entry.path { return err_reply(-22); } }
    err_reply(-2) // ENOENT
}

/// link(oldpath, newpath) — create a second name for an existing file.
///
/// Cross-filesystem links are EXDEV, and `cp -l`/`mv` rely on getting exactly
/// that to fall back to a copy. Directory sources are EPERM (Linux reserves
/// directory hard links for the filesystem's own "." and ".." and refuses them
/// to userspace, because a directory cycle has no safe unwind).
fn handle_link(pid: u32, old_ptr: usize, new_ptr: usize) -> Message {
    let (obuf, olen) = match read_cstr_raw(old_ptr) { Some(r) => r, None => return err_reply(-14) };
    let (nbuf, nlen) = match read_cstr_raw(new_ptr) { Some(r) => r, None => return err_reply(-14) };
    let (oraw, nraw) = (&obuf[..olen], &nbuf[..nlen]);
    if devfs_owns(strip_trailing_slash(oraw)) {
        return devfs_link(pid, strip_trailing_slash(oraw), strip_trailing_slash(nraw));
    }

    let (otmp, ntmp) = (tmpfs_path(oraw), tmpfs_path(nraw));
    match (otmp, ntmp) {
        (Some(old), Some(new)) => {
            if new.len() > MAX_TMP_PATH - 1 { return err_reply(-36); } // ENAMETOOLONG
            let cred = cred_of(pid);
            let mut tmp = TMP_FILES.lock();
            let src = match tmp_find(&tmp, old) { Some(i) => i, None => return err_reply(-2) };
            if tmp[src].is_dir { return err_reply(-1); } // EPERM
            if let Err(e) = tmp_create_gate(&tmp, new, &cred) { return err_reply(e); }
            let owner = tmp_owner(&tmp, src);
            let may_rw = tmp_may(&tmp, src, &cred, xattr::MAY_READ | xattr::MAY_WRITE);
            if xattr::hardlink_denied_c(&tmp_meta(&tmp[owner]), &cred, may_rw) {
                return err_reply(-1); // EPERM — protected_hardlinks
            }
            let idx = match tmp_free_slot(&mut *tmp) {
                Some(i) => i,
                None    => return err_reply(-28), // ENOSPC
            };
            tmp.claim(idx, new, false);
            // The alias carries no bytes of its own — everything that reads
            // content goes through tmp_owner() to `owner`. Mode/uid/gid are
            // mirrored only so a lock-free peek at the slot isn't nonsense;
            // stat() reads them from the owner regardless.
            tmp.set_link_to(idx, owner);
            tmp[idx].is_fifo = tmp[owner].is_fifo;
            tmp[idx].is_link = tmp[owner].is_link;
            tmp[idx].is_sock = tmp[owner].is_sock;
            tmp[idx].sock_id = tmp[owner].sock_id;
            tmp[idx].devtype = tmp[owner].devtype;
            tmp[idx].rdev    = tmp[owner].rdev;
            tmp[idx].mode    = tmp[owner].mode;
            tmp[idx].uid     = tmp[owner].uid;
            tmp[idx].gid     = tmp[owner].gid;
            ok_reply()
        }
        (None, None) => {
            // Both outside tmpfs: legal only if one mount owns both.
            match (find_mount_port(oraw), find_mount_port(nraw)) {
                (Some(a), Some(b)) if a == b => {
                    let mut proxy = Message::empty();
                    proxy.tag = VFS_LINK;
                    proxy.data[0..8].copy_from_slice(&(old_ptr as u64).to_le_bytes());
                    proxy.data[8..16].copy_from_slice(&(new_ptr as u64).to_le_bytes());
                    call_port(a, proxy)
                }
                (Some(_), Some(_)) => err_reply(-18), // EXDEV
                _                  => err_reply(-30), // EROFS
            }
        }
        // Exactly one side is tmpfs — different filesystems by construction.
        _ => err_reply(-18), // EXDEV
    }
}

fn handle_rmdir(pid: u32, path_ptr: usize) -> Message {
    mnt::name_gen_bump(); // mntiso-bindpin: bind pins re-verify after this
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];

    if cgroupfs::is_path(strip_trailing_slash(raw)) {
        let r = cgroupfs::rmdir(pid, strip_trailing_slash(raw));
        return if r < 0 { err_reply(r as i32) } else { ok_reply() };
    }

    if let Some(path) = tmpfs_path(raw) {
        if is_tmpfs_root(path) { return err_reply(-16); } // EBUSY — mount root
        let cred = cred_of(pid);
        let open_fds = tmp_open_fd_mask();
        let mut tmp = TMP_FILES.lock();
        let parent = match tmp_parent(path) { Some(p) => p, None => return err_reply(-16) };
        if let Err(e) = tmp_dir_may(&tmp, parent, &cred, xattr::MAY_EXEC) { return err_reply(e); }
        let idx = match tmp_find(&tmp, path) { Some(i) => i, None => return err_reply(-2) };
        if !tmp[idx].is_dir { return err_reply(-20); } // ENOTDIR
        if let Err(e) = tmp_may_delete(&tmp, parent, idx, &cred) { return err_reply(e); }
        if tmp_has_descendants(&tmp, path, idx) { return err_reply(-39); } // ENOTEMPTY
        // A directory still open keeps its slot, nameless, until the last
        // close (`tmp_release_ephemeral`): freeing it handed the slot, and
        // the open descriptor with it, to the next file created.
        if open_fds.has(idx) { tmp.make_nameless(idx); } else { tmp.release(idx); }
        return ok_reply();
    }
    if let Some(port) = find_mount_port(raw) {
        let mut proxy = Message::empty();
        proxy.tag = VFS_RMDIR;
        proxy.data[0..8].copy_from_slice(&(path_ptr as u64).to_le_bytes());
        return call_port(port, proxy);
    }
    for &dir in RAMFS_DIRS { if raw == dir { return err_reply(-16); } } // EBUSY
    err_reply(-30) // EROFS
}

// ── chmod/chown ──────────────────────────────────────────────────────────────
//
// tmpfs entries carry real per-file mode/uid/gid handled locally below.
// Mounted filesystems (currently f2fs) are routed to their mount server,
// which persists the change to the on-disk inode. RAMFS and device nodes
// have no mount port and no per-file storage, so they remain EROFS/EPERM
// rather than the previous silent no-op.

/// `follow` is false for the `AT_SYMLINK_NOFOLLOW` form. The tmpfs branch needs
/// no special handling — `path_args` has already resolved (or deliberately not
/// resolved) the path by the time we get here — but the mounted branch must
/// forward the distinction, since the server does its own lookup.
fn handle_chmod(pid: u32, path_ptr: usize, mode: u32, follow: bool) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];
    // tmpfs first: after pivot_root the F2FS mount's prefix is "/", so a
    // find_mount_port() probe here matched every /tmp path and turned every
    // chmod under /tmp into EROFS.
    if let Some(path) = tmpfs_path(raw) {
        let cred = cred_of(pid);
        // A tmpfs mount root is root-owned with a fixed mode: anyone else is
        // EPERM (not the owner), and root may only "set" the mode it has.
        // HOST root only (design idmap 2.3): never a namespace root.
        if is_tmpfs_root(path) {
            let cur = ramfs_dir_mode(path) & 0o7777;
            return if cred.host_root && mode & 0o7777 == cur { ok_reply() } else { err_reply(-1) };
        }
        let mut tmp = TMP_FILES.lock();
        return match tmp_find(&tmp, path) {
            Some(idx) => {
                let owner = tmp_owner(&tmp, idx);
                let om = tmp_meta(&tmp[owner]);
                if cred.euid != om.uid && !cred.cap_inode(xattr::CAP_FOWNER, om.uid, om.gid) { return err_reply(-1); } // EPERM
                let mode = xattr::chmod_filter_mode_c(&cred, &om, mode);
                tmp[owner].mode = mode & 0o7777;
                tmp_acl_chmod_sync(&mut tmp, owner, mode);
                tmp_touch_ctime(&mut tmp[owner]);
                ok_reply()
            }
            None => err_reply(-2), // ENOENT
        };
    }
    if let Some(n) = pts_number(raw) { return pts_chmod(pid, n, mode); }
    // Mounted filesystems (e.g. f2fs on / or /data) now persist mode changes
    // themselves — this used to be a blanket EROFS regardless of mount, which
    // is what made `chmod 640 /data/f` fail even though writes/mkdir/rm all
    // worked fine on the same volume.
    if let Some(port) = find_mount_port(raw) {
        let mut proxy = Message::empty();
        proxy.tag = if follow { VFS_CHMOD } else { VFS_LCHMOD };
        proxy.data[0..8].copy_from_slice(&(path_ptr as u64).to_le_bytes());
        proxy.data[8..16].copy_from_slice(&(mode as u64).to_le_bytes());
        return call_port(port, proxy);
    }
    err_reply(-30) // EROFS
}

/// Tag `pid`'s fd with the mount it was opened through (see `ofd::set_mnt`).
fn fd_note_mnt(pid: u32, fd: usize, ns: u16, mid: u32, mfl: u8) {
    let mut tbls = TblSlot::new();
    if let Some(tbl) = find_tbl(pid, &mut tbls) {
        if tbl.fds.used(fd) { ofd::set_mnt(tbl.fds[fd].ofd, ns, mid, mfl); }
    }
}

/// Mount flags recorded when `pid`'s fd was opened.
pub fn fd_mnt_flags(pid: u32, fd: usize) -> Option<u8> {
    let pid = sched::tgid_of(pid);
    let ofd = {
        let mut tbls = TblSlot::new();
        match find_tbl(pid, &mut tbls) {
            Some(tbl) if tbl.fds.used(fd) => tbl.fds[fd].ofd,
            _ => return None,
        }
    };
    ofd::mnt_flags_of(ofd)
}

/// The (mount namespace, mount id) `pid`'s fd was opened through, if recorded.
pub fn fd_mnt(pid: u32, fd: usize) -> Option<(u16, u32)> {
    let pid = sched::tgid_of(pid);
    let ofd = {
        let mut tbls = TblSlot::new();
        match find_tbl(pid, &mut tbls) {
            Some(tbl) if tbl.fds.used(fd) => tbl.fds[fd].ofd,
            _ => return None,
        }
    };
    ofd::mnt_of(ofd)
}

/// `(st_dev, st_ino)` out of a filled `struct stat` buffer (same offsets on both ABIs).
pub fn read_stat_id(stat_ptr: usize) -> (u64, u64) {
    unsafe { ((stat_ptr as *const u64).read_unaligned(), ((stat_ptr + 8) as *const u64).read_unaligned()) }
}

/// EROFS when `fd` was opened through a mount that is read-only now.
fn fd_mnt_ro(pid: u32, fd: usize) -> bool {
    let ofd = {
        let mut tbls = TblSlot::new();
        match find_tbl(pid, &mut tbls) {
            Some(tbl) if tbl.fds.used(fd) => tbl.fds[fd].ofd,
            _ => return false,
        }
    };
    match ofd::mnt_of(ofd) {
        Some((u16::MAX, mid)) => alias::flags_by_id(mid).or_else(|| ofd::mnt_flags_of(ofd)).map_or(false, |f| f & mnt::MNT_RDONLY != 0),
        Some((ns, mid)) => mnt::flags_of(ns, mid) & mnt::MNT_RDONLY != 0,
        None => false,
    }
}

fn handle_fchmod(pid: u32, fd: usize, mode: u32) -> Message {
    if fd_mnt_ro(pid, fd) { return err_reply(-30); }
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }
    match tbl.fds[fd].kind {
        VnodeKind::TmpFile { idx, .. } => {
            drop(tbls);
            let cred = cred_of(pid);
            let mut tmp = TMP_FILES.lock();
            let owner = tmp_owner(&tmp, idx);
            let om = tmp_meta(&tmp[owner]);
            if cred.euid != om.uid && !cred.cap_inode(xattr::CAP_FOWNER, om.uid, om.gid) { return err_reply(-1); } // EPERM
            let mode = xattr::chmod_filter_mode_c(&cred, &om, mode);
            tmp[owner].mode = mode & 0o7777;
            tmp_acl_chmod_sync(&mut tmp, owner, mode);
            tmp_touch_ctime(&mut tmp[owner]);
            ok_reply()
        }
        VnodeKind::MountedFile { port, file_id } => {
            drop(tbls);
            let mut proxy = Message::empty();
            proxy.tag = VFS_FCHMOD;
            proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&(mode as u64).to_le_bytes());
            call_port(port, proxy)
        }
        VnodeKind::Pty { pair, is_master: false } if tty_server::pty::vt_of(pair as usize) == 0 => {
            drop(tbls);
            pts_chmod(pid, pair as usize, mode)
        }
        _ => err_reply(-1), // EPERM — devices/RAMFS have fixed, root-owned modes
    }
}

/// `u32::MAX` for `uid`/`gid` means "leave unchanged" (mirrors chown(2)'s `-1`).
/// See `handle_chmod` for what `follow` does — `lchown(2)` is the common
/// caller of the false case.
fn handle_chown(pid: u32, path_ptr: usize, uid: u32, gid: u32, follow: bool) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];
    if cgroupfs::is_path(strip_trailing_slash(raw)) {
        let r = cgroupfs::chown(pid, strip_trailing_slash(raw), uid, gid);
        return if r < 0 { err_reply(r as i32) } else { ok_reply() };
    }
    if let Some(path) = tmpfs_path(raw) {
        let cred = cred_of(pid);
        // Mount roots: root:root and fixed (see handle_chmod).
        if is_tmpfs_root(path) {
            let same = (uid == u32::MAX || uid == 0) && (gid == u32::MAX || gid == 0);
            return if cred.host_root && same { ok_reply() } else { err_reply(-1) }; // host root only
        }
        let mut tmp = TMP_FILES.lock();
        return match tmp_find(&tmp, path) {
            Some(idx) => { let o = tmp_owner(&tmp, idx); apply_chown(&mut tmp[o], &cred, uid, gid) }
            None => err_reply(-2), // ENOENT
        };
    }
    if let Some(n) = pts_number(raw) { return pts_chown(pid, n, uid, gid); }
    // See handle_chmod: mounted filesystems now persist owner changes too.
    if let Some(port) = find_mount_port(raw) {
        let mut proxy = Message::empty();
        proxy.tag = if follow { VFS_CHOWN } else { VFS_LCHOWN };
        proxy.data[0..8].copy_from_slice(&(path_ptr as u64).to_le_bytes());
        proxy.data[8..16].copy_from_slice(&(uid as u64).to_le_bytes());
        proxy.data[16..24].copy_from_slice(&(gid as u64).to_le_bytes());
        return call_port(port, proxy);
    }
    err_reply(-30) // EROFS
}

fn handle_fchown(pid: u32, fd: usize, uid: u32, gid: u32) -> Message {
    if fd_mnt_ro(pid, fd) { return err_reply(-30); }
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
    if !tbl.fds.used(fd) { return err_reply(-9); }
    match tbl.fds[fd].kind {
        VnodeKind::TmpFile { idx, .. } => {
            drop(tbls);
            let cred = cred_of(pid);
            let mut tmp = TMP_FILES.lock();
            let o = tmp_owner(&tmp, idx);
            apply_chown(&mut tmp[o], &cred, uid, gid)
        }
        VnodeKind::MountedFile { port, file_id } => {
            drop(tbls);
            let mut proxy = Message::empty();
            proxy.tag = VFS_FCHOWN;
            proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&(uid as u64).to_le_bytes());
            proxy.data[16..24].copy_from_slice(&(gid as u64).to_le_bytes());
            call_port(port, proxy)
        }
        VnodeKind::Pty { pair, is_master: false } if tty_server::pty::vt_of(pair as usize) == 0 => {
            drop(tbls);
            pts_chown(pid, pair as usize, uid, gid)
        }
        _ => err_reply(-1),
    }
}

/// chown(2) rules, the same ones `chown_inode` applies on f2fs: only root may
/// change the owning uid (a non-root owner may restate its own); the owner may
/// change the gid only to a group it belongs to, which with no supplementary
/// groups means its egid; anyone else is EPERM. All-or-nothing.
fn apply_chown(e: &mut TmpFileEntry, cred: &xattr::Cred, uid: u32, gid: u32) -> Message {
    // Linux chown_ok/chgrp_ok: CAP_CHOWN over the inode (held in the caller's
    // user namespace, owner and group of the inode mapped there), else the
    // owner restating its uid and moving the group to one of its own.
    if !cred.cap_inode(xattr::CAP_CHOWN, e.uid, e.gid) {
        if cred.euid != e.uid { return err_reply(-1); }                // EPERM
        if uid != u32::MAX && uid != e.uid { return err_reply(-1); }
        if gid != u32::MAX && gid != e.gid && !cred.in_group(gid) { return err_reply(-1); }
    }
    if uid != u32::MAX { e.uid = uid; }
    if gid != u32::MAX { e.gid = gid; }
    tmp_touch_ctime(e);
    ok_reply()
}

// ── utimensat ────────────────────────────────────────────────────────────────

/// The utimensat(2) permission rule (Linux `utimes_common`): an explicit time
/// needs ownership or root (EPERM); "now" needs ownership, root, or write
/// permission on the inode (EACCES). Returns the errno to reply with, or 0.
fn utimens_permission(meta: &xattr::FileMeta, acl: Option<&[u8]>, cred: &xattr::Cred, explicit: bool) -> i32 {
    if cred.euid == meta.uid || cred.cap_inode(xattr::CAP_FOWNER, meta.uid, meta.gid) { return 0; }
    if explicit { return -1; } // EPERM
    if xattr::may_access(meta, cred, acl, xattr::MAY_WRITE) { 0 } else { -13 } // EACCES
}

/// Apply the decoded times to a tmpfs entry (`nsec == UTIME_OMIT` skips one).
fn tmp_apply_times(e: &mut TmpFileEntry, at: (i64, i64), mt: (i64, i64)) {
    if at.1 != UTIME_OMIT { e.atime = at; }
    if mt.1 != UTIME_OMIT { e.mtime = mt; }
    e.ctime = sched::clock_ts();
}

/// `VFS_UTIMENS` / `VFS_LUTIMENS` (path forms). `path_args` has already
/// resolved (or not) the final symlink for the tmpfs branch; the mounted
/// branch forwards the whole message so the server does its own lookup.
fn handle_utimens(pid: u32, msg: &Message, follow: bool) -> Message {
    let path_ptr = arg(msg, 0) as usize;
    let at = (arg(msg, 1) as i64, arg(msg, 2) as i64);
    let mt = (arg(msg, 3) as i64, arg(msg, 4) as i64);
    let explicit = arg(msg, 5) & UTIMENS_EXPLICIT != 0;
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];
    if let Some(path) = tmpfs_path(raw) {
        let cred = cred_of(pid);
        if is_tmpfs_root(path) { return if cred.host_root { ok_reply() } else { err_reply(-1) }; } // host root only
        let mut tmp = TMP_FILES.lock();
        let idx = match tmp_find(&tmp, path) { Some(i) => i, None => return err_reply(-2) };
        let owner = tmp_owner(&tmp, idx);
        let meta = tmp_meta(&tmp[owner]);
        let acl = xattr::find(&tmp.xattr(owner)[..], xattr::IDX_ACL_ACCESS, b"");
        let err = utimens_permission(&meta, acl, &cred, explicit);
        if err != 0 { return err_reply(err); }
        tmp_apply_times(&mut tmp[owner], at, mt);
        return ok_reply();
    }
    if let Some(port) = find_mount_port(raw) {
        let mut proxy = *msg;
        proxy.tag = if follow { VFS_UTIMENS } else { VFS_LUTIMENS };
        return call_port(port, proxy);
    }
    err_reply(-30) // EROFS
}

/// `VFS_FUTIMENS(fd, ...)` — the fd form; rewritten to the mount-local
/// file_id when proxied, exactly like fchmod.
fn handle_futimens(pid: u32, msg: &Message) -> Message {
    let fd = arg(msg, 0) as usize;
    let at = (arg(msg, 1) as i64, arg(msg, 2) as i64);
    let mt = (arg(msg, 3) as i64, arg(msg, 4) as i64);
    let explicit = arg(msg, 5) & UTIMENS_EXPLICIT != 0;
    if fd_mnt_ro(pid, fd) { return err_reply(-30); }
    let kind = {
        let mut tbls = TblSlot::new();
        let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
        if !tbl.fds.used(fd) { return err_reply(-9); }
        tbl.fds[fd].kind
    };
    match kind {
        VnodeKind::TmpFile { idx, .. } => {
            let cred = cred_of(pid);
            let mut tmp = TMP_FILES.lock();
            let owner = tmp_owner(&tmp, idx);
            let meta = tmp_meta(&tmp[owner]);
            let acl = xattr::find(&tmp.xattr(owner)[..], xattr::IDX_ACL_ACCESS, b"");
            let err = utimens_permission(&meta, acl, &cred, explicit);
            if err != 0 { return err_reply(err); }
            tmp_apply_times(&mut tmp[owner], at, mt);
            ok_reply()
        }
        VnodeKind::MountedFile { port, file_id } => {
            let mut proxy = *msg;
            proxy.tag = VFS_FUTIMENS;
            proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
            call_port(port, proxy)
        }
        _ => err_reply(-1), // EPERM — devices/RAMFS have no timestamps to set
    }
}

// ── extended attributes + POSIX ACLs ─────────────────────────────────────────
//
// The wire format, size caps, namespace permission gates, and the ACL
// evaluator all live in the `xattr` crate — this file only stores the per-inode
// arena (`TmpFileEntry::xattr`) and routes the thirteen ops to it. Every op
// resolves hard links through `tmp_owner()` first, so aliases share one set of
// attributes exactly as they share their bytes. Crate errors are POSITIVE
// errnos; reply with `-(e as i64)`.

/// S_IFMT type bits for a tmpfs entry, synthesised from its flags exactly as
/// `stat_common` does — the `mode` field itself carries permission bits only.
fn tmp_ifmt(e: &TmpFileEntry) -> u16 {
    if e.is_dir { 0o040000 }
    else if e.is_link { 0o120000 }
    else if e.is_sock { 0o140000 }
    else if e.is_fifo { 0o010000 }
    else if e.devtype == 1 { 0o020000 }
    else if e.devtype == 2 { 0o060000 }
    else { 0o100000 }
}

/// Build the `xattr::FileMeta` the permission gates and ACL evaluator expect:
/// `mode` carries the S_IFMT type bits together with the stored permission bits.
fn tmp_meta(e: &TmpFileEntry) -> xattr::FileMeta {
    xattr::FileMeta {
        mode: tmp_ifmt(e) | (e.mode as u16 & 0o7777),
        uid:  e.uid,
        gid:  e.gid,
    }
}

/// Does tmpfs file `idx` carry a set-ID bit a write would strip? The cheap
/// pre-check that spares the common write the caller's fsuid lookup.
fn tmp_has_setid(tmp: &[TmpFileEntry], idx: usize) -> bool {
    xattr::setid_kill_bits(tmp_meta(&tmp[tmp_owner(tmp, idx)]).mode, u32::MAX) != 0
}

/// Strip the set-ID bits of tmpfs file `idx` as a write, truncate or O_TRUNC
/// open by `fsuid` requires (`xattr::setid_kill_bits`, Linux
/// `file_remove_privs`).
fn tmp_kill_setid(tmp: &mut [TmpFileEntry], idx: usize, cred: &xattr::Cred) {
    let o = tmp_owner(tmp, idx);
    let kill = xattr::setid_kill_bits_c(&tmp_meta(&tmp[o]), cred);
    if kill != 0 {
        tmp[o].mode &= !(kill as u32);
        tmp_touch_ctime(&mut tmp[o]);
    }
}

/// The caller's credentials as every permission gate consumes them: the
/// FILESYSTEM uid/gid (Linux fsuid/fsgid, equal to the effective ids unless
/// setfsuid/setfsgid moved them) plus the supplementary groups. `Cred::euid`
/// therefore holds the fsuid. This is the ONLY constructor of an
/// `xattr::Cred` from a pid in the tree; `sched::fsids_of` answers 0 for a pid
/// it cannot find (the boot-time mount path), so an unknown caller is root,
/// exactly as before.
pub fn cred_of(pid: u32) -> xattr::Cred {
    if let Some(c) = ovl::cred_override(pid) { return c; }
    let (fsuid, fsgid) = sched::fsids_of(pid);
    let mut c = xattr::Cred::new(fsuid, fsgid);
    c.ngroups = sched::groups_of(pid, &mut c.groups) as u8;
    cred_with_ns(c, pid, false)
}

/// Effective uid 0 in the INITIAL user namespace: the "host root only" gates
/// (trusted.*, device and sysclass nodes, tmpfs mount roots). A namespace
/// root (kuid != 0) and a kuid-0 task that left namespace 0 are both false.
fn host_root_of(pid: u32) -> bool {
    sched::euid_of(pid) == 0 && sched::ns::userns_of(pid) == 0
}

/// Attach the caller's user-namespace view and effective capabilities to
/// `c`. Namespace 0 (every task today without a userns) keeps the identity
/// view, so `host_root` is exactly the old `euid == 0`. `real` = the
/// `access(2)` flavour: the capabilities follow the REAL uid (Linux
/// `access_override_creds`): effective if real == effective uid, else the
/// permitted set when the real uid is the namespace root, else none.
fn cred_with_ns(c: xattr::Cred, pid: u32, real: bool) -> xattr::Cred {
    let ns = sched::ns::userns_of(pid);
    // Unknown pid (boot-time mount path): keep the kernel-private `Cred::new`.
    if ns == 0 && sched::caps::caps_of(pid).is_none() { return c; }
    let v = if ns == 0 { xattr::IdView::HOST } else {
        let sv = sched::ns::view_of_ns(ns);
        xattr::IdView { ns: sv.ns, n: sv.n, ext: sv.ext }
    };
    let caps = match sched::caps::caps_of(pid) {
        Some(cs) if !real || sched::euid_of(pid) == c.euid => cs[0],
        Some(cs) if sched::ns::root_k(ns) == Some(c.euid) => cs[1],
        _ => 0,
    };
    c.with_ns(v, caps)
}

/// The caller's credentials built from the REAL uid/gid rather than the
/// effective ones. POSIX `access()`/`faccessat()` (without `AT_EACCESS`) ask
/// "could the real user do this", specifically so a setuid-root program can
/// check what its invoker — not itself — is allowed to do. Same supplementary
/// group list either way: this kernel has no setuid-triggered group swap.
pub fn real_cred_of(pid: u32) -> xattr::Cred {
    let mut c = xattr::Cred::new(sched::ruid_of(pid), sched::rgid_of(pid));
    c.ngroups = sched::groups_of(pid, &mut c.groups) as u8;
    cred_with_ns(c, pid, true)
}

/// Stamp all three timestamps with "now" — a freshly created entry.
fn tmp_stamp_new(e: &mut TmpFileEntry) {
    let now = sched::clock_ts();
    e.atime = now; e.mtime = now; e.ctime = now;
}
/// Data changed: mtime and ctime.
fn tmp_touch_mtime(e: &mut TmpFileEntry) {
    let now = sched::clock_ts();
    e.mtime = now; e.ctime = now;
}
/// Metadata changed: ctime.
fn tmp_touch_ctime(e: &mut TmpFileEntry) {
    e.ctime = sched::clock_ts();
}

/// Linux's `relatime` mount default (what `/proc/mounts` already claims for
/// this filesystem, per the `rw,relatime` line below — before this, that was
/// aspirational, since nothing ever touched atime on a read at all): skip the
/// update unless the file's mtime or ctime is at or past the current atime,
/// or the current atime is at least a day stale. Keeps a read from dirtying
/// the entry (and, on f2fs, the inode block) on every call.
const RELATIME_INTERVAL_SEC: i64 = 86_400;
fn relatime_needs_update(atime: (i64, i64), mtime: (i64, i64), ctime: (i64, i64), now: (i64, i64)) -> bool {
    if atime.0 < mtime.0 || (atime.0 == mtime.0 && atime.1 <= mtime.1) { return true; }
    if atime.0 < ctime.0 || (atime.0 == ctime.0 && atime.1 <= ctime.1) { return true; }
    now.0.saturating_sub(atime.0) >= RELATIME_INTERVAL_SEC
}

/// Initialise the permission bits, inherited ACLs and timestamps of the
/// just-allocated entry `idx` at `path` — Linux `posix_acl_create`: if the
/// parent directory carries a default ACL, the child's mode comes from it
/// (umask ignored), a non-trivial result is stored as the child's access ACL,
/// and a child directory also inherits the default ACL itself. Otherwise the
/// mode is `mode & !umask`. `mode` is the caller's requested permission bits.
fn tmp_init_created(tmp: &mut TmpPool, idx: usize, path: &[u8], mode: u16, umask: u16, is_dir: bool) {
    // The parent's default ACL, copied out first: the parent and the child
    // live in the same pool slice.
    let mut dbuf = [0u8; xattr::ACL_INHERIT_MAX];
    let mut dlen = 0usize;
    if let Some(parent) = tmp_parent(path) {
        if !is_tmpfs_root(parent) {
            if let Some(pi) = tmp_find(tmp, parent) {
                if let Some(v) = xattr::find(&tmp.xattr(pi)[..], xattr::IDX_ACL_DEFAULT, b"") {
                    if v.len() <= dbuf.len() { dbuf[..v.len()].copy_from_slice(v); dlen = v.len(); }
                }
            }
        }
    }
    let dacl = if dlen > 0 { Some(&dbuf[..dlen]) } else { None };
    let c = xattr::acl_create(dacl, mode, umask, is_dir);
    tmp[idx].mode = c.mode as u32;
    // The xattr arena is allocated only when there is an ACL to store.
    if c.access_len > 0 {
        let _ = xattr::set(&mut tmp.xattr_mut(idx)[..], xattr::IDX_ACL_ACCESS, b"", &c.access[..c.access_len], 0);
    }
    if c.inherit_default && dlen > 0 {
        let _ = xattr::set(&mut tmp.xattr_mut(idx)[..], xattr::IDX_ACL_DEFAULT, b"", &dbuf[..dlen], 0);
    }
    tmp_stamp_new(&mut tmp[idx]);
}

/// posix_acl_chmod: after a mode change, rewrite any stored *access* ACL so its
/// USER_OBJ / mask-or-GROUP_OBJ / OTHER entries track the new owner/group/other
/// bits. Absent (or trivial, hence unstored) ACLs make this a no-op.
fn tmp_acl_chmod_sync(tmp: &mut TmpPool, owner: usize, new_mode: u32) {
    // Read through the shared view first: a slot with no arena (no ACL) must
    // not get one allocated by a plain chmod.
    let mut buf = [0u8; 256];
    let len = match xattr::find(&tmp.xattr(owner)[..], xattr::IDX_ACL_ACCESS, b"") {
        Some(v) if v.len() <= buf.len() => { buf[..v.len()].copy_from_slice(v); v.len() }
        _ => return,
    };
    let xa = tmp.xattr_mut(owner);
    xattr::acl_chmod_rewrite(&mut buf[..len], new_mode as u16);
    let _ = xattr::set(&mut xa[..], xattr::IDX_ACL_ACCESS, b"", &buf[..len], 0);
}

/// Build a proxy message forwarding an xattr op to a mount server verbatim.
/// Unused trailing args are zero — harmless for the shorter ops.
fn xattr_proxy(port: u32, tag: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> Message {
    let mut proxy = Message::empty();
    proxy.tag = tag;
    proxy.data[0..8].copy_from_slice(&a0.to_le_bytes());
    proxy.data[8..16].copy_from_slice(&a1.to_le_bytes());
    proxy.data[16..24].copy_from_slice(&a2.to_le_bytes());
    proxy.data[24..32].copy_from_slice(&a3.to_le_bytes());
    proxy.data[32..40].copy_from_slice(&a4.to_le_bytes());
    call_port(port, proxy)
}

// ── local tmpfs operations (all inside one TMP_FILES lock) ────────────────────

fn tmp_setxattr_local(tmp: &mut TmpPool, owner: usize, cred: &xattr::Cred,
                      name_ptr: usize, val_ptr: usize, size: usize, flags: u32) -> Message {
    let (e, xa) = tmp.entry_xattr_mut(owner);
    let (nbuf, nlen) = match read_cstr_raw(name_ptr) { Some(r) => r, None => return err_reply(-14) };
    if nlen == 0 || nlen > xattr::XATTR_NAME_MAX { return err_reply(-xattr::ERANGE); }
    let (idx, suf) = match xattr::split_name(&nbuf[..nlen]) {
        Some(v) => v, None => return err_reply(-xattr::EOPNOTSUPP),
    };
    let meta = tmp_meta(e);
    let acl = xattr::find(&xa[..], xattr::IDX_ACL_ACCESS, b"");
    if let Err(er) = xattr::may_write_xattr(idx, &meta, cred, acl) {
        return err_reply(-er);
    }
    // The ACL namespaces keep the inode mode in lock-step with their permission
    // bits (posix_acl semantics); user.*/trusted.* are opaque blobs.
    if idx == xattr::IDX_ACL_ACCESS || idx == xattr::IDX_ACL_DEFAULT {
        // An empty value clears the ACL (setfacl -b/-k). Absence is not an
        // error — the requested end state ("no such ACL") already holds.
        if size == 0 {
            let _ = xattr::remove(&mut xa[..], idx, suf);
            return ok_reply();
        }
        let val = unsafe { core::slice::from_raw_parts(val_ptr as *const u8, size) };
        let mut kbuf = [0u8; xattr::ACL_INHERIT_MAX];
        let val = match xattr::acl_validate(val) {
            Ok(_) => match xattr::acl_to_kernel(&cred.view, val, &mut kbuf) {
                Ok(n) => &kbuf[..n],
                Err(er) => return err_reply(-er),
            },
            Err(_) => return err_reply(-xattr::EINVAL),
        };
        let summary = match xattr::acl_validate(val) {
            Ok(s) => s, Err(_) => return err_reply(-xattr::EINVAL),
        };
        if idx == xattr::IDX_ACL_ACCESS {
            let bits = xattr::acl_mode_bits(&summary) as u32 & 0o777;
            if xattr::acl_is_trivial(&summary) {
                // Fully representable as mode bits: fold into the mode and store
                // nothing (Linux drops a trivial access ACL).
                e.mode = (e.mode & !0o777) | bits;
                let _ = xattr::remove(&mut xa[..], xattr::IDX_ACL_ACCESS, b"");
                return ok_reply();
            }
            if let Err(er) = xattr::set(&mut xa[..], idx, suf, val, flags) {
                return err_reply(-er);
            }
            e.mode = (e.mode & !0o777) | bits;
            return ok_reply();
        }
        // IDX_ACL_DEFAULT: the dir-only guard already ran in may_write_xattr.
        return match xattr::set(&mut xa[..], idx, suf, val, flags) {
            Ok(_) => ok_reply(), Err(er) => err_reply(-er),
        };
    }
    let val: &[u8] = if size == 0 { &[] }
                     else { unsafe { core::slice::from_raw_parts(val_ptr as *const u8, size) } };
    match xattr::set(&mut xa[..], idx, suf, val, flags) {
        Ok(_) => ok_reply(), Err(er) => err_reply(-er),
    }
}

fn tmp_getxattr_local(tmp: &TmpPool, owner: usize, cred: &xattr::Cred,
                      name_ptr: usize, val_ptr: usize, size: usize) -> Message {
    let (e, xa) = (&tmp[owner], tmp.xattr(owner));
    let (nbuf, nlen) = match read_cstr_raw(name_ptr) { Some(r) => r, None => return err_reply(-14) };
    if nlen == 0 || nlen > xattr::XATTR_NAME_MAX { return err_reply(-xattr::ERANGE); }
    let (idx, suf) = match xattr::split_name(&nbuf[..nlen]) {
        Some(v) => v, None => return err_reply(-xattr::EOPNOTSUPP),
    };
    let meta = tmp_meta(e);
    let acl = xattr::find(&xa[..], xattr::IDX_ACL_ACCESS, b"");
    if let Err(er) = xattr::may_read_xattr(idx, &meta, cred, acl) {
        return err_reply(-er);
    }
    let value = match xattr::find(&xa[..], idx, suf) {
        Some(v) => v, None => return err_reply(-xattr::ENODATA),
    };
    let mut kbuf = [0u8; xattr::ACL_INHERIT_MAX];
    let value: &[u8] = if (idx == xattr::IDX_ACL_ACCESS || idx == xattr::IDX_ACL_DEFAULT)
        && cred.view.ns != 0 {
        match xattr::acl_from_kernel(&cred.view, value, &mut kbuf) {
            Ok(n) => &kbuf[..n],
            Err(er) => return err_reply(-er),
        }
    } else { value };
    let len = value.len();
    if size == 0 { return val_reply(len as u64); }
    if size < len { return err_reply(-xattr::ERANGE); }
    unsafe { core::ptr::copy_nonoverlapping(value.as_ptr(), val_ptr as *mut u8, len); }
    val_reply(len as u64)
}

fn tmp_listxattr_local(tmp: &TmpPool, owner: usize, host_root: bool, list_ptr: usize, size: usize) -> Message {
    let xa = tmp.xattr(owner);
    // O(1) fast path — the `ls -l` stat storm hits this on every entry.
    if xattr::is_empty(&xa[..]) { return val_reply(0); }
    let out: &mut [u8] = if size == 0 { &mut [] }
                         else { unsafe { core::slice::from_raw_parts_mut(list_ptr as *mut u8, size) } };
    // trusted.* is host root only
    match xattr::list(&xa[..], out, host_root) {
        Ok(n) => val_reply(n as u64), Err(er) => err_reply(-er),
    }
}

fn tmp_removexattr_local(tmp: &mut TmpPool, owner: usize, cred: &xattr::Cred, name_ptr: usize) -> Message {
    let (e, xa) = tmp.entry_xattr_mut(owner);
    let (nbuf, nlen) = match read_cstr_raw(name_ptr) { Some(r) => r, None => return err_reply(-14) };
    if nlen == 0 || nlen > xattr::XATTR_NAME_MAX { return err_reply(-xattr::ERANGE); }
    let (idx, suf) = match xattr::split_name(&nbuf[..nlen]) {
        Some(v) => v, None => return err_reply(-xattr::EOPNOTSUPP),
    };
    let meta = tmp_meta(e);
    let acl = xattr::find(&xa[..], xattr::IDX_ACL_ACCESS, b"");
    if let Err(er) = xattr::may_write_xattr(idx, &meta, cred, acl) {
        return err_reply(-er);
    }
    // ACL removal needs no mode resync — the mode stays as it is (Linux).
    match xattr::remove(&mut xa[..], idx, suf) {
        Ok(_) => ok_reply(), Err(er) => err_reply(-er),
    }
}

// ── path forms (arg0 is a path; l-forms already resolved by path_args) ────────

fn handle_setxattr(pid: u32, tag: u64, path_ptr: usize, name_ptr: usize,
                   val_ptr: usize, size: usize, flags: u32) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];
    if let Some(path) = tmpfs_path(raw) {
        let cred = cred_of(pid);
        let mut tmp = TMP_FILES.lock();
        let idx = match tmp_find(&tmp, path) { Some(i) => i, None => return err_reply(-2) };
        let owner = tmp_owner(&tmp, idx);
        return tmp_setxattr_local(&mut tmp, owner, &cred, name_ptr, val_ptr, size, flags);
    }
    if let Some(port) = find_mount_port(raw) {
        return xattr_proxy(port, tag, path_ptr as u64, name_ptr as u64,
                           val_ptr as u64, size as u64, flags as u64);
    }
    err_reply(-95) // EOPNOTSUPP
}

fn handle_getxattr(pid: u32, tag: u64, path_ptr: usize, name_ptr: usize,
                   val_ptr: usize, size: usize) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];
    if let Some(path) = tmpfs_path(raw) {
        let cred = cred_of(pid);
        let tmp = TMP_FILES.lock();
        let idx = match tmp_find(&tmp, path) { Some(i) => i, None => return err_reply(-2) };
        let owner = tmp_owner(&tmp, idx);
        return tmp_getxattr_local(&tmp, owner, &cred, name_ptr, val_ptr, size);
    }
    if let Some(port) = find_mount_port(raw) {
        return xattr_proxy(port, tag, path_ptr as u64, name_ptr as u64, val_ptr as u64, size as u64, 0);
    }
    err_reply(-95)
}

fn handle_listxattr(pid: u32, tag: u64, path_ptr: usize, list_ptr: usize, size: usize) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];
    if let Some(path) = tmpfs_path(raw) {
        let euid = sched::caps::host_capable_of(pid, sched::caps::CAP_SYS_ADMIN);
        let tmp = TMP_FILES.lock();
        let idx = match tmp_find(&tmp, path) { Some(i) => i, None => return err_reply(-2) };
        let owner = tmp_owner(&tmp, idx);
        return tmp_listxattr_local(&tmp, owner, euid, list_ptr, size);
    }
    if let Some(port) = find_mount_port(raw) {
        return xattr_proxy(port, tag, path_ptr as u64, list_ptr as u64, size as u64, 0, 0);
    }
    err_reply(-95)
}

fn handle_removexattr(pid: u32, tag: u64, path_ptr: usize, name_ptr: usize) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];
    if let Some(path) = tmpfs_path(raw) {
        let cred = cred_of(pid);
        let mut tmp = TMP_FILES.lock();
        let idx = match tmp_find(&tmp, path) { Some(i) => i, None => return err_reply(-2) };
        let owner = tmp_owner(&tmp, idx);
        return tmp_removexattr_local(&mut tmp, owner, &cred, name_ptr);
    }
    if let Some(port) = find_mount_port(raw) {
        return xattr_proxy(port, tag, path_ptr as u64, name_ptr as u64, 0, 0, 0);
    }
    err_reply(-95)
}

/// faccessat(2): permission probe honoring any stored access ACL. `amode==0`
/// (F_OK) is pure existence. A path this server does not own answers -38 so the
/// kernel's legacy fallback runs.
/// `eaccess`: use the caller's effective ids (Linux `AT_EACCESS`) instead of
/// the real ids `access(2)`/plain `faccessat(2)` ask with. Real ids are the
/// POSIX default — the whole point of `access()` is answering "could the
/// real (often unprivileged) user do this", not "can I, right now".
/// Owner and mode of a block device node (and /dev/loop-control): root:disk
/// (gid 6) 0660, the same values fstat/stat report for it.
fn blockdev_meta(dev: u16) -> xattr::FileMeta {
    const S_IFBLK: u16 = 0o060000;
    const S_IFCHR: u16 = 0o020000;
    let ifmt = if block::dev_is_char(dev) { S_IFCHR } else { S_IFBLK };
    xattr::FileMeta { mode: ifmt | 0o660, uid: 0, gid: 6 }
}

/// The access an open(2) with `flags` asks for (O_ACCMODE).
fn open_mask(flags: u32) -> u8 {
    match flags & 3 {
        0 => xattr::MAY_READ,
        1 => xattr::MAY_WRITE,
        _ => xattr::MAY_READ | xattr::MAY_WRITE,
    }
}

const GID_TTY: u32 = 5;    // /etc/group, Debian's number: /dev/pts/N
const GID_AUDIO: u32 = 29;  // /etc/group, Debian's number
const GID_VIDEO: u32 = 44;  // /etc/group, Debian's number
const GID_INPUT: u32 = 104; // /etc/group, Debian's number
/// `netdev`: may open /dev/netctl for writing, i.e. manage the network
/// (docs/servers/netctl.md). Same number as `netctl_abi::GID_NETDEV`.
const GID_NETDEV: u32 = 108;

/// The ALSA character major. The audio server's /dev/pipewire is the one
/// node under it (116:16, pcmC0D0p's number); like /dev/snd/* it is
/// root:audio 0660.
pub const SOUND_MAJOR: u32 = 116;
/// /dev/hci0 (Linux's misc vhci number; one raw HCI controller).
pub const HCI_MAJOR: u32 = 10;
pub const HCI_MINOR: u32 = 137;

/// Owner/mode of a dynamic character device by its major/minor, following
/// udev's defaults: evdev (13) root:input 0660 — anyone who can open it reads
/// every keystroke; the DRM primary node (226, minor < 128) root:video 0660 —
/// modesetting, scanout; sound (116) root:audio 0660 — playback, and the
/// device is single-writer, so an open holds it for the whole system; render
/// nodes and everything else 0666.
fn dyn_dev_meta(major: u32, minor: u32) -> xattr::FileMeta {
    const S_IFCHR: u16 = 0o020000;
    match (major, minor) {
        (13, _) => xattr::FileMeta { mode: S_IFCHR | 0o660, uid: 0, gid: GID_INPUT },
        // /dev/uinput: root only (Linux default) - it can forge any input.
        (10, 223) => xattr::FileMeta { mode: S_IFCHR | 0o600, uid: 0, gid: 0 },
        (SOUND_MAJOR, _) => xattr::FileMeta { mode: S_IFCHR | 0o660, uid: 0, gid: GID_AUDIO },
        (226, m) if m < 128 => xattr::FileMeta { mode: S_IFCHR | 0o660, uid: 0, gid: GID_VIDEO },
        // /dev/video* (V4L2 capture): root:video 0660, as udev does.
        (81, _) => xattr::FileMeta { mode: S_IFCHR | 0o660, uid: 0, gid: GID_VIDEO },
        // /dev/netctl: everyone reads state, root and netdev may manage. The
        // net server records per open whether it was opened for writing and
        // refuses mutating ioctls on a read-only open (EPERM).
        (NETCTL_MAJOR, NETCTL_MINOR) => xattr::FileMeta { mode: S_IFCHR | 0o664, uid: 0, gid: GID_NETDEV },
        // /dev/hci0: raw HCI, only btd (root) opens it (docs/servers/bluetooth.md A).
        (HCI_MAJOR, HCI_MINOR) => xattr::FileMeta { mode: S_IFCHR | 0o600, uid: 0, gid: 0 },
        _ => xattr::FileMeta { mode: S_IFCHR | 0o666, uid: 0, gid: 0 },
    }
}

/// Whether `pid` (effective ids + groups) may write a dynamic device node
/// with this major/minor — the check open(2) makes, for a server whose
/// clients reach it without an open (the audio server's legacy IPC port).
pub fn dyn_dev_may_write(major: u32, minor: u32, pid: u32) -> bool {
    xattr::may_access(&dyn_dev_meta(major, minor), &cred_of(pid), None, xattr::MAY_WRITE)
}

/// [`dyn_dev_meta`] from a dev_t in [`makedev`]'s encoding.
fn dyn_dev_meta_rdev(rdev: u64) -> xattr::FileMeta {
    dyn_dev_meta(((rdev >> 8) & 0xfff) as u32, ((rdev & 0xff) | ((rdev >> 12) & !0xff)) as u32)
}

/// /dev/fb0: root:video 0660 (writing it paints over whoever owns the screen).
fn fb_meta() -> xattr::FileMeta {
    xattr::FileMeta { mode: 0o020000 | 0o660, uid: 0, gid: GID_VIDEO }
}

fn handle_access(pid: u32, path_ptr: usize, amode: u32, eaccess: bool) -> Message {
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let raw = &pbuf[..plen];
    if cgroupfs::is_path(strip_trailing_slash(raw)) { return err_reply(-38); } // stat-based fallback
    if let Some(path) = tmpfs_path(raw) {
        let cred = if eaccess { cred_of(pid) } else { real_cred_of(pid) };
        let tmp = TMP_FILES.lock();
        // Not a pool entry (e.g. "/tmp" itself, a ramfs dir): -38 so the
        // kernel's stat-based fallback answers, exactly as before VFS_ACCESS
        // existed. A genuinely missing file still ends up ENOENT there.
        let idx = match tmp_find(&tmp, path) { Some(i) => i, None => return err_reply(-38) };
        let owner = tmp_owner(&tmp, idx);
        if amode == 0 { return ok_reply(); } // F_OK: existence only
        let (e, xa) = (&tmp[owner], tmp.xattr(owner));
        let meta = tmp_meta(e);
        let acl = xattr::find(&xa[..], xattr::IDX_ACL_ACCESS, b"");
        let ok = xattr::access_check(&meta, &cred, acl,
                                     amode & 4 != 0, amode & 2 != 0, amode & 1 != 0);
        return if ok { ok_reply() } else { err_reply(-13) }; // EACCES
    }
    // Synthesized block device nodes live under the root mount's prefix but
    // not on it: answer from the mode stat reports (see blockdev_meta).
    // The same goes for the registered character devices and /dev/fb0.
    let dev_meta = if let Some(dev) = block::lookup_dev_node(raw) {
        Some(blockdev_meta(dev))
    } else if raw == b"/dev/fb0" {
        Some(fb_meta())
    } else {
        let devices = DYNAMIC_DEVICES.lock();
        devices.iter()
            .find(|d| d.in_use && d.path.as_bytes() == raw)
            .map(|d| dyn_dev_meta(d.major, d.minor))
    };
    if let Some(meta) = dev_meta {
        if amode == 0 { return ok_reply(); }
        let cred = if eaccess { cred_of(pid) } else { real_cred_of(pid) };
        let ok = xattr::access_check(&meta, &cred, None,
                                     amode & 4 != 0, amode & 2 != 0, amode & 1 != 0);
        return if ok { ok_reply() } else { err_reply(-13) };
    }
    // procfs is synthesized here, not stored on the root mount: proxying
    // would ask the root filesystem, which answers ENOENT, so `[ -r
    // /proc/meminfo ]` failed while `cat` worked. The kernel's stat-based
    // fallback answers from the mode stat reports.
    if raw == b"/proc" || raw.starts_with(b"/proc/")
        // /etc/resolv.conf is generated too (DHCP lease), absent on the root
        // mount: inotify_add_watch (aardvark-dns) needs access to say yes.
        || raw == b"/etc/resolv.conf" {
        return err_reply(-38);
    }
    if let Some(port) = find_mount_port(raw) {
        return xattr_proxy(port, VFS_ACCESS, path_ptr as u64, amode as u64, eaccess as u64, 0, 0);
    }
    err_reply(-38) // ENOSYS — let the kernel's legacy access() fallback run
}

// ── fd forms (arg0 is an fd; rewritten to the mount-local file_id when proxied) ─
//
// Copy the vnode kind out under the FD_TABLES lock and drop it before touching
// TMP_FILES — the fixed FD_TABLES → TMP_FILES ordering every other handler uses.

fn handle_fsetxattr(pid: u32, fd: usize, name_ptr: usize,
                    val_ptr: usize, size: usize, flags: u32) -> Message {
    let kind = {
        let mut tbls = TblSlot::new();
        let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
        if !tbl.fds.used(fd) { return err_reply(-9); }
        tbl.fds[fd].kind
    };
    match kind {
        VnodeKind::TmpFile { idx, .. } => {
            let cred = cred_of(pid);
            let mut tmp = TMP_FILES.lock();
            let owner = tmp_owner(&tmp, idx);
            tmp_setxattr_local(&mut tmp, owner, &cred, name_ptr, val_ptr, size, flags)
        }
        VnodeKind::MountedFile { port, file_id } =>
            xattr_proxy(port, VFS_FSETXATTR, file_id as u64, name_ptr as u64,
                        val_ptr as u64, size as u64, flags as u64),
        _ => err_reply(-95),
    }
}

fn handle_fgetxattr(pid: u32, fd: usize, name_ptr: usize, val_ptr: usize, size: usize) -> Message {
    let kind = {
        let mut tbls = TblSlot::new();
        let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
        if !tbl.fds.used(fd) { return err_reply(-9); }
        tbl.fds[fd].kind
    };
    match kind {
        VnodeKind::TmpFile { idx, .. } => {
            let cred = cred_of(pid);
            let tmp = TMP_FILES.lock();
            let owner = tmp_owner(&tmp, idx);
            tmp_getxattr_local(&tmp, owner, &cred, name_ptr, val_ptr, size)
        }
        VnodeKind::MountedFile { port, file_id } =>
            xattr_proxy(port, VFS_FGETXATTR, file_id as u64, name_ptr as u64, val_ptr as u64, size as u64, 0),
        _ => err_reply(-95),
    }
}

fn handle_flistxattr(pid: u32, fd: usize, list_ptr: usize, size: usize) -> Message {
    let kind = {
        let mut tbls = TblSlot::new();
        let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
        if !tbl.fds.used(fd) { return err_reply(-9); }
        tbl.fds[fd].kind
    };
    match kind {
        VnodeKind::TmpFile { idx, .. } => {
            let euid = sched::caps::host_capable_of(pid, sched::caps::CAP_SYS_ADMIN);
            let tmp = TMP_FILES.lock();
            let owner = tmp_owner(&tmp, idx);
            tmp_listxattr_local(&tmp, owner, euid, list_ptr, size)
        }
        VnodeKind::MountedFile { port, file_id } =>
            xattr_proxy(port, VFS_FLISTXATTR, file_id as u64, list_ptr as u64, size as u64, 0, 0),
        _ => err_reply(-95),
    }
}

fn handle_fremovexattr(pid: u32, fd: usize, name_ptr: usize) -> Message {
    let kind = {
        let mut tbls = TblSlot::new();
        let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
        if !tbl.fds.used(fd) { return err_reply(-9); }
        tbl.fds[fd].kind
    };
    match kind {
        VnodeKind::TmpFile { idx, .. } => {
            let cred = cred_of(pid);
            let mut tmp = TMP_FILES.lock();
            let owner = tmp_owner(&tmp, idx);
            tmp_removexattr_local(&mut tmp, owner, &cred, name_ptr)
        }
        VnodeKind::MountedFile { port, file_id } =>
            xattr_proxy(port, VFS_FREMOVEXATTR, file_id as u64, name_ptr as u64, 0, 0, 0),
        _ => err_reply(-95),
    }
}

// ── stat(2) support ──────────────────────────────────────────────────────────

// `struct stat` is NOT the same shape on both targets we build for, and
// getting it wrong is worse than an ABI nit: the caller's buffer is only
// ever as large as its *own* platform's definition, so writing the x86-64
// 144-byte form into an AArch64 128-byte `struct stat` local overruns it
// by 16 bytes and stomps whatever the compiler parked after it (often the
// saved FP/LR). Every stat producer in the tree must go through the
// helpers below rather than open-coding offsets.
//
// x86-64 (arch-specific layout, 144 bytes):
//    0: st_dev  (u64)     8: st_ino  (u64)    16: st_nlink (u64)
//   24: st_mode (u32)    28: st_uid  (u32)    32: st_gid   (u32)
//   36: __pad0  (u32)    40: st_rdev (u64)    48: st_size  (i64)
//   56: st_blksize (i64) 64: st_blocks (i64)  72..144: timestamps
//
// AArch64 (asm-generic layout, 128 bytes) — note st_mode and st_nlink are
// both u32 and swap places relative to x86-64, which is exactly the pair
// of fields that decides "is this executable":
//    0: st_dev  (u64)     8: st_ino  (u64)    16: st_mode  (u32)
//   20: st_nlink (u32)   24: st_uid  (u32)    28: st_gid   (u32)
//   32: st_rdev (u64)    40: __pad1  (u64)    48: st_size  (i64)
//   56: st_blksize (i32) 60: __pad2  (i32)    64: st_blocks (i64)
//   72..128: timestamps
#[cfg(target_arch = "x86_64")]
pub const STAT_SIZE: usize = 144;
#[cfg(target_arch = "aarch64")]
pub const STAT_SIZE: usize = 128;

/// Byte offset of `st_mode` within `struct stat` for the target ABI.
#[cfg(target_arch = "x86_64")]
pub const ST_MODE_OFF: usize = 24;
#[cfg(target_arch = "aarch64")]
pub const ST_MODE_OFF: usize = 16;

/// Read `st_mode` back out of a filled `struct stat` buffer.
pub fn read_stat_mode(stat_ptr: usize) -> u32 {
    unsafe { ((stat_ptr + ST_MODE_OFF) as *const u32).read_unaligned() }
}

/// Byte offset of `st_uid` (`st_gid` follows it) for the target ABI.
#[cfg(target_arch = "x86_64")]
pub const ST_UID_OFF: usize = 28;
#[cfg(target_arch = "aarch64")]
pub const ST_UID_OFF: usize = 24;

/// Read `(st_uid, st_gid)` back out of a filled `struct stat` buffer.
pub fn read_stat_owner(stat_ptr: usize) -> (u32, u32) {
    unsafe {
        (((stat_ptr + ST_UID_OFF) as *const u32).read_unaligned(),
         ((stat_ptr + ST_UID_OFF + 4) as *const u32).read_unaligned())
    }
}

/// Rewrite `(st_uid, st_gid)` of a kernel-resident `struct stat` from kuids to
/// the calling task's user-namespace view (65534 unmapped). Called on the way
/// to user memory: kernel buffers carry kuids (idmap design H1).
pub fn translate_stat_owner_for_user(stat_ptr: usize) {
    let (u, g) = read_stat_owner(stat_ptr);
    unsafe {
        ((stat_ptr + ST_UID_OFF) as *mut u32).write_unaligned(sched::ns::view_id(0, u));
        ((stat_ptr + ST_UID_OFF + 4) as *mut u32).write_unaligned(sched::ns::view_id(1, g));
    }
}

/// Synthesise a stable, path-unique inode number.
///
/// tmpfs and the initrd have no real inode numbers, so `st_ino` used to be
/// derived either from the path *length* (`plen + 10000` / `plen + 20000`) or
/// from the caller's path pointer. Both collide constantly: every /tmp file
/// whose path happened to be the same length reported the same inode, and
/// uutils `cp` compares `(st_dev, st_ino)` to refuse copying a file onto
/// itself — so `cp /tmp/a.txt /tmp/b.txt` would fail with "are the same
/// file". The pointer variant became actively wrong once the kernel started
/// passing a resolved path buffer rather than the user pointer, since that
/// address is a stack slot and repeats across calls.
///
/// FNV-1a over the path bytes is stable across calls and distinct per path.
/// `salt` separates the tmpfs-dir / tmpfs-file / initrd namespaces.
fn path_ino(path: &[u8], salt: u64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ salt;
    for &b in path {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Keep it nonzero — 0 reads as "no inode" to some callers.
    (h & 0x0000_ffff_ffff_ffff) | 1
}

fn write_stat(stat_ptr: usize, mode: u32, size: u64, ino: u64) {
    write_stat_owned(stat_ptr, mode, size, ino, 0, 0);
}

fn write_stat_owned(stat_ptr: usize, mode: u32, size: u64, ino: u64, uid: u32, gid: u32) {
    write_stat_full(stat_ptr, mode, 1, size, ino, uid, gid);
}

/// Fill a `struct stat` in the target's native layout.
///
/// `mode` carries both the file-type bits (S_IFREG/S_IFDIR/S_IFCHR/…) and
/// the permission bits; `nlink` is the real hard-link count (the f2fs /bin
/// directory has ~105 names sharing one coreutils inode, so a hardcoded 1
/// is a visible lie).
pub fn write_stat_full(
    stat_ptr: usize,
    mode:     u32,
    nlink:    u64,
    size:     u64,
    ino:      u64,
    uid:      u32,
    gid:      u32,
) {
    write_stat_full_rdev(stat_ptr, mode, nlink, size, ino, uid, gid, 0);
}

/// `st_blocks` (512-byte units) of a tmpfs regular file: its allocated pages,
/// so a sparse file's holes are not billed (`du`, `ls -s`). Caller holds
/// TMP_FILES (lock order TMP_FILES → TMP_VMOS).
fn tmp_st_blocks(owner: usize) -> u64 {
    TMP_VMOS.lock()[owner].as_ref()
        .map_or(0, |v| if v.borrowed { v.pages.len() } else { v.resident_pages() }) as u64 * 8
}

/// As `write_stat_full`, but with `st_blocks` supplied rather than derived from
/// `st_size`.
///
/// Deriving it is only right for a densely allocated file. A sparse one — a
/// disk image ftruncated to 32 GiB with 30 blocks actually written, which is
/// what `disks-rs` builds — then reports 32 GiB of occupied space on a volume
/// that does not have it, and `du` bills the caller for every hole. A
/// filesystem that knows the real count passes it here.
pub fn write_stat_full_blocks(
    stat_ptr: usize,
    mode:     u32,
    nlink:    u64,
    size:     u64,
    ino:      u64,
    uid:      u32,
    gid:      u32,
    blocks:   u64,
) {
    write_stat_full_rdev(stat_ptr, mode, nlink, size, ino, uid, gid, 0);
    unsafe {
        sched::uaccess::copy_raw((stat_ptr as *mut u8).add(64), &blocks as *const u64 as *const u8, 8);
    }
}

/// As `write_stat_full`, but also fills `st_rdev` (device number) for
/// character/block device nodes. The st_rdev offset is arch-specific
/// (x86-64: 40, aarch64: 32 — see the layout comment above).
pub fn write_stat_full_rdev(
    stat_ptr: usize,
    mode:     u32,
    nlink:    u64,
    size:     u64,
    ino:      u64,
    uid:      u32,
    gid:      u32,
    rdev:     u64,
) {
    // Kernel buffers carry kuids; translation happens only on the copy to
    // user memory, as the calling task's user namespace sees them (65534
    // unmapped). Kernel callers (exec_setid, access checks) read raw kuids.
    let to_user = sched::uaccess::user_range_ok(stat_ptr, STAT_SIZE);
    // View translation uses current_userns(): the fill must run on the
    // caller's own thread (fs ports are direct kernel handlers).
    debug_assert!(!to_user || sched::current_pid() != 0, "stat fill off the caller's thread");
    let (uid, gid) = if to_user {
        (sched::ns::view_id(0, uid), sched::ns::view_id(1, gid))
    } else { (uid, gid) };
    unsafe {
        // Built in a kernel buffer, then one PAN-safe copy out.
        let mut kb = [0u8; STAT_SIZE];
        let p = kb.as_mut_ptr();
        (p.add(0) as *mut u64).write_unaligned(1u64); // st_dev
        (p.add(8) as *mut u64).write_unaligned(ino);  // st_ino

        #[cfg(target_arch = "x86_64")]
        {
            (p.add(16) as *mut u64).write_unaligned(nlink); // st_nlink (u64)
            (p.add(24) as *mut u32).write_unaligned(mode);  // st_mode
            (p.add(28) as *mut u32).write_unaligned(uid);   // st_uid
            (p.add(32) as *mut u32).write_unaligned(gid);   // st_gid
            (p.add(40) as *mut u64).write_unaligned(rdev);  // st_rdev
            (p.add(56) as *mut i64).write_unaligned(4096i64); // st_blksize (long)
        }
        #[cfg(target_arch = "aarch64")]
        {
            (p.add(16) as *mut u32).write_unaligned(mode);         // st_mode
            (p.add(20) as *mut u32).write_unaligned(nlink as u32); // st_nlink (u32)
            (p.add(24) as *mut u32).write_unaligned(uid);          // st_uid
            (p.add(28) as *mut u32).write_unaligned(gid);          // st_gid
            (p.add(32) as *mut u64).write_unaligned(rdev);         // st_rdev
            (p.add(56) as *mut i32).write_unaligned(4096i32);      // st_blksize (int)
        }

        (p.add(48) as *mut u64).write_unaligned(size);                // st_size
        (p.add(64) as *mut u64).write_unaligned((size + 511) / 512);  // st_blocks
        sched::uaccess::copy_raw(stat_ptr as *mut u8, kb.as_ptr(), STAT_SIZE);
    }
}

/// Fill `st_atim` / `st_mtim` / `st_ctim` (bytes 72..120 on both ABIs: three
/// `{ i64 sec, i64 nsec }` pairs) of an already-filled `struct stat`.
pub fn write_stat_times(stat_ptr: usize, atime: (i64, i64), mtime: (i64, i64), ctime: (i64, i64)) {
    unsafe {
        let mut kb = [0u8; 48];
        let p = kb.as_mut_ptr();
        (p.add(0)  as *mut i64).write_unaligned(atime.0);
        (p.add(8)  as *mut i64).write_unaligned(atime.1);
        (p.add(16)  as *mut i64).write_unaligned(mtime.0);
        (p.add(24)  as *mut i64).write_unaligned(mtime.1);
        (p.add(32) as *mut i64).write_unaligned(ctime.0);
        (p.add(40) as *mut i64).write_unaligned(ctime.1);
        sched::uaccess::copy_raw((stat_ptr as *mut u8).add(72), kb.as_ptr(), 48);
    }
}

// ── struct statfs ─────────────────────────────────────────────────────────────
//
// Unlike `struct stat` (whose x86-64 layout is bespoke and whose aarch64
// layout is the asm-generic one — see STAT_SIZE above), `struct statfs` is the
// *same* on both of our targets: arch/x86/include/uapi/asm/statfs.h just
// includes <asm-generic/statfs.h>, and arm64 has no asm/statfs.h at all so it
// gets the generic one too. With __BITS_PER_LONG == 64 the generic header
// defines __statfs_word = __kernel_long_t, i.e. every field is 64-bit:
//
//    0: f_type     8: f_bsize   16: f_blocks  24: f_bfree   32: f_bavail
//   40: f_files   48: f_ffree   56: f_fsid(8) 64: f_namelen 72: f_frsize
//   80: f_flags   88: f_spare[4]                            → 120 bytes
//
// The cfg split is kept anyway so that the day a 32-bit or otherwise divergent
// target appears, this is the one place to change — and so nobody has to
// re-derive the "are these actually the same?" argument above.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub const STATFS_SIZE: usize = 120;
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const STATFS_WORD: usize = 8;

/// The numbers a filesystem reports for `statfs`/`fstatfs`.
///
/// All block counts are in units of `bsize`.
#[derive(Clone, Copy)]
pub struct StatfsVals {
    pub f_type:   u64,
    pub bsize:    u64,
    pub blocks:   u64,
    pub bfree:    u64,
    pub bavail:   u64,
    pub files:    u64,
    pub ffree:    u64,
    pub fsid:     u64,
    pub namelen:  u64,
}

/// Well-known `f_type` magics, as reported by Linux.
pub const TMPFS_MAGIC: u64 = 0x0102_1994;
pub const F2FS_MAGIC:  u64 = 0xF2F5_2010;
pub const PROC_MAGIC:  u64 = 0x0000_9fa0;

/// Fill a `struct statfs` in the target's native layout.
///
/// `blocks` must never be zero for a filesystem that should be visible: uutils
/// `df` drops every filesystem whose `f_blocks == 0` unless `-a` is given, and
/// with all of them dropped it prints "no file systems processed". That is
/// exactly what the old fixed-zero statfs stub caused.
pub fn write_statfs(buf_ptr: usize, v: &StatfsVals) {
    unsafe {
        // Built in a kernel buffer, then one PAN-safe copy out.
        let mut kb = [0u8; STATFS_SIZE];
        let p = kb.as_mut_ptr();
        let mut put = |idx: usize, val: u64| {
            (p.add(idx * STATFS_WORD) as *mut u64).write_unaligned(val);
        };
        put(0, v.f_type);
        put(1, v.bsize);
        put(2, v.blocks);
        put(3, v.bfree);
        put(4, v.bavail);
        put(5, v.files);
        put(6, v.ffree);
        put(7, v.fsid);      // f_fsid — two 32-bit words, written as one u64
        put(8, v.namelen);
        put(9, v.bsize);     // f_frsize: we have no fragment size distinct from bsize
        put(10, 0);          // f_flags (ST_* mount flags) — nothing to report
        sched::uaccess::copy_raw(buf_ptr as *mut u8, kb.as_ptr(), STATFS_SIZE);
    }
}

/// Reply carrying nothing but a status; `statfs` handlers write through the
/// caller's buffer pointer like the `stat` family does.
fn statfs_reply() -> Message { ok_reply() }

/// statfs(path) — answer for whichever filesystem owns `path`.
///
/// A path under a registered mount is forwarded verbatim to that mount's
/// server, which is the only component that knows the volume's real geometry.
/// Everything else (tmpfs, initrd/RamFS, /proc, /dev) is served from the tmpfs
/// pool figures below, which are the true capacity of the in-memory store.
fn handle_statfs(path_ptr: usize, buf_ptr: usize) -> Message {
    if buf_ptr == 0 || path_ptr == 0 { return err_reply(-14); }
    let (pbuf, plen) = match read_cstr_raw(path_ptr) {
        Some(r) => r,
        None    => return err_reply(-14),
    };
    let path = strip_trailing_slash(&pbuf[..plen]);

    // /proc and /dev are synthetic and never live on a mount, even after
    // pivot_root has made "/" a prefix match for everything. Only the
    // synthesized `/sys/class/block` subtree is synthetic the same way; the
    // rest of /sys is staged on disk and falls through to the mount lookup.
    if cgroupfs::is_path(path) {
        write_statfs(buf_ptr, &cgroup_statfs());
        return statfs_reply();
    }
    // A bound namespace file (/.leandros-ns/..) is nsfs: containers/common
    // checks the magic of the file it bound at /run/netns/<name>.
    if path.starts_with(alias::NS_SRC) {
        let mut v = procfs_statfs();
        v.f_type = 0x6e73_6673;
        write_statfs(buf_ptr, &v);
        return statfs_reply();
    }
    if !is_tmp_path(path)
        && (path.starts_with(b"/proc") || path.starts_with(b"/dev") || block::is_sysfs_path(path) || sysclass::is_path(path)) {
        write_statfs(buf_ptr, &procfs_statfs());
        return statfs_reply();
    }
    if !is_tmp_path(path) {
        if let Some(port) = find_mount_port(path) {
            let mut proxy = Message::empty();
            proxy.tag = VFS_STATFS;
            proxy.data[0..8].copy_from_slice(&(path_ptr as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&(buf_ptr as u64).to_le_bytes());
            let r = call_port(port, proxy);
            if reply_ok(&r) { statfs_set_flags(buf_ptr, mount_st_flags(port)); }
            return r;
        }
    }
    write_statfs(buf_ptr, &tmpfs_statfs());
    if tmpfs_path(path).is_some() { statfs_set_flags(buf_ptr, tmpfs_st_flags(path)); }
    statfs_reply()
}

/// Is `r` a success reply (a non-negative value)?
fn reply_ok(r: &Message) -> bool {
    i64::from_le_bytes(r.data[0..8].try_into().unwrap_or([0xff; 8])) >= 0
}

/// Store a mount's ST_* flags into a filled `struct statfs` (f_flags).
fn statfs_set_flags(buf_ptr: usize, flags: u64) {
    let _ = sched::uaccess::write_user::<u64>(buf_ptr + 10 * STATFS_WORD, flags);
}

/// fstatfs(fd) — same answer as `handle_statfs`, selected by descriptor.
///
/// Only a `MountedFile` can name a real volume; every other vnode kind lives
/// in this server's own memory, so it reports the tmpfs pool.
fn handle_fstatfs(pid: u32, fd: usize, buf_ptr: usize) -> Message {
    if buf_ptr == 0 { return err_reply(-14); }
    let mut tmp_idx = None;
    let mut is_cg = false;
    let mut is_ns = false;
    let port = {
        let mut tbls = TblSlot::new();
        let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
        if !tbl.fds.used(fd) { return err_reply(-9); }
        match tbl.fds[fd].kind {
            VnodeKind::NsFd { .. } => { is_ns = true; None }
            VnodeKind::MountedFile { port, .. } => Some(port),
            VnodeKind::TmpFile { idx, .. } => { tmp_idx = Some(idx); None }
            VnodeKind::SysBlock { dev, .. } if cgroupfs::is_dev(dev) => { is_cg = true; None }
            _ => None,
        }
    };
    if let Some(idx) = tmp_idx {
        let tmp = TMP_FILES.lock();
        let p = tmp[idx].path();
        if tmp[idx].ephemeral() && (p.starts_with(cgroupfs::SNAP_R) || p.starts_with(cgroupfs::SNAP_W)) { is_cg = true; }
    }
    if is_ns {
        let mut v = procfs_statfs();
        v.f_type = 0x6e73_6673;
        write_statfs(buf_ptr, &v);
        return statfs_reply();
    }
    if is_cg {
        write_statfs(buf_ptr, &cgroup_statfs());
        return statfs_reply();
    }
    if let Some(port) = port {
        let mut proxy = Message::empty();
        proxy.tag = VFS_STATFS;
        proxy.data[0..8].copy_from_slice(&0u64.to_le_bytes()); // no path — port is the volume
        proxy.data[8..16].copy_from_slice(&(buf_ptr as u64).to_le_bytes());
        let r = call_port(port, proxy);
        if reply_ok(&r) { statfs_set_flags(buf_ptr, mount_st_flags(port)); }
        return r;
    }
    write_statfs(buf_ptr, &tmpfs_statfs());
    if let Some(idx) = tmp_idx {
        let fl = { let tmp = TMP_FILES.lock(); tmpfs_st_flags(tmp[idx].path()) };
        if fl != 0 { statfs_set_flags(buf_ptr, fl); }
    }
    statfs_reply()
}

/// Live figures for tmpfs: `MAX_TMP_FILES` inodes sharing the page budget,
/// counted in 4 KiB blocks. Both numbers are exact — every page a tmpfs file
/// holds is charged in `TMP_PAGES_USED` until it is freed.
fn tmpfs_statfs() -> StatfsVals {
    const BSIZE: u64 = 4096;
    // Capacity is the page budget (half of RAM, Linux's default `size=`), and
    // usage is the pages tmpfs actually holds — holes cost nothing, exactly as
    // `df` reports a sparse file on Linux tmpfs.
    let total_blocks = tmpfs_budget_pages() as u64;
    let used_blocks = tmpfs_used_pages() as u64;
    let used_slots = TMP_FILES.lock().count() as u64;
    let free_blocks = total_blocks.saturating_sub(used_blocks);
    StatfsVals {
        f_type:  TMPFS_MAGIC,
        bsize:   BSIZE,
        blocks:  total_blocks,
        bfree:   free_blocks,
        bavail:  free_blocks,
        files:   MAX_TMP_FILES as u64,
        ffree:   (MAX_TMP_FILES as u64).saturating_sub(used_slots),
        fsid:    0x0102_1994,
        namelen: NAME_MAX as u64,
    }
}

/// /proc and /dev: zero-capacity pseudo-filesystems, exactly as Linux reports
/// them. `df` filters these out by fstype long before f_blocks matters, and
/// `df /proc` prints a 0-block line rather than an error — both match Linux.
fn cgroup_statfs() -> StatfsVals {
    StatfsVals {
        f_type: cgroupfs::MAGIC, bsize: 4096, blocks: 0, bfree: 0, bavail: 0,
        files: 0, ffree: 0, fsid: 0, namelen: 255,
    }
}

fn procfs_statfs() -> StatfsVals {
    StatfsVals {
        f_type: PROC_MAGIC, bsize: 4096, blocks: 0, bfree: 0, bavail: 0,
        files: 0, ffree: 0, fsid: 0, namelen: 255,
    }
}

/// fstat(fd) — report metadata for an *open descriptor*.
///
/// Unlike the path-based stat family this has only an fd, so it answers from
/// the vnode kind recorded in the fd table. The file-type bits are the whole
/// point: the kernel used to fabricate a flat `S_IFREG|0644` for every fd above
/// 2, which made a pipe end indistinguishable from a regular file. tokio's
/// `pipe::Receiver::from_file` gates on `S_ISFIFO` and rejected brush's
/// command-substitution pipe with "not a pipe", so `$(...)` failed before it
/// ever ran anything.
///
/// `MountedFile` still reports S_IFREG with an lseek-derived size — resolving
/// its real type needs a per-mount "stat this open file" operation that no
/// filesystem implements yet. That is unchanged from the previous behavior and
/// is the one remaining gap.
fn handle_fstat(pid: u32, fd: usize, stat_ptr: usize) -> Message {
    const S_IFIFO: u32 = 0o010000;
    const S_IFCHR: u32 = 0o020000;
    const S_IFDIR: u32 = 0o040000;
    const S_IFREG: u32 = 0o100000;
    const S_IFSOCK: u32 = 0o140000;

    if stat_ptr == 0 { return err_reply(-14); }

    let kind = {
        let mut tbls = TblSlot::new();
        let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
        if !tbl.fds.used(fd) { return err_reply(-9); }
        tbl.fds[fd].kind
    };

    // A mounted file's real type, size and owner live in the mount server's
    // inode — only it can tell a directory fd from a regular-file fd. Proxy the
    // whole stat there. The shared tail below cannot serve this: it hardcoded
    // S_IFREG, so `fstat` on a directory fd read as a regular file, which is
    // exactly what made musl `fdopendir` (issued before every `readdir`) return
    // ENOTDIR and broke fd-based traversal — `rm -r`, `du`, GNU fts.
    if let VnodeKind::MountedFile { port, file_id } = kind {
        let mut proxy = Message::empty();
        proxy.tag = VFS_FSTAT;
        proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
        proxy.data[8..16].copy_from_slice(&(stat_ptr as u64).to_le_bytes());
        let r = call_port(port, proxy);
        if reply_val(&r) == 0 { set_stat_dev(stat_ptr, mount_dev_by_port(port)); }
        return r;
    }

    // A block device node. `S_IFBLK` is load-bearing: `fs::metadata().is_file()`
    // is false for one, tools branch on the type, and st_size must be the
    // device capacity so a caller that sizes by stat agrees with lseek(END).
    // /dev/loop-control is the exception — it is a character device (10:237).
    if let VnodeKind::BlockDev { dev, .. } = kind {
        const S_IFBLK: u32 = 0o060000;
        let ifmt = if block::dev_is_char(dev) { S_IFCHR } else { S_IFBLK };
        write_stat_full_rdev(stat_ptr, ifmt | 0o660, 1, block::dev_size(dev),
                             block::dev_ino(dev), 0, 6, block::dev_rdev(dev));
        return ok_reply();
    }

    // A synthesized sysfs directory. musl's `fdopendir` (issued by every
    // `opendir`, and therefore by `fs::read_dir`) fstats the fd and answers
    // ENOTDIR unless this says S_IFDIR.
    if let VnodeKind::SysBlock { dev, .. } = kind {
        if cgroupfs::is_dev(dev) {
            let cg = cgroupfs::dev_cg(dev);
            let (u, g) = sched::cgroup::dir_owner(cg);
            write_stat_full(stat_ptr, S_IFDIR | 0o755, (2 + sched::cgroup::nr_children(cg)) as u64, 0,
                            0x6000_0000 + (cg as u64) * 64, u, g);
            return ok_reply();
        }
    }
    if let VnodeKind::SysBlock { dev, level, .. } = kind {
        write_stat_full(stat_ptr, S_IFDIR | 0o755, 1, 0,
                        0x5000_0000 + (dev as u64) * 8 + level as u64, 0, 0);
        return ok_reply();
    }

    // A dynamic device node (DRM card0, evdev eventN) must report its real
    // st_rdev — libdrm computes major(st_rdev)==226 to find /sys/dev/char/226:0,
    // and libudev keys on it. The rdev comes from the registry (port, dev_id).
    if let VnodeKind::DynamicDevice { port, dev_id, .. } = kind {
        let rdev = lookup_device_rdev(port, dev_id);
        let m = dyn_dev_meta_rdev(rdev);
        write_stat_full_rdev(stat_ptr, m.mode as u32, 1, 0, 0, m.uid, m.gid, rdev);
        return ok_reply();
    }

    // A pty slave reports its node's owner and mode (`PTS_META`), exactly as
    // stat("/dev/pts/N") does.
    if let VnodeKind::Pty { pair, is_master: false } = kind {
        if tty_server::pty::vt_of(pair as usize) == 0 {
            let m = pts_meta(pair as usize);
            write_stat_full_rdev(stat_ptr, m.mode as u32, 1, 0, pty_ino(pair, false), m.uid, m.gid,
                                 char_dev_rdev(&kind));
            return ok_reply();
        }
    }

    // (mode, size, ino)
    let (mode, size, ino): (u32, u64, u64) = match kind {
        VnodeKind::Pipe { ring, .. } => {
            // st_size on a FIFO is 0 on Linux; the ring index is a stable,
            // unique-per-pipe inode number, which is what `pipe:[N]` in
            // /proc/self/fd already reports.
            (S_IFIFO | 0o600, 0, 0x1000_0000 + ring as u64)
        }
        // A pty end is a character device — `isatty()` is TCGETS, but
        // `ttyname()` additionally readlinks /proc/self/fd/N and demands that
        // stat of the path it reads back agree with this. 0620 (tty group) is
        // the mode a real /dev/pts/N carries. The inode must be distinct per
        // end and per pair, and stable, so `stat("/dev/pts/N")` can reproduce
        // it; distinct from the pipe (0x1000_0000), tmpfs (0x2000_0000) and
        // console (0x3000_0000) ranges.
        VnodeKind::Pty { pair, is_master } => {
            // A VT session's slave IS `/dev/ttyN`: same inode stat() on the
            // path reports, or ttyname() rejects it.
            match tty_server::pty::vt_of(pair as usize) {
                0 => (S_IFCHR | 0o620, 0, pty_ino(pair, is_master)),
                n => (S_IFCHR | 0o620, 0, VT_INO_BASE + n as u64),
            }
        }
        // A console proxy (a dup'd stdio fd, or an fd opened on /dev/tty or
        // /dev/stdin) is the console, so it reports the console's inode — the
        // same one stat("/dev/console") reports. Without that agreement
        // ttyname() rejects the fd; see CONSOLE_INO.
        VnodeKind::DevStdio { .. } => (S_IFCHR | 0o666, 0, CONSOLE_INO),
        // A VT is a character device with its OWN inode, not the console's:
        // ttyname() on a /dev/tty2 fd must read back /dev/tty2. 0620 is the
        // mode a real /dev/ttyN carries.
        VnodeKind::DevVt { vt, .. } => (S_IFCHR | 0o620, 0, VT_INO_BASE + vt as u64),
        VnodeKind::DevNull | VnodeKind::DevZero | VnodeKind::DevFull | VnodeKind::DevUrandom
        | VnodeKind::DevFb { .. } => (S_IFCHR | 0o666, 0, 0),
        // DynamicDevice is handled by the early return above (it needs st_rdev);
        // this arm exists only for match exhaustiveness.
        VnodeKind::DynamicDevice { .. } => return err_reply(-9),
        // Likewise handled by the early returns above.
        VnodeKind::BlockDev { .. } | VnodeKind::SysBlock { .. } => return err_reply(-9),
        // A pseudo-directory reports S_IFDIR with size 0. It used to report
        // S_IFREG with `size = data.len()`, i.e. the length of its own path —
        // which is what let memmap2 map `/tmp` as a 4-byte "file".
        VnodeKind::RamFile { is_dir: true, .. } => (S_IFDIR | 0o755, 0, 0),
        VnodeKind::RamFile { data, .. } => (S_IFREG | 0o644, data.len() as u64, 0),
        VnodeKind::TmpFile { idx, .. } => {
            let t = TMP_FILES.lock();
            let e = &t[idx];
            let ino = t[tmp_owner(&t, idx)].ino;
            if e.is_dir       { (S_IFDIR | (e.mode & 0o7777), 0, ino) }
            else if e.is_sock { (S_IFSOCK | (e.mode & 0o7777), 0, ino) }
            else if e.is_fifo { (S_IFIFO | (e.mode & 0o7777), 0, ino) }
            else if e.devtype != 0 { ((if e.devtype == 1 { 0o020000 } else { 0o060000 }) | (e.mode & 0o7777), 0, ino) }
            else              { (S_IFREG | (e.mode & 0o7777), e.len as u64, ino) }
        }
        // eventfd/timerfd/signalfd/inotify are anon-inode files and report S_IFREG.
        VnodeKind::EventFd { .. } | VnodeKind::TimerFd { .. } | VnodeKind::Mq { .. }
        | VnodeKind::SignalFd { .. } | VnodeKind::Inotify { .. }
        | VnodeKind::PidFd { .. } | VnodeKind::BpfProg { .. } => (S_IFREG | 0o600, 0, 0),
        // An open_tree(OPEN_TREE_CLONE) descriptor (kind 4) is the cloned
        // mount's root: Linux reports the type of that root, and crun decides
        // between creating a directory or a file mount point from it.
        VnodeKind::NsFd { kind: 4, id } if mnt::is_anon(id) && mnt::anon_is_dir(id) =>
            (S_IFDIR | 0o755, 0, mnt::anon_root_ident(id).unwrap_or_else(|| sched::ns::ns_ino(4, id))),
        VnodeKind::NsFd { kind, id } => (S_IFREG | 0o444, 0, sched::ns::ns_ino(kind as usize, id)),
        // Handled by the early return above (proxied to the owning mount);
        // this arm exists only for match exhaustiveness.
        VnodeKind::MountedFile { .. } => return err_reply(-9),
        // The kernel answers fstat on a socket alias from the socket.
        VnodeKind::None | VnodeKind::SockAlias { .. } | VnodeKind::EpollFd { .. } => return err_reply(-9),
    };

    // st_nlink must agree with what path-based stat reports for the same file,
    // or `ln f g && stat f` and `ln f g && stat <fd>` disagree.
    let nlink = match kind {
        VnodeKind::TmpFile { idx, .. } => { let t = TMP_FILES.lock(); tmp_nlink(&t, idx) }
        _ => 1,
    };
    write_stat_full_rdev(stat_ptr, mode, nlink, size, ino, 0, 0, char_dev_rdev(&kind));
    if let VnodeKind::TmpFile { idx, .. } = kind {
        let t = TMP_FILES.lock();
        let owner = tmp_owner(&t, idx);
        let e = &t[owner];
        let (at, mt, ct) = (e.atime, e.mtime, e.ctime);
        let (uid, gid) = (e.uid, e.gid);
        let blocks = if mode & 0o170000 == S_IFREG { Some(tmp_st_blocks(owner)) } else { None };
        drop(t);
        // The owner, as path stat reports it. fstat answered uid/gid 0 for
        // every tmpfs file, so a program checking that the directory it just
        // created is its own (Bun: /tmp/claude-<uid>) refused it.
        write_stat_full(stat_ptr, mode, nlink, size, ino, uid, gid);
        write_stat_times(stat_ptr, at, mt, ct);
        if let Some(b) = blocks {
            unsafe { sched::uaccess::copy_raw((stat_ptr as *mut u8).add(64), &b as *const u64 as *const u8, 8); }
        }
        let t = TMP_FILES.lock();
        let dev = tmpfs_dev_of(t[owner].path());
        drop(t);
        if let Some(d) = dev { set_stat_dev(stat_ptr, d); }
    }
    ok_reply()
}

fn handle_stat(path_ptr: usize, stat_ptr: usize) -> Message {
    stat_common(path_ptr, stat_ptr, true)
}

/// `st_rdev` of a terminal or memory-device fd, Linux's numbers: a pty slave
/// is `/dev/pts/N` (136 + N/256, N%256) and its master `/dev/ptmx` (5:2), a
/// VT session's slave and a VT node `/dev/ttyN` (4:N), the console (5:1),
/// /dev/null (1:3), /dev/zero (1:5), /dev/urandom (1:9), /dev/fb0 (29:0).
/// They agree with `/proc/<pid>/stat`'s tty_nr (`tty_server::ctty_dev_of_sid`)
/// and with a path stat of the node (`dev_node_rdev`). 0 for anything else.
fn char_dev_rdev(kind: &VnodeKind) -> u64 {
    match *kind {
        VnodeKind::Pty { is_master: true, .. } => makedev(5, 2),
        VnodeKind::Pty { pair, .. } => match tty_server::pty::vt_of(pair as usize) {
            0 => pts_rdev(tty_server::pty::ident(pair as usize).1 as u32),
            n => makedev(4, n as u32),
        },
        VnodeKind::DevStdio { .. } => makedev(5, 1),
        VnodeKind::DevVt { vt, .. } => makedev(4, vt as u32),
        VnodeKind::DevNull => makedev(1, 3),
        VnodeKind::DevZero => makedev(1, 5),
        VnodeKind::DevFull => makedev(1, 7),
        VnodeKind::DevUrandom => makedev(1, 9),
        VnodeKind::DevFb { .. } => makedev(29, 0),
        _ => 0,
    }
}

/// `/dev/pts/N`'s device number (Unix98 pty slaves: majors 136..143).
fn pts_rdev(n: u32) -> u64 { makedev(136 + n / 256, n % 256) }

/// `st_rdev` of the fixed /dev nodes a path stat answers (see `char_dev_rdev`
/// for the fd side). `/dev/tty` itself is 5:0 when the caller has no pty
/// controlling terminal (a pty one reports the pty, as the fd open hands back).
fn dev_node_rdev(path: &[u8]) -> u64 {
    match path {
        b"/dev/null" => makedev(1, 3),
        b"/dev/zero" => makedev(1, 5),
        b"/dev/full" => makedev(1, 7),
        b"/dev/random" => makedev(1, 8),
        b"/dev/urandom" => makedev(1, 9),
        b"/dev/tty" => makedev(5, 0),
        b"/dev/console" => makedev(5, 1),
        b"/dev/ptmx" => makedev(5, 2),
        b"/dev/fb0" => makedev(29, 0),
        _ => if let Some(vt) = vt_number(path) { makedev(4, vt as u32) }
             else if let Some(n) = pts_number(path) { pts_rdev(tty_server::pty::ident(n).1 as u32) }
             else { 0 },
    }
}

/// Shared body of `stat` and `lstat`.
///
/// `follow == true` is `stat(2)`: the caller reached here through the
/// resolution choke point in `handle()`, so `path` already names the symlink's
/// target and no S_IFLNK entry can be found at the end of it.
/// `follow == false` is `lstat(2)`: the final component was deliberately left
/// unresolved, so a tmpfs symlink is reported as S_IFLNK with the length of
/// its target as `st_size` (which is what `ls -l` prints after the `->`), and
/// the query is forwarded to mount servers as VFS_LSTAT rather than VFS_STAT.
fn stat_common(path_ptr: usize, stat_ptr: usize, follow: bool) -> Message {
    let r = stat_common_inner(path_ptr, stat_ptr, follow);
    // A path answered from a built-in tmpfs root carries that root's
    // anonymous st_dev (mount-served paths are stamped at the proxy site).
    if reply_val(&r) == 0 {
        if let Some((pbuf, plen)) = read_cstr_raw(path_ptr) {
            let path = &pbuf[..plen];
            if !mounted_inside_tmpfs(path) {
                if let Some(d) = tmpfs_dev_of(path) { set_stat_dev(stat_ptr, d); }
            }
        }
    }
    r
}

fn stat_common_inner(path_ptr: usize, stat_ptr: usize, follow: bool) -> Message {
    if stat_ptr == 0 { return err_reply(-14); }
    let (pbuf, plen) = match read_cstr_raw(path_ptr) { Some(r) => r, None => return err_reply(-14) };
    let path = &pbuf[..plen];

    if let Some(name) = mqueue::path_name(strip_trailing_slash(path)) {
        return match mqueue::stat_path(sched::current_pid(), name) {
            Some((mode, u, g, ino, size)) => { write_stat_owned(stat_ptr, mode, size, ino, u, g); ok_reply() }
            None => err_reply(-2),
        };
    }

    // `/proc/<pid>/root[/...]` stats what it names under "/" (see handle_open).
    if let Some(rest) = proc_pid_root_rest(path) {
        let mut buf = alloc::vec![0u8; HOST_PATH_MAX];
        buf[0] = b'/';
        let rest = rest.strip_prefix(b"/").unwrap_or(rest);
        let n = rest.len().min(buf.len() - 2);
        buf[1..1 + n].copy_from_slice(&rest[..n]);
        return stat_common(buf.as_ptr() as usize, stat_ptr, follow);
    } else if proc_pid_root_dead(path) {
        return err_reply(-2);
    }

    // /proc/<pid>, /proc/<pid>/<file>, /proc/<pid>/task[/<tid>[/<file>]]:
    // owned by the process's effective ids, as on Linux.
    if let Some(r) = proc_ns_file(path, sched::current_pid()) {
        let Some((pid, k)) = r else { return err_reply(-2) };
        if sched::proc_stat_of(pid).is_none() { return err_reply(-2); }
        let (u, g) = sched::cred::ids_of(pid).map(|(u, g)| (u[1], g[1])).unwrap_or((0, 0));
        write_stat_full(stat_ptr, 0o100444, 1, 0, sched::ns::ns_ino(k, sched::ns::ns_id_of(pid, k)), u, g);
        return ok_reply();
    }
    if let Some(k) = proc_stat_kind(path) {
        let Some((dir, owner, name)) = k else { return err_reply(-2) };
        let (u, g) = sched::cred::proc_dump_owner(owner);
        let mode = if dir { 0o040555 } else if name == b"comm" || name == b"uid_map" || name == b"gid_map" || name == b"setgroups" || name == b"oom_score_adj" { 0o100644 } else { 0o100444 };
        write_stat_full(stat_ptr, mode, if dir { 2 } else { 1 }, 0, path_ino(path, 7), u, g);
        return ok_reply();
    }
    // LeandrOS's diagnostic /proc files exist only in handle_open's chain;
    // without this, stat and access(2) said ENOENT while open worked.
    if PROC_DIAG_FILES.contains(&strip_trailing_slash(path)) {
        write_stat_full(stat_ptr, 0o100444, 1, 0, path_ino(path, 7), 0, 0);
        return ok_reply();
    }
    // The generated system files (`gen_proc_system`) exist for stat too:
    // mount(2) of a bind onto one (OCI masked paths, lxcfs) stats its target.
    if PROC_SYSTEM_FILES.contains(&strip_trailing_slash(path)) {
        write_stat_full(stat_ptr, 0o100444, 1, 0, path_ino(path, 7), 0, 0);
        return ok_reply();
    }

    if let Some(lookup_path) = should_lookup_ramfs(path) {
        // A GPU backend's DRM PCI identity file (see `drm_sysfs_attr`).
        if is_drm_sysfs_override(lookup_path) {
            let mut buf = [0u8; TMP_BUF_SIZE];
            let n = drm_sysfs_attr(lookup_path, &mut buf).unwrap_or(0);
            write_stat(stat_ptr, 0o100444, n as u64, path_ino(lookup_path, 5));
            return ok_reply();
        }
        // Known static directories.
        for &dir in RAMFS_DIRS {
            if lookup_path == dir {
                write_stat(stat_ptr, ramfs_dir_mode(dir), 0, 1 + dir.as_ptr() as u64 & 0xFFFF);
                return ok_reply();
            }
        }
        // tmpfs directories.
        if let Some(tpath) = tmpfs_path(path) {
            let tmp = TMP_FILES.lock();
            if let Some((idx, e)) = tmp_find(&tmp, tpath).map(|i| (i, &tmp[i])).filter(|(_, e)| e.is_dir) {
                // The slot's inode, the number fstat on an open directory
                // reports. It used to be a hash of the path, so stat and
                // fstat of one directory disagreed, and a program checking
                // that the directory it holds open is the one at its path
                // (the Claude Code CLI pins its task-output directory that
                // way) took it for a swapped one.
                let ino = e.ino;
                let mode = 0o040000 | if e.mode != 0 { e.mode } else { 0o755 };
                let (uid, gid) = (e.uid, e.gid);
                let (at, mt, ct) = (e.atime, e.mtime, e.ctime);
                drop(tmp);
                write_stat_owned(stat_ptr, mode, 0, ino, uid, gid);
                write_stat_times(stat_ptr, at, mt, ct);
                return ok_reply();
            }
        }
        // The console. Must be answered before the RamFS sweep below, which
        // would otherwise report the placeholder /dev/tty entry as a zero-byte
        // S_IFREG — and must carry CONSOLE_INO so it agrees with fstat on the
        // console fd itself. See CONSOLE_INO for why ttyname() depends on it.
        if lookup_path == b"/dev/tty" || lookup_path == b"/dev/console" {
            // /dev/tty follows the same ctty resolution `open` does, or
            // `stat("/dev/tty")` disagrees with `fstat` on the fd that open
            // just handed back — and disagreement there is precisely what
            // ttyname() treats as "this fd is not that path".
            if lookup_path == b"/dev/tty" {
                if let Some(n) = tty_server::pty::ctty_for_sid(sched::current_sid()) {
                    let rdev = match tty_server::pty::vt_of(n) { 0 => pts_rdev(tty_server::pty::ident(n).1 as u32), v => makedev(4, v as u32) };
                    write_stat_full_rdev(stat_ptr, 0o020620, 1, 0, pty_ino(n as u16, false), 0, 0, rdev);
                    return ok_reply();
                }
            }
            write_stat_full_rdev(stat_ptr, 0o020666, 1, 0, CONSOLE_INO, 0, 0, dev_node_rdev(lookup_path));
            return ok_reply();
        }
        // The virtual consoles. Same reason as /dev/ptmx below: the RAMFS sweep
        // would report the placeholder entry as a zero-byte S_IFREG, and a
        // libseat/logind shim that stats /dev/tty0 to decide whether the system
        // has VTs would conclude it does not.
        if let Some(vt) = vt_number(lookup_path) {
            write_stat_full_rdev(stat_ptr, 0o020620, 1, 0, VT_INO_BASE + vt as u64, 0, 0, dev_node_rdev(lookup_path));
            return ok_reply();
        }
        // Special device files.
        if lookup_path == b"/dev/null" || lookup_path == b"/dev/zero" || lookup_path == b"/dev/full" || lookup_path == b"/dev/urandom"
           || lookup_path == b"/dev/random" {
            write_stat_full_rdev(stat_ptr, 0o020666, 1, 0, 2, 0, 0, dev_node_rdev(lookup_path));
            return ok_reply();
        }
        if lookup_path == b"/dev/fb0" {
            let m = fb_meta();
            write_stat_full_rdev(stat_ptr, m.mode as u32, 1, 0, 2, m.uid, m.gid, makedev(29, 0));
            return ok_reply();
        }
        if lookup_path == b"/dev/stdin" || lookup_path == b"/dev/stdout" || lookup_path == b"/dev/stderr" {
            write_stat(stat_ptr, 0o020666, 0, 3);
            return ok_reply();
        }
        // Pty nodes are character devices, and their inode numbers must match
        // what fstat reports on the corresponding fd — see pty_ino. Answered
        // ahead of the RAMFS sweep, which would otherwise report /dev/ptmx as
        // a zero-byte S_IFREG and make ttyname() reject the fd.
        if let Some(inst) = ptmx_inst(lookup_path) {
            let m = ptmx_meta(inst);
            write_stat_full_rdev(stat_ptr, m.mode as u32, 1, 0, pty_ino(0, true), 0, 0, makedev(5, 2));
            return ok_reply();
        }
        if let Some(n) = pts_number(lookup_path) {
            let m = pts_meta(n);
            write_stat_full_rdev(stat_ptr, m.mode as u32, 1, 0, pty_ino(n as u16, false), m.uid, m.gid, pts_rdev(tty_server::pty::ident(n).1 as u32));
            return ok_reply();
        }
        // Block device nodes. Must agree byte for byte with what `handle_fstat`
        // reports for an fd on the same path — a `fs::metadata` caller and an
        // `fstat` caller have to see the same device.
        if let Some(dev) = block::lookup_dev_node(lookup_path) {
            const S_IFBLK: u32 = 0o060000;
            let ifmt = if block::dev_is_char(dev) { 0o020000 } else { S_IFBLK };
            write_stat_full_rdev(stat_ptr, ifmt | 0o660, 1, block::dev_size(dev),
                                 block::dev_ino(dev), 0, 6, block::dev_rdev(dev));
            return ok_reply();
        }
        // The synthesized sysfs subtree: `/sys/class/block` directories, then
        // their attribute files. `/sys` and `/sys/class` themselves, and
        // everything outside this subtree, are staged on disk and answered
        // by the ordinary mount lookup below, not here.
        if cgroupfs::is_path(lookup_path) {
            return match cgroupfs::stat(sched::current_pid(), lookup_path) {
                Some((m, nl, ino, u, g)) => { write_stat_full(stat_ptr, m, nl as u64, 0, ino, u, g); ok_reply() }
                None => err_reply(-2),
            };
        }
        if !follow && lookup_path.starts_with(b"/sys/") {
            let mut lb = [0u8; 96];
            if let Some(n) = sysclass::link(lookup_path, &mut lb) {
                write_stat(stat_ptr, 0o120777, n as u64, path_ino(lookup_path, 6));
                return ok_reply();
            }
        }
        if sysclass::is_path(lookup_path) {
            if let Some((dev, level)) = sysclass::dir(lookup_path) {
                write_stat(stat_ptr, 0o040755, 0, 0x5000_0000 + (dev as u64) * 8 + level as u64);
                return ok_reply();
            }
            let mut buf = [0u8; TMP_BUF_SIZE];
            if let Some(n) = sysclass::attr(lookup_path, &mut buf) {
                if sysclass::is_video_writable(lookup_path) {
                    write_stat_owned(stat_ptr, 0o100664, n as u64, path_ino(lookup_path, 5), 0, sysclass::VIDEO_GID);
                    return ok_reply();
                }
                let m = if sysclass::is_writable(lookup_path) { 0o100644 } else { 0o100444 };
                write_stat(stat_ptr, m, n as u64, path_ino(lookup_path, 5));
                return ok_reply();
            }
            return err_reply(-2);
        }
        if block::is_sysfs_path(lookup_path) {
            if let Some((dev, level)) = block::sysfs_dir(lookup_path) {
                write_stat(stat_ptr, 0o040755, 0,
                           0x5000_0000 + (dev as u64) * 8 + level as u64);
                return ok_reply();
            }
            let mut buf = [0u8; TMP_BUF_SIZE];
            if let Some(n) = block::sysfs_attr(lookup_path, &mut buf) {
                write_stat(stat_ptr, 0o100444, n as u64, path_ino(lookup_path, 5));
                return ok_reply();
            }
            // ENOENT — nothing else exists under the synthesized subtree
            return err_reply(-2);
        }
        // Dynamic devices. Report the real st_rdev so a path-based stat of
        // /dev/dri/card0 agrees with fstat (libdrm derives 226:0 from it).
        let dyn_rdev = {
            let devices = DYNAMIC_DEVICES.lock();
            devices.iter()
                .find(|d| d.in_use && d.path.as_bytes() == lookup_path)
                .map(|d| makedev(d.major, d.minor))
        };
        if let Some(rdev) = dyn_rdev {
            let m = dyn_dev_meta_rdev(rdev);
            write_stat_full_rdev(stat_ptr, m.mode as u32, 1, 0, 4, m.uid, m.gid, rdev);
            return ok_reply();
        }
        // Static RamFS files.
        for entry in RAMFS {
            if lookup_path == entry.path {
                write_stat(stat_ptr, 0o100444, entry.data.len() as u64, entry.path.as_ptr() as u64 & 0xFFFF);
                return ok_reply();
            }
        }
        // tmpfs files.
        if let Some(tpath) = tmpfs_path(path) {
            let tmp = TMP_FILES.lock();
            if let Some(idx) = tmp_find(&tmp, tpath).filter(|&i| !tmp[i].is_dir) {
                // Hard links share one inode, so both st_ino and st_nlink must
                // come from the data-owning slot — `ls -i` and `stat` are how
                // callers verify a link took, and a per-name inode number would
                // make two links look like two files.
                let owner = tmp_owner(&tmp, idx);
                let nlink = tmp_nlink(&tmp, idx);
                let e = &tmp[owner];
                let size = e.len as u64;
                // S_IFLNK / S_IFSOCK / S_IFIFO / S_IFREG
                let ifmt: u32 = if !follow && tmp[idx].is_link { 0o120000 }
                                else if e.is_sock { 0o140000 }
                                else if e.is_fifo { 0o010000 }
                                else if e.devtype == 1 { 0o020000 }
                                else if e.devtype == 2 { 0o060000 }
                                else { 0o100000 };
                let default_mode = if ifmt == 0o120000 { 0o777 } else { 0o644 };
                let mode = ifmt | if e.mode != 0 { e.mode } else { default_mode };
                let ino = e.ino;
                let (uid, gid) = (e.uid, e.gid);
                let (at, mt, ct) = (e.atime, e.mtime, e.ctime);
                let blocks = if ifmt == 0o100000 { Some(tmp_st_blocks(owner)) } else { None };
                let rdev = if e.devtype != 0 { e.rdev as u64 } else { 0 };
                drop(tmp);
                if rdev != 0 || ifmt == 0o020000 || ifmt == 0o060000 {
                    write_stat_full_rdev(stat_ptr, mode, nlink, size, ino, uid, gid, rdev);
                } else { match blocks {
                    Some(b) => write_stat_full_blocks(stat_ptr, mode, nlink, size, ino, uid, gid, b),
                    None    => write_stat_full(stat_ptr, mode, nlink, size, ino, uid, gid),
                } }
                write_stat_times(stat_ptr, at, mt, ct);
                return ok_reply();
            }
            // Not in the pool: the name does not exist. `handle_open` already
            // answers ENOENT here; stat used to fall through to the mount
            // table, and since init pivot_roots F2FS onto "/" that shipped
            // every missed /tmp, /dev/shm and /run/user lookup to the F2FS
            // server as an IPC round trip (lane tmpfast: the larger half of a
            // `test -e /tmp/nope`), where it could also find a stray on-disk
            // file that open() would refuse. Only a filesystem mounted
            // *inside* a tmpfs namespace still gets asked.
            if !mounted_inside_tmpfs(path) { return err_reply(-2); } // ENOENT
        }
        // initrd CPIO archive. Everything in it is a boot-time executable,
        // and execve's permission gate falls back to these mode bits when a
        // filesystem cannot answer VFS_ACCESS — so report them executable.
        if let Some(data) = find_in_initrd(lookup_path) {
            write_stat(stat_ptr, 0o100555, data.len() as u64, path_ino(lookup_path, 3));
            return ok_reply();
        }
    }

    // Fall back to mounted filesystems.
    if let Some(port) = find_mount_port(path) {
        let mut proxy = Message::empty();
        proxy.tag = if follow { VFS_STAT } else { VFS_LSTAT };
        proxy.data[0..8].copy_from_slice(&(path_ptr as u64).to_le_bytes());
        proxy.data[8..16].copy_from_slice(&(stat_ptr as u64).to_le_bytes());
        let mut r = call_port(port, proxy);
        // A mount server that predates VFS_LSTAT answers ENOSYS; degrade to
        // stat rather than failing the call outright.
        if !follow && reply_val(&r) == -38 {
            let mut retry = Message::empty();
            retry.tag = VFS_STAT;
            retry.data[0..8].copy_from_slice(&(path_ptr as u64).to_le_bytes());
            retry.data[8..16].copy_from_slice(&(stat_ptr as u64).to_le_bytes());
            r = call_port(port, retry);
        }
        if reply_val(&r) == 0 { set_stat_dev(stat_ptr, mount_dev_by_port(port)); }
        return r;
    }

    err_reply(-2) // ENOENT
}

enum FdInfo { Static(&'static [u8]), Pipe(usize), RamData(*const u8), TmpIdx(usize),
              Mounted(u32, u32), PtsSlave(u16), Cg(sched::cgroup::CgId), Bad }

fn handle_fd_path(pid: u32, fd: usize, buf_ptr: usize, buf_len: usize) -> Message {
    if buf_ptr == 0 || buf_len == 0 { return err_reply(-14); }
    // fd tables are keyed by tgid, like every other lookup here (find_tbl).
    // Matching the raw pid missed the table for any non-leader thread, so a
    // threaded process got EBADF for descriptors it plainly held.
    let pid = sched::tgid_of(pid);

    // The console has no fd-table entry at all: an unredirected fd 0/1/2 is
    // handled above this layer, so `tbl.fds[0].in_use` is false and the lookup
    // below would answer EBADF for the one descriptor most likely to be asked
    // about. That EBADF is what made `tty` print "not a tty" on a real console:
    // ttyname() readlinks /proc/self/fd/0 and treats any error as "no tty".
    //
    // Answer with the console's own device path. Checked before the lock —
    // fd_is_console_stdio takes FD_TABLES itself — and it is false for a
    // redirected fd 0 (a file, pipe or directory has a real entry), so
    // redirection keeps reporting the redirected target.
    if fd_is_console_stdio(pid, fd) {
        let p: &[u8] = b"/dev/console";
        let c = p.len().min(buf_len);
        unsafe { let _ = sched::uaccess::copy_raw(buf_ptr as *mut u8, p.as_ptr(), c); }
        return val_reply(c as u64);
    }

    let info = {
        let mut tbls = TblSlot::new();
        let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return err_reply(-9) };
        if !tbl.fds.used(fd) { return err_reply(-9); }
        match &tbl.fds[fd].kind {
            VnodeKind::DevNull => FdInfo::Static(b"/dev/null"),
            VnodeKind::DevZero => FdInfo::Static(b"/dev/zero"),
            VnodeKind::DevFull => FdInfo::Static(b"/dev/full"),
            VnodeKind::Pipe { ring, .. } => FdInfo::Pipe(*ring),
            // ptsname(3) in some libcs is a readlink of /proc/self/fd/N on the
            // *master*, and ttyname(3) on the slave is the same readlink — so
            // both ends must name the path they were opened by, not an
            // "anon_inode" placeholder.
            VnodeKind::Pty { is_master: true, .. } => FdInfo::Static(b"/dev/ptmx"),
            VnodeKind::Pty { pair, is_master: false } => match tty_server::pty::vt_of(*pair as usize) {
                0 => FdInfo::PtsSlave(tty_server::pty::ident(*pair as usize).1),
                2 => FdInfo::Static(b"/dev/tty2"), 3 => FdInfo::Static(b"/dev/tty3"),
                4 => FdInfo::Static(b"/dev/tty4"), 5 => FdInfo::Static(b"/dev/tty5"),
                _ => FdInfo::Static(b"/dev/tty6"),
            },
            // A pseudo-directory's `data` *is* its path, so report it directly;
            // it is not in RAMFS and the reverse data-pointer lookup below
            // would answer ENOENT for it.
            VnodeKind::RamFile { data, is_dir: true, .. } => FdInfo::Static(data),
            VnodeKind::RamFile { data, .. } => FdInfo::RamData(data.as_ptr()),
            VnodeKind::TmpFile { idx, .. } => FdInfo::TmpIdx(*idx),
            VnodeKind::EventFd { .. } => FdInfo::Static(b"eventfd"),
            VnodeKind::Mq { .. } => FdInfo::Static(b"mqueue"),
            VnodeKind::TimerFd { .. } => FdInfo::Static(b"timerfd"),
            VnodeKind::PidFd { .. } => FdInfo::Static(b"anon_inode:[pidfd]"),
            VnodeKind::BpfProg { .. } => FdInfo::Static(b"anon_inode:bpf-prog"),
            VnodeKind::NsFd { .. } => FdInfo::Static(b"anon_inode:[nsfs]"),
            VnodeKind::DevUrandom => FdInfo::Static(b"/dev/urandom"),
            // ttyname(3) readlinks /proc/self/fd/N and stats what it reads
            // back, so a VT fd must name the node it was opened by.
            VnodeKind::DevVt { vt, .. } => FdInfo::Static(match vt {
                0 => b"/dev/tty0", 1 => b"/dev/tty1", 2 => b"/dev/tty2",
                3 => b"/dev/tty3", 4 => b"/dev/tty4", 5 => b"/dev/tty5",
                _ => b"/dev/tty6",
            }),
            VnodeKind::DevStdio { target_fd: 0 } => FdInfo::Static(b"/dev/stdin"),
            VnodeKind::DevStdio { target_fd: 1 } => FdInfo::Static(b"/dev/stdout"),
            VnodeKind::DevStdio { .. } => FdInfo::Static(b"/dev/stderr"),
            // A file on a mounted filesystem: only the owning mount server
            // knows the path behind its file_id, so ask it.
            VnodeKind::MountedFile { port, file_id } => FdInfo::Mounted(*port, *file_id),
            // A cgroupfs directory fd names its tree path, so openat/openat2
            // relative to it (crun) resolve.
            VnodeKind::SysBlock { dev, .. } if cgroupfs::is_dev(*dev) => FdInfo::Cg(cgroupfs::dev_cg(*dev)),
            VnodeKind::SockAlias { .. } => FdInfo::Static(b"socket:[0]"),
            VnodeKind::EpollFd { .. } => FdInfo::Static(b"anon_inode:[eventpoll]"),
            _ => FdInfo::Bad,
        }
    };
    match info {
        FdInfo::Bad => err_reply(-9),
        FdInfo::Mounted(port, file_id) => {
            let mut proxy = Message::empty();
            proxy.tag = VFS_FD_PATH;
            proxy.data[0..8].copy_from_slice(&(file_id as u64).to_le_bytes());
            proxy.data[8..16].copy_from_slice(&(buf_ptr as u64).to_le_bytes());
            proxy.data[16..24].copy_from_slice(&(buf_len as u64).to_le_bytes());
            call_port(port, proxy)
        }
        FdInfo::Static(p) => {
            let c = p.len().min(buf_len);
            unsafe { let _ = sched::uaccess::copy_raw(buf_ptr as *mut u8, p.as_ptr(), c); }
            val_reply(c as u64)
        }
        FdInfo::Pipe(r) => {
            let mut b = [0u8; 32]; let pref = b"pipe:["; b[..6].copy_from_slice(pref);
            let mut n = 6; let mut v = r;
            if v == 0 { b[n] = b'0'; n += 1; }
            else { let mut d = [0u8; 10]; let mut di = 0; while v > 0 { d[di] = b'0'+(v%10) as u8; di += 1; v /= 10; } for i in (0..di).rev() { b[n] = d[i]; n += 1; } }
            b[n] = b']'; n += 1;
            let c = n.min(buf_len);
            unsafe { let _ = sched::uaccess::copy_raw(buf_ptr as *mut u8, b.as_ptr(), c); }
            val_reply(c as u64)
        }
        FdInfo::Cg(cg) => {
            let mut p = alloc::vec::Vec::from(cgroupfs::ROOT);
            let rel = sched::cgroup::path_rel(cg, 0);
            if rel != "/" { p.extend_from_slice(rel.as_bytes()); }
            let c = p.len().min(buf_len);
            unsafe { let _ = sched::uaccess::copy_raw(buf_ptr as *mut u8, p.as_ptr(), c); }
            val_reply(c as u64)
        }
        FdInfo::PtsSlave(n) => {
            let mut b = [0u8; 16];
            let pref = b"/dev/pts/";
            b[..pref.len()].copy_from_slice(pref);
            let mut n_out = pref.len();
            let mut v = n as usize;
            if v == 0 { b[n_out] = b'0'; n_out += 1; }
            else {
                let mut d = [0u8; 5]; let mut di = 0;
                while v > 0 { d[di] = b'0' + (v % 10) as u8; di += 1; v /= 10; }
                for i in (0..di).rev() { b[n_out] = d[i]; n_out += 1; }
            }
            let c = n_out.min(buf_len);
            unsafe { let _ = sched::uaccess::copy_raw(buf_ptr as *mut u8, b.as_ptr(), c); }
            val_reply(c as u64)
        }
        FdInfo::RamData(ptr) => {
            match RAMFS.iter().find(|e| e.data.as_ptr() == ptr) {
                Some(e) => { let c = e.path.len().min(buf_len); unsafe { let _ = sched::uaccess::copy_raw(buf_ptr as *mut u8, e.path.as_ptr(), c); } val_reply(c as u64) }
                None => err_reply(-2),
            }
        }
        FdInfo::TmpIdx(i) => {
            // Staged, then copied with no lock held. copy_raw rather than
            // copy_to_user: the kernel calls this with its own buffers too
            // (sys_readlinkat /proc/<pid>/fd/N, file_name_from_fd).
            let mut st = [0u8; 4096 + 16];
            let l = {
                let tmp = TMP_FILES.lock();
                if !(i < tmp.len() && tmp[i].in_use()) { return err_reply(-9); }
                // An unlinked file or removed directory still open reads as
                // "<path> (deleted)" (Linux d_path); tmpfs paths are the
                // names, so a rename is followed already.
                let suffix: &[u8] = if tmp[i].unlinked() { b" (deleted)" } else { b"" };
                let origin = if tmp[i].path().starts_with(b"/tmp/.psys_") {
                    PSYS_ORIGIN.lock().iter().find(|e| e.0 == i).map(|e| e.1.clone())
                } else { None };
                let path = match &origin { Some(o) => &o[..], None => tmp[i].path() };
                let mut l = 0usize;
                for &c in path.iter().chain(suffix.iter()) {
                    if l == buf_len || l == st.len() { break; }
                    st[l] = c; l += 1;
                }
                l
            };
            if unsafe { sched::uaccess::copy_raw(buf_ptr as *mut u8, st.as_ptr(), l) } != 0 {
                return err_reply(-14);
            }
            val_reply(l as u64)
        }
    }
}
